//! Full-text search over each meeting's head version (migration 4).
//!
//! Two tables. `search_docs` is ordinary: one row per meeting (`meeting_id` UNIQUE), with the
//! indexed version, the text as shown (`body`), the same text case-folded in Rust (`folded`) and a
//! compact binary word map. `search_fts` is a contentless FTS5 table over `folded` only, with the
//! `trigram` tokenizer, linked by rowid (`search_fts.rowid = search_docs.id`). Every lookup and
//! delete goes through the ordinary table's keys or the FTS rowid: nothing scans the FTS table.
//!
//! Why trigram: Korean words carry particles (회의를, 회의는), so a substring index finds "회의"
//! inside them where a word tokenizer would not. The tokenizer is case-sensitive on purpose: both
//! the body and the query are folded by `fold` here, so the FTS path (3+ characters), the short
//! path (`instr` over `folded`) and the exact positions found in Rust always agree.
//!
//! The head is what the app opens a meeting on: the latest approved version, else the latest
//! draft, else the latest version of any kind. (Staleness of plugin results ignores live
//! versions, because a plugin result never reads captions; search should find captions when
//! they are all a meeting has.) Every stored version goes through `insert_version`, which calls
//! `index_head` in the same transaction. Meetings stored before migration 4 have no row yet:
//! `index_pending` fills them one meeting per transaction, a little at every open and before a
//! search, so an open never holds the write lock for long.
//!
//! Text: whitespace is collapsed and hidden characters (`text::is_hidden_char`, NUL included) are
//! dropped, in words and in queries alike. There is no Unicode normalization (no NFC crate is
//! vendored): a decomposed "é" in a transcript is not found by a composed one, and the reverse.

use std::time::{Duration, Instant};

use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use unicode_general_category::{get_general_category, GeneralCategory as Gc};

use super::Store;
use crate::contract::MeetingData;
use crate::error::{Error, Result};
use crate::text::is_hidden_char;

/// Longer input is cut to this many characters.
pub const MAX_QUERY_CHARS: usize = 100;
/// At most this many meetings are returned, newest first.
pub const MAX_MEETINGS: usize = 50;
/// At most this many hits are listed per meeting (`matches` still counts all of them).
pub const MAX_HITS_PER_MEETING: usize = 5;
/// Characters of context on each side of a hit.
const CONTEXT_CHARS: usize = 30;
/// How long `open` and `search` may spend filling in meetings that are not indexed yet. The app
/// opens a store per command, so one search keystroke spends at most the sum (plus the meeting
/// that was being indexed when the budget ran out).
pub const OPEN_INDEX_BUDGET: Duration = Duration::from_millis(100);
pub const SEARCH_INDEX_BUDGET: Duration = Duration::from_millis(200);

pub(super) const MIGRATION: &str = "
    CREATE TABLE search_docs (
        id INTEGER PRIMARY KEY,             -- = search_fts.rowid
        meeting_id TEXT NOT NULL UNIQUE REFERENCES meetings(id),
        version_id TEXT NOT NULL,           -- the head that was indexed
        unreadable INTEGER NOT NULL DEFAULT 0, -- 1: that version could not be read; nothing is indexed
        body TEXT NOT NULL,                 -- one line per channel, words joined by single spaces
        folded TEXT NOT NULL,               -- body folded by Rust, char for char
        word_map BLOB NOT NULL              -- see `encode_map`
    );
    CREATE VIRTUAL TABLE search_fts USING fts5(
        folded, content = '', contentless_delete = 1, tokenize = 'trigram case_sensitive 1'
    );
    -- read at every open (warnings) and search (count): only the few unreadable rows
    CREATE INDEX search_docs_unreadable ON search_docs(meeting_id, version_id) WHERE unreadable = 1;
";

const ROW_OF_MEETING: &str = "SELECT id, version_id FROM search_docs WHERE meeting_id=?1";
const UNREADABLE: &str = "SELECT meeting_id, version_id FROM search_docs WHERE unreadable = 1 ORDER BY meeting_id";

/// Orders a meeting's versions so that the head comes first (approved > draft > live, newest).
/// Unlike the staleness head (approved > draft, `RESULT_SELECT`), live counts here: plugins never
/// read captions, but a meeting that only has captions should still be found, as the app opens it.
const HEAD_ORDER: &str = "ORDER BY CASE kind WHEN 'approved' THEN 2 WHEN 'draft' THEN 1 ELSE 0 END DESC, seq DESC LIMIT 1";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchHit {
    /// the text before the match (with a leading "…" when cut), the match as written, and after
    pub before: String,
    pub text: String,
    pub after: String,
    /// first and last word of the match: an anchor into `version_id`
    pub from: String,
    pub to: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub meeting_id: String,
    pub title: Option<String>,
    pub started_at: Option<String>,
    pub created_at: String,
    /// the indexed head version the hits point into
    pub version_id: String,
    pub kind: String,
    /// every occurrence in the meeting; only the first `MAX_HITS_PER_MEETING` (in time) are in `hits`
    pub matches: usize,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchOutcome {
    pub meetings: Vec<SearchResult>,
    /// more meetings matched than `MAX_MEETINGS`
    pub more: bool,
    /// meetings not indexed yet (filled in at later opens and searches)
    pub pending: usize,
    /// meetings left out because their current version could not be read
    pub skipped: usize,
}

// ---------------------------------------------------------------- text

/// One char per char (the first of its lowercase), so offsets agree between body and folded.
/// Final sigma folds like sigma: ΟΔΟΣ lowercases to "οδος" in a word but to "οδοσ" alone.
fn fold(c: char) -> char {
    match c.to_lowercase().next().unwrap_or(c) {
        'ς' => 'σ',
        l => l,
    }
}

fn fold_str(s: &str) -> String {
    s.chars().map(fold).collect()
}

/// Drop hidden characters, collapse whitespace runs to one space, trim.
fn clean(s: &str) -> String {
    let kept: String = s.chars().filter(|&c| !is_hidden_char(c) || c.is_whitespace()).collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_combining(c: char) -> bool {
    matches!(get_general_category(c), Gc::NonspacingMark | Gc::SpacingMark | Gc::EnclosingMark)
}

/// The query as it is matched: cleaned, cut, folded.
fn prepare_query(q: &str) -> String {
    let q: String = clean(q).chars().take(MAX_QUERY_CHARS).collect();
    fold_str(q.trim_end())
}

/// One FTS5 string: the whole query is a single quoted phrase, `"` doubled. With the trigram
/// tokenizer that is a substring match, and no operator or column filter can be smuggled in.
fn fts_phrase(q: &str) -> String {
    format!("\"{}\"", q.replace('"', "\"\""))
}

// ---------------------------------------------------------------- word map

/// One word: (id, start_ms, end_ms, char offset in body).
type Word = (String, i64, i64, u32);

/// Little-endian: `n: u32`, then `offset[n]: u32` (chars into body), `start[n]: i64`,
/// `end[n]: i64`, `id_end[n]: u32` (byte end of each id in the id area), then the ids, UTF-8.
/// Fixed-width sections, so a lookup reads a few bytes of the blob, never all of it.
fn encode_map(words: &[Word]) -> Vec<u8> {
    let n = words.len();
    let mut out = Vec::with_capacity(4 + n * 24 + words.iter().map(|w| w.0.len()).sum::<usize>());
    out.extend((n as u32).to_le_bytes());
    words.iter().for_each(|w| out.extend(w.3.to_le_bytes()));
    words.iter().for_each(|w| out.extend(w.1.to_le_bytes()));
    words.iter().for_each(|w| out.extend(w.2.to_le_bytes()));
    let mut end = 0u32;
    for w in words {
        end += w.0.len() as u32;
        out.extend(end.to_le_bytes());
    }
    words.iter().for_each(|w| out.extend(w.0.as_bytes()));
    out
}

/// Random access into a stored map (an open blob, or bytes in a test).
trait MapRead {
    fn read(&self, at: usize, buf: &mut [u8]) -> Result<()>;
    fn size(&self) -> usize;
}

impl MapRead for [u8] {
    fn read(&self, at: usize, buf: &mut [u8]) -> Result<()> {
        let src = self.get(at..at + buf.len()).ok_or_else(|| Error::Corrupt("search word map is truncated".into()))?;
        buf.copy_from_slice(src);
        Ok(())
    }
    fn size(&self) -> usize {
        self.len()
    }
}

impl MapRead for rusqlite::blob::Blob<'_> {
    fn read(&self, at: usize, buf: &mut [u8]) -> Result<()> {
        self.read_at_exact(buf, at).map_err(|e| Error::Corrupt(format!("search word map: {e}")))
    }
    fn size(&self) -> usize {
        self.len()
    }
}

struct Map<'a, R: MapRead + ?Sized> {
    r: &'a R,
    n: usize,
}

impl<'a, R: MapRead + ?Sized> Map<'a, R> {
    fn new(r: &'a R) -> Result<Self> {
        let mut b = [0u8; 4];
        r.read(0, &mut b)?;
        let n = u32::from_le_bytes(b) as usize;
        if 4 + 24 * n > r.size() {
            return Err(Error::Corrupt("search word map is shorter than its word count".into()));
        }
        Ok(Map { r, n })
    }
    fn u32_at(&self, at: usize) -> Result<u32> {
        let mut b = [0u8; 4];
        self.r.read(at, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn i64_at(&self, at: usize) -> Result<i64> {
        let mut b = [0u8; 8];
        self.r.read(at, &mut b)?;
        Ok(i64::from_le_bytes(b))
    }
    fn offset(&self, k: usize) -> Result<u32> {
        self.u32_at(4 + 4 * k)
    }
    /// The word covering char offset `at`: the last one starting at or before it.
    fn word_at(&self, at: usize) -> Result<usize> {
        let (mut lo, mut hi) = (0usize, self.n); // first index whose offset > at
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.offset(mid)? as usize <= at {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo.saturating_sub(1))
    }
    fn word(&self, k: usize) -> Result<(String, i64, i64)> {
        let n = self.n;
        let start = self.i64_at(4 + 4 * n + 8 * k)?;
        let end = self.i64_at(4 + 12 * n + 8 * k)?;
        let ends = 4 + 20 * n;
        let id_lo = if k == 0 { 0 } else { self.u32_at(ends + 4 * (k - 1))? as usize };
        let id_hi = self.u32_at(ends + 4 * k)? as usize;
        if id_hi < id_lo || ends + 4 * n + id_hi > self.r.size() {
            return Err(Error::Corrupt("search word map has a bad id range".into()));
        }
        let mut id = vec![0u8; id_hi - id_lo];
        self.r.read(ends + 4 * n + id_lo, &mut id)?;
        Ok((String::from_utf8(id).map_err(|e| Error::Corrupt(format!("search word map: {e}")))?, start, end))
    }
}

/// The body is one line per channel, each in the stored (time) order: words of the two channels
/// interleave in time, and a phrase is only ever said on one of them. A query never contains a
/// line break, so no match spans two channels.
fn build(md: &MeetingData) -> (String, Vec<Word>) {
    let mut channels: Vec<u32> = md.words.iter().map(|w| w.channel).collect();
    channels.sort_unstable();
    channels.dedup();
    let (mut body, mut chars, mut map) = (String::new(), 0u32, Vec::with_capacity(md.words.len()));
    for ch in channels {
        let mut first = true;
        for w in md.words.iter().filter(|w| w.channel == ch) {
            let t = clean(&w.text);
            if t.is_empty() {
                continue;
            }
            if !body.is_empty() {
                body.push(if first { '\n' } else { ' ' });
                chars += 1;
            }
            first = false;
            map.push((w.id.clone(), w.start_ms, w.end_ms, chars));
            chars += t.chars().count() as u32;
            body.push_str(&t);
        }
    }
    (body, map)
}

// ---------------------------------------------------------------- hits

/// Where a hit sits in the body, in chars: the match, and the context to show around it.
#[derive(Debug, PartialEq)]
struct Place {
    at: usize,
    end: usize,
    lo: usize,
    hi: usize,
    /// which line (channel) of the body
    line: usize,
    cut_lo: bool,
    cut_hi: bool,
}

/// Every non-overlapping occurrence of `needle` in `folded` (both folded), with its context:
/// at most `CONTEXT_CHARS` each side, never past the line (channel), never splitting a
/// combining mark from its base.
fn places(folded: &[char], needle: &[char]) -> Vec<Place> {
    let mut out = Vec::new();
    if needle.is_empty() || needle.len() > folded.len() {
        return out;
    }
    let breaks: Vec<usize> = folded.iter().enumerate().filter_map(|(i, &c)| (c == '\n').then_some(i)).collect();
    let mut i = 0;
    while i + needle.len() <= folded.len() {
        if folded[i..i + needle.len()] != *needle {
            i += 1;
            continue;
        }
        let end = i + needle.len();
        let k = breaks.partition_point(|&b| b < i); // the line holding the match
        let line_lo = if k == 0 { 0 } else { breaks[k - 1] + 1 };
        let line_hi = breaks.get(k).copied().unwrap_or(folded.len());
        let mut lo = i.saturating_sub(CONTEXT_CHARS).max(line_lo);
        while lo > line_lo && is_combining(folded[lo]) {
            lo -= 1; // keep the base with its marks
        }
        let mut hi = (end + CONTEXT_CHARS).min(line_hi);
        while hi < line_hi && is_combining(folded[hi]) {
            hi += 1;
        }
        out.push(Place { at: i, end, lo, hi, line: k, cut_lo: lo > line_lo, cut_hi: hi < line_hi });
        i = end;
    }
    out
}

/// Total count, and the first `keep` hits in time order. `text(from, len)` returns `len` chars of
/// the body starting at char `from`.
fn hits_of<R: MapRead + ?Sized>(
    folded: &str,
    q: &str,
    map: &Map<'_, R>,
    keep: usize,
    text: &dyn Fn(usize, usize) -> Result<String>,
) -> Result<(usize, Vec<SearchHit>)> {
    let folded: Vec<char> = folded.chars().collect();
    let needle: Vec<char> = q.chars().collect();
    let all = places(&folded, &needle);
    if map.n == 0 {
        return Ok((0, vec![]));
    }
    // Within a line (channel) words are in time order, so the earliest `keep` hits overall are
    // among the first `keep` of each line: only those are looked up in the map.
    let mut timed = Vec::new();
    let (mut line, mut taken) = (usize::MAX, 0);
    for p in &all {
        if p.line != line {
            (line, taken) = (p.line, 0);
        }
        if taken == keep {
            continue;
        }
        taken += 1;
        let a = map.word_at(p.at)?;
        timed.push((map.i64_at(4 + 4 * map.n + 8 * a)?, p.at, a, p));
    }
    timed.sort_by_key(|t| (t.0, t.1));
    let mut hits = Vec::new();
    for (_, _, a, p) in timed.into_iter().take(keep) {
        let b = map.word_at(p.end - 1)?;
        let (from, start_ms, _) = map.word(a)?;
        let (to, _, end_ms) = map.word(b)?;
        let snip = text(p.lo, p.hi - p.lo)?.chars().collect::<Vec<char>>();
        let part = |r: std::ops::Range<usize>| snip.get(r).map(|s| s.iter().collect::<String>()).unwrap_or_default();
        hits.push(SearchHit {
            before: format!("{}{}", if p.cut_lo { "…" } else { "" }, part(0..p.at - p.lo)),
            text: part(p.at - p.lo..p.end - p.lo),
            after: format!("{}{}", part(p.end - p.lo..p.hi - p.lo), if p.cut_hi { "…" } else { "" }),
            from,
            to,
            start_ms,
            end_ms: end_ms.max(start_ms),
        });
    }
    Ok((all.len(), hits))
}

// ---------------------------------------------------------------- store

impl Store {
    /// Point the meeting's search row at its current head. `just_stored` is the version that was
    /// just inserted, used as-is when it is the head (no reload). Inside the caller's transaction.
    /// Returns false when the head could not be read: the row then records it as unreadable.
    pub(super) fn index_head(&self, meeting_id: &str, just_stored: Option<&MeetingData>) -> Result<bool> {
        let head: Option<String> = self
            .conn
            .query_row(&format!("SELECT id FROM versions WHERE meeting_id=?1 {HEAD_ORDER}"), [meeting_id], |r| r.get(0))
            .optional()?;
        let row: Option<(i64, String)> = self
            .conn
            .query_row(ROW_OF_MEETING, [meeting_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let (Some(h), Some((_, v))) = (&head, &row) {
            if h == v {
                return Ok(true);
            }
        }
        if let Some((id, _)) = row {
            self.conn.execute("DELETE FROM search_fts WHERE rowid=?1", [id])?;
            self.conn.execute("DELETE FROM search_docs WHERE id=?1", [id])?;
        }
        let Some(head) = head else { return Ok(true) };
        let loaded;
        let md = match just_stored {
            Some(md) if md.version.id == head => Some(md),
            _ => match self.version(&head) {
                Ok(m) => {
                    loaded = m;
                    Some(&loaded)
                }
                Err(Error::Corrupt(_) | Error::NotFound(_)) => None,
                Err(e) => return Err(e),
            },
        };
        let Some(md) = md else {
            // remembered, so it is not retried at every open; a new version replaces it
            self.conn.execute(
                "INSERT INTO search_docs (meeting_id, version_id, unreadable, body, folded, word_map) VALUES (?1,?2,1,'','',x'00000000')",
                params![meeting_id, head],
            )?;
            return Ok(false);
        };
        let (body, map) = build(md);
        let folded = fold_str(&body);
        self.conn.execute(
            "INSERT INTO search_docs (meeting_id, version_id, body, folded, word_map) VALUES (?1,?2,?3,?4,?5)",
            params![meeting_id, head, body, folded, encode_map(&map)],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute("INSERT INTO search_fts (rowid, folded) VALUES (?1,?2)", params![id, folded])?;
        Ok(true)
    }

    /// Meetings that have versions but no search row yet, newest first (the ones most likely
    /// searched for are ready first).
    fn unindexed(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT m.id FROM meetings m
             WHERE EXISTS (SELECT 1 FROM versions v WHERE v.meeting_id = m.id)
               AND NOT EXISTS (SELECT 1 FROM search_docs d WHERE d.meeting_id = m.id)
             ORDER BY COALESCE(m.started_at, m.created_at) DESC, m.id",
        )?;
        let ids = st.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(ids)
    }

    /// Index meetings that have no search row yet, one meeting per transaction, until `budget`
    /// is spent (`None`: all of them). Returns how many are still not indexed.
    pub fn index_pending(&self, budget: Option<Duration>) -> Result<usize> {
        let t0 = Instant::now();
        let ids = self.unindexed()?;
        for (done, id) in ids.iter().enumerate() {
            if budget.is_some_and(|b| t0.elapsed() >= b) {
                return Ok(ids.len() - done);
            }
            self.immediate(|| self.index_head(id, None))?;
        }
        Ok(0)
    }

    /// Meetings whose current version could not be read for search: (meeting, version).
    fn unreadable(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare(UNREADABLE)?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// One warning line naming the meetings left out of search (at most 5 by name), if any.
    pub(super) fn unreadable_warning(&self) -> Result<Option<String>> {
        let rows = self.unreadable()?;
        if rows.is_empty() {
            return Ok(None);
        }
        let named: Vec<String> = rows.iter().take(5).map(|(m, v)| format!("{m} ({v})")).collect();
        let rest = if rows.len() > 5 { format!(" and {} more", rows.len() - 5) } else { String::new() };
        Ok(Some(format!("{} meeting(s) left out of search, their current version could not be read: {}{rest}", rows.len(), named.join(", "))))
    }

    /// Forget a meeting's search row (a damaged one): the backfill builds it again.
    fn drop_row(&self, id: i64) -> Result<()> {
        self.immediate(|| {
            self.conn.execute("DELETE FROM search_fts WHERE rowid=?1", [id])?;
            self.conn.execute("DELETE FROM search_docs WHERE id=?1", [id])?;
            Ok(())
        })
    }

    /// Meetings whose head text contains `query` (literal, case-insensitive), newest first.
    /// Empty or whitespace-only input finds nothing. Meetings not indexed yet are indexed first,
    /// within `SEARCH_INDEX_BUDGET`; the rest are counted in `pending`.
    #[allow(clippy::type_complexity)]
    pub fn search(&self, query: &str) -> Result<SearchOutcome> {
        // best effort, as at open: a busy database (or one bad meeting) must not cost the search
        let pending = match self.index_pending(Some(SEARCH_INDEX_BUDGET)) {
            Ok(n) => n,
            Err(_) => self.unindexed().map_or(0, |v| v.len()),
        };
        let skipped = self.unreadable().map_or(0, |v| v.len());
        let q = prepare_query(query);
        if q.is_empty() {
            return Ok(SearchOutcome { meetings: vec![], more: false, pending, skipped });
        }
        let (cond, arg) = if q.chars().count() >= 3 {
            ("d.id IN (SELECT rowid FROM search_fts WHERE search_fts MATCH ?1)", fts_phrase(&q))
        } else {
            ("instr(d.folded, ?1) > 0", q.clone())
        };
        // the candidates only: ids and the meeting row; the text is read for the kept ones
        let sql = format!(
            "SELECT d.id, d.meeting_id, d.version_id, m.title, m.started_at, m.created_at, v.kind
             FROM search_docs d JOIN meetings m ON m.id = d.meeting_id JOIN versions v ON v.id = d.version_id
             WHERE d.unreadable = 0 AND {cond}
             ORDER BY COALESCE(m.started_at, m.created_at) DESC, m.id LIMIT {}",
            MAX_MEETINGS + 1
        );
        let mut st = self.conn.prepare(&sql)?;
        let mut rows: Vec<(i64, String, String, Option<String>, Option<String>, String, String)> = st
            .query_map([arg], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let more = rows.len() > MAX_MEETINGS;
        rows.truncate(MAX_MEETINGS);
        let mut out = Vec::with_capacity(rows.len());
        let mut damaged = 0;
        for (id, meeting_id, version_id, title, started_at, created_at, kind) in rows {
            match self.hits_of_row(id, &q) {
                Ok((0, _)) => {}
                Ok((matches, hits)) => out.push(SearchResult { meeting_id, title, started_at, created_at, version_id, kind, matches, hits }),
                // a damaged row costs only itself: it is dropped and built again by the backfill
                Err(Error::Corrupt(_)) => {
                    damaged += 1;
                    let _ = self.drop_row(id);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(SearchOutcome { meetings: out, more, pending: pending + damaged, skipped })
    }

    fn hits_of_row(&self, id: i64, q: &str) -> Result<(usize, Vec<SearchHit>)> {
        let folded: String = self.conn.query_row("SELECT folded FROM search_docs WHERE id=?1", [id], |r| r.get(0))?;
        let blob = self.conn.blob_open(rusqlite::MAIN_DB, c"search_docs", c"word_map", id, true)?;
        let map = Map::new(&blob)?;
        let text = |from: usize, len: usize| -> Result<String> {
            Ok(self.conn.query_row("SELECT substr(body, ?2, ?3) FROM search_docs WHERE id=?1", params![id, from as i64 + 1, len as i64], |r| r.get(0))?)
        };
        hits_of(&folded, q, &map, MAX_HITS_PER_MEETING, &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// body and map for words on one line; a word starting with '\n' begins the next channel
    fn layout(words: &[&str]) -> (String, Vec<Word>) {
        let (mut body, mut map, mut at) = (String::new(), vec![], 0u32);
        for (i, w) in words.iter().enumerate() {
            let (sep, w) = match w.strip_prefix('\n') {
                Some(rest) => ('\n', rest),
                None => (' ', *w),
            };
            if i > 0 {
                body.push(sep);
                at += 1;
            }
            map.push((format!("w{i}"), i as i64 * 1000, i as i64 * 1000 + 500, at));
            body.push_str(w);
            at += w.chars().count() as u32;
        }
        (body, map)
    }

    fn run(words: &[&str], times: Option<&[i64]>, q: &str) -> (usize, Vec<SearchHit>) {
        let (body, mut map) = layout(words);
        if let Some(t) = times {
            map.iter_mut().zip(t).for_each(|(w, &s)| (w.1, w.2) = (s, s + 500));
        }
        let bytes = encode_map(&map);
        let m = Map::new(bytes.as_slice()).unwrap();
        let chars: Vec<char> = body.chars().collect();
        let text = |from: usize, len: usize| Ok(chars[from..from + len].iter().collect());
        hits_of(&fold_str(&body), &prepare_query(q), &m, 5, &text).unwrap()
    }

    #[test]
    fn the_word_map_round_trips() {
        let words: Vec<Word> = vec![("w0".into(), 10, 20, 0), ("긴-아이디".into(), -5, i64::MAX, 7), ("".into(), 0, 0, 9)];
        let bytes = encode_map(&words);
        let m = Map::new(bytes.as_slice()).unwrap();
        for (k, w) in words.iter().enumerate() {
            assert_eq!(m.word(k).unwrap(), (w.0.clone(), w.1, w.2));
            assert_eq!(m.offset(k).unwrap(), w.3);
        }
        assert_eq!((m.word_at(0).unwrap(), m.word_at(6).unwrap(), m.word_at(7).unwrap(), m.word_at(100).unwrap()), (0, 0, 1, 2));
        assert!(Map::new(&bytes[..2]).is_err(), "a truncated map is an error, not a panic");
    }

    #[test]
    fn a_hit_spanning_words_anchors_both_ends() {
        let (n, hits) = run(&["그", "API", "서버가", "느려요"], None, "api 서버");
        assert_eq!(n, 1);
        let h = &hits[0];
        assert_eq!((h.from.as_str(), h.to.as_str(), h.start_ms, h.end_ms), ("w1", "w2", 1000, 2500));
        assert_eq!((h.before.as_str(), h.text.as_str(), h.after.as_str()), ("그 ", "API 서버", "가 느려요"));
    }

    #[test]
    fn context_stays_on_its_channel_line_and_hits_are_in_time_order() {
        let words = ["나중", "배포", "\n먼저", "배포"];
        let (n, hits) = run(&words, Some(&[5000, 6000, 1000, 2000]), "배포");
        assert_eq!(n, 2);
        assert_eq!((hits[0].from.as_str(), hits[0].before.as_str(), hits[0].after.as_str()), ("w3", "먼저 ", ""));
        assert_eq!((hits[1].from.as_str(), hits[1].before.as_str(), hits[1].after.as_str()), ("w1", "나중 ", ""));
        assert_eq!(run(&words, None, "배포 먼저").0, 0, "no match across channels");
    }

    #[test]
    fn folding_is_unicode_and_the_same_for_body_and_query() {
        for (word, q) in [("Δx", "δx"), ("ÉCOLE", "éc"), ("İstanbul", "istanbul"), ("İstanbul", "İSTANBUL"), ("ΟΔΟΣ", "οδος"), ("οδος", "ΟΔΟΣ"), ("Straße", "STRASSE")] {
            let found = run(&[word], None, q).0;
            // ß has no one-char uppercase: STRASSE folds to strasse, which is not in straße
            assert_eq!(found, usize::from(word != "Straße"), "{word} / {q}");
        }
        let (_, hits) = run(&["ÉCOLE"], None, "éc");
        assert_eq!(hits[0].text, "ÉC", "the hit shows the original text");
    }

    #[test]
    fn context_does_not_split_a_combining_mark_from_its_base() {
        // "e" + acute + 28 + space = 31 chars before the match: a plain 30-char cut would start
        // on the combining acute; it moves back to take the "e" too
        let pre = format!("e\u{301}{}", "가".repeat(28));
        let (_, hits) = run(&[&pre, "찾기"], None, "찾기");
        assert_eq!(hits[0].before, format!("{pre} "));
        // and forward: the context after the match keeps a mark with its base
        let post = format!("{}e\u{301}끝", "나".repeat(28));
        let (_, hits) = run(&["찾기", &post], None, "찾기");
        assert!(hits[0].after.trim_end_matches('…').ends_with("e\u{301}"), "{:?}", hits[0].after);
    }

    fn plan(s: &Store, sql: &str) -> String {
        let mut st = s.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let params: &[&str] = if sql.contains('?') { &["x"] } else { &[] };
        let rows = st.query_map(rusqlite::params_from_iter(params), |r| r.get::<_, String>(3)).unwrap();
        rows.map(|r| r.unwrap()).collect::<Vec<_>>().join(" | ")
    }

    /// The lookups a write makes go through keys: the regression that scanned the whole index
    /// (and read every stored word map) at every write must not come back.
    #[test]
    fn write_path_lookups_use_keys_not_scans() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(tmp.path()).unwrap();
        let p = plan(&s, ROW_OF_MEETING);
        assert!(p.contains("USING INDEX") && !p.contains("SCAN"), "{p}");
        let p = plan(&s, &format!("SELECT id FROM versions WHERE meeting_id=?1 {HEAD_ORDER}"));
        assert!(p.contains("USING INDEX versions_meeting") && !p.contains("SCAN versions"), "{p}");
        // read at every open and search: only the partial index of the few unreadable rows
        let p = plan(&s, UNREADABLE);
        assert_eq!(p, "SCAN search_docs USING COVERING INDEX search_docs_unreadable");
    }

    #[test]
    fn the_backfill_takes_the_newest_meetings_first() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(tmp.path()).unwrap();
        for (m, at) in [("a", Some("2026-01-01T00:00:00Z")), ("b", None), ("c", Some("2026-03-01T00:00:00Z")), ("none", None)] {
            s.conn.execute("INSERT INTO meetings (id, started_at, duration_ms, created_at) VALUES (?1, ?2, 0, '2026-02-01T00:00:00Z')", params![m, at]).unwrap();
            if m != "none" {
                s.conn.execute("INSERT INTO versions (id, meeting_id, kind, created_at, content_sha256, json) VALUES (?1,?2,'draft','x','x','{}')", params![format!("{m}.draft.1"), m]).unwrap();
            }
        }
        assert_eq!(s.unindexed().unwrap(), ["c", "b", "a"], "newest first; a meeting without versions has nothing to index");
    }

    /// Indexing one meeting costs the same with 1 or 41 long meetings (~4,400 words each) indexed. Times
    /// `index_head` alone (no validation, no JSON), so a lookup that scans stored text shows.
    #[test]
    fn index_cost_does_not_grow_with_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(tmp.path()).unwrap();
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/fixtures/meeting01/meeting-data.draft.json");
        let base: MeetingData = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let mut big = base.clone();
        big.words = (0..40).flat_map(|k| base.words.iter().map(move |w| { let mut w = w.clone(); w.id = format!("{}-{k}", w.id); w })).collect();
        let add = |i: usize, md: &MeetingData| -> Duration {
            let (m, v) = (format!("m{i}"), format!("m{i}.draft.1"));
            s.conn.execute("INSERT INTO meetings (id, duration_ms, created_at) VALUES (?1, 0, 'x')", [&m]).unwrap();
            s.conn.execute("INSERT INTO versions (id, meeting_id, kind, created_at, content_sha256, json) VALUES (?1,?2,'draft','x','x','{}')", [&v, &m]).unwrap();
            let mut md = md.clone();
            (md.meeting.id, md.version.id) = (m.clone(), v);
            let t0 = Instant::now();
            s.immediate(|| s.index_head(&m, Some(&md))).unwrap();
            t0.elapsed()
        };
        let mut i = 0;
        let mut median = |n: usize, md: &MeetingData| {
            let mut t: Vec<Duration> = (0..n).map(|_| { i += 1; add(i, md) }).collect();
            t.sort();
            t[n / 2]
        };
        median(1, &big);
        let small = median(9, &base);
        median(40, &big);
        let large = median(9, &base);
        assert!(large < small * 2 + Duration::from_micros(500), "indexing a meeting took {small:?} with 1 long meeting indexed, {large:?} with 41");
    }

    #[test]
    fn queries_are_cleaned_literal_and_cut() {
        assert_eq!(fts_phrase("a\" OR body:*"), "\"a\"\" OR body:*\"");
        assert_eq!(prepare_query("  A\0P\u{200b}I \t 서버 "), "api 서버");
        assert_eq!(prepare_query("\0\u{feff}"), "");
        assert_eq!(prepare_query(&"가".repeat(500)).chars().count(), MAX_QUERY_CHARS);
    }
}
