<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# 기여 안내

meetko는 아직 초기 단계입니다. 이슈와 PR을 환영하지만, 방향이 맞는지 큰 변경은 먼저 이슈로 이야기해 주세요.

## 빌드와 테스트

CI(`.github/workflows/ci.yml`)가 돌리는 것과 같은 명령입니다.

```
# 프런트엔드: 앱 크레이트를 컴파일하기 전에 먼저 빌드해야 합니다
(cd app && npm ci && npm run build)

# 플러그인 예제를 먼저 빌드합니다 (호스트 적합성 테스트가 이 바이너리를 씁니다)
(cd plugins/talk-time-rs && cargo build --release --locked)
(cd plugins/talk-time-rs && cargo clippy --locked --all-targets -- -D warnings)

# 파이썬 참조 구현과 계약 테스트
python -m pip install -r requirements-dev.txt
python -m pytest tests
python -I tools/plugin-index/test_build_index.py   # Python 3.11 이상

# Rust 워크스페이스 (core, cli)
cargo test --workspace --locked
cargo clippy --workspace --locked --all-targets -- -D warnings

# 데스크톱 앱 크레이트 (워크스페이스 밖)
(cd app/src-tauri && cargo test --locked && cargo clippy --locked --all-targets -- -D warnings)
```

엔진은 워크스페이스 밖의 별도 크레이트이고 외부 라이브러리가 필요해서 따로 테스트합니다(macOS만).

```
engines/sherpa-stream/fetch-libs.sh   && (cd engines/sherpa-stream   && cargo test --locked)
engines/nemotron-stream/fetch-libs.sh && (cd engines/nemotron-stream && cargo test --locked)
(cd capture/macos && ./run-tests.sh)
```

테스트와 빌드에만 쓰는 환경변수:

- `MEETKO_REQUIRE_MODELS=1`: 모델이 없으면 모델이 필요한 테스트는 건너뜁니다. 이 값을 주면 건너뛰지 않고 실패합니다(엔진 테스트).
- `MEETKO_REQUIRE_PLUGINS=1`: `plugins/talk-time-rs`가 빌드되지 않았을 때 호스트 적합성 테스트를 건너뛰지 않고 실패합니다. CI가 설정합니다.
- `MEETKO_ENGINE_DEV_RPATH=1`: 릴리스 빌드 엔진도 저장소의 `vendor/` 라이브러리를 찾게 합니다(엔진의 `--release` 테스트용, 배포용 빌드에는 쓰지 않음).
- `MEETKO_NEMO_BACKEND=vulkan`: `engines/nemotron-stream/fetch-libs.sh`가 Windows Vulkan 빌드를 받습니다(기본 `cpu`).
- `MEETKO_TEST_PACED=1`: nemotron 엔진의 `stdio` 테스트가 실제 시간 속도로 소리를 넣어 지연을 잽니다.
- `MEETKO_DIAR_MULTI_WAV=<wav 경로>`: nemotron 화자 분리 테스트에 쓸, 여러 사람이 말하는 녹음 파일.

자세한 쓰임은 [engines/nemotron-stream/README.md](engines/nemotron-stream/README.md)와 [engines/sherpa-stream/README.md](engines/sherpa-stream/README.md)에 있습니다. `cargo fmt --check`는 아직 CI에서 돌리지 않습니다.

테스트는 인터넷에 연결하지 않습니다. 모델 내려받기 테스트(`crates/meetko-core/tests/model_setup.rs`)는 `127.0.0.1`에 띄운 테스트 서버와 작은 파일만 씁니다. 회의 알림의 CoreAudio 조회 테스트는 macOS에서만 돌고, 아무것도 녹음하지 않습니다. 검색 성능 측정은 기본으로 건너뛰며(`#[ignore]`), 직접 돌리려면 `cargo test --release -p meetko-core --test search bench -- --ignored --nocapture`입니다.

## 계약을 바꿀 때

`contract/schemas/`, `docs/rfc-0001-contract.md`, `docs/capture-interface.md`의 변경은 코드보다 먼저 RFC 형식의 논의(이슈)를 거칩니다. 무엇이 왜 바뀌는지, 기존 엔진·플러그인이 어떻게 되는지를 적어 주세요. 스키마를 바꾸면 참조 구현과 테스트도 함께 바꿉니다.

## 커밋에 넣지 말 것

- 오디오 녹음, 모델 파일, 개인 데이터(실제 회의 전사본, 이름, 이메일, 토큰, 내 컴퓨터의 절대 경로)는 커밋하지 마세요. 저장소의 `raw/`, `models/`는 git에서 제외되어 있습니다.
- 테스트 데이터는 공개해도 되는 것만 씁니다(`contract/fixtures/`의 출처는 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) 참고).

## 기타

- 기여한 코드는 AGPL-3.0-or-later로 배포됩니다. DCO 서명(`Signed-off-by`)은 요구하지 않습니다.
- `SPDX-License-Identifier` 줄은 일부 파일(주요 문서와 `capture/macos`의 Swift 파일 등)에만 있습니다. 줄이 없는 파일도 따로 표시가 없으면 AGPL-3.0-or-later입니다.
- 플러그인은 각자의 저장소에 공개하는 것을 권장합니다. 플러그인을 올리는 방법은 [docs/plugin-publishing.md](docs/plugin-publishing.md)를 보세요.
- PR에는 무엇을, 왜 바꿨는지와 실행한 테스트를 적어 주세요.

## English (brief)

Open an issue before large changes. Build and test with the commands above (the same as CI): build the frontend first (`npm ci && npm run build` in `app/`), build `plugins/talk-time-rs` in release mode, then `python -m pytest tests`, `cargo test --workspace --locked`, and the app crate tests. The engine crates and the Swift capture sidecar are tested separately and on macOS only. Contract changes (`contract/schemas/`, the RFC, the capture interface) need an RFC-style discussion first. Do not commit audio, model files or personal data. No DCO sign-off is required; contributions are licensed AGPL-3.0-or-later. `SPDX-License-Identifier` lines are present only in some files (main docs, the Swift capture sources and a few others); files without one are AGPL-3.0-or-later unless marked otherwise. Test/build-only environment variables (`MEETKO_REQUIRE_MODELS`, `MEETKO_REQUIRE_PLUGINS`, `MEETKO_ENGINE_DEV_RPATH`, `MEETKO_NEMO_BACKEND`, `MEETKO_TEST_PACED`, `MEETKO_DIAR_MULTI_WAV`) are listed in the Korean section above and in the engine READMEs. Plugins are best published in their own repositories; see `docs/plugin-publishing.md` (GitHub topic `meetko-plugin`, installed with `meetko plugin install github:OWNER/REPO@REF`). Plugin runs are sandboxed on macOS only (`docs/plugin-security.md`); on Windows and Linux they run as your user, so review plugin code accordingly. Gallery index tests: `python -I tools/plugin-index/test_build_index.py`.
