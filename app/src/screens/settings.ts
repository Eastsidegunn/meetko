// What the app will run and read, and the two things it keeps itself: the models it set up for the
// user (setup.ts: downloaded or copied into the data folder, checked against their pins) and
// 회의 알림 (meeting detection, app-settings.json in the data folder). Executables are never taken
// from the data folder.
import { api, asError, type MeetingWatch, type Setup, type Status } from "../api";
import { announce, h, notice } from "../dom";
import { onStatus, recheck, setupSection } from "../setup";
import { vocabularySection } from "../vocabulary";

export function settingsScreen(main: HTMLElement, status: Status, onCleanup: (f: () => void) => void = () => {}): void {
  const row = (k: string, s: Setup) =>
    h("div", { class: "setrow" }, h("dt", { text: k }), h("dd", { class: `val${s.path ? "" : " missing"}`, text: s.path ?? "찾을 수 없음" }), h("span"));
  const files = h("dl");
  const paintFiles = (s: Status) => files.replaceChildren(row("Whisper 모델", s.model), row("whisper-cli", s.whisper_cli), row("녹음 프로그램", s.capture));
  paintFiles(status);
  onStatus(paintFiles, onCleanup); // 다시 확인 and a finished download repaint in place: focus and scroll stay
  const set = h("div", { class: "set" });
  const again = h("button", { type: "button", class: "btn", text: "다시 확인" });
  const result = h("div", { role: "status" });
  again.addEventListener("click", async () => {
    again.setAttribute("aria-disabled", "true"); // stays focusable
    result.replaceChildren();
    try { await recheck(); announce("다시 확인했어요."); } catch { result.replaceChildren(notice("danger", "상태를 확인하지 못했어요.", "잠시 뒤 다시 시도해 주세요.")); }
    again.removeAttribute("aria-disabled");
  });
  set.append(
    setupSection(onCleanup),
    h("section", { class: "set-sec" }, h("h2", { text: "찾은 파일" }),
      h("p", { class: "c-2 t-footnote", text: "실행 파일은 환경변수(MEETKO_WHISPER_CLI, MEETKO_CAPTURE_BIN), 앱 옆 파일, PATH에서만 찾아요. 데이터 폴더의 설정 파일에 적힌 실행 파일 경로는 쓰지 않아요." }),
      files),
    h("div", {}, again), result,
    meetingSection(),
    vocabularySection(onCleanup),
    // license notice (AGPL section 5(d)): plain text, the app has no way to open external links
    h("p", { class: "c-3 t-caption", text: `meetko ${__APP_VERSION__} · AGPL-3.0-or-later · 보증 없음 · 소스 코드: github.com/Eastsidegunn/meetko` }));
  main.replaceChildren(h("div", { class: "page" }, h("div", { class: "page-in" }, h("div", { class: "page-head" }, h("h1", { class: "t-title1", text: "설정" })), set)));
}

/** The one setting the app keeps itself (in the data folder): whether to notice meetings. On by default. */
function meetingSection(): HTMLElement {
  const on = h("input", { type: "checkbox", id: "mw-on", disabled: true }) as HTMLInputElement;
  const web = h("input", { type: "checkbox", id: "mw-web", disabled: true }) as HTMLInputElement;
  const msg = h("div", { role: "status" });
  const sec = h("section", { class: "set-sec" }, h("h2", { text: "회의 알림" }),
    h("p", { class: "c-2 t-footnote", text: "Zoom, Teams 같은 회의 앱이 마이크를 쓰기 시작하면 녹음할지 물어봐요. 기본으로 켜져 있어요. 끄면 다시 묻지 않아요. 어떤 앱이 마이크를 쓰는지만 확인하고, 화면·창 제목·웹 주소는 보지 않아요. 묻기만 하고 저절로 녹음하지 않아요." }),
    h("label", { class: "mw-row", for: "mw-on" }, on, h("span", { text: "회의가 시작되면 녹음할지 묻기" })),
    h("label", { class: "mw-row", for: "mw-web" }, web, h("span", { text: "브라우저가 마이크를 쓸 때도 묻기 (웹 회의)" })),
    msg);
  const paint = (w: MeetingWatch) => {
    on.checked = w.enabled; web.checked = w.browsers;
    on.disabled = !w.available; web.disabled = !w.available || !w.enabled;
    if (!w.available) msg.replaceChildren(h("p", { class: "c-2 t-footnote", text: "이 기기에서는 아직 쓸 수 없어요. 지금은 macOS에서만 돼요." }));
  };
  let saving = false, again = false;
  // the boxes stay enabled while saving (disabling the focused one would drop keyboard focus); a change
  // made meanwhile is saved right after, and the boxes are repainted only from the last save
  const save = async (): Promise<void> => {
    if (saving) { again = true; return; }
    saving = true; sec.setAttribute("aria-busy", "true");
    msg.replaceChildren();
    try {
      const w = await api.meetingWatchSet(on.checked, web.checked);
      if (!again) paint(w);
    } catch (e) {
      msg.replaceChildren(notice("danger", "설정을 저장하지 못했어요.", asError(e).message));
      if (!again) try { paint(await api.meetingWatchGet()); } catch { /* keep what is shown */ }
    }
    saving = false; sec.removeAttribute("aria-busy");
    if (again) { again = false; await save(); }
  };
  on.addEventListener("change", () => void save());
  web.addEventListener("change", () => void save());
  api.meetingWatchGet().then(paint, (e) => msg.replaceChildren(notice("danger", "회의 알림 설정을 읽지 못했어요.", asError(e).message)));
  return sec;
}
