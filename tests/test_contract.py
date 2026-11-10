import copy
import json
import sys
from pathlib import Path

import pytest
from hypothesis import given, settings, strategies as st

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from reference import core, host  # noqa: E402

FIX = ROOT / "contract" / "fixtures" / "meeting01"
VERSIONS = ["draft", "live"]
# sys.executable: "python3" does not exist on Windows runners, and this is the interpreter under test
EXE = ".exe" if sys.platform == "win32" else ""
PLUGINS = {
    "action-items-py": ([sys.executable, "-I", "-S", "plugin.py"], ROOT / "plugins" / "action-items-py"),
    "talk-time-rs": ([str(ROOT / "plugins" / "talk-time-rs" / "target" / "release" / f"talk-time-rs{EXE}")],
                     ROOT / "plugins" / "talk-time-rs"),
}


def load(name):
    return json.loads((FIX / name).read_text())


# ------------------------------------------------------------ golden fixtures

@pytest.mark.parametrize("version", VERSIONS)
def test_fixture_meeting_data_is_valid(version):
    md = load(f"meeting-data.{version}.json")
    assert core.schema_errors(core.MEETING_DATA, md) == []
    assert core.invariant_errors(md) == []


@pytest.mark.parametrize("name", sorted(p.name for p in FIX.glob("engine.*.json")))
def test_fixture_engine_results_are_valid(name):
    doc = load(name)
    for d in doc if isinstance(doc, list) else [doc]:
        assert core.schema_errors(core.ENGINE_RESULT, d) == []


def test_fixtures_carry_no_local_paths():
    for p in FIX.glob("*.json"):
        assert "/Users/" not in p.read_text(), p.name


# ------------------------------------------------------------ invariants hold for any engine output

# engine-result already forbids whitespace and control characters inside a word
word_text = st.text(st.characters(blacklist_categories=("Cc", "Cf", "Cs", "Zs", "Zl", "Zp")), min_size=1, max_size=8) \
    .filter(lambda t: not any(c.isspace() for c in t))
word = st.tuples(st.integers(0, 60_000), st.integers(0, 3_000), word_text)


@st.composite
def channel(draw, diarized):
    raw = draw(st.lists(word, max_size=40))
    words = [{"start_ms": s, "end_ms": s + d, "text": t.strip()} for s, d, t in raw]
    c = {"role": "loopback", "words": words, "coverage": [{"start_ms": 0, "end_ms": 63_000}],
         "producers": [{"role": "asr", "engine": {"id": "x", "version": "0"}, "network": False}]}
    if diarized:
        turns = draw(st.lists(st.tuples(st.integers(0, 60_000), st.integers(1, 5_000), st.sampled_from("ABC")),
                              max_size=10))
        c["diarization"] = {"turns": [{"start_ms": s, "end_ms": s + d, "speaker": k} for s, d, k in turns]}
    return c


@settings(max_examples=200, deadline=None)
@given(channel(diarized=False), channel(diarized=True))
def test_compose_always_satisfies_invariants(mic, loop):
    mic["role"] = "mic"
    md = core.compose(
        {"id": "m", "duration_ms": 63_000, "channels": [{"index": 0, "role": "mic"}, {"index": 1, "role": "loopback"}]},
        {"sha256": "0" * 64, "sample_rate": 16000, "duration_ms": 63_000},
        {"id": "v1", "kind": "draft"}, {0: mic, 1: loop})
    assert core.schema_errors(core.MEETING_DATA, md) == []
    assert core.invariant_errors(md) == []
    for w in md["words"]:
        a = {"from": w["id"], "to": w["id"]}
        assert core.resolve(md, a)[1:] == (w["start_ms"], w["end_ms"])


def test_invariants_catch_broken_data():
    md = load("meeting-data.draft.json")
    bad = copy.deepcopy(md)
    bad["words"][3]["start_ms"], bad["words"][4]["start_ms"] = bad["words"][4]["start_ms"] + 1, 0
    assert any(e.startswith("I2") for e in core.invariant_errors(bad))
    bad = copy.deepcopy(md)
    bad["words"][0]["speaker"] = "nobody"
    assert any(e.startswith("I4") for e in core.invariant_errors(bad))
    bad = copy.deepcopy(md)
    bad["turns"].pop()
    assert any("cover every word" in e for e in core.invariant_errors(bad))


# ------------------------------------------------------------ the core rejects bad plugin output

def good_output(md):
    w = md["words"]
    return {"contract": "0.1", "plugin": {"id": "t", "version": "0"}, "input": {"version_id": md["version"]["id"]},
            "items": [{"type": "action_items", "items": [{"text": "x", "owner": md["speakers"][0]["id"],
                                                           "evidence": [{"from": w[0]["id"], "to": w[2]["id"]}]}]}]}


@pytest.mark.parametrize("mutate, rule", [
    (lambda o, md: o.update(extra=1), "Additional properties"),
    (lambda o, md: o["items"][0]["items"][0].update(color="red"), "Additional properties"),
    (lambda o, md: o["input"].update(version_id="other"), "O1"),
    (lambda o, md: o["items"][0]["items"][0]["evidence"][0].update(**{"from": "w99999"}), "O2"),
    (lambda o, md: o["items"][0]["items"][0]["evidence"][0].update(**{"from": md["words"][5]["id"]}), "from after to"),
    (lambda o, md: o["items"][0]["items"][0].update(owner="s99"), "O3"),
    (lambda o, md: o["items"][0]["items"][0].update(evidence=[]), "should be non-empty"),
    (lambda o, md: o["items"].append({"type": "html", "html": "<script>"}), "is not one of"),
])
def test_core_rejects_bad_output(mutate, rule):
    md = load("meeting-data.draft.json")
    out = good_output(md)
    assert core.output_errors(md, out) == []
    mutate(out, md)
    errs = core.output_errors(md, out)
    assert any(rule in e for e in errs), errs


# ------------------------------------------------------------ conformance: every plugin x every version

def with_unknown_fields(md):
    md = copy.deepcopy(md)
    md["future_branch"] = {"anything": [1, 2]}
    md["meeting"]["future"] = True
    for w in md["words"]:
        w["lang"] = "ko"
    return md


@pytest.mark.parametrize("plugin", sorted(PLUGINS))
@pytest.mark.parametrize("version", VERSIONS)
def test_plugin_conformance(plugin, version):
    cmd, cwd = PLUGINS[plugin]
    md = load(f"meeting-data.{version}.json")
    p = host.Plugin(cmd, cwd=cwd)
    try:
        out = p.run(md)  # raises on any schema or reference error
        assert out["items"], "plugin returned nothing on a real meeting"
        assert p.run(with_unknown_fields(md)) == out, "unknown fields changed the result"
        assert p.run(md) == out, "plugin is not a function of its input"
    finally:
        p.close()


@pytest.mark.parametrize("plugin", sorted(PLUGINS))
def test_plugin_refuses_unknown_contract(plugin):
    cmd, cwd = PLUGINS[plugin]
    import subprocess
    r = subprocess.run(cmd, cwd=cwd, input='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"contract":["9.0"]}}\n',
                       capture_output=True, text=True, timeout=10)
    assert json.loads(r.stdout.splitlines()[0])["error"]["code"] == -32001
