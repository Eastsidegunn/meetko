//! Speaker names, merges and edited turns: overlays beside immutable versions (migration 6).

use std::path::{Path, PathBuf};

use meetko_core::error::Error;
use meetko_core::export;
use meetko_core::overlay::View;
use meetko_core::store::{ImportChannel, ImportOptions, Store};
use meetko_core::contract::MeetingData;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn wav(path: &Path, frames: u32) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    (0..frames).for_each(|i| w.write_sample((i % 100) as i16).unwrap());
    w.finalize().unwrap();
}

fn setup() -> (tempfile::TempDir, Store, String) {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a.wav"), tmp.path().join("b.wav"));
    wav(&a, 16000);
    wav(&b, 16000);
    let s = Store::open(&tmp.path().join("data")).unwrap();
    let ch = |index, role: &str, path: &Path| ImportChannel { index, role: role.into(), label: None, path: path.into(), expected_sha256: None, coverage: None };
    let out = s.import_meeting(vec![ch(0, "mic", &a), ch(1, "loopback", &b)], ImportOptions::default()).unwrap();
    (tmp, s, out.meeting.id)
}

fn draft_for(meeting: &str) -> MeetingData {
    let text = std::fs::read_to_string(root().join("contract/fixtures/meeting01/meeting-data.draft.json")).unwrap();
    let mut md: MeetingData = serde_json::from_str(&text).unwrap();
    md.meeting.id = meeting.into();
    md
}

fn hits(s: &Store, q: &str) -> usize {
    s.search(q).unwrap().meetings.iter().map(|m| m.matches).sum()
}

#[test]
fn names_merges_and_resets_stay_beside_the_version() {
    let (_tmp, s, m) = setup();
    let v = s.create_version(draft_for(&m)).unwrap().version.id;
    let raw = s.version_raw(&v).unwrap();
    let o = s.rename_speaker(&v, "s1", "  민수 ").unwrap();
    assert_eq!(o.speakers["s1"].name.as_deref(), Some("민수"));
    let md = s.version(&v).unwrap();
    assert_eq!(o.speaker_name(&md, "s1"), "민수");
    // "나" is the same person as 민수
    let o = s.merge_speaker(&v, "s0", "s1").unwrap();
    assert_eq!(o.speaker_name(&md, "s0"), "민수");
    // naming a merged speaker names the one it was merged into
    let o = s.rename_speaker(&v, "s0", "김민수").unwrap();
    assert_eq!((o.speaker_name(&md, "s0"), o.speaker_name(&md, "s1")), ("김민수", "김민수"));
    assert!(o.speakers["s0"].name.is_none());
    // a circle is refused
    assert!(matches!(s.merge_speaker(&v, "s1", "s0"), Err(Error::Invalid(_))));
    assert!(matches!(s.merge_speaker(&v, "s1", "s1"), Err(Error::Invalid(_))));
    assert!(matches!(s.rename_speaker(&v, "s9", "x"), Err(Error::NotFound(_))));
    assert!(matches!(s.rename_speaker(&v, "s1", " \n"), Err(Error::Invalid(_))));
    // reset: s0 shows its own label again, s1 keeps its name
    let o = s.reset_speaker(&v, "s0").unwrap();
    assert_eq!((o.speaker_name(&md, "s0"), o.speaker_name(&md, "s1")), ("나", "김민수"));
    // giving a speaker its own label back is no name at all
    let o = s.rename_speaker(&v, "s1", "상대 1").unwrap();
    assert!(o.is_empty(), "{o:?}");
    assert_eq!(s.version_raw(&v).unwrap(), raw, "the stored version is never touched");
}

#[test]
fn merging_keeps_chains_one_step_deep() {
    let (_tmp, s, m) = setup();
    let mut md = draft_for(&m);
    // a third speaker on the remote channel
    let mut extra = md.speakers[1].clone();
    extra.id = "s2".into();
    extra.label = Some("상대 2".into());
    md.speakers.push(extra);
    let v = s.create_version(md).unwrap().version.id;
    s.merge_speaker(&v, "s2", "s1").unwrap();
    let o = s.merge_speaker(&v, "s1", "s0").unwrap();
    assert_eq!(o.speakers["s2"].merged_into.as_deref(), Some("s0"), "s2 follows s1 into s0");
    assert_eq!(o.speakers["s1"].merged_into.as_deref(), Some("s0"));
}

#[test]
fn an_edit_is_shown_exported_and_searched_and_can_be_undone() {
    let (_tmp, s, m) = setup();
    let v = s.create_version(draft_for(&m)).unwrap().version.id;
    let raw = s.version_raw(&v).unwrap();
    assert_eq!(hits(&s, "게이트웨이에서"), 1);
    let o = s.edit_turn(&v, "t0000", "지난주 장애 회고부터 할게요.\n프록시에서 타임아웃이 났어요.").unwrap();
    assert_eq!(o.edits["t0000"].text, "지난주 장애 회고부터 할게요. 프록시에서 타임아웃이 났어요.");
    assert_eq!(s.version_raw(&v).unwrap(), raw, "the words stay stored");
    // search finds what is shown, and anchors the hit at the turn
    assert_eq!(hits(&s, "게이트웨이에서"), 0);
    let found = s.search("프록시에서").unwrap();
    let h = &found.meetings[0].hits[0];
    // the anchor is the whole edited turn, and the hit says it is edited text
    assert_eq!((h.from.as_str(), h.to.as_str(), h.start_ms, h.end_ms, h.edited), ("w00000", "w00008", 700, 6740, true));
    assert!(found.meetings[0].hits.iter().all(|h| h.edited));
    // a hit in words nobody edited is not marked
    assert!(!s.search("marketing").unwrap().meetings[0].hits[0].edited);
    // exports read the overlay
    let md = s.version(&v).unwrap();
    let txt = export::turns_text(&md, "?", false, true, &View::of(&o)).unwrap();
    assert!(txt.starts_with("상대 1: 지난주 장애 회고부터 할게요. 프록시에서"), "{txt}");
    // text equal to the words is no edit
    let original = export::turns_text(&md, "?", false, false, &View::plain()).unwrap().lines().next().unwrap().to_string();
    assert!(s.edit_turn(&v, "t0000", &original).unwrap().edits.is_empty());
    s.edit_turn(&v, "t0000", "다시 고침").unwrap();
    assert_eq!(hits(&s, "다시 고침"), 1);
    // 원래대로
    assert!(s.reset_turn(&v, "t0000").unwrap().edits.is_empty());
    assert_eq!((hits(&s, "다시 고침"), hits(&s, "게이트웨이에서")), (0, 1));
    assert!(matches!(s.edit_turn(&v, "t9999", "x"), Err(Error::NotFound(_))));
    assert!(matches!(s.reset_turn(&v, "t9999"), Err(Error::NotFound(_))));
    assert!(matches!(s.edit_turn(&v, "t0000", "   "), Err(Error::Invalid(_))));
}

#[test]
fn the_heads_own_transcript_is_never_set_aside_and_discarding_reindexes() {
    let (_tmp, s, m) = setup();
    let other = |md: &mut MeetingData| md.words[0].text = "지지난주".into();
    // draft A, then B, then A again: A is the head
    let a1 = s.create_version(draft_for(&m)).unwrap().version.id;
    s.edit_turn(&a1, "t0001", "에이에서 고친 문장").unwrap();
    let mut md = draft_for(&m);
    other(&mut md);
    let b = s.create_version(md).unwrap().version.id;
    s.edit_turn(&b, "t0001", "비에서 고친 문장").unwrap();
    let a2 = s.create_version(draft_for(&m)).unwrap().version.id;
    assert_eq!(s.overlay(&a2).unwrap().edits["t0001"].text, "에이에서 고친 문장", "A's edits are back with A");
    assert_eq!(hits(&s, "에이에서"), 1);
    // from B's side A came first, but A is the head: it is not B's to discard
    assert_eq!(s.overlays_set_aside(&b).unwrap(), Default::default());
    assert_eq!(s.discard_set_aside(&b).unwrap(), 0);
    assert_eq!(s.overlay(&a2).unwrap().edits.len(), 1);
    // from the head nothing is earlier than A; B's are newer and are not offered either
    assert_eq!(s.overlays_set_aside(&a2).unwrap(), Default::default());
    // B again, approved: now A's edits are set aside from B, and discarding them reindexes the head (B)
    let mut md = draft_for(&m);
    other(&mut md);
    let b2 = s.create_version(md).unwrap().version.id;
    let ab = s.approve(&b2).unwrap().version.id;
    assert_eq!(hits(&s, "비에서"), 1);
    let aside = s.overlays_set_aside(&ab).unwrap();
    assert_eq!((aside.edits, aside.version_id.as_deref()), (1, Some(a2.as_str())));
    assert_eq!(s.discard_set_aside(&ab).unwrap(), 1);
    assert!(s.overlay(&a2).unwrap().is_empty());
    assert_eq!((hits(&s, "비에서"), hits(&s, "에이에서")), (1, 0));
}

#[test]
fn approving_keeps_the_overlays_a_new_transcript_sets_them_aside() {
    let (_tmp, s, m) = setup();
    let d1 = s.create_version(draft_for(&m)).unwrap().version.id;
    s.rename_speaker(&d1, "s1", "민수").unwrap();
    s.edit_turn(&d1, "t0001", "고친 문장").unwrap();
    // the approved copy has the same content: same names and edits
    let a1 = s.approve(&d1).unwrap().version.id;
    let o = s.overlay(&a1).unwrap();
    assert_eq!((o.speakers.len(), o.edits.len()), (1, 1));
    assert_eq!(s.overlays_set_aside(&a1).unwrap().edits, 0);
    // a new transcript (다시 받아쓰기): other words, so other content
    let mut md = draft_for(&m);
    md.words[0].text = "지지난주".into();
    let d2 = s.create_version(md).unwrap().version.id;
    assert!(s.overlay(&d2).unwrap().is_empty(), "ids of another transcript are not the same people or sentences");
    // a newer draft does not count as earlier from the approved version's side
    assert_eq!(s.overlays_set_aside(&a1).unwrap(), Default::default());
    // nor is the approved one (the head) set aside from the new draft's side: it is what is current
    assert_eq!(s.overlays_set_aside(&d2).unwrap(), Default::default());
    // once the new transcript is the head, search shows its own words, not the old edit
    let a2 = s.approve(&d2).unwrap().version.id;
    assert_eq!((hits(&s, "고친 문장"), hits(&s, "지지난주")), (0, 1));
    let aside = s.overlays_set_aside(&a2).unwrap();
    assert_eq!((aside.edits, aside.speakers, aside.version_id.as_deref()), (1, 1, Some(a1.as_str())));
    // the earlier version still shows them until they are discarded
    assert_eq!(s.overlay(&a1).unwrap().edits.len(), 1);
    assert_eq!(s.discard_set_aside(&a2).unwrap(), 2);
    assert!(s.overlay(&a1).unwrap().is_empty());
    assert_eq!(s.overlays_set_aside(&a2).unwrap(), Default::default());
}

#[test]
fn migration_6_adds_the_overlay_tables_to_a_version_5_database() {
    let (tmp, s, m) = setup();
    let v = s.create_version(draft_for(&m)).unwrap().version.id;
    drop(s);
    let db = tmp.path().join("data/meetko.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("DROP TABLE speaker_overlays; DROP TABLE turn_edits; PRAGMA user_version = 5;").unwrap();
    }
    let s = Store::open(&tmp.path().join("data")).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    let ver: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(ver, 6);
    assert!(s.overlay(&v).unwrap().is_empty());
    s.edit_turn(&v, "t0000", "새 문장").unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM turn_edits", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}
