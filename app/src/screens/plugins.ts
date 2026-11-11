import { api, asError, type GalleryEntry, type PluginInfo, type SecretState } from "../api";
import { h, hashBtn, icon, notice } from "../dom";
import { KEYCHAIN_DENIED_CHANGE } from "../labels";
import { openDialog } from "../pop";

/** Object particle for a Korean label: 을 after a final consonant, 를 after a vowel. */
function obj(label: string): string {
  const c = label.charCodeAt(label.length - 1);
  if (c >= 0xac00 && c <= 0xd7a3) return `${label}${(c - 0xac00) % 28 ? "을" : "를"}`;
  return `${label}을(를)`;
}

/** "이 플러그인은 API 키(A, B)와 네트워크를 써요" — said before every install of a plugin that declares keys. */
export function keyNotice(secrets: string[]): HTMLElement | null {
  if (!secrets.length) return null;
  return notice("warn", `이 플러그인은 API 키(${secrets.join(", ")})와 네트워크를 써요.`,
    "키는 설치한 뒤 플러그인 화면의 'API 키 넣기'에서 넣어요. 키는 이 플러그인에만, 실행할 때만 건네져요. 키를 받은 플러그인은 회의 전사본을 키의 서비스로 보낼 수 있고, 키를 다른 용도로 쓸 수도 있어요. 믿는 플러그인에만 키를 넣으세요.");
}

/** Known vendors by name prefix, so a row says in plain Korean what the value is for. */
const VENDORS: [string, string][] = [["ANTHROPIC_", "Anthropic(Claude)"], ["OPENAI_", "OpenAI 호환"], ["GOOGLE_", "Google"], ["GEMINI_", "Google Gemini"], ["MISTRAL_", "Mistral"], ["GROQ_", "Groq"]];

/** "Anthropic(Claude) API 키", "OpenAI 호환 서버 주소", "Anthropic(Claude) 모델 이름"; unknown names stay as they are. */
export function secretLabel(name: string): string {
  const vendor = VENDORS.find(([p]) => name.startsWith(p))?.[1];
  const what = name.endsWith("_BASE_URL") ? "서버 주소" : name.endsWith("_MODEL") ? "모델 이름" : name.endsWith("_TOKEN") ? "토큰" : "API 키";
  return vendor ? `${vendor} ${what}` : name;
}

/** A key dialog failure in plain Korean when the Mac's keychain said no; anything else as the core said it. */
function keyErrorText(e: unknown): string {
  const err = asError(e);
  return err.code === "secret_access_denied" ? KEYCHAIN_DENIED_CHANGE : err.message;
}

/** Set or remove the declared keys and settings of an installed plugin. Keys go one way, to the OS
 *  keychain: this dialog only ever learns 설정됨 / 없음. Settings (a model name, a server address) are
 *  not secret and show their value. */
function openKeys(p: PluginInfo): void {
  openDialog(`${p.id} API 키`, (dlg, close) => {
    const rows = h("div", { class: "secrets" });
    const msg = h("div", { role: "alert" });
    const said = h("p", { class: "c-2 sec-said", role: "status", "aria-live": "polite" });
    /** `focus`: the name whose row should hold focus after a re-render (the row the user just used) */
    const render = (states: SecretState[], focus?: string) => {
      rows.replaceChildren(...states.map((s) => {
        const id = `sec-${s.name}`;
        const secret = s.kind === "secret";
        const input = h("input", {
          id, type: secret ? "password" : "text", class: "sec-input", autocomplete: "off", spellcheck: "false",
          placeholder: secret ? (s.set ? "새 값으로 바꾸기" : "값 붙여넣기") : (s.set ? "" : "비워 두면 기본값"),
        }) as HTMLInputElement;
        if (!secret && s.value) input.value = s.value;
        const save = h("button", { type: "button", class: "btn btn-primary", text: "저장", disabled: true }) as HTMLButtonElement;
        const del = s.set ? h("button", { type: "button", class: "btn", text: "지우기" }) as HTMLButtonElement : null;
        input.addEventListener("input", () => { save.disabled = !input.value.trim() || (!secret && input.value.trim() === (s.value ?? "")); });
        const run = async (f: () => Promise<SecretState[]>, ok: string, fail: string) => {
          msg.replaceChildren(); said.textContent = "";
          save.disabled = true; if (del) del.disabled = true;
          try { render(await f(), s.name); said.textContent = ok; }
          catch (e) { msg.append(notice("danger", fail, keyErrorText(e))); save.disabled = !input.value.trim(); if (del) del.disabled = false; input.focus(); }
        };
        const label = secretLabel(s.name);
        const submit = () => {
          const v = input.value.trim();
          if (!v || save.disabled) return;
          if (secret) input.value = ""; // the field never keeps a key once it is sent
          void run(() => api.pluginSecretSet(p.id, s.name, v), `${obj(label)} 저장했어요.`, "저장하지 못했어요.");
        };
        save.addEventListener("click", submit);
        input.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); submit(); } });
        del?.addEventListener("click", () => void run(() => api.pluginSecretClear(p.id, s.name), `${obj(label)} 지웠어요.`, "지우지 못했어요."));
        const state = secret ? (s.set ? "설정됨" : "없음") : (s.set ? "설정됨" : "기본값");
        const row = h("div", { class: "secrow" },
          h("label", { for: id, class: "lb" }, h("span", { class: "svc", text: label }), label !== s.name ? h("span", { class: "nm", text: s.name }) : null),
          h("span", { class: s.set ? "st tag-ok" : "st", text: state }),
          input, h("div", { class: "secacts" }, del, save));
        if (focus === s.name) queueMicrotask(() => input.focus({ preventScroll: true }));
        return row;
      }));
    };
    dlg.append(h("h2", { text: "API 키 넣기" }),
      h("div", { class: "addbox" },
        h("p", { class: "c-2", text: `${p.id}에만 쓰여요. 키는 이 컴퓨터의 키체인에 저장되고, 저장한 뒤에는 화면에 다시 보이지 않아요. 플러그인을 실행할 때만 건네져요. 모델 이름과 서버 주소는 키가 아니라서 그대로 보여요.` }),
        h("p", { class: "c-2", text: "여기서 저장하면 실행할 때 맥이 따로 묻지 않아요. 명령줄(meetko plugin secret set)로 저장한 키는 이 앱이 처음 쓸 때 맥이 키체인 사용을 허락할지 물을 수 있어요. 그 창이 묻는 암호는 이 맥에 로그인할 때 쓰는 암호예요. 넣은 뒤 ‘항상 허용’을 누르면 다음부터 묻지 않아요. 앱을 새로 설치하거나 업데이트한 뒤에는 한 번 더 물을 수 있어요." }),
        msg, rows, said),
      h("div", { class: "acts" }, h("button", { type: "button", class: "btn", text: "닫기", onclick: close })));
    api.pluginSecrets(p.id).then((st) => render(st, st[0]?.name), (e) => msg.append(notice("danger", "키 상태를 읽지 못했어요.", keyErrorText(e))));
  });
}

/** The permissions as the plugin declared them. meetko 0.1 does not enforce them, and says so. */
export function permTags(p: { network: boolean; audio: string; secrets?: string[] }): HTMLElement[] {
  const tags = [
    p.network ? h("span", { class: "tag tag-warn" }, icon("net", 12), "네트워크") : null,
    p.secrets?.length ? h("span", { class: "tag tag-warn", title: p.secrets.join(", ") }, "API 키") : null,
    p.audio !== "none" ? h("span", { class: "tag tag-warn" }, icon("wave", 12), "오디오") : null,
  ].filter((t): t is HTMLElement => !!t);
  return tags.length ? tags : [h("span", { class: "c-3", text: "없음" })];
}

export async function pluginsScreen(main: HTMLElement): Promise<void> {
  const inner = h("div", { class: "page-in" });
  const msg = h("div", { role: "alert" });
  const list = h("div");
  const add = h("button", { type: "button", class: "btn btn-primary" }, icon("folder", 14), "폴더에서 추가");
  const gallery = h("section", { class: "set-sec gal" });
  inner.append(h("div", { class: "page-head" }, h("h1", { class: "t-title1", text: "플러그인" }), add), msg, list, gallery);
  main.replaceChildren(h("div", { class: "page" }, inner));

  const refresh = async () => {
    list.replaceChildren();
    let ps: PluginInfo[];
    try { ps = await api.listPlugins(); } catch (e) { list.append(notice("danger", "플러그인 목록을 불러오지 못했어요.", asError(e).message)); return; }
    if (!ps.length) {
      list.append(h("div", { class: "empty" }, h("h2", { text: "추가된 플러그인이 없어요" }),
        h("p", { text: "플러그인은 전사본을 읽고 요약, 액션 아이템, 표 같은 결과를 만들어요. 모든 줄이 근거 단어를 가리켜야 해요. 폴더를 골라 추가하면 권한과 해시를 먼저 보여드려요." }),
        h("button", { type: "button", class: "btn btn-primary btn-lg", onclick: () => void pick() }, icon("folder", 14), "폴더에서 추가")));
      return;
    }
    list.append(h("div", { class: "pg-head" }, h("span", { text: "이름" }), h("span", { text: "선언된 권한" }), h("span", { class: "decl" })),
      ...ps.map((p) => h("div", { class: "pgrow" },
        h("div", { class: "pn" }, h("span", { class: "nm", text: p.id, title: p.id }), h("span", { class: "vv", text: `@${p.version}` }), p.description ? h("small", { text: p.description }) : null, p.source ? h("small", { text: p.source }) : null),
        h("div", { class: "perms" }, ...permTags(p)),
        h("div", { class: "decl" },
          h("span", { text: p.isolated ? "격리됨 · 네트워크는 선언했을 때만" : "선언됨 · 강제되지 않음" }),
          p.secrets.length || p.settings.length ? h("button", { type: "button", class: "btn keys-btn", text: "API 키 넣기", onclick: () => openKeys(p) }) : null))));
  };

  /** pick (native folder dialog; the path never reaches JS) -> confirm with permissions and hashes -> add */
  async function pick(): Promise<void> {
    msg.replaceChildren();
    let p: PluginInfo | null;
    try { p = await api.pluginPick(); } catch (e) { msg.append(notice("danger", "플러그인을 읽지 못했어요.", asError(e).message)); return; }
    if (!p) return;
    confirmAdd(p, (code, replace) => api.pluginAdd(code, replace));
  }

  /** the one consent dialog: folder picks and gallery installs both end here, with the same words */
  function confirmAdd(plugin: PluginInfo, doAdd: (codeSha256: string, replace: string | null) => Promise<PluginInfo>): void {
    openDialog(`${plugin.id}@${plugin.version} 추가`, (dlg, close) => {
      const ok = h("button", { type: "button", class: "btn btn-primary", text: plugin.replaces ? "교체" : "추가" });
      // the second confirmation: same id, different source. Shows both and needs its own tick.
      const rep = plugin.replaces;
      const sure = h("input", { type: "checkbox", id: "replace-sure" }) as HTMLInputElement;
      if (rep) {
        ok.disabled = true;
        sure.addEventListener("change", () => { ok.disabled = !sure.checked; });
      }
      ok.addEventListener("click", async () => {
        ok.disabled = true;
        try { await doAdd(plugin.code_sha256, rep?.key ?? null); close(); await refresh(); }
        catch (e) { close(); msg.append(notice("danger", "플러그인을 추가하지 못했어요.", asError(e).message)); }
      });
      dlg.append(h("h2", { text: `${plugin.id}@${plugin.version} 추가` }),
        h("div", { class: "addbox" },
          plugin.description ? h("p", { text: plugin.description }) : null,
          h("div", { class: "path", text: plugin.source ?? plugin.folder }),
          h("dl", { class: "kv" },
            h("dt", { text: "네트워크" }), h("dd", {}, plugin.network ? h("span", { class: "tag-warn", text: "요청함" }) : "요청 안 함"),
            h("dt", { text: "오디오" }), h("dd", { text: plugin.audio === "none" ? "요청 안 함" : plugin.audio }),
            h("dt", { text: "API 키" }), h("dd", {}, plugin.secrets.length ? h("span", { class: "tag-warn", text: plugin.secrets.join(", ") }) : "요청 안 함"),
            plugin.settings.length ? [h("dt", { text: "설정" }), h("dd", { text: plugin.settings.join(", ") })] : null,
            h("dt", { text: "표시 형식" }), h("dd", { text: plugin.display.join(", ") || "—" }),
            h("dt", { text: "manifest" }), h("dd", {}, hashBtn(plugin.manifest_sha256)),
            h("dt", { text: "코드 해시" }), h("dd", {}, hashBtn(plugin.code_sha256))),
          rep ? h("div", { class: "replace-box" },
            notice("danger", "같은 이름의 플러그인이 이미 설치되어 있어요.", "다른 곳에서 온 코드로 바뀌어요. 내가 바꾸려는 게 맞을 때만 교체하세요."),
            h("dl", { class: "kv" },
              h("dt", { text: "지금 설치됨" }), h("dd", { text: `${plugin.id}@${rep.version} · ${rep.source ?? "내 컴퓨터의 폴더"}` }),
              h("dt", { text: "새로 설치됨" }), h("dd", { text: `${plugin.id}@${plugin.version} · ${plugin.source ?? plugin.folder}` })),
            h("label", { for: "replace-sure" }, sure, " 위 내용을 확인했고, 기존 플러그인을 교체합니다")) : null,
          keyNotice(plugin.secrets),
          plugin.keeps_values ? h("p", { class: "c-2", text: "이미 설치된 같은 플러그인을 다시 추가하는 거라, 저장해 둔 API 키와 설정은 그대로 써요." }) : null,
          rep && (plugin.secrets.length || plugin.settings.length) ? h("p", { class: "c-2", text: "교체하면 기존 플러그인에 저장해 둔 API 키와 설정은 넘어가지 않고 지워져요." }) : null,
          plugin.source ? notice("warn", "이 플러그인은 검증되지 않았습니다.", "누구나 올릴 수 있는 인터넷의 코드이고, 둘러보기 목록은 사람이 검토한 것이 아니에요. 내려받은 파일을 그대로(빌드 없이) 실행해요. 위 커밋에 고정되어 설치돼요.") : null,
          plugin.isolated
            ? notice("warn", "이 플러그인은 격리된 상태로 실행돼요. 그래도 믿는 코드만 추가하세요.",
              "막는 것: 네트워크(선언했을 때만 허용), 내 홈 폴더, meetko 데이터(녹음·기록), 다른 임시 파일, 외부 드라이브를 읽거나 쓰는 것, 다른 프로그램을 실행하는 것, 다른 프로세스의 목록과 환경변수를 보는 것. 막지 못하는 것: 여전히 내 컴퓨터에서 실행되고, CPU·메모리는 제한되지 않아요. 네트워크 권한을 선언한 플러그인은 자기가 읽은 내용을 밖으로 보낼 수 있어요. 읽는 것은 건네준 회의 전사본뿐이에요. 코드나 manifest가 나중에 바뀌면 다시 추가하기 전까지 실행되지 않아요.")
            : notice("warn", "이 권한은 선언일 뿐 강제되지 않아요.", `플러그인은 내 계정 권한으로 실행돼서 선언과 다르게 동작해도 막을 수 없어요. 직접 확인한 코드만 추가하세요. 코드나 manifest가 나중에 바뀌면 다시 추가하기 전까지 실행되지 않아요.`)),
        h("div", { class: "acts" }, h("button", { type: "button", class: "btn", text: "취소", onclick: close }), ok));
    });
  }

  /** 둘러보기: nothing leaves the machine until a button is pressed; the download is done by the app, not this page */
  function showGalleryIntro(): void {
    const load = h("button", { type: "button", class: "btn", onclick: () => void browse() }, icon("net", 14), "목록 불러오기");
    gallery.replaceChildren(h("h2", { text: "둘러보기" }),
      h("p", { class: "c-2", text: "GitHub에 올라온 meetko 플러그인 목록이에요. 버튼을 누를 때만 인터넷에 연결하고, 누르기 전에는 아무것도 내려받지 않아요. 목록은 사람이 검토한 것이 아니에요." }),
      h("div", {}, load));
  }

  async function browse(): Promise<void> {
    msg.replaceChildren();
    gallery.replaceChildren(h("h2", { text: "둘러보기" }), h("p", { class: "c-2", text: "목록을 불러오는 중이에요…" }));
    let items: GalleryEntry[];
    try { items = await api.galleryList(); }
    catch (e) {
      const err = asError(e);
      showGalleryIntro();
      gallery.append(err.code === "gallery_unavailable"
        ? notice("info", "플러그인 갤러리가 아직 열리지 않았어요.", "명령줄에서 GitHub 주소(meetko plugin install github:OWNER/REPO@REF)로 직접 설치할 수 있어요.")
        : notice("danger", "목록을 불러오지 못했어요.", err.message));
      return;
    }
    const again = h("button", { type: "button", class: "btn", text: "다시 불러오기", onclick: () => void browse() });
    gallery.replaceChildren(h("h2", { text: "둘러보기" }));
    if (!items.length) { gallery.append(h("p", { class: "c-2", text: "아직 올라온 플러그인이 없어요." }), h("div", {}, again)); return; }
    gallery.append(h("div", { class: "pg-head" }, h("span", { text: "이름" }), h("span", { text: "선언된 권한" }), h("span", { class: "decl" })),
      ...items.map((g) => {
        const inst = h("button", { type: "button", class: "btn", text: "설치" });
        inst.addEventListener("click", () => void preview(g, inst));
        return h("div", { class: "pgrow" },
          h("div", { class: "pn" }, h("span", { class: "nm", text: g.id, title: g.id }), h("span", { class: "vv", text: `@${g.version}` }),
            g.description ? h("small", { text: g.description }) : null,
            h("small", { text: `github.com/${g.repo} · ${g.ref} · ★${g.stars}${g.license ? ` · ${g.license}` : ""}` })),
          h("div", { class: "perms" }, ...permTags({ network: g.permissions.network, audio: g.permissions.audio ? "full" : "none", secrets: g.permissions.secrets })),
          h("div", { class: "decl" }, inst));
      }),
      h("div", {}, again));
  }

  /** downloads the pinned commit, then shows the same consent as a folder add; installs only on the click there */
  async function preview(g: GalleryEntry, btn: HTMLButtonElement): Promise<void> {
    msg.replaceChildren();
    btn.disabled = true; btn.textContent = "내려받는 중…";
    try { const pv = await api.galleryPreview(g.id);
      confirmAdd(pv, (code, replace) => api.galleryInstall(pv.token ?? "", code, replace)); }
    catch (e) { msg.append(notice("danger", "플러그인을 내려받지 못했어요.", asError(e).message)); }
    finally { btn.disabled = false; btn.textContent = "설치"; }
  }
  add.addEventListener("click", () => void pick());
  showGalleryIntro();
  await refresh();
}
