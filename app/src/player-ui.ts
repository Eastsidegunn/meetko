// Transport bar: play/pause, time, channel choice, and the timeline (markers, one coverage
// lane per channel with the unrecorded stretches hatched, the playhead).
import type { Anchor } from "./api";
import { h, icon, mmss } from "./dom";
import type { Resolved } from "./model";
import type { Channel, Player } from "./player";
import { markerKind } from "./labels";

const CHANNELS: [Channel, string][] = [["both", "둘 다"], ["me", "나만"], ["other", "상대만"]];

/** One coverage lane: a channel and the stretches of it that were not recorded. */
export interface Lane { index: number; label: string; gaps: [number, number][] }
export interface Marker { kind: string; label: string; at: Anchor }

export interface PlayerUi {
  root: HTMLElement;
  setMarkers(markers: Marker[]): void;
  /** stop listening to the player (the bar is rebuilt per version) */
  destroy(): void;
}

export function buildPlayer(player: Player, lanes: Lane[], resolve: (a: Anchor) => Resolved, onMarker: (a: Anchor) => void, onChannel: (c: Channel) => void): PlayerUi {
  const dur = player.dur;
  const pct = (ms: number) => `${(Math.max(0, Math.min(dur, ms)) / dur) * 100}%`;

  // ---- transport
  const playBtn = h("button", { type: "button", class: "pl-play", "aria-label": "재생", title: "재생 / 일시정지 (Space)" }, icon("play", 14));
  const cur = h("b", { text: "00:00" });
  const timeEl = h("div", { class: "pl-time num" }, cur, h("span", { text: ` / ${mmss(dur)}` }));
  const seg = h("div", { class: "seg", role: "radiogroup", "aria-label": "재생 채널" });
  const segBtns = CHANNELS.map(([k, label]) => {
    const b = h("button", { type: "button", class: "btn", role: "radio", "aria-checked": "false", disabled: !player.has(k), text: label, title: player.has(k) ? null : "이 회의에는 해당 채널이 없습니다" });
    b.addEventListener("click", () => onChannel(k));
    seg.append(b);
    return [k, b] as const;
  });

  // ---- timeline
  const tl = h("div", { class: "tl", role: "slider", tabindex: "0", "aria-label": "재생 위치", "aria-valuemin": "0", "aria-valuemax": String(Math.round(dur / 1000)), "aria-valuenow": "0" });
  const markersEl = h("div", { class: "tl-markers" });
  tl.append(markersEl);
  const laneEls: HTMLElement[] = [];
  lanes.forEach((ch, k) => {
    const lane = h("div", { class: `tl-lane l${k}`, title: `${ch.label} — 녹음되지 않은 구간은 빗금` });
    for (const [a, b] of ch.gaps) {
      const g = h("div", { class: "gap" });
      g.style.left = pct(a);
      g.style.width = `${((b - a) / dur) * 100}%`;
      lane.append(g);
    }
    laneEls.push(lane);
    const lbl = h("span", { class: "tl-lbl", text: ch.label });
    lbl.style.top = `${k === 0 ? 30 : 42}px`;
    tl.append(lane, lbl);
  });
  const ph = h("div", { class: "tl-ph" });
  const hov = h("div", { class: "tl-hover" });
  const tip = h("div", { class: "tl-tip" });
  hov.hidden = tip.hidden = true;
  tl.append(ph, hov, tip);

  function setMarkers(markers: Marker[]): void {
    markersEl.replaceChildren();
    for (const m of markers) {
      const r = resolve(m.at);
      if (!r.ok) continue;
      const text = `${markerKind(m.kind)}: ${m.label} ${mmss(r.start)}`;
      const b = h("button", { type: "button", class: `mk ${m.kind}`, "aria-label": text, title: text }, h("i"));
      b.style.left = pct(r.start);
      b.addEventListener("click", (e) => { e.stopPropagation(); onMarker(m.at); });
      b.addEventListener("pointerdown", (e) => e.stopPropagation()); // a marker is not a scrub
      markersEl.append(b);
    }
  }

  const msAt = (e: PointerEvent) => { const r = tl.getBoundingClientRect(); return Math.max(0, Math.min(dur, ((e.clientX - r.left) / r.width) * dur)); };
  let dragging = false;
  tl.addEventListener("pointerdown", (e) => { dragging = true; tl.setPointerCapture(e.pointerId); player.seek(msAt(e)); });
  tl.addEventListener("pointermove", (e) => {
    const r = tl.getBoundingClientRect(), x = e.clientX - r.left, ms = msAt(e);
    hov.hidden = tip.hidden = false;
    hov.style.left = `${x}px`;
    tip.style.left = `${Math.max(24, Math.min(r.width - 24, x))}px`;
    tip.textContent = mmss(ms);
    if (dragging) player.seek(ms);
  });
  tl.addEventListener("pointerup", () => { dragging = false; });
  tl.addEventListener("pointerleave", () => { hov.hidden = tip.hidden = true; });
  tl.addEventListener("keydown", (e) => {
    const step = e.shiftKey ? 30000 : 5000;
    if (e.key === "ArrowRight") { player.seek(player.t + step); e.preventDefault(); }
    else if (e.key === "ArrowLeft") { player.seek(player.t - step); e.preventDefault(); }
  });

  let lastSec = -1;
  const paint = (kind: string) => {
    if (kind === "state") {
      playBtn.replaceChildren(icon(player.playing ? "pause" : "play", 14));
      playBtn.setAttribute("aria-label", player.playing ? "일시정지" : "재생");
    }
    if (kind === "channel") {
      segBtns.forEach(([k, b]) => b.setAttribute("aria-checked", String(k === player.channel)));
      const only = player.channel === "both" ? null : Number(player.sourceOf(player.channel));
      laneEls.forEach((l, k) => l.classList.toggle("muted", only !== null && lanes[k].index !== only));
    }
    const sec = Math.floor(player.t / 1000);
    if (sec !== lastSec) { lastSec = sec; cur.textContent = mmss(player.t); tl.setAttribute("aria-valuenow", String(sec)); }
    ph.style.left = pct(player.t);
  };
  const off = player.on(paint);
  paint("state"); paint("channel");
  playBtn.addEventListener("click", () => player.toggle());

  const root = h("div", { class: "pl" }, h("div", { class: "pl-row" }, playBtn, timeEl, h("div", { class: "pl-sp" }), seg), tl);
  return { root, setMarkers, destroy: off };
}
