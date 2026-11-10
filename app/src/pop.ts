// Popover menu and modal dialog. Motion: 180 ms ease, opacity + scale .98 -> 1 from the trigger.
import { h, type Child } from "./dom";

const reduced = () => matchMedia("(prefers-reduced-motion: reduce)").matches;
let current: { el: HTMLElement; anchor: HTMLElement; off: () => void } | null = null;

export function closePop(immediate = false): void {
  if (!current) return;
  const { el, anchor, off } = current;
  current = null; off();
  if (immediate || reduced()) el.remove();
  else { el.classList.add("closing"); el.addEventListener("animationend", () => el.remove(), { once: true }); setTimeout(() => el.remove(), 260); }
  anchor.setAttribute("aria-expanded", "false");
  if (!immediate) anchor.focus({ preventScroll: true });
}

export function openPop(anchor: HTMLElement, content: Child, { align = "left" }: { align?: "left" | "right" } = {}): void {
  closePop(true);
  const el = h("div", { class: "pop", role: "menu" }, content);
  document.body.append(el);
  const r = anchor.getBoundingClientRect(), w = el.offsetWidth;
  const left = Math.max(8, Math.min(align === "right" ? r.right - w : r.left, innerWidth - w - 8));
  el.style.left = `${left}px`;
  el.style.top = `${r.bottom + 6}px`;
  el.style.transformOrigin = `${Math.round(r.left + r.width / 2 - left)}px 0px`; // grows from the trigger
  anchor.setAttribute("aria-expanded", "true");
  const items = [...el.querySelectorAll<HTMLElement>("button")];
  const onDown = (e: Event) => { if (!el.contains(e.target as Node) && !anchor.contains(e.target as Node)) closePop(); };
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") { e.stopPropagation(); closePop(); }
    else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const i = items.indexOf(document.activeElement as HTMLElement), n = items.length;
      items[(i + (e.key === "ArrowDown" ? 1 : -1) + n) % n]?.focus();
    }
  };
  document.addEventListener("pointerdown", onDown, true);
  document.addEventListener("keydown", onKey, true);
  current = { el, anchor, off: () => { document.removeEventListener("pointerdown", onDown, true); document.removeEventListener("keydown", onKey, true); } };
  items[0]?.focus({ preventScroll: true });
}

/** A modal. `build` fills the dialog and gets `close`; focus is trapped and returns to the opener. */
export function openDialog(label: string, build: (dlg: HTMLElement, close: () => void) => void, onClose?: () => void): void {
  const prev = document.activeElement as HTMLElement | null;
  const dlg = h("div", { class: "dlg", role: "dialog", "aria-modal": "true", "aria-label": label });
  const scrim = h("div", { class: "scrim" }, dlg);
  const close = () => {
    document.removeEventListener("keydown", onKey, true);
    if (reduced()) scrim.remove(); else { scrim.classList.add("closing"); setTimeout(() => scrim.remove(), 200); }
    prev?.focus({ preventScroll: true });
    onClose?.();
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") { e.stopPropagation(); close(); return; }
    if (e.key !== "Tab") return;
    const f = [...dlg.querySelectorAll<HTMLElement>("button:not(:disabled)")];
    if (!f.length) return;
    const i = f.indexOf(document.activeElement as HTMLElement);
    if (e.shiftKey && i <= 0) { f[f.length - 1].focus(); e.preventDefault(); }
    else if (!e.shiftKey && i === f.length - 1) { f[0].focus(); e.preventDefault(); }
  };
  scrim.addEventListener("pointerdown", (e) => { if (e.target === scrim) close(); });
  document.addEventListener("keydown", onKey, true);
  build(dlg, close);
  document.body.append(scrim);
  (dlg.querySelector<HTMLElement>(".btn-primary") ?? dlg.querySelector<HTMLElement>("button"))?.focus({ preventScroll: true });
}

/** Confirm/cancel dialog; resolves true only on the confirm button. */
export function confirmDialog(title: string, body: Node, confirmLabel: string): Promise<boolean> {
  return new Promise((resolve) => {
    let answer = false;
    openDialog(title, (dlg, close) => {
      const ok = h("button", { type: "button", class: "btn btn-primary", text: confirmLabel, onclick: () => { answer = true; close(); } });
      const cancel = h("button", { type: "button", class: "btn", text: "취소", onclick: close });
      dlg.append(h("h2", { text: title }), body, h("div", { class: "acts" }, cancel, ok));
    }, () => resolve(answer));
  });
}
