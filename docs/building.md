<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Building meetko from source

There is no installable app yet: meetko 0.1.0 is built from source, and the live-caption and diarization engines are
not bundled with the app, so they are built separately. Only macOS 15+ on Apple Silicon has been verified end to end.

Test commands are in [CONTRIBUTING.md](../CONTRIBUTING.md). Environment variables and the CLI are in
[configuration.md](configuration.md).

## Requirements

- **macOS: an Apple Silicon Mac.** The default engine's `engines/nemotron-stream/fetch-libs.sh` only fetches Apple
  Silicon (arm64) libraries on macOS.
- **Rust** (CI uses 1.98.1).
- **Node.js** (CI uses 24).
- **Xcode Command Line Tools** on macOS (Swift, for the capture sidecar; also Apple's `/usr/bin/python3`, which Python
  plugins run with).
- **Python 3.12** for the tests.

## Build

```sh
# Frontend first: the Tauri app crate reads app/dist at compile time
(cd app && npm ci && npm run build)

# Core and CLI (the Cargo workspace)
cargo build --workspace

# macOS capture sidecar (Swift)
(cd capture/macos && swift build -c release)

# Live-caption and diarization engine (a standalone crate outside the workspace)
engines/nemotron-stream/fetch-libs.sh && (cd engines/nemotron-stream && cargo build)

# Desktop app (a separate crate outside the workspace)
(cd app && npm run tauri dev)
```

`fetch-libs.sh` downloads a pinned version of a prebuilt library archive, checks it against its sha256 and unpacks it
into `vendor/` (ignored by git). Only the macOS build has actually been tried.

Optional engines:

```sh
# Older live-caption engine (sherpa-onnx)
engines/sherpa-stream/fetch-libs.sh && (cd engines/sherpa-stream && cargo build)

# Experimental Qwen3-ASR re-transcription engine (macOS Apple Silicon only)
engines/qwen3-asr/fetch-libs.sh && (cd engines/qwen3-asr && cargo build)
```

## First run (macOS)

1. Build in the order above and open the app with `npm run tauri dev`. A development build finds the capture program
   (`capture/macos/.build/release/`) and the engine (`engines/nemotron-stream/target/debug/`) inside the repository on
   its own.
2. Download the **live-caption model** (742 MB) from the setup card in the meeting list, or from
   **Settings › 받아쓰기 준비** (transcription setup). The diarization model (107 MB) is optional. Before downloading,
   the app shows the size, publisher, license and source; an interrupted download resumes. If you already have the
   files, "이미 받은 파일 고르기" (choose files you already have) imports them. Either way a file is used only when it
   matches its pinned sha256 ([model-setup.md](model-setup.md)).
3. Start a recording. The first time, macOS asks for microphone and "Screen & System Audio Recording" permission.
   Screen contents are not saved.
4. When the recording stops, the live captions become the draft. Review it, approve it, then export it or run plugins.

### Whisper re-transcription (optional)

Install the program with `brew install whisper-cpp` and download a ggml model, then set `[whisper] model` in
`config.toml` in the data folder and set `MEETKO_WHISPER_CLI` (an app opened from Finder does not see Homebrew's
`PATH`). The settings screen shows the same steps.

## Models and pins

Models are not in this repository or in the app, and meetko does not redistribute them. Each model has its own license;
check the original source's terms before use. The model table is in the [README](../README.md#models).

When you redistribute an OpenMDW-1.1 model to others, you must include the full license text (it is in
[THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md)).

The live-caption, diarization and Qwen3-ASR models have per-file sha256 values pinned in
[engines/nemotron-stream/models.toml](../engines/nemotron-stream/models.toml),
[engines/sherpa-stream/models.toml](../engines/sherpa-stream/models.toml) and
[engines/qwen3-asr/models.toml](../engines/qwen3-asr/models.toml). The core refuses a model that does not match
(the development switches `*_UNPINNED=1` apply only to a folder named by an environment variable, never to models the
app downloaded). Models the app downloaded or imported live under `models/` in the data folder, which is used when no
environment variable is set. For file names, conversion steps and folder layout see each engine's README
([nemotron](../engines/nemotron-stream/README.md), [sherpa](../engines/sherpa-stream/README.md)).

## Platform status

- **macOS 15+ (Apple Silicon):** the only platform verified end to end.
- **Windows:** a CI configuration (frontend build, Python reference tests, tests and clippy for the workspace and the
  app crate) is included but currently turned off. There is no capture program or meeting prompt, and it has not been
  used for real.
- **Linux:** neither built nor tested.
- Plugin isolation exists on macOS only (OS sandbox). See [plugins.md](plugins.md).
