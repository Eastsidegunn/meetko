<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Third-party notices

`meetko-nemotron-stream` and `meetko-nemotron-diar` link, at run time, prebuilt shared libraries that are not part of this
repository. `fetch-libs.sh` downloads the official release archive (pinned by sha256) into `vendor/`,
together with the license texts that ship in it, in `vendor/licenses/`. Anyone who redistributes the
engine must ship those texts.

| component | version | license | source |
|---|---|---|---|
| NeMo-Speech.cpp (`libnemo_speech_asr`, `libnemo_speech_asr_c`, `libnemo_speech_text_normalization`) | 0.2.0 | Apache-2.0 | https://github.com/NVIDIA/NeMo-Speech.cpp |
| ggml (`libggml`, `libggml-base`, `libggml-cpu`, `libggml-blas`, `libggml-metal`) | 0.25.1 | MIT | https://github.com/ggml-org/llama.cpp (`ggml/`) |

`fetch-libs.sh` copies the archive's complete `share/licenses/nemo-speech/` tree to
`vendor/licenses/nemo-speech/`, and that is what must ship with the libraries:

- `LICENSE` (Apache-2.0) and `NOTICE` of NeMo-Speech.cpp, and `THIRD_PARTY_NOTICES.md`, which says what
  is built into the libraries;
- `third_party/`: ggml and llama.cpp (MIT, copyright The ggml authors), SentencePiece with its Abseil,
  darts-clone and protobuf-lite texts, OpenFst, re2, protobuf, Sparrowhawk (text normalization),
  miniaudio and cpp-httplib (macOS archive; the Linux archive adds `gcc-runtime/copyright`, the
  Windows archives list the vcpkg builds of Abseil, protobuf, SentencePiece and utf8-range instead of
  OpenFst, re2 and Sparrowhawk).

The archive also contains `libllama`, `nemo-speech` (CLI/server), the NMT and TTS libraries; this
engine uses none of them and `fetch-libs.sh` does not copy those files, but their license texts come
along in the tree. Only the libraries listed in the table above are loaded.

The Windows archives additionally carry the Microsoft Visual C++ runtime DLLs and the Linux archive
the GCC runtime libraries (`libstdc++`, `libgcc_s`, `libgomp`, `libatomic`), which `fetch-libs.sh`
copies for those platforms: their licenses are those of the Visual C++ Redistributable and of the GCC
runtime library exception. Neither platform has been exercised yet.

The streaming model, `nvidia/nemotron-3.5-asr-streaming-0.6b` (quantized GGUF
`nemotron-3.5-asr-streaming-0.6b.q8_0.gguf`), is a separate download under the OpenMDW-1.1 license
(https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b); it is neither included here nor
redistributed by this repository. Review its terms before shipping it with the app.

The diarization model, `nvidia/Nemotron-3-Diarization` (the quantized GGUF `Nemotron-3-Diarization.q8_0.gguf` that
NVIDIA publishes in that repository, revision `f667ed73aee57d40cc39428eb768b4fd87a0a29e`), is
likewise a separate download under the OpenMDW-1.1 license (https://huggingface.co/nvidia/Nemotron-3-Diarization);
it is neither included nor redistributed here. Review its terms before shipping it with the app. `fetch-libs.sh`
also keeps the release's `diar.h` in `vendor/`, the header `src/diar.rs` is written against.
