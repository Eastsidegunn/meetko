//! whisper.cpp engine: adapter (raw `-ojf` JSON -> engine-result) and runner.
//!
//! Port of `reference/adapters.py::whisper_cpp`; read its docstring for the raw facts
//! (subword tokens, t_dtw in 10 ms units, silent -1 under flash attention).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::contract::EngineWord;
use crate::error::{Error, Result};
use crate::util;

pub const DEFAULT_DTW: &str = "large.v3.turbo";

struct Pending {
    start_ms: i64,
    text: String,
    p: Vec<f64>,
    seg_end: i64,
}

/// Python's `round(x, 4)` is correctly rounded; so is Rust's `{:.4}`.
fn round4(x: f64) -> f64 {
    format!("{x:.4}").parse().unwrap_or(x)
}

pub fn adapt(
    raw: &Value,
    audio_sha256: &str,
    params: &Value,
    engine_version: &str,
    model_id: &str,
    model_sha256: Option<&str>,
) -> Result<Value> {
    let bad = |what: &str| Error::BadEngineOutput(format!("unexpected whisper JSON: {what}"));
    let segments = raw["transcription"].as_array().ok_or_else(|| bad("no transcription"))?;
    let (mut words, mut timing) = (Vec::<Pending>::new(), "dtw");
    for seg in segments {
        let seg_end = seg["offsets"]["to"].as_i64().ok_or_else(|| bad("segment offsets"))?;
        let mut current: Option<Pending> = None;
        for tok in seg["tokens"].as_array().ok_or_else(|| bad("tokens"))? {
            let text = tok["text"].as_str().ok_or_else(|| bad("token text"))?;
            if text.starts_with("[_") {
                continue;
            }
            let dtw = tok["t_dtw"].as_i64().ok_or_else(|| bad("t_dtw"))?;
            if dtw < 0 {
                timing = "token";
            }
            let t = if dtw >= 0 { dtw * 10 } else { tok["offsets"]["from"].as_i64().ok_or_else(|| bad("offsets"))? };
            let p = tok["p"].as_f64().ok_or_else(|| bad("p"))?;
            match current.as_mut() {
                Some(cur) if !text.starts_with(' ') => {
                    cur.text.push_str(text);
                    cur.p.push(p);
                }
                _ => {
                    if let Some(done) = current.take() {
                        words.push(done);
                    }
                    current = Some(Pending { start_ms: t, text: text.trim().to_string(), p: vec![p], seg_end });
                }
            }
        }
        words.extend(current);
    }
    words.retain(|w| !w.text.is_empty());
    let mut out = Vec::new();
    let mut prev_start = 0;
    for (i, w) in words.iter().enumerate() {
        let start = w.start_ms.max(prev_start); // DTW points are not guaranteed monotonic
        let nxt = words.get(i + 1).map_or(w.seg_end, |n| n.start_ms);
        let end = start.max(if nxt > start { nxt.min(w.seg_end) } else { w.seg_end });
        let p = w.p.iter().cloned().fold(f64::INFINITY, f64::min);
        out.push(EngineWord { start_ms: start, end_ms: end, text: w.text.clone(), p: Some(round4(p)), end_estimated: true });
        prev_start = start;
    }
    let duration_ms = segments.iter().filter_map(|s| s["offsets"]["to"].as_i64()).max().unwrap_or(0).max(0);
    let mut model = json!({ "id": model_id });
    if let Some(sha) = model_sha256 {
        model["sha256"] = json!(sha);
    }
    Ok(json!({
        "contract": "0.1", "kind": "asr.batch",
        "engine": { "id": "whisper.cpp", "version": engine_version },
        "model": model,
        "audio_sha256": audio_sha256, "params": params,
        "language": raw["result"]["language"].as_str().unwrap_or(""),
        "word_timing": timing, "network": false,
        "coverage": [{ "start_ms": 0, "end_ms": duration_ms }],
        "words": out,
    }))
}

// Decode settings. Every one of them is passed on the command line AND recorded, from
// this one table, so the provenance cannot drift from what ran.
const TEMPERATURE: f64 = 0.0;
const TEMPERATURE_INC: f64 = 0.2;
const ENTROPY_THOLD: f64 = 2.4;
const LOGPROB_THOLD: f64 = -1.0;
const NO_SPEECH_THOLD: f64 = 0.6;
const BEAM_SIZE: u32 = 5;
const BEST_OF: u32 = 5;

/// Decode parameters as recorded in the producer: exactly what `decode_args` passes.
pub fn params_used(language: &str, dtw: &str) -> Value {
    json!({ "language": language, "max_context": 0, "dtw": dtw, "flash_attn": false,
            "temperature": TEMPERATURE, "temperature_inc": TEMPERATURE_INC, "entropy_thold": ENTROPY_THOLD,
            "logprob_thold": LOGPROB_THOLD, "no_speech_thold": NO_SPEECH_THOLD,
            "beam_size": BEAM_SIZE, "best_of": BEST_OF })
}

fn decode_args(language: &str, dtw: &str) -> Vec<String> {
    let f = |x: f64| format!("{x:?}");
    let mut a: Vec<String> = ["-l", language, "-mc", "0", "-ojf", "-dtw", dtw, "-nfa", "-np", "-pp"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for (flag, v) in [
        ("-tp", f(TEMPERATURE)),
        ("-tpi", f(TEMPERATURE_INC)),
        ("-et", f(ENTROPY_THOLD)),
        ("-lpt", f(LOGPROB_THOLD)),
        ("-nth", f(NO_SPEECH_THOLD)),
        ("-bs", BEAM_SIZE.to_string()),
        ("-bo", BEST_OF.to_string()),
    ] {
        a.push(flag.into());
        a.push(v);
    }
    a
}

// ---------------------------------------------------------------- engine seam

/// Progress of a batch transcription: channel `channel` of `channels`, 0..=100.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub channel: u32,
    pub channels: u32,
    pub percent: u8,
    /// the channel is transcribed; its speakers are being separated (optional, after the 100 %)
    pub diarizing: bool,
}

pub struct BatchRequest<'a> {
    /// the file the engine reads; the caller has hashed exactly this file
    pub wav: &'a Path,
    pub audio_sha256: &'a str,
    pub language: &'a str,
    pub channel: u32,
    pub channels: u32,
    pub data_dir: &'a Path,
}

/// An asr.batch engine: one mono channel file in, one engine-result out.
pub trait BatchEngine {
    fn transcribe(&self, req: &BatchRequest, progress: &dyn Fn(Progress), cancel: &AtomicBool) -> Result<Value>;
}

// ---------------------------------------------------------------- whisper-cli

#[derive(Debug, Clone)]
pub struct WhisperCli {
    pub cli: PathBuf,
    /// read only; never recorded, only its id and sha256 are
    pub model: PathBuf,
    /// a claimed hash is checked against the file, not trusted
    pub model_sha256: Option<String>,
    pub dtw: String,
}

pub(crate) fn model_stamp(meta: &std::fs::Metadata) -> (u64, u128, u64) {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(meta);
    #[cfg(not(unix))]
    let inode = 0;
    (meta.len(), mtime, inode)
}

/// whisper-cli prints "whisper_print_progress_callback: progress =  10%" on stderr.
fn parse_progress(line: &str) -> Option<u8> {
    let rest = line.split("progress =").nth(1)?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().ok().map(|n| n.min(100) as u8)
}

impl WhisperCli {
    /// Data settings: env wins over `<data_dir>/config.toml` (`[whisper] model, model_sha256, dtw`).
    /// The executable is NOT read from config.toml: a plugin can write that file, and a path in it
    /// would run as soon as the user transcribes. Only env `MEETKO_WHISPER_CLI`, a sibling of the
    /// running executable, or `whisper-cli` on PATH are used (docs/plugin-security.md).
    pub fn load(data_dir: &Path) -> Result<Self> {
        let file: toml::Table = match std::fs::read_to_string(data_dir.join("config.toml")) {
            Ok(s) => s.parse().map_err(|e| Error::Engine(format!("config.toml: {e}")))?,
            Err(_) => toml::Table::new(),
        };
        let from_file = |k: &str| file.get("whisper").and_then(|w| w.get(k)).and_then(|v| v.as_str()).map(String::from);
        let get = |env: &str, key: &str| std::env::var(env).ok().filter(|v| !v.is_empty()).or_else(|| from_file(key));
        let model = get("MEETKO_WHISPER_MODEL", "model")
            .ok_or_else(|| Error::ModelMissing("set MEETKO_WHISPER_MODEL (or [whisper] model in config.toml)".into()))?;
        let model_sha256 = get("MEETKO_WHISPER_MODEL_SHA256", "model_sha256");
        if let Some(s) = &model_sha256 {
            if !util::is_sha256(s) {
                return Err(Error::Engine("model sha256 must be 64 lowercase hex characters".into()));
            }
        }
        Ok(WhisperCli {
            cli: Self::resolve_cli(),
            model: model.into(),
            model_sha256,
            dtw: get("MEETKO_WHISPER_DTW", "dtw").unwrap_or_else(|| DEFAULT_DTW.into()),
        })
    }

    /// env, else a sibling of the running executable, else the bare name (PATH lookup at spawn).
    pub fn resolve_cli() -> PathBuf {
        if let Some(p) = std::env::var_os("MEETKO_WHISPER_CLI").filter(|v| !v.is_empty()) {
            return p.into();
        }
        let name = if cfg!(windows) { "whisper-cli.exe" } else { "whisper-cli" };
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join(name)))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| "whisper-cli".into())
    }

    pub fn model_id(&self) -> String {
        self.model.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "unknown".into())
    }

    /// The model's sha256, hashed from the file (cached by path hash + size + mtime_ns +
    /// inode, since it is >1 GB). A claimed hash that differs from the file is an error.
    pub fn model_hash(&self, data_dir: &Path) -> Result<String> {
        let meta = std::fs::metadata(&self.model).map_err(|e| Error::ModelMissing(format!("cannot read the whisper model: {e}")))?;
        let (len, mtime, inode) = model_stamp(&meta);
        let canon = dunce::canonicalize(&self.model).unwrap_or_else(|_| self.model.clone());
        let key = format!("{}:{len}:{mtime}:{inode}", util::sha256_bytes(canon.to_string_lossy().as_bytes()));
        let cache_path = data_dir.join("model-sha256-cache.json");
        let mut cache: serde_json::Map<String, Value> = std::fs::read_to_string(&cache_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let sha = match cache.get(&key).and_then(Value::as_str).filter(|s| util::is_sha256(s)) {
            Some(s) => s.to_string(),
            None => {
                let sha = util::sha256_file(&self.model)?;
                cache.insert(key, json!(sha));
                util::create_private_dir(data_dir)?;
                util::write_private(&cache_path, serde_json::to_string(&cache)?.as_bytes())?;
                sha
            }
        };
        if let Some(claimed) = &self.model_sha256 {
            if *claimed != sha {
                return Err(Error::Engine("the model file does not match the configured sha256".into()));
            }
        }
        Ok(sha)
    }

    /// `whisper-cli --version` prints "whisper.cpp version: 1.9.5". Always asked of the binary.
    pub fn engine_version(&self) -> Result<String> {
        let home = tempfile::tempdir()?;
        let mut cmd = Command::new(&self.cli);
        util::sandbox_env(&mut cmd, home.path());
        let out = cmd
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::EngineNotFound(format!("cannot run whisper-cli: {e}")))?;
        // some builds print the version on stderr
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        text.lines()
            .find(|l| l.contains("version"))
            .and_then(|l| l.split_whitespace().last())
            .filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .map(String::from)
            .ok_or_else(|| Error::BadEngineOutput("whisper-cli --version printed no version".into()))
    }
}

impl BatchEngine for WhisperCli {
    fn transcribe(&self, req: &BatchRequest, progress: &dyn Fn(Progress), cancel: &AtomicBool) -> Result<Value> {
        let version = self.engine_version()?;
        // hash before running, re-stat after: the recorded sha is what was read
        let before = std::fs::metadata(&self.model).map_err(|e| Error::ModelMissing(format!("cannot read the whisper model: {e}")))?;
        let sha = self.model_hash(req.data_dir)?;
        let tmp = tempfile::tempdir()?;
        let stem = tmp.path().join("out");
        let mut cmd = Command::new(&self.cli);
        util::sandbox_env(&mut cmd, tmp.path()); // secrets in the user's environment stay out of whisper-cli
        cmd.arg("-m").arg(&self.model).arg("-f").arg(req.wav).args(decode_args(req.language, &self.dtw)).arg("-of").arg(&stem);
        util::own_process_group(&mut cmd);
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::EngineNotFound(format!("cannot run whisper-cli: {e}")))?;
        let (tx, rx) = mpsc::channel::<u8>();
        let tail = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let (mut err, tail2) = (child.stderr.take().expect("piped"), tail.clone());
        let reader = std::thread::spawn(move || {
            let (mut chunk, mut line) = ([0u8; 4096], Vec::<u8>::new());
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                for &b in &chunk[..n] {
                    if b == b'\n' || b == b'\r' {
                        if let Some(p) = parse_progress(&String::from_utf8_lossy(&line)) {
                            let _ = tx.send(p);
                        }
                        line.clear();
                    } else if line.len() < 4096 {
                        line.push(b);
                    }
                }
                let mut t = tail2.lock().unwrap();
                t.extend_from_slice(&chunk[..n]);
                let excess = t.len().saturating_sub(4096);
                t.drain(..excess);
            }
        });
        let status = loop {
            if cancel.load(Ordering::SeqCst) {
                util::kill_tree(&mut child);
                let _ = reader.join();
                return Err(Error::Cancelled);
            }
            if let Ok(p) = rx.recv_timeout(Duration::from_millis(50)) {
                progress(Progress { channel: req.channel, channels: req.channels, percent: p, diarizing: false });
            }
            if let Some(st) = child.try_wait()? {
                break st;
            }
        };
        let _ = reader.join();
        while let Ok(p) = rx.try_recv() {
            progress(Progress { channel: req.channel, channels: req.channels, percent: p, diarizing: false });
        }
        if !status.success() {
            let tail = String::from_utf8_lossy(&tail.lock().unwrap()).into_owned();
            return Err(Error::Engine(format!("whisper-cli exited with {status}: {tail}")));
        }
        let after = std::fs::metadata(&self.model)?;
        if model_stamp(&before) != model_stamp(&after) {
            return Err(Error::Engine("the model file changed while whisper-cli was running".into()));
        }
        let raw = std::fs::read_to_string(stem.with_extension("json"))
            .map_err(|e| Error::BadEngineOutput(format!("whisper-cli wrote no JSON: {e}")))?;
        let raw: Value = serde_json::from_str(&raw).map_err(|e| Error::BadEngineOutput(format!("whisper JSON: {e}")))?;
        adapt(&raw, req.audio_sha256, &params_used(req.language, &self.dtw), &version, &self.model_id(), Some(&sha))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn whisper_cli_does_not_inherit_the_environment() {
        if !util::in_secret_child("whisper::tests::whisper_cli_does_not_inherit_the_environment") {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let cli = tmp.path().join("whisper-cli");
        // dumps what it was given; `-of <stem>` is where the JSON goes
        let script = r#"#!/bin/sh
env > "$(dirname "$0")/env.out"
for a in "$@"; do [ "$a" = --version ] && { echo "whisper.cpp version: 1.9.5"; exit 0; }; done
while [ $# -gt 0 ]; do [ "$1" = -of ] && stem="$2"; shift; done
echo '{"result":{"language":"ko"},"transcription":[{"offsets":{"from":0,"to":1000},"tokens":[{"text":" a","p":0.5,"t_dtw":50,"offsets":{"from":0,"to":100}}]}]}' > "$stem.json"
"#;
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let model = tmp.path().join("model.bin");
        std::fs::write(&model, "m").unwrap();
        let engine = WhisperCli { cli, model, model_sha256: None, dtw: "large.v3.turbo".into() };
        let wav = tmp.path().join("in.wav");
        std::fs::write(&wav, "x").unwrap();
        let req = BatchRequest { wav: &wav, audio_sha256: &"0".repeat(64), language: "ko", channel: 0, channels: 1, data_dir: tmp.path() };
        engine.transcribe(&req, &|_| {}, &AtomicBool::new(false)).unwrap();
        let env = std::fs::read_to_string(tmp.path().join("env.out")).unwrap();
        assert!(!env.contains("MEETKO_TEST_SECRET"), "the secret reached whisper-cli");
        assert!(env.lines().any(|l| l.starts_with("PATH=")), "PATH is on the allowlist");
    }
}
