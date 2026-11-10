"""raw engine outputs (raw/, local) -> golden fixtures (contract/fixtures/, committed).

Fixtures hold text and times only, never audio or local paths.

usage: python -I tools/build_fixtures.py
"""
import hashlib
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from reference import adapters, core  # noqa: E402

RAW = ROOT / "raw"
OUT = ROOT / "contract" / "fixtures" / "meeting01"
WHISPER_PARAMS = {"language": "ko", "max_context": 0, "dtw": "large.v3.turbo", "flash_attn": False,
                  "temperature": 0.0, "temperature_inc": 0.2, "entropy_thold": 2.4, "logprob_thold": -1.0, "no_speech_thold": 0.6}
WHISPER_MODEL_SHA = sys.argv[1] if len(sys.argv) > 1 else None


def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def dump(name, doc):
    (OUT / name).write_text(json.dumps(doc, ensure_ascii=False, indent=1) + "\n")


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    ref = json.loads((RAW / "meeting01" / "reference.json").read_text())
    dur = int(round(ref["duration"] * 1000))
    audio = {ch: sha(RAW / "meeting01" / f"{ch}.wav") for ch in ("mic", "loopback")}
    meeting = {"id": "meeting01", "title": "fixture: incident retro", "duration_ms": dur,
               "language_hint": "ko",
               "channels": [{"index": 0, "role": "mic", "label": "나"},
                            {"index": 1, "role": "loopback", "label": "상대"}]}
    recording = {"sha256": sha(RAW / "meeting01" / "meeting.wav"), "sample_rate": 16000, "duration_ms": dur}

    whisper = {ch: adapters.whisper_cpp(RAW / "engines" / "whisper-cpp" / f"{ch}.json", audio[ch],
                                        WHISPER_PARAMS, "1.9.5", "ggml-large-v3-turbo", WHISPER_MODEL_SHA)
               for ch in ("mic", "loopback")}
    diar = adapters.rttm(RAW / "engines" / "sherpa-diar" / "loopback.rttm", audio["loopback"], "2.0.1",
                         "pyannote-segmentation-3-0+3dspeaker-eres2net-base",
                         {"num_clusters": -1, "threshold": 0.5, "min_duration_on": 0.3, "min_duration_off": 0.5})
    vad = {ch: adapters.rttm(RAW / "engines" / "sherpa-vad" / f"{ch}.rttm", audio[ch], "2.0.1", "silero_vad",
                             {"threshold": 0.5, "min_silence_duration": 0.5, "min_speech_duration": 0.25},
                             engine_id="sherpa-onnx.vad")
           for ch in ("mic", "loopback")}
    stream = {ch: adapters.sherpa_stream(RAW / "engines" / "sherpa-stream" / f"{ch}.jsonl")
              for ch in ("mic", "loopback")}
    for ch in ("mic", "loopback"):
        dump(f"engine.whisper-cpp.{ch}.json", whisper[ch])
        dump(f"engine.sherpa-stream.{ch}.json", stream[ch])
        dump(f"engine.sherpa-vad.{ch}.json", vad[ch])
    dump("engine.sherpa-diar.loopback.json", diar)
    for doc in [*whisper.values(), diar, *vad.values(), *(e for ev in stream.values() for e in ev)]:
        errs = core.schema_errors(core.ENGINE_RESULT, doc)
        assert not errs, errs[:5]

    def producer(result, role, ch):
        p = {"role": role, "engine": result["engine"], "channels": [ch], "network": False}
        for k in ("model", "params", "word_timing"):
            if k in result:
                p[k] = result[k]
        return p

    def batch_channel(ch, idx, label, d, single):
        r = whisper[ch]
        prods = [dict(producer(r, "asr", idx), granularity="word"), producer(d, "diarization", idx)]
        return {"role": ch, "label": label, "words": r["words"], "coverage": r["coverage"],
                "producers": prods, "diarization": d, "single_speaker": single}

    draft = core.compose(meeting, recording, {"id": "meeting01.draft.1", "kind": "draft"},
                         {0: batch_channel("mic", 0, "나", vad["mic"], True),
                          1: batch_channel("loopback", 1, "상대", diar, False)})

    sherpa_engine = {"id": "sherpa-onnx.online", "version": "2.0.1"}
    live_channels = {}
    for idx, ch, label in ((0, "mic", "나"), (1, "loopback", "상대")):
        live_channels[idx] = {
            "role": ch, "label": label, "words": core.live_words(stream[ch]),
            "coverage": [{"start_ms": 0, "end_ms": dur}],
            "producers": [{"role": "asr", "engine": sherpa_engine,
                           "model": {"id": "sherpa-onnx-streaming-zipformer-korean-2024-06-16"},
                           "channels": [idx], "word_timing": "transducer", "granularity": "token",
                           "network": False, "params": {"chunk_ms": 100, "endpoint_rule2_s": 0.8}},
                          producer(vad[ch], "diarization", idx)],
            "diarization": vad[ch], "single_speaker": True,
        }
    live = core.compose(meeting, recording, {"id": "meeting01.live.1", "kind": "live"}, live_channels)

    for name, md in (("meeting-data.draft.json", draft), ("meeting-data.live.json", live)):
        errs = core.schema_errors(core.MEETING_DATA, md) + core.invariant_errors(md)
        assert not errs, (name, errs[:10])
        dump(name, md)
        print(name, len(md["words"]), "words", len(md["turns"]), "turns", len(md["speakers"]), "speakers")

    # ground truth in contract-adjacent form, for scoring
    truth = {"utterances": [{k: u[k] for k in ("id", "channel", "speaker", "start", "speech_start", "end", "text", "source")}
                            for u in ref["utterances"]], "licenses": ref["licenses"]}
    dump("reference.json", truth)


if __name__ == "__main__":
    main()
