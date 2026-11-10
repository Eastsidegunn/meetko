use std::sync::{Arc, Barrier};

use super::*;
use crate::error::Error;

fn wav(path: &Path, frames: u32) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    (0..frames).for_each(|i| w.write_sample((i % 100) as i16).unwrap());
    w.finalize().unwrap();
}

fn setup() -> (tempfile::TempDir, Store, String) {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a.wav"), tmp.path().join("b.wav"));
    wav(&a, 70_000);
    wav(&b, 70_000);
    let s = Store::open(&tmp.path().join("data")).unwrap();
    let ch = |index, role: &str, path: &Path| ImportChannel { index, role: role.into(), label: None, path: path.into(), expected_sha256: None, coverage: None };
    let out = s.import_meeting(vec![ch(0, "mic", &a), ch(1, "loopback", &b)], ImportOptions::default()).unwrap();
    (tmp, s, out.meeting.id)
}

/// the committed draft fixture, re-homed to `meeting`
fn draft_for(meeting: &str) -> MeetingData {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/fixtures/meeting01/meeting-data.draft.json");
    let mut md: MeetingData = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    md.meeting.id = meeting.into();
    md
}

fn approved_count(s: &Store, m: &str) -> usize {
    s.versions_of(m).unwrap().iter().filter(|v| v.kind == VersionKind::Approved).count()
}

#[test]
fn duration_is_floored_like_the_capture_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("a.wav");
    wav(&p, 16_009); // 1000.56 ms: rounding would say 1001
    assert_eq!(wav_info(&p).unwrap().1, 1000);
}

#[test]
fn concurrent_approves_of_one_draft_make_one_version() {
    let (tmp, s, m) = setup();
    let draft = s.create_version(draft_for(&m)).unwrap().version.id;
    let data = tmp.path().join("data");
    let gate = Arc::new(Barrier::new(4));
    let hs: Vec<_> = (0..4)
        .map(|_| {
            let (data, draft, gate) = (data.clone(), draft.clone(), gate.clone());
            std::thread::spawn(move || {
                let s = Store::open(&data).unwrap(); // its own connection
                gate.wait();
                s.approve(&draft).unwrap().version.id
            })
        })
        .collect();
    let ids: std::collections::BTreeSet<String> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(ids.len(), 1, "every caller gets the same approved version: {ids:?}");
    assert_eq!(approved_count(&s, &m), 1);
    assert_eq!(s.approve(&draft).unwrap().version.id, *ids.iter().next().unwrap(), "approving again is a no-op");
}

#[test]
fn approving_an_older_draft_cannot_roll_the_head_back() {
    let (_tmp, s, m) = setup();
    let old = s.create_version(draft_for(&m)).unwrap().version.id;
    let mut changed = draft_for(&m);
    changed.words[0].text = "바뀐".into();
    let new = s.create_version(changed).unwrap().version.id;
    s.approve(&new).unwrap();
    let err = s.approve(&old).expect_err("must be refused");
    assert!(matches!(err, Error::StaleDraft(_)), "{err:?}");
    assert_eq!(err.code(), "stale_draft");
    assert_eq!(approved_count(&s, &m), 1, "nothing was stored");
    // a draft made after the approval is fine
    let mut later = draft_for(&m);
    later.words[0].text = "또".into();
    let later = s.create_version(later).unwrap().version.id;
    s.approve(&later).unwrap();
}

/// 8 racers on a fresh dir, many rounds: none may fail with `database is locked`, and the
/// loser of the creation race must not wipe the winner's database.
#[test]
fn a_fresh_data_dir_opens_concurrently() {
    let tmp = tempfile::tempdir().unwrap();
    for round in 0..100 {
        let dir = tmp.path().join(format!("d{round}"));
        let gate = Arc::new(Barrier::new(8));
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let (dir, gate) = (dir.clone(), gate.clone());
                std::thread::spawn(move || {
                    gate.wait();
                    Store::open(&dir).map(|_| ()).map_err(|e| e.to_string())
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap().unwrap_or_else(|e| panic!("round {round}: {e}"));
        }
    }
}

#[test]
fn opening_never_truncates_an_existing_database() {
    let (tmp, s, m) = setup();
    drop(s);
    let again = Store::open(&tmp.path().join("data")).unwrap();
    assert_eq!(again.list_meetings().unwrap().len(), 1, "{m}");
}

#[cfg(unix)]
#[test]
fn tighten_private_dir_makes_a_loose_dir_private() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    util::tighten_private_dir(&dir).unwrap();
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
}
