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

echo "== summary =="
echo "passed: $PASS_COUNT"
echo "failed: $FAIL_COUNT"
exit $FAIL_COUNT
