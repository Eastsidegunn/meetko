//! Batch transcription by an engine process (`batch::BatchProcess`, the protocol, staging, the
//! pipeline) with a fake engine in Python. The real engine is tested in `engines/qwen3-asr/tests/`.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use meetko_core::batch::{BatchConfig, BatchProcess};
use meetko_core::pipeline;
use meetko_core::store::{ImportChannel, ImportOptions, Store};
use meetko_core::whisper::Progress;
use meetko_core::Error;

/// An `asr.batch` engine speaking the real protocol: two progress steps, then three words. The result
/// names the pid that answered. `mode`: ok | fault (engine.error, exit 1) | hang | other_audio | wrong_kind.
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
        out({"jsonrpc": "2.0", "id": id, "result": {"contract": "0.1", "engine": {"id": "fake/batch", "version": "0"}, "role": "asr.batch", "model": {"id": "fake-model"}}})
    elif meth == "transcribe.run":
        p = m["params"]
        if MODE == "fault":
            out({"jsonrpc": "2.0", "method": "engine.error", "params": {"message": "the library failed"}})
            sys.exit(1)
        out({"jsonrpc": "2.0", "method": "transcribe.progress", "params": {"percent": 50}})
        if MODE == "hang":
            time.sleep(600)
        out({"jsonrpc": "2.0", "method": "transcribe.progress", "params": {"percent": 100}})
        words = [{"start_ms": i * 150, "end_ms": i * 150 + 100, "text": "말" + str(i)} for i in range(3)]
        res = {"contract": "0.1", "kind": "asr.batch", "engine": {"id": "fake/batch", "version": "0"}, "model": {"id": "fake-model", "sha256": "0" * 64},
               "audio_sha256": p["audio_sha256"], "params": {"pid": os.getpid(), "language": p["language"], "cwd": os.getcwd(), "cwd_home": os.path.realpath(os.getcwd()) == os.path.realpath(os.environ.get("HOME", "-"))}, "language": p["language"],
               "word_timing": "aligned", "network": False, "coverage": [{"start_ms": 0, "end_ms": 600}], "words": words}
        if MODE == "other_audio":
            res["audio_sha256"] = "0" * 64
        if MODE == "wrong_kind":
            res["kind"] = "diarization"
        out({"jsonrpc": "2.0", "id": id, "result": res})
    elif meth == "shutdown":
        out({"jsonrpc": "2.0", "id": id, "result": {}})
        break
"#;
    let p = dir.join(format!("fake-batch-{mode}.py"));
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
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a.wav"), tmp.path().join("b.wav"));
    wav(&a, 9600);
    wav(&b, 9600);
    let store = Store::open(&tmp.path().join("data")).unwrap();
    let ch = |index, role: &str, label: &str, path: &Path| ImportChannel { index, role: role.into(), label: Some(label.into()), path: path.into(), expected_sha256: None, coverage: None };
    let meeting = store.import_meeting(vec![ch(0, "mic", "나", &a), ch(1, "loopback", "상대", &b)], ImportOptions::default()).unwrap().meeting.id;
    let dir = tmp.path().to_path_buf();
    Fixture { _tmp: tmp, dir, store, meeting }
}

/// The engine plus a model directory laid out like the pinned one (its contents are not the model).
fn process(f: &Fixture, mode: &str, unpinned: bool, timeout: Duration) -> BatchProcess {
    let model_dir = f.dir.join("batch-model");
    std::fs::create_dir_all(&model_dir).unwrap();
    std::fs::write(model_dir.join("qwen3-asr-1.7b-q8_0.gguf"), "asr").unwrap();
    std::fs::write(model_dir.join("qwen3-forced-aligner-0.6b-q8_0.gguf"), "aligner").unwrap();
    BatchProcess::new(BatchConfig { engine_bin: fake_engine(&f.dir, mode), model_dir, unpinned, timeout, sandbox: false })
}

#[test]
fn a_batch_engine_makes_the_draft_and_the_core_records_what_it_staged() {
    let f = fixture();
    let p = process(&f, "ok", true, Duration::from_secs(10));
    let seen = Mutex::new(Vec::<Progress>::new());
    let md = pipeline::transcribe(&f.store, &p, &f.meeting, Some("ko"), &|x| seen.lock().unwrap().push(x), &AtomicBool::new(false)).unwrap();
    assert_eq!(md.words.len(), 6, "three words per channel");
    let asr: Vec<_> = md.version.producers.iter().filter(|p| p["role"] == "asr").collect();
    assert_eq!(asr.len(), 2);
    for pr in &asr {
        assert_eq!(pr["engine"]["id"], "fake/batch");
        assert_eq!(pr["word_timing"], "aligned");
        assert_eq!(pr["network"], false);
        assert_ne!(pr["model"]["sha256"], "0".repeat(64), "the digest of what the core staged, not the engine's claim");
        assert_eq!(pr["params"]["model_pinned"], false);
        assert_eq!(pr["params"]["language"], "ko");
    }
    assert_eq!(asr[0]["params"]["pid"], asr[1]["params"]["pid"], "one process serves every channel");
    let seen = seen.lock().unwrap();
    for ch in [0, 1] {
        let steps: Vec<u8> = seen.iter().filter(|s| s.channel == ch).map(|s| s.percent).collect();
        assert_eq!(steps, [50, 100], "channel {ch}: {seen:?}");
    }
}

#[test]
fn the_model_detects_the_language_unless_one_was_named() {
    let f = fixture();
    let p = process(&f, "ok", true, Duration::from_secs(10));
    let md = pipeline::transcribe(&f.store, &p, &f.meeting, None, &|_| {}, &AtomicBool::new(false)).unwrap();
    let asr = md.version.producers.iter().find(|p| p["role"] == "asr").unwrap();
    assert_eq!(asr["params"]["language"], "auto", "no hint: not the whisper default ko");
    // the core records how it ran the engine: these tests turn the sandbox off for the fake engine
    assert_eq!(asr["params"]["sandbox"], if cfg!(target_os = "macos") { "off" } else { "none" });
    // started in its own private HOME, not in whatever folder the app was started from
    assert_eq!(asr["params"]["cwd_home"], true, "{}", asr["params"]);
    assert_ne!(asr["params"]["cwd"].as_str().map(PathBuf::from), std::env::current_dir().ok());
}

#[test]
fn an_unpinned_model_is_refused_unless_allowed() {
    let f = fixture();
    let p = process(&f, "ok", false, Duration::from_secs(10));
    let e = pipeline::transcribe(&f.store, &p, &f.meeting, None, &|_| {}, &AtomicBool::new(false)).unwrap_err();
    assert!(matches!(e, Error::ModelMissing(_)), "{e}");
    assert!(f.store.versions_of(&f.meeting).unwrap().is_empty());
}

#[test]
fn a_failing_or_lying_engine_costs_the_run_not_the_store() {
    for (mode, want) in [("fault", "the library failed"), ("other_audio", "different audio"), ("wrong_kind", "asr.batch")] {
        let f = fixture();
        let p = process(&f, mode, true, Duration::from_secs(10));
        let e = pipeline::transcribe(&f.store, &p, &f.meeting, None, &|_| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(e.to_string().contains(want), "{mode}: {e}");
        assert!(f.store.versions_of(&f.meeting).unwrap().is_empty(), "{mode}");
    }
}

#[test]
fn cancelling_ends_a_running_engine() {
    let f = fixture();
    let p = process(&f, "hang", true, Duration::from_secs(60));
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
            cancel.store(true, Ordering::SeqCst);
        });
        let e = pipeline::transcribe(&f.store, &p, &f.meeting, None, &|_| {}, &cancel).unwrap_err();
        assert!(matches!(e, Error::Cancelled), "{e}");
    });
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_hanging_engine_runs_out_of_time() {
    let f = fixture();
    let p = process(&f, "hang", true, Duration::from_millis(300));
    let e = pipeline::transcribe(&f.store, &p, &f.meeting, None, &|_| {}, &AtomicBool::new(false)).unwrap_err();
    assert!(matches!(e, Error::Timeout(_)), "{e}");
}
