// A jump handed from one screen to the meeting screen: "open this meeting at these words".
// Kept in memory, not in the URL, so the route stays `#/meeting/<id>`; the meeting screen takes
// it once, when it has loaded that meeting.
import type { Anchor } from "./api";

export interface Jump { meetingId: string; versionId: string; anchor: Anchor }

let pending: Jump | null = null;

export function setJump(j: Jump): void { pending = j; }

/** The jump for `meetingId`, if one is waiting; it is cleared either way. */
export function takeJump(meetingId: string): Jump | null {
  const j = pending;
  pending = null;
  return j && j.meetingId === meetingId ? j : null;
}
