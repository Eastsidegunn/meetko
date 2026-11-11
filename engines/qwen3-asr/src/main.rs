//! meetko batch ASR engine (experimental): Qwen3-ASR-1.7B for the text and Qwen3-ForcedAligner-0.6B
//! for the word times, through the CrispASR C API, over NDJSON JSON-RPC 2.0 on stdio.
//!
//! Requests: initialize, transcribe.run `{wav, audio_sha256, language}`, shutdown. The wav is a 16 kHz
//! mono PCM16 file the caller has already hashed (`audio_sha256` is echoed into the result; this
//! engine does not hash). The reply is a contract `asr.batch` engine result; while it runs, the engine
//! sends `transcribe.progress {percent}` notifications. The models come from MEETKO_BATCH_MODEL (a
//! directory holding the two files `models.toml` names), never from a request.
//!
//! A library failure ends the process after one `engine.error` notification (exit code 1); a request
//! the engine cannot serve (no model, an unusable wav, bad parameters) is an ordinary error reply and
//! the process goes on. Stdout carries protocol only: the protocol writes to a duplicate of the
//! original stdout (close-on-exec), and fd 1 itself is pointed at stderr, so whatever the library
//! prints goes to the log.
//!
//! No network, no other programs: CrispASR can fetch models it does not have with `curl` or `wget` for
//! some of its other backends. This engine always names both model files, so it never takes that path.
//! As a second line it sets its own PATH to a directory that does not exist, as the first thing it
//! does (an empty PATH would search the current directory); the core also starts it in a private empty
//! working directory and, on macOS, under a sandbox profile that denies the network and any other
//! program (`meetko_core::sandbox::engine_profile`). Run by hand, only the PATH guard applies.

mod ffi;

use std::io::{BufRead, Write};
use std::os::fd::FromRawFd;
use std::path::PathBuf;

use meetko_qwen3_asr::{self as eng, rpc, Cut, Tally, Word, CONTRACT, CUT};
use serde_json::{json, Value};

const ENGINE_ID: &str = "meetko/qwen3-asr";
/// the pin id in `models.toml`, the two files it names inside the model directory, and where they come from
const MODEL_ID: &str = "qwen3-asr-1.7b-crispasr-q8_0";
const ASR_FILE: &str = "qwen3-asr-1.7b-q8_0.gguf";
const ASR_SOURCE: &str = "cstr/qwen3-asr-1.7b-GGUF@674df5d44b50a63e7102a18895ed20e3f91de301";
const ALIGNER_FILE: &str = "qwen3-forced-aligner-0.6b-q8_0.gguf";
const ALIGNER_SOURCE: &str = "cstr/qwen3-forced-aligner-0.6b-GGUF@1ec5110602ccab18c878ddebedab0891e290a95c";
const BACKEND: &str = "qwen3";
const TEMPERATURE: f32 = 0.0;
/// PATH while the engine runs: a directory that does not exist, so a lookup by name finds nothing
const NO_PATH: &str = "/nonexistent/meetko";

struct Engine {
    out: std::fs::File,
    asr: Option<ffi::Session>,
    threads: i32,
}

fn engine_info() -> Value {
    json!({ "id": ENGINE_ID, "version": format!("{}+crispasr-{}.c-api-{}", env!("CARGO_PKG_VERSION"), env!("CRISPASR_RELEASE"), ffi::version()) })
}

impl Engine {
    fn fatal(&mut self, message: &str) -> ! {
        rpc::notify(&mut self.out, "engine.error", json!({ "message": message }));
        eprintln!("fatal: {message}");
        // SAFETY: ends at once; the protocol stream was flushed. `exit` would run the ggml backends'
        // static destructors while a model is loaded (ggml-metal asserts there)
        unsafe { libc::_exit(1) }
    }

    fn model_dir() -> Result<PathBuf, String> {
        let d = std::env::var_os("MEETKO_BATCH_MODEL").filter(|v| !v.is_empty()).ok_or("MEETKO_BATCH_MODEL is not set")?;
        let d = PathBuf::from(d);
        for f in [ASR_FILE, ALIGNER_FILE] {
            if !d.join(f).is_file() {
                return Err(format!("{f} is missing in MEETKO_BATCH_MODEL"));
            }
        }
        Ok(d)
    }

    fn run(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let wav = params["wav"].as_str().filter(|w| !w.is_empty()).ok_or((-32602, "wav is required".to_string()))?;
        let sha = params["audio_sha256"].as_str().filter(|s| eng::is_sha256(s)).ok_or((-32602, "audio_sha256 must be 64 lowercase hex digits".to_string()))?;
        let requested = params["language"].as_str().unwrap_or("auto");
        let lang = eng::language(Some(requested)).map_err(|e| (-32602, e))?;
        let dir = Self::model_dir().map_err(|e| (-32010, e))?;
        let mut wav = eng::Wav::open(std::path::Path::new(wav)).map_err(|e| (-32010, e))?;
        let asr_path = dir.join(ASR_FILE).to_string_lossy().into_owned();
        let aligner = dir.join(ALIGNER_FILE).to_string_lossy().into_owned();
        if self.asr.is_none() {
            match ffi::Session::open(&asr_path, BACKEND, self.threads) {
                Ok(mut s) => {
                    s.set_temperature(TEMPERATURE, 0);
                    self.asr = Some(s);
                }
                Err(e) => self.fatal(&e),
            }
        }
        let cut: Cut = CUT;
        let total = wav.samples.max(1);
        let mut tally = Tally::default();
        let mut tokens_known = true;
        let mut words: Vec<Word> = Vec::new();
        // one piece in memory at a time: read up to a piece's length, cut it, go on from the cut
        let mut a = 0;
        while a < wav.samples {
            let window = match wav.read(a, eng::max_samples(cut)) {
                Ok(w) => w,
                Err(e) => return Err((-32010, e)),
            };
            let n = eng::next_cut(&window, a + window.len() >= wav.samples, cut).max(1);
            let piece = &window[..n];
            let b = a + n;
            let (lo, hi) = (eng::duration_ms(a), eng::duration_ms(b));
            tally.pieces += 1;
            if eng::is_silent(piece, cut) {
                tally.silent_pieces += 1;
            } else {
                let budget = eng::max_new_tokens(piece.len());
                let asr = self.asr.as_mut().expect("loaded above");
                let (text, tokens) = match asr.transcribe(piece, &lang, budget) {
                    Ok(t) => t,
                    Err(e) => self.fatal(&e),
                };
                match tokens {
                    Some(t) if t >= budget as usize => tally.capped_pieces += 1,
                    Some(_) => {}
                    None => tokens_known = false,
                }
                if !text.trim().is_empty() {
                    let aligned = match ffi::align(&aligner, &text, piece, lo, self.threads) {
                        Ok(al) if !al.is_empty() => al.into_iter().map(|w| (w.text, w.start_ms, w.end_ms)).collect(),
                        other => {
                            if let Err(e) = other {
                                eprintln!("alignment failed on a piece, its words are spread over it: {e}");
                            }
                            tally.unaligned_pieces += 1;
                            Vec::new()
                        }
                    };
                    let floor = words.last().map_or(0, |w| w.start_ms);
                    let (placed, spread) = eng::place(&eng::group(&text, &aligned), lo, hi, floor);
                    tally.spread_words += spread;
                    words.extend(placed);
                }
            }
            rpc::notify(&mut self.out, "transcribe.progress", json!({ "percent": (b * 100 / total).min(100) }));
            a = b;
        }
        let params = json!({
            "backend": BACKEND, "language": if lang.is_empty() { "auto" } else { lang.as_str() }, "language_requested": requested,
            "temperature": TEMPERATURE, "max_new_tokens_per_second": 12, "max_new_tokens_min": 64, "token_counts_known": tokens_known,
            "piece_max_ms": cut.max_ms, "cut_search_ms": cut.search_ms, "sound_dbfs": cut.sound_dbfs, "min_sound_ms": cut.min_sound_ms,
            "asr_model": ASR_FILE, "asr_model_source": ASR_SOURCE, "aligner_model": ALIGNER_FILE, "aligner_model_source": ALIGNER_SOURCE,
            "gpu": if cfg!(target_os = "macos") { "metal" } else { "cpu" }, "threads": self.threads,
        });
        Ok(eng::result(&engine_info(), MODEL_ID, sha, params, &lang, &words, eng::duration_ms(wav.samples), &tally))
    }
}

fn main() {
    // before anything else, the library included: no program can be found by name from here on
    std::env::set_var("PATH", NO_PATH);
    // the protocol keeps the original stdout (close-on-exec, above the standard descriptors); fd 1
    // becomes stderr for anything the library prints
    // SAFETY: plain descriptor calls before any other thread exists
    let out = unsafe {
        let fd = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3);
        if fd < 0 || libc::dup2(2, 1) < 0 {
            eprintln!("cannot set up stdout");
            libc::_exit(1);
        }
        std::fs::File::from_raw_fd(fd)
    };
    if cfg!(target_os = "macos") {
        ffi::set_gpu_backend("metal");
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8)) as i32;
    let mut e = Engine { out, asr: None, threads };
    for line in std::io::stdin().lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(err) if err.kind() == std::io::ErrorKind::InvalidData => e.fatal("stdin carried a line that is not UTF-8"),
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
                let r = if !offered {
                    Err((-32001, "unsupported contract".into()))
                } else {
                    Ok(json!({ "contract": CONTRACT, "engine": engine_info(), "role": "asr.batch", "model": { "id": MODEL_ID } }))
                };
                rpc::reply(&mut e.out, id, r);
            }
            ("transcribe.run", Some(id)) => {
                let r = e.run(params);
                rpc::reply(&mut e.out, id, r);
            }
            ("shutdown", Some(id)) => {
                rpc::reply(&mut e.out, id, Ok(json!({})));
                break;
            }
            (_, Some(id)) => rpc::reply(&mut e.out, id, Err((-32601, "method not found".into()))),
            _ => {}
        }
    }
    // unload before the process ends, then end without running the backends' static destructors
    drop(e.asr.take());
    ffi::free_aligner();
    let _ = e.out.flush();
    // SAFETY: everything is flushed
    unsafe { libc::_exit(0) }
}
