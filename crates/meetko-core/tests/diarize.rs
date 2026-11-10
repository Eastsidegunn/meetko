//! Batch diarization through the real wiring (`pipeline::transcribe_with`, `DiarProcess`, the
//! engine protocol) with a fake engine in Python and a fake ASR. The real engine is tested in
//! `engines/nemotron-stream/tests/diar.rs`.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use meetko_core::contract::{invariant_errors, meeting_data_validator, schema_errors, Basis, MeetingData, SpeakerStatus};
use meetko_core::diarize::{DiarConfig, DiarProcess};
use meetko_core::pipeline::{self, Transcribed};
use meetko_core::store::{ImportChannel, ImportOptions, Store};
use meetko_core::whisper::{BatchEngine, BatchRequest, Progress};
use meetko_core::Error;
use serde_json::{json, Value};

/// Six words 100 ms apart on every channel, 80 ms long.
struct Asr;

impl BatchEngine for Asr {
    fn transcribe(&self, req: &BatchRequest, _p: &dyn Fn(Progress), _c: &AtomicBool) -> meetko_core::Result<Value> {
        let words: Vec<Value> = (0..6).map(|i| json!({"start_ms": i * 100, "end_ms": i * 100 + 80, "text": format!("w{i}")})).collect();
        Ok(json!({
            "contract": "0.1", "kind": "asr.batch", "engine": {"id": "fake", "version": "1"}, "model": {"id": "m"},
            "audio_sha256": req.audio_sha256, "params": {}, "word_timing": "token", "network": false,
            "coverage": [{"start_ms": 0, "end_ms": 600}], "words": words
        }))
    }
}

/// A diarization engine speaking the real protocol. The first `diarize.run` it is ever asked (counted
/// in a file, so it survives a restart of the process) is the mic channel: one speaker for the whole
/// audio; the second is the loopback: speaker "x" for 0-300 ms, "y" for 300-600. The result names the
/// pid that answered. `mode`: ok | fault (engine.error, exit 1) | hang | other_audio | bad_schema |
/// fail_once (the first request fails, the pid is left in a file) | tiny (y is only 100 ms) |
/// late (turns run past the end of the recording) | bare (no model, no params).
fn fake_engine(dir: &Path, mode: &str) -> PathBuf {
    let src = r#"#!/usr/bin/env python3
import json, os, sys, time
MODE = "MODE"
def out(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    meth, id = m.get("method"), m.get("id")
    if meth == "initialize":
        out({"jsonrpc": "2.0", "id": id, "result": {"contract": "0.1", "engine": {"id": "fake/diar", "version": "0"}, "role": "diarization", "model": {"id": "fake-model"}}})
    elif meth == "diarize.run":
        counter = __file__ + ".calls"
        with open(counter, "a") as f:
            f.write("x")
        calls = os.path.getsize(counter)
        marker = __file__ + ".failed"
        once = MODE == "fail_once" and not os.path.exists(marker)
        if once:
            open(marker, "w").write(str(os.getpid()))
        if MODE == "fault" or once:
            out({"jsonrpc": "2.0", "method": "engine.error", "params": {"message": "the model could not be applied"}})
            sys.exit(1)
        if MODE == "hang":
            time.sleep(600)
        p = m["params"]
        y0 = 500 if MODE == "tiny" else 300
        turns = [{"start_ms": 0, "end_ms": 600, "speaker": "a"}] if calls == 1 else [{"start_ms": 0, "end_ms": y0, "speaker": "x"}, {"start_ms": y0, "end_ms": 600, "speaker": "y"}]
        if MODE == "late":
            turns[-1]["end_ms"] = 99999
        res = {"contract": "0.1", "kind": "diarization", "engine": {"id": "fake/diar", "version": "0"}, "model": {"id": "fake-model"},
               "audio_sha256": p["audio_sha256"], "params": {"mode": "fake", "pid": os.getpid()}, "network": False, "turns": turns}
        if MODE == "not_object":
            res = "ok"
        elif MODE == "bare":
            del res["model"], res["params"]
        if MODE == "other_audio":
            res["audio_sha256"] = "0" * 64
        if MODE == "bad_schema":
            res["turns"] = [{"start_ms": "soon", "end_ms": 1, "speaker": "a"}]
        out({"jsonrpc": "2.0", "id": id, "result": res})
    elif meth == "shutdown":
        out({"jsonrpc": "2.0", "id": id, "result": {}})
        break
"#;
    let p = dir.join(format!("fake-diar-{mode}.py"));
    std::fs::write(&p, src.replace("MODE", mode)).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn wav(path: &Path, frames: u32) {
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    (0..frames).for_each(|i| w.write_sample((i % 100) as i16).unwrap());
    w.finalize().unwrap();
}

struct Fixture {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    store: Store,
    meeting: String,
}

/// A 0.6 s meeting (mic "나", loopback "상대") in a store.
fn fixture() -> Fixture {
    fixture_of(None)
}

/// `duration_ms`: the recording's, when it is not the longest channel's.
fn fixture_of(duration_ms: Option<i64>) -> Fixture {
    fixture_with(duration_ms, "상대")
}

fn fixture_with(duration_ms: Option<i64>, loopback_label: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a.wav"), tmp.path().join("b.wav"));
    wav(&a, 9600);
    wav(&b, 9600);
    let store = Store::open(&tmp.path().join("data")).unwrap();
    let ch = |index, role: &str, label: &str, path: &Path| ImportChannel { index, role: role.into(), label: Some(label.into()), path: path.into(), expected_sha256: None, coverage: None };
    let meeting = store.import_meeting(vec![ch(0, "mic", "나", &a), ch(1, "loopback", loopback_label, &b)], ImportOptions { duration_ms, ..Default::default() }).unwrap().meeting.id;
    let dir = tmp.path().to_path_buf();
    Fixture { _tmp: tmp, dir, store, meeting }
}

/// The engine plus an (unpinned) model directory next to it.
fn process(f: &Fixture, mode: &str, unpinned: bool) -> DiarProcess {
    process_within(f, mode, unpinned, Duration::from_secs(1))
}

fn process_within(f: &Fixture, mode: &str, unpinned: bool, timeout: Duration) -> DiarProcess {
    let model_dir = f.dir.join("diar-model");
    std::fs::create_dir_all(&model_dir).unwrap();
    std::fs::write(model_dir.join("Nemotron-3-Diarization.q8_0.gguf"), "weights").unwrap();
    DiarProcess::new(DiarConfig { engine_bin: fake_engine(&f.dir, mode), model_dir, unpinned, timeout, min_speaker_ms: 0 })
}

fn run(f: &Fixture, d: Option<&DiarProcess>, cancel: &AtomicBool) -> meetko_core::Result<Transcribed> {
    pipeline::transcribe_with(&f.store, &Asr, d.map(|d| d as &dyn meetko_core::diarize::DiarEngine), &f.meeting, None, &|_| {}, cancel)
}

fn assert_valid(md: &MeetingData) {
    let doc = md.to_value();
    assert!(schema_errors(meeting_data_validator(), &doc).is_empty(), "{:?}", schema_errors(meeting_data_validator(), &doc));
    assert!(invariant_errors(md).is_empty(), "{:?}", invariant_errors(md));
}

fn labels(md: &MeetingData, ch: u32) -> Vec<(Basis, String)> {
    md.speakers.iter().filter(|s| s.channel == ch).map(|s| (s.basis, s.label.clone().unwrap())).collect()
}

fn speaker_of_words(md: &MeetingData, ch: u32) -> Vec<Option<String>> {
    md.words.iter().filter(|w| w.channel == ch).map(|w| w.speaker.as_ref().map(|id| md.speakers.iter().find(|s| &s.id == id).unwrap().label.clone().unwrap())).collect()
}

fn without_diarization_every_channel_is_one_speaker(md: &MeetingData) {
    assert_valid(md);
    assert_eq!(labels(md, 0), [(Basis::Channel, "나".to_string())]);
    assert_eq!(labels(md, 1), [(Basis::Channel, "상대".to_string())]);
    assert!(md.words.iter().all(|w| w.speaker_status == SpeakerStatus::Assigned));
    assert!(md.speaker_spans.is_empty(), "no engine, no speech spans");
    assert!(md.version.producers.iter().all(|p| p["role"] != "diarization"));
}

#[test]
fn speakers_are_named_per_channel_and_the_producer_is_recorded() {
    let f = fixture();
    let d = process(&f, "ok", true);
    let done = run(&f, Some(&d), &AtomicBool::new(false)).unwrap();
    assert!(done.warnings.is_empty(), "{:?}", done.warnings);
    let md = done.data;
    assert_valid(&md);
    // the mic has one speaker: it stays "나"; the loopback has two: "상대 1" and "상대 2", by first speech
    assert_eq!(labels(&md, 0), [(Basis::Channel, "나".to_string())]);
    assert_eq!(labels(&md, 1), [(Basis::Diarization, "상대 1".to_string()), (Basis::Diarization, "상대 2".to_string())]);
    let who = |l: &str| Some(l.to_string());
    assert_eq!(speaker_of_words(&md, 0), vec![who("나"); 6]);
    assert_eq!(speaker_of_words(&md, 1), [who("상대 1"), who("상대 1"), who("상대 1"), who("상대 2"), who("상대 2"), who("상대 2")]);
    assert!(md.words.iter().all(|w| w.speaker_status == SpeakerStatus::Assigned));
    // the turns follow the speakers
    let named: std::collections::BTreeSet<&str> = md.turns.iter().map(|t| md.speakers.iter().find(|s| Some(&s.id) == t.speaker.as_ref()).unwrap().label.as_deref().unwrap()).collect();
    assert_eq!(named, ["나", "상대 1", "상대 2"].into());
    // speech spans exist for both channels (the mic's single speaker keeps its turns as spans)
    assert!(md.speaker_spans.iter().any(|s| s.channel == 0) && md.speaker_spans.iter().any(|s| s.channel == 1));
    // producers: an asr and a diarization entry per channel, the latter with the model digest and the pin state
    let dia: Vec<&Value> = md.version.producers.iter().filter(|p| p["role"] == "diarization").collect();
    assert_eq!(dia.len(), 2);
    assert_eq!(dia.iter().map(|p| p["channels"][0].as_u64().unwrap()).collect::<Vec<_>>(), [0, 1]);
    for p in &dia {
        assert_eq!(p["engine"]["id"], "fake/diar");
        assert_eq!(p["network"], false);
        assert!(meetko_core::util::is_sha256(p["model"]["sha256"].as_str().unwrap()));
        assert_eq!(p["params"]["model_pinned"], false);
    }
    assert_eq!(md.version.producers.iter().filter(|p| p["role"] == "asr").count(), 2);
}

#[test]
fn without_an_engine_the_result_is_what_it_always_was() {
    let f = fixture();
    let done = run(&f, None, &AtomicBool::new(false)).unwrap();
    assert!(done.warnings.is_empty());
    without_diarization_every_channel_is_one_speaker(&done.data);
    // and the old entry point is the same thing
    let md = pipeline::transcribe(&f.store, &Asr, &f.meeting, None, &|_| {}, &AtomicBool::new(false)).unwrap();
    without_diarization_every_channel_is_one_speaker(&md);
    assert_eq!(md.words.len(), done.data.words.len());
}

/// Whatever the engine does wrong, the draft is stored with the speakers of the channels and the
/// reason is a warning per channel.
fn degrades(mode: &str, unpinned: bool, expect: &str) {
    let f = fixture();
    let d = process(&f, mode, unpinned);
    let start = Instant::now();
    let done = run(&f, Some(&d), &AtomicBool::new(false)).unwrap_or_else(|e| panic!("{mode}: the transcript was lost: {e}"));
    assert!(start.elapsed() < Duration::from_secs(20), "{mode}: took {:?}", start.elapsed());
    without_diarization_every_channel_is_one_speaker(&done.data);
    assert_eq!(done.data.words.len(), 12, "{mode}");
    assert_eq!(done.warnings.len(), 2, "{mode}: {:?}", done.warnings);
    assert!(done.warnings.iter().all(|w| w.contains("speaker separation failed") && w.contains(expect)), "{mode}: {:?}", done.warnings);
    assert!(f.store.version(&done.data.version.id).is_ok(), "{mode}: stored");
}

#[test]
fn an_engine_that_dies_costs_the_speakers_not_the_transcript() {
    degrades("fault", true, "model could not be applied");
}

#[test]
fn an_engine_that_hangs_is_killed_at_its_time_budget() {
    degrades("hang", true, "timed out");
}

#[test]
fn a_result_for_other_audio_or_breaking_the_schema_is_not_used() {
    degrades("other_audio", true, "different audio");
    degrades("bad_schema", true, "diarization result");
}

#[test]
fn a_result_that_is_not_an_object_is_a_warning_not_a_panic() {
    degrades("not_object", true, "not an object");
}

#[test]
fn an_unpinned_model_is_refused_unless_development_allows_it() {
    degrades("ok", false, "not one of the pinned models");
}

#[test]
fn one_failed_channel_does_not_take_the_other_down() {
    let f = fixture();
    let d = process(&f, "fail_once", true);
    let done = run(&f, Some(&d), &AtomicBool::new(false)).unwrap();
    assert_valid(&done.data);
    assert_eq!(done.warnings.len(), 1, "{:?}", done.warnings);
    assert!(done.warnings[0].contains("channel 1"));
    // the mic (the failed request) kept its one speaker
    assert_eq!(labels(&done.data, 0), [(Basis::Channel, "나".to_string())]);
    // the loopback is the engine's second request ever, and got its two speakers: the first process died
    // on the first request, so a second one answered, and not the one that failed
    assert_eq!(labels(&done.data, 1), [(Basis::Diarization, "상대 1".to_string()), (Basis::Diarization, "상대 2".to_string())]);
    let failed: u64 = std::fs::read_to_string(f.dir.join("fake-diar-fail_once.py.failed")).unwrap().parse().unwrap();
    let dia: Vec<&Value> = done.data.version.producers.iter().filter(|p| p["role"] == "diarization").collect();
    assert_eq!(dia.len(), 1);
    let answered = dia[0]["params"]["pid"].as_u64().unwrap();
    assert_ne!(answered, failed, "a fresh process, not the one that failed");
}

#[test]
fn a_speaker_who_barely_spoke_is_merged_and_the_rule_is_recorded() {
    let f = fixture();
    // "y" speaks 100 ms of the loopback; with a 200 ms rule it is merged into "x" and the channel is one speaker
    let model_dir = f.dir.join("diar-model");
    std::fs::create_dir_all(&model_dir).unwrap();
    std::fs::write(model_dir.join("Nemotron-3-Diarization.q8_0.gguf"), "weights").unwrap();
    let d = DiarProcess::new(DiarConfig { engine_bin: fake_engine(&f.dir, "tiny"), model_dir, unpinned: true, timeout: Duration::from_secs(1), min_speaker_ms: 200 });
    let done = run(&f, Some(&d), &AtomicBool::new(false)).unwrap();
    assert_valid(&done.data);
    assert_eq!(labels(&done.data, 1), [(Basis::Channel, "상대".to_string())]);
    assert!(speaker_of_words(&done.data, 1).iter().all(|s| s.as_deref() == Some("상대")), "merged words stay attributed");
    let p = done.data.version.producers.iter().find(|p| p["role"] == "diarization" && p["channels"][0] == 1).unwrap();
    assert_eq!((p["params"]["min_speaker_ms"].as_i64(), p["params"]["merged_speakers"].as_u64()), (Some(200), Some(1)));
}

#[test]
fn turns_past_the_recording_cannot_cost_the_draft() {
    // the recording is 550 ms though the files are 600: turns up to 99999 ms are clamped to 550
    let f = fixture_of(Some(550));
    let d = process(&f, "late", true);
    let done = run(&f, Some(&d), &AtomicBool::new(false)).unwrap();
    assert!(done.warnings.is_empty(), "{:?}", done.warnings);
    assert_valid(&done.data);
    assert!(done.data.speaker_spans.iter().all(|s| s.end_ms <= 550));
    assert_eq!(labels(&done.data, 1).len(), 2);
}

#[test]
fn what_the_core_staged_is_recorded_even_when_the_engine_says_nothing_of_it() {
    let f = fixture();
    let d = process(&f, "bare", true);
    let done = run(&f, Some(&d), &AtomicBool::new(false)).unwrap();
    assert!(done.warnings.is_empty(), "{:?}", done.warnings);
    let p = done.data.version.producers.iter().find(|p| p["role"] == "diarization").unwrap();
    assert_eq!(p["model"]["id"], "fake-model", "from initialize");
    assert!(meetko_core::util::is_sha256(p["model"]["sha256"].as_str().unwrap()));
    assert_eq!(p["params"]["model_pinned"], false);
}

#[test]
fn cancelling_during_diarization_cancels_the_run() {
    let f = fixture();
    // a long budget: only the cancel can end the wait
    let d = process_within(&f, "hang", true, Duration::from_secs(300));
    let cancel = AtomicBool::new(false);
    let start = Instant::now();
    let r = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(700));
            cancel.store(true, Ordering::SeqCst);
        });
        run(&f, Some(&d), &cancel)
    });
    assert!(matches!(r, Err(Error::Cancelled)), "{:?}", r.err());
    assert!(start.elapsed() < Duration::from_secs(15));
    assert!(f.store.versions_of(&f.meeting).unwrap().is_empty(), "a cancelled run stores nothing");
}

#[test]
fn a_longest_allowed_label_keeps_its_speakers_within_the_schema_limit() {
    let label = "상".repeat(100);
    let f = fixture_with(None, &label);
    let d = process(&f, "ok", true);
    let done = run(&f, Some(&d), &AtomicBool::new(false)).unwrap();
    assert!(done.warnings.is_empty(), "no fallback needed: {:?}", done.warnings);
    assert_valid(&done.data);
    let names: Vec<String> = labels(&done.data, 1).into_iter().map(|(_, l)| l).collect();
    assert_eq!(names.len(), 2);
    assert!(names.iter().all(|n| n.chars().count() == 100), "{names:?}");
    assert!(names[0].ends_with(" 1") && names[1].ends_with(" 2"));
}
