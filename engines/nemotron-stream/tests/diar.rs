//! The diarization binary over the stdio protocol. The tests that need the model and a recording
//! pass after saying so when they are not there, unless MEETKO_REQUIRE_MODELS=1 (as in CI), when
//! their absence fails. Inputs, by default from the repository's gitignored models/ and raw/:
//!   MEETKO_DIAR_MODEL      the model directory (holds Nemotron-3-Diarization.q8_0.gguf)
//!   MEETKO_DIAR_WAV        a 16 kHz mono PCM16 wav (default raw/meeting01/loopback.wav; its two
//!                          speakers are macOS TTS voices which the model merges into one)
//!   MEETKO_DIAR_MULTI_WAV  a recording with several real speakers (no default: an AIHub meeting clip)
//! The protocol tests (rejections, fatal paths) need only the binary. `-- --nocapture` shows the turns.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meetko_core::contract::{engine_result_validator, schema_errors};
use serde_json::{json, Value};

const SHA: &str = "ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34";

fn repo(p: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
}

fn env_path(k: &str) -> Option<PathBuf> {
    std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn required() -> bool {
    std::env::var_os("MEETKO_REQUIRE_MODELS").is_some_and(|v| v == "1")
}

/// `what` (a model directory or a wav) or None after saying so; a failure when the models are required.
fn input(what: &str, p: Option<PathBuf>) -> Option<PathBuf> {
    match p.filter(|p| p.exists()) {
        Some(p) => Some(p),
        None => {
            assert!(!required(), "{what} is missing, but MEETKO_REQUIRE_MODELS=1");
            eprintln!("skipped: {what} is not there (see the header of this file)");
            None
        }
    }
}

fn model() -> Option<PathBuf> {
    input("the diarization model directory", Some(env_path("MEETKO_DIAR_MODEL").unwrap_or_else(|| repo("models/nemotron-3-diarization"))))
}

fn wav_path() -> Option<PathBuf> {
    input("the wav", Some(env_path("MEETKO_DIAR_WAV").unwrap_or_else(|| repo("raw/meeting01/loopback.wav"))))
}

fn write_wav(path: &Path, channels: u16, frames: usize) {
    let mut b = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
    b.extend(16u32.to_le_bytes());
    b.extend([1u16, channels].iter().flat_map(|v| v.to_le_bytes()));
    b.extend(16000u32.to_le_bytes());
    b.extend((32000 * u32::from(channels)).to_le_bytes());
    b.extend((2 * channels).to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend((frames as u32 * 2 * u32::from(channels)).to_le_bytes());
    b.extend(vec![0u8; frames * 2 * channels as usize]);
    std::fs::write(path, b).unwrap();
}

struct Eng {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    stderr: Arc<Mutex<String>>,
    errors: Vec<Value>,
    next_id: u64,
}

impl Eng {
    /// `model`: the value of MEETKO_DIAR_MODEL, or the variable unset.
    fn start(model: Option<&Path>) -> Eng {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_meetko-nemotron-diar"));
        cmd.env_remove("MEETKO_DIAR_MODEL");
        if let Some(m) = model {
            cmd.env("MEETKO_DIAR_MODEL", m);
        }
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<Value>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                let v: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("stdout carried something that is not JSON: {line}"));
                let _ = tx.send(v);
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let (mut err, sink) = (child.stderr.take().unwrap(), stderr.clone());
        std::thread::spawn(move || {
            let (mut b, mut chunk) = (Vec::new(), [0u8; 4096]);
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                b.extend_from_slice(&chunk[..n]);
                *sink.lock().unwrap() = String::from_utf8_lossy(&b).into_owned();
            }
        });
        Eng { child, stdin, rx, stderr, errors: vec![], next_id: 0 }
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.stdin.write_all(bytes).and_then(|_| self.stdin.flush()).unwrap();
    }

    /// A request and its reply: Ok(result) or Err(error object).
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send_raw(format!("{}\n", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).as_bytes());
        loop {
            let v = self.rx.recv_timeout(Duration::from_secs(300)).expect("the engine answered");
            if v["id"] == json!(id) {
                return if v.get("error").is_some() { Err(v["error"].clone()) } else { Ok(v["result"].clone()) };
            }
            self.note(v);
        }
    }

    fn note(&mut self, v: Value) {
        if v["method"] == "engine.error" {
            self.errors.push(v["params"].clone());
        }
    }

    fn init(&mut self) -> Value {
        self.call("initialize", json!({ "contract": ["0.1"] })).unwrap()
    }

    fn diarize(&mut self, wav: &Path) -> Result<Value, Value> {
        self.call("diarize.run", json!({ "wav": wav, "audio_sha256": SHA }))
    }

    /// Read until the engine closes stdout, then its exit code.
    fn die(&mut self) -> Option<i32> {
        while let Ok(v) = self.rx.recv_timeout(Duration::from_secs(60)) {
            self.note(v);
        }
        let code = self.child.wait().unwrap().code();
        std::thread::sleep(Duration::from_millis(100)); // the stderr reader catches up
        code
    }

    fn shutdown(&mut self) {
        self.call("shutdown", json!({})).unwrap();
        assert_eq!(self.die(), Some(0), "a polite end is exit code 0");
    }
}

/// Checks that hold for any recording, and the speakers found.
fn check_result(r: &Value, wav: &Path) -> Vec<String> {
    let errs = schema_errors(engine_result_validator(), r);
    assert!(errs.is_empty(), "{errs:?}");
    assert_eq!((r["kind"].as_str(), r["audio_sha256"].as_str(), r["network"].as_bool()), (Some("diarization"), Some(SHA), Some(false)));
    assert_eq!(r["params"]["max_speakers"], 8);
    let duration_ms = (std::fs::metadata(wav).unwrap().len().saturating_sub(44) / 32) as i64;
    let turns = r["turns"].as_array().unwrap();
    assert!(!turns.is_empty(), "no speech found");
    let mut prev = 0;
    for t in turns {
        let (s, e) = (t["start_ms"].as_i64().unwrap(), t["end_ms"].as_i64().unwrap());
        assert!(prev <= s && s < e && e <= duration_ms + 100, "turn out of order or outside the audio ({duration_ms} ms): {t}");
        assert!(t["speaker"].as_str().unwrap().starts_with("spk"));
        prev = s;
    }
    let mut speakers: Vec<String> = turns.iter().map(|t| t["speaker"].as_str().unwrap().to_string()).collect();
    speakers.sort();
    speakers.dedup();
    speakers
}

#[test]
fn a_real_recording_through_the_real_binary() {
    let (Some(model), Some(wav)) = (model(), wav_path()) else { return };
    let mut eng = Eng::start(Some(&model));
    let init = eng.init();
    assert_eq!((init["role"].as_str(), init["contract"].as_str()), (Some("diarization"), Some("0.1")));
    let start = Instant::now();
    let first = eng.diarize(&wav).unwrap();
    let wall = start.elapsed().as_secs_f64();
    let speakers = check_result(&first, &wav);
    // the model stays loaded for the next request and the answer does not change
    let second = eng.diarize(&wav).unwrap();
    assert_eq!(first["turns"], second["turns"], "the same audio, the same turns");
    eprintln!("{} turns, speakers {speakers:?}, {wall:.1} s including the model load; first turns {}", first["turns"].as_array().unwrap().len(), first["turns"].as_array().unwrap().iter().take(3).map(|t| t.to_string()).collect::<Vec<_>>().join(" "));
    eng.shutdown();
    assert!(eng.errors.is_empty());
    assert!(!speakers.is_empty());
}

#[test]
fn a_meeting_with_several_speakers_is_told_apart() {
    let Some(model) = model() else { return };
    let Some(wav) = input("MEETKO_DIAR_MULTI_WAV (a recording with several speakers)", env_path("MEETKO_DIAR_MULTI_WAV")) else { return };
    let mut eng = Eng::start(Some(&model));
    eng.init();
    let start = Instant::now();
    let r = eng.diarize(&wav).unwrap();
    let speakers = check_result(&r, &wav);
    eprintln!("{} turns, speakers {speakers:?}, {:.1} s", r["turns"].as_array().unwrap().len(), start.elapsed().as_secs_f64());
    assert!(speakers.len() >= 2, "one speaker found in a recording of several: {speakers:?}");
    assert!(speakers.len() <= 8);
    eng.shutdown();
}

/// What the core does with it: stage the pinned model, run the engine, check the result.
#[test]
fn the_core_runs_it_with_the_pinned_model() {
    use meetko_core::diarize::{checked, DiarConfig, DiarEngine, DiarProcess, DiarRequest};
    let (Some(model), Some(wav)) = (model(), wav_path()) else { return };
    let cfg = DiarConfig { engine_bin: env!("CARGO_BIN_EXE_meetko-nemotron-diar").into(), model_dir: model, unpinned: false, timeout: Duration::from_secs(120), min_speaker_ms: 0 };
    let d = DiarProcess::new(cfg);
    let duration_ms = (std::fs::metadata(&wav).unwrap().len().saturating_sub(44) / 32) as i64;
    let req = DiarRequest { wav: &wav, audio_sha256: SHA, channel: 0, duration_ms };
    let r = d.diarize(&req, &std::sync::atomic::AtomicBool::new(false)).unwrap();
    assert_eq!(r["params"]["model_pinned"], true, "the staged copy matched the pin in models.toml");
    assert!(r["model"]["sha256"].as_str().is_some_and(|s| s.len() == 64));
    assert!(!checked(&r, SHA, duration_ms).unwrap().turns.is_empty());
}

// ---- protocol: these need the binary only

#[test]
fn requests_it_cannot_serve_are_error_replies_and_the_process_goes_on() {
    let tmp = tempfile::tempdir().unwrap();
    let (stereo, mono) = (tmp.path().join("stereo.wav"), tmp.path().join("mono.wav"));
    write_wav(&stereo, 2, 1600);
    write_wav(&mono, 1, 1600);
    let mut eng = Eng::start(None);
    assert_eq!(eng.call("initialize", json!({ "contract": ["9.9"] })).unwrap_err()["code"], -32001);
    eng.init();
    assert_eq!(eng.call("nope", json!({})).unwrap_err()["code"], -32601);
    assert_eq!(eng.call("diarize.run", json!({ "audio_sha256": SHA })).unwrap_err()["code"], -32602, "no wav");
    assert_eq!(eng.call("diarize.run", json!({ "wav": mono, "audio_sha256": "nope" })).unwrap_err()["code"], -32602, "no sha256");
    let e = eng.diarize(&tmp.path().join("missing.wav")).unwrap_err();
    assert_eq!(e["code"], -32010, "{e}");
    let e = eng.diarize(&stereo).unwrap_err();
    assert!(e["code"] == -32010 && e["message"].as_str().unwrap().contains("mono PCM16"), "{e}");
    let e = eng.diarize(&mono).unwrap_err(); // a good wav, but no model configured
    assert!(e["code"] == -32010 && e["message"].as_str().unwrap().contains("MEETKO_DIAR_MODEL is not set"), "{e}");
    eng.send_raw(b"this is not json\n"); // ignored: every request carries its id
    eng.shutdown();
    assert!(eng.errors.is_empty());
}

/// A fatal path says `engine.error` once, exits with code 1 and does not trip over ggml's static
/// destructors (GGML_ASSERT / SIGABRT).
fn assert_clean_fatal(eng: &mut Eng, what: &str) {
    let code = eng.die();
    assert_eq!(code, Some(1), "{what}: exit code (None = killed by a signal)");
    assert_eq!(eng.errors.len(), 1, "{what}: {:?}", eng.errors);
    assert!(eng.errors[0]["message"].as_str().is_some_and(|m| !m.is_empty()));
    let err = eng.stderr.lock().unwrap().clone();
    assert!(!err.contains("GGML_ASSERT") && !err.contains("abort"), "{what}: stderr: {err}");
}

#[test]
fn a_line_that_is_not_utf8_is_one_engine_error_and_a_clean_exit_1() {
    let mut eng = Eng::start(None);
    eng.init();
    eng.send_raw(b"{\"method\": \"\xff\xfe\"}\n");
    assert_clean_fatal(&mut eng, "non-UTF-8 line");
}

#[test]
fn a_model_file_the_library_cannot_load_is_one_engine_error_and_a_clean_exit_1() {
    let tmp = tempfile::tempdir().unwrap();
    let (model, wav) = (tmp.path().join("Nemotron-3-Diarization.q8_0.gguf"), tmp.path().join("a.wav"));
    std::fs::write(&model, b"GGUF this is not a model").unwrap();
    write_wav(&wav, 1, 16000);
    let mut eng = Eng::start(Some(tmp.path()));
    eng.init();
    eng.send_raw(format!("{}\n", json!({ "jsonrpc": "2.0", "id": 7, "method": "diarize.run", "params": { "wav": wav, "audio_sha256": SHA } })).as_bytes());
    assert_clean_fatal(&mut eng, "unloadable model");
    assert!(eng.errors[0]["message"].as_str().unwrap().contains("cannot load the model"), "{:?}", eng.errors);
}
