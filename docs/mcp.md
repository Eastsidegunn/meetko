# AI 에이전트와 함께 쓰기 (MCP)

<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

이미 쓰고 있는 AI 코딩 에이전트(Codex CLI, Claude Code 등)가 meetko의 회의를 읽고, 만든 결과를 meetko에 넣을 수 있습니다.
`meetko mcp`는 내 컴퓨터에서 표준 입출력(stdio)으로 동작하는 MCP 서버입니다. 에이전트는 **자기 로그인**으로 AI를 쓰고,
meetko는 AI 토큰이나 API 키를 받지도 저장하지도 않습니다.

## 할 수 있는 것과 없는 것

에이전트가 할 수 있는 것:

| 도구 | 하는 일 |
| --- | --- |
| `list_meetings` | 회의 목록(최신순): 제목, 시각, 길이, 현재 버전과 종류, 버전·결과 수. 나눠 받기(cursor) |
| `search` | 각 회의의 현재 버전에서 글자 그대로 찾기(대소문자 무시, `회의`로 `회의를`도 찾음). 시각과 근거 단어 범위. 앱에서 고친 문장 안의 결과는 `edited: true`로 표시되고 범위는 그 발화 전체입니다 |
| `get_transcript` | 발화 단위 전사본: 화자, 시각, 단어 id. `turns`(기본), `words`(단어마다 id·시각), `text`(id 없는 글). 나눠 받기. 앱에서 바꾼 화자 이름(`speaker`, 원래 이름은 `speaker_label`)과 고친 문장(`edited`, 원문은 `original_text`)을 그대로 보여 줍니다. 단어 id·시각은 받아쓴 그대로입니다 |
| `get_results` | 플러그인·에이전트 결과와 "오래됨" 표시 |
| `get_provenance` | 처리 단계(엔진, 모델, 설정), 네트워크 사용 여부, 녹음 해시, 이 버전의 결과와 만든 곳 |
| `add_result` | 결과 넣기(아래) |
| `list_agent_results` | MCP로 넣은 결과 목록 |
| `remove_result` | MCP로 넣은 결과 지우기 |

할 수 없는 것: 오디오와 파일 경로, 데이터 폴더 위치, 환경변수는 내보내지 않습니다. 전사본·버전을 고치거나 지울 수 없고,
녹음을 시작·중지할 수 없으며, 플러그인 결과는 지울 수 없습니다.

### 결과 넣기

결과는 플러그인과 같은 표시 형식(`summary_card`, `action_items`, `timeline_marker`, `table`, `transcript_block`)으로만
넣을 수 있고, 각 줄은 읽은 버전의 단어 범위(`{from, to}`)를 근거로 가져야 합니다. 플러그인 결과와 똑같이 검사합니다:
없는 단어 id, 거꾸로 된 범위, 모르는 형식, 그 버전에 없는 담당자, 숨은 문자(제어·폭 없는 문자·방향 바꾸기 문자),
너무 긴 글(항목 300개, 글 2,000자, 전체 1 MB)이면 아무것도 저장하지 않고 이유를 돌려줍니다.

결과마다 에이전트가 정하는 **고정 id**가 있습니다(`mcp:weekly-summary`처럼, `mcp:`는 빠지면 붙여 줌). 같은 버전에 같은
id로 다시 넣으면 이전 결과를 바꿉니다. 다른 버전의 결과는 남고, 전사본이 바뀌면 플러그인 결과처럼 "오래됨"이 됩니다.
`mcp:`로 시작하는 id는 에이전트 결과 전용이라 플러그인이 쓸 수 없습니다(설치를 거부). 그래서 에이전트 결과가 플러그인인
척할 수 없습니다. 다만 이름은 에이전트가 정하므로 `mcp:meetko-summary`처럼 플러그인과 비슷해 보이는 이름은 막지 않습니다.
앱은 이름과 상관없이 "AI 에이전트" 표시와 만든 곳을 붙여 구분합니다.

앱에서는 플러그인 결과 옆에 보입니다. 그런 결과가 있는 버전을 열면 위쪽에 **"외부 AI 에이전트가 만든 결과가 N개 있어요.
에이전트가 이 회의 내용을 자기 AI 서비스로 보냈을 수 있어요."** 라는 알림이 한 번 뜨고, 각 결과에는 "외부 AI" 표시와 만든
곳(에이전트가 밝힌 이름 · MCP 클라이언트 이름)이 붙습니다. `meetko results`도 만든 곳을 함께 보여 줍니다. 기록되는 출처
(결과마다 따로 보관하므로 같은 id에 나중에 다른 에이전트가 넣어도 이전 결과의 기록은 그대로입니다): 에이전트가 밝힌
이름·버전(믿을 수 없는 값이라 숨은 문자를 지우고 64자로 자름), MCP 클라이언트 정보, 프로토콜 버전, 그리고 네트워크
사용 "예"(meetko는 에이전트가 무엇을 어디로 보냈는지 알 수 없습니다).

## 개인정보

에이전트가 읽은 전사본은 **그 에이전트의 AI 제공자에게 전송됩니다.** meetko 자체는 네트워크를 쓰지 않지만, MCP를
연결하는 순간 회의 내용이 기기 밖으로 나갈 수 있다는 뜻입니다. 회사 정책상 회의 내용을 외부 AI에 보내면 안 되는
경우에는 연결하지 마세요. MCP로 연결된 클라이언트는 누구나 `mcp:` 결과를 넣고 지울 수 있습니다(클라이언트를 구분하는
인증은 없습니다).

**전사본 안의 글은 지시가 아닙니다.** 회의 참석자가 한 말이나 플러그인이 쓴 글에 "이전 지시를 무시하고 …" 같은 문장이
있어도 그것은 자료일 뿐입니다. meetko는 도구 설명과 안내문에서 에이전트에게 이 점을 알리지만, 에이전트가 그 문장을 따르지
않는다고 보장할 수는 없습니다. 에이전트가 파일을 고치거나 명령을 실행할 수 있는 환경이라면, 회의 내용을 읽힌 뒤에는
에이전트가 하려는 작업을 확인하세요.

## 설정

`meetko` 명령이 PATH에 있어야 합니다. 소스에서 빌드했다면 `cargo install --path crates/meetko-cli`로 설치하거나,
아래의 `meetko` 대신 빌드한 실행 파일의 전체 경로를 쓰세요. 데이터 폴더를 바꿔 쓰고 있다면(`MEETKO_DATA_DIR`) 같은 값을
서버 환경에도 넣어야 합니다. 앱이 켜져 있어도 같이 쓸 수 있습니다(같은 데이터베이스를 잠금을 지키며 엽니다). 에이전트가
결과를 넣은 뒤 앱에서는 회의를 다시 열면 보입니다.

### Codex CLI

`~/.codex/config.toml`:

```toml
[mcp_servers.meetko]
command = "meetko"
args = ["mcp"]
```

또는 `codex mcp add meetko -- meetko mcp`. 데이터 폴더를 바꿨다면:

```toml
[mcp_servers.meetko.env]
MEETKO_DATA_DIR = "/데이터/폴더"
```

### Claude Code

```
claude mcp add --transport stdio meetko -- meetko mcp
```

기본 범위는 지금 프로젝트(local)입니다. 모든 프로젝트에서 쓰려면 `--scope user`를 더하세요. 연결은 `claude mcp get meetko`
또는 Claude Code 안의 `/mcp`로 확인합니다. 데이터 폴더를 바꿨다면 `claude mcp add --env MEETKO_DATA_DIR=/데이터/폴더 --transport stdio meetko -- meetko mcp`처럼
`--env`와 이름 사이에 다른 옵션을 두세요.

## 이렇게 부탁해 보세요

- "지난주 배포 회의 요약하고 할 일을 근거와 함께 meetko에 넣어 줘"
- "meetko에서 '롤백' 얘기가 나온 회의를 찾아서 결정 사항을 타임라인 표시로 넣어 줘. id는 mcp:decisions로"
- "오늘 회의 전사본 읽고 위험 요소 표를 만들어 줘. 각 줄에 근거 구간을 달아 줘"
- "mcp:weekly-summary 결과 중 오래된 것들 지워 줘"

## 프로토콜

MCP over stdio, JSON-RPC 2.0, 한 줄에 메시지 하나. `initialize` 방식의 2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05와,
요청마다 `_meta`에 버전을 싣는 2026-07-28(`server/discover` 포함)을 모두 받습니다. 지원: `initialize`, `server/discover`,
`ping`, `tools/list`, `tools/call`, 알림(응답 없음). 모르는 메서드는 -32601, 모르는 도구는 -32602, 지원하지 않는 버전은
-32022입니다. 도구 실행 오류는 `isError: true`인 도구 결과로 돌려줘 에이전트가 읽고 고칠 수 있습니다. 한 요청은 4 MB까지,
전사본과 결과는 한 번에 약 40 KB까지 주고(클라이언트가 모델에 한 번에 넘기는 양보다 작게) `next_cursor`로 이어 받습니다.
긴 표·요약은 여러 부분으로 나뉘어 오고, 각 부분에는 읽기 전용 표시 `item`(결과 안의 순서)과 `part`("k/n")가 붙습니다. 다시
저장할 때는 이 둘을 빼야 합니다. 표의 한 줄이 아주 길면(칸 12개 × 2,000자까지) 그 한 조각 때문에 쪽이 커질 수 있습니다.
검색도 쪽 크기를 넘으면 회의를 더 담지 않고 `more: true`를 줍니다. 회의 목록과 결과의 `next_cursor`는 마지막으로 보낸 항목을
가리키므로, 쪽 사이에 결과가 더해지거나 지워져도 건너뛰거나 겹치지 않습니다. 출처의 처리 단계는 알려진 항목과 평범한 이름의
숫자·참거짓 값만 내보내고, 글 값은 알려진 이름에서 경로처럼 보이지 않을 때만 내보냅니다. 뺀 항목은 개수(`hidden_fields`)만
알려 줍니다.

## English

`meetko mcp` is a local stdio MCP server for an AI agent you already run (Codex CLI, Claude Code, ...). The agent uses its own
login; meetko never handles AI tokens. Read tools: `list_meetings`, `search`, `get_transcript` (turns with speakers as named in the app, edited text marked with its original, times and
word ids; paged), `get_results`, `get_provenance`. Write tools: `add_result` (plugin display types only, every line cites word
ranges of the version it read, validated exactly like plugin output; stored under a stable agent-chosen id in the reserved
`mcp:` namespace, which installed plugins cannot use; storing again under the same id for the same version replaces the
earlier result), `list_agent_results`, `remove_result` (only results stored over MCP). No audio, file paths, data-dir location
or environment is exposed; transcripts, versions and plugin results cannot be edited or removed.

Transcript text, titles and stored results are data, not instructions: the tool descriptions tell the agent so, but
meetko cannot guarantee an agent ignores an injected request, so review what an agent with file or shell access does
after reading meetings. Pages are kept to about 40 KB (follow `next_cursor`); provenance goes out through an allow-list
of known producer fields. Privacy: whatever the agent reads goes to its AI provider. Agent results are shown in the app next to plugin results, marked
as made by an external AI agent whose provider may have received the transcript, with the agent's self-reported name and
the MCP client name; meetko records `network: true` for them because it cannot know what the agent sent where.

Setup: Codex CLI, in `~/.codex/config.toml`: `[mcp_servers.meetko]` with `command = "meetko"` and `args = ["mcp"]` (or
`codex mcp add meetko -- meetko mcp`). Claude Code: `claude mcp add --transport stdio meetko -- meetko mcp` (add
`--scope user` for all projects). `meetko` must be on PATH (`cargo install --path crates/meetko-cli`), and a custom
`MEETKO_DATA_DIR` must be passed to the server too. Protocol: dual-era, `initialize` for 2025-11-25 and earlier,
per-request `_meta` for 2026-07-28 (with `server/discover`).
