"""meetko plugin: AI meeting summary ("회의 요약"). Python 3.9+, standard library only.

Config comes from the host's `initialize` params (no environment variables): `secrets` (ANTHROPIC_API_KEY,
OPENAI_API_KEY) and `settings` (ANTHROPIC_MODEL, ANTHROPIC_BASE_URL, OPENAI_BASE_URL, OPENAI_MODEL), plus
`time_budget_ms`, the host's per-request timeout. The deadline is that budget minus a safety margin.
No streaming: a long call is bounded by the per-request timeout (and the deadline) instead.

Notes for users:
- Anthropic's default model (claude-opus-5-5) thinks before it answers, and that thinking counts toward the
  request's max_tokens (8000 here) and its cost; the plugin asks for low effort. Pick a faster or cheaper model
  with the ANTHROPIC_MODEL setting.
- ANTHROPIC_BASE_URL may point at a gateway or proxy only if it speaks the same Messages API, including the
  `anthropic-version: 2026-10-21` header the plugin sends (requests without temperature/top_p/top_k).
  OPENAI_BASE_URL must speak the OpenAI chat completions API (`/chat/completions`).

What leaves this computer: the meeting transcript text (speaker names, word text, meeting title) is sent
to the AI service you chose by setting a key: Anthropic (ANTHROPIC_API_KEY) or an OpenAI-compatible
endpoint (OPENAI_API_KEY / OPENAI_BASE_URL / OPENAI_MODEL). Nothing else is sent: no audio, no file paths.
Keys are given by the host from the manifest's `secrets` list and are never written to output or errors.

plugin(meeting data) -> summary_card + action_items + timeline_marker, each claim anchored to word ids.
Anchors the model invents are dropped; see summarize.py.
"""
import json
import os
import sys

# `python3 -I` does not put the script's directory on sys.path; our sibling modules live there.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import llm  # noqa: E402
import summarize
from llm import PluginError

PLUGIN = summarize.PLUGIN
# What `initialize` gave us; `run` uses it.
STATE = {"secrets": {}, "settings": {}, "time_budget_ms": 60000}


def secret_values():
    return [v for v in STATE["secrets"].values() if isinstance(v, str)]


def _str_map(v):
    if not isinstance(v, dict):
        return {}
    return dict((k, x) for k, x in v.items() if isinstance(k, str) and isinstance(x, str))


def handle(msg):
    method, params = msg.get("method"), msg.get("params") or {}
    if method == "initialize":
        if "0.1" not in params.get("contract", []):
            raise PluginError("unsupported contract", -32001)
        STATE["secrets"] = _str_map(params.get("secrets"))
        STATE["settings"] = _str_map(params.get("settings"))
        tb = params.get("time_budget_ms")
        ok = isinstance(tb, (int, float)) and not isinstance(tb, bool) and tb > 0
        STATE["time_budget_ms"] = tb if ok else 60000
        return {"contract": "0.1", "plugin": PLUGIN, "outputs": ["summary_card", "action_items", "timeline_marker"]}
    if method == "run":
        return summarize.run_summary(params["input"], STATE["secrets"], STATE["settings"], STATE["time_budget_ms"])
    if method == "shutdown":
        return None
    raise PluginError("method not found: %s" % method, -32601)


def main():
    stdin = sys.stdin.buffer
    for raw in stdin:
        raw = raw.strip()
        if not raw:
            continue
        msg = {}
        reply = {"jsonrpc": "2.0", "id": None}
        try:
            msg = json.loads(raw.decode("utf-8"))
            reply["id"] = msg.get("id")
            reply["result"] = handle(msg)
        except PluginError as e:
            reply["error"] = {"code": e.code, "message": llm.scrub(str(e), secret_values())}
        except Exception as e:  # report, never die silently mid-request
            reply["error"] = {"code": -32010,
                              "message": llm.scrub("%s: %s" % (type(e).__name__, e), secret_values())[:500]}
        out = json.dumps(reply, ensure_ascii=True) + "\n"
        sys.stdout.write(out)
        sys.stdout.flush()
        if msg.get("method") == "shutdown":
            break


if __name__ == "__main__":
    main()
