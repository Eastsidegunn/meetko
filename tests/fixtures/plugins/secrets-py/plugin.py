"""Test fixture: POSTs to OPENAI_BASE_URL/chat/completions with the OPENAI_API_KEY the host gave in
`initialize` and returns the reply text, unredacted, as a summary_card (redaction is the host's job)."""
import json
import sys
import urllib.request

PLUGIN = {"id": "meetko/secrets-fixture", "version": "0.1.0"}
STATE = {}


def run(md):
    s = STATE.get("secrets", {})
    base = STATE.get("settings", {}).get("OPENAI_BASE_URL", "")
    body = json.dumps({"model": STATE.get("settings", {}).get("OPENAI_MODEL", ""), "messages": []}).encode()
    headers = {"Authorization": "Bearer " + s.get("OPENAI_API_KEY", ""), "Content-Type": "application/json"}
    req = urllib.request.Request(base + "/chat/completions", data=body, method="POST", headers=headers)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(req, timeout=20) as r:
        text = json.load(r)["choices"][0]["message"]["content"]
    w = md["words"][0]["id"]
    card = {"type": "summary_card", "title": "fixture", "points": [{"text": text[:2000], "evidence": [{"from": w, "to": w}]}]}
    return {"contract": "0.1", "plugin": PLUGIN, "input": {"version_id": md["version"]["id"]}, "items": [card]}


def handle(msg):
    method, params = msg.get("method"), msg.get("params") or {}
    if method == "initialize":
        STATE["secrets"] = params.get("secrets") or {}
        STATE["settings"] = params.get("settings") or {}
        return {"contract": "0.1", "plugin": PLUGIN, "outputs": ["summary_card"]}
    if method == "run":
        return run(params["input"])
    if method == "shutdown":
        return None
    return None


for line in sys.stdin:
    msg = json.loads(line)
    reply = {"jsonrpc": "2.0", "id": msg.get("id")}
    try:
        reply["result"] = handle(msg)
    except Exception as e:
        reply["error"] = {"code": -32010, "message": type(e).__name__}
    sys.stdout.write(json.dumps(reply) + "\n")
    sys.stdout.flush()
    if msg.get("method") == "shutdown":
        break
