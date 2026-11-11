//! Batch transcription by an engine process (role `asr.batch`), the alternative to whisper-cli for
//! 다시 받아쓰기. The one that exists is `engines/qwen3-asr` (Qwen3-ASR-1.7B with Qwen3-ForcedAligner
//! word times, experimental).
//!
//! Like diarization (`diarize.rs`) it is a separate native process speaking NDJSON JSON-RPC over stdio:
//! `transcribe.run {wav, audio_sha256, language}` answers a contract `asr.batch` engine result, and
//! `transcribe.progress {percent}` notifications come while it runs. The model is native-parser input
//! and is handled like the others (`live.rs`): copied into a private directory while it is hashed,
//! checked against the pin in `models.toml`, and only that copy is loaded; the recorded `model.sha256`
//! is the combined digest of what was staged, whatever the engine says. One process serves all
//! channels of a run (the model loads once) and ends with the `BatchProcess`. On macOS it runs under
//! `sandbox::engine_profile` (no network, no other program, no AppleEvents or LaunchServices, writes
//! only to its own HOME and the Metal caches, no reads of the user's home or the data dir beyond its own
//! files, the staged model and the temp folder), recorded as `params.sandbox`. Fail closed: when the
//! profile cannot be applied it does not run, unless `MEETKO_BATCH_SANDBOX=off` (development, recorded
//! as `params.sandbox = "off"`).
//!
//! The language: an engine of this kind detects it, so the request says `auto` unless the meeting or
//! the caller named one (`BatchRequest::language_explicit`); whisper keeps its `ko` default.
//!
//! Which batch engine 다시 받아쓰기 uses is the user's choice (the app keeps it); whisper stays the default.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::live::{self, EngineClient, SharedModel, ROLE_BATCH};
use crate::util;
use crate::whisper::{BatchEngine, BatchRequest, Progress};

const ENGINE_DIR: &str = "qwen3-asr";
const ENGINE_BIN: &str = "meetko-qwen3-asr";

/// Time budget of a request: this, plus twice the length of the audio (a recording is transcribed
/// several times faster than real time on the machine measured; a CPU-only machine is slower, and
/// the first request also loads the models).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// The batch engines the user can choose from (the app keeps the choice as this id).
pub const WHISPER: &str = "whisper";
pub const QWEN3_ASR: &str = "qwen3-asr";
pub const ENGINES: [&str; 2] = [WHISPER, QWEN3_ASR];

#[derive(Debug, Clone)]
pub struct BatchConfig {
    pub engine_bin: PathBuf,
    pub model_dir: PathBuf,
    /// accept a model that is not in `models.toml` (development only; recorded in provenance)
    pub unpinned: bool,
    /// added to twice the audio length to make the time budget of a request
    pub timeout: Duration,
    /// macOS: run the engine under `sandbox::engine_profile`. On in the
    /// app; the core's tests turn it off for their fake engine, a script whose interpreter is another program.
    pub sandbox: bool,
}

impl BatchConfig {
    /// The real environment: `MEETKO_BATCH_MODEL` (the model directory; unset = the one the app keeps in
    /// `<data dir>/models/`), `MEETKO_BATCH_ENGINE` (the binary; else a sibling of the executable, else
    /// the dev build), `MEETKO_BATCH_MODEL_UNPINNED=1` (development, only for a directory the variable
    /// named) and `MEETKO_BATCH_TIMEOUT_SECS`. Unlike diarization this is asked for, so a missing model
    /// or engine is an error.
    pub fn from_env() -> Result<BatchConfig> {
        let env = std::env::var_os("MEETKO_BATCH_MODEL").map(PathBuf::from);
        let data_dir = util::data_dir();
        let (model_dir, from_env) = live::model_dir_for(ROLE_BATCH, env, &data_dir)
            .ok_or_else(|| Error::ModelMissing("the Qwen3-ASR model is not set up (Settings, or MEETKO_BATCH_MODEL)".into()))?;
        let engine_bin = find_engine()?;
        let unpinned = from_env && std::env::var("MEETKO_BATCH_MODEL_UNPINNED").is_ok_and(|v| v == "1");
        live::check_model_for(ROLE_BATCH, &model_dir, unpinned, &data_dir.join("model-sha256-cache.json"))?;
        let timeout = std::env::var("MEETKO_BATCH_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).map_or(DEFAULT_TIMEOUT, Duration::from_secs);
        let sandbox = if !cfg!(target_os = "macos") {
            false
        } else if std::env::var("MEETKO_BATCH_SANDBOX").is_ok_and(|v| v == "off") {
            eprintln!("MEETKO_BATCH_SANDBOX=off: the batch engine runs without its sandbox (development only)");
            false
        } else {
            crate::sandbox::engine_preflight()?;
            true
        };
        Ok(BatchConfig { engine_bin, model_dir, unpinned, timeout, sandbox })
    }
}

/// Whether this system can run the engine under `sandbox::engine_profile` (macOS, and the profile applies).
pub fn sandbox_available() -> bool {
    crate::sandbox::engine_preflight().is_ok()
}

/// What `params.sandbox` says: the profile, `off` (macOS without it: development), or `none` (no sandbox
/// on this system yet).
fn sandbox_label(on: bool) -> &'static str {
    if on {
        crate::sandbox::MACOS_SBPL
    } else if cfg!(target_os = "macos") {
        crate::sandbox::OFF
    } else {
        crate::sandbox::NONE
    }
}

/// The engine's profile, from its private HOME and absolute binary (`EngineClient::spawn_sandboxed`):
/// it reads its own directory and libraries (an app bundle's Frameworks/ or lib/, the repository's
/// vendor/lib for a dev build), the staged model and the temp folder (the wav of every request is staged
/// there), and nothing else of the user's home or the data dir.
fn profile_for(staged: &Path, home: &Path, bin: &Path) -> Result<String> {
    let dir = bin.parent().unwrap_or(Path::new("/"));
    let mut allow_read = vec![dir.to_path_buf(), staged.to_path_buf(), std::env::temp_dir()];
    allow_read.extend(["../Frameworks", "../lib", "../../vendor/lib"].iter().filter_map(|r| dunce::canonicalize(dir.join(r)).ok()));
    let mut deny_read = vec![util::data_dir()];
    deny_read.extend(std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from));
    crate::sandbox::engine_profile(&crate::sandbox::EngineSandbox { exe: bin.into(), home: home.into(), deny_read, allow_read, allow_write: crate::sandbox::metal_cache_dirs() })
}

/// The batch engine's binary with the real environment: `MEETKO_BATCH_ENGINE`, else a sibling of the
/// executable, else (debug builds) the dev build. Never from a data-dir file.
pub fn find_engine() -> Result<PathBuf> {
    let explicit = std::env::var_os("MEETKO_BATCH_ENGINE").filter(|v| !v.is_empty()).map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    live::pick_bin(
        explicit,
        "MEETKO_BATCH_ENGINE",
        "the Qwen3-ASR engine",
        exe.as_deref().and_then(Path::parent),
        std::env::current_dir().ok().as_deref(),
        ENGINE_DIR,
        &live::exe_name(ENGINE_BIN),
    )
}

/// The engine process behind `BatchEngine`, started on the first channel and kept for the next. A
/// process that failed is dropped (killed with its group). The staged model lives as long as this value.
pub struct BatchProcess {
    cfg: BatchConfig,
    model: SharedModel,
    client: Mutex<Option<(EngineClient, Receiver<u8>)>>,
}

impl BatchProcess {
    pub fn new(cfg: BatchConfig) -> BatchProcess {
        BatchProcess { cfg, model: SharedModel::new(ROLE_BATCH), client: Mutex::new(None) }
    }
}

impl BatchEngine for BatchProcess {
    fn transcribe(&self, req: &BatchRequest, progress: &dyn Fn(Progress), cancel: &AtomicBool) -> Result<Value> {
        let abort = || cancel.load(Ordering::SeqCst);
        let staged = self.model.get(&self.cfg.model_dir, self.cfg.unpinned, &abort)?;
        let mut slot = self.client.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            let (tx, rx) = mpsc::sync_channel::<u8>(64);
            let on_progress = move |p: Value| {
                if let Some(n) = p["percent"].as_u64() {
                    let _ = tx.try_send(n.min(100) as u8); // a full queue drops a step, never blocks the reader
                }
            };
            let staged_path = staged.path.clone();
            let make = move |home: &Path, bin: &Path| profile_for(&staged_path, home, bin);
            let profile: Option<live::MakeProfile> = if self.cfg.sandbox { Some(&make) } else { None };
            let client = EngineClient::spawn_sandboxed(&self.cfg.engine_bin, profile, ("MEETKO_BATCH_MODEL", &staged.path), ROLE_BATCH, 4, on_progress, &abort)?;
            *slot = Some((client, rx));
        }
        let (client, rx) = slot.as_mut().expect("started above");
        while rx.try_recv().is_ok() {} // steps of an earlier channel that arrived late
        let wav = req.wav.to_str().ok_or_else(|| Error::Engine("the wav path is not UTF-8".into()))?;
        // the model detects the language unless the meeting or the caller named one: Korean meetings
        // with English terms are what it is for, and a forced language can make it translate
        let language = if req.language_explicit { req.language } else { "auto" };
        let params = json!({ "wav": wav, "audio_sha256": req.audio_sha256, "language": language });
        let duration = std::fs::metadata(req.wav).map(|m| m.len() / 32).unwrap_or(0); // 16 kHz mono PCM16: 32 bytes per ms
        let budget = self.cfg.timeout + Duration::from_millis(duration.saturating_mul(2));
        // the wait polls `abort` every 50 ms: the engine's progress is passed on from there
        let step = || {
            while let Ok(p) = rx.try_recv() {
                progress(Progress { channel: req.channel, channels: req.channels, percent: p, diarizing: false });
            }
            abort()
        };
        let res = match client.call_with("transcribe.run", params, budget, &step) {
            Ok(r) => r,
            Err(e) => {
                let detail = client.fault().or_else(|| matches!(e, Error::Engine(_)).then(|| client.stderr_tail()));
                *slot = None; // killed on drop; the next channel starts a new one
                if let Some(d) = detail.filter(|d| !d.trim().is_empty()) {
                    eprintln!("batch engine: {}", d.trim());
                }
                return Err(e);
            }
        };
        step();
        let mut res = checked(res, req.audio_sha256, &staged.check.digest, staged.check.pinned).inspect_err(|_| {
            *slot = None; // an engine that answers like this is not trusted with the next channel
        })?;
        // how the core ran it, whatever the engine says
        res["params"]["sandbox"] = json!(sandbox_label(self.cfg.sandbox));
        Ok(res)
    }
}

impl Drop for BatchProcess {
    fn drop(&mut self) {
        if let Some((c, _)) = self.client.get_mut().unwrap_or_else(|p| p.into_inner()).take() {
            c.shutdown(&|| false);
        }
    }
}

/// An engine's answer as the result the core records: an `asr.batch` result for exactly this audio,
/// with the staged model's digest and whether it was pinned (what the core verified, not what the
/// engine says). The schema is checked by the pipeline like whisper's.
pub fn checked(mut res: Value, audio_sha256: &str, digest: &str, pinned: bool) -> Result<Value> {
    if !res.is_object() || res["kind"] != "asr.batch" {
        return Err(Error::BadEngineOutput("the batch engine did not answer an asr.batch result".into()));
    }
    if res["audio_sha256"] != audio_sha256 {
        return Err(Error::BadEngineOutput("the batch engine answered for different audio".into()));
    }
    if !res["model"].is_object() {
        res["model"] = json!({ "id": "unknown" });
    }
    res["model"]["sha256"] = json!(digest);
    if !res["params"].is_object() {
        res["params"] = json!({});
    }
    res["params"]["model_pinned"] = json!(pinned);
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_core_records_what_it_staged() {
        let sha = "ab".repeat(32);
        let r = json!({"kind": "asr.batch", "audio_sha256": sha, "model": {"id": "m", "sha256": "0".repeat(64)}, "params": {"a": 1}});
        let out = checked(r, &sha, &"cd".repeat(32), true).unwrap();
        assert_eq!(out["model"]["sha256"], "cd".repeat(32));
        assert_eq!(out["params"], json!({"a": 1, "model_pinned": true}));
        assert!(checked(json!({"kind": "asr.batch", "audio_sha256": "0".repeat(64)}), &sha, "x", true).is_err(), "other audio");
        assert!(checked(json!({"kind": "diarization", "audio_sha256": sha}), &sha, "x", true).is_err());
        assert!(checked(json!([]), &sha, "x", true).is_err());
    }
}
