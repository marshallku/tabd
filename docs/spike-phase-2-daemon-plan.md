# Spike phase 2 — Rust daemon (TS UDS protocol compat)

기존 TS daemon (`src/server/daemon.ts`)과 byte-compatible한 newline-delimited
JSON-RPC over UDS server를 Rust로 구현. TS CLI의 3개 명령 (navigate/eval/get-text)
+ daemon control이 socket path만 override하면 spike daemon에 그대로 붙는 것을 검증.
**MCP 액션 전체 호환은 비목표** (phase 2b+).

## 목표 (codex round 2 C7 — scope 좁힘)

phase 2는 **CLI-only minimum viable compat**. MCP 액션 전체 호환은 phase 2b 이상 (별도 plan).

1. **TS protocol byte-compatible wire format** — `{id, action, params?}\n` → `{id, success, data?|error?}\n`
2. **3개 daemon control 액션**: `daemon.ping` / `daemon.health` / `daemon.shutdown`
3. **3개 driver 액션** (TS의 핵심 3개 CLI 명령 backing): `tabs.navigate` / `execution.executeJs` / `dom.getText`
4. **Chromium reuse** — daemon 라이프타임 동안 1회 spawn (기존 spike의 매번 spawn 변경)
5. **호환 검증 범위**: `./bin/ai-browser.js navigate|eval|get-text|daemon health|daemon stop` 만.
   다른 TS CLI 명령(`tabs list`/`click`/`type-text` 등)이나 MCP server 호환은 비목표.

## Non-goals (이 phase에서 안 함)

- 다른 driver actions (interaction.*, capture.*, cookies.*, secrets.*, monitor.*, wait.*, dom.querySelector 등 — phase 2b 이상)
- spike의 phase 0~1d 명령 (get-text/query-all/find-all)를 daemon으로 노출 — 별개 명령 표면 유지
- Multi-tab 관리 — `tabs.navigate`는 active tab에만 동작 (TS의 multi-tab은 spike 외)
- TS의 `--user-data-dir` / persistent 옵션 — spike daemon은 임시 profile만
- TS의 detailed drain/restart 로직 — minimum viable supervisor

## UDS Protocol (TS와 동일)

### Wire format
- Request: `{"id":"x","action":"tabs.navigate","params":{"url":"..."}}\n`
- Response: `{"id":"x","success":true,"data":{...}}\n` 또는
            `{"id":"x","success":false,"error":"..."}\n`
- 각 메시지는 single line of UTF-8 JSON + 한 개의 `\n` 종결.

### Buffer handling
- TS: socket data 누적 → `\n` 단위 split → 빈 line skip
- Rust: tokio `BufReader::lines()` 사용 (자동 \n split, 빈 line 처리 명시)

### Daemon control 액션
- `daemon.ping` → `{pid: u32, ready: bool}`
- `daemon.health` (codex round 1 C4 — TS health shape 유지):
  ```json
  {
    "pid": <u32>,
    "uptimeMs": <u64>,
    "ready": true,
    "accepting": <bool>,
    "inflight": <u32>,
    "totalRequests": <u64>,
    "lastError": null | { "action": "...", "message": "...", "at": <epoch_ms> },
    "driver": null | { "chromiumPid": <u32> }
  }
  ```
  - **`lastError`**: 추적 — 모든 실패 driver action 후 `{action, message, at: now()}` 갱신
  - **`driver`**: `null` 반환 (codex round 2 C5 — chromium-cdp 호환).
  TS chromium-cdp runtime이 `getDriverHealth`를 구현하지 않아서 TS health에서도 `driver: null`.
  spike도 byte-compat 위해 `null` 통일. chromiumPid 등 supervisor field는 phase 2c (supervisor)
  단계에서 별개 health endpoint나 driver wrapper로 추가 — phase 2 scope 외.
- `daemon.shutdown` → `{stopping: true}` + 50ms 뒤 shutdown 시작 (자세한 lifecycle은 아래 Shutdown 섹션)

### Driver 액션 (minimum viable)

**호환 target (codex round 1 C3)**: `BROWSER_RUNTIME=chromium-cdp` 모드의 TS 런타임만.
Playwright 모드는 response shape이 다르므로 (예: navigate가 `{tabId, url, title}`) spike와
호환되지 않음 — 검증 smoke에서 명시적으로 `BROWSER_RUNTIME=chromium-cdp` 강제.

- `tabs.navigate {url, timeout?}` → `{url}` (chromium-cdp의 src/server/runtimes/cdp.ts:798 shape)
- `execution.executeJs {code, timeout?}` → 평가 결과 raw value (TS는 `result.value` 평면화, spike도 동일)
- `dom.getText {selector?, raw?}` → string (chromium-cdp의 src/server/runtimes/cdp.ts:854~872 동작 그대로)

## Daemon lifecycle

### Boot (codex round 5 C2 — early accept + ready gate)

1. socket path 결정 — `$AI_BROWSER_BASE_DIR/daemon.sock` 또는 기본 경로
   - **기본 경로 (spike)**: `$XDG_RUNTIME_DIR/ai-browser-rs/daemon.sock` (TS의 `ai-browser`와 충돌 회피)
   - **호환 모드**: `AI_BROWSER_BASE_DIR=$XDG_RUNTIME_DIR/ai-browser` 로 override 시 TS와 같은 경로
2. parent dir 생성 (`mkdir -p`)
3. **단일 bind = atomic lock (`bind_listener_with_lock`)**:
   - `UnixListener::bind(&socket_path)` 시도 → 성공 시 그 listener를 run()에 반환.
   - 실패 (`EADDRINUSE`)면 PID/connect probe 후 stale socket unlink + 재시도 1회.
4. **accept loop 즉시 시작** (chromium 부팅 전) — TS와 동일하게 control 액션 (ping/health) 이
   ready=false 동안에도 응답. driver 액션은 `wait_ready()` 로 ready 도달 후 진행.
5. 백그라운드 boot task:
   - Browser::launch (kill_on_drop)
   - CdpClient::connect
   - PID file 작성
   - `ready.store(true)` + `ready_notify.notify_waiters()`
   - boot 실패 시 cleanup path: socket file + pid file unlink + process exit non-zero

driver 액션의 ready 대기:
```rust
async fn wait_ready(&self) {
    loop {
        let notified = self.ready_notify.notified();
        tokio::pin!(notified);
        if self.ready.load(Ordering::Acquire) { return; }
        notified.await;
    }
}
```

`daemon.health` 응답은 ready=false 동안 `{ready: false, accepting: ...}` 반환 (TS와 동일).

### Request 처리
- 각 socket connection 별 BufReader → `next_line().await` loop
- 빈 line skip, JSON parse 실패면 `{id:"", success:false, error:"invalid JSON"}`
- daemon control은 즉시 응답 (bridge 안 거침)
- driver 액션은 shared CdpClient (Arc<...>)로 dispatch
- accept gate (`accepting` flag) — shutdown 시작하면 false → 새 driver 요청 거절

### Concurrency

**핵심 (codex round 1 C2)**: `CdpClient`는 CDP frame 단위 직렬화만 함 (request id + oneshot
pending). 하지만 driver action은 multi-frame composite — 예를 들어 `tabs.navigate`는
`Page.navigate` + `Runtime.evaluate(readyState)` 폴링이다. 다른 action이 그 사이에 끼면
페이지 상태가 깨질 수 있다.

TS Playwright runtime은 `ActionQueue`(`src/server/runtimes/playwright.ts:548`)로 high-level
mutex를 둔다. spike phase 2는 **multi-tab 비목표 (codex I3)** 이므로 단일 글로벌
action mutex로 충분:

```rust
struct DaemonState {
    action_mutex: Arc<tokio::sync::Mutex<()>>,
    client: Arc<CdpClient>,
    // ...
}

async fn handle_driver_action(state: &DaemonState, ...) -> Response {
    let _guard = state.action_mutex.lock().await;
    // 이 lock guard 안에서 navigate/eval/getText 모두 직렬화
}
```

`daemon.ping`/`daemon.health`/`daemon.shutdown`은 lock 안 잡음 (TS의 control bypass 동일).

### Shutdown (codex round 1 C5 — listener-keep + drain, round 4 I2 — 순서 명확화)

TS의 lifecycle 호환 위해 listener는 drain 완료까지 keep — daemon.health/ping은 drain 중에도
응답 가능, 새 driver action은 `accepting=false`라 명시 거절.

순서:
1. `daemon.shutdown` 받으면 `accepting.swap(false, AcqRel)` 으로 즉시 gate 닫기 (응답 write 전에)
2. **첫 호출만** drain 백그라운드 task spawn (idempotent — `drain_started.swap(true)` 가 false였을 때만)
3. `{stopping: true}` 응답 write (TS의 src/server/daemon.ts:357와 동일 shape)
4. **listener는 그대로 유지** — 새 connection 받음, control 액션 (ping/health/shutdown) 응답 가능
5. 백그라운드 drain task:
   - 50ms grace (응답 flush 보장)
   - inflight==0 도달까지 drain_notify 기반 wait — `AI_BROWSER_DRAIN_TIMEOUT_MS` (default 10000) timeout
   - drain 완료 또는 timeout → `drain_complete = true` + drain_notify wake
6. run()의 accept loop은 `wait_drain_complete()` arm으로 break — listener drop (새 connection 거절)
7. CdpClient close (mpsc tx drop으로 writer/reader background task 자연 종료) → Browser shutdown (SIGTERM → 2s → SIGKILL fallback)
8. socket file + pid file unlink
9. process exit 0

`accepting=false` 동안 incoming driver action 응답:
- `{id, success: false, error: "daemon is shutting down (drain in progress)"}` (TS와 byte-exact)

## Rust 구현 구조

### 새 모듈: `crates/cdp-spike/src/daemon.rs`

```rust
pub async fn run(base_dir: Option<&str>) -> Result<()> {
    let paths = resolve_paths(base_dir)?;
    let listener = bind_listener_with_lock(&paths).await?;

    // Boot failure cleanup (codex round 3 I3 + round 4 C4) — bind succeeded
    // so socket file exists. Wrap ALL post-bind boot steps (browser, CDP,
    // pid file) in the same cleanup path so any failure unlinks the socket
    // before bubbling the error.
    let boot = async {
        let browser = Browser::launch().await?;
        let client = CdpClient::connect(browser.ws_endpoint()).await?;
        write_pid_file(&paths)?;
        Ok::<_, anyhow::Error>((browser, client))
    };
    let (browser, client) = match boot.await {
        Ok(pair) => pair,
        Err(e) => {
            let _ = std::fs::remove_file(&paths.socket_path);
            let _ = std::fs::remove_file(&paths.pid_path);
            return Err(e);
        }
    };
    let state = DaemonState::new(client, browser);

    // listener stays alive UNTIL drain completes (codex round 3 C1).
    // shutdown signal sets accepting=false; drain_done fires only when
    // inflight reaches 0 or drain timeout — only then accept loop breaks.
    loop {
        tokio::select! {
            r = listener.accept() => {
                let (sock, _) = r?;
                let state = state.clone();
                tokio::spawn(handle_connection(sock, state));
            }
            _ = state.wait_drain_complete() => break,
        }
    }
    // accept loop now closed. cleanup CDP + browser + sockfile.
    state.cleanup(&paths).await?;
    Ok(())
}

async fn handle_connection(sock: UnixStream, state: DaemonState) -> Result<()> {
    let (reader, writer) = sock.into_split();
    let mut lines = BufReader::new(reader).lines();
    let writer = Arc::new(tokio::sync::Mutex::new(writer));
    while let Ok(Some(line)) = lines.next_line().await {
        if line.is_empty() { continue; }
        let state = state.clone();
        let writer = writer.clone();
        // codex round 2 C2 — inflight counted from accept (after gate, before
        // mutex wait) so drain sees queued work. control actions don't count.
        tokio::spawn(async move {
            let resp = process_request(&line, &state).await;
            let mut w = writer.lock().await;
            let _ = w.write_all(resp.as_bytes()).await;
            let _ = w.write_all(b"\n").await;
        });
    }
    Ok(())
}

async fn process_request(line: &str, state: &DaemonState) -> String {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(_) => return error_response("", "invalid JSON in request"),
    };

    // Control actions answer immediately — no accept gate, no action mutex,
    // no inflight counter (TS bypass pattern, src/server/daemon.ts:352~390).
    if req.action == "daemon.ping" { return state.ping(&req.id); }
    if req.action == "daemon.health" { return state.health(&req.id); }
    if req.action == "daemon.shutdown" { return state.shutdown(&req.id).await; }

    // Driver actions: atomic admit (gate + inflight + totalRequests in one
    // critical section, codex round 3 C2/C3). Returns an RAII guard that
    // decrements inflight on Drop — survives panic/cancellation (C4).
    let _guard = match state.try_admit() {
        Some(g) => g,
        None => return error_response(&req.id, "daemon is shutting down (drain in progress)"),
    };
    // Wait for chromium+cdp boot to complete (codex round 5 C2).
    state.wait_ready().await;
    let _action_lock = state.action_mutex.lock().await;
    let result = match req.action.as_str() {
        "tabs.navigate" => state.handle_navigate(&req).await,
        "execution.executeJs" => state.handle_eval(&req).await,
        "dom.getText" => state.handle_get_text(&req).await,
        other => error_response(&req.id, &format!("unsupported action: {other}")),
    };
    // lastError tracking (codex round 5 C3) — update on every non-success
    // driver response so daemon.health reflects the most recent failure.
    state.record_response(&req.action, &result).await;
    // _guard / _action_lock drop here in reverse order → inflight decrement
    // + drain_notify wake. Any panic from above also unwinds through Drop.
    result
}
```

`record_response(action, response_string)`:
- response를 partial-parse (success bool + error message)
- success=false 면 `last_error.lock().await = Some(LastError { action, message, at_epoch_ms: now() })`
- success=true 면 no-op (TS는 마지막 error 유지)

### Atomic admit gate (codex round 3 C2/C3/C4)

`try_admit()` 는 "accepting" 확인 + inflight increment + totalRequests increment 를 단일
trip으로 처리하고, panic-safe RAII guard 를 반환:

```rust
struct DaemonState {
    accepting: Arc<AtomicBool>,
    inflight: Arc<AtomicU32>,
    total_requests: Arc<AtomicU64>,
    drain_notify: Arc<tokio::sync::Notify>,
    // ... action_mutex, client, browser holder, last_error, etc.
}

pub struct InflightGuard {
    inflight: Arc<AtomicU32>,
    notify: Arc<tokio::sync::Notify>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        // sync atomic decrement → safe in Drop (no async needed)
        self.inflight.fetch_sub(1, Ordering::AcqRel);
        // wake any waiter that needs inflight == 0
        self.notify.notify_waiters();
    }
}

impl DaemonState {
    fn try_admit(&self) -> Option<InflightGuard> {
        // Speculative increment. We need accepting to be true both BEFORE
        // and AFTER the increment to close the check-then-act window —
        // otherwise shutdown can sneak in between the two ops.
        if !self.accepting.load(Ordering::Acquire) { return None; }
        self.inflight.fetch_add(1, Ordering::AcqRel);
        if !self.accepting.load(Ordering::Acquire) {
            // shutdown began after our pre-check — undo the speculative
            // increment and reject.
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            self.drain_notify.notify_waiters();
            return None;
        }
        self.total_requests.fetch_add(1, Ordering::AcqRel);
        Some(InflightGuard {
            inflight: self.inflight.clone(),
            notify: self.drain_notify.clone(),
        })
    }

    /// Called by `daemon.shutdown` action. Idempotent — only the FIRST call
    /// schedules the drain task (codex round 4 C2). Closes the gate, replies
    /// {stopping: true}, drain runs in background.
    async fn shutdown(&self, req_id: &str) -> String {
        // swap(false): only the thread that observed true → false races wins.
        let was_accepting = self.accepting.swap(false, Ordering::AcqRel);
        // Wake any admitter currently in its post-increment recheck so they
        // see the gate closure and back out.
        self.drain_notify.notify_waiters();

        // Idempotent — drain only scheduled once across multiple shutdowns.
        if was_accepting && !self.drain_started.swap(true, Ordering::AcqRel) {
            let drain_state = self.clone();
            tokio::spawn(async move { drain_state.run_drain().await; });
        }
        success_response(req_id, json!({ "stopping": true }))
    }

    async fn run_drain(&self) {
        // 50ms grace for in-flight responses to flush before we start counting.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let timeout = Duration::from_millis(drain_timeout_ms_env());
        let _ = tokio::time::timeout(timeout, async {
            // tokio Notify lost-wake pattern (codex round 5 C1):
            // register notified() future BEFORE the atomic load. If notify
            // fires between load and await, the future already holds a permit
            // and await returns immediately. Without this, drain can hang.
            loop {
                let notified = self.drain_notify.notified();
                tokio::pin!(notified);
                if self.inflight.load(Ordering::Acquire) == 0 { return; }
                notified.await;
            }
        }).await;
        // Sticky flag + Notify pair (codex round 4 C1) — set flag FIRST,
        // then notify_waiters. wait_drain_complete polls the flag in the
        // same register-before-load pattern.
        self.drain_complete.store(true, Ordering::Release);
        self.drain_notify.notify_waiters();
    }

    async fn wait_drain_complete(&self) {
        loop {
            let notified = self.drain_notify.notified();
            tokio::pin!(notified);
            if self.drain_complete.load(Ordering::Acquire) { return; }
            notified.await;
        }
    }
}
```

### `CdpClient::close` 변경 (codex round 1 C6 + round 2 C6 — JoinHandle 처리 명시)

기존 `pub async fn close(self) -> Result<()>` 는 `Arc<CdpClient>` 와 충돌. phase 2를 위해
**`pub async fn close(&self) -> Result<()>`** 로 변경:

필드 변경:
- `out_tx: mpsc::UnboundedSender<String>` → `out_tx: tokio::sync::Mutex<Option<mpsc::UnboundedSender<String>>>`
- `reader: Option<JoinHandle<()>>` / `writer: Option<JoinHandle<()>>` 제거 — **join 안 함**.
  background task는 sender drop → mpsc closed → writer task exit → sink close → reader EOF → 자연 종료.
  process가 살아있는 동안만 의미 있는 task이므로 명시 join이 spike scope에서 불필요.

close 동작:
- `out_tx` 의 Mutex 잡고 `Option::take()` → Sender drop. 그게 전부.
- 멱등 — 이미 None이면 no-op
- close 후 `dispatch` 의 `out_tx.lock().await.as_ref()` 가 None이면 "cdp writer task closed" error

기존 caller (page::teardown) 영향:
- `client.close().await` → `&self` 호출이라 변경 없음. `Browser::shutdown(self)` 도 그대로.

단위 테스트 추가:
- close 두 번 호출 → 둘 다 Ok
- close 후 dispatch → "cdp writer task closed" error

### `DaemonState` (codex round 4 C3 — single atomic model)

```rust
struct DaemonState {
    // Atomic state — no async lock needed for hot path
    ready: Arc<AtomicBool>,             // chromium+cdp boot complete (codex round 5 C2)
    ready_notify: Arc<tokio::sync::Notify>,
    accepting: Arc<AtomicBool>,         // gate
    inflight: Arc<AtomicU32>,           // active driver actions
    total_requests: Arc<AtomicU64>,     // ever-admitted count
    drain_complete: Arc<AtomicBool>,    // sticky flag — set when drain done
    drain_started: Arc<AtomicBool>,     // for idempotent shutdown (codex round 4 C2)
    drain_notify: Arc<tokio::sync::Notify>,  // wake admitters/waiters

    // Locked state — only updated/read on slow paths
    last_error: Arc<tokio::sync::Mutex<Option<LastError>>>,
    // Long-lived CDP client — set by boot task (None until ready)
    client: Arc<tokio::sync::Mutex<Option<Arc<CdpClient>>>>,
    browser: Arc<tokio::sync::Mutex<Option<Browser>>>,  // taken at cleanup
    action_mutex: Arc<tokio::sync::Mutex<()>>,  // codex round 2 C2 — global action lock

    started_at: Instant,
}

struct LastError {
    action: String,
    message: String,
    at_epoch_ms: u64,
}
```

(`DaemonInner` 제거 — atomic 필드들과 last_error Mutex로 충분. shutdown 신호는
`drain_complete` AtomicBool + Notify pair로 처리. oneshot::Sender 등 불필요.)

### Path resolution: `src/daemon/paths.rs` (또는 daemon.rs 내부)

```rust
struct DaemonPaths { socket_path: PathBuf, pid_path: PathBuf, base_dir: PathBuf }

fn resolve_paths(override_base: Option<&str>) -> Result<DaemonPaths> {
    let base_dir = if let Some(p) = override_base { PathBuf::from(p) }
        else if let Ok(d) = std::env::var("AI_BROWSER_BASE_DIR") { PathBuf::from(d) }
        else if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
            PathBuf::from(d).join("ai-browser-rs")
        }
        else {
            // fallback — TS도 같은 fallback
            PathBuf::from(std::env::var("HOME")?).join(".cache/ai-browser-rs")
        };
    Ok(DaemonPaths {
        socket_path: base_dir.join("daemon.sock"),
        pid_path: base_dir.join("daemon.pid"),
        base_dir,
    })
}
```

**기본 base_dir은 `ai-browser-rs`** (TS의 `ai-browser`와 충돌 회피). 호환 검증 시
`AI_BROWSER_BASE_DIR=$XDG_RUNTIME_DIR/ai-browser` 로 override.

### CLI: `src/main.rs` 확장

새 서브커맨드:
- `cdp-spike daemon start [--base-dir DIR]` — foreground daemon (foreground only, spike scope)
- `cdp-spike daemon stop [--base-dir DIR]` — 기존 daemon에 shutdown 전송
- `cdp-spike daemon ping [--base-dir DIR]` — 응답 받아 stdout JSON
- `cdp-spike daemon health [--base-dir DIR]` — health JSON

기존 spike 명령들 (navigate/eval/fetch-text/get-text/query-all/find-all)은 standalone
유지 — daemon에 의존하지 않음 (phase 2 scope는 daemon 신규 등록만).

## 호환 검증

`tests/spike-daemon-compat.sh` 신규.

**TS CLI 실제 daemon 서브커맨드 (codex round 2 C3)**: `start` / `stop` / `restart` /
`status` / `health` (src/cli/index.ts:449~555). `ping`은 daemon-internal action일 뿐
CLI 서브커맨드 아님. compat smoke은 위 5개와 `navigate` / `eval` / `get-text` 만.

**`BROWSER_RUNTIME=chromium-cdp` 강제 (codex round 2 C4)**: TS의 `withDaemon()`이 daemon
연결 실패 시 자동으로 in-process TS daemon spawn할 수 있음. spike daemon이 죽으면
검증이 silent fail. compat smoke은:
- `BROWSER_RUNTIME=chromium-cdp` env 강제 (chromium-cdp 호환 target)
- spike daemon 응답 확인: 매 명령 전후로 `daemon health` ping → 응답 없으면 fail-fast
- 만약 TS가 자체 daemon spawn해버리면 PID 비교로 detect (spike daemon PID vs 응답 PID)

흐름:
1. tmp dir 생성, `AI_BROWSER_BASE_DIR=<tmp>` 설정
2. spike daemon 백그라운드 start: `cdp-spike daemon start --base-dir <tmp> &` 후 PID 저장
3. socket file 등장 대기 (timeout 5s)
4. spike daemon health 직접 호출로 PID 확인 — 이후 TS CLI health 응답과 비교
5. TS CLI 명령 실행 (모두 `AI_BROWSER_BASE_DIR=<tmp>` + `BROWSER_RUNTIME=chromium-cdp`):
   - `./bin/ai-browser.js daemon health` → JSON 파싱, `pid == spike_daemon_pid` 검증
   - `./bin/ai-browser.js navigate data:text/html,<h1>Hi</h1>` → exit 0
   - `./bin/ai-browser.js eval "document.title"` → 결과 확인
   - `./bin/ai-browser.js get-text --selector h1` → "Hi"
6. `./bin/ai-browser.js daemon stop` (response 반환 즉시 종료)
7. socket file 제거 poll (codex round 2 I2 — 최대 5s 기다림)
8. spike daemon process 종료 확인 (kill -0 PID = no)
9. tmp dir cleanup

TS CLI의 액션 매핑:
- `ai-browser navigate <url>` → `tabs.navigate`
- `ai-browser eval <expr>` → `execution.executeJs`
- `ai-browser get-text [--selector S] [--raw]` → `dom.getText`

각 액션이 spike daemon에서 byte-exact 결과 반환하는지 검증.

## 검증 게이트

### 단위 테스트 (`cargo test`)

- `parse_request` 4 케이스: 정상 / params 없음 / 필드 누락 / 잘못된 JSON
- `error_response` shape 검증
- `resolve_paths` 4 케이스: override / env / XDG_RUNTIME_DIR / HOME fallback
- `Request`/`Response` serde round-trip

라이브 (`#[ignore]` smoke):
- daemon start → **spike-internal ping** (Rust CLI 의 daemon ping, TS의 daemon.ping action을 raw로 보내는 별도 명령) → shutdown 1-cycle (codex round 3 I2 — "spike-internal" 명시)
- **concurrent driver action mutex 검증** (codex round 2 I3):
  - 동시에 `tabs.navigate` + `execution.executeJs` 두 요청을 다른 connection에서 send
  - action_mutex가 직렬화 보장 → 결과는 순차 (race 없이 정상 응답)
  - 단순 동시 ping은 mutex 검증 안 됨 (control은 lock 안 잡음)

### 호환 smoke (`tests/spike-daemon-compat.sh`)
- 5 케이스 (위 호환 검증 흐름)
- 모든 케이스에서 TS CLI exit 0 + stdout 정확

### 회귀
- 기존 64 unit + 53 parity 케이스 그대로 통과
- spike phase 0~1d 명령 (navigate/eval/.../find-all)은 daemon과 무관, 영향 없음

## 작업 순서

1. `/codex-plan` 으로 압박 테스트 (multi-round)
2. `Cargo.toml` — tokio "net" feature 추가 (`UnixListener`/`UnixStream`)
3. `src/daemon.rs` 신규 — `run()`, `DaemonState`, `process_request`, `handle_connection`
4. `src/daemon/paths.rs` (또는 인라인) — `resolve_paths` + `bind_socket_lock`
5. `src/daemon/actions.rs` (또는 daemon.rs 내) — `handle_navigate`/`handle_eval`/`handle_get_text` (기존 cmd::page / build_text_body 재사용)
6. `src/main.rs` — `Daemon { Start, Stop, Ping, Health }` 서브커맨드
7. 단위 테스트 (request parse, paths)
8. `tests/spike-daemon-compat.sh` — TS CLI override 호환 smoke
9. `cargo test` + 호환 smoke 모두 그린
10. `codex-review.sh --uncommitted --context-file <brief>` → APPROVED
11. `~/save.sh "Add cdp-spike daemon with TS-compatible UDS protocol"`

## 검토 포인트 (codex-plan)

1. spike의 단발성 Chromium spawn → daemon의 reuse 전환 시 cdp.rs의 multi-caller race condition
2. `tabs.navigate` 의 response shape이 TS와 정확히 일치 (`{url}` only)
3. `dom.getText`의 default selector / collapse / trim — phase 1a의 build_text_body 재사용
4. `execution.executeJs` 결과 직접 반환 — TS는 `result.value` 평면화. spike 호환 형태
5. shutdown 50ms grace — 응답 flush 보장? OS socket buffer 의존?
6. `AI_BROWSER_BASE_DIR` env var — TS CLI도 같은 이름 사용? `getDaemonPaths` 함수 확인
7. multi-tab 모델 — spike daemon은 active tab만. tabs.navigate는 active tab에 동작
8. PID file format — TS는 plain int. spike도 동일 (atoi 호환)
9. `daemon.health.driver` 가 TS shape (`{chromiumPid, restartAttempt, ...}`) vs spike의 `null` — phase 2 의 minimum subset 결정
10. Browser shutdown 순서 — accepting=false → listener close → drain (혹은 skip) → CdpClient close → Browser SIGTERM

## 다음 단계 (이 phase 후)

- phase 2b: 더 많은 driver actions (interaction.*, capture.*, cookies.*, dom.querySelector 등)
- phase 2c: graceful drain + restart-on-crash supervisor
- phase 3: 배포 형태 결정 (단일 Rust 바이너리 vs hybrid)
