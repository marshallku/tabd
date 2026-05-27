# Spike phase 1c — `query-all` multi-element 텍스트 추출

`get-text`(단일 element → string)와 별도로 `query-all`(다수 element → JSON array)
신규 명령. selector/testid/role 모드 모두 multi 지원.

## 목표

1. **모든 매치 element의 텍스트를 array로** — JSON으로 stdout 출력
2. **selector/testid/role 모드 통일** — get-text와 동일한 3-way 선택 인터페이스
3. **기존 `build_text_body` 단일 SoT 재사용** — drift 불가

## Non-goals (이 phase에서 안 함)

- multi-target 진입 (iframe / shadow DOM)
- 매치 element의 추가 메타데이터 (tag, attributes, position 등) — 텍스트만
- TS parity — TS chromium-cdp는 `dom.querySelector` 가 array 반환하지만 결과 shape이 다름 (객체 array)

## CLI 표면

```
cdp-spike query-all <url> [TARGET] [--raw] [--limit N] [--timeout MS]

TARGET (정확히 하나 필수):
  --selector <CSS>
  --testid <ID>
  --role <ROLE> [--name <NAME>]

  --raw          textContent 원본 (collapse/trim 없음)
  --limit N      추출 cap (기본 100). selector/testid 경로는 `[...].slice(0, N)` —
                 query는 전체 매치 후 slice (DOM 매칭 자체는 cap 안 됨, JS 측 텍스트
                 추출만 cap). AX 경로는 successful 결과 N개 도달 시 loop 중단.
  --timeout MS   navigate timeout
```

차이 (get-text와 비교):
- default selector chain 없음 — TARGET 명시 필수 (mass extraction은 의도해야)
- 출력: **JSON array of strings** (`["text1", "text2"]`), 빈 매치는 `[]`, exit 0
- `--name` requires `--role` (get-text와 동일)
- 3-way mutex (clap arg group + validate_target_flags)
- `--limit N`: querySelectorAll 매치 cap. 페이지에 button 수백 개일 때 폭주 방지

## Rust 구현

### 새 파일 `cmd/query_all.rs`

```rust
pub async fn run(
    url: &str,
    selector: Option<&str>,
    testid: Option<&str>,
    role: Option<&str>,
    name: Option<&str>,
    raw: bool,
    limit: u32,
    timeout_ms: u64,
) -> Result<()> {
    validate_target_flags_strict(selector, testid, role, name)?;  // TARGET 필수
    let (browser, client) = page::open(url, timeout_ms).await?;

    // teardown 보장 위해 await ?를 inline 하지 않고 result 변수에 모음
    // (get_text.rs와 동일 패턴 — codex round 1 C1)
    let result: Result<Vec<String>> = if let Some(r) = role {
        ax_query_all(&client, r, name, raw, limit).await
    } else {
        match build_query_all_expr(selector, testid, raw, limit) {
            Ok(expr) => eval_to_string_array(&client, &expr).await,
            Err(e) => Err(e),
        }
    };
    let _ = page::teardown(browser, client).await;

    println!("{}", serde_json::to_string(&result?)?);
    Ok(())
}
```

### `validate_target_flags_strict`

get-text의 `validate_target_flags`와 같지만 **TARGET 하나 필수**. all-none → error.

이걸 위해 기존 `validate_target_flags`를 다음과 같이 변경:
- `cmd/get_text.rs`에서 `pub fn validate_target_flags(..., require_one: bool)`로 확장
- get-text는 `require_one=false` (default chain 가능), query-all은 `require_one=true`

또는 더 깔끔하게 별도 함수로:
- `validate_target_flags` (get-text 용, all-none OK)
- `validate_target_flags_strict` (query-all 용, all-none → error)
- 내부 분기 차이만 한 줄. 함수 분리가 의도 명확.

### `build_query_all_expr` (selector/testid 경로)

```rust
fn build_query_all_expr(
    selector: Option<&str>,
    testid: Option<&str>,
    raw: bool,
    limit: u32,
) -> Result<String> {
    let els_expr = build_els_expr(selector, testid)?;
    let body = build_text_body(raw);  // 기존 helper, get_text.rs 에서 export

    Ok(format!(
        r#"(() => {{
    const els = {els_expr};
    const limited = [...els].slice(0, {limit});
    return limited.map(target => {{
        {body}
    }});
}})()"#
    ))
}

fn build_els_expr(selector: Option<&str>, testid: Option<&str>) -> Result<String> {
    match (selector, testid) {
        (Some(s), None) => {
            let sel_lit = serde_json::to_string(s)?;
            Ok(format!("document.querySelectorAll({sel_lit})"))
        }
        (None, Some(t)) => {
            let testid_lit = serde_json::to_string(t)?;
            Ok(format!(
                "[...document.querySelectorAll('[data-testid]')].filter(el => el.dataset.testid === {testid_lit})"
            ))
        }
        _ => unreachable!("validate_target_flags_strict requires exactly one"),
    }
}
```

`build_text_body`는 `return` 문 포함이라 arrow function block 안에서 마지막 return으로 동작
— 명시적 export 필요.

`get_text.rs`에서:
```rust
pub(super) fn build_text_body(raw: bool) -> String { /* 기존 그대로 */ }
```
(`pub(super)` — cmd/ 내 sibling 모듈에서만 접근. 외부 노출 안 함.)

### `eval_to_string_array`

```rust
async fn eval_to_string_array(client: &CdpClient, expr: &str) -> Result<Vec<String>> {
    let value = evaluate_value(client, expr).await?
        .ok_or_else(|| anyhow!("query-all: evaluate returned undefined"))?;
    let arr = value.as_array()
        .ok_or_else(|| anyhow!("query-all: expected array, got {value}"))?;
    arr.iter()
        .map(|v| v.as_str().map(String::from)
            .ok_or_else(|| anyhow!("query-all: array element is not a string: {v}")))
        .collect()
}
```

### `ax_query_all` — Accessibility 경로

```rust
async fn ax_query_all(
    client: &CdpClient,
    role: &str,
    name: Option<&str>,
    raw: bool,
    limit: u32,
) -> Result<Vec<String>> {
    client.send("Accessibility.enable", json!({})).await?;
    let doc = client.send("DOM.getDocument", json!({ "depth": 0 })).await?;
    let root_node_id = doc.get("root").and_then(|r| r.get("nodeId"))
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("DOM.getDocument missing root.nodeId"))?;

    let mut params = json!({ "nodeId": root_node_id, "role": role });
    if let Some(n) = name { params["accessibleName"] = Value::String(n.to_owned()); }
    let q = client.send("Accessibility.queryAXTree", params).await?;
    let nodes = q.get("nodes").and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Accessibility.queryAXTree missing nodes"))?;

    let body = build_text_body(raw);
    let fn_decl = format!("function() {{ const target = this; {body} }}");

    let mut texts: Vec<String> = Vec::new();
    for node in nodes {
        // limit 의미 (codex round 1 I2): "최대 N개 successful 결과를 반환".
        // ignored / virtual / resolveNode 실패는 모두 skip — limit 카운트와 무관.
        // 비록 작업이 더 들어가지만 (skipped 노드 inspect cost) 사용자가 기대하는
        // 결과 array 크기가 명확함.
        if texts.len() as u32 >= limit { break; }
        if node.get("ignored").and_then(Value::as_bool) == Some(true) { continue; }
        let backend_id = match node.get("backendDOMNodeId").and_then(Value::as_u64) {
            Some(id) => id,
            None => continue,  // virtual node (label 등), skip
        };
        let resolved = match client.send("DOM.resolveNode",
            json!({ "backendNodeId": backend_id })).await {
            Ok(r) => r,
            Err(_) => continue,  // detached node, skip
        };
        let object_id = match resolved.get("object").and_then(|o| o.get("objectId"))
            .and_then(Value::as_str) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let r = client.send("Runtime.callFunctionOn", json!({
            "objectId": object_id,
            "functionDeclaration": fn_decl,
            "returnByValue": true,
            "awaitPromise": true,
        })).await?;
        if let Some(s) = unwrap_runtime_result(&r, "Runtime.callFunctionOn")?
            .and_then(|v| v.as_str().map(String::from)) {
            texts.push(s);
        }
    }
    Ok(texts)
}
```

**Round-trip 비용 명시**: AX 경로는 visible 매치당 (resolveNode + callFunctionOn) = 2 CDP
호출. limit이 "successful 결과 cap"이라 skipped/detached 노드 inspect도 추가 비용이
들어갈 수 있음 (loop은 nodes 전체를 훑되 push가 limit에 도달하면 중단). 페이지에 많은
ignored / virtual AX 노드 있는 경우 호출 수가 2N 이상 될 수 있음. spike scope에서
acceptable, 큰 페이지에서는 limit 줄이는 게 운영자 책임.

**`build_text_body` export (codex round 1 I1)**: phase 1c 한정으로 `pub(super) fn`로
선언해 cmd/ 내 모듈에서만 접근 가능하게. 추후 cmd/text.rs 같은 별도 helper 모듈로
재배치하는 건 phase 1d+ 결정 사항.

## 검증 게이트

### 단위 테스트 (`cargo test`)

- `validate_target_flags_strict` 케이스:
  - all-none → error (get-text와 다른 핵심 차이)
  - 각 단독 (selector / testid / role) → OK
  - role + name → OK
  - mutex 3 케이스 → error
  - name without role → error
- `build_els_expr`:
  - selector → `document.querySelectorAll(LIT)`
  - testid → `[...].filter(el => el.dataset.testid === LIT)`
- `build_query_all_expr`:
  - 결과에 `slice(0, LIMIT)` 포함 (limit 적용 확인)
  - 결과에 `.map(target => { build_text_body(raw) })` 포함 (body 재사용 확인)
- `eval_to_string_array` 단위 테스트는 evaluate_value 의존 — 통합 테스트 영역

### parity smoke 확장 (`tests/spike-parity.sh`)

TS chromium-cdp는 array 반환하는 동등 명령이 없으므로 (`dom.querySelector`는 객체 array)
**spike-only 라이브 스모크** 섹션 추가:

| 케이스 | spike 명령 | 검증 |
|---|---|---|
| selector multi hit | `query-all data:...<li>a</li><li>b</li><li>c</li>... --selector li` | `["a","b","c"]` |
| selector empty | `query-all data:...<p>x</p>... --selector h1` | `[]` |
| selector --limit | `query-all data:...<li>...3개... --selector li --limit 2` | `["a","b"]` |
| testid multi | `query-all data:...<span data-testid=item>...3개... --testid item` | `["a","b","c"]` |
| testid (단일 매치) | `query-all data:...<span data-testid=x>v</span>... --testid x` | `["v"]` |
| role multi | `query-all data:...3 buttons... --role button` | `["a","b","c"]` |
| role + name select | `query-all data:...3 buttons, one matches... --role button --name "Two"` | `["Two"]` (단일) |
| role aria-hidden 필터 | `query-all data:...<button aria-hidden>X</button><button>Y</button>... --role button` | `["Y"]` |
| role --limit | `query-all data:...4 buttons... --role button --limit 2` | `["a","b"]` |
| raw 비교 | default vs --raw 매치된 trim 차이 | text array 차이 검증 |
| 잘못된 mutex | `--selector + --role` | exit 2 (clap) |
| TARGET 없음 | `query-all data:... --raw` (no selector/testid/role) | exit 1 (validate strict) |
| 잘못된 CSS selector | `query-all data:... --selector "[[bad"` | exit 1 (Runtime.evaluate throws — codex round 1 I4) |

### 회귀
- 기존 39 unit + 25 parity 케이스 그대로 통과
- `build_text_body` 가 `pub` 으로 export 되어도 외부 caller가 query_all 모듈 외 없음 확인

## 작업 순서

1. `/codex-plan` 으로 압박 테스트 (multi-round)
2. `cmd/get_text.rs` — `build_text_body`를 `pub(super) fn` 으로 노출 (cmd/ 내 sibling만)
3. `cmd/query_all.rs` 신규 — 위 구현
4. `cmd/mod.rs` — `pub mod query_all;` 추가
5. `src/main.rs` — `QueryAll` 서브커맨드 추가. clap arg group id는 **`qa_target`** (get-text의 `gt_target`과 별개 — codex round 1 I3). 인자 표면은 get-text와 동일 + `--limit`
6. 단위 테스트 (validate strict + build_els + build_query_all_expr 패턴 검증)
7. `tests/spike-parity.sh` — phase-1c 섹션 신규 (10~12 케이스)
8. `cargo test` + `npm run e2e:spike-parity` 모두 그린
9. `codex-review.sh --uncommitted --context-file <brief>` → APPROVED
10. `~/save.sh "Add cdp-spike query-all for multi-element text extraction"`

## 검토 포인트 (codex-plan 라운드 시)

1. `validate_target_flags_strict`를 get-text의 그것과 별도 함수로 두는 결정 — 단순함 vs 코드 중복
2. `build_text_body`를 `pub` 노출 — module boundary 침범 우려
3. AX 경로의 N round-trip 비용 — 더 효율적 batching 가능한지 (예: `DOM.resolveNode` 다수 한 번에 → 그러나 callFunctionOn은 여전히 per-objectId)
4. `eval_to_string_array`의 element 비-string 처리 — body는 항상 string 반환하지만 방어 분기 합리적인지
5. limit=0 의미: empty array (cap=0) vs no cap. 명시 결정 필요 (plan은 cap=0으로 가정)
6. JSON 출력의 trailing newline (println!) — 향후 파서 영향
7. testid 케이스에서 `filter(...).slice(...)`의 순서 — filter 먼저 vs slice 먼저 (현재 plan은 filter 먼저)

## 다음 단계 후보 (1c 후)

- phase 1d: `find-all-meta` — 매치 element의 (text, tag, role, attributes) 객체 array. UI 자동화 도구 가까운 표면.
- phase 2: daemon UDS 프로토콜 Rust 재구현
- phase 3: 배포 형태 결정
