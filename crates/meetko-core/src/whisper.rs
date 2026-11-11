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
use crate::whisper_prompt::{self, Prompt};

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
    adapt_mapped(raw, audio_sha256, params, engine_version, model_id, model_sha256, None)
}

/// `adapt` for a run with `--vad`: whisper.cpp maps its segment times back onto the input audio but
/// leaves token times (`t_dtw`, `offsets`) on the shortened speech-only audio it decoded. `map` is
/// that run's own time map (`VadMap`); every token time goes through it, segment times are kept.
pub fn adapt_mapped(
    raw: &Value,
    audio_sha256: &str,
    params: &Value,
    engine_version: &str,
    model_id: &str,
    model_sha256: Option<&str>,
    map: Option<&VadMap>,
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
            let t = map.map_or(t, |m| m.original_ms(t));
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

// VAD settings (`--vad`, whisper.cpp 1.7.6 or newer): whisper.cpp's own defaults, passed explicitly
// and recorded like the decode settings. Silero gives each 32 ms window a speech probability; whisper
// then decodes only the windows above the threshold (with padding), so silence never reaches it.
//
// Opt-in only (MEETKO_WHISPER_VAD_MODEL), for evaluation: on the 2026-11-10 comparison (four synthetic
// Korean-English meetings, large-v3-turbo, whisper.cpp 1.9.5) the speech check alone removed every
// invented word, and VAD on top dropped whole stretches of real speech in some meetings (CER 17.6% ->
// 19.8%; other VAD settings moved the loss to other meetings, none was better than no VAD).
const VAD_THRESHOLD: f64 = 0.5;
const VAD_MIN_SPEECH_MS: u32 = 250;
const VAD_MIN_SILENCE_MS: u32 = 100;
const VAD_SPEECH_PAD_MS: u32 = 30;
const VAD_SAMPLES_OVERLAP: f64 = 0.1;

/// The pinned VAD model (`engines/whisper-cpp/models.toml`) and its file name in the model directory.
pub const VAD_MODEL_ID: &str = "silero-vad-v6.2.0-ggml";
pub const VAD_MODEL_FILE: &str = "ggml-silero-v6.2.0.bin";

/// Whether a run used whisper.cpp's VAD, as recorded in the producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vad {
    /// `--vad` with the pinned model, and its sha256 (the pin's: the staged copy matched it)
    Used(&'static str),
    /// not used, and why: `no_speech` (the channel was skipped) | `not_enabled` | `no_time_map`
    Unused(&'static str),
}

/// Whether a run gave whisper the user's hint terms as its prompt (`whisper_prompt`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hints {
    /// the user has no hint terms
    None,
    Used(Prompt),
    /// there are hint terms but they were not given, and why: `unsupported` (this whisper-cli has no
    /// `--carry-initial-prompt`) | `no_tokenizer` (the model's vocabulary
    /// could not be read, so the prompt could not be measured) | `too_long` (not even the first fits)
    Unused(&'static str),
}

impl Hints {
    fn prompt(&self) -> Option<&Prompt> {
        match self {
            Hints::Used(p) => Some(p),
            _ => None,
        }
    }
}

/// `-mc`: no decoded text is carried into the next window; with a prompt, room for exactly the prompt
/// (see `whisper_prompt`).
fn max_context(prompt: Option<&Prompt>) -> usize {
    prompt.map_or(0, |p| p.tokens + 1)
}

/// Decode parameters as recorded in the producer: exactly what `decode_args` passes, plus the two
/// steps around it that are meetko's own (the speech check before, and whether VAD ran). A prompt is
/// recorded by its size only, not its terms (`Prompt::params`).
pub fn params_used(language: &str, dtw: &str, vad: &Vad, gate: crate::speech_gate::Verdict, hints: &Hints) -> Value {
    let mut p = json!({ "language": language, "max_context": max_context(hints.prompt()), "dtw": dtw, "flash_attn": false,
            "temperature": TEMPERATURE, "temperature_inc": TEMPERATURE_INC, "entropy_thold": ENTROPY_THOLD,
            "logprob_thold": LOGPROB_THOLD, "no_speech_thold": NO_SPEECH_THOLD,
            "beam_size": BEAM_SIZE, "best_of": BEST_OF,
            "speech_gate": crate::speech_gate::params(gate) });
    match vad {
        Vad::Used(sha) => {
            p["vad"] = json!(true);
            p["vad_model"] = json!({ "id": VAD_MODEL_ID, "sha256": sha });
            p["vad_threshold"] = json!(VAD_THRESHOLD);
            p["vad_min_speech_ms"] = json!(VAD_MIN_SPEECH_MS);
            p["vad_min_silence_ms"] = json!(VAD_MIN_SILENCE_MS);
            p["vad_speech_pad_ms"] = json!(VAD_SPEECH_PAD_MS);
            p["vad_samples_overlap"] = json!(VAD_SAMPLES_OVERLAP);
        }
        Vad::Unused(why) => {
            p["vad"] = json!(false);
            p["vad_unused"] = json!(why);
        }
    }
    match hints {
        Hints::None => {}
        Hints::Used(prompt) => {
            p["prompt"] = prompt.params();
            p["carry_initial_prompt"] = json!(true);
        }
        Hints::Unused(why) => p["prompt_unused"] = json!(why),
    }
    p
}

/// `vad`: the staged VAD model to run with. With VAD whisper-cli must print its log (no `-np`): the
/// time map it builds is only printed there (`VadMap::parse`). `prompt`: the hint terms, given to every
/// window and nothing else carried (`whisper_prompt`).
fn decode_args(language: &str, dtw: &str, vad: Option<&Path>, prompt: Option<&Prompt>) -> Vec<String> {
    let f = |x: f64| format!("{x:?}");
    let mc = max_context(prompt).to_string();
    let mut a: Vec<String> = ["-l", language, "-mc", &mc, "-ojf", "-dtw", dtw, "-nfa", "-pp"].iter().map(|s| s.to_string()).collect();
    if let Some(p) = prompt {
        a.extend(["--prompt".to_string(), p.text.clone(), "--carry-initial-prompt".to_string()]);
    }
    let mut pairs = vec![
        ("-tp", f(TEMPERATURE)),
        ("-tpi", f(TEMPERATURE_INC)),
        ("-et", f(ENTROPY_THOLD)),
        ("-lpt", f(LOGPROB_THOLD)),
        ("-nth", f(NO_SPEECH_THOLD)),
        ("-bs", BEAM_SIZE.to_string()),
        ("-bo", BEST_OF.to_string()),
    ];
    match vad {
        None => a.push("-np".into()),
        Some(model) => {
            a.push("--vad".into());
            pairs.extend([
                ("-vm", model.to_string_lossy().into_owned()),
                ("-vt", f(VAD_THRESHOLD)),
                ("-vspd", VAD_MIN_SPEECH_MS.to_string()),
                ("-vsd", VAD_MIN_SILENCE_MS.to_string()),
                ("-vp", VAD_SPEECH_PAD_MS.to_string()),
                ("-vo", f(VAD_SAMPLES_OVERLAP)),
            ]);
        }
    }
    for (flag, v) in pairs {
        a.push(flag.into());
        a.push(v);
    }
    a
}

/// The time map of a `--vad` run: whisper.cpp decodes the speech segments joined with 0.1 s of
/// silence between them, maps its segment times back with a piecewise-linear table, and does not map
/// token times (1.9.5: `whisper_vad` builds the table, `map_processed_to_original_time` applies it, to
/// segments only). The table is rebuilt here from the `vad_segment_info` lines whisper.cpp logs (one per
/// segment, in centiseconds printed as seconds with two decimals, so exactly), in the same way, and
/// checked against the point count it logs. Times are milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VadMap {
    /// (time in the decoded audio, time in the input audio), increasing in the first
    points: Vec<(i64, i64)>,
}

impl VadMap {
    /// From whisper-cli's log. None when the log has no table, or one that does not agree with the
    /// point count whisper.cpp says it made (a changed log format must not give wrong times).
    pub fn parse(log: &str) -> Option<VadMap> {
        let cs = |s: &str| s.trim().parse::<f64>().ok().map(|x| (x * 100.0).round() as i64);
        let mut segs = Vec::new(); // (orig_start, orig_end, vad_start, vad_end), centiseconds
        let mut count = None;
        for line in log.lines() {
            if let Some(rest) = line.split("vad_segment_info:").nth(1) {
                let field = |k: &str| rest.split(',').find_map(|kv| kv.trim().strip_prefix(k).and_then(|v| v.strip_prefix(':')).and_then(cs));
                segs.push((field("orig_start")?, field("orig_end")?, field("vad_start")?, field("vad_end")?));
            } else if let Some(rest) = line.split("Created time mapping table with").nth(1) {
                count = rest.split_whitespace().next().and_then(|n| n.parse::<usize>().ok());
            }
        }
        let mut pts = Vec::with_capacity(segs.len() * 3);
        for (i, &(o0, o1, v0, v1)) in segs.iter().enumerate() {
            pts.push((v0, o0));
            pts.push((v1, o1));
            if let Some(&(next_o0, _, next_v0, _)) = segs.get(i + 1) {
                // the 0.1 s of silence whisper.cpp puts before the next segment: its start maps to this
                // segment's end, its end (the next segment's start) to the next segment's start
                pts.push((next_v0 - 10, o1));
                pts.push((next_v0, next_o0));
            }
        }
        pts.sort_by_key(|p| p.0); // stable: of equal decoded times the first pushed is kept
        pts.dedup_by_key(|p| p.0);
        if segs.is_empty() || count != Some(pts.len()) {
            return None;
        }
        Some(VadMap { points: pts.into_iter().map(|(v, o)| (v * 10, o * 10)).collect() })
    }

    /// whisper.cpp's `map_processed_to_original_time`, in milliseconds.
    pub fn original_ms(&self, t: i64) -> i64 {
        let p = &self.points;
        let (first, last) = (p[0], p[p.len() - 1]);
        if t <= first.0 {
            return first.1;
        }
        if t >= last.0 {
            return last.1;
        }
        let i = p.partition_point(|q| q.0 < t);
        if p[i].0 == t {
            return p[i].1;
        }
        let (lo, hi) = (p[i - 1], p[i]);
        lo.1 + (t - lo.0) * (hi.1 - lo.1) / (hi.0 - lo.0)
    }
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
    /// the caller or the meeting named the language; false when `language` is the default (`ko`). An
    /// engine that detects the language itself (`batch.rs`) lets it do so then; whisper uses `language`
    pub language_explicit: bool,
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
    /// the VAD model directory (`MEETKO_WHISPER_VAD_MODEL`): run with `--vad`. None: no VAD (the default)
    pub vad: Option<PathBuf>,
    /// the user's hint terms (`vocabulary`), given as the prompt when they fit (`whisper_prompt`)
    pub hints: Vec<String>,
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
        // VAD is opt-in (see VAD_THRESHOLD): only a directory the environment names, never config.toml,
        // and what it holds is hashed against the pin at every run
        let vad = std::env::var_os("MEETKO_WHISPER_VAD_MODEL").filter(|v| !v.is_empty()).map(PathBuf::from);
        Ok(WhisperCli {
            cli: Self::resolve_cli(),
            model: model.into(),
            model_sha256,
            dtw: get("MEETKO_WHISPER_DTW", "dtw").unwrap_or_else(|| DEFAULT_DTW.into()),
            vad,
            hints: Vec::new(),
        })
    }

    /// With the user's hint terms (checked by `vocabulary`).
    pub fn with_hints(mut self, hints: Vec<String>) -> Self {
        self.hints = hints;
        self
    }

    /// The prompt this run gives whisper: the hint terms that fit, measured with the model's own
    /// tokenizer. Hints that cannot be measured are not given (and that is recorded).
    fn hints(&self) -> Hints {
        if self.hints.is_empty() {
            return Hints::None;
        }
        if !self.supports_carry_prompt() {
            return Hints::Unused("unsupported");
        }
        match whisper_prompt::Tokenizer::from_model(&self.model) {
            Err(_) => Hints::Unused("no_tokenizer"),
            Ok(tok) => whisper_prompt::build(&tok, &self.hints).map_or(Hints::Unused("too_long"), Hints::Used),
        }
    }

    /// Whether this whisper-cli has whisper.cpp's VAD (1.7.6 or newer): its `--help` lists `--vad-model`.
    pub fn supports_vad(&self) -> bool {
        self.help_lists("--vad-model")
    }

    /// Whether this whisper-cli can give the prompt to every window (`--carry-initial-prompt`, whisper.cpp
    /// 1.8 or newer). An older one prints its usage for an unknown option and exits 0 without a
    /// transcript, so the flag is never passed unasked.
    pub fn supports_carry_prompt(&self) -> bool {
        self.help_lists("--carry-initial-prompt")
    }

    fn help_lists(&self, flag: &str) -> bool {
        let Ok(home) = tempfile::tempdir() else { return false };
        let mut cmd = Command::new(&self.cli);
        util::sandbox_env(&mut cmd, home.path());
        cmd.arg("--help")
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(flag) || String::from_utf8_lossy(&o.stderr).contains(flag))
    }

    /// The VAD model to run with, when VAD is asked for: a private copy, hashed while it was made and
    /// matched against the pin. Asked for and not usable (no VAD in this whisper-cli, not the pinned
    /// file) is an error, never a quiet run without it.
    fn stage_vad(&self, cancel: &AtomicBool) -> Result<Option<crate::live::StagedModel>> {
        let Some(dir) = &self.vad else { return Ok(None) };
        if !self.supports_vad() {
            return Err(Error::Engine("MEETKO_WHISPER_VAD_MODEL is set, but this whisper-cli has no VAD (whisper.cpp 1.7.6 or newer has)".into()));
        }
        crate::live::stage_model_for(crate::live::ROLE_WHISPER_VAD, dir, false, &|| cancel.load(Ordering::SeqCst)).map(Some)
    }

    /// One whisper-cli run: the raw JSON, and with `vad` the time map from its log.
    fn run(&self, req: &BatchRequest, vad: Option<&Path>, prompt: Option<&Prompt>, progress: &dyn Fn(Progress), cancel: &AtomicBool) -> Result<(Value, Option<VadMap>)> {
        let tmp = tempfile::tempdir()?;
        let stem = tmp.path().join("out");
        let mut cmd = Command::new(&self.cli);
        util::sandbox_env(&mut cmd, tmp.path()); // secrets in the user's environment stay out of whisper-cli
        cmd.arg("-m").arg(&self.model).arg("-f").arg(req.wav).args(decode_args(req.language, &self.dtw, vad, prompt)).arg("-of").arg(&stem);
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
            // the lines of the VAD time map (bounded: about 100 bytes per speech segment)
            let mut vad_log = String::new();
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                for &b in &chunk[..n] {
                    if b == b'\n' || b == b'\r' {
                        let l = String::from_utf8_lossy(&line);
                        if let Some(p) = parse_progress(&l) {
                            let _ = tx.send(p);
                        } else if (l.contains("vad_segment_info:") || l.contains("Created time mapping table")) && vad_log.len() < 64 << 20 {
                            vad_log.push_str(&l);
                            vad_log.push('\n');
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
            vad_log
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
        let vad_log = reader.join().unwrap_or_default();
        while let Ok(p) = rx.try_recv() {
            progress(Progress { channel: req.channel, channels: req.channels, percent: p, diarizing: false });
        }
        if !status.success() {
            let tail = String::from_utf8_lossy(&tail.lock().unwrap()).into_owned();
            return Err(Error::Engine(format!("whisper-cli exited with {status}: {tail}")));
        }
        let raw = std::fs::read_to_string(stem.with_extension("json"))
            .map_err(|e| Error::BadEngineOutput(format!("whisper-cli wrote no JSON: {e}")))?;
        let raw: Value = serde_json::from_str(&raw).map_err(|e| Error::BadEngineOutput(format!("whisper JSON: {e}")))?;
        Ok((raw, vad.and_then(|_| VadMap::parse(&vad_log))))
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
        let gate = crate::speech_gate::check(req.wav, cancel)?;
        let full = |res: &mut Value| {
            // the whole channel was looked at (VAD decodes only speech, so the last segment can end early;
            // a skipped channel was examined and judged dead or steady). Coverage is "attempted", never
            // "has speech": a skipped channel says so in its params (`skipped`), and the app shows it.
            if let Some(d) = gate.duration_ms {
                res["coverage"] = json!([{ "start_ms": 0, "end_ms": d }]);
            }
        };
        if gate.verdict == crate::speech_gate::Verdict::NoSpeech {
            // nothing for whisper to hear: it would only invent text (subtitle credits) here
            progress(Progress { channel: req.channel, channels: req.channels, percent: 100, diarizing: false });
            let mut params = params_used(req.language, &self.dtw, &Vad::Unused("no_speech"), gate.verdict, &Hints::None);
            params["skipped"] = json!("no_speech");
            let empty = json!({ "result": { "language": req.language }, "transcription": [] });
            let mut res = adapt(&empty, req.audio_sha256, &params, &version, &self.model_id(), Some(&sha))?;
            full(&mut res);
            return Ok(res);
        }
        let staged = self.stage_vad(cancel)?;
        let vad_sha = match staged {
            Some(_) => Some(crate::live::pinned_file_sha256(VAD_MODEL_ID, VAD_MODEL_FILE).ok_or_else(|| Error::Engine("the VAD model has no pin".into()))?),
            None => None,
        };
        let hints = self.hints();
        let (raw, vad, map) = self.decode(req, staged.as_ref().map(|m| m.path.join(VAD_MODEL_FILE)).as_deref().zip(vad_sha), hints.prompt(), progress, cancel)?;
        let after = std::fs::metadata(&self.model)?;
        if model_stamp(&before) != model_stamp(&after) {
            return Err(Error::Engine("the model file changed while whisper-cli was running".into()));
        }
        let params = params_used(req.language, &self.dtw, &vad, gate.verdict, &hints);
        let mut res = adapt_mapped(&raw, req.audio_sha256, &params, &version, &self.model_id(), Some(&sha), map.as_ref())?;
        if matches!(vad, Vad::Used(_)) {
            full(&mut res);
        }
        Ok(res)
    }
}

impl WhisperCli {
    /// whisper-cli on the channel, with `vad` (the staged model and its pinned sha256) when given: the raw
    /// JSON, whether VAD was used, and its time map. A VAD run whose time map cannot be read is run
    /// again without VAD (words whose times cannot be mapped back are worse than no VAD).
    fn decode(&self, req: &BatchRequest, vad: Option<(&Path, &'static str)>, prompt: Option<&Prompt>, progress: &dyn Fn(Progress), cancel: &AtomicBool) -> Result<(Value, Vad, Option<VadMap>)> {
        let Some((model, sha)) = vad else {
            let (raw, _) = self.run(req, None, prompt, progress, cancel)?;
            return Ok((raw, Vad::Unused("not_enabled"), None));
        };
        let (raw, map) = self.run(req, Some(model), prompt, progress, cancel)?;
        let has_tokens = raw["transcription"].as_array().is_some_and(|s| s.iter().any(|seg| seg["tokens"].as_array().is_some_and(|t| !t.is_empty())));
        if has_tokens && map.is_none() {
            let (raw, _) = self.run(req, None, prompt, progress, cancel)?;
            return Ok((raw, Vad::Unused("no_time_map"), None));
        }
        Ok((raw, Vad::Used(sha), map))
    }
}

#[cfg(test)]
mod vad_tests {
    use super::*;

    /// whisper-cli 1.9.5's log of a `--vad` run (the start of a real one, the table cut to three segments).
    pub(super) const LOG: &str = "\
whisper_vad: vad_segment_info: orig_start: 8.87, orig_end: 11.04, vad_start: 0.00, vad_end: 2.17
whisper_vad: vad_segment_info: orig_start: 11.55, orig_end: 12.99, vad_start: 2.37, vad_end: 3.81
whisper_vad: vad_segment_info: orig_start: 13.79, orig_end: 15.77, vad_start: 4.01, vad_end: 5.99
whisper_vad: Created time mapping table with 8 points
";

    #[test]
    fn the_time_map_is_whisper_cpps_own() {
        let m = VadMap::parse(LOG).unwrap();
        assert_eq!(m.points, [(0, 8870), (2170, 11040), (2270, 11040), (2370, 11550), (3810, 12990), (3910, 12990), (4010, 13790), (5990, 15770)]);
        // inside a segment: shifted; in the 0.1 s of inserted silence: stretched over the real gap
        assert_eq!(m.original_ms(20), 8890);
        assert_eq!(m.original_ms(4040), 13820);
        assert_eq!(m.original_ms(2200), 11040);
        assert_eq!(m.original_ms(2300), 11193);
        // before and after the table: its ends
        assert_eq!((m.original_ms(-5), m.original_ms(9000)), (8870, 15770));
    }

    #[test]
    fn a_table_that_does_not_add_up_is_not_used() {
        assert_eq!(VadMap::parse(&LOG.replace("with 8 points", "with 9 points")), None);
        assert_eq!(VadMap::parse(&LOG.replace("whisper_vad: Created time mapping table with 8 points\n", "")), None);
        assert_eq!(VadMap::parse(&LOG.replace("orig_end: 12.99", "orig_stop: 12.99")), None);
        assert_eq!(VadMap::parse("whisper_vad: Created time mapping table with 0 points\n"), None);
    }

    #[test]
    fn token_times_go_through_the_map_and_segment_times_do_not() {
        let tok = |text: &str, dtw: i64| json!({"text": text, "p": 0.9, "t_dtw": dtw, "offsets": {"from": dtw * 10, "to": dtw * 10 + 100}});
        // whisper.cpp already mapped the segment (8870..12990); its tokens are still on the decoded audio
        let raw = json!({"result": {"language": "ko"}, "transcription": [
            {"offsets": {"from": 8870, "to": 12990}, "tokens": [tok(" It", 2), tok(" was", 150), tok(" bad.", 300)]}]});
        let m = VadMap::parse(LOG).unwrap();
        let r = adapt_mapped(&raw, &"0".repeat(64), &json!({}), "1.9.5", "m", None, Some(&m)).unwrap();
        let w = r["words"].as_array().unwrap();
        assert_eq!(w.iter().map(|w| w["start_ms"].as_i64().unwrap()).collect::<Vec<_>>(), [8890, 10370, 12180]);
        assert_eq!(w[2]["end_ms"], 12990, "the last word ends with its segment");
        // without the map the same JSON would put them at the start of the recording
        let plain = adapt(&raw, &"0".repeat(64), &json!({}), "1.9.5", "m", None).unwrap();
        assert_eq!(plain["words"][0]["start_ms"], 20);
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
        let engine = WhisperCli { cli, model, model_sha256: None, dtw: "large.v3.turbo".into(), vad: None, hints: vec![] };
        let wav = tmp.path().join("in.wav");
        std::fs::write(&wav, "x").unwrap();
        let req = BatchRequest { wav: &wav, audio_sha256: &"0".repeat(64), language: "ko", language_explicit: true, channel: 0, channels: 1, data_dir: tmp.path() };
        engine.transcribe(&req, &|_| {}, &AtomicBool::new(false)).unwrap();
        let env = std::fs::read_to_string(tmp.path().join("env.out")).unwrap();
        assert!(!env.contains("MEETKO_TEST_SECRET"), "the secret reached whisper-cli");
        assert!(env.lines().any(|l| l.starts_with("PATH=")), "PATH is on the allowlist");
    }

    /// A fake whisper-cli for `--vad` runs: with `--vad` it logs `log` on stderr (as 1.9.5 does, among
    /// other lines) and writes token times on the shortened audio; without, the times of the recording.
    /// Every run's arguments go to `runs.txt`.
    fn vad_cli(dir: &Path, log: &str) -> WhisperCli {
        std::fs::write(dir.join("vad.log"), log).unwrap();
        // a segment whisper.cpp already mapped (8870..12990) whose tokens are on the decoded audio
        std::fs::write(dir.join("vad.json"), r#"{"result":{"language":"ko"},"transcription":[{"offsets":{"from":8870,"to":12990},"tokens":[
{"text":" It","p":0.9,"t_dtw":2,"offsets":{"from":20,"to":120}},{"text":" was","p":0.9,"t_dtw":150,"offsets":{"from":1500,"to":1600}},
{"text":" bad.","p":0.9,"t_dtw":300,"offsets":{"from":3000,"to":3100}}]}]}"#).unwrap();
        std::fs::write(dir.join("plain.json"), r#"{"result":{"language":"ko"},"transcription":[{"offsets":{"from":8000,"to":13000},"tokens":[
{"text":" plain","p":0.9,"t_dtw":900,"offsets":{"from":9000,"to":9100}}]}]}"#).unwrap();
        let cli = dir.join("whisper-cli");
        let script = format!(
            r#"#!/bin/sh
d="{0}"
[ "$1" = --version ] && {{ echo "whisper.cpp version: 1.9.5"; exit 0; }}
echo "$@" >> "$d/runs.txt"
vad=0; for a in "$@"; do [ "$a" = --vad ] && vad=1; done
while [ $# -gt 0 ]; do [ "$1" = -of ] && stem="$2"; shift; done
if [ $vad = 1 ]; then
  echo "whisper_vad: detected 3 speech segments" >&2
  cat "$d/vad.log" >&2
  echo "whisper_print_progress_callback: progress = 100%" >&2
  cp "$d/vad.json" "$stem.json"
else
  cp "$d/plain.json" "$stem.json"
fi
"#,
            dir.display()
        );
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let model = dir.join("model.bin");
        std::fs::write(&model, "m").unwrap();
        WhisperCli { cli, model, model_sha256: None, dtw: "large.v3.turbo".into(), vad: None, hints: vec![] }
    }

    fn vad_req(dir: &Path) -> (PathBuf, PathBuf) {
        let (wav, vm) = (dir.join("in.wav"), dir.join("vad.bin"));
        std::fs::write(&wav, "x").unwrap();
        std::fs::write(&vm, "v").unwrap();
        (wav, vm)
    }

    #[test]
    fn a_vad_run_maps_its_word_times_from_whisper_cpps_log() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let engine = vad_cli(t, super::vad_tests::LOG);
        let (wav, vm) = vad_req(t);
        let req = BatchRequest { wav: &wav, audio_sha256: &"0".repeat(64), language: "ko", language_explicit: true, channel: 0, channels: 1, data_dir: t };
        let (raw, vad, map) = engine.decode(&req, Some((&vm, "a".repeat(64).leak())), None, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert!(matches!(vad, Vad::Used(_)));
        let runs = std::fs::read_to_string(t.join("runs.txt")).unwrap();
        assert_eq!(runs.lines().count(), 1);
        let args: Vec<&str> = runs.split_whitespace().collect();
        assert!(args.contains(&"--vad") && !args.contains(&"-np"), "with VAD the log is on: {runs}");
        let flag = |f: &str| args.iter().position(|w| *w == f).map(|i| args[i + 1]);
        assert_eq!(flag("-vm"), Some(vm.to_str().unwrap()));
        let params = params_used("ko", "large.v3.turbo", &vad, crate::speech_gate::Verdict::Speech, &Hints::None);
        for (f, k) in [("-vt", "vad_threshold"), ("-vspd", "vad_min_speech_ms"), ("-vsd", "vad_min_silence_ms"), ("-vp", "vad_speech_pad_ms"), ("-vo", "vad_samples_overlap")] {
            assert_eq!(flag(f).unwrap().parse::<f64>().unwrap(), params[k].as_f64().unwrap(), "{k}");
        }
        let r = adapt_mapped(&raw, &"0".repeat(64), &params, "1.9.5", "m", None, map.as_ref()).unwrap();
        let starts: Vec<i64> = r["words"].as_array().unwrap().iter().map(|w| w["start_ms"].as_i64().unwrap()).collect();
        assert_eq!(starts, [8890, 10370, 12180], "word times are on the recording, not on the shortened audio");
        assert_eq!(r["params"]["vad_model"]["sha256"], "a".repeat(64));
    }

    #[test]
    fn a_vad_run_without_a_readable_time_map_is_run_again_without_vad() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let engine = vad_cli(t, "whisper_vad: some other log line\n");
        let (wav, vm) = vad_req(t);
        let req = BatchRequest { wav: &wav, audio_sha256: &"0".repeat(64), language: "ko", language_explicit: true, channel: 0, channels: 1, data_dir: t };
        let (raw, vad, map) = engine.decode(&req, Some((&vm, "a".repeat(64).leak())), None, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!((vad, map), (Vad::Unused("no_time_map"), None));
        let runs: Vec<String> = std::fs::read_to_string(t.join("runs.txt")).unwrap().lines().map(String::from).collect();
        assert_eq!(runs.len(), 2);
        assert!(runs[0].contains("--vad") && !runs[1].contains("--vad") && runs[1].contains("-np"));
        assert_eq!(raw["transcription"][0]["tokens"][0]["text"], " plain", "the result of the run without VAD");
    }
}
