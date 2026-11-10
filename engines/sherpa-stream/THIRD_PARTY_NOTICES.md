<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Third-party notices

`meetko-sherpa-stream` links, at run time, prebuilt shared libraries that are not part of this
repository. `fetch-libs.sh` downloads them (pinned by sha256) into `vendor/`, together with their
license texts in `vendor/licenses/`. Anyone who redistributes the engine must ship those texts.

| component | version | license | source |
|---|---|---|---|
| sherpa-onnx (C API, `libsherpa-onnx-c-api`, `libsherpa-onnx-cxx-api`) | 2.0.1 | Apache-2.0 | https://github.com/k2-fsa/sherpa-onnx |
| ONNX Runtime (`libonnxruntime`, bundled in the sherpa-onnx release) | 1.28.2 | MIT | https://github.com/microsoft/onnxruntime |

ONNX Runtime's own third-party notices (`ThirdPartyNotices.txt` of v1.28.2) apply to the library
and are fetched into `vendor/licenses/onnxruntime-ThirdPartyNotices.txt`.
The sherpa-onnx project publishes no separate NOTICE file for this release.

The streaming model (a Korean zipformer from the sherpa-onnx model zoo) is a separate download with
its own license; it is neither included here nor redistributed by this repository.
