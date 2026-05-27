# Spike phase 1b — Accessibility tree query

`get-text`에 `--role <ROLE> [--name <NAME>]` 추가. 매치 노드는 CDP `Accessibility`
도메인의 `queryAXTree`로 찾고, 텍스트 추출은 기존 get-text 로직(`innerText`, collapse,
trim, raw) 그대로 재사용. spike의 새 CDP 도메인 학습 가치 + 일관된 인터페이스.

## 목표

1. **CDP `Accessibility` 도메인 도입** — `Accessibility.queryAXTree` 사용 (spike에서
   첫 비-Target/Page/Runtime/DOM 도메인)
2. **role + accessible name 기반 노드 query** — Chromium이 계산한 AX 시맨틱 직접 활용
   (aria-label, label, computed name 등 다 포함)
3. **기존 get-text 로직 재사용** — 텍스트 추출 방식 (innerText/textContent/collapse/trim/raw)은
   selector/testid/role 모두 동일

## Non-goals (이 phase에서 안 함)

- `Accessibility.getFullAXTree` 트리 dump (사용자 향 UI 아님 — spike scope 외)
- name **부분/regex 매치** — CDP queryAXTree는 정확 매치만 지원
- **여러 매치** 처리 — 첫 매치만 반환
- TS `chromium-cdp`와 byte-exact parity — TS는 Accessibility 도메인 안 씀
- shadow DOM / iframe AX 트리 진입

## CDP `Accessibility` 도메인 흐름

```
1. Accessibility.enable                    (도메인 활성화)
2. DOM.getDocument                         { depth: 0 } → root nodeId
3. Accessibility.queryAXTree               { nodeId: root, role?: ROLE, accessibleName?: NAME }
                                           → { nodes: [{ nodeId, role.value, name.value, backendDOMNodeId, ... }] }
4. nodes[0].backendDOMNodeId 추출
5. DOM.resolveNode                         { backendNodeId } → { object: { objectId } }
6. Runtime.callFunctionOn                  { objectId, functionDeclaration, returnByValue: true }
                                           → 기존 get-text 텍스트 추출 (innerText/raw)
7. evaluate_value 와 동일한 결과 unwrap (Value::String → stdout)
```

각 단계에 spike 한도 적용:
- `Accessibility.enable`는 connect 시 일괄이 아닌 query 시점 lazy enable
- queryAXTree는 root nodeId에 대해 한 번만 호출 (whole-document scope)
- nodes 배열이 비어있으면 → "no AX node matches" 명확한 에러 (selector miss와 다름 — body fallback 안 함)
- **`ignored` 노드 필터 (codex round 1 C1)**: CDP는 hidden / aria-hidden / 시각적으로 가려진
  노드도 `ignored: true` 플래그와 함께 반환한다. spike는 사용자가 인지 가능한 노드만 매치
  하도록 `nodes.iter().find(|n| n.get("ignored").and_then(Value::as_bool) != Some(true))`
  로 첫 매치. 매치 노드가 없으면 (모두 ignored) → fail. ignored 노드까지 원하는 경우는
  spike scope 외 (`--include-ignored` 같은 플래그 도입은 phase 1c 이후).
- backendDOMNodeId가 없는 노드 (예: AX-only 노드, virtual node) → fail
- resolveNode가 detached 노드 → fail
- **CDP send timeout 없음 (codex round 1 I1)**: `CdpClient::dispatch`는 인덴파인 wait이라
  `--timeout`은 page::open만 보호. AX whole-subtree 계산이 길어지는 경우 명령 자체가
  hang 가능. spike scope에서는 acceptable 한도 — 명시.

## CLI 표면 확장

```
cdp-spike get-text <url> [OPTIONS]

  --selector <CSS>           (기존)
  --testid <ID>              (기존)
  --role <ROLE>              (신규) ARIA role (button, link, heading, etc.)
  --name <NAME>              (신규, --role과 같이) accessible name 정확 매치
  --raw                      (기존)
  --timeout <MS>             (기존)
```

상호 배타 (clap arg group `gt_target`):
- `--selector` ↔ `--testid` ↔ `--role` 셋 다 한 그룹, 동시 사용 금지
- `--name`은 `--role`과만 같이 — `--selector`/`--testid` + `--name` 조합은 에러

검증 분기:
- `--name` 있는데 `--role` 없음 → 에러 ("--name requires --role")
- `--role` 단독 → role 매치하는 첫 AX 노드
- `--role + --name` → 둘 다 정확 매치 첫 AX 노드

## Rust 구현 위치

핵심 — **JS 텍스트 추출 본문은 단일 함수로 추출, 두 경로가 재사용** (codex round 1 C2 반영).
`build_text_body(raw: bool)` 가 `target` 식별자 위에서 동작하는 JS 본문을 반환:

```rust
// cmd/get_text.rs
fn build_text_body(raw: bool) -> String {
    let raw_lit = serde_json::to_string(&raw).unwrap();  // "true" or "false"
    format!(
        r#"if ({raw_lit}) return target.textContent ?? "";
const text = target.innerText ?? target.textContent ?? "";
return text.replace(/\n{{3,}}/g, "\n\n").trim();"#
    )
}
```

selector/testid/default 경로는 `(() => {{ const target = {target_expr}; {body} }})()` 로
Runtime.evaluate. AX 경로는 `function() {{ const target = this; {body} }}` 로
Runtime.callFunctionOn. 두 경로 모두 같은 `build_text_body(raw)` 호출 → drift 불가.

기존 `build_get_text_expr`도 이 helper 위에 thin wrapper로 재작성:

```rust
fn build_get_text_expr(
    selector: Option<&str>,
    testid: Option<&str>,
    raw: bool,
) -> Result<String> {
    let target_expr = build_target_expr(selector, testid)?;  // 기존 match 분기
    let body = build_text_body(raw);
    Ok(format!("(() => {{ const target = {target_expr};\n{body} }})()"))
}
```

run() 구조:

```rust
pub async fn run(
    url: &str,
    selector: Option<&str>,
    testid: Option<&str>,
    role: Option<&str>,
    name: Option<&str>,
    raw: bool,
    timeout_ms: u64,
) -> Result<()> {
    validate_target_flags(selector, testid, role, name)?;
    let (browser, client) = page::open(url, timeout_ms).await?;

    let result = if let Some(r) = role {
        // AX 경로
        ax_get_text(&client, r, name, raw).await
    } else {
        // selector/testid/default 경로
        let expr = build_get_text_expr(selector, testid, raw)?;
        evaluate_value(&client, &expr).await
    };

    let _ = page::teardown(browser, client).await;
    // 기존 출력 분기 (String → stdout, 그 외 → JSON, None → error)
}

fn validate_target_flags(
    selector: Option<&str>,
    testid: Option<&str>,
    role: Option<&str>,
    name: Option<&str>,
) -> Result<()> {
    let count = [selector.is_some(), testid.is_some(), role.is_some()]
        .iter().filter(|&&x| x).count();
    if count > 1 {
        bail!("--selector, --testid, --role are mutually exclusive");
    }
    if name.is_some() && role.is_none() {
        bail!("--name requires --role");
    }
    Ok(())
}

async fn ax_get_text(
    client: &CdpClient,
    role: &str,
    name: Option<&str>,
    raw: bool,
) -> Result<Option<Value>> {
    // 1. Accessibility.enable (idempotent — CDP spec)
    client.send("Accessibility.enable", json!({})).await?;

    // 2. DOM.getDocument → root nodeId
    let doc = client.send("DOM.getDocument", json!({ "depth": 0 })).await?;
    let root_node_id = doc.get("root").and_then(|r| r.get("nodeId"))
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("DOM.getDocument missing root.nodeId"))?;

    // 3. queryAXTree (role 매치는 implicit role도 포함 — CDP spec)
    let mut params = json!({ "nodeId": root_node_id, "role": role });
    if let Some(n) = name { params["accessibleName"] = Value::String(n.to_owned()); }
    let q = client.send("Accessibility.queryAXTree", params).await?;
    let nodes = q.get("nodes").and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Accessibility.queryAXTree missing nodes"))?;

    // 4. Ignored 노드 필터 (codex C1) → 첫 visible 매치
    let visible = nodes.iter().find(|n| {
        n.get("ignored").and_then(Value::as_bool) != Some(true)
    });
    let node = visible.ok_or_else(|| anyhow!(
        "no visible AX node matches role={role}{} (all {} match(es) are ignored)",
        name.map(|n| format!(" name={n:?}")).unwrap_or_default(),
        nodes.len()
    ))?;

    let backend_node_id = node.get("backendDOMNodeId")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("AX node missing backendDOMNodeId (virtual node?)"))?;

    // 5. DOM.resolveNode → objectId
    let resolved = client.send("DOM.resolveNode", json!({ "backendNodeId": backend_node_id })).await?;
    let object_id = resolved.get("object").and_then(|o| o.get("objectId"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("DOM.resolveNode missing object.objectId"))?
        .to_owned();

    // 6. Runtime.callFunctionOn — 같은 build_text_body 재사용
    let body = build_text_body(raw);
    let fn_decl = format!("function() {{ const target = this;\n{body} }}");
    let r = client.send("Runtime.callFunctionOn", json!({
        "objectId": object_id,
        "functionDeclaration": fn_decl,
        "returnByValue": true,
    })).await?;
    unwrap_runtime_result(&r, "Runtime.callFunctionOn")
}
```

`unwrap_runtime_result` 추출 (eval.rs) — codex round 1 I2 반영, operation label 인자:

```rust
// cmd/eval.rs
pub fn unwrap_runtime_result(raw: &Value, op: &str) -> Result<Option<Value>> {
    if let Some(exc) = raw.get("exceptionDetails") {
        let msg = exc.get("exception")
            .and_then(|e| e.get("description"))
            .and_then(Value::as_str)
            .or_else(|| exc.get("text").and_then(Value::as_str))
            .unwrap_or("evaluation threw");
        bail!("{op}: {msg}");
    }
    let result_obj = raw.get("result")
        .ok_or_else(|| anyhow!("{op} response missing 'result'"))?;
    if matches!(result_obj.get("type").and_then(Value::as_str), Some("undefined")) {
        return Ok(None);
    }
    if let Some(unser) = result_obj.get("unserializableValue").and_then(Value::as_str) {
        return Ok(Some(Value::String(unser.to_owned())));
    }
    if let Some(value) = result_obj.get("value") {
        return Ok(Some(value.clone()));
    }
    let type_str = result_obj.get("type").and_then(Value::as_str).unwrap_or("<no type>");
    bail!("{op} returned a non-serializable {type_str}");
}

pub async fn evaluate_value(client: &CdpClient, expr: &str) -> Result<Option<Value>> {
    let raw = client.send("Runtime.evaluate", json!({
        "expression": expr, "returnByValue": true, "awaitPromise": true,
    })).await?;
    unwrap_runtime_result(&raw, "Runtime.evaluate")
}
```

## 검증 게이트

### 단위 테스트 (`cargo test`)

- `validate_target_flags` 6 케이스:
  - all None → OK
  - selector only / testid only / role only → 각각 OK
  - selector + testid → fail
  - selector + role → fail
  - testid + role → fail
  - role + name → OK
  - name without role → fail
- `unwrap_runtime_result` 추출 시 기존 eval 단위 테스트가 evaluate_value 경로 + 새 callFunctionOn 경로 모두 커버하는지 확인 (분리 후 evaluate_value 단위 테스트 유지)
- AX 호출 시퀀스 자체는 라이브 (Chromium 의존)

### parity 확장 (`tests/spike-parity.sh`)

TS는 Accessibility 도메인 안 쓰므로 byte-exact parity 불가. 대신 **spike-only 라이브 스모크**
케이스 추가:

| 케이스 | spike 명령 | 검증 |
|---|---|---|
| --role hit | `get-text data:text/html,<button>Click</button> --role button` | "Click" |
| --role + --name hit | `... <button>X</button><button>Click</button> --role button --name "Click"` | "Click" |
| --role + name (aria-label, computed name 진가) | `... <button aria-label="Save changes">💾</button> --role button --name "Save changes"` | "💾" (DOM text — aria-label 매치 후 button 내부 텍스트 반환). codex round 1 I3 반영 |
| --role + name (`<label for>`, computed name 진가) | `... <label for="email">Email</label><input id="email" type="text" value="x"> --role textbox --name "Email"` | "" (input element text is empty; AX matched, DOM text 빈 string). codex round 1 I3 반영 |
| --role miss | `... <p>Plain</p> --role button` | exit 1 + error 메시지 |
| --role + --name miss | `... <button>X</button> --role button --name "NotHere"` | exit 1 |
| --role hit + ignored 필터 | `... <button aria-hidden="true">Hidden</button><button>Visible</button> --role button` | "Visible" (hidden 노드 건너뛰기, codex round 1 C1 반영) |
| --role + --name + --raw | `... <button>  Trim me  </button> --role button --name "Trim me" --raw` | "  Trim me  " (정렬 유지) |
| --selector + --role | (clap arg group 거절) | exit 2 |
| --name without --role | (validate_target_flags 거절) | exit 1 |

별도 PARITY 라벨 분리:
```
== spike phase-1b accessibility (spike-only, no TS parity) ==
PASS  --role hit  → Click
...
```

기존 TS-parity 14 케이스는 그대로 유지.

### 회귀

- 기존 24 unit test + 14 parity 케이스 그대로 통과
- 기존 cmd/get_text.rs 의 selector/testid/default 경로 시그니처 변경 — caller (main.rs) 만 영향

## 작업 순서

1. **이 plan을 `/codex-plan`으로 압박 테스트** (multi-round, 같은 thread)
2. `cmd/eval.rs` — `unwrap_runtime_result(raw)` pub helper 추출, `evaluate_value`는 그 위에 thin wrapper
3. `cmd/get_text.rs` — `run` 시그니처 확장 (role/name), `validate_target_flags` 신규, `ax_get_text` 신규
4. `cmd/get_text.rs` 단위 테스트 — validate_target_flags 6 케이스
5. `src/main.rs` — `--role`/`--name` 인자 추가, 같은 `gt_target` arg group에 role 포함
6. `tests/spike-parity.sh` — phase-1b 섹션 신규 (spike-only 케이스 5~7개)
7. `cargo test` + `npm run e2e:spike-parity` 모두 그린
8. `codex-review.sh --uncommitted --context-file <brief>` → APPROVED
9. `~/save.sh "Add cdp-spike get-text accessibility role/name query"`

## 검토 포인트 (codex-plan 라운드 시 확인 요청)

1. `validate_target_flags` 분기 매트릭스가 완전한지 (selector+name / testid+name 같은 잘못된 조합도 다루는가)
2. `Accessibility.enable`을 lazy 호출 — 두 번째 query 시 idempotent인지 (CDP 보장)
3. `DOM.getDocument`의 root nodeId가 navigation 후 invalidate되는 케이스 — 매 query마다 재호출 vs 캐시?
4. `Accessibility.queryAXTree`의 `nodeId` 파라미터는 DOM nodeId vs AX nodeId — 명확화
5. `backendDOMNodeId` 없는 AX-only 노드 (예: text의 `text alternative`) 처리
6. `Runtime.callFunctionOn`의 functionDeclaration 안 JS escape — Rust string 안 `\\n` vs `\n` (현재 plan 코드는 `\\n` 사용)
7. role 매치 시 implicit role (예: `<button>` 의 `button` role) 도 매치되는지 — CDP spec 확인
8. `unwrap_runtime_result` 추출이 dead code 만들지 않는지 (evaluate_value 단위 테스트가 helper 분리 후도 의미 있는지)

## 다음 단계 (이 phase 후)

phase 1c — 진정한 multi-element 동시 추출:
- `find-all-by-role <url> <role>` → 매치된 노드들의 텍스트 array (JSON)
- 사용자 향 페이지 탐색 UX 완성

phase 2 (별개 plan) — daemon UDS 프로토콜 Rust 재구현 ... (기존 spike plan과 동일)
