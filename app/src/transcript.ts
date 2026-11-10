// The transcript: turns -> words, with the playhead word and the paged follow.
// Follow design (own, see design-notes.md): the current word may sit anywhere in a band of the
// viewport (22%-62%) and nothing moves; when it leaves the band the view glides once so the word
// lands at 38%. A transcript that moves under the eyes is harder to read than one that holds
// still and pages.
import { h, mmss, mmssd } from "./dom";
import type { Model } from "./model";
import type { Kind, Player } from "./player";

const BAND_TOP = 0.22, BAND_BOTTOM = 0.62, TARGET = 0.38, GLIDE_MS = 240;

// cubic-bezier(0.4, 0, 0.6, 1) solved by Newton steps, for the glide
function bezier(x1: number, y1: number, x2: number, y2: number): (x: number) => number {
  const cx = 3 * x1, bx = 3 * (x2 - x1) - cx, ax = 1 - cx - bx, cy = 3 * y1, by = 3 * (y2 - y1) - cy, ay = 1 - cy - by;
  const sx = (t: number) => ((ax * t + bx) * t + cx) * t, sy = (t: number) => ((ay * t + by) * t + cy) * t;
  return (x) => {
    let t = x;
    for (let i = 0; i < 8; i++) { const e = sx(t) - x, d = (3 * ax * t + 2 * bx) * t + cx; if (Math.abs(e) < 1e-4 || !d) break; t -= e / d; }
    return sy(t);
  };
}
const ease = bezier(0.4, 0, 0.6, 1);

export interface TranscriptOpts {
  onFollowChange(follow: boolean, currentOutOfView: boolean): void;
  /** hovered/focused word index, -1 for none */
  onInfo(i: number): void;
}

export function buildTranscript(model: Model, player: Player, opts: TranscriptOpts) {
  const { words, turns } = model;
  const root = h("div", { class: "tr-scroll" });
  const col = h("div", { class: "tr-col", role: "group", "aria-label": "전사본. 좌우 화살표로 단어, 위아래 화살표로 턴 이동, Enter로 그 지점부터 재생" });
  root.append(col);
  const wordEls: HTMLElement[] = new Array(words.length);
  const turnEls: HTMLElement[] = [];
  const frag = document.createDocumentFragment();
  turns.forEach((t, ti) => {
    const status = model.turnStatus(t);
    const label = status === "unassigned" ? "화자 미지정" : model.speakerLabel(t.speaker) ?? t.speaker ?? "";
    const ts = h("button", { type: "button", class: "ts", text: mmss(t.start_ms), tabindex: "-1", "aria-label": `${label}, ${mmss(t.start_ms)}부터 재생`, data: { t: String(ti) } });
    const meta = h("div", { class: "turn-meta" },
      h("span", { class: "spk", text: label, title: label }),
      status === "ambiguous" ? h("span", { class: "spk-flag", text: "화자 불분명" }) : null, ts);
    const text = h("p", { class: "turn-text" });
    for (let i = t.i0; i <= t.i1; i++) {
      // one tab stop in all (roving tabindex below); the words are the keyboard path
      const el = h("span", { class: "w", text: words[i].text, data: { i: String(i) } });
      wordEls[i] = el;
      text.append(el, " ");
    }
    const turn = h("div", { class: `turn${model.isMe(t.speaker) ? " me" : ""}`, role: "group", "aria-label": `${label} ${mmss(t.start_ms)}`, data: { ch: String(words[t.i0].channel) } }, meta, text);
    turnEls.push(turn);
    frag.append(turn);
  });
  col.append(frag);
  wordEls[0]?.setAttribute("tabindex", "0");

  const st = { follow: true, cur: -1, curTurn: -1, rangeEls: [] as HTMLElement[], anim: 0, focusIdx: 0 };
  const reduced = () => matchMedia("(prefers-reduced-motion: reduce)").matches;

  function animateTo(target: number): void {
    cancelAnimationFrame(st.anim);
    const from = root.scrollTop, max = root.scrollHeight - root.clientHeight;
    target = Math.max(0, Math.min(max, target));
    // far jumps are instant, near ones glide
    if (reduced() || Math.abs(target - from) < 1 || Math.abs(target - from) > root.clientHeight * 2) { root.scrollTop = target; return; }
    const t0 = performance.now();
    const step = (now: number) => {
      const p = Math.min(1, (now - t0) / GLIDE_MS);
      root.scrollTop = from + (target - from) * ease(p);
      if (p < 1) st.anim = requestAnimationFrame(step);
    };
    st.anim = requestAnimationFrame(step);
  }
  const relY = (el: HTMLElement) => el.getBoundingClientRect().top - root.getBoundingClientRect().top;
  const inView = (el: HTMLElement) => { const y = relY(el); return y >= 0 && y + 20 <= root.clientHeight; };

  function revealWord(i: number, force = false): void {
    const el = wordEls[i];
    if (!el) return;
    const H = root.clientHeight;
    let y = relY(el);
    const turn = turnEls[model.turnOf[i]];
    if (turn && (y < -H * 3 || y > H * 4)) {
      // far away + content-visibility: jump near it first, then refine from measured geometry
      root.scrollTop = turn.offsetTop - H * TARGET;
      y = relY(el);
      // content-visibility turns settle their real height after the jump: re-centre twice
      const refine = () => { const d = relY(el) - H * TARGET; if (Math.abs(d) > 4) root.scrollTop += d; };
      requestAnimationFrame(() => { refine(); requestAnimationFrame(refine); });
    }
    if (force || y < H * BAND_TOP || y + 27 > H * BAND_BOTTOM) animateTo(root.scrollTop + y - H * TARGET);
  }

  const outOfView = () => st.cur >= 0 && !!wordEls[st.cur] && !inView(wordEls[st.cur]);
  const updateFollowUI = () => opts.onFollowChange(st.follow, outOfView());
  function setFollow(v: boolean): void { if (st.follow !== v) { st.follow = v; updateFollowUI(); } }

  function setCurrent(i: number): void {
    if (i === st.cur) return;
    if (st.cur >= 0) {
      // the 100 ms fade lives only on the two words involved: a transition on all 20k words drops frames while scrolling
      const o = wordEls[st.cur];
      o.classList.add("fade"); o.classList.remove("cur");
      setTimeout(() => o.classList.remove("fade"), 160);
    }
    if (st.curTurn >= 0) turnEls[st.curTurn]?.classList.remove("cur");
    st.cur = i;
    st.curTurn = i >= 0 ? model.turnOf[i] : -1;
    if (i >= 0) { wordEls[i].classList.add("cur"); turnEls[st.curTurn]?.classList.add("cur"); }
  }
  function onTime(): void {
    const i = model.wordAt(player.t);
    const changed = i !== st.cur;
    setCurrent(i);
    if (changed && i >= 0 && st.follow && player.playing) revealWord(i);
  }

  function applyRange(): void {
    for (const el of st.rangeEls) el.classList.remove("ev");
    st.rangeEls = [];
    const r = player.range;
    if (!r) return;
    for (let i = r.i0; i <= r.i1; i++) { wordEls[i]?.classList.add("ev"); st.rangeEls.push(wordEls[i]); }
    setFollow(true);
    const first = wordEls[r.i0];
    if (first) revealWord(r.i0, !inView(first) || relY(first) > root.clientHeight * BAND_BOTTOM);
    updateFollowUI();
  }
  /** The words of the channel that is not being played are muted, not hidden. */
  function applyChannel(ch: number | null): void {
    for (const t of turnEls) {
      if (ch === null || t.dataset.ch === String(ch)) t.removeAttribute("data-muted"); else t.dataset.muted = "1";
    }
  }

  const off = player.on((kind: Kind) => {
    if (kind === "time") onTime();
    else if (kind === "seek") { setFollow(true); onTime(); if (st.cur >= 0) revealWord(st.cur); updateFollowUI(); }
    else if (kind === "range") applyRange();
    else if (kind === "state") updateFollowUI();
  });

  // ---- only real user input pauses follow; programmatic scrolls never do
  const pause = () => { st.follow = false; cancelAnimationFrame(st.anim); updateFollowUI(); };
  root.addEventListener("wheel", pause, { passive: true });
  root.addEventListener("touchmove", pause, { passive: true });
  root.addEventListener("pointerdown", (e) => { if (e.target === root && e.offsetX > root.clientWidth) pause(); }); // scrollbar grab
  root.addEventListener("scroll", () => updateFollowUI(), { passive: true });

  // ---- pointer: one listener for all words (20k words, no per-word handlers)
  const wordOf = (e: Event): number => {
    const el = (e.target as HTMLElement).closest?.(".w") as HTMLElement | null;
    return el ? Number(el.dataset.i) : -1;
  };
  root.addEventListener("click", (e) => {
    const i = wordOf(e);
    if (i >= 0) { setStop(i); player.seek(words[i].start_ms, { play: true }); return; }
    const ts = (e.target as HTMLElement).closest?.(".ts") as HTMLElement | null;
    if (ts) player.seek(turns[Number(ts.dataset.t)].start_ms, { play: true });
  });
  const info = (e: Event) => opts.onInfo(wordOf(e));
  root.addEventListener("mouseover", (e) => { const i = wordOf(e); if (i >= 0) opts.onInfo(i); });
  root.addEventListener("mouseleave", () => opts.onInfo(-1));
  root.addEventListener("focusin", info);
  root.addEventListener("focusout", () => opts.onInfo(-1));

  // ---- keyboard: the words are a roving-tabindex group
  function setStop(i: number): void {
    if (i !== st.focusIdx) wordEls[st.focusIdx]?.removeAttribute("tabindex");
    wordEls[i].setAttribute("tabindex", "0");
    st.focusIdx = i;
  }
  function focusWord(i: number): void {
    const to = Math.max(0, Math.min(words.length - 1, i));
    setStop(to);
    wordEls[to].focus({ preventScroll: true });
    const H = root.clientHeight, y = relY(wordEls[to]);
    if (y < 40 || y + 40 > H) root.scrollTop += y - H * 0.4; // keyboard reveal: instant, and not a user scroll
  }
  root.addEventListener("keydown", (e) => {
    if (["PageUp", "PageDown", "Home", "End"].includes(e.key)) pause();
    const i = wordOf(e);
    if (i < 0) return;
    let to: number | null = null;
    if (e.key === "ArrowRight") to = i + 1;
    else if (e.key === "ArrowLeft") to = i - 1;
    else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      const t = turns[model.turnOf[i] + (e.key === "ArrowDown" ? 1 : -1)];
      if (t) to = t.i0;
    } else if (e.key === "Enter") { e.preventDefault(); player.seek(words[i].start_ms, { play: true }); return; }
    if (to !== null) { e.preventDefault(); focusWord(to); }
  });

  return {
    root, col, wordEls, turnEls, applyChannel,
    /** Make word `i` the tab stop and focus it, without scrolling (a search jump reveals it itself). */
    focusWord(i: number): void { if (!wordEls[i]) return; setStop(i); wordEls[i].focus({ preventScroll: true }); },
    resume(): void { setFollow(true); revealWord(st.cur >= 0 ? st.cur : 0, true); updateFollowUI(); },
    destroy(): void { off(); cancelAnimationFrame(st.anim); },
  };
}

/** The footer line for a hovered/focused word: timing and the engine's own caveats. */
export function wordInfoText(model: Model, i: number): string {
  const w = model.words[i];
  if (!w) return "";
  const parts = [`${mmssd(w.start_ms)}${w.end_ms > w.start_ms ? "–" + mmssd(w.end_ms) : ""} · ${model.chanLabel(w.channel)}`];
  if (w.p !== undefined) parts.push(`신뢰도 ${Math.round(w.p * 100)}%`);
  if (w.end_estimated) parts.push("끝 시각 추정");
  if (w.speaker_status === "ambiguous") parts.push("화자 불분명");
  if (w.speaker_status === "unassigned") parts.push("화자 미지정");
  return parts.join(" · ");
}
