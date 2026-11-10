<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# meetko-sherpa-stream

The legacy `asr.stream` engine for live captions (the default is now `engines/nemotron-stream`): a native process around the sherpa-onnx online
transducer (C API, Apache-2.0). The core starts one per channel while a capture runs and talks to it
in NDJSON JSON-RPC 2.0 over stdio, like a plugin (`crates/meetko-core/src/live.rs`).

Not a member of the root workspace: it links prebuilt shared libraries, which a plain
`cargo test --workspace` must not need.

## Build and run

```
engines/sherpa-stream/fetch-libs.sh          # pinned sherpa-onnx 2.0.1, sha256-checked, into vendor/ (gitignored)
(cd engines/sherpa-stream && cargo build)    # the debug build is what a debug app finds by itself
export MEETKO_STREAM_MODEL=/path/to/sherpa-onnx-streaming-zipformer-korean-2024-06-16
export MEETKO_STREAM_ENGINE=/path/to/meetko-sherpa-stream     # optional for a debug app
```

The model directory needs `encoder-epoch-99-avg-1.int8.onnx`, `decoder-epoch-99-avg-1.onnx`,
`joiner-epoch-99-avg-1.int8.onnx` and `tokens.txt`.

### The model is native-parser input

onnxruntime parses the model files, so the core does not load whatever the directory holds:

- `models.toml` pins the sha256 of every file of the known model. A directory that does not match is
  refused, unless `MEETKO_STREAM_MODEL_UNPINNED=1` is set (development only). The stored live
  version then says `params.model_pinned = false`.
- The core copies the four files into a private 0700 directory **while hashing them** and the engine
  loads that copy, so there is no check-then-load race and a later change of the source does not matter.
  It costs a ~140 MB copy per channel at the start of a recording (well under a second, done off the
  recording's path); a hard link or clone would share the inode with a source that others can write.
- The combined digest (sha256 over `"<file>:<sha256>\n"` lines sorted by file name) is stored as
  `model.sha256` in the live producer.
- Keep the model out of the plugin-writable data dir: a plugin that can write the model's directory
  can still change what a *later* run stages. Pins catch it, but do not rely on that alone.

### Libraries, rpath, distribution

`fetch-libs.sh` verifies the archive, refuses unsafe tar members, copies only regular files, checks
each dylib against its pinned hash (also when it is already installed), and fetches the license texts
into `vendor/licenses/`.

The binary finds `libsherpa-onnx-c-api.dylib` and `libonnxruntime.dylib` through relative rpaths only.
Release builds get `@executable_path/../Frameworks` and `@executable_path/../lib`. Debug builds (or
`MEETKO_ENGINE_DEV_RPATH=1`) also get the repository-relative `@executable_path/../../vendor/lib`.

For distribution: the upstream dylibs are not signed with your identity. Re-sign them (and the engine)
with your Developer ID under the hardened runtime. With library validation on (the default) every
loaded dylib must carry the same team id, so the re-signing is required, not optional; do not turn
library validation off to avoid it. Notarize the bundle afterwards.

The hand-written FFI in `src/ffi.rs` is checked against `c-api.h` of the pinned version (a layout test);
a different library version needs that file checked again.

## Results and their limits

Against the Python experiment on a 69 s recording, every word start matched to the millisecond, the
text differed by 1 character of 107, and the Rust engine closed one extra segment. The vendored
onnxruntime differs numerically from the pip wheel's build, so endpointing at a borderline "." can
flip; exact parity is not claimed.

## Protocol

| message | direction | |
|---|---|---|
| `initialize {contract:["0.1"]}` | request | `{contract, engine:{id,version}, role:"asr.stream", model:{id}}` |
| `stream.open {sample_rate:16000, language}` | request | loads the model; `{params:{...}}` records the decode settings |
| `stream.push {pcm_s16le_b64}` | notification | 16 kHz mono PCM16, 100 ms per push |
| `stream.event` | notification, engine to core | params are contract `asr_stream_event` objects |
| `stream.error {message}` | notification, engine to core | the engine cannot continue (bad input); it exits next |
| `stream.close` | request | flush; the remaining final event(s) arrive before the reply |
| `shutdown` | request | |

`emitted_at_ms` is the audio fed so far, so `emitted_at_ms - unit.start_ms` is the latency. The engine
never drops a chunk silently: invalid PCM, a push without an open stream, or a non-UTF-8 line ends it
with `stream.error` (so does a line that is not JSON while a stream is open), because a dropped chunk would shift every later timestamp.

The core bounds what an engine can make it hold: 10 000 segments, 2 000 units per event, 64 MB folded,
and more than 200 events per second for 2 s in a row counts as a failed engine.

## Third-party software

This engine links sherpa-onnx (Apache-2.0) and ONNX Runtime (MIT) at run time. See
`THIRD_PARTY_NOTICES.md`; the license texts are fetched into `vendor/licenses/` and must ship with any
redistribution.
