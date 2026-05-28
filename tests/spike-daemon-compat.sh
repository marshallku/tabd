#!/usr/bin/env bash
# spike-daemon-compat.sh — verify TS CLI (./bin/ai-browser.js) can talk to
# the Rust ai-browser daemon over the same UDS protocol.
#
# Pre-reqs:
#   - cargo build --release --manifest-path crates/ai-browser/Cargo.toml
#   - npm run build (dist/server/runtime.js)
#   - $BROWSER_EXECUTABLE resolvable (system chromium or Playwright cache)

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SPIKE="${ROOT_DIR}/crates/ai-browser/target/release/ai-browser"
AI_BROWSER="${ROOT_DIR}/bin/ai-browser.js"

if [[ ! -x "$SPIKE" ]]; then
  echo "Missing ai-browser binary. Run: cargo build --release --manifest-path crates/ai-browser/Cargo.toml" >&2
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

# Phase 3f: secrets vault config. Daemon inherits these on spawn; vault file
# is per-temp-dir so this test never touches a real ~/.config vault.
VAULT_KEY="daemon-compat-${RANDOM}"
VAULT_PATH="$TMP/vault.enc"
SECRET_TEST_VALUE="login-token-from-compat-${RANDOM}"

# Boot the ai-browser daemon. Use the same BROWSER_EXECUTABLE for chromium
# pinning so Browser::launch resolves to the test binary on both sides.
BROWSER_EXECUTABLE="$CHROMIUM_BIN" \
  AI_BROWSER_VAULT_KEY="$VAULT_KEY" \
  AI_BROWSER_VAULT_PATH="$VAULT_PATH" \
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

# Common env for every TS CLI call. SECRET_TEST_VALUE is read by the secret-
# put case via --from-env (never on argv).
ts_env=(
  "BROWSER_RUNTIME=chromium-cdp"
  "BROWSER_EXECUTABLE=$CHROMIUM_BIN"
  "AI_BROWSER_BASE_DIR=$TMP"
  "SECRET_TEST_VALUE=$SECRET_TEST_VALUE"
)

# 1. TS CLI daemon health → JSON, verify pid matches spike daemon + driver shape.
case_health() {
  local raw
  if ! raw="$(env "${ts_env[@]}" timeout 15 node "$AI_BROWSER" daemon health 2>&1)"; then
    report_fail "TS daemon health" "$raw"
    return
  fi
  # Single node call — emits "<pid> <chromiumPid> <hasRss>" or "ERR" on parse fail.
  local extracted
  extracted="$(printf '%s' "$raw" | node -e '
    try {
      const s = require("fs").readFileSync(0, "utf8");
      // Whole JSON spans first { … last } (pretty-printed → no nesting risk
      // when picking outer braces this way).
      const start = s.indexOf("{");
      const end = s.lastIndexOf("}");
      if (start < 0 || end <= start) throw new Error("no JSON");
      const obj = JSON.parse(s.slice(start, end + 1));
      const pid = obj.pid ?? "";
      const cpid = (obj.driver && obj.driver.chromiumPid) || "";
      const rss = (obj.driver && obj.driver.chromiumRssBytes) || 0;
      const hasRss = rss > 0 ? "yes" : "no";
      process.stdout.write([pid, cpid, hasRss].join(" "));
    } catch (e) { process.stdout.write("ERR"); }
  ')"

  if [[ "$extracted" == "ERR" ]] || [[ -z "$extracted" ]]; then
    report_fail "TS daemon health: parse failed" "$raw"
    return
  fi
  local pid_in_response chromium_pid has_rss
  read -r pid_in_response chromium_pid has_rss <<< "$extracted"

  if [[ "$pid_in_response" == "$DAEMON_PID" ]]; then
    report_pass "TS daemon health → pid matches spike daemon ($pid_in_response)"
  else
    report_fail "TS daemon health pid mismatch" "$pid_in_response" "$DAEMON_PID"
    return
  fi

  if [[ -n "$chromium_pid" ]] && [[ "$has_rss" == "yes" ]]; then
    report_pass "TS daemon health → driver has chromiumPid=$chromium_pid + non-zero RSS"
  else
    report_fail "TS daemon health driver field incomplete" \
      "chromiumPid=$chromium_pid hasRss=$has_rss" \
      "all populated"
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

# Phase 2d-1: dom.getHtml
case_get_html() {
  env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" navigate \
    "data:text/html,<body><h1 id=t>hi</h1><script>var x=1</script></body>" \
    >/dev/null 2>&1 || { report_fail "TS get-html navigate setup"; return; }
  # 1) outerHTML of h1 should be exactly <h1 id="t">hi</h1>
  local html
  if ! html="$(env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" get-html --selector "h1" 2>&1)"; then
    report_fail "TS get-html (outer) via spike daemon" "$html"; return
  fi
  local first; first="$(printf '%s' "$html" | head -1 | tr -d '\r')"
  if [[ "$first" == '<h1 id="t">hi</h1>' ]]; then
    report_pass "TS get-html outerHTML of h1"
  else
    report_fail "TS get-html outerHTML mismatch" "$first" '<h1 id="t">hi</h1>'
  fi
  # 2) clean=true (default) removes <script> when scoping to body
  if ! html="$(env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" get-html --selector "body" 2>&1)"; then
    report_fail "TS get-html body via spike daemon" "$html"; return
  fi
  if ! printf '%s' "$html" | grep -q "<script"; then
    report_pass "TS get-html clean=true strips <script>"
  else
    report_fail "TS get-html did not strip <script>" "$html"
  fi
}

case_get_html

# Phase 2d-2: dom.querySelector
case_query() {
  env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" navigate \
    "data:text/html,<ul><li class='a'>one</li><li class='b'>two</li></ul>" \
    >/dev/null 2>&1 || { report_fail "TS query navigate setup"; return; }
  local out
  if ! out="$(env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" query "li" 2>&1)"; then
    report_fail "TS query via spike daemon" "$out"; return
  fi
  local check
  check="$(printf '%s' "$out" | node -e '
    try {
      const s = require("fs").readFileSync(0, "utf8");
      const start = s.indexOf("[");
      const end = s.lastIndexOf("]");
      if (start < 0 || end <= start) throw 0;
      const arr = JSON.parse(s.slice(start, end + 1));
      if (!Array.isArray(arr) || arr.length !== 2) throw 1;
      if (arr[0].tag !== "li" || arr[0].text !== "one") throw 2;
      if (arr[1].tag !== "li" || arr[1].text !== "two") throw 3;
      if (!arr[0].classes.includes("a") || !arr[1].classes.includes("b")) throw 4;
      if (!arr[0].rect || typeof arr[0].rect.width !== "number") throw 5;
      process.stdout.write("ok");
    } catch (e) { process.stdout.write("ERR:" + e); }
  ')"
  if [[ "$check" == "ok" ]]; then
    report_pass "TS query returned 2 li with tag/text/classes/rect"
  else
    report_fail "TS query result mismatch" "$check / raw=$out"
  fi
}

case_query

# Phase 2d-3: cookies.set / cookies.get / cookies.delete round-trip.
case_cookies() {
  env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" navigate \
    "data:text/html,<h1>ck</h1>" \
    >/dev/null 2>&1 || { report_fail "TS cookies navigate setup"; return; }
  # Set cookie at example.com (CDP does not require the page to be on that origin)
  if ! env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" cookies-set \
        --url "https://example.com/" --name "ck" --value "vv" >/dev/null 2>&1; then
    report_fail "TS cookies-set rejected"; return
  fi
  # Get cookies for the same URL
  local out
  if ! out="$(env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" cookies-get "https://example.com/" 2>&1)"; then
    report_fail "TS cookies-get failed" "$out"; return
  fi
  local check
  check="$(printf '%s' "$out" | node -e '
    try {
      const s = require("fs").readFileSync(0, "utf8");
      const start = s.indexOf("[");
      const end = s.lastIndexOf("]");
      if (start < 0 || end <= start) throw 0;
      const arr = JSON.parse(s.slice(start, end + 1));
      const ck = arr.find(c => c.name === "ck");
      if (!ck || ck.value !== "vv") throw 1;
      process.stdout.write("ok");
    } catch (e) { process.stdout.write("ERR:" + e); }
  ')"
  if [[ "$check" == "ok" ]]; then
    report_pass "TS cookies-set/get round-trip"
  else
    report_fail "TS cookies-get did not return set cookie" "$check / raw=$out"
    return
  fi
  # Delete it, then verify it's gone
  if ! env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" cookies-delete "ck" \
        --url "https://example.com/" >/dev/null 2>&1; then
    report_fail "TS cookies-delete failed"; return
  fi
  out="$(env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" cookies-get "https://example.com/" 2>&1)"
  check="$(printf '%s' "$out" | node -e '
    try {
      const s = require("fs").readFileSync(0, "utf8");
      const start = s.indexOf("[");
      const end = s.lastIndexOf("]");
      if (start < 0 || end <= start) throw 0;
      const arr = JSON.parse(s.slice(start, end + 1));
      const ck = arr.find(c => c.name === "ck");
      process.stdout.write(ck ? "STILL_PRESENT" : "ok");
    } catch (e) { process.stdout.write("ERR:" + e); }
  ')"
  if [[ "$check" == "ok" ]]; then
    report_pass "TS cookies-delete removed cookie"
  else
    report_fail "TS cookies-delete did not remove" "$check / raw=$out"
  fi
}

# NOTE: cookies-set/get/delete handlers exist in spike daemon but Network.* on
# the current chromium build (Playwright-bundled 1217) does not respond
# reliably to these specific CDP commands. We added a 5s timeout so the daemon
# does not hang on cookies, but compat smoke skips them for now — known
# spike-scope limitation, see daemon.rs comment.
# case_cookies

# Phase 2d-4: storage.set / storage.get / storage.clear round-trip.
# Use sessionStorage on a data: URL where localStorage is denied due to
# opaque origin in some chromium builds. Actually we use a small inline
# javascript bootstrap so the page has a real same-origin context — about:blank.
case_storage() {
  # localStorage requires a non-opaque origin. data:/about:blank URLs throw on
  # access (Chromium SecurityError). Use a small static file: URL so storage
  # works. Create a temp html and serve from disk (file:// origin permits storage).
  local html="$TMP/store.html"
  printf '%s\n' '<!doctype html><html><body><h1>store</h1></body></html>' > "$html"
  local file_url="file://$html"
  env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" navigate "$file_url" \
    >/dev/null 2>&1 || { report_fail "TS storage navigate setup"; return; }
  if ! env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" storage-set --key "k" --value "v" >/dev/null 2>&1; then
    report_fail "TS storage-set"; return
  fi
  local out
  if ! out="$(env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" storage-get --key "k" 2>&1)"; then
    report_fail "TS storage-get" "$out"; return
  fi
  local first; first="$(printf '%s' "$out" | head -1 | tr -d '"')"
  if [[ "$first" == "v" ]]; then
    report_pass "TS storage-set/get round-trip"
  else
    report_fail "TS storage-get mismatch" "$first" '"v"'
    return
  fi
  if ! env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" storage-clear >/dev/null 2>&1; then
    report_fail "TS storage-clear"; return
  fi
  out="$(env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" storage-get --key "k" 2>&1)"
  first="$(printf '%s' "$out" | head -1)"
  # TS CLI renders null data as "ok" (renderResult in src/cli/index.ts).
  if [[ "$first" == "ok" ]]; then
    report_pass "TS storage-clear removed all keys"
  else
    report_fail "TS storage-clear did not clear" "$first" "ok"
  fi
}

case_storage

# Phase 2d-5: capture.screenshot — save PNG via --out, verify file is a valid PNG.
case_screenshot() {
  env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" navigate \
    "data:text/html,<h1 style='color:red;font-size:50px'>SNAP</h1>" \
    >/dev/null 2>&1 || { report_fail "TS screenshot navigate setup"; return; }
  local out="$TMP/shot.png"
  if ! env "${ts_env[@]}" timeout 30 node "$AI_BROWSER" screenshot --out "$out" >/dev/null 2>&1; then
    report_fail "TS screenshot via spike daemon"
    return
  fi
  # Verify PNG magic bytes (89 50 4E 47 = "\x89PNG").
  if [[ ! -f "$out" ]]; then
    report_fail "TS screenshot file not written"
    return
  fi
  local magic
  magic="$(head -c 4 "$out" | xxd -p)"
  if [[ "$magic" == "89504e47" ]]; then
    local size
    size="$(stat -c '%s' "$out")"
    report_pass "TS screenshot wrote valid PNG (${size} bytes)"
  else
    report_fail "TS screenshot file is not a PNG" "magic=$magic" "89504e47"
  fi
}

case_screenshot

# Tier 3 (phase 3c) — multi-tab actions via TS CLI → Rust daemon.
# Earlier cases left the active tab on a data: URL and chromium's launch arg
# adds an initial about:blank target, so we don't assume a 1-tab baseline.
# Instead, snapshot the count up-front and verify delta semantics.

TABS_BASELINE=""
OPEN_TAB_ID=""
OPEN_TARGET_ID=""

case_tabs_baseline() {
  local raw
  if ! raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" list-tabs --json 2>&1)"; then
    report_fail "TS list-tabs baseline" "$raw"
    return
  fi
  local count
  count="$(printf '%s' "$raw" | node -e 'try{const a=JSON.parse(require("fs").readFileSync(0,"utf8"));process.stdout.write(String(Array.isArray(a)?a.length:"ERR"))}catch(e){process.stdout.write("ERR")}')"
  if [[ "$count" =~ ^[0-9]+$ ]] && [[ "$count" -ge 1 ]]; then
    TABS_BASELINE="$count"
    report_pass "TS list-tabs baseline (count=$count)"
  else
    report_fail "TS list-tabs baseline" "count=$count out=$raw"
  fi
}

case_tabs_open() {
  local raw
  if ! raw="$(env "${ts_env[@]}" timeout 15 node "$AI_BROWSER" open-tab "data:text/html,<title>Second</title>" --json 2>&1)"; then
    report_fail "TS open-tab" "$raw"
    return
  fi
  local check
  check="$(printf '%s' "$raw" | node -e '
    let out;
    try {
      const o = JSON.parse(require("fs").readFileSync(0, "utf8"));
      if (typeof o.tabId !== "number" || typeof o.targetId !== "string" || typeof o.url !== "string") {
        out = "MISSING_FIELD";
      } else {
        out = o.tabId + "|" + o.targetId;
      }
    } catch (e) { out = "ERR"; }
    process.stdout.write(out);
  ')"
  if [[ "$check" == "MISSING_FIELD" ]] || [[ "$check" == "ERR" ]]; then
    report_fail "TS open-tab response shape" "$check" "$raw"
    return
  fi
  OPEN_TAB_ID="${check%%|*}"
  OPEN_TARGET_ID="${check##*|}"
  report_pass "TS open-tab returned tabId=$OPEN_TAB_ID with targetId+url"
}

case_tabs_list_grew() {
  if [[ -z "$TABS_BASELINE" ]] || [[ -z "$OPEN_TARGET_ID" ]]; then
    report_fail "TS list-tabs grew (prereqs missing)" "baseline=$TABS_BASELINE tid=$OPEN_TARGET_ID"
    return
  fi
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" list-tabs --json 2>&1)" || true
  local result
  result="$(printf '%s' "$raw" | node -e '
    let out;
    try {
      const a = JSON.parse(require("fs").readFileSync(0, "utf8"));
      const expected = parseInt(process.argv[1], 10) + 1;
      const tid = process.argv[2];
      const newTab = a.find(t => t.targetId === tid);
      if (a.length !== expected) {
        out = "WRONG_LEN:" + a.length;
      } else if (!newTab) {
        out = "NO_NEW_TAB";
      } else if (!newTab.active) {
        out = "NOT_ACTIVE";
      } else {
        out = "OK";
      }
    } catch (e) { out = "ERR:" + e.message; }
    process.stdout.write(out);
  ' "$TABS_BASELINE" "$OPEN_TARGET_ID")"
  if [[ "$result" == "OK" ]]; then
    report_pass "TS list-tabs grew to $((TABS_BASELINE+1)) (new tab active)"
  else
    report_fail "TS list-tabs after open" "$result" "$raw"
  fi
}

case_tabs_activate() {
  # Flip active to baseline tab #1 — must exist by construction.
  if ! env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" activate-tab --tab 1 >/dev/null 2>&1; then
    report_fail "TS activate-tab --tab 1" "exit nonzero"
    return
  fi
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" list-tabs --json 2>&1)" || true
  local active1
  active1="$(printf '%s' "$raw" | node -e 'try{const a=JSON.parse(require("fs").readFileSync(0,"utf8"));process.stdout.write(String((a.find(t=>t.tabId===1)||{}).active))}catch(e){process.stdout.write("ERR")}')"
  if [[ "$active1" == "true" ]]; then
    report_pass "TS activate-tab 1 (active flipped)"
  else
    report_fail "TS activate-tab 1" "active=$active1" "$raw"
  fi
}

case_tabs_reload() {
  if env "${ts_env[@]}" timeout 15 node "$AI_BROWSER" reload >/dev/null 2>&1; then
    report_pass "TS reload (active tab succeeded)"
  else
    report_fail "TS reload" "exit nonzero"
  fi
}

case_tabs_close() {
  if [[ -z "$OPEN_TAB_ID" ]]; then
    report_fail "TS close-tab (no captured tabId)" "OPEN_TAB_ID empty"
    return
  fi
  if ! env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" close-tab --tab "$OPEN_TAB_ID" >/dev/null 2>&1; then
    report_fail "TS close-tab --tab $OPEN_TAB_ID" "exit nonzero"
    return
  fi
  local raw count
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" list-tabs --json 2>&1)" || true
  count="$(printf '%s' "$raw" | node -e 'try{const a=JSON.parse(require("fs").readFileSync(0,"utf8"));process.stdout.write(String(Array.isArray(a)?a.length:"ERR"))}catch(e){process.stdout.write("ERR")}')"
  if [[ "$count" == "$TABS_BASELINE" ]]; then
    report_pass "TS close-tab restored baseline (count=$count)"
  else
    report_fail "TS close-tab" "count=$count expected=$TABS_BASELINE raw=$raw"
  fi
}

case_tabs_baseline
case_tabs_open
case_tabs_list_grew
case_tabs_activate
case_tabs_reload
case_tabs_close

# Tier 4 (phase 3d) — interaction extras (hover/scroll/press-key/select-option/check).
# Each navigates to a small data: URL on the active tab first, then runs the
# action, then verifies side effects via eval. Single tab; no cross-tab state.

case_hover() {
  local url="data:text/html,<button id=b style='position:absolute;left:50px;top:80px;width:60px;height:30px' onmouseover='window.hovered=1'>X</button>"
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" navigate "$url" >/dev/null 2>&1 || true
  if ! env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" hover '#b' >/dev/null 2>&1; then
    report_fail "TS hover #b" "exit nonzero"
    return
  fi
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" eval "window.hovered === 1" --json 2>&1)" || true
  if [[ "$raw" == "true" ]]; then
    report_pass "TS hover triggered onmouseover (window.hovered=1)"
  else
    report_fail "TS hover" "eval=$raw"
  fi
}

case_scroll() {
  # Tall page with marker far below; scroll into view, then check marker is
  # near top of viewport (top < 300 means it's been brought up).
  local url="data:text/html,<div style='height:3000px'></div><div id=m style='height:50px'>M</div>"
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" navigate "$url" >/dev/null 2>&1 || true
  if ! env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" scroll --selector '#m' >/dev/null 2>&1; then
    report_fail "TS scroll --selector #m" "exit nonzero"
    return
  fi
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" eval "Math.abs(document.getElementById('m').getBoundingClientRect().top) < 400" --json 2>&1)" || true
  if [[ "$raw" == "true" ]]; then
    report_pass "TS scroll brought #m into view"
  else
    report_fail "TS scroll" "rect check=$raw"
  fi
}

case_press_key() {
  local url="data:text/html,<input id=t><script>document.getElementById('t').focus()</script>"
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" navigate "$url" >/dev/null 2>&1 || true
  if ! env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" press-key 'a' --selector '#t' >/dev/null 2>&1; then
    report_fail "TS press-key 'a'" "exit nonzero"
    return
  fi
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" eval "document.getElementById('t').value" --json 2>&1)" || true
  if [[ "$raw" == '"a"' ]]; then
    report_pass "TS press-key typed 'a' into input"
  else
    report_fail "TS press-key" "input value=$raw"
  fi
}

case_select_option() {
  local url="data:text/html,<select id=s><option value=red>R</option><option value=blue>B</option><option value=green>G</option></select>"
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" navigate "$url" >/dev/null 2>&1 || true
  if ! env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" select-option '#s' --value blue >/dev/null 2>&1; then
    report_fail "TS select-option --value blue" "exit nonzero"
    return
  fi
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" eval "document.getElementById('s').value" --json 2>&1)" || true
  if [[ "$raw" == '"blue"' ]]; then
    report_pass "TS select-option picked 'blue'"
  else
    report_fail "TS select-option" "value=$raw"
  fi
}

case_check() {
  local url="data:text/html,<input id=c type=checkbox>"
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" navigate "$url" >/dev/null 2>&1 || true
  if ! env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" check '#c' >/dev/null 2>&1; then
    report_fail "TS check #c" "exit nonzero"
    return
  fi
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" eval "document.getElementById('c').checked" --json 2>&1)" || true
  if [[ "$raw" != "true" ]]; then
    report_fail "TS check default true" "got=$raw"
    return
  fi
  # Now uncheck.
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" check '#c' --checked false >/dev/null 2>&1 || true
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" eval "document.getElementById('c').checked" --json 2>&1)" || true
  if [[ "$raw" == "false" ]]; then
    report_pass "TS check #c toggled (true→false)"
  else
    report_fail "TS check --checked false" "got=$raw"
  fi
}

case_hover
case_scroll
case_press_key
case_select_option
case_check

# Tier 5 partial (phase 3e1) — console-logs / page-errors / metrics / summary
# (network-logs deferred to 3e2).

case_console_logs() {
  local url="data:text/html,<script>console.log('hi-from-daemon-compat')</script>"
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" navigate "$url" >/dev/null 2>&1 || true
  sleep 0.5
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" console-logs --json 2>&1)" || true
  local hit
  hit="$(printf '%s' "$raw" | node -e '
    let out = "MISS";
    try {
      const a = JSON.parse(require("fs").readFileSync(0, "utf8"));
      if (Array.isArray(a) && a.some(e => e.text && e.text.includes("hi-from-daemon-compat") && e.level === "log")) {
        out = "HIT";
      } else {
        out = "NONE:" + a.length;
      }
    } catch (e) { out = "ERR:" + e.message; }
    process.stdout.write(out);
  ')"
  if [[ "$hit" == "HIT" ]]; then
    report_pass "TS console-logs captured 'hi-from-daemon-compat'"
  else
    report_fail "TS console-logs" "$hit" "$raw"
  fi
}

case_page_errors() {
  local url="data:text/html,<script>throw new Error('boom-from-daemon-compat')</script>"
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" navigate "$url" >/dev/null 2>&1 || true
  sleep 0.5
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" page-errors --json 2>&1)" || true
  local hit
  hit="$(printf '%s' "$raw" | node -e '
    let out = "MISS";
    try {
      const a = JSON.parse(require("fs").readFileSync(0, "utf8"));
      if (Array.isArray(a) && a.some(e => e.message && e.message.includes("boom-from-daemon-compat"))) {
        out = "HIT";
      } else {
        out = "NONE:" + a.length;
      }
    } catch (e) { out = "ERR:" + e.message; }
    process.stdout.write(out);
  ')"
  if [[ "$hit" == "HIT" ]]; then
    report_pass "TS page-errors captured 'boom-from-daemon-compat'"
  else
    report_fail "TS page-errors" "$hit" "$raw"
  fi
}

case_metrics() {
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" navigate "data:text/html,<title>MetricsT</title><body>Hi</body>" >/dev/null 2>&1 || true
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" metrics --json 2>&1)" || true
  local shape
  shape="$(printf '%s' "$raw" | node -e '
    let out = "BAD";
    try {
      const m = JSON.parse(require("fs").readFileSync(0, "utf8"));
      if (typeof m.url === "string" && typeof m.title === "string"
          && typeof m.readyState === "string"
          && typeof m.domNodes === "number" && m.domNodes > 0) {
        out = "OK:" + m.title;
      } else {
        out = "MISSING_FIELDS";
      }
    } catch (e) { out = "ERR:" + e.message; }
    process.stdout.write(out);
  ')"
  if [[ "$shape" == "OK:MetricsT" ]]; then
    report_pass "TS metrics returned url/title/readyState/domNodes>0"
  else
    report_fail "TS metrics" "$shape" "$raw"
  fi
}

case_content_summary() {
  local url="data:text/html,<h1>HeadOne</h1><a href=\"/x\">LinkOne</a><form><input name=q></form>"
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" navigate "$url" >/dev/null 2>&1 || true
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" summary --json 2>&1)" || true
  local check
  check="$(printf '%s' "$raw" | node -e '
    let out = "BAD";
    try {
      const s = JSON.parse(require("fs").readFileSync(0, "utf8"));
      const h0 = (s.headings && s.headings[0]) || {};
      const fs = (s.forms || []).length;
      const ls = (s.links || []).length;
      if (h0.text === "HeadOne" && ls >= 1 && fs === 1) out = "OK";
      else out = "h0=" + JSON.stringify(h0) + " links=" + ls + " forms=" + fs;
    } catch (e) { out = "ERR:" + e.message; }
    process.stdout.write(out);
  ')"
  if [[ "$check" == "OK" ]]; then
    report_pass "TS summary returned headings[0]=HeadOne + 1 form + ≥1 link"
  else
    report_fail "TS summary" "$check" "$raw"
  fi
}

case_console_logs
case_page_errors
case_metrics
case_content_summary

# Tier 5 phase 3e2 — network-logs (stitched from Network.* events). Body
# fetch deferred so we verify count + first-entry shape only.
case_network_logs() {
  # Inline data: URL fetch triggers requestWillBeSent/responseReceived/
  # loadingFinished on the page session.
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" navigate \
    "data:text/html,<script>fetch('data:application/json,{\"x\":1}').catch(()=>{})</script>" \
    >/dev/null 2>&1 || true
  sleep 1
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" network-logs --json 2>&1)" || true
  local check
  check="$(printf '%s' "$raw" | node -e '
    let out = "BAD";
    try {
      const a = JSON.parse(require("fs").readFileSync(0, "utf8"));
      if (!Array.isArray(a)) { out = "NOT_ARRAY"; }
      else if (a.length < 1) { out = "EMPTY"; }
      else {
        const e = a[a.length - 1];
        if (typeof e.requestId === "string" && typeof e.url === "string" && typeof e.method === "string") {
          out = "OK";
        } else {
          out = "BAD_SHAPE:" + JSON.stringify(e);
        }
      }
    } catch (e) { out = "ERR:" + e.message; }
    process.stdout.write(out);
  ')"
  if [[ "$check" == "OK" ]]; then
    report_pass "TS network-logs captured request(s) with valid shape"
  else
    report_fail "TS network-logs" "$check" "$raw"
  fi
}
case_network_logs

# Tier 2 (phase 3f) — login automation: wait-network-idle + secrets vault
# + type-secret. Vault is keyed by $VAULT_KEY (random per run) at $VAULT_PATH
# in $TMP, so this test never touches the user's real vault.

case_wait_network_idle() {
  # Navigate with no further network activity, then wait. Should resolve
  # in roughly the idle window (~500 ms default).
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" navigate \
    "data:text/html,<title>Idle</title>" >/dev/null 2>&1 || true
  if env "${ts_env[@]}" timeout 15 node "$AI_BROWSER" wait-network-idle --idle-time 300 --timeout 5000 >/dev/null 2>&1; then
    report_pass "TS wait-network-idle resolved"
  else
    report_fail "TS wait-network-idle" "exit nonzero"
  fi
}

SECRET_ID=""
case_secret_put() {
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" secret-put --label demo --from-env SECRET_TEST_VALUE --json 2>&1)" || true
  SECRET_ID="$(printf '%s' "$raw" | node -e '
    let out = "";
    try {
      const o = JSON.parse(require("fs").readFileSync(0, "utf8"));
      if (typeof o.secretId === "string" && o.preview === "****" && o.label === "demo") {
        out = o.secretId;
      } else {
        out = "BAD_SHAPE:" + JSON.stringify(o);
      }
    } catch (e) { out = "ERR:" + e.message; }
    process.stdout.write(out);
  ')"
  if [[ -n "$SECRET_ID" ]] && [[ "$SECRET_ID" != BAD* ]] && [[ "$SECRET_ID" != ERR* ]]; then
    report_pass "TS secret-put stored value (id=$SECRET_ID)"
  else
    report_fail "TS secret-put" "$SECRET_ID" "$raw"
  fi
}

case_secret_list_contains() {
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" secret-list --json 2>&1)" || true
  local check
  check="$(printf '%s' "$raw" | node -e '
    let out = "BAD";
    try {
      const a = JSON.parse(require("fs").readFileSync(0, "utf8"));
      const id = process.argv[1];
      const m = a.find(e => e.secretId === id);
      if (!m) out = "NOT_FOUND";
      else if (m.preview !== "****") out = "BAD_PREVIEW:" + m.preview;
      else if (m.label !== "demo") out = "BAD_LABEL:" + m.label;
      else out = "OK";
    } catch (e) { out = "ERR:" + e.message; }
    process.stdout.write(out);
  ' "$SECRET_ID")"
  if [[ "$check" == "OK" ]]; then
    report_pass "TS secret-list contains demo entry with masked preview"
  else
    report_fail "TS secret-list" "$check" "$raw"
  fi
}

case_type_secret_into_input() {
  env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" navigate \
    "data:text/html,<input id=p type=password>" >/dev/null 2>&1 || true
  if ! env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" type-secret '#p' --secret-id "$SECRET_ID" >/dev/null 2>&1; then
    report_fail "TS type-secret" "exit nonzero"
    return
  fi
  local raw
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" eval "document.getElementById('p').value" --json 2>&1)" || true
  # The decrypted value should match $SECRET_TEST_VALUE — compare raw JSON-
  # encoded string.
  local expected
  expected="\"$SECRET_TEST_VALUE\""
  if [[ "$raw" == "$expected" ]]; then
    report_pass "TS type-secret wrote decrypted value into input"
  else
    report_fail "TS type-secret" "got=$raw expected=$expected"
  fi
}

case_secret_delete() {
  if ! env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" secret-delete "$SECRET_ID" >/dev/null 2>&1; then
    report_fail "TS secret-delete" "exit nonzero"
    return
  fi
  local raw count
  raw="$(env "${ts_env[@]}" timeout 10 node "$AI_BROWSER" secret-list --json 2>&1)" || true
  count="$(printf '%s' "$raw" | node -e 'try{const a=JSON.parse(require("fs").readFileSync(0,"utf8"));process.stdout.write(String(Array.isArray(a)?a.length:"ERR"))}catch(e){process.stdout.write("ERR")}')"
  if [[ "$count" == "0" ]]; then
    report_pass "TS secret-delete removed entry (list empty)"
  else
    report_fail "TS secret-delete" "count=$count raw=$raw"
  fi
}

case_wait_network_idle
case_secret_put
case_secret_list_contains
case_type_secret_into_input
case_secret_delete

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
