//! Live transcription while a capture runs.
//!
//! The streaming engine is a separate native process (role `asr.stream`, NDJSON JSON-RPC over
//! stdio like plugins, one process per channel). A `LiveSession` tails the capture provider's
//! growing `mic.wav` / `loopback.wav`, pushes 100 ms chunks to that channel's engine, folds the
//! returned `asr.stream` events by replace-by-revision, and reports captions through a callback.
//! After the capture stops, `finish` drains the files and closes the engines (within a deadline);
//! `store_live` turns the result into a `live` version with the same `live_words` + `compose` the
//! batch path uses.
//!
//! Recording is the priority: nothing here can fail or block the capture. An engine that cannot
//! start, dies, floods, or reads a damaged input only means fewer captions, reported as a quiet
//! `LiveUpdate::Error`. The WAV file is the buffer: a slow engine is waited for, no audio is dropped.
//!
//! Live transcripts are not byte-for-byte what the Python experiment produced. Word starts matched
//! it to the millisecond in the measured case, but the vendored onnxruntime differs numerically
//! from the pip wheel's, so a borderline endpoint (a trailing ".") can flip: one extra segment and
//! one character in 69 s of audio.
//!
//! The model is native-parser input. It is copied into a private directory while it is hashed,
//! checked against the pins in `engines/*/models.toml`, and only that copy is loaded.
//!
//! Two engines exist (`engines/sherpa-stream`, `engines/nemotron-stream`). The model directory
//! decides which one runs: each pinned model belongs to one engine, and `MEETKO_STREAM_ENGINE`
//! overrides the choice of binary.
//!
//! The same pins, staging and binary lookup serve the batch diarization engine (`crate::diarize`):
//! a pin carries the `role` of the engine that loads it (default `asr.stream`).
//!
//! With a diarization model and engine (`LiveSession::start_with`), each channel also runs a live
//! speaker separation next to its captions (`diar`): provisional speakers of the caption segments
//! while recording, and the speakers of the stored live version.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::contract::{compose, engine_result_validator, live_words, schema_errors, ChannelInput, EngineWord, MeetingData, StreamEvent, VersionKind, CONTRACT};
use crate::diarize::DiarConfig;
use crate::error::{Error, Result};
use crate::host::{read_lines, Line};
use crate::pipeline::{batch_channel, with_diarization};

pub mod diar;
pub use diar::{DiarLive, SegmentSpeaker};
use crate::store::Store;
use crate::util::{self, kill_tree, own_process_group};

/// Audio per `stream.push`. The engine sees exactly what the Python experiment measured.
pub const CHUNK_MS: usize = 100;
const CHUNK_BYTES: usize = 16 * 2 * CHUNK_MS;
/// How often the tailers look for new audio; the provider appends in bursts of its own.
const POLL: Duration = Duration::from_millis(20);
/// Loading the model is the slow part of `stream.open`.
const OPEN_TIMEOUT: Duration = Duration::from_secs(60);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `finish` waits for the engines before it kills them and keeps what was folded.
pub const FINISH_DEADLINE: Duration = Duration::from_secs(15);
/// Pending pushes the writer thread may hold (~100 s of audio). When it is full the tailer waits.
pub const WRITE_QUEUE: usize = 1024;
/// Audio read ahead of the engine; the file holds the rest.
const MAX_BUFFERED: usize = 1 << 20;

/// Bounds on what an engine may make the core hold or process.
pub const MAX_SEGMENTS: usize = 10_000;
pub const MAX_UNITS: usize = 2_000;
pub const MAX_FOLDED_BYTES: usize = 64 << 20;
pub const MAX_EVENTS_PER_SEC: u32 = 200;
const MAX_HOT_SECONDS: u32 = 2;

/// The engines and the pins of the models each one loads (`engines/<name>/models.toml`).
const ENGINES: [(&str, &str); 4] = [
    ("nemotron-stream", include_str!("../../../engines/nemotron-stream/models.toml")),
    ("sherpa-stream", include_str!("../../../engines/sherpa-stream/models.toml")),
    ("qwen3-asr", include_str!("../../../engines/qwen3-asr/models.toml")),
    // whisper-cli is the user's program; this file pins only the model data meetko hands to it
    ("whisper-cpp", include_str!("../../../engines/whisper-cpp/models.toml")),
];

pub const ROLE_STREAM: &str = "asr.stream";
pub const ROLE_DIARIZATION: &str = "diarization";
/// A batch transcription engine other than whisper-cli (`batch.rs`).
pub const ROLE_BATCH: &str = "asr.batch";
/// The voice activity detection model whisper-cli runs before it transcribes (`whisper.rs`).
pub const ROLE_WHISPER_VAD: &str = "whisper.vad";

/// File name of an engine's binary: `sherpa-stream` -> `meetko-sherpa-stream[.exe]`.
fn engine_bin(engine: &str) -> String {
    exe_name(&format!("meetko-{engine}"))
}

pub(crate) fn exe_name(bin: &str) -> String {
    format!("{bin}{}", if cfg!(windows) { ".exe" } else { "" })
}

// ---------------------------------------------------------------- configuration

#[derive(Debug, Clone)]
pub struct LiveConfig {
    pub engine_bin: PathBuf,
    pub model_dir: PathBuf,
    pub language: String,
    /// accept a model that is not in `models.toml` (development only; recorded in provenance)
    pub unpinned: bool,
    /// pending `stream.push` messages per engine
    pub queue: usize,
}

/// Where the binary of `engine` (a directory name under `engines/`) is: `explicit` (env
/// `MEETKO_STREAM_ENGINE`), else a sibling of the app executable, else (debug builds only) the dev
/// build. Never read from config.toml, for the reason `capture_run::pick_bin` gives. Pure over its
/// inputs so it can be tested.
pub fn pick_engine(explicit: Option<PathBuf>, exe_dir: Option<&Path>, cwd: Option<&Path>, engine: &str) -> Result<PathBuf> {
    pick_bin(explicit, "MEETKO_STREAM_ENGINE", "the live caption engine", exe_dir, cwd, engine, &engine_bin(engine))
}

/// `pick_engine` for any binary: `dir` is the directory under `engines/`, `bin` the file name the
/// build produces there (one directory may build several), `var` and `what` name it in the errors.
pub(crate) fn pick_bin(explicit: Option<PathBuf>, var: &str, what: &str, exe_dir: Option<&Path>, cwd: Option<&Path>, dir: &str, bin: &str) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return if p.is_file() { Ok(p) } else { Err(Error::EngineNotFound(format!("{var} does not point to a file"))) };
    }
    if let Some(p) = exe_dir.map(|d| d.join(bin)).filter(|p| p.is_file()) {
        return Ok(p);
    }
    // the dev build, relative to the repository root (debug builds only, like the capture provider:
    // a release app never runs a binary found by walking directories). Only the debug build carries
    // the repository-relative rpaths.
    #[cfg(debug_assertions)]
    for root in [exe_dir, cwd].into_iter().flatten() {
        if let Some(p) = root.ancestors().map(|a| a.join(format!("engines/{dir}/target/debug/{bin}"))).find(|p| p.is_file()) {
            return Ok(p);
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = (cwd, dir);
    Err(Error::EngineNotFound(format!("{what} was not found: set {var} or put {bin} next to the app")))
}

/// The binary of the live caption engine `engine` with the real environment (see `pick_engine`).
pub fn find_engine(engine: &str) -> Result<PathBuf> {
    let explicit = std::env::var_os("MEETKO_STREAM_ENGINE").filter(|v| !v.is_empty()).map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    pick_engine(explicit, exe.as_deref().and_then(Path::parent), std::env::current_dir().ok().as_deref(), engine)
}

// ---- model pins

#[derive(Debug, Deserialize)]
struct Pin {
    id: String,
    /// the role of the engine that loads this model
    #[serde(default = "stream_role")]
    role: String,
    files: BTreeMap<String, String>,
    /// the engine that loads this model (the `ENGINES` entry the pin came from)
    #[serde(skip)]
    engine: &'static str,
}

fn stream_role() -> String {
    ROLE_STREAM.into()
}

/// How the errors name the model of a role and the variable that points at it.
fn names(role: &str) -> (&'static str, &'static str) {
    if role == ROLE_DIARIZATION {
        ("the diarization model", "MEETKO_DIAR_MODEL")
    } else if role == ROLE_BATCH {
        ("the batch transcription model", "MEETKO_BATCH_MODEL")
    } else if role == ROLE_WHISPER_VAD {
        ("the whisper VAD model", "MEETKO_WHISPER_VAD_MODEL")
    } else {
        ("the streaming model", "MEETKO_STREAM_MODEL")
    }
}

#[derive(Debug, Deserialize)]
struct Pins {
    model: Vec<Pin>,
}

fn pins() -> &'static [Pin] {
    static P: OnceLock<Vec<Pin>> = OnceLock::new();
    P.get_or_init(|| {
        ENGINES
            .iter()
            .flat_map(|(engine, text)| {
                let pins: Pins = toml::from_str(text).expect("models.toml parses");
                pins.model.into_iter().map(move |p| Pin { engine, ..p })
            })
            .collect()
    })
}

/// The id of the pinned model whose files all match `files` (name -> sha256).
pub fn pinned_model(files: &BTreeMap<String, String>) -> Option<&'static str> {
    pins().iter().find(|p| p.files.iter().all(|(f, h)| files.get(f) == Some(h))).map(|p| p.id.as_str())
}

/// The pinned sha256 of `file` in the model `model_id`.
pub fn pinned_file_sha256(model_id: &str, file: &str) -> Option<&'static str> {
    pins().iter().find(|p| p.id == model_id).and_then(|p| p.files.get(file)).map(String::as_str)
}

/// The model directory to use for `role`, and whether the environment named it: the one in
/// `MEETKO_STREAM_MODEL` / `MEETKO_DIAR_MODEL` always wins, else the app-managed one when it holds a
/// model. Data only: the files are checked against the pins wherever they come from, and the
/// development switch `*_UNPINNED` applies to a directory the environment named, never to the app's.
pub fn model_dir_for(role: &str, env: Option<PathBuf>, data_dir: &Path) -> Option<(PathBuf, bool)> {
    match env.filter(|p| !p.as_os_str().is_empty()) {
        Some(p) => Some((p, true)),
        None => crate::model_setup::managed_model_dir(data_dir, role).map(|p| (p, false)),
    }
}

/// The known model whose files all exist in `dir`: it says which files to read and which engine
/// runs them. An unpinned directory (development) is still laid out like a known model.
fn layout(role: &str, dir: &Path) -> Result<&'static Pin> {
    let of_role = || pins().iter().filter(|p| p.role == role);
    let what = names(role).0;
    of_role().find(|p| p.files.keys().all(|f| dir.join(f).is_file())).ok_or_else(|| {
        // a directory that has some of one model's files is that model, incomplete: name what is missing
        if let Some((p, f)) = of_role().filter(|p| p.files.keys().any(|f| dir.join(f).is_file())).find_map(|p| p.files.keys().find(|f| !dir.join(f).is_file()).map(|f| (p, f))) {
            return Error::ModelMissing(format!("{f} is missing in {what} directory ({})", p.id));
        }
        let known: Vec<String> = of_role().map(|p| format!("{} ({})", p.id, p.files.keys().cloned().collect::<Vec<_>>().join(", "))).collect();
        Error::ModelMissing(format!("{what} directory holds none of the known models; each needs its files: {}", known.join("; ")))
    })
}

/// Canonical combined digest: sha256 over `"<file>:<sha256>\n"` lines sorted by file name.
pub fn model_digest(files: &BTreeMap<String, String>) -> String {
    util::sha256_bytes(files.iter().map(|(n, h)| format!("{n}:{h}\n")).collect::<String>().as_bytes())
}

/// What a model directory is: its combined digest and whether it is a pinned model.
#[derive(Debug, Clone)]
pub struct ModelCheck {
    pub digest: String,
    pub pinned: bool,
    /// the engine (directory name under `engines/`) that loads this kind of model
    pub engine: &'static str,
}

fn decide(role: &str, files: &BTreeMap<String, String>, engine: &'static str, allow_unpinned: bool) -> Result<ModelCheck> {
    let pinned = pins().iter().any(|p| p.role == role && p.files.iter().all(|(f, h)| files.get(f) == Some(h)));
    if !pinned && !allow_unpinned {
        let (what, var) = names(role);
        return Err(Error::ModelMissing(format!("{what} is not one of the pinned models ({var}_UNPINNED=1 allows it for development)")));
    }
    Ok(ModelCheck { digest: model_digest(files), pinned, engine })
}

/// sha256 of a file, cached by path hash + size + mtime_ns + inode in a 0600 file (like the
/// whisper model hash). Only for the cheap pre-flight check: what the engine loads is hashed
/// from its own private copy (`stage_model`), never taken from this cache.
pub(crate) fn cached_sha256(path: &Path, cache_path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path)?;
    let (len, mtime, inode) = crate::whisper::model_stamp(&meta);
    let canon = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let key = format!("{}:{len}:{mtime}:{inode}", util::sha256_bytes(canon.to_string_lossy().as_bytes()));
    let mut cache: serde_json::Map<String, Value> = std::fs::read_to_string(cache_path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    if let Some(s) = cache.get(&key).and_then(Value::as_str).filter(|s| util::is_sha256(s)) {
        return Ok(s.to_string());
    }
    let sha = util::sha256_file(path)?;
    cache.insert(key, json!(sha));
    // the cache only saves time: a status check must not fail because it cannot be written
    let write = || -> Result<()> {
        if let Some(d) = cache_path.parent() {
            util::create_private_dir(d)?;
        }
        util::write_private(cache_path, serde_json::to_string(&cache)?.as_bytes())?;
        Ok(())
    };
    if let Err(e) = write() {
        eprintln!("model hash cache not written: {e}");
    }
    Ok(sha)
}

/// Record a sha256 the caller has just verified for `path` (a model it wrote), so the next status
/// check does not hash it again. Best effort, like the cache itself.
pub fn remember_sha256(path: &Path, sha: &str, cache_path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    let (len, mtime, inode) = crate::whisper::model_stamp(&meta);
    let canon = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let key = format!("{}:{len}:{mtime}:{inode}", util::sha256_bytes(canon.to_string_lossy().as_bytes()));
    let mut cache: serde_json::Map<String, Value> = std::fs::read_to_string(cache_path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    cache.insert(key, json!(sha));
    if let Ok(text) = serde_json::to_string(&cache) {
        let _ = util::write_private(cache_path, text.as_bytes());
    }
}

/// Pre-flight: the files of a known model exist and match a pin (or unpinned is allowed). Never names a path.
pub fn check_model(dir: &Path, allow_unpinned: bool, cache_path: &Path) -> Result<ModelCheck> {
    check_model_for(ROLE_STREAM, dir, allow_unpinned, cache_path)
}

pub fn check_model_for(role: &str, dir: &Path, allow_unpinned: bool, cache_path: &Path) -> Result<ModelCheck> {
    if !dir.is_dir() {
        return Err(Error::ModelMissing(format!("{} does not point to a model directory", names(role).1)));
    }
    let known = layout(role, dir)?;
    let mut files = BTreeMap::new();
    for f in known.files.keys() {
        files.insert(f.clone(), cached_sha256(&dir.join(f), cache_path)?);
    }
    decide(role, &files, known.engine, allow_unpinned)
}

/// The verified model the engine will load: a private copy, hashed while it was made, so the
/// bytes that were checked are the bytes that are loaded (no check-then-load race, and a plugin
/// that can write the source directory cannot swap the model afterwards). A plain copy of ~140 MB
/// costs a few seconds (742 MB for Nemotron); the directory goes away with this value. A recording
/// stages once and its channels share the result (`SharedModel`).
pub struct StagedModel {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
    pub check: ModelCheck,
}

const STAGE_PREFIX: &str = "meetko-model-";

/// Remove staging directories that a crashed process left behind: `meetko-model-<pid>-*` whose
/// process is gone. Runs once per process; unix only (elsewhere the directories stay). A live
/// process, or a pid that cannot be told apart, is never touched.
fn clean_stale_stages() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[cfg(unix)]
        {
            let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) else { return };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(pid) = name.strip_prefix(STAGE_PREFIX).and_then(|r| r.split('-').next()).and_then(|p| p.parse::<u32>().ok()) else { continue };
                let alive = pid == std::process::id()
                    || Command::new("kill").args(["-0", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status().map_or(true, |s| s.success());
                // only real directories of ours: a symlink named like one is left alone
                if !alive && e.file_type().is_ok_and(|t| t.is_dir()) {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
    });
}

/// The staged model of one recording, made by the first channel that needs it and shared by the
/// others; the directory goes away when the last channel lets go. A failure is remembered too, so
/// the second channel does not repeat a 742 MB copy to fail the same way.
pub struct SharedModel {
    role: &'static str,
    cell: Mutex<Option<Result<Arc<StagedModel>>>>,
}

impl Default for SharedModel {
    fn default() -> Self {
        SharedModel::new(ROLE_STREAM)
    }
}

/// A remembered refusal, said again with its kind kept (the errors cannot be cloned). What staging
/// can fail with is an I/O error or a model problem; anything else keeps its text.
fn replay(e: &Error) -> Error {
    match e {
        Error::Io(io) => Error::Io(std::io::Error::new(io.kind(), io.to_string())), // the text has no "io: " prefix yet
        Error::ModelMissing(m) => Error::ModelMissing(m.clone()),
        other => Error::ModelMissing(other.to_string()),
    }
}

impl SharedModel {
    /// For the models of engines of `role`.
    pub fn new(role: &'static str) -> SharedModel {
        SharedModel { role, cell: Mutex::new(None) }
    }

    pub fn get(&self, src: &Path, allow_unpinned: bool, abort: &dyn Fn() -> bool) -> Result<Arc<StagedModel>> {
        let mut cell = self.cell.lock().unwrap_or_else(|p| p.into_inner()); // a panicked channel must not stop the other
        if cell.is_none() {
            clean_stale_stages();
            match stage_model_for(self.role, src, allow_unpinned, abort) {
                Ok(m) => *cell = Some(Ok(Arc::new(m))),
                Err(Error::Cancelled) => return Err(Error::Cancelled), // not a verdict: nothing is remembered
                Err(e) => *cell = Some(Err(e)),
            }
        }
        match cell.as_ref().expect("set above") {
            Ok(m) => Ok(m.clone()),
            Err(e) => Err(replay(e)),
        }
    }
}

pub fn stage_model(src: &Path, allow_unpinned: bool, abort: &dyn Fn() -> bool) -> Result<StagedModel> {
    stage_model_for(ROLE_STREAM, src, allow_unpinned, abort)
}

pub fn stage_model_for(role: &str, src: &Path, allow_unpinned: bool, abort: &dyn Fn() -> bool) -> Result<StagedModel> {
    let dir = tempfile::Builder::new().prefix(&format!("{STAGE_PREFIX}{}-", std::process::id())).tempdir()?; // 0700
    let known = layout(role, src)?;
    let name = src.file_name().map(|n| n.to_os_string()).unwrap_or_else(|| "model".into());
    let path = dir.path().join(name);
    std::fs::create_dir(&path)?;
    let mut files = BTreeMap::new();
    let mut buf = vec![0u8; 1 << 20];
    for f in known.files.keys() {
        let mut from = std::fs::File::open(src.join(f)).map_err(|_| Error::ModelMissing(format!("{f} is missing in {} directory", names(role).0)))?;
        if !from.metadata()?.is_file() {
            return Err(Error::ModelMissing(format!("{f} is not a regular file")));
        }
        let mut to = util::create_private(&path.join(f))?;
        let mut h = <sha2::Sha256 as sha2::Digest>::new();
        loop {
            if abort() {
                return Err(Error::Cancelled);
            }
            let n = from.read(&mut buf)?;
            if n == 0 {
                break;
            }
            sha2::Digest::update(&mut h, &buf[..n]);
            to.write_all(&buf[..n])?;
        }
        drop(to);
        // the engine only reads it
        let mut perm = std::fs::metadata(path.join(f))?.permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(path.join(f), perm)?;
        files.insert(f.clone(), util::hex(&sha2::Digest::finalize(h)));
    }
    let check = decide(role, &files, known.engine, allow_unpinned)?;
    Ok(StagedModel { _dir: dir, path, check })
}

impl LiveConfig {
    /// The real environment. The model is named by `MEETKO_STREAM_MODEL`, else it is the one the app
    /// downloaded or copied into `<data dir>/models/` (`model_setup`); the webview never names a
    /// path. It also picks the engine (unless `MEETKO_STREAM_ENGINE` names a binary). The language
    /// is `MEETKO_STREAM_LANGUAGE` (default `auto`, the model detects the language per utterance,
    /// which handles Korean with English terms best; `ko` fixes Korean; engines that know no
    /// languages ignore it).
    pub fn from_env() -> Result<LiveConfig> {
        let explicit = std::env::var_os("MEETKO_STREAM_ENGINE").filter(|v| !v.is_empty()).map(PathBuf::from);
        let exe = std::env::current_exe().ok();
        let (model_dir, from_env) = model_dir_for(ROLE_STREAM, std::env::var_os("MEETKO_STREAM_MODEL").map(PathBuf::from), &util::data_dir())
            .ok_or_else(|| Error::ModelMissing("no streaming model: download it in the app's settings or set MEETKO_STREAM_MODEL".into()))?;
        let unpinned = from_env && std::env::var("MEETKO_STREAM_MODEL_UNPINNED").is_ok_and(|v| v == "1");
        // cheapest first: the directory, which model it holds (so which engine), the engine, and
        // only then the hashes (cached by file stamp, so a status check is a stat after the first)
        if !model_dir.is_dir() {
            return Err(Error::ModelMissing("MEETKO_STREAM_MODEL does not point to a model directory".into()));
        }
        let known = layout(ROLE_STREAM, &model_dir)?;
        let engine_bin = pick_engine(explicit, exe.as_deref().and_then(Path::parent), std::env::current_dir().ok().as_deref(), known.engine)?;
        check_model(&model_dir, unpinned, &util::data_dir().join("model-sha256-cache.json"))?;
        let language = std::env::var("MEETKO_STREAM_LANGUAGE").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "auto".into());
        Ok(LiveConfig { engine_bin, model_dir, language, unpinned, queue: WRITE_QUEUE })
    }
}

// ---------------------------------------------------------------- tailing a growing WAV

/// Reads the PCM of a WAV that another process is still appending to. The size fields in the
/// header lag by up to 5 s (docs/capture-interface.md), so the file length is the truth; only the
/// position of the data chunk and the format come from the header. A file that shrinks or is
/// replaced under us is an error: the audio read so far no longer belongs to what is on disk.
pub struct WavTail {
    file: std::fs::File,
    path: PathBuf,
    ident: Option<(u64, u64)>,
    data_start: Option<u64>,
    pos: u64,
}

#[cfg(unix)]
fn identity(m: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn identity(_: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// `Ok(None)`: not enough bytes yet. The offset of the first PCM byte otherwise.
fn parse_wav_header(b: &[u8]) -> Result<Option<u64>> {
    if b.len() < 12 {
        return Ok(None);
    }
    if &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(Error::Engine("not a WAV file".into()));
    }
    let mut off = 12usize;
    loop {
        if off + 8 > b.len() {
            return Ok(None);
        }
        let id = &b[off..off + 4];
        let size = u32::from_le_bytes([b[off + 4], b[off + 5], b[off + 6], b[off + 7]]) as usize;
        if id == b"fmt " {
            if off + 8 + 16 > b.len() {
                return Ok(None);
            }
            let f = &b[off + 8..];
            let (tag, ch, rate, bits) = (u16::from_le_bytes([f[0], f[1]]), u16::from_le_bytes([f[2], f[3]]), u32::from_le_bytes([f[4], f[5], f[6], f[7]]), u16::from_le_bytes([f[14], f[15]]));
            if tag != 1 || ch != 1 || rate != 16000 || bits != 16 {
                return Err(Error::Engine("the channel file is not 16 kHz mono PCM16".into()));
            }
        }
        if id == b"data" {
            return Ok(Some(off as u64 + 8));
        }
        off = off.saturating_add(8).saturating_add(size).saturating_add(size & 1);
        if off > 4096 {
            return Err(Error::Engine("no data chunk in the first 4 KiB of the WAV".into()));
        }
    }
}

impl WavTail {
    pub fn open(path: &Path) -> std::io::Result<WavTail> {
        let file = std::fs::File::open(path)?;
        let ident = identity(&file.metadata()?);
        Ok(WavTail { file, path: path.to_path_buf(), ident, data_start: None, pos: 0 })
    }

    /// New whole samples since the last call (an even number of bytes), at most ~1 MiB. Empty
    /// when the file has not grown or the header is not complete yet.
    pub fn read_new(&mut self) -> Result<Vec<u8>> {
        if let Some(id) = self.ident {
            match std::fs::metadata(&self.path) {
                Ok(now) if identity(&now) == Some(id) => {}
                Ok(_) => return Err(Error::Engine("the channel file was replaced while it was being read".into())),
                Err(_) => return Err(Error::Engine("the channel file disappeared while it was being read".into())),
            }
        }
        let len = self.file.metadata()?.len();
        if self.data_start.is_some() && len < self.pos {
            return Err(Error::Engine("the channel file shrank while it was being read".into()));
        }
        let start = match self.data_start {
            Some(s) => s,
            None => {
                let mut head = vec![0u8; 4096];
                self.file.seek(SeekFrom::Start(0))?;
                let n = self.file.read(&mut head)?;
                head.truncate(n);
                match parse_wav_header(&head)? {
                    Some(s) => {
                        self.data_start = Some(s);
                        self.pos = s;
                        s
                    }
                    None => return Ok(Vec::new()),
                }
            }
        };
        let avail = len.saturating_sub(self.pos.max(start)) & !1;
        let n = avail.min(1 << 20) as usize;
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; n];
        self.file.seek(SeekFrom::Start(self.pos))?;
        self.file.read_exact(&mut buf)?;
        self.pos += n as u64;
        Ok(buf)
    }
}

// ---------------------------------------------------------------- folding events

/// Replace-by-revision: the latest revision of a segment replaces the earlier one entirely,
/// and a final never changes again. Memory stays at one event per segment, and is counted.
#[derive(Default)]
pub struct Folder {
    segments: BTreeMap<i64, Value>,
    bytes: usize,
}

fn weight(ev: &Value) -> usize {
    // text and units dominate; a bound, not an exact size
    ev["text"].as_str().map_or(0, str::len) + ev["units"].as_array().map_or(0, |u| u.iter().map(|x| x["text"].as_str().map_or(0, str::len) + 24).sum::<usize>()) + 64
}

impl Folder {
    /// True when the event changed the folded state (a stale revision or a repeat does not).
    pub fn apply(&mut self, ev: &Value) -> bool {
        let (Some(seg), Some(rev)) = (ev["segment"].as_i64(), ev["revision"].as_i64()) else { return false };
        match self.segments.get(&seg) {
            Some(old) if old["final"] == json!(true) => return false,
            Some(old) if old["revision"].as_i64().is_some_and(|r| r >= rev) => return false,
            _ => {}
        }
        if let Some(old) = self.segments.insert(seg, ev.clone()) {
            self.bytes -= weight(&old);
        }
        self.bytes += weight(ev);
        true
    }

    pub fn events(&self) -> impl Iterator<Item = &Value> {
        self.segments.values()
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The `emitted_at_ms` of the last final: how far the transcript is known to reach.
    pub fn last_final_ms(&self) -> i64 {
        self.segments.values().filter(|e| e["final"] == json!(true)).filter_map(|e| e["emitted_at_ms"].as_i64()).max().unwrap_or(0)
    }
}

/// Resource limits on one engine's output. `Err` means the engine counts as failed.
#[derive(Default)]
pub struct Guard {
    second: Option<Instant>,
    in_second: u32,
    hot: u32,
}

impl Guard {
    /// More than MAX_EVENTS_PER_SEC events in each of MAX_HOT_SECONDS consecutive seconds is a
    /// flood (a short burst is not).
    pub fn rate(&mut self, now: Instant) -> std::result::Result<(), String> {
        match self.second {
            Some(t) if now.duration_since(t) < Duration::from_secs(1) => self.in_second += 1,
            prev => {
                let gap = prev.map_or(0, |t| now.duration_since(t).as_secs());
                self.hot = if self.in_second > MAX_EVENTS_PER_SEC && gap < 2 { self.hot + 1 } else { 0 };
                self.second = Some(now);
                self.in_second = 1;
            }
        }
        // the running second counts too, so a flood is caught as soon as the second hot second is over the line
        if self.hot + u32::from(self.in_second > MAX_EVENTS_PER_SEC) >= MAX_HOT_SECONDS {
            return Err(format!("the engine sent more than {MAX_EVENTS_PER_SEC} events per second"));
        }
        Ok(())
    }

    /// Size limits, judged on the folded state after an event was applied.
    pub fn size(folder: &Folder) -> std::result::Result<(), String> {
        if folder.len() > MAX_SEGMENTS {
            return Err(format!("the engine produced more than {MAX_SEGMENTS} segments"));
        }
        if folder.bytes() > MAX_FOLDED_BYTES {
            return Err("the engine's output outgrew the memory budget".into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- the engine process

/// What `push` did with a chunk.
#[derive(Debug, PartialEq, Eq)]
pub enum Push {
    Queued,
    /// the engine's input queue is full: keep the audio and try again shortly
    Full,
}

/// Makes an engine's sandbox profile from its private HOME and its absolute binary (`EngineClient::spawn_sandboxed`).
pub(crate) type MakeProfile<'a> = &'a dyn Fn(&Path, &Path) -> Result<String>;

/// One engine process. Requests are synchronous with a time budget; `stream.push` is a
/// notification queued to a writer thread, so pushing never blocks the caller.
pub struct EngineClient {
    child: Child,
    to_child: Option<SyncSender<String>>,
    replies: Receiver<Value>,
    pending: Arc<AtomicU64>,
    dead: Arc<AtomicBool>,
    /// a `stream.error` the engine announced before exiting
    fault: Arc<Mutex<Option<String>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_id: u64,
    _home: tempfile::TempDir,
    /// the `initialize` result: `{contract, engine, role, model}`
    pub info: Value,
}

impl EngineClient {
    /// Start the engine on the (already verified) model directory `model` and run `initialize`.
    /// `on_event` gets the params of every `stream.event` notification (of an `asr.batch` engine:
    /// `transcribe.progress`), unvalidated, on the reader thread. `abort` is polled while waiting, so a stop during start-up returns quickly.
    pub fn spawn(cfg: &LiveConfig, model: &Path, on_event: impl FnMut(Value) + Send + 'static, abort: &dyn Fn() -> bool) -> Result<EngineClient> {
        EngineClient::spawn_bin(&cfg.engine_bin, ("MEETKO_STREAM_MODEL", model), ROLE_STREAM, cfg.queue, on_event, abort)
    }

    /// Any engine of the NDJSON protocol: `bin` gets `model_env` (variable, verified model path) and
    /// nothing else of the environment, and must answer `initialize` with `role`. `queue` bounds the
    /// requests waiting for its stdin.
    pub(crate) fn spawn_bin(bin: &Path, model_env: (&str, &Path), role: &str, queue: usize, on_event: impl FnMut(Value) + Send + 'static, abort: &dyn Fn() -> bool) -> Result<EngineClient> {
        EngineClient::spawn_sandboxed(bin, None, model_env, role, queue, on_event, abort)
    }

    /// `spawn_bin`, on macOS optionally under `sandbox-exec -p <profile>` (`batch.rs`); `profile` is made
    /// from the engine's private HOME and its absolute binary. Every engine runs with that HOME as its
    /// working directory: a program that looks a name up in the current directory (an empty PATH entry
    /// does that) finds nothing there that it did not make.
    pub(crate) fn spawn_sandboxed(
        bin: &Path,
        profile: Option<MakeProfile>,
        model_env: (&str, &Path),
        role: &str,
        queue: usize,
        mut on_event: impl FnMut(Value) + Send + 'static,
        abort: &dyn Fn() -> bool,
    ) -> Result<EngineClient> {
        let home = tempfile::Builder::new().prefix("meetko-engine-").tempdir()?;
        // absolute before the working directory changes (a variable may name a relative path)
        let bin = dunce::canonicalize(bin).unwrap_or_else(|_| bin.to_path_buf());
        let model = dunce::canonicalize(model_env.1).unwrap_or_else(|_| model_env.1.to_path_buf());
        let mut cmd = match profile {
            Some(make) => {
                let mut c = Command::new(crate::sandbox::SANDBOX_EXEC);
                c.arg("-p").arg(make(home.path(), &bin)?).arg(&bin);
                c
            }
            None => Command::new(&bin),
        };
        cmd.current_dir(home.path());
        // the engine gets the model directory and nothing else of the environment
        crate::util::sandbox_env(&mut cmd, home.path());
        cmd.env(model_env.0, &model);
        own_process_group(&mut cmd);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::EngineNotFound(format!("cannot start the engine: {e}")))?;
        let mut stdin = child.stdin.take().expect("piped");
        let (to_child, wr) = mpsc::sync_channel::<String>(queue.max(1));
        std::thread::spawn(move || {
            for line in wr {
                if stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        let dead = Arc::new(AtomicBool::new(false));
        let fault = Arc::new(Mutex::new(None));
        let pending = Arc::new(AtomicU64::new(0));
        let (line_tx, line_rx) = mpsc::sync_channel::<Line>(64);
        let stdout = child.stdout.take().expect("piped");
        std::thread::spawn(move || read_lines(stdout, line_tx));
        // at most one reply is ever expected: anything else is dropped, never queued
        let (reply_tx, replies) = mpsc::sync_channel::<Value>(2);
        let (dead2, fault2, pending2) = (dead.clone(), fault.clone(), pending.clone());
        // a batch engine's progress and a diarization session's updates go to `on_event` too
        let extra = match role {
            ROLE_BATCH => "transcribe.progress",
            ROLE_DIARIZATION => "diarize.event",
            _ => "stream.event",
        };
        std::thread::spawn(move || {
            for line in line_rx {
                let Ok(line) = line else { break };
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                if msg["method"] == "stream.event" || msg["method"] == extra {
                    on_event(msg["params"].clone());
                } else if msg["method"] == "stream.error" || msg["method"] == "engine.error" {
                    *fault2.lock().unwrap() = Some(msg["params"]["message"].as_str().unwrap_or("engine error").chars().take(200).collect());
                } else if msg.get("id").and_then(Value::as_u64).is_some_and(|id| id != 0 && id == pending2.load(Ordering::SeqCst)) {
                    let _ = reply_tx.try_send(msg);
                }
            }
            dead2.store(true, Ordering::SeqCst); // stdout closed (or broke the framing): the engine is gone
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let (mut err, sink) = (child.stderr.take().expect("piped"), stderr.clone());
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut b = sink.lock().unwrap();
                b.extend_from_slice(&chunk[..n]);
                let excess = b.len().saturating_sub(8192);
                b.drain(..excess);
            }
        });
        let mut c = EngineClient { child, to_child: Some(to_child), replies, pending, dead, fault, stderr, next_id: 0, _home: home, info: Value::Null };
        c.info = c.call_with("initialize", json!({ "contract": [CONTRACT] }), CALL_TIMEOUT, abort)?;
        if c.info["contract"] != json!(CONTRACT) || c.info["role"] != json!(role) {
            return Err(Error::Engine(format!("the engine is not a {role} engine for contract 0.1")));
        }
        Ok(c)
    }

    pub fn is_dead(&mut self) -> bool {
        self.dead.load(Ordering::SeqCst) || util::has_exited(&self.child)
    }

    /// Why the engine failed, when it said so itself.
    pub fn fault(&self) -> Option<String> {
        let said = || self.fault.lock().unwrap().clone();
        if said().is_none() && util::has_exited(&self.child) {
            // the process may be gone before the reader thread has parsed its last words: let it
            // reach end of file (bounded; a grandchild holding the pipe open must not hold us)
            let end = Instant::now() + Duration::from_millis(500);
            while !self.dead.load(Ordering::SeqCst) && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        said()
    }

    /// The engine's own stderr tail. Kept in logs only: it can name library paths.
    pub fn stderr_tail(&self) -> String {
        let b = self.stderr.lock().unwrap();
        String::from_utf8_lossy(&b[b.len().saturating_sub(1000)..]).into_owned()
    }

    pub fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.call_with(method, params, timeout, &|| false)
    }

    /// A request with a time budget. A full input queue is waited out (within the budget), not an
    /// error; `abort` returning true ends the wait with `Error::Cancelled` and kills the engine.
    pub fn call_with(&mut self, method: &str, params: Value, timeout: Duration, abort: &dyn Fn() -> bool) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.pending.store(id, Ordering::SeqCst);
        let result = self.exchange(id, method, params, timeout, abort);
        self.pending.store(0, Ordering::SeqCst);
        if matches!(result, Err(Error::Cancelled) | Err(Error::Timeout(_))) {
            self.kill();
        }
        result
    }

    fn exchange(&mut self, id: u64, method: &str, params: Value, timeout: Duration, abort: &dyn Fn() -> bool) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        let mut line = format!("{}\n", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let Some(tx) = self.to_child.as_ref() else { return Err(Error::Engine("the engine is not accepting input".into())) };
            match tx.try_send(line) {
                Ok(()) => break,
                Err(TrySendError::Full(l)) => {
                    line = l;
                    if abort() {
                        return Err(Error::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        return Err(Error::Timeout(timeout));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(TrySendError::Disconnected(_)) => return Err(Error::Engine("the engine is not accepting input".into())),
            }
        }
        let reply = loop {
            match self.replies.recv_timeout(Duration::from_millis(50)) {
                Ok(r) => break r,
                Err(RecvTimeoutError::Timeout) => {
                    if abort() {
                        return Err(Error::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        return Err(Error::Timeout(timeout));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.kill();
                    return Err(Error::Engine(self.fault().unwrap_or_else(|| "the engine exited".into())));
                }
            }
        };
        if reply["id"] != json!(id) {
            return Err(Error::Engine("reply id mismatch".into()));
        }
        if let Some(e) = reply.get("error") {
            return Err(Error::Engine(format!("{}: {}", e["code"], e["message"].as_str().unwrap_or(""))));
        }
        Ok(reply["result"].clone())
    }

    /// Queue 16 kHz mono PCM16 for the engine. Never blocks. `Ok(Push::Full)` means the engine
    /// is behind: keep the bytes and retry. `Err` means the engine is gone.
    pub fn push(&self, pcm: &[u8]) -> Result<Push> {
        self.push_as("stream.push", pcm)
    }

    /// `push` under another notification name (a diarization session's `diarize.push`).
    pub fn push_as(&self, method: &str, pcm: &[u8]) -> Result<Push> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(pcm);
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": { "pcm_s16le_b64": b64 } });
        match self.to_child.as_ref().map(|tx| tx.try_send(format!("{msg}\n"))) {
            Some(Ok(())) => Ok(Push::Queued),
            Some(Err(TrySendError::Full(_))) => Ok(Push::Full),
            _ => Err(Error::Engine("the engine is not accepting input".into())),
        }
    }

    pub fn kill(&mut self) {
        kill_tree(&mut self.child);
    }

    /// Polite end: `shutdown`, then wait briefly. Drop kills the group while the pid is still
    /// ours, then reaps.
    pub fn shutdown(mut self, abort: &dyn Fn() -> bool) {
        let _ = self.call_with("shutdown", json!({}), Duration::from_secs(3), abort);
        self.to_child = None;
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end && !util::has_exited(&self.child) && !abort() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for EngineClient {
    fn drop(&mut self) {
        // always take the group: a straggler must not outlive the recording it served
        kill_tree(&mut self.child);
    }
}

// ---------------------------------------------------------------- the session

/// What the app shows. `channel` is the capture channel index (0 = mic, 1 = loopback).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LiveUpdate {
    Caption {
        channel: u32,
        segment: i64,
        revision: i64,
        #[serde(rename = "final")]
        is_final: bool,
        start_ms: i64,
        text: String,
        emitted_at_ms: i64,
    },
    /// Quiet: the recording goes on, this channel just has no (more) captions.
    Error { channel: u32, message: String },
    /// The channel's caption engine has its model loaded and listens: captions follow speech.
    Ready { channel: u32 },
    /// Provisional speakers of caption segments, only those that changed. They may change again
    /// while the live diarization learns; `speaker: None` withdraws a label. Never stored as such:
    /// the stored live version gets its speakers from the session's final turns.
    Speakers { channel: u32, labels: Vec<SegmentSpeaker> },
}

pub type OnUpdate = Arc<dyn Fn(LiveUpdate) + Send + Sync>;
/// Sees every accepted event (channel, the full `asr.stream` event with its units) as it is folded.
/// For measurements; the app does not need it.
pub type Tap = Arc<dyn Fn(u32, &Value) + Send + Sync>;

/// What one channel's engine produced.
#[derive(Debug, Clone)]
pub struct ChannelLive {
    pub index: u32,
    /// the folded events: the latest revision of each segment
    pub events: Vec<Value>,
    /// `initialize` result and the `params` the engine reported at `stream.open`; None when it never ran
    pub engine: Option<Value>,
    pub model: Option<Value>,
    /// combined sha256 of the model files the engine loaded, and whether they matched a pin
    pub model_sha256: Option<String>,
    pub model_pinned: bool,
    pub params: Value,
    /// the engine was closed after the last audio: the whole channel was transcribed
    pub complete: bool,
    /// how far the transcript is known to reach when it is not `complete`
    pub covered_ms: i64,
    pub error: Option<String>,
    /// events that broke the schema or the bounds and were dropped
    pub rejected: u32,
}

#[derive(Debug, Default)]
pub struct LiveOutcome {
    pub channels: BTreeMap<u32, ChannelLive>,
    /// the live speaker separation of each channel, when it ran (`LiveSession::start_with`)
    pub diar: BTreeMap<u32, DiarLive>,
}

impl LiveOutcome {
    pub fn errors(&self) -> Vec<String> {
        self.channels.values().filter_map(|c| c.error.as_ref().map(|e| format!("channel {}: {e}", c.index))).collect()
    }
}

struct Flags {
    /// the capture has ended: drain the files and close
    stop: AtomicBool,
    /// discard everything
    cancel: AtomicBool,
    /// the finish deadline passed: keep what is folded and end now
    kill: AtomicBool,
    /// the captions are in and the speaker separation had its extra moment (`DIAR_GRACE`): end it
    diar_kill: AtomicBool,
}

pub struct LiveSession {
    flags: Arc<Flags>,
    workers: Vec<JoinHandle<ChannelLive>>,
    diar_workers: Vec<JoinHandle<DiarLive>>,
    diar_opened: Vec<Arc<AtomicBool>>,
}

const CHANNEL_FILES: [(u32, &str); 2] = [(0, "mic.wav"), (1, "loopback.wav")];

impl LiveSession {
    /// Start one worker per channel. Returns at once: the engines load their model on their own
    /// threads, and the capture directory may not even exist yet.
    pub fn start(cfg: LiveConfig, capture_dir: &Path, on_update: OnUpdate) -> LiveSession {
        Self::start_with(cfg, None, capture_dir, on_update, None)
    }

    pub fn start_tapped(cfg: LiveConfig, capture_dir: &Path, on_update: OnUpdate, tap: Option<Tap>) -> LiveSession {
        Self::start_with(cfg, None, capture_dir, on_update, tap)
    }

    /// `start`, and with `diar` a live speaker separation per channel next to the captions
    /// (`diar::run_channel`): provisional speakers while recording, the stored live version's
    /// speakers after it. Without `diar` (no model or engine) everything is as without it.
    pub fn start_with(cfg: LiveConfig, diar: Option<DiarConfig>, capture_dir: &Path, on_update: OnUpdate, tap: Option<Tap>) -> LiveSession {
        Self::start_lagging(cfg, diar, capture_dir, on_update, tap, diar::MAX_LAG)
    }

    /// `start_with` and how far a speaker separation may fall behind before it is given up.
    pub fn start_lagging(cfg: LiveConfig, diar: Option<DiarConfig>, capture_dir: &Path, on_update: OnUpdate, tap: Option<Tap>, max_lag: Duration) -> LiveSession {
        let flags = Arc::new(Flags { stop: AtomicBool::new(false), cancel: AtomicBool::new(false), kill: AtomicBool::new(false), diar_kill: AtomicBool::new(false) });
        let model = Arc::new(SharedModel::default());
        let diar_model = Arc::new(SharedModel::new(ROLE_DIARIZATION));
        let spans: Vec<diar::Spans> = CHANNEL_FILES.iter().map(|_| diar::Spans::default()).collect();
        // a caption channel is settled once its engine listens or its worker has ended
        let settled: Vec<Arc<AtomicBool>> = CHANNEL_FILES.iter().map(|_| Arc::default()).collect();
        let workers = CHANNEL_FILES
            .iter()
            .zip(&spans)
            .zip(&settled)
            .map(|((&(index, name), spans), settled)| {
                let (cfg, path, flags, tap, model, spans, settled) = (cfg.clone(), capture_dir.join(name), flags.clone(), tap.clone(), model.clone(), spans.clone(), settled.clone());
                let (inner, s2) = (on_update.clone(), settled.clone());
                let on_update: OnUpdate = Arc::new(move |u| {
                    if matches!(u, LiveUpdate::Ready { .. }) {
                        s2.store(true, Ordering::SeqCst);
                    }
                    inner(u)
                });
                std::thread::spawn(move || {
                    let c = run_channel(&cfg, &model, index, &path, &flags, &on_update, tap, &spans);
                    settled.store(true, Ordering::SeqCst);
                    c
                })
            })
            .collect();
        let diar_opened: Vec<Arc<AtomicBool>> = match diar {
            None => vec![],
            Some(_) => CHANNEL_FILES.iter().map(|_| Arc::default()).collect(),
        };
        let diar_workers = match diar {
            None => vec![],
            Some(dcfg) => CHANNEL_FILES
                .iter()
                .zip(&spans)
                .zip(&diar_opened)
                .map(|((&(index, name), spans), opened)| {
                    let (dcfg, path, flags, on_update, model, spans) = (dcfg.clone(), capture_dir.join(name), flags.clone(), on_update.clone(), diar_model.clone(), spans.clone());
                    let signals = diar::Signals { captions: settled.clone(), opened: opened.clone() };
                    std::thread::spawn(move || {
                        let d = diar::run_channel(&dcfg, &model, index, &path, &flags, &on_update, &spans, max_lag, &signals);
                        signals.opened.store(true, Ordering::SeqCst);
                        d
                    })
                })
                .collect(),
        };
        LiveSession { flags, workers, diar_workers, diar_opened }
    }

    /// Every speaker separation has opened its session or ended (true without one). For tests and
    /// measurements: the app does not need it.
    pub fn diar_settled(&self) -> bool {
        self.diar_opened.iter().all(|o| o.load(Ordering::SeqCst))
    }

    /// `finish_within(FINISH_DEADLINE)`.
    pub fn finish(self) -> LiveOutcome {
        self.finish_within(FINISH_DEADLINE)
    }

    /// Call after the capture provider has stopped: drain what is left in the files, flush the
    /// engines (final events) and collect the result. When `deadline` passes the engines are
    /// killed and what was folded so far is kept; those channels are covered only as far as their
    /// last final, so the rest reads as "not transcribed". A speaker separation that has not
    /// closed by then is dropped (the draft separates the speakers after the recording instead).
    pub fn finish_within(mut self, deadline: Duration) -> LiveOutcome {
        self.flags.stop.store(true, Ordering::SeqCst);
        let end = Instant::now() + deadline;
        let mut captions_done: Option<Instant> = None;
        while self.workers.iter().any(|w| !w.is_finished()) || self.diar_workers.iter().any(|w| !w.is_finished()) {
            if Instant::now() >= end {
                self.flags.kill.store(true, Ordering::SeqCst);
                break;
            }
            // the captions never wait long for the speakers: once they are in, the separation
            // gets DIAR_GRACE more, then it is dropped (the draft separates after the recording)
            if !self.workers.iter().any(|w| !w.is_finished()) {
                let at = *captions_done.get_or_insert_with(Instant::now);
                if at.elapsed() >= diar::DIAR_GRACE {
                    self.flags.diar_kill.store(true, Ordering::SeqCst);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut out = LiveOutcome::default();
        for w in std::mem::take(&mut self.workers) {
            if let Ok(c) = w.join() {
                out.channels.insert(c.index, c);
            }
        }
        for w in std::mem::take(&mut self.diar_workers) {
            if let Ok(d) = w.join() {
                out.diar.insert(d.index, d);
            }
        }
        out
    }

    /// Discard everything (recording abandoned, app closing).
    pub fn cancel(self) {
        drop(self);
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        // workers see the flag, kill their engines and end by themselves
        self.flags.cancel.store(true, Ordering::SeqCst);
    }
}

/// Why a channel's feeding ended early.
enum Failure {
    /// the engine died, flooded, or fell over: it cannot be asked to flush
    Engine(String),
    /// the audio file broke: the engine is fine and can still flush what it was given
    Input(String),
}

/// Where an accepted event's segment lies: from its start to its last unit, or, for a partial
/// without units, to the audio fed so far.
fn segment_span(ev: &Value) -> (i64, i64) {
    let start = ev["start_ms"].as_i64().unwrap_or(0);
    let last = ev["units"].as_array().and_then(|u| u.last()).map(|u| u["end_ms"].as_i64().or_else(|| u["start_ms"].as_i64()).unwrap_or(start));
    let end = last.unwrap_or_else(|| ev["emitted_at_ms"].as_i64().unwrap_or(start));
    (start, end.max(start))
}

#[allow(clippy::too_many_arguments)]
fn run_channel(cfg: &LiveConfig, model: &SharedModel, index: u32, path: &Path, flags: &Flags, on_update: &OnUpdate, tap: Option<Tap>, spans: &diar::Spans) -> ChannelLive {
    let folder = Arc::new(Mutex::new(Folder::default()));
    let rejected = Arc::new(Mutex::new((0u32, None::<String>)));
    let bad: Arc<Mutex<Option<String>>> = Arc::default();
    let mut res = ChannelLive {
        index,
        events: vec![],
        engine: None,
        model: None,
        model_sha256: None,
        model_pinned: false,
        params: json!({}),
        complete: false,
        covered_ms: 0,
        error: None,
        rejected: 0,
    };
    let quiet = |msg: &str| on_update(LiveUpdate::Error { channel: index, message: msg.to_string() });
    let cancelled = || flags.cancel.load(Ordering::SeqCst);
    // while loading, a stop is enough to give up: no engine means nothing to drain
    let abort_loading = || cancelled() || flags.stop.load(Ordering::SeqCst) || flags.kill.load(Ordering::SeqCst);
    let abort_late = || cancelled() || flags.kill.load(Ordering::SeqCst);

    // the capture directory appears when the provider starts; no file yet means no audio yet
    let mut tail = loop {
        if abort_loading() {
            return res;
        }
        match WavTail::open(path) {
            Ok(t) => break t,
            Err(_) => std::thread::sleep(POLL),
        }
    };

    // verify and copy the model before the engine sees it
    let staged = match model.get(&cfg.model_dir, cfg.unpinned, &abort_loading) {
        Ok(m) => m,
        Err(Error::Cancelled) => return res,
        Err(e) => {
            res.error = Some(e.to_string());
            quiet("the streaming model could not be verified");
            return res;
        }
    };
    res.model_sha256 = Some(staged.check.digest.clone());
    res.model_pinned = staged.check.pinned;

    let (f2, r2, b2, cb, sp) = (folder.clone(), rejected.clone(), bad.clone(), on_update.clone(), spans.clone());
    let mut guard = Guard::default();
    let started = EngineClient::spawn(
        cfg,
        &staged.path,
        move |ev| {
            if b2.lock().unwrap().is_some() {
                return; // already failed: ignore the rest of the flood
            }
            if let Err(why) = guard.rate(Instant::now()) {
                *b2.lock().unwrap() = Some(why);
                return;
            }
            let mut errs = schema_errors(engine_result_validator(), &ev);
            if errs.is_empty() && ev["units"].as_array().is_some_and(|u| u.len() > MAX_UNITS) {
                errs.push(format!("more than {MAX_UNITS} units in one event"));
            }
            if !errs.is_empty() {
                let mut r = r2.lock().unwrap();
                r.0 += 1;
                r.1.get_or_insert_with(|| errs[0].clone());
                return;
            }
            let mut f = f2.lock().unwrap();
            if f.apply(&ev) {
                if let Err(why) = Guard::size(&f) {
                    *b2.lock().unwrap() = Some(why);
                    return;
                }
                drop(f);
                if let Some(seg) = ev["segment"].as_i64() {
                    sp.lock().unwrap().insert(seg, segment_span(&ev)); // bounded with the folder
                }
                if let Some(t) = &tap {
                    t(index, &ev);
                }
                cb(LiveUpdate::Caption {
                    channel: index,
                    segment: ev["segment"].as_i64().unwrap_or(0),
                    revision: ev["revision"].as_i64().unwrap_or(0),
                    is_final: ev["final"] == json!(true),
                    start_ms: ev["start_ms"].as_i64().unwrap_or(0),
                    text: ev["text"].as_str().unwrap_or("").to_string(),
                    emitted_at_ms: ev["emitted_at_ms"].as_i64().unwrap_or(0),
                });
            }
        },
        &abort_loading,
    );
    let mut engine = match started {
        Ok(e) => e,
        Err(Error::Cancelled) => return res,
        Err(e) => {
            res.error = Some(e.to_string());
            quiet("the live caption engine could not start");
            return res;
        }
    };
    res.engine = Some(engine.info["engine"].clone());
    res.model = Some(engine.info["model"].clone());
    match engine.call_with("stream.open", json!({ "sample_rate": 16000, "language": cfg.language }), OPEN_TIMEOUT, &abort_loading) {
        Ok(r) => {
            res.params = r.get("params").cloned().unwrap_or_else(|| json!({}));
            on_update(LiveUpdate::Ready { channel: index });
        }
        Err(Error::Cancelled) => return res,
        Err(e) => {
            res.error = Some(e.to_string());
            quiet("the live caption engine could not load its model");
            return res;
        }
    }

    // feed: audio the engine cannot take yet stays in `buf` and in the file, nothing is dropped
    let mut buf: Vec<u8> = Vec::new();
    let mut fed_bytes: u64 = 0;
    let mut failure: Option<Failure> = None;
    loop {
        if cancelled() {
            return res;
        }
        if flags.kill.load(Ordering::SeqCst) {
            failure = Some(Failure::Engine("stopped at the finish deadline".into()));
            break;
        }
        if let Some(why) = bad.lock().unwrap().clone() {
            failure = Some(Failure::Engine(why));
            break;
        }
        if engine.is_dead() {
            failure = Some(Failure::Engine(engine.fault().unwrap_or_else(|| "the live caption engine stopped".into())));
            break;
        }
        let stopping = flags.stop.load(Ordering::SeqCst); // read before the file: stop is set after the capture ended
        let idle;
        if buf.len() < MAX_BUFFERED {
            match tail.read_new() {
                Ok(bytes) => {
                    idle = bytes.is_empty();
                    buf.extend_from_slice(&bytes);
                }
                Err(e) => {
                    failure = Some(Failure::Input(e.to_string()));
                    break;
                }
            }
        } else {
            idle = false;
        }
        let (mut sent, mut full) = (0, false);
        while buf.len() - sent >= CHUNK_BYTES {
            match engine.push(&buf[sent..sent + CHUNK_BYTES]) {
                Ok(Push::Queued) => sent += CHUNK_BYTES,
                Ok(Push::Full) => {
                    full = true;
                    break;
                }
                Err(e) => {
                    failure = Some(Failure::Engine(engine.fault().unwrap_or_else(|| e.to_string())));
                    break;
                }
            }
        }
        buf.drain(..sent);
        fed_bytes += sent as u64;
        if failure.is_some() {
            break;
        }
        if full {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        if idle {
            if stopping {
                break;
            }
            std::thread::sleep(POLL);
        }
    }
    if failure.is_none() && !buf.is_empty() {
        // the last partial chunk; wait out a full queue, within the deadline
        loop {
            if abort_late() {
                failure = Some(Failure::Engine("stopped at the finish deadline".into()));
                break;
            }
            match engine.push(&buf) {
                Ok(Push::Queued) => {
                    fed_bytes += buf.len() as u64;
                    break;
                }
                Ok(Push::Full) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => {
                    failure = Some(Failure::Engine(e.to_string()));
                    break;
                }
            }
        }
    }
    // pushes only enqueue, so the guard can trip after the loop ended: look again before and after the flush
    if failure.is_none() {
        if let Some(why) = bad.lock().unwrap().clone() {
            failure = Some(Failure::Engine(why));
        }
    }
    let input_failed = matches!(failure, Some(Failure::Input(_)));
    if failure.is_none() || input_failed {
        // flush: the remaining final event(s) arrive before the reply
        match engine.call_with("stream.close", json!({}), CLOSE_TIMEOUT, &abort_late) {
            Ok(_) if input_failed => res.covered_ms = fed_bytes as i64 / 32, // what the engine was given and flushed
            Ok(_) => res.complete = true,
            Err(Error::Cancelled) if cancelled() => return res,
            Err(e) => failure = Some(Failure::Engine(engine.fault().unwrap_or_else(|| e.to_string()))),
        }
        // events are read in order, so a flood before the reply has tripped the guard by now
        if let (Some(why), false) = (bad.lock().unwrap().clone(), matches!(failure, Some(Failure::Engine(_)))) {
            failure = Some(Failure::Engine(why));
            res.complete = false;
            res.covered_ms = 0; // an engine failure covers only up to the last final (set below)
        }
    }
    if let Some(f) = &failure {
        let (m, note) = match f {
            Failure::Engine(m) => (m, "the live caption engine stopped; this channel has no captions after this point"),
            Failure::Input(m) => (m, "the recording's audio file changed while it was read; this channel has no captions after this point"),
        };
        res.error = Some(m.clone());
        quiet(note);
        std::thread::sleep(Duration::from_millis(50)); // events already on the pipe
    }
    engine.shutdown(&abort_late);
    let f = folder.lock().unwrap();
    res.events = f.events().cloned().collect();
    // an engine failure covers only up to the last final; an input failure up to what was fed
    res.covered_ms = res.covered_ms.max(f.last_final_ms());
    let r = rejected.lock().unwrap();
    res.rejected = r.0;
    if let (Some(why), None) = (&r.1, &res.error) {
        res.error = Some(format!("{} events were rejected: {why}", r.0));
    }
    res
}

// ---------------------------------------------------------------- the stored version

/// The sha256 of a whole stored channel file, and the length and sha256 of its PCM: what a
/// `WavTail` reads from it (everything after the header, in whole samples).
fn file_and_pcm_sha256(path: &Path) -> Result<(String, u64, String)> {
    use sha2::Digest;
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut head = vec![0u8; len.min(4096) as usize];
    f.read_exact(&mut head)?;
    let off = parse_wav_header(&head)?.ok_or_else(|| Error::Corrupt("the channel file has no complete header".into()))?;
    let pcm_end = off + (len.saturating_sub(off) & !1);
    f.seek(SeekFrom::Start(0))?;
    let (mut whole, mut pcm) = (sha2::Sha256::default(), sha2::Sha256::default());
    let (mut pos, mut buf) = (0u64, vec![0u8; 1 << 20]);
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        whole.update(&buf[..n]);
        let (a, b) = (pos.clamp(off, pcm_end), (pos + n as u64).clamp(off, pcm_end));
        pcm.update(&buf[(a - pos) as usize..(b - pos) as usize]);
        pos += n as u64;
    }
    Ok((util::hex(&whole.finalize()), pcm_end - off, util::hex(&pcm.finalize())))
}

/// The final result of a channel's live speaker separation as a `diarization` engine result of
/// the stored channel file, or `None` when it cannot be one: the session failed or was cut off, or
/// it was not fed exactly the stored file's audio. The session read the provider's file as it
/// grew, so the turns are attached under the sha256 import recorded for that file only when the
/// file still has that sha256 and the PCM fed to the engine is byte for byte the file's PCM (same
/// length, same sha256), and the engine says it was fed that much. The model is the copy the core
/// staged, as for the batch pass.
fn live_diarization(d: &DiarLive, f: &crate::store::ChannelFile) -> Option<Value> {
    let mut r = d.result.clone().filter(|r| d.complete && r.is_object())?;
    let (file_sha, pcm_len, pcm_sha) = file_and_pcm_sha256(&f.path).ok()?;
    if file_sha != f.sha256 || pcm_len != d.fed_bytes || d.fed_sha256.as_deref() != Some(pcm_sha.as_str()) {
        return None;
    }
    if r["params"]["fed_ms"].as_i64() != Some((d.fed_bytes / 32) as i64) {
        return None; // the engine counted other audio than the core sent
    }
    r["audio_sha256"] = json!(f.sha256);
    if !r["model"].is_object() {
        r["model"] = json!({ "id": "unknown" });
    }
    if let Some(sha) = &d.model_sha256 {
        r["model"]["sha256"] = json!(sha);
    }
    if !r["params"].is_object() {
        r["params"] = json!({});
    }
    r["params"]["model_pinned"] = json!(d.model_pinned);
    r["params"]["live"] = json!(true); // provenance: made during the recording, not by a pass over the file
    Some(r)
}

/// Build and store the `live` version of an imported meeting from what the engines produced.
/// `Ok(None)` when no engine ran at all (nothing to record). A channel whose engine died is
/// covered only as far as its last final, so the gap reads as "not transcribed", never as silence.
///
/// A channel whose live speaker separation completed gets its speakers here, through the same
/// `with_diarization` + compose as a batch draft (producer role `diarization`, `params.mode`
/// streaming, `params.live` true), so its first draft needs no second pass. Speakers never cost
/// the captions: a result that does not fit, or a version that does not store with it, is stored
/// without them and the draft separates the speakers after the recording as before.
pub fn store_live(store: &Store, meeting_id: &str, outcome: &LiveOutcome, chunk_ms: usize) -> Result<Option<MeetingData>> {
    let (meeting, recording, files) = store.meeting_and_recording(meeting_id)?;
    let mut channels = BTreeMap::new();
    let mut plain = BTreeMap::new();
    for f in &files {
        let Some(c) = outcome.channels.get(&f.index).filter(|c| c.engine.is_some()) else { continue };
        let evs: Vec<StreamEvent> = c.events.iter().filter_map(|e| serde_json::from_value(e.clone()).ok()).collect();
        let reach = if c.complete { recording.duration_ms } else { c.covered_ms.min(recording.duration_ms) };
        let mut words: Vec<EngineWord> = live_words(&evs);
        words.retain(|w| w.start_ms < reach);
        for w in &mut words {
            w.end_ms = w.end_ms.min(reach); // the last word ends where the engine stopped listening
        }
        let mut params = c.params.as_object().cloned().unwrap_or_default();
        params.insert("chunk_ms".into(), json!(chunk_ms));
        params.insert("model_pinned".into(), json!(c.model_pinned)); // false: loaded under MEETKO_STREAM_MODEL_UNPINNED
        let mut result = json!({
            "engine": c.engine, "word_timing": "transducer", "granularity": "token", "network": false,
            "params": params, "words": words,
            "coverage": if reach > 0 { json!([{ "start_ms": 0, "end_ms": reach }]) } else { json!([]) },
        });
        if let Some(m) = &c.model {
            result["model"] = m.clone();
            if let Some(sha) = &c.model_sha256 {
                result["model"]["sha256"] = json!(sha); // canonical digest over the model files, see `model_digest`
            }
        }
        let mut ci: ChannelInput = batch_channel(&result, f.index, &f.role, f.label.as_deref(), f.coverage.as_deref())?;
        plain.insert(f.index, ci.clone());
        if let Some((d, r)) = outcome.diar.get(&f.index).and_then(|d| live_diarization(d, f).map(|r| (d, r))) {
            // on failure the channel is left as it was: its speakers come from the draft's pass
            let _ = with_diarization(&mut ci, &r, &f.sha256, recording.duration_ms, f.index, d.min_speaker_ms);
        }
        channels.insert(f.index, ci);
    }
    if channels.is_empty() {
        return Ok(None);
    }
    let version = || ("pending".to_string(), VersionKind::Live);
    let md = compose(meeting.clone(), recording.clone(), version(), &channels);
    match store.create_version(md) {
        Err(e) if channels.values().any(|c| c.diarization.is_some()) && !matches!(e, Error::Cancelled) => Ok(Some(store.create_version(compose(meeting, recording, version(), &plain))?)),
        r => Ok(Some(r?)),
    }
}

#[cfg(all(test, unix))]
mod tests;
