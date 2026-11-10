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

- Source: the publisher's Hugging Face repository at a fixed commit,
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

The tests use a local server and small files. Checked once by hand (2026-11-10): the real 107 MB diarization file came
through the Hugging Face redirect, matched its pin, and was found by `managed_model_dir`; an interrupted try resumed.
Not verified: the 742 MB streaming download, and a recording or diarization run with a downloaded model.
