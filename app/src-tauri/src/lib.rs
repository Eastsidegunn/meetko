//! meetko desktop app: a thin Tauri shell over `meetko-core`. Every command is a typed
//! wrapper around `api::Core`; long work runs on its own thread and reports through events.

pub mod api;
pub mod meeting_watch;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use api::{ApiError, ApiResult, Core};
use meetko_core::capture_run::{self, CaptureSession};
use meetko_core::diarize::{DiarConfig, DiarEngine, DiarProcess};
use meetko_core::live::{LiveConfig, LiveSession, LiveUpdate};
use meetko_core::whisper::{Progress, WhisperCli};
use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

/// At most one of each long job; the cancel flag is the core's own.
#[derive(Default)]
struct Jobs {
    transcribe: Option<Arc<AtomicBool>>,
    plugin_busy: bool,
    /// a save dialog of `export_save` is open
    export_saving: bool,
    capture: Option<CaptureSession>,
    /// live captions of the running capture; None when not set up or not started
    live: Option<LiveSession>,
    /// set under the lock before the (awaited) spawn, so two starts cannot both pass the check
    capture_starting: bool,
    /// bumped at every start and stop; events and pollers of an older session are ignored
    capture_gen: u64,
    /// the folder the user picked, kept here so JS never sends a path back
    pending_plugin: Option<PathBuf>,
    /// the gallery download the user is looking at (unpacked in a private temp dir), with the
    /// token of the preview it belongs to: an install must present the same token
    pending_gallery: api::PendingGallery,
    /// the index rows this app fetched; previews are looked up here by id, so the webview cannot
    /// name a repo or commit of its own
    gallery_index: Vec<meetko_core::plugin_source::GalleryEntry>,
    /// the model download or copy in flight (one at a time), and its cancel flag
    model: Option<(api::ModelJobView, Arc<AtomicBool>)>,
    /// a file dialog of `model_pick` is open
    model_picking: bool,
}

/// Resets a busy flag when dropped, so a panicking worker thread cannot leave the app stuck.
struct Reset {
    app: AppHandle,
    f: fn(&mut Jobs),
}

impl Drop for Reset {
    fn drop(&mut self) {
        let st = self.app.state::<AppState>();
        let mut jobs = st.jobs.lock().unwrap_or_else(|p| p.into_inner());
        (self.f)(&mut jobs);
    }
}

struct AppState {
    core: Arc<Core>,
    jobs: Mutex<Jobs>,
    meeting: Arc<meeting_watch::Shared>,
}

/// Payload of the `transcribe` and `plugin-run` events.
#[derive(Serialize, Clone, Default)]
struct JobEvent {
    /// progress | done | error | cancelled
    state: &'static str,
    meeting_id: Option<String>,
    version_id: Option<String>,
    plugin_id: Option<String>,
    result_id: Option<i64>,
    channel: u32,
    channels: u32,
    percent: u8,
    /// the channel is transcribed and its speakers are being separated
    diarizing: bool,
    /// the job makes the first draft from the live captions (no batch transcription)
    from_live: bool,
    code: Option<&'static str>,
    message: Option<String>,
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> ApiResult<T> + Send + 'static) -> ApiResult<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| ApiError::new("error", e.to_string()))?
}

fn lock(state: &AppState) -> std::sync::MutexGuard<'_, Jobs> {
    state.jobs.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------- reads

#[tauri::command]
async fn status(state: State<'_, AppState>) -> ApiResult<api::Status> {
    let core = state.core.clone();
    blocking(move || Ok(core.status())).await
}

#[tauri::command]
async fn list_meetings(state: State<'_, AppState>) -> ApiResult<Vec<api::MeetingSummary>> {
    let core = state.core.clone();
    blocking(move || core.list_meetings()).await
}

#[tauri::command]
async fn get_meeting(state: State<'_, AppState>, meeting_id: String) -> ApiResult<api::MeetingDetail> {
    let core = state.core.clone();
    blocking(move || core.get_meeting(&meeting_id)).await
}

#[tauri::command]
async fn get_version(state: State<'_, AppState>, version_id: String) -> ApiResult<serde_json::Value> {
    let core = state.core.clone();
    blocking(move || core.get_version(&version_id)).await
}

#[tauri::command]
async fn resolve_anchor(state: State<'_, AppState>, version_id: String, from: String, to: String) -> ApiResult<api::Anchor> {
    let core = state.core.clone();
    blocking(move || core.resolve_anchor(&version_id, &from, &to)).await
}

#[tauri::command]
async fn search(state: State<'_, AppState>, query: String) -> ApiResult<meetko_core::store::search::SearchOutcome> {
    let core = state.core.clone();
    blocking(move || core.search(&query)).await
}

#[tauri::command]
async fn list_results(state: State<'_, AppState>, version_id: String) -> ApiResult<Vec<api::ResultView>> {
    let core = state.core.clone();
    blocking(move || core.list_results(&version_id)).await
}

/// Stale results across all versions of a meeting (the head's own are in `list_results`).
#[tauri::command]
async fn list_stale(state: State<'_, AppState>, meeting_id: String) -> ApiResult<Vec<api::ResultView>> {
    let core = state.core.clone();
    blocking(move || core.list_stale(&meeting_id)).await
}

#[tauri::command]
async fn list_plugins(state: State<'_, AppState>) -> ApiResult<Vec<api::PluginPreview>> {
    let core = state.core.clone();
    blocking(move || core.list_plugins()).await
}

/// The text of a version in `format` (md, txt, srt, vtt, plain, speakers), for the clipboard.
/// `title` is the one the screen shows (see `Core::export_version`).
#[tauri::command]
async fn export_version(state: State<'_, AppState>, version_id: String, format: String, title: Option<String>) -> ApiResult<String> {
    let core = state.core.clone();
    blocking(move || Ok(core.export_version(&version_id, &format, title.as_deref())?.text)).await
}

/// Native save dialog (modal to the window), then write the export there. Like `plugin_pick`, the
/// path never leaves Rust: JS gets the chosen file's name only, or None when the user cancelled.
/// One at a time: a second request while a dialog is open is `busy`.
#[tauri::command]
async fn export_save(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
    version_id: String,
    format: String,
    title: Option<String>,
) -> ApiResult<Option<String>> {
    {
        let mut jobs = lock(&state);
        if jobs.export_saving {
            return Err(ApiError::new("busy", "a save dialog is already open"));
        }
        jobs.export_saving = true;
    }
    let _reset = Reset { app: app.clone(), f: |j| j.export_saving = false };
    let core = state.core.clone();
    // built first: a version that cannot be exported fails before any dialog opens
    let ex = blocking(move || core.export_version(&version_id, &format, title.as_deref())).await?;
    let (name, ext, filter) = (ex.file_name.clone(), ex.format.extension(), ex.format.filter_name());
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app.dialog().file().set_parent(&window).set_file_name(name).add_filter(filter, &[ext]).blocking_save_file()
    })
    .await
    .map_err(|e| ApiError::new("error", e.to_string()))?;
    let Some(path) = picked.and_then(|p| p.into_path().ok()) else { return Ok(None) };
    blocking(move || {
        std::fs::write(&path, ex.text.as_bytes()).map_err(|e| ApiError::new("export_write_failed", e.to_string()))?;
        Ok(Some(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()))
    })
    .await
}

// ---------------------------------------------------------------- writes

#[tauri::command]
async fn approve(state: State<'_, AppState>, version_id: String) -> ApiResult<String> {
    let core = state.core.clone();
    blocking(move || core.approve(&version_id)).await
}

/// What a `transcribe` job makes: a batch transcription (whisper), or the first draft from the
/// meeting's live captions. Both share the one job slot, its cancel flag and its events.
#[derive(Clone, Copy, PartialEq)]
enum DraftJob {
    Batch,
    FromLive,
}

#[tauri::command]
async fn transcribe_start(app: AppHandle, state: State<'_, AppState>, meeting_id: String) -> ApiResult<()> {
    start_draft_job(&app, &state, meeting_id, DraftJob::Batch)
}

/// The draft/approve flow without whisper: make the first draft from the live captions.
#[tauri::command]
async fn live_draft_start(app: AppHandle, state: State<'_, AppState>, meeting_id: String) -> ApiResult<()> {
    start_draft_job(&app, &state, meeting_id, DraftJob::FromLive)
}

fn start_draft_job(app: &AppHandle, state: &AppState, meeting_id: String, kind: DraftJob) -> ApiResult<()> {
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut jobs = lock(state);
        if jobs.transcribe.is_some() {
            return Err(ApiError::new("busy", "a transcription is already running"));
        }
        jobs.transcribe = Some(cancel.clone());
    }
    let core = state.core.clone();
    let app = app.clone();
    std::thread::spawn(move || {
        let _reset = Reset { app: app.clone(), f: |j| j.transcribe = None };
        let event = |state, f: &dyn Fn(&mut JobEvent)| {
            let mut e = JobEvent { state, meeting_id: Some(meeting_id.clone()), from_live: kind == DraftJob::FromLive, ..Default::default() };
            f(&mut e);
            e
        };
        let ev = |state, f: &dyn Fn(&mut JobEvent)| {
            let _ = app.emit("transcribe", event(state, f));
        };
        let progress = |p: Progress| ev("progress", &|e| (e.channel, e.channels, e.percent, e.diarizing) = (p.channel, p.channels, p.percent, p.diarizing));
        // speaker separation is optional (MEETKO_DIAR_MODEL, like MEETKO_STREAM_MODEL for live captions)
        // and never costs the transcript: a model that is set but unusable is only a warning
        let (diar, mut notes, off) = match DiarConfig::from_env() {
            Ok(c) => (c.map(DiarProcess::new), vec![], false),
            Err(e) => (None, vec![format!("speaker separation is off: {e}")], true),
        };
        // "preparing": hashing a large model for the first time can take a while before any progress.
        // Every job sends it, so the UI knows a job runs even when it is over at once
        ev("progress", &|e| e.channels = 0);
        let diar_ref = diar.as_ref().map(|d| d as &dyn DiarEngine);
        let res = match kind {
            DraftJob::Batch => WhisperCli::load(&core.data_dir)
                .map_err(ApiError::from)
                .and_then(|engine| core.transcribe_diarized(&meeting_id, &engine, diar_ref, &progress, &cancel)),
            DraftJob::FromLive => core.draft_from_live(&meeting_id, diar_ref, &progress, &cancel),
        };
        let last = match res {
            Ok((v, warnings)) => {
                notes.extend(warnings);
                // the draft is stored; the UI shows what it could not do
                event("done", &|e| {
                    e.version_id = Some(v.clone());
                    if !notes.is_empty() {
                        // off: the model or engine could not be used at all; failed: a channel's separation failed
                        (e.code, e.message) = (Some(if off { "diarization_off" } else { "diarization_failed" }), Some(notes.join("; ")));
                    }
                })
            }
            Err(e) if e.code == "cancelled" => event("cancelled", &|_| {}),
            Err(err) => event("error", &|e| (e.code, e.message) = (Some(err.code), Some(err.message.clone()))),
        };
        drop(diar); // ends the diarization engine process
        drop(_reset); // free the slot before the UI hears that the job is over (as plugin_run does)
        let _ = app.emit("transcribe", last);
    });
    Ok(())
}

#[tauri::command]
async fn transcribe_cancel(state: State<'_, AppState>) -> ApiResult<()> {
    if let Some(flag) = &lock(&state).transcribe {
        flag.store(true, Ordering::SeqCst);
    }
    Ok(())
}

#[tauri::command]
async fn plugin_run(app: AppHandle, state: State<'_, AppState>, plugin_id: String, version_id: String) -> ApiResult<()> {
    {
        let mut jobs = lock(&state);
        if jobs.plugin_busy {
            return Err(ApiError::new("busy", "a plugin is already running"));
        }
        jobs.plugin_busy = true;
    }
    let core = state.core.clone();
    // the core's host enforces the time budget; it has no cancel hook yet
    std::thread::spawn(move || {
        let _reset = Reset { app: app.clone(), f: |j| j.plugin_busy = false };
        let mut e = JobEvent { state: "done", plugin_id: Some(plugin_id.clone()), version_id: Some(version_id.clone()), ..Default::default() };
        match core.run_plugin(&plugin_id, &version_id) {
            Ok(r) => e.result_id = Some(r.id),
            Err(err) => (e.state, e.code, e.message) = ("error", Some(err.code), Some(err.message)),
        }
        drop(_reset); // free the slot before the UI hears that the run is over
        let _ = app.emit("plugin-run", e);
    });
    Ok(())
}

/// Native folder picker. The path stays in Rust; JS gets the preview to show in its dialog.
#[tauri::command]
async fn plugin_pick(app: AppHandle, window: WebviewWindow, state: State<'_, AppState>) -> ApiResult<Option<api::PluginPreview>> {
    lock(&state).pending_plugin = None; // a cancelled or failed pick must not leave an old folder armed
    let core = state.core.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || app.dialog().file().set_parent(&window).blocking_pick_folder())
        .await
        .map_err(|e| ApiError::new("error", e.to_string()))?;
    let Some(dir) = picked.and_then(|p| p.into_path().ok()) else { return Ok(None) };
    let preview = core.inspect_plugin(&dir)?;
    lock(&state).pending_plugin = Some(dir);
    Ok(Some(preview))
}

#[tauri::command]
async fn plugin_add(state: State<'_, AppState>, code_sha256: String, replace: Option<String>) -> ApiResult<api::PluginPreview> {
    let dir = lock(&state).pending_plugin.take().ok_or_else(|| ApiError::new("not_found", "no plugin was picked"))?;
    let core = state.core.clone();
    blocking(move || core.add_plugin(&dir, &code_sha256, replace.as_deref())).await
}

/// Gallery: all three touch the network only because the user clicked (browse / install), and the
/// download happens here, not in the webview (its CSP allows no network).
#[tauri::command]
async fn gallery_list(state: State<'_, AppState>) -> ApiResult<Vec<meetko_core::plugin_source::GalleryEntry>> {
    let core = state.core.clone();
    let list = blocking(move || core.gallery_list(&meetko_core::plugin_source::index_url())).await?;
    lock(&state).gallery_index = list.clone();
    Ok(list)
}

/// `id` names a row of the index fetched above; the repo and commit come from that row.
#[tauri::command]
async fn gallery_preview(state: State<'_, AppState>, id: String) -> ApiResult<api::PluginPreview> {
    let entry = {
        let mut jobs = lock(&state);
        jobs.pending_gallery.clear();
        jobs.gallery_index.iter().find(|e| e.id == id).cloned()
    };
    let entry = entry.ok_or_else(|| ApiError::new("not_found", "that plugin is not in the list; load the list again"))?;
    let core = state.core.clone();
    let (fetched, mut preview) = blocking(move || core.gallery_preview(&entry, &meetko_core::plugin_source::Endpoints::from_env()?)).await?;
    preview.token = Some(lock(&state).pending_gallery.set(fetched));
    Ok(preview)
}

#[tauri::command]
async fn gallery_install(state: State<'_, AppState>, token: String, code_sha256: String, replace: Option<String>) -> ApiResult<api::PluginPreview> {
    let fetched = lock(&state).pending_gallery.take(&token)?;
    let core = state.core.clone();
    blocking(move || core.gallery_install(&fetched, &code_sha256, replace.as_deref())).await
}

// ---------------------------------------------------------------- model setup

/// Payload of the `model-setup` event.
#[derive(Serialize, Clone)]
struct ModelEvent {
    key: String,
    /// download | import
    kind: &'static str,
    /// progress | done | error | cancelled
    state: &'static str,
    done: u64,
    total: u64,
    code: Option<&'static str>,
    message: Option<String>,
}

#[tauri::command]
async fn model_setup(state: State<'_, AppState>) -> ApiResult<api::ModelSetup> {
    let core = state.core.clone();
    let mut view = blocking(move || Ok(core.model_setup())).await?;
    view.job = lock(&state).model.as_ref().map(|(j, _)| j.clone());
    Ok(view)
}

fn offer_of(key: &str) -> ApiResult<&'static meetko_core::model_setup::Offer> {
    meetko_core::model_setup::offer(key).ok_or_else(|| ApiError::new("invalid_data", "no such model"))
}

/// Take the one model job slot, or say it is taken.
fn claim_model_job(state: &AppState, key: &str, kind: &'static str) -> ApiResult<Arc<AtomicBool>> {
    let mut jobs = lock(state);
    if jobs.model.is_some() {
        return Err(ApiError::new("busy", "a model is already being prepared"));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    jobs.model = Some((api::ModelJobView { key: key.to_string(), kind }, cancel.clone()));
    Ok(cancel)
}

/// Run a claimed model job on its own thread and report it through `model-setup` events.
/// `src` is the file the user picked (import), or None (download from the publisher).
fn run_model_job(app: AppHandle, core: Arc<Core>, o: &'static meetko_core::model_setup::Offer, src: Option<PathBuf>, cancel: Arc<AtomicBool>) {
    let kind = if src.is_some() { "import" } else { "download" };
    std::thread::spawn(move || {
        let _reset = Reset { app: app.clone(), f: |j| j.model = None };
        let ev = |state, done, total, code, message| ModelEvent { key: o.key.into(), kind, state, done, total, code, message };
        // progress at most every 1/200 of the file, so the webview is not flooded
        let step = (o.size / 200).max(1);
        let last = std::sync::atomic::AtomicU64::new(0);
        let progress = |done: u64, total: u64| {
            let prev = last.load(Ordering::Relaxed);
            if done == total || done >= prev + step || done < prev {
                last.store(done, Ordering::Relaxed);
                let _ = app.emit("model-setup", ev("progress", done, total, None, None));
            }
        };
        let _ = app.emit("model-setup", ev("progress", 0, o.size, None, None));
        let res = match &src {
            None => meetko_core::model_setup::install(o, &core.data_dir, &progress, &cancel),
            Some(p) => meetko_core::model_setup::install_from(o, &core.data_dir, p, &progress, &cancel),
        };
        let last = match res {
            Ok(()) => ev("done", o.size, o.size, None, None),
            Err(meetko_core::Error::Cancelled) => ev("cancelled", 0, o.size, None, None),
            Err(e) => ev("error", 0, o.size, Some(e.code()), Some(e.to_string())),
        };
        drop(_reset); // free the slot before the UI hears that the job is over
        let _ = app.emit("model-setup", last);
    });
}

/// Download a pinned model from its publisher. Only ever on the user's click; what, where, how big
/// and the license are on screen before it (`model_setup`).
#[tauri::command]
async fn model_download(app: AppHandle, state: State<'_, AppState>, key: String) -> ApiResult<()> {
    let o = offer_of(&key)?;
    let cancel = claim_model_job(&state, o.key, "download")?;
    run_model_job(app, state.core.clone(), o, None, cancel);
    Ok(())
}

/// Native file picker for a model file the user already has. The path stays in Rust (as in
/// `plugin_pick`); the file is copied into the app's folder only when it is the pinned model.
/// False when the user cancelled the dialog.
#[tauri::command]
async fn model_pick(app: AppHandle, window: WebviewWindow, state: State<'_, AppState>, key: String) -> ApiResult<bool> {
    let o = offer_of(&key)?;
    {
        let mut jobs = lock(&state);
        if jobs.model.is_some() || jobs.model_picking {
            return Err(ApiError::new("busy", "a model is already being prepared"));
        }
        jobs.model_picking = true;
    }
    let reset = Reset { app: app.clone(), f: |j| j.model_picking = false };
    let dialog = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || dialog.dialog().file().set_parent(&window).add_filter("GGUF", &["gguf"]).blocking_pick_file())
        .await
        .map_err(|e| ApiError::new("error", e.to_string()))?;
    drop(reset);
    let Some(path) = picked.and_then(|p| p.into_path().ok()) else { return Ok(false) };
    let cancel = claim_model_job(&state, o.key, "import")?;
    run_model_job(app, state.core.clone(), o, Some(path), cancel);
    Ok(true)
}

#[tauri::command]
async fn setup_card_dismiss(state: State<'_, AppState>) -> ApiResult<()> {
    let core = state.core.clone();
    blocking(move || core.dismiss_setup_card()).await
}

#[tauri::command]
async fn model_cancel(state: State<'_, AppState>) -> ApiResult<()> {
    if let Some((_, flag)) = &lock(&state).model {
        flag.store(true, Ordering::SeqCst);
    }
    Ok(())
}

// ---------------------------------------------------------------- capture

#[tauri::command]
async fn capture_start(app: AppHandle, state: State<'_, AppState>) -> ApiResult<()> {
    let id = {
        let mut jobs = lock(&state);
        if jobs.capture.is_some() || jobs.capture_starting {
            return Err(ApiError::new("busy", "a recording is already running"));
        }
        jobs.capture_starting = true;
        jobs.capture_gen += 1;
        jobs.capture_gen
    };
    let _reset = Reset { app: app.clone(), f: |j| j.capture_starting = false };
    let core = state.core.clone();
    let emitter = app.clone();
    let session = blocking(move || {
        let bin = capture_run::resolve_bin(&core.data_dir)?;
        Ok(CaptureSession::start(&bin, &core.data_dir, move |ev| {
            // an event of a session that was stopped or replaced is stale
            let st = emitter.state::<AppState>();
            if lock(&st).capture_gen == id {
                let _ = emitter.emit("capture", ev);
            }
        })?)
    })
    .await?;
    // Live captions are best effort and never part of the start: no config, no captions.
    let live = LiveConfig::from_env().ok().map(|cfg| {
        let emitter = app.clone();
        LiveSession::start(
            cfg,
            session.out_dir(),
            Arc::new(move |u: LiveUpdate| {
                let st = emitter.state::<AppState>();
                if lock(&st).capture_gen == id {
                    let _ = emitter.emit("live", u);
                }
            }),
        )
    });
    {
        let mut jobs = lock(&state);
        jobs.capture = Some(session);
        jobs.live = live;
    }
    // a provider that dies on its own (permission error, lost stream) tells nobody: poll for it
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let state = app.state::<AppState>();
        let running = {
            let mut jobs = lock(&state);
            if jobs.capture_gen != id {
                return; // stopped, or a newer session owns the slot
            }
            jobs.capture.as_mut().map(CaptureSession::is_running)
        };
        match running {
            None => return,
            Some(false) => {
                let _ = app.emit("capture", json!({ "event": "exited" }));
                return;
            }
            Some(true) => {}
        }
    });
    Ok(())
}

/// Take the running recording out of the slots for a stop. A stop that arrives while the start
/// is still in flight is refused *before* anything changes: bumping `capture_gen` now would make
/// the starting session's own events and poller look stale and leave an orphan recording that
/// nobody can stop. The UI may ask again once the start has finished.
fn take_for_stop(jobs: &mut Jobs) -> ApiResult<(CaptureSession, Option<LiveSession>)> {
    if jobs.capture_starting {
        return Err(ApiError::new("busy", "the recording is still starting; stop it once it has started"));
    }
    let session = jobs.capture.take().ok_or_else(|| ApiError::new("not_found", "no recording is running"))?;
    jobs.capture_gen += 1; // late events of this session are dropped from here on
    Ok((session, jobs.live.take()))
}

#[tauri::command]
async fn capture_stop(app: AppHandle, state: State<'_, AppState>) -> ApiResult<api::CaptureDone> {
    let (session, live) = take_for_stop(&mut lock(&state))?;
    let core = state.core.clone();
    let done = blocking(move || {
        let outcome = session.stop(capture_run::STOP_TIMEOUT)?;
        // after the provider has stopped its files are complete: drain them, then import
        // (which moves the audio away from the provider's directory)
        let live_out = live.map(LiveSession::finish);
        Ok(core.finish_capture(&outcome, live_out.as_ref()))
    })
    .await?;
    // The live captions are the first draft: review, approval and plugins work without whisper.
    // Only for a meeting that has no draft yet (a re-imported recording may have one), and only when
    // the job slot is free; otherwise the meeting screen offers the same as a button.
    if let (Some(m), Some(_)) = (&done.meeting_id, &done.live_version_id) {
        let core = state.core.clone();
        let m2 = m.clone();
        if blocking(move || core.live_draft_source(&m2)).await.ok().flatten().is_some() {
            let _ = start_draft_job(&app, &state, m.clone(), DraftJob::FromLive);
        }
    }
    Ok(done)
}

// ---------------------------------------------------------------- meeting detection

fn watch_state(m: &meeting_watch::Shared) -> meeting_watch::WatchState {
    let s = m.settings();
    let offer = meeting_watch::OfferView::of(&m.watch());
    meeting_watch::WatchState { available: meeting_watch::AVAILABLE, enabled: s.enabled && meeting_watch::AVAILABLE, browsers: s.browsers, offer }
}

#[tauri::command]
async fn meeting_watch_get(state: State<'_, AppState>) -> ApiResult<meeting_watch::WatchState> {
    Ok(watch_state(&state.meeting))
}

#[tauri::command]
async fn meeting_watch_set(app: AppHandle, state: State<'_, AppState>, enabled: bool, browsers: bool) -> ApiResult<meeting_watch::WatchState> {
    let m = state.meeting.clone();
    let s = meeting_watch::Settings { enabled: enabled && meeting_watch::AVAILABLE, browsers };
    let dir = m.data_dir.clone();
    blocking(move || meeting_watch::save_settings(&dir, s).map_err(|e| ApiError::new("io", e.to_string()))).await?;
    *m.settings.lock().unwrap_or_else(|p| p.into_inner()) = s;
    let recording = lock(&state).capture.is_some();
    if !s.enabled {
        watch_step(&app, &m, |w, now| w.tick(now, false, false, &[]));
    } else if !s.browsers {
        watch_step(&app, &m, |w, _| w.forget(|a| a.browser, recording)); // a browser question goes at once
    }
    Ok(watch_state(&m))
}

/// The user answered the view with `seq`: `answer` is yes (녹음 시작 once the recording runs, or
/// 녹음 정지) | no (이번엔 안 함 / 계속 녹음) | hide (Escape). The webview itself starts or stops the
/// recording through the usual commands; this only records the answer.
#[tauri::command]
async fn meeting_prompt_answer(app: AppHandle, state: State<'_, AppState>, seq: u64, answer: String) -> ApiResult<()> {
    let answer = match answer.as_str() {
        "yes" => meeting_watch::Answer::Yes,
        "no" => meeting_watch::Answer::No,
        "hide" => meeting_watch::Answer::Hide,
        _ => return Err(ApiError::new("invalid_data", "answer must be yes, no or hide")),
    };
    let recording = lock(&state).capture.is_some();
    watch_step(&app, &state.meeting, |w, now| w.answer(now, seq, answer, recording));
    Ok(())
}

/// Run one change of the watch and tell the webview. The event goes out under the lock, so
/// views leave in the order the state changed (each also carries its `seq`).
fn watch_step(app: &AppHandle, m: &meeting_watch::Shared, f: impl FnOnce(&mut meeting_watch::Watch, u64) -> Option<meeting_watch::Offer>) {
    let offer = {
        let mut w = m.watch();
        let Some(offer) = f(&mut w, m.now()) else { return };
        let _ = app.emit("meeting-prompt", meeting_watch::OfferView::of(&w));
        offer
    };
    if offer != meeting_watch::Offer::None {
        // in the background: bounce the Dock icon once (no notification permission needed)
        if let Some(win) = app.get_webview_window("main") {
            if !win.is_focused().unwrap_or(true) {
                let _ = win.request_user_attention(Some(tauri::UserAttentionType::Informational));
            }
        }
    }
}

/// Looks at the microphone every few seconds, only while the setting is on.
fn spawn_meeting_watch(app: AppHandle) {
    if !meeting_watch::AVAILABLE {
        return;
    }
    let own = std::process::id() as i32;
    std::thread::spawn(move || loop {
        std::thread::sleep(meeting_watch::POLL);
        let state = app.state::<AppState>();
        let m = state.meeting.clone();
        let s = m.settings();
        let recording = {
            let jobs = lock(&state);
            // a start in flight is not a recording yet; the watch also waits a few seconds before it counts
            jobs.capture.is_some()
        };
        let hits = if s.enabled {
            // a failed query says nothing about the meeting: skip this poll rather than read it as "nobody"
            let Some(users) = meeting_watch::mic_users() else { continue };
            meeting_watch::pick(&users, own, s.browsers)
        } else {
            vec![]
        };
        watch_step(&app, &m, |w, now| w.tick(now, s.enabled, recording, &hits));
    });
}

// ---------------------------------------------------------------- audio protocol

/// `convertFileSrc` percent-encodes the whole path, so the separating slash arrives as `%2F`.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        let hex = (b[i] == b'%' && i + 2 < b.len()).then(|| std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())).flatten();
        match hex {
            Some(v) => (out.push(v), i += 3),
            None => (out.push(b[i]), i += 1),
        };
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn audio_response(core: &Core, uri_path: &str, range: Option<&str>) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::Response;
    // /<meeting id>/<channel | mix>; nothing else is accepted, so no path can be smuggled in
    let ok = |s: &str| !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b));
    let decoded = percent_decode(uri_path);
    let parts: Vec<&str> = decoded.trim_start_matches('/').split('/').collect();
    let fail = |code: u16| Response::builder().status(code).body(Vec::new()).expect("static response");
    let [meeting, which] = parts[..] else { return fail(404) };
    if !ok(meeting) || !ok(which) {
        return fail(404);
    }
    match core.audio(meeting, which, range) {
        Ok(r) => {
            let mut b = Response::builder()
                .status(r.status)
                .header("Content-Type", "audio/wav")
                .header("Accept-Ranges", "bytes")
                .header("Cache-Control", "no-store")
                .header("X-Content-Type-Options", "nosniff")
                .header("Content-Length", r.body.len().to_string());
            if let Some(cr) = r.content_range {
                b = b.header("Content-Range", cr);
            }
            b.body(r.body).unwrap_or_else(|_| fail(500))
        }
        Err(code) => fail(code),
    }
}

pub fn run() {
    let core = Arc::new(Core::from_env());
    let audio_core = core.clone();
    let meeting = Arc::new(meeting_watch::Shared::new(core.data_dir.clone()));
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState { core, jobs: Mutex::new(Jobs::default()), meeting })
        .setup(|app| {
            spawn_meeting_watch(app.handle().clone());
            Ok(())
        })
        .register_asynchronous_uri_scheme_protocol("meetko-audio", move |_ctx, req, responder| {
            let core = audio_core.clone();
            let path = req.uri().path().to_string();
            let range = req.headers().get("range").and_then(|v| v.to_str().ok()).map(String::from);
            tauri::async_runtime::spawn_blocking(move || responder.respond(audio_response(&core, &path, range.as_deref())));
        })
        .invoke_handler(tauri::generate_handler![
            status, list_meetings, get_meeting, get_version, resolve_anchor, approve, transcribe_start, live_draft_start, transcribe_cancel,
            capture_start, capture_stop, list_plugins, plugin_pick, plugin_add, plugin_run, list_results, list_stale,
            gallery_list, gallery_preview, gallery_install, export_version, export_save,
            meeting_watch_get, meeting_watch_set, meeting_prompt_answer,
            search, model_setup, model_download, model_pick, model_cancel, setup_card_dismiss
        ])
        .run(tauri::generate_context!())
        .expect("error while running the meetko app");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stop_during_start_is_refused_and_changes_nothing() {
        let mut jobs = Jobs { capture_starting: true, capture_gen: 7, ..Default::default() };
        let e = take_for_stop(&mut jobs).err().expect("refused");
        assert_eq!(e.code, "busy");
        assert_eq!(jobs.capture_gen, 7, "the starting session keeps its generation");
        assert!(jobs.capture_starting);
    }

    #[test]
    fn a_stop_with_nothing_running_is_not_found_and_keeps_the_generation() {
        let mut jobs = Jobs { capture_gen: 3, ..Default::default() };
        assert_eq!(take_for_stop(&mut jobs).err().expect("refused").code, "not_found");
        assert_eq!(jobs.capture_gen, 3);
    }

    /// A command registered in `generate_handler!` but missing from `build.rs` or the capability is
    /// refused at run time while every other test passes.
    #[test]
    fn every_handled_command_is_built_and_allowed() {
        let lib = include_str!("lib.rs");
        let start = lib.find(concat!("generate_handler", "![")).expect("handler list") + "generate_handler![".len();
        let handled: Vec<&str> = lib[start..][..lib[start..].find(']').expect("end of list")]
            .split(',').map(str::trim).filter(|c| !c.is_empty()).collect();
        assert!(handled.len() > 10, "{handled:?}");
        let build = include_str!("../build.rs");
        let caps = include_str!("../capabilities/default.json");
        for c in handled {
            assert!(build.contains(&format!("\"{c}\"")), "{c} is not in build.rs");
            assert!(caps.contains(&format!("\"allow-{}\"", c.replace('_', "-"))), "{c} is not allowed in capabilities/default.json");
        }
    }
}
