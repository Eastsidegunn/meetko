<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# meetko

한국어·영어 회의 전사본을 내 컴퓨터 안에서 만드는 로컬 우선 데스크톱 앱입니다. 녹음, 전사, 저장, 플러그인 실행이 모두 사용자 기기에서 일어납니다. 중심 계약은 **단어 단위 타임스탬프**입니다. 엔진과 모델이 만든 회의 데이터는 단어마다 시작·끝 시각(ms)과 화자를 갖고, 코어는 이를 버전으로 저장·승인하며, 플러그인은 그 데이터를 받아 화면에 보여줄 값을 돌려주는 함수입니다. 모델, 엔진, 플러그인은 모두 계약으로 붙고 서로 교체할 수 있습니다. 대상 사용자는 한국어를 쓰는 개발자와 기술 팀입니다.

라이선스: AGPL-3.0-or-later. Copyright (C) 2026 Eastsidegunn

## 할 수 있는 것

- **녹음**: 내 목소리(마이크)와 상대 목소리(시스템 소리)를 따로 녹음합니다(macOS).
- **실시간 자막이 곧 초안**: 녹음하는 동안 자막이 나오고, 녹음이 끝나면 그 자막이 바로 검토할 수 있는 초안이 됩니다. 화자 구분 모델이 있으면 화자도 나눕니다.
- **더 정확한 초안으로 자동 다시 받아쓰기(선택)**: 실시간 자막은 화면에 보여 주는 용도이고, 녹음이 끝나 실시간 자막 초안이 만들어지면 이어서 준비된 일괄 전사 엔진 중 가장 정확한 것(Qwen3-ASR이 준비되어 있으면 Qwen3-ASR, 아니면 Whisper)으로 녹음 전체를 다시 받아 적어 새 초안을 만듭니다. 그동안 실시간 자막 초안을 읽을 수 있고, 끝나면 화면이 새 초안으로 조용히 바뀝니다(사용자가 직접 고른 버전을 보고 있으면 "새 초안이 준비됐어요 · 보기"만 띄웁니다). 이미 승인한 버전이 있으면 새 초안은 승인해야 기준이 됩니다. 실시간 세션이 같은 오디오로 화자를 나눠 두었다면 그 화자 구분을 그대로 쓰고(출처에 실시간 세션이 만든 것으로 남습니다), 아니면 지금처럼 다시 나눕니다. 설정 › 받아쓰기 준비에서 엔진(자동/Whisper/Qwen3-ASR)과 "녹음이 끝나면 자동으로 다시 받아쓰기"(기본 켜짐)를 바꿀 수 있고, 회의 화면의 "다시 받아쓰기"로 언제든 직접 할 수 있습니다. 엔진이 하나도 없으면 실시간 자막 초안만 남고 앱은 그대로 동작합니다. 사내 비교(한국어·영어 회의)에서 실시간 자막 CER은 38.9%, Whisper large-v3-turbo는 17.6%, Qwen3-ASR은 16.7%였고, Qwen3-ASR은 10분 회의에 약 2.5–3분 걸렸습니다. 명령줄 `meetko`에는 실시간 자막이 없어서 이 단계도 없습니다(`meetko transcribe`로 직접 받아 씁니다).
- **검토와 승인**: 단어마다 시각이 있어 그 위치의 소리를 바로 들을 수 있고, 고친 결과는 새 버전으로 저장됩니다.
- **화자 이름과 문장 고치기**: "화자 2"를 실제 이름으로 바꾸거나 두 화자를 한 사람으로 합칠 수 있고, 잘못 받아 적힌 문장을 그 자리에서 고칠 수 있습니다. 원래 받아쓴 내용은 그대로 두고 위에 덧씌우는 방식이라 언제든 "원래대로" 되돌릴 수 있고, 내보내기·검색·AI 에이전트 연결(MCP)에도 반영됩니다. 다시 받아써서 새 전사본이 생기면 이전 이름과 고친 문장은 옮기지 않고 이전 버전에 남겨 둡니다.
- **용어 사전**: 설정 › 용어 사전에 자주 나오는 이름·제품명·영어 용어를 적어 두면 Whisper로 다시 받아쓸 때 힌트로 넘깁니다(Qwen3-ASR과 실시간 자막에는 아직 쓰지 않습니다). "찾아 바꾸기" 목록은 위에서부터 한 번씩 화면·내보내기에 적용되고, 저장된 원본은 바뀌지 않습니다. JSON으로 가져오기·내보내기를 할 수 있습니다.
- **내보내기**: 회의 화면의 "내보내기"에서 Markdown(.md), 텍스트(.txt), SRT, WebVTT 파일로 저장하거나 여러 형태로 복사합니다(Markdown, 텍스트만, 시각·화자 포함, 화자 포함).
- **검색**: 회의 목록에서 모든 회의의 내용을 찾습니다(승인본, 없으면 초안, 없으면 실시간 자막). 명령줄에서는 `meetko search 검색어`.
- **회의 알림(macOS, 기본으로 켜짐)**: 회의 앱이 마이크를 쓰기 시작하면 "녹음할까요?"라고 묻습니다. 아래 "회의 알림"을 보세요.
- **플러그인**: 전사본을 받아 요약, 액션아이템, 발화 시간 같은 값을 돌려주는 별도 프로그램입니다.

## 현재 상태와 한계

- 릴리스 전(pre-release) 단계입니다. 버전은 0.1.0이고 계약, 매니페스트, 화면은 바뀔 수 있습니다. 아직 내려받아 바로 설치하는 앱 파일은 없고 소스에서 빌드해야 합니다. 실시간 자막·화자 구분 엔진은 앱에 들어 있지 않아서 따로 빌드합니다(아래 "처음 쓰기").
- **macOS 15 이상(Apple Silicon)에서만** 끝까지 확인했습니다.
- **Windows**: CI 설정(프런트엔드 빌드, 파이썬 참조 테스트, 워크스페이스와 앱 크레이트의 테스트·clippy)은 들어 있지만 지금은 꺼 두었습니다. 하지만 녹음 프로그램(capture)과 회의 알림이 없고, 실제로 써 보지 않았습니다.
- **Linux**: 빌드도 테스트도 하지 않았습니다.
- 플러그인 격리는 macOS에만 있습니다(OS 샌드박스). Windows·Linux에서는 아직 없고 플러그인이 내 계정 권한으로 실행됩니다. 아래 "플러그인"을 읽어 주세요.
- 알려진 문제(예: 스피커 소리가 마이크로 들어가 같은 말이 두 번 전사됨)는 [docs/known-issues.md](docs/known-issues.md)에 있습니다.

## 인터넷 연결

녹음, 전사, 검색, 저장은 모두 기기 안에서 합니다. 앱이 인터넷에 연결하는 것은 사용자가 아래 버튼을 누를 때뿐입니다. 백그라운드 업데이트 확인이나 사용 통계 전송은 없습니다.

- 설정 › 받아쓰기 준비(또는 회의 목록의 안내)에서 모델 **내려받기**: Hugging Face(`huggingface.co`, 파일은 `*.hf.co`에서 옴)
- 플러그인 화면의 **둘러보기**(`목록 불러오기`)와 **설치**: GitHub(`raw.githubusercontent.com`, `api.github.com`, `codeload.github.com`)
- 명령줄 `meetko plugin info|install github:...`: GitHub

플러그인이 네트워크를 쓰려면 매니페스트에 선언하고 설치할 때 동의를 받아야 합니다(macOS에서만 강제됩니다. 아래 "플러그인").

## 구조

```
 capture/macos  (Swift 사이드카: 마이크 + 시스템 오디오 -> mic.wav, loopback.wav, capture.json)
        |
        v
 crates/meetko-core  -- 저장, 버전, 승인, 검색, 내보내기, 모델 받기, 플러그인 호스트
   |        |    \
   |        |     crates/meetko-cli   (명령줄)
   |        app/  (Tauri v2 + TypeScript UI, app/src-tauri)
   |
   +-- engines/nemotron-stream   실시간 자막(asr.stream) + 화자 분리(diarization), NDJSON JSON-RPC (stdio)
   +-- engines/sherpa-stream     실시간 자막(asr.stream), 같은 프로토콜 (예전 엔진)
   +-- whisper.cpp               다시 받아쓰기용 일괄 전사 (외부 실행 파일 whisper-cli, 선택)
   +-- engines/qwen3-asr         다시 받아쓰기(asr.batch, 실험 기능): Qwen3-ASR + 단어 시각 정렬, NDJSON JSON-RPC (stdio)
   +-- plugins/                  예제: action-items-py, talk-time-rs, summary-py(API 키)
 contract/ + docs/rfc-0001-contract.md   스키마와 계약 설명
 reference/                              파이썬 참조 구현 (tests/가 계약을 검증)
```

각 부분은 별도 프로세스이고 프로토콜로만 연결됩니다. 자세한 원칙은 [docs/manifest-draft.md](docs/manifest-draft.md)와 [docs/capture-interface.md](docs/capture-interface.md)를 보세요.

## 소스에서 빌드

필요한 것: macOS에서는 Apple Silicon Mac(기본 엔진의 `engines/nemotron-stream/fetch-libs.sh`가 macOS에서는 Apple Silicon용 라이브러리만 받습니다), Rust(CI는 1.98.1 사용), Node.js(CI는 24), macOS에서는 Xcode Command Line Tools(Swift). 테스트용으로 Python 3.12가 필요합니다.

```
# 프런트엔드 (앱 크레이트를 컴파일하기 전에 먼저: tauri가 app/dist를 컴파일 때 읽습니다)
(cd app && npm ci && npm run build)

# 코어와 CLI (워크스페이스)
cargo build --workspace

# macOS 녹음 프로그램 (Swift)
(cd capture/macos && swift build -c release)

# 실시간 자막·화자 구분 엔진 (워크스페이스 밖의 독립 크레이트)
engines/nemotron-stream/fetch-libs.sh && (cd engines/nemotron-stream && cargo build)

# 데스크톱 앱 (워크스페이스 밖의 별도 크레이트)
(cd app && npm run tauri dev)
```

`fetch-libs.sh`는 고정된 버전의 미리 빌드된 라이브러리 압축 파일을 sha256으로 확인하고 `vendor/`(git 제외)에 풉니다. macOS용만 실제로 빌드해 봤습니다. 예전 엔진은 `engines/sherpa-stream/fetch-libs.sh && (cd engines/sherpa-stream && cargo build)`입니다. 실험 기능인 Qwen3-ASR 다시 받아쓰기 엔진은 `engines/qwen3-asr/fetch-libs.sh && (cd engines/qwen3-asr && cargo build)`입니다(macOS Apple Silicon만). 테스트 명령은 [CONTRIBUTING.md](CONTRIBUTING.md)에 있습니다.

## 처음 쓰기 (macOS)

1. 위 순서대로 빌드하고 `npm run tauri dev`로 앱을 엽니다. 개발 빌드는 저장소 안의 녹음 프로그램(`capture/macos/.build/release/`)과 엔진(`engines/nemotron-stream/target/debug/`)을 알아서 찾습니다.
2. 회의 목록의 안내나 **설정 › 받아쓰기 준비**에서 **실시간 자막 모델**(742 MB)을 내려받습니다. 화자 구분 모델(107 MB)은 선택입니다. 받기 전에 크기, 배포처, 라이선스, 출처를 보여 주고, 끊기면 이어 받습니다. 이미 받은 파일이 있으면 "이미 받은 파일 고르기"로 가져옵니다. 어느 쪽이든 고정된 sha256과 맞아야 씁니다([docs/model-setup.md](docs/model-setup.md)).
3. 녹음을 시작합니다. 처음 한 번 macOS가 마이크와 "화면 및 시스템 오디오 녹음" 권한을 묻습니다. 화면 내용은 저장하지 않습니다.
4. 녹음이 끝나면 실시간 자막이 초안이 됩니다. 검토하고 승인한 뒤 내보내거나 플러그인을 실행합니다.

Whisper로 다시 받아쓰기를 하려면 `brew install whisper-cpp`로 프로그램을 설치하고 ggml 모델을 받은 뒤, 데이터 폴더의 `config.toml`에 `[whisper] model`을 적고 `MEETKO_WHISPER_CLI`를 지정합니다(Finder에서 연 앱은 Homebrew의 PATH를 보지 못합니다). 설정 화면에도 같은 순서가 나옵니다.

## 모델

모델은 이 저장소에 없고 앱에도 들어 있지 않으며, meetko가 재배포하지 않습니다. 각 모델은 자기 라이선스를 따릅니다. 쓰기 전에 원 출처의 약관을 확인하세요.

| 용도 | 모델 | 구하는 방법 | 라이선스 |
|---|---|---|---|
| 실시간 자막 (필수) | `nvidia/nemotron-3.5-asr-streaming-0.6b` (q8_0 GGUF, 742 MB) | 앱에서 내려받기 | OpenMDW-1.1 |
| 화자 구분 (선택) | `nvidia/Nemotron-3-Diarization` (q8_0 GGUF, 107 MB) | 앱에서 내려받기 | OpenMDW-1.1 |
| 다시 받아쓰기 (선택) | whisper.cpp용 ggml 모델 (예: `ggml-large-v3-turbo.bin`, Hugging Face `ggerganov/whisper.cpp`) | 직접 받기 | MIT |
| 다시 받아쓰기 (실험 기능, Whisper 대신 고를 수 있음) | `Qwen/Qwen3-ASR-1.7B` + 단어 시각용 `Qwen/Qwen3-ForcedAligner-0.6B` (CrispASR 작성자의 q8_0 GGUF 변환 `cstr/qwen3-asr-1.7b-GGUF`, `cstr/qwen3-forced-aligner-0.6b-GGUF`, 합쳐 약 3.5 GB) | 앱에서 내려받기 (개발용 빌드에서만 엔진이 있음) | Apache-2.0 (엔진 CrispASR는 MIT) |
| 실시간 자막(예전 엔진) | sherpa-onnx 한국어 streaming zipformer (`sherpa-onnx-streaming-zipformer-korean-2024-06-16`) | 직접 받기 (앱은 제공하지 않음) | 모델 카드에 라이선스 표기 없음. 이름으로 보아 KsponSpeech로 학습했고, 그 데이터의 이용 조건을 따를 수 있음. 연구·시험용으로만 권장 |

OpenMDW-1.1 모델을 다른 사람에게 다시 배포할 때는 라이선스 전문을 함께 넣어야 합니다(전문은 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)에 있습니다).

실시간·화자 분리 모델은 파일별 sha256이 [engines/nemotron-stream/models.toml](engines/nemotron-stream/models.toml), [engines/sherpa-stream/models.toml](engines/sherpa-stream/models.toml), [engines/qwen3-asr/models.toml](engines/qwen3-asr/models.toml)에 고정되어 있습니다. 코어는 이 값과 다른 모델을 거부합니다(개발용 `*_UNPINNED=1`은 환경변수로 지정한 폴더에만 적용되고, 앱이 받은 모델에는 적용되지 않습니다). 앱에서 받거나 고른 모델은 데이터 폴더의 `models/` 아래에 두고, 환경변수가 없을 때 이 폴더를 씁니다. 파일 이름, 변환 방법, 디렉터리 구성은 각 엔진의 README([nemotron](engines/nemotron-stream/README.md), [sherpa](engines/sherpa-stream/README.md))를 보세요.

## 회의 알림

설정 › 회의 알림. macOS에서 기본으로 켜져 있고, 스위치로 끌 수 있습니다(끈 상태는 저장됩니다). 회의 앱(Zoom, Teams, Slack 허들, Webex, FaceTime 등)이 마이크를 6초쯤 계속 쓰면 "회의가 시작된 것 같아요. 녹음할까요?"라고 묻고, 녹음 중에 회의 앱이 30초 동안 마이크를 놓으면 "녹음을 정지할까요?"라고 묻습니다. 정지 질문은 회의 앱에서만 나오고 브라우저에서는 나오지 않습니다(브라우저는 회의가 아닌 일로도 마이크를 잡거나 놓기 때문입니다). 브라우저(웹 회의)까지 볼지는 따로 끌 수 있습니다. **스스로 녹음을 시작하거나 멈추지 않습니다.**

보는 것: 3초마다 macOS(CoreAudio)에 "지금 어느 프로그램이 마이크 입력을 쓰는지"(프로그램의 번들 id)만 묻습니다. 보지 않는 것: 창 제목, 브라우저 탭, 주소, 화면 내용, 소리. 손쉬운 사용·화면 기록 권한을 쓰지 않고, 아무것도 기록하거나 보내지 않습니다. 자세한 규칙은 [docs/meeting-detection.md](docs/meeting-detection.md)에 있습니다.

## 환경변수와 명령줄

코드가 실제로 읽는 환경변수입니다. 개발이나 직접 설정할 때 쓰는 것이고, 보통은 설정하지 않아도 됩니다. 환경변수가 앱 설정보다 우선합니다.

| 변수 | 의미 |
|---|---|
| `MEETKO_DATA_DIR` | 데이터 폴더. 기본값은 사용자별 로컬 데이터 폴더 아래 `meetko` (macOS: `~/Library/Application Support/meetko`) |
| `MEETKO_WHISPER_MODEL` | whisper ggml 모델 파일 경로 (데이터 폴더 `config.toml`의 `[whisper] model`로도 지정 가능) |
| `MEETKO_WHISPER_MODEL_SHA256`, `MEETKO_WHISPER_DTW` | 모델 해시 고정, DTW 설정 |
| `MEETKO_WHISPER_CLI` | `whisper-cli` 실행 파일. 없으면 앱 옆 파일, PATH 순으로 찾음 |
| `MEETKO_WHISPER_VAD_MODEL` | (평가용, 기본 꺼짐) whisper.cpp VAD 모델 디렉터리(`ggml-silero-v6.2.0.bin`, sha256 고정). 지정하면 `--vad`로 받아씀 ([docs/model-setup.md](docs/model-setup.md)) |
| `MEETKO_CAPTURE_BIN` | 녹음 프로그램 실행 파일 |
| `MEETKO_STREAM_MODEL`, `MEETKO_STREAM_ENGINE` | 실시간 자막 모델 디렉터리(없으면 앱이 받아 둔 모델), 엔진 실행 파일 |
| `MEETKO_STREAM_LANGUAGE` | 실시간 자막 언어(기본 `auto`) |
| `MEETKO_DIAR_MODEL`, `MEETKO_DIAR_ENGINE` | 화자 분리 모델 디렉터리, 엔진. 지정하지 않고 앱이 받아 둔 모델도 없으면 화자 분리를 하지 않음 |
| `MEETKO_BATCH_MODEL`, `MEETKO_BATCH_ENGINE`, `MEETKO_BATCH_TIMEOUT_SECS` | 다시 받아쓰기에 Qwen3-ASR(실험)을 쓸 때(설정이 "자동"이면 준비되어 있을 때 먼저 씀): 모델 디렉터리(없으면 앱이 받아 둔 모델), 엔진 실행 파일, 시간 한도 |
| `MEETKO_DIAR_TIMEOUT_SECS` | 화자 분리 엔진 실행 시간 제한(초) |
| `MEETKO_DIAR_MIN_SPEAKER_SECS` | 채널에서 이 시간(초)보다 적게 말한 화자는 다른 화자와 구분하지 않음. 기본 10, 0이면 이 규칙을 끔 |
| `MEETKO_PLUGIN_TIMEOUT_SECS` | 플러그인 요청 시간 제한 |
| `MEETKO_PLUGIN_SANDBOX` | 개발용. `off`로 두면 macOS 샌드박스 없이 플러그인을 실행(결과에 "격리 없이 실행됨"으로 기록) |
| `MEETKO_PLUGIN_INDEX_URL` | 플러그인 둘러보기가 읽는 목록(`index.json`) 주소. 기본값은 `https://raw.githubusercontent.com/Eastsidegunn/meetko-plugins/main/index.json` |
| `MEETKO_GITHUB_API`, `MEETKO_GITHUB_CODELOAD` | GitHub에서 플러그인을 받을 때 쓰는 API·내려받기 주소. 테스트용 재지정(https, 또는 localhost의 http만) |
| `MEETKO_STREAM_MODEL_UNPINNED`, `MEETKO_DIAR_MODEL_UNPINNED`, `MEETKO_BATCH_MODEL_UNPINNED` | 개발용. 환경변수로 지정한 모델 폴더의 고정 해시 검사를 끔(앱이 받은 모델에는 적용 안 됨) |
| `MEETKO_BATCH_SANDBOX=off` | 개발용(macOS). Qwen3-ASR 엔진을 격리 없이 돌림. 출처에 `sandbox: off`로 남음 |

테스트와 빌드에만 쓰는 변수(`MEETKO_REQUIRE_MODELS`, `MEETKO_REQUIRE_PLUGINS`, `MEETKO_ENGINE_DEV_RPATH`, `MEETKO_NEMO_BACKEND`, `MEETKO_TEST_PACED`, `MEETKO_DIAR_MULTI_WAV`)는 [CONTRIBUTING.md](CONTRIBUTING.md)와 각 엔진의 README에 있습니다.

CLI(`meetko`)의 명령: `import`, `transcribe`(whisper), `approve`, `plugin add|install|info|list`, `run`, `show`, `export`(webvtt, ctm, rttm, text), `results`, `list`, `search`, `mcp`(AI 에이전트 연결, 아래). Markdown·SRT 내보내기는 지금은 앱에만 있습니다. 자세한 옵션은 `cargo run -p meetko-cli -- --help`로 확인하세요.

## 플러그인

플러그인은 회의 데이터를 받아 표시 값(요약, 액션아이템, 발화 시간 등)을 돌려주는 순수 함수 형태의 별도 프로세스입니다. 예제는 [plugins/](plugins/)에 있고, 형식은 [docs/manifest-draft.md](docs/manifest-draft.md)(초안)를 보세요.

**보안 주의.**
- **macOS:** 플러그인은 OS 샌드박스(`sandbox-exec`) 안에서 실행됩니다. 막는 것: 네트워크(매니페스트에 `network = true`를 선언하고 설치 때 동의했을 때만 허용), 내 홈 폴더와 `/Users` 전체, 다른 프로세스의 목록·명령줄·환경변수, 다른 프로그램 실행, meetko 데이터 폴더(녹음·데이터베이스·다른 회의·설정), 다른 임시 파일(`/private/tmp` 등), 외부 드라이브(`/Volumes`), 그리고 자기 설치 폴더를 포함한 대부분의 쓰기(쓸 수 있는 곳은 실행마다 새로 만드는 임시 HOME뿐). 샌드박스를 만들 수 없으면 플러그인을 실행하지 않습니다. 매니페스트의 `python3`는 PATH와 상관없이 Apple의 `/usr/bin/python3`로 실행하며(Homebrew 파이썬과 홈 폴더 바로 아래에 설치한 파이썬은 거부), 이를 위해 Xcode Command Line Tools(`xcode-select --install`)가 필요합니다. 개발용으로만 `MEETKO_PLUGIN_SANDBOX=off`로 끌 수 있고, 이때는 결과에 "격리 없이 실행됨"으로 기록됩니다.
- **막지 못하는 것:** 플러그인은 여전히 내 컴퓨터에서 실행되고, CPU·메모리는 제한하지 않으며(시간 제한만 있음), 기본은 모두 거부이고 나열한 것만 허용합니다(그래서 위에 적지 않은 것도 막혀 있습니다). 네트워크 권한을 선언한 플러그인은 아무 곳으로나 보낼 수 있으므로, 그 플러그인이 읽는 것(건네준 회의 전사본)을 밖으로 보낼 수 있습니다. 오디오는 플러그인에 전달하지 않습니다.
- **Windows·Linux:** 아직 격리가 없습니다. 매니페스트의 네트워크·오디오 권한은 선언일 뿐이고, 플러그인은 내 사용자 계정 권한으로 실행되어 계정이 읽을 수 있는 파일을 읽고 네트워크를 쓸 수 있습니다.

믿는 플러그인만 설치하세요. 자세한 내용과 알려진 빈틈은 [docs/plugin-security.md](docs/plugin-security.md)에 있습니다.

### 플러그인 설치·공개

```
meetko plugin info    github:OWNER/REPO@REF   # 내려받아 매니페스트와 권한만 보여 줌. 설치하지 않음
meetko plugin install github:OWNER/REPO@REF   # 권한을 보여 주고 동의를 받은 뒤 설치
meetko plugin install ./내-폴더                # 로컬 폴더도 가능 (plugin add와 같음)
```

앱의 플러그인 화면에 있는 **둘러보기**에서도 GitHub의 플러그인 목록을 보고 설치할 수 있습니다(버튼을 누를 때만 인터넷에 연결). 목록에 오르는 것은 형식 검사를 통과했다는 뜻일 뿐 **사람의 검토가 없습니다**. 설치는 태그를 풀어 **정확한 커밋에 고정**하고, 설치 중에는 아무것도 빌드·실행하지 않으며 받은 파일을 그대로 씁니다. 만들고 올리는 방법은 [docs/plugin-publishing.md](docs/plugin-publishing.md)를 보세요.

## AI 에이전트 연결 (MCP)

`meetko mcp`로 Codex CLI, Claude Code 같은 AI 에이전트가 회의 목록·전사본(단어 id와 시각)·출처·결과를 읽고, 근거가 달린
결과를 meetko에 넣을 수 있습니다. 에이전트는 자기 로그인으로 AI를 쓰고, meetko는 토큰을 다루지 않습니다. 에이전트가 읽은
전사본은 그 AI 제공자에게 전송되고, 넣은 결과는 앱에서 "외부 AI 에이전트가 만든 결과"로 표시됩니다. 설정과 예시는
[docs/mcp.md](docs/mcp.md)를 보세요.

## 계약

엔진과 코어, 코어와 플러그인 사이의 데이터 형식은 [docs/rfc-0001-contract.md](docs/rfc-0001-contract.md)와 `contract/schemas/`의 JSON 스키마가 정의합니다. 계약을 바꾸려면 먼저 논의가 필요합니다([CONTRIBUTING.md](CONTRIBUTING.md)). 제3자 구성 요소는 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)를 보세요.

## English

meetko is a local-first desktop app that produces Korean-English meeting transcripts on your own machine. Its central contract is word-level timestamps: engines and models produce meeting data in which every word has start/end times in milliseconds and a speaker; the core stores, versions and approves it; plugins are functions that take that data and return values for display. Models, engines and plugins attach through a contract (see `docs/rfc-0001-contract.md`). Written in Rust (core, CLI, engines), Tauri v2 + TypeScript (app) and Swift (macOS capture sidecar).

Features: recording of mic and system audio as separate channels (macOS); live captions that become the first draft when the recording stops, with optional speaker diarization; then, automatically, a re-transcription of the whole recording with the most accurate batch engine that is set up (Qwen3-ASR, experimental, else whisper.cpp), which becomes the new draft unless something is already approved, reusing the live session's speaker separation when it covers the same audio (settings: engine auto/Whisper/Qwen3-ASR, and an on-by-default switch; also by hand as "다시 받아쓰기"); review and approval with word-level audio jump; speaker names, speaker merges and in-place text edits kept as an overlay over the original (reversible, reflected in exports, search and MCP); a custom vocabulary (hint terms for Whisper and an ordered find-and-replace list applied for display only); export to Markdown, text, SRT and WebVTT plus copy variants; full-text search across meetings (also `meetko search`); and a meeting prompt (macOS, on by default) that asks whether to record when a known meeting app has been using the microphone for a few seconds. The prompt only reads which processes use audio input (CoreAudio, bundle id); no window titles, tabs, URLs, screen contents or audio, nothing recorded or sent, and it never starts or stops a recording by itself (`docs/meeting-detection.md`).

Status: pre-release (0.1.0), build from source (no installable app yet; the engines are not bundled). Verified on macOS 15+ (Apple Silicon) only; a CI configuration that builds and tests the workspace and the app crate on macOS and Windows is included but currently turned off; Windows has no capture program or meeting prompt and is untested in practice, and Linux is neither built nor tested. First run: build as above, open the app, then download the live-caption model (742 MB, required) and optionally the diarization model (107 MB) in Settings → 받아쓰기 준비; whisper is optional. Network: the app connects only when you click a button: model download (Hugging Face) and plugin gallery/install (GitHub); there is no background update check or telemetry. Models are not included or redistributed; the downloaded models are used only when their sha256 matches the pin (`docs/model-setup.md`); each has its own license (see the table above, `engines/*/models.toml` and the OpenMDW-1.1 text in `THIRD_PARTY_NOTICES.md`). Plugins: on macOS they run in an OS sandbox (`sandbox-exec`) that denies network unless the plugin declares and you consent to it (IP only, no Unix sockets), starting any child process, seeing other processes (list, command line, environment), and reading your home folder, the meetko data dir, other temp files and external drives, and writing anywhere but a per-run scratch HOME. The profile is deny-by-default: only what is listed is allowed, so anything not named here is blocked too. CPU and memory are not limited, and a plugin with network permission can send out the transcript it is given. On Windows and Linux there is no isolation yet: plugins run as your user and declared permissions are not enforced (see `docs/plugin-security.md`). Plugins install from GitHub with `meetko plugin install github:OWNER/REPO@REF` (pinned to a commit, nothing is built at install, no human review; see `docs/plugin-publishing.md`). AI agents (Codex CLI, Claude Code) can read meetings and store evidence-linked results through a local stdio MCP server, `meetko mcp`; what they read goes to their AI provider, and their results are marked as such in the app (`docs/mcp.md`). Build commands are listed above in the Korean section (on macOS an Apple Silicon Mac is required: `engines/nemotron-stream/fetch-libs.sh` only fetches arm64 libraries there); contribution notes are in `CONTRIBUTING.md`. License: AGPL-3.0-or-later. Copyright (C) 2026 Eastsidegunn.
