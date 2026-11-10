"""Engine adapters: raw engine output -> engine-result 0.1.

An adapter is the only place that knows an engine's quirks. Everything it
learned about the raw output is written down next to the code that handles it.
"""
import json
from pathlib import Path


def whisper_cpp(raw_path, audio_sha256, params, engine_version, model_id, model_sha256=None):
    """whisper.cpp `-ojf` (output-json-full) -> asr.batch.

    Raw facts (whisper.cpp 1.9.5, measured on fixtures/meeting01):
    - tokens are subwords; a token starting with a space starts a new word.
      " 올라" + "오면" -> "올라오면".
    - special tokens look like "[_BEG_]" / "[_TT_141]" and carry no text.
    - `offsets` on tokens can be seconds off (a whole segment collapsed onto its
      first token's time, or a segment starting 3 s early). `t_dtw` (10 ms
      units, needs `-dtw <preset>` AND `-nfa`) matched the reference within
      ~0.2 s. With flash attention on, t_dtw is silently -1.
    - t_dtw is a single point per token: there is no word end. The end is
      the next word's start (or the segment end) and is marked end_estimated.
    - the JSON's own `params` holds only model path and language, not the
      decode parameters; the caller must pass what it actually ran.
    """
    raw = json.loads(Path(raw_path).read_text())
    words, timing = [], "dtw"
    for seg in raw["transcription"]:
        seg_end = seg["offsets"]["to"]
        current = None
        for tok in seg["tokens"]:
            text = tok["text"]
            if text.startswith("[_"):
                continue
            if tok["t_dtw"] < 0:
                timing = "token"
            t = tok["t_dtw"] * 10 if tok["t_dtw"] >= 0 else tok["offsets"]["from"]
            if current is None or text.startswith(" "):
                if current:
                    words.append(current)
                current = {"start_ms": t, "text": text.strip(), "p": [tok["p"]], "seg_end": seg_end}
            else:
                current["text"] += text
                current["p"].append(tok["p"])
        if current:
            words.append(current)
    words = [w for w in words if w["text"]]
    out, prev_start = [], 0
    for i, w in enumerate(words):
        start = max(w["start_ms"], prev_start)  # DTW points are not guaranteed monotonic
        nxt = words[i + 1]["start_ms"] if i + 1 < len(words) else w["seg_end"]
        end = max(start, min(nxt, w["seg_end"]) if nxt > start else w["seg_end"])
        out.append({"start_ms": int(start), "end_ms": int(end), "text": w["text"],
                    "p": round(min(w["p"]), 4), "end_estimated": True})
        prev_start = start
    duration_ms = max([s["offsets"]["to"] for s in raw["transcription"]] + [0])
    return {
        "contract": "0.1", "kind": "asr.batch",
        "engine": {"id": "whisper.cpp", "version": engine_version},
        "model": {"id": model_id, **({"sha256": model_sha256} if model_sha256 else {})},
        "audio_sha256": audio_sha256, "params": params,
        "language": raw.get("result", {}).get("language", ""),
        "word_timing": timing, "network": False,
        "coverage": [{"start_ms": 0, "end_ms": int(duration_ms)}],
        "words": out,
    }


def sherpa_stream(raw_jsonl):
    """run_sherpa_stream.py JSONL -> list of asr.stream events.

    Raw facts (sherpa-onnx 2.0.1, korean zipformer 2024-06-16):
    - token timestamps are relative to the current segment; `start_time` gives
      the segment's absolute start (it differs from our reset time by ~0.1 s,
      so the engine's own value is used).
    - tokens have a start only. A token starting with a space opens a word, but
      the model often drops spaces ("그니까그건액션설정문제아니었어요?").
    - on this fixture partials only ever appended; other engines rewrite, so the
      contract uses replace-by-revision, not append.
    - English-dominant utterances on a Korean-only model produced no text at
      all: absence of text is not absence of speech.
    """
    events, revisions = [], {}
    for line in Path(raw_jsonl).read_text().splitlines():
        e = json.loads(line)
        seg = e["segment"]
        revisions[seg] = revisions.get(seg, -1) + 1
        base = e["start_time"] if e["start_time"] >= 0 else e["segment_start"]
        units, pending_space = [], False
        for tok, ts in zip(e["tokens"], e["timestamps"]):
            if not tok.strip():  # a bare " " token: the next token opens a word
                pending_space = True
                continue
            text = (" " + tok.lstrip()) if pending_space else tok
            pending_space = False
            units.append({"text": text, "start_ms": int(round((base + ts) * 1000))})
        events.append({
            "contract": "0.1", "kind": "asr.stream", "segment": seg,
            "revision": revisions[seg], "final": e["kind"] == "final",
            "start_ms": int(round(base * 1000)), "text": e["text"].strip(),
            "emitted_at_ms": int(round(e["audio_time"] * 1000)),
            "units": units,
        })
    return events


def rttm(raw_path, audio_sha256, engine_version, model_id, params, engine_id="sherpa-onnx.diarization"):
    """NIST RTTM SPEAKER lines -> diarization. VAD output is a one-label RTTM."""
    turns = []
    for line in Path(raw_path).read_text().splitlines():
        f = line.split()
        if not f or f[0] != "SPEAKER":
            continue
        start, dur = float(f[3]), float(f[4])
        turns.append({"start_ms": int(round(start * 1000)), "end_ms": int(round((start + dur) * 1000)),
                      "speaker": f[7]})
    return {"contract": "0.1", "kind": "diarization",
            "engine": {"id": engine_id, "version": engine_version},
            "model": {"id": model_id}, "audio_sha256": audio_sha256, "params": params,
            "network": False, "turns": turns}
