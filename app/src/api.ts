// Typed wrappers over the Tauri commands. This is the only file that calls `invoke`.
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface ApiError { code: string; message: string }

export interface Setup { ok: boolean; path: string | null; code: string | null; detail: string | null }
export interface Status { dev_build: boolean; model: Setup; whisper_cli: Setup; capture: Setup; live: Setup }

/** 받아쓰기 (batch) needs both the model and the whisper-cli engine; every screen gates on this one check. */
export const whisperReady = (s: Status): boolean => s.model.ok && s.whisper_cli.ok;
/** What 받아쓰기 is missing, as a noun for "…이 있어야": the model first, else the engine; null when ready. */
export const whisperNeed = (s: Status): string | null => (!s.model.ok ? "전사 모델" : !s.whisper_cli.ok ? "전사 엔진(whisper-cli)" : null);

export type Kind = "live" | "draft" | "approved";

export interface MeetingSummary {
  id: string; title: string | null; started_at: string | null; created_at: string;
  duration_ms: number; latest_version_id: string | null; latest_kind: Kind | null; stale_count: number;
}
export interface ChannelView { index: number; role: "mic" | "loopback" | string; label: string | null; sha256: string }
export interface VersionView { id: string; kind: Kind; parent: string | null; created_at: string }
export interface MeetingDetail {
  id: string; title: string | null; started_at: string | null; duration_ms: number;
  channels: ChannelView[]; versions: VersionView[];
}

// meeting-data (contract 0.1), only the parts the UI reads
export interface Word { id: string; channel: number; start_ms: number; end_ms: number; text: string; speaker: string | null; speaker_status: string; end_estimated?: boolean; p?: number }
export interface Turn { id: string; speaker: string | null; first_word: string; last_word: string; start_ms: number; end_ms: number }
export interface Speaker { id: string; channel: number; basis: string; label?: string }
export interface Producer {
  role: string; engine: { id: string; version: string }; model?: { id: string; sha256?: string };
  channels?: number[]; params?: Record<string, unknown>; word_timing?: string; granularity?: string; network: boolean;
  [k: string]: unknown;
}
export interface MeetingData {
  meeting: { id: string; title?: string; duration_ms: number; channels: { index: number; role: string; label?: string }[] };
  recording: { sha256: string; sample_rate: number; duration_ms: number };
  version: { id: string; kind: Kind; parent?: string; created_at?: string; producers: Producer[] };
  coverage: { channel: number; start_ms: number; end_ms: number }[];
  speakers: Speaker[]; words: Word[]; turns: Turn[];
}

export interface Anchor { from: string; to: string }
export interface Resolved { channels: number[]; start_ms: number; end_ms: number }

export interface PluginInfo {
  id: string; version: string; description: string; network: boolean; audio: string;
  display: string[]; code_sha256: string; manifest_sha256: string; folder: string; notice: string;
  /** true when this build runs plugins in the OS sandbox (macOS) */
  isolated: boolean;
  /** "github:OWNER/REPO @ ref (commit)" for a plugin that came from the network, else null */
  source: string | null;
  /** set when adding would overwrite an installed plugin with the same id from a different source */
  replaces: { key: string; version: string; source: string | null } | null;
  /** gallery previews only: installing needs this token back */
  token: string | null;
}
/** One row of the gallery index: what the repo declares, not a review. */
export interface GalleryEntry {
  id: string; name: string; version: string; description: string; repo: string; ref: string; sha: string;
  permissions: { network: boolean; audio: boolean }; runtime: string; license: string; stars: number; updated: string;
}
export interface ResultView {
  id: number; plugin_id: string; plugin_version: string; version_id: string; created_at: string; stale: boolean;
  output: { items: PluginItem[] }; plugin_code_sha256: string; plugin_manifest_sha256: string;
  declared_network: boolean; declared_audio: string;
  /** isolation the run had: macos-sbpl | none | off | unrecorded */
  sandbox: string;
}
export interface Evidenced { text: string; evidence: Anchor[] }
export type PluginItem =
  | { type: "action_items"; items: (Evidenced & { owner?: string | null; due?: string })[] }
  | { type: "summary_card"; title: string; points: Evidenced[] }
  | { type: "timeline_marker"; kind: string; label: string; at: Anchor }
  | { type: "table"; title?: string; columns: string[]; rows: { cells: string[]; evidence?: Anchor[] }[] }
  | { type: "transcript_block"; title?: string; anchor: Anchor; note?: string };

/** One place a search matched: the text around it, and the words it covers in `version_id`. */
export interface SearchHit { before: string; text: string; after: string; from: string; to: string; start_ms: number; end_ms: number }
export interface SearchResult {
  meeting_id: string; title: string | null; started_at: string | null; created_at: string;
  version_id: string; kind: Kind; matches: number; hits: SearchHit[];
}
/** `more`: more meetings matched than were returned; `pending`: not indexed yet; `skipped`: unreadable, left out */
export interface SearchOutcome { meetings: SearchResult[]; more: boolean; pending: number; skipped: number }

export interface ProviderError { code: string; message: string; recoverable: boolean }
export interface CaptureDone {
  meeting_id: string | null; existing: boolean; warnings: string[]; provider_errors: ProviderError[]; error: string | null;
  live_version_id: string | null; live_note: "live_unavailable" | "live_not_stored" | null;
}
/** Live captions: `channel` is the capture channel (0 = mic, 1 = loopback). A later revision of a segment replaces the earlier one. */
export type LiveEvent =
  | { event: "caption"; channel: number; segment: number; revision: number; final: boolean; start_ms: number; text: string; emitted_at_ms: number }
  | { event: "error"; channel: number; message: string };
export type CaptureEvent =
  | { event: "started"; sample_rate: number | null; started_at: string | null }
  | { event: "level"; channel: number; rms: number; at_ms: number }
  | { event: "gap"; channel: number; start_ms: number; end_ms: number; reason: string; kind: string }
  | { event: "error"; code: string; message: string; recoverable: boolean }
  | { event: "stopped" }
  | { event: "exited" };

export interface JobEvent {
  state: "progress" | "done" | "error" | "cancelled";
  meeting_id: string | null; version_id: string | null; plugin_id: string | null; result_id: number | null;
  channel: number; channels: number; percent: number; diarizing?: boolean;
  /** the job makes the first draft from the live captions, not a batch transcription */
  from_live?: boolean; code: string | null; message: string | null;
}

/** Meeting detection (macOS): `offer` is what the app is asking right now. */
export interface MeetingOffer { offer: "none" | "start" | "end"; app: string | null; browser: boolean; seq: number }
export interface MeetingWatch { available: boolean; enabled: boolean; browsers: boolean; offer: MeetingOffer }

/** A model the app can set up (no paths: the page learns the state, not where files live). */
export type ModelKey = "stream" | "diarization";
export interface ModelPiece {
  key: ModelKey;
  /** bad: a file is there but it is not the pinned model (or the environment variable points at something unusable) */
  state: "ready" | "missing" | "bad";
  /** the environment names this model; the app leaves it alone */
  from_env: boolean;
  /** the engine that loads it was found */
  engine: boolean;
  /** the engine is named by its environment variable (a developer setup), found or not */
  engine_from_env: boolean;
  size: number; partial: number; license: string; publisher: string; repo: string; host: string; detail: string | null;
}
export interface ModelSetup { dev_build: boolean; pieces: ModelPiece[]; job: { key: ModelKey; kind: "download" | "import" } | null; card_dismissed: boolean }
export interface ModelEvent {
  key: ModelKey; kind: "download" | "import"; state: "progress" | "done" | "error" | "cancelled";
  done: number; total: number; code: string | null; message: string | null;
}

export const api = {
  status: () => invoke<Status>("status"),
  listMeetings: () => invoke<MeetingSummary[]>("list_meetings"),
  getMeeting: (meetingId: string) => invoke<MeetingDetail>("get_meeting", { meetingId }),
  getVersion: (versionId: string) => invoke<MeetingData>("get_version", { versionId }),
  resolve: (versionId: string, a: Anchor) => invoke<Resolved>("resolve_anchor", { versionId, from: a.from, to: a.to }),
  approve: (versionId: string) => invoke<string>("approve", { versionId }),
  transcribeStart: (meetingId: string) => invoke<void>("transcribe_start", { meetingId }),
  /** the first draft from the meeting's live captions (no whisper needed); same events as transcribeStart */
  liveDraftStart: (meetingId: string) => invoke<void>("live_draft_start", { meetingId }),
  transcribeCancel: () => invoke<void>("transcribe_cancel"),
  captureStart: () => invoke<void>("capture_start"),
  captureStop: () => invoke<CaptureDone>("capture_stop"),
  listPlugins: () => invoke<PluginInfo[]>("list_plugins"),
  pluginPick: () => invoke<PluginInfo | null>("plugin_pick"),
  pluginAdd: (codeSha256: string, replace: string | null) => invoke<PluginInfo>("plugin_add", { codeSha256, replace }),
  /** each of these three goes to the network, and only when the user clicks */
  galleryList: () => invoke<GalleryEntry[]>("gallery_list"),
  /** the entry is looked up by id in the list the app itself fetched; the page never names a repo or commit */
  galleryPreview: (id: string) => invoke<PluginInfo>("gallery_preview", { id }),
  galleryInstall: (token: string, codeSha256: string, replace: string | null) => invoke<PluginInfo>("gallery_install", { token, codeSha256, replace }),
  pluginRun: (pluginId: string, versionId: string) => invoke<void>("plugin_run", { pluginId, versionId }),
  listResults: (versionId: string) => invoke<ResultView[]>("list_results", { versionId }),
  listStale: (meetingId: string) => invoke<ResultView[]>("list_stale", { meetingId }),
  /** the text, for the clipboard; `title` is the one the toolbar shows */
  exportVersion: (versionId: string, format: ExportFormat, title: string) => invoke<string>("export_version", { versionId, format, title }),
  /** opens the native save dialog and writes there; the saved file's name, or null when cancelled */
  exportSave: (versionId: string, format: ExportFormat, title: string) => invoke<string | null>("export_save", { versionId, format, title }),
  meetingWatchGet: () => invoke<MeetingWatch>("meeting_watch_get"),
  meetingWatchSet: (enabled: boolean, browsers: boolean) => invoke<MeetingWatch>("meeting_watch_set", { enabled, browsers }),
  /** records the answer only; starting or stopping goes through captureStart / captureStop */
  meetingPromptAnswer: (seq: number, answer: "yes" | "no" | "hide") => invoke<void>("meeting_prompt_answer", { seq, answer }),
  /** literal text, never query syntax; the core cuts long input */
  search: (query: string) => invoke<SearchOutcome>("search", { query }),
  modelSetup: () => invoke<ModelSetup>("model_setup"),
  /** goes to the network (the publisher on Hugging Face), only when the user clicks; progress comes as model-setup events */
  modelDownload: (key: ModelKey) => invoke<void>("model_download", { key }),
  /** opens the native file picker; false when the user cancelled. The path stays in Rust */
  modelPick: (key: ModelKey) => invoke<boolean>("model_pick", { key }),
  modelCancel: () => invoke<void>("model_cancel"),
  /** 나중에 on the setup card, remembered in the data folder */
  setupCardDismiss: () => invoke<void>("setup_card_dismiss"),
};

/** md, txt (times and speakers), srt, vtt are files; plain and speakers are copy variants */
export type ExportFormat = "md" | "txt" | "srt" | "vtt" | "plain" | "speakers";

export const onTranscribe = (f: (e: JobEvent) => void): Promise<UnlistenFn> => listen<JobEvent>("transcribe", (e) => f(e.payload));
export const onPluginRun = (f: (e: JobEvent) => void): Promise<UnlistenFn> => listen<JobEvent>("plugin-run", (e) => f(e.payload));
export const onLive = (f: (e: LiveEvent) => void): Promise<UnlistenFn> => listen<LiveEvent>("live", (e) => f(e.payload));
export const onMeetingPrompt = (f: (e: MeetingOffer) => void): Promise<UnlistenFn> => listen<MeetingOffer>("meeting-prompt", (e) => f(e.payload));
export const onModelSetup = (f: (e: ModelEvent) => void): Promise<UnlistenFn> => listen<ModelEvent>("model-setup", (e) => f(e.payload));
export const onCapture = (f: (e: CaptureEvent) => void): Promise<UnlistenFn> => listen<CaptureEvent>("capture", (e) => f(e.payload));

/** `which`: "0" | "1" (a channel) or "mix". Served by the app's own protocol; see src-tauri. */
export const audioUrl = (meetingId: string, which: string): string => convertFileSrc(`${meetingId}/${which}`, "meetko-audio");

/** Tauri rejects with the serialized ApiError; anything else is made into one. */
export function asError(e: unknown): ApiError {
  if (e && typeof e === "object" && "message" in e) return e as ApiError;
  return { code: "error", message: String(e) };
}
