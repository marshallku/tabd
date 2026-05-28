# Spike phase 1d — `find-all` element 메타데이터 JSON array

`query-all` (텍스트만)와 별도로 `find-all` — 매치 element의 메타데이터 객체 array.
UI 자동화 도구 가까운 표면 (셀렉터/페이지 inspection 용).

## 목표

1. **메타데이터 객체 array 출력** — tag, text, id, classes, attrs, rect
2. **path 무관 일관된 shape** — selector/testid/role 모드 모두 같은 객체 shape
3. **build_text_body 단일 SoT 재사용** — 텍스트 계산은 phase 1a~1c와 동일

## Non-goals

- 매치 element 의 후속 액션 (click, type 등) — 메타데이터 추출만
- iframe / shadow DOM 진입
- AX 트리의 role/name 정보를 출력에 포함 — query 입력에만 사용, 출력은 DOM 정보로 일관
- visibility 계산 (rect 0으로 client가 추론 가능 — 명시 필드 안 둠)

## 출력 객체 shape

각 element 당:
```json
{
  "tag": "button",
  "text": "Submit",
  "id": "save-btn" | null,
  "classes": ["btn", "primary"],
  "attrs": {
    "role": "button",
    "aria-label": "Save",
    "name": "save",
    "href": "https://...",
    "value": "..."
  },
  "rect": { "x": 100, "y": 200, "w": 120, "h": 40 }
}
```

- `tag`: lowercase HTML tag name (`element.tagName.toLowerCase()`)
- `text`: `build_text_body` 결과 (raw vs innerText collapse trim)
- `id`: `element.id || null` (빈 string도 null로)
- `classes`: `[...element.classList]` — 배열
- `attrs`: 키 inclusion 규칙 (codex round 2 C1 — `value` 특례 명시):
  - **role / aria-label / name / href**: `getAttribute(k)` 결과가 null이면 키 미포함.
    빈 string은 그대로 포함 (예: `aria-label=""`).
  - **value (특례)**:
    - input/textarea/select element: `element.value` (live property) **항상 포함**.
      빈 string도 의미 있는 정보 (user가 비운 input vs unset). 따라서 key 항상 존재.
    - 그 외 element: `getAttribute("value")` — 없으면 키 미포함, markup 그대로.
  - 의미적으로 `attrs.value`의 presence는 element 타입에 따라 다름: input-like는 live
    state, 그 외는 markup attribute. plan + 출력 객체 shape 문서에서 모두 명시.
- `rect`: `getBoundingClientRect()`의 `x/y/width/height`. width or height가 0이면
  hidden 추론 가능 (client filter).

전체 출력: JSON array of objects, 빈 결과 `[]`, exit 0.

## CLI 표면

```
cdp-spike find-all <url> [TARGET] [--raw] [--limit N] [--timeout MS]

TARGET (정확히 하나 필수, query-all과 동일):
  --selector <CSS>
  --testid <ID>
  --role <ROLE> [--name <NAME>]

  --raw          text 필드의 textContent 원본 (collapse/trim 없음)
  --limit N      추출 cap (기본 100). query-all과 동일 의미.
  --timeout MS   navigate timeout
```

`validate_target_flags_strict` 재사용 (cmd/query_all.rs에서 export).

## Rust 구현

### `cmd/find_all.rs` (신규)

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
    validate_target_flags_strict(selector, testid, role, name)?;  // query_all에서 재사용
    let (browser, client) = page::open(url, timeout_ms).await?;

    let result: Result<Vec<Value>> = if let Some(r) = role {
        ax_find_all(&client, r, name, raw, limit).await
    } else {
        match build_find_all_expr(selector, testid, raw, limit) {
            Ok(expr) => eval_to_value_array(&client, &expr).await,
            Err(e) => Err(e),
        }
    };
    let _ = page::teardown(browser, client).await;

    let objects = result?;
    println!("{}", serde_json::to_string(&objects)?);
    Ok(())
}
```

### `build_meta_js_body(raw)` 단일 SoT (codex round 1 I1)

```rust
/// JS body that defines `textOf` and `metaOf`. Selector/testid path wraps
/// it in `(() => { ... return [...els].slice(0, LIMIT).map(metaOf); })()`,
/// AX path wraps it in `function() { return metaOf(this); }` (callFunctionOn).
/// Both paths inherit the same shape — drift impossible.
pub(super) fn build_meta_js_body(raw: bool) -> String {
    let text_body = build_text_body(raw);
    format!(r#"const ATTR_KEYS = ["role", "aria-label", "name", "href", "value"];
const textOf = (target) => {{ {text_body} }};
// value 특례 — input/textarea/select는 live property (빈 string도 포함),
// 그 외는 markup attribute (없으면 null). codex round 2 C1.
const liveValueOf = (el) => {{
    if (el instanceof HTMLInputElement
        || el instanceof HTMLTextAreaElement
        || el instanceof HTMLSelectElement) {{
        return {{ present: true, value: el.value }};
    }}
    const v = el.getAttribute("value");
    return {{ present: v !== null, value: v }};
}};
const metaOf = (el) => {{
    if (!(el instanceof Element)) return null;
    const r = el.getBoundingClientRect();
    const attrs = {{}};
    for (const k of ATTR_KEYS) {{
        if (k === "value") {{
            const lv = liveValueOf(el);
            if (lv.present) attrs.value = lv.value;
        }} else {{
            const v = el.getAttribute(k);
            if (v !== null) attrs[k] = v;
        }}
    }}
    return {{
        tag: el.tagName.toLowerCase(),
        text: textOf(el),
        id: el.id || null,
        classes: [...el.classList],
        attrs,
        rect: {{ x: r.x, y: r.y, w: r.width, h: r.height }},
    }};
}};"#)
}
```

### `build_find_all_expr` (selector/testid 경로)

```rust
fn build_find_all_expr(
    selector: Option<&str>,
    testid: Option<&str>,
    raw: bool,
    limit: u32,
) -> Result<String> {
    let els_expr = build_els_expr(selector, testid)?;  // query_all에서 재사용
    let meta_body = build_meta_js_body(raw);
    Ok(format!(
        r#"(() => {{
    {meta_body}
    const els = {els_expr};
    return [...els].slice(0, {limit}).map(metaOf).filter(x => x !== null);
}})()"#
    ))
}
```

### `eval_to_value_array`

```rust
async fn eval_to_value_array(client: &CdpClient, expr: &str) -> Result<Vec<Value>> {
    let value = evaluate_value(client, expr)
        .await?
        .ok_or_else(|| anyhow!("find-all: evaluate returned undefined"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| anyhow!("find-all: expected array, got: {value}"))?;
    Ok(arr.clone())
}
```

### AX traversal shared (codex round 1 I2 — Rule of Three)

3번째 AX query/resolve loop (`get_text::ax_get_text`, `query_all::ax_query_all`,
`find_all::ax_find_all`). 공통 helper `cmd/ax.rs` 신규로 추출:

```rust
// cmd/ax.rs
pub(super) async fn traverse_visible_nodes(
    client: &CdpClient,
    role: &str,
    name: Option<&str>,
    limit: u32,
    fn_decl: &str,
) -> Result<Vec<Value>> {
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

    let mut results: Vec<Value> = Vec::new();
    for node in nodes {
        if results.len() as u32 >= limit { break; }
        if node.get("ignored").and_then(Value::as_bool) == Some(true) { continue; }
        let backend_id = match node.get("backendDOMNodeId").and_then(Value::as_u64) {
            Some(id) => id, None => continue,
        };
        let resolved = match client.send("DOM.resolveNode",
            json!({ "backendNodeId": backend_id })).await {
            Ok(r) => r, Err(_) => continue,
        };
        let object_id = match resolved.get("object").and_then(|o| o.get("objectId"))
            .and_then(Value::as_str) {
            Some(s) => s.to_owned(), None => continue,
        };
        let r = client.send("Runtime.callFunctionOn", json!({
            "objectId": object_id,
            "functionDeclaration": fn_decl,
            "returnByValue": true,
            "awaitPromise": true,
        })).await?;
        // null 반환 (metaOf에서 instance check 실패 등) 도 results에 안 넣음
        if let Some(v) = unwrap_runtime_result(&r, "Runtime.callFunctionOn")? {
            if !v.is_null() { results.push(v); }
        }
    }
    Ok(results)
}
```

기존 ax_get_text / ax_query_all도 이걸 위에 얹도록 변경:
- ax_get_text: limit=1, fn_decl이 단순 텍스트 반환, results[0]를 unwrap
- ax_query_all: 모든 results를 string array로 변환
- ax_find_all: results 그대로 (Vec<Value> = JSON objects)

### `ax_find_all`

```rust
async fn ax_find_all(
    client: &CdpClient,
    role: &str,
    name: Option<&str>,
    raw: bool,
    limit: u32,
) -> Result<Vec<Value>> {
    let meta_body = build_meta_js_body(raw);  // codex round 1 I1
    // metaOf already checks instanceof Element → returns null for non-Element nodes
    // (codex round 1 C1). traverse_visible_nodes 가 null 결과는 skip 함.
    let fn_decl = format!("function() {{\n{meta_body}\nreturn metaOf(this);\n}}");
    traverse_visible_nodes(client, role, name, limit, &fn_decl).await
}
```

### `ax_get_text` refactor (codex round 2 I1 — diagnostic 보존)

기존 get_text의 ax_get_text는 "no AX node matches" / "all ignored" 명시 에러를 출력했음.
traverse_visible_nodes로 옮기면 이 진단이 사라지므로, get_text path만 다시 명시:

```rust
async fn ax_get_text(
    client: &CdpClient,
    role: &str,
    name: Option<&str>,
    raw: bool,
) -> Result<Option<Value>> {
    let body = build_text_body(raw);
    let fn_decl = format!("function() {{ const target = this; {body} }}");
    let results = traverse_visible_nodes(client, role, name, 1, &fn_decl).await?;
    if results.is_empty() {
        bail!(
            "no AX node matches role={role}{}",
            name.map(|n| format!(" name={n:?}")).unwrap_or_default()
        );
    }
    Ok(Some(results.into_iter().next().unwrap()))
}
```

ax_query_all과 ax_find_all은 empty array가 정상 출력 (사용자가 array 다룸 — 미스는 빈 array).

## 검증 게이트

### 단위 테스트 (`cargo test`)

`build_find_all_expr` 패턴:
- selector 경로: 결과에 `tag`, `text`, `id`, `classes`, `attrs`, `rect` 키 모두 포함
- testid 경로: `el.dataset.testid === LIT` 필터 포함
- raw 경로: textContent 직접 반환 (build_text_body 재사용 확인)
- `slice(0, LIMIT)` 포함
- `ATTR_KEYS` whitelist 정확히 5개

### parity smoke 확장

spike-only (TS 동등 명령 없음). `fa_case` 헬퍼 — `qa_case`와 같은 set -e 우회 패턴.
출력은 JSON object array — bash에서 jq 없이 비교하려면 expected_stdout 전체 매치 어려움.
대신 **field-level 검증**: 출력을 jq나 node로 파싱해서 특정 필드 추출:

```bash
fa_case_field() {
  local label="$1" url="$2" jq_filter="$3" expected="$4"
  shift 4
  local actual_stdout actual_value stderr_file
  # stderr 분리 캡처 (codex round 1 I4) — stdout만 JSON 파싱.
  stderr_file="$(mktemp)"
  if ! actual_stdout="$("$SPIKE" find-all "$url" "$@" 2>"$stderr_file")"; then
    local err; err="$(cat "$stderr_file")"; rm -f "$stderr_file"
    report_fail "$label" "(spike failed: $err)" "($expected)"
    return
  fi
  rm -f "$stderr_file"
  # node 파싱/eval 실패도 set -e 트립하지 않도록 if-else로 격리
  # (codex round 2 I2)
  if ! actual_value="$(printf '%s' "$actual_stdout" | node -e "
      const arr = JSON.parse(require('fs').readFileSync(0, 'utf8'));
      const filter = process.argv[1];
      process.stdout.write(JSON.stringify(eval(filter)));
    " "$jq_filter" 2>/dev/null)"; then
    report_fail "$label" "(node parse/eval failed; stdout=$actual_stdout)" "($expected)"
    return
  fi
  if [[ "$actual_value" == "$expected" ]]; then
    report_pass "$label" "$actual_value"
  else
    report_fail "$label" "$actual_value" "$expected"
  fi
}
```

`jq_filter`는 JS expression — node가 평가. 예: `arr.map(x => x.tag)` → `["li","li"]`.

케이스 (10개):
| 케이스 | 검증 |
|---|---|
| selector tags | `arr.map(x => x.tag)` = `["li","li","li"]` |
| selector text | `arr.map(x => x.text)` = `["a","b","c"]` |
| selector id | id 필드 정확 추출 (또는 null) |
| selector classes | classes 배열 정확 |
| selector attrs whitelist | aria-label, role 같은 attribute 정확 추출 |
| selector rect | 모든 element가 x/y/w/h 가진 객체 |
| testid mode | tag 정확 (span 등) |
| role mode | --role button → 매치 button들 메타 |
| --limit caps | array length 정확 |
| empty match | `[]` |
| TARGET missing | exit 1 |
| --selector + --role | exit 2 |

### 회귀
- 기존 56 unit + 37 parity 케이스 그대로 통과

## 작업 순서

1. `/codex-plan` 으로 압박 테스트 (multi-round)
2. **AX traversal helper 추출** (`cmd/ax.rs` 신규) — `traverse_visible_nodes(client, role, name, limit, fn_decl) -> Result<Vec<Value>>`. null 결과 skip.
3. `cmd/get_text.rs::ax_get_text` / `cmd/query_all.rs::ax_query_all` 을 신규 helper 위에 얹도록 refactor. 단위 테스트는 그대로 통과해야 함.
4. `cmd/query_all.rs` — `validate_target_flags_strict` 와 `build_els_expr` 을 `pub(super)` 로 노출
5. `cmd/get_text.rs` — `build_meta_js_body(raw)` 신규 `pub(super)` 노출. build_text_body 위에 textOf + liveValueOf + metaOf 정의.
6. `cmd/find_all.rs` 신규 — build_find_all_expr + eval_to_value_array + ax_find_all + run
7. `cmd/mod.rs` — `pub mod ax;` + `pub mod find_all;` 추가
8. `src/main.rs` — `FindAll` 서브커맨드. clap arg group id: **`fa_target`**
9. 단위 테스트 (build_meta_js_body shape 검증, ATTR_KEYS 정확성, build_find_all_expr 패턴)
10. `tests/spike-parity.sh` — phase-1d 섹션 + fa_case_field 헬퍼 (stderr 분리 + node로 JSON 파싱)
11. `cargo test` + `npm run e2e:spike-parity` 모두 그린 (회귀 검증)
12. `codex-review.sh --uncommitted --context-file <brief>` → APPROVED
13. `~/save.sh "Add cdp-spike find-all for element metadata extraction"`

## 검토 포인트 (codex-plan)

1. `validate_target_flags_strict` 와 `build_els_expr` 의 `pub(super)` 노출 — query_all에서
   find_all로 cross-module 사용. 깔끔한지 vs 별도 helper 모듈 (`cmd/target.rs`) 추출 권장인지
2. ATTR_KEYS 5개 whitelist의 적정성 — id/class 별도 필드와의 분리 의도가 명확한가
3. id field가 `element.id || null` 인데 `null` vs `""` 결정 — `""` 도 빈 id로 본다는 명시
4. rect 좌표가 viewport 기준 — scroll된 페이지에서 사용자 혼란 가능. 명시 필요
5. eval_to_value_array가 각 element의 shape 검증 안 함 — JS가 잘못 반환한 경우 다운스트림
   crash. 방어 분기 필요한가
6. AX 경로에서 attrs.role이 element.getAttribute("role") — `<button>` 같은 implicit role은
   못 잡음 (DOM attribute는 없음, AX 시맨틱만). 명시
7. find-all 출력의 jq 없이 검증 — node 인라인이 jq 의존 회피의 좋은 방법인지

## 다음 단계 (1d 후)

- phase 2: daemon UDS 프로토콜 Rust 재구현 (TS CLI/MCP가 그대로 붙음)
- phase 3: 배포 형태 (단일 Rust 바이너리 vs hybrid)
