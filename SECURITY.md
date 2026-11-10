<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# 보안 정책

## 취약점 신고

공개 이슈로 올리지 마세요. GitHub의 비공개 취약점 신고를 쓰세요: 저장소의 **Security** 탭 -> **Report a vulnerability**. 재현 방법, 영향 범위, 확인한 버전(또는 커밋)을 적어 주세요. 녹음이나 실제 회의 내용은 첨부하지 마세요.

meetko는 초기 단계의 프로젝트라 응답 시간을 약속하지 못합니다. 확인하는 대로 신고 창구에서 답하겠습니다.

## 범위에 대한 메모

- **플러그인 실행 모델.** 플러그인은 별도 프로세스로 실행됩니다. macOS에서는 OS 샌드박스(`sandbox-exec`)가 네트워크(선언·동의하지 않으면), 홈 폴더, meetko 데이터 폴더, 다른 임시 파일, 외부 드라이브의 읽기와 임시 HOME 밖의 쓰기를 막습니다. CPU·메모리는 제한하지 않고, 네트워크를 선언한 플러그인은 자기가 받은 전사본을 보낼 수 있습니다. Windows와 Linux에는 아직 격리가 없어서 플러그인이 사용자 계정 권한으로 실행되고 네트워크·오디오 권한은 선언일 뿐입니다. 이것은 알려진 한계입니다. 샌드박스를 우회하는 방법은 관심 대상입니다. 현재 강제되는 것과 알려진 빈틈은 [docs/plugin-security.md](docs/plugin-security.md)에 있습니다. 이 문서에 이미 적힌 한계는 취약점이라기보다 알려진 작업 항목이지만, 문서와 실제 동작이 다르다면 신고해 주세요.
- **엔진과 모델.** 엔진(`engines/`)은 네이티브 라이브러리를 링크하고 모델 파일을 네이티브 코드로 읽습니다. 코어는 알려진 모델의 sha256을 고정해 검증하고, 검증하며 복사한 비공개 디렉터리의 사본만 엔진에 넘깁니다. 이 검증을 우회하거나 엔진 프로토콜 처리(잘못된 입력, 종료 처리)에서 문제가 생기는 경우가 관심 대상입니다. 외부 라이브러리 자체의 취약점은 해당 프로젝트에 먼저 알려 주세요.
- **모델 내려받기.** 앱은 사용자가 "내려받기"를 누를 때만 Hugging Face에서 고정된 커밋의 모델 파일을 받습니다. HTTPS만 쓰고, 넘겨받는 주소(redirect)도 Hugging Face 주소일 때만 따라가며, 크기와 sha256이 고정값과 맞을 때만 파일을 씁니다([docs/model-setup.md](docs/model-setup.md)). 이 확인을 우회해 다른 파일을 쓰게 만들거나, 다른 주소로 연결하게 만드는 방법은 관심 대상입니다.
- **인터넷 연결.** 앱은 모델 내려받기, 플러그인 둘러보기·설치 버튼을 누를 때만 인터넷에 연결합니다. 버튼 없이 연결이 일어난다면 신고해 주세요.
- **회의 알림(macOS).** 3초마다 CoreAudio에 어느 프로세스가 마이크 입력을 쓰는지(번들 id)만 묻고, 아무것도 기록하거나 보내지 않습니다. 녹음을 스스로 시작하지 않습니다([docs/meeting-detection.md](docs/meeting-detection.md)). 이보다 많은 것을 읽거나, 묻지 않고 녹음이 시작되는 경우는 신고해 주세요.
- **로컬 데이터 폴더.** 녹음, 전사본, 버전, 검색 색인, 플러그인 결과, 앱이 받은 모델(`models/`), 앱 설정(`app-settings.json`)은 사용자 기기의 데이터 폴더(`MEETKO_DATA_DIR`, 기본은 사용자별 로컬 데이터 폴더 아래 `meetko`)에 저장됩니다. 이 폴더의 권한과 다른 프로세스(플러그인 포함)가 이 폴더를 읽거나 바꿀 수 있는 경로는 보안 관심사입니다.
- 지원 대상은 최신 `main` 브랜치 하나입니다. 릴리스된 버전이 아직 없습니다.

## English

Please do not open a public issue. Use GitHub private vulnerability reporting: the repository's Security tab, then "Report a vulnerability". Include reproduction steps, impact and the version or commit; do not attach recordings or real meeting content. Scope notes: plugins are separate processes; on macOS they run in an OS sandbox (`sandbox-exec`, deny by default: network only if declared and consented, no home folder, no meetko data dir, no child processes, writes only to a per-run scratch HOME), with no CPU/memory limit, and a plugin with network permission can send out the transcript it receives; on Windows and Linux there is no isolation yet and declared permissions are not enforced (see `docs/plugin-security.md`); engines load native libraries and parse model files, with pinned sha256 verification in the core; model downloads happen only on a click, over HTTPS from Hugging Face hosts only (redirects included), and a file is used only when its size and sha256 match the pin (`docs/model-setup.md`); the app connects to the network only when you click model download or the plugin gallery/install, so any other connection is a bug worth reporting; the macOS meeting prompt reads only which processes use audio input and never starts a recording by itself (`docs/meeting-detection.md`); the local data directory holds recordings and transcripts. Only the latest `main` is supported; there is no tagged release yet.
