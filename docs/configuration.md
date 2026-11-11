<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Configuration and command line

Most people never need anything on this page: the app sets itself up from **Settings**. These are the environment
variables the code actually reads, for development or manual setup. **An environment variable takes precedence over
the app's settings.**

## Environment variables

| Variable | Meaning |
|---|---|
| `MEETKO_DATA_DIR` | Data folder. Default: `meetko` under the per-user local data folder (macOS: `~/Library/Application Support/meetko`) |
| `MEETKO_WHISPER_MODEL` | Path to the whisper ggml model file (can also be set as `[whisper] model` in `config.toml` in the data folder) |
| `MEETKO_WHISPER_MODEL_SHA256`, `MEETKO_WHISPER_DTW` | Pin the model hash; DTW setting |
| `MEETKO_WHISPER_CLI` | The `whisper-cli` executable. If unset, a file next to the app, then `PATH` |
| `MEETKO_WHISPER_VAD_MODEL` | (For evaluation, off by default) whisper.cpp VAD model directory (`ggml-silero-v6.2.0.bin`, sha256 pinned). When set, transcription runs with `--vad` ([model-setup.md](model-setup.md)) |
| `MEETKO_CAPTURE_BIN` | The capture program executable |
| `MEETKO_STREAM_MODEL`, `MEETKO_STREAM_ENGINE` | Live-caption model directory (if unset, the model the app downloaded) and engine executable |
| `MEETKO_STREAM_LANGUAGE` | Live-caption language (default `auto`) |
| `MEETKO_DIAR_MODEL`, `MEETKO_DIAR_ENGINE` | Diarization model directory and engine. If neither is set and the app has no downloaded model, no diarization runs |
| `MEETKO_BATCH_MODEL`, `MEETKO_BATCH_ENGINE`, `MEETKO_BATCH_TIMEOUT_SECS` | For re-transcription with Qwen3-ASR (experimental; with the setting on "auto" it is preferred when ready): model directory (if unset, the model the app downloaded), engine executable, time limit |
| `MEETKO_DIAR_TIMEOUT_SECS` | Time limit (seconds) for a diarization engine run |
| `MEETKO_DIAR_MIN_SPEAKER_SECS` | A speaker who talked less than this many seconds on a channel is not kept apart from the others. Default 10; 0 turns the rule off |
| `MEETKO_PLUGIN_TIMEOUT_SECS` | Plugin request time limit |
| `MEETKO_PLUGIN_SANDBOX` | Development only. `off` runs plugins without the macOS sandbox (the result is recorded as run without isolation) |
| `MEETKO_PLUGIN_INDEX_URL` | Address of the plugin list (`index.json`) the Browse screen reads. Default `https://raw.githubusercontent.com/Eastsidegunn/meetko-plugins/main/index.json` |
| `MEETKO_GITHUB_API`, `MEETKO_GITHUB_CODELOAD` | GitHub API and download addresses used to fetch plugins. Overrides for testing (https, or http on localhost only) |
| `MEETKO_STREAM_MODEL_UNPINNED`, `MEETKO_DIAR_MODEL_UNPINNED`, `MEETKO_BATCH_MODEL_UNPINNED` | Development only. Turn off the pinned-hash check for a model folder named by an environment variable (never applies to models the app downloaded) |
| `MEETKO_BATCH_SANDBOX=off` | Development only (macOS). Runs the Qwen3-ASR engine without isolation; recorded in provenance as `sandbox: off` |

Variables used only for tests and builds (`MEETKO_REQUIRE_MODELS`, `MEETKO_REQUIRE_PLUGINS`,
`MEETKO_ENGINE_DEV_RPATH`, `MEETKO_NEMO_BACKEND`, `MEETKO_TEST_PACED`, `MEETKO_DIAR_MULTI_WAV`) are documented in
[CONTRIBUTING.md](../CONTRIBUTING.md) and in each engine's README.

## Command line (`meetko`)

Commands: `import`, `transcribe` (whisper), `approve`, `plugin add|install|info|list`,
`plugin secret set|clear|status` (API keys and settings a plugin declares; values are read from stdin and kept in the
macOS keychain, see [plugins.md](plugins.md)), `run`, `show`,
`export` (webvtt, ctm, rttm, text), `results`, `list`, `search`, `mcp` (AI agent connection, see [mcp.md](mcp.md), Korean).

- Markdown and SRT export are currently in the app only.
- The CLI has no live captions, so there is no automatic re-transcription step either; use `meetko transcribe`.
- Search from the command line: `meetko search <query>`.
- Full options: `cargo run -p meetko-cli -- --help`.

## App settings worth knowing

- **Settings › 받아쓰기 준비** (transcription setup): model downloads, the re-transcription engine
  (auto / Whisper / Qwen3-ASR) and "녹음이 끝나면 자동으로 다시 받아쓰기" (re-transcribe automatically when a
  recording ends, on by default). See [model-setup.md](model-setup.md).
- **Settings › 용어 사전** (vocabulary): hint terms and find-and-replace. See [how-it-works.md](how-it-works.md#vocabulary).
- **Settings › 회의 알림** (meeting prompt). See [meeting-detection.md](meeting-detection.md).
