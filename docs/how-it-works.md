<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# How meetko works

The longer version of the feature list in the [README](../README.md). The app's interface is in Korean; Korean labels
are quoted with an English gloss.

## Components

The core is a Rust library inside the desktop app and the CLI. Capture, engines and plugins are separate processes
that talk to the core only through a protocol. The principles are in
[manifest-draft.md](manifest-draft.md) (Korean) and [capture-interface.md](capture-interface.md).

| Path | What it is |
|---|---|
| `capture/macos` | Swift sidecar: microphone + system audio, written as `mic.wav`, `loopback.wav` and `capture.json` |
| `crates/meetko-core` | Storage, versions, approval, search, export, model downloads, plugin host |
| `crates/meetko-cli` | The command line, `meetko` |
| `app/` | Desktop app: Tauri v2 + TypeScript UI (`app/src-tauri`) |
| `engines/nemotron-stream` | Live captions (`asr.stream`) and speaker diarization, NDJSON JSON-RPC over stdio |
| `engines/sherpa-stream` | Live captions (`asr.stream`) with the same protocol (the older engine) |
| whisper.cpp | Batch transcription for re-transcription (external executable `whisper-cli`, optional) |
| `engines/qwen3-asr` | Re-transcription (`asr.batch`, experimental): Qwen3-ASR plus word-time alignment, NDJSON JSON-RPC over stdio |
| plugins | Not in this repository: the catalog and official examples are in [Eastsidegunn/meetko-plugins](https://github.com/Eastsidegunn/meetko-plugins), third-party plugins in their authors' repositories (see [plugins.md](plugins.md)). Test fixtures stay in `tests/fixtures/plugins/` |
| `contract/` + [rfc-0001-contract.md](rfc-0001-contract.md) | Schemas and the contract description |
| `reference/` | Python reference implementation (`tests/` checks the contract against it) |

The core never captures audio itself. Rust is used for the core, CLI and engines, Tauri v2 + TypeScript for the app,
and Swift for the macOS capture sidecar.

## Recording

Your voice (microphone) and the other side (system audio) are recorded as separate channels on one meeting timeline
(macOS). The channel labels the side: channel 0 is "나" (me), channel 1 is "상대" (the other side).

## Live captions become the first draft

Captions appear while you record, and when the recording stops they become a draft you can review right away. With a
diarization model, speakers are separated too.

## Automatic re-transcription (optional)

Live captions are for the screen. Once the live draft exists after a recording, the whole recording is transcribed
again with the most accurate batch engine that is ready (Qwen3-ASR if it is set up, otherwise Whisper), which makes a
new draft.

- You can read the live draft meanwhile; when the new draft is done the screen switches to it quietly. If you are
  looking at a version you picked yourself, only a "새 초안이 준비됐어요 · 보기" (new draft ready · view) notice appears.
- If a version is already approved, the new draft becomes the reference only after you approve it.
- If the live session already separated speakers on the same audio, that separation is reused (provenance records it
  as made by the live session); otherwise speakers are separated again.
- **Settings › 받아쓰기 준비** chooses the engine (auto / Whisper / Qwen3-ASR) and the switch "녹음이 끝나면 자동으로
  다시 받아쓰기" (on by default). The meeting screen's "다시 받아쓰기" (re-transcribe) runs it by hand at any time.
- With no engine at all, only the live draft remains and the app keeps working.
- The CLI has no live captions, so it has no automatic step; use `meetko transcribe`.

Internal measurement on synthetic Korean–English meetings (character error rate, lower is better): live captions
38.9%, Whisper large-v3-turbo 17.6%, Qwen3-ASR 16.7%. Qwen3-ASR took about 2.5–3 minutes per 10-minute meeting on the
machine measured. Limits of this step are listed in [known-issues.md](known-issues.md); engine details in
[model-setup.md](model-setup.md).

## Review and approval

Every word has a time, so you can jump to the audio at that word. Corrections are saved as a new version.

## Speaker names and text edits

Rename "화자 2" (Speaker 2) to a real name, merge two speakers into one person, or fix a misheard sentence in place.
The original transcript stays as it is and edits are laid over it, so "원래대로" (restore) undoes them at any time.
Edits are reflected in export, search and the AI agent connection (MCP). When a re-transcription makes a new
transcript, earlier names and edits are not carried over; they stay with the earlier version.

## Vocabulary

In **Settings › 용어 사전** (vocabulary), list names, product names and English terms that come up often; they are
passed as hints when re-transcribing with Whisper (not yet used by Qwen3-ASR or live captions). The find-and-replace
list is applied once each, top to bottom, to what is shown and exported; the stored original does not change. The
vocabulary can be imported and exported as JSON.

## Export

"내보내기" (export) on the meeting screen saves Markdown (.md), text (.txt), SRT or WebVTT files, or copies in several
forms (Markdown, text only, with times and speakers, with speakers). The CLI exports webvtt, ctm, rttm and text.

## Search

The meeting list searches the contents of every meeting (the approved version, else the draft, else the live
captions). From the command line: `meetko search <query>`.

## Meeting prompt (macOS, on by default)

**Settings › 회의 알림** (meeting prompt). On by default on macOS; a switch turns it off and that choice is kept.
When a known meeting app (the list is in [meeting-detection.md](meeting-detection.md#apps)) has been using the microphone for about 6
seconds, the app asks "회의가 시작된 것 같아요. 녹음할까요?" (looks like a meeting started, record?). While recording, if
the meeting app lets go of the microphone for 30 seconds, it asks "녹음을 정지할까요?" (stop recording?). The stop
question comes only from meeting apps, not browsers, because browsers take and release the microphone for other
reasons too. Watching browsers (web meetings) has its own switch. **It never starts or stops a recording by itself.**

What it looks at: every 3 seconds it asks macOS (CoreAudio) only which programs are using microphone input (their
bundle ids). What it does not look at: window titles, browser tabs, addresses, screen contents, sound. It uses no
Accessibility or Screen Recording permission and records or sends nothing. Full rules:
[meeting-detection.md](meeting-detection.md).

## The contract

The data formats between engines and the core, and between the core and plugins, are defined by
[rfc-0001-contract.md](rfc-0001-contract.md) and the JSON schemas in `contract/schemas/`. Changing the contract needs a
discussion first ([CONTRIBUTING.md](../CONTRIBUTING.md)). Third-party components are listed in
[THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
