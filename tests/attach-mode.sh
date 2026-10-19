#!/usr/bin/env bash
# attach-mode.sh — end-to-end for `--attach`: drive a browser that is already
# running, without launching one and without touching its other tabs.
#
# Stands in for the human's browser with a scratch Chrome on
# `--remote-debugging-port=0`, which writes the same DevToolsActivePort the
# chrome://inspect/#remote-debugging toggle does. What this cannot cover is the
# toggle's per-connection Allow dialog — that needs a human click.
#
# Headed by default (agent tabs get a window of their own). Needs a graphical
# session; ATTACH_HEADLESS=1 runs it headless where there is none.

set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ROOT_DIR}/crates/tabd/target/release/tabd"
[[ -x "$BIN" ]] || { echo "Missing tabd binary. cargo build --release --manifest-path crates/tabd/Cargo.toml" >&2; exit 2; }

CHROME="${BROWSER_EXECUTABLE:-}"
if [[ -z "$CHROME" ]]; then
    for c in "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
             "$(command -v google-chrome 2>/dev/null)" "$(command -v chromium 2>/dev/null)"; do
        [[ -n "$c" && -x "$c" ]] && { CHROME="$c"; break; }
    done
fi
[[ -x "$CHROME" ]] || { echo "No Chrome found; set BROWSER_EXECUTABLE" >&2; exit 2; }

PASS=0; FAIL=0
pass() { echo "PASS  $1"; PASS=$((PASS+1)); }
fail() { echo "FAIL  $1"; echo "      $2"; FAIL=$((FAIL+1)); }

TMP="$(mktemp -d -t tabd-attach.XXXX)"
BASE="$TMP/base"
PROFILE="$TMP/profile"
export TABD_ATTACH_USER_DATA_DIR="$PROFILE"
unset TABD_ATTACH_WS TABD_BASE_DIR
mkdir -p "$TMP/pages"
echo "<title>human</title><h1>human</h1>" > "$TMP/pages/human.html"
echo "<title>agent1</title><h1>agent1</h1><a id=pop href=agent3.html target=_blank>pop</a>" > "$TMP/pages/agent1.html"
echo "<title>agent2</title><h1>agent2</h1>" > "$TMP/pages/agent2.html"
echo "<title>agent3</title><h1>agent3</h1>" > "$TMP/pages/agent3.html"

CHROME_PID=""
start_chrome() {
    local extra=()
    [[ "${ATTACH_HEADLESS:-0}" == 1 ]] && extra+=(--headless=new)
    rm -f "$PROFILE/DevToolsActivePort"
    "$CHROME" --remote-debugging-port=0 --user-data-dir="$PROFILE" --no-first-run \
        --no-default-browser-check ${extra[@]+"${extra[@]}"} "file://$TMP/pages/human.html" \
        >/dev/null 2>&1 &
    CHROME_PID=$!
    for _ in $(seq 1 100); do
        [[ -s "$PROFILE/DevToolsActivePort" ]] && return 0
        sleep 0.1
    done
    return 1
}
devtools_port() { head -1 "$PROFILE/DevToolsActivePort"; }
all_page_urls() { curl -s "http://127.0.0.1:$(devtools_port)/json/list"; }

cleanup() {
    "$BIN" daemon stop --attach --base-dir "$BASE" >/dev/null 2>&1
    [[ -n "$CHROME_PID" ]] && kill "$CHROME_PID" 2>/dev/null
    sleep 1
    pkill -f -- "--user-data-dir=$PROFILE" 2>/dev/null
    rm -rf "$TMP"
}
trap cleanup EXIT

t() { "$BIN" "$@" --attach --base-dir "$BASE"; }

echo "== attach-mode =="
start_chrome && pass "scratch browser up with a DevToolsActivePort" \
    || { fail "scratch browser" "no DevToolsActivePort in $PROFILE"; exit 1; }

# 1. The human's tab is not ours: an attach daemon lists nothing at first.
OUT="$(t list-tabs --json 2>&1)"; RC=$?
[[ $RC -eq 0 && "$OUT" == "[]" ]] && pass "human tabs are not listed (cold attach)" \
    || fail "cold list-tabs" "rc=$RC out=$OUT"

# 2. navigate with no agent tab opens one, in the attached browser.
OUT="$(t navigate "file://$TMP/pages/agent1.html" 2>&1)"; RC=$?
[[ $RC -eq 0 ]] && pass "navigate opened an agent tab" || fail "navigate" "rc=$RC out=$OUT"
OUT="$(t get-text --selector h1 2>&1)"
[[ "$OUT" == "agent1" ]] && pass "driver actions run in the agent tab" || fail "get-text" "$OUT"

# 3. open-tab adds a second agent tab.
OUT="$(t open-tab "file://$TMP/pages/agent2.html" --json 2>&1)"; RC=$?
[[ $RC -eq 0 ]] && grep -q '"tabId":2' <<<"$OUT" && pass "open-tab returned tabId 2" \
    || fail "open-tab" "rc=$RC out=$OUT"
sleep 0.5

# 4. A tab an agent tab opens is adopted. Opened with trusted key input on a
#    focused target=_blank link: `click` is a JS el.click() without a user
#    gesture, which the popup blocker stops (same as headless).
t activate-tab --tab 1 >/dev/null 2>&1
t eval "document.querySelector('#pop').focus()" >/dev/null 2>&1
t press-key Enter >/dev/null 2>&1
sleep 1
OUT="$(t list-tabs --json 2>&1)"
COUNT="$(grep -o '"tabId"' <<<"$OUT" | wc -l | tr -d ' ')"
[[ "$COUNT" == 3 ]] && grep -q agent3.html <<<"$OUT" \
    && pass "popup opened by an agent tab is adopted" || fail "popup adoption" "$OUT"
grep -q human.html <<<"$OUT" && fail "human tab stays hidden" "$OUT" \
    || pass "human tab still not listed with agent tabs open"

# 5. Browser-wide actions are refused.
OUT="$(t cookies-get --json 2>&1)"; RC=$?
[[ $RC -ne 0 ]] && grep -q attach_mode_unsupported <<<"$OUT" \
    && pass "cookies-get refused (attach_mode_unsupported)" || fail "cookies refusal" "rc=$RC out=$OUT"

# 6. close-tab closes an agent tab, never a human one.
t close-tab --tab 2 >/dev/null 2>&1
OUT="$(t list-tabs --json 2>&1)"
COUNT="$(grep -o '"tabId"' <<<"$OUT" | wc -l | tr -d ' ')"
[[ "$COUNT" == 2 ]] && pass "close-tab closed one agent tab" || fail "close-tab" "$OUT"

# 6b. The human closes the active agent tab; the next navigate opens a fresh
#     one instead of driving the dead session.
ACTIVE_ID="$(t list-tabs --json | grep -o '"active":true,"tabId":[0-9]*,"targetId":"[^"]*"' | sed 's/.*"targetId":"//;s/"//')"
curl -s "http://127.0.0.1:$(devtools_port)/json/close/$ACTIVE_ID" >/dev/null
sleep 0.5
OUT="$(t navigate "file://$TMP/pages/agent2.html" 2>&1)"; RC=$?
TEXT="$(t get-text --selector h1 2>&1)"
[[ $RC -eq 0 && "$TEXT" == "agent2" ]] && pass "navigate after the human closed the active tab opens a new one" \
    || fail "navigate after active tab closed" "rc=$RC out=$OUT text=$TEXT active=$ACTIVE_ID"

# 7. daemon stop detaches only: the browser and every tab survive.
"$BIN" daemon stop --attach --base-dir "$BASE" >/dev/null 2>&1
sleep 1
kill -0 "$CHROME_PID" 2>/dev/null && pass "browser still running after daemon stop" \
    || fail "browser survives daemon stop" "pid $CHROME_PID gone"
URLS="$(all_page_urls)"
grep -q human.html <<<"$URLS" && grep -q agent2.html <<<"$URLS" \
    && pass "human and agent tabs left open after daemon stop" || fail "tabs survive stop" "$URLS"

# 8. A new daemon owns nothing from the previous connection.
OUT="$(t list-tabs --json 2>&1)"
[[ "$OUT" == "[]" ]] && pass "reattach starts with no owned tabs" || fail "reattach" "$OUT"

# 9. The browser going away is reported, and the next action says why.
kill "$CHROME_PID" 2>/dev/null; wait "$CHROME_PID" 2>/dev/null; CHROME_PID=""
sleep 1
OUT="$("$BIN" daemon health --attach --base-dir "$BASE" 2>&1)"
grep -q '"connected":false' <<<"$OUT" && pass "health reports disconnected" || fail "health" "$OUT"
OUT="$(t list-tabs 2>&1)"; RC=$?
[[ $RC -ne 0 ]] && grep -q "remote-debugging" <<<"$OUT" \
    && pass "action after browser exit explains how to enable attach" || fail "post-exit error" "rc=$RC out=$OUT"

echo
echo "attach-mode: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
