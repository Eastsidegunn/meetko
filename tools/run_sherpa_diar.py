"""Offline speaker diarization with sherpa-onnx (pyannote segmentation +
3D-Speaker embedding). Writes RTTM, the NIST format the scorers read.

usage: python -I tools/run_sherpa_diar.py <models_dir> <wav> <out.rttm> [num_speakers]
"""
import sys
from pathlib import Path

import sherpa_onnx
import soundfile as sf


def main(models_dir, wav, out_path, num_speakers="-1"):
    m = Path(models_dir)
    config = sherpa_onnx.OfflineSpeakerDiarizationConfig(
        segmentation=sherpa_onnx.OfflineSpeakerSegmentationModelConfig(
            pyannote=sherpa_onnx.OfflineSpeakerSegmentationPyannoteModelConfig(
                model=str(m / "sherpa-onnx-pyannote-segmentation-3-0" / "model.onnx")),
        ),
        embedding=sherpa_onnx.SpeakerEmbeddingExtractorConfig(
            model=str(m / "3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx")),
        clustering=sherpa_onnx.FastClusteringConfig(num_clusters=int(num_speakers), threshold=0.5),
        min_duration_on=0.3,
        min_duration_off=0.5,
    )
    assert config.validate()
    sd = sherpa_onnx.OfflineSpeakerDiarization(config)
    audio, sr = sf.read(wav, dtype="float32")
    assert sr == sd.sample_rate and audio.ndim == 1
    result = sd.process(audio).sort_by_start_time()
    uri = Path(wav).stem
    lines = [f"SPEAKER {uri} 1 {r.start:.3f} {r.end - r.start:.3f} <NA> <NA> spk{r.speaker} <NA> <NA>"
             for r in result]
    Path(out_path).write_text("\n".join(lines) + "\n")
    print(f"{wav}: {len(lines)} turns, {len({r.speaker for r in result})} speakers")


if __name__ == "__main__":
    main(*sys.argv[1:5])
