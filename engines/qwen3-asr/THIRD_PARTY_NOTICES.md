<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Third-party notices

`meetko-qwen3-asr` links, at run time, prebuilt shared libraries that are not part of this repository.
`fetch-libs.sh` downloads the official release archive (pinned by sha256) into `vendor/`, together with the
license texts that ship in it, in `vendor/licenses/`. Anyone who redistributes the engine must ship those texts.

| component | version | license | source |
|---|---|---|---|
| CrispASR (`libcrispasr`) | 0.8.53 (C API 0.7.1) | MIT | https://github.com/CrispStrobe/CrispASR |
| ggml (`libggml`, `libggml-base`, `libggml-cpu`, `libggml-blas`, `libggml-metal`) | 0.26.0 | MIT | https://github.com/ggml-org/ggml |

`vendor/licenses/crispasr-LICENSE` is CrispASR's MIT license (copyright the ggml authors and the CrispASR
contributors). `vendor/licenses/crispasr-THIRD_PARTY_NOTICES.txt` lists what is built into `libcrispasr` with each
license text: ggml, llamafile sgemm, KleidiAI, glint, nlohmann/json (MIT); RNNoise, libogg, libopus, opusfile,
OpenBLAS (BSD-3-Clause); miniaudio (public domain / MIT-0); OmniVoice prompt vocabularies (Apache-2.0); and, for
other platforms' archives only, the GCC and LLVM OpenMP runtimes. All are compatible with this repository's
AGPL-3.0-or-later. The same file also describes models CrispASR can fetch for its other backends; this engine uses
none of them (it names its two model files, sets PATH to a directory that does not exist and, on macOS, runs
under a sandbox profile without network access or other programs; see the README).

The models are not part of the library or of this repository: Qwen3-ASR-1.7B and Qwen3-ForcedAligner-0.6B
(Apache-2.0, Qwen), as the q8_0 GGUF conversions `cstr/qwen3-asr-1.7b-GGUF` and
`cstr/qwen3-forced-aligner-0.6b-GGUF` (pinned in `models.toml`). Checked 2026-11-10 at the pinned revisions: both
`cstr/*` model cards state `license: apache-2.0`, name the Qwen repository as `base_model` and say they redistribute
under the upstream terms. The Qwen Hugging Face repositories state Apache-2.0 in their card metadata and hold no
LICENSE or NOTICE file; Qwen's code repository (QwenLM/Qwen3-ASR) has an Apache-2.0 LICENSE and no NOTICE file. So
there is no NOTICE text to carry; attribution is the model names and the license, as in the app's settings.
