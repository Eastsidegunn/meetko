// 용어 사전 in Settings: hint terms (given to whisper before it transcribes) and an ordered replacement
// list (applied to what is shown and exported; the stored transcript stays as it was). The editor holds
// unsaved changes until 저장; an imported file only fills the editor. The preview runs the same core
// function as the transcript (`vocabulary_preview`).
import { api, asError, type Replacement, type Vocabulary } from "./api";
import { announce, h, icon, notice } from "./dom";

const same = (a: Vocabulary, b: Vocabulary): boolean => JSON.stringify(a) === JSON.stringify(b);
const hintsOf = (text: string): string[] => text.split("\n").map((s) => s.trim()).filter((s) => s !== "");
/** the limits the app checks on save (`meetko_core::vocabulary`), said before saving */
const MAX_HINTS = 500, MAX_HINT_CHARS = 100, MAX_TERM_CHARS = 200, MAX_REPLACEMENTS = 1000;

export function vocabularySection(onCleanup: (f: () => void) => void): HTMLElement {
  let saved: Vocabulary = { hints: [], replacements: [] };
  let rows: Replacement[] = [];
  let left = false, busy = false, previewSeq = 0;
  onCleanup(() => { left = true; });

  const hints = h("textarea", { class: "field voc-hints", id: "voc-hints", rows: "5", spellcheck: "false", placeholder: "예: 쿠버네티스\nSprint Review\n김지민" }) as HTMLTextAreaElement;
  const hintCount = h("p", { class: "c-3 t-caption num", "aria-live": "polite" });
  const hintWarn = h("p", { class: "voc-warn t-caption", "aria-live": "polite" });
  const list = h("div", { class: "voc-rows", role: "list", "aria-label": "바꾸기 목록" });
  const add = h("button", { type: "button", class: "btn" }, icon("plus", 14), "바꾸기 추가");
  const sample = h("input", { type: "text", class: "field", id: "voc-sample", placeholder: "예문을 적으면 바뀐 결과를 보여 줘요", autocomplete: "off", spellcheck: "false" }) as HTMLInputElement;
  const out = h("p", { class: "voc-out", "aria-live": "polite" });
  const save = h("button", { type: "button", class: "btn btn-primary", text: "저장" });
  const revert = h("button", { type: "button", class: "btn", text: "되돌리기" });
  const imp = h("button", { type: "button", class: "btn", text: "가져오기…" });
  const exp = h("button", { type: "button", class: "btn", text: "내보내기…" });
  const msg = h("div", { role: "status" });

  const current = (): Vocabulary => ({ hints: hintsOf(hints.value), replacements: rows.map((r) => ({ ...r })) });
  const dirty = (): boolean => !same(current(), saved);
  const setDisabled = (b: HTMLElement, off: boolean) => { if (off) b.setAttribute("aria-disabled", "true"); else b.removeAttribute("aria-disabled"); };
  function paintState(): void {
    const hs = hintsOf(hints.value), n = hs.length;
    hintCount.textContent = n ? `용어 ${n.toLocaleString()}개 · ${MAX_HINTS}개까지, 한 줄에 ${MAX_HINT_CHARS}자까지` : `아직 적은 용어가 없어요 · ${MAX_HINTS}개까지, 한 줄에 ${MAX_HINT_CHARS}자까지`;
    const long = hs.findIndex((x) => [...x].length > MAX_HINT_CHARS);
    hintWarn.textContent = n > MAX_HINTS ? `용어가 ${MAX_HINTS}개를 넘었어요. 저장하려면 줄여 주세요.`
      : long >= 0 ? `${long + 1}번째 용어가 ${MAX_HINT_CHARS}자를 넘어요.` : "";
    setDisabled(add, rows.length >= MAX_REPLACEMENTS);
    const d = dirty();
    setDisabled(save, !d || busy); setDisabled(revert, !d || busy);
    setDisabled(imp, busy); setDisabled(exp, busy);
  }

  async function preview(): Promise<void> {
    const seq = ++previewSeq;
    const text = sample.value;
    if (!text) { out.textContent = ""; return; }
    try {
      const r = await api.vocabularyPreview(rows.filter((x) => x.find !== ""), text);
      if (!left && seq === previewSeq) out.textContent = r;
    } catch (e) {
      if (!left && seq === previewSeq) out.textContent = `미리 보지 못했어요. ${asError(e).message}`;
    }
  }
  const changed = () => { paintState(); void preview(); };

  function paintRows(focus?: { row: number; part: "find" | "replace" | "up" | "down" | "del" | "add" }): void {
    const els = rows.map((r, i) => {
      const n = i + 1;
      const find = h("input", { type: "text", class: "field", value: r.find, placeholder: "찾을 말", maxlength: String(MAX_TERM_CHARS), "aria-label": `${n}번째 찾을 말`, autocomplete: "off", spellcheck: "false" }) as HTMLInputElement;
      const rep = h("input", { type: "text", class: "field", value: r.replace, placeholder: "바꿀 말 (비우면 지워요)", maxlength: String(MAX_TERM_CHARS), "aria-label": `${n}번째 바꿀 말`, autocomplete: "off", spellcheck: "false" }) as HTMLInputElement;
      find.addEventListener("input", () => { rows[i].find = find.value; changed(); });
      rep.addEventListener("input", () => { rows[i].replace = rep.value; changed(); });
      const move = (to: number, part: "up" | "down") => {
        if (to < 0 || to >= rows.length) return;
        [rows[i], rows[to]] = [rows[to], rows[i]];
        paintRows({ row: to, part }); changed();
        announce(`${n}번째 바꾸기를 ${to + 1}번째로 옮겼어요.`);
      };
      const up = h("button", { type: "button", class: "btn btn-icon", title: "위로", "aria-label": `${n}번째 바꾸기를 위로` }, icon("arrowup", 14));
      const down = h("button", { type: "button", class: "btn btn-icon", title: "아래로", "aria-label": `${n}번째 바꾸기를 아래로` }, icon("arrowdown", 14));
      const del = h("button", { type: "button", class: "btn btn-icon", title: "삭제", "aria-label": `${n}번째 바꾸기 삭제` }, icon("x", 14));
      setDisabled(up, i === 0); setDisabled(down, i === rows.length - 1);
      up.addEventListener("click", () => { if (i > 0) move(i - 1, "up"); });
      down.addEventListener("click", () => { if (i < rows.length - 1) move(i + 1, "down"); });
      del.addEventListener("click", () => {
        rows.splice(i, 1);
        paintRows(rows.length ? { row: Math.min(i, rows.length - 1), part: "del" } : { row: -1, part: "add" }); changed();
        announce("바꾸기를 지웠어요.");
      });
      const el = h("div", { class: "voc-row", role: "listitem" }, find, h("span", { class: "voc-arrow", "aria-hidden": "true", text: "→" }), rep, up, down, del);
      if (focus && focus.row === i) {
        const target = { find, replace: rep, up, down, del, add }[focus.part];
        // a move button that is now at the end of the list is disabled: keep the focus on the row
        queueMicrotask(() => (target.getAttribute("aria-disabled") ? find : target).focus());
      }
      return el;
    });
    list.replaceChildren(...els);
    if (focus && focus.row < 0) queueMicrotask(() => add.focus());
  }

  function fill(v: Vocabulary): void {
    hints.value = v.hints.join("\n");
    rows = v.replacements.map((r) => ({ find: r.find, replace: r.replace }));
    paintRows(); changed();
  }

  hints.addEventListener("input", () => paintState());
  sample.addEventListener("input", () => void preview());
  add.addEventListener("click", () => { if (rows.length >= MAX_REPLACEMENTS) return; rows.push({ find: "", replace: "" }); paintRows({ row: rows.length - 1, part: "find" }); changed(); });
  revert.addEventListener("click", () => { if (!revert.getAttribute("aria-disabled")) { fill(saved); msg.replaceChildren(); announce("저장된 내용으로 되돌렸어요."); } });

  async function run(f: () => Promise<void>): Promise<void> {
    if (busy) return;
    busy = true; paintState(); msg.replaceChildren();
    try { await f(); } finally { busy = false; if (!left) paintState(); }
  }
  save.addEventListener("click", () => {
    if (save.getAttribute("aria-disabled")) return;
    void run(async () => {
      if (rows.some((r) => r.find === "")) { msg.replaceChildren(notice("warn", "찾을 말이 빈 줄이 있어요.", "채우거나 지운 뒤 저장해 주세요.")); return; }
      try {
        saved = await api.vocabularySet(current());
        if (left) return;
        fill(saved);
        announce("용어 사전을 저장했어요.");
        msg.replaceChildren(h("p", { class: "c-2 t-footnote", text: "저장했어요. 바꾸기는 회의를 다시 열면 보여요. 용어는 다음 받아쓰기부터 써요." }));
      } catch (e) { if (!left) msg.replaceChildren(notice("danger", "저장하지 못했어요.", asError(e).message)); }
    });
  });
  imp.addEventListener("click", () => {
    if (imp.getAttribute("aria-disabled")) return;
    void run(async () => {
      try {
        const v = await api.vocabularyImport();
        if (left || !v) return;
        fill(v);
        msg.replaceChildren(notice("info", "가져왔어요.", `용어 ${v.hints.length.toLocaleString()}개, 바꾸기 ${v.replacements.length.toLocaleString()}개예요. 확인하고 저장해 주세요.`));
      } catch (e) { if (!left) msg.replaceChildren(notice("danger", "가져오지 못했어요.", asError(e).message)); }
    });
  });
  exp.addEventListener("click", () => {
    if (exp.getAttribute("aria-disabled")) return;
    void run(async () => {
      if (dirty()) { msg.replaceChildren(notice("warn", "저장하지 않은 변경이 있어요.", "내보내기는 저장된 내용만 써요. 먼저 저장해 주세요.")); return; }
      try {
        const name = await api.vocabularyExport();
        if (!left && name) msg.replaceChildren(h("p", { class: "c-2 t-footnote", text: `${name}(으)로 내보냈어요.` }));
      } catch (e) { if (!left) msg.replaceChildren(notice("danger", "내보내지 못했어요.", asError(e).message)); }
    });
  });

  const sec = h("section", { class: "set-sec voc" }, h("h2", { text: "용어 사전" }),
    h("p", { class: "c-2 t-footnote", text: "회의에 자주 나오는 이름과 용어를 적어 두면 받아쓰기에 참고해요. 바꾸기 목록은 화면과 내보내기에 보이는 글자만 바꾸고, 저장된 전사본은 그대로 둬요." }),
    h("h3", {}, h("label", { for: "voc-hints", text: "참고할 용어" })),
    h("p", { class: "c-2 t-footnote", text: "한 줄에 하나씩 적어요. Whisper로 받아쓸 때 위에서부터 들어갈 수 있는 만큼 알려 줘요. Qwen3-ASR과 실시간 자막에는 아직 쓰지 않아요. 조용한 구간에서는 적어 둔 용어가 가끔 그대로 받아써질 수 있어요." }),
    hints, hintCount, hintWarn,
    h("h3", { text: "바꾸기" }),
    h("p", { class: "c-2 t-footnote", text: "위에서부터 차례로 한 번씩 바꿔요. 아래 줄은 위 줄이 바꾼 결과에 이어서 적용돼요. 띄어쓰기와 대소문자까지 똑같은 말만 바꿔요. 바꾼 결과가 너무 길어지는 부분에는 목록을 적용하지 않아요." }),
    list, h("div", {}, add),
    h("h3", {}, h("label", { for: "voc-sample", text: "미리 보기" })),
    sample, out,
    h("div", { class: "voc-acts" }, save, revert, imp, exp),
    msg);

  paintState();
  api.vocabularyGet().then((v) => { if (left) return; saved = v; fill(v); },
    (e) => { if (!left) msg.replaceChildren(notice("danger", "용어 사전을 읽지 못했어요.", `${asError(e).message} 저장하면 지금 보이는 내용으로 새로 써요.`)); });
  return sec;
}
