// The transcript: turns -> words, with the playhead word and the paged follow.
// Follow design (own, see design-notes.md): the current word may sit anywhere in a band of the
// viewport (22%-62%) and nothing moves; when it leaves the band the view glides once so the word
// lands at 38%. A transcript that moves under the eyes is harder to read than one that holds
// still and pages.
import { h, mmss, mmssd } from "./dom";
import type { Model, TurnIx } from "./model";
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
  /** the speaker of turn `ti` was chosen (click, or S on a word): open its menu at `anchor` */
  onSpeaker(ti: number, anchor: HTMLElement): void;
  /** store `text` as turn `ti`'s text; resolves to null when saved (the model's overlay is updated), else the message to show */
  saveEdit(ti: number, text: string): Promise<string | null>;
  /** "원래대로": back to the transcript's words; null when done, else the message to show */
  resetEdit(ti: number): Promise<string | null>;
  /** false while editing must wait (the edits on file could not be read: the field would start from the wrong text) */
  canEdit(): boolean;
}

const EDIT_HINT = "고친 문장은 단어 대신 문장 단위로 재생돼요.";

export function buildTranscript(model: Model, player: Player, opts: TranscriptOpts) {
  const { words, turns } = model;
  const root = h("div", { class: "tr-scroll" });
  const col = h("div", { class: "tr-col", role: "group", "aria-label": "전사본. 좌우 화살표로 단어, 위아래 화살표로 턴 이동, Enter로 그 지점부터 재생, E로 문장 고치기, S로 화자 이름 바꾸기" });
  root.append(col);
  const wordEls: HTMLElement[] = new Array(words.length);
  const turnEls: HTMLElement[] = [];
  /** the parts of a turn that carry the speaker's name */
  const metaEls: { spk: HTMLElement; ts: HTMLElement; edit: HTMLElement }[] = [];

  const labelOf = (t: TurnIx): string => (model.turnStatus(t) === "unassigned" ? "화자 미지정" : model.speakerLabel(t.speaker) ?? t.speaker ?? "");

  /** One turn: its meta column (speaker, time, the quiet edit control) and its text. An edited turn's
   *  text is one element standing for all its words: it has no word times of its own. */
  function buildTurn(t: TurnIx, ti: number): HTMLElement {
    const status = model.turnStatus(t), label = labelOf(t), edited = model.edited(t);
    const ts = h("button", { type: "button", class: "ts", text: mmss(t.start_ms), tabindex: "-1", "aria-label": `${label}, ${mmss(t.start_ms)}부터 재생`, data: { t: String(ti) } });
    // a turn with a speaker can be renamed or merged from its name; tabindex -1 like the time: S on a word reaches it
    const spk = t.speaker
      ? h("button", { type: "button", class: "spk spk-btn", text: label, title: label, tabindex: "-1", "aria-haspopup": "menu", "aria-expanded": "false", "aria-label": `화자 ${label}: 이름 바꾸기, 같은 사람으로 합치기`, data: { s: String(ti) } })
      : h("span", { class: "spk", text: label, title: label });
    const edit = h("button", { type: "button", class: "turn-act", text: "고치기", tabindex: "-1", "aria-label": `${label} ${mmss(t.start_ms)} 문장 고치기`, data: { e: String(ti) } });
    const meta = h("div", { class: "turn-meta" },
      spk, status === "ambiguous" ? h("span", { class: "spk-flag", text: "화자 불분명" }) : null, ts,
      edited ? h("span", { class: "ed-mark", text: "고침", title: "고친 문장이에요. 받아쓴 원문은 그대로 보관돼요." }) : null, edit);
    metaEls[ti] = { spk, ts, edit };
    const text = h("p", { class: "turn-text" });
    if (edited) {
      const el = h("span", { class: "w w-ed", text: model.turnText(t), data: { i: String(t.i0) } });
      for (let i = t.i0; i <= t.i1; i++) wordEls[i] = el;
      text.append(el);
    } else {
      for (let i = t.i0; i <= t.i1; i++) {
        // one tab stop in all (roving tabindex below); the words are the keyboard path. A word a 용어 사전
        // replacement removed keeps its (hidden) place, so the indexes and times stay the stored ones
        const shown = model.text(i);
        const el = h("span", { class: "w", text: shown, hidden: !shown, data: { i: String(i) } });
        wordEls[i] = el;
        text.append(el, shown ? " " : "");
      }
    }
    // a turn whose every word a 용어 사전 replacement removed keeps its place, hidden (an edit is never empty)
    const gone = !text.textContent;
    return h("div", { class: `turn${model.isMe(t.speaker) ? " me" : ""}`, role: "group", "aria-label": `${label} ${mmss(t.start_ms)}${edited ? ", 고친 문장" : ""}`, hidden: gone, data: { ch: String(words[t.i0].channel) } }, meta, text);
  }
  /** word `i` has a box on screen: a word of an edited turn (the turn's one element), or a word a 용어 사전 replacement did not empty */
  const visible = (i: number): boolean => {
    const t = turns[model.turnOf[i]];
    return t && model.edited(t) ? true : model.text(i) !== "";
  };
  /** the turn's first visible word, -1 when it has none (every word emptied by a 용어 사전 replacement) */
  const firstVisible = (t: TurnIx): number => {
    for (let i = t.i0; i <= t.i1; i++) if (visible(i)) return i;
    return -1;
  };
  /** the keyboard stop for turn `t`: its first visible word, else the nearest visible one before, else after */
  const stopIn = (t: TurnIx): number => {
    const f = firstVisible(t);
    if (f >= 0) return f;
    for (let j = t.i0 - 1; j >= 0; j--) if (visible(j)) return j;
    for (let j = t.i1 + 1; j < words.length; j++) if (visible(j)) return j;
    return t.i0;
  };

  const frag = document.createDocumentFragment();
  turns.forEach((t, ti) => {
    const el = buildTurn(t, ti);
    turnEls.push(el);
    frag.append(el);
  });
  col.append(frag);
  const first = model.words.findIndex((_, i) => visible(i));
  wordEls[Math.max(0, first)]?.setAttribute("tabindex", "0");

  const st = { follow: true, cur: -1, curTurn: -1, rangeEls: [] as HTMLElement[], anim: 0, focusIdx: Math.max(0, first), ch: null as number | null, editing: -1 };
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
    if (i >= 0 && st.cur >= 0 && wordEls[i] === wordEls[st.cur]) { st.cur = i; return; } // inside one edited turn
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
    // a word a 용어 사전 replacement emptied is hidden (no box to scroll to): the playhead stays on the
    // nearest shown word before it
    let i = model.wordAt(player.t);
    while (i >= 0 && !visible(i)) i--;
    const changed = i !== st.cur;
    setCurrent(i);
    if (changed && i >= 0 && st.follow && player.playing) revealWord(i);
  }

  /** Mark the words of the evidence range; true when there is one. */
  function markRange(): boolean {
    for (const el of st.rangeEls) el.classList.remove("ev");
    st.rangeEls = [];
    const r = player.range;
    if (!r) return false;
    for (let i = r.i0; i <= r.i1; i++) { wordEls[i]?.classList.add("ev"); st.rangeEls.push(wordEls[i]); }
    return true;
  }
  function applyRange(): void {
    const r = player.range;
    if (!markRange() || !r) return;
    setFollow(true);
    // reveal the first word of the range that is shown (a 용어 사전 replacement may have emptied some;
    // a hidden span has no place to scroll to; an edited turn is shown as one element): else the
    // nearest shown one before it
    let i0 = r.i0;
    while (i0 <= r.i1 && !visible(i0)) i0++;
    if (i0 > r.i1) { i0 = r.i0; while (i0 >= 0 && !visible(i0)) i0--; }
    const first = i0 >= 0 ? wordEls[i0] : undefined;
    if (first) revealWord(i0, !inView(first) || relY(first) > root.clientHeight * BAND_BOTTOM);
    updateFollowUI();
  }
  /** The words of the channel that is not being played are muted, not hidden. */
  function applyChannel(ch: number | null): void {
    st.ch = ch;
    for (const t of turnEls) muteTurn(t);
  }
  function muteTurn(t: HTMLElement): void {
    if (st.ch === null || t.dataset.ch === String(st.ch)) t.removeAttribute("data-muted"); else t.dataset.muted = "1";
  }

  /** Build turn `ti` again from the model (after an edit or "원래대로"), keeping the playhead, the
   *  evidence marks, the muted channel and the keyboard stop. */
  function refreshTurn(ti: number): void {
    const t = turns[ti], old = turnEls[ti];
    if (!t || !old) return;
    if (st.editing === ti) st.editing = -1;
    const n = buildTurn(t, ti);
    old.replaceWith(n);
    turnEls[ti] = n;
    muteTurn(n);
    if (st.curTurn === ti) n.classList.add("cur");
    if (st.cur >= t.i0 && st.cur <= t.i1) wordEls[st.cur].classList.add("cur");
    if (st.focusIdx >= t.i0 && st.focusIdx <= t.i1) {
      if (visible(st.focusIdx)) wordEls[st.focusIdx].setAttribute("tabindex", "0");
      else setStop(stopIn(t)); // the word under the stop is hidden now ("원래대로" brought back emptied words)
    }
    markRange();
  }

  /** The speaker names changed (rename, merge, reset): every turn shows its new name. */
  function relabel(): void {
    turns.forEach((t, ti) => {
      const m = metaEls[ti], label = labelOf(t);
      if (!m) return;
      m.spk.textContent = label; m.spk.title = label;
      if (m.spk.tagName === "BUTTON") m.spk.setAttribute("aria-label", `화자 ${label}: 이름 바꾸기, 같은 사람으로 합치기`);
      m.ts.setAttribute("aria-label", `${label}, ${mmss(t.start_ms)}부터 재생`);
      m.edit.setAttribute("aria-label", `${label} ${mmss(t.start_ms)} 문장 고치기`);
      turnEls[ti].setAttribute("aria-label", `${label} ${mmss(t.start_ms)}${model.edited(t) ? ", 고친 문장" : ""}`);
    });
  }

  /** Edit turn `ti` in place: the text becomes a field; Enter saves, Escape cancels. */
  function startEdit(ti: number): void {
    if (!opts.canEdit()) return;
    const t = turns[ti], turnEl = turnEls[ti];
    if (!t || !turnEl) return;
    if (st.editing >= 0 && st.editing !== ti) refreshTurn(st.editing); // one at a time: the other one is dropped
    st.editing = ti;
    const textEl = turnEl.querySelector(".turn-text");
    if (!textEl) return;
    const label = labelOf(t);
    const area = h("textarea", { class: "field turn-input", rows: "1", "aria-label": `${label} ${mmss(t.start_ms)} 문장`, spellcheck: "false" });
    area.value = model.turnText(t);
    const msg = h("span", { class: "t-footnote c-3 turn-msg", text: EDIT_HINT, "aria-live": "polite" });
    const done = () => {
      refreshTurn(ti);
      const to = stopIn(t), el = wordEls[to];
      if (el) { setStop(to); el.focus({ preventScroll: true }); }
    };
    const run = async (job: () => Promise<string | null>) => {
      if (box.querySelector("[aria-disabled]")) return;
      // aria-disabled, not disabled: the focused button keeps the focus while it saves
      box.querySelectorAll("button").forEach((b) => b.setAttribute("aria-disabled", "true"));
      const err = await job();
      if (!box.isConnected) return; // the transcript was rebuilt meanwhile
      box.querySelectorAll("button").forEach((b) => b.removeAttribute("aria-disabled"));
      if (err === null) { done(); return; }
      msg.textContent = err; msg.classList.add("danger");
      area.focus();
    };
    const save = h("button", { type: "button", class: "btn btn-primary", text: "저장", onclick: () => void run(() => opts.saveEdit(ti, area.value)) });
    const cancel = h("button", { type: "button", class: "btn", text: "취소", onclick: done });
    const reset = model.edited(t) ? h("button", { type: "button", class: "btn", text: "원래대로", title: "받아쓴 원문으로 되돌려요", onclick: () => void run(() => opts.resetEdit(ti)) }) : null;
    const box = h("div", { class: "turn-editor" }, area, h("div", { class: "turn-edit-acts" }, msg, reset, cancel, save));
    const fit = () => { area.style.height = "auto"; area.style.height = `${area.scrollHeight + 2}px`; };
    area.addEventListener("input", () => { fit(); if (msg.classList.contains("danger")) { msg.classList.remove("danger"); msg.textContent = EDIT_HINT; } });
    area.addEventListener("keydown", (e) => {
      if (e.isComposing || e.keyCode === 229) return; // the Korean IME is still composing: Enter finishes the syllable
      if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); done(); }
      else if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); save.click(); }
    });
    textEl.replaceWith(box);
    fit();
    area.focus({ preventScroll: true });
    area.setSelectionRange(area.value.length, area.value.length);
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
    const target = e.target as HTMLElement;
    const act = target.closest?.(".turn-act") as HTMLElement | null;
    if (act) { startEdit(Number(act.dataset.e)); return; }
    const spk = target.closest?.(".spk-btn") as HTMLElement | null;
    if (spk) { opts.onSpeaker(Number(spk.dataset.s), spk); return; }
    if (target.closest?.(".turn-editor")) return;
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
    let to = Math.max(0, Math.min(words.length - 1, i));
    const t = turns[model.turnOf[to]];
    if (t && model.edited(t)) to = t.i0; // the stop of an edited turn is its first word
    if (!wordEls[to]) return;
    setStop(to);
    wordEls[to].focus({ preventScroll: true });
    const H = root.clientHeight, y = relY(wordEls[to]);
    if (y < 40 || y + 40 > H) root.scrollTop += y - H * 0.4; // keyboard reveal: instant, and not a user scroll
  }
  /** the next shown word from `i` in direction `d` (hidden ones are skipped), or `i` itself */
  const shownFrom = (i: number, d: 1 | -1): number => {
    for (let j = i + d; j >= 0 && j < words.length; j += d) if (visible(j)) return j;
    return i;
  };
  root.addEventListener("keydown", (e) => {
    // moving the caret in the edit field is not reading elsewhere: only the transcript itself pauses follow
    if (["PageUp", "PageDown", "Home", "End"].includes(e.key) && !(e.target as HTMLElement).closest?.("textarea, input")) pause();
    const i = wordOf(e);
    if (i < 0) return;
    const plain = !e.metaKey && !e.ctrlKey && !e.altKey;
    // by key position, so a Korean keyboard layout works too
    if (plain && e.code === "KeyE") { e.preventDefault(); startEdit(model.turnOf[i]); return; }
    if (plain && e.code === "KeyS") {
      const ti = model.turnOf[i], spk = metaEls[ti]?.spk;
      if (spk?.tagName === "BUTTON") { e.preventDefault(); opts.onSpeaker(ti, spk); }
      return;
    }
    let to: number | null = null;
    // an edited turn is one stop: step over the rest of its words
    if (e.key === "ArrowRight") to = shownFrom(wordEls[i + 1] === wordEls[i] ? turns[model.turnOf[i]].i1 : i, 1);
    else if (e.key === "ArrowLeft") to = shownFrom(i, -1);
    else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      // the next turn that has a visible word (a turn every word of which was emptied is hidden)
      const d = e.key === "ArrowDown" ? 1 : -1;
      for (let ti = model.turnOf[i] + d; ti >= 0 && ti < turns.length; ti += d) {
        const f = firstVisible(turns[ti]);
        if (f >= 0) { to = f; break; }
      }
    } else if (e.key === "Enter") { e.preventDefault(); player.seek(words[i].start_ms, { play: true }); return; }
    if (to !== null) { e.preventDefault(); focusWord(to); }
  });

  return {
    root, col, wordEls, turnEls, applyChannel, relabel, refreshTurn, startEdit,
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
  const t = model.turns[model.turnOf[i]];
  // an edited turn has the turn's time only: its words' times belong to the original text
  if (t && model.edited(t)) return `${mmssd(t.start_ms)}–${mmssd(t.end_ms)} · ${model.chanLabel(w.channel)} · 고친 문장이라 문장 단위 시각만 있어요`;
  const parts = [`${mmssd(w.start_ms)}${w.end_ms > w.start_ms ? "–" + mmssd(w.end_ms) : ""} · ${model.chanLabel(w.channel)}`];
  if (w.p !== undefined) parts.push(`신뢰도 ${Math.round(w.p * 100)}%`);
  if (w.end_estimated) parts.push("끝 시각 추정");
  if (w.speaker_status === "ambiguous") parts.push("화자 불분명");
  if (w.speaker_status === "unassigned") parts.push("화자 미지정");
  if (model.text(i) !== w.text) parts.push(`원래 받아쓴 말: ${w.text}`); // a 용어 사전 replacement changed it
  return parts.join(" · ");
}
