"""Reference core: compose engine results into meeting data, check invariants,
validate plugin output. This is the executable form of docs/rfc-0001-contract.md.
"""
import json
from pathlib import Path

from jsonschema import Draft202012Validator, FormatChecker

SCHEMAS = Path(__file__).resolve().parent.parent / "contract" / "schemas"


def _validator(name):
    schema = json.loads((SCHEMAS / name).read_text())
    return Draft202012Validator(schema, format_checker=FormatChecker())


MEETING_DATA = _validator("meeting-data.schema.json")
PLUGIN_OUTPUT = _validator("plugin-output.schema.json")
ENGINE_RESULT = _validator("engine-result.schema.json")


TURN_GAP_MS = 2000  # a pause this long ends a turn even when the speaker continues


def schema_errors(validator, doc):
    return [f"{'/'.join(map(str, e.absolute_path)) or '<root>'}: {e.message}"
            for e in validator.iter_errors(doc)]


# ---------------------------------------------------------------- composing

def live_words(events):
    """Fold asr.stream events into word dicts: the latest revision of each
    final segment wins (replace semantics). Ends are estimated."""
    latest = {}
    for e in events:
        if e["final"]:
            prev = latest.get(e["segment"])
            if prev is None or e["revision"] >= prev["revision"]:
                latest[e["segment"]] = e
    words = []
    for seg in sorted(latest):
        e = latest[seg]
        seg_words = []
        for u in e["units"]:
            if not seg_words or u["text"].startswith(" "):
                seg_words.append({"start_ms": u["start_ms"], "text": u["text"].strip()})
            else:
                seg_words[-1]["text"] += u["text"]
        for i, w in enumerate(seg_words):
            end = seg_words[i + 1]["start_ms"] if i + 1 < len(seg_words) else e["emitted_at_ms"]
            words.append({"start_ms": w["start_ms"], "end_ms": max(end, w["start_ms"]),
                          "text": w["text"], "end_estimated": True})
    return [w for w in words if w["text"]]


def _overlap(a0, a1, b0, b1):
    return max(0, min(a1, b1) - max(a0, b0))


def assign_speaker(word, spans):
    """A word goes to a speaker only when exactly one speaker covers at least
    half of it. Zero-length words use their start point."""
    start, end = word["start_ms"], word["end_ms"]
    length = end - start
    covering = set()
    for s in spans:
        if length == 0:
            if s["start_ms"] <= start < s["end_ms"]:
                covering.add(s["speaker"])
        elif _overlap(start, end, s["start_ms"], s["end_ms"]) * 2 >= length:
            covering.add(s["speaker"])
    if len(covering) == 1:
        return covering.pop(), "assigned"
    return None, ("ambiguous" if covering else "unassigned")


def compose(meeting, recording, version, channels):
    """channels: {index: {"role", "words": [...], "coverage": [...],
    "producers": [...], "diarization": diarization-result | None}}.
    Engine times are channel-local; `offset_ms` maps them to the meeting."""
    speakers, spans, words, coverage, producers = [], [], [], [], []
    for ch in sorted(channels):
        c = channels[ch]
        off = c.get("offset_ms", 0)
        producers.extend(c["producers"])
        coverage.extend({"channel": ch, "start_ms": s["start_ms"] + off, "end_ms": s["end_ms"] + off}
                        for s in c["coverage"])
        diar = c.get("diarization")
        if diar is None or c.get("single_speaker"):
            # the channel decides the speaker; speech spans come from a one-label
            # diarization (VAD) when there is one, otherwise there are none:
            # coverage is not speech, and using it would count silence as talk
            sid = f"s{len(speakers)}"
            speakers.append({"id": sid, "channel": ch, "basis": "channel", "label": c.get("label", c["role"])})
            ch_spans = [{"speaker": sid, "channel": ch, "start_ms": t["start_ms"] + off,
                         "end_ms": t["end_ms"] + off} for t in (diar["turns"] if diar else [])]
            ch_speaker = sid
        else:
            local = {}
            for t in diar["turns"]:
                if t["speaker"] not in local:
                    local[t["speaker"]] = f"s{len(speakers)}"
                    speakers.append({"id": local[t["speaker"]], "channel": ch, "basis": "diarization",
                                     "label": f"{c.get('label', c['role'])} {len(local)}"})
            ch_spans = [{"speaker": local[t["speaker"]], "channel": ch,
                         "start_ms": t["start_ms"] + off, "end_ms": t["end_ms"] + off}
                        for t in diar["turns"]]
        spans.extend(ch_spans)
        for pos, w in enumerate(c["words"]):
            ww = {"channel": ch, "start_ms": w["start_ms"] + off, "end_ms": w["end_ms"] + off,
                  "text": w["text"], "_pos": pos}
            if "p" in w:
                ww["p"] = w["p"]
            if w.get("end_estimated"):
                ww["end_estimated"] = True
            if diar is None or c.get("single_speaker"):
                ww["speaker"], ww["speaker_status"] = ch_speaker, "assigned"
            else:
                ww["speaker"], ww["speaker_status"] = assign_speaker(ww, ch_spans)
            words.append(ww)
    words.sort(key=lambda w: (w["start_ms"], w["channel"], w["_pos"]))
    for i, w in enumerate(words):
        del w["_pos"]
        w["id"] = f"w{i:05d}"
    turns = []
    for w in words:
        t = turns[-1] if turns else None
        if t and t["_key"] == (w["channel"], w["speaker"]) and w["start_ms"] - t["end_ms"] < TURN_GAP_MS:
            t["last_word"], t["end_ms"] = w["id"], max(t["end_ms"], w["end_ms"])
        else:
            turns.append({"_key": (w["channel"], w["speaker"]), "id": f"t{len(turns):04d}",
                          "speaker": w["speaker"], "first_word": w["id"], "last_word": w["id"],
                          "start_ms": w["start_ms"], "end_ms": w["end_ms"]})
    for t in turns:
        del t["_key"]
    # engines round and pad: a span or word may run past the end of the audio.
    # The recording is the authority, so clip to it (found by property testing).
    dur = recording["duration_ms"]
    for x in (*spans, *words, *turns, *coverage):
        x["start_ms"], x["end_ms"] = min(x["start_ms"], dur), min(x["end_ms"], dur)
    spans = [s for s in spans if s["end_ms"] > s["start_ms"]]
    spans.sort(key=lambda s: (s["start_ms"], s["channel"]))
    return {"contract": "0.1", "meeting": meeting, "recording": recording,
            "version": {**version, "producers": producers}, "coverage": coverage,
            "speakers": speakers, "speaker_spans": spans, "words": words, "turns": turns}


# ---------------------------------------------------------------- invariants

def invariant_errors(md):
    """Rules JSON Schema cannot express. Each message names the rule (I1..I9)."""
    errs = []
    dur = md["recording"]["duration_ms"]
    channels = {c["index"] for c in md["meeting"]["channels"]}
    speakers = {s["id"]: s for s in md["speakers"]}
    for kind in ("words", "speakers", "turns"):
        ids = [x["id"] for x in md[kind]]
        if len(ids) != len(set(ids)):
            errs.append(f"I1 duplicate ids in {kind}")
    index = {w["id"]: i for i, w in enumerate(md["words"])}
    prev = None
    for w in md["words"]:
        key = (w["start_ms"], w["channel"])
        if prev is not None and key < prev:
            errs.append(f"I2 {w['id']} out of order")
        prev = key
        if not 0 <= w["start_ms"] <= w["end_ms"] <= dur:
            errs.append(f"I3 {w['id']} times {w['start_ms']}..{w['end_ms']} outside 0..{dur}")
        if w["channel"] not in channels:
            errs.append(f"I4 {w['id']} unknown channel {w['channel']}")
        sp = w["speaker"]
        if (sp is None) != (w["speaker_status"] != "assigned"):
            errs.append(f"I4 {w['id']} speaker/status mismatch")
        if sp is not None and (sp not in speakers or speakers[sp]["channel"] != w["channel"]):
            errs.append(f"I4 {w['id']} speaker {sp} missing or on another channel")
        if not any(c["channel"] == w["channel"] and c["start_ms"] <= w["start_ms"] and w["end_ms"] <= c["end_ms"]
                   for c in md["coverage"]):
            errs.append(f"I5 {w['id']} outside coverage")
    for s in md["speaker_spans"]:
        if s["speaker"] not in speakers or speakers[s["speaker"]]["channel"] != s["channel"]:
            errs.append(f"I6 span speaker {s['speaker']} missing or on another channel")
        if not 0 <= s["start_ms"] <= s["end_ms"] <= dur:
            errs.append(f"I6 span {s['start_ms']}..{s['end_ms']} outside 0..{dur}")
    covered = []
    for t in md["turns"]:
        a, b = index.get(t["first_word"]), index.get(t["last_word"])
        if a is None or b is None or a > b:
            errs.append(f"I7 {t['id']} bad word range")
            continue
        run = md["words"][a:b + 1]
        if any((w["channel"], w["speaker"]) != (run[0]["channel"], t["speaker"]) for w in run):
            errs.append(f"I7 {t['id']} mixes channels or speakers")
        if t["start_ms"] != min(w["start_ms"] for w in run) or t["end_ms"] != max(w["end_ms"] for w in run):
            errs.append(f"I7 {t['id']} time is not its words' span")
        end = run[0]["end_ms"]
        for w in run[1:]:
            if w["start_ms"] - end >= TURN_GAP_MS:
                errs.append(f"I7 {t['id']} spans a pause of {TURN_GAP_MS} ms or more")
                break
            end = max(end, w["end_ms"])
        covered.extend(range(a, b + 1))
    if sorted(covered) != list(range(len(md["words"]))):
        errs.append("I7 turns do not cover every word exactly once")
    if not any(p["role"] == "asr" for p in md["version"]["producers"]):
        errs.append("I8 no asr producer")
    for c in md["coverage"]:
        if c["channel"] not in channels or c["start_ms"] > c["end_ms"]:
            errs.append(f"I9 bad coverage {c}")
    return errs


# ---------------------------------------------------------------- plugin output

def output_errors(md, out):
    """Schema (strict) + references into the input version."""
    errs = schema_errors(PLUGIN_OUTPUT, out)
    if errs:
        return errs
    if out["input"]["version_id"] != md["version"]["id"]:
        errs.append("O1 output is for another version")
    index = {w["id"]: i for i, w in enumerate(md["words"])}
    speakers = {s["id"] for s in md["speakers"]}

    def anchor(a, where):
        if a["from"] not in index or a["to"] not in index:
            errs.append(f"O2 {where}: unknown word id")
        elif index[a["from"]] > index[a["to"]]:
            errs.append(f"O2 {where}: from after to")

    for n, it in enumerate(out["items"]):
        where = f"items/{n}"
        if it["type"] == "transcript_block":
            anchor(it["anchor"], where)
        elif it["type"] == "timeline_marker":
            anchor(it["at"], where)
        elif it["type"] == "summary_card":
            for p in it["points"]:
                for a in p["evidence"]:
                    anchor(a, where)
        elif it["type"] == "action_items":
            for x in it["items"]:
                for a in x["evidence"]:
                    anchor(a, where)
                if x.get("owner") is not None and x["owner"] not in speakers:
                    errs.append(f"O3 {where}: owner {x['owner']} is not a speaker of this version")
        elif it["type"] == "table":
            for r in it["rows"]:
                if len(r["cells"]) != len(it["columns"]):
                    errs.append(f"O4 {where}: row width differs from columns")
                for a in r["evidence"]:
                    anchor(a, where)
    return errs


def resolve(md, a):
    """Anchor -> (channel set, start_ms, end_ms). The GUI jumps here."""
    index = {w["id"]: i for i, w in enumerate(md["words"])}
    run = md["words"][index[a["from"]]:index[a["to"]] + 1]
    return sorted({w["channel"] for w in run}), min(w["start_ms"] for w in run), max(w["end_ms"] for w in run)
