#!/usr/bin/env bash
# react-controlled-input.sh — regression test for the React controlled-input
# typing fix (D134 / REACT_SAFE_SET_VALUE).
#
# The Toss Invest sign-in form is a React controlled input: `el.value = x`
# followed by an `input` event does NOT fire React's onChange, because React
# overrides the instance-level value setter to keep its internal value tracker
# in sync, so the tracker already equals the assigned value and React treats the
# event as a no-op. tabd must set the value via the *prototype* setter to bypass
# that override. This test reproduces React's `_valueTracker` mechanism exactly
# (no CDN/React needed) and asserts that `tabd type` / `tabd type-secret` cause
# the mock onChange to actually commit.
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

resolve_chromium() {
  if [[ -n "${BROWSER_EXECUTABLE:-}" && -x "$BROWSER_EXECUTABLE" ]]; then
    printf '%s' "$BROWSER_EXECUTABLE"; return
  fi
  for c in google-chrome google-chrome-stable chromium chromium-browser; do
    if command -v "$c" >/dev/null 2>&1; then command -v "$c"; return; fi
  done
  shopt -s nullglob
  for d in \
    /Applications/Google\ Chrome.app/Contents/MacOS/Google\ Chrome \
    /Applications/Chromium.app/Contents/MacOS/Chromium \
    "$HOME"/.cache/ms-playwright/chromium-*/chrome-linux64/chrome \
    "$HOME"/.cache/ms-playwright/chromium-*/chrome-mac*/Chromium.app/Contents/MacOS/Chromium; do
    [[ -x "$d" ]] && { printf '%s' "$d"; shopt -u nullglob; return; }
  done
  shopt -u nullglob
}
CHROMIUM_BIN="$(resolve_chromium || true)"
[[ -n "$CHROMIUM_BIN" ]] || { echo "no chromium found"; exit 2; }
export BROWSER_EXECUTABLE="$CHROMIUM_BIN"

TMP="$(mktemp -d -t tabd-react.XXXX)"
export TABD_BASE_DIR="$TMP"
export TABD_VAULT_KEY="react-test-passphrase"
cleanup() {
  "$BIN" daemon stop --base-dir "$TMP" >/dev/null 2>&1 || true
  sleep 0.5
  rm -rf "$TMP"
}
trap cleanup EXIT

# A page whose #ctrl input mimics a React controlled input: an instance-level
# value setter advances a tracker (as React's inputValueTracking does), and the
# input listener only "commits" (window.__committed) when the node value differs
# from the tracker — exactly React's onChange dedupe. Direct `el.value = x`
# would make tracker === value → no commit; the prototype-setter path leaves the
# tracker stale → commit fires.
# Installs a React value-tracker on a given element and records committed
# onChange values on a window-scoped key. Single-quoted JS so it drops cleanly
# into both a top-level <script> and a double-quoted srcdoc attribute.
REACT_SHIM="
function installReactInput(el, win, key) {
  let tracked = '';
  const ctor = el.tagName === 'TEXTAREA' ? win.HTMLTextAreaElement : win.HTMLInputElement;
  const proto = Object.getOwnPropertyDescriptor(ctor.prototype, 'value');
  Object.defineProperty(el, 'value', {
    configurable: true,
    get() { return proto.get.call(this); },
    set(v) { tracked = '' + v; proto.set.call(this, v); },
  });
  win[key] = null;
  el.addEventListener('input', () => {
    const cur = proto.get.call(el);
    if (cur === tracked) return;
    tracked = cur;
    win[key] = cur;
  });
}
"

# The iframe uses srcdoc so it inherits the parent's origin (same-origin) — the
# parent can then read its window, and its controlled input still lives in a
# separate realm, exercising the ownerDocument.defaultView constructor lookup.
# A <textarea> (not <input>) makes the cross-realm bug observable: with the old
# top-window instanceof check it falls through to HTMLInputElement's value setter
# and calling that on a textarea throws "Illegal invocation".
FRAME_DOC="<textarea id='fctrl'></textarea><script>${REACT_SHIM} installReactInput(document.getElementById('fctrl'), window, '__committed');</script>"

HTML="$TMP/react.html"
cat >"$HTML" <<EOF
<!doctype html>
<meta charset="utf-8">
<title>react-controlled</title>
<input id="ctrl" type="text">
<div id="edit" contenteditable="true"></div>
<iframe id="fr" srcdoc="$FRAME_DOC"></iframe>
<script>$REACT_SHIM
installReactInput(document.getElementById("ctrl"), window, "__committed");
// contentEditable has no value tracker; just record that an input event fired.
window.__editInput = null;
document.getElementById("edit").addEventListener("input", (e) => {
  window.__editInput = e.target.textContent;
});
</script>
EOF

PASS_COUNT=0
FAIL_COUNT=0
pass() { printf "PASS  %s\n" "$1"; PASS_COUNT=$((PASS_COUNT + 1)); }
fail() { printf "FAIL  %s\n" "$1"; [[ -n "${2:-}" ]] && printf "  detail: %s\n" "$2"; FAIL_COUNT=$((FAIL_COUNT + 1)); }

echo "== react-controlled-input (onChange fires) =="

"$BIN" navigate "file://$HTML" >/dev/null 2>&1 || { echo "navigate failed"; exit 1; }

# type into the React-style controlled input
"$BIN" type --selector "#ctrl" --text "01099998888" >/dev/null 2>&1 || true
COMMITTED="$("$BIN" eval 'window.__committed' --json 2>/dev/null)"
if echo "$COMMITTED" | grep -q '01099998888'; then
  pass "type fires React onChange (committed=01099998888)"
else
  fail "type fires React onChange" "committed=$COMMITTED"
fi

# The DOM value must also reflect the typed text.
DOMVAL="$("$BIN" eval 'document.getElementById("ctrl").value' --json 2>/dev/null)"
if echo "$DOMVAL" | grep -q '01099998888'; then
  pass "type sets DOM value"
else
  fail "type sets DOM value" "value=$DOMVAL"
fi

# type-secret through the same controlled input. secret-put keeps plaintext off
# argv (stdin) and auto-generates an id we read back from the JSON response.
"$BIN" eval 'window.__committed = null; document.getElementById("ctrl").value = ""' >/dev/null 2>&1 || true
PUT_OUT="$(printf 'hunter2secret' | "$BIN" secret-put --stdin --label react-test --json 2>/dev/null || true)"
SID="$(printf '%s' "$PUT_OUT" | sed -n 's/.*"secretId"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
if [[ -z "$SID" ]]; then
  fail "secret-put returned an id" "out=$PUT_OUT"
else
  "$BIN" type-secret --selector "#ctrl" --secret-id "$SID" >/dev/null 2>&1 || true
  SEC_COMMITTED="$("$BIN" eval 'window.__committed' --json 2>/dev/null)"
  if echo "$SEC_COMMITTED" | grep -q 'hunter2secret'; then
    pass "type-secret fires React onChange"
  else
    fail "type-secret fires React onChange" "committed=$SEC_COMMITTED"
  fi
  "$BIN" secret-delete --secret-id "$SID" >/dev/null 2>&1 || true
fi

# type into a contentEditable element. It has no `value`, so the native
# prototype setter must NOT be used (it would throw "Illegal invocation");
# handle_type falls back to execCommand("insertText") and fires input/change.
"$BIN" type --selector "#edit" --text "hello world" >/dev/null 2>&1 || true
EDIT_TEXT="$("$BIN" eval 'document.getElementById("edit").textContent' --json 2>/dev/null)"
EDIT_INPUT="$("$BIN" eval 'window.__editInput' --json 2>/dev/null)"
if echo "$EDIT_TEXT" | grep -q 'hello world' && echo "$EDIT_INPUT" | grep -q 'hello world'; then
  pass "type into contentEditable sets text and fires input"
else
  fail "type into contentEditable" "text=$EDIT_TEXT input=$EDIT_INPUT"
fi

# type --frame into a controlled input that lives in a same-origin iframe. The
# element belongs to the iframe's realm, so the setter must resolve constructors
# via ownerDocument.defaultView or Chrome throws "Illegal invocation".
"$BIN" type --selector "#fctrl" --text "07011112222" --frame "#fr" >/dev/null 2>&1 || true
FR_COMMITTED="$("$BIN" eval 'document.querySelector("#fr").contentDocument.defaultView.__committed' --json 2>/dev/null)"
if echo "$FR_COMMITTED" | grep -q '07011112222'; then
  pass "type --frame fires React onChange in iframe realm"
else
  fail "type --frame fires React onChange in iframe realm" "committed=$FR_COMMITTED"
fi

echo
echo "== summary: $PASS_COUNT passed, $FAIL_COUNT failed =="
[[ "$FAIL_COUNT" -eq 0 ]]
