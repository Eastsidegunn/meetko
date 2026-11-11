import { api, asError } from "../api";
import { announce, dayText, durText, h, icon, kindName, mmss, notice } from "../dom";
import { setJump } from "../jump";
import { setupCard } from "../setup";
import { transcribeBus, transcribing } from "../jobs";
import type { MeetingSummary, SearchOutcome, Status } from "../api";

/** kept across visits, so coming back from a result shows the same results */
let lastQuery = "";
const SEARCH_DELAY_MS = 200;
/** while an IME is still composing, the result is announced only after this pause */
const SETTLE_MS = 600;
/** the core's cut (`MAX_QUERY_CHARS`), in code points */
const MAX_QUERY_CHARS = 100;

export async function listScreen(main: HTMLElement, _status: Status, onCleanup: (f: () => void) => void = () => {}): Promise<void> {
  const inner = h("div", { class: "page-in" },
    h("div", { class: "page-head" }, h("h1", { class: "t-title1", text: "회의" }), h("a", { class: "btn btn-primary", href: "#/record" }, icon("record", 12), "새 녹음")));
  main.replaceChildren(h("div", { class: "page" }, inner));
  // with live captions set up a recording becomes a draft on its own; the batch model is only for
  // the optional 다시 받아쓰기, so its absence is not a blocker worth a banner here. Without them a
  // calm card offers the one download that gets a new user there (setup.ts)
  inner.append(setupCard(onCleanup));
  let list: MeetingSummary[];
  try { list = await api.listMeetings(); } catch (e) { inner.append(notice("danger", "회의 목록을 불러오지 못했어요.", asError(e).message)); return; }
  if (!list.length) {
    inner.append(h("div", { class: "empty" }, h("h2", { text: "아직 회의가 없어요" }),
      h("p", { text: "녹음을 시작하면 내 마이크와 상대 소리를 따로 기록하고, 끝나면 전사본을 만들 수 있어요. 원본 오디오는 이 기기에만 남아요." }),
      h("a", { class: "btn btn-primary btn-lg", href: "#/record" }, icon("record", 12), "녹음 시작")));
    return;
  }
  // a running job (the automatic 다시 받아쓰기 after a recording, or any draft job) shows as one quiet
  // line under its meeting, with its cancel; the row's version follows when the job ends
  const rows = new Map<string, { item: HTMLElement; ver: HTMLElement; job: HTMLElement | null; key: string; text: HTMLElement | null; bar: HTMLElement | null }>();
  const verCell = (kind: MeetingSummary["latest_kind"]) => h("span", { class: `ver ${kind ?? ""}` }, h("i", { class: "dot" }), kindName(kind));
  const meetings = h("div", {},
    h("div", { class: "mlist-head" }, ...["제목", "날짜", "길이", "최신 버전"].map((t) => h("span", { text: t })), h("span", { class: "stale", text: "오래된 결과" })),
    // the whole row is the link, so the title is reachable by keyboard and middle-click
    h("div", { role: "list" }, ...list.map((m) => {
      const ver = verCell(m.latest_kind);
      const item = h("div", { role: "listitem" },
        h("a", { class: "row mrow", href: `#/meeting/${encodeURIComponent(m.id)}` },
          h("span", { class: "mtitle trunc", text: m.title ?? "제목 없음", title: m.title }),
          h("span", { class: "cell-2", text: dayText(m.started_at ?? m.created_at) }),
          h("span", { class: "cell-2 num", text: durText(m.duration_ms) }),
          ver,
          h("span", { class: `stale num${m.stale_count ? " has" : ""}`, text: m.stale_count ? `${m.stale_count}개` : "—" })));
      rows.set(m.id, { item, ver, job: null, key: "", text: null, bar: null });
      return item;
    })));
  /** Show (or update, or take away) the job line of a meeting. Only the text and bar change while it runs, so focus on its cancel stays. */
  function paintJob(id: string): void {
    const r = rows.get(id);
    if (!r) return;
    const e = transcribing.get(id);
    if (!e) { r.job?.remove(); Object.assign(r, { job: null, key: "", text: null, bar: null }); return; }
    const pct = e.channels > 0 ? Math.round((e.channel * 100 + e.percent) / e.channels) : null;
    const text = e.auto ? `더 정확하게 다시 받아쓰는 중…${pct === null ? "" : ` ${pct}%`}`
      : e.from_live ? "초안 만드는 중…" : `받아쓰는 중…${pct === null ? "" : ` ${pct}%`}`;
    const key = e.auto ? "auto" : e.from_live ? "live" : "batch";
    if (r.job && r.key === key && r.text && r.bar) {
      r.text.textContent = text;
      r.bar.style.width = `${pct ?? 0}%`;
      return;
    }
    r.job?.remove();
    const t = h("span", { class: "num", text }), bar = h("i");
    bar.style.width = `${pct ?? 0}%`;
    const job = h("div", { class: "mrow-job" }, t, h("div", { class: "progress", "aria-hidden": "true" }, bar),
      h("button", { type: "button", class: "btn", text: "취소", "aria-label": e.auto ? "다시 받아쓰기 취소" : "취소", onclick: () => void api.transcribeCancel() }));
    r.item.append(job);
    Object.assign(r, { job, key, text: t, bar });
  }
  for (const id of transcribing.keys()) paintJob(id);
  const offJobs = transcribeBus.on((e) => {
    const id = e.meeting_id ?? "";
    paintJob(id);
    if (e.state === "progress" || !rows.has(id)) return;
    if (e.auto && e.state === "done") announce("새 초안이 준비됐어요");
    // the meeting's newest version changed (or not): read it again for this row only
    void api.listMeetings().then((all) => {
      const m = all.find((x) => x.id === id), r = rows.get(id);
      if (!m || !r || left) return;
      const ver = verCell(m.latest_kind);
      r.ver.replaceWith(ver);
      r.ver = ver;
    }, () => {});
  });
  onCleanup(() => { offJobs(); });

  // ---------- search: replaces the list while there is a query ----------
  const input = h("input", { type: "search", class: "search-input", placeholder: "전사본에서 찾기", "aria-label": "모든 회의의 전사본에서 찾기", autocomplete: "off", spellcheck: "false" });
  input.value = lastQuery;
  const clearBtn = h("button", { type: "button", class: "search-clear", "aria-label": "검색어 지우기", title: "검색어 지우기", hidden: !lastQuery }, icon("x", 12));
  const box = h("div", { class: "search-box", role: "search" }, icon("search", 14), input, clearBtn);
  const body = h("div");
  inner.append(box, body);

  // `seq` is bumped whenever a search is scheduled or dropped, so only the latest answer paints.
  // Korean IME: while a syllable is being composed the text changes on every key (ㅎ→회→횡→회의).
  // Searches still run and paint (the macOS IME keeps the last syllable composing until the next
  // key), so the screen always matches the field; only the announcement waits until the input
  // settles: composition end, or a pause of SETTLE_MS.
  let seq = 0, timer = 0, said = 0, composing = false, left = false;
  const say = (text: string) => { clearTimeout(said); said = window.setTimeout(() => announce(text), composing ? SETTLE_MS : 0); };
  const busy = (on: boolean) => { body.setAttribute("aria-busy", String(on)); body.classList.toggle("sr-busy", on); };
  async function run(q: string, mine: number): Promise<void> {
    busy(true);
    let found: SearchOutcome;
    try { found = await api.search(q); } catch (e) {
      if (mine === seq && !left) { busy(false); body.replaceChildren(notice("danger", "검색하지 못했어요.", asError(e).message)); }
      return;
    }
    if (mine !== seq || left) return; // a newer query is on its way, or the screen is gone
    busy(false);
    const n = found.meetings.length;
    // what the search could not cover goes first, quietly
    const notes = [
      found.pending ? notice("info", `회의 ${found.pending.toLocaleString()}개는 아직 검색 준비 중이에요.`, "잠시 뒤 다시 찾으면 함께 찾아요.") : null,
      found.skipped ? h("p", { class: "sr-note t-footnote c-3 num", text: `회의 ${found.skipped.toLocaleString()}개는 전사본을 읽지 못해 검색에서 빠졌어요.` }) : null,
    ];
    body.replaceChildren(...notes.filter((x): x is HTMLElement => !!x), n ? renderResults(found) : h("div", { class: "empty small" },
      h("h2", { text: "찾는 내용이 없어요" }), h("p", { text: "다른 말로 찾아보세요. 회의마다 기준 버전의 전사본에서 찾아요." })));
    say(n ? summary(found) : "찾는 내용이 없어요");
  }
  function update(now = false): void {
    // the core cuts at 100 characters (code points); cut the same here so the IPC stays small
    const q = Array.from(input.value.trim()).slice(0, MAX_QUERY_CHARS).join("");
    lastQuery = input.value;
    clearBtn.hidden = !input.value;
    clearTimeout(timer); clearTimeout(said);
    const mine = ++seq;
    if (!q) { busy(false); body.replaceChildren(meetings); return; }
    if (now) void run(q, mine); else timer = window.setTimeout(() => void run(q, mine), SEARCH_DELAY_MS);
  }
  input.addEventListener("input", () => update());
  input.addEventListener("compositionstart", () => { composing = true; });
  input.addEventListener("compositionend", () => { composing = false; update(); });
  input.addEventListener("keydown", (e) => {
    if (e.isComposing || e.keyCode === 229) return; // the IME owns Enter and Escape while composing
    if (e.key === "Enter") { e.preventDefault(); update(true); }
    else if (e.key === "Escape" && input.value) { e.preventDefault(); input.value = ""; update(); }
  });
  clearBtn.addEventListener("click", () => { input.value = ""; update(); input.focus(); });
  onCleanup(() => { left = true; seq++; clearTimeout(timer); clearTimeout(said); });
  update(true);
}

const summary = (o: SearchOutcome): string =>
  `회의 ${o.meetings.length}개${o.more ? " 이상" : ""}에서 ${o.meetings.reduce((n, m) => n + m.matches, 0).toLocaleString()}곳을 찾았어요`;

function renderResults(o: SearchOutcome): HTMLElement {
  const found = o.meetings;
  return h("div", { class: "sr-list" },
    h("p", { class: "sr-sum t-footnote c-3 num", text: summary(o) }),
    ...found.map((m) => {
      const href = `#/meeting/${encodeURIComponent(m.meeting_id)}`;
      const more = m.matches - m.hits.length;
      return h("section", { class: "sr-meeting" },
        h("div", { class: "sr-head" },
          h("a", { class: "sr-title trunc link-fast", href, text: m.title ?? "제목 없음", title: m.title ?? "" }),
          h("span", { class: "cell-2", text: dayText(m.started_at ?? m.created_at) }),
          h("span", { class: `ver ${m.kind}` }, h("i", { class: "dot" }), kindName(m.kind))),
        h("div", { role: "list" }, ...m.hits.map((hit) => h("div", { role: "listitem" },
          h("a", { class: "row sr-hit", href, "aria-label": `${mmss(hit.start_ms)}, ${hit.before}${hit.text}${hit.after}`,
            // the meeting screen takes this once it has loaded the meeting, and shows the words
            onclick: () => setJump({ meetingId: m.meeting_id, versionId: m.version_id, anchor: { from: hit.from, to: hit.to } }) },
            h("span", { class: "sr-time num", text: mmss(hit.start_ms) }),
            h("span", { class: "sr-snip" }, hit.before, h("mark", { text: hit.text }), hit.after))))),
        more > 0 ? h("p", { class: "sr-more t-footnote c-3 num", text: `이 회의에서 ${more.toLocaleString()}곳을 더 찾았어요` }) : null);
    }),
    o.more ? h("p", { class: "sr-cap t-footnote c-3 num", text: `가장 최근 회의 ${found.length}개까지만 보여 줘요. 검색어를 더 구체적으로 써 보세요.` }) : null);
}
