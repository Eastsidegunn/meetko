//! Conformance of the real sherpa-onnx streaming engine against the schema and against the
//! Python experiment. Needs the built engine and local audio/model, so it runs only when
//! these are set (otherwise it passes after saying so):
//!   MEETKO_STREAM_ENGINE   the engine binary
//!   MEETKO_STREAM_MODEL    the model directory
//!   MEETKO_CONFORMANCE_WAV a 16 kHz mono PCM16 wav
//!   MEETKO_CONFORMANCE_REF (optional) the Python experiment's JSONL for the same wav
//! Run with `-- --nocapture` to see both texts.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meetko_core::contract::{engine_result_validator, schema_errors};
use meetko_core::live::{EngineClient, Folder, LiveConfig, Push, WavTail};
use serde_json::{json, Value};

fn env(k: &str) -> Option<PathBuf> {
    std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn distance(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, y) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(x != y)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[test]
fn engine_output_validates_and_matches_the_python_experiment() {
    let (Some(bin), Some(model), Some(wav)) = (env("MEETKO_STREAM_ENGINE"), env("MEETKO_STREAM_MODEL"), env("MEETKO_CONFORMANCE_WAV")) else {
        eprintln!("skipped: MEETKO_STREAM_ENGINE, MEETKO_STREAM_MODEL and MEETKO_CONFORMANCE_WAV are not all set");
        return;
    };
    let cfg = LiveConfig { engine_bin: bin, model_dir: model.clone(), language: "ko".into(), unpinned: true, queue: 1024 };
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = events.clone();
    let mut engine = EngineClient::spawn(&cfg, &model, move |e| sink.lock().unwrap().push(e), &|| false).unwrap();
    engine.call("stream.open", json!({ "sample_rate": 16000, "language": "ko" }), Duration::from_secs(60)).unwrap();

    let mut tail = WavTail::open(&wav).unwrap();
    let mut pcm = Vec::new();
    loop {
        let b = tail.read_new().unwrap();
        if b.is_empty() {
            break;
        }
        pcm.extend(b);
    }
    for chunk in pcm.chunks(3200) {
        while engine.push(chunk).unwrap() == Push::Full {
            assert!(!engine.is_dead());
            std::thread::sleep(Duration::from_millis(5)); // the queue is full: wait for the engine
        }
    }
    engine.call("stream.close", json!({}), Duration::from_secs(120)).unwrap();
    engine.shutdown(&|| false);
    std::thread::sleep(Duration::from_millis(100));

    let all = events.lock().unwrap().clone();
    assert!(!all.is_empty());
    for e in &all {
        let errs = schema_errors(engine_result_validator(), e);
        assert!(errs.is_empty(), "{errs:?} in {e}");
    }
    let mut folder = Folder::default();
    all.iter().for_each(|e| {
        folder.apply(e);
    });
    let ours: String = folder.events().filter(|e| e["final"] == json!(true)).map(|e| e["text"].as_str().unwrap().to_string()).collect::<Vec<_>>().join(" ");
    let partials = all.iter().filter(|e| e["final"] == json!(false)).count();
    eprintln!("rust engine: {} events ({partials} partial), {} segments\n  {ours}", all.len(), folder.events().count());

    if let Some(r) = env("MEETKO_CONFORMANCE_REF") {
        let text = std::fs::read_to_string(r).unwrap();
        let py: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let theirs: String = py.iter().filter(|e| e["kind"] == "final").map(|e| e["text"].as_str().unwrap().trim().to_string()).collect::<Vec<_>>().join(" ");
        eprintln!("python experiment: {} events, {} finals\n  {theirs}", py.len(), py.iter().filter(|e| e["kind"] == "final").count());
        let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<Vec<char>>();
        let (a, b) = (strip(&ours), strip(&theirs));
        let d = distance(&a, &b);
        eprintln!("character edit distance (spaces ignored): {d} of {}", b.len());
        assert!(d * 50 <= b.len(), "the texts differ by more than 2%");
    }
}
