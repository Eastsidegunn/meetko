<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# meetko-qwen3-asr (experimental)

An `asr.batch` engine for 다시 받아쓰기, next to whisper-cli: a native process that transcribes with
Qwen3-ASR-1.7B and times every word with Qwen3-ForcedAligner-0.6B, both through the CrispASR C API (MIT, ggml).
The user picks it in Settings › 받아쓰기 준비 › 다시 받아쓰기 엔진 (marked 실험 기능); Whisper stays the default.
The core starts one process per transcription and talks to it in NDJSON JSON-RPC 2.0 over stdio
(`crates/meetko-core/src/batch.rs`).

Not a member of the root workspace: it links prebuilt shared libraries, which a plain `cargo test --workspace`
must not need. macOS arm64 only so far (Metal); the Windows archive of the same release is named in
`fetch-libs.sh` but has not been unpacked, built or run.

**Development builds only.** No release build or app bundle carries this engine (or its libraries) yet: a
debug app finds the repository's `target/debug/meetko-qwen3-asr`, anything else needs `MEETKO_BATCH_ENGINE` or the
binary next to the app. In a release build without it the settings show the Qwen3-ASR rows as unavailable.

Why Qwen3-ASR and this runtime (checked 2026-11-10):

- Weights: Apache-2.0 (Qwen/Qwen3-ASR-1.7B, Qwen/Qwen3-ForcedAligner-0.6B, not gated). Korean is supported by
  both (the aligner's 11 languages include Korean).
- Runtime: Qwen ships Python only (transformers/vLLM). CrispASR (MIT, prebuilt for macOS arm64 and Windows x64) runs
  both models without Python. sherpa-onnx, already used here, runs Qwen3-ASR too, but only the 0.6B export exists
  and it gives no word times; llama.cpp runs Qwen3-ASR without an aligner.
- Times: Qwen3-ASR alone gives no timestamps. The forced aligner places each unit of the transcript (Korean per
  syllable, Latin script per word); the engine maps them back onto the transcript's own space-separated words, so
  `word_timing` is `aligned` and every word has a measured start and end.
- Model files: Qwen publishes no GGUF. The files are the q8_0 conversions by CrispASR's author on Hugging Face
  (`cstr/qwen3-asr-1.7b-GGUF`, `cstr/qwen3-forced-aligner-0.6b-GGUF`), pinned by commit and sha256 in `models.toml`.
  The app says so where it offers them. That is a third party between Qwen and the user: the pins make sure every
  user gets the same bytes, not that they equal Qwen's weights.

## Build and run

```
engines/qwen3-asr/fetch-libs.sh        # pinned CrispASR 0.8.53 (libraries, headers, license texts), into vendor/ (gitignored)
(cd engines/qwen3-asr && cargo build)  # the debug build is what a debug app finds by itself
export MEETKO_BATCH_MODEL=/path/to/model-dir   # holds the two .gguf files of models.toml; unset = the app's own
export MEETKO_BATCH_ENGINE=/path/to/meetko-qwen3-asr   # optional for a debug app
```

The model files are native-parser input and are handled like the other engines' (see `engines/nemotron-stream`):
the core copies both into a private 0700 directory while hashing them, refuses anything that does not match the
pin (unless `MEETKO_BATCH_MODEL_UNPINNED=1`, development only, recorded as `params.model_pinned = false`) and hands
the engine that copy; `model.sha256` in the result is the combined digest of what was staged. That is a 3.5 GB copy
once per transcription (seconds in a release build; minutes in an unoptimized debug build of the core).

Libraries: `fetch-libs.sh` verifies the archive, refuses absolute or parent paths in it, copies only regular files
(each under the install name the libraries load each other by, `libcrispasr.1.dylib`, `libggml*.0.dylib`), and
pins every copied library, the two headers `src/ffi.rs` follows and the license texts. Rpaths are as in the other
engines (`@executable_path/../Frameworks`, `../lib`; debug builds also `vendor/lib`). For distribution the upstream
dylibs (ad-hoc signed) must be re-signed with your Developer ID like the others.

## Behaviour

- Pieces: the audio is read and cut a piece at a time (at most 30 s in memory, whatever the recording's length),
  each cut at the quietest 20 ms in the last 4 s before the limit. A piece with less than 200 ms of 20 ms frames at
  or above −50 dBFS is silence (a click or a breath alone is not speech) and is not decoded, so the model cannot make
  up words there (LLM-style ASR can, on silence). Each piece gets a token budget of 12 per second (at least 64), so a
  decoder caught in a repetition stops; a piece that uses the whole budget is counted (`params.capped_pieces`; when
  the library reports no token counts for a piece, `params.token_counts_known` is false).
- Decoding is greedy (temperature 0). The language comes from the request: `auto` (or nothing) lets the model detect
  it, a tag gives its base language (`ko-KR` is `ko`). The core sends `auto` unless the meeting or the caller named a
  language (whisper keeps its `ko` default), because a forced language can make the model translate English terms
  instead of transcribing them. `params.language` is what ran, `params.language_requested` what was asked.
- Each piece's transcript is aligned with the aligner over the same piece (the aligner takes up to 5 minutes). The
  words are the transcript's own (its spacing and punctuation); each gets the times of the aligner units its letters
  fall in. A word the aligner wrote differently (a number spelled out, say) is looked for a little further on, so it
  costs that word its time, not the rest of the piece; such words, and every word of a piece the aligner could not
  place, are spread evenly between the timed words around them. Times stay inside their piece and never go
  backwards. `word_timing` is `aligned` only when every word was timed by the aligner; with any spread word the
  whole result says `none` (`params.spread_words`, `params.unaligned_pieces`).
- `params` records what ran: backend, language, temperature, the token budget, the cut settings, both model files
  and their sources (`cstr/…@<commit>`), GPU and threads, and the counts above. `network: false`. The core adds
  `model_pinned` and `sandbox` (`macos-sbpl`, `off` or `none`: how it ran the engine).
- No network, no other programs: CrispASR can fetch models with `curl`/`wget` for some of its other backends. This
  engine names both model files, so it does not take that path. As further lines: its first act is to set its own
  `PATH` to a directory that does not exist (an empty `PATH` would search the current directory); the core starts it
  in a private empty working directory; and on macOS the core runs it under `sandbox-exec` with
  `meetko_core::sandbox::engine_profile`. That profile is allow-by-default (Metal needs mach services, IOKit and
  system files that a deny-by-default profile would have to list) and takes away:
  - all network access (TCP, UDP, Unix sockets, name resolution), so `network: false` is enforced;
  - fork, and starting any program but the engine itself;
  - AppleEvents, and the LaunchServices services (`launchservicesd`, `lsd.open`, `lsd.mapdb`, `lsd.modifydb`), so it
    cannot open a URL in the browser or start an app that would run outside the sandbox;
  - writing anywhere but its private HOME, `/dev/null`, `/dev/tty` and the Metal shader caches
    (`<DARWIN_USER_CACHE_DIR>/com.apple.metal`, `com.apple.metalfe`, under /var/folders; without them every start
    compiles all kernels again, about 75 s instead of under 1 s);
  - reading the user's home and the meetko data dir, except its own binary and library folders, the staged model
    and the temp folder (where the core stages the model and each request's wav).

  What stays open: reading files outside those trees, and mach services other than LaunchServices (Metal's among
  them). Checked: a transcription under the profile completes and the second start uses the Metal cache;
  `crates/meetko-core/src/sandbox.rs` tests that another program, a read in the denied tree, a write outside the
  HOME and a LaunchServices query are refused. **Fail closed:** when the profile cannot be applied (no
  `sandbox-exec`, or a preflight run fails) the core does not run the engine and the app says so, unless
  `MEETKO_BATCH_SANDBOX=off` is set (development only; recorded as `params.sandbox = "off"`). On other systems
  (`params.sandbox = "none"`), and when the engine is run by hand, only the PATH guard and the working directory
  apply.
- Stdout carries protocol only: the engine keeps a close-on-exec duplicate of the original stdout for the protocol
  and points fd 1 at stderr, so whatever the library prints goes to the log.
- Speed (one machine, M-series Mac, Metal, debug engine build with the release library): 52 s of synthetic Korean
  speech in about 6 s including loading both models. Memory: the two q8_0 models are about 3.5 GB.

## Protocol

| message | direction | |
|---|---|---|
| `initialize {contract:["0.1"]}` | request | `{contract, engine:{id,version}, role:"asr.batch", model:{id}}` |
| `transcribe.run {wav, audio_sha256, language}` | request | a contract `asr.batch` engine result |
| `transcribe.progress {percent}` | notification, engine to core | after each piece |
| `engine.error {message}` | notification, engine to core | the library failed; the engine exits with code 1 next |
| `shutdown` | request | |

`wav` is a 16 kHz mono PCM16 file the caller has hashed (`audio_sha256` is echoed); anything else is refused with
`-32010`, as are missing model files. Bad parameters are `-32602`, an unknown method `-32601`; the process goes on.
A library failure or a line that is not UTF-8 ends it after one `engine.error` (exit code 1 through `_exit`, so the
ggml backends' static destructors do not run with a model loaded).

## Tests

```
cargo test                                   # unit tests and the protocol without models
MEETKO_REQUIRE_MODELS=1 MEETKO_BATCH_MODEL=/path/to/model-dir MEETKO_QWEN3_TEST_WAV=/path/to/speech.wav cargo test
```

Without models the stdio tests cover the protocol, the refused requests and the fatal path. With both variables
set, a real transcription is checked against the contract schema the core uses (word times in order and inside the
audio, Korean words, progress up to 100), and the core's `BatchProcess` runs the engine on the pinned model
(`model_pinned: true`). `crates/meetko-core/tests/batch.rs` covers the core side with a fake engine.

## Third-party software

This engine links CrispASR (MIT) and ggml (MIT). See `THIRD_PARTY_NOTICES.md`.
