//! The command layer without Tauri: plain functions over the core, so they can be tested
//! headless. `lib.rs` wraps each in a typed `#[tauri::command]`. Nothing here takes a
//! filesystem path from the webview except `add_plugin`, which is fed the folder the user
//! picked in the native dialog (`lib.rs` never accepts a path from JS at all).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use meetko_core::capture_run::{CaptureOutcome, ProviderError};
use meetko_core::contract::{resolve, VersionKind};
use meetko_core::diarize::DiarEngine;
use meetko_core::live::{self, LiveConfig, LiveOutcome};
use meetko_core::manifest::{plugin_code_sha256, Manifest};
use meetko_core::plugin_source::{self, Endpoints, GalleryEntry, PluginSource};
use meetko_core::overlay::{Overlay, View};
use meetko_core::store::overlay::SetAside;
use meetko_core::store::search::SearchOutcome;
use meetko_core::store::{PluginRow, ResultRow, Store};
use meetko_core::vocabulary::{self, Replacement, Vocabulary};
use meetko_core::whisper::{BatchEngine, Progress, WhisperCli};
use meetko_core::secrets::{self, SecretStore};
use meetko_core::{batch, capture_run, export, host, model_setup, pipeline, util};
use serde::Serialize;
use serde_json::Value;

/// A refused vocabulary in the words of the settings screen (해요체; the code stays `invalid_data`).
pub fn problem_text(p: &vocabulary::Problem) -> String {
    use vocabulary::{Problem as P, MAX_FILE_BYTES, MAX_FIND_CHARS, MAX_HINTS, MAX_HINT_CHARS, MAX_REPLACEMENTS, MAX_REPLACE_CHARS, VERSION};
    match p {
        P::TooLarge => format!("용어 사전이 {}KB를 넘어요.", MAX_FILE_BYTES >> 10),
        P::NotVocabulary(_) => "용어 사전 파일이 아니에요. meetko에서 내보낸 JSON 파일인지 확인해 주세요.".into(),
        P::Version(n) => format!("버전 {n} 용어 사전은 읽을 수 없어요. 이 앱은 버전 {VERSION}을 읽어요."),
        P::TooManyHints => format!("용어는 {MAX_HINTS}개까지 적을 수 있어요."),
        P::TooManyReplacements => format!("바꾸기는 {MAX_REPLACEMENTS}개까지 둘 수 있어요."),
        P::HintTooLong(i) => format!("{i}번째 용어가 {MAX_HINT_CHARS}자를 넘어요."),
        P::HintHidden(i) => format!("{i}번째 용어에 쓸 수 없는 문자(탭 같은 제어 문자)가 있어요."),
        P::EmptyFind(i) => format!("{i}번째 바꾸기에 찾을 말이 없어요."),
        P::FindTooLong(i) => format!("{i}번째 바꾸기의 찾을 말이 {MAX_FIND_CHARS}자를 넘어요."),
        P::ReplaceTooLong(i) => format!("{i}번째 바꾸기의 바꿀 말이 {MAX_REPLACE_CHARS}자를 넘어요."),
        P::ReplacementHidden(i) => format!("{i}번째 바꾸기에 쓸 수 없는 문자(줄바꿈, 탭 같은 제어 문자)가 있어요."),
    }
}

/// The longest sample sentence the vocabulary preview takes.
pub const PREVIEW_MAX_CHARS: usize = 2000;

// the Korean channel names are an app-layer concern (the core only knows mic/loopback)
/// The key of `app-settings.json` that remembers the setup card was put away.
pub const SETUP_CARD_KEY: &str = "setup_card";
/// The key of `app-settings.json` that holds the engine 다시 받아쓰기 uses: [`BATCH_AUTO`] or one of
/// `meetko_core::batch::ENGINES`.
pub const BATCH_ENGINE_KEY: &str = "batch_engine";
/// The engine choice "the most accurate one that is ready" (Qwen3-ASR, else Whisper); the default.
pub const BATCH_AUTO: &str = "auto";
/// What the user can choose for 다시 받아쓰기, in the order the settings show them.
pub const BATCH_CHOICES: [&str; 3] = [BATCH_AUTO, batch::WHISPER, batch::QWEN3_ASR];
/// The key of `app-settings.json` that says whether a recording is transcribed again by a batch engine
/// once its live draft exists (`true` unless the user turned it off).
pub const AUTO_RETRANSCRIBE_KEY: &str = "auto_retranscribe";

/// Which batch engines could run right now.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Ready {
    /// whisper-cli and its model were found
    pub whisper: bool,
    /// the Qwen3-ASR engine and both of its pinned models are there (and its sandbox applies)
    pub qwen: bool,
}

/// The engine a batch job runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pick {
    pub engine: &'static str,
    /// the user chose the other engine, which is not ready: this one runs instead (the UI says so)
    pub fallback: bool,
}

/// The engine for 다시 받아쓰기: with `auto` the most accurate one that is ready (Qwen3-ASR measured
/// best on Korean–English meetings, then Whisper); an explicit choice when it is ready, else the other
/// ready one as a fallback. `None` when nothing is ready.
pub fn pick_engine(choice: &str, ready: Ready) -> Option<Pick> {
    let ok = |e: &str| if e == batch::QWEN3_ASR { ready.qwen } else { ready.whisper };
    let order: [&'static str; 2] = if choice == batch::WHISPER { [batch::WHISPER, batch::QWEN3_ASR] } else { [batch::QWEN3_ASR, batch::WHISPER] };
    let explicit = choice != BATCH_AUTO;
    order.into_iter().enumerate().find(|(_, e)| ok(e)).map(|(i, engine)| Pick { engine, fallback: explicit && i > 0 })
}

/// Payload of the `transcribe` and `plugin-run` events.
#[derive(Serialize, Clone, Default, Debug)]
pub struct JobEvent {
    /// progress | done | error | cancelled
    pub state: &'static str,
    pub meeting_id: Option<String>,
    pub version_id: Option<String>,
    pub plugin_id: Option<String>,
    pub result_id: Option<i64>,
    pub channel: u32,
    pub channels: u32,
    pub percent: u8,
    /// the channel is transcribed and its speakers are being separated
    pub diarizing: bool,
    /// the job makes the first draft from the live captions (no batch transcription)
    pub from_live: bool,
    /// the batch transcription that started on its own after the live draft of a new recording
    pub auto: bool,
    /// the batch engine that runs (`whisper` | `qwen3-asr`); None for a draft from the live captions
    pub engine: Option<&'static str>,
    /// the engine the user chose is not ready; `engine` runs instead
    pub fallback: bool,
    pub code: Option<&'static str>,
    pub message: Option<String>,
}

/// What a `transcribe` job makes. All share the one job slot, its cancel flag and its events.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DraftJob {
    /// a batch transcription (다시 받아쓰기) with the engine `pick_engine` names
    Batch,
    /// the first draft from the meeting's live captions
    FromLive,
    /// `FromLive`, then (when the setting is on and an engine is ready) a batch transcription that
    /// reuses the live speakers: what a stopped recording starts
    FromLiveThenBatch,
}

/// Where a job gets its batch engines: the real ones in the app, fakes in tests.
pub trait Engines {
    fn ready(&self) -> Ready;
    fn open(&self, engine: &'static str) -> ApiResult<Box<dyn BatchEngine>>;
}

/// The batch engines of the real environment.
pub struct SystemEngines {
    pub data_dir: PathBuf,
}

impl Engines for SystemEngines {
    fn ready(&self) -> Ready {
        Ready { whisper: whisper_setup(&self.data_dir).2.ok, qwen: batch::BatchConfig::from_env().is_ok() }
    }
    fn open(&self, engine: &'static str) -> ApiResult<Box<dyn BatchEngine>> {
        Ok(if engine == batch::QWEN3_ASR {
            Box::new(batch::BatchProcess::new(batch::BatchConfig::from_env()?))
        } else {
            Box::new(WhisperCli::load(&self.data_dir)?.with_hints(vocabulary::hints_for_engine(&self.data_dir)))
        })
    }
}

/// The speaker separation a job may use: the engine (optional), and what was said about it when it
/// was set up (a model that is set but unusable is only a note, `off`).
#[derive(Default)]
pub struct DiarSetup<'a> {
    pub engine: Option<&'a dyn DiarEngine>,
    pub notes: Vec<String>,
    pub off: bool,
}

/// whisper's model, its engine (whisper-cli), and the two together.
fn whisper_setup(data_dir: &Path) -> (Setup, Setup, Setup) {
    let model = match WhisperCli::load(data_dir) {
        Err(e) => Setup::bad(e.code(), e.to_string()),
        Ok(w) if w.model.is_file() => Setup::ok().at(&w.model),
        Ok(w) => Setup::bad("model_missing", "the configured model file does not exist").at(&w.model),
    };
    // the engine binary is independent of the model: env, a sibling of the app, or PATH
    let cli = WhisperCli::resolve_cli();
    let found = if cli.components().count() > 1 { cli.is_file().then(|| cli.clone()) } else { host::find_on_path(&cli) };
    let whisper_cli = match found {
        Some(p) => Setup::ok().at(&p),
        None => Setup::bad("engine_not_found", format!("{} was not found", cli.display())),
    };
    let both = if model.ok && whisper_cli.ok {
        Setup::ok()
    } else {
        let (code, detail) = if model.ok { (whisper_cli.code, &whisper_cli.detail) } else { (model.code, &model.detail) };
        Setup { ok: false, path: None, code, detail: detail.clone() }
    };
    (model, whisper_cli, both)
}

pub const MIC_LABEL: &str = "나";
pub const LOOPBACK_LABEL: &str = "상대";

/// Shown with every permission list. Plain on purpose (docs/plugin-security.md).
pub const NOTICE_DECLARED: &str = "declared, not enforced";
pub const NOTICE_SANDBOXED: &str = "sandboxed: no network unless declared";

/// Whether this build (and environment: `MEETKO_PLUGIN_SANDBOX=off` counts) isolates plugins.
/// Asks the core for the label a run would get, so the two cannot disagree.
pub fn plugins_isolated() -> bool {
    meetko_core::sandbox::Isolation::production(Path::new("")).label() == meetko_core::sandbox::MACOS_SBPL
}

fn permission_notice() -> &'static str {
    if plugins_isolated() { NOTICE_SANDBOXED } else { NOTICE_DECLARED }
}

fn source_text(s: &PluginSource) -> String {
    format!("{} @ {} ({})", s.repo, s.reference, &s.sha[..12])
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

impl From<meetko_core::Error> for ApiError {
    fn from(e: meetko_core::Error) -> Self {
        ApiError { code: e.code(), message: e.to_string() }
    }
}

impl ApiError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        ApiError { code, message: message.into() }
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

// ---------------------------------------------------------------- views

#[derive(Serialize)]
pub struct Setup {
    pub ok: bool,
    /// the resolved file (executable or model), shown so the user can see what will run
    pub path: Option<String>,
    /// machine code the UI turns into Korean setup text; `None` when ok
    pub code: Option<&'static str>,
    pub detail: Option<String>,
}

impl Setup {
    fn ok() -> Self {
        Setup { ok: true, path: None, code: None, detail: None }
    }
    fn at(mut self, p: &Path) -> Self {
        self.path = Some(p.display().to_string());
        self
    }
    fn bad(code: &'static str, detail: impl Into<String>) -> Self {
        Setup { ok: false, path: None, code: Some(code), detail: Some(detail.into()) }
    }
}

#[derive(Serialize)]
pub struct Status {
    /// a debug build (engines come from the repository's build output, so the setup texts differ)
    pub dev_build: bool,
    pub model: Setup,
    pub whisper_cli: Setup,
    pub capture: Setup,
    /// live captions (engine + streaming model); recording works without them
    pub live: Setup,
    /// the user's choice for 다시 받아쓰기: `auto` | `whisper` | `qwen3-asr`
    pub batch_choice: &'static str,
    /// the engine 다시 받아쓰기 runs now (`pick_engine`); when none is ready, the one to set up (the
    /// chosen one, whisper for `auto`)
    pub batch_engine: &'static str,
    /// the chosen engine is not ready and the other one runs instead
    pub batch_fallback: bool,
    /// whether some engine is ready; the code and detail of the one to set up when not
    pub batch: Setup,
    /// a stopped recording is transcribed again on its own once its live draft exists
    pub auto_retranscribe: bool,
}

/// One model the app can set up for the user (`meetko_core::model_setup::OFFERS`). No path: the
/// webview learns the state and what a download would be, not where files live.
#[derive(Serialize, Debug, Clone)]
pub struct ModelPiece {
    /// stream | diarization | qwen3-asr | qwen3-aligner
    pub key: &'static str,
    /// ready | missing | bad (a file is there but it is not the pinned model, or the variable is wrong)
    pub state: &'static str,
    /// the environment names this model (MEETKO_STREAM_MODEL / MEETKO_DIAR_MODEL): the app leaves it alone
    pub from_env: bool,
    /// the engine that loads it was found (env, next to the app, or the dev build)
    pub engine: bool,
    /// the engine is named by its variable (MEETKO_STREAM_ENGINE / MEETKO_DIAR_ENGINE): a developer setup
    pub engine_from_env: bool,
    /// bytes of the download, and of an interrupted one that the next click resumes
    pub size: u64,
    pub partial: u64,
    pub license: &'static str,
    pub publisher: &'static str,
    /// "nvidia/…" (or "cstr/…") on Hugging Face, and the host the download starts at
    pub repo: &'static str,
    pub host: &'static str,
    pub detail: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct ModelJobView {
    pub key: String,
    /// download | import
    pub kind: &'static str,
}

#[derive(Serialize, Debug, Clone)]
pub struct ModelSetup {
    /// a debug build: the engines are found in the repository's build output, so the missing-engine
    /// text names the build steps instead of the app bundle
    pub dev_build: bool,
    pub pieces: Vec<ModelPiece>,
    /// the download or copy running now, if any
    pub job: Option<ModelJobView>,
    /// the user put the meeting list's setup card away (나중에); settings still offer everything
    pub card_dismissed: bool,
    /// the user's choice for 다시 받아쓰기 (`auto` | `whisper` | `qwen3-asr`)
    pub batch_engine: &'static str,
    /// a stopped recording is transcribed again on its own once its live draft exists
    pub auto_retranscribe: bool,
}

/// Whether the engine is missing, or `MEETKO_STREAM_ENGINE` names something that is not there.
fn engine_code() -> &'static str {
    if std::env::var_os("MEETKO_STREAM_ENGINE").is_some_and(|v| !v.is_empty()) { "stream_engine_invalid" } else { "stream_engine_missing" }
}

/// The setup state of one offer, pure over its inputs: `env` is the model variable, `unpinned` its
/// development switch, `engine` whether the engine binary was found.
pub fn model_piece(o: &model_setup::Offer, env: Option<PathBuf>, unpinned: bool, data_dir: &Path, engine: bool) -> ModelPiece {
    let env = env.filter(|p| !p.as_os_str().is_empty());
    let from_env = env.is_some();
    let (state, detail) = match live::model_dir_for(o.role, env, data_dir) {
        None => ("missing", None),
        // the app's own folder: this offer's file on its own, so the second file of a two-file model is
        // "missing" (not "bad") while the first is being fetched, and "bad" only when it is there and wrong
        Some((_, false)) => match o.file_state(data_dir) {
            model_setup::FileState::Missing => ("missing", None),
            model_setup::FileState::Ready => ("ready", None),
            model_setup::FileState::Bad => ("bad", Some(format!("{} is not the pinned file", o.file))),
        },
        // the development switch only covers a directory the environment named
        Some((dir, named)) => match live::check_model_for(o.role, &dir, unpinned && named, &data_dir.join("model-sha256-cache.json")) {
            Ok(_) => ("ready", None),
            Err(e) => ("bad", Some(e.to_string())),
        },
    };
    ModelPiece {
        key: o.key,
        state,
        from_env,
        engine,
        engine_from_env: false,
        size: o.size,
        partial: if from_env { 0 } else { o.partial(data_dir) },
        license: o.license,
        publisher: o.publisher,
        repo: o.repo,
        host: "huggingface.co",
        detail,
    }
}

#[derive(Serialize)]
pub struct MeetingSummary {
    pub id: String,
    pub title: Option<String>,
    pub started_at: Option<String>,
    pub created_at: String,
    pub duration_ms: i64,
    pub latest_version_id: Option<String>,
    pub latest_kind: Option<&'static str>,
    pub stale_count: usize,
}

#[derive(Serialize)]
pub struct ChannelView {
    pub index: u32,
    pub role: String,
    pub label: Option<String>,
    pub sha256: String,
}

#[derive(Serialize)]
pub struct VersionView {
    pub id: String,
    pub kind: &'static str,
    pub parent: Option<String>,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct MeetingDetail {
    pub id: String,
    pub title: Option<String>,
    pub started_at: Option<String>,
    pub duration_ms: i64,
    pub channels: Vec<ChannelView>,
    pub versions: Vec<VersionView>,
}

#[derive(Serialize, Clone)]
pub struct PluginPreview {
    pub id: String,
    pub version: String,
    pub description: String,
    pub network: bool,
    pub audio: String,
    /// declared secret names (API keys the user sets); never their values
    pub secrets: Vec<String>,
    /// declared setting names (model, base URL)
    pub settings: Vec<String>,
    /// adding this re-adds an installed plugin from the same origin: its saved keys and settings stay
    pub keeps_values: bool,
    pub display: Vec<String>,
    pub code_sha256: String,
    pub manifest_sha256: String,
    /// the folder's own name only, never its full path
    pub folder: String,
    pub notice: &'static str,
    /// true when plugins run in the OS sandbox in this build
    pub isolated: bool,
    /// `github:OWNER/REPO @ <ref> (<commit>)` for a plugin that came from the network
    pub source: Option<String>,
    /// set when adding this would replace an installed plugin with the same id that came from a
    /// different source: the UI must ask again, showing both, before it passes `replace`
    pub replaces: Option<Replaces>,
    /// identifies this gallery preview; installing needs the same token back (set by the app layer)
    pub token: Option<String>,
}

/// One declared secret or setting of an installed plugin. A secret's value is never sent to the
/// webview (only whether it is set); a setting's value is plain configuration and is shown.
pub type SecretState = secrets::Entry;

/// The installed plugin an add would overwrite.
#[derive(Serialize, Clone)]
pub struct Replaces {
    /// identifies exactly the installed state shown; the install must send it back as its confirmation
    pub key: String,
    pub version: String,
    /// where it came from; `None` = a local folder
    pub source: Option<String>,
}

#[derive(Serialize)]
pub struct ResultView {
    pub id: i64,
    pub plugin_id: String,
    pub plugin_version: String,
    pub version_id: String,
    pub created_at: String,
    pub stale: bool,
    pub output: Value,
    pub plugin_code_sha256: String,
    pub plugin_manifest_sha256: String,
    pub declared_network: bool,
    pub declared_audio: String,
    /// the isolation the run had: macos-sbpl | none | off | unrecorded | mcp (an external AI agent)
    pub sandbox: String,
    /// for a result from an AI agent over MCP: `{via, network, protocol, client: {name, version},
    /// agent: {name, version}}` (self-reported, cleaned); `None` for plugin results
    pub agent: Option<Value>,
}

impl From<ResultRow> for ResultView {
    fn from(r: ResultRow) -> Self {
        ResultView {
            id: r.id,
            plugin_id: r.plugin_id,
            plugin_version: r.plugin_version,
            version_id: r.version_id,
            created_at: r.created_at,
            stale: r.stale,
            output: r.output,
            plugin_code_sha256: r.plugin_code_sha256,
            plugin_manifest_sha256: r.plugin_manifest_sha256,
            declared_network: r.declared_network,
            declared_audio: r.declared_audio,
            sandbox: r.sandbox,
            agent: r.agent,
        }
    }
}

#[derive(Serialize)]
pub struct Anchor {
    pub channels: Vec<u32>,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Serialize)]
pub struct CaptureDone {
    pub meeting_id: Option<String>,
    pub existing: bool,
    pub warnings: Vec<String>,
    /// everything the provider reported, with its fix text
    pub provider_errors: Vec<ProviderError>,
    pub error: Option<String>,
    /// the `live` version made from the captions, when there was one
    pub live_version_id: Option<String>,
    /// quiet: set when live captions were expected but could not be kept (machine code, no paths)
    pub live_note: Option<&'static str>,
}

/// The one gallery download the user is looking at. Each preview gets a fresh token; an install
/// must present the token of the preview still pending, so a click that belongs to an older
/// (replaced) preview cannot install the newer download.
#[derive(Default)]
pub struct PendingGallery {
    seq: u64,
    slot: Option<(u64, plugin_source::Fetched)>,
}

impl PendingGallery {
    pub fn clear(&mut self) {
        self.slot = None;
    }

    /// Keep `fetched` as the pending preview; returns its token.
    pub fn set(&mut self, fetched: plugin_source::Fetched) -> String {
        self.seq += 1;
        self.slot = Some((self.seq, fetched));
        self.seq.to_string()
    }

    /// The pending tree if `token` is its token (and only then is it taken).
    pub fn take(&mut self, token: &str) -> ApiResult<plugin_source::Fetched> {
        match self.slot.take() {
            Some((t, f)) if t.to_string() == token => Ok(f),
            other => {
                self.slot = other; // a stale click leaves the current preview armed
                Err(ApiError::new("not_found", "this preview is no longer current; open the plugin again"))
            }
        }
    }
}

// ---------------------------------------------------------------- export

/// The label of a turn without a speaker, as the transcript shows it.
pub const UNASSIGNED_LABEL: &str = "화자 미지정";
pub const EXPORT_NOTE_DRAFT: &str = "아직 승인하지 않은 초안이에요.";
pub const EXPORT_NOTE_LIVE: &str = "녹음하는 동안 받아 적은 실시간 자막이에요. 정확하지 않을 수 있어요.";

/// md, txt (stamps and speakers), srt, vtt are files; plain and speakers are copy variants.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExportFormat {
    Markdown,
    Text,
    Srt,
    Vtt,
    Plain,
    Speakers,
}

impl ExportFormat {
    pub fn parse(s: &str) -> ApiResult<Self> {
        Ok(match s {
            "md" => Self::Markdown,
            "txt" => Self::Text,
            "srt" => Self::Srt,
            "vtt" => Self::Vtt,
            "plain" => Self::Plain,
            "speakers" => Self::Speakers,
            _ => return Err(ApiError::new("invalid_data", format!("unknown export format {s:?}"))),
        })
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Srt => "srt",
            Self::Vtt => "vtt",
            Self::Text | Self::Plain | Self::Speakers => "txt",
        }
    }

    /// the save dialog's filter name
    pub fn filter_name(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Srt => "SubRip 자막",
            Self::Vtt => "WebVTT 자막",
            Self::Text | Self::Plain | Self::Speakers => "텍스트",
        }
    }
}

/// What the meeting screen needs to show a version as the user changed it.
#[derive(Debug, Serialize)]
pub struct OverlayView {
    pub version_id: String,
    pub overlay: Overlay,
    /// names and edits of earlier transcripts that this version does not show
    pub set_aside: SetAside,
}

/// The changes the screen can make; each returns the new [`OverlayView`].
pub enum OverlayChange {
    Rename { speaker_id: String, name: String },
    Merge { speaker_id: String, into: String },
    ResetSpeaker { speaker_id: String },
    Edit { turn_id: String, text: String },
    ResetTurn { turn_id: String },
    DiscardSetAside,
}

pub struct Export {
    pub text: String,
    /// suggested in the save dialog; the title with the characters file systems refuse replaced
    pub file_name: String,
    pub format: ExportFormat,
}

/// The longest stem in UTF-8 bytes: file systems count bytes (255), and the suffix needs room.
pub const FILE_STEM_MAX_BYTES: usize = 200;

/// A file name from a meeting title: separators, characters Windows refuses and hidden ones
/// (controls, bidi overrides, zero-width: `text::is_hidden_char`) become `-`; no spaces or dots
/// at either end; at most [`FILE_STEM_MAX_BYTES`]; a Windows device name (`CON`, `com1.x`) gets `_`.
pub fn file_stem(title: &str) -> String {
    let mut s = String::new();
    for c in title.chars() {
        let c = if meetko_core::text::is_hidden_char(c) || r#"/\:*?"<>|"#.contains(c) { '-' } else { c };
        if s.len() + c.len_utf8() > FILE_STEM_MAX_BYTES {
            break;
        }
        s.push(c);
    }
    let s = s.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if s.is_empty() {
        return "meetko".into();
    }
    let base = s.split('.').next().unwrap_or(s).trim_end().to_ascii_uppercase();
    // COM/LPT take a digit 0-9 or a superscript one (Windows reserves `COM¹` too)
    let port = |p: &str| base.strip_prefix(p).is_some_and(|d| matches!(d.chars().collect::<Vec<_>>()[..], [c] if c.is_ascii_digit() || "¹²³".contains(c)));
    let device = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") || port("COM") || port("LPT");
    if device { format!("_{s}") } else { s.to_string() }
}

// ---------------------------------------------------------------- the core handle

pub struct Core {
    pub data_dir: PathBuf,
    /// where plugin secrets live (the OS keychain; an in-memory store in tests)
    pub secrets: Arc<dyn SecretStore>,
}

impl Core {
    pub fn from_env() -> Self {
        Core::new(util::data_dir())
    }

    pub fn new(data_dir: PathBuf) -> Self {
        Core { data_dir, secrets: secrets::system() }
    }

    /// One connection per call: `Store` is not `Sync`, and SQLite (WAL + busy_timeout) is
    /// shared with the CLI anyway.
    pub fn store(&self) -> ApiResult<Store> {
        Ok(Store::open(&self.data_dir)?)
    }

    pub fn status(&self) -> Status {
        let (model, whisper_cli, whisper) = whisper_setup(&self.data_dir);
        let capture = match capture_run::resolve_bin(&self.data_dir) {
            Ok(p) => Setup::ok().at(&p),
            Err(e) => Setup::bad("capture_missing", e.to_string()),
        };
        // no path for Qwen3-ASR: the webview learns the state, not where things live
        let qwen = match batch::BatchConfig::from_env() {
            Ok(_) => Setup::ok(),
            Err(e) => Setup::bad(e.code(), e.to_string()),
        };
        let choice = self.batch_choice();
        let (batch_engine, batch_fallback, batch) = match pick_engine(choice, Ready { whisper: whisper.ok, qwen: qwen.ok }) {
            Some(p) => (p.engine, p.fallback, Setup::ok()),
            // nothing is ready: what the chosen engine (whisper for auto) is missing
            None if choice == batch::QWEN3_ASR => (batch::QWEN3_ASR, false, qwen),
            None => (batch::WHISPER, false, whisper),
        };
        Status {
            dev_build: cfg!(debug_assertions),
            model,
            whisper_cli,
            capture,
            live: Self::live_setup(),
            batch_choice: choice,
            batch_engine,
            batch_fallback,
            batch,
            auto_retranscribe: self.auto_retranscribe(),
        }
    }

    /// The user's choice for 다시 받아쓰기 in `app-settings.json` (`BATCH_CHOICES`); `auto` when there is
    /// none or it is not one the app knows.
    pub fn batch_choice(&self) -> &'static str {
        let chosen = crate::meeting_watch::read_key(&self.data_dir, BATCH_ENGINE_KEY);
        BATCH_CHOICES.into_iter().find(|e| chosen.as_ref().and_then(|v| v.as_str()) == Some(e)).unwrap_or(BATCH_AUTO)
    }

    /// Keep the user's choice of the engine for 다시 받아쓰기. Only a known choice is kept.
    pub fn set_batch_engine(&self, engine: &str) -> ApiResult<()> {
        if !BATCH_CHOICES.contains(&engine) {
            return Err(ApiError::new("invalid_data", "no such engine"));
        }
        crate::meeting_watch::save_key(&self.data_dir, BATCH_ENGINE_KEY, serde_json::json!(engine)).map_err(|e| ApiError::new("io", e.to_string()))
    }

    /// Whether a stopped recording is transcribed again on its own (on unless the user turned it off).
    pub fn auto_retranscribe(&self) -> bool {
        crate::meeting_watch::read_key(&self.data_dir, AUTO_RETRANSCRIBE_KEY).and_then(|v| v.as_bool()).unwrap_or(true)
    }

    pub fn set_auto_retranscribe(&self, on: bool) -> ApiResult<()> {
        crate::meeting_watch::save_key(&self.data_dir, AUTO_RETRANSCRIBE_KEY, serde_json::json!(on)).map_err(|e| ApiError::new("io", e.to_string()))
    }

    // ------------------------------------------------------------ vocabulary (용어 사전)

    /// The stored vocabulary (`vocabulary.json` in the data folder). A damaged file is an error here, so
    /// the settings can say so; elsewhere it counts as empty.
    pub fn vocabulary(&self) -> ApiResult<Vocabulary> {
        vocabulary::load_checked(&self.data_dir)?.map_err(|p| ApiError::new("invalid_data", format!("저장된 용어 사전 파일을 읽지 못했어요. {}", problem_text(&p))))
    }

    /// Check and store the whole vocabulary; returns what was kept (hints trimmed and deduplicated).
    pub fn set_vocabulary(&self, v: Vocabulary) -> ApiResult<Vocabulary> {
        let v = v.validated().map_err(|p| ApiError::new("invalid_data", problem_text(&p)))?;
        Ok(vocabulary::save(&self.data_dir, v)?)
    }

    /// The settings' live preview: `sample` with `replacements` (not stored yet) applied, by the same
    /// function the transcript and the exports use. Rules with nothing to find are skipped.
    pub fn vocabulary_preview(&self, replacements: Vec<Replacement>, sample: &str) -> ApiResult<String> {
        if replacements.len() > vocabulary::MAX_REPLACEMENTS || sample.chars().count() > PREVIEW_MAX_CHARS {
            return Err(ApiError::new("invalid_data", format!("예문은 {PREVIEW_MAX_CHARS}자까지 미리 볼 수 있어요.")));
        }
        let v = Vocabulary { hints: vec![], replacements: replacements.into_iter().filter(|r| !r.find.is_empty()).collect() };
        Ok(vocabulary::apply_replacements(&v, sample))
    }

    /// An imported vocabulary file, checked (size, version, caps, no control characters). Not stored:
    /// the settings show it and the user saves it.
    pub fn vocabulary_read_file(&self, path: &Path) -> ApiResult<Vocabulary> {
        vocabulary::read_file(path)?.map_err(|p| ApiError::new("invalid_data", problem_text(&p)))
    }

    /// The stored vocabulary as the versioned JSON the import reads.
    pub fn vocabulary_json(&self) -> ApiResult<String> {
        Ok(vocabulary::to_json(&self.vocabulary()?))
    }

    /// The vocabulary for what is shown and exported: a damaged file counts as empty (the original
    /// text is shown; the settings report the file).
    fn shown_vocabulary(&self) -> Vocabulary {
        vocabulary::load(&self.data_dir).unwrap_or_default()
    }

    /// The shown text of every word of a version (`vocabulary::display_words`), or None when the
    /// replacements change nothing there. The stored version is not touched.
    pub fn display_words(&self, version_id: &str) -> ApiResult<Option<Vec<String>>> {
        let v = self.shown_vocabulary();
        if v.replacements.is_empty() {
            return Ok(None);
        }
        Ok(vocabulary::display_words(&v, &self.store()?.version(version_id)?))
    }

    /// Setup state of live captions. Missing parts are a note on the record screen, not an error:
    /// the codes are what `LiveConfig::from_env` fails with.
    pub fn live_setup() -> Setup {
        match LiveConfig::from_env() {
            Ok(_) => Setup::ok(), // no path: the webview learns the state, not where things live
            Err(e @ meetko_core::Error::EngineNotFound(_)) => Setup::bad(engine_code(), e.to_string()),
            // the model is looked at first; a missing engine is the bigger news (no download helps)
            Err(e) => match live::find_engine("nemotron-stream") {
                Err(engine) => Setup::bad(engine_code(), engine.to_string()),
                // a model is there but it is not the pinned one (or the variable points at nothing usable)
                Ok(_) if live::model_dir_for(live::ROLE_STREAM, std::env::var_os("MEETKO_STREAM_MODEL").map(PathBuf::from), &util::data_dir()).is_some() => {
                    Setup::bad("stream_model_bad", e.to_string())
                }
                Ok(_) => Setup::bad("stream_model_missing", e.to_string()),
            },
        }
    }

    /// The models the app can set up, with the real environment (variables win over the app's own
    /// folder, as in `LiveConfig::from_env`). Hashes are cached, so after the first check this is a stat.
    pub fn model_setup(&self) -> ModelSetup {
        let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
        let on = |k: &str| std::env::var(k).is_ok_and(|v| v == "1");
        let pieces = model_setup::OFFERS
            .iter()
            .map(|o| {
                let (model_var, unpinned_var, engine_var, engine) = if o.role == live::ROLE_DIARIZATION {
                    ("MEETKO_DIAR_MODEL", "MEETKO_DIAR_MODEL_UNPINNED", "MEETKO_DIAR_ENGINE", meetko_core::diarize::find_engine().is_ok())
                } else if o.role == live::ROLE_BATCH {
                    ("MEETKO_BATCH_MODEL", "MEETKO_BATCH_MODEL_UNPINNED", "MEETKO_BATCH_ENGINE", batch::find_engine().is_ok())
                } else {
                    ("MEETKO_STREAM_MODEL", "MEETKO_STREAM_MODEL_UNPINNED", "MEETKO_STREAM_ENGINE", live::find_engine("nemotron-stream").is_ok())
                };
                ModelPiece { engine_from_env: std::env::var_os(engine_var).is_some_and(|v| !v.is_empty()), ..model_piece(o, var(model_var), on(unpinned_var), &self.data_dir, engine) }
            })
            .collect();
        let card_dismissed = crate::meeting_watch::read_key(&self.data_dir, SETUP_CARD_KEY).and_then(|v| v.get("dismissed").and_then(|d| d.as_bool())).unwrap_or(false);
        ModelSetup { dev_build: cfg!(debug_assertions), pieces, job: None, card_dismissed, batch_engine: self.batch_choice(), auto_retranscribe: self.auto_retranscribe() }
    }

    /// 나중에 on the setup card, kept in `app-settings.json` like the meeting detection switches.
    pub fn dismiss_setup_card(&self) -> ApiResult<()> {
        crate::meeting_watch::save_key(&self.data_dir, SETUP_CARD_KEY, serde_json::json!({ "dismissed": true })).map_err(|e| ApiError::new("io", e.to_string()))
    }

    pub fn list_meetings(&self) -> ApiResult<Vec<MeetingSummary>> {
        let store = self.store()?;
        let mut out = Vec::new();
        for m in store.list_meetings()? {
            let versions = store.versions_of(&m.id)?;
            let latest = versions.last();
            let started_at = store.meeting_and_recording(&m.id)?.0.started_at;
            out.push(MeetingSummary {
                stale_count: store.stale_results(&m.id)?.len(),
                latest_version_id: latest.map(|v| v.id.clone()),
                latest_kind: latest.map(|v| v.kind.as_str()),
                id: m.id,
                title: m.title,
                started_at,
                created_at: m.created_at,
                duration_ms: m.duration_ms,
            });
        }
        // newest first
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    pub fn get_meeting(&self, meeting_id: &str) -> ApiResult<MeetingDetail> {
        let store = self.store()?;
        let (meeting, _rec, files) = store.meeting_and_recording(meeting_id)?;
        let versions = store
            .versions_of(meeting_id)?
            .into_iter()
            .map(|v| VersionView { id: v.id, kind: v.kind.as_str(), parent: v.parent, created_at: v.created_at })
            .collect();
        Ok(MeetingDetail {
            id: meeting.id,
            title: meeting.title,
            started_at: meeting.started_at,
            duration_ms: meeting.duration_ms,
            channels: files.into_iter().map(|f| ChannelView { index: f.index, role: f.role, label: f.label, sha256: f.sha256 }).collect(),
            versions,
        })
    }

    /// The version document exactly as stored (producers, coverage and all).
    pub fn get_version(&self, version_id: &str) -> ApiResult<Value> {
        Ok(self.store()?.version_raw(version_id)?)
    }

    pub fn resolve_anchor(&self, version_id: &str, from: &str, to: &str) -> ApiResult<Anchor> {
        let md = self.store()?.version(version_id)?;
        let (channels, start_ms, end_ms) =
            resolve(&md, from, to).ok_or_else(|| ApiError::new("invalid_data", format!("anchor {from}..{to} does not resolve in {version_id}")))?;
        Ok(Anchor { channels, start_ms, end_ms })
    }

    /// `version_id` in `format` (see [`ExportFormat`]), with the file name the save dialog suggests.
    /// A live or draft version says so in the text (md, txt, vtt) and in the file name. `title` is
    /// the one the screen shows (its fallback formats the date in local time, which only the
    /// webview knows); without it the stored title, else the start date, is used.
    pub fn export_version(&self, version_id: &str, format: &str, title: Option<&str>) -> ApiResult<Export> {
        let format = ExportFormat::parse(format)?;
        let store = self.store()?;
        let md = store.version(version_id)?;
        // what is exported is what the screen shows (overlay::display_text): the names, the edited
        // text as typed, the vocabulary replacements on every other turn; the stored version unchanged
        let overlay = store.overlay(version_id)?;
        let vocabulary = self.shown_vocabulary();
        let view = View::shown(&overlay, &vocabulary);
        let title = match (title, &md.meeting.title, &md.meeting.started_at) {
            (Some(t), _, _) if !t.trim().is_empty() => t.trim().to_string(),
            (_, Some(t), _) if !t.trim().is_empty() => t.clone(),
            (_, _, Some(at)) => format!("회의 {}", at.get(..10).unwrap_or(at)),
            _ => "제목 없음".into(),
        };
        // one line: a break would end the Markdown heading and the text header
        let title: String = title.chars().map(|c| if meetko_core::text::is_hidden_char(c) { ' ' } else { c }).collect();
        let note = match md.version.kind {
            VersionKind::Live => Some(EXPORT_NOTE_LIVE),
            VersionKind::Draft => Some(EXPORT_NOTE_DRAFT),
            VersionKind::Approved => None,
        };
        let doc = export::Doc { title: &title, unassigned: UNASSIGNED_LABEL, note };
        let text = match format {
            ExportFormat::Markdown => export::markdown(&md, &doc, "전사", &view)?,
            ExportFormat::Text => export::timed_text(&md, &doc, &view)?,
            ExportFormat::Srt => export::srt(&md, UNASSIGNED_LABEL, &view)?,
            ExportFormat::Vtt => export::webvtt_with(&md, UNASSIGNED_LABEL, note, &view)?,
            ExportFormat::Plain => export::turns_text(&md, UNASSIGNED_LABEL, false, false, &view)?,
            ExportFormat::Speakers => export::turns_text(&md, UNASSIGNED_LABEL, false, true, &view)?,
        };
        let suffix = match md.version.kind {
            VersionKind::Live => "-실시간",
            VersionKind::Draft => "-초안",
            VersionKind::Approved => "",
        };
        Ok(Export { text, file_name: format!("{}{suffix}.{}", file_stem(&title), format.extension()), format })
    }

    /// The speaker names, merges and edited turns `version_id` is shown with, and what earlier
    /// transcripts of the meeting had that this one does not show.
    pub fn overlay(&self, version_id: &str) -> ApiResult<OverlayView> {
        let store = self.store()?;
        let overlay = store.overlay(version_id)?;
        Self::overlay_view(&store, version_id, overlay)
    }

    fn overlay_view(store: &Store, version_id: &str, overlay: Overlay) -> ApiResult<OverlayView> {
        Ok(OverlayView { version_id: version_id.to_string(), overlay, set_aside: store.overlays_set_aside(version_id)? })
    }

    /// One change to the overlay of `version_id` (see `meetko_core::store::overlay`).
    pub fn change_overlay(&self, version_id: &str, change: OverlayChange) -> ApiResult<OverlayView> {
        let store = self.store()?;
        let overlay = match change {
            OverlayChange::Rename { speaker_id, name } => store.rename_speaker(version_id, &speaker_id, &name)?,
            OverlayChange::Merge { speaker_id, into } => store.merge_speaker(version_id, &speaker_id, &into)?,
            OverlayChange::ResetSpeaker { speaker_id } => store.reset_speaker(version_id, &speaker_id)?,
            OverlayChange::Edit { turn_id, text } => store.edit_turn(version_id, &turn_id, &text)?,
            OverlayChange::ResetTurn { turn_id } => store.reset_turn(version_id, &turn_id)?,
            OverlayChange::DiscardSetAside => {
                store.discard_set_aside(version_id)?;
                store.overlay(version_id)?
            }
        };
        Self::overlay_view(&store, version_id, overlay)
    }

    /// Literal text search over each meeting's head version (see `meetko_core::store::search`).
    /// Any input is safe: it is never parsed as query syntax, and long input is cut.
    /// The search reads the stored text (edited turns as edited); the snippets are shown with the
    /// replacements applied to unedited text (each piece on its own: a rule that spans the edge of
    /// the match is not applied there).
    pub fn search(&self, query: &str) -> ApiResult<SearchOutcome> {
        Ok(self.store()?.search_shown(query, Some(&self.shown_vocabulary()))?)
    }

    pub fn approve(&self, version_id: &str) -> ApiResult<String> {
        let store = self.store()?;
        if store.version_row(version_id)?.kind != VersionKind::Draft {
            return Err(ApiError::new("invalid_data", "only a draft can be approved"));
        }
        Ok(store.approve(version_id)?.version.id)
    }

    /// Blocking; the caller runs it off the main thread. Returns the new draft's id.
    pub fn transcribe(&self, meeting_id: &str, engine: &dyn BatchEngine, progress: &dyn Fn(Progress), cancel: &AtomicBool) -> ApiResult<String> {
        Ok(self.transcribe_diarized(meeting_id, engine, None, progress, cancel)?.0)
    }

    /// `transcribe` with speaker separation when `diar` is given. Also returns the warnings of the
    /// channels whose separation failed: those kept one speaker, the draft is stored all the same.
    pub fn transcribe_diarized(
        &self,
        meeting_id: &str,
        engine: &dyn BatchEngine,
        diar: Option<&dyn DiarEngine>,
        progress: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> ApiResult<(String, Vec<String>)> {
        let store = self.store()?;
        let done = pipeline::transcribe_with(&store, engine, diar, meeting_id, None, progress, cancel)?;
        Ok((done.data.version.id, done.warnings))
    }

    /// The newest live version of a meeting that has no draft or approved version yet: the one a
    /// first draft would be made from. `None` when there is nothing to make or it is made already.
    pub fn live_draft_source(&self, meeting_id: &str) -> ApiResult<Option<String>> {
        let store = self.store()?;
        if store.has_draft_or_approved(meeting_id)? {
            return Ok(None);
        }
        Ok(store.versions_of(meeting_id)?.into_iter().rev().find(|v| v.kind == VersionKind::Live).map(|v| v.id))
    }

    /// Blocking; make the first draft from the newest live version (see `pipeline::draft_from_live`),
    /// with speaker separation when `diar` is given. Returns the draft's id and the warnings.
    pub fn draft_from_live(&self, meeting_id: &str, diar: Option<&dyn DiarEngine>, progress: &dyn Fn(Progress), cancel: &AtomicBool) -> ApiResult<(String, Vec<String>)> {
        let store = self.store()?;
        let live = store
            .versions_of(meeting_id)?
            .into_iter()
            .rev()
            .find(|v| v.kind == VersionKind::Live)
            .ok_or_else(|| ApiError::new("not_found", "this meeting has no live captions"))?;
        let done = pipeline::draft_from_live(&store, &live.id, diar, progress, cancel)?;
        Ok((done.data.version.id, done.warnings))
    }

    /// Run one `transcribe` job to its end (blocking; the app runs it on its own thread, holding the
    /// job slot all along). Every event but the last goes to `emit`; the last one is returned, so the
    /// caller can free the slot before the UI hears that the job is over.
    ///
    /// `FromLiveThenBatch` makes the live draft (its `done` goes to `emit` like any first draft), then,
    /// when the setting is on and `pick_engine` finds a ready engine, transcribes the recording again
    /// with it in the same slot (`auto` events). That pass reuses the speakers the live session
    /// separated (`pipeline::transcribe_reusing`). Its draft is stored like any 다시 받아쓰기: it becomes
    /// the head unless something is approved by then, and results made on the live draft go stale.
    /// A failure or cancel of that pass leaves the live draft as it is.
    pub fn run_draft_job(&self, kind: DraftJob, meeting_id: &str, engines: &dyn Engines, diar: &DiarSetup, emit: &dyn Fn(JobEvent), cancel: &AtomicBool) -> JobEvent {
        let base = |state, from_live, pick: Option<Pick>| JobEvent {
            state,
            meeting_id: Some(meeting_id.to_string()),
            from_live,
            auto: kind == DraftJob::FromLiveThenBatch && !from_live,
            engine: pick.map(|p| p.engine),
            fallback: pick.is_some_and(|p| p.fallback),
            ..Default::default()
        };
        let finish = |res: ApiResult<(String, Vec<String>)>, mut notes: Vec<String>, off: bool, from_live, pick| match res {
            Ok((v, warnings)) => {
                notes.extend(warnings);
                let mut e = JobEvent { version_id: Some(v), ..base("done", from_live, pick) };
                if !notes.is_empty() {
                    // off: the model or engine could not be used at all; failed: a channel's separation failed
                    (e.code, e.message) = (Some(if off { "diarization_off" } else { "diarization_failed" }), Some(notes.join("; ")));
                }
                e
            }
            Err(e) if e.code == "cancelled" => base("cancelled", from_live, pick),
            Err(err) => JobEvent { code: Some(err.code), message: Some(err.message), ..base("error", from_live, pick) },
        };
        let batch = |pick: Pick, speakers_from: Option<String>, notes: Vec<String>, off: bool| {
            let progress = |p: Progress| emit(JobEvent { channel: p.channel, channels: p.channels, percent: p.percent, diarizing: p.diarizing, ..base("progress", false, Some(pick)) });
            // "preparing": staging or hashing a large model can take a while before any progress
            emit(base("progress", false, Some(pick)));
            let res = engines.open(pick.engine).and_then(|engine| {
                let store = self.store()?;
                let done = pipeline::transcribe_reusing(&store, &*engine, diar.engine, meeting_id, speakers_from.as_deref(), None, &progress, cancel)?;
                Ok((done.data.version.id, done.warnings))
            });
            finish(res, notes, off, false, Some(pick))
        };
        if kind == DraftJob::Batch {
            // nothing ready: the chosen engine (whisper for auto) is tried, so the error says what it misses
            let choice = self.batch_choice();
            let pick = pick_engine(choice, engines.ready()).unwrap_or(Pick { engine: if choice == batch::QWEN3_ASR { batch::QWEN3_ASR } else { batch::WHISPER }, fallback: false });
            return batch(pick, None, diar.notes.clone(), diar.off);
        }
        let progress = |p: Progress| emit(JobEvent { channel: p.channel, channels: p.channels, percent: p.percent, diarizing: p.diarizing, ..base("progress", true, None) });
        emit(base("progress", true, None));
        let live = finish(self.draft_from_live(meeting_id, diar.engine, &progress, cancel), diar.notes.clone(), diar.off, true, None);
        if kind == DraftJob::FromLive || live.state != "done" || cancel.load(std::sync::atomic::Ordering::SeqCst) || !self.auto_retranscribe() {
            return live;
        }
        let Some(pick) = pick_engine(self.batch_choice(), engines.ready()) else { return live };
        let speakers_from = live.version_id.as_deref().and_then(|v| self.store().ok()?.version_row(v).ok()?.parent);
        emit(live);
        // the speaker notes were said with the live draft already: only this pass's own failures
        batch(pick, speakers_from, vec![], false)
    }

    // ---------------------------------------------------------------- capture

    /// Import what a finished capture session left. Never fails the call: the UI needs the
    /// provider's own error text (permission fixes) even when nothing could be imported.
    pub fn finish_capture(&self, outcome: &CaptureOutcome, live_out: Option<&LiveOutcome>) -> CaptureDone {
        let imported = self.store().and_then(|s| outcome.import(&s, (MIC_LABEL, LOOPBACK_LABEL), None).map_err(ApiError::from));
        match imported {
            Ok((imp, warnings)) => {
                // the live version is a bonus on top of a saved recording: whatever goes wrong
                // here leaves the meeting as it is and is reported as a quiet note
                let (live_version_id, live_note) = match live_out {
                    None => (None, None),
                    Some(lo) => match self.store().and_then(|s| live::store_live(&s, &imp.meeting.id, lo, live::CHUNK_MS).map_err(ApiError::from)) {
                        Ok(Some(md)) => (Some(md.version.id), None),
                        Ok(None) => (None, Some("live_unavailable")),
                        Err(_) => (None, Some("live_not_stored")),
                    },
                };
                CaptureDone { meeting_id: Some(imp.meeting.id), existing: imp.existing, warnings, provider_errors: outcome.errors.clone(), error: None, live_version_id, live_note }
            }
            Err(e) => CaptureDone {
                meeting_id: None,
                existing: false,
                warnings: vec![],
                provider_errors: outcome.errors.clone(),
                error: Some(e.message),
                live_version_id: None,
                live_note: None,
            },
        }
    }

    // ---------------------------------------------------------------- plugins

    fn preview(dir: &Path) -> ApiResult<PluginPreview> {
        let (manifest, manifest_sha) = Manifest::load(dir)?;
        manifest.require_plugin()?;
        let p = &manifest.package;
        Ok(PluginPreview {
            id: p.id.clone(),
            version: p.version.clone(),
            description: p.description.clone(),
            network: manifest.permissions.network,
            audio: manifest.input.audio.clone(),
            secrets: manifest.permissions.secrets.clone(),
            settings: manifest.permissions.settings.clone(),
            keeps_values: false,
            display: manifest.output.display.clone(),
            code_sha256: plugin_code_sha256(&manifest, dir)?,
            manifest_sha256: manifest_sha,
            folder: dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            notice: permission_notice(),
            isolated: plugins_isolated(),
            source: None,
            replaces: None,
            token: None,
        })
    }

    /// What adding `id` from `new` (None = a local folder) would overwrite, if it is from elsewhere.
    fn replaces(&self, id: &str, new: Option<&PluginSource>) -> ApiResult<Option<Replaces>> {
        Ok(self.store()?.replaced_origin(id, new)?.map(|(version, old)| Replaces { key: meetko_core::store::replace_key(&version, old.as_ref()), version, source: old.as_ref().map(source_text) }))
    }

    /// Network, on the user's click only: download the gallery index from `index_url`.
    pub fn gallery_list(&self, index_url: &str) -> ApiResult<Vec<GalleryEntry>> {
        Ok(plugin_source::fetch_index(index_url)?)
    }

    /// Network, on the user's click only: download and unpack the commit `entry` pins (the entry is
    /// one the app itself fetched in the index, never a value from the webview) and preview what
    /// would be installed. The tarball is what users get (`git archive` honours `export-subst`), so
    /// it must say what the index said: id, version and declared permissions. Nothing is installed.
    pub fn gallery_preview(&self, entry: &GalleryEntry, ep: &Endpoints) -> ApiResult<(plugin_source::Fetched, PluginPreview)> {
        let spec = entry.spec()?;
        let fetched = plugin_source::fetch(ep, &spec)?;
        let mut preview = Self::preview(&fetched.dir)?;
        let differs = |what: &str| ApiError::new("index_mismatch", format!("the downloaded plugin's {what} differs from what the gallery listed; refusing to install it"));
        if preview.id != entry.id {
            return Err(differs("id"));
        }
        if preview.version != entry.version {
            return Err(differs("version"));
        }
        if preview.network != entry.permissions.network {
            return Err(differs("network permission"));
        }
        if (preview.audio != "none") != entry.permissions.audio {
            return Err(differs("audio permission"));
        }
        if preview.secrets != entry.permissions.secrets {
            return Err(differs("API key (secrets) list"));
        }
        if preview.settings != entry.permissions.settings {
            return Err(differs("settings list"));
        }
        preview.folder = spec.repo.clone();
        preview.source = Some(source_text(&fetched.source));
        preview.replaces = self.replaces(&preview.id, Some(&fetched.source))?;
        preview.keeps_values = self.keeps_values(&preview, Some(&fetched.source))?;
        Ok((fetched, preview))
    }

    /// Install the tree the user saw. `expected_code_sha256` is the hash that was on screen;
    /// `replace` is the `replaces.key` the user confirmed (the installed state that was shown).
    pub fn gallery_install(&self, fetched: &plugin_source::Fetched, expected_code_sha256: &str, replace: Option<&str>) -> ApiResult<PluginPreview> {
        let store = self.store()?;
        let prior = self.prior_of(&store, &fetched.dir)?;
        let row = store.install_fetched(fetched, Some(expected_code_sha256), replace)?;
        self.forget_moved(&store, prior);
        let mut preview = Self::preview(&row.dir)?;
        preview.source = Some(source_text(&fetched.source));
        Ok(preview)
    }

    pub fn inspect_plugin(&self, dir: &Path) -> ApiResult<PluginPreview> {
        let mut preview = Self::preview(dir)?;
        preview.replaces = self.replaces(&preview.id, None)?;
        preview.keeps_values = self.keeps_values(&preview, None)?;
        Ok(preview)
    }

    /// Register the plugin the user confirmed. `expected_code_sha256` is the hash that was
    /// on screen: a folder that changed between the preview and the click is refused.
    pub fn add_plugin(&self, dir: &Path, expected_code_sha256: &str, replace: Option<&str>) -> ApiResult<PluginPreview> {
        // copies the folder into the data dir and verifies the COPY against the hash on screen
        let store = self.store()?;
        let prior = self.prior_of(&store, dir)?;
        let row = store.install_plugin_from(dir, Some(expected_code_sha256), None, replace)?;
        self.forget_moved(&store, prior);
        Self::preview(&row.dir)
    }

    /// The id is installed from the same origin and both versions declare keys or settings: the
    /// saved values carry over (said in the consent dialog). From another origin they never do.
    fn keeps_values(&self, preview: &PluginPreview, new: Option<&PluginSource>) -> ApiResult<bool> {
        let store = self.store()?;
        let declares = !(preview.secrets.is_empty() && preview.settings.is_empty());
        let installed = match store.plugin(&preview.id) {
            Ok((_, m)) => !(m.permissions.secrets.is_empty() && m.permissions.settings.is_empty()),
            Err(meetko_core::Error::NotFound(_)) => false,
            Err(e) => return Err(e.into()),
        };
        Ok(declares && installed && store.replaced_origin(&preview.id, new)?.is_none())
    }

    /// What is installed under the id of the plugin in `dir`, before an install over it.
    fn prior_of(&self, store: &Store, dir: &Path) -> ApiResult<Option<secrets::Prior>> {
        let (manifest, _) = Manifest::load(dir)?;
        Ok(secrets::Prior::of(store, &manifest.package.id)?)
    }

    /// After an install: an id that moved to another origin loses the old origin's keys. They were
    /// unreachable already (keys are per origin), so a failure here only leaves them behind.
    fn forget_moved(&self, store: &Store, prior: Option<secrets::Prior>) {
        if let Some(p) = prior {
            let _ = p.forget_if_moved(store, &*self.secrets);
        }
    }

    /// Each declared secret of an installed plugin and whether it is set. Never a value.
    pub fn plugin_secrets(&self, plugin_id: &str) -> ApiResult<Vec<SecretState>> {
        let store = self.store()?;
        Ok(secrets::status(&store, &*self.secrets, plugin_id)?)
    }

    /// Save a secret the user typed; the answer is the new status only, the value never comes back.
    pub fn set_plugin_secret(&self, plugin_id: &str, name: &str, value: &str) -> ApiResult<Vec<SecretState>> {
        secrets::set(&self.store()?, &*self.secrets, plugin_id, name, value)?;
        self.plugin_secrets(plugin_id)
    }

    pub fn clear_plugin_secret(&self, plugin_id: &str, name: &str) -> ApiResult<Vec<SecretState>> {
        secrets::clear(&self.store()?, &*self.secrets, plugin_id, name)?;
        self.plugin_secrets(plugin_id)
    }

    pub fn list_plugins(&self) -> ApiResult<Vec<PluginPreview>> {
        let store = self.store()?;
        store
            .list_plugins()?
            .into_iter()
            .map(|row: PluginRow| {
                let (_, manifest) = store.plugin(&row.id)?;
                let source = store.plugin_source(&row.id)?.map(|s| source_text(&s));
                Ok(PluginPreview {
                    id: row.id,
                    version: row.version,
                    description: manifest.package.description.clone(),
                    network: row.network,
                    audio: row.audio,
                    secrets: manifest.permissions.secrets.clone(),
                    settings: manifest.permissions.settings.clone(),
                    keeps_values: false,
                    display: manifest.output.display.clone(),
                    code_sha256: row.code_sha256,
                    manifest_sha256: row.manifest_sha256,
                    folder: row.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    notice: permission_notice(),
                    isolated: plugins_isolated(),
                    source,
                    replaces: None,
                    token: None,
                })
            })
            .collect()
    }

    /// Blocking (up to the plugin timeout); the caller runs it off the main thread.
    pub fn run_plugin(&self, plugin_id: &str, version_id: &str) -> ApiResult<ResultView> {
        let store = self.store()?;
        let secs = std::env::var("MEETKO_PLUGIN_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(host::DEFAULT_TIMEOUT.as_secs());
        let id = pipeline::run_plugin_with(&store, &*self.secrets, plugin_id, version_id, Duration::from_secs(secs))?;
        store
            .results_for_version(version_id)?
            .into_iter()
            .find(|r| r.id == id)
            .map(ResultView::from)
            .ok_or_else(|| ApiError::new("not_found", "the stored result is missing"))
    }

    pub fn list_stale(&self, meeting_id: &str) -> ApiResult<Vec<ResultView>> {
        Ok(self.store()?.stale_results(meeting_id)?.into_iter().map(ResultView::from).collect())
    }

    pub fn list_results(&self, version_id: &str) -> ApiResult<Vec<ResultView>> {
        Ok(self.store()?.results_for_version(version_id)?.into_iter().map(ResultView::from).collect())
    }

    // ---------------------------------------------------------------- audio

    #[cfg(test)]
    fn channel_path(&self, meeting_id: &str, index: u32) -> PathBuf {
        self.store().unwrap().channel_files(meeting_id).unwrap().into_iter().find(|f| f.index == index).unwrap().path
    }

    /// Serve a stored channel (`"0"`, `"1"`) or the sample-wise sum of both (`"mix"`) as WAV,
    /// with HTTP range support (WebKit will not seek media without it). The file path is
    /// looked up in the database and must stay inside the data dir; the caller names only a
    /// meeting id and a channel. `Err(status)` is an HTTP status for the protocol handler.
    pub fn audio(&self, meeting_id: &str, which: &str, range: Option<&str>) -> Result<AudioReply, u16> {
        let store = self.store().map_err(|_| 500u16)?;
        let files = store.channel_files(meeting_id).map_err(|_| 404u16)?;
        let root = std::fs::canonicalize(&self.data_dir).map_err(|_| 500u16)?;
        let inside = |p: &Path| -> Result<PathBuf, u16> {
            let c = std::fs::canonicalize(p).map_err(|_| 404u16)?;
            if c.starts_with(&root) { Ok(c) } else { Err(403) }
        };
        let src = if which == "mix" {
            if files.len() != 2 {
                return Err(404);
            }
            Source::Mix(inside(&files[0].path)?, inside(&files[1].path)?)
        } else {
            let idx: u32 = which.parse().map_err(|_| 404u16)?;
            Source::One(inside(&files.iter().find(|f| f.index == idx).ok_or(404u16)?.path)?)
        };
        serve(&src, range)
    }
}

pub struct AudioReply {
    pub status: u16,
    pub content_range: Option<String>,
    pub total: u64,
    pub body: Vec<u8>,
}

enum Source {
    One(PathBuf),
    Mix(PathBuf, PathBuf),
}

const WAV_HEADER: u64 = 44;
/// A reply never carries more than this; an open-ended range gets a shorter 206, which is legal.
const MAX_CHUNK: u64 = 1 << 21;

fn io500(_: std::io::Error) -> u16 {
    500
}

fn read_at(path: &Path, off: u64, len: u64) -> Result<Vec<u8>, u16> {
    let mut f = std::fs::File::open(path).map_err(io500)?;
    f.seek(SeekFrom::Start(off)).map_err(io500)?;
    let mut buf = Vec::new();
    f.take(len).read_to_end(&mut buf).map_err(io500)?;
    Ok(buf)
}

/// `bytes=a-b`, `bytes=a-`, `bytes=-n` against a body of `total` bytes.
fn parse_range(h: &str, total: u64) -> Option<(u64, u64)> {
    let spec = h.strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        (total.saturating_sub(n), total.checked_sub(1)?)
    } else {
        (a.parse().ok()?, if b.is_empty() { total.checked_sub(1)? } else { b.parse::<u64>().ok()?.min(total.checked_sub(1)?) })
    };
    (start <= end && start < total).then_some((start, end))
}

fn serve(src: &Source, range: Option<&str>) -> Result<AudioReply, u16> {
    let total = match src {
        Source::One(p) => std::fs::metadata(p).map_err(io500)?.len(),
        Source::Mix(a, b) => std::fs::metadata(a).map_err(io500)?.len().min(std::fs::metadata(b).map_err(io500)?.len()),
    };
    if total < WAV_HEADER {
        return Err(500);
    }
    let (start, end, partial) = match range {
        Some(h) => match parse_range(h, total) {
            Some((s, e)) => (s, e.min(s + MAX_CHUNK - 1), true),
            None => return Err(416),
        },
        None => (0, (total - 1).min(MAX_CHUNK - 1), total > MAX_CHUNK),
    };
    let len = end - start + 1;
    let body = match src {
        Source::One(p) => read_at(p, start, len)?,
        Source::Mix(a, b) => mix(a, b, total, start, len)?,
    };
    Ok(AudioReply {
        status: if partial { 206 } else { 200 },
        content_range: partial.then(|| format!("bytes {start}-{}/{total}", start + body.len() as u64 - 1)),
        total,
        body,
    })
}

/// Bytes `[start, start+len)` of the virtual WAV that sums both channels (saturating).
/// The header comes from channel 0 with the sizes patched; samples are summed on 2-byte
/// boundaries, so a range that starts mid-sample is widened and trimmed back.
fn mix(a: &Path, b: &Path, total: u64, start: u64, len: u64) -> Result<Vec<u8>, u16> {
    let mut out = Vec::with_capacity(len as usize);
    let end = start + len;
    if start < WAV_HEADER {
        let mut h = read_at(a, 0, WAV_HEADER)?;
        h[4..8].copy_from_slice(&((total - 8) as u32).to_le_bytes());
        h[40..44].copy_from_slice(&((total - WAV_HEADER) as u32).to_le_bytes());
        out.extend_from_slice(&h[start as usize..end.min(WAV_HEADER) as usize]);
    }
    if end > WAV_HEADER {
        let from = start.max(WAV_HEADER);
        let from_even = WAV_HEADER + ((from - WAV_HEADER) & !1);
        let to_even = (WAV_HEADER + ((end - WAV_HEADER + 1) & !1)).min(total & !1);
        let n = to_even.saturating_sub(from_even);
        let (x, y) = (read_at(a, from_even, n)?, read_at(b, from_even, n)?);
        let mut samples = Vec::with_capacity(x.len());
        for (p, q) in x.as_chunks::<2>().0.iter().zip(y.as_chunks::<2>().0) {
            let s = i32::from(i16::from_le_bytes(*p)) + i32::from(i16::from_le_bytes(*q));
            samples.extend_from_slice(&(s.clamp(-32768, 32767) as i16).to_le_bytes());
        }
        let skip = (from - from_even) as usize;
        let take = (end - from) as usize;
        out.extend(samples.iter().skip(skip).take(take));
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
