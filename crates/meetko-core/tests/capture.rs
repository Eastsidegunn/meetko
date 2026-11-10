//! capture.json import: validation, the rules the schema cannot express, coverage from gaps.

use std::path::{Path, PathBuf};

use meetko_core::capture::{coverage_from_gaps, load};
use meetko_core::contract::Span;
use meetko_core::store::{recording_sha256, Store};
use meetko_core::util::sha256_file;
use serde_json::{json, Value};

fn wav(path: &Path, frames: u32) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    (0..frames).for_each(|i| w.write_sample((i % 50) as i16).unwrap());
    w.finalize().unwrap();
}

/// 4 s capture with mic gaps: late_start fill 0..300, dropout loss 1000..1200, trim marker at 2000
fn make_capture(dir: &Path, edit: impl FnOnce(&mut Value)) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    wav(&dir.join("mic.wav"), 64_000);
    wav(&dir.join("loopback.wav"), 64_000);
    let (a, b) = (sha256_file(&dir.join("mic.wav")).unwrap(), sha256_file(&dir.join("loopback.wav")).unwrap());
    let mut doc = json!({
        "contract": "0.1", "kind": "capture", "provider": {"id": "test", "version": "0"},
        "started_at": "2026-10-27T09:18:22.781Z", "sample_rate": 16000, "duration_ms": 4000,
        "complete": true, "end_reason": "user",
        "recording_sha256": recording_sha256(&[(0, a.clone()), (1, b.clone())]),
        "channels": [
          {"index": 0, "role": "mic", "file": "mic.wav", "frames": 64000, "sha256": a, "device": "default_input",
           "dropped_buffers": 0, "gaps": [
             {"start_ms": 0, "end_ms": 300, "reason": "late_start", "kind": "fill"},
             {"start_ms": 1000, "end_ms": 1200, "reason": "dropout", "kind": "loss"},
             {"start_ms": 2000, "end_ms": 2001, "reason": "overlap_trimmed", "kind": "trim", "removed_ms": 10}]},
          {"index": 1, "role": "loopback", "file": "loopback.wav", "frames": 64000, "sha256": b, "device": "system_output",
           "dropped_buffers": 0, "gaps": [{"start_ms": 500, "end_ms": 900, "reason": "no_data", "kind": "no_data"}]}
        ]
    });
    edit(&mut doc);
    let p = dir.join("capture.json");
    std::fs::write(&p, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    p
}

fn spans(v: &[Span]) -> Vec<(i64, i64)> {
    v.iter().map(|s| (s.start_ms, s.end_ms)).collect()
}

#[test]
fn loss_and_trim_are_not_coverage_fill_and_no_data_are() {
    let g = |s, e, k: &str| (s, e, k.to_string());
    let cov = coverage_from_gaps(4000, &[g(0, 300, "fill"), g(1000, 1200, "loss"), g(1100, 1500, "loss"), g(2000, 2010, "trim"), g(500, 900, "no_data")]);
    assert_eq!(spans(&cov), vec![(0, 1000), (1500, 2000), (2010, 4000)]);
    assert_eq!(spans(&coverage_from_gaps(4000, &[g(0, 4000, "loss")])), vec![]);
    assert_eq!(spans(&coverage_from_gaps(4000, &[])), vec![(0, 4000)]);
}

#[test]
fn speech_across_a_trim_marker_survives_but_words_in_lost_audio_do_not() {
    let word = |s: i64, e: i64, t: &str| json!({"start_ms": s, "end_ms": e, "text": t, "end_estimated": true});
    let result = json!({"contract": "0.1", "kind": "asr.batch", "engine": {"id": "e", "version": "1"}, "network": false,
        "coverage": [{"start_ms": 0, "end_ms": 4000}],
        "words": [word(1990, 2100, "across"), word(1100, 1150, "lost"), word(300, 400, "fine")]});
    let cap = [Span { start_ms: 0, end_ms: 1000 }, Span { start_ms: 1200, end_ms: 2000 }, Span { start_ms: 2001, end_ms: 4000 }];
    let c = meetko_core::pipeline::batch_channel(&result, 0, "mic", None, Some(&cap)).unwrap();
    let texts: Vec<_> = c.words.iter().map(|w| w.text.as_str()).collect();
    assert_eq!(texts, ["across", "fine"]);
    assert_eq!(c.producers[0]["dropped_words_outside_coverage"], 1);
}

#[test]
fn a_valid_capture_imports_with_derived_coverage_and_the_manifest_hash() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = make_capture(&tmp.path().join("cap"), |_| {});
    // a directory or the file itself both work
    let c = load(&manifest, false).unwrap();
    load(manifest.parent().unwrap(), false).unwrap();
    let want_rec = serde_json::from_slice::<Value>(&std::fs::read(&manifest).unwrap()).unwrap()["recording_sha256"].as_str().unwrap().to_string();
    let s = Store::open(&tmp.path().join("data")).unwrap();
    let out = s.import_meeting(c.channels, c.options).unwrap();
    assert_eq!(out.meeting.duration_ms, 4000);
    let (meeting, rec, files) = s.meeting_and_recording(&out.meeting.id).unwrap();
    assert_eq!(rec.sha256, want_rec, "recording.sha256 is the manifest's definition");
    assert_eq!(rec.duration_ms, 4000);
    assert_eq!(meeting.started_at.as_deref(), Some("2026-10-27T09:18:22.781Z"));
    assert_eq!(spans(files[0].coverage.as_ref().unwrap()), vec![(0, 1000), (1200, 2000), (2001, 4000)]);
    assert_eq!(spans(files[1].coverage.as_ref().unwrap()), vec![(0, 4000)]);
}

#[test]
fn bad_captures_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let refuse = |name: &str, edit: Box<dyn FnOnce(&mut Value)>, allow: bool| {
        let p = make_capture(&t.join(name), edit);
        let err = load(&p, allow).err().unwrap_or_else(|| panic!("{name} was accepted")).to_string();
        println!("{name}: {err}");
    };
    refuse("incomplete", Box::new(|d| { d["complete"] = json!(false); }), false);
    refuse("frames", Box::new(|d| { d["channels"][1]["frames"] = json!(63999); }), false);
    refuse("duration", Box::new(|d| { d["duration_ms"] = json!(3999); }), false);
    refuse("gap-outside", Box::new(|d| { d["channels"][0]["gaps"][1]["end_ms"] = json!(4001); }), false);
    refuse("gap-reversed", Box::new(|d| { d["channels"][0]["gaps"][1]["start_ms"] = json!(1300); }), false);
    refuse("gap-empty", Box::new(|d| { d["channels"][0]["gaps"][1]["end_ms"] = json!(1000); }), false);
    refuse("marker-span", Box::new(|d| { d["channels"][0]["gaps"][2]["end_ms"] = json!(2010); }), false);
    refuse("long-fill", Box::new(|d| { d["channels"][0]["gaps"][0]["end_ms"] = json!(1500); d["channels"][0]["gaps"][1]["start_ms"] = json!(1600); d["channels"][0]["gaps"][1]["end_ms"] = json!(1700); }), false);
    refuse("gap-order", Box::new(|d| { d["channels"][0]["gaps"].as_array_mut().unwrap().swap(0, 1); }), false);
    refuse("gap-overlap", Box::new(|d| { d["channels"][0]["gaps"][1]["start_ms"] = json!(200); }), false);
    refuse("schema-unknown-field", Box::new(|d| { d["extra"] = json!(1); }), false);
    refuse("file-path", Box::new(|d| { d["channels"][0]["file"] = json!("../mic.wav"); }), false);
    // the recording hash is checked by the store, with the same definition it uses to name the files
    let p = make_capture(&t.join("sha-recording"), |d| { d["recording_sha256"] = json!("0".repeat(64)); });
    let c = load(&p, false).unwrap();
    let s = Store::open(&t.join("data0")).unwrap();
    assert!(s.import_meeting(c.channels, c.options).is_err());
    // the wrong channel hash is caught while copying
    let p = make_capture(&t.join("sha-channel"), |d| { d["channels"][0]["sha256"] = json!("1".repeat(64)); });
    let c = load(&p, false).unwrap();
    let s = Store::open(&t.join("data")).unwrap();
    assert!(s.import_meeting(c.channels, c.options).is_err());
    // an incomplete capture is accepted only when asked for
    let p = make_capture(&t.join("incomplete-ok"), |d| { d["complete"] = json!(false); });
    assert!(load(&p, true).is_ok());
}

#[cfg(unix)]
#[test]
fn symlinked_channel_files_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("cap");
    let p = make_capture(&dir, |_| {});
    let real = tmp.path().join("real.wav");
    std::fs::rename(dir.join("mic.wav"), &real).unwrap();
    std::os::unix::fs::symlink(&real, dir.join("mic.wav")).unwrap();
    let err = load(&p, false).err().unwrap().to_string();
    assert!(err.contains("symlink"), "{err}");
}

/// A real recording from the capture sidecar, if `MEETKO_CAPTURE_TEST_DIR` names one (it may
/// predate the schema, so refusing it is acceptable; what matters is that the outcome is a clear message).
#[test]
fn real_capture_if_present() {
    let Some(dir) = std::env::var_os("MEETKO_CAPTURE_TEST_DIR").map(std::path::PathBuf::from).filter(|d| d.join("capture.json").exists()) else {
        eprintln!("SKIP: MEETKO_CAPTURE_TEST_DIR not set or has no capture.json");
        return;
    };
    match load(&dir, false) {
        Ok(c) => {
            let tmp = tempfile::tempdir().unwrap();
            let s = Store::open(tmp.path()).unwrap();
            let out = s.import_meeting(c.channels, c.options).unwrap();
            eprintln!("REAL CAPTURE imported as {} ({} ms)", out.meeting.id, out.meeting.duration_ms);
        }
        Err(e) => eprintln!("REAL CAPTURE refused: {e}"),
    }
}
