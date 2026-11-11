// The meeting screen's "내보내기" control: save the version on screen through the native save
// dialog (opened by the backend, which also writes the file), or copy one of its text variants.
// Built once per screen: the toolbar is rebuilt on every progress event and re-attaches the same
// element, so a running save, its status text and an open menu survive the repaint.
import { api, asError, type ExportFormat, type Kind } from "./api";
import { announce, h, icon, notice } from "./dom";
import { exportErrorText } from "./labels";
import { closePop, openPop } from "./pop";

const FILES: [ExportFormat, string, string][] = [
  ["md", "Markdown으로 저장…", ".md · 시각·화자 포함, 노트 앱에서 바로 열려요"],
  ["txt", "텍스트로 저장…", ".txt · 시각·화자 포함"],
  ["srt", "SRT 자막으로 저장…", ".srt · 화자 포함"],
  ["vtt", "WebVTT 자막으로 저장…", ".vtt · 화자 포함"],
];
const COPIES: [ExportFormat, string][] = [
  ["md", "Markdown 복사"], ["plain", "텍스트만 복사"], ["txt", "시각·화자 넣어 복사"], ["speakers", "화자 넣어 복사"],
];
const COPIED: Record<Kind, string> = { approved: "복사했어요", draft: "초안을 복사했어요", live: "실시간 자막을 복사했어요" };

/** Write `ready` (fetched when the menu opened) inside the click, where WebKit allows it. Without
 * it the pending text goes in as a ClipboardItem, still inside the click; `writeText` after the
 * wait is the last resort and may be refused by WebKit (no user gesture left). An export failure
 * rejects as itself. */
async function copyText(ready: string | undefined, text: () => Promise<string>): Promise<void> {
  if (ready !== undefined) {
    try { await navigator.clipboard.writeText(ready); return; } catch { throw { code: "clipboard", message: "" }; }
  }
  const pending = text();
  try {
    await navigator.clipboard.write([new ClipboardItem({ "text/plain": pending.then((t) => new Blob([t], { type: "text/plain" })) })]);
  } catch {
    const t = await pending;
    try { await navigator.clipboard.writeText(t); } catch { throw { code: "clipboard", message: "" }; }
  }
}

/** A failure that arrives after the user left the meeting: shown at the corner until closed. */
function lateNotice(lead: string, text: string): void {
  const host = document.querySelector(".toast-host") ?? document.body.appendChild(h("div", { class: "toast-host" }));
  const n = notice("danger", lead, text, h("button", { type: "button", class: "btn", text: "닫기", onclick: () => n.remove() }));
  host.append(n);
}

export interface ExportControl {
  root: HTMLElement;
  /** the version on screen and the title the toolbar shows; null hides the control's menu */
  show(v: { id: string; kind: Kind } | null, title: string): void;
  /** the version's names or edited text changed: copy texts fetched earlier are out of date */
  changed(): void;
}

/** `onError(lead, text)` shows a failure in the screen's notice area; `alive()` is false once the
 * screen is gone, and the failure then goes to a corner notice instead. */
export function exportControl(onError: (lead: string, text: string) => void, alive: () => boolean): ExportControl {
  const done = h("span", { class: "t-footnote c-3 mt-done" });
  const btn = h("button", { type: "button", class: "btn", "aria-haspopup": "menu", "aria-expanded": "false" }, "내보내기", icon("chevron", 12));
  let cur: { id: string; kind: Kind } | null = null, title = "", timer = 0;
  // copy texts fetched when the menu opens, keyed by what they were fetched for (version + title)
  let ready = new Map<ExportFormat, string>(), readyFor = "";
  const keyOf = (id: string) => JSON.stringify([id, title]);
  const busy = () => btn.getAttribute("aria-disabled") === "true";
  const menuOpen = () => btn.getAttribute("aria-expanded") === "true";

  const run = async (lead: string, job: () => Promise<string | null>) => {
    // aria-disabled, not disabled: the focused button keeps focus while the job runs
    btn.setAttribute("aria-disabled", "true");
    try {
      const ok = await job();
      if (ok) {
        done.textContent = ok; announce(ok);
        clearTimeout(timer); timer = window.setTimeout(() => { done.textContent = ""; }, 2500);
      }
    } catch (e) {
      const text = exportErrorText(asError(e).code);
      if (alive()) onError(lead, text); else lateNotice(lead, text); // its role=alert announces it
    } finally { btn.removeAttribute("aria-disabled"); }
  };
  const save = (id: string, f: ExportFormat) => run("파일로 저장하지 못했어요.", async () => ((await api.exportSave(id, f, title)) !== null ? "저장했어요" : null));
  const copy = (id: string, kind: Kind, f: ExportFormat) => {
    const r = readyFor === keyOf(id) ? ready.get(f) : undefined; // read now: the write must start inside the click
    return run("복사하지 못했어요.", async () => { await copyText(r, () => api.exportVersion(id, f, title)); return COPIED[kind]; });
  };

  btn.addEventListener("click", () => {
    const v = cur;
    if (!v || busy()) return;
    const key = keyOf(v.id);
    if (readyFor !== key) { ready = new Map(); readyFor = key; }
    const into = ready; // a result that lands after the key changed goes into a map nobody reads
    for (const [f] of COPIES) if (!into.has(f)) void api.exportVersion(v.id, f, title).then((t) => { into.set(f, t); }, () => {});
    openPop(btn, h("div", { class: "menu" },
      ...FILES.map(([f, label, sub]) => h("button", { type: "button", class: "row", role: "menuitem", onclick: () => { closePop(); void save(v.id, f); } },
        h("span"), h("span", {}, label, h("small", { text: sub })), h("span"))),
      h("div", { class: "sep" }),
      ...COPIES.map(([f, label]) => h("button", { type: "button", class: "row", role: "menuitem", onclick: () => { closePop(); void copy(v.id, v.kind, f); } },
        h("span", { class: "c-3" }, icon("copy", 14)), h("span", { text: label }), h("span")))), { align: "right" });
  });

  return {
    root: h("span", { class: "mt-act" }, done, btn),
    changed() { ready = new Map(); readyFor = ""; },
    show(v, t) {
      // a menu opened for another version (or for none) must not act on what is now on screen
      if (menuOpen() && (!v || v.id !== cur?.id)) {
        const inMenu = document.activeElement?.closest(".pop") != null;
        closePop(true);
        // the toolbar keeps focus on the button across the repaint that follows (meeting.ts)
        if (inMenu && btn.isConnected) btn.focus({ preventScroll: true });
      }
      cur = v; title = t;
    },
  };
}
