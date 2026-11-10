//! meetko diarization engine: NVIDIA Nemotron-3-Diarization (NeMo-Speech.cpp) over NDJSON JSON-RPC
//! 2.0 on stdio. Batch: one request names a recording, the reply is a contract `diarization`
//! engine result.
//!
//! Requests: initialize, diarize.run `{wav, audio_sha256}`, shutdown. The wav is a mono PCM16 file
//! the caller has already hashed (read as it is diarized; `audio_sha256` is echoed into the result; this engine does not
//! hash). The model comes from MEETKO_DIAR_MODEL (a directory holding the file named by
//! `models.toml`, or the .gguf itself), never from a request. Logs go to stderr; stdout carries
//! protocol only. A failure of the library, or a line that is not UTF-8, ends the process after one
//! `engine.error` notification (exit code 1, see `rpc::fatal`); a request the engine cannot serve
//! (no model, an unusable wav) is an ordinary error reply and the process goes on.
//!
//! The model is not hashed here: the core copies it into a private directory while hashing it
//! against `models.toml` and hands this process that copy (crates/meetko-core/src/live.rs).

use std::io::BufRead;
use std::path::PathBuf;

use meetko_nemotron_stream::{diar, ffi, rpc};
use rpc::reply;
use serde_json::{json, Value};

const CONTRACT: &str = "0.1";
const ENGINE_ID: &str = "meetko/nemotron-diar";
/// the file name `models.toml` pins inside the model directory
const MODEL_FILE: &str = "Nemotron-3-Diarization.q8_0.gguf";
const MODEL_ID: &str = "nemotron-3-diarization-q8_0";
/// the macOS build is Metal; the Linux and Windows CPU builds have no GPU 0
const GPU: i32 = if cfg!(target_os = "macos") { 0 } else { -1 };

fn fatal(message: &str) -> ! {
    rpc::fatal("engine.error", message)
}

/// MEETKO_DIAR_MODEL is the model directory (or the .gguf itself, for a run by hand).
fn model_file() -> Result<PathBuf, String> {
    let d = std::env::var_os("MEETKO_DIAR_MODEL").filter(|v| !v.is_empty()).ok_or("MEETKO_DIAR_MODEL is not set")?;
    let p = PathBuf::from(d);
    let p = if p.is_dir() { p.join(MODEL_FILE) } else { p };
    if p.is_file() {
        Ok(p)
    } else {
        Err(format!("{MODEL_FILE} is missing in MEETKO_DIAR_MODEL"))
    }
}

/// The engine result for one recording.
fn run(model: &mut Option<diar::Diarizer>, params: &Value) -> Result<Value, (i64, String)> {
    let wav = params["wav"].as_str().filter(|w| !w.is_empty()).ok_or((-32602, "wav is required".to_string()))?;
    let sha = params["audio_sha256"].as_str().filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).ok_or((-32602, "audio_sha256 must be 64 lowercase hex digits".to_string()))?;
    let mut pcm = diar::open_wav(std::path::Path::new(wav)).map_err(|e| (-32010, e.0))?;
    if model.is_none() {
        let file = model_file().map_err(|e| (-32010, e))?;
        let path = std::ffi::CString::new(file.to_string_lossy().as_bytes()).map_err(|_| (-32010, "model path contains NUL".to_string()))?;
        *model = Some(diar::Diarizer::new(&path, GPU).unwrap_or_else(|e| fatal(&e)));
    }
    let d = model.as_mut().expect("loaded above");
    let duration_ms = pcm.duration_ms();
    let segments = d.run(&mut pcm).unwrap_or_else(|e| fatal(&e));
    Ok(json!({
        "contract": CONTRACT, "kind": "diarization",
        "engine": { "id": ENGINE_ID, "version": format!("{}+nemo-speech-{}", env!("CARGO_PKG_VERSION"), ffi::version()) },
        "model": { "id": MODEL_ID },
        "audio_sha256": sha,
        "params": { "mode": "streaming", "preset": "model default", "push_ms": diar::PUSH_MS, "segmentation": "library defaults", "max_speakers": d.capacity() },
        "network": false,
        "turns": diar::turns(&segments, duration_ms),
    }))
}

fn main() {
    let mut model: Option<diar::Diarizer> = None;
    for line in std::io::stdin().lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => fatal("stdin carried a line that is not UTF-8"),
            Err(_) => break, // stdin closed
        };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            eprintln!("not JSON, ignored");
            continue;
        };
        let (id, method) = (msg.get("id").cloned(), msg["method"].as_str().unwrap_or(""));
        let params = &msg["params"];
        match (method, &id) {
            ("initialize", Some(id)) => {
                let offered = params["contract"].as_array().is_some_and(|a| a.iter().any(|c| c == CONTRACT));
                reply(id, if !offered { Err((-32001, "unsupported contract".into())) } else {
                    Ok(json!({
                        "contract": CONTRACT,
                        "engine": { "id": ENGINE_ID, "version": format!("{}+nemo-speech-{}", env!("CARGO_PKG_VERSION"), ffi::version()) },
                        "role": "diarization", "model": { "id": MODEL_ID },
                    }))
                });
            }
            ("diarize.run", Some(id)) => reply(id, run(&mut model, params)),
            ("shutdown", Some(id)) => {
                reply(id, Ok(json!({})));
                break;
            }
            (_, Some(id)) => reply(id, Err((-32601, "method not found".into()))),
            _ => {}
        }
    }
    drop(model); // before the static destructors of the ggml backends run
}
