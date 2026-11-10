// First-run setup of the models the app can fetch for the user (live captions, speaker separation),
// and the plain steps for what stays manual (the engine, whisper for 다시 받아쓰기).
// The download or copy runs in Rust and outlives the screen that started it, so its state lives here,
// fed by the `model-setup` events. The app status lives here too once it changes: views that show
// setup subscribe and repaint themselves in place, so a finished download or 다시 확인 never rebuilds
// the screen (focus, scroll and the search field stay where they were).
import { api, onModelSetup, whisperReady, type ApiError, type ModelEvent, type ModelKey, type ModelPiece, type ModelSetup, type Status } from "./api";
import { announce, h, notice, type Child } from "./dom";

/** the job in flight (last progress), the last failure, and the model that just became ready */
const st = { job: null as ModelEvent | null, error: null as { key: ModelKey; code: string } | null, finished: null as ModelKey | null };
const jobSubs = new Set<(e: ModelEvent | null) => void>();
const emitJob = (e: ModelEvent | null) => jobSubs.forEach((f) => f(e));
const statusSubs = new Set<(s: Status) => void>();
let fetchStatus: () => Promise<Status> = () => api.status();
let current: Status | null = null;

const NAME: Record<ModelKey, string> = { stream: "실시간 자막 모델", diarization: "화자 구분 모델" };
const ENGINE: Record<ModelKey, { bin: string; env: string; what: string }> = {
  stream: { bin: "meetko-nemotron-stream", env: "MEETKO_STREAM_ENGINE", what: "실시간 자막" },
  diarization: { bin: "meetko-nemotron-diar", env: "MEETKO_DIAR_ENGINE", what: "화자 구분" },
};

/** Called once at boot. `fetch` reads the app status again (and keeps the app's copy). */
export async function initSetup(status: Status, fetch: () => Promise<Status>): Promise<void> {
  current = status;
  fetchStatus = fetch;
  await onModelSetup((e) => {
    if (e.state === "progress") { st.job = e; st.error = null; emitJob(e); return; }
    st.job = null;
    if (e.state === "done") {
      st.finished = e.key; st.error = null;
      announce(e.key === "stream" ? "실시간 자막 모델을 준비했어요." : `${NAME[e.key]}을 준비했어요.`);
      void recheck().catch(() => emitJob(null)); // views repaint with the new status and pieces together
      return;
    }
    // said once here; the notice the views show for it has no alert role, so it is not said again
    if (e.state === "error") { st.error = { key: e.key, code: e.code ?? "error" }; announce(`${errorLead(st.error.code)} ${errorDetail(st.error.code, undefined)}`); }
    else announce(e.kind === "import" ? "가져오기를 멈췄어요." : "받기를 멈췄어요. 다시 누르면 이어서 받아요.");
    emitJob(null);
  });
}

/** Read the status again and let the views that show it repaint in place. */
export async function recheck(): Promise<Status> {
  const s = await fetchStatus();
  current = s;
  statusSubs.forEach((f) => f(s));
  return s;
}

/** The status the setup views last saw (null before boot). */
export const currentStatus = (): Status | null => current;

/** For screens that show setup-dependent text: called with every new status until cleanup. */
export function onStatus(f: (s: Status) => void, onCleanup: (f: () => void) => void): void {
  statusSubs.add(f);
  onCleanup(() => statusSubs.delete(f));
}

const mb = (n: number): string => `${Math.round(n / 1e6).toLocaleString()}MB`;
const pct = (d: number, t: number): number => (t > 0 ? Math.min(100, Math.floor((d / t) * 100)) : 0);

function errorLead(code: string): string {
  if (code === "wrong_file") return "고른 파일이 이 모델이 아니에요.";
  if (code === "disk_full") return "디스크 공간이 부족해요.";
  if (code === "checksum_mismatch" || code === "too_big") return "받은 파일이 원래 파일과 달라서 지웠어요.";
  if (code === "network") return "인터넷 연결 문제로 받지 못했어요.";
  if (code === "host_not_allowed") return "허용하지 않은 주소로 연결하려 해서 멈췄어요.";
  if (code === "busy") return "이미 이 모델을 준비하고 있어요.";
  return "모델을 준비하지 못했어요.";
}

function errorDetail(code: string, piece: ModelPiece | undefined): string {
  if (code === "wrong_file") return "크기나 내용이 원래 파일과 달라요. 아무것도 바뀌지 않았어요. Hugging Face에서 받은 원래 .gguf 파일을 골라 주세요.";
  if (code === "disk_full") return piece ? `약 ${mb(piece.size - piece.partial)}가 더 필요해요. 공간을 비운 뒤 다시 눌러 주세요.` : "공간을 비운 뒤 다시 눌러 주세요.";
  if (code === "checksum_mismatch" || code === "too_big") return "잘못된 파일은 쓰지 않아요. 다시 받아 주세요.";
  if (code === "network") return piece?.partial ? `받은 ${mb(piece.partial)}는 남겨 뒀어요. 연결을 확인하고 다시 누르면 이어서 받아요.` : "받은 데까지는 남겨 뒀어요. 연결을 확인하고 다시 누르면 이어서 받아요.";
  if (code === "host_not_allowed") return "모델은 Hugging Face에서만 받아요. 잠시 뒤 다시 시도해 주세요.";
  return "잠시 뒤 다시 시도해 주세요.";
}

/** A notice for an error that was already announced: shown, not said again on every repaint. */
function quietError(code: string, piece: ModelPiece | undefined): HTMLElement {
  const n = notice("danger", errorLead(code), errorDetail(code, piece));
  n.removeAttribute("role");
  return n;
}

type Paint = (setup: ModelSetup, status: Status) => Child;

/**
 * A box that paints `paint(setup, status)` and repaints when a job starts or ends or the status
 * changes. Progress only moves the bars in place. When the box had focus, it goes to the box's
 * `[data-autofocus]` element, else its first button, else `lostFocus()`.
 */
function setupBox(paint: Paint, onCleanup: (f: () => void) => void, lostFocus: () => void = () => {}): HTMLElement {
  const box = h("div", { class: "ms-box" });
  let setup: ModelSetup | null = null, left = false, asked = 0;
  const draw = () => {
    if (!setup || !current) return;
    const was = box.contains(document.activeElement) ? document.activeElement as HTMLElement : null;
    box.replaceChildren(...[paint(setup, current)].flat(3).filter((c): c is Node | string => !!c));
    if (!was) return;
    // the same control when it is still there, else the box's main one, else away from the box
    const same = [...box.querySelectorAll<HTMLElement>("button:not(:disabled), a[href], summary")].find((el) => el.tagName === was.tagName && el.textContent === was.textContent);
    const next = same ?? box.querySelector<HTMLElement>("[data-autofocus]") ?? box.querySelector<HTMLElement>("button:not(:disabled), a[href]");
    if (next) next.focus(); else lostFocus();
  };
  const load = async () => {
    const mine = ++asked; // only the newest answer paints: an older one may still name a finished job
    let got: ModelSetup;
    try { got = await api.modelSetup(); } catch {
      if (!left && mine === asked) box.replaceChildren(notice("danger", "준비 상태를 확인하지 못했어요.", "잠시 뒤 다시 시도해 주세요."));
      return;
    }
    if (left || mine !== asked) return;
    setup = got;
    // a job that started before this screen (or before a reload) keeps showing its progress
    if (setup.job && !st.job) st.job = { key: setup.job.key, kind: setup.job.kind, state: "progress", done: 0, total: 0, code: null, message: null };
    if (!setup.job && st.job) st.job = null;
    draw();
  };
  const onJob = (e: ModelEvent | null) => {
    if (left) return;
    if (e && box.querySelector(`[data-progress="${e.key}"]`)) { moveBars(box, e); return; }
    if (e) { draw(); return; } // a job started: show its bar
    void load(); // a job ended: the pieces changed (partial bytes, an error)
  };
  jobSubs.add(onJob);
  const onStat = () => { if (!left) void load(); };
  statusSubs.add(onStat);
  onCleanup(() => { left = true; jobSubs.delete(onJob); statusSubs.delete(onStat); });
  void load();
  return box;
}

function moveBars(root: HTMLElement, e: ModelEvent): void {
  for (const el of root.querySelectorAll<HTMLElement>(`[data-progress="${e.key}"]`)) {
    const p = pct(e.done, e.total);
    el.setAttribute("aria-valuenow", String(p));
    el.setAttribute("aria-valuetext", progressText(e));
    const bar = el.querySelector<HTMLElement>(".progress > i");
    if (bar) bar.style.width = `${p}%`;
    const t = el.querySelector<HTMLElement>(".ms-ptext");
    if (t) t.textContent = progressText(e);
  }
}

function progressText(e: ModelEvent): string {
  if (e.done <= 0 || e.total <= 0) return "준비하는 중…";
  const p = pct(e.done, e.total);
  return e.kind === "import" ? `확인하며 복사하는 중 · ${p}%` : `${mb(e.done)} / ${mb(e.total)} · ${p}%`;
}

/** Whether this piece can be fetched in this build: a release build without its engine cannot use it. */
const offerable = (piece: ModelPiece, setup: ModelSetup): boolean => piece.engine || piece.engine_from_env || setup.dev_build;

/** The buttons of one model, or its progress while it is being prepared. */
function actions(piece: ModelPiece, setup: ModelSetup, primary: boolean, label = "내려받기"): HTMLElement {
  const job = st.job;
  if (job && job.key === piece.key) {
    const p = pct(job.done, job.total);
    const bar = h("i");
    bar.style.width = `${p}%`;
    return h("div", { class: "ms-run" },
      h("div", { class: "ms-prog", role: "progressbar", "aria-label": `${NAME[piece.key]} ${job.kind === "import" ? "가져오는 중" : "받는 중"}`,
        "aria-valuemin": "0", "aria-valuemax": "100", "aria-valuenow": String(p), "aria-valuetext": progressText(job), data: { progress: piece.key } },
        h("div", { class: "progress" }, bar), h("span", { class: "ms-ptext num c-2 t-footnote", text: progressText(job) })),
      h("button", { type: "button", class: "btn", text: job.kind === "import" ? "그만 가져오기" : "그만 받기", "data-autofocus": "",
        onclick: () => void api.modelCancel().catch(() => { /* the job ends on its own */ }) }),
      h("span", { class: "c-3 t-footnote", text: "다른 화면으로 가도 계속돼요." }));
  }
  const other = !!job || !!setup.job; // another model is being prepared
  const msg = h("div", { role: "status" });
  const fail = (e: unknown) => {
    const err: ApiError = (e && typeof e === "object" && "code" in e) ? e as ApiError : { code: "error", message: "" };
    msg.replaceChildren(quietError(err.code, piece));
    announce(errorLead(err.code));
  };
  const resume = piece.partial > 0 && piece.partial < piece.size;
  const down = h("button", { type: "button", class: `btn${primary ? " btn-primary" : ""}`, disabled: other, "data-autofocus": "",
    text: resume ? `이어서 받기 (${mb(piece.partial)} 받음)` : piece.state === "bad" ? "다시 받기" : label,
    // aria-disabled, not disabled: the focused button keeps focus until the progress replaces it
    onclick: () => {
      if (down.getAttribute("aria-disabled") === "true") return;
      st.error = null; down.setAttribute("aria-disabled", "true");
      api.modelDownload(piece.key).catch((e) => { down.removeAttribute("aria-disabled"); fail(e); });
    } });
  const pick = h("button", { type: "button", class: "btn", disabled: other, text: "이미 받은 파일 고르기",
    onclick: () => { st.error = null; api.modelPick(piece.key).catch(fail); } });
  const err = st.error && st.error.key === piece.key ? quietError(st.error.code, piece) : null;
  return h("div", { class: "ms-acts-wrap" }, err, h("div", { class: "ms-acts" }, down, pick),
    other ? h("p", { class: "c-3 t-footnote", text: "다른 모델을 받는 중이에요. 끝나면 받을 수 있어요." }) : null, msg);
}

/** "약 742MB · NVIDIA · OpenMDW-1.1 라이선스 · Hugging Face(…)에서 받아요. 누를 때만 인터넷에 연결해요." */
function meta(piece: ModelPiece): HTMLElement {
  return h("p", { class: "ms-meta c-2 t-footnote num" },
    `약 ${mb(piece.size)} · ${piece.publisher} · ${piece.license} 라이선스 · Hugging Face(`, h("span", { class: "mono", text: piece.repo }),
    ")에서 받아요. 누를 때만 인터넷에 연결하고, 받은 파일은 원래 파일과 똑같은지 확인한 뒤에만 써요.");
}

/** The developer-only way: the binary and its variable, folded away. */
function devDetails(summary: string, ...body: Child[]): HTMLElement {
  return h("details", { class: "ms-how" }, h("summary", { text: summary }), h("div", { class: "ms-how-body c-2 t-footnote" }, ...body));
}

/** What the engine needs. A dev build finds it in the repository's build output; a release build does not carry one yet. */
function engineHelp(dev: boolean, key: ModelKey, fromEnv = false): HTMLElement {
  const e = ENGINE[key];
  if (fromEnv) {
    return h("p", { class: "c-2 t-footnote" }, "환경변수 ", h("code", { class: "mono", text: e.env }), "이 가리키는 곳에 엔진이 없어요. 고치거나 지우면 앱 옆이나 개발 빌드의 엔진을 찾아요.");
  }
  if (dev) {
    return h("div", { class: "ms-engine" },
      h("p", { class: "c-2 t-footnote", text: `${e.what}은 엔진이 있어야 돌아가요. 개발 빌드에서는 저장소에서 엔진을 한 번 빌드하면 앱이 알아서 찾아요.` }),
      h("ol", { class: "steps" },
        h("li", {}, h("span", {}, "저장소 맨 위 폴더에서 ", h("code", { class: "mono", text: "engines/nemotron-stream/fetch-libs.sh" }), "를 실행해요.")),
        h("li", {}, h("span", {}, "이어서 ", h("code", { class: "mono", text: "cd engines/nemotron-stream && cargo build" }), "를 실행해요.")),
        h("li", {}, h("span", { text: "‘다시 확인’을 눌러요." }))));
  }
  return devDetails("직접 설정하기 (개발자용)",
    h("p", {}, "엔진 실행 파일 ", h("code", { class: "mono", text: e.bin }), "을 앱 옆에 두거나 환경변수 ", h("code", { class: "mono", text: e.env }), "로 지정하면 써요. 앱을 다시 열거나 ‘다시 확인’을 눌러요."));
}

const piece = (s: ModelSetup, k: ModelKey): ModelPiece | undefined => s.pieces.find((p) => p.key === k);

/** 다시 확인 in the card: says what it found and keeps focus on itself (or on what replaced it). */
function recheckButton(): HTMLButtonElement {
  const b = h("button", { type: "button", class: "btn", text: "다시 확인", "data-autofocus": "" });
  b.addEventListener("click", async () => {
    b.setAttribute("aria-disabled", "true");
    let s: Status;
    try { s = await recheck(); } catch { b.removeAttribute("aria-disabled"); announce("상태를 확인하지 못했어요."); return; }
    if (s.live.ok) { st.finished = "stream"; announce("엔진을 찾았어요."); }
    else announce(s.live.code === "stream_engine_missing" ? "아직 엔진을 찾지 못했어요." : "엔진을 찾았어요.");
    b.removeAttribute("aria-disabled");
  });
  return b;
}

let cardHiddenNow = false;

/**
 * The calm setup card of the meeting list: what is missing for "녹음하면 자막과 초안이 나온다" and
 * the one action that fixes it. Repaints itself in place; empty when live captions work (except a
 * short "준비됐어요" right after a download).
 */
export function setupCard(onCleanup: (f: () => void) => void): HTMLElement {
  const slot = h("div", { class: "ms-slot" });
  // where focus goes when the card goes away under it: the page's main action, else its heading
  const away = () => (document.querySelector<HTMLElement>(".page-head .btn-primary") ?? document.querySelector<HTMLElement>(".page-head h1"))?.focus();
  // the "준비됐어요" notice is shown on this visit only
  onCleanup(() => { if (shownReady) st.finished = null; });
  let shownReady = false;
  const box = setupBox((s, status) => {
    if (status.live.ok) {
      if (st.finished !== "stream") return null;
      shownReady = true;
      return notice("info", "준비됐어요.", "이제 녹음하면 자막이 나오고, 끝나면 그 자막이 바로 초안이 돼요.",
        h("a", { class: "btn btn-primary", href: "#/record", text: "녹음하러 가기", "data-autofocus": "", onclick: () => { st.finished = null; } }));
    }
    if (cardHiddenNow || s.card_dismissed) return null;
    const p = piece(s, "stream");
    if (!p) return null;
    const later = h("button", { type: "button", class: "btn ms-later", text: "나중에", onclick: () => {
      cardHiddenNow = true;
      void api.setupCardDismiss().catch(() => { /* hidden for now; it comes back next time */ });
      const focused = slot.contains(document.activeElement);
      box.replaceChildren(h("p", { class: "ms-later-note c-3 t-footnote", text: "설정 › 받아쓰기 준비에서 언제든 받을 수 있어요." }));
      announce("나중에 받을게요. 설정 › 받아쓰기 준비에서 언제든 받을 수 있어요.");
      if (focused) away();
    } });
    const head = (text: string) => h("div", { class: "ms-head" }, h("h2", { id: "ms-card-h", text }), later);
    const card = (...kids: Child[]) => h("section", { class: "ms-card", "aria-labelledby": "ms-card-h" }, ...kids);
    if (p.from_env && p.state !== "ready") {
      return card(head("환경변수로 지정한 실시간 자막 모델을 쓸 수 없어요"),
        h("p", { class: "c-2" }, h("code", { class: "mono", text: "MEETKO_STREAM_MODEL" }), "이 가리키는 곳에 맞는 모델이 없어요. 고치거나 지우면 앱에서 받은 모델을 써요."),
        p.detail ? devDetails("자세한 내용 (개발자용)", h("p", { class: "mono", text: p.detail })) : null);
    }
    if (!offerable(p, s)) {
      // a release build without the engine: nothing to download that it could use
      const soft = whisperReady(status); // nothing is missing for this user: a side note, not a warning
      return card(head(soft ? "실시간 자막은 이 버전에서 아직 쓸 수 없어요" : "이 버전에서는 아직 실시간 자막을 쓸 수 없어요"),
        h("p", { class: "c-2", text: soft
          ? "녹음이 끝나면 지금처럼 Whisper로 받아쓸 수 있어요. 실시간 자막 엔진이 들어간 버전에서 여기서 바로 준비할 수 있어요."
          : "녹음은 그대로 할 수 있어요. 실시간 자막 엔진이 들어간 버전에서 여기서 바로 준비할 수 있어요." }),
        engineHelp(false, "stream"), h("div", { class: "ms-acts" }, recheckButton()));
    }
    if (p.state !== "ready") {
      const optional = whisperReady(status) && p.state === "missing";
      return card(
        head(p.state === "bad" ? "받아 둔 실시간 자막 모델이 원래 파일과 달라요" : optional ? "녹음하는 동안 자막도 볼 수 있어요" : "시작하려면 실시간 자막 모델이 필요해요"),
        h("p", { class: "c-2", text: p.state === "bad" ? "받아 둔 파일이 원래 파일과 달라서 쓰지 않아요. 다시 받거나 원래 파일을 골라 주세요."
          : optional ? "실시간 자막 모델을 받으면 녹음하는 동안 자막이 나오고, 끝나면 그 자막이 바로 초안이 돼요. 모델은 이 기기에만 저장돼요."
            : "녹음하는 동안 자막이 나오고, 녹음이 끝나면 그 자막이 바로 초안이 돼요. 모델은 이 기기에만 저장돼요." }),
        meta(p),
        actions(p, s, true, optional ? "실시간 자막 모델 받기" : "내려받기"),
        p.engine ? null : [engineHelp(s.dev_build, "stream", p.engine_from_env), h("div", { class: "ms-acts" }, recheckButton())]);
    }
    // the model is there; what is left is the engine (a dev build: release builds were handled above)
    return card(head("실시간 자막 엔진이 필요해요"), engineHelp(s.dev_build, "stream", p.engine_from_env), h("div", { class: "ms-acts" }, recheckButton()));
  }, onCleanup, away);
  slot.append(box);
  return slot;
}

function stateTag(text: string, kind: "ok" | "warn" | "danger" | "none"): HTMLElement {
  return h("span", { class: `tag${kind === "none" ? "" : ` tag-${kind}`}`, text });
}

/** One line of the settings list: name, state, what it is for, then its actions or steps. */
function pieceRow(name: string, tag: HTMLElement, desc: string, ...rest: Child[]): HTMLElement {
  return h("div", { class: "ms-row", role: "listitem" }, h("div", { class: "ms-row-h" }, h("h3", { text: name }), tag), h("p", { class: "c-2 t-footnote", text: desc }), ...rest);
}

/** The model part of a settings row: where it comes from, or how to get it. */
function modelBody(p: ModelPiece, s: ModelSetup, envVar: string, primary: boolean): Child {
  if (p.from_env) {
    return [h("p", { class: "c-3 t-footnote" }, "환경변수 ", h("code", { class: "mono", text: envVar }),
      p.state === "ready" ? "로 지정한 모델을 써요." : "이 가리키는 곳에 맞는 모델이 없어요. 고치거나 지우면 앱에서 받은 모델을 써요."),
      p.state !== "ready" && p.detail ? devDetails("자세한 내용 (개발자용)", h("p", { class: "mono", text: p.detail })) : null];
  }
  if (p.state === "ready") return h("p", { class: "c-3 t-footnote num", text: `앱이 받아 둔 모델 · 약 ${mb(p.size)} · ${p.license}` });
  // the stream engine has its own row with the details; the diarization engine is only named here
  if (!offerable(p, s)) return [h("p", { class: "c-3 t-footnote", text: "엔진이 준비되면 받을 수 있어요." }), p.key === "diarization" ? engineHelp(false, p.key) : null];
  return [p.state === "bad" ? h("p", { class: "c-2 t-footnote", text: "받아 둔 파일이 원래 파일과 달라서 쓰지 않아요. 다시 받거나 원래 파일을 골라 주세요." }) : null,
    meta(p), actions(p, s, primary)];
}

function modelTag(p: ModelPiece, s: ModelSetup, optional: boolean): HTMLElement {
  if (p.state === "ready") return p.engine ? stateTag("준비됨", "ok") : stateTag("엔진 필요", "warn");
  if (p.state === "bad") return stateTag("확인 필요", "danger");
  if (!offerable(p, s)) return stateTag("이 버전에서는 안 돼요", "none");
  return optional ? stateTag("선택", "none") : stateTag("필요", "warn");
}

/** Settings: each piece as 준비됨 / 필요 / 선택, with the same actions as the card. */
export function setupSection(onCleanup: (f: () => void) => void): HTMLElement {
  const box = setupBox((s, status) => {
    const rows: Child[] = [];
    const p = piece(s, "stream");
    if (p) {
      rows.push(pieceRow(NAME.stream, modelTag(p, s, false), "녹음하는 동안 자막을 만들고, 녹음이 끝나면 그 자막이 바로 초안이 돼요.", modelBody(p, s, "MEETKO_STREAM_MODEL", true)));
      rows.push(pieceRow("실시간 자막 엔진", p.engine ? stateTag("준비됨", "ok") : stateTag("필요", "warn"), "모델을 돌리는 프로그램이에요.",
        p.engine ? null : s.dev_build || p.engine_from_env ? engineHelp(s.dev_build, "stream", p.engine_from_env) : [h("p", { class: "c-3 t-footnote", text: "이 버전에는 실시간 자막 엔진이 들어 있지 않아요." }), engineHelp(false, "stream")]));
    }
    const d = piece(s, "diarization");
    if (d) {
      rows.push(pieceRow(NAME.diarization, modelTag(d, s, true), "여러 사람이 말한 녹음에서 누가 말했는지 나눠요. 없어도 ‘나’와 ‘상대’로는 나뉘어요.",
        modelBody(d, s, "MEETKO_DIAR_MODEL", false),
        d.engine || !(s.dev_build || d.engine_from_env) ? null : engineHelp(s.dev_build, "diarization", d.engine_from_env)));
    }
    rows.push(whisperRow(status));
    return h("div", { class: "ms-list", role: "list" }, ...rows);
  }, onCleanup);
  return h("section", { class: "set-sec" }, h("h2", { text: "받아쓰기 준비" }), box);
}

/** whisper stays manual: the app does not download programs. Optional, for 다시 받아쓰기 only. */
function whisperRow(status: Status): HTMLElement {
  const ready = whisperReady(status);
  const code = (t: string) => h("code", { class: "mono", text: t });
  const steps = h("details", { class: "ms-how" }, h("summary", { text: "설정하는 방법" }),
    h("ol", { class: "steps" },
      h("li", {}, h("span", {}, "whisper.cpp를 설치해요. macOS: 터미널에서 ", code("brew install whisper-cpp"), "를 실행해요. Windows: whisper.cpp 릴리스에서 whisper-cli.exe를 받아요.")),
      h("li", {}, h("span", {}, "Whisper 모델 파일을 받아요. 예: Hugging Face ", code("ggerganov/whisper.cpp"), "의 ", code("ggml-large-v3-turbo.bin"), "(약 1.6GB, MIT 라이선스).")),
      h("li", {}, h("span", {}, "데이터 폴더의 config.toml에 ", code('[whisper] model = "모델 파일 경로"'), "를 적어요. 데이터 폴더는 macOS: ", code("~/Library/Application Support/meetko"), ", Windows: ", code("%LOCALAPPDATA%\\meetko"), "예요.")),
      h("li", {}, h("span", {}, "앱은 whisper-cli를 PATH나 앱 옆에서 찾아요. macOS: Finder에서 연 앱은 Homebrew 폴더를 모를 수 있어서, 터미널에서 앱을 열거나 환경변수 ", code("MEETKO_WHISPER_CLI"), "로 경로를 지정해요.")),
      h("li", {}, h("span", { text: "아래의 ‘다시 확인’을 눌러요." }))));
  const missing = !status.model.ok ? "Whisper 모델이 아직 없어요." : !status.whisper_cli.ok ? "whisper-cli를 아직 찾지 못했어요." : null;
  return pieceRow("다시 받아쓰기용 Whisper", ready ? stateTag("준비됨", "ok") : stateTag("선택", "none"),
    "녹음 전체를 더 정확하게 다시 받아 적을 때만 써요. 없어도 실시간 자막이 초안이 돼요.",
    ready ? null : [missing ? h("p", { class: "c-3 t-footnote", text: missing }) : null, steps]);
}
