// Provenance panel: exact, findable, quiet. It only gets loud when data left the device.
import { dateText, h, hashBtn, kindName, mmss } from "./dom";
import type { MeetingData, Producer } from "./api";
import type { Model } from "./model";

const ROLE: Record<string, string> = { asr: "전사", diarization: "화자 분리", alignment: "정렬", edit: "편집" };
const WORD_TIMING: Record<string, string> = { dtw: "DTW (정확)", token: "토큰 (정확도 낮음)", transducer: "transducer", aligned: "강제 정렬", none: "없음" };

const unk = (t = "기록 없음") => h("span", { class: "unk", text: t });
const kv = (...rows: (readonly [string, Node | string] | null)[]) =>
  h("dl", { class: "kv" }, ...rows.flatMap((r) => (r ? [h("dt", { text: r[0] }), h("dd", {}, r[1])] : [])));

export function renderProvenance(model: Model, d: MeetingData, seek: (ms: number) => void): HTMLElement {
  const { flags } = model;
  const root = h("div", { class: "pv" });

  // 1. did anything leave the device?
  const sent = flags.network;
  root.append(h("section", { class: "pv-sec" }, h("h3", { text: "외부 전송" }),
    sent.length
      ? h("div", { class: "sendbox out" }, h("strong", { text: "데이터가 기기 밖으로 나갔어요. " }), `${sent.map((p) => `${p.engine.id} ${p.engine.version}`).join(", ")} 단계가 네트워크를 사용했다고 기록돼 있어요.`)
      : h("div", { class: "sendbox", text: "외부 전송 없음 — 이 버전의 모든 처리 단계가 기기 안에서 실행됐어요. 원본 오디오는 이 기기에 있어요." })));

  // 2. version and recording
  const rec = d.recording, v = d.version;
  root.append(h("section", { class: "pv-sec" }, h("h3", { text: "버전과 녹음" }), kv(
    ["버전", v.id], ["종류", kindName(v.kind)], ["부모", v.parent ?? "—"], ["생성", dateText(v.created_at)],
    ["녹음 sha256", rec.sha256 ? hashBtn(rec.sha256) : unk()],
    ["길이", `${mmss(rec.duration_ms)} · ${rec.sample_rate ? `${rec.sample_rate / 1000} kHz` : "—"}`],
    ["단어 끝 시각", flags.estimated === 0 ? "모두 측정값" : flags.estimated === d.words.length ? "모두 추정" : `${flags.estimated.toLocaleString()} / ${d.words.length.toLocaleString()} 추정`])));

  // 3. producers, in the order they ran
  const ps = h("section", { class: "pv-sec" }, h("h3", { text: "처리 단계" }));
  for (const p of v.producers) ps.append(producer(p, d));
  root.append(ps);

  // 4. what nobody recorded
  const gs = h("section", { class: "pv-sec" }, h("h3", { text: "녹음되지 않은 구간" }));
  let any = false;
  for (const ch of d.meeting.channels) {
    for (const [a, b] of model.gaps.get(ch.index) ?? []) {
      any = true;
      gs.append(h("button", { type: "button", class: "row gaprow", onclick: () => seek(a) },
        h("span", { text: `${model.chanLabel(ch.index)} · ${mmss(a)}–${mmss(b)}` }), h("span", { class: "c-3", text: `${Math.round((b - a) / 1000)}초` })));
    }
  }
  if (!any) gs.append(h("div", { class: "c-2 t-footnote", text: "없음 — 모든 채널이 처음부터 끝까지 기록됐어요." }));
  root.append(gs);
  return root;
}

function producer(p: Producer, d: MeetingData): HTMLElement {
  const known = new Set(["role", "engine", "model", "channels", "params", "word_timing", "granularity", "network"]);
  const extras = Object.entries(p).filter(([k]) => !known.has(k));
  const params = p.params ? Object.entries(p.params) : null;
  const chans = (p.channels ?? []).map((i) => d.meeting.channels.find((c) => c.index === i)?.label ?? String(i)).join(", ");
  return h("div", { class: "prod" },
    h("div", { class: "ph" }, ROLE[p.role] ?? p.role),
    kv(
      ["엔진", h("span", {}, p.engine.id, " ", h("span", { class: p.engine.version === "unknown" ? "unk" : "c-3", text: p.engine.version === "unknown" ? "버전 unknown" : p.engine.version }))],
      ["모델", h("span", {}, p.model ? p.model.id : unk(), p.model?.sha256 ? hashBtn(p.model.sha256) : null)],
      p.word_timing ? ["단어 시각", p.word_timing === "token" ? h("span", { class: "tag-warn", text: WORD_TIMING.token }) : (WORD_TIMING[p.word_timing] ?? p.word_timing) + (p.granularity ? ` · ${p.granularity}` : "")] : null,
      ["네트워크", p.network ? h("span", { class: "tag-warn", text: "사용함" }) : "사용 안 함"],
      chans ? ["채널", chans] : null),
    params
      ? h("details", {}, h("summary", { text: `params ${params.length}개` }), h("div", { class: "mono" }, ...params.flatMap(([k, val]) => [h("span", { text: k }), h("span", { text: JSON.stringify(val) })])))
      : kv(["params", unk()]),
    // fields this build does not know are shown, not dropped
    extras.length ? kv(...extras.map(([k, val]) => [k, JSON.stringify(val)] as const)) : null);
}
