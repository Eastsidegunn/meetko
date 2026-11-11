//! The real binary over stdio. Without models: the protocol, the refused requests and the fatal path.
//! With models (`MEETKO_BATCH_MODEL`, the directory holding both files, and `MEETKO_QWEN3_TEST_WAV`, a
//! 16 kHz mono PCM16 recording with speech): a real transcription, checked against the contract schema
//! the core uses. Without them that test skips, or fails under `MEETKO_REQUIRE_MODELS=1`.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_meetko-qwen3-asr");

fn run(lines: &[String], env: &[(&str, &str)]) -> (Vec<Value>, i32) {
    let mut cmd = Command::new(BIN);
    cmd.env_clear().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("the engine starts");
    {
        let mut stdin = child.stdin.take().unwrap();
        for l in lines {
            stdin.write_all(l.as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
        }
    }
    let out: Vec<Value> = BufReader::new(child.stdout.take().unwrap()).lines().map(|l| serde_json::from_str(&l.unwrap()).expect("stdout is protocol only")).collect();
    (out, child.wait().unwrap().code().unwrap_or(-1))
}

fn req(id: u64, method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
}

fn sha() -> String {
    "ab".repeat(32)
}

#[test]
fn protocol_and_refusals_without_a_model() {
    let tmp = tempfile::tempdir().unwrap();
    let lines = [
        req(1, "initialize", json!({ "contract": ["0.1"] })),
        req(2, "transcribe.run", json!({ "wav": "x.wav", "audio_sha256": sha() })),
        req(3, "transcribe.run", json!({ "wav": "x.wav", "audio_sha256": "nothex" })),
        req(4, "transcribe.run", json!({ "wav": "x.wav", "audio_sha256": sha(), "language": "korean" })),
        req(5, "nope", json!({})),
        req(6, "transcribe.run", json!({ "wav": "x.wav", "audio_sha256": sha(), "language": "ko-KR" })),
        req(7, "shutdown", json!({})),
    ];
    let (out, code) = run(&lines, &[("MEETKO_BATCH_MODEL", tmp.path().to_str().unwrap())]);
    assert_eq!(code, 0);
    assert_eq!(out[0]["result"]["role"], "asr.batch");
    assert_eq!(out[0]["result"]["contract"], "0.1");
    assert!(out[0]["result"]["engine"]["version"].as_str().unwrap().contains("+crispasr-"));
    assert_eq!(out[1]["error"]["code"], -32010, "no model files: {}", out[1]);
    assert_eq!(out[2]["error"]["code"], -32602);
    assert_eq!(out[3]["error"]["code"], -32602);
    assert_eq!(out[4]["error"]["code"], -32601);
    assert_eq!(out[5]["error"]["code"], -32010, "a tag with a region is taken as its language; the model is what is missing");
    assert_eq!(out[6]["result"], json!({}));
}

#[test]
fn an_unsupported_contract_is_refused() {
    let (out, _) = run(&[req(1, "initialize", json!({ "contract": ["9.9"] }))], &[]);
    assert_eq!(out[0]["error"]["code"], -32001);
}

#[test]
fn a_line_that_is_not_utf8_ends_the_engine() {
    let mut cmd = Command::new(BIN);
    let mut child = cmd.env_clear().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"\xff\xfe\n").unwrap();
    let out: Vec<Value> = BufReader::new(child.stdout.take().unwrap()).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    assert_eq!(child.wait().unwrap().code(), Some(1));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["method"], "engine.error");
}

#[test]
fn a_real_transcription_meets_the_contract() {
    let (Ok(model), Ok(wav)) = (std::env::var("MEETKO_BATCH_MODEL"), std::env::var("MEETKO_QWEN3_TEST_WAV")) else {
        assert!(std::env::var("MEETKO_REQUIRE_MODELS").as_deref() != Ok("1"), "MEETKO_BATCH_MODEL and MEETKO_QWEN3_TEST_WAV are required");
        eprintln!("skipped: set MEETKO_BATCH_MODEL and MEETKO_QWEN3_TEST_WAV");
        return;
    };
    let lines = [req(1, "initialize", json!({ "contract": ["0.1"] })), req(2, "transcribe.run", json!({ "wav": wav, "audio_sha256": sha(), "language": "ko" })), req(3, "shutdown", json!({}))];
    let (out, code) = run(&lines, &[("MEETKO_BATCH_MODEL", &model)]);
    assert_eq!(code, 0);
    let progress: Vec<u64> = out.iter().filter(|m| m["method"] == "transcribe.progress").map(|m| m["params"]["percent"].as_u64().unwrap()).collect();
    assert_eq!(progress.last(), Some(&100));
    assert!(progress.windows(2).all(|w| w[0] <= w[1]));
    let r = &out.iter().find(|m| m["id"] == 2).expect("a reply")["result"];
    let errs = meetko_core::contract::schema_errors(meetko_core::contract::engine_result_validator(), r);
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!(r["audio_sha256"], sha());
    assert_eq!(r["network"], false);
    assert_eq!(r["word_timing"], "aligned");
    assert_eq!((r["params"]["spread_words"].as_u64(), r["params"]["unaligned_pieces"].as_u64()), (Some(0), Some(0)), "every word measured by the aligner");
    assert_eq!(r["params"]["capped_pieces"], 0);
    assert_eq!(r["params"]["language_requested"], "ko");
    let words = r["words"].as_array().unwrap();
    assert!(!words.is_empty());
    let end = r["coverage"][0]["end_ms"].as_i64().unwrap();
    let mut prev = 0;
    for w in words {
        let (s, e) = (w["start_ms"].as_i64().unwrap(), w["end_ms"].as_i64().unwrap());
        assert!(s >= prev && e >= s && e <= end, "{w}");
        prev = s;
    }
    assert!(words.iter().any(|w| w["text"].as_str().unwrap().chars().any(|c| ('가'..='힣').contains(&c))), "Korean words");
}

/// The core's side with the real engine: the model is staged and checked against its pin.
#[test]
fn the_core_runs_the_engine_on_the_pinned_model() {
    use meetko_core::batch::{BatchConfig, BatchProcess};
    use meetko_core::whisper::{BatchEngine, BatchRequest};
    let (Ok(model), Ok(wav)) = (std::env::var("MEETKO_BATCH_MODEL"), std::env::var("MEETKO_QWEN3_TEST_WAV")) else {
        assert!(std::env::var("MEETKO_REQUIRE_MODELS").as_deref() != Ok("1"), "MEETKO_BATCH_MODEL and MEETKO_QWEN3_TEST_WAV are required");
        eprintln!("skipped: set MEETKO_BATCH_MODEL and MEETKO_QWEN3_TEST_WAV");
        return;
    };
    let cfg = BatchConfig { engine_bin: BIN.into(), model_dir: model.into(), unpinned: false, timeout: std::time::Duration::from_secs(600), sandbox: meetko_core::batch::sandbox_available() };
    let p = BatchProcess::new(cfg);
    let tmp = tempfile::tempdir().unwrap();
    let req = BatchRequest { wav: std::path::Path::new(&wav), audio_sha256: &sha(), language: "ko", language_explicit: false, channel: 0, channels: 1, data_dir: tmp.path() };
    let steps = std::sync::Mutex::new(Vec::new());
    let r = p.transcribe(&req, &|s| steps.lock().unwrap().push(s.percent), &std::sync::atomic::AtomicBool::new(false)).unwrap();
    assert_eq!(r["params"]["model_pinned"], true);
    // the default language is not forced on the model; the core says how it ran the engine
    assert_eq!(r["params"]["language"], "auto");
    if meetko_core::batch::sandbox_available() {
        assert_eq!(r["params"]["sandbox"], "macos-sbpl");
    }
    assert_eq!(r["model"]["id"], "qwen3-asr-1.7b-crispasr-q8_0");
    assert!(meetko_core::util::is_sha256(r["model"]["sha256"].as_str().unwrap()));
    assert_eq!(steps.lock().unwrap().last(), Some(&100));
    let errs = meetko_core::contract::schema_errors(meetko_core::contract::engine_result_validator(), &r);
    assert!(errs.is_empty(), "{errs:?}");
}
