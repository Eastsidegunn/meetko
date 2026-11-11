// Results panel: one renderer per display type of the plugin-output contract. Every line a
// plugin produces points back at words: the play chip seeks, plays the span and highlights it.
import type { Anchor, PluginItem, ResultView } from "./api";
import { dateText, h, icon, mmss } from "./dom";
import { KEYCHAIN_DENIED_RUN, KEYCHAIN_DENIED_WHY, markerKind } from "./labels";
import type { Model } from "./model";

export interface Failure { plugin_id: string; version?: string; code: string | null; message: string }
export interface ResultsOpts {
  results: ResultView[];
  /** the plugin being run on this version right now */
  running: { id: string; version: string } | null;
  failures: Failure[];
  play(a: Anchor): void;
  rerun(pluginId: string): void;
}

/** "2026-10-23" -> "10월 23일" without a timezone shift. */
const dueText = (d: string): string => { const m = /^\d{4}-(\d{2})-(\d{2})$/.exec(d); return m ? `${+m[1]}월 ${+m[2]}일까지` : d; };

export function renderResults(model: Model, o: ResultsOpts): HTMLElement {
  const wrap = h("div", { class: "ri-wrap" });
  wrap.style.display = "contents";

  const playFirst = (anchors: Anchor[]) => { const a = anchors.find((x) => model.resolve(x).ok); if (a) o.play(a); };
  const chips = (anchors: Anchor[]) => {
    const box = h("div", { class: "ev-chips" });
    for (const a of anchors) {
      const r = model.resolve(a);
      if (!r.ok) {
        box.append(h("span", { class: "chip missing", title: "이 버전에서 근거 단어를 찾을 수 없어요. 플러그인을 다시 실행하면 새 근거가 붙어요." }, icon("warn", 10), "근거를 찾을 수 없음"));
        continue;
      }
      box.append(h("button", { type: "button", class: "chip", title: `${mmss(r.start)}–${mmss(r.end)} 구간 듣기`, "aria-label": `근거 ${mmss(r.start)} 구간 듣기`, onclick: () => o.play(a) }, icon("play", 10), mmss(r.start)));
    }
    return box;
  };
  const ownerChip = (id: string | null | undefined) => {
    const l = id ? model.speakerLabel(id) ?? id : "담당자 없음";
    return h("span", { class: "chip plain", text: l, title: l });
  };
  const evText = (text: string, anchors: Anchor[], cls = "") =>
    h("button", { type: "button", class: `row ev-text ${cls}`.trim(), text, onclick: () => playFirst(anchors) });

  function table(it: Extract<PluginItem, { type: "table" }>): HTMLElement {
    const colgroup = h("colgroup", {}, ...it.columns.map(() => h("col")), h("col", { class: "evc" }));
    const tb = h("tbody");
    for (const row of it.rows) {
      const tr = h("tr");
      for (const c of row.cells) {
        const td = h("td", { class: c.length <= 14 ? "nw" : null });
        const inner = h("div", { text: c });
        td.append(inner);
        if (c.length > 60) { // a 2,000-character cell must not own the panel
          inner.className = "clamp";
          const label = `더 보기 (${c.length.toLocaleString()}자)`;
          const more = h("button", { type: "button", class: "link link-fast more", text: label });
          more.addEventListener("click", () => { more.textContent = inner.classList.toggle("clamp") ? label : "접기"; });
          td.append(more);
        }
        tr.append(td);
      }
      const ev = h("td", { class: "evc" });
      const a = row.evidence?.[0], r = a ? model.resolve(a) : null;
      if (a && r?.ok) ev.append(h("button", { type: "button", class: "chip", "aria-label": `근거 ${mmss(r.start)} 구간 듣기`, onclick: () => o.play(a) }, icon("play", 10), mmss(r.start)));
      else if (a) ev.append(h("span", { class: "chip missing", title: "근거 단어를 찾을 수 없어요" }, icon("warn", 10)));
      tr.append(ev);
      tb.append(tr);
    }
    return h("div", { class: "ri" }, it.title ? h("div", { class: "ri-h", text: it.title }) : null,
      h("table", { class: "tbl" }, colgroup, h("thead", {}, h("tr", {}, ...it.columns.map((c) => h("th", { text: c })), h("th", { text: "근거" }))), tb));
  }

  function markerGroup(markers: Extract<PluginItem, { type: "timeline_marker" }>[]): HTMLElement {
    const LIM = 5;
    const list = h("div");
    const row = (m: (typeof markers)[number]) => {
      const r = model.resolve(m.at);
      return h("button", { type: "button", class: "row mkrow", "aria-disabled": r.ok ? null : "true", onclick: () => r.ok && o.play(m.at) },
        h("span", { class: `mk ${m.kind}` }, h("i")),
        h("span", { class: "trunc", text: m.label, title: `${markerKind(m.kind)} · ${m.label}` }),
        h("span", { class: "t", text: r.ok ? mmss(r.start) : "—" }));
    };
    markers.slice(0, LIM).forEach((m) => list.append(row(m)));
    const box = h("div", { class: "ri" }, h("div", { class: "ri-h", text: `타임라인 표시 ${markers.length}` }), list);
    if (markers.length > LIM) {
      const more = h("button", { type: "button", class: "link link-fast more", text: `${markers.length - LIM}개 더 보기` });
      more.addEventListener("click", () => { markers.slice(LIM).forEach((m) => list.append(row(m))); more.remove(); });
      box.append(more);
    }
    return box;
  }

  function item(it: PluginItem): HTMLElement {
    switch (it.type) {
      case "summary_card":
        return h("div", { class: "ri" }, h("div", { class: "ri-h", text: it.title }),
          ...it.points.map((p) => h("div", { class: "evrow" }, evText(p.text, p.evidence), chips(p.evidence))));
      case "action_items":
        return h("div", { class: "ri" }, h("div", { class: "ri-h", text: "액션 아이템" }),
          ...it.items.map((a) => h("div", { class: "act" }, evText(a.text, a.evidence),
            h("div", { class: "act-meta" }, ownerChip(a.owner), a.due ? h("span", { text: dueText(a.due) }) : null, chips(a.evidence)))));
      case "table":
        return table(it);
      case "transcript_block": {
        const r = model.resolve(it.anchor);
        let txt = "";
        if (r.ok) { // a long block is cut at about 240 characters; the chip plays all of it
          const parts: string[] = [];
          for (let i = r.i0; i <= r.i1 && parts.join(" ").length < 240; i++) if (model.text(i)) parts.push(model.text(i));
          txt = parts.join(" ") + (r.i1 - r.i0 + 1 > parts.length ? " …" : "");
        }
        return h("div", { class: "ri" }, it.title ? h("div", { class: "ri-h", text: it.title }) : null,
          evText(r.ok ? txt : "근거를 찾을 수 없어요", [it.anchor], "quote"),
          it.note ? h("div", { class: "ri-sub", text: it.note }) : null, chips([it.anchor]));
      }
      default: // a type this build does not know: say so rather than drop it
        return h("div", { class: "ri" }, h("span", { class: "ri-sub", text: `알 수 없는 표시 타입: ${(it as { type: string }).type}` }));
    }
  }

  const tag = (name: "net" | "wave", label: string) => h("span", { class: "tag tag-warn" }, icon(name, 12), label);
  const isolatedRun = (r: ResultView) => r.sandbox === "macos-sbpl";
  function perms(net: boolean, audio: string, isolated: boolean): HTMLElement | null {
    const tags = [net ? tag("net", "네트워크") : null, audio !== "none" ? tag("wave", "오디오") : null];
    // 'no permissions' is said once, in the meta line
    return tags.some(Boolean) ? h("div", { class: "rc-perm" }, ...tags, h("span", { text: isolated ? "선언한 권한 · 격리 안에서 실행됨" : "선언됨 · 강제되지 않음" })) : null;
  }
  const head = (id: string, version: string | null, meta: (HTMLElement | null)[], ...extra: (HTMLElement | null)[]) =>
    h("div", { class: "rc-h" }, h("div", { class: "rc-name" }, id, version ? h("span", { text: `@${version}` }) : null), h("div", { class: "rc-meta" }, ...meta), ...extra);

  /** A result an external AI agent stored over MCP: the transcript went to that agent's AI service. */
  const fromAgent = (r: ResultView) => r.sandbox === "mcp" && r.plugin_id.startsWith("mcp:");
  // the version-level notice says the transcript may have left the device; the card only names who made it
  function agentLine(r: ResultView): HTMLElement {
    const agentName = r.agent?.agent?.name, clientName = r.agent?.client?.name;
    const who = agentName || clientName ? [agentName, clientName].filter(Boolean).join(" · ") : "기록 없음";
    return h("div", { class: "rc-perm" }, tag("net", "외부 AI"), h("span", { text: `만든 곳: ${who}` }));
  }
  const unknownVersion = (v: string) => (v === "unknown" || v === "" ? null : v);

  function card(r: ResultView): HTMLElement {
    const markers = r.output.items.filter((i): i is Extract<PluginItem, { type: "timeline_marker" }> => i.type === "timeline_marker");
    const body = h("div", { class: "rc-b" });
    for (const it of r.output.items) if (it.type !== "timeline_marker") body.append(item(it));
    if (markers.length) body.append(markerGroup(markers));
    if (!r.output.items.length) body.append(h("div", { class: "ri" }, h("span", { class: "ri-sub", text: "결과가 비어 있어요. 실패가 아니라 찾은 것이 없다는 뜻이에요." })));
    const noPerm = !r.declared_network && r.declared_audio === "none";
    if (fromAgent(r)) {
      return h("article", { class: `rc${r.stale ? " stale" : ""}` },
        head(r.plugin_id, unknownVersion(r.plugin_version), [
          h("span", { text: dateText(r.created_at) }),
          r.stale ? h("span", { class: "stale-t", text: "오래됨" }) : h("span", { class: "fresh", text: "최신" })],
        agentLine(r)), body);
    }
    return h("article", { class: `rc${r.stale ? " stale" : ""}` },
      head(r.plugin_id, r.plugin_version, [
        h("span", { text: dateText(r.created_at) }),
        noPerm ? h("span", { text: "권한 없음" }) : null,
        isolatedRun(r) ? h("span", { class: "fresh", text: "격리됨" }) : h("span", { class: "c-3", text: "격리 없이 실행됨" }),
        r.stale ? h("span", { class: "stale-t", text: "오래됨" }) : h("span", { class: "fresh", text: "최신" }),
        r.stale ? h("button", { type: "button", class: "link link-fast", text: "다시 실행", onclick: () => o.rerun(r.plugin_id) }) : null],
      perms(r.declared_network, r.declared_audio, isolatedRun(r))), body);
  }

  // what needs attention comes first: failed output, then the running plugin, then stored results
  for (const f of o.failures) {
    const rejected = f.code === "invalid_data" || f.code === "bad_engine_output"; // the contract refused its output
    const keychain = f.code === "secret_access_denied"; // the plugin was not started: the Mac did not hand over its key
    wrap.append(h("article", { class: "rc" },
      h("div", { class: "rc-h" }, h("div", { class: "rc-name" }, f.plugin_id, f.version ? h("span", { text: `@${f.version}` }) : null),
        h("div", { class: "rc-meta" }, h("span", { class: "tag-danger", text: rejected ? "출력 거부됨" : keychain ? "키를 못 꺼냄" : "실행 실패" }), h("span", { text: "저장되지 않았어요" }))),
      h("div", { class: "rej", role: "alert" },
        h("div", { text: rejected ? "플러그인이 계약에 맞지 않는 결과를 돌려줘서 받지 않았어요. 이전 결과는 그대로예요."
          : keychain ? KEYCHAIN_DENIED_RUN : "플러그인을 끝까지 실행하지 못했어요. 이전 결과는 그대로예요." }),
        keychain ? h("div", { class: "c-2", text: KEYCHAIN_DENIED_WHY }) : h("div", { class: "mono", text: f.message }),
        h("div", {}, h("button", { type: "button", class: "btn", text: "다시 실행", onclick: () => o.rerun(f.plugin_id) })))));
  }
  if (o.running) wrap.append(h("article", { class: "rc" }, head(o.running.id, o.running.version, []),
    h("div", { class: "rc-b" }, h("div", { class: "spinner-row", role: "status" }, h("i", { class: "spin" }), h("span", { text: "실행 중" })))));
  for (const r of o.results) wrap.append(card(r));
  return wrap;
}
