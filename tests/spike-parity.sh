#!/usr/bin/env bash
# spike-parity.sh — compare `cdp-spike` stdout against the TS `chromium-cdp`
# runtime for the same data: URL scenarios. Byte-exact comparison via `cmp`
# (preserves trailing newlines, unlike `$(...)` capture).
#
# Pre-reqs (no external CLI tools beyond what npm + cargo already bring):
#   - cargo build --release --manifest-path crates/cdp-spike/Cargo.toml
#   - npm run build  (produces dist/server/runtime.js)
#   - node + cargo on $PATH
#
# To avoid clashing with a running daemon (spike plan codex C2), the TS side
# uses BROWSER_USER_DATA_DIR=$(mktemp) and BROWSER_DEBUG_PORT=19222.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SPIKE="${ROOT_DIR}/crates/cdp-spike/target/release/cdp-spike"
DIST_RUNTIME="${ROOT_DIR}/dist/server/runtime.js"

if [[ ! -x "$SPIKE" ]]; then
  echo "Missing spike binary. Run: cargo build --release --manifest-path crates/cdp-spike/Cargo.toml" >&2
  exit 2
fi
if [[ ! -f "$DIST_RUNTIME" ]]; then
  echo "Missing TS dist. Run: npm run build" >&2
  exit 2
fi

cd "$ROOT_DIR"

# Resolve a Chromium binary once for both sides. The TS chromium-cdp runtime
# only checks /usr/bin paths and does not auto-discover Playwright's bundled
# Chromium (src/server/runtimes/cdp.ts:400). The spike does, but for parity
# we force both to use the same binary.
resolve_chromium() {
  if [[ -n "${BROWSER_EXECUTABLE:-}" && -x "$BROWSER_EXECUTABLE" ]]; then
    printf '%s' "$BROWSER_EXECUTABLE"
    return
  fi
  for c in google-chrome google-chrome-stable chromium chromium-browser; do
    if command -v "$c" >/dev/null 2>&1; then
      command -v "$c"
      return
    fi
  done
  local best=""
  local best_ver=0
  shopt -s nullglob
  for d in "$HOME"/.cache/ms-playwright/chromium-*/chrome-linux64/chrome; do
    [[ -x "$d" ]] || continue
    local ver
    ver="$(echo "$d" | sed -E 's|.*/chromium-([0-9]+)/.*|\1|')"
    if (( ver > best_ver )); then
      best_ver=$ver
      best="$d"
    fi
  done
  shopt -u nullglob
  [[ -n "$best" ]] && printf '%s' "$best"
}

CHROMIUM_BIN="$(resolve_chromium || true)"
if [[ -z "$CHROMIUM_BIN" ]]; then
  echo "No Chromium binary found. Set BROWSER_EXECUTABLE or install chromium." >&2
  exit 2
fi
echo "chromium: $CHROMIUM_BIN"

# JSON-encode a string from bash via node (avoids a jq dependency).
json_string() {
  node -e 'process.stdout.write(JSON.stringify(process.argv[1]))' "$1"
}

# Run the same JS expression against the TS chromium-cdp runtime and emit its
# stdout to file. Uses console.log so trailing newline matches the spike side
# (where println! adds one) — byte-exact comparison via cmp is then meaningful
# (codex C2 from prior review round).
ts_eval_to() {
  local url="$1" code="$2" out="$3"
  local tmp
  tmp="$(mktemp -d -t cdp-spike-parity.XXXX)"
  BROWSER_RUNTIME=chromium-cdp \
  BROWSER_EXECUTABLE="$CHROMIUM_BIN" \
  BROWSER_USER_DATA_DIR="$tmp" \
  BROWSER_DEBUG_PORT=19222 \
  PARITY_URL="$url" \
  PARITY_CODE="$code" \
  node --input-type=module -e '
    const { createRuntime } = await import("./dist/server/runtime.js");
    const r = createRuntime();
    await r.init();
    const url = process.env.PARITY_URL;
    const code = process.env.PARITY_CODE;
    const nav = await r.execute("tabs.navigate", { url });
    if (!nav.success) { console.error("TS navigate failed:", nav); process.exit(1); }
    const res = await r.execute("execution.executeJs", { code });
    if (!res.success) { console.error("TS evaluate failed:", res); process.exit(1); }
    console.log(typeof res.data === "string" ? res.data : JSON.stringify(res.data));
    await r.close();
  ' > "$out"
  local rc=$?
  rm -rf "$tmp"
  return $rc
}

# Encode arbitrary HTML (including real newlines) as a base64 data: URL so both
# sides receive byte-identical input and shell quoting cannot corrupt newlines.
# Callers MUST pass actual newlines (use $'...' C-quoted strings or here-docs);
# regular double-quoted strings keep \n as literal backslash-n.
make_data_url() {
  local html="$1"
  local b64
  b64="$(printf '%s' "$html" | base64 -w0)"
  printf 'data:text/html;base64,%s' "$b64"
}

# Run TS dom.getText action. params built from env to avoid shell-quoting traps:
#   PARITY_URL — data: URL
#   PARITY_SELECTOR_PRESENT — "1" to include params.selector
#   PARITY_SELECTOR — selector string (only read when SELECTOR_PRESENT=1)
#   PARITY_RAW — "1" → true, anything else → false (TS checks `=== true`)
ts_get_text_to() {
  local url="$1" selector_opt="$2" raw="$3" out="$4"
  local tmp
  tmp="$(mktemp -d -t cdp-spike-parity.XXXX)"
  local sel_present="0"
  if [[ -n "${selector_opt}" ]]; then
    sel_present="1"
  fi
  BROWSER_RUNTIME=chromium-cdp \
  BROWSER_EXECUTABLE="$CHROMIUM_BIN" \
  BROWSER_USER_DATA_DIR="$tmp" \
  BROWSER_DEBUG_PORT=19222 \
  PARITY_URL="$url" \
  PARITY_SELECTOR_PRESENT="$sel_present" \
  PARITY_SELECTOR="$selector_opt" \
  PARITY_RAW="$raw" \
  node --input-type=module -e '
    const { createRuntime } = await import("./dist/server/runtime.js");
    const r = createRuntime();
    await r.init();
    const nav = await r.execute("tabs.navigate", { url: process.env.PARITY_URL });
    if (!nav.success) { console.error("TS navigate failed:", nav); process.exit(1); }
    const params = {};
    if (process.env.PARITY_SELECTOR_PRESENT === "1") {
      params.selector = process.env.PARITY_SELECTOR;
    }
    params.raw = process.env.PARITY_RAW === "1";
    const res = await r.execute("dom.getText", params);
    if (!res.success) { console.error("TS dom.getText failed:", res); process.exit(1); }
    console.log(typeof res.data === "string" ? res.data : JSON.stringify(res.data));
    await r.close();
  ' > "$out"
  local rc=$?
  rm -rf "$tmp"
  return $rc
}

# Run only navigate via TS (no executeJs); used for the navigate-only case.
ts_navigate() {
  local url="$1"
  local tmp
  tmp="$(mktemp -d -t cdp-spike-parity.XXXX)"
  BROWSER_RUNTIME=chromium-cdp \
  BROWSER_EXECUTABLE="$CHROMIUM_BIN" \
  BROWSER_USER_DATA_DIR="$tmp" \
  BROWSER_DEBUG_PORT=19222 \
  PARITY_URL="$url" \
  node --input-type=module -e '
    const { createRuntime } = await import("./dist/server/runtime.js");
    const r = createRuntime();
    await r.init();
    const nav = await r.execute("tabs.navigate", { url: process.env.PARITY_URL });
    if (!nav.success) { console.error("TS navigate failed:", nav); process.exit(1); }
    await r.close();
  '
  local rc=$?
  rm -rf "$tmp"
  return $rc
}

PASS_COUNT=0
FAIL_COUNT=0

report_pass() {
  printf "PASS  %s%s\n" "$1" "${2:+  → $2}"
  PASS_COUNT=$((PASS_COUNT + 1))
}
report_fail() {
  printf "FAIL  %s\n" "$1"
  [[ -n "${2:-}" ]] && printf "  spike: %s\n" "$2"
  [[ -n "${3:-}" ]] && printf "  ts:    %s\n" "$3"
  FAIL_COUNT=$((FAIL_COUNT + 1))
}

# navigate-only case: both sides must exit 0 (spike plan C1 from prior review —
# this case had no TS invocation before).
case_navigate() {
  local label="$1" url="$2"
  if ! "$SPIKE" navigate "$url" >/dev/null 2>&1; then
    report_fail "$label" "(spike exit != 0)" "(skipped)"
    return
  fi
  if ! ts_navigate "$url" >/dev/null 2>&1; then
    report_fail "$label" "(ok)" "(ts exit != 0)"
    return
  fi
  report_pass "$label" "both exit 0"
}

# Byte-exact stdout compare via cmp -s (preserves trailing newlines).
case_eval() {
  local label="$1" url="$2" code="$3"
  local sf tf
  sf="$(mktemp)"
  tf="$(mktemp)"
  "$SPIKE" eval "$url" "$code" > "$sf"
  ts_eval_to "$url" "$code" "$tf"
  if cmp -s "$sf" "$tf"; then
    report_pass "$label" "$(cat "$sf" | tr -d '\n')"
    rm -f "$sf" "$tf"
  else
    report_fail "$label" "$(xxd "$sf" | head -1)" "$(xxd "$tf" | head -1)"
    rm -f "$sf" "$tf"
  fi
}

case_fetch_text() {
  local label="$1" url="$2" selector="$3"
  local sf tf
  sf="$(mktemp)"
  tf="$(mktemp)"
  "$SPIKE" fetch-text "$url" "$selector" > "$sf"
  # Mirror spike's build_text_expr exactly so the JS executed on the TS side
  # is byte-identical to what spike injects.
  local lit
  lit="$(json_string "$selector")"
  local code="(document.querySelector(${lit})?.textContent) ?? ''"
  ts_eval_to "$url" "$code" "$tf"
  if cmp -s "$sf" "$tf"; then
    report_pass "$label" "$(cat "$sf" | tr -d '\n')"
    rm -f "$sf" "$tf"
  else
    report_fail "$label" "$(xxd "$sf" | head -1)" "$(xxd "$tf" | head -1)"
    rm -f "$sf" "$tf"
  fi
}

# get-text parity. spike_selector / spike_testid: at most one is non-empty.
# When both empty, spike uses default chain. ts_selector mirrors spike's effective
# selector — for testid, it's "[data-testid=\"X\"]" which is equivalent for plain IDs.
case_get_text() {
  local label="$1" url="$2"
  local spike_selector="$3" spike_testid="$4"
  local ts_selector="$5"
  local raw="$6"   # "1" or "0"
  local sf tf
  sf="$(mktemp)"
  tf="$(mktemp)"
  local args=(get-text "$url")
  if [[ -n "$spike_selector" ]]; then args+=(--selector "$spike_selector"); fi
  if [[ -n "$spike_testid" ]]; then args+=(--testid "$spike_testid"); fi
  if [[ "$raw" == "1" ]]; then args+=(--raw); fi
  "$SPIKE" "${args[@]}" > "$sf"
  ts_get_text_to "$url" "$ts_selector" "$raw" "$tf"
  if cmp -s "$sf" "$tf"; then
    report_pass "$label" "$(cat "$sf" | tr -d '\n')"
    rm -f "$sf" "$tf"
  else
    report_fail "$label" "$(xxd "$sf" | head -1)" "$(xxd "$tf" | head -1)"
    rm -f "$sf" "$tf"
  fi
}

echo "== spike parity smoke =="

case_navigate "navigate data:" "data:text/html,<h1>Hi</h1>"

case_eval "eval document.title (T)" \
  "data:text/html,<title>T</title><h1>x</h1>" \
  "document.title"

case_eval "eval 1+1" \
  "data:text/html,<h1>Hi</h1>" \
  "1+1"

case_fetch_text "fetch-text h1" \
  "data:text/html,<h1>Hi</h1>" \
  "h1"

case_fetch_text "fetch-text no-match" \
  "data:text/html,<h1>x</h1>" \
  "no-match"

# get-text: default chain (selector empty on both sides → TS uses "main, article, body")
case_get_text "get-text default chain → body" \
  "data:text/html,<main>M</main><article>A</article><body>B</body>" \
  "" "" "" "0"

case_get_text "get-text default chain (body-only page)" \
  "data:text/html,<body>Plain body</body>" \
  "" "" "" "0"

# get-text: explicit selector
case_get_text "get-text --selector h1" \
  "data:text/html,<h1>Heading</h1><p>Body</p>" \
  "h1" "" "h1" "0"

# get-text: selector miss → body fallback (TS's ?? document.body)
case_get_text "get-text --selector miss → body" \
  "data:text/html,<body>Fallback content</body>" \
  "no-match-here" "" "no-match-here" "0"

# get-text: --testid (simple ID). TS-side uses equivalent [data-testid="..."].
case_get_text "get-text --testid hit (simple ID)" \
  "data:text/html,<span data-testid=x>V</span><body>B</body>" \
  "" "x" '[data-testid="x"]' "0"

case_get_text "get-text --testid miss → body" \
  "data:text/html,<body>Body again</body>" \
  "" "does-not-exist" '[data-testid="does-not-exist"]' "0"

# get-text: trim. Default mode strips outer whitespace.
case_get_text "get-text default trim" \
  "data:text/html,<body>  trim me  </body>" \
  "" "" "" "0"

# get-text: raw vs default newline collapse. Use base64 data: URL so actual
# newlines reach the browser (codex round 1 BP4).
NEWLINE_URL="$(make_data_url $'<pre>a\n\n\n\nb</pre>')"
case_get_text "get-text default collapses 4 \\n → 2" \
  "$NEWLINE_URL" "pre" "" "pre" "0"

case_get_text "get-text --raw preserves 4 \\n" \
  "$NEWLINE_URL" "pre" "" "pre" "1"

# Phase 1b — Accessibility queryAXTree cases. TS chromium-cdp doesn't expose
# the Accessibility domain, so these are spike-only checks (no TS parity).
# Verdict signal: spike command exits as expected (0 for hits, 1 for miss,
# 2 for clap rejections).

echo "== phase 1b: accessibility (spike-only, no TS parity) =="

ax_case() {
  local label="$1" url="$2" expected_exit="$3"
  shift 3
  local actual_out actual_rc
  # set -e would normally abort on a non-zero spike exit (e.g. --role miss).
  # Use `if` to consume the exit status so the smoke can continue testing
  # both success and failure cases.
  if actual_out="$("$SPIKE" get-text "$url" "$@" 2>&1)"; then
    actual_rc=0
  else
    actual_rc=$?
  fi
  if [[ "$actual_rc" == "$expected_exit" ]]; then
    report_pass "$label" "$(printf '%s' "$actual_out" | head -1 | tr -d '\n')"
  else
    report_fail "$label" "rc=$actual_rc out=$actual_out" "expected rc=$expected_exit"
  fi
}

ax_case "AX --role button hit" \
  "data:text/html,<button>Click</button>" 0 \
  --role button

ax_case "AX --role button + --name select" \
  "data:text/html,<button>X</button><button>Click</button>" 0 \
  --role button --name "Click"

ax_case "AX aria-label computed name → DOM text" \
  "data:text/html,<button aria-label=\"Save changes\">SAVE</button>" 0 \
  --role button --name "Save changes"

ax_case "AX <label for> computed name → matches input" \
  "data:text/html,<label for=\"e\">Email</label><input id=\"e\" type=\"text\" value=\"x\">" 0 \
  --role textbox --name "Email"

ax_case "AX --role miss exits 1" \
  "data:text/html,<p>Plain</p>" 1 \
  --role button

ax_case "AX --role + --name miss exits 1" \
  "data:text/html,<button>X</button>" 1 \
  --role button --name "NotHere"

ax_case "AX aria-hidden filtered (first visible wins)" \
  "data:text/html,<button aria-hidden=\"true\">Hidden</button><button>Visible</button>" 0 \
  --role button

ax_case "AX --raw preserves whitespace" \
  "data:text/html,<button>  Trim me  </button>" 0 \
  --role button --name "Trim me" --raw

ax_case "AX --selector + --role rejected by clap" \
  "data:text/html,x" 2 \
  --selector h1 --role button

ax_case "AX --testid + --role rejected by clap" \
  "data:text/html,x" 2 \
  --testid foo --role button

ax_case "AX --name without --role rejected by clap" \
  "data:text/html,x" 2 \
  --name "Click"

# Phase 1c — query-all (multi-element extraction). spike-only, no TS parity:
# TS chromium-cdp returns a different shape (object array, not text array).
echo "== phase 1c: query-all (spike-only, no TS parity) =="

qa_case() {
  local label="$1" url="$2" expected_exit="$3" expected_stdout="$4"
  shift 4
  local actual_out actual_rc
  if actual_out="$("$SPIKE" query-all "$url" "$@" 2>&1)"; then
    actual_rc=0
  else
    actual_rc=$?
  fi
  local first_line
  first_line="$(printf '%s' "$actual_out" | head -1)"
  if [[ "$actual_rc" == "$expected_exit" ]] \
      && { [[ -z "$expected_stdout" ]] || [[ "$first_line" == "$expected_stdout" ]]; }; then
    report_pass "$label" "$first_line"
  else
    report_fail "$label" \
      "rc=$actual_rc out=$first_line" \
      "expected rc=$expected_exit stdout=$expected_stdout"
  fi
}

qa_case "QA selector multi (3 lis)" \
  "data:text/html,<li>a</li><li>b</li><li>c</li>" 0 '["a","b","c"]' \
  --selector li

qa_case "QA selector empty → []" \
  "data:text/html,<p>x</p>" 0 '[]' \
  --selector h1

qa_case "QA selector --limit caps result" \
  "data:text/html,<li>a</li><li>b</li><li>c</li>" 0 '["a","b"]' \
  --selector li --limit 2

qa_case "QA testid filters by dataset" \
  "data:text/html,<span data-testid=item>a</span><span data-testid=item>b</span><span data-testid=other>x</span>" 0 '["a","b"]' \
  --testid item

qa_case "QA testid single match → single element array" \
  "data:text/html,<span data-testid=x>v</span><span data-testid=y>w</span>" 0 '["v"]' \
  --testid x

qa_case "QA role multi (3 buttons)" \
  "data:text/html,<button>A</button><button>B</button><button>C</button>" 0 '["A","B","C"]' \
  --role button

qa_case "QA role + name selects one" \
  "data:text/html,<button>X</button><button>Two</button><button>Three</button>" 0 '["Two"]' \
  --role button --name "Two"

qa_case "QA role aria-hidden filtered" \
  "data:text/html,<button aria-hidden=true>Hidden</button><button>Visible</button>" 0 '["Visible"]' \
  --role button

qa_case "QA role --limit caps result" \
  "data:text/html,<button>A</button><button>B</button><button>C</button><button>D</button>" 0 '["A","B"]' \
  --role button --limit 2

qa_case "QA TARGET missing rejected" \
  "data:text/html,x" 1 "" \
  --raw

qa_case "QA --selector + --role rejected by clap" \
  "data:text/html,x" 2 "" \
  --selector li --role button

qa_case "QA bad CSS selector exits 1" \
  "data:text/html,<p>x</p>" 1 "" \
  --selector "[[bad"

echo "== summary =="
echo "passed: $PASS_COUNT"
echo "failed: $FAIL_COUNT"
exit $FAIL_COUNT
