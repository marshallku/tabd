# Rust + CDP spike

기존 TS daemon과 별개로, Rust에서 Chromium을 CDP(Chrome DevTools Protocol)로 직접 다루는
spike crate를 만든다. 목적은 두 가지:

1. **동작 동등성 확인** — `src/server/runtimes/cdp.ts`의 최소 동작(navigate + textContent
   추출 + 임의 JS evaluate)을 Rust로 재현해, 같은 입력에 같은 출력을 내는지 본다.
2. **Playwright 의존성 제거 가능성 측정** — 시스템 Chromium 재사용 시 Playwright 번들
   Chromium(~250MB) 없이 실용 가능한지 + Node 런타임/`playwright-core` 제거가 얼마나
   유의미한 절감인지 수치로 본다.

## Non-goals (이 spike에서 안 함)

- daemon 통합 / UDS 프로토콜 호환
- MCP wiring
- tabs / cookies / storage / persistent secrets
- macOS / Windows 검증 (Linux 한정)
- 프로덕션 품질 (에러 메시지, 재시도, 로깅 등 최소)
- **TS `get-text`의 풀 시맨틱 재현** — 1차 단계는 단일 selector + `textContent`만.
  `innerText` / 공백라인 압축 / fallback 셀렉터(`"main, article, body"` → `body`)는
  spike 후속 phase 1에서 다룬다 (codex C2)

기존 TS 코드는 **한 줄도 안 건드린다**. spike가 회수되어도 손해 없는 구조 유지.

## TS와의 동등성 범위 (executable spec)

참조 spec: `src/server/runtimes/cdp.ts` (line 400 ~ 875 부근) + `tests/e2e-cdp-parity.mjs`.

| 측면 | TS 현재 동작 | spike 1차 목표 | spike 후속 phase |
|---|---|---|---|
| 런치 플래그 | `--no-first-run --disable-dev-shm-usage --disable-background-networking --disable-sync --disable-extensions --no-sandbox` | 동일하게 다 붙임 | — |
| endpoint 결정 | stderr `DevTools listening on ws://...` 파싱 + `/json/version` readiness 확인 | 동일 | — |
| navigate 완료 신호 | `Page.navigate` + `document.readyState` 폴링 | 동일 (`loadEventFired`는 보조 신호로만) | — |
| 텍스트 추출 selector | default `"main, article, body"` → fallback `body` | 명시 selector만 (default 없음) | default + fallback |
| 텍스트 추출 방식 | `innerText`, 공백라인 압축, trim, `raw` 옵션 시 `textContent` | `textContent` 단독 | `innerText` + 압축 + trim |
| evaluate | `Runtime.evaluate(code, returnByValue: true)` | 동일 | — |

**1차 parity 검증 방법**: TS 측에 동일 `textContent` 추출 명령을 직접 invoke해서 (spike와
같은 단일 selector + raw textContent 시나리오) 비교. `e2e-cdp-parity.mjs`는 TS 풀 시맨틱
기준이라 spike와 직접 diff하지 않고, 동일 selector/expression에 대한 stdout만 비교.

## 디렉터리

```
crates/
  cdp-spike/
    Cargo.toml
    src/
      main.rs        # clap CLI 진입점
      browser.rs     # Chromium spawn + endpoint 결정 (/json/version 폴링)
      cdp.rs         # WebSocket + JSON-RPC 클라이언트
      cmd/
        navigate.rs
        eval.rs
        fetch_text.rs
```

이름을 `cdp-spike`로 둔 이유: 이게 graduate해서 진짜 core가 되기 전까지 "스파이크"라는
신호를 이름에 남겨두고 싶다. 본격 통합 결정 시 `ai-browser-core`로 rename.

## 백엔드

**Chromium + CDP over WebSocket**. FFI 없음.

CDP는 단순한 JSON-RPC over WebSocket이므로 Rust ↔ Chromium 연동은 소켓+JSON 수준에서 끝난다.
C++ 임베드/바인딩 불필요.

## Chromium 바이너리 탐색 순위

1. `$BROWSER_EXECUTABLE` env (TS 쪽과 변수명 통일)
2. 시스템 PATH: `google-chrome` → `google-chrome-stable` → `chromium` → `chromium-browser`
3. Playwright cache: `~/.cache/ms-playwright/chromium-*/chrome-linux64/chrome` (개발 머신 fallback)
4. 없으면 actionable error: 설치 안내 메시지

## Chromium 런치 플래그 (TS 측 + 보강)

```
--headless=new
--disable-gpu
--no-first-run
--no-default-browser-check
--no-sandbox
--disable-dev-shm-usage
--disable-background-networking
--disable-sync
--disable-extensions
--remote-debugging-address=127.0.0.1    # 보강 — TS에는 없는 loopback 명시 (codex I3)
--remote-debugging-port=0
--user-data-dir=<tempfile::TempDir>
about:blank                              # 초기 URL — TS와 동일
```

TS와 동일 + `--remote-debugging-address=127.0.0.1` 추가 (codex I1). port=0 + 자동 할당.

## Endpoint 결정 전략 (codex C2 — 일관성 수정)

port=0 이면 사전에 포트를 알 수 없으므로 **stderr 파싱이 critical path**.
`/json/version` 은 fallback이 아니라 readiness 확인용:

1. spawn 직후 stderr line-by-line read (deadline 10s) → 첫 번째
   `DevTools listening on ws://127.0.0.1:<port>/devtools/browser/<uuid>` 라인 캡처
2. 캡처한 URL의 port로 `GET http://127.0.0.1:<port>/json/version` 폴링 (200ms 간격, 5s timeout)
   — 응답 200 + `webSocketDebuggerUrl` 필드 존재 시 ready
3. `/json/version` 응답의 `webSocketDebuggerUrl` 사용 (stderr 캡처한 URL은 polling 대상
   port 알기 위한 도구로만 사용)
4. stderr 라인 캡처 실패 (Chromium 버전 차이로 line format 변경) 시 → fail-fast, actionable
   에러 (`"failed to capture DevTools endpoint from chromium stderr within 10s"`)

대안 — pre-bind port (TCP listener 잡았다 놓고 그 port를 `--remote-debugging-port=<N>`로
전달)는 race-prone이라 채택 안 함. stderr 파싱 한 줄에 자신감 가는 게 더 단순.

## 의존성

```toml
[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "io-util", "process", "time", "sync"] }
tokio-tungstenite = "0.24"
futures-util = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
clap = { version = "4", features = ["derive"] }
anyhow = "1"
tempfile = "3"           # disposable --user-data-dir (codex I2)
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }  # /json/version probe
```

`chromiumoxide` 같은 기성 crate는 **일부러 안 쓴다** — 의존성 트리/footprint 측정 정확도
+ CDP 학습 동기. 단 navigate가 1~2일 안에 안 뚫리면 chromiumoxide로 1시간 spike 따로
돌려서 우리 구현 문제냐 환경 문제냐 가른다.

## 서브커맨드 (CLI 표면)

> **세션 한계 인지**: 이 spike는 daemon이 없다. 매 invocation마다 새 Chrome 한 번 spawn →
> 한 페이지 작업 → 종료. 그래서 명령은 "한 페이지 안에서 끝나는 단위"로 설계 (codex C1).

| 명령 | 동작 |
|---|---|
| `cdp-spike navigate <url> [--timeout 30000]` | 페이지 이동 + readyState 폴링까지. 종료 코드만 본다 |
| `cdp-spike eval <url> <expr> [--json] [--timeout 30000]` | navigate 후 `Runtime.evaluate(expr, returnByValue: true)` → stdout |
| `cdp-spike fetch-text <url> <selector> [--timeout 30000]` | navigate 후 `document.querySelector(SEL)?.textContent ?? ""` → stdout |

`fetch-text`는 "TS `get-text`와 동등"이라고 부르지 **않는다** — phase 1 follow-up까지는
명시 selector + raw textContent 한정 (codex C2).

## CDP 시퀀스 (fetch-text 케이스)

1. tempfile::TempDir 생성 → `--user-data-dir`
2. Chromium spawn (위 런치 플래그 전부)
3. stderr scan으로 `DevTools listening on ws://127.0.0.1:<port>/...` 라인 캡처 (필수 — port=0이므로 사전에 알 수 없음)
4. `/json/version` 폴링 → `webSocketDebuggerUrl`
5. Browser-level WebSocket 연결
6. `Target.createTarget {url: "about:blank"}` → targetId
7. `Target.attachToTarget {targetId, flatten: true}` → sessionId
8. 이후 모든 메시지에 `sessionId` 부착
9. `Page.enable`, `Runtime.enable`
10. `Page.navigate {url}` → response의 frameId 기억
11. `document.readyState` 폴링 (200ms, `interactive` 또는 `complete`까지)
    - `Page.loadEventFired` 이벤트는 보조 신호로 listen만 함
12. `Runtime.evaluate { expression, returnByValue: true }` —
    `expression` 은 `format!("(document.querySelector({})?.textContent) ?? ''", serde_json::to_string(&selector)?)`
    로 안전 임베드 (codex C3). 따옴표/백슬래시/유니코드 selector도 깨지지 않음
13. `Browser.close` → child wait → TempDir drop으로 정리

## 성공 기준 (codex C1 — 실재 TS CLI 표면과 정합)

```bash
# spike
cargo run --release -- navigate "data:text/html,<h1>Hi</h1>"               # exit 0
cargo run --release -- eval "data:text/html,<h1>Hi</h1>" "document.title"  # ""
cargo run --release -- fetch-text "data:text/html,<h1>Hi</h1>" "h1"        # "Hi"

# 비교 대상 (TS) — 같은 selector + raw textContent 시나리오를 Node 스크립트로 직접
# invoke해서 stdout diff. createRuntime()은 인자 없이 process.env.BROWSER_RUNTIME 읽고
# BrowserDriver(init/close/execute)만 노출. execute는 BridgeResponse{id,success,data}
# 반환 — value 분해 아님 (codex C1).
#
# 데몬/기본 Chrome 프로필 충돌 방지를 위해 별도 tmp profile + 비-기본 debug port 사용
# (codex C2):
TMP=$(mktemp -d)
node --input-type=module -e "
  process.env.BROWSER_RUNTIME = 'chromium-cdp';
  process.env.BROWSER_USER_DATA_DIR = '$TMP';
  process.env.BROWSER_DEBUG_PORT = '19222';  // 비-기본 포트, 데몬 9222와 충돌 회피
  const { createRuntime } = await import('./dist/server/runtime.js');
  const r = createRuntime();
  await r.init();
  const nav = await r.execute('tabs.navigate', { url: 'data:text/html,<h1>Hi</h1>' });
  if (!nav.success) { console.error(nav); process.exit(1); }
  const res = await r.execute('execution.executeJs', {
    code: \"document.querySelector('h1')?.textContent ?? ''\",
  });
  if (!res.success) { console.error(res); process.exit(1); }
  console.log(res.data);
  await r.close();
"
rm -rf \"$TMP\"
```

마지막 stdout이 spike fetch-text 출력과 byte-exact match면 1차 parity 통과.
**`example.com` 같은 외부 URL은 smoke에서 빼고** `data:` URL로 결정적으로 (codex I4).

## 측정 항목 (codex C5 — clean breakdown)

footprint 비교 표. 단위 MB.

| 컴포넌트 | 측정 방법 | 현재 (Playwright 기본) | 현재 (chromium-cdp + 시스템) | spike (Rust + 시스템) |
|---|---|---|---|---|
| Node 런타임 | `du -sh $(which node)` 의존 트리 | ~50 | ~50 | 0 |
| `playwright-core` + deps | `du -sh node_modules/` | TBD | TBD | 0 |
| 번들 Chromium | `du -sh ~/.cache/ms-playwright/chromium-*/` | ~280 | 0 (시스템 사용 시) | 0 |
| 시스템 Chromium | `du -sh $(which google-chrome 또는 chromium)` | n/a | 측정 | 측정 |
| Rust 바이너리 | `ls -lh target/release/cdp-spike` | n/a | n/a | TBD |
| **총 추가 disk** | 합산 | TBD | TBD | **TBD** |
| Chrome 프로세스 RSS | `/proc/<pid>/status VmRSS` | 측정 | 측정 | 측정 |
| spawn → 첫 응답 latency | `time` | 측정 | 측정 | 측정 |
| 코드 LOC (spike 단독) | `tokei crates/cdp-spike/` | n/a | n/a | TBD |
| 의존성 트리 항목 수 | `cargo tree | wc -l` | n/a | n/a | TBD |

이 표가 채워지면 "Playwright 제거가 실제로 얼마나 줄여주는가"가 숫자로 보인다.
중간 열(`chromium-cdp + 시스템`)이 핵심 — 이미 TS만으로도 시스템 Chrome 쓰면 번들
Chromium은 제거 가능하기 때문. spike의 진짜 추가 가치는 Node + node_modules 라인.

## 중단 기준

- 1~2일 working 안에 `navigate + fetch-text` 가 통하지 않으면 멈춰서 재평가
- 동일 시간 내 `chromiumoxide` 로 retry 해서 통하면 → 우리 구현 문제 (계속 진행)
- chromiumoxide 로도 안 통하면 → CDP/Chromium 자체가 환경 문제 (별개 디버깅)

## 검증 게이트

spike도 cross-review에 통과시킨다. 단 unit/E2E 테스트는 spike 단계에서 다음 수준만:

- `tests/smoke.sh` — `data:` URL 3종 (navigate / eval / fetch-text) 실제 실행 후 TS
  invoke 결과와 stdout 비교
- `cargo test` — CDP JSON-RPC frame build/parse, selector expression escape 같은
  순수 함수 단위만 (네트워크 없음)

`/cross-review` 시 codex에게 검토 요청할 포인트:

1. stderr 파싱 + `/json/version` 폴백 견고성 (Chromium 버전 차이)
2. tmp user-data-dir 정리 누락 경로 (panic / signal)
3. WebSocket close / 부분 응답 처리는 없지만 spike 범위 외임을 명시
4. Rust idiomatic / clippy 권장

## 다음 단계 (spike 성공 시, 후속 plan으로 분리)

phase 1 (TS `get-text` 풀 시맨틱 재현):
- default selector chain (`"main, article, body"` → `body`)
- `innerText` 추출 + 공백라인 collapse + trim
- `raw` 옵션 분기

phase 2 (daemon UDS 호환):
- 기존 TS daemon UDS 프로토콜 (`src/server/bridge.ts`)을 Rust로 listen
- TS CLI/MCP가 그대로 붙는지 확인

phase 3 (배포 형태 결정):
- (A) 데몬만 Rust → CLI/MCP는 TS 유지
- (B) 전부 Rust 단일 바이너리
- (C) hybrid — N-API로 Rust CDP 런타임을 TS daemon에 끼움

spike 결과 (LOC, footprint, 개발 난이도) 보고 결정.

## 작업 순서

1. ~~이 plan을 `/codex-plan`으로 압박 테스트~~ → round 1 완료, 5 CRITICAL 반영 후 round 2
2. `crates/cdp-spike/` scaffold (`cargo new --bin`, Cargo.toml 채우기)
3. browser.rs — Chromium spawn + `/json/version` 폴링 + 런치 플래그 풀세트
4. cdp.rs — WebSocket connect + send/recv + sessionId routing
5. cmd/navigate.rs — `Page.navigate` + `readyState` 폴링
6. cmd/eval.rs — `Runtime.evaluate`
7. cmd/fetch_text.rs — querySelector + textContent
8. tests/smoke.sh — `data:` URL 3종 + TS 측 동일 invoke와 stdout diff
9. 측정 결과를 이 문서 footprint 표에 채워넣음
10. `/cross-review` 통과 후 `~/dev/save.sh "spike: rust + cdp navigate/eval/fetch-text"`

## 비고

- 워크스페이스 구성: 지금은 Rust 코드가 하나뿐이므로 root Cargo.toml은 만들지 않고
  `crates/cdp-spike/Cargo.toml` 단독으로 시작. 추후 crate가 늘면 workspace로 승격.
- Node.js `package.json`과의 공존: `.gitignore`에 `target/` 추가. `Cargo.lock`은 spike
  단계에서는 commit (재현성).
- ci 변경 없음 (spike이므로 별도 워크플로 안 만듦).
