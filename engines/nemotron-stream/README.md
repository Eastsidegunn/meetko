<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# meetko-nemotron-stream

An `asr.stream` engine for live captions, next to `engines/sherpa-stream` and speaking the same
protocol: a native process around NVIDIA's Nemotron 3.5 streaming RNNT (`nemotron-3.5-asr-streaming-0.6b`)
through the NeMo-Speech.cpp C API (Apache-2.0). The core starts one per channel while a capture runs and
talks to it in NDJSON JSON-RPC 2.0 over stdio (`crates/meetko-core/src/live.rs`).

Why: on 5 h of Korean meetings the CER was 14.0 % against 46.6 % for the sherpa zipformer
(Whisper batch: 13.5 %). These numbers were measured on meeting data that is not included in this repository
(the AIHub data cannot be redistributed), so they cannot be reproduced from it. The model is weaker on English words inside Korean speech; `auto` and a fresh stream per utterance help (see Language and Code-switched speech).

Not a member of the root workspace: it links prebuilt shared libraries, which a plain
`cargo test --workspace` must not need.

This crate builds two engines on the same libraries: `meetko-nemotron-stream` (above) and
`meetko-nemotron-diar`, the speaker diarization engine, batch and live (see Diarization below). They share
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
- Every utterance is decoded by a fresh library stream (`src/session.rs`): when the library ends an utterance
  with words, the engine closes the stream, opens a new one on the same loaded model, and feeds it again the
  audio from the end of that utterance's last word (at most 10 s). Word times of the new stream are shifted by
  where it began. One stream for the whole recording carried the model's encoder cache and decoder state from
  one utterance into the next; after a switch between Korean and English the model then wrote the next
  utterance in the wrong script or, more often, emitted nothing at all for whole sentences. `stream.open`
  records this as `params.stream_per_utterance: true`. See "Code-switched speech" below for the measurement.
- Streaming policy: 160 ms chunks, right context 1 (the library's low-latency preset), greedy RNNT.
  GPU 0 (Metal) on macOS, CPU elsewhere. `stream.open`'s result records these as `params`.
- `emitted_at_ms` is the audio fed so far, as in the sherpa engine.

### Language

The language comes from `MEETKO_STREAM_LANGUAGE` (the core reads it and sends it in `stream.open`; run by
hand, the engine reads it too). The default is `auto`: the model detects the language per utterance. On
the same (unpublished) Korean meetings come out the same as with `ko` (CER 14.1 % against 14.0 %) while a
Korean-English code-switch set improves from 76.6 % to 44.8 %. `ko` is `ko-KR` and fixes Korean, `en` is
`en-US`, and any `xx-YY` the model supports passes through. Anything else is refused with `-32602`.

### Code-switched speech

Measured on four synthetic meetings (40 min) built from HiKE Korean-English code-switched clips (Apache-2.0;
not in this repository), `auto`, through the core's live path: with one library stream for the whole
recording the captions had a CER of 61.8 % (letters and digits), and 9,218 of the 19,937 reference characters
were deleted: whole sentences after a switch of language came out as nothing. Feeding the same audio to the
library's offline call gave the same gaps, while each utterance cut out and decoded alone came back, so the
loss came from state carried between utterances, not from the audio. With a fresh stream per utterance the CER
is 38.9 % and 4,034 characters are deleted; English terms found 54.7 % (was 27.8 %). Decoding each reference
utterance alone (the best this model can do with perfect segmentation) gives the same deletion rate on a
6-minute subset (18.8 % against 19.1 %), so what is still lost is the model's own limit on a switch inside one
utterance (Whisper large-v3-turbo after the recording: 17.6 % on the same meetings). Korean-only speech is
unchanged (a TTS meeting: 3.9 % before, 4.2 % after; `raw/meeting01` recall 0.989 before, 0.979 after), and so
is the latency on the audio clock (first text of a segment 0.2 s after its first word, finals 0.9 s after the
last). The extra cost is the replay, about one second of audio decoded twice per utterance.

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
`diarize.run {wav, audio_sha256}`, the live session (`diarize.open`, `diarize.push`, `diarize.event`,
`diarize.close`, see Live session below), `shutdown`. `wav` is a mono PCM16 file (8-96 kHz; the library resamples)
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

### Live session

The same streaming pass, fed while a capture records (`crates/meetko-core/src/live/diar.rs`: one process per
channel next to the caption engine, reading the same growing WAV). The sessions start once both caption engines
listen, so the captions warm up first; the audio recorded meanwhile waits in the file. It is our engine protocol,
not part of the contract; only its final result is a contract `diarization` result.

- `diarize.open {sample_rate}` (8000-96000; the core sends 16000) opens a session and loads the model if needed;
  the reply is `{params}` (`mode: streaming`, `live: true`, as below). One session at a time per process
  (`-32602` for a second); no model is `-32010` and the process goes on.
- `diarize.push {pcm_s16le_b64}` notifications: mono PCM16, any length (the core sends 100 ms). Invalid PCM, a push
  without a session, or a line that is not JSON while a session is open ends the process like a library failure
  (one `engine.error`, exit code 1): dropped audio would shift every later turn.
- `diarize.event {keep, turns, labelled_ms, emitted_at_ms}` notifications, at most one per second of audio the
  model has labelled: keep the first `keep` turns of what was sent so far and replace the rest with `turns`
  (integer ms from the start of the session, labels `spk1`...). The model revises the recent past (a turn grows, a
  short one appears), so a turn already sent can come again; turns before `keep` never change. `labelled_ms` is
  how far the model has labelled (it trails the audio by a chunk and its right context), `emitted_at_ms` the audio
  pushed so far. An event goes out every second even when nothing changed: the core measures the session's lag by it.
- `diarize.close {audio_sha256?}` finishes the stream (labels the tail) and answers with the same result as
  `diarize.run`, with `params.live: true` and `params.fed_ms` (and without `push_ms`). `audio_sha256` is echoed
  when given; the core does not give it, because the session read no file. It adds the sha256 import recorded for
  the channel file itself, and only when the file still has that sha256, the PCM the core pushed is exactly the
  file's PCM (same length, same sha256 of the bytes pushed), and the engine's `fed_ms` agrees.

Fed the same audio, the closed session gives the same turns as `diarize.run` (`tests/diar.rs` checks it on both
channels of `raw/meeting01`). On an M-series Mac a debug build labels 69 s of audio in 1.3-3 s (20-50x real time).

What the core does with it: while recording, each caption segment gets the speaker who covers most of it, after
the same 10 s merge rule and numbering as the stored version ("상대 1", "상대 2"; nothing while the channel has one
speaker left), sent to the app as `live` events of kind `speakers` at most once a second per channel; nothing of
them is stored. These labels are provisional: a number can change while the model learns, and the stored speakers
(from the closed session's final turns, with the merge rule applied to the whole recording) may be numbered
differently from what the record screen showed. At stop the session is closed and `store_live` gives its turns to
compose as for a batch draft (producer role `diarization`, `params.mode: streaming`, `params.live: true`, the staged
model's sha256 and `model_pinned`), so `draft_from_live` keeps those speakers and runs no second pass for that
channel. A session that cannot start, dies, breaks the protocol, falls more than 20 s behind the recording (plus
the audio that came in while its model loaded, up to another 20 s), or is not closed within 5 s after the captions
are in is dropped (a stop never waits long for it): its labels are withdrawn, the captions are untouched, and the
draft separates that channel after the recording as before.

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
MEETKO_CODESWITCH_WAV=/path/to/clip.wav ...     # + clip.json (utterances[].text): code-switched speech is not dropped
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

The live session: through the binary, the events fold (by `keep`) into the start of the final turns, the closed
session equals `diarize.run` on the same audio, and its requests and fatal paths (a push without a session; odd
PCM or a torn line in one) behave as above. `the_core_separates_speakers_live_next_to_the_captions` runs the
core's `LiveSession` with both real engines on both channels of `raw/meeting01` (mic: `MEETKO_DIAR_MIC_WAV`,
default `raw/meeting01/mic.wav`; it also needs the streaming model) while the files grow at 4x real time, or in
real time with `MEETKO_TEST_PACED=1`, and prints how long after a caption became final its speaker was named;
the stored live version must carry both channels' live speakers. The core side with a fake engine is in
`crates/meetko-core/src/live/tests.rs` (labels while recording, the stored live version and a first draft that
runs no second pass, a session that falls behind, dies, breaks the protocol or cannot open, a result that does
not match the stored file).

## Third-party software

This engine links NeMo-Speech.cpp (Apache-2.0) and ggml (MIT). See `THIRD_PARTY_NOTICES.md`.
