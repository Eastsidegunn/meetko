//! meetko diarization engine: NVIDIA Nemotron-3-Diarization (NeMo-Speech.cpp) over NDJSON JSON-RPC
//! 2.0 on stdio. Batch: one request names a recording, the reply is a contract `diarization`
//! engine result. Live: a session is opened, fed PCM16 while a capture records, reports its turns
//! as they grow, and is closed with the same result.
//!
//! Requests: initialize, diarize.run `{wav, audio_sha256}`, diarize.open `{sample_rate}`,
//! diarize.close `{audio_sha256?}`, shutdown. Notification in: diarize.push `{pcm_s16le_b64}`.
//! Notification out: diarize.event `{keep, turns, labelled_ms, emitted_at_ms}`.
//!
//! The wav of `diarize.run` is a mono PCM16 file the caller has already hashed (read as it is
//! diarized; `audio_sha256` is echoed into the result; this engine does not hash). The model comes
//! from MEETKO_DIAR_MODEL (a directory holding the file named by `models.toml`, or the .gguf
//! itself), never from a request. Logs go to stderr; stdout carries protocol only. A failure of the
//! library, a line that is not UTF-8, or (as in the stream engine) a push the engine cannot take
//! (invalid PCM, no open session, a line that is not JSON while a session is open) ends the
//! process after one `engine.error` notification (exit code 1, see `rpc::fatal`): dropping audio
//! silently would shift every later turn. A request the engine cannot serve (no model, an unusable
//! wav) is an ordinary error reply and the process goes on.
//!
//! The model is not hashed here: the core copies it into a private directory while hashing it
//! against `models.toml` and hands this process that copy (crates/meetko-core/src/live.rs).

use std::cell::OnceCell;
use std::io::BufRead;
use std::path::PathBuf;

use meetko_nemotron_stream::{diar, ffi, rpc};
use rpc::{out, reply};
use serde_json::{json, Value};

const CONTRACT: &str = "0.1";
const ENGINE_ID: &str = "meetko/nemotron-diar";
/// the file name `models.toml` pins inside the model directory
const MODEL_FILE: &str = "Nemotron-3-Diarization.q8_0.gguf";
const MODEL_ID: &str = "nemotron-3-diarization-q8_0";
/// the macOS build is Metal; the Linux and Windows CPU builds have no GPU 0
const GPU: i32 = if cfg!(target_os = "macos") { 0 } else { -1 };
/// A live session reports its turns at most this often (in audio time labelled). Each report is
/// also the heartbeat the core measures the session's lag by, so one goes out even when nothing
/// changed.
const EVENT_MS: i64 = 1000;

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

/// The model, loaded on the first request that needs it and kept. A request without a usable
/// model file is refused; a file the library cannot load ends the process.
fn loaded(model: &OnceCell<diar::Diarizer>) -> Result<&diar::Diarizer, (i64, String)> {
    if model.get().is_none() {
        let file = model_file().map_err(|e| (-32010, e))?;
        let path = std::ffi::CString::new(file.to_string_lossy().as_bytes()).map_err(|_| (-32010, "model path contains NUL".to_string()))?;
        let _ = model.set(diar::Diarizer::new(&path, GPU).unwrap_or_else(|e| fatal(&e)));
    }
    Ok(model.get().expect("loaded above"))
}

fn sha_param(params: &Value) -> Option<Result<&str, (i64, String)>> {
    let v = params.get("audio_sha256").filter(|v| !v.is_null())?;
    Some(v.as_str().filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).ok_or((-32602, "audio_sha256 must be 64 lowercase hex digits".to_string())))
}

fn engine_info() -> Value {
    json!({ "id": ENGINE_ID, "version": format!("{}+nemo-speech-{}", env!("CARGO_PKG_VERSION"), ffi::version()) })
}

/// What the pass did, the same for a recording and a live session (`push_ms` is how the batch
/// pass slices the file; a live session takes whatever the caller pushes).
fn pass_params(d: &diar::Diarizer, live: bool) -> Value {
    let mut p = json!({ "mode": "streaming", "preset": "model default", "segmentation": "library defaults", "max_speakers": d.capacity() });
    if live {
        p["live"] = json!(true);
    } else {
        p["push_ms"] = json!(diar::PUSH_MS);
    }
    p
}

/// A contract `diarization` result. `audio_sha256` is left out when the caller of a live session
/// did not name the audio: it is then the caller's to add, for the file it fed (the core does).
fn result(d: &diar::Diarizer, sha: Option<&str>, live: bool, turns: Vec<Value>) -> Value {
    let mut r = json!({
        "contract": CONTRACT, "kind": "diarization", "engine": engine_info(), "model": { "id": MODEL_ID },
        "params": pass_params(d, live), "network": false, "turns": turns,
    });
    if let Some(s) = sha {
        r["audio_sha256"] = json!(s);
    }
    r
}

/// The engine result for one recording.
fn run(model: &OnceCell<diar::Diarizer>, params: &Value) -> Result<Value, (i64, String)> {
    let wav = params["wav"].as_str().filter(|w| !w.is_empty()).ok_or((-32602, "wav is required".to_string()))?;
    let sha = sha_param(params).unwrap_or_else(|| Err((-32602, "audio_sha256 is required".to_string())))?;
    let mut pcm = diar::open_wav(std::path::Path::new(wav)).map_err(|e| (-32010, e.0))?;
    let d = loaded(model)?;
    let duration_ms = pcm.duration_ms();
    let segments = d.run(&mut pcm).unwrap_or_else(|e| fatal(&e));
    Ok(result(d, Some(sha), false, diar::turns(&segments, duration_ms)))
}

/// A live session: audio pushed while a capture records.
struct Live<'a> {
    stream: diar::Stream<'a>,
    rate: i32,
    /// samples pushed: the audio clock of `emitted_at_ms`
    pushed: u64,
    /// labelled audio at the last report
    reported_ms: i64,
    /// the turns as last reported
    sent: Vec<Value>,
}

impl Live<'_> {
    fn pushed_ms(&self) -> i64 {
        (self.pushed * 1000 / self.rate as u64) as i64
    }

    fn turns(&self) -> Vec<Value> {
        diar::turns(&self.stream.segments().unwrap_or_else(|e| fatal(&e)), self.pushed_ms())
    }

    fn push(&mut self, samples: &[f32]) {
        self.stream.push(samples, self.rate).unwrap_or_else(|e| fatal(&e));
        self.pushed += samples.len() as u64;
        let labelled = self.stream.labelled_ms();
        if labelled - self.reported_ms < EVENT_MS {
            return;
        }
        self.reported_ms = labelled;
        let next = self.turns();
        let keep = diar::kept(&self.sent, &next);
        let ev = json!({ "keep": keep, "turns": &next[keep..], "labelled_ms": labelled, "emitted_at_ms": self.pushed_ms() });
        self.sent = next;
        out(&json!({ "jsonrpc": "2.0", "method": "diarize.event", "params": ev }));
    }
}

fn main() {
    // declared before the session, so the session (which borrows it) is dropped first
    let model: OnceCell<diar::Diarizer> = OnceCell::new();
    let mut live: Option<Live> = None;
    for line in std::io::stdin().lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => fatal("stdin carried a line that is not UTF-8"),
            Err(_) => break, // stdin closed
        };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            if live.is_some() {
                fatal("stdin carried a line that is not JSON"); // it may have been a push
            }
            eprintln!("not JSON, ignored");
            continue;
        };
        let (id, method) = (msg.get("id").cloned(), msg["method"].as_str().unwrap_or(""));
        let params = &msg["params"];
        match (method, &id) {
            ("diarize.push", _) => {
                let Some(s) = live.as_mut() else { fatal("diarize.push without an open session") };
                match params["pcm_s16le_b64"].as_str().and_then(rpc::decode_pcm) {
                    Some(samples) => s.push(&samples),
                    None => fatal("diarize.push carried invalid PCM"),
                }
            }
            ("initialize", Some(id)) => {
                let offered = params["contract"].as_array().is_some_and(|a| a.iter().any(|c| c == CONTRACT));
                reply(id, if !offered { Err((-32001, "unsupported contract".into())) } else {
                    Ok(json!({ "contract": CONTRACT, "engine": engine_info(), "role": "diarization", "model": { "id": MODEL_ID } }))
                });
            }
            ("diarize.run", Some(id)) => reply(id, run(&model, params)),
            ("diarize.open", Some(id)) => {
                let r = (|| {
                    if live.is_some() {
                        return Err((-32602, "a session is already open".to_string()));
                    }
                    let rate = params["sample_rate"].as_i64().filter(|r| (8000..=96000).contains(r)).ok_or((-32602, "sample_rate must be between 8000 and 96000".to_string()))?;
                    let d = loaded(&model)?;
                    let stream = d.open().unwrap_or_else(|e| fatal(&e));
                    live = Some(Live { stream, rate: rate as i32, pushed: 0, reported_ms: 0, sent: vec![] });
                    Ok(json!({ "params": pass_params(d, true) }))
                })();
                reply(id, r);
            }
            ("diarize.close", Some(id)) => {
                let r = (|| {
                    let sha = sha_param(params).transpose()?;
                    let mut s = live.take().ok_or((-32602, "no session is open".to_string()))?;
                    s.stream.finish().unwrap_or_else(|e| fatal(&e));
                    let turns = s.turns();
                    let fed_ms = s.pushed_ms();
                    drop(s);
                    let d = model.get().expect("a session implies a model");
                    let mut r = result(d, sha, true, turns);
                    r["params"]["fed_ms"] = json!(fed_ms);
                    Ok(r)
                })();
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
    drop(live);
    drop(model); // before the static destructors of the ggml backends run
}
