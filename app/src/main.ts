import "./tokens.css";
import "./theme.css";
import "./base.css";
import "./app.css";
import { api, asError, type Status } from "./api";
import { h, icon, notice } from "./dom";
import { initJobs } from "./jobs";
import { initMeetingPrompt } from "./meeting-prompt";
import { closePop } from "./pop";
import { listScreen } from "./screens/list";
import { meetingScreen } from "./screens/meeting";
import { pluginsScreen } from "./screens/plugins";
import { isRecording, onRecorderChange, recordScreen } from "./screens/record";
import { settingsScreen } from "./screens/settings";
import { initSetup } from "./setup";

const app = document.getElementById("app")!;
// one polite live region for the whole app (announce() writes to it)
document.body.append(h("div", { id: "live", class: "sr", role: "status", "aria-live": "polite" }));

/** If the backend cannot even report its status, treat everything as not set up and say so. */
const unknown = { ok: false, path: null, code: "error", detail: "상태를 확인하지 못했습니다" };
let status: Status = { dev_build: false, model: unknown, whisper_cli: unknown, capture: unknown, live: unknown, batch_choice: "auto", batch_engine: "whisper", batch_fallback: false, batch: unknown, auto_retranscribe: true };

const NAV: [string, string, string][] = [["meetings", "회의", "list"], ["record", "녹음", "record"], ["plugins", "플러그인", "puzzle"], ["settings", "설정", "gear"]];
let cleanup: () => void = () => {};
let routeSeq = 0;

async function route(): Promise<void> {
  const mine = ++routeSeq; // screens render asynchronously: a newer navigation must win
  cleanup(); cleanup = () => {};
  closePop(true);
  const [seg = "meetings", arg] = location.hash.replace(/^#\//, "").split("/");
  const page = NAV.some(([k]) => k === seg) || seg === "meeting" ? seg : "meetings";
  app.dataset.rail = page === "meeting" ? "1" : "0"; // the meeting screen needs the width: the sidebar shrinks to icons

  const recBadge = h("span", { class: "rec-on", hidden: !isRecording(), title: "녹음 중" }, h("i", { class: "dot" }), h("span", { class: "lbl", text: "녹음 중" }));
  const side = h("nav", { class: "side", "aria-label": "주 메뉴" }, h("div", { class: "side-brand", text: "meetko" }),
    ...NAV.map(([k, label, ic]) => h("a", { class: "row", href: `#/${k}`, title: label, "aria-current": page === k || (page === "meeting" && k === "meetings") ? "page" : null },
      icon(ic, 16), h("span", { class: "lbl", text: label }), k === "record" ? recBadge : null)));
  const main = h("main", { class: "main" });
  app.replaceChildren(side, main);
  const offRec = onRecorderChange(() => { recBadge.hidden = !isRecording(); });

  // Screens register their teardown here before anything can throw, so a screen that fails
  // half-way (or is superseded while still loading) is still cleaned up.
  const cleanups: (() => void)[] = [offRec];
  const onCleanup = (f: () => void) => { cleanups.push(f); };
  const undo = () => { for (const f of cleanups.splice(0).reverse()) { try { f(); } catch { /* keep tearing down the rest */ } } };
  const open = (id: string) => { location.hash = `#/meeting/${encodeURIComponent(id)}`; };
  try {
    if (page === "record") recordScreen(main, status, open, onCleanup);
    else if (page === "plugins") await pluginsScreen(main);
    else if (page === "settings") settingsScreen(main, status, onCleanup);
    else if (page === "meeting" && arg) await meetingScreen(main, decodeURIComponent(arg), status, onCleanup);
    else await listScreen(main, status, onCleanup);
  } catch (e) {
    // whatever the screen managed to set up is torn down by its registered cleanups; the user still gets a visible error and a way out
    main.replaceChildren(h("div", { class: "page" }, h("div", { class: "page-in" },
      notice("danger", "화면을 표시하지 못했어요.", asError(e).message,
        h("button", { type: "button", class: "btn", text: "다시 시도", onclick: () => void route() }),
        h("a", { class: "btn", href: "#/meetings", text: "회의 목록" })))));
  }
  if (mine !== routeSeq) { undo(); return; } // superseded while rendering: undo this screen
  cleanup = undo;
}

/** Read the status again. The views that show it repaint themselves in place (setup.ts `onStatus`),
 * so focus, scroll and the search field stay; nothing is re-routed. */
async function fetchStatus(): Promise<Status> {
  status = await api.status();
  return status;
}

async function boot(): Promise<void> {
  await initJobs();
  try { status = await api.status(); } catch { /* keep "unknown": everything reads as not set up */ }
  await initSetup(status, fetchStatus);
  window.addEventListener("hashchange", () => void route());
  await route();
  await initMeetingPrompt(() => status.capture.ok); // after the first screen: the prompt sits above whatever is shown
}

void boot();
