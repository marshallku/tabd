#!/usr/bin/env bash
# cli-direct-smoke.sh — verify the Rust `tabd` binary's CLI dispatcher
# end-to-end: argv parsing, daemon auto-spawn, --json/--out rendering. Calls
# the binary directly (no TS bridge), matching how a graduated phase-B install
# would be used.
#
# Pre-reqs:
#   - cargo build --release --manifest-path crates/tabd/Cargo.toml
#   - $BROWSER_EXECUTABLE resolvable (system chromium or Playwright cache)

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ROOT_DIR}/crates/tabd/target/release/tabd"

if [[ ! -x "$BIN" ]]; then
  echo "Missing tabd binary. Run: cargo build --release --manifest-path crates/tabd/Cargo.toml" >&2
  exit 2
fi

# Same chromium-resolution dance as spike-daemon-compat.
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
export BROWSER_EXECUTABLE="$CHROMIUM_BIN"

TMP="$(mktemp -d -t tabd-cli-direct.XXXX)"
export TABD_BASE_DIR="$TMP"

cleanup() {
  # Best-effort daemon stop — auto-spawned daemons need an explicit shutdown.
  "$BIN" daemon stop --base-dir "$TMP" >/dev/null 2>&1 || true
  sleep 0.5
  rm -rf "$TMP"
}
trap cleanup EXIT

PASS_COUNT=0
FAIL_COUNT=0
pass() { printf "PASS  %s\n" "$1"; PASS_COUNT=$((PASS_COUNT + 1)); }
fail() { printf "FAIL  %s\n" "$1"; [[ -n "${2:-}" ]] && printf "  detail: %s\n" "$2"; FAIL_COUNT=$((FAIL_COUNT + 1)); }

echo "== cli-direct-smoke (auto-spawn + render) =="

# Prepare a local page (file:// origin keeps storage/cookies usable, but we
# only care about navigate + get-text here).
HTML="$TMP/page.html"
cat > "$HTML" <<'EOF'
<!doctype html>
<html><head><title>Direct</title></head>
<body><h1>Direct Smoke</h1></body></html>
EOF

# Case 1: navigate with no daemon running → dispatcher auto-spawns one.
if "$BIN" navigate "file://$HTML" >"$TMP/nav.out" 2>"$TMP/nav.err"; then
  pass "auto-spawn on first navigate (rc=0)"
else
  rc=$?
  fail "auto-spawn on first navigate" "rc=$rc; stderr=$(cat "$TMP/nav.err")"
fi

# Case 2: second call uses the same daemon — health pid should be stable.
HEALTH1="$("$BIN" daemon health --base-dir "$TMP" 2>/dev/null || true)"
PID1="$(printf '%s' "$HEALTH1" | node -e 'const d=JSON.parse(require("fs").readFileSync(0,"utf8"));process.stdout.write(String(d.pid||""))' 2>/dev/null || true)"
"$BIN" eval "1+1" >/dev/null 2>&1 || true
HEALTH2="$("$BIN" daemon health --base-dir "$TMP" 2>/dev/null || true)"
PID2="$(printf '%s' "$HEALTH2" | node -e 'const d=JSON.parse(require("fs").readFileSync(0,"utf8"));process.stdout.write(String(d.pid||""))' 2>/dev/null || true)"
if [[ -n "$PID1" && "$PID1" == "$PID2" ]]; then
  pass "idempotent spawn (daemon pid stable: $PID1)"
else
  fail "idempotent spawn" "pid1=$PID1 pid2=$PID2"
fi

# Case 3: --json formats output as compact JSON (string gets quoted).
JSON_OUT="$("$BIN" get-text --selector h1 --json 2>/dev/null || true)"
if [[ "$JSON_OUT" == '"Direct Smoke"' ]]; then
  pass "--json wraps string ('\"Direct Smoke\"')"
else
  fail "--json output shape" "got: $JSON_OUT"
fi

# Case 4: --out FILE base64 round-trip via capture.screenshot.
PNG_OUT="$TMP/shot.png"
if "$BIN" screenshot --out "$PNG_OUT" --json >/dev/null 2>&1; then
  if [[ -f "$PNG_OUT" ]]; then
    SIZE="$(stat -c%s "$PNG_OUT" 2>/dev/null || stat -f%z "$PNG_OUT" 2>/dev/null || echo 0)"
    MAGIC="$(xxd -l 4 -p "$PNG_OUT" 2>/dev/null || true)"
    if [[ "$MAGIC" == "89504e47" && "$SIZE" -gt 0 ]]; then
      pass "--out PNG round-trip (${SIZE} bytes, magic OK)"
    else
      fail "--out PNG payload" "size=$SIZE magic=$MAGIC"
    fi
  else
    fail "--out PNG" "file not written"
  fi
else
  fail "--out PNG" "screenshot exit nonzero"
fi

# Case 5: structured error — missing selector → errorCode + exit 5.
set +e
CLICK_OUT="$("$BIN" click '#definitely-not-there' --timeout 1200 --json 2>/dev/null)"
CLICK_RC=$?
set -e
if [[ "$CLICK_RC" == "5" && "$CLICK_OUT" == *'"errorCode":"selector_not_found"'* ]]; then
  pass "selector_not_found → errorCode + exit 5"
else
  fail "selector_not_found error contract" "rc=$CLICK_RC out=$CLICK_OUT"
fi

# Case 6: structured error — bad tab index → tab_not_found + exit 5.
set +e
TAB_OUT="$("$BIN" console-logs --tab 99 --json 2>/dev/null)"
TAB_RC=$?
set -e
if [[ "$TAB_RC" == "5" && "$TAB_OUT" == *'"errorCode":"tab_not_found"'* ]]; then
  pass "tab_not_found → errorCode + exit 5"
else
  fail "tab_not_found error contract" "rc=$TAB_RC out=$TAB_OUT"
fi

# Case 7: structured error — dead socket + no auto-spawn → daemon_unreachable
# + exit 3, and --json still emits a JSON envelope (not bare prose).
DEAD_DIR="$(mktemp -d -t tabd-dead.XXXX)"
set +e
DEAD_OUT="$(TABD_NO_AUTO_SPAWN=1 TABD_BASE_DIR="$DEAD_DIR" "$BIN" get-text --json 2>/dev/null)"
DEAD_RC=$?
set -e
rm -rf "$DEAD_DIR"
if [[ "$DEAD_RC" == "3" && "$DEAD_OUT" == *'"errorCode":"daemon_unreachable"'* ]]; then
  pass "daemon_unreachable → JSON envelope + exit 3"
else
  fail "daemon_unreachable error contract" "rc=$DEAD_RC out=$DEAD_OUT"
fi

echo "== summary =="
echo "passed: $PASS_COUNT"
echo "failed: $FAIL_COUNT"
exit $FAIL_COUNT
