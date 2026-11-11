"""meetko plugin: action items and decisions, rule based. Standard library only.

plugin(meeting data) -> action_items + timeline_marker
"""
import json
import re
import sys

PLUGIN = {"id": "meetko/action-items-py", "version": "0.1.0"}
ACTION = re.compile(r"담당|하겠습니다|할게요|action\s*item|액션\s*아이템|review|리뷰", re.I)
DECISION = re.compile(r"결정|decid", re.I)


def run(md):
    words = md["words"]
    pos = {w["id"]: i for i, w in enumerate(words)}
    actions, items = [], []
    for t in md["turns"]:
        run_words = words[pos[t["first_word"]]:pos[t["last_word"]] + 1]
        text = " ".join(w["text"] for w in run_words)
        anchor = {"from": t["first_word"], "to": t["last_word"]}
        if ACTION.search(text):
            actions.append({"text": text[:2000], "owner": t["speaker"], "evidence": [anchor]})
        if DECISION.search(text):
            items.append({"type": "timeline_marker", "kind": "decision", "label": text[:200], "at": anchor})
    if actions:
        items.insert(0, {"type": "action_items", "items": actions[:100]})
    return {"contract": "0.1", "plugin": PLUGIN, "input": {"version_id": md["version"]["id"]}, "items": items}


def handle(msg):
    method, params = msg.get("method"), msg.get("params") or {}
    if method == "initialize":
        if "0.1" not in params.get("contract", []):
            raise RpcError(-32001, "unsupported contract")
        return {"contract": "0.1", "plugin": PLUGIN, "outputs": ["action_items", "timeline_marker"]}
    if method == "run":
        return run(params["input"])
    if method == "shutdown":
        return None
    raise RpcError(-32601, f"method not found: {method}")


class RpcError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def main():
    for line in sys.stdin:
        msg = json.loads(line)
        reply = {"jsonrpc": "2.0", "id": msg.get("id")}
        try:
            reply["result"] = handle(msg)
        except RpcError as e:
            reply["error"] = {"code": e.code, "message": str(e)}
        except Exception as e:  # report, never die silently mid-request
            reply["error"] = {"code": -32010, "message": f"{type(e).__name__}: {e}"}
        sys.stdout.write(json.dumps(reply, ensure_ascii=False) + "\n")
        sys.stdout.flush()
        if msg.get("method") == "shutdown":
            break


if __name__ == "__main__":
    main()
