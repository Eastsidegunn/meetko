"""External check that the contract loses nothing: score raw engine output and
the contract's standard-format exports with the same third-party scorers
(jiwer CER, pyannote.metrics DER). The numbers must match.

usage: python -I tools/score.py
"""
import json
import re
import sys
import unicodedata
from pathlib import Path

import jiwer
from pyannote.core import Annotation, Segment
from pyannote.metrics.diarization import DiarizationErrorRate

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
from reference import export  # noqa: E402

FIX = ROOT / "contract" / "fixtures" / "meeting01"
RAW = ROOT / "raw" / "engines"
CH = {0: "mic", 1: "loopback"}


def norm(s):
    s = unicodedata.normalize("NFC", s).lower()
    return re.sub(r"[\s\W_]+", "", s)


def annotation(lines):
    a = Annotation()
    for line in lines:
        f = line.split()
        if f and f[0] == "SPEAKER":
            a[Segment(float(f[3]), float(f[3]) + float(f[4]))] = f[7]
    return a


def main():
    ref = json.loads((FIX / "reference.json").read_text())
    rows = []
    for name in ("draft", "live"):
        md = json.loads((FIX / f"meeting-data.{name}.json").read_text())
        for ch, chname in CH.items():
            truth = norm("".join(u["text"] for u in ref["utterances"] if u["channel"] == ch))
            contract_text = norm(export.text(md, ch))
            if name == "draft":
                raw = json.loads((RAW / "whisper-cpp" / f"{chname}.json").read_text())
                raw_text = norm("".join(s["text"] for s in raw["transcription"]))
            else:
                finals = [json.loads(l) for l in (RAW / "sherpa-stream" / f"{chname}.jsonl").read_text().splitlines()]
                raw_text = norm("".join(e["text"] for e in finals if e["kind"] == "final"))
            rows.append((name, chname, jiwer.cer(truth, raw_text), jiwer.cer(truth, contract_text)))
    print("CER (reference vs raw engine | vs contract export)")
    for name, ch, a, b in rows:
        print(f"  {name:5s} {ch:8s} raw {a:.4f}  contract {b:.4f}  {'SAME' if abs(a - b) < 1e-9 else 'DIFF'}")

    md = json.loads((FIX / "meeting-data.draft.json").read_text())
    truth = Annotation()
    for u in ref["utterances"]:
        if u["channel"] == 1:
            truth[Segment(u["start"], u["end"])] = u["speaker"]
    raw = annotation((RAW / "sherpa-diar" / "loopback.rttm").read_text().splitlines())
    via = annotation(export.rttm(md, "loopback", 1).splitlines())
    der = DiarizationErrorRate(collar=0.0)
    a, b = der(truth, raw), der(truth, via)
    print(f"DER loopback (collar 0): raw {a:.4f}  contract {b:.4f}  {'SAME' if abs(a - b) < 1e-9 else 'DIFF'}")

    print("first-word start error vs reference speech onset (s)")
    for ch, chname in CH.items():
        raw = json.loads((RAW / "whisper-cpp" / f"{chname}.json").read_text())
        seg_starts = [s["offsets"]["from"] / 1000 for s in raw["transcription"]]
        words = [w["start_ms"] / 1000 for w in md["words"] if w["channel"] == ch]
        for u in ref["utterances"]:
            if u["channel"] != ch:
                continue
            on = u["speech_start"]
            w = min(words, key=lambda t: abs(t - on))
            s = min(seg_starts, key=lambda t: abs(t - on))
            print(f"  {u['id']} {chname:8s} dtw-word {w - on:+.2f}   token/segment {s - on:+.2f}")

    lat = []
    for chname in CH.values():
        seen = set()
        for line in (RAW / "sherpa-stream" / f"{chname}.jsonl").read_text().splitlines():
            e = json.loads(line)
            for k, ts in enumerate(e["timestamps"]):
                if (e["segment"], k) not in seen:
                    seen.add((e["segment"], k))
                    lat.append(e["audio_time"] - (e["start_time"] + ts))
    lat.sort()
    print(f"live token latency (audio time fed - token time): p50 {lat[len(lat) // 2]:.2f}s "
          f"p95 {lat[int(len(lat) * 0.95)]:.2f}s n={len(lat)}  (excludes compute; ~6 ms per 100 ms chunk)")


if __name__ == "__main__":
    main()
