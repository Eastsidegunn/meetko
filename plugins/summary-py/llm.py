"""HTTP calls to Anthropic Messages or an OpenAI-compatible chat API. urllib only.
The key is never put in an error message; the caller scrubs every message as well."""
import ipaddress
import json
import time
import urllib.error
import urllib.parse
import urllib.request

DEFAULT_ANTHROPIC_MODEL = "claude-opus-5-5"
DEFAULT_OPENAI_MODEL = "gpt-4.1-mini"
DEFAULT_ANTHROPIC_BASE = "https://api.anthropic.com"
DEFAULT_OPENAI_BASE = "https://api.openai.com/v1"
ANTHROPIC_VERSION = "2026-10-21"
MAX_REQUEST_SECS = 300.0
RETRY_SLEEP_SECS = 2.0
MIN_REMAINING_SECS = 1.0
MAX_ERROR_BODY = 65536


class PluginError(Exception):
    def __init__(self, message, code=-32010):
        super().__init__(message)
        self.code = code


def scrub(text, secrets):
    for s in secrets:
        if s and len(s) >= 4:
            text = text.replace(s, "***")
    return text


def is_loopback(host):
    if host.lower() == "localhost":
        return True
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


def check_base_url(url, what):
    try:
        p = urllib.parse.urlsplit(url)
        host = p.hostname
        p.port  # raises ValueError on a bad port
    except ValueError:
        raise PluginError("%s 주소가 올바르지 않아요. (invalid URL)" % what)
    if p.scheme not in ("http", "https") or not host:
        raise PluginError("%s 주소는 http:// 또는 https:// 로 시작해야 해요." % what)
    if p.username is not None or p.password is not None or "@" in p.netloc:
        raise PluginError("%s 주소에 사용자 정보(@)를 넣을 수 없어요." % what)
    if p.scheme == "http" and not is_loopback(host):
        raise PluginError("%s: http:// 는 이 컴퓨터(localhost)만 허용해요. 다른 서버는 https:// 를 쓰세요. "
                          "(plain http is only allowed for loopback hosts)" % what)
    return url.rstrip("/")


class Config(object):
    def __init__(self, provider, base, key, model):
        self.provider, self.base, self.key, self.model = provider, base, key, model

    def secrets(self):
        return [self.key] if self.key else []


def _get(d, name):
    v = (d or {}).get(name)
    return v.strip() if isinstance(v, str) else ""


def load_config(secrets, settings):
    """Pick the provider from what the host gave in `initialize` (secrets, settings).
    Raises PluginError if nothing is configured."""
    ak = _get(secrets, "ANTHROPIC_API_KEY")
    if ak:
        base = check_base_url(_get(settings, "ANTHROPIC_BASE_URL") or DEFAULT_ANTHROPIC_BASE, "ANTHROPIC_BASE_URL")
        return Config("anthropic", base, ak, _get(settings, "ANTHROPIC_MODEL") or DEFAULT_ANTHROPIC_MODEL)
    ok = _get(secrets, "OPENAI_API_KEY")
    ob = _get(settings, "OPENAI_BASE_URL")
    if ok or ob:
        base = check_base_url(ob or DEFAULT_OPENAI_BASE, "OPENAI_BASE_URL")
        return Config("openai", base, ok, _get(settings, "OPENAI_MODEL") or DEFAULT_OPENAI_MODEL)
    raise PluginError(
        "AI 서비스 키가 없어요. 플러그인 화면에서 \"API 키 넣기\"로 ANTHROPIC_API_KEY (또는 OPENAI_API_KEY / OPENAI_BASE_URL)를 넣어 주세요. "
        "명령줄: meetko plugin secret set meetko/summary-py ANTHROPIC_API_KEY "
        "(No API key is set. Set ANTHROPIC_API_KEY or OPENAI_* for this plugin.)")


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None  # surfaces as an HTTPError with a 3xx code


def _opener():
    # no proxies: the host controls the network
    return urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect())


def _provider_message(raw):
    try:
        d = json.loads(raw.decode("utf-8", "replace"))
        e = d.get("error") if isinstance(d, dict) else None
        if isinstance(e, dict):
            return str(e.get("message") or "")
        if isinstance(e, str):
            return e
    except Exception:
        pass
    return raw.decode("utf-8", "replace")


def _http_error(code, raw, secrets, wait=None):
    detail = scrub(_provider_message(raw), secrets)[:300].strip()
    if code == 402:
        return PluginError("AI 서비스 결제나 크레딧에 문제가 있어요 (402). 계정의 결제 상태와 남은 크레딧을 확인해 주세요. (billing problem)")
    if code in (401, 403):
        return PluginError("API 키가 거부됐어요 (%d). 키가 맞는지, 권한이 있는지 확인해 주세요. (authentication failed)" % code)
    if code == 429:
        when = ("%d초 뒤에 다시 시도해 주세요." % int(wait + 0.999)) if wait is not None else "잠시 후 다시 시도해 주세요."
        return PluginError("요청이 너무 많아요 (429). %s (rate limited, retry later)" % when)
    if code in (400, 404, 413, 422):
        return PluginError("AI 서비스가 요청을 거부했어요 (%d): %s" % (code, detail))
    if code >= 500:
        return PluginError("AI 서비스가 지금 응답하지 못해요 (%d). 잠시 후 다시 시도해 주세요. (provider unavailable)" % code)
    return PluginError("예상하지 못한 응답이에요 (%d): %s" % (code, detail))


def budget_error():
    return PluginError("제한 시간이 다 됐어요. 회의가 길어서 그 안에 요약하지 못했어요. 호스트 설정 MEETKO_PLUGIN_TIMEOUT_SECS 로 제한 시간을 늘릴 수 있어요. "
                       "(time limit ran out; raise MEETKO_PLUGIN_TIMEOUT_SECS)")


def _retry_after(e):
    """Seconds from a Retry-After header (numeric form only), else None."""
    try:
        v = float((e.headers.get("retry-after") or "").strip())
    except (ValueError, AttributeError):
        return None
    return v if 0 <= v < 1e6 else None


def post_json(url, headers, body, deadline, secrets):
    """POST JSON, return the parsed reply. At most one retry on 429/5xx."""
    data = json.dumps(body).encode("utf-8")
    wait = None
    for attempt in (0, 1):
        rem = deadline - time.monotonic()
        if rem < MIN_REMAINING_SECS:
            raise budget_error()
        req = urllib.request.Request(url, data=data, method="POST", headers=headers)
        try:
            with _opener().open(req, timeout=min(rem, MAX_REQUEST_SECS)) as r:
                raw = r.read(64 * 1024 * 1024)
            try:
                return json.loads(raw.decode("utf-8"))
            except ValueError:
                raise PluginError("AI 서비스 응답을 읽을 수 없어요. (response is not JSON)")
        except urllib.error.HTTPError as e:
            try:
                raw = e.read(MAX_ERROR_BODY)
            except Exception:
                raw = b""
            if 300 <= e.code < 400:
                raise PluginError("AI 서비스 주소가 다른 곳으로 넘어가려 해서 막았어요 (%d). 주소를 확인해 주세요. (redirect refused)" % e.code)
            wait = _retry_after(e)
            err = _http_error(e.code, raw, secrets, wait)
            retry = e.code == 429 or e.code >= 500
        except PluginError:
            raise
        except Exception as e:  # timeout, connection refused, DNS, TLS ...
            text = scrub("%s: %s" % (type(e).__name__, e), secrets)[:200]
            if "timed out" in text.lower():
                err = PluginError("AI 서비스가 제때 답하지 않았어요. (request timed out)")
            else:
                err = PluginError("네트워크 오류로 AI 서비스에 닿지 못했어요: %s" % text)
            retry, wait = False, None
        if attempt == 0 and retry:
            rem2 = deadline - time.monotonic()
            if wait is not None:
                if wait + 5 < rem2:  # the server told us how long; honour it only if it fits
                    time.sleep(wait)
                    continue
            elif rem2 > RETRY_SLEEP_SECS + 5:
                time.sleep(RETRY_SLEEP_SECS)
                continue
        raise err
    raise budget_error()


def complete(cfg, system, user, max_tokens, deadline):
    """Return (text, truncated)."""
    secrets = cfg.secrets()
    if cfg.provider == "anthropic":
        headers = {"content-type": "application/json", "x-api-key": cfg.key, "anthropic-version": ANTHROPIC_VERSION}
        body = {"model": cfg.model, "max_tokens": max_tokens, "system": system,
                "messages": [{"role": "user", "content": user}], "output_config": {"effort": "low"}}
        d = post_json(cfg.base + "/v1/messages", headers, body, deadline, secrets)
        if not isinstance(d, dict):
            raise PluginError("AI 서비스 응답 형식이 달라요. (unexpected response)")
        stop = d.get("stop_reason")
        if stop == "refusal":
            raise PluginError("AI가 이 내용의 요약을 거절했어요. (model refusal)")
        blocks = d.get("content") or []
        text = "".join(b.get("text", "") for b in blocks if isinstance(b, dict) and b.get("type") == "text")
        return text, stop == "max_tokens"
    headers = {"content-type": "application/json"}
    if cfg.key:
        headers["Authorization"] = "Bearer " + cfg.key
    # api.openai.com wants max_completion_tokens (max_tokens is deprecated and rejected by reasoning
    # models); local OpenAI-compatible servers (Ollama, llama.cpp, ...) still expect max_tokens.
    limit_key = "max_completion_tokens" if (urllib.parse.urlsplit(cfg.base).hostname or "").lower() == "api.openai.com" else "max_tokens"
    body = {"model": cfg.model, limit_key: max_tokens,
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}]}
    d = post_json(cfg.base + "/chat/completions", headers, body, deadline, secrets)
    try:
        ch = d["choices"][0]
        return ch["message"]["content"] or "", ch.get("finish_reason") == "length"
    except (KeyError, IndexError, TypeError):
        raise PluginError("AI 서비스 응답 형식이 달라요. (unexpected response shape)")
