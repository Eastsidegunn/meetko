//! meetko asr.stream engine: sherpa-onnx online transducer over NDJSON JSON-RPC 2.0 on stdio.
//!
//! Requests: initialize, stream.open, stream.close, shutdown. Notification: stream.push
//! (a 100 ms PCM16 chunk, base64). The engine answers with `stream.event` notifications whose
//! params are contract `asr_stream_event` objects. Logs go to stderr; stdout carries protocol only.
//! The model directory comes from MEETKO_STREAM_MODEL, never from a request.

mod events;
mod ffi;

use std::ffi::CString;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use base64::Engine as _;
use serde_json::{json, Value};

const CONTRACT: &str = "0.1";
const ENGINE_ID: &str = "meetko/sherpa-stream";
/// the Python experiment's values (reference/adapters.py docstring)
const ENDPOINT: ffi::Endpoint = ffi::Endpoint { rule1_min_trailing_silence: 2.4, rule2_min_trailing_silence: 0.8, rule3_min_utterance_length: 20.0 };
const THREADS: i32 = 2;
/// a segment whose text nears the schema's 4000 characters is closed by force
const MAX_SEGMENT_CHARS: usize = 3000;
const MODEL_FILES: [&str; 4] = ["encoder-epoch-99-avg-1.int8.onnx", "decoder-epoch-99-avg-1.onnx", "joiner-epoch-99-avg-1.int8.onnx", "tokens.txt"];

fn out(v: &Value) {
    let mut so = std::io::stdout().lock();
    let _ = writeln!(so, "{v}");
    let _ = so.flush();
}

/// A protocol failure the core must treat as the engine failing: say so, then stop. Dropping
/// input silently would shift `fed` and make every later timestamp early.
fn fatal(message: &str) -> ! {
    out(&json!({ "jsonrpc": "2.0", "method": "stream.error", "params": { "message": message } }));
    eprintln!("fatal: {message}");
    std::process::exit(1);
}

fn model_dir() -> Result<PathBuf, String> {
    let d = std::env::var_os("MEETKO_STREAM_MODEL").filter(|v| !v.is_empty()).ok_or("MEETKO_STREAM_MODEL is not set")?;
    Ok(PathBuf::from(d))
}

fn model_files(dir: &std::path::Path) -> Result<ffi::ModelFiles, String> {
    let c = |name: &str| -> Result<CString, String> {
        let p = dir.join(name);
        if !p.is_file() {
            return Err(format!("model file {name} is missing in MEETKO_STREAM_MODEL"));
        }
        CString::new(p.to_string_lossy().as_bytes()).map_err(|_| "model path contains NUL".to_string())
    };
    Ok(ffi::ModelFiles { encoder: c(MODEL_FILES[0])?, decoder: c(MODEL_FILES[1])?, joiner: c(MODEL_FILES[2])?, tokens: c(MODEL_FILES[3])? })
}

/// One utterance-by-utterance decode loop. `fed` counts the samples given to the model, which is
/// the audio clock: `emitted_at_ms` is how much audio the engine had when it said something.
struct Session {
    rec: ffi::Recognizer,
    fed: u64,
    segment: i64,
    revision: i64,
    seg_start_s: f64,
    last_text: String,
}

impl Session {
    fn fed_ms(&self) -> i64 {
        (self.fed * 1000 / 16000) as i64
    }

    fn emit(&mut self, is_final: bool, snap: &ffi::Snapshot) {
        let base = if snap.start_time >= 0.0 { snap.start_time } else { self.seg_start_s };
        let ev = events::event(&events::EventIn {
            segment: self.segment,
            revision: self.revision,
            is_final,
            base_s: base,
            text: &snap.text,
            tokens: &snap.tokens,
            timestamps: &snap.timestamps,
            fed_ms: self.fed_ms(),
        });
        self.revision += 1;
        out(&json!({ "jsonrpc": "2.0", "method": "stream.event", "params": ev }));
    }

    fn close_segment(&mut self, snap: &ffi::Snapshot) {
        if !snap.text.trim().is_empty() {
            self.emit(true, snap);
            self.segment += 1;
            self.revision = 0;
        }
        self.rec.reset();
        self.seg_start_s = self.fed as f64 / 16000.0;
        self.last_text.clear();
    }

    fn push(&mut self, samples: &[f32]) {
        self.rec.accept(samples);
        self.fed += samples.len() as u64;
        self.rec.decode_ready();
        let snap = self.rec.snapshot();
        let endpoint = self.rec.is_endpoint() || snap.text.chars().count() > MAX_SEGMENT_CHARS;
        if endpoint {
            self.close_segment(&snap);
        } else if snap.text != self.last_text {
            self.last_text = snap.text.clone();
            self.emit(false, &snap);
        }
    }

    /// End of audio: flush what the model still holds as a last final.
    fn finish(&mut self) {
        self.rec.input_finished();
        self.rec.decode_ready();
        let snap = self.rec.snapshot();
        self.close_segment(&snap);
    }
}

fn decode_pcm(b64: &str) -> Option<Vec<f32>> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    if bytes.len() % 2 != 0 {
        return None; // half a sample: the sender broke the framing
    }
    Some(bytes.chunks_exact(2).map(|p| f32::from(i16::from_le_bytes([p[0], p[1]])) / 32768.0).collect())
}

fn reply(id: &Value, result: Result<Value, (i64, String)>) {
    match result {
        Ok(r) => out(&json!({ "jsonrpc": "2.0", "id": id, "result": r })),
        Err((code, message)) => out(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })),
    }
}

fn main() {
    let model = model_dir();
    let model_id = model.as_ref().ok().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut session: Option<Session> = None;
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
                match params["pcm_s16le_b64"].as_str().and_then(decode_pcm) {
                    Some(samples) => s.push(&samples),
                    None => fatal("stream.push carried invalid PCM"),
                }
            }
            ("initialize", Some(id)) => {
                let offered = params["contract"].as_array().is_some_and(|a| a.iter().any(|c| c == CONTRACT));
                reply(id, if !offered { Err((-32001, "unsupported contract".into())) } else {
                    Ok(json!({
                        "contract": CONTRACT,
                        "engine": { "id": ENGINE_ID, "version": format!("{}+sherpa-onnx-{}", env!("CARGO_PKG_VERSION"), ffi::version()) },
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
                    let dir = model.clone().map_err(|e| (-32010, e))?;
                    let files = model_files(&dir).map_err(|e| (-32010, e))?;
                    let rec = ffi::Recognizer::new(&files, ENDPOINT, THREADS).map_err(|e| (-32010, e))?;
                    session = Some(Session { rec, fed: 0, segment: 0, revision: 0, seg_start_s: 0.0, last_text: String::new() });
                    Ok(json!({ "params": {
                        "decoding": "greedy_search", "num_threads": THREADS,
                        "endpoint": { "rule1_min_trailing_silence_s": ENDPOINT.rule1_min_trailing_silence,
                                      "rule2_min_trailing_silence_s": ENDPOINT.rule2_min_trailing_silence,
                                      "rule3_min_utterance_length_s": ENDPOINT.rule3_min_utterance_length },
                    }}))
                })();
                reply(id, r);
            }
            ("stream.close", Some(id)) => {
                let r = match session.take() {
                    Some(mut s) => {
                        s.finish();
                        Ok(json!({ "fed_ms": s.fed_ms(), "segments": s.segment }))
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
