"""Build a dual-channel synthetic meeting for contract work.

ch0 (mic, "me")       : real Korean-English code-switching speech from HiKE (Apache-2.0)
ch1 (loopback, remote): macOS TTS, two voices, scripted tech-meeting lines

The audio stays local (raw/ is not committed); reference.json records the
ground truth: who spoke, on which channel, when, and what.

usage: python -I tools/make_fixture.py <hike.parquet> <out_dir>
"""
import io
import json
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np
import pyarrow.parquet as pq
import soundfile as sf

SR = 16000
GAP = 0.6
NOISE_DBFS = -55.0
SEED = 7

REMOTE = {
    "remote_a": "Yuna",
    "remote_b": "Reed (Korean (South Korea))",
}

# (speaker, text or None for the next HiKE utterance, extra)
SCRIPT = [
    ("remote_a", "지난주 incident 회고부터 할게요. API gateway에서 timeout이 연속으로 났습니다."),
    ("me", None),
    ("remote_b", "그거 connection pool 설정 문제 아니었어요?"),
    ("me", None),
    ("remote_b", "pool size를 늘리고 retry 로직에 backoff를 넣었다고 들었어요.", {"overlap": 0.8}),
    ("remote_a", "맞아요. 그래서 이번 sprint에 load test를 같이 돌려보려고요."),
    ("me", None),
    ("silence", 4.0),
    ("remote_a", "결정사항으로 남기고, action item은 제가 담당할게요."),
    ("me", None),
    ("remote_b", "PR 올라오면 제가 review 하겠습니다."),
    ("me", None),
]


def hike_utterances(parquet):
    t = pq.read_table(parquet).to_pydict()
    rows = [i for i, c in enumerate(t["category"]) if c == "software development"]
    rows.sort(key=lambda i: t["sample_id"][i])
    for i in rows:
        data, sr = sf.read(io.BytesIO(t["audio"][i]["bytes"]), dtype="float32")
        assert sr == SR and data.ndim == 1
        yield data, {"source": f"hike:{t['sample_id'][i]}", "text": t["text"][i],
                     "cs_level": t["cs_level"][i]}


def tts(voice, text):
    with tempfile.TemporaryDirectory() as d:
        wav = Path(d) / "t.wav"
        subprocess.run(["say", "-v", voice, "-o", str(wav), f"--data-format=LEI16@{SR}", text],
                       check=True)
        data, sr = sf.read(wav, dtype="float32")
    assert sr == SR
    return data


def main(parquet, out_dir):
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    hike = hike_utterances(parquet)
    placed = []  # (channel, start_sample, audio, meta)
    cursor = 0.5
    for entry in SCRIPT:
        if entry[0] == "silence":
            cursor += entry[1]
            continue
        speaker, text = entry[0], entry[1]
        extra = entry[2] if len(entry) > 2 else {}
        if speaker == "me":
            audio, meta = next(hike)
            channel = 0
        else:
            audio = tts(REMOTE[speaker], text)
            meta = {"source": f"tts:macos:{REMOTE[speaker]}", "text": text}
            channel = 1
        # overlap is measured against the previous utterance's end, not the gap
        start = cursor - GAP - extra["overlap"] if "overlap" in extra else cursor
        placed.append((channel, int(round(start * SR)), audio,
                       {"speaker": speaker, "channel": channel, **meta}))
        cursor = start + len(audio) / SR + GAP

    total = max(s + len(a) for _, s, a, _ in placed) + SR
    rng = np.random.default_rng(SEED)
    mix = rng.normal(0, 10 ** (NOISE_DBFS / 20), (total, 2)).astype("float32")
    utterances = []
    for n, (ch, s, a, meta) in enumerate(placed):
        mix[s:s + len(a), ch] += a
        # HiKE clips carry ~1 s of leading silence; timing is scored against speech onset
        energy = np.sqrt(np.convolve(a ** 2, np.ones(320) / 320, "same"))
        onset = s + int(np.argmax(energy > 0.02))
        utterances.append({"id": f"u{n:02d}", **meta,
                           "start_sample": s, "end_sample": s + len(a),
                           "start": round(s / SR, 3), "end": round((s + len(a)) / SR, 3),
                           "speech_start": round(onset / SR, 3)})
    peak = np.abs(mix).max()
    if peak > 0.99:
        mix *= 0.99 / peak
    sf.write(out / "meeting.wav", mix, SR, subtype="PCM_16")
    for ch, name in ((0, "mic"), (1, "loopback")):
        sf.write(out / f"{name}.wav", mix[:, ch], SR, subtype="PCM_16")
    ref = {"sample_rate": SR, "channels": {"0": "mic", "1": "loopback"},
           "duration": round(total / SR, 3), "utterances": utterances,
           "licenses": {"hike": "Apache-2.0 (thetaone-ai/HiKE)",
                        "tts": "macOS system voice output; redistribution unverified, keep local"}}
    (out / "reference.json").write_text(json.dumps(ref, ensure_ascii=False, indent=2))
    print(f"{out}: {ref['duration']}s, {len(utterances)} utterances")


if __name__ == "__main__":
    main(*sys.argv[1:3])
