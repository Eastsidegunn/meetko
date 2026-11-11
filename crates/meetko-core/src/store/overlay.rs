//! Speaker names, merges and edited turn text (migration 6): overlays on a transcript, see
//! `crate::overlay` for what they mean and how they are shown.
//!
//! Rows are keyed by (meeting, `content_sha256` of the transcript, speaker or turn id). Versions
//! stay immutable: nothing here touches `versions`. Approving copies a draft's content, so its
//! overlays apply to the approved copy as well. A new transcript of the meeting (다시 받아쓰기, a
//! diarization run) has other content, so the overlays made on an earlier one do not apply to it:
//! they stay with the earlier version (shown when that version is opened) and are reported by
//! `overlays_set_aside` until the user discards them. Speaker and turn ids are positions in one
//! transcript (`s1` is "the second speaker found", `t0004` "the fifth turn"), so carrying a name or
//! an edit to another transcript by id would put it on the wrong person or sentence.
//!
//! Merges are kept one step deep: merging A into B points A at B's own target, and every speaker
//! that pointed at A now points at that target too.

use std::collections::{BTreeMap, HashMap};

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use super::Store;
use crate::contract::MeetingData;
use crate::error::{Error, Result};
use crate::overlay::{clean_edit, clean_name, original_text, Overlay, SpeakerOverlay, TurnEdit};
use crate::util::now_rfc3339;

pub(super) const MIGRATION: &str = "
    CREATE TABLE speaker_overlays (
        meeting_id TEXT NOT NULL REFERENCES meetings(id),
        content_sha256 TEXT NOT NULL,       -- the transcript the speaker id belongs to (versions.content_sha256)
        speaker_id TEXT NOT NULL,
        name TEXT,                          -- shown instead of the label; NULL when only merged
        merged_into TEXT,                   -- another speaker id of the same transcript
        updated_at TEXT NOT NULL,
        PRIMARY KEY (meeting_id, content_sha256, speaker_id)
    );
    CREATE TABLE turn_edits (
        meeting_id TEXT NOT NULL REFERENCES meetings(id),
        content_sha256 TEXT NOT NULL,       -- the transcript the turn id belongs to
        turn_id TEXT NOT NULL,
        text TEXT NOT NULL,                 -- shown instead of the turn's words; the words stay stored
        edited_at TEXT NOT NULL,
        PRIMARY KEY (meeting_id, content_sha256, turn_id)
    );
";

/// Overlays made on earlier transcripts of a meeting, which the version asked about does not show.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SetAside {
    /// edited turns
    pub edits: usize,
    /// renamed or merged speakers
    pub speakers: usize,
    /// the newest version that shows them (open it to see them)
    pub version_id: Option<String>,
}

impl Store {
    /// (meeting, content hash) of a version.
    fn transcript_of(&self, version_id: &str) -> Result<(String, String)> {
        let r = self.version_row(version_id)?;
        Ok((r.meeting_id, r.content_sha256))
    }

    /// The overlays of the transcript with this content.
    pub(super) fn overlay_for_content(&self, meeting_id: &str, content_sha256: &str) -> Result<Overlay> {
        let mut o = Overlay::default();
        let mut st = self.conn.prepare(
            "SELECT speaker_id, name, merged_into FROM speaker_overlays WHERE meeting_id=?1 AND content_sha256=?2",
        )?;
        let rows = st.query_map(params![meeting_id, content_sha256], |r| {
            Ok((r.get::<_, String>(0)?, SpeakerOverlay { name: r.get(1)?, merged_into: r.get(2)? }))
        })?;
        for row in rows {
            let (id, s) = row?;
            o.speakers.insert(id, s);
        }
        let mut st = self.conn.prepare("SELECT turn_id, text, edited_at FROM turn_edits WHERE meeting_id=?1 AND content_sha256=?2")?;
        let rows = st.query_map(params![meeting_id, content_sha256], |r| {
            Ok((r.get::<_, String>(0)?, TurnEdit { text: r.get(1)?, edited_at: r.get(2)? }))
        })?;
        for row in rows {
            let (id, e) = row?;
            o.edits.insert(id, e);
        }
        Ok(o)
    }

    /// The names, merges and edits a version is shown with.
    pub fn overlay(&self, version_id: &str) -> Result<Overlay> {
        let (m, sha) = self.transcript_of(version_id)?;
        self.overlay_for_content(&m, &sha)
    }

    /// The version and its overlay, checked to belong together: what every write starts from.
    fn load_for_write(&self, version_id: &str) -> Result<(MeetingData, String, String, Overlay)> {
        let (m, sha) = self.transcript_of(version_id)?;
        let md = self.version(version_id)?;
        let o = self.overlay_for_content(&m, &sha)?;
        Ok((md, m, sha, o))
    }

    fn require_speaker(md: &MeetingData, id: &str) -> Result<()> {
        if md.speakers.iter().any(|s| s.id == id) {
            Ok(())
        } else {
            Err(Error::NotFound(format!("speaker {id} in {}", md.version.id)))
        }
    }

    fn put_speaker(&self, m: &str, sha: &str, id: &str, s: &SpeakerOverlay) -> Result<()> {
        if s.name.is_none() && s.merged_into.is_none() {
            self.conn.execute(
                "DELETE FROM speaker_overlays WHERE meeting_id=?1 AND content_sha256=?2 AND speaker_id=?3",
                params![m, sha, id],
            )?;
        } else {
            self.conn.execute(
                "INSERT INTO speaker_overlays (meeting_id, content_sha256, speaker_id, name, merged_into, updated_at) VALUES (?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(meeting_id, content_sha256, speaker_id) DO UPDATE SET name=excluded.name, merged_into=excluded.merged_into, updated_at=excluded.updated_at",
                params![m, sha, id, s.name, s.merged_into, now_rfc3339()],
            )?;
        }
        Ok(())
    }

    /// Show speaker `speaker_id` (or, when it is merged, the speaker it was merged into) as `name`
    /// in this transcript. A name equal to the speaker's own label clears the name.
    pub fn rename_speaker(&self, version_id: &str, speaker_id: &str, name: &str) -> Result<Overlay> {
        let name = clean_name(name)?;
        self.immediate(|| {
            let (md, m, sha, o) = self.load_for_write(version_id)?;
            Self::require_speaker(&md, speaker_id)?;
            let target = o.target(speaker_id).to_string();
            let label = md.speakers.iter().find(|s| s.id == target).and_then(|s| s.label.clone());
            let mut row = o.speakers.get(&target).cloned().unwrap_or_default();
            row.name = (label.as_deref() != Some(name.as_str())).then_some(name);
            self.put_speaker(&m, &sha, &target, &row)?;
            self.overlay_for_content(&m, &sha)
        })
    }

    /// Show every turn of `speaker_id` as speaker `into` ("the same person"). Its own name is
    /// dropped; speakers merged into it follow it.
    pub fn merge_speaker(&self, version_id: &str, speaker_id: &str, into: &str) -> Result<Overlay> {
        self.immediate(|| {
            let (md, m, sha, o) = self.load_for_write(version_id)?;
            Self::require_speaker(&md, speaker_id)?;
            Self::require_speaker(&md, into)?;
            let target = o.target(into).to_string();
            if target == speaker_id {
                return Err(Error::Invalid(format!("{speaker_id} cannot be merged into itself")));
            }
            self.put_speaker(&m, &sha, speaker_id, &SpeakerOverlay { name: None, merged_into: Some(target.clone()) })?;
            self.conn.execute(
                "UPDATE speaker_overlays SET merged_into=?4, updated_at=?5 WHERE meeting_id=?1 AND content_sha256=?2 AND merged_into=?3",
                params![m, sha, speaker_id, target, now_rfc3339()],
            )?;
            self.overlay_for_content(&m, &sha)
        })
    }

    /// Back to the label the transcript gave `speaker_id`: its name and its merge are removed.
    /// Speakers merged into it stay merged into it.
    pub fn reset_speaker(&self, version_id: &str, speaker_id: &str) -> Result<Overlay> {
        self.immediate(|| {
            let (md, m, sha, _) = self.load_for_write(version_id)?;
            Self::require_speaker(&md, speaker_id)?;
            self.put_speaker(&m, &sha, speaker_id, &SpeakerOverlay::default())?;
            self.overlay_for_content(&m, &sha)
        })
    }

    /// Show turn `turn_id` with `text` instead of its words. Text equal to the words removes the
    /// edit. The search row of the meeting follows in the same transaction.
    pub fn edit_turn(&self, version_id: &str, turn_id: &str, text: &str) -> Result<Overlay> {
        let text = clean_edit(text)?;
        self.immediate(|| {
            let (md, m, sha, _) = self.load_for_write(version_id)?;
            let t = md.turns.iter().find(|t| t.id == turn_id).ok_or_else(|| Error::NotFound(format!("turn {turn_id} in {version_id}")))?;
            let pos: HashMap<&str, usize> = md.words.iter().enumerate().map(|(i, w)| (w.id.as_str(), i)).collect();
            let original = original_text(crate::export::turn_words(&md, &pos, t)?);
            if original == text {
                self.conn.execute(
                    "DELETE FROM turn_edits WHERE meeting_id=?1 AND content_sha256=?2 AND turn_id=?3",
                    params![m, sha, turn_id],
                )?;
            } else {
                self.conn.execute(
                    "INSERT INTO turn_edits (meeting_id, content_sha256, turn_id, text, edited_at) VALUES (?1,?2,?3,?4,?5)
                     ON CONFLICT(meeting_id, content_sha256, turn_id) DO UPDATE SET text=excluded.text, edited_at=excluded.edited_at",
                    params![m, sha, turn_id, text, now_rfc3339()],
                )?;
            }
            self.reindex(&m)?;
            self.overlay_for_content(&m, &sha)
        })
    }

    /// Back to the words the transcript has ("원래대로").
    pub fn reset_turn(&self, version_id: &str, turn_id: &str) -> Result<Overlay> {
        self.immediate(|| {
            let (m, sha) = self.transcript_of(version_id)?;
            if !self.version(version_id)?.turns.iter().any(|t| t.id == turn_id) {
                return Err(Error::NotFound(format!("turn {turn_id} in {version_id}")));
            }
            self.conn.execute(
                "DELETE FROM turn_edits WHERE meeting_id=?1 AND content_sha256=?2 AND turn_id=?3",
                params![m, sha, turn_id],
            )?;
            self.reindex(&m)?;
            self.overlay_for_content(&m, &sha)
        })
    }

    /// Rebuild the meeting's search row (its head may show edited text). Inside the caller's transaction.
    fn reindex(&self, meeting_id: &str) -> Result<()> {
        let row: Option<i64> = self.conn.query_row("SELECT id FROM search_docs WHERE meeting_id=?1", [meeting_id], |r| r.get(0)).optional()?;
        if let Some(id) = row {
            self.conn.execute("DELETE FROM search_fts WHERE rowid=?1", [id])?;
            self.conn.execute("DELETE FROM search_docs WHERE id=?1", [id])?;
        }
        self.index_head(meeting_id, None)?;
        Ok(())
    }

    /// Contents of the meeting's transcripts that first appeared before the one of `version_id`,
    /// newest last, each with the newest version holding it.
    fn earlier_transcripts(&self, version_id: &str) -> Result<(String, Vec<(String, String)>)> {
        let (m, sha) = self.transcript_of(version_id)?;
        let mut st = self.conn.prepare("SELECT id, content_sha256 FROM versions WHERE meeting_id=?1 ORDER BY seq")?;
        let rows: Vec<(String, String)> = st.query_map([&m], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        let mut first: Vec<String> = Vec::new(); // contents in order of first appearance
        let mut newest: BTreeMap<String, String> = BTreeMap::new();
        for (id, c) in rows {
            if !first.contains(&c) {
                first.push(c.clone());
            }
            newest.insert(c, id);
        }
        // the head's own transcript is never "earlier": a draft A, B, A sequence makes A the head
        // again, and its names and edits are the ones on screen, whichever version asks
        let head: Option<String> = self
            .conn
            .query_row(&format!("SELECT content_sha256 FROM versions WHERE meeting_id=?1 {}", super::search::HEAD_ORDER), [&m], |r| r.get(0))
            .optional()?;
        let mine = first.iter().position(|c| *c == sha).unwrap_or(first.len());
        let earlier = first[..mine]
            .iter()
            .filter(|c| head.as_deref() != Some(c.as_str()))
            .map(|c| (c.clone(), newest[c].clone()))
            .collect();
        Ok((m, earlier))
    }

    /// Names and edits made on earlier transcripts of the meeting that `version_id` does not show
    /// (a new transcript has new speaker and turn ids). The app says so once, quietly.
    pub fn overlays_set_aside(&self, version_id: &str) -> Result<SetAside> {
        let (m, earlier) = self.earlier_transcripts(version_id)?;
        let mut out = SetAside::default();
        for (c, v) in &earlier {
            let n = |table: &str| -> Result<usize> {
                let n: i64 = self.conn.query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE meeting_id=?1 AND content_sha256=?2"),
                    params![m, c],
                    |r| r.get(0),
                )?;
                Ok(n as usize)
            };
            let (e, s) = (n("turn_edits")?, n("speaker_overlays")?);
            if e + s > 0 {
                out.edits += e;
                out.speakers += s;
                out.version_id = Some(v.clone());
            }
        }
        Ok(out)
    }

    /// Forget the overlays `overlays_set_aside` reports for `version_id`. Returns how many rows went.
    pub fn discard_set_aside(&self, version_id: &str) -> Result<usize> {
        self.immediate(|| {
            let (m, earlier) = self.earlier_transcripts(version_id)?;
            let mut n = 0;
            for (c, _) in &earlier {
                n += self.conn.execute("DELETE FROM turn_edits WHERE meeting_id=?1 AND content_sha256=?2", params![m, c])?;
                n += self.conn.execute("DELETE FROM speaker_overlays WHERE meeting_id=?1 AND content_sha256=?2", params![m, c])?;
            }
            self.reindex(&m)?; // the search row follows whatever the head shows
            Ok(n)
        })
    }
}
