//! Storage: immutable versions, stale rules, import safety, plugin pinning.

use std::path::{Path, PathBuf};

use meetko_core::contract::MeetingData;
use meetko_core::error::Error;
use meetko_core::manifest::{plugin_code_sha256, Manifest};
use meetko_core::store::{ImportChannel, ImportOptions, Store};
use meetko_core::{pipeline, util};
use serde_json::{json, Value};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn wav(path: &Path, frames: u32) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    (0..frames).for_each(|i| w.write_sample((i % 100) as i16).unwrap());
    w.finalize().unwrap();
}

fn ch(index: u32, role: &str, path: &Path) -> ImportChannel {
    ImportChannel { index, role: role.into(), label: None, path: path.into(), expected_sha256: None, coverage: None }
}

/// a store with one imported meeting; returns (tmp, store, meeting id)
fn setup(frames: u32) -> (tempfile::TempDir, Store, String) {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a.wav"), tmp.path().join("b.wav"));
    wav(&a, frames);
    wav(&b, frames);
    let s = Store::open(&tmp.path().join("data")).unwrap();
    let out = s.import_meeting(vec![ch(0, "mic", &a), ch(1, "loopback", &b)], ImportOptions::default()).unwrap();
    (tmp, s, out.meeting.id)
}

/// the committed draft fixture, re-homed to `meeting`
fn draft_for(meeting: &str) -> MeetingData {
    let text = std::fs::read_to_string(root().join("contract/fixtures/meeting01/meeting-data.draft.json")).unwrap();
    let mut md: MeetingData = serde_json::from_str(&text).unwrap();
    md.meeting.id = meeting.into();
    md
}

fn add_action_items(s: &Store) -> meetko_core::store::PluginRow {
    let dir = root().join("plugins/action-items-py");
    let (m, msha) = Manifest::load(&dir).unwrap();
    s.add_plugin(&m, &dir, &msha, &plugin_code_sha256(&m, &dir).unwrap()).unwrap()
}

fn empty_output(version_id: &str) -> Value {
    json!({"contract": "0.1", "plugin": {"id": "t", "version": "0"}, "input": {"version_id": version_id}, "items": []})
}

#[test]
fn versions_are_immutable_and_user_version_is_set() {
    let (tmp, s, m) = setup(16000);
    s.create_version(draft_for(&m)).unwrap();
    drop(s);
    let conn = rusqlite::Connection::open(tmp.path().join("data/meetko.db")).unwrap();
    assert!(conn.execute("UPDATE versions SET json='{}'", []).is_err());
    assert!(conn.execute("DELETE FROM versions", []).is_err());
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(v, 4); // 2 = result sandbox, 3 = plugin_sources, 4 = search_index
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode, "wal");
    Store::open(&tmp.path().join("data")).unwrap(); // reopening migrates nothing
}

#[test]
fn identical_approve_is_not_stale_but_changed_content_is() {
    let (_t, s, m) = setup(70_000);
    let draft = s.create_version(draft_for(&m)).unwrap();
    let plugin = add_action_items(&s);
    s.add_result(&plugin, &draft.version.id, &empty_output(&draft.version.id)).unwrap();

    let approved = s.approve(&draft.version.id).unwrap();
    assert_eq!(approved.version.parent.as_deref(), Some(draft.version.id.as_str()));
    assert_ne!(approved.version.id, draft.version.id);
    assert!(s.stale_results(&m).unwrap().is_empty(), "approving identical content must not stale results");

    // a new draft with different words does not change the head while an approved one exists...
    let mut changed = draft_for(&m);
    changed.words[0].text = "바뀐".into();
    s.create_version(changed.clone()).unwrap();
    assert!(s.stale_results(&m).unwrap().is_empty());
    // ...but approving it does
    let d2 = s.versions_of(&m).unwrap().into_iter().rev().find(|v| v.kind == meetko_core::contract::VersionKind::Draft).unwrap();
    s.approve(&d2.id).unwrap();
    let stale = s.stale_results(&m).unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].version_id, draft.version.id);
}

#[test]
fn approve_keeps_every_stored_byte_outside_version() {
    let (tmp, s, m) = setup(70_000);
    // a version that carries a field this build does not model, inserted as a newer core might
    let mut raw = draft_for(&m).to_value();
    raw["version"]["id"] = json!(format!("{m}.draft.1"));
    raw["future_branch"] = json!({"anything": [1, 2]});
    raw["words"][0]["lang"] = json!("ko");
    {
        let conn = rusqlite::Connection::open(tmp.path().join("data/meetko.db")).unwrap();
        conn.execute(
            "INSERT INTO versions (id, meeting_id, kind, created_at, content_sha256, json) VALUES (?1,?2,'draft','2026-01-01T00:00:00Z',?3,?4)",
            rusqlite::params![format!("{m}.draft.1"), m, util::content_sha256(&raw), raw.to_string()],
        )
        .unwrap();
    }
    let approved = s.approve(&format!("{m}.draft.1")).unwrap();
    let new = s.version_raw(&approved.version.id).unwrap();
    assert_eq!(new["future_branch"], raw["future_branch"]);
    assert_eq!(new["words"][0]["lang"], "ko");
    assert_eq!(util::content_sha256(&new), util::content_sha256(&raw));
    assert_eq!(new["version"]["kind"], "approved");
}

#[test]
fn concurrent_version_ids_do_not_collide() {
    let (tmp, _s, m) = setup(70_000);
    let data = tmp.path().join("data");
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let (data, m) = (data.clone(), m.clone());
            std::thread::spawn(move || {
                let s = Store::open(&data).unwrap(); // its own connection
                (0..15).map(|_| s.create_version(draft_for(&m)).unwrap().version.id).collect::<Vec<_>>()
            })
        })
        .collect();
    let mut ids: Vec<String> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 30);
}

#[test]
fn importing_over_a_corrupt_stored_file_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a.wav"), tmp.path().join("b.wav"));
    wav(&a, 8000);
    wav(&b, 9000);
    let s = Store::open(&tmp.path().join("data")).unwrap();
    let first = s.import_meeting(vec![ch(0, "mic", &a), ch(1, "loopback", &b)], ImportOptions::default()).unwrap();
    assert!(!first.existing);
    // same recording again: the existing meeting, nothing new
    let again = s.import_meeting(vec![ch(0, "mic", &a), ch(1, "loopback", &b)], ImportOptions::default()).unwrap();
    assert!(again.existing && again.meeting.id == first.meeting.id);
    // damage the stored copy; a third import must not trust it
    let rec = std::fs::read_dir(tmp.path().join("data/recordings")).unwrap().next().unwrap().unwrap().path();
    let stored = rec.join("ch0-mic.wav");
    let mut bytes = std::fs::read(&stored).unwrap();
    *bytes.last_mut().unwrap() ^= 0xff;
    std::fs::write(&stored, bytes).unwrap();
    let err = s.import_meeting(vec![ch(0, "mic", &a), ch(1, "loopback", &b)], ImportOptions::default()).err().unwrap();
    assert!(matches!(err, Error::Corrupt(_)), "{err}");
    // and a wrong claimed hash is refused while copying
    let mut bad = ch(0, "mic", &a);
    bad.expected_sha256 = Some("0".repeat(64));
    assert!(s.import_meeting(vec![bad, ch(1, "loopback", &b)], ImportOptions::default()).is_err());
}

#[cfg(unix)]
#[test]
fn data_dir_and_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let (tmp, _s, _m) = setup(8000);
    let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(tmp.path().join("data")), 0o700);
    assert_eq!(mode(tmp.path().join("data/meetko.db")), 0o600);
    let rec = std::fs::read_dir(tmp.path().join("data/recordings")).unwrap().next().unwrap().unwrap().path();
    assert_eq!(mode(rec.join("ch0-mic.wav")), 0o600);
}

#[test]
fn a_changed_plugin_is_refused_at_add_and_at_run() {
    let (tmp, s, m) = setup(70_000);
    let draft = s.create_version(draft_for(&m)).unwrap();
    // a private copy of the plugin, so the repo's own files are never touched
    let dir = tmp.path().join("plugin");
    std::fs::create_dir_all(&dir).unwrap();
    for f in ["meetko.toml", "plugin.py"] {
        std::fs::copy(root().join("plugins/action-items-py").join(f), dir.join(f)).unwrap();
    }
    let (manifest, msha) = Manifest::load(&dir).unwrap();
    let row = s.add_plugin(&manifest, &dir, &msha, &plugin_code_sha256(&manifest, &dir).unwrap()).unwrap();
    let id = pipeline::run_plugin(&s, &row.id, &draft.version.id, std::time::Duration::from_secs(30)).unwrap();
    // the result carries a snapshot of what ran
    assert_eq!(s.results_for_version(&draft.version.id).unwrap()[0].plugin_code_sha256, row.code_sha256);
    assert!(id > 0);
    // ... and of the isolation the run had
    assert_eq!(s.results_for_version(&draft.version.id).unwrap()[0].sandbox, if cfg!(target_os = "macos") { "macos-sbpl" } else { "none" });

    // change one byte of code
    let code = std::fs::read_to_string(dir.join("plugin.py")).unwrap();
    std::fs::write(dir.join("plugin.py"), code + "\n# changed\n").unwrap();
    let err = pipeline::run_plugin(&s, &row.id, &draft.version.id, std::time::Duration::from_secs(30)).unwrap_err();
    assert!(err.to_string().contains("changed since it was added"), "{err}");
    // adding the changed code under the same id@version is refused
    let (_, msha) = Manifest::load(&dir).unwrap();
    let err = s.add_plugin(&manifest, &dir, &msha, &plugin_code_sha256(&manifest, &dir).unwrap()).unwrap_err();
    assert!(err.to_string().contains("already added"), "{err}");
}

#[test]
fn results_are_validated_again_when_stored() {
    let (_t, s, m) = setup(70_000);
    let draft = s.create_version(draft_for(&m)).unwrap();
    let plugin = add_action_items(&s);
    let mut bad = empty_output(&draft.version.id);
    bad["input"]["version_id"] = json!("someone-else");
    assert!(s.add_result(&plugin, &draft.version.id, &bad).is_err());
    bad = empty_output(&draft.version.id);
    bad["extra"] = json!(1);
    assert!(s.add_result(&plugin, &draft.version.id, &bad).is_err());
}

#[test]
fn invalid_data_is_not_stored() {
    let (_t, s, m) = setup(70_000);
    let mut md = draft_for(&m);
    md.turns.pop();
    assert!(s.create_version(md).is_err());
}

#[test]
fn export_of_a_broken_turn_is_an_error_not_a_panic() {
    let mut md = draft_for("x");
    md.turns[0].last_word = "w99999".into();
    assert!(meetko_core::export::webvtt(&md).is_err());
}

#[test]
fn swapping_a_compiled_binary_after_add_is_refused() {
    let (tmp, s, m) = setup(70_000);
    let draft = s.create_version(draft_for(&m)).unwrap();
    // the executable sits under target/, which the directory hash skips
    let dir = tmp.path().join("bin-plugin");
    std::fs::create_dir_all(dir.join("target/release")).unwrap();
    std::fs::write(
        dir.join("meetko.toml"),
        "[package]\nid=\"t/bin\"\nversion=\"1\"\nkind=\"plugin\"\ncontract=\"0.1\"\n[run]\ncommand=[\"target/release/p\"]\nprotocol=\"stdio-jsonrpc\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("target/release/p"), b"#!/bin/sh\nexit 0\n").unwrap();
    let (manifest, msha) = Manifest::load(&dir).unwrap();
    let before = plugin_code_sha256(&manifest, &dir).unwrap();
    let row = s.add_plugin(&manifest, &dir, &msha, &before).unwrap();
    std::fs::write(dir.join("target/release/p"), b"#!/bin/sh\nexit 1\n").unwrap();
    assert_ne!(plugin_code_sha256(&manifest, &dir).unwrap(), before);
    let err = pipeline::run_plugin(&s, &row.id, &draft.version.id, std::time::Duration::from_secs(5)).unwrap_err();
    assert!(err.to_string().contains("changed since it was added"), "{err}");
}

#[test]
fn a_plugin_whose_program_is_missing_cannot_be_pinned() {
    // otherwise the program could be dropped in after approval without changing the hash
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("no-bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("meetko.toml"),
        "[package]\nid=\"t/nobin\"\nversion=\"1\"\nkind=\"plugin\"\ncontract=\"0.1\"\n[run]\ncommand=[\"target/release/p\"]\nprotocol=\"stdio-jsonrpc\"\n",
    )
    .unwrap();
    let (manifest, _) = Manifest::load(&dir).unwrap();
    let err = plugin_code_sha256(&manifest, &dir).unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");
}

#[test]
fn install_copies_the_plugin_and_runs_only_the_copy() {
    let (tmp, s, m) = setup(70_000);
    let draft = s.create_version(draft_for(&m)).unwrap();
    let src = tmp.path().join("plugin");
    std::fs::create_dir_all(src.join("target/release")).unwrap();
    for f in ["meetko.toml", "plugin.py"] {
        std::fs::copy(root().join("plugins/action-items-py").join(f), src.join(f)).unwrap();
    }
    let (manifest, _) = Manifest::load(&src).unwrap();
    let shown = plugin_code_sha256(&manifest, &src).unwrap();
    // a hash that is not the one the user saw is refused
    let err = s.install_plugin(&src, Some(&"0".repeat(64))).unwrap_err();
    assert_eq!(err.code(), "plugin_changed");
    let row = s.install_plugin(&src, Some(&shown)).unwrap();
    assert_eq!(row.code_sha256, shown);
    assert!(row.dir.starts_with(dunce::canonicalize(s.dir()).unwrap().join("plugins")), "{:?}", row.dir);
    assert!(row.dir.file_name().unwrap().to_string_lossy().starts_with("meetko_action-items-py@0.1.0-"));
    // installing the same bytes again reuses the copy
    assert_eq!(s.install_plugin(&src, None).unwrap().dir, row.dir);
    // editing the source afterwards changes nothing: the copy runs, and still matches its hash
    let code = std::fs::read_to_string(src.join("plugin.py")).unwrap();
    std::fs::write(src.join("plugin.py"), code + "\nraise SystemExit(1)\n").unwrap();
    assert!(pipeline::run_plugin(&s, &row.id, &draft.version.id, std::time::Duration::from_secs(30)).is_ok());
    // editing the copy is caught by the per-run recheck
    std::fs::write(row.dir.join("plugin.py"), "# x\n").unwrap();
    let err = pipeline::run_plugin(&s, &row.id, &draft.version.id, std::time::Duration::from_secs(30)).unwrap_err();
    assert!(err.to_string().contains("changed since it was added"), "{err}");
}

#[cfg(unix)]
#[test]
fn install_refuses_symlinks_and_copies_a_compiled_command_file() {
    let (tmp, s, _m) = setup(16000);
    let src = tmp.path().join("bin-plugin");
    std::fs::create_dir_all(src.join("target/release")).unwrap();
    std::fs::write(
        src.join("meetko.toml"),
        "[package]\nid=\"t/bin\"\nversion=\"1\"\nkind=\"plugin\"\ncontract=\"0.1\"\n[run]\ncommand=[\"target/release/p\"]\nprotocol=\"stdio-jsonrpc\"\n",
    )
    .unwrap();
    std::fs::write(src.join("target/release/p"), b"#!/bin/sh\nexit 0\n").unwrap();
    let row = s.install_plugin(&src, None).unwrap();
    assert!(row.dir.join("target/release/p").is_file(), "the executable under target/ is copied by name");
    std::os::unix::fs::symlink("/etc/hosts", src.join("link")).unwrap();
    assert!(s.install_plugin(&src, None).unwrap_err().to_string().contains("symlink"));
}

#[cfg(unix)]
#[test]
fn a_preexisting_custom_data_dir_keeps_its_permissions_and_warns() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let shared = tmp.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    let s = Store::open(&shared).unwrap();
    assert_eq!(std::fs::metadata(&shared).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(s.warnings().len(), 1, "{:?}", s.warnings());
    assert!(s.warnings()[0].contains("shared"));
    // a dir meetko creates itself is private and raises no warning
    let own = Store::open(&tmp.path().join("own")).unwrap();
    assert!(own.warnings().is_empty());
    assert_eq!(std::fs::metadata(tmp.path().join("own")).unwrap().permissions().mode() & 0o777, 0o700);
}

#[test]
fn approving_an_old_draft_again_after_a_newer_approval_is_stale() {
    let (_t, s, m) = setup(70_000);
    let d1 = s.create_version(draft_for(&m)).unwrap();
    let a = s.approve(&d1.version.id).unwrap();
    let mut changed = draft_for(&m);
    changed.words[0].text = "바뀐".into();
    let d2 = s.create_version(changed).unwrap();
    let b = s.approve(&d2.version.id).unwrap();
    assert_ne!(a.version.id, b.version.id);
    let err = s.approve(&d1.version.id).expect_err("draft1 is no longer the head");
    assert!(matches!(err, Error::StaleDraft(_)), "{err}");
    // approving the current head again is still idempotent
    assert_eq!(s.approve(&d2.version.id).unwrap().version.id, b.version.id);
}
