//! Full-text search over each meeting's head version (migration 4).

use std::path::{Path, PathBuf};

use meetko_core::contract::{MeetingData, VersionKind};
use meetko_core::store::search::{self, SearchResult};
use meetko_core::store::{ImportChannel, ImportOptions, Store};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn wav(path: &Path, frames: u32, seed: i16) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    (0..frames).for_each(|i| w.write_sample((i % 100) as i16 + seed).unwrap());
    w.finalize().unwrap();
}

/// a new meeting with its own recording (`seed` keeps the recording hashes apart)
fn meeting(tmp: &Path, s: &Store, seed: i16, title: &str) -> String {
    let (a, b) = (tmp.join(format!("a{seed}.wav")), tmp.join(format!("b{seed}.wav")));
    wav(&a, 16000, seed);
    wav(&b, 16000, seed);
    let ch = |index, role: &str, path: &Path| ImportChannel { index, role: role.into(), label: None, path: path.into(), expected_sha256: None, coverage: None };
    let opts = ImportOptions { title: Some(title.into()), started_at: Some(format!("2026-11-01T{:02}:{:02}:00Z", seed / 60, seed % 60)), ..Default::default() };
    s.import_meeting(vec![ch(0, "mic", &a), ch(1, "loopback", &b)], opts).unwrap().meeting.id
}

fn draft_for(meeting: &str) -> MeetingData {
    let text = std::fs::read_to_string(root().join("contract/fixtures/meeting01/meeting-data.draft.json")).unwrap();
    let mut md: MeetingData = serde_json::from_str(&text).unwrap();
    md.meeting.id = meeting.into();
    md
}

/// the fixture with a few words replaced
fn with_words(meeting: &str, kind: VersionKind, words: &[(usize, &str)]) -> MeetingData {
    let mut md = draft_for(meeting);
    md.version.kind = kind;
    for (i, t) in words {
        md.words[*i].text = (*t).into();
    }
    md
}

fn open() -> (tempfile::TempDir, Store) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(&tmp.path().join("data")).unwrap();
    (tmp, s)
}

#[test]
fn korean_particles_are_found_by_the_stem() {
    let (tmp, s) = open();
    let m = meeting(tmp.path(), &s, 0, "주간 회의");
    s.create_version(with_words(&m, VersionKind::Draft, &[(0, "회의를"), (3, "회의는")])).unwrap();
    let r = find(&s, "회의").unwrap(); // 2 chars: the LIKE path
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].matches, 2);
    let md = s.version(&r[0].version_id).unwrap();
    assert_eq!(r[0].hits[0].from, md.words[0].id);
    assert_eq!(r[0].hits[0].text, "회의");
    assert_eq!(r[0].hits[0].after.chars().next(), Some('를'));
    assert_eq!(r[0].hits[1].from, md.words[3].id);
    assert_eq!(r[0].hits[1].start_ms, md.words[3].start_ms);
    // 3 chars: the trigram path, inside a word with a particle
    let r = find(&s, "인시던").unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].hits[0].from, md.words[1].id);
    assert_eq!(r[0].title.as_deref(), Some("주간 회의"));
}

#[test]
fn mixed_korean_and_english_across_words_case_insensitive() {
    let (tmp, s) = open();
    let m = meeting(tmp.path(), &s, 0, "t");
    let md = s.create_version(with_words(&m, VersionKind::Draft, &[(4, "API"), (5, "서버가")])).unwrap();
    for q in ["API 서버", "api 서버", "  api   서버 "] {
        let r = find(&s, q).unwrap();
        assert_eq!(r.len(), 1, "{q:?}");
        let h = &r[0].hits[0];
        assert_eq!((h.from.as_str(), h.to.as_str()), (md.words[4].id.as_str(), md.words[5].id.as_str()), "{q:?}");
        assert_eq!(h.text, "API 서버");
        assert_eq!(h.start_ms, md.words[4].start_ms);
        assert_eq!(h.end_ms, md.words[5].end_ms);
    }
    assert!(find(&s, "MARKETING team").unwrap().len() == 1);
    assert!(find(&s, "없는말입니다").unwrap().is_empty());
}

#[test]
fn short_queries_and_empty_input() {
    let (tmp, s) = open();
    let m = meeting(tmp.path(), &s, 0, "t");
    s.create_version(with_words(&m, VersionKind::Draft, &[(0, "풀"), (1, "x")])).unwrap();
    assert!(find(&s, "").unwrap().is_empty());
    assert!(find(&s, "   ").unwrap().is_empty());
    assert_eq!(find(&s, "풀").unwrap().len(), 1);
    assert_eq!(find(&s, "X").unwrap().len(), 1);
}

#[test]
fn fts_and_like_syntax_is_literal_and_never_errors() {
    let (tmp, s) = open();
    let m = meeting(tmp.path(), &s, 0, "t");
    s.create_version(with_words(&m, VersionKind::Draft, &[(0, "50%_할인"), (1, "\"따옴표\""), (2, "a\\b")])).unwrap();
    for q in ["\"", "\"\"", "*", "OR", "NOT x", "body:API", "NEAR(a b)", "a\" OR \"b", "(", "^", "%", "_", "\\", "'; DROP TABLE meetings; --", "%%%"] {
        find(&s, q).unwrap_or_else(|e| panic!("{q:?}: {e}"));
    }
    assert!(find(&s, "%").unwrap().len() == 1, "a literal percent sign");
    assert!(find(&s, "_").unwrap().len() == 1);
    assert!(find(&s, "%_할").unwrap().len() == 1);
    assert!(find(&s, "\"따옴").unwrap().len() == 1);
    assert!(find(&s, "a\\b").unwrap().len() == 1);
    assert!(find(&s, "NOT x").unwrap().is_empty());
    let long = "가".repeat(10_000);
    assert!(find(&s, &long).unwrap().is_empty());
    assert_eq!(s.list_meetings().unwrap().len(), 1);
}

#[test]
fn the_index_follows_the_head_version() {
    let (tmp, s) = open();
    let m = meeting(tmp.path(), &s, 0, "t");
    let live = s.create_version(with_words(&m, VersionKind::Live, &[(0, "실시간자막")])).unwrap();
    let r = find(&s, "실시간자막").unwrap();
    assert_eq!((r[0].version_id.as_str(), r[0].kind.as_str()), (live.version.id.as_str(), "live"), "only live: live is the head");
    let d1 = s.create_version(with_words(&m, VersionKind::Draft, &[(0, "첫초안")])).unwrap();
    assert!(find(&s, "실시간자막").unwrap().is_empty(), "a draft replaces live");
    assert_eq!(find(&s, "첫초안").unwrap()[0].version_id, d1.version.id);
    let a = s.approve(&d1.version.id).unwrap();
    let r = find(&s, "첫초안").unwrap();
    assert_eq!((r[0].version_id.as_str(), r[0].kind.as_str()), (a.version.id.as_str(), "approved"));
    // a newer draft does not replace an approved head
    s.create_version(with_words(&m, VersionKind::Draft, &[(0, "두번째초안")])).unwrap();
    assert!(find(&s, "두번째초안").unwrap().is_empty());
    assert_eq!(find(&s, "첫초안").unwrap()[0].version_id, a.version.id);
    let d2 = s.versions_of(&m).unwrap().last().unwrap().id.clone();
    s.approve(&d2).unwrap();
    assert!(find(&s, "첫초안").unwrap().is_empty());
    assert_eq!(find(&s, "두번째초안").unwrap().len(), 1);
}

#[test]
fn results_are_per_meeting_newest_first() {
    let (tmp, s) = open();
    let a = meeting(tmp.path(), &s, 0, "먼저");
    s.create_version(with_words(&a, VersionKind::Draft, &[(0, "배포")])).unwrap();
    let b = meeting(tmp.path(), &s, 1, "나중");
    s.create_version(with_words(&b, VersionKind::Draft, &[(0, "배포"), (1, "배포")])).unwrap();
    let c = meeting(tmp.path(), &s, 2, "전사 없음");
    let r = find(&s, "배포").unwrap();
    assert_eq!(r.iter().map(|x| x.meeting_id.as_str()).collect::<Vec<_>>(), [b.as_str(), a.as_str()]);
    assert_eq!(r[0].matches, 2);
    assert!(r.iter().all(|x| x.meeting_id != c));
}

#[test]
fn a_version_3_database_is_backfilled_on_open() {
    let (tmp, s) = open();
    let m = meeting(tmp.path(), &s, 0, "t");
    let d = s.create_version(with_words(&m, VersionKind::Draft, &[(0, "백필확인")])).unwrap();
    s.approve(&d.version.id).unwrap();
    drop(s);
    // make it look like a database written by a build that knew only migrations 1-3
    let db = tmp.path().join("data/meetko.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("DROP TABLE search_docs; DROP TABLE search_fts; PRAGMA user_version = 3;").unwrap();
    }
    let s = Store::open(&tmp.path().join("data")).unwrap();
    let r = find(&s, "백필확인").unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].kind, "approved");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(v, 4);
    drop(s);
    Store::open(&tmp.path().join("data")).unwrap(); // reopening migrates nothing
}

fn find(s: &Store, q: &str) -> meetko_core::Result<Vec<SearchResult>> {
    s.search(q).map(|o| o.meetings)
}

/// The fixture repeated `reps` times back to back (70 s each), renumbered: a long meeting.
fn long_meeting(meeting: &str, reps: usize) -> MeetingData {
    let base = draft_for(meeting);
    const SPAN: i64 = 70_000;
    let mut md = base.clone();
    md.words.clear();
    md.turns.clear();
    md.speaker_spans.clear();
    let n = base.words.len();
    let wid = |k: usize, id: &str| format!("w{:07}", k * n + id[1..].parse::<usize>().unwrap());
    for k in 0..reps {
        let dt = SPAN * k as i64;
        for w in &base.words {
            let mut w = w.clone();
            w.id = wid(k, &w.id);
            (w.start_ms, w.end_ms) = (w.start_ms + dt, w.end_ms + dt);
            md.words.push(w);
        }
        for (i, t) in base.turns.iter().enumerate() {
            let mut t = t.clone();
            t.id = format!("t{:06}", k * base.turns.len() + i);
            (t.first_word, t.last_word) = (wid(k, &t.first_word), wid(k, &t.last_word));
            (t.start_ms, t.end_ms) = (t.start_ms + dt, t.end_ms + dt);
            md.turns.push(t);
        }
        for s in &base.speaker_spans {
            let mut s = s.clone();
            (s.start_ms, s.end_ms) = (s.start_ms + dt, s.end_ms + dt);
            md.speaker_spans.push(s);
        }
    }
    let total = SPAN * reps as i64;
    (md.meeting.duration_ms, md.recording.duration_ms) = (total, total);
    for c in &mut md.coverage {
        c.end_ms += SPAN * (reps as i64 - 1);
    }
    md
}

/// Simulate a database written before migration 4: the search tables are gone.
fn back_to_v3(data: &Path) {
    let conn = rusqlite::Connection::open(data.join("meetko.db")).unwrap();
    conn.execute_batch("DROP TABLE search_docs; DROP TABLE search_fts; PRAGMA user_version = 3;").unwrap();
}

#[test]
fn nul_hidden_characters_and_non_ascii_case_on_the_short_path() {
    let (tmp, s) = open();
    let m = meeting(tmp.path(), &s, 0, "t");
    s.create_version(with_words(&m, VersionKind::Draft, &[(0, "Δx"), (1, "ÉCOLE"), (2, "İstanbul")])).unwrap();
    for q in ["\0", "a\0b", "\0\0\0", "\u{200b}", "\u{202e}x"] {
        find(&s, q).unwrap_or_else(|e| panic!("{q:?}: {e}"));
    }
    assert_eq!(find(&s, "δx").unwrap().len(), 1, "2 chars, non-ASCII case");
    assert_eq!(find(&s, "éc").unwrap().len(), 1);
    assert_eq!(find(&s, "éco").unwrap().len(), 1, "3 chars: the FTS path agrees");
    assert_eq!(find(&s, "ISTANBUL").unwrap().len(), 1);
    assert_eq!(find(&s, "Δ\0x").unwrap().len(), 1, "a NUL inside the query is dropped");
    let h = &find(&s, "éc").unwrap()[0].hits[0];
    assert_eq!(h.text, "ÉC");
}

#[test]
fn the_meeting_cap_is_reported_and_newest_are_kept() {
    let (tmp, s) = open();
    let n = search::MAX_MEETINGS as i16 + 2;
    let mut ids = vec![];
    for seed in 0..n {
        let m = meeting(tmp.path(), &s, seed, "t");
        s.create_version(with_words(&m, VersionKind::Draft, &[(0, "공통어")])).unwrap();
        ids.push(m);
    }
    let o = s.search("공통어").unwrap();
    assert!(o.more);
    assert_eq!(o.meetings.len(), search::MAX_MEETINGS);
    assert_eq!(o.meetings[0].meeting_id, ids[n as usize - 1], "newest first");
    let o = s.search("공통").unwrap(); // the short path is capped the same way
    assert!(o.more && o.meetings.len() == search::MAX_MEETINGS);
}

#[test]
fn an_unreadable_head_is_skipped_with_a_warning_and_does_not_block_writes() {
    let (tmp, s) = open();
    let good = meeting(tmp.path(), &s, 0, "good");
    s.create_version(with_words(&good, VersionKind::Draft, &[(0, "멀쩡한")])).unwrap();
    let bad = meeting(tmp.path(), &s, 1, "bad");
    let v = s.create_version(with_words(&bad, VersionKind::Draft, &[(0, "멀쩡한")])).unwrap().version.id;
    drop(s);
    let data = tmp.path().join("data");
    {
        let conn = rusqlite::Connection::open(data.join("meetko.db")).unwrap();
        conn.execute_batch("DROP TRIGGER versions_no_update").unwrap();
        conn.execute("UPDATE versions SET json='{\"broken\"' WHERE id=?1", [&v]).unwrap();
    }
    back_to_v3(&data);
    let s = Store::open(&data).expect("an unreadable version must not fail the open");
    let lines: Vec<_> = s.warnings().iter().filter(|w| w.contains("search")).collect();
    assert_eq!(lines.len(), 1, "one line: {lines:?}");
    assert!(lines[0].contains(&bad) && !lines[0].contains('\n'), "{lines:?}");
    assert_eq!(s.index_pending(None).unwrap(), 0, "remembered, not retried forever");
    let o = s.search("멀쩡한").unwrap();
    assert_eq!(o.meetings.iter().map(|m| m.meeting_id.as_str()).collect::<Vec<_>>(), [good.as_str()]);
    assert_eq!(o.skipped, 1, "the app can say a meeting was left out");
    drop(s);
    let s = Store::open(&data).unwrap();
    assert!(s.warnings().iter().any(|w| w.contains(&bad)), "told again at every open");
    // a new version of that meeting is stored and indexed as usual
    s.create_version(with_words(&bad, VersionKind::Draft, &[(0, "고친")])).unwrap();
    assert_eq!(find(&s, "고친").unwrap().len(), 1);
}

#[test]
fn the_backfill_is_incremental_and_reported_as_pending() {
    let (tmp, s) = open();
    for seed in 0..3 {
        let m = meeting(tmp.path(), &s, seed, "t");
        s.create_version(with_words(&m, VersionKind::Draft, &[(0, "나중에")])).unwrap();
    }
    drop(s);
    let data = tmp.path().join("data");
    back_to_v3(&data);
    let s = Store::open(&data).unwrap();
    let conn = rusqlite::Connection::open(data.join("meetko.db")).unwrap();
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(v, 4, "the schema is committed first");
    // empty the rows the open may have filled: the next steps fill them one meeting at a time
    conn.execute_batch("DELETE FROM search_fts; DELETE FROM search_docs;").unwrap();
    assert_eq!(s.index_pending(Some(std::time::Duration::ZERO)).unwrap(), 3);
    let o = s.search("나중에").unwrap();
    assert_eq!((o.meetings.len(), o.pending), (3, 0));
}

#[test]
fn a_damaged_word_map_costs_only_its_own_meeting_and_is_rebuilt() {
    let (tmp, s) = open();
    let a = meeting(tmp.path(), &s, 0, "a");
    s.create_version(with_words(&a, VersionKind::Draft, &[(0, "손상된")])).unwrap();
    let b = meeting(tmp.path(), &s, 1, "b");
    s.create_version(with_words(&b, VersionKind::Draft, &[(0, "손상된")])).unwrap();
    let conn = rusqlite::Connection::open(tmp.path().join("data/meetko.db")).unwrap();
    // claims a million words in four bytes
    conn.execute("UPDATE search_docs SET word_map = x'40420f00' WHERE meeting_id = ?1", [&a]).unwrap();
    let o = s.search("손상된").unwrap();
    assert_eq!(o.meetings.iter().map(|m| m.meeting_id.as_str()).collect::<Vec<_>>(), [b.as_str()]);
    assert_eq!(o.pending, 1, "the damaged row waits to be built again");
    let o = s.search("손상된").unwrap();
    assert_eq!((o.meetings.len(), o.pending), (2, 0), "rebuilt from its version");
}

/// Another writer holds the lock while meetings wait to be indexed: the search answers from
/// what is indexed (after the busy timeout) instead of failing.
#[test]
fn a_busy_database_does_not_fail_the_search() {
    let (tmp, s) = open();
    for (seed, word) in [(0, "잠금"), (1, "잠금")] {
        let m = meeting(tmp.path(), &s, seed, "t");
        s.create_version(with_words(&m, VersionKind::Draft, &[(0, word)])).unwrap();
    }
    let conn = rusqlite::Connection::open(tmp.path().join("data/meetko.db")).unwrap();
    conn.execute_batch("DELETE FROM search_fts WHERE rowid = (SELECT MIN(id) FROM search_docs); DELETE FROM search_docs WHERE id = (SELECT MIN(id) FROM search_docs);").unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let o = s.search("잠금").expect("a busy database must not fail the search");
    assert_eq!((o.meetings.len(), o.pending), (1, 1));
    conn.execute_batch("ROLLBACK").unwrap();
    assert_eq!(s.search("잠금").unwrap().meetings.len(), 2);
}

/// The search index must cost a write the same however large the index already is: lookups go
/// through keys and rowids, never a scan of the stored texts. A coarse end-to-end bound at CI size;
/// the sensitive guards are the unit tests in `store/search.rs` (query plans, `index_head` alone).
#[test]
fn index_writes_do_not_grow_with_the_index() {
    let (tmp, s) = open();
    let probe = meeting(tmp.path(), &s, 0, "probe");
    let write = |s: &Store, i: usize| {
        let t0 = std::time::Instant::now();
        s.create_version(with_words(&probe, VersionKind::Draft, &[(0, &format!("측정{i}"))])).unwrap();
        t0.elapsed()
    };
    let median = |s: &Store, base: usize| {
        let mut t: Vec<_> = (0..9).map(|i| write(s, base + i)).collect();
        t.sort();
        t[4]
    };
    let big = long_meeting("x", 20); // ~2,200 words, ~23 minutes
    let fill = |s: &Store, from: i16, to: i16| {
        for seed in from..to {
            let mut md = big.clone();
            md.meeting.id = meeting(tmp.path(), s, seed, "long");
            s.create_version(md).unwrap();
        }
    };
    fill(&s, 1, 2);
    let small = median(&s, 0);
    fill(&s, 2, 41);
    let large = median(&s, 100);
    // loose on purpose (debug build, shared CI runners); the query-plan unit tests are the real guard
    assert!(large < small * 10 + std::time::Duration::from_millis(50), "a write took {small:?} with 1 long meeting indexed, {large:?} with 40");
    // a full backfill of all 41 meetings, outside any one long transaction
    drop(s);
    let data = tmp.path().join("data");
    back_to_v3(&data);
    let s = Store::open(&data).unwrap();
    let t0 = std::time::Instant::now();
    assert_eq!(s.index_pending(None).unwrap(), 0);
    let all = t0.elapsed();
    assert!(all < std::time::Duration::from_secs(20), "backfill of 41 meetings took {all:?}");
    assert_eq!(find(&s, "인시던트").unwrap().len(), 41, "the probe says it too");
}

/// Timings for the review, not run in CI:
/// `cargo test --release -p meetko-core --test search bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench() {
    let n: i16 = std::env::var("BENCH_MEETINGS").ok().and_then(|v| v.parse().ok()).unwrap_or(100);
    let (tmp, s) = open();
    let big = long_meeting("x", 100); // ~11,000 words, ~2 h
    let t0 = std::time::Instant::now();
    for seed in 0..n {
        let mut md = big.clone();
        md.meeting.id = meeting(tmp.path(), &s, seed, "long");
        s.create_version(md).unwrap();
    }
    eprintln!("stored {n} meetings in {:?}", t0.elapsed());
    let probe = meeting(tmp.path(), &s, n, "probe");
    let mut t: Vec<_> = (0..9)
        .map(|i| {
            let t0 = std::time::Instant::now();
            s.create_version(with_words(&probe, VersionKind::Draft, &[(0, &format!("측정{i}"))])).unwrap();
            t0.elapsed()
        })
        .collect();
    t.sort();
    eprintln!("small write with {n} long meetings indexed: median {:?}", t[4]);
    for q in ["인시던트", "풀", "API 게이트웨이"] {
        let t0 = std::time::Instant::now();
        let r = find(&s, q).unwrap();
        eprintln!("search {q:?}: {} meetings in {:?}", r.len(), t0.elapsed());
    }
    drop(s);
    let data = tmp.path().join("data");
    back_to_v3(&data);
    let t0 = std::time::Instant::now();
    let s = Store::open(&data).unwrap();
    let opened = t0.elapsed();
    let left = s.index_pending(None).unwrap();
    eprintln!("v3 open {opened:?}; open + full backfill {:?} (left {left})", t0.elapsed());
}
