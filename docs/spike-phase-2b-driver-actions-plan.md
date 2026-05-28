# Spike phase 2b — daemon driver actions 확장 (click/type/wait)

Phase 2 daemon에 자동화 핵심 4개 액션 추가. TS CLI의 `click` / `type` / `wait-selector` /
`wait-url` 명령이 spike daemon으로도 동작하게.

## 목표

1. **4개 driver 액션 추가**: `interaction.click` / `interaction.type` / `wait.selector` / `wait.url`
2. **TS CLI 4개 명령 추가 호환**: `click <sel>` / `type <sel> <text>` / `wait-selector <sel>` / `wait-url <pat>`
3. **자동화 workflow 검증**: navigate → wait → click → type → eval/get-text 한 흐름

## Non-goals (phase 2c 이상)

- 진짜 CDP `Input.dispatchMouseEvent` / `Input.dispatchKeyEvent` — spike는 JS-based 단순 구현
- hover / mouseMove / pressKey / scroll / selectOption / check 등 다른 interaction
- iframe / shadow DOM penetration
- file upload / drag-and-drop

## TS 액션 spec

### `interaction.click {selector, timeout?}`
TS의 chromium-cdp는 `waitForActionable` + `Input.dispatchMouseEvent`. spike scope에서는
JS `element.click()` 으로 단순화. visible 검증은 spike도 같이.

```js
(() => {
  const el = document.querySelector(SEL);
  if (!el) throw new Error("Selector not found: SEL");
  el.click();
  return { ok: true };
})()
```

대기는 spike phase 2b 도 polling — `selector_visible` 평가가 true 될 때까지 200ms 간격.

### `interaction.type {selector, text, delay?, timeout?}`
TS는 character 단위 `Input.dispatchKeyEvent`. spike는:
1. `element.focus()`
2. `element.value = text`
3. `element.dispatchEvent(new Event('input', {bubbles: true}))`
4. `element.dispatchEvent(new Event('change', {bubbles: true}))`

framework form library (React/Vue) 호환은 제한적이지만 plain HTML form은 OK.

### `wait.selector {selector, timeout?}`
TS의 `waitForSelector` 패턴 — Runtime.evaluate 폴링. 200ms 간격, 기본 timeout 30s.

```js
(() => {
  const el = document.querySelector(SEL);
  return el ? true : false;
})()
```

true 반환까지 대기.

### `wait.url {pattern, patternType?, timeout?}`
TS는 `urlMatch.ts` 의 compileUrlMatcher 사용 — exact / glob / regex 3종.

spike는 동일 로직 inline (작은 함수):
- `exact`: `url === pattern`
- `glob`: pattern을 regex로 변환 (`*` → `.*`, 다른 특수문자 escape) 후 match
- `regex`: `new RegExp(pattern).test(url)`

`document.location.href` 폴링.

## CLI 표면 (TS 측만 사용; spike는 daemon driver action으로만 호환)

TS의 기존 명령 그대로 사용. spike CLI에 추가 명령 없음 (spike는 standalone get-text 등은
이미 phase 0~1d에서 충분).

```bash
ai-browser click <selector> [--timeout N]
ai-browser type <selector> <text> [--delay N] [--timeout N]
ai-browser wait-selector <selector> [--timeout N]
ai-browser wait-url <pattern> [--type exact|glob|regex] [--timeout N]
```

각 명령이 spike daemon으로 `AI_BROWSER_BASE_DIR=$TMP` 호환 검증.

## Rust 구현

### `crates/cdp-spike/src/daemon.rs` — handlers 추가

```rust
async fn handle_click(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let timeout_ms = optional_u64(params, "timeout", 30_000);
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, timeout_ms).await?;
    let sel_lit = serde_json::to_string(&selector).unwrap();
    let expr = format!(
        "(() => {{
            const el = document.querySelector({sel_lit});
            if (!el) throw new Error('Selector not found: ' + {sel_lit});
            el.click();
            return {{ ok: true }};
        }})()"
    );
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|v| v)
        .map_err(|e| e.to_string())
}

async fn handle_type(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let text = require_string(params, "text")?;
    let timeout_ms = optional_u64(params, "timeout", 30_000);
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, timeout_ms).await?;
    let sel_lit = serde_json::to_string(&selector).unwrap();
    let text_lit = serde_json::to_string(&text).unwrap();
    let expr = format!(
        "(() => {{
            const el = document.querySelector({sel_lit});
            if (!el) throw new Error('Selector not found: ' + {sel_lit});
            el.focus();
            el.value = {text_lit};
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            return {{ ok: true }};
        }})()"
    );
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map_err(|e| e.to_string())
}

async fn handle_wait_selector(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let timeout_ms = optional_u64(params, "timeout", 30_000);
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, timeout_ms).await
        .map(|_| Some(json!({ "found": true })))
        .map_err(|e| e.to_string())
}

async fn handle_wait_url(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let pattern = require_string(params, "pattern")?;
    let pattern_type = params.get("patternType").and_then(Value::as_str).unwrap_or("exact").to_owned();
    let timeout_ms = optional_u64(params, "timeout", 30_000);
    let client = client_or_err(state).await?;
    // poll location.href and match per pattern_type
    let matcher = compile_url_matcher(&pattern, &pattern_type)?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Ok(Some(Value::String(url))) = crate::cmd::eval::evaluate_value(
            &client, "document.location.href"
        ).await {
            if matcher(&url) {
                return Ok(Some(json!({ "url": url })));
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("wait-url timed out after {timeout_ms}ms (pattern={pattern} type={pattern_type})"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
```

`wait_for_selector_visible` — phase 2b 공통 helper:
```rust
async fn wait_for_selector_visible(client: &Arc<CdpClient>, selector: &str, timeout_ms: u64) -> Result<(), String> {
    let sel_lit = serde_json::to_string(selector).unwrap();
    let probe = format!(
        "(() => {{
            const el = document.querySelector({sel_lit});
            if (!el) return false;
            const rect = el.getBoundingClientRect();
            const style = getComputedStyle(el);
            return rect.width > 0 && rect.height > 0 && style.visibility !== 'hidden' && style.display !== 'none';
        }})()"
    );
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Ok(Some(Value::Bool(true))) = crate::cmd::eval::evaluate_value(client, &probe).await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("selector {selector} not visible after {timeout_ms}ms"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
```

`compile_url_matcher` — phase 2b inline (TS의 `compileUrlMatcher` 참조):
- exact: `move |u: &str| u == pattern_owned.as_str()`
- glob: escape regex special chars except `*`, replace `*` with `.*`, anchored with `^…$`, compile to regex
- regex: directly compile with `regex` crate

`regex` crate 의존성 추가 필요 — phase 2b의 새 dep.

### `process_request` 분기 확장

```rust
match req.action.as_str() {
    "tabs.navigate" => handle_navigate(state, &req.params).await,
    "execution.executeJs" => handle_eval(state, &req.params).await,
    "dom.getText" => handle_get_text(state, &req.params).await,
    // phase 2b 신규:
    "interaction.click" => handle_click(state, &req.params).await,
    "interaction.type" => handle_type(state, &req.params).await,
    "wait.selector" => handle_wait_selector(state, &req.params).await,
    "wait.url" => handle_wait_url(state, &req.params).await,
    other => Err(format!("unsupported action: {other}")),
}
```

## 검증 게이트

### 단위 테스트 (`cargo test`)

- `compile_url_matcher` 케이스:
  - exact: `https://example.com/page` matches `https://example.com/page` only
  - glob: `https://*.example.com/*` matches `https://api.example.com/foo` not `https://example.org`
  - regex: `^https://example\.com/.*$` matches per regex semantics
  - glob special chars: `?` `.` `+` etc. escaped properly

### 호환 smoke (`tests/spike-daemon-compat.sh` 확장)

기존 6 case 유지 + 4 신규:

| 케이스 | TS CLI 명령 | 검증 |
|---|---|---|
| click | `navigate data:...<button id=b onclick='window.clicked=1'>X</button>` + `click '#b'` + `eval 'window.clicked'` → `1` | clicked=1 |
| type | `navigate data:...<input id=i>` + `type '#i' hello` + `eval 'document.querySelector("#i").value'` → `"hello"` | value 정확 |
| wait-selector | `navigate data:...<script>setTimeout(()=>document.body.innerHTML+='<div id=late></div>', 300)</script>` + `wait-selector '#late'` | exit 0 |
| wait-url | `navigate data:...` + `eval 'setTimeout(()=>{location.href="data:text/html,<h1>done</h1>"}, 300)'` + `wait-url 'data:text/html,<h1>*'` → ? — wait-url의 glob 매칭은 복잡 |

wait-url smoke은 단순화: glob 패턴이 `data:*` 같이 광범위하게 매칭. 또는 같은 URL에 머무는 케이스로 exact 매칭.

### 회귀

- 기존 73 unit + 53 parity + 6 daemon compat 케이스 그대로 통과
- 신규: 약 4~5 unit (compile_url_matcher) + 4 daemon compat

## 작업 순서

1. `/codex-plan` 으로 압박 테스트 (2~3 라운드 목표, race-free 핵심은 phase 2에 이미 있음)
2. `Cargo.toml` — `regex` crate 추가
3. `src/daemon.rs` — 4 handler + helper (wait_for_selector_visible, compile_url_matcher) 추가
4. 단위 테스트 (compile_url_matcher 4~5 케이스)
5. `tests/spike-daemon-compat.sh` — 4 신규 case 추가
6. `cargo test` + `bash tests/spike-daemon-compat.sh` 모두 그린
7. `codex-review.sh --uncommitted --context-file <brief>` → APPROVED
8. `~/save.sh "Add cdp-spike daemon interaction and wait actions"`

## 검토 포인트

1. JS-based click/type vs 진짜 CDP Input dispatching — spike scope 한계 명시
2. `compile_url_matcher` glob escape — `.`/`+`/`?` 등 regex 특수문자 안전 처리
3. wait_for_selector_visible polling 간격 (200ms)이 TS와 일치
4. `interaction.type` 의 input/change event 발화가 React/Vue framework 호환 영향
5. spike CLI에 추가 명령 없음 — TS CLI만 통해서 daemon 호환

## 다음 단계 후보

- phase 2c: supervisor (crash-restart, RSS monitor, driver health)
- phase 2d: 추가 액션 (screenshot, cookies, storage 등)
- phase 3: 배포 형태 결정
