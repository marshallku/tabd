# Spike phase 1a — `get-text` 풀 시맨틱 + `data-testid` 별칭

이전 spike (master, 7+ 커밋, `crates/cdp-spike/`)에 새 명령 `get-text`를 추가해서
TS `chromium-cdp` 측 `dom.getText`와 동작 동등성을 달성한다.
`fetch-text`(phase 0, `textContent` 단독)는 그대로 둔다 — 두 명령의 역할이 다르다:

- `fetch-text <url> <selector>`: low-level, 단일 selector + 원본 `textContent ?? ''`
- `get-text <url> [...]`: high-level, default selector chain + `innerText` + 공백 압축

## 목표

1. **TS `dom.getText` 풀 시맨틱 재현** — byte-exact parity
2. **`data-testid` 별칭** — `--testid foo` 가 `[data-testid="foo"]` selector로 풀림

## Non-goals (이 phase에서 안 함)

- 접근성 트리 query (phase 1b로 분리)
- multi-element 추출 (querySelectorAll)
- shadow DOM penetration
- iframe 진입

## TS spec (src/server/runtimes/cdp.ts:854~872)

```ts
const selector = typeof params.selector === "string" ? params.selector : "main, article, body";
const raw = params.raw === true;
const code = `
    (() => {
        const target = document.querySelector(${JSON.stringify(selector)}) ?? document.body;
        if (${JSON.stringify(raw)}) return target.textContent ?? "";
        const text = target.innerText ?? target.textContent ?? "";
        return text.replace(/\n{3,}/g, "\n\n").trim();
    })()
`;
return String(await this.evaluate(targetId, code));
```

핵심 동작 분해:

| 측면 | 동작 |
|---|---|
| default selector | `"main, article, body"` — 단일 `querySelector` 호출 (CSS selector list, 첫 매치) |
| selector miss fallback | `?? document.body` — querySelector null이면 body 반환 |
| body 자체 없음 | `target`이 `null`이면 `.textContent` 접근 시 throw — 실용상 항상 body 존재 |
| raw 모드 | `target.textContent ?? ""` (collapse/trim 없음) |
| 기본 모드 추출 | `target.innerText ?? target.textContent ?? ""` |
| 기본 모드 정규화 | `replace(/\n{3,}/g, "\n\n")` — **3개 이상 연속 \n을 2개로 축약** (1, 2개 \n은 그대로) |
| 기본 모드 trim | 양 끝 공백/줄바꿈 제거 |
| 반환 타입 | TS는 `String(...)` 강제 변환 — Rust 측은 `Some(String)` |

## CLI 표면

```
cdp-spike get-text <url> [OPTIONS]

  --selector <CSS>     명시 CSS selector (기본: "main, article, body")
  --testid <ID>        data-testid 단축 — 내부적으로 [data-testid="ID"]로 변환
  --raw                textContent 원본 (collapse/trim 없음)
  --timeout <MS>       navigate timeout (default 30000)
```

상호 배타:
- `--selector` ↔ `--testid` 둘 다 주면 에러
- 둘 다 안 주면 default selector chain 사용

## `--testid` 별칭 변환 (codex round 1 break-point 1 반영)

CSS string escape는 newline/CR/form-feed/NUL/control-chars 까지 다뤄야 하고, 빈 문자열도
사실 `[data-testid=""]` 로 유효. 그래서 selector string을 build하지 **않고**, JS 측에서
**JS string equality 비교**로 매칭한다 (CSS attribute escape 우회):

```js
[...document.querySelectorAll('[data-testid]')]
  .find(el => el.dataset.testid === SAFE_JSON_LITERAL)
```

여기서 `SAFE_JSON_LITERAL`은 `serde_json::to_string(testid)` 결과 — JSON string 리터럴이라
모든 unicode/control char 안전. `[data-testid]` 자체는 attribute presence selector라
값 escape 무관.

장점:
- escape 분기 없음 (testid 값에 newline/quote/backslash 무엇이든 안전)
- 빈 문자열 허용 (TS와 동일)
- CSS spec 의존 제거

단점:
- 페이지에 `data-testid` 요소 많으면 미세하게 느림 (spike scope에서 무시)

단위 테스트는 `build_target_expr(testid=...)` 결과 string이 위 패턴을 포함하는지만 확인 —
실제 JS 동작 검증은 parity smoke에서.

## JavaScript expression 구성 (Rust 측)

```rust
fn build_get_text_expr(selector: Option<&str>, testid: Option<&str>, raw: bool) -> Result<String> {
    let raw_lit = serde_json::to_string(&raw)?;

    // target 분기 — selector / testid / default 셋 다 ?? document.body 폴백
    let target_expr = match (selector, testid) {
        (Some(s), None) => {
            let sel_lit = serde_json::to_string(s)?;
            format!("document.querySelector({sel_lit}) ?? document.body")
        }
        (None, Some(t)) => {
            let testid_lit = serde_json::to_string(t)?;
            format!(
                "([...document.querySelectorAll('[data-testid]')]\
                 .find(el => el.dataset.testid === {testid_lit})) ?? document.body"
            )
        }
        (None, None) => {
            // default selector chain — TS와 byte-exact
            r#"document.querySelector("main, article, body") ?? document.body"#.to_string()
        }
        (Some(_), Some(_)) => unreachable!("CLI arg group rejects --selector + --testid"),
    };

    Ok(format!(
        r#"(() => {{
            const target = {target_expr};
            if ({raw_lit}) return target.textContent ?? "";
            const text = target.innerText ?? target.textContent ?? "";
            return text.replace(/\n{{3,}}/g, "\n\n").trim();
        }})()"#
    ))
}
```

주의: `format!` macro 안에서 JS의 `{` `}`는 `{{` `}}` 로 escape.

## 검증 게이트

### 단위 테스트 (`cargo test`)

- `build_get_text_expr(selector, testid, raw)` 케이스:
  - default (selector=None, testid=None, raw=false): `main, article, body` chain + body 폴백
  - 명시 selector (selector=Some, testid=None, raw=false): `querySelector(LIT) ?? document.body`
  - testid (selector=None, testid=Some, raw=false): `querySelectorAll('[data-testid]').find(... === LIT)` 패턴 포함
  - raw=true: 결과 expression이 `if (true) return target.textContent ?? ""` 형태로 early-return하는지 확인 (builder는 collapse/trim 코드도 늘 포함하지만, raw 모드는 evaluate 시 early-return이 먼저 발동 → 실제 동작이 textContent 원본)
  - testid 값에 특수문자 (newline, 큰따옴표, 백슬래시, 빈 문자열) → 결과 expression이 serde_json escape 사용했는지
- mutually exclusive flag 검증 (--selector + --testid 동시 에러) — clap arg group 단위 테스트 또는 통합 테스트

### parity 확장 (`tests/spike-parity.sh`) — codex round 1 break-points 2/3/4 반영

기존 5 케이스 유지 + 새 케이스 추가:

| 케이스 | spike 명령 | TS 측 dom.getText params | 검증 |
|---|---|---|---|
| default → main 우선 | `get-text data:text/html,<main>M</main><article>A</article>` | `{}` | "M" |
| default → body fallback | `get-text data:text/html,<body>Plain</body>` (main/article 없음) | `{}` | "Plain" |
| --selector hit | `get-text ... --selector h1` (페이지에 h1 존재) | `{selector:"h1"}` | "T" |
| **--selector miss → body 폴백** | `get-text ... --selector no-match` | `{selector:"no-match"}` | body 텍스트 |
| --testid hit (simple ID) | `get-text ... --testid x` | `{selector:"[data-testid=\"x\"]"}` (TS dom.getText로 동치 selector 호출) | "V" |
| --testid miss → body (simple ID) | `get-text ... --testid does-not-exist` | `{selector:"[data-testid=\"does-not-exist\"]"}` | body 텍스트 |
| **default 모드 collapse** | `<pre>` 안에 4개 \n 분리된 문자 (base64로 인코딩) | `{}` | `a\n\nb` (3+ → 2) |
| **raw 모드 보존** | 같은 페이지, `--raw` | `{raw:true}` | `a\n\n\n\nb` |
| default 모드 trim | `<body>  trim me  </body>` | `{}` | "trim me" |

**Boolean / selector 전달 주의 (break-point 2)**:
TS 측 `dom.getText` 호출 시:
- `raw` 는 반드시 JS `true`/`false` boolean — env var를 `RAW=1|0`으로 받아서
  `params.raw = process.env.RAW === '1'` 로 변환 (`"true"` string은 TS의 `=== true`에 false)
- `selector` 가 없을 때는 `params.selector`를 **omit** — `""` 빈 문자열로 넘기면 TS가
  `querySelector("")` 호출해서 throw

**newline 인코딩 주의 (break-point 4)**:
`data:` URL은 base64 인코딩으로 만든다 — bash의 `data:text/html,<pre>a\n\n\n\nb</pre>`는
\n이 literal 두 글자라 newline이 아니다. 양쪽 사이드 모두에 같은 base64 URL 사용:

```bash
make_data_url() {
  local html="$1"
  local b64
  b64="$(printf '%s' "$html" | base64 -w0)"
  printf 'data:text/html;base64,%s' "$b64"
}

# 호출 측은 반드시 C-quoted string ($'...')로 actual newline을 전달.
# "<pre>a\n\nb</pre>" 같은 일반 double quote는 \n이 literal 두 글자 그대로 들어간다.
url="$(make_data_url $'<pre>a\n\n\n\nb</pre>')"
```

`dom.getText`는 evaluate가 아닌 별도 action — TS 호출 경로:
`r.execute("dom.getText", params)` → BridgeResponse `.data` 는 string. parity 헬퍼
`ts_get_text_to(url, selector_opt, raw, out)` 신규.

testid 케이스의 parity 전략 (round 2 break-point 1 반영) — spike builder는 private이라
bash에서 expression을 직접 호출할 수 없다. 두 단계로 나눠 검증:

- **단순 ID** (`x`, `my-btn` 같이 CSS attribute selector에 그대로 들어갈 수 있는 값):
  parity smoke에서 spike `--testid x` 결과를 TS `dom.getText({selector: "[data-testid=\"x\"]"})`
  결과와 byte-exact 비교. simple ID 범위에서는 두 selector 방식이 동등.
- **특수 문자 포함 ID** (newline / 큰따옴표 / 백슬래시 등 — CSS attribute escape 영역):
  parity smoke로는 검증하지 않음. spike 단위 테스트에서 `build_get_text_expr(None, Some(weird), _)`
  결과 expression의 구조만 (serde_json escape 포함 여부) 확인. 실제 페이지 매칭 동작은
  `cdp_evaluate_roundtrip` 류 ignored smoke로 옵션 (필수 아님).

### 회귀

- 기존 5 parity 케이스 그대로 통과
- 기존 16 unit test 그대로 통과
- launch_smoke / cdp_evaluate_roundtrip ignored smoke 그대로 통과

## 작업 순서

1. **이 plan을 `/codex-plan`으로 압박 테스트** (multi-round, 같은 thread)
2. CLI에 `get-text` 서브커맨드 추가 (`src/main.rs`) — selector/testid mutually exclusive (clap arg group)
3. `src/cmd/get_text.rs` 신규:
   - `build_get_text_expr(selector: Option<&str>, testid: Option<&str>, raw: bool) -> Result<String>` 단일 함수 + 단위 테스트
     (CSS attribute escape 함수 별도 추출하지 않음 — JS 측 string equality로 회피)
   - `run(url, selector, testid, raw, timeout_ms)` — page::open + evaluate_value + 출력
4. `src/cmd/mod.rs`에 `pub mod get_text;` 추가
5. `tests/spike-parity.sh` 확장 — `ts_get_text(url, selector, raw)` 헬퍼 + 6~7 신규 케이스
6. `cargo test` + `npm run e2e:spike-parity` 모두 그린 (e2e:spike-parity = `npm run build` + cargo release + parity 한 번에. `bash tests/spike-parity.sh` 직접 호출은 dist 갱신 누락 위험 — package script 경유 강제)
7. `bash ~/.claude/scripts/codex-review.sh --uncommitted --context-file <brief>` 통과
8. `~/save.sh "Add cdp-spike get-text with full TS semantics and testid alias"`

## 검토 포인트 (codex-plan 라운드 시 확인 요청)

1. `build_get_text_expr`의 `format!` 안 JS escape (`{` `}` → `{{` `}}`)이 모든 케이스에서 정확한지
2. default selector chain의 단일 querySelector 동작이 TS와 일치하는지 (`"a, b, c"` CSS list semantics)
3. `--testid` JS string equality 경로 — testid 값이 `serde_json::to_string` JSON 리터럴로 안전하게 embed 되는지 (CSS attribute escape 분기는 의도적으로 회피)
4. parity smoke가 `dom.getText` action을 직접 부르도록 변경된 부분 — BridgeResponse.data 추출 경로 변동 없음 확인
5. mutually exclusive flag 검증을 clap arg group으로 vs 수동 분기로 처리할지

## 다음 단계 (이 phase 후)

phase 1b — 접근성 트리 query:
- `Accessibility.enable` + `Accessibility.getFullAXTree` 또는 `Accessibility.queryAXTree`
- `--role button --name "Submit"` 패턴 + AX 노드 → DOM 노드 매핑
- 별도 plan 문서로 분리
