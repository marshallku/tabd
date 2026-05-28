#!/usr/bin/env bash
# spike-daemon-compat.sh — verify TS CLI (./bin/ai-browser.js) can talk to
# the Rust spike daemon over the same UDS protocol.
#
# Pre-reqs:
#   - cargo build --release --manifest-path crates/cdp-spike/Cargo.toml
#   - npm run build (dist/server/runtime.js)
#   - $BROWSER_EXECUTABLE resolvable (system chromium or Playwright cache)

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SPIKE="${ROOT_DIR}/crates/cdp-spike/target/release/cdp-spike"
AI_BROWSER="${ROOT_DIR}/bin/ai-browser.js"

if [[ ! -x "$SPIKE" ]]; then
  echo "Missing spike binary. Run: cargo build --release --manifest-path crates/cdp-spike/Cargo.toml" >&2
  exit 2
fi
if [[ ! -f "$ROOT_DIR/dist/server/runtime.js" ]]; then
  echo "Missing TS dist. Run: npm run build" >&2
  exit 2
fi
if [[ ! -x "$AI_BROWSER" ]]; then
  echo "Missing $AI_BROWSER" >&2
  exit 2
fi

# Resolve chromium for both sides — TS chromium-cdp only looks at /usr/bin,
# spike auto-discovers Playwright cache. Force-pin so they agree.
resolve_chromium() {
  if [[ -n "${BROWSER_EXECUTABLE:-}" && -x "$BROWSER_EXECUTABLE" ]]; then
    printf '%s' "$BROWSER_EXECUTABLE"; return
  fi
  for c in google-chrome google-chrome-stable chromium chromium-browser; do
    if command -v "$c" >/dev/null 2>&1; then command -v "$c"; return; fi
  done
  shopt -s nullglob
  local best="" best_ver=0
  for d in "$HOME"/.cache/ms-playwright/chromium-*/chrome-linux64/chrome; do
    [[ -x "$d" ]] || continue
    local ver
    ver="$(echo "$d" | sed -E 's|.*/chromium-([0-9]+)/.*|\1|')"
    if (( ver > best_ver )); then best_ver=$ver; best="$d"; fi
  done
  shopt -u nullglob
  [[ -n "$best" ]] && printf '%s' "$best"
}
CHROMIUM_BIN="$(resolve_chromium || true)"
[[ -n "$CHROMIUM_BIN" ]] || { echo "no chromium found"; exit 2; }
echo "chromium: $CHROMIUM_BIN"

TMP="$(mktemp -d -t spike-daemon-compat.XXXX)"
cleanup() {
  if [[ -n "${DAEMON_PID:-}" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

# Boot the spike daemon. Use the same BROWSER_EXECUTABLE for chromium pinning
# so cdp-spike's Browser::launch resolves to the test binary.
BROWSER_EXECUTABLE="$CHROMIUM_BIN" \
  "$SPIKE" daemon start --base-dir "$TMP" \
  >"$TMP/daemon.log" 2>&1 &
DAEMON_PID=$!
echo "spike daemon started, pid=$DAEMON_PID"

# Wait for socket file to appear (timeout 5s) — boot is async.
for _ in 1 2 3 4 5 6 7 8 9 10; do
  [[ -S "$TMP/daemon.sock" ]] && break
  sleep 0.5
done
if [[ ! -S "$TMP/daemon.sock" ]]; then
  echo "spike daemon socket did not appear within 5s" >&2
  cat "$TMP/daemon.log" >&2
  exit 1
fi

# Wait until daemon reports ready (chromium+cdp boot done).
for _ in $(seq 1 30); do
  resp="$("$SPIKE" daemon ping --base-dir "$TMP" 2>/dev/null || true)"
  if echo "$resp" | grep -q '"ready":true'; then
    break
  fi
  sleep 0.5
done

PASS_COUNT=0
FAIL_COUNT=0
report_pass() { printf "PASS  %s\n" "$1"; PASS_COUNT=$((PASS_COUNT+1)); }
report_fail() {
  printf "FAIL  %s\n" "$1"
  [[ -n "${2:-}" ]] && printf "  got: %s\n" "$2"
  [[ -n "${3:-}" ]] && printf "  want: %s\n" "$3"
  FAIL_COUNT=$((FAIL_COUNT+1))
}

# Common env for every TS CLI call.
ts_env=(
  "BROWSER_RUNTIME=chromium-cdp"
  "BROWSER_EXECUTABLE=$CHROMIUM_BIN"
  "AI_BROWSER_BASE_DIR=$TMP"
)

# 1. TS CLI daemon health → JSON, verify pid matches spike daemon.
case_health() {
  local raw pid_in_response
  if ! raw="$(env "${ts_env[@]}" timeout 15 node "$AI_BROWSER" daemon health 2>&1)"; then
    report_fail "TS daemon health" "$raw"
    return
  fi
  # Output is multi-line pretty JSON. Pipe the whole stdout to node which
  # extracts the {...} block (last balanced one) and reads .pid.
  pid_in_response="$(printf '%s' "$raw" | node -e '
    const s = require("fs").readFileSync(0, "utf8");
    // pick the last JSON object in the output
    const start = s.lastIndexOf("{");
    const end = s.lastIndexOf("}");
    if (start < 0 || end <= start) { process.exit(1); }
    const obj = JSON.parse(s.slice(start, end + 1));
    process.stdout.write(String(obj.pid || ""));
  ' 2>/dev/null || true)"
  if [[ "$pid_in_response" == "$DAEMON_PID" ]]; then
    report_pass "TS daemon health → pid matches spike daemon ($pid_in_response)"
  else
    report_fail "TS daemon health pid mismatch" "$pid_in_response" "$DAEMON_PID"
  fi
}

# 2. TS CLI navigate via spike daemon.
case_navigate() {
  local out rc
  if out="$(env "${ts_env[@]}" node "$AI_BROWSER" navigate "data:text/html,<h1>Hi</h1>" 2>&1)"; then
    rc=0
  else
    rc=$?
  fi
  if [[ $rc -eq 0 ]]; then
    report_pass "TS navigate via spike daemon (rc=0)"
  else
    report_fail "TS navigate failed" "$out"
  fi
}

# 3. TS CLI eval via spike daemon.
case_eval() {
  local out rc
  if out="$(env "${ts_env[@]}" node "$AI_BROWSER" eval "document.title" 2>&1)"; then
    rc=0
  else
    rc=$?
  fi
  if [[ $rc -eq 0 ]]; then
    report_pass "TS eval via spike daemon (got: $(printf '%s' "$out" | head -1))"
  else
    report_fail "TS eval failed" "$out"
  fi
}

# 4. TS CLI get-text via spike daemon → should return text content of <h1>.
case_get_text() {
  local out rc
  # navigate first so the page state is the same one we'll read from.
  env "${ts_env[@]}" node "$AI_BROWSER" navigate "data:text/html,<h1>Hello</h1>" >/dev/null 2>&1 || true
  if out="$(env "${ts_env[@]}" node "$AI_BROWSER" get-text --selector h1 2>&1)"; then
    rc=0
  else
    rc=$?
  fi
  local first_line
  first_line="$(printf '%s' "$out" | head -1)"
  if [[ $rc -eq 0 ]] && [[ "$first_line" == "Hello" ]]; then
    report_pass "TS get-text via spike daemon → 'Hello'"
  else
    report_fail "TS get-text failed/mismatch" "$first_line (rc=$rc)" "Hello"
  fi
}

echo "== spike daemon TS CLI compat =="

case_health
case_navigate
case_eval
case_get_text

# Phase 2b: 4 driver actions for automation workflow (click/type/wait-selector/wait-url).
# Each is a TS CLI command backed by an action newly supported in spike daemon.

case_click() {
  # navigate to a page with a button that sets window.clicked on click,
  # then click via TS CLI, then verify window.clicked via TS eval.
  local navout clickout evalout
  env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" navigate \
    "data:text/html,<button id=b onclick='window.clicked=1'>Go</button>" \
    >/dev/null 2>&1 || { report_fail "TS click navigate setup"; return; }
  if ! clickout="$(env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" click "#b" 2>&1)"; then
    report_fail "TS click via spike daemon" "$clickout"; return
  fi
  if ! evalout="$(env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" eval "window.clicked" 2>&1)"; then
    report_fail "TS click verify (eval after click)" "$evalout"; return
  fi
  # eval output is the value (1) or possibly "1" — check trimmed contains "1"
  local first; first="$(printf '%s' "$evalout" | head -1 | tr -d '"')"
  if [[ "$first" == "1" ]]; then
    report_pass "TS click → window.clicked=1"
  else
    report_fail "TS click did not set window.clicked" "$first" "1"
  fi
}

case_type() {
  local navout typeout evalout
  env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" navigate \
    "data:text/html,<input id=i type=text>" \
    >/dev/null 2>&1 || { report_fail "TS type navigate setup"; return; }
  if ! typeout="$(env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" type "#i" "hello world" 2>&1)"; then
    report_fail "TS type via spike daemon" "$typeout"; return
  fi
  if ! evalout="$(env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" eval "document.querySelector('#i').value" 2>&1)"; then
    report_fail "TS type verify (read value back)" "$evalout"; return
  fi
  local first; first="$(printf '%s' "$evalout" | head -1)"
  # eval likely prints "hello world" (with quotes JSON-stringified) — accept both
  if [[ "$first" == '"hello world"' ]] || [[ "$first" == "hello world" ]]; then
    report_pass "TS type → input value = 'hello world'"
  else
    report_fail "TS type did not set value" "$first" '"hello world"'
  fi
}

case_wait_selector() {
  # Navigate to a page that injects an element after 300ms. wait-selector
  # should succeed within default timeout.
  env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" navigate \
    "data:text/html,<script>setTimeout(()=>{document.body.innerHTML+='<div id=late>here</div>'},300)</script>" \
    >/dev/null 2>&1 || { report_fail "TS wait-selector navigate setup"; return; }
  if env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" wait-selector "#late" >/dev/null 2>&1; then
    report_pass "TS wait-selector found delayed #late"
  else
    report_fail "TS wait-selector timed out or failed"
  fi
}

case_wait_url() {
  # Navigate to a data: URL, then wait-url with a glob that matches it.
  # Tests both the glob compilation and the polling/match path.
  env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" navigate \
    "data:text/html,<h1>here</h1>" \
    >/dev/null 2>&1 || { report_fail "TS wait-url navigate setup"; return; }
  # Glob "data:*" should match any data: URL — immediate match on poll.
  if env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" wait-url "data:*" --pattern-type glob >/dev/null 2>&1; then
    report_pass "TS wait-url glob 'data:*' matched current URL"
  else
    report_fail "TS wait-url did not match current data: URL"
  fi
}

case_click
case_type
case_wait_selector
case_wait_url

# 5. Stop via TS CLI.
echo "stopping spike daemon via TS CLI..."
env "${ts_env[@]}" node "$AI_BROWSER" daemon stop >/dev/null 2>&1 || true

# Wait for socket + pid file removal (up to 5s).
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if [[ ! -S "$TMP/daemon.sock" ]] && [[ ! -e "$TMP/daemon.pid" ]]; then
    break
  fi
  sleep 0.5
done
if [[ ! -S "$TMP/daemon.sock" ]] && [[ ! -e "$TMP/daemon.pid" ]]; then
  report_pass "daemon stop cleanup (socket + pid removed)"
else
  report_fail "daemon stop cleanup incomplete" \
    "socket exists=$([[ -S $TMP/daemon.sock ]] && echo yes || echo no), pid exists=$([[ -e $TMP/daemon.pid ]] && echo yes || echo no)"
fi

# Wait for daemon process to exit (kill -0 PID returns 1 = gone).
for _ in 1 2 3 4 5; do
  if ! kill -0 "$DAEMON_PID" 2>/dev/null; then break; fi
  sleep 0.5
done
if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
  report_pass "daemon process exited"
  DAEMON_PID=""  # prevent trap kill
else
  report_fail "daemon process still running" "pid=$DAEMON_PID"
fi

echo "== summary =="
echo "passed: $PASS_COUNT"
echo "failed: $FAIL_COUNT"
exit $FAIL_COUNT
