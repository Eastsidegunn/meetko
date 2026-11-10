# 매니페스트 초안 v0

2026-10-27 · 초안. 필드 이름과 값은 바뀔 수 있다. 정한 것은 구조다.

## 원칙

- 코어는 사용자 데이터(녹음, 전사, 버전, 승인)를 관리하고 wiring만 한다.
- 앞단(엔진과 모델)은 회의 데이터를 **만든다**. 계약으로 교체한다.
- 뒷단(플러그인)은 **함수**다. 회의 데이터를 넣으면 GUI 계약을 따르는 값을 돌려준다. 요약, 정리, 액션아이템 추출 등. 마켓플레이스의 대상이 이쪽이다.
- 모든 실행 패키지는 별도 프로세스로 돌고 프로토콜로만 붙는다. 서로 부르지 않는다.
- 권한은 매니페스트에 선언하고, 사용자가 설치할 때 승인한다. 기본은 로컬이다.

## 계약은 둘이다

```
[엔진 + 모델] ──계약 A: 회의 데이터──▶ [코어: 저장·버전·승인] ──계약 B: 회의 데이터──▶ [플러그인] ──표시 타입──▶ [GUI]
```

- **계약 A (엔진 → 코어)**: 엔진은 다섯 갈래를 내놓는다. 단어 단위 타임스탬프가 중심이다.
- **계약 B (코어 → 플러그인 → GUI)**: 플러그인은 코어가 정리한 회의 데이터를 받아, 고정된 표시 타입으로 결과를 돌려준다.

플러그인은 엔진을 모른다. 어떤 엔진으로 전사했든 같은 회의 데이터를 받는다. 앞단과 뒷단이 각자 교체될 수 있는 이유다.

## 패키지는 세 종류다

| 종류 | 자리 | 무엇 | 코드 | 예 |
| --- | --- | --- | --- | --- |
| `engine` | 앞단 | 오디오를 회의 데이터로 바꾸는 프로세스 | 있음 | whisper.cpp, sherpa-onnx, 클라우드 ASR, 정렬기 |
| `model` | 앞단 | 엔진이 읽는 가중치 | 없음 (선언만) | large-v3 q5_0, 한국어 zipformer |
| `plugin` | 뒷단 | 회의 데이터를 가지고 일하는 모듈 | 있음 | 요약, 회의록 정리, 액션아이템, 용어 추출, 화자 이름 정리 |

마켓플레이스는 `plugin`이 중심이다. `engine`과 `model`도 같은 설치 경로를 쓰지만, 사용자가 고르는 화면은 따로 둔다.

파일 이름은 `meetko.toml`. 주석을 달 수 있고 Rust/Tauri 생태계(Cargo)와 같은 형식이다.

## 공통 필드

```toml
[package]
id = "meetko/summary-ko"      # 게시자/이름
version = "0.1.0"             # semver
kind = "plugin"               # engine | model | plugin
contract = "0.1"              # 따르는 계약 버전. 코어가 호환 여부를 판단한다
license = "AGPL-3.0"
description = "한국어 회의 요약"
homepage = "https://example.com/summary-ko"   # 선택. 둘러보기 목록에 보인다. https만
```

`license`, `description`, `homepage`는 선택이다(코어는 `homepage`를 쓰지 않는다). 모르는 필드는 무시한다. 다만 둘러보기 목록(`docs/plugin-publishing.md`)은 `description`을 요구하고, 런타임(`python`, `node`, `binary` 등)은 `run.command`의 첫 항목에서 계산한다. 따로 적는 필드가 아니다.

## 플러그인 매니페스트 (뒷단, 마켓플레이스)

플러그인은 함수다.

```
plugin(회의 데이터) → 표시 타입 값
```

- 입력은 회의 데이터 하나뿐이다. 다른 플러그인의 결과는 받지 않는다
- 출력은 GUI 계약(표시 타입)을 따르는 값뿐이다. 파일을 쓰거나 외부로 내보내지 않는다
- 상태를 갖지 않는다. 결과의 저장과 버전 연결은 코어가 한다
- 언제 부를지는 코어(와 사용자)가 정한다. 플러그인은 자신이 언제 도는지 모른다

```toml
[package]
id = "meetko/summary-ko"
version = "0.1.0"
kind = "plugin"
contract = "0.1"
license = "AGPL-3.0"
description = "한국어 회의 요약과 결정사항 정리"

[run]
command = ["bin/summary-ko"]  # 배열: 셸을 거치지 않는다
protocol = "stdio-jsonrpc"    # 계약 0.1. 서버 모드용 ws는 같은 메시지를 다른 전송으로 보낸다

[input]                       # 받을 회의 데이터
transcript = true             # 단어 타임스탬프와 화자가 붙은 전사
speakers = true
meeting = true                # 제목, 참석자, 날짜 등 메타데이터
audio = "none"                # none | full — 오디오는 별도 권한

[output]                      # 돌려줄 표시 타입
display = ["summary_card", "action_items"]

[permissions]
network = true                # LLM API를 부른다면 true. 설치 시 승인, 결과에 "외부 전송" 표시. 선언하지 않으면 macOS 샌드박스가 네트워크를 막는다
```

### 플러그인이 받는 회의 데이터

코어가 정리해서 넘긴다. 플러그인은 엔진이나 원시 스트림을 직접 만지지 않는다.

- 전사: 단어마다 시작·끝 시각, 텍스트, 화자, 채널
- 화자 구간과 화자 이름
- 회의 메타데이터와 이 데이터가 어느 버전(live / draft / approved)인지
- 오디오: `audio = "full"`을 선언하고 승인받은 플러그인만

### 플러그인이 GUI로 보내는 것 — 표시 타입

`transcript_block`, `timeline_marker`, `summary_card`, `action_items`, `table`.

플러그인은 UI를 직접 그리지 않는다. 고정된 타입으로 데이터를 보내면 GUI가 그린다. 모든 결과는 오디오 타임라인의 시각을 가리킬 수 있어서, 요약 문장을 누르면 그 발언으로 이동할 수 있다. 타입은 늘릴 수는 있어도 줄이지 않는다.

### 플러그인 결과도 버전에 묶인다

플러그인 결과는 입력으로 쓴 전사 버전에 묶여 저장된다. 승인본이 바뀌면 그 위에서 만든 요약은 stale로 표시되고, 요청이 있을 때만 다시 돌린다.

## 엔진 매니페스트 (앞단)

```toml
[package]
id = "meetko/whisper-cpp"
version = "0.1.0"
kind = "engine"
contract = "0.1"
license = "MIT"

[run]
command = "bin/meetko-whisper-cpp"
protocol = "ws"

[role]
provides = "asr.batch"        # asr.stream | asr.batch | diarization | alignment
word_timestamps = "native"    # native | aligned | none

[permissions]
network = false               # 클라우드 ASR이면 true

[accepts]
model_engine = "whisper.cpp"  # 이 엔진에 맞는 model 패키지를 받는다
```

| 자리 | 받는 것 | 내놓는 것 |
| --- | --- | --- |
| `asr.stream` | 실시간 오디오 | 잠정·확정 세그먼트 (라이브 자막) |
| `asr.batch` | 전체 오디오 + 용어 목록 | 단어 타임스탬프 전사 |
| `diarization` | 오디오 | 화자 구간 |
| `alignment` | 전체 오디오 + 전사 | 단어 시각을 보강한 전사 |

`word_timestamps = "none"`인 엔진은 `alignment` 엔진과 짝지어야만 `asr.batch`로 켜진다.

## 모델 매니페스트 (앞단)

```toml
[package]
id = "ggml-org/whisper-large-v3-q5_0"
version = "1.0.0"
kind = "model"
contract = "0.1"
license = "MIT"

[model]
engine = "whisper.cpp"
languages = ["ko", "en", "*"]
sample_rate = 16000

[[files]]
url = "https://..."
sha256 = "..."
size = 1080000000

[defaults]                    # 엔진이 해석하는 디코드 기본값
max_context = 0
no_speech_thold = 0.6
```

## 코어가 버전마다 기록하는 것

플러그인이나 엔진이 아니라 코어가 쓴다. 신뢰의 근거는 코어가 쥔다.

- 원본 오디오 SHA-256
- 엔진 id와 버전, 모델 id와 sha256, 실제 디코드 파라미터
- 단어 타임스탬프 출처 (native / aligned + 정렬 엔진 id)
- 외부 전송 여부 (`network = true` 패키지를 거쳤는지)
- 플러그인 결과라면 플러그인 id와 버전, 입력으로 쓴 전사 버전

## 설치

1.0은 레지스트리 없이 시작한다.

```
meetko install https://github.com/<게시자>/<저장소>   # Git URL
meetko install ./my-plugin                           # 로컬 경로
```

설치할 때 권한(`audio`, `network`)을 보여주고 승인받는다. 현재 구현은 `meetko plugin install github:OWNER/REPO@REF`(커밋 고정, 설치 중 빌드 없음)이고 [plugin-publishing.md](plugin-publishing.md)에 있다. macOS에서는 선언하지 않은 네트워크와 홈·데이터 폴더 읽기를 샌드박스가 막고, 그 밖의 OS에서는 권한이 선언일 뿐이다([plugin-security.md](plugin-security.md)). 마켓플레이스(레지스트리와 탐색 화면)는 사용자가 생긴 뒤에 붙이며, 매니페스트는 바뀌지 않는다.

## 아직 정하지 않은 것

- 회의 중(live 데이터)에 부르는 플러그인의 시간 예산
- 같은 자리에 엔진이 둘 이상일 때 무엇을 쓸지 (사용자 선택, 회의별 선택)
- `contract` 호환 규칙
- 플랫폼별 실행 파일 배포
- 서명과 신뢰 모델, 마켓플레이스 심사 여부
