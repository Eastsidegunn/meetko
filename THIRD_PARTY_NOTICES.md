<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Third-party notices

meetko itself is AGPL-3.0-or-later (see `LICENSE`). This file lists the third-party software and data that
this repository uses, fetches or refers to. The per-engine files are authoritative for their libraries:

- [engines/nemotron-stream/THIRD_PARTY_NOTICES.md](engines/nemotron-stream/THIRD_PARTY_NOTICES.md)
- [engines/sherpa-stream/THIRD_PARTY_NOTICES.md](engines/sherpa-stream/THIRD_PARTY_NOTICES.md)

## Code adapted into this repository

`crates/meetko-core/src/whisper_prompt.rs` ports whisper.cpp's tokenizer (the pre-split pattern and the
longest-match loop of `tokenize`) and its model-header layout to Rust, to count a prompt's tokens exactly.

```
MIT License

Copyright (c) 2023-2026 The ggml authors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Software fetched or run next to meetko

None of these is committed to this repository. The engines' `fetch-libs.sh` scripts download pinned,
sha256-checked archives into `vendor/` together with the upstream license texts; anyone who redistributes
a build must ship those texts.

| component | license | source | used by |
|---|---|---|---|
| whisper.cpp (`whisper-cli`, run as an external executable for batch transcription) | MIT | https://github.com/ggml-org/whisper.cpp | `crates/meetko-core` |
| NVIDIA NeMo-Speech.cpp 0.2.0 | Apache-2.0 | https://github.com/NVIDIA/NeMo-Speech.cpp | `engines/nemotron-stream` |
| ggml 0.25.1 (inside the NeMo-Speech.cpp archive) | MIT | https://github.com/ggml-org/llama.cpp (`ggml/`) | `engines/nemotron-stream` |
| sherpa-onnx 2.0.1 | Apache-2.0 | https://github.com/k2-fsa/sherpa-onnx | `engines/sherpa-stream` |
| ONNX Runtime 1.28.2 (bundled in the sherpa-onnx release) | MIT | https://github.com/microsoft/onnxruntime | `engines/sherpa-stream` |
| CrispASR 0.8.53 (`libcrispasr`, prebuilt release; it carries its own notices for ggml, llamafile sgemm, KleidiAI, glint, nlohmann/json, RNNoise, libogg/libopus/opusfile, miniaudio and others in `THIRD_PARTY_NOTICES.txt`, which `fetch-libs.sh` keeps) | MIT | https://github.com/CrispStrobe/CrispASR | `engines/qwen3-asr` (experimental) |
| ggml 0.26.0 (inside the CrispASR archive) | MIT | https://github.com/ggml-org/ggml | `engines/qwen3-asr` |

The NeMo-Speech.cpp archive also carries texts for SentencePiece, Abseil, protobuf, OpenFst, re2, Sparrowhawk
and others; the Windows and Linux archives add the Visual C++ and GCC runtime libraries. See the nemotron
notices for the list.

## Models (not redistributed)

Model files are not part of this repository and meetko does not redistribute them. Each is governed by its own
license; check it before use or redistribution. Pinned hashes are in `engines/*/models.toml`.

### Downloaded by the app on request

When the user clicks 내려받기 (Settings → 받아쓰기 준비), the app fetches these files from the publisher's Hugging
Face repository at a fixed commit and keeps them only when size and sha256 match the pin
(`crates/meetko-core/src/model_setup.rs`, `docs/model-setup.md`). The app shows the license and source before the
download. Both are licensed under OpenMDW-1.1 (full text below). **Anyone who distributes these files (for example
in an installer, an image or a mirror) must include a copy of the OpenMDW-1.1 text and keep the copyright and origin
notices that come with the models.**

| model | repository and revision | file (size, sha256) | license |
|---|---|---|---|
| NVIDIA Nemotron 3.5 ASR Streaming 0.6B, q8_0 GGUF (live captions) | https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b at `ea30d66debe3740a08b573244286791d423d6b3e` | `nemotron-3.5-asr-streaming-0.6b.q8_0.gguf` (742,090,464 bytes, `3fc991d3badad7277c11030a7519832cddaf2057aafed6d4b25147e953a070b1`), stored by meetko as `model.q8_0.gguf` | OpenMDW-1.1 |
| NVIDIA Nemotron 3 Diarization, q8_0 GGUF (speaker diarization) | https://huggingface.co/nvidia/Nemotron-3-Diarization at `f667ed73aee57d40cc39428eb768b4fd87a0a29e` | `Nemotron-3-Diarization.q8_0.gguf` (107,012,128 bytes, `08456d9e22cd9a323c0364d98375f3746d6e68507ebb705cd46438c534c7a3a1`) | OpenMDW-1.1 |

### Downloaded by the user

| model | license | source |
|---|---|---|
| whisper ggml model (e.g. `ggml-large-v3-turbo.bin`), optional, for 다시 받아쓰기 | MIT (Hugging Face `ggerganov/whisper.cpp`; OpenAI Whisper weights are MIT) | https://huggingface.co/ggerganov/whisper.cpp |
| Qwen3-ASR-1.7B, as the q8_0 GGUF conversion `cstr/qwen3-asr-1.7b-GGUF` (revision `674df5d44b50a63e7102a18895ed20e3f91de301`) of `Qwen/Qwen3-ASR-1.7B` | Apache-2.0 (Qwen; the conversion keeps it) | https://huggingface.co/Qwen/Qwen3-ASR-1.7B, https://huggingface.co/cstr/qwen3-asr-1.7b-GGUF |
| Qwen3-ForcedAligner-0.6B, as the q8_0 GGUF conversion `cstr/qwen3-forced-aligner-0.6b-GGUF` (revision `1ec5110602ccab18c878ddebedab0891e290a95c`) of `Qwen/Qwen3-ForcedAligner-0.6B` | Apache-2.0 | https://huggingface.co/Qwen/Qwen3-ForcedAligner-0.6B, https://huggingface.co/cstr/qwen3-forced-aligner-0.6b-GGUF |
| sherpa-onnx streaming zipformer Korean (`sherpa-onnx-streaming-zipformer-korean-2024-06-16`), legacy engine, not offered by the app | no license stated on the model card (checked 2026-10-31); converted from an icefall model whose name indicates KsponSpeech training data, whose terms may restrict use. **Research/testing only** | https://github.com/k2-fsa/sherpa-onnx (model zoo) |

### OpenMDW License Agreement, version 1.1

<!-- Fetched from https://raw.githubusercontent.com/OpenMDW/OpenMDW/main/1.1/LICENSE.OpenMDW-1.1 on 2026-11-10;
     identical to the file at commit b26b32b34ad2edcc29a7707abb68dcfb25a538c1 of https://github.com/OpenMDW/OpenMDW
     (the file was last changed in commit 9067ba54b2f84bc9e15a05734f570ec88e9f445d). Reproduced verbatim. -->

```
OpenMDW License Agreement, version 1.1 (OpenMDW-1.1)

By exercising rights granted to you under this agreement, you accept and agree
to its terms.

As used in this agreement, "Model Materials" means the materials provided to
you under this agreement, consisting of: (1) one or more machine learning
models (including architecture and parameters); and (2) all related artifacts
(including associated data, documentation and software) that are provided to
you hereunder.

Subject to your compliance with this agreement, permission is hereby granted,
free of charge, to deal in the Model Materials without restriction, including
under all copyright, patent, database, and trade secret rights included or
embodied therein.

If you distribute any portion of the Model Materials, you shall retain in your
distribution (1) a copy of this agreement, and (2) all copyright notices and
other notices of origin included in the Model Materials that are applicable to
your distribution.

If you file, maintain, or voluntarily participate in a lawsuit against any
person or entity asserting that the Model Materials directly or indirectly
infringe any patent or copyright, then all rights and grants made to you
hereunder are terminated, unless that lawsuit was in response to a
corresponding lawsuit first brought against you.

This agreement does not impose any restrictions or obligations with respect to
any use, modification, or sharing of any outputs generated by using the Model
Materials.

THE MODEL MATERIALS ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE, TITLE, NONINFRINGEMENT, ACCURACY, OR THE
ABSENCE OF LATENT OR OTHER DEFECTS OR ERRORS, WHETHER OR NOT DISCOVERABLE, ALL
TO THE GREATEST EXTENT PERMISSIBLE UNDER APPLICABLE LAW.

YOU ARE SOLELY RESPONSIBLE FOR (1) CLEARING RIGHTS OF OTHER PERSONS THAT MAY
APPLY TO THE MODEL MATERIALS OR ANY USE THEREOF, INCLUDING WITHOUT LIMITATION
ANY PERSON'S COPYRIGHTS OR OTHER RIGHTS INCLUDED OR EMBODIED IN THE MODEL
MATERIALS; (2) OBTAINING ANY NECESSARY CONSENTS, PERMISSIONS OR OTHER RIGHTS
REQUIRED FOR ANY USE OF THE MODEL MATERIALS; OR (3) PERFORMING ANY DUE
DILIGENCE OR UNDERTAKING ANY OTHER INVESTIGATIONS INTO THE MODEL MATERIALS OR
ANYTHING INCORPORATED OR EMBODIED THEREIN.

IN NO EVENT SHALL THE PROVIDERS OF THE MODEL MATERIALS BE LIABLE FOR ANY CLAIM,
DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE MODEL MATERIALS, THE
USE THEREOF OR OTHER DEALINGS THEREIN.
```

## Used by `tools/` to produce fixtures (not shipped)

Run by hand with Python (`sherpa-onnx` and `soundfile` packages, deliberately not in `requirements-dev.txt`, which is
for the test suite). The model files are downloaded by the person running the scripts and are not part of meetko.

| component | license | used by |
|---|---|---|
| pyannote segmentation 3.0 (sherpa-onnx ONNX export) | see upstream | `tools/run_sherpa_diar.py` |
| 3D-Speaker ERes2Net (`3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx`) | see upstream | `tools/run_sherpa_diar.py` |
| Silero VAD (`silero_vad.onnx`) | see upstream | `tools/run_vad.py` |
| `sherpa-onnx`, `soundfile` (Python packages) | see upstream | `tools/run_sherpa_diar.py`, `tools/run_vad.py`, `tools/run_sherpa_stream.py` |

## Data

`contract/fixtures/meeting01/` contains transcripts and engine outputs derived from a test recording
generated by `tools/make_fixture.py`. The microphone-side utterance texts come from the HiKE dataset
(https://github.com/thetaone-ai/HiKE, Apache-2.0, thetaone-ai; full license text in
[LICENSES/Apache-2.0.txt](LICENSES/Apache-2.0.txt)). The texts are excerpted: only the utterances used in the
recording are included, and each is marked with its HiKE `source` id in `reference.json`. The other side of the recording is text-to-speech of scripts written for
this project. The audio itself is not committed.

## Rust and npm dependencies

The Rust crates (`Cargo.lock` files in the workspace, `app/src-tauri/`, `engines/*/`, `plugins/talk-time-rs/`)
and the npm packages of `app/` (`app/package-lock.json`) are used under their own licenses. They are not
listed here; the lockfiles name every package and version. To produce a full license report for a
distributable build, run for example `cargo about` per crate and a license checker over the npm lockfile.

The core compiles SQLite into the binary (`rusqlite` with `bundled`, through `libsqlite3-sys`); SQLite is in the
public domain (https://sqlite.org/copyright.html). Full-text search uses SQLite's built-in FTS5 module, part of the
same source. Meeting detection calls macOS CoreAudio through its C API and adds no package.
