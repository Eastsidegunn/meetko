// Turns the contract's meeting-data into the indexes the views need. No DOM here.
import type { Anchor, MeetingData, ChannelView, Overlay, Producer, Turn, Word } from "./api";

/** Holes shorter than this are rounding between two whisper segments, not a gap in the recording. */
const MIN_GAP_MS = 250;

export interface TurnIx extends Turn { i0: number; i1: number }
export type Resolved = { ok: true; i0: number; i1: number; start: number; end: number } | { ok: false };

export interface Model {
  md: MeetingData; words: Word[]; turns: TurnIx[]; dur: number;
  /** the text shown for word `i`: the 용어 사전 replacements applied (the stored word is unchanged); "" when a replacement removed it.
   *  Inside an edited turn the words are not shown (`turnText` is) */
  text(i: number): string;
  /** turn index of each word, -1 when no turn covers it */
  turnOf: Int32Array;
  /** per channel index: the stretches that were NOT recorded */
  gaps: Map<number, [number, number][]>;
  wordAt(ms: number): number;
  resolve(a: Anchor): Resolved;
  /** the name the speaker is shown with: the one the user gave (or the one of the speaker it was merged into), else its label */
  speakerLabel(id: string | null): string | null;
  /** the label the transcript itself gave the speaker */
  speakerOriginal(id: string | null): string | null;
  /** the speaker `id` is shown as (itself, or the one it was merged into) */
  speakerTarget(id: string): string;
  /** the user's names and edits (replaced after each change) */
  overlay: Overlay;
  /** the user edited this turn's text: it is shown instead of the words */
  edited(t: Turn): boolean;
  /** the turn's text as shown (the core's `overlay::display_text`): the edit as typed, else the shown words (`text`) */
  turnText(t: TurnIx): string;
  /** the channel a speaker was recorded on is the mic: "me" */
  isMe(speaker: string | null): boolean;
  turnStatus(t: TurnIx): "assigned" | "ambiguous" | "unassigned";
  chanLabel(index: number): string;
  flags: { tokenTiming: boolean; network: Producer[]; estimated: number };
}

export const roleLabel = (role: string, label?: string | null): string => label ?? (role === "mic" ? "나" : role === "loopback" ? "상대" : role);

const NO_OVERLAY: Overlay = { speakers: {}, edits: {} };

/** `overlay`: the user's names and edits. `display`: the shown text of every word (`vocabularyDisplay`),
 *  or null to show the stored text. An edited turn shows its edit, never the replaced words. */
export function prepare(md: MeetingData, channels: ChannelView[], overlay: Overlay = NO_OVERLAY, display: string[] | null = null): Model {
  const words = md.words;
  const shown = display && display.length === words.length ? display : null;
  const idIndex = new Map(words.map((w, i) => [w.id, i]));
  const speakers = new Map(md.speakers.map((s) => [s.id, s]));
  // running max of start times: words are ordered by start, but one out of order must not break the search
  const starts = new Float64Array(words.length);
  let m = 0;
  words.forEach((w, i) => { m = Math.max(m, w.start_ms); starts[i] = m; });
  const turns: TurnIx[] = [];
  for (const t of md.turns) {
    const i0 = idIndex.get(t.first_word), i1 = idIndex.get(t.last_word);
    if (i0 !== undefined && i1 !== undefined && i0 <= i1) turns.push({ ...t, i0, i1 });
  }
  const turnOf = new Int32Array(words.length).fill(-1);
  turns.forEach((t, ti) => { for (let i = t.i0; i <= t.i1; i++) turnOf[i] = ti; });
  const dur = md.meeting.duration_ms;

  const gaps = new Map<number, [number, number][]>();
  for (const ch of md.meeting.channels) {
    const out: [number, number][] = [];
    let at = 0;
    for (const c of md.coverage.filter((c) => c.channel === ch.index).sort((a, b) => a.start_ms - b.start_ms)) {
      if (c.start_ms - at >= MIN_GAP_MS) out.push([at, c.start_ms]);
      at = Math.max(at, c.end_ms);
    }
    if (dur - at >= MIN_GAP_MS) out.push([at, dur]);
    gaps.set(ch.index, out);
  }

  const chanLabel = (index: number): string => {
    const c = channels.find((x) => x.index === index);
    return roleLabel(c?.role ?? "", md.meeting.channels.find((x) => x.index === index)?.label ?? c?.label);
  };
  const prods = md.version.producers;
  const model: Model = {
    md, words, turns, dur, turnOf, gaps, chanLabel, overlay,
    text: (i) => shown?.[i] ?? words[i]?.text ?? "",
    wordAt(ms) {
      let lo = 0, hi = words.length - 1, r = -1;
      while (lo <= hi) { const mid = (lo + hi) >> 1; if (starts[mid] <= ms) { r = mid; lo = mid + 1; } else hi = mid - 1; }
      return r;
    },
    resolve(a) { // time comes from the words, never from the plugin
      const i0 = idIndex.get(a.from), i1 = idIndex.get(a.to);
      if (i0 === undefined || i1 === undefined || i1 < i0) return { ok: false };
      return { ok: true, i0, i1, start: words[i0].start_ms, end: Math.max(words[i1].end_ms, words[i0].start_ms) };
    },
    speakerTarget(id) { // merges are kept one step deep by the core; a few steps at most here
      let cur = id;
      for (let i = 0; i < 8; i++) { const next = model.overlay.speakers[cur]?.merged_into; if (!next || next === cur || next === id) break; cur = next; }
      return cur;
    },
    speakerOriginal(id) { const s = speakers.get(id ?? ""); return s ? s.label ?? s.id : null; },
    speakerLabel(id) {
      if (!speakers.has(id ?? "")) return null;
      const t = model.speakerTarget(id!);
      return model.overlay.speakers[t]?.name ?? model.speakerOriginal(t) ?? t;
    },
    edited(t) { return Object.hasOwn(model.overlay.edits, t.id); },
    turnText(t) {
      if (model.edited(t)) return model.overlay.edits[t.id].text;
      const out: string[] = [];
      for (let i = t.i0; i <= t.i1; i++) { const s = model.text(i); if (s) out.push(s); }
      return out.join(" ");
    },
    isMe(id) {
      const s = speakers.get(id ?? "");
      return !!s && channels.find((c) => c.index === s.channel)?.role === "mic";
    },
    turnStatus(t) { // one stray unassigned word does not relabel a turn: flag it when half or more is unsure
      if (!t.speaker) return "unassigned";
      let bad = 0;
      for (let i = t.i0; i <= t.i1; i++) if (words[i].speaker_status !== "assigned") bad++;
      return bad * 2 >= t.i1 - t.i0 + 1 ? "ambiguous" : "assigned";
    },
    flags: {
      tokenTiming: prods.some((p) => p.word_timing === "token"),
      network: prods.filter((p) => p.network),
      estimated: words.filter((w) => w.end_estimated).length,
    },
  };
  return model;
}
