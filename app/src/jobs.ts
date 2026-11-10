// Long jobs outlive the screen that started them, so their state lives here, fed by the
// backend events once at startup. Views subscribe while they are on screen.
import { onCapture, onLive, onPluginRun, onTranscribe, type CaptureEvent, type JobEvent, type LiveEvent } from "./api";

type Listener<T> = (e: T) => void;
function channel<T>() {
  const ls = new Set<Listener<T>>();
  return { on: (f: Listener<T>) => { ls.add(f); return () => ls.delete(f); }, emit: (e: T) => ls.forEach((f) => f(e)) };
}

export const transcribeBus = channel<JobEvent>();
export const pluginBus = channel<JobEvent>();
export const captureBus = channel<CaptureEvent>();
export const liveBus = channel<LiveEvent>();

/** meeting id -> last progress, for a transcription that is running right now */
export const transcribing = new Map<string, JobEvent>();
/** the plugin run in flight (one at a time) and the version it reads */
export const pluginRunning = { id: null as string | null, versionId: null as string | null };

export async function initJobs(): Promise<void> {
  await onTranscribe((e) => {
    const id = e.meeting_id ?? "";
    if (e.state === "progress") transcribing.set(id, e); else transcribing.delete(id);
    transcribeBus.emit(e);
  });
  await onPluginRun((e) => { pluginRunning.id = null; pluginRunning.versionId = null; pluginBus.emit(e); });
  await onCapture((e) => captureBus.emit(e));
  await onLive((e) => liveBus.emit(e));
}
