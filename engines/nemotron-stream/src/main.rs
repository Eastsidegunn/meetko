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

use std::io::BufRead;
use std::path::PathBuf;

use base64::Engine as _;
use meetko_nemotron_stream::{ffi, rpc};
use rpc::{out, reply};
use serde_json::{json, Value};

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
/// a segment whose text nears the schema's 4000 characters is closed by force
const MAX_SEGMENT_CHARS: usize = 3000;

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

/// One utterance-by-utterance decode loop. `fed` counts the samples given to the model, which is
/// the audio clock: `emitted_at_ms` is how much audio the engine had when it said something.
struct Session {
    dec: ffi::Decoder,
    fed: u64,
    segment: i64,
    revision: i64,
    /// where the current segment begins as far as is known: the end of the previous final's last word
    seg_start_ms: i64,
    last_text: String,
}

impl Session {
    fn fed_ms(&self) -> i64 {
        (self.fed * 1000 / 16000) as i64
    }

    fn emit(&mut self, h: &ffi::Hypothesis) {
        let ev = events::event(&events::EventIn {
            segment: self.segment,
            revision: self.revision,
            is_final: h.is_final,
            start_ms: self.seg_start_ms,
            text: &h.text,
            words: &h.words,
            fed_ms: self.fed_ms(),
        });
        self.revision += 1;
        out(&json!({ "jsonrpc": "2.0", "method": "stream.event", "params": ev }));
    }

    fn handle(&mut self, h: ffi::Hypothesis) {
        if h.is_final {
            if !h.text.trim().is_empty() {
                self.emit(&h);
                self.segment += 1;
                self.revision = 0;
            }
            self.seg_start_ms = h.words.last().map_or(self.seg_start_ms, |w| w.end_ms.max(self.seg_start_ms));
            self.last_text.clear();
        } else if h.text != self.last_text {
            self.last_text = h.text.clone();
            self.emit(&h);
        }
    }

    /// Everything the model can say about the audio pushed so far.
    fn drain(&mut self) {
        loop {
            match self.dec.next() {
                Ok(Some(h)) => self.handle(h),
                Ok(None) => break,
                Err(e) => fatal(&e),
            }
        }
    }

    fn push(&mut self, samples: &[f32]) {
        if let Err(e) = self.dec.push(samples) {
            fatal(&e);
        }
        self.fed += samples.len() as u64;
        self.drain();
        if self.last_text.chars().count() > MAX_SEGMENT_CHARS {
            if let Err(e) = self.dec.force_endpoint() {
                fatal(&e);
            }
            self.drain();
        }
    }

    /// End of audio: flush what the model still holds as a last final.
    fn finish(&mut self) {
        if let Err(e) = self.dec.finish() {
            fatal(&e);
        }
        self.drain();
        if !self.last_text.trim().is_empty() {
            // the library said nothing final about the tail: keep the text, without words
            let h = ffi::Hypothesis { is_final: true, text: std::mem::take(&mut self.last_text), words: vec![] };
            self.handle(h);
        }
    }
}

fn decode_pcm(b64: &str) -> Option<Vec<f32>> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    if bytes.len() % 2 != 0 {
        return None; // half a sample: the sender broke the framing
    }
    Some(bytes.as_chunks::<2>().0.iter().map(|p| f32::from(i16::from_le_bytes(*p)) / 32768.0).collect())
}

fn main() {
    let model = model_file();
    let model_id = std::env::var_os("MEETKO_STREAM_MODEL").map(PathBuf::from).and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
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
                    session = Some(Session { dec, fed: 0, segment: 0, revision: 0, seg_start_ms: 0, last_text: String::new() });
                    Ok(json!({ "params": {
                        "decoding": "rnnt_greedy", "language": language.to_string_lossy(),
                        "chunk_ms": (SETTINGS.chunk_s * 1000.0).round() as i64, "rnnt_right_context": SETTINGS.right_context,
                        "endpoint": { "stop_history_eou_ms": SETTINGS.eou_ms },
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
