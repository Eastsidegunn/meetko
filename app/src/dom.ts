// DOM helpers. Text only ever goes in through textContent / text nodes: transcripts and plugin
// output are untrusted, so there is no innerHTML. CSP forbids inline styles, so there is no
// `style` attribute either: dynamic values are set through the CSSOM (`el.style.x = ...`).
const SVG_NS = "http://www.w3.org/2000/svg";

export type Child = Node | string | null | undefined | false | Child[];
type Props = Record<string, unknown>;

export function h<K extends keyof HTMLElementTagNameMap>(tag: K, props: Props | null = {}, ...kids: Child[]): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props ?? {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") el.className = v as string;
    else if (k === "text") el.textContent = v as string;
    else if (k === "style") throw new Error("inline style is forbidden by the CSP; use el.style");
    else if (k === "data") Object.assign(el.dataset, v);
    else if (k.startsWith("on") && typeof v === "function") el.addEventListener(k.slice(2), v as EventListener);
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  append(el, kids);
  return el;
}

export function append(el: Element, kids: Child[]): void {
  for (const c of kids.flat(3) as Exclude<Child, Child[]>[]) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c);
  }
}

export const clear = (el: Element): void => el.replaceChildren();

// ---- icons: 16px grid, 1.5 stroke, drawn here (no third-party glyphs) ----
const ICONS: Record<string, string> = {
  play: "M5.5 3.5 L12.5 8 L5.5 12.5 Z",
  pause: "M5 3.5 V12.5 M11 3.5 V12.5",
  chevron: "M4 6 L8 10 L12 6",
  check: "M3.5 8.5 L6.5 11.5 L12.5 4.5",
  copy: "M5.5 5.5 H12 V12 H5.5 Z M3.5 10.5 V3.5 H10.5",
  plus: "M8 3.5 V12.5 M3.5 8 H12.5",
  list: "M3 4.5 H13 M3 8 H13 M3 11.5 H13",
  record: "M8 3 a5 5 0 1 0 0.01 0 Z",
  puzzle: "M3.5 6 H6 a1.5 1.5 0 1 1 3 0 H11.5 V8.5 a1.5 1.5 0 1 1 0 3 V13 H3.5 Z",
  gear: "M8 5.5 a2.5 2.5 0 1 0 0.01 0 Z M8 2 V3.5 M8 12.5 V14 M2 8 H3.5 M12.5 8 H14 M3.8 3.8 L4.9 4.9 M11.1 11.1 L12.2 12.2 M12.2 3.8 L11.1 4.9 M4.9 11.1 L3.8 12.2",
  warn: "M8 2.5 L14 13 H2 Z M8 6.5 V9.5 M8 11.2 V11.3",
  net: "M8 2.5 a5.5 5.5 0 1 0 0.01 0 Z M2.5 8 H13.5 M8 2.5 C5.5 5 5.5 11 8 13.5 M8 2.5 C10.5 5 10.5 11 8 13.5",
  wave: "M3 7 V9 M5.5 5 V11 M8 3 V13 M10.5 5 V11 M13 7 V9",
  x: "M4 4 L12 12 M12 4 L4 12",
  folder: "M2.5 4.5 H6.5 L8 6 H13.5 V12.5 H2.5 Z",
  arrowdown: "M8 3 V12 M4.5 8.5 L8 12 L11.5 8.5",
  arrowup: "M8 13 V4 M4.5 7.5 L8 4 L11.5 7.5",
  info: "M8 2.5 a5.5 5.5 0 1 0 0.01 0 Z M8 7 V11 M8 5 V5.1",
  search: "M7 3 a4 4 0 1 0 0.01 0 Z M10 10 L13 13",
};
export function icon(name: string, size = 16): SVGSVGElement {
  const s = document.createElementNS(SVG_NS, "svg");
  const set = (k: string, v: string) => s.setAttribute(k, v);
  set("viewBox", "0 0 16 16"); set("width", String(size)); set("height", String(size));
  set("aria-hidden", "true"); set("fill", "none"); set("stroke", "currentColor");
  set("stroke-width", "1.5"); set("stroke-linecap", "round"); set("stroke-linejoin", "round");
  const p = document.createElementNS(SVG_NS, "path");
  p.setAttribute("d", ICONS[name] ?? ICONS.info);
  if (name === "play" || name === "record") p.setAttribute("fill", "currentColor");
  s.append(p);
  return s;
}

// ---- formatting ----
const p2 = (n: number) => String(n).padStart(2, "0");
export function mmss(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000)), hh = Math.floor(s / 3600), mm = Math.floor((s % 3600) / 60);
  return hh > 0 ? `${hh}:${p2(mm)}:${p2(s % 60)}` : `${p2(mm)}:${p2(s % 60)}`;
}
export const mmssd = (ms: number): string => `${mmss(ms)}.${Math.floor((ms % 1000) / 100)}`;
export function durText(ms: number): string {
  const m = Math.round(ms / 60000);
  if (ms < 60000) return `${Math.round(ms / 1000)}초`;
  return m < 60 ? `${m}분` : `${Math.floor(m / 60)}시간 ${m % 60}분`;
}
export function dateText(iso: string | null | undefined): string {
  const d = iso ? new Date(iso) : null;
  return d && !isNaN(d.getTime()) ? `${d.getMonth() + 1}월 ${d.getDate()}일 ${p2(d.getHours())}:${p2(d.getMinutes())}` : iso ?? "";
}
export function dayText(iso: string | null | undefined): string {
  const d = iso ? new Date(iso) : null;
  return d && !isNaN(d.getTime()) ? `${d.getMonth() + 1}월 ${d.getDate()}일` : iso ?? "";
}
export const shortHash = (s: string): string => (s.length > 16 ? s.slice(0, 8) + "…" + s.slice(-6) : s);
export const kindName = (k: string | null): string => (k === "approved" ? "승인본" : k === "draft" ? "초안" : k === "live" ? "실시간" : "전사 전");

/** A hash, shortened; the button copies the whole value. */
export function hashBtn(full: string): HTMLButtonElement {
  const label = h("span", { text: shortHash(full) });
  const b = h("button", { type: "button", class: "hashbtn", title: `${full} — 눌러서 복사`, "aria-label": "전체 sha256 복사" }, label, icon("copy", 12));
  b.addEventListener("click", async () => {
    let t = "복사됨";
    try { await navigator.clipboard.writeText(full); } catch { t = "복사 실패"; } // the title still shows the full value
    label.textContent = t;
    setTimeout(() => { label.textContent = shortHash(full); }, 1200);
  });
  return b;
}

/** Announce text to screen readers through the polite live region (created once at boot). */
export function announce(text: string): void {
  const el = document.getElementById("live");
  if (!el) return;
  el.textContent = "";
  setTimeout(() => { el.textContent = text; }, 30); // a second tick, so repeating the same text is announced again
}

/** The Korean notice block used for every warning and error: icon, bold lead, then detail. */
export function notice(kind: "info" | "warn" | "danger", lead: string, detail: Child, ...acts: Child[]): HTMLElement {
  return h("div", { class: `notice ${kind === "info" ? "" : kind}`.trim(), role: kind === "danger" ? "alert" : null },
    icon(kind === "info" ? "info" : "warn", 16),
    h("div", { class: "grow" }, lead ? h("strong", { text: lead + " " }) : null, detail, acts.length ? h("div", { class: "acts" }, ...acts) : null));
}
