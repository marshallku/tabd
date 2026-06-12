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
<body>
<h1>Direct Smoke</h1>
<button id="alert-btn" onclick="alert('hello from dialog')">Alert</button>
<button id="do-btn" onclick="document.title='Clicked'">Do Thing</button>
<input type="file" id="file-in" style="display:none">
<label for="file-in">Attach file</label>
<script>
  setTimeout(() => {
    const div = document.createElement("div");
    div.textContent = "Deferred Text";
    document.body.appendChild(div);
  }, 300);
</script>
</body></html>
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

# Case 8: dialog auto-handling — a click that fires alert() must not wedge
# (the reader auto-dismisses per the default policy) and must be recorded.
set +e
"$BIN" click '#alert-btn' --timeout 5000 >/dev/null 2>&1
ALERT_RC=$?
DIALOGS_OUT="$("$BIN" dialogs --json 2>/dev/null)"
set -e
if [[ "$ALERT_RC" == "0" && "$DIALOGS_OUT" == *'"dialogType":"alert"'* && "$DIALOGS_OUT" == *'"action":"dismiss"'* ]]; then
  pass "alert() auto-dismissed without wedging click + recorded in dialogs"
else
  fail "dialog auto-handling" "rc=$ALERT_RC dialogs=$DIALOGS_OUT"
fi

# Case 9: wait-text — finds delayed text; absent text expires with exit 4.
set +e
WT_OUT="$("$BIN" wait-text 'Deferred Text' --timeout 5000 --json 2>/dev/null)"
WT_RC=$?
"$BIN" wait-text 'definitely-absent-string-xyz' --timeout 1200 >/dev/null 2>&1
WT_MISS_RC=$?
set -e
if [[ "$WT_RC" == "0" && "$WT_OUT" == *'"found":true'* && "$WT_MISS_RC" == "4" ]]; then
  pass "wait-text found + expiry exit 4"
else
  fail "wait-text contract" "hit rc=$WT_RC out=$WT_OUT; miss rc=$WT_MISS_RC"
fi

# Case 10: --max-chars clamp on get-html appends a visible truncation marker.
CLAMP_OUT="$("$BIN" get-html --max-chars 50 --json 2>/dev/null || true)"
if [[ "$CLAMP_OUT" == *'truncated: 50 of '* ]]; then
  pass "get-html --max-chars truncation marker"
else
  fail "get-html clamp" "got: $CLAMP_OUT"
fi

# Case 11: oversized non-string eval result errors instead of emitting
# truncated (corrupt) JSON.
set +e
BIG_OUT="$("$BIN" eval 'Array.from({length: 100000}, (_, i) => i)' --max-chars 1000 --json 2>/dev/null)"
BIG_RC=$?
set -e
if [[ "$BIG_RC" == "1" && "$BIG_OUT" == *'"errorCode":"output_too_large"'* ]]; then
  pass "oversized eval → exit 1 + output_too_large"
else
  fail "eval output clamp" "rc=$BIG_RC out=$BIG_OUT"
fi

# Case 12: click --text finds and clicks by visible label.
set +e
"$BIN" click --text 'do thing' --timeout 5000 >/dev/null 2>&1
CT_RC=$?
TITLE_OUT="$("$BIN" eval 'document.title' --json 2>/dev/null)"
set -e
if [[ "$CT_RC" == "0" && "$TITLE_OUT" == '"Clicked"' ]]; then
  pass "click --text by visible label"
else
  fail "click --text" "rc=$CT_RC title=$TITLE_OUT"
fi

# Case 13: query --text (unscoped) returns the deepest matching element only.
QT_OUT="$("$BIN" query --text 'Deferred Text' --json 2>/dev/null || true)"
if [[ "$QT_OUT" == *'"tag":"div"'* && "$QT_OUT" != *'"tag":"body"'* ]]; then
  pass "query --text deepest-match filter"
else
  fail "query --text" "got: $QT_OUT"
fi

# Case 14: click --text for an absent label → selector_not_found / exit 5.
set +e
"$BIN" click --text 'absent-label-xyz' --timeout 1200 >/dev/null 2>&1
CT_MISS_RC=$?
set -e
if [[ "$CT_MISS_RC" == "5" ]]; then
  pass "click --text absent label → exit 5"
else
  fail "click --text absent label" "rc=$CT_MISS_RC"
fi

# Case 15: upload — relative path resolved against the CLI's cwd (different
# from the daemon's cwd), set on a hidden file input via DOM.setFileInputFiles.
echo "csv,data" > "$TMP/upload-src.txt"
set +e
(cd "$TMP" && "$BIN" upload '#file-in' 'upload-src.txt') >/dev/null 2>&1
UP_RC=$?
UP_NAME="$("$BIN" eval 'document.querySelector("#file-in").files[0]?.name ?? "none"' --json 2>/dev/null)"
set -e
if [[ "$UP_RC" == "0" && "$UP_NAME" == '"upload-src.txt"' ]]; then
  pass "upload via relative path onto hidden file input"
else
  fail "upload" "rc=$UP_RC files0=$UP_NAME"
fi

# Case 16: upload with a missing file → client-side usage error, exit 2.
set +e
"$BIN" upload '#file-in' '/definitely/missing/file.bin' >/dev/null 2>&1
UP_MISS_RC=$?
set -e
if [[ "$UP_MISS_RC" == "2" ]]; then
  pass "upload missing file → exit 2 (client-side)"
else
  fail "upload missing file" "rc=$UP_MISS_RC"
fi

echo "== summary =="
echo "passed: $PASS_COUNT"
echo "failed: $FAIL_COUNT"
exit $FAIL_COUNT
