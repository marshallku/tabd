#!/usr/bin/env bash
# visual-daemon-lifecycle.sh — V1 WU-B end-to-end.
#
# Drives a real visual daemon against a THROWAWAY profile and asserts the
# lifecycle rules that make visual mode different from the headless daemon:
# it never restarts the browser, `Closed` means the child was reaped, the
# profile takes one owner, and a driver action cannot reach the browser.
#
# Needs a graphical session — a visual launch is by definition not headless.
# Over ssh on Linux, export first:
#   XDG_RUNTIME_DIR WAYLAND_DISPLAY DISPLAY XDG_SESSION_TYPE=wayland
#   DBUS_SESSION_BUS_ADDRESS
#
# Pre-req: cargo build --release --manifest-path crates/tabd/Cargo.toml

set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ROOT_DIR}/crates/tabd/target/release/tabd"
[[ -x "$BIN" ]] || { echo "Missing tabd binary. cargo build --release --manifest-path crates/tabd/Cargo.toml" >&2; exit 2; }

PASS=0; FAIL=0
pass() { echo "PASS  $1"; PASS=$((PASS+1)); }
fail() { echo "FAIL  $1"; echo "      $2"; FAIL=$((FAIL+1)); }

TMP="$(mktemp -d -t tabd-visual.XXXX)"
PROFILE="$TMP/profile"
BASE_A="$TMP/base-a"
BASE_B="$TMP/base-b"
export TABD_VISUAL_PROFILE_DIR="$PROFILE"

# One JSON request over the daemon socket. Python because the protocol is
# newline-delimited JSON over a unix socket and nc's -U support varies.
req() {
    python3 - "$1" "$2" "${3:-{\}}" <<'PY'
import json, socket, sys
sock, action, params = sys.argv[1], sys.argv[2], sys.argv[3]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(90)
try:
    s.connect(sock)
except OSError as e:
    print(json.dumps({"success": False, "error": f"connect: {e}", "errorCode": "daemon_unreachable"}))
    sys.exit(0)
s.sendall((json.dumps({"id": "e2e", "action": action, "params": json.loads(params)}) + "\n").encode())
buf = b""
while not buf.endswith(b"\n"):
    chunk = s.recv(65536)
    if not chunk:
        break
    buf += chunk
print(buf.decode().strip() or json.dumps({"success": False, "error": "no response"}))
PY
}

jqf() { python3 -c "import json,sys; v=json.load(sys.stdin)
for k in sys.argv[1].split('.'):
    v = v.get(k) if isinstance(v, dict) else None
print('' if v is None else v)" "$1"; }

cleanup() {
    for base in "$BASE_A" "$BASE_B" "$TMP/base-c"; do
        [[ -S "$base/daemon.sock" ]] && req "$base/daemon.sock" daemon.shutdown >/dev/null 2>&1
    done
    sleep 2
    # Backstop: nothing may survive against the scratch profile.
    pkill -f -- "--user-data-dir=$PROFILE" 2>/dev/null
    rm -rf "$TMP"
}
trap cleanup EXIT

start_daemon() {
    local base="$1"
    local profile="${2:-$PROFILE}"
    mkdir -p "$base"
    TABD_NO_AUTO_SPAWN=1 TABD_VISUAL_PROFILE_DIR="$profile" \
        "$BIN" daemon start --visual --base-dir "$base" \
        >"$base/daemon.log" 2>&1 &
    for _ in $(seq 1 60); do
        [[ -S "$base/daemon.sock" ]] && return 0
        sleep 0.2
    done
    return 1
}

browser_pid() { req "$BASE_A/daemon.sock" daemon.health | jqf data.driver.chromiumPid; }

echo "== visual-daemon-lifecycle =="
echo "profile: $PROFILE"

start_daemon "$BASE_A" || { echo "daemon A never bound its socket"; cat "$BASE_A/daemon.log"; exit 1; }

# 1. Reachable with the browser closed. This is the case that would deadlock
#    if readiness gated the lifecycle actions.
H="$(req "$BASE_A/daemon.sock" daemon.health)"
[[ "$(echo "$H" | jqf data.mode)" == "visual" ]] \
    && pass "daemon reports visual mode" || fail "daemon reports visual mode" "$H"
[[ "$(echo "$H" | jqf data.browserState)" == "closed" ]] \
    && pass "browser starts Closed, daemon still reachable" || fail "browser starts Closed" "$H"
[[ "$(echo "$H" | jqf data.ready)" == "False" ]] \
    && pass "not ready with no browser" || fail "not ready with no browser" "$H"

# 2. A driver action cannot reach the human's browser.
R="$(req "$BASE_A/daemon.sock" tabs.navigate '{"url":"https://example.com"}')"
[[ "$(echo "$R" | jqf errorCode)" == "visual_mode_unsupported" ]] \
    && pass "driver action rejected (visual_mode_unsupported)" \
    || fail "driver action rejected" "$R"

# 3. ensure opens the browser and the urls.
mkdir -p "$TMP/pages"
for n in one two; do echo "<title>$n</title>" > "$TMP/pages/$n.html"; done
R="$(req "$BASE_A/daemon.sock" browser.ensure "{\"urls\":[\"file://$TMP/pages/one.html\",\"file://$TMP/pages/two.html\"]}")"
[[ "$(echo "$R" | jqf success)" == "True" ]] \
    && pass "browser.ensure launched the browser" || fail "browser.ensure launched" "$R"
[[ "$(echo "$R" | jqf data.browserState)" == "running" ]] \
    && pass "browserState becomes Running after the CDP handshake" \
    || fail "browserState Running" "$R"
# Start urls are reported as `requested`, not `opened`: they went on the
# command line and nothing confirmed a target was created for them.
if python3 -c "import json,sys; d=json.loads(sys.argv[1])['data']['results']
sys.exit(0 if d and all(r['status']=='requested' for r in d) else 1)" "$R"; then
    pass "start urls reported as requested, not claimed opened"
else
    fail "start urls reported as requested" "$R"
fi

PID1="$(browser_pid)"
[[ -n "$PID1" && "$PID1" != "None" ]] \
    && pass "health reports the browser pid ($PID1)" || fail "health reports a pid" "$(req "$BASE_A/daemon.sock" daemon.health)"

# 4. A second daemon on the same profile must not get in — including one that
#    reaches it through a symlink, which would otherwise take a second lock on
#    one user-data-dir.
ALIAS="$TMP/profile-alias"
ln -s "$PROFILE" "$ALIAS"
if start_daemon "$BASE_B" "$ALIAS"; then
    R="$(req "$BASE_B/daemon.sock" browser.ensure '{}')"
    [[ "$(echo "$R" | jqf errorCode)" == "profile_locked" ]] \
        && pass "a daemon reaching the profile via a symlink is refused too" \
        || fail "aliased daemon refused" "$R"
    req "$BASE_B/daemon.sock" daemon.shutdown >/dev/null
else
    fail "second daemon started" "socket never appeared"
fi

# 5. Kill the browser: the daemon must notice, must NOT restart, and must say why.
kill -9 "$PID1" 2>/dev/null
NOTICED=""
for _ in $(seq 1 40); do
    H="$(req "$BASE_A/daemon.sock" daemon.health)"
    if [[ "$(echo "$H" | jqf data.browserState)" == "closed" ]]; then NOTICED=yes; break; fi
    sleep 0.5
done
[[ -n "$NOTICED" ]] && pass "transport EOF drove the state to Closed" || fail "state reached Closed" "$H"
[[ "$(echo "$H" | jqf data.notReadyReason)" == "browser_closed" ]] \
    && pass "notReadyReason is browser_closed" || fail "notReadyReason" "$H"
[[ "$(echo "$H" | jqf data.ready)" == "False" ]] \
    && pass "not ready after the browser closed" || fail "not ready after close" "$H"

sleep 6
H="$(req "$BASE_A/daemon.sock" daemon.health)"
if [[ "$(echo "$H" | jqf data.browserState)" == "closed" && "$(echo "$H" | jqf data.driver)" == "" ]]; then
    pass "no restart after 6s (visual never restarts)"
else
    fail "no restart after 6s" "$H"
fi
[[ "$(echo "$H" | jqf data.restartAttempts)" == "" ]] \
    && pass "supervisor is not running in visual mode" \
    || fail "supervisor is not running" "$H"

# 5b. A launch that fails the handshake must clean up after itself: no
#     orphaned browser, and the profile lock released so the next try works.
#     A browser that exits immediately is the cheap stand-in for the real
#     case (a Chromium singleton hand-off), which also leaves us connected to
#     a dead pipe.
BASE_C="$TMP/base-c"
mkdir -p "$BASE_C"
FAKE_PROFILE="$TMP/profile-fake"
# A bare name, so this also covers `$BROWSER_EXECUTABLE` being resolved
# through $PATH rather than against the daemon's working directory.
TABD_NO_AUTO_SPAWN=1 TABD_VISUAL_PROFILE_DIR="$FAKE_PROFILE" BROWSER_EXECUTABLE=true \
    "$BIN" daemon start --visual --base-dir "$BASE_C" >"$BASE_C/daemon.log" 2>&1 &
for _ in $(seq 1 60); do [[ -S "$BASE_C/daemon.sock" ]] && break; sleep 0.2; done
if [[ -S "$BASE_C/daemon.sock" ]]; then
    R="$(req "$BASE_C/daemon.sock" browser.ensure '{}')"
    [[ "$(echo "$R" | jqf success)" == "False" ]] \
        && pass "a browser that never answers the handshake fails the launch" \
        || fail "handshake failure fails the launch" "$R"
    H="$(req "$BASE_C/daemon.sock" daemon.health)"
    [[ "$(echo "$H" | jqf data.browserState)" == "failed" ]] \
        && pass "browserState is Failed after a bad launch" || fail "browserState Failed" "$H"
    [[ "$(echo "$H" | jqf data.driver)" == "" ]] \
        && pass "no browser left behind after a bad launch" || fail "no browser left behind" "$H"
    # The lock must be free again: a second attempt must not report it held.
    R="$(req "$BASE_C/daemon.sock" browser.ensure '{}')"
    [[ "$(echo "$R" | jqf errorCode)" != "profile_locked" ]] \
        && pass "profile lock released after a failed launch" \
        || fail "profile lock released after a failed launch" "$R"
    req "$BASE_C/daemon.sock" daemon.shutdown >/dev/null
else
    fail "daemon C started" "socket never appeared"
fi

# 5c. A profile is bound to the browser that created it.
R="$(req "$BASE_A/daemon.sock" browser.status '{}')"
[[ "$(echo "$R" | jqf success)" == "True" ]] \
    && pass "browser.status is served" || fail "browser.status is served" "$R"
[[ -f "$PROFILE.browser" ]] \
    && pass "profile recorded its browser binding" || fail "profile binding recorded" "no $PROFILE.browser"

# 6. A closed browser can be reopened — the reason the daemon stays up.
R="$(req "$BASE_A/daemon.sock" browser.ensure '{}')"
[[ "$(echo "$R" | jqf data.browserState)" == "running" ]] \
    && pass "browser.ensure reopens a Closed browser" || fail "ensure reopens" "$R"
PID2="$(browser_pid)"
[[ -n "$PID2" && "$PID2" != "$PID1" ]] \
    && pass "reopen produced a new browser ($PID1 → $PID2)" || fail "reopen new pid" "got $PID2"

# 7. Shutdown is terminal and takes the browser with it, gracefully.
req "$BASE_A/daemon.sock" daemon.shutdown >/dev/null
GONE=""
for _ in $(seq 1 40); do
    kill -0 "$PID2" 2>/dev/null || { GONE=yes; break; }
    sleep 0.5
done
[[ -n "$GONE" ]] && pass "daemon.shutdown closed the browser" || fail "shutdown closed the browser" "pid $PID2 still alive"
for _ in $(seq 1 20); do [[ -S "$BASE_A/daemon.sock" ]] || break; sleep 0.5; done
[[ -S "$BASE_A/daemon.sock" ]] && fail "socket removed on exit" "still present" || pass "socket removed on exit"

echo "== summary =="
echo "passed: $PASS"
echo "failed: $FAIL"
[[ "$FAIL" -eq 0 ]]
