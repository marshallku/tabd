---
name: tabd
description: 헤드리스 브라우저(tabd)로 웹 페이지 자동화. 사용자가 "이 사이트의 데이터", "로그인", "스크래핑", "API 응답 캡쳐", "스크린샷", "쿠키", "form 자동 채우기", "페이지 모니터링" 같은 작업을 요청하거나 chromium이 필요한 모든 상황에서 사용. SSH 친화적 단일 Rust binary, daemon-shared chromium.
user-invocable: false
allowed-tools: Bash, Read
effort: low
---

# tabd

`tabd`는 chromium을 daemon으로 띄워두고 CLI로 조작. 단일 Rust binary (Node/Python/MCP 의존 없음). 한 번 daemon이 뜨면 후속 CLI 호출이 같은 chromium·쿠키·세션·탭을 공유.

별도 MCP server나 SDK 없음. `Bash` 도구로 `tabd <action>`을 그냥 호출하면 됨.

## 언제 활성화

사용자 요청 중 다음에 해당하면 tabd 사용을 고려:

- "이 페이지의 ...을 가져와줘" — 스크래핑 / 데이터 추출
- "이 사이트 로그인해서 ..." — 인증된 세션이 필요한 워크플로
- "이 페이지 스크린샷" — 시각적 캡쳐
- "이 API 응답 캡쳐" — 백엔드 통신 관찰
- "form 채우고 제출"
- "쿠키 / localStorage 어떻게 돼있어?"
- "이 페이지 console.log / 네트워크 요청 뭐가 일어나?"
- 외부 fetch로 안 되는 사이트 (anti-bot, JS-rendered, auth-walled)

## 첫 호출

```bash
# daemon 자동 spawn — 첫 액션이 호출되면 자동으로 띄움. 명시적 확인 옵션:
tabd daemon ping 2>/dev/null || true

# 곧장 액션
tabd navigate https://example.com
tabd get-text --selector h1
tabd screenshot --out /tmp/page.png
```

데이터 추출의 80%는 `tabd eval` + 브라우저 내 `fetch`가 정답. 쿠키·CSRF·세션 다 자동 적용:

```bash
tabd eval 'await fetch("/api/data").then(r => r.json())' --json
```

`tabd` 외 별도 인증 처리 / cURL fallback이 필요한 케이스가 더 드물다는 점 인지하고 진행.

## 핵심 명령

| 작업 | 명령 |
|---|---|
| 페이지 이동 | `tabd navigate <url>` |
| 텍스트 추출 | `tabd get-text --selector <css>` |
| 페이지 요약 (LLM-friendly) | `tabd summary --json` |
| JS 실행 / fetch | `tabd eval '<js>' --json` |
| 스크린샷 | `tabd screenshot --out file.png` |
| 모바일/반응형 뷰 | `tabd set-viewport 390 844 --mobile` |
| 파일 다운로드 받기 | `tabd download-dir ./out` (1회) → 클릭 → `tabd wait-download --json` (savedPath) |
| 클릭 / 타이핑 | `tabd click <sel>` / `tabd click --text "로그인"` (셀렉터 모를 때) / `tabd type <sel> <text>` |
| 비밀번호 입력 | `tabd type-secret <sel> --secret-id <id>` |
| 파일 업로드 | `tabd upload <sel> <file>` (`<input type=file>` 전용, 숨겨진 input OK) |
| 로딩 대기 | `tabd wait-selector <sel>` / `tabd wait-url <pat> --pattern-type glob` / `tabd wait-text "문구"` |
| 네트워크 idle | `tabd wait-network-idle --idle-time 1500` |
| 쿠키 / 네트워크 로그 | `tabd cookies-get <url> --json` / `tabd network-logs --url-contains /api/ --json` |
| 새 탭 / 활성 전환 | `tabd open-tab <url>` / `tabd activate-tab --tab N` |

모든 옵션과 응답 shape은 같은 디렉터리의 `commands.md`에. 시나리오 패턴은 `cookbook.md`. 복잡한 요청이면 두 문서를 `Read` 도구로 펼친 후 진행.

`tabd <action> --help`는 동작 안 함 (clap external_subcommand catch-all) — commands.md가 그 역할.

## 시나리오 → cookbook 매핑

| 사용자 요청 유형 | cookbook 섹션 |
|---|---|
| 로그인 + 2FA + 데이터 추출 | §1 |
| API 응답 캡쳐 (3가지 패턴 비교) | §2 |
| 매번 로그인 안 하게 세션 저장/복원 | §3 |
| 두 사이트 비교 (다탭) | §4 |
| 무한 스크롤 모든 항목 | §5 |
| CI / 한 번만 쓰는 격리 daemon | §6 |
| 특정 버튼 후 응답 캡쳐 | §7 |

## AI가 자주 빠지는 함정

1. **`tabd eval` 응답이 비어 보임** → JS가 `undefined` 반환. IIFE의 마지막에 `return value` 또는 `null`. JSON 추출은 `--json` 플래그.

2. **SPA에서 `type`/`click`이 element 못 찾음** → navigate 직후 element가 아직 마운트 안 됨. `tabd wait-selector '...' --timeout 10000` 한 줄 끼울 것. `type`/`click` 자체는 내부적으로 30s 대기하지만 element가 그 시간 안에 나타나야 함.

3. **`wait-network-idle --idle-time 500` (디폴트)이 SPA에 짧음** → 디바운스 fetch 많은 페이지는 `--idle-time 1500 --timeout 15000`.

4. **`screenshot`은 active tab만** — 다른 탭 찍으려면 `activate-tab --tab N` 선행.

5. **`cookies-set`은 origin context 필요** → `navigate https://app.example.com/` 후에 호출. 그렇지 않으면 cookie 거부될 수 있음.

6. **비밀번호는 `secret-put` → `type-secret`**. argv에 plaintext 절대 금지. `secret-put`은 `--from-env VAR` / `--from-file PATH` / `--stdin` 셋 중 하나.

7. **`network-logs --include-body`는 deferred** — 응답 본문 안 들어옴. 본문 필요하면 같은 URL을 `tabd eval` + `fetch`로 재호출.

8. **daemon 재시작 = 세션 소실** (cookies / localStorage / 열린 탭 모두 chromium TempDir). 영속화 필요하면 cookbook §3 (cookies-get + storage-get 후 재주입).

9. **JS dialog (alert/confirm/prompt)는 daemon이 자동 처리** — 기본 dismiss, `beforeunload`는 자동 accept (navigation 막힘 방지). confirm을 수락해야 하는 플로우면 클릭 **전에** `tabd dialog-policy accept` (필요시 `--prompt-text`). 무슨 dialog가 떴고 어떻게 처리됐는지는 `tabd dialogs --json`으로 감사. 이미 열린 dialog를 나중에 응답하는 건 구조적으로 불가능 (action lock) — policy는 사전 설정.

10. **iframe 안 요소는 `--frame '<iframe selector>'`로 접근** — `get-text`/`get-html`/`query`/`click`/`type`/`wait-selector`/`wait-text`에서 지원. same-origin 프레임만 가능하고 cross-origin이면 `invalid_request`로 즉시 실패 (결제 위젯 등 cross-origin iframe은 자동화 불가 — 사용자에게 한계 안내).

11. **다운로드는 기본 버려짐 — `download-dir <dir>`로 opt-in 후 캡처**. 파일은 guid로 저장(`<dir>/<guid>`)되고 원래 이름은 `downloads`/`wait-download`의 `suggestedFilename`에. `wait-download`의 `savedPath`로 받아서 직접 rename. daemon/chromium 재시작하면 다시 `download-dir` 호출해야 함. tabd는 저장 파일을 절대 안 지움.

12. **`get-html`/`get-text`/`eval` 출력은 기본 500k chars에서 잘림** (`…[truncated: …]` 마커 부착). 전체가 필요하면 `--max-chars 0`, 더 줄이려면 `--max-chars 5000` 등. 객체를 반환하는 `eval`이 한도를 넘으면 `output_too_large` 에러 — 표현식에서 덜 가져오게 좁힐 것.

## 시크릿 (vault) 사용

`secret-*` 액션과 `type-secret`은 `$TABD_VAULT_KEY` 환경 변수가 daemon에 있어야 함. 사용자 환경에 없으면 안내:

```bash
echo "TABD_VAULT_KEY가 필요합니다. ~/.config/tabd/vault.env (mode 0600)에:" >&2
echo "  TABD_VAULT_KEY=<passphrase>" >&2
echo "그 후 'source ~/.config/tabd/vault.env' 또는 systemd EnvironmentFile에 등록" >&2
```

저장된 secret ID는 출력에 노출돼도 무해 (vault의 random ID, plaintext 아님). 사용자에게 출력해도 됨.

## 격리된 daemon (사용자 daemon 안 건드리기)

긴 스크립트 / CI성 작업은 사용자의 default daemon과 분리:

```bash
BASE="$(mktemp -d -t tabd-job.XXXX)"
export TABD_BASE_DIR="$BASE"
trap 'tabd daemon stop --base-dir "$BASE" 2>/dev/null; rm -rf "$BASE"' EXIT
# ... tabd 액션들 ...
```

세부 패턴은 cookbook §6.

## 출력 다루기

- `--json` 없이 호출하면 stdout이 사람-친화 (큰 객체는 pretty-print). 파이프로 jq 등에 넘길 거면 `--json` 필수.
- 에러는 stderr `error: <message> [<errorCode>]` + nonzero exit. `--json`이면 실패 envelope `{success:false, error, errorCode}`가 stdout으로 나옴. `set -e` 환경에서 자연스럽게 트랩됨.
- **에러 분기는 errorCode/exit code로** (메시지 텍스트 파싱 금지): exit `5` = `selector_not_found`/`tab_not_found` (셀렉터 재탐색), `4` = `timeout` (재시도 또는 `--timeout` 증가), `3` = `daemon_unreachable` (daemon 재시작), `1` = 그 외 (`eval_error`, `vault_error`, `invalid_request`, `cdp_not_ready`, `internal`). 전체 표는 commands.md "Errors & exit codes".
- 큰 응답 (HTML, 스크린샷 base64) 은 그대로 stdout으로 쏟지 말고 `--out FILE` 또는 파일로 redirect.

## 더 알아야 할 때

이 skill 디렉터리(`{{SKILL_DIR}}`) 안에 docs 4개가 함께 install됨. Read 도구로 직접 접근:

- `Read {{SKILL_DIR}}/commands.md` — 39개 액션의 옵션 / 응답 shape (가장 자주 lookup)
- `Read {{SKILL_DIR}}/cookbook.md` — 시나리오 코드 패턴 (login + 2FA, API 캡쳐 3패턴, 세션 저장/복원, 무한 스크롤, CI 격리 daemon)
- `Read {{SKILL_DIR}}/operations.md` — daemon 자체 운영 (systemd / launchd / drain / health 모니터)
- `Read {{SKILL_DIR}}/architecture.md` — 왜 이렇게 설계됐는지 (multi-tab registry, reader-task deadlock, supervisor 등)

복잡한 워크플로일수록 commands.md를 먼저 펼쳐서 해당 액션의 옵션·응답을 확인한 후 코드를 짜는 게 errcase 줄임.
