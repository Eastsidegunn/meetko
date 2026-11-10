"""Feed a mono wav to a sherpa-onnx streaming transducer in 100 ms chunks,
as a live capture would, and dump every raw result change as JSONL.

Nothing is normalised here: this is raw material for the contract.

usage: python -I tools/run_sherpa_stream.py <model_dir> <wav> <out.jsonl>
"""
import json
import sys
import time
from pathlib import Path

import sherpa_onnx
import soundfile as sf

CHUNK_S = 0.1


def main(model_dir, wav, out_path):
    m = Path(model_dir)
    rec = sherpa_onnx.OnlineRecognizer.from_transducer(
        tokens=str(m / "tokens.txt"),
        encoder=str(m / "encoder-epoch-99-avg-1.int8.onnx"),
        decoder=str(m / "decoder-epoch-99-avg-1.onnx"),
        joiner=str(m / "joiner-epoch-99-avg-1.int8.onnx"),
        sample_rate=16000,
        enable_endpoint_detection=True,
        rule1_min_trailing_silence=2.4,
        rule2_min_trailing_silence=0.8,
        rule3_min_utterance_length=20,
    )
    audio, sr = sf.read(wav, dtype="float32")
    assert sr == 16000 and audio.ndim == 1
    stream = rec.create_stream()
    step = int(CHUNK_S * sr)
    segment = 0
    segment_start = 0.0  # audio time at the last reset
    last_text = ""
    events = []
    for i in range(0, len(audio), step):
        chunk = audio[i:i + step]
        stream.accept_waveform(sr, chunk)
        t0 = time.perf_counter()
        while rec.is_ready(stream):
            rec.decode_stream(stream)
        compute_ms = (time.perf_counter() - t0) * 1000
        audio_t = (i + len(chunk)) / sr
        r = rec.get_result_all(stream)
        endpoint = rec.is_endpoint(stream)
        if r.text != last_text or (endpoint and r.text):
            events.append({
                "kind": "final" if endpoint else "partial",
                "segment": segment,
                "audio_time": round(audio_t, 3),
                "segment_start": round(segment_start, 3),
                "compute_ms": round(compute_ms, 2),
                "text": r.text,
                "tokens": list(r.tokens),
                "timestamps": [round(t, 3) for t in r.timestamps],
                "start_time": round(getattr(r, "start_time", -1.0), 3),
            })
            last_text = r.text
        if endpoint:
            if r.text:
                segment += 1
            rec.reset(stream)
            segment_start = audio_t
            last_text = ""
    stream.input_finished()
    while rec.is_ready(stream):
        rec.decode_stream(stream)
    r = rec.get_result_all(stream)
    if r.text:
        events.append({"kind": "final", "segment": segment, "audio_time": round(len(audio) / sr, 3),
                       "segment_start": round(segment_start, 3), "compute_ms": 0, "text": r.text,
                       "tokens": list(r.tokens), "timestamps": [round(t, 3) for t in r.timestamps],
                       "start_time": round(getattr(r, "start_time", -1.0), 3), "flushed": True})
    Path(out_path).write_text("\n".join(json.dumps(e, ensure_ascii=False) for e in events) + "\n")
    finals = [e for e in events if e["kind"] == "final"]
    print(f"{wav}: {len(events)} events, {len(finals)} finals")


if __name__ == "__main__":
    main(*sys.argv[1:4])
