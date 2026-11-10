// Playback for the meeting view: one <audio> element fed by the app's own `meetko-audio`
// protocol (a single channel, or a server-side sum of both so "both" cannot drift apart).
// Nothing is decoded in JS.
//
// The playhead (`t`) is interpolated: `timeupdate` fires only ~4 times a second, which is too
// coarse for a word highlight or for stopping an evidence span on its last word. So every
// timeupdate re-bases a clock on audio.currentTime, and requestAnimationFrame advances it in
// between. While the audio is stalled (`waiting`) the clock follows currentTime instead.
import { audioUrl } from "./api";

export type Channel = "both" | "me" | "other";
export type Kind = "time" | "seek" | "state" | "range" | "channel";
export interface Span { i0: number; i1: number }

/** Let the last word finish: word end times are often estimated. */
const SPAN_TAIL_MS = 300;

export class Player {
  readonly audio = new Audio();
  t = 0;
  channel: Channel = "both";
  range: Span | null = null;
  private stopAt: number | null = null;
  private base = { ms: 0, at: 0 };
  private stalled = false;
  private dead = false; // set by destroy(): an in-flight setChannel / playRange must not touch the released element
  private raf = 0;
  private pending: number | null = null; // a seek asked for before the metadata arrived
  private subs = new Set<(k: Kind) => void>();

  /** `sources` maps a channel choice to the url variant ("mix", "0", "1"); a missing key means the meeting lacks that channel. */
  constructor(private meetingId: string, readonly dur: number, private sources: Partial<Record<Channel, string>>) {
    const a = this.audio;
    a.preload = "metadata";
    a.src = audioUrl(meetingId, sources.both ?? "mix");
    a.addEventListener("timeupdate", () => {
      this.rebase();
      this.t = Math.min(this.dur, a.currentTime * 1000);
      this.step();
      if (!this.playing) this.emit("time");
    });
    a.addEventListener("waiting", () => { this.stalled = true; });
    a.addEventListener("playing", () => { this.stalled = false; this.rebase(); this.onState(); });
    for (const ev of ["play", "pause", "ended"]) a.addEventListener(ev, () => this.onState());
    a.addEventListener("loadedmetadata", () => { if (this.pending !== null) { a.currentTime = this.pending / 1000; this.pending = null; } });
  }

  get playing(): boolean { return !this.audio.paused; }
  has(c: Channel): boolean { return this.sources[c] !== undefined; }
  /** "mix", or the index of the single channel behind "me" / "other" */
  sourceOf(c: Channel): string | undefined { return this.sources[c]; }
  on(f: (k: Kind) => void): () => void { this.subs.add(f); return () => this.subs.delete(f); }
  private emit(k: Kind): void { for (const f of this.subs) f(k); }

  private rebase(): void { this.base = { ms: this.audio.currentTime * 1000, at: performance.now() }; }

  private onState(): void {
    cancelAnimationFrame(this.raf);
    this.t = Math.min(this.dur, this.audio.currentTime * 1000);
    this.rebase();
    if (this.playing) this.raf = requestAnimationFrame(this.frame);
    this.emit("time"); this.emit("state");
  }

  private frame = (): void => {
    if (!this.playing) return;
    this.t = Math.min(this.dur, this.stalled ? this.audio.currentTime * 1000 : this.base.ms + (performance.now() - this.base.at));
    this.step();
    this.emit("time");
    if (this.playing) this.raf = requestAnimationFrame(this.frame);
  };

  /** An evidence span ends the moment the clock passes it. */
  private step(): void {
    if (this.stopAt !== null && this.t >= this.stopAt) {
      this.t = this.stopAt;
      this.stopAt = null;
      this.audio.pause(); // the pause event publishes the final state
    }
  }

  private async start(): Promise<void> {
    if (this.dead) return;
    try { await this.audio.play(); } catch { /* autoplay refused, or a newer load replaced this one */ }
  }

  toggle(): void {
    if (this.playing) { this.audio.pause(); return; }
    if (this.t >= this.dur - 50) this.seek(0);
    this.stopAt = null;
    void this.start();
  }

  seek(ms: number, opts: { play?: boolean; keepRange?: boolean } = {}): void {
    this.t = Math.max(0, Math.min(this.dur, ms));
    this.stopAt = null;
    if (this.range && !opts.keepRange) { this.range = null; this.emit("range"); }
    if (this.audio.readyState >= 1) this.audio.currentTime = this.t / 1000; else this.pending = this.t;
    this.rebase();
    this.emit("seek"); this.emit("time");
    if (opts.play) void this.start();
  }

  /** Switch channel. Keeps the position and the play state. */
  async setChannel(c: Channel): Promise<void> {
    const src = this.sources[c];
    if (src === undefined || c === this.channel) return;
    const at = this.t, wasPlaying = this.playing;
    this.audio.pause(); // nothing keeps playing at the old position while the new file loads
    this.channel = c;
    // an error also settles it, so a file that fails to load cannot hang the caller
    const loaded = new Promise<void>((res) => {
      for (const ev of ["loadedmetadata", "error"]) this.audio.addEventListener(ev, () => res(), { once: true });
    });
    this.audio.src = audioUrl(this.meetingId, src);
    await loaded;
    if (this.dead) return;
    this.audio.currentTime = at / 1000;
    this.rebase();
    this.emit("channel");
    if (wasPlaying) void this.start();
  }

  /** Evidence: jump to the span, highlight its words, play it, stop at its end. */
  async playRange(startMs: number, endMs: number, span: Span, channel: Channel = this.channel): Promise<void> {
    this.audio.pause();
    await this.setChannel(this.has(channel) ? channel : "both");
    if (this.dead) return;
    this.seek(startMs, { keepRange: true });
    this.range = span; this.emit("range");
    this.stopAt = Math.min(this.dur, endMs + SPAN_TAIL_MS);
    await this.start();
  }

  /** Search: move to the span and highlight its words, without playing. */
  showRange(startMs: number, span: Span): void {
    this.seek(startMs, { keepRange: true });
    this.range = span; this.emit("range");
  }

  destroy(): void {
    this.dead = true;
    cancelAnimationFrame(this.raf);
    this.subs.clear();
    this.audio.pause();
    this.audio.removeAttribute("src");
    this.audio.load();
  }
}
