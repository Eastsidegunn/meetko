// The meeting prompt: a small card above every screen when the backend thinks a meeting started
// (or ended while recording). It only asks; recording starts or stops when the user clicks.
import { api, onMeetingPrompt, type MeetingOffer } from "./api";
import { announce, h, icon } from "./dom";
import { recordingStarted, startRecording, stopRecording } from "./screens/record";

// first in the tab order (inserted before #app), so the keyboard reaches it at once
const card = h("section", { class: "mprompt", role: "region", "aria-label": "회의 알림", hidden: true });
/** the newest view shown; an older or equal one (event vs. the boot query) is ignored */
let seq = -1;
/** where focus was before it went into the card, to put it back when the card closes */
let lastFocus: HTMLElement | null = null;
/** focus is (or was, before a button got disabled and blurred) inside the card */
let focusInCard = false;
let captureReady: () => boolean = () => true;

function setShown(on: boolean): void {
  card.hidden = !on;
  document.body.classList.toggle("has-mprompt", on);
  if (on) document.documentElement.style.setProperty("--mprompt-h", `${card.offsetHeight + 40}px`);
  else {
    card.replaceChildren();
    if (focusInCard) { focusInCard = false; restoreFocus(); }
  }
}

function restoreFocus(): void {
  const back = lastFocus?.isConnected ? lastFocus : null;
  if (back) { back.focus(); if (document.activeElement === back) return; }
  focusMain();
}

/** The record button if it can take focus, else the screen's title, else the main area. */
function focusMain(): void {
  const main = document.querySelector<HTMLElement>("main");
  const target = document.querySelector<HTMLButtonElement>(".rec-btn:not(:disabled)") ?? main?.querySelector<HTMLElement>("h1") ?? main;
  if (!target) return;
  if (!(target instanceof HTMLButtonElement)) target.tabIndex = -1;
  target.focus();
}

/** Go to the record screen and let it render (routes render asynchronously). */
async function toRecordScreen(): Promise<void> {
  if (location.hash !== "#/record") location.hash = "#/record";
  await new Promise((r) => setTimeout(r, 0));
}

function show(o: MeetingOffer): void {
  if (o.seq <= seq) return;
  seq = o.seq;
  const view = o.seq;
  if (o.offer === "none" || !o.app) { setShown(false); return; }
  const start = o.offer === "start";
  const ready = !start || captureReady();
  const lead = start ? "회의가 시작된 것 같아요. 녹음할까요?" : "회의가 끝난 것 같아요. 녹음을 정지할까요?";
  const detail = start
    ? `${o.app}에서 마이크를 쓰고 있어요.${o.browser ? " 웹 회의일 수 있어요." : ""}${ready ? "" : " 녹음 프로그램을 찾지 못해서 지금은 녹음할 수 없어요."}`
    : `${o.app}에서 마이크를 더 이상 쓰지 않아요.`;
  const err = h("div", { class: "mprompt-err", role: "status" });
  const btns: HTMLButtonElement[] = [];
  const busy = (on: boolean) => btns.forEach((b) => { b.disabled = on; });
  const act = (f: () => Promise<boolean>, failText: string) => async () => {
    focusInCard = card.contains(document.activeElement) || focusInCard; // read before disabling blurs the button
    busy(true); err.textContent = "";
    let ok = false;
    try { ok = await f(); } catch { ok = false; }
    if (ok || seq !== view) return; // done, or a newer view replaced this one
    busy(false); err.textContent = failText;
    btns[0]?.focus();
  };
  const answer = (a: "yes" | "no" | "hide") => api.meetingPromptAnswer(view, a);
  const yes = act(async () => {
    await toRecordScreen();
    if (start) {
      await startRecording();
      // only a recording the provider reports as started answers "yes"; a failed start leaves the card up
      if (!(await recordingStarted())) return false;
      await answer("yes").catch(() => {});
    } else {
      await answer("yes").catch(() => {});
      await stopRecording();
    }
    if (seq === view) setShown(false);
    focusMain();
    return true;
  }, start ? "녹음을 시작하지 못했어요. 녹음 화면에서 이유를 확인해 주세요." : "응답을 보내지 못했어요. 다시 눌러 주세요.");
  const openRecord = act(async () => { await toRecordScreen(); if (seq === view) setShown(false); focusMain(); return true; }, ""); // no answer: nothing is decided
  const no = act(async () => { await answer("no"); if (seq === view) setShown(false); return true; }, "응답을 보내지 못했어요. 다시 눌러 주세요.");
  const primary = !ready
    ? h("button", { type: "button", class: "btn btn-primary", text: "녹음 화면 열기", onclick: openRecord })
    : h("button", { type: "button", class: "btn btn-primary", text: start ? "녹음 시작" : "녹음 정지", onclick: yes });
  btns.push(primary, h("button", { type: "button", class: "btn", text: start ? "이번엔 안 함" : "계속 녹음", onclick: no }));
  card.replaceChildren(icon(start ? "record" : "info", 16), h("div", { class: "grow" },
    h("strong", { text: lead }), h("div", { class: "c-2", text: detail }), h("div", { class: "acts" }, ...btns), err,
    // it is on by default: say once, quietly, where it is turned off
    start ? h("p", { class: "mprompt-off" }, "회의 알림은 ", h("a", { href: "#/settings", text: "설정" }), "에서 끌 수 있어요.") : null));
  card.classList.toggle("is-start", start);
  setShown(true);
  if (focusInCard) primary.focus(); // re-rendered while the user was in the card: stay in it
  announce(lead);
}

export async function initMeetingPrompt(ready: () => boolean): Promise<void> {
  captureReady = ready;
  document.body.insertBefore(card, document.getElementById("app"));
  document.addEventListener("focusin", (e) => {
    const inCard = e.target instanceof Node && card.contains(e.target);
    focusInCard = inCard;
    if (!inCard && e.target instanceof HTMLElement) lastFocus = e.target;
  });
  // Escape: not now, for this meeting only (no cooldown); the backend frees the slot for other apps
  card.addEventListener("keydown", (e) => {
    if (e.key !== "Escape" || card.hidden) return;
    e.preventDefault();
    void api.meetingPromptAnswer(seq, "hide").catch(() => {});
    setShown(false);
  });
  await onMeetingPrompt(show);
  try { show((await api.meetingWatchGet()).offer); } catch { /* no prompt then */ } // an offer made before the page loaded
}
