<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# meetko-nemotron-stream

An `asr.stream` engine for live captions, next to `engines/sherpa-stream` and speaking the same
protocol: a native process around NVIDIA's Nemotron 3.5 streaming RNNT (`nemotron-3.5-asr-streaming-0.6b`)
through the NeMo-Speech.cpp C API (Apache-2.0). The core starts one per channel while a capture runs and
talks to it in NDJSON JSON-RPC 2.0 over stdio (`crates/meetko-core/src/live.rs`).

Why: on 5 h of Korean meetings the CER was 14.0 % against 46.6 % for the sherpa zipformer
(Whisper batch: 13.5 %). These numbers were measured on meeting data that is not included in this repository
(the AIHub data cannot be redistributed), so they cannot be reproduced from it. The model is weaker on English words inside Korean speech; `auto` helps (see Language).

Not a member of the root workspace: it links prebuilt shared libraries, which a plain
`cargo test --workspace` must not need.

This crate builds two engines on the same libraries: `meetko-nemotron-stream` (above) and
`meetko-nemotron-diar`, the batch speaker diarization engine (see Diarization below). They share
`vendor/`, `build.rs`, `fetch-libs.sh`, the hand-written C declarations and the protocol plumbing
(`src/lib.rs`: `ffi`, `diar`, `rpc`); a separate crate would have needed a second copy of ~100 MB of
libraries and of the same FFI helpers. The cost is that they are built and shipped together.

## Build and run

```
engines/nemotron-stream/fetch-libs.sh           # pinned NeMo-Speech.cpp 0.2.0, sha256-checked, into vendor/ (gitignored)
(cd engines/nemotron-stream && cargo build)     # the debug build is what a debug app finds by itself
export MEETKO_STREAM_MODEL=/path/to/model-dir   # holds model.q8_0.gguf
export MEETKO_STREAM_LANGUAGE=auto              # optional: auto (default), ko, en-US, ...
export MEETKO_STREAM_ENGINE=/path/to/meetko-nemotron-stream     # optional for a debug app
```

Without `MEETKO_STREAM_MODEL` (and `MEETKO_DIAR_MODEL`) the core uses the model the app downloaded or copied into
`<data dir>/models/` (Settings → 받아쓰기 준비; [docs/model-setup.md](../../docs/model-setup.md)), checked against the
same pins. The engines themselves are not downloaded.

The core picks the engine from the model directory: `models.toml` of each engine says which files make
up a known model, and the directory that holds the files of this one runs this engine (the sherpa
directory runs `meetko-sherpa-stream`). `MEETKO_STREAM_ENGINE` overrides the binary.

### The model is native-parser input

The weights are the Hugging Face file `nemotron-3.5-asr-streaming-0.6b.q8_0.gguf` of
`nvidia/nemotron-3.5-asr-streaming-0.6b` (OpenMDW-1.1), kept in the model directory as
`model.q8_0.gguf` (742 MB, sha256 pinned in `models.toml`). The same rules as for the sherpa model apply:
the core copies the file into a private 0700 directory **while hashing it**, refuses anything that does
not match the pin (unless `MEETKO_STREAM_MODEL_UNPINNED=1`, development only) and hands the engine that
copy. The engine itself does not hash the file again, because the bytes it opens are exactly the ones
the core verified; run by hand it trusts whatever `MEETKO_STREAM_MODEL` names. A recording stages the
model once and its channels share the copy (read-only); the directory is removed when the last channel
ends, and a directory left by a crashed process (`meetko-model-<pid>-*`) is removed by the next start.
The copy takes a few seconds, more in an unoptimized debug build of the core, because of the hashing.

### Libraries, rpath, distribution

`fetch-libs.sh` verifies the archive and, on macOS, every library it keeps (arm64 with Metal), refuses
unsafe archive members and symlinks that leave their directory, and copies the license texts of the
archive into `vendor/licenses/nemo-speech/`. Linux (x86_64 CPU) and Windows (x86_64 CPU, `MEETKO_NEMO_BACKEND=vulkan`
for Vulkan) are pinned and unpacked the same way but **have not been built or run**.

Rpaths are as in `sherpa-stream`: release builds look in `@executable_path/../Frameworks` and
`../lib`; debug builds (or `MEETKO_ENGINE_DEV_RPATH=1`, needed to run the release tests) also in
`vendor/lib`. The libraries find each other through their own `@loader_path`. For distribution the
upstream dylibs must be re-signed with your Developer ID like the sherpa ones (see its README).

`src/ffi.rs` is hand-written against `asr.h` of the pinned release (`vendor/asr.h`); a layout test asks the
library for its own struct size.

## Behaviour

- The final `text` is the library's transcript with punctuation (`enable_automatic_punctuation`, which this
  model needs to keep what its words and partials already carry), so a caption does not lose its periods
  when a segment finalizes.
- Words come from the library with start and end in milliseconds from the start of the stream (checked: they
  are not per-utterance). Only finals carry words (`units`); a partial carries the transcript so far with
  empty `units`, so a caption can show text before the words are final. The core builds words from finals only.
- An utterance ends after 800 ms of trailing silence (the library's endpointing, token-silence based);
  `MAX_SEGMENT_CHARS` forces an end like in the sherpa engine. The tail is flushed by `stream.close`.
- Streaming policy: 160 ms chunks, right context 1 (the library's low-latency preset), greedy RNNT.
  GPU 0 (Metal) on macOS, CPU elsewhere. `stream.open`'s result records these as `params`.
- `emitted_at_ms` is the audio fed so far, as in the sherpa engine.

### Language

The language comes from `MEETKO_STREAM_LANGUAGE` (the core reads it and sends it in `stream.open`; run by
hand, the engine reads it too). The default is `auto`: the model detects the language per utterance. On
the same (unpublished) Korean meetings come out the same as with `ko` (CER 14.1 % against 14.0 %) while a
Korean-English code-switch set improves from 76.6 % to 44.8 %. `ko` is `ko-KR` and fixes Korean, `en` is
`en-US`, and any `xx-YY` the model supports passes through. Anything else is refused with `-32602`.

## Protocol

Identical to `engines/sherpa-stream` (see its README): `initialize`, `stream.open {sample_rate:16000,
language}`, `stream.push` notifications of 100 ms PCM16, `stream.event` notifications that are contract
`asr_stream_event` objects, `stream.error`, `stream.close`, `shutdown`. Invalid PCM, a push without an open
stream, a non-UTF-8 line, or (while a stream is open) a line that is not JSON ends the process: one
`stream.error`, then exit code 1 through `_exit` (a normal exit runs ggml-metal's static destructors while
the model is loaded, which asserts). Stdout carries protocol only.

## Diarization (`meetko-nemotron-diar`)

Role `diarization`, manifest `meetko-diar.toml`. After batch ASR the core runs it once per channel and
gives compose the turns, so words get speakers (`crates/meetko-core/src/diarize.rs`,
`pipeline::transcribe_with`). The model is `nvidia/Nemotron-3-Diarization` (OpenMDW-1.1) as the q8_0 GGUF
`Nemotron-3-Diarization.q8_0.gguf` (107 MB, up to 8 speakers, 10 ms frames), pinned in `models.toml` with
`role = "diarization"` and staged and verified by the core exactly like the streaming model. It beat
Sortformer-4spk-v2 on 30 AIHub Korean meetings, data that is not included in this repository (streaming DER 15.7 % against 23.6 %, speaker confusion
4.1 % against 9.6 %, speaker-count error 0.2), which gets much worse from 5 speakers on.

```
export MEETKO_DIAR_MODEL=/path/to/model-dir     # holds Nemotron-3-Diarization.q8_0.gguf; unset = no diarization
export MEETKO_DIAR_ENGINE=/path/to/meetko-nemotron-diar   # optional for a debug app or the CLI in the repository
export MEETKO_DIAR_MODEL_UNPINNED=1             # development only; recorded as params.model_pinned=false
export MEETKO_DIAR_TIMEOUT_SECS=120             # added to the audio length to make the time budget of a request
```

Protocol (NDJSON JSON-RPC 2.0, stdout carries protocol only): `initialize` (answers `role: "diarization"`),
`diarize.run {wav, audio_sha256}`, `shutdown`. `wav` is a mono PCM16 file (8-96 kHz; the library resamples)
that the caller has already hashed; `audio_sha256` is echoed into the result, which is a contract
`diarization` engine result: integer-ms `turns` with engine-local labels `spk1`, `spk2` ..., `params`
(`mode: streaming`, the model's default geometry, the library's segmentation defaults, `max_speakers`)
and `network: false`. The model loads on the first request and stays loaded; the answer for the same audio
is the same. A request the engine cannot serve (no model, a stereo wav, bad parameters) is an ordinary
error reply (`-32010` input rejected, `-32602`, `-32601`) and the process goes on. A library failure
(including a model file that does not load) or a line that is not UTF-8 ends the process after one
`engine.error` notification with exit code 1 through `_exit`, as in the stream engine.

The pass is the *streaming* one (160 ms pushes, like the library's own example and the benchmark), so whole
recordings work; the offline full-attention path is limited to about 6.6 minutes. On an M-series Mac a
10-minute meeting takes about 9 s in a debug build (Metal), a 69 s recording about 1 s. Linux and Windows
use the CPU build and have not been run.

How the core names the speakers: a channel with one detected speaker (or none) keeps the channel's own speaker
and label, "나" or "상대", as without diarization; with several they are "나 1", "나 2" / "상대 1", "상대 2" in
order of first speech. Words that no single speaker covers by half stay unassigned or ambiguous, as
the contract says. The model separates voices, not people: two TTS voices may be merged, and a single
person may be split in two. If the engine fails, times out or breaks the contract, the transcript is stored
with channel speakers and a warning is shown.

## Tests

```
cargo test                                      # unit tests; the stdio tests skip without model and wav
MEETKO_REQUIRE_MODELS=1 cargo test              # ... and fail instead of skipping
MEETKO_ENGINE_DEV_RPATH=1 cargo test --release --test stdio -- --nocapture   # the real model, real-time factor
MEETKO_TEST_PACED=1 ...                         # feed in real time to see the wall-clock latency
MEETKO_DIAR_MULTI_WAV=/path/to/meeting.wav MEETKO_REQUIRE_MODELS=1 cargo test --test diar -- --nocapture
```

`tests/stdio.rs` also covers the fatal paths (odd-length PCM, a torn line, a push without a stream: one
`stream.error`, exit code 1, no `GGML_ASSERT`), re-opening a closed stream, and compares the transcript with
`raw/meeting01/reference.json` (recall of the reference's Hangul and digits, CER with bounds explained in the
file). The main test pushes a wav in 100 ms chunks through the real binary, validates every event against
the contract schema the core uses (`meetko-core` is a dev-dependency), and checks Korean finals, word
times in order and inside the audio, consecutive segments and revisions. It uses `models/` and
`raw/meeting01/loopback.wav` of the repository when present.

`tests/diar.rs` runs the diarization binary: a real recording (`raw/meeting01/loopback.wav`, whose two TTS
voices are merged into one speaker, so no count is asserted) checked against the contract schema, turns in
order and inside the audio, the same answer on a second request; a recording with several speakers
(`MEETKO_DIAR_MULTI_WAV`, no default: an AIHub meeting clip gives 3 speakers) with at least two found; the
core's `DiarProcess` with the pinned model (`model_pinned: true`); and, without any model, the rejected
requests and the fatal paths (a line that is not UTF-8, a model file that does not load: one `engine.error`,
exit code 1, no `GGML_ASSERT`). Without the models the model-backed tests skip, or fail under
`MEETKO_REQUIRE_MODELS=1`. `crates/meetko-core/tests/diarize.rs` covers the core side with a fake engine:
success, a dying engine, a hanging one, output for other audio or breaking the schema, an unpinned model,
cancelling, and the path without an engine.

## Third-party software

This engine links NeMo-Speech.cpp (Apache-2.0) and ggml (MIT). See `THIRD_PARTY_NOTICES.md`.
