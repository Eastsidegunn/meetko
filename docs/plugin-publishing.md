# 플러그인 게시와 설치

2026-10-31 · meetko 플러그인은 심사 없이 퍼진다. 만든 사람이 GitHub에 올리고 태그를 달면 목록에 나타나고, 쓰는 사람은 커밋에 고정해서 설치한다. 허가를 받아야 하는 PR은 없다.

*English summary:* anyone can publish a meetko plugin by pushing a GitHub repo with the topic `meetko-plugin` and a `meetko.toml` at the root. A daily job lists such repos in a gallery index; users install with `meetko plugin install github:OWNER/REPO@REF`, which pins the exact commit. Nothing is built at install time and nobody reviews the code.

## 플러그인 만들기 (게시하는 쪽)

1. GitHub 저장소를 만든다. 공개 저장소여야 한다.
2. 저장소 **루트**에 `meetko.toml`을 둔다. 형식은 [manifest-draft.md](manifest-draft.md)를 따른다. 목록에 오르려면 아래가 필요하다.
   - `package.id = "<게시자>/<이름>"`(소문자, 숫자, `.` `_` `-`, 각 부분 64자 이내). **게시자는 저장소 소유자 이름과 같아야 한다**(대소문자 무시). `acme/summary`는 `github.com/acme/...`에서만 올라가고, `meetko plugin install`과 앱 설치도 같은 규칙을 **설치할 때** 다시 검사한다(게시자 부분이 없거나 id 형식이 아니면 GitHub 설치가 거부된다). 매니페스트 문자열(id, 버전, 설명 등)에 제어 문자와 방향·보이지 않는 서식 문자가 있으면 파싱 단계에서 거부한다. 남의 이름을 쓸 수 없게 하는 최소한의 장치다.
   - `version`은 semver(`1.2.3`), `kind = "plugin"`, `contract = "0.1"`, `run.protocol = "stdio-jsonrpc"`.
   - `description`은 한 줄(300자 이내, 줄바꿈·제어 문자·방향 바꾸는 문자 금지). `license`, `homepage`(https)는 선택.
   - 필요한 권한을 정직하게 선언한다: `permissions.network`, `input.audio`, 그리고 API 키를 쓴다면 `permissions.secrets`(키 이름만, `_KEY`/`_TOKEN`/`_SECRET`으로 끝남, 최대 4개)와 필요하면 `permissions.settings`(모델 이름·서버 주소, `_MODEL`/`_BASE_URL`로 끝남, 최대 4개). 둘 다 `network = true`가 필요하다([manifest-draft.md](manifest-draft.md)). 키 값을 저장소나 매니페스트에 넣지 않는다. 사용자가 자기 키를 넣는다.
3. 저장소 **topic**에 `meetko-plugin`을 추가한다(저장소 첫 화면 오른쪽 톱니바퀴).
4. 버전을 낼 때 **release(태그)**를 만든다. 목록은 최신 release 태그를 기준으로 하고, release가 없으면 기본 브랜치의 최신 커밋을 쓴다.

### 설치할 때 빌드하지 않는다

meetko는 설치 중에 `cargo`, `npm`, `pip` 같은 것을 **실행하지 않는다**. 내려받은 파일이 그대로 실행되어야 한다.

- 인터프리터로 도는 플러그인(Python, Node 등)은 소스를 그대로 올리면 된다. 의존 패키지는 표준 라이브러리만 쓰거나, 저장소 안에 포함한다.
- 컴파일이 필요한 플러그인은 **빌드된 실행 파일을 저장소에 커밋**하고 `run.command`가 그 경로를 가리키게 한다(예: `bin/summary-ko`). 사용자는 그 파일을 그대로 실행하게 되므로, 소스와 바이너리가 맞는지 확인할 방법(재현 빌드 문서 등)을 README에 적어 두는 것이 좋다.
- 받을 수 없는 것과 한도(설치와 같은 코드가 검사한다):
  - 심볼릭 링크, 하드 링크, 장치 파일. 절대 경로, `..`.
  - 내려받는 크기 25MB, 압축을 푼 파일 합계 100MB, 폴더 깊이 32.
  - 항목(entry)은 **폴더와 메타데이터까지 모두 세어** 5000개까지다. 빈 폴더를 많이 만들어도 한도에 걸린다.
  - 압축을 풀면 크기 한도를 넘는 스트림(압축 폭탄)은 풀다가 중단하고, 비정상적으로 큰 tar 확장 헤더도 읽기 전에 거부한다.
  - Windows에서 만들 수 없는 이름: `CON`, `PRN`, `AUX`, `NUL`, `COM0`-`COM9`, `LPT0`-`LPT9`(확장자가 붙어도, 대소문자 무관), 점이나 공백으로 끝나는 이름, `...`, 그리고 `\ : * ? " < > |`와 제어 문자.

## 설치하기 (쓰는 쪽)

```
meetko plugin info    github:OWNER/REPO@v1.2.0    # 내려받아 매니페스트와 권한만 보여 준다. 설치하지 않는다
meetko plugin install github:OWNER/REPO@v1.2.0    # 권한을 보여 주고 동의를 받은 뒤 설치한다
meetko plugin install ./내-폴더                     # 로컬 폴더도 같다 (plugin add와 동일)
```

- `REF`는 태그(권장) 또는 40자 전체 커밋 sha다. 브랜치 이름은 그 순간의 커밋으로 풀리지만, 아래 확인을 통과해야 한다(기본 브랜치가 아닌 브랜치의 끝은 보통 통과하지 못한다). `pull/…`, `refs/…` 이름은 받지 않는다(포크의 PR 커밋을 가리킬 수 있다). 7~39자의 16진수처럼 생긴 이름은 먼저 **태그/브랜치 이름**으로 풀고(짧은 sha로 취급하지 않는다), 아래 확인을 거친다.
- **커밋이 정말 그 저장소의 것인지 확인한다.** GitHub는 전체 sha를 주면 같은 포크 네트워크의 *어느 포크*에서 온 커밋이든 부모 저장소 이름으로 내려 준다. 그래서 `github:acme/x@<sha>`가 acme의 코드라는 보장이 없다. meetko는 풀린 모든 커밋을 받기 전에 GitHub API로 확인하고, **기본 브랜치의 조상(또는 같은 커밋)이거나 저장소의 태그(주석 태그와 태그의 태그도 따라간다, 1000개까지)가 가리키는 커밋**일 때만 설치한다. 확인할 수 없으면 이유와 함께 거부한다. 그러므로 안전한 선택은 **릴리스 태그**이고, sha로 고정하려면 기본 브랜치에 들어간 커밋이나 태그된 커밋을 쓴다. 기본 브랜치에도 태그에도 없는 커밋(별도 브랜치 끝 등)은 설치되지 않는다.
- **버전은 커밋으로 고정된다.** 태그를 주면 meetko가 전체 sha로 풀어서 그 커밋을 받고, 설치 기록에 `github:OWNER/REPO`, 커밋 sha, 요청한 REF를 남긴다(`meetko plugin list`에 보인다). 나중에 태그를 다른 커밋으로 옮겨도 이미 설치된 것은 바뀌지 않는다. 새 버전은 다시 `install`해야 들어온다.
- 네트워크는 이 명령(과 앱의 둘러보기/설치 버튼)을 누를 때만 쓴다. 백그라운드 업데이트 확인은 없다.
- 받은 파일은 기존 설치 경로를 그대로 지난다: 안전한 압축 해제, 검증된 복사, 선언된 권한 표시와 동의, manifest/코드 해시 기록. 이후 코드가 한 글자라도 바뀌면 실행되지 않는다.
- 공개 저장소만 된다. GitHub 로그인 없이 호출하므로 같은 네트워크에서 시간당 요청이 많으면 잠시 거절될 수 있다. 설치 한 번에 API 요청이 몇 번(이름 풀이, 저장소 정보, 비교, 태그 목록) 나간다. 전체 sha로 설치해도 위의 소속 확인 때문에 요청이 생략되지 않는다.
- **같은 id가 이미 다른 출처로 설치돼 있으면** 거부된다(다른 저장소, 또는 GitHub와 로컬 폴더의 차이). 정말 바꾸려면 `meetko plugin install ... --replace`(명령이 보여 준 `REPLACES:` 줄의 설치 상태에만 적용된다)(로컬 폴더는 `plugin add ... --replace`)로 명시한다. 앱에서는 이전 출처와 새 출처를 나란히 보여 주는 두 번째 확인이 나온다. 같은 저장소의 새 커밋은 업데이트이므로 플래그가 필요 없다. 로컬 폴더로 설치하면 이전의 GitHub 출처 기록은 지워진다.
- **API 키를 쓰는 플러그인**은 설치 전에 "이 플러그인은 API 키(…)와 네트워크를 써요"라고 보여 준다. 키는 설치한 뒤 `meetko plugin secret set <id> <이름>`(값은 stdin, 예: `pbpaste | meetko plugin secret set meetko/summary-py ANTHROPIC_API_KEY`)이나 앱의 `API 키 넣기`로 넣고, `meetko plugin secret status <id>`로 넣었는지 확인할 수 있다(키 값은 다시 보이지 않고, 모델 이름 같은 설정은 보인다). 키는 플러그인 id와 **출처**에 묶인다. 다른 출처의 같은 id로 교체하면 키는 넘어가지 않고 지워진다. 같은 저장소의 업데이트는 키를 유지한다.
- 테스트나 사내 미러에서는 `MEETKO_GITHUB_API`, `MEETKO_GITHUB_CODELOAD`로 주소를 바꿀 수 있다. 앱의 목록 주소는 `MEETKO_PLUGIN_INDEX_URL`이다. 세 주소 모두 `https://`여야 하고, 평문 `http://`는 디버그 빌드이거나 `localhost`/`127.0.0.1`/`::1`일 때만 받는다. 릴리스 빌드는 그 밖의 http를 연결하지 않는다. 연결 제한 시간은 15초, 읽기 대기는 30초(느려도 흐르는 회선은 25MB 한도 안에서 끝낼 수 있다)이고 전체는 30분이다.

앱에서는 플러그인 화면의 **둘러보기**에서 `목록 불러오기`를 눌러야 인터넷에 연결한다. `설치`를 누르면 앱이 받아 둔 목록의 그 항목이 고정한 커밋을 내려받아(화면은 저장소나 커밋을 직접 지정하지 못한다) 위와 같은 동의 창(선언된 권한, 해시, "검증되지 않았습니다")을 보여 주고, `추가`를 눌러야 설치된다. 내려받은 매니페스트의 id, 버전, 네트워크·오디오 권한, API 키·설정 이름 목록이 목록과 하나라도 다르면 미리보기 단계에서 거부한다. 미리보기마다 토큰이 붙고, 더 새 미리보기가 열렸으면 이전 창의 `추가`는 거부된다.

## 목록(갤러리)이 확인하는 것과 확인하지 않는 것

`tools/plugin-index/build_index.py`가 하루 한 번 `meetko-plugin` topic 저장소를 찾아 `index.json`을 만든다(예시 워크플로: `tools/plugin-index/gallery-workflow.yml`. 목록 저장소 [`Eastsidegunn/meetko-plugins`](https://github.com/Eastsidegunn/meetko-plugins)에 복사해서 쓴다. 앱은 기본으로 `https://raw.githubusercontent.com/Eastsidegunn/meetko-plugins/main/index.json`을 읽는다). **Python 3.11 이상**이 필요하다(`tomllib`; 더 낮으면 안내 후 종료). 표준 라이브러리만 쓴다.

`meetko.toml`은 GitHub Contents API가 아니라 **그 커밋의 tarball 안에서** 읽는다. codeload tarball은 `git archive` 결과라서 `.gitattributes`의 `export-subst`가 적용되어 원본 파일과 다를 수 있고, 사용자가 설치하는 것은 tarball 쪽이기 때문이다. tarball은 크기 상한을 두고 스트리밍으로만 읽으며 디스크에 풀지 않는다. 앱도 설치 전에 내려받은 매니페스트를 목록과 다시 대조한다.

확인하는 것:

- `meetko.toml`이 있고 파싱된다. 필수 필드, `kind`, `contract` 호환, 프로토콜, 실행 경로(절대 경로나 `..` 금지).
- `permissions.secrets`/`settings`가 있으면 코어와 같은 규칙(이름 모양과 끝맺음, 예약 이름, 각각 최대 4개, `network = true`)을 검사하고, 목록 항목의 `permissions.secrets`/`settings`로 옮긴다(선언했을 때만 생기는 필드).
- `package.id`의 게시자가 저장소 소유자와 같다. 같은 id는 별이 많은 저장소 하나만 오른다.
- 매니페스트의 표가 표 모양인지, 문자열에 제어·방향 문자가 없는지. 저장소 하나의 매니페스트가 이상해도 그 저장소만 건너뛰고 나머지는 계속한다.
- 보관됨(archived), 포크, 비공개 저장소는 제외한다.

확인하지 **않는** 것:

- **코드를 보지 않는다. 사람의 검토가 없다.** 목록에 있다는 것은 "형식이 맞다"는 뜻일 뿐 안전하다는 뜻이 아니다.
- 선언한 권한이 사실인지 확인하지 않는다. macOS에서는 샌드박스가 선언 없는 네트워크와 홈·데이터 폴더 읽기를 막지만, Windows와 Linux에서는 아직 격리가 없어 권한이 선언일 뿐이고 플러그인이 내 계정 권한으로 실행된다(`docs/plugin-security.md`). 네트워크를 선언한 플러그인은 자기가 받은 전사본을 밖으로 보낼 수 있다.
- 설치 뒤 저장소가 바뀌거나 사라지는 것은 목록과 상관없이 커밋 고정으로만 막는다.
- 이름 사칭을 완전히 막지는 못한다. 설명과 저장소 주소를 직접 보고 고른다.

## 나쁜 플러그인 신고

목록 저장소(`Eastsidegunn/meetko-plugins`)의 GitHub issue로 저장소 주소, 커밋, 무엇이 문제인지(악성 동작, 사칭, 선언과 다른 권한 사용 등)를 적는다. 관리자는 목록에서 뺄 수 있다(`blocklist.txt`: 한 줄에 `OWNER/REPO`, 소유자 전체를 막는 `OWNER/*`, 또는 40자 커밋 sha). 이미 설치한 사람의 컴퓨터에서는 자동으로 지워지지 않는다. `meetko plugin list`로 출처를 확인할 수 있다. (제거 명령은 아직 없다.) 보안 취약점은 공개 issue 대신 저장소의 보안 연락 방법을 쓴다.

## 목록 빌드 안전장치 (관리자용)

- GitHub 검색이 200이 아니면 빌드가 **실패**한다(줄어든 목록을 커밋하지 않는다).
- `--previous index.json`을 주면 새 목록의 플러그인이 이전의 절반보다 적을 때 실패한다. 의도한 변경이면 `--allow-shrink`.
- 예시 워크플로는 읽기 전용 `build` 작업과, 만들어진 파일만 커밋하는 `commit` 작업으로 나뉜다. `build`는 읽기 전용 `GITHUB_TOKEN`을 받지만(API 한도 때문에 필요), 저장소 인증 정보는 남기지 않는다(`persist-credentials: false`). 쓰기 권한은 `commit`에만 있다. checkout과 setup-python은 커밋 sha로 고정했고(이 저장소의 CI와 같은 값), 아티팩트 액션 둘은 아직 태그라서 sha로 고정하는 TODO가 적혀 있다.
