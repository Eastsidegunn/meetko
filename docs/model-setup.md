# Model setup in the app

<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

A new user gets live captions (and with them the first draft of every recording) without environment variables or
config files: the meeting list shows a setup card when live captions cannot run, and Settings → 받아쓰기 준비 lists each
piece as 준비됨 / 필요 / 선택.

A model is offered only where it can run: a release build without the engine does not offer the download at all
(the card says "이 버전에서는 아직 실시간 자막을 쓸 수 없어요"; the binary name and variable sit in a folded
"직접 설정하기 (개발자용)"), and the same holds for the diarization model and its engine. A user with whisper set up sees
the card as an offer ("녹음하는 동안 자막도 볼 수 있어요"), not a requirement. 나중에 is remembered in `app-settings.json`
under `setup_card` (next to `meeting_prompt`); the settings keep offering everything. A finished download, 다시 확인 and
the end of a recording repaint the setup views in place (no re-route), so focus, scroll and the search field stay.

| Piece | How the app sets it up |
|---|---|
| 실시간 자막 모델 (`nemotron-3.5-asr-streaming-0.6b-q8_0`, 742 MB, NVIDIA, OpenMDW-1.1) | 내려받기, or 이미 받은 파일 고르기 |
| 화자 구분 모델 (`nemotron-3-diarization-q8_0`, 107 MB, NVIDIA, OpenMDW-1.1), optional | 내려받기, or 이미 받은 파일 고르기 |
| The engines (`meetko-nemotron-stream`, `meetko-nemotron-diar`) | not downloaded; see below |
| 다시 받아쓰기용 Whisper (whisper-cli + a ggml model), optional | not downloaded; the settings show plain steps |
| 다시 받아쓰기 엔진 choice: 자동 (default), Whisper or Qwen3-ASR (실험 기능); 녹음이 끝나면 자동으로 다시 받아쓰기 (on by default) | settings, kept in `app-settings.json` under `batch_engine` and `auto_retranscribe` |
| Qwen3-ASR 받아쓰기 모델 (`qwen3-asr-1.7b-q8_0.gguf`, 2,507 MB) and Qwen3 단어 시각 모델 (`qwen3-forced-aligner-0.6b-q8_0.gguf`, 986 MB), optional, experimental | 내려받기, or 이미 받은 파일 고르기, each; both go into one folder (`models/qwen3-asr-1.7b-crispasr-q8_0/`, one pin) |
| The Qwen3-ASR engine (`meetko-qwen3-asr`) | not downloaded, like the other engines |

## The 다시 받아쓰기 engine (experimental Qwen3-ASR)

Settings › 받아쓰기 준비 has a 다시 받아쓰기 엔진 choice, 자동 (준비된 것 중 가장 정확한 것), Whisper or Qwen3-ASR, each
engine shown as 준비됨 / 필요 with its size, source and license; Qwen3-ASR is marked "실험 기능 · 결과를 확인해 주세요". The
choice is kept in `app-settings.json` (`batch_engine`: `auto` | `whisper` | `qwen3-asr`; anything else, or nothing, is
`auto`). `auto` runs Qwen3-ASR when its engine and both pinned models are ready (it measured best on Korean–English
meetings), else Whisper. An explicit choice is respected; when it is not ready the other ready engine runs and the
screens say so (`Status.batch_fallback`). The meeting screen's 다시 받아쓰기, the automatic re-transcription after a
recording (`auto_retranscribe`, on by default; see known-issues.md) and the record screen's hints use the same pick
(`Status.batch_engine`, `Status.batch`). Live captions are not affected.

Qwen3-ASR needs two files: the transcript model and Qwen's forced aligner that gives every word its time (the contract
needs word times; Qwen3-ASR alone gives none). Qwen publishes safetensors only, so the files are the q8_0 GGUF
conversions published by CrispASR's author (`cstr/…` on Hugging Face), pinned by commit and sha256 like the others; the
settings name both Qwen and the conversion. The engine is `engines/qwen3-asr` (CrispASR C library, MIT; see its README).
`MEETKO_BATCH_MODEL` / `MEETKO_BATCH_ENGINE` / `MEETKO_BATCH_MODEL_UNPINNED` work like their stream/diarization
counterparts. A transcription stages both files (3.5 GB) into a private folder while hashing them, once per run. On
macOS the engine runs under a sandbox profile without network access, other programs, AppleEvents or LaunchServices,
and with writes and home-folder reads limited to what it needs (`engines/qwen3-asr/README.md`); if the profile cannot be
applied the app refuses to run it ("이 컴퓨터에서는 Qwen3-ASR 엔진을 안전하게 격리해서 돌릴 수 없어서 멈췄어요") unless
`MEETKO_BATCH_SANDBOX=off` is set for development. No release build carries the Qwen3-ASR engine yet (development
builds find it in the repository's build output); the settings then show its rows as unavailable. Each of the two settings rows
judges its own file (a file is "bad" only when it is there and does not match its pin), and a file that is already in
place and matches is not downloaded again.

## Silence and Whisper (다시 받아쓰기)

Whisper given audio without speech does not return nothing: on a muted mic channel it writes subtitle credits
("한글자막 by …", "…광고를 포함하고 있습니다") every 30 seconds (`crates/meetko-core/src/whisper.rs`):

- Before whisper, a cheap energy check (`speech_gate.rs`) removes only channels that are dead or steady. It measures
  20 ms frames; frames at or below -80 dBFS (muted devices, the zero-filled spans a capture writes) are left out, and
  the floor is the 10th percentile of the rest. A channel is skipped when every frame is at or below -80 dBFS, or when
  less than 100 ms in total is 3 dB or more above the floor and the frames' spread (90th minus 10th percentile) is
  under 2 dB. A skipped channel is not sent to whisper, gets no words, and its producer says `skipped: "no_speech"`;
  the provenance panel shows "말소리 없음으로 건너뜀" for it. Its coverage stays the whole file: coverage means the
  channel was looked at, not that it was silent. Anything else goes to whisper as before: speech even at or below
  the level of steady noise (0 dB and -3 dB SNR are tested), quiet speech over a quiet floor (speech 40 dB below
  normal level, at the noise floor, was still sent), clicks, coughs, typing, a noise floor that changes level, and a
  file the check cannot read, so hallucinations on such channels are not addressed by it. Known edge: heavily
  compressed speech or music held within 3 dB with no pauses could be judged steady. Channels it sends on are
  transcribed exactly as before.
- whisper.cpp's own VAD (`--vad`, whisper.cpp 1.7.6 or newer) is opt-in and not offered by the app: set
  `MEETKO_WHISPER_VAD_MODEL` to a directory holding `ggml-silero-v6.2.0.bin` (pinned in
  `engines/whisper-cpp/models.toml`; copied privately and checked against the pin at every run, and a file that does
  not match, or a whisper-cli without VAD, is an error). On the 2026-11-10 comparison it added nothing the speech check
  had not already done and dropped whole stretches of real speech in some meetings, so it is off by default.
  whisper.cpp maps its segment times back onto the recording but not its word times (1.9.5), so meetko rebuilds
  whisper.cpp's time map from its log (checked against the point count it logs) and maps every word; if the map
  cannot be read, the channel is transcribed again without VAD.

The producer's `params` record both: `speech_gate` (settings and result), `vad` (true with `vad_model` id and sha256 and
the VAD settings, or false with `vad_unused`: `no_speech`, `not_enabled`, `no_time_map`). A skipped channel (or one
transcribed with VAD) covers the whole file. There is no phrase list: the
JSON whisper-cli writes has no per-segment no-speech probability or average log-probability to filter on, and the
token probabilities of the credits (0.1 to 0.98) overlap those of real speech.

## Where models go and how they are found

Models the app fetched or copied live in `<data dir>/models/<pin id>/<file>` (a 0700 folder). `LiveConfig::from_env`
and `DiarConfig::from_env` read that folder when `MEETKO_STREAM_MODEL` / `MEETKO_DIAR_MODEL` are unset; a variable
always wins. Both are read per recording and per transcription, and the status per call, so a finished download is
used at the next recording without restarting the app.

A file in that folder is only a candidate: it is hashed against `engines/*/models.toml` before every use and staged
exactly like a directory named by a variable (`crates/meetko-core/src/live.rs`). A file that was swapped later is
refused and the settings show it as 확인 필요. The development switches `MEETKO_STREAM_MODEL_UNPINNED` /
`MEETKO_DIAR_MODEL_UNPINNED` apply only to a directory a variable named, never to the app's folder. The app's own
diarization model without a diarization engine means no diarization (quietly), while a model named by
`MEETKO_DIAR_MODEL` without its engine is still reported. Executables are never located through the data dir
(`capture_run::pick_bin`).

The folders `models/` and `models/<pin id>/` must be real directories (a symbolic link is refused before anything is
created through it) and are tightened to 0700 when they are ours; the `.part` is opened without following links
and locked (`flock`, unix) so two processes never write the same file; the lock is held through the rename, and the
folder is synced after it. An import copies into its own `<file>.import`, so it never wipes a half-done download, and
opens only a regular file (a FIFO or device is refused before it could block).

## Download

`crates/meetko-core/src/model_setup.rs`, started only by a click, with size, publisher, license and source on screen
before it.

- Source: the publisher's Hugging Face repository at a fixed commit (for the Qwen3 files, the converter's repository, see above),
  `https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b/resolve/ea30d66debe3740a08b573244286791d423d6b3e/nemotron-3.5-asr-streaming-0.6b.q8_0.gguf`
  and
  `https://huggingface.co/nvidia/Nemotron-3-Diarization/resolve/f667ed73aee57d40cc39428eb768b4fd87a0a29e/Nemotron-3-Diarization.q8_0.gguf`.
  Hugging Face's `paths-info` for these commits reports the pinned sha256 and these sizes (checked 2026-11-10). The
  streaming file is stored as `model.q8_0.gguf`, the name its pin uses.
- HTTPS on the default port only. Redirects are followed by the app, not the HTTP client, and every hop must be
  `huggingface.co` or a subdomain of `hf.co` / `huggingface.co` (Hugging Face sends the file from `*.cdn.hf.co`). At
  most 5 hops. Plain http to a loopback test server exists only behind an explicit test field of `Source`, which no
  offer sets.
- Connect timeout 15 s, 30 s without a byte ends the attempt, no overall limit (a slow link may take its time).
- Never more bytes than the pinned size; free space is checked first (unix).
- The bytes go to `<file>.part`, are hashed on the way, and the file is renamed into place only when the sha256
  matches. A mismatch or an oversized answer removes the part.
- Cancel or a broken connection keeps the part; the next click resumes with a `Range` request after hashing what is
  there. A server that ignores the range sends the whole file and the part starts over.
- One job at a time; progress comes as `model-setup` events and survives leaving the screen.

이미 받은 파일 고르기 opens the native file picker in Rust (the path never reaches the webview), refuses a file of
another size before copying, and copies the file in while hashing it; the user's file is only read.

Errors the UI words plainly: no connection (the part is kept), disk full, checksum mismatch (file removed), a host
outside the list, a picked file that is not the model.

## What stays manual, and why

- **Engines.** A debug build finds them in the repository's build output (`engines/nemotron-stream/target/debug/`,
  linked against `vendor/lib` by its rpath), so the setup shows the two build steps (`fetch-libs.sh`, `cargo build`).
  A release build looks next to the app executable or in `MEETKO_STREAM_ENGINE` / `MEETKO_DIAR_ENGINE`; no bundle
  carries them yet, and the app does not download programs.
- **Whisper.** Optional now that live captions become the draft. whisper-cli is a program (not downloaded); its model
  is not pinned the way the streaming models are. The settings list the steps (`brew install whisper-cpp`, a ggml
  model, `[whisper] model` in `config.toml`, and `MEETKO_WHISPER_CLI` because an app opened from Finder does not see
  Homebrew's PATH).
- **The legacy sherpa model.** Its license is unclear (see the README), so the app does not offer it.
- **SenseVoice-Small** (considered for 다시 받아쓰기, 2026-11-10). Its weights are under the FunASR Model Open Source
  License Agreement v1.1, whose section 3 says the model "is provided for reference and learning purposes only", whose
  section 4.2 ends every license for "unjustified denigration" of the model, and whose section 6 lets the licensor
  change the terms with effect for everyone who keeps using it. The maintainers have answered that section 3 is only a
  disclaimer and commercial use is allowed, but marked that answer (SenseVoice issue #334, 2026-09-22) as not the
  license owner's final confirmation. Until it is, the app does not offer it. sherpa-onnx (already used here) would run
  it with token times; nothing else stands in the way.

The tests use a local server and small files. Checked once by hand (2026-11-10): the real 107 MB diarization file came
through the Hugging Face redirect, matched its pin, and was found by `managed_model_dir`; an interrupted try resumed.
Not verified: the 742 MB streaming download, and a recording or diarization run with a downloaded model.
