"""Silero VAD (sherpa-onnx) -> RTTM with one speaker label.

On a channel whose speaker is known (the mic), VAD is a one-speaker
diarization, so it rides the diarization branch instead of adding a new one.

usage: python -I tools/run_vad.py <silero_vad.onnx> <wav> <out.rttm>
"""
import sys
from pathlib import Path

import sherpa_onnx
import soundfile as sf


def main(model, wav, out_path):
    cfg = sherpa_onnx.VadModelConfig()
    cfg.silero_vad.model = model
    cfg.silero_vad.threshold = 0.5
    cfg.silero_vad.min_silence_duration = 0.5
    cfg.silero_vad.min_speech_duration = 0.25
    cfg.sample_rate = 16000
    vad = sherpa_onnx.VoiceActivityDetector(cfg, buffer_size_in_seconds=120)
    audio, sr = sf.read(wav, dtype="float32")
    assert sr == 16000 and audio.ndim == 1
    window = cfg.silero_vad.window_size
    spans = []
    for i in range(0, len(audio), window):
        vad.accept_waveform(audio[i:i + window])
        while not vad.empty():
            seg = vad.front
            spans.append((seg.start / sr, len(seg.samples) / sr))
            vad.pop()
    vad.flush()
    while not vad.empty():
        seg = vad.front
        spans.append((seg.start / sr, len(seg.samples) / sr))
        vad.pop()
    uri = Path(wav).stem
    Path(out_path).write_text("".join(f"SPEAKER {uri} 1 {s:.3f} {d:.3f} <NA> <NA> speech <NA> <NA>\n"
                                      for s, d in spans))
    print(f"{wav}: {len(spans)} speech spans")


if __name__ == "__main__":
    main(*sys.argv[1:4])
