//! The command layer without Tauri: plain functions over the core, so they can be tested
//! headless. `lib.rs` wraps each in a typed `#[tauri::command]`. Nothing here takes a
//! filesystem path from the webview except `add_plugin`, which is fed the folder the user
//! picked in the native dialog (`lib.rs` never accepts a path from JS at all).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use meetko_core::capture_run::{CaptureOutcome, ProviderError};
use meetko_core::contract::{resolve, VersionKind};
use meetko_core::diarize::DiarEngine;
use meetko_core::live::{self, LiveConfig, LiveOutcome};
use meetko_core::manifest::{plugin_code_sha256, Manifest};
use meetko_core::plugin_source::{self, Endpoints, GalleryEntry, PluginSource};
use meetko_core::store::search::SearchOutcome;
use meetko_core::store::{PluginRow, ResultRow, Store};
use meetko_core::whisper::{BatchEngine, Progress, WhisperCli};
use meetko_core::{capture_run, export, host, model_setup, pipeline, util};
use serde::Serialize;
use serde_json::Value;

// the Korean channel names are an app-layer concern (the core only knows mic/loopback)
/// The key of `app-settings.json` that remembers the setup card was put away.
pub const SETUP_CARD_KEY: &str = "setup_card";

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
}

/// One model the app can set up for the user (`meetko_core::model_setup::OFFERS`). No path: the
/// webview learns the state and what a download would be, not where files live.
#[derive(Serialize, Debug, Clone)]
pub struct ModelPiece {
    /// stream | diarization
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
    /// "nvidia/…" on Hugging Face, and the host the download starts at
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
    /// the isolation the run had: macos-sbpl | none | off | unrecorded
    pub sandbox: String,
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
}

impl Core {
    pub fn from_env() -> Self {
        Core { data_dir: util::data_dir() }
    }

    /// One connection per call: `Store` is not `Sync`, and SQLite (WAL + busy_timeout) is
    /// shared with the CLI anyway.
    pub fn store(&self) -> ApiResult<Store> {
        Ok(Store::open(&self.data_dir)?)
    }

    pub fn status(&self) -> Status {
        let model = match WhisperCli::load(&self.data_dir) {
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
        let capture = match capture_run::resolve_bin(&self.data_dir) {
            Ok(p) => Setup::ok().at(&p),
            Err(e) => Setup::bad("capture_missing", e.to_string()),
        };
        Status { dev_build: cfg!(debug_assertions), model, whisper_cli, capture, live: Self::live_setup() }
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
                let (model_var, unpinned_var, engine) = if o.role == live::ROLE_DIARIZATION {
                    ("MEETKO_DIAR_MODEL", "MEETKO_DIAR_MODEL_UNPINNED", meetko_core::diarize::find_engine().is_ok())
                } else {
                    ("MEETKO_STREAM_MODEL", "MEETKO_STREAM_MODEL_UNPINNED", live::find_engine("nemotron-stream").is_ok())
                };
                let engine_var = if o.role == live::ROLE_DIARIZATION { "MEETKO_DIAR_ENGINE" } else { "MEETKO_STREAM_ENGINE" };
                ModelPiece { engine_from_env: std::env::var_os(engine_var).is_some_and(|v| !v.is_empty()), ..model_piece(o, var(model_var), on(unpinned_var), &self.data_dir, engine) }
            })
            .collect();
        let card_dismissed = crate::meeting_watch::read_key(&self.data_dir, SETUP_CARD_KEY).and_then(|v| v.get("dismissed").and_then(|d| d.as_bool())).unwrap_or(false);
        ModelSetup { dev_build: cfg!(debug_assertions), pieces, job: None, card_dismissed }
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
        let md = self.store()?.version(version_id)?;
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
            ExportFormat::Markdown => export::markdown(&md, &doc, "전사")?,
            ExportFormat::Text => export::timed_text(&md, &doc)?,
            ExportFormat::Srt => export::srt(&md, UNASSIGNED_LABEL)?,
            ExportFormat::Vtt => export::webvtt_with(&md, UNASSIGNED_LABEL, note)?,
            ExportFormat::Plain => export::turns_text(&md, UNASSIGNED_LABEL, false, false)?,
            ExportFormat::Speakers => export::turns_text(&md, UNASSIGNED_LABEL, false, true)?,
        };
        let suffix = match md.version.kind {
            VersionKind::Live => "-실시간",
            VersionKind::Draft => "-초안",
            VersionKind::Approved => "",
        };
        Ok(Export { text, file_name: format!("{}{suffix}.{}", file_stem(&title), format.extension()), format })
    }

    /// Literal text search over each meeting's head version (see `meetko_core::store::search`).
    /// Any input is safe: it is never parsed as query syntax, and long input is cut.
    pub fn search(&self, query: &str) -> ApiResult<SearchOutcome> {
        Ok(self.store()?.search(query)?)
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
        preview.folder = spec.repo.clone();
        preview.source = Some(source_text(&fetched.source));
        preview.replaces = self.replaces(&preview.id, Some(&fetched.source))?;
        Ok((fetched, preview))
    }

    /// Install the tree the user saw. `expected_code_sha256` is the hash that was on screen;
    /// `replace` is the `replaces.key` the user confirmed (the installed state that was shown).
    pub fn gallery_install(&self, fetched: &plugin_source::Fetched, expected_code_sha256: &str, replace: Option<&str>) -> ApiResult<PluginPreview> {
        let row = self.store()?.install_fetched(fetched, Some(expected_code_sha256), replace)?;
        let mut preview = Self::preview(&row.dir)?;
        preview.source = Some(source_text(&fetched.source));
        Ok(preview)
    }

    pub fn inspect_plugin(&self, dir: &Path) -> ApiResult<PluginPreview> {
        let mut preview = Self::preview(dir)?;
        preview.replaces = self.replaces(&preview.id, None)?;
        Ok(preview)
    }

    /// Register the plugin the user confirmed. `expected_code_sha256` is the hash that was
    /// on screen: a folder that changed between the preview and the click is refused.
    pub fn add_plugin(&self, dir: &Path, expected_code_sha256: &str, replace: Option<&str>) -> ApiResult<PluginPreview> {
        // copies the folder into the data dir and verifies the COPY against the hash on screen
        let row = self.store()?.install_plugin_from(dir, Some(expected_code_sha256), None, replace)?;
        Self::preview(&row.dir)
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
        let id = pipeline::run_plugin(&store, plugin_id, version_id, Duration::from_secs(secs))?;
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
