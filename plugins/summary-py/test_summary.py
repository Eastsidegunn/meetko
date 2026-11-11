"""Tests for the summary plugin. Standard library only; no real network (a fake server on 127.0.0.1).

Run: python3 -I -m unittest   (from this directory)   or   python3 -I plugins/summary-py/test_summary.py
"""
import copy
import json
import os
import subprocess
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
FIXTURE = os.path.join(ROOT, "contract", "fixtures", "meeting01", "meeting-data.draft.json")
SCHEMA = os.path.join(ROOT, "contract", "schemas", "plugin-output.schema.json")
sys.path.insert(0, HERE)

import llm  # noqa: E402
import summarize  # noqa: E402
import templates  # noqa: E402

KEY = "test-key-not-real"


def load_fixture():
    with open(FIXTURE, encoding="utf-8") as f:
        return json.load(f)


CANNED = {
    "one_line": {"text": "지난주 인시던트의 원인과 후속 조치를 정리했다.", "evidence": [["w00000", "w00022"]]},
    "points": [
        {"text": "valid point", "evidence": [["w00023", "w00042"]]},
        {"text": "invented point", "evidence": [["w09998", "w09999"]]},
        {"text": "mixed point", "evidence": [["w00043", "w00058"], ["w09998", "w09999"]]},
        {"text": "reversed point", "evidence": [["w00068", "w00059"]]},
        {"text": "no evidence point", "evidence": []},
    ],
    "decisions": [{"text": "롤백 절차를 문서화한다", "evidence": [["w00076", "w00091"]]}],
    "actions": [
        {"text": "문서 작성", "owner": "s0", "due_spoken": "다음 주 금요일", "due_date": None, "evidence": [["w00092", "w00095"]]},
        {"text": "알림 추가", "owner": "상대 1", "due_spoken": "2026년 12월 1일", "due_date": "2026-12-01", "evidence": [["w00096", "w00108"]]},
        {"text": "unknown owner", "owner": "nobody", "due": None, "due_date": "내일", "evidence": [["w00001", "w00002"]]},
        {"text": "invented action", "owner": "s0", "evidence": [["w12345", "w12346"]]},
    ],
    "extra": [{"text": "원인 쟁점", "evidence": [["w00069", "w00075"]]}],
}


class Fake(object):
    """Fake provider. handler(path, headers, body_dict) -> (status, body_dict_or_str, extra_headers)."""

    def __init__(self, handler):
        self.requests = []
        self.handler = handler
        outer = self

        class H(BaseHTTPRequestHandler):
            def do_POST(self):
                n = int(self.headers.get("content-length") or 0)
                body = json.loads(self.rfile.read(n).decode("utf-8"))
                outer.requests.append({"path": self.path, "headers": dict(self.headers.items()), "body": body})
                status, payload, extra = outer.handler(self.path, self.headers, body)
                data = payload.encode() if isinstance(payload, str) else json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                for k, v in (extra or {}).items():
                    self.send_header(k, v)
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *a):
                pass

        self.server = HTTPServer(("127.0.0.1", 0), H)
        self.url = "http://127.0.0.1:%d" % self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def anthropic_reply(obj, stop="end_turn", fence=False):
    text = json.dumps(obj, ensure_ascii=False)
    if fence:
        text = "```json\n" + text + "\n```"
    return {"content": [{"type": "thinking", "thinking": "hmm"}, {"type": "text", "text": text}], "stop_reason": stop}


SECRET_NAMES = ("ANTHROPIC_API_KEY", "OPENAI_API_KEY")


def split_cfg(env):
    return ({k: v for k, v in env.items() if k in SECRET_NAMES},
            {k: v for k, v in env.items() if k not in SECRET_NAMES})


def run_summary_env(md, env, **kw):
    sec, setg = split_cfg(env)
    return summarize.run_summary(md, sec, setg, **kw)


def run_plugin(env, md=None, time_budget_ms=None, process_env=None):
    """Run the plugin as the host would (config only via initialize); return (init_reply, run_reply, stderr)."""
    md = md or load_fixture()
    sec, setg = split_cfg(env)
    params = {"contract": ["0.1"]}
    if sec:
        params["secrets"] = sec
    if setg:
        params["settings"] = setg
    if time_budget_ms is not None:
        params["time_budget_ms"] = time_budget_ms
    msgs = [{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params},
            {"jsonrpc": "2.0", "id": 2, "method": "run", "params": {"input": md}},
            {"jsonrpc": "2.0", "id": 3, "method": "shutdown"}]
    e = {"PYTHONUTF8": "1"}
    e.update(process_env or {})
    p = subprocess.run([sys.executable, "-I", "-S", "plugin.py"], cwd=HERE, env=e, timeout=60,
                       input="".join(json.dumps(m) + "\n" for m in msgs).encode("utf-8"),
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    replies = [json.loads(l) for l in p.stdout.decode("utf-8").splitlines() if l.strip()]
    return replies[0], replies[1], p.stderr.decode("utf-8", "replace")


def host_rule_errors(md, out):
    """Port of the host's output_errors anchor/owner rules (crates/meetko-core/src/contract.rs)."""
    idx = {w["id"]: i for i, w in enumerate(md["words"])}
    spk = {s["id"] for s in md["speakers"]}
    errs = []

    def anc(a):
        if a["from"] not in idx or a["to"] not in idx:
            errs.append("unknown id %s" % a)
        elif idx[a["from"]] > idx[a["to"]]:
            errs.append("from after to %s" % a)
    for it in out["items"]:
        t = it["type"]
        if t == "timeline_marker":
            anc(it["at"])
        elif t == "summary_card":
            for p in it["points"]:
                [anc(a) for a in p["evidence"]]
        elif t == "action_items":
            for x in it["items"]:
                [anc(a) for a in x["evidence"]]
                if x.get("owner") is not None and x["owner"] not in spk:
                    errs.append("owner %s" % x["owner"])
    if out["input"]["version_id"] != md["version"]["id"]:
        errs.append("version")
    return errs


def schema_errors(out):
    try:
        import jsonschema
    except ImportError:
        return None
    with open(SCHEMA, encoding="utf-8") as f:
        schema = json.load(f)
    v = jsonschema.Draft202012Validator(schema, format_checker=jsonschema.FormatChecker())
    return [e.message for e in v.iter_errors(out)]


class ProtocolTests(unittest.TestCase):
    def check_ok_output(self, out):
        md = load_fixture()
        self.assertEqual(host_rule_errors(md, out), [])
        se = schema_errors(out)
        if se is not None:
            self.assertEqual(se, [])
        self.assertNotIn(KEY, json.dumps(out))

    def test_anthropic_happy_path(self):
        fake = Fake(lambda p, h, b: (200, anthropic_reply(CANNED, fence=True), None))
        self.addCleanup(fake.close)
        init, rep, err = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        self.assertEqual(init["result"]["outputs"], ["summary_card", "action_items", "timeline_marker"])
        self.assertNotIn("error", rep, rep)
        self.assertEqual(len(fake.requests), 1)
        r = fake.requests[0]
        self.assertEqual(r["path"], "/v1/messages")
        hdr = {k.lower(): v for k, v in r["headers"].items()}
        self.assertEqual(hdr["x-api-key"], KEY)
        self.assertEqual(hdr["anthropic-version"], "2026-10-21")
        for k in ("temperature", "top_p", "top_k", "thinking"):
            self.assertNotIn(k, r["body"])
        self.assertEqual(r["body"]["model"], llm.DEFAULT_ANTHROPIC_MODEL)
        self.assertEqual(r["body"]["output_config"], {"effort": "low"})
        self.assertIn("[w00000-w00008] 상대 1:", r["body"]["messages"][0]["content"])
        out = rep["result"]
        self.check_ok_output(out)
        card = [i for i in out["items"] if i["type"] == "summary_card"][0]
        texts = [p["text"] for p in card["points"]]
        self.assertTrue(any("valid point" in t for t in texts))
        self.assertTrue(any("mixed point" in t for t in texts))
        self.assertFalse(any("invented" in t for t in texts))
        self.assertFalse(any("no evidence" in t for t in texts))
        mixed = [p for p in card["points"] if "mixed point" in p["text"]][0]
        self.assertEqual(mixed["evidence"], [{"from": "w00043", "to": "w00058"}])
        rev = [p for p in card["points"] if "reversed point" in p["text"]][0]
        self.assertEqual(rev["evidence"], [{"from": "w00059", "to": "w00068"}])
        acts = [i for i in out["items"] if i["type"] == "action_items"][0]["items"]
        self.assertEqual([a["text"].split(" (")[0] for a in acts], ["문서 작성", "알림 추가", "unknown owner"])
        self.assertEqual(acts[0]["owner"], "s0")
        self.assertIn("다음 주 금요일", acts[0]["text"])
        self.assertEqual(acts[1]["owner"], "s1")
        self.assertEqual(acts[1]["due"], "2026-12-01")
        self.assertNotIn("owner", acts[2])
        self.assertNotIn("due", acts[2])
        marks = [i for i in out["items"] if i["type"] == "timeline_marker"]
        self.assertEqual([m["kind"] for m in marks], ["decision"])
        self.assertIn("기술 리뷰", card["title"])  # fixture title: "incident retro"

    def test_every_anchor_exists_in_input(self):
        fake = Fake(lambda p, h, b: (200, anthropic_reply(CANNED), None))
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        ids = {w["id"] for w in load_fixture()["words"]}
        n = 0
        for it in rep["result"]["items"]:
            ancs = []
            if it["type"] == "timeline_marker":
                ancs = [it["at"]]
            elif it["type"] == "summary_card":
                ancs = [a for p in it["points"] for a in p["evidence"]]
            elif it["type"] == "action_items":
                ancs = [a for x in it["items"] for a in x["evidence"]]
            for a in ancs:
                n += 1
                self.assertIn(a["from"], ids)
                self.assertIn(a["to"], ids)
        self.assertGreater(n, 5)

    def test_openai_compatible(self):
        def h(path, headers, body):
            return 200, {"choices": [{"message": {"content": json.dumps(CANNED)}, "finish_reason": "stop"}]}, None
        fake = Fake(h)
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"OPENAI_API_KEY": KEY, "OPENAI_BASE_URL": fake.url + "/v1", "OPENAI_MODEL": "my-model"})
        self.assertNotIn("error", rep, rep)
        r = fake.requests[0]
        hdr = {k.lower(): v for k, v in r["headers"].items()}
        self.assertEqual(r["path"], "/v1/chat/completions")
        self.assertEqual(hdr["authorization"], "Bearer " + KEY)
        self.assertEqual(r["body"]["model"], "my-model")
        self.assertEqual(r["body"]["messages"][0]["role"], "system")
        self.check_ok_output(rep["result"])

    def test_openai_without_key_has_no_auth_header(self):
        fake = Fake(lambda p, h, b: (200, {"choices": [{"message": {"content": json.dumps(CANNED)}}]}, None))
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"OPENAI_BASE_URL": fake.url})
        self.assertNotIn("error", rep, rep)
        hdr = {k.lower(): v for k, v in fake.requests[0]["headers"].items()}
        self.assertNotIn("authorization", hdr)
        self.assertEqual(fake.requests[0]["body"]["model"], llm.DEFAULT_OPENAI_MODEL)

    def test_secret_never_echoed(self):
        msg = "bad request for key %s: invalid" % KEY
        fake = Fake(lambda p, h, b: (400, {"error": {"type": "invalid_request_error", "message": msg}}, None))
        self.addCleanup(fake.close)
        init, rep, err = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        self.assertEqual(rep["error"]["code"], -32010)
        self.assertIn("400", rep["error"]["message"])
        self.assertIn("invalid", rep["error"]["message"])
        self.assertNotIn(KEY, json.dumps(rep))
        self.assertNotIn(KEY, err)

    def test_missing_key(self):
        _, rep, _ = run_plugin({})
        self.assertEqual(rep["error"]["code"], -32010)
        self.assertIn("ANTHROPIC_API_KEY", rep["error"]["message"])
        self.assertIn("API 키 넣기", rep["error"]["message"])
        self.assertIn("meetko plugin secret set meetko/summary-py", rep["error"]["message"])

    def test_401(self):
        fake = Fake(lambda p, h, b: (401, {"error": {"message": "invalid x-api-key " + KEY}}, None))
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        self.assertIn("API 키가 거부됐어요 (401)", rep["error"]["message"])
        self.assertNotIn(KEY, rep["error"]["message"])
        self.assertEqual(len(fake.requests), 1)

    def test_429_retries_once_then_rate_limit_text(self):
        old = llm.RETRY_SLEEP_SECS
        fake = Fake(lambda p, h, b: (429, {"error": {"message": "slow down"}}, None))
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        self.assertIn("429", rep["error"]["message"])
        self.assertIn("잠시 후", rep["error"]["message"])
        self.assertEqual(len(fake.requests), 2)  # one retry at most
        self.assertEqual(llm.RETRY_SLEEP_SECS, old)

    def test_refusal(self):
        fake = Fake(lambda p, h, b: (200, {"content": [], "stop_reason": "refusal"}, None))
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        self.assertIn("거절", rep["error"]["message"])

    def test_max_tokens_truncated_unparsable_is_error(self):
        fake = Fake(lambda p, h, b: (200, {"content": [{"type": "text", "text": '{"one_line": {"text": "x'}],
                                           "stop_reason": "max_tokens"}, None))
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        self.assertIn("잘려", rep["error"]["message"])

    def test_non_loopback_http_refused(self):
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": "http://example.com"})
        self.assertIn("http://", rep["error"]["message"])
        self.assertIn("localhost", rep["error"]["message"])
        _, rep, _ = run_plugin({"OPENAI_API_KEY": KEY, "OPENAI_BASE_URL": "http://10.0.0.5/v1"})
        self.assertIn("error", rep)

    def test_userinfo_and_scheme_refused(self):
        for u in ("https://user:pw@api.example.com", "http://u@127.0.0.1:1", "ftp://127.0.0.1", "file:///etc/passwd"):
            with self.assertRaises(llm.PluginError, msg=u):
                llm.check_base_url(u, "X")

    def test_redirect_refused(self):
        target = Fake(lambda p, h, b: (200, anthropic_reply(CANNED), None))
        self.addCleanup(target.close)
        fake = Fake(lambda p, h, b: (302, "", {"Location": target.url + "/v1/messages"}))
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        self.assertIn("error", rep)
        self.assertIn("redirect", rep["error"]["message"])
        self.assertEqual(target.requests, [])

    def test_connection_refused_is_network_error(self):
        f = Fake(lambda p, h, b: (200, {}, None))
        url = f.url
        f.close()
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": url})
        self.assertIn("네트워크", rep["error"]["message"])

    def test_unknown_method_and_bad_contract(self):
        p = subprocess.run([sys.executable, "-I", "-S", "plugin.py"], cwd=HERE, env={"PYTHONUTF8": "1"}, timeout=30,
                           input=b'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"contract":["9"]}}\n'
                                 b'{"jsonrpc":"2.0","id":2,"method":"nope"}\n',
                           stdout=subprocess.PIPE)
        r = [json.loads(l) for l in p.stdout.decode().splitlines()]
        self.assertEqual(r[0]["error"]["code"], -32001)
        self.assertEqual(r[1]["error"]["code"], -32601)


class InProcessTests(unittest.TestCase):
    def setUp(self):
        self.saved = (summarize.SINGLE_CALL_TOKENS, summarize.CHUNK_TOKENS, llm.RETRY_SLEEP_SECS,
                      summarize.MIN_BUDGET_SECS)
        llm.RETRY_SLEEP_SECS = 0

    def tearDown(self):
        (summarize.SINGLE_CALL_TOKENS, summarize.CHUNK_TOKENS, llm.RETRY_SLEEP_SECS,
         summarize.MIN_BUDGET_SECS) = self.saved

    def env(self, fake):
        return {"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url}

    def test_lines_use_real_ids_and_cover_all_words(self):
        md = load_fixture()
        lines = summarize.build_lines(md)
        self.assertEqual(lines[0]["from"], "w00000")
        self.assertEqual(lines[-1]["to"], "w00108")
        ids = [w["id"] for w in md["words"]]
        covered = []
        for ln in lines:
            covered.extend(ids[ids.index(ln["from"]):ids.index(ln["to"]) + 1])
        self.assertEqual(covered, ids)  # contiguous, no overlap, no gap
        self.assertTrue(lines[0]["rendered"].startswith("[w00000-w00008] 상대 1: "))

    def test_long_turn_split_at_max_words(self):
        md = load_fixture()
        old = summarize.LINE_MAX_WORDS
        summarize.LINE_MAX_WORDS = 5
        try:
            lines = summarize.build_lines(md)
        finally:
            summarize.LINE_MAX_WORDS = old
        ids = [w["id"] for w in md["words"]]
        self.assertTrue(all(ids.index(l["to"]) - ids.index(l["from"]) < 5 for l in lines))

    def test_chunk_lines_respects_budget(self):
        lines = summarize.build_lines(load_fixture())
        chunks = summarize.chunk_lines(lines, 120)
        self.assertGreater(len(chunks), 1)
        self.assertEqual([l for c in chunks for l in c], lines)
        for c in chunks:
            if len(c) > 1:
                self.assertLessEqual(sum(summarize.estimate_tokens(l["rendered"]) + 1 for l in c), 120)

    def test_parse_model_json(self):
        p = summarize.parse_model_json
        self.assertEqual(p('```json\n{"a": 1}\n```'), {"a": 1})
        self.assertEqual(p('설명입니다 {"a": {"b": 2}} 끝'), {"a": {"b": 2}})
        self.assertEqual(p('{bad} {"ok": true}'), {"ok": True})
        self.assertIsNone(p("no json"))
        self.assertIsNone(p(""))

    def test_clean_evidence_forms(self):
        pos = {"w1": 0, "w2": 1, "w3": 2, "a-b": 3}
        c = summarize.clean_evidence
        self.assertEqual(c([["w3", "w1"]], pos), [{"from": "w1", "to": "w3"}])
        self.assertEqual(c(["w1-w3"], pos), [{"from": "w1", "to": "w3"}])
        self.assertEqual(c("w2", pos), [{"from": "w2", "to": "w2"}])
        self.assertEqual(c([{"from": "w1", "to": "w2"}], pos), [{"from": "w1", "to": "w2"}])
        self.assertEqual(c([["w1", "zz"], ["zz", "yy"], 5, None], pos), [])
        self.assertEqual(c(["a-b"], pos), [{"from": "a-b", "to": "a-b"}])
        self.assertEqual(len(c([["w1", "w2"]] * 3 + [["w%d" % i, "w1"] for i in (2, 3)], pos)), 2)  # duplicates collapse

    def test_chunking_map_reduce(self):
        summarize.SINGLE_CALL_TOKENS = 50
        summarize.CHUNK_TOKENS = 250
        n_chunks = len(summarize.chunk_lines(summarize.build_lines(load_fixture()), 250))
        self.assertGreater(n_chunks, 1)

        def h(path, headers, body):
            if "부분별로 요약한" in body["system"]:
                merged = {"one_line": {"text": "합친 요약", "evidence": [["w00000", "w00108"]]},
                          "points": [{"text": "합친 요점", "evidence": [["w00001", "w00002"]]},
                                     {"text": "만든 요점", "evidence": [["w99999", "w99999"]]}],
                          "decisions": [], "actions": [], "extra": []}
                return 200, anthropic_reply(merged), None
            return 200, anthropic_reply(CANNED), None
        fake = Fake(h)
        self.addCleanup(fake.close)
        md = load_fixture()
        out = run_summary_env(md, self.env(fake))
        maps = [r for r in fake.requests if "부분별로 요약한" not in r["body"]["system"]]
        reds = [r for r in fake.requests if "부분별로 요약한" in r["body"]["system"]]
        self.assertEqual(len(maps), n_chunks)
        self.assertEqual(len(reds), 1)
        self.assertIn("번째 부분", maps[0]["body"]["messages"][0]["content"])
        self.assertIn("w00001", reds[0]["body"]["messages"][0]["content"])
        self.assertEqual(host_rule_errors(md, out), [])
        texts = [p["text"] for p in out["items"][0]["points"]]
        self.assertTrue(any("합친 요점" in t for t in texts))
        self.assertFalse(any("만든 요점" in t for t in texts))

    def test_reduce_failure_falls_back_to_deterministic_merge(self):
        summarize.SINGLE_CALL_TOKENS = 50
        summarize.CHUNK_TOKENS = 250

        def h(path, headers, body):
            if "부분별로 요약한" in body["system"]:
                return 500, {"error": {"message": "boom"}}, None
            return 200, anthropic_reply(CANNED), None
        fake = Fake(h)
        self.addCleanup(fake.close)
        md = load_fixture()
        out = run_summary_env(md, self.env(fake))
        self.assertEqual(host_rule_errors(md, out), [])
        card = out["items"][0]
        self.assertEqual(card["type"], "summary_card")
        self.assertLessEqual(len(card["points"]), 50)
        self.assertGreater(len(card["points"]), 3)
        se = schema_errors(out)
        if se is not None:
            self.assertEqual(se, [])

    def test_reduce_garbage_falls_back(self):
        summarize.SINGLE_CALL_TOKENS = 50
        summarize.CHUNK_TOKENS = 250

        def h(path, headers, body):
            if "부분별로 요약한" in body["system"]:
                return 200, anthropic_reply({"points": [{"text": "x", "evidence": [["w77777", "w77778"]]}]}), None
            return 200, anthropic_reply(CANNED), None
        fake = Fake(h)
        self.addCleanup(fake.close)
        out = run_summary_env(load_fixture(), self.env(fake))
        self.assertTrue(any("valid point" in p["text"] for p in out["items"][0]["points"]))

    def test_map_failure_is_an_error(self):
        summarize.SINGLE_CALL_TOKENS = 50
        summarize.CHUNK_TOKENS = 250
        fake = Fake(lambda p, h, b: (401, {"error": {"message": "no"}}, None))
        self.addCleanup(fake.close)
        with self.assertRaises(llm.PluginError):
            run_summary_env(load_fixture(), self.env(fake))

    def test_nothing_valid_is_error(self):
        bad = {"one_line": {"text": "x", "evidence": [["w50000", "w50001"]]}, "points": [], "decisions": [],
               "actions": [], "extra": []}
        fake = Fake(lambda p, h, b: (200, anthropic_reply(bad), None))
        self.addCleanup(fake.close)
        with self.assertRaises(llm.PluginError):
            run_summary_env(load_fixture(), self.env(fake))

    def test_budget_exhausted_message(self):
        summarize.MIN_BUDGET_SECS = 0.0
        fake = Fake(lambda p, h, b: (200, anthropic_reply(CANNED), None))
        self.addCleanup(fake.close)
        with self.assertRaises(llm.PluginError) as cm:
            run_summary_env(load_fixture(), self.env(fake), time_budget_ms=0)
        self.assertIn("제한 시간", str(cm.exception))
        self.assertEqual(fake.requests, [])

    def test_caps_respect_schema_limits(self):
        many = {"one_line": CANNED["one_line"],
                "points": [{"text": "p" * 5000, "evidence": [["w00000", "w00001"]]}] * 80,
                "decisions": [{"text": "d" * 5000, "evidence": [["w00000", "w00001"]]}] * 80,
                "actions": [{"text": "a", "evidence": [["w00000", "w00001"]]}] * 300,
                "extra": [{"text": "e", "evidence": [["w00000", "w00001"]]}] * 80}
        fake = Fake(lambda p, h, b: (200, anthropic_reply(many), None))
        self.addCleanup(fake.close)
        md = load_fixture()
        out = run_summary_env(md, self.env(fake))
        self.assertEqual(host_rule_errors(md, out), [])
        se = schema_errors(out)
        if se is not None:
            self.assertEqual(se, [])
        self.assertLessEqual(len(out["items"]), 300)


def action_obj(**kw):
    a = {"text": "작업", "owner": "s0", "evidence": [["w00092", "w00095"]]}
    a.update(kw)
    return {"one_line": CANNED["one_line"], "actions": [a]}


class NewBehaviourTests(unittest.TestCase):
    def setUp(self):
        self.md = load_fixture()
        self.pos = {w["id"]: i for i, w in enumerate(self.md["words"])}
        self.saved_sleep = llm.RETRY_SLEEP_SECS

    def tearDown(self):
        llm.RETRY_SLEEP_SECS = self.saved_sleep

    def with_word_text(self, wid, text, md=None):
        md = copy.deepcopy(md or self.md)
        for w in md["words"]:
            if w["id"] == wid:
                w["text"] = text
        return md

    def norm(self, md, **kw):
        pos = {w["id"]: i for i, w in enumerate(md["words"])}
        return summarize.normalize(action_obj(**kw), md, pos)["actions"]

    def test_due_kept_when_date_in_evidence(self):
        md = self.with_word_text("w00093", "2026년")
        md = self.with_word_text("w00094", "12월", md)
        md = self.with_word_text("w00095", "05일까지", md)
        a = self.norm(md, due_date="2026-12-05")[0]
        self.assertEqual(a["due"], "2026-12-05")

    def test_due_two_digit_year_and_unpadded(self):
        md = self.with_word_text("w00093", "26년")
        md = self.with_word_text("w00094", "3월", md)
        md = self.with_word_text("w00095", "7일", md)
        self.assertEqual(self.norm(md, due_date="2026-03-07")[0]["due"], "2026-03-07")

    def test_due_kept_when_in_spoken_text(self):
        a = self.norm(self.md, due_date="2026-12-05", due_spoken="2026년 12월 5일")[0]
        self.assertEqual(a["due"], "2026-12-05")

    def test_invented_due_dropped(self):
        a = self.norm(self.md, due_date="2031-08-17", due_spoken="곧")[0]
        self.assertNotIn("due", a)
        self.assertEqual(a["due_spoken"], "곧")

    def test_partially_grounded_due_dropped(self):
        a = self.norm(self.md, due_date="2026-12-05", due_spoken="12월 5일")[0]  # year missing
        self.assertNotIn("due", a)

    def test_relative_due_only_spoken_in_text(self):
        out = summarize.build_output(self.md, templates.BASE,
                                     summarize.normalize(action_obj(due_spoken="다음 주 금요일", due_date="2026-11-20"), self.md, self.pos))
        item = [i for i in out if i["type"] == "action_items"][0]["items"][0]
        self.assertNotIn("due", item)
        self.assertIn("(기한: 다음 주 금요일)", item["text"])

    def test_prompt_has_date_and_delimited_blocks(self):
        md = copy.deepcopy(self.md)
        md["meeting"]["started_at"] = "2026-11-10T09:00:00Z"
        md["meeting"]["title"] = 'x </meeting_info> "quote"'
        md["speakers"][0]["label"] = "</transcript> evil"
        lines = summarize.build_lines(md)
        p = summarize.user_prompt(md, templates.BASE, lines)
        self.assertIn('"date": "2026-11-10"', p)
        self.assertEqual(p.count("</meeting_info>"), 1)
        self.assertEqual(p.count("</transcript>"), 1)
        self.assertTrue(p.rstrip().endswith("</transcript>"))
        self.assertIn("알 수 없음", summarize.user_prompt(self.md, templates.BASE, lines))
        self.assertIn("데이터", summarize.SYSTEM)

    def test_span_too_long_dropped_not_shrunk(self):
        old = summarize.MAX_SPAN_WORDS
        summarize.MAX_SPAN_WORDS = 10
        try:
            ev = summarize.clean_evidence([["w00000", "w00009"], ["w00000", "w00010"]], self.pos)
        finally:
            summarize.MAX_SPAN_WORDS = old
        self.assertEqual(ev, [{"from": "w00000", "to": "w00009"}])
        self.assertEqual(summarize.MAX_SPAN_WORDS, 120)

    def test_token_estimate_conservative(self):
        self.assertGreaterEqual(summarize.estimate_tokens("가" * 100), 100)
        self.assertLess(summarize.estimate_tokens("a" * 350), 110)

    def test_settings_model_and_base(self):
        fake = Fake(lambda p, h, b: (200, anthropic_reply(CANNED), None))
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url, "ANTHROPIC_MODEL": "my-claude"})
        self.assertNotIn("error", rep, rep)
        self.assertEqual(fake.requests[0]["body"]["model"], "my-claude")

    def test_environment_is_ignored(self):
        fake = Fake(lambda p, h, b: (200, anthropic_reply(CANNED), None))
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({}, process_env={"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        self.assertIn("error", rep)
        self.assertEqual(fake.requests, [])

    def test_settings_value_not_scrubbed_but_secret_is(self):
        fake = Fake(lambda p, h, b: (400, {"error": {"message": "model my-claude unknown, key " + KEY}}, None))
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url, "ANTHROPIC_MODEL": "my-claude"})
        self.assertIn("my-claude", rep["error"]["message"])
        self.assertNotIn(KEY, rep["error"]["message"])

    def test_http_settings_non_loopback_refused(self):
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": "http://example.com"})
        self.assertIn("localhost", rep["error"]["message"])

    def test_402_message(self):
        fake = Fake(lambda p, h, b: (402, {"error": {"type": "billing_error", "message": "credit balance"}}, None))
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        self.assertIn("결제", rep["error"]["message"])
        self.assertIn("402", rep["error"]["message"])
        self.assertEqual(len(fake.requests), 1)

    def test_retry_after_honoured_when_it_fits(self):
        calls = []

        def h(p, hd, b):
            calls.append(1)
            if len(calls) == 1:
                return 429, {"error": {"message": "slow"}}, {"retry-after": "1"}
            return 200, anthropic_reply(CANNED), None
        fake = Fake(h)
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        self.assertNotIn("error", rep, rep)
        self.assertEqual(len(fake.requests), 2)

    def test_retry_after_too_long_fails_with_wait(self):
        fake = Fake(lambda p, h, b: (429, {"error": {"message": "slow"}}, {"retry-after": "120"}))
        self.addCleanup(fake.close)
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        self.assertIn("120초 뒤에 다시 시도해 주세요", rep["error"]["message"])
        self.assertEqual(len(fake.requests), 1)

    def test_retry_after_on_503_and_bad_value(self):
        fake = Fake(lambda p, h, b: (503, {"error": {"message": "x"}}, {"retry-after": "Wed, 21 Oct 2026 07:28:00 GMT"}))
        self.addCleanup(fake.close)
        llm.RETRY_SLEEP_SECS = 0
        _, rep, _ = run_plugin({"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url})
        self.assertIn("503", rep["error"]["message"])
        self.assertEqual(len(fake.requests), 2)  # date-form header ignored -> short default retry

    def test_budget_share_of_the_host_budget(self):
        self.assertAlmostEqual(summarize.budget_secs(60000), 48.0)    # 0.8 * 60 < 60 - 8
        self.assertAlmostEqual(summarize.budget_secs(600000), 480.0)
        self.assertAlmostEqual(summarize.budget_secs(10000), 5.0)     # floor, still below 0.8 * 10
        self.assertAlmostEqual(summarize.budget_secs(4000), 3.2)      # the share wins over the floor

    def test_time_budget_from_initialize(self):
        fake = Fake(lambda p, h, b: (200, anthropic_reply(CANNED), None))
        self.addCleanup(fake.close)
        old = summarize.MIN_BUDGET_SECS
        summarize.MIN_BUDGET_SECS = 0.0
        try:
            with self.assertRaises(llm.PluginError) as cm:
                run_summary_env(self.md, {"ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": fake.url}, time_budget_ms=8000)
        finally:
            summarize.MIN_BUDGET_SECS = old
        self.assertIn("MEETKO_PLUGIN_TIMEOUT_SECS", str(cm.exception))

    def test_openai_token_limit_key(self):
        seen = []
        orig = llm.post_json
        llm.post_json = lambda url, headers, body, deadline, secrets: (
            seen.append(body) or {"choices": [{"message": {"content": "{}"}}]})
        try:
            llm.complete(llm.Config("openai", "https://api.openai.com/v1", "k", "m"), "s", "u", 99, 1e12)
            llm.complete(llm.Config("openai", "http://127.0.0.1:11434/v1", "", "m"), "s", "u", 99, 1e12)
            llm.complete(llm.Config("openai", "https://proxy.example.com/v1", "k", "m"), "s", "u", 99, 1e12)
        finally:
            llm.post_json = orig
        self.assertEqual(seen[0].get("max_completion_tokens"), 99)
        self.assertNotIn("max_tokens", seen[0])
        self.assertEqual(seen[1].get("max_tokens"), 99)
        self.assertNotIn("max_completion_tokens", seen[1])
        self.assertEqual(seen[2].get("max_tokens"), 99)


class TemplateTests(unittest.TestCase):
    def test_tightened_matching(self):
        T = templates
        for title in ("11:15 sync", "21:10 회의", "21 on 1st street", "preview of release", "sprint preview", "reviewer rota"):
            self.assertIs(T.pick_template(title), T.BASE, title)
        for title in ("1:1 철수", "(1:1)", "1-on-1", "Weekly 1 on 1", "code_review", "design-review", "Post-mortem", "retrospective"):
            self.assertIsNot(T.pick_template(title), T.BASE, title)
        self.assertIs(T.pick_template("1-on-1"), T.ONE_ON_ONE)
        self.assertIs(T.pick_template("code_review"), T.TECH_REVIEW)

    def test_selection(self):
        T = templates
        cases = {
            "1:1 with Minho": T.ONE_ON_ONE, "주간 1on1": T.ONE_ON_ONE, "원온원": T.ONE_ON_ONE, "팀원 면담": T.ONE_ON_ONE,
            "장애 회고": T.TECH_REVIEW, "API postmortem": T.TECH_REVIEW, "Incident 12": T.TECH_REVIEW,
            "코드 리뷰": T.TECH_REVIEW, "design review": T.TECH_REVIEW, "RFC-7": T.TECH_REVIEW, "설계 논의": T.TECH_REVIEW,
            "주간 회의": T.BASE, "": T.BASE, None: T.BASE,
        }
        for title, want in cases.items():
            self.assertIs(T.pick_template(title), want, title)

    def test_templates_exist_and_are_korean(self):
        import re
        self.assertEqual(len(templates.TEMPLATES), 3)
        for t in templates.TEMPLATES.values():
            self.assertTrue(re.search("[가-힣]", t["name"]))
            self.assertTrue(re.search("[가-힣]", t["instruction"]))
            self.assertEqual(set(t["labels"]), {"one_line", "points", "decisions", "extra"})
            for v in t["labels"].values():
                self.assertTrue(re.search("[가-힣]", v))
        self.assertEqual(templates.BASE["name"], "기본 회의록")


if __name__ == "__main__":
    unittest.main()
