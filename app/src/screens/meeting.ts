// The meeting screen: toolbar (version, transcribe, approve), transport + timeline, the
// transcript, and the inspector (results / provenance). It owns the screen's state; the pieces
// it composes (player, transcript, results, provenance) are separate modules.
import { api, asError, whisperNeed, whisperReady, type Anchor, type MeetingData, type MeetingDetail, type PluginInfo, type ResultView, type Status, type VersionView } from "../api";
import { announce, dateText, h, icon, kindName, mmss, notice } from "../dom";
import { apiErrorText, liveDraftErrorText } from "../labels";
import { pluginBus, pluginRunning, transcribeBus, transcribing } from "../jobs";
import { prepare, roleLabel, type Model } from "../model";
import { Player, type Channel } from "../player";
import { buildPlayer, type PlayerUi } from "../player-ui";
import { closePop, confirmDialog, openPop } from "../pop";
import { exportControl } from "../export-menu";
import { renderProvenance } from "../provenance";
import { renderResults, type Failure } from "../results";
import { buildTranscript, wordInfoText } from "../transcript";
import { takeJump } from "../jump";

/** head = the latest approved version, else the latest draft (the core's definition of stale) */
function headOf(vs: VersionView[]): VersionView | undefined {
  return [...vs].reverse().find((v) => v.kind === "approved") ?? [...vs].reverse().find((v) => v.kind === "draft") ?? vs[vs.length - 1];
}

export async function meetingScreen(main: HTMLElement, meetingId: string, status: Status, onCleanup: (f: () => void) => void = () => {}): Promise<() => void> {
  const jump = takeJump(meetingId); // a search result asked to open at these words
  let detail: MeetingDetail, plugins: PluginInfo[];
  try { [detail, plugins] = await Promise.all([api.getMeeting(meetingId), api.listPlugins()]); } catch (e) {
    main.replaceChildren(h("div", { class: "page" }, h("div", { class: "page-in" }, notice("danger", "회의를 열지 못했어요.", asError(e).message), h("a", { class: "btn", href: "#/meetings", text: "회의 목록" }))));
    return () => {};
  }

  // "나만" / "상대만" are the mic / loopback channel files; "둘 다" is the server-side sum
  const sources: Partial<Record<Channel, string>> = { both: "mix" };
  for (const c of detail.channels) if (c.role === "mic") sources.me = String(c.index); else if (c.role === "loopback") sources.other = String(c.index);
  const player = new Player(meetingId, detail.duration_ms, sources);
  // set by teardown: every async path that paints checks it after an await, so a load that was
  // still in flight when the user left cannot rebuild the UI around the destroyed player
  let disposed = false;
  const channelOf = (index: number): Channel => (sources.me === String(index) ? "me" : sources.other === String(index) ? "other" : "both");

  let versionId = "", model: Model | null = null, data: MeetingData | null = null;
  let results: ResultView[] = [], staleElsewhere: ResultView[] = [];
  let tab: "results" | "prov" = "results";
  let failures: Failure[] = [];
  let loadErr: string | null = null; // the meeting or version could not be loaded: say so instead of showing "not transcribed"
  let flash: { kind: "warn" | "danger"; lead: string; text: string; settings?: boolean } | null = null;
  let tr: ReturnType<typeof buildTranscript> | null = null, ui: PlayerUi | null = null;
  const ready = whisperReady(status), need = whisperNeed(status) ?? "";

  // ---------- frame ----------
  const barSlot = h("header", { class: "mt-bar" });
  const plSlot = h("div");
  const notices = h("div", { class: "tr-notices" });
  const trSlot = h("div", { class: "tr-slot" });
  const followWrap = h("div", { class: "follow-wrap" });
  const followBtn = h("button", { type: "button", class: "follow" }, icon("arrowdown", 14), "다시 따라가기");
  const footInfo = h("span", { class: "num" }), footR = h("span");
  const foot = h("div", { class: "tr-foot" }, footInfo, h("span", { class: "sp" }), footR);
  const body = h("div", { class: "ins-body", role: "tabpanel" });
  const tabs = h("div", { class: "seg", role: "tablist", "aria-label": "보기" });
  const tabBtns = (["results", "prov"] as const).map((k) => {
    const b = h("button", { type: "button", class: "btn", role: "tab", text: k === "results" ? "결과" : "출처", onclick: () => setTab(k) });
    tabs.append(b);
    return [k, b] as const;
  });
  main.replaceChildren(h("div", { class: "mt" }, barSlot, plSlot,
    h("div", { class: "mt-body" }, h("section", { class: "tr" }, notices, trSlot, followWrap, foot), h("aside", { class: "ins", "aria-label": "결과와 출처" }, h("div", { class: "ins-tabs" }, tabs), body))));

  // one export control for the screen: the toolbar repaints often and re-attaches it
  const exp = exportControl((lead, text) => { flash = { kind: "danger", lead, text }; paintNotices(); }, () => !disposed);
  const say = (e: unknown, lead = "") => { if (disposed) return; flash = { kind: "danger", lead, text: asError(e).message }; paintNotices(); };
  const head = () => headOf(detail.versions);
  /** no draft or approved version yet (at most live captions) */
  const noDraft = () => !detail.versions.some((x) => x.kind !== "live");
  const hasLive = () => detail.versions.some((x) => x.kind === "live");
  /** a draft/transcription job runs for another meeting (the backend runs one at a time) */
  const busyElsewhere = () => [...transcribing.keys()].some((id) => id !== meetingId);
  /** where a draft (or the draft an approval was made from) came from, as a quiet sub-line in the version menu */
  const sourceOf = (v: VersionView): string => {
    const draft = v.kind === "approved" ? detail.versions.find((x) => x.id === v.parent) : v;
    if (draft?.kind !== "draft") return "";
    return detail.versions.find((x) => x.id === draft.parent)?.kind === "live" ? " · 실시간 자막에서" : " · 받아쓰기로";
  };
  const cur = () => detail.versions.find((v) => v.id === versionId);
  // job state last painted: another meeting busy, this meeting's job phase, the last 25% step announced
  let elsewhere = busyElsewhere(), jobPhase = transcribing.get(meetingId) ? "run" : "", spoken = -1;

  // ---------- toolbar ----------
  function paintBar(): void {
    // the live captions become the first draft ("초안 만들기"); the batch transcription is the optional
    // "다시 받아쓰기", offered only when whisper is set up (with no live captions it is the only way)
    const job = transcribing.get(meetingId), v = cur(), none = noDraft(), live = hasLive();
    const items: Node[] = [h("a", { class: "btn mt-crumb", href: "#/meetings" }, icon("chevron", 12), "회의")];
    (items[0].firstChild as SVGElement).style.transform = "rotate(90deg)";
    const title = detail.title ?? (detail.started_at ? `회의 ${dateText(detail.started_at)}` : "제목 없음");
    items.push(h("h1", { class: "mt-title trunc", text: title, title }));
    if (v) {
      const verBtn = h("button", { type: "button", class: `btn mt-ver ${v.kind}`, "aria-haspopup": "menu", "aria-expanded": "false", "aria-label": "버전" },
        h("i", { class: "dot" }), h("span", { text: `${kindName(v.kind)} · ${dateText(v.created_at)}` }), icon("chevron", 12));
      verBtn.addEventListener("click", () => versionMenu(verBtn));
      items.push(verBtn);
    }
    items.push(h("div", { class: "mt-sp" }));
    const right = h("div", { class: "mt-act" });
    // exports exactly the version on screen; a draft or live one says so in the file
    exp.show(v && model ? { id: v.id, kind: v.kind } : null, title); // may move focus from its closed menu to its button
    const expFocused = exp.root.contains(document.activeElement);
    if (v && model) right.append(exp.root);
    if (job) {
      const prep = job.channels === 0, pct = prep ? 0 : Math.round((job.channel * 100 + job.percent) / job.channels);
      const what = job.from_live ? "초안 만들기" : "받아쓰기";
      const bar = h("i");
      bar.style.width = `${pct}%`;
      right.append(
        h("div", { class: "retx", role: "status" }, h("span", { class: "num", text: prep ? (job.from_live ? "초안 만드는 중…" : "준비 중… (처음에는 모델 검증에 시간이 걸려요)") : `${job.diarizing ? "화자 구분" : what} 중 ${pct}% · 채널 ${job.channel + 1}/${job.channels}` }),
          h("div", { class: "progress", role: "progressbar", "aria-label": `${what} 진행률`, "aria-valuenow": prep ? null : String(pct) }, bar)),
        h("button", { type: "button", class: "btn", text: "취소", onclick: () => void api.transcribeCancel() }));
    } else {
      // one job at a time across meetings: while another meeting's runs, these wait (the notice above the transcript says why)
      const other = busyElsewhere();
      const btn = (text: string, primary: boolean, go: () => void) => h("button", { type: "button", class: `btn${primary ? " btn-primary" : ""}`, text, disabled: other, onclick: go });
      if (ready) right.append(btn(none && !live ? "받아쓰기" : "다시 받아쓰기", false, () => void startTranscribe()));
      if (none && live) right.append(btn("초안 만들기", true, () => void startLiveDraft()));
    }
    if (v?.kind === "draft") right.append(h("button", { type: "button", class: "btn btn-primary", text: "승인…", disabled: !!job, onclick: () => void approve(v) }));
    else if (v?.kind === "approved") right.append(h("button", { type: "button", class: "btn btn-primary", text: "승인됨", disabled: true }));
    items.push(right);
    barSlot.replaceChildren(...items);
    if (expFocused && exp.root.isConnected) exp.root.querySelector("button")?.focus({ preventScroll: true }); // the repaint detached it for a moment
  }

  function versionMenu(anchor: HTMLElement): void {
    const base = head();
    const rows = [...detail.versions].reverse().map((v) => h("button", { type: "button", class: "row", role: "menuitemradio", "aria-checked": String(v.id === versionId),
      onclick: () => { closePop(); void loadVersion(v.id); } },
      h("span", { class: "c-3" }, v.id === versionId ? icon("check", 14) : null),
      h("span", {}, kindName(v.kind), h("small", { text: `${dateText(v.created_at)}${sourceOf(v)} · ${v.id}` })),
      v.id === base?.id ? h("span", { class: "tag", text: "기준" }) : h("span")));
    const extra = ready && !transcribing.get(meetingId) && !busyElsewhere()
      ? [h("div", { class: "sep" }), h("button", { type: "button", class: "row", role: "menuitem", onclick: () => { closePop(); void startTranscribe(); } }, h("span"), h("span", { text: "다시 받아쓰기로 새 초안 만들기" }), h("span"))]
      : [];
    openPop(anchor, h("div", { class: "menu" }, ...rows, ...extra));
  }

  async function startTranscribe(): Promise<void> {
    flash = null;
    if (!ready) {
      flash = { kind: "danger", lead: `${need}을 찾을 수 없어요.`, text: "전사 모델과 전사 엔진(whisper-cli)이 모두 있어야 받아쓸 수 있어요. 기존 전사본은 그대로예요.", settings: true };
      paintNotices(); return;
    }
    // the new draft takes over from the base version (now, or once approved): say so before the long run
    if (detail.versions.length) {
      const approved = detail.versions.some((x) => x.kind === "approved");
      const ok = await confirmDialog("다시 받아쓰기를 할까요?", h("div", { class: "dlg-body" },
        h("p", { text: "녹음 전체를 전사 모델로 다시 받아 적어 새 초안을 만들어요. 몇 분 걸릴 수 있어요. 지금 버전은 지워지지 않아요." }),
        h("p", { text: approved
          ? "새 초안은 승인해야 기준이 돼요. 승인하면 지금 기준인 승인본에서 만든 플러그인 결과는 ‘오래됨’으로 표시돼요."
          : "새 초안이 바로 기준이 돼요. 지금 기준 버전에서 만든 플러그인 결과는 ‘오래됨’으로 표시돼요." })), "다시 받아쓰기");
      if (!ok || disposed) return;
    }
    try { await api.transcribeStart(meetingId); if (disposed) return; paintBar(); paintPane(); } // the first progress event (preparing) arrives from the backend right away
    catch (e) { const err = asError(e); say({ ...err, message: apiErrorText(err.code, err.message) }, "받아쓰기를 시작하지 못했어요."); }
  }

  /** The live captions as the first draft: no whisper needed. Usually already started when the recording stopped. */
  async function startLiveDraft(): Promise<void> {
    flash = null;
    try { await api.liveDraftStart(meetingId); if (disposed) return; paintBar(); }
    catch (e) {
      if (disposed) return;
      const err = asError(e);
      // busy with this very meeting (the automatic draft after recording): it is in progress, not a failure
      if (err.code === "busy" && (transcribing.has(meetingId) || !transcribing.size)) { paintBar(); if (!transcribing.has(meetingId)) void reload(); return; }
      say({ ...err, message: liveDraftErrorText(err.code) }, "초안을 만들지 못했어요.");
    }
  }

  async function approve(v: VersionView): Promise<void> {
    const ok = await confirmDialog("이 초안을 승인본으로 만들까요?", h("div", { class: "dlg-body" },
      h("p", { text: `${dateText(v.created_at)}에 만든 초안(${v.id})을 승인해요. 같은 내용의 새 승인본이 만들어지고, 초안과 승인본은 이후 바뀌지 않아요.` }),
      h("p", { text: "플러그인은 이 승인본을 기준으로 실행돼요. 이전 버전에서 만든 결과는 ‘오래됨’으로 표시되고, 새 기준으로 다시 실행할 수 있어요." })), "승인");
    if (!ok || disposed) return;
    try { const id = await api.approve(v.id); if (disposed) return; await reload(id); announce("승인했습니다"); } catch (e) { const err = asError(e); say({ ...err, message: apiErrorText(err.code, err.message) }, "승인하지 못했어요."); }
  }

  // ---------- notices (above the transcript) ----------
  function paintNotices(): void {
    const out: HTMLElement[] = [];
    if (flash) {
      const f = flash;
      out.push(notice(f.kind, f.lead, f.text, f.settings ? h("a", { class: "btn", href: "#/settings", text: "설정 열기" }) : null,
        h("button", { type: "button", class: "btn", text: "닫기", onclick: () => { flash = null; paintNotices(); } })));
    }
    const v = cur(), base = head();
    // what the draft actions can do right now: running here, waiting for another meeting's job, or ready
    const job = transcribing.get(meetingId), waiting = !job && busyElsewhere();
    if (job?.from_live) out.push(notice("info", "실시간 자막으로 초안을 만들고 있어요.", job.diarizing ? "화자를 나누는 데 몇 분 걸릴 수 있어요. 다른 화면으로 가도 계속돼요." : "다른 화면으로 가도 계속돼요."));
    else if (v?.kind === "live" && noDraft()) out.push(notice("info", "녹음하는 동안 받아 적은 실시간 자막이에요.", waiting
      ? "다른 회의의 작업이 끝나면 ‘초안 만들기’를 누를 수 있어요."
      : `‘초안 만들기’를 누르면 검토하고 승인할 수 있는 초안이 돼요.${ready ? " 더 정확한 결과가 필요하면 ‘다시 받아쓰기’를 누르세요." : ""}`));
    else if (waiting && ready) out.push(notice("info", "다른 회의에서 받아쓰는 중이에요.", "끝나면 이 회의도 받아쓸 수 있어요."));
    if (v && base && v.id !== base.id) out.push(notice("info", "이 버전은 현재 기준 버전이 아니에요.", "여기서 만든 플러그인 결과는 ‘오래됨’으로 표시돼요."));
    if (model?.flags.tokenTiming) out.push(notice("info", "단어 시각이 정확하지 않을 수 있어요.", "이 전사는 토큰 시각을 썼어서 재생 위치가 몇 초까지 어긋날 수 있어요.", h("button", { type: "button", class: "link", text: "출처 보기", onclick: () => setTab("prov") })));
    if (model?.flags.network.length) out.push(notice("warn", "이 버전은 데이터를 기기 밖으로 보냈어요.", `${model.flags.network.map((p) => p.engine.id).join(", ")} 단계가 네트워크를 썼어요.`));
    const other = staleElsewhere.filter((r) => r.version_id !== versionId);
    if (other.length) {
      const byVersion = new Map<string, number>();
      for (const r of other) byVersion.set(r.version_id, (byVersion.get(r.version_id) ?? 0) + 1);
      out.push(notice("warn", `다른 버전에서 만든 오래된 결과가 ${other.length}개 있어요.`, "", ...[...byVersion].map(([vid, n]) => {
        const ver = detail.versions.find((x) => x.id === vid);
        return h("button", { type: "button", class: "btn", text: `${ver ? `${kindName(ver.kind)} · ${dateText(ver.created_at)}` : vid} (${n}개) 보기`, onclick: () => void loadVersion(vid) });
      })));
    }
    notices.replaceChildren(...out);
    notices.hidden = out.length === 0;
  }

  // ---------- transcript pane ----------
  function paintPane(): void {
    tr?.destroy(); tr = null;
    followWrap.replaceChildren();
    footR.textContent = "";
    if (!model) {
      footInfo.textContent = "";
      if (!versionId && detail.versions.length && loadErr === null) { trSlot.replaceChildren(h("div", { class: "tr-scroll" }, h("div", { class: "spinner-row", role: "status" }, h("i", { class: "spin" }), h("span", { text: "불러오는 중" })))); return; }
      const job = transcribing.get(meetingId);
      trSlot.replaceChildren(h("div", { class: "tr-scroll" }, job
        ? h("div", { class: "empty" }, h("div", { class: "spinner-row" }, h("i", { class: "spin" })), h("h2", { text: job.from_live ? "초안을 만드는 중이에요" : "받아쓰는 중이에요" }), h("p", { text: "끝나면 여기에 전사본이 나타나요. 다른 화면으로 가도 계속돼요." }))
        : loadErr !== null
        ? h("div", { class: "empty", role: "alert" }, h("h2", { text: "전사본을 불러오지 못했어요" }), h("p", { text: loadErr }),
          h("button", { type: "button", class: "btn btn-primary btn-lg", text: "다시 시도", onclick: () => void reload(versionId || undefined) }))
        : ready
        ? h("div", { class: "empty" }, h("h2", { text: "아직 전사본이 없어요" }),
          h("p", { text: "받아쓰면 내 마이크와 상대 소리를 따로 받아 적은 초안이 만들어져요. 원본 오디오는 이 기기에만 있어요." }),
          h("button", { type: "button", class: "btn btn-primary btn-lg", text: "받아쓰기", disabled: busyElsewhere(), onclick: () => void startTranscribe() }))
        // no live captions (they failed, or the recording predates them) and no batch model: nothing to press
        : h("div", { class: "empty" }, h("h2", { text: "아직 전사본이 없어요" }),
          h("p", { text: `실시간 자막이 없는 녹음이라 ${need}이 있어야 전사본을 만들 수 있어요.` }),
          h("a", { class: "btn", href: "#/settings", text: "설정 열기" }))));
      return;
    }
    const m = model;
    const setFoot = (i: number) => { footInfo.textContent = i >= 0 ? wordInfoText(m, i) : `턴 ${m.turns.length.toLocaleString()} · 단어 ${m.words.length.toLocaleString()}`; };
    const t = (tr = buildTranscript(m, player, {
      onInfo: setFoot,
      onFollowChange: (follow, out) => {
        footR.textContent = follow ? (player.playing ? "재생 위치를 따라가는 중" : "") : "스크롤 중 · 따라가기 멈춤";
        const show = !follow && out;
        if (show && !followWrap.firstChild) followWrap.append(followBtn);
        if (!show && followWrap.firstChild) followWrap.replaceChildren();
      },
    }));
    setFoot(-1);
    trSlot.replaceChildren(t.root);
    applyChannel();
  }
  followBtn.addEventListener("click", () => tr?.resume());
  const applyChannel = () => tr?.applyChannel(player.channel === "both" ? null : Number(player.sourceOf(player.channel)));
  const offPlayer = player.on((k) => { if (k === "channel") applyChannel(); });

  function paintPlayer(): void {
    ui?.destroy();
    const lanes = model
      ? model.md.meeting.channels.map((c) => ({ index: c.index, label: model!.chanLabel(c.index), gaps: model!.gaps.get(c.index) ?? [] }))
      : detail.channels.map((c) => ({ index: c.index, label: roleLabel(c.role, c.label), gaps: [] as [number, number][] }));
    ui = buildPlayer(player, lanes, (a) => model?.resolve(a) ?? { ok: false }, (a) => void playAnchor(a), (c) => void player.setChannel(c));
    plSlot.replaceChildren(ui.root);
    paintMarkers();
  }
  function paintMarkers(): void {
    ui?.setMarkers(results.filter((r) => !r.stale).flatMap((r) => r.output.items.flatMap((it) => (it.type === "timeline_marker" ? [{ kind: it.kind, label: it.label, at: it.at }] : []))));
  }

  /** Evidence: the core resolves the anchor to channels and a time span; then play it and stop at its end. */
  async function playAnchor(a: Anchor): Promise<void> {
    const local = model?.resolve(a), vid = versionId;
    if (!local?.ok) return;
    try {
      const r = await api.resolve(vid, a);
      if (disposed || vid !== versionId) return;
      await player.playRange(r.start_ms, r.end_ms, { i0: local.i0, i1: local.i1 }, r.channels.length === 1 ? channelOf(r.channels[0]) : "both");
    } catch (e) { say(e, "구간을 재생하지 못했어요."); }
  }

  // ---------- inspector ----------
  function setTab(t: typeof tab): void { tab = t; paintInspector(); }
  function paintInspector(): void {
    tabBtns.forEach(([k, b]) => { b.setAttribute("aria-selected", String(k === tab)); });
    body.scrollTop = 0;
    if (tab === "prov" && model && data) { body.replaceChildren(renderProvenance(model, data, (ms) => player.seek(ms))); return; }
    if (!model && loadErr !== null) { body.replaceChildren(); return; }
    if (!model) { body.replaceChildren(h("div", { class: "empty small" }, h("h2", { text: "전사본이 있으면 결과가 여기에 나타나요" }), h("p", { text: "먼저 초안을 만드세요. 그다음 플러그인을 실행해 요약과 액션 아이템을 만들 수 있어요." }))); return; }
    const running = pluginRunning.id !== null && pluginRunning.versionId === versionId ? plugins.find((p) => p.id === pluginRunning.id) ?? null : null;
    const runBtn = h("button", { type: "button", class: "btn", "aria-haspopup": "menu", "aria-expanded": "false", disabled: pluginRunning.id !== null }, icon("plus", 12), "플러그인 실행", icon("chevron", 12));
    runBtn.addEventListener("click", () => runMenu(runBtn));
    const bar = h("div", { class: "ins-bar" }, h("span", { class: "t-footnote c-3", text: `결과 ${results.length}개` }), h("span", { class: "mt-sp" }), runBtn);
    body.replaceChildren(bar);
    if (!results.length && !running && !failures.length) {
      body.append(h("div", { class: "empty small" }, h("h2", { text: "아직 결과가 없어요" }), h("p", { text: "플러그인을 실행하면 요약, 액션 아이템, 표가 여기에 나타나고 각 줄에서 원래 발언으로 이동할 수 있어요." })));
    } else {
      body.append(renderResults(model, { results, running: running ? { id: running.id, version: running.version } : null, failures, play: (a) => void playAnchor(a), rerun: (id) => rerun(id) }));
    }
  }

  function runMenu(anchor: HTMLElement): void {
    const rows: HTMLElement[] = plugins.map((p) => h("button", { type: "button", class: "row", role: "menuitem", onclick: () => { closePop(); void runPlugin(p); } },
      h("span"), h("span", {}, p.id, h("small", { text: `@${p.version}${p.network ? " · 네트워크" : ""}${p.audio !== "none" ? " · 오디오" : ""}` })), h("span")));
    if (!rows.length) rows.push(h("a", { class: "row", href: "#/plugins", role: "menuitem" }, h("span"), h("span", { text: "추가된 플러그인이 없어요. 플러그인 화면에서 추가하세요" }), h("span")));
    openPop(anchor, h("div", { class: "menu" }, ...rows), { align: "right" });
  }

  async function runPlugin(p: PluginInfo): Promise<void> {
    failures = failures.filter((f) => f.plugin_id !== p.id);
    pluginRunning.id = p.id; pluginRunning.versionId = versionId;
    paintInspector();
    try { await api.pluginRun(p.id, versionId); } catch (e) {
      if (disposed) return;
      pluginRunning.id = null; pluginRunning.versionId = null;
      failures.push({ plugin_id: p.id, version: p.version, code: asError(e).code, message: asError(e).message });
      paintInspector();
    }
  }
  function rerun(id: string): void {
    const p = plugins.find((x) => x.id === id);
    if (p) void runPlugin(p); else { flash = { kind: "warn", lead: "플러그인을 찾을 수 없어요.", text: `${id}가 더 이상 추가된 목록에 없어요.` }; paintNotices(); }
  }

  // ---------- loading ----------
  async function loadVersion(id: string): Promise<void> {
    versionId = id; model = null; data = null; results = []; player.range = null; loadErr = null;
    paintBar(); paintPane();
    if (id) {
      try {
        let stale: ResultView[];
        [data, results, stale] = await Promise.all([api.getVersion(id), api.listResults(id), api.listStale(meetingId)]);
        if (disposed || id !== versionId) return; // left the screen, or a newer pick won while this one was loading
        staleElsewhere = stale;
        model = prepare(data, detail.channels);
      } catch (e) { if (!disposed && id === versionId) { loadErr = asError(e).message; say(e, "이 버전을 불러오지 못했어요."); } }
    }
    if (disposed) return;
    paintBar(); paintPlayer(); paintPane(); paintNotices(); paintInspector();
  }

  async function reload(select?: string): Promise<void> {
    let fresh: MeetingDetail;
    try { fresh = await api.getMeeting(meetingId); } catch (e) {
      if (disposed) return;
      loadErr = asError(e).message; model = null; data = null;
      say(e, "회의를 다시 불러오지 못했어요."); paintBar(); paintPane(); paintInspector();
      return;
    }
    if (disposed) return;
    detail = fresh;
    await loadVersion(select ?? (detail.versions.find((v) => v.id === versionId) ? versionId : head()?.id ?? ""));
  }

  // ---------- events ----------
  const offs = [
    transcribeBus.on((e) => {
      // another meeting's job only matters when it starts or ends (it enables or disables this one's
      // actions); repainting on its every event would take the focus and orphan an open menu here
      if (e.meeting_id !== meetingId) {
        const now = busyElsewhere();
        if (now !== elsewhere) { elsewhere = now; paintBar(); paintNotices(); if (!model) paintPane(); }
        return;
      }
      paintBar();
      const what = e.from_live ? "초안 만들기" : "받아쓰기";
      const phase = e.state === "progress" ? (e.diarizing ? "diar" : "run") : "";
      if (phase !== jobPhase) { jobPhase = phase; paintNotices(); } // the notice says what runs here
      if (e.state === "progress") {
        if (!model) paintPane();
        if (e.channels === 0) spoken = -1; // a job (re)starts
        const pct = e.channels > 0 ? Math.round((e.channel * 100 + e.percent) / e.channels) : 0, step = Math.floor(pct / 25) * 25;
        if (e.channels > 0 && step > spoken) { spoken = step; announce(`${e.diarizing ? "화자 구분" : what} 중 ${step}%`); }
        return;
      }
      // a draft from the live captions that exists already (another click, the automatic one) is not a failure
      if (e.from_live && e.state === "error" && e.code === "stale_draft") { void reload(); return; }
      announce(e.state === "done" ? (e.from_live ? "초안을 만들었습니다" : "받아쓰기를 마쳤습니다") : e.state === "cancelled" ? `${what}를 취소했습니다` : e.from_live ? "초안을 만들지 못했습니다" : "받아쓰지 못했습니다");
      if (e.state === "done" && e.version_id) {
        // the draft is saved; speaker separation (optional) is what did not work
        if (e.code === "diarization_failed" || e.code === "diarization_off") flash = e.code === "diarization_off"
          // the model or its engine cannot be used at all: say so once, with the way to fix it
          ? { kind: "warn", lead: "화자를 나누지 않았어요.", text: `화자 구분 모델이나 엔진을 쓸 수 없어서 건너뛰었어요. ${e.from_live ? "초안" : "전사본"}은 저장했어요. 설정 › 받아쓰기 준비에서 확인할 수 있어요.`, settings: true }
          : { kind: "warn", lead: "화자를 나누지 못했어요.", text: `${e.from_live ? "초안" : "전사본"}은 저장했고, 해당 채널은 한 명으로 표시돼요.` };
        void reload(e.version_id);
      } else {
        flash = e.state === "error" ? { kind: "danger", lead: e.from_live ? "초안을 만들지 못했어요." : "받아쓰지 못했어요.", text: e.from_live ? liveDraftErrorText(e.code ?? "") : apiErrorText(e.code ?? "", e.message ?? e.code ?? "") } : { kind: "warn", lead: `${what}를 취소했어요.`, text: "저장된 버전은 바뀌지 않았어요." };
        paintNotices(); paintPane();
      }
    }),
    pluginBus.on(async (e) => {
      if (e.state === "error" && e.plugin_id) failures.push({ plugin_id: e.plugin_id, version: plugins.find((p) => p.id === e.plugin_id)?.version, code: e.code, message: e.message ?? "" });
      if (e.version_id === versionId) {
        try { [results, staleElsewhere] = await Promise.all([api.listResults(versionId), api.listStale(meetingId)]); } catch (err) { say(err); }
        if (disposed) return;
        paintMarkers(); paintNotices();
      }
      paintInspector();
      announce(e.state === "error" ? "플러그인 실행에 실패했습니다" : "플러그인 실행을 마쳤습니다");
    }),
  ];

  // Space plays/pauses unless a control has it
  const onKey = (e: KeyboardEvent) => {
    if (e.code !== "Space" || (e.target as HTMLElement).closest("button, input, textarea, select, [role=slider], summary, a, [role=dialog]")) return;
    e.preventDefault();
    player.toggle();
  };
  document.addEventListener("keydown", onKey);

  // the teardown exists before anything else awaits: a failing load must not leak the handlers, buses and player
  const teardown = () => { if (disposed) return; disposed = true; document.removeEventListener("keydown", onKey); offs.forEach((f) => f()); offPlayer(); tr?.destroy(); ui?.destroy(); player.destroy(); closePop(true); };
  onCleanup(teardown); // the route also runs it if this screen is superseded before it returns
  paintBar(); paintPlayer(); paintInspector();
  // a jump opens the version the search read (the head when it searched) and shows the words
  const jumpTo = jump && detail.versions.some((v) => v.id === jump.versionId) ? jump.versionId : undefined;
  void reload(jumpTo).then(() => {
    if (!jump || disposed || !model) return;
    if (jumpTo && versionId !== jumpTo) return; // something else (a pick, a finished transcription) chose the version meanwhile
    const r = jumpTo ? model.resolve(jump.anchor) : ({ ok: false } as const);
    if (!r.ok) {
      flash = { kind: "warn", lead: "검색한 위치를 이 버전에서 찾지 못했어요.", text: "검색한 뒤에 전사본이 바뀌었을 수 있어요. 목록에서 다시 찾아보세요." };
      paintNotices();
      return;
    }
    player.showRange(r.start, { i0: r.i0, i1: r.i1 });
    tr?.focusWord(r.i0); // keyboard users continue from the match
    announce(`${mmss(r.start)}, 찾은 곳으로 이동했어요`);
  }).catch((e) => { loadErr = asError(e).message; say(e, "회의를 불러오지 못했어요."); paintPane(); paintInspector(); });
  return teardown;
}
