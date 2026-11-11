import { api, asError, batchNeed, batchReady, engineName, type CaptureDone, type CaptureEvent, type ProviderError, type Status } from "../api";
import { announce, h, mmss, notice } from "../dom";
import { captureBus, liveBus } from "../jobs";
import { errorHeading, gapReason, warning } from "../labels";
import { currentStatus, onStatus, recheck } from "../setup";

// The recorder is a singleton: a recording keeps going while the user looks at other screens.
type Phase = "idle" | "starting" | "recording" | "stopping";
interface Gap { channel: number; start_ms: number; end_ms: number; reason: string }
interface Rec {
  phase: Phase; startedAt: number | null; levels: [number, number]; peaks: [number, number, number, number];
  gaps: Gap[]; errors: ProviderError[]; done: CaptureDone | null; failure: string | null;
  captions: [Map<number, Cap>, Map<number, Cap>]; liveLost: [boolean, boolean];
  /** the caption engine listens (or a caption came): until then the lane says it is getting ready */
  liveReady: [boolean, boolean];
  /** provisional speaker number of each caption segment (live speaker separation); absent = unknown */
  who: [Map<number, number>, Map<number, number>];
}
/** One caption segment; a later revision replaces the earlier one entirely, a final never changes. */
interface Cap { revision: number; final: boolean; text: string }
const fresh = (): Pick<Rec, "captions" | "liveLost" | "liveReady" | "who"> => ({ captions: [new Map(), new Map()], liveLost: [false, false], liveReady: [false, false], who: [new Map(), new Map()] });
const rec: Rec = { phase: "idle", startedAt: null, levels: [0, 0], peaks: [0, 0, 0, 0], gaps: [], errors: [], done: null, failure: null, ...fresh() };
const capSubs = new Set<(channel: number, segment: number) => void>();
/** speakers changed on a channel: the lane redraws its markers (at most once a frame) */
const whoSubs = new Set<(channel: number) => void>();
/** set while the record screen is on screen: a saved live version opens the meeting only then */
let openMeeting: ((id: string) => void) | null = null;
const subs = new Set<() => void>();
const notify = () => subs.forEach((f) => f());
export const isRecording = (): boolean => rec.phase !== "idle";
/** For the meeting prompt: start or stop as if the record button had been pressed (no-op in any other phase). */
export const startRecording = (): Promise<void> => (rec.phase === "idle" ? start() : Promise.resolve());
export const stopRecording = (): Promise<void> => (rec.phase === "recording" ? stop() : Promise.resolve());
/** Resolves true once the provider reports "started", false if the start ends in idle. */
export function recordingStarted(): Promise<boolean> {
  return new Promise((done) => {
    const check = () => { if (rec.phase === "recording" || rec.phase === "idle") { off(); done(rec.phase === "recording"); } };
    const off = onRecorderChange(check);
    check();
  });
}
export function onRecorderChange(f: () => void): () => void { subs.add(f); return () => subs.delete(f); }

const NAMES = [["나", "마이크"], ["상대", "시스템 오디오"]] as const; // mic, loopback (app-layer names)
const PEAK_HOLD_MS = 1500;
/** dBFS from -60 to 0 reads far better than linear rms (speech sits around 0.01-0.1) */
const db = (rms: number) => 20 * Math.log10(Math.max(rms, 1e-6));
const unit = (rms: number) => Math.max(0, Math.min(1, (db(rms) + 60) / 60));

liveBus.on((e) => {
  if (rec.phase === "idle" || e.channel > 1) return;
  if (e.event === "error") { rec.liveLost[e.channel] = true; notify(); return; } // quiet: the lane says so, the recording goes on
  if (e.event === "ready") { if (!rec.liveReady[e.channel]) { rec.liveReady[e.channel] = true; notify(); } return; }
  if (e.event === "speakers") {
    const who = rec.who[e.channel];
    for (const l of e.labels) { if (l.speaker === null) who.delete(l.segment); else who.set(l.segment, l.speaker); }
    whoSubs.forEach((f) => f(e.channel));
    return;
  }
  const seg = rec.captions[e.channel], old = seg.get(e.segment);
  if (old && (old.final || old.revision >= e.revision)) return;
  seg.set(e.segment, { revision: e.revision, final: e.final, text: e.text });
  const first = !rec.liveReady[e.channel];
  rec.liveReady[e.channel] = true;
  capSubs.forEach((f) => f(e.channel, e.segment));
  if (first) notify();
});

captureBus.on((e: CaptureEvent) => {
  switch (e.event) {
    case "started": rec.phase = "recording"; rec.startedAt = Date.now(); announce("녹음을 시작했습니다"); break;
    case "level":
      if (e.channel < 2) {
        rec.levels[e.channel] = e.rms;
        const [p, at] = [rec.peaks[e.channel * 2], rec.peaks[e.channel * 2 + 1]], u = unit(e.rms), now = Date.now();
        if (u >= p || now - at > PEAK_HOLD_MS) { rec.peaks[e.channel * 2] = u; rec.peaks[e.channel * 2 + 1] = now; }
      }
      break;
    case "gap": {
      rec.gaps.push({ channel: e.channel, start_ms: e.start_ms, end_ms: e.end_ms, reason: e.reason });
      announce(`${NAMES[e.channel]?.[0] ?? e.channel} 채널 ${mmss(e.start_ms)}부터 끊김 (${gapReason(e.reason)})`);
      break;
    }
    case "error": rec.errors.push({ code: e.code, message: e.message, recoverable: e.recoverable }); announce(`${errorHeading(e.code)}. ${e.message}`); break;
    case "exited": if (rec.phase !== "stopping") void stop(); return; // the provider ended by itself: finalize what it left
    default: return;
  }
  notify();
});

/** Whether live captions were set up when this recording started: the backend decides at capture
 * start, so a model that becomes ready mid-recording only counts from the next one. */
let liveAtStart: boolean | null = null;

async function start(): Promise<void> {
  liveAtStart = currentStatus()?.live.ok ?? null;
  Object.assign(rec, { phase: "starting", startedAt: null, levels: [0, 0], peaks: [0, 0, 0, 0], gaps: [], errors: [], done: null, failure: null, ...fresh() });
  notify();
  try { await api.captureStart(); } catch (e) { rec.phase = "idle"; rec.failure = asError(e).message; notify(); }
}

async function stop(): Promise<void> {
  rec.phase = "stopping";
  notify();
  try {
    rec.done = await api.captureStop();
    rec.errors = rec.done.provider_errors; // the complete list, including errors the UI missed
    announce(rec.done.meeting_id ? "녹음을 저장했습니다" : "녹음을 저장하지 못했습니다");
  } catch (e) { rec.failure = asError(e).message; }
  rec.phase = "idle";
  rec.startedAt = null;
  notify();
  // the captions were saved as a `live` version and become the first draft (started by capture_stop): show it
  if (rec.done?.meeting_id && rec.done.live_version_id) openMeeting?.(rec.done.meeting_id);
}

const MAX_SEGMENTS_SHOWN = 200;
/** One caption lane. Partials read in the tertiary colour, finals in the primary one; text goes in by textContent. */
function lane(i: 0 | 1): { el: HTMLElement; paint(): void; update(segment: number): void; relabelSoon(): void } {
  const log = h("div", { class: "cap-scroll", role: "log", "aria-label": `${NAMES[i][0]} 실시간 자막`, "aria-live": "off", tabindex: "0" });
  const empty = h("span", { class: "cap-empty" });
  const el = h("section", { class: "cap-lane" }, h("div", { class: "cap-h t-footnote c-2", text: NAMES[i][0] }), log);
  const spans = new Map<number, HTMLElement>();
  /** speaker markers ("상대 1"), each before the first segment of a run of the same speaker */
  const markers = new Map<number, HTMLElement>();
  let follow = true; // paged-follow in spirit: stay at the end until the user scrolls up, resume when back at the end
  log.addEventListener("scroll", () => { follow = log.scrollHeight - log.scrollTop - log.clientHeight < 24; });
  const stick = () => { if (follow) log.scrollTop = log.scrollHeight; };
  const put = (seg: number) => {
    const cap = rec.captions[i].get(seg);
    if (!cap) return;
    let sp = spans.get(seg);
    if (!sp) {
      sp = h("span", { class: "cap" });
      spans.set(seg, sp);
      log.insertBefore(sp, empty.parentNode === log ? empty : null);
      if (spans.size > MAX_SEGMENTS_SHOWN) {
        const [k, old] = spans.entries().next().value as [number, HTMLElement];
        old.remove(); spans.delete(k); markers.get(k)?.remove(); markers.delete(k);
        relabelSoon(); // the run that began there needs its marker on the next segment
      }
    }
    sp.textContent = cap.text;
    sp.classList.toggle("fin", cap.final);
  };
  // Provisional speakers may change while the separation learns: markers are redrawn in place, at
  // most once a frame, and never announced (the log is aria-live off).
  let whoFrame = 0;
  const relabel = () => {
    whoFrame = 0;
    const who = rec.who[i];
    let prev: number | undefined;
    for (const [seg, sp] of spans) {
      const s = who.get(seg);
      const want = s !== undefined && s !== prev ? `${NAMES[i][0]} ${s}` : null;
      if (s !== undefined) prev = s;
      let m = markers.get(seg);
      if (!want) { if (m) { m.remove(); markers.delete(seg); } continue; }
      if (!m) { m = h("span", { class: "cap-who" }); markers.set(seg, m); }
      if (m.nextSibling !== sp) log.insertBefore(m, sp);
      if (m.textContent !== want) m.textContent = want;
    }
    stick();
  };
  const relabelSoon = () => { if (!whoFrame) whoFrame = requestAnimationFrame(relabel); };
  return {
    el,
    relabelSoon,
    paint() {
      if (spans.size === 0) { for (const seg of [...rec.captions[i].keys()].sort((x, y) => x - y)) put(seg); relabel(); } // the screen was opened again mid-recording
      const waiting = rec.phase === "recording" || rec.phase === "starting";
      const msg = rec.liveLost[i] ? "자막이 멈췄어요. 녹음은 계속돼요."
        : spans.size === 0 && waiting && !rec.liveReady[i] ? "자막 준비 중…"
        : spans.size === 0 && rec.phase === "recording" ? "말하면 여기에 자막이 나와요" : "";
      empty.textContent = msg;
      empty.classList.toggle("lost", rec.liveLost[i]);
      if (msg) log.append(empty); else empty.remove();
      stick();
    },
    update(seg: number) {
      put(seg);
      if (rec.who[i].has(seg)) relabelSoon(); // its speaker came before the caption did
      if (!rec.liveLost[i]) empty.remove();
      stick();
    },
  };
}

function meter(i: 0 | 1): { el: HTMLElement; set(): void } {
  const fill = h("i"), peak = h("b"), dbEl = h("span", { class: "db num" });
  const bar = h("div", { class: "meter-bar" }, fill, peak);
  const el = h("div", { class: "meter" }, h("div", { class: "nm" }, NAMES[i][0], h("small", { text: NAMES[i][1] })), bar, dbEl);
  return {
    el,
    set() {
      const on = rec.phase === "recording", rms = rec.levels[i], silent = on && rms < 1e-4;
      el.classList.toggle("silent", silent);
      fill.style.width = `${on ? Math.round(unit(rms) * 100) : 0}%`;
      fill.className = unit(rms) > 0.85 ? "hot" : "";
      peak.hidden = !on || silent;
      peak.style.left = `${Math.round(rec.peaks[i * 2] * 100)}%`;
      dbEl.textContent = !on ? "—" : silent ? "무음" : `${Math.round(db(rms))} dB`;
      bar.setAttribute("role", "meter");
      bar.setAttribute("aria-label", `${NAMES[i][0]} 입력 크기`);
      bar.setAttribute("aria-valuetext", dbEl.textContent);
    },
  };
}

/** Setup note when live captions cannot run. Recording itself is unaffected, and the note says so. */
function liveSetupNote(code: string | null, dev: boolean): HTMLElement {
  const open = h("a", { class: "btn", href: "#/settings", text: "설정 열기" });
  // a release build without the engine: nothing the user can download fixes it, so no promise of captions
  if (code === "stream_engine_missing" && !dev) return notice("info", "이 버전에는 실시간 자막 엔진이 없어요.", "녹음은 그대로 할 수 있어요.");
  if (code === "stream_engine_missing") return notice("info", "실시간 자막 엔진을 찾지 못했어요.", "녹음은 그대로 할 수 있어요. 엔진을 준비하는 방법은 설정에 있어요.", open);
  if (code === "stream_engine_invalid") return notice("info", "MEETKO_STREAM_ENGINE이 가리키는 엔진을 찾지 못했어요.", "녹음은 그대로 할 수 있어요. 환경변수를 고치거나 지우면 앱 옆이나 개발 빌드의 엔진을 찾아요.");
  if (code === "stream_model_bad") return notice("info", "받아 둔 실시간 자막 모델이 원래 파일과 달라요.", "녹음은 그대로 할 수 있어요. 설정에서 다시 받거나 원래 파일을 골라 주세요.", open);
  if (code === "stream_model_missing") return notice("info", "실시간 자막 모델이 아직 없어요.", "녹음은 그대로 할 수 있어요. 설정에서 모델을 받으면 다음 녹음부터 자막이 나와요.", open);
  return notice("info", "실시간 자막을 쓸 수 없어요.", "녹음은 그대로 할 수 있어요.", open);
}

export function recordScreen(main: HTMLElement, status: Status, open: (meetingId: string) => void, onCleanup: (f: () => void) => void = () => {}): () => void {
  const btn = h("button", { type: "button", class: "rec-btn" }, h("i"));
  const time = h("div", { class: "rec-time num", text: "00:00" });
  const stat = h("div", { class: "rec-status" });
  const meters = [meter(0), meter(1)];
  const extra = h("div", { class: "rec-extra" });
  let ready = status.capture.ok;
  let liveOk = status.live.ok;
  // while a recording runs, what it started with; the new status applies once it is back to idle
  const live = (): boolean => (rec.phase !== "idle" && liveAtStart !== null ? liveAtStart : liveOk);
  let whisperOk = batchReady(status); // an engine for 다시 받아쓰기 is ready (the chosen one, or the one 자동 or the fallback picks)
  const lanes = [lane(0), lane(1)];
  const caps = h("div", { class: "caps", hidden: true }, ...lanes.map((l) => l.el));
  const offCaps = (() => {
    const f = (c: number, seg: number) => lanes[c]?.update(seg), g = (c: number) => lanes[c]?.relabelSoon();
    capSubs.add(f); whoSubs.add(g);
    return () => { capSubs.delete(f); whoSubs.delete(g); };
  })();
  openMeeting = open;
  let tick: ReturnType<typeof setInterval> | undefined, off: () => void = () => {};
  // registered before anything below can throw (draw, DOM building)
  const teardown = () => { clearInterval(tick); off(); offCaps(); openMeeting = null; };
  onCleanup(teardown);
  let lastSig = "";
  const hint = h("p", { class: "c-2 t-footnote" });
  const paintHint = () => {
    hint.textContent = `${live()
      ? `녹음 중에 실시간 자막이 나오고, 끝나면 자막이 바로 초안이 돼요.${!whisperOk ? ""
        : status.auto_retranscribe
        ? ` 이어서 ${engineName(status.batch_engine)}로 녹음 전체를 더 정확하게 다시 받아 적어 새 초안을 만들어요. 10분 회의에 몇 분 걸리고, 그동안 실시간 자막 초안을 읽을 수 있어요.`
        : " 더 정확한 결과가 필요하면 회의에서 ‘다시 받아쓰기’를 누르세요."}`
      : "회의가 끝난 뒤에 받아써요. 이 설정에서는 녹음 중에 자막이 나오지 않아요."} 원본 오디오는 이 기기에만 저장돼요. ‘나’는 마이크, ‘상대’는 시스템 소리예요. 스피커로 들으면 상대 목소리가 마이크에도 섞이니 이어폰을 권해요.`;
  };
  paintHint();
  // a model set up meanwhile (or 다시 확인 elsewhere) shows here without leaving the screen
  onStatus((s) => { status = s; ready = s.capture.ok; liveOk = s.live.ok; whisperOk = batchReady(s); lastSig = ""; paintHint(); draw(); }, onCleanup);
  let lastPhase = rec.phase;

  btn.addEventListener("click", () => void (rec.phase === "idle" ? start() : rec.phase === "recording" ? stop() : undefined));

  const draw = () => {
    // back to idle after a recording: read the setup again (the next recording uses whatever is there now)
    if (rec.phase === "idle" && lastPhase !== "idle") void recheck().catch(() => {});
    if (rec.phase !== lastPhase) { lastPhase = rec.phase; paintHint(); }
    const on = rec.phase === "recording";
    btn.disabled = rec.phase === "starting" || rec.phase === "stopping" || (rec.phase === "idle" && !ready);
    btn.setAttribute("aria-pressed", String(on));
    btn.setAttribute("aria-label", on ? "녹음 정지" : "녹음 시작");
    stat.textContent = !ready ? "녹음할 수 없어요" : { idle: "준비됨", starting: "시작하는 중…", recording: "녹음 중 · 두 채널 기록", stopping: "저장하는 중…" }[rec.phase];
    meters.forEach((m) => m.set());
    // the lanes exist only for a recording that has (or had) captions; no model means no lanes, just a note below
    const showCaps = live() && (rec.phase !== "idle" || rec.captions[0].size + rec.captions[1].size > 0);
    caps.hidden = !showCaps;
    if (showCaps) lanes.forEach((l) => l.paint());
    // level events arrive many times a second: only rebuild the notices when they changed
    const sig = `${live()}|${status.live.code}|${rec.phase}|${rec.errors.length}|${rec.gaps.length}|${rec.failure}|${rec.done?.meeting_id}|${rec.done?.live_note}`;
    if (sig === lastSig) return;
    lastSig = sig;
    extra.replaceChildren();
    if (!ready) extra.append(notice("danger", "녹음 프로그램을 찾지 못했어요.", `환경변수 MEETKO_CAPTURE_BIN에 경로를 지정하거나 앱 옆에 두세요. (${status.capture.detail ?? ""})`));
    for (const e of rec.errors) extra.append(notice(e.recoverable ? "warn" : "danger", errorHeading(e.code), h("div", { class: "fix", text: e.message }))); // the provider's own words name the exact OS setting
    if (!live()) extra.append(liveSetupNote(rec.phase === "idle" ? status.live.code : null, status.dev_build));
    if (rec.failure) extra.append(notice("danger", "녹음을 시작하거나 마치지 못했어요.", rec.failure));
    const lastGap = rec.gaps[rec.gaps.length - 1];
    if (lastGap) {
      const ch = NAMES[lastGap.channel]?.[0] ?? String(lastGap.channel);
      extra.append(notice("warn", `${ch} 채널이 끊겼어요.`, `${mmss(lastGap.start_ms)}부터 ${mmss(lastGap.end_ms)}까지 데이터가 들어오지 않았어요(${gapReason(lastGap.reason)}). 기록은 계속되고, 이 구간은 녹음되지 않은 구간으로 표시돼요.`));
      extra.append(h("div", { class: "gaplist" }, h("div", { class: "t-footnote c-2", text: "끊긴 구간" }),
        ...rec.gaps.slice(-5).map((g, i, all) => h("div", { class: `tag${i === all.length - 1 ? " tag-warn" : ""}` }, `${NAMES[g.channel]?.[0] ?? g.channel} · ${mmss(g.start_ms)} – ${mmss(g.end_ms)} · ${gapReason(g.reason)}`))));
    }
    const d = rec.done;
    if (d?.meeting_id) {
      const id = d.meeting_id;
      extra.append(notice("info", d.existing ? "이미 가져온 녹음이에요." : "녹음을 저장했어요.", "", h("button", { type: "button", class: "btn btn-primary", text: "회의 열기", onclick: () => open(id) })));
      for (const w of d.warnings) extra.append(notice("warn", "", warning(w)));
      if (d.live_note) extra.append(notice("info", "실시간 자막은 저장하지 못했어요.", whisperOk
        ? "녹음은 그대로 저장됐어요. 회의를 열어 받아쓰면 전사본을 만들 수 있어요."
        : `녹음은 그대로 저장됐어요. 전사본을 만들려면 설정에서 ${batchNeed(status)}을 설정해 주세요.`));
    } else if (d?.error) extra.append(notice("danger", "저장하지 못했어요.", d.error));
  };

  tick = setInterval(() => { time.textContent = rec.startedAt ? mmss(Date.now() - rec.startedAt) : "00:00"; }, 250);
  off = onRecorderChange(draw);
  draw();

  main.replaceChildren(h("div", { class: "page" }, h("div", { class: "rec" },
    h("div", { class: "page-head" }, h("h1", { class: "t-title1", text: "녹음" })),
    h("div", { class: "rec-top" }, btn, h("div", {}, time, stat)),
    h("div", { class: "meters" }, ...meters.map((m) => m.el)),
    caps,
    hint,
    extra)));
  return teardown;
}
