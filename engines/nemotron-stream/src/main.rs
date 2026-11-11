//! meetko asr.stream engine: NVIDIA Nemotron streaming RNNT (NeMo-Speech.cpp) over NDJSON
//! JSON-RPC 2.0 on stdio.
//!
//! Requests: initialize, stream.open, stream.close, shutdown. Notification: stream.push
//! (a 100 ms PCM16 chunk, base64). The engine answers with `stream.event` notifications whose
//! params are contract `asr_stream_event` objects. Logs go to stderr; stdout carries protocol only.
//! The model comes from MEETKO_STREAM_MODEL, never from a request; the language from the
//! `stream.open` request (the core sets it from MEETKO_STREAM_LANGUAGE) or, run by hand, from
//! MEETKO_STREAM_LANGUAGE itself.
//!
//! The model is not hashed here: the core copies it into a private directory while hashing it
//! against `models.toml` and hands this process that copy (crates/meetko-core/src/live.rs).

mod events;
mod session;

use std::io::BufRead;
use std::path::PathBuf;

use meetko_nemotron_stream::{ffi, rpc};
use rpc::{out, reply};
use serde_json::{json, Value};
use session::Session;

/// Write out the events a session collected.
fn flush<R: session::Recognize>(s: &mut Session<R>) {
    for ev in s.out.drain(..) {
        out(&json!({ "jsonrpc": "2.0", "method": "stream.event", "params": ev }));
    }
}

const CONTRACT: &str = "0.1";
const ENGINE_ID: &str = "meetko/nemotron-stream";
/// the file name `models.toml` pins inside the model directory
const MODEL_FILE: &str = "model.q8_0.gguf";
const SETTINGS: ffi::Settings = ffi::Settings {
    // the macOS build is Metal; the Linux and Windows CPU builds have no GPU 0
    gpu: if cfg!(target_os = "macos") { 0 } else { -1 },
    // the library's low-latency preset (~160 ms)
    chunk_s: 0.16,
    right_context: 1,
    // the same trailing silence the transducer engine ends an utterance on
    eou_ms: 800,
};

/// A protocol failure the core must treat as the engine failing: say so, then stop. Dropping
/// input silently would shift `fed` and make every later timestamp early (`rpc::fatal` says how
/// the process ends).
fn fatal(message: &str) -> ! {
    rpc::fatal("stream.error", message)
}

/// MEETKO_STREAM_MODEL is the model directory (or the .gguf itself, for a run by hand).
fn model_file() -> Result<PathBuf, String> {
    let d = std::env::var_os("MEETKO_STREAM_MODEL").filter(|v| !v.is_empty()).ok_or("MEETKO_STREAM_MODEL is not set")?;
    let p = PathBuf::from(d);
    let p = if p.is_dir() { p.join(MODEL_FILE) } else { p };
    if p.is_file() {
        Ok(p)
    } else {
        Err(format!("{MODEL_FILE} is missing in MEETKO_STREAM_MODEL"))
    }
}

fn main() {
    let model = model_file();
    let model_id = std::env::var_os("MEETKO_STREAM_MODEL").map(PathBuf::from).and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
    let mut session: Option<Session<ffi::Decoder>> = None;
    for line in std::io::stdin().lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => fatal("stdin carried a line that is not UTF-8"),
            Err(_) => break, // stdin closed
        };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            if session.is_some() {
                fatal("stdin carried a line that is not JSON"); // it may have been a push
            }
            eprintln!("not JSON, ignored");
            continue;
        };
        let (id, method) = (msg.get("id").cloned(), msg["method"].as_str().unwrap_or(""));
        let params = &msg["params"];
        match (method, &id) {
            ("stream.push", _) => {
                let Some(s) = session.as_mut() else { fatal("stream.push without an open stream") };
                match params["pcm_s16le_b64"].as_str().and_then(rpc::decode_pcm) {
                    Some(samples) => {
                        let r = s.push(&samples);
                        flush(s);
                        if let Err(e) = r {
                            fatal(&e);
                        }
                    }
                    None => fatal("stream.push carried invalid PCM"),
                }
            }
            ("initialize", Some(id)) => {
                let offered = params["contract"].as_array().is_some_and(|a| a.iter().any(|c| c == CONTRACT));
                reply(id, if !offered { Err((-32001, "unsupported contract".into())) } else {
                    Ok(json!({
                        "contract": CONTRACT,
                        "engine": { "id": ENGINE_ID, "version": format!("{}+nemo-speech-{}", env!("CARGO_PKG_VERSION"), ffi::version()) },
                        "role": "asr.stream", "model": { "id": model_id },
                    }))
                });
            }
            ("stream.open", Some(id)) => {
                let r = (|| {
                    if session.is_some() {
                        return Err((-32602, "a stream is already open".to_string()));
                    }
                    if params["sample_rate"] != 16000 {
                        return Err((-32602, "only 16000 Hz is supported".into()));
                    }
                    let requested = params["language"].as_str().map(str::to_string).or_else(|| std::env::var("MEETKO_STREAM_LANGUAGE").ok()).unwrap_or_default();
                    let language = ffi::language_code(&requested).map_err(|e| (-32602, e))?;
                    let file = model.clone().map_err(|e| (-32010, e))?;
                    let path = std::ffi::CString::new(file.to_string_lossy().as_bytes()).map_err(|_| (-32010, "model path contains NUL".to_string()))?;
                    let dec = ffi::Decoder::new(&path, &language, &SETTINGS).map_err(|e| (-32010, e))?;
                    session = Some(Session::new(dec));
                    Ok(json!({ "params": {
                        "decoding": "rnnt_greedy", "language": language.to_string_lossy(),
                        "chunk_ms": (SETTINGS.chunk_s * 1000.0).round() as i64, "rnnt_right_context": SETTINGS.right_context,
                        "endpoint": { "stop_history_eou_ms": SETTINGS.eou_ms },
                        // a fresh library stream per utterance, fed again from the last word's end (src/session.rs)
                        "stream_per_utterance": true,
                    }}))
                })();
                reply(id, r);
            }
            ("stream.close", Some(id)) => {
                let r = match session.take() {
                    Some(mut s) => {
                        let r = s.finish();
                        flush(&mut s);
                        if let Err(e) = r {
                            fatal(&e);
                        }
                        Ok(json!({ "fed_ms": s.fed_ms(), "segments": s.segments() }))
                    }
                    None => Err((-32602, "no stream is open".to_string())),
                };
                reply(id, r);
            }
            ("shutdown", Some(id)) => {
                reply(id, Ok(json!({})));
                break;
            }
            (_, Some(id)) => reply(id, Err((-32601, "method not found".into()))),
            _ => {}
        }
    }
}
