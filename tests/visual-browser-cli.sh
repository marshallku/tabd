#!/usr/bin/env bash
# visual-browser-cli.sh — V1 WU-C end-to-end for `tabd browser`.
#
# The owner entry point the default-browser registration calls. Asserts it
# starts the visual daemon by itself, opens urls through the daemon's pipe,
# refuses switch-shaped input, and warns about session restore exactly once
# per browser start rather than on every link.
#
# Needs a graphical session; see visual-daemon-lifecycle.sh for the ssh env.

set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ROOT_DIR}/crates/tabd/target/release/tabd"
[[ -x "$BIN" ]] || { echo "Missing tabd binary. cargo build --release --manifest-path crates/tabd/Cargo.toml" >&2; exit 2; }

PASS=0; FAIL=0
pass() { echo "PASS  $1"; PASS=$((PASS+1)); }
fail() { echo "FAIL  $1"; echo "      $2"; FAIL=$((FAIL+1)); }

TMP="$(mktemp -d -t tabd-browser-cli.XXXX)"
BASE="$TMP/base"
export TABD_VISUAL_PROFILE_DIR="$TMP/profile"
mkdir -p "$TMP/pages"
for n in one two; do echo "<title>$n</title>" > "$TMP/pages/$n.html"; done

cleanup() {
    "$BIN" daemon stop --base-dir "$BASE" >/dev/null 2>&1
    sleep 2
    pkill -f -- "--user-data-dir=$TABD_VISUAL_PROFILE_DIR" 2>/dev/null
    rm -rf "$TMP"
}
trap cleanup EXIT

browser() { "$BIN" browser --base-dir "$BASE" "$@"; }

echo "== visual-browser-cli =="

# 1. No daemon yet: `tabd browser` has to bring the whole stack up itself.
#    This is the cold path a clicked link takes.
OUT="$(browser "file://$TMP/pages/one.html" 2>&1)"; RC=$?
[[ $RC -eq 0 ]] && pass "cold start: tabd browser spawned the daemon and opened a url" \
    || fail "cold start" "rc=$RC out=$OUT"
[[ -S "$BASE/daemon.sock" ]] && pass "visual daemon socket exists after cold start" \
    || fail "daemon socket exists" "no $BASE/daemon.sock"

# The url went on the browser's command line, so it is `requested`, not a
# claim that a target was created.
grep -q "requested file://$TMP/pages/one.html" <<<"$OUT" \
    && pass "start url reported as requested" || fail "start url requested" "$OUT"

# A profile that does not reopen its tabs must be warned about — once, on the
# launch. A brand-new profile reads as `unknown` rather than `off` (Chromium
# has not written `Preferences` yet), and that warns too: the person setting
# the profile up is exactly who needs to hear it.
grep -qi "continue where you left off" <<<"$OUT" \
    && pass "warned about session restore on the launch" || fail "session-restore warning" "$OUT"

# 2. Warm path: the browser is already up, so the url goes over the existing
#    pipe and gets a real target id — and the warning does NOT repeat.
OUT="$(browser "file://$TMP/pages/two.html" 2>&1)"; RC=$?
[[ $RC -eq 0 ]] && pass "warm open succeeded" || fail "warm open" "rc=$RC out=$OUT"
grep -q "opened file://$TMP/pages/two.html" <<<"$OUT" \
    && pass "warm url reported as opened (a target was created)" || fail "warm url opened" "$OUT"
grep -qi "continue where you left off" <<<"$OUT" \
    && fail "warning is not repeated per link" "$OUT" \
    || pass "warning not repeated when the browser was already running"

# 3. Several urls at once — a `.desktop` Exec=… %U hands over a burst.
OUT="$(browser "file://$TMP/pages/one.html" "file://$TMP/pages/two.html" 2>&1)"; RC=$?
if [[ $RC -eq 0 && "$(grep -c '^opened ' <<<"$OUT")" -eq 2 ]]; then
    pass "multiple urls in one invocation (%U)"
else
    fail "multiple urls" "rc=$RC out=$OUT"
fi

# 4. No url: make sure the browser is up, say so, open nothing.
OUT="$(browser 2>&1)"; RC=$?
[[ $RC -eq 0 ]] && grep -q "browser already running" <<<"$OUT" \
    && pass "no url reports the browser is already running" || fail "no url" "rc=$RC out=$OUT"

# 5. Hostile input. These reach the browser's command line, so the CLI must
#    refuse them rather than pass them through.
for hostile in "--no-sandbox" "javascript:alert(1)" "data:text/html,x"; do
    OUT="$(browser "$hostile" 2>&1)"; RC=$?
    [[ $RC -ne 0 ]] && pass "refused $hostile" || fail "refused $hostile" "rc=$RC out=$OUT"
done

# 6. --json emits the raw envelope for scripting.
OUT="$(browser --json "file://$TMP/pages/one.html" 2>/dev/null)"
if python3 -c "import json,sys; d=json.loads(sys.argv[1]); sys.exit(0 if d['success'] and d['data']['results'] else 1)" "$OUT"; then
    pass "--json emits the raw response"
else
    fail "--json" "$OUT"
fi

# 7. A headless daemon on the same base dir must not be mistaken for ours.
HBASE="$TMP/headless"
mkdir -p "$HBASE"
TABD_NO_AUTO_SPAWN=1 "$BIN" daemon start --base-dir "$HBASE" >"$HBASE/log" 2>&1 &
for _ in $(seq 1 60); do [[ -S "$HBASE/daemon.sock" ]] && break; sleep 0.2; done
if [[ -S "$HBASE/daemon.sock" ]]; then
    OUT="$("$BIN" browser --base-dir "$HBASE" 2>&1)"; RC=$?
    { [[ $RC -ne 0 ]] && grep -q "headless daemon" <<<"$OUT"; } \
        && pass "refuses to drive a headless daemon" || fail "refuses headless daemon" "rc=$RC out=$OUT"
    "$BIN" daemon stop --base-dir "$HBASE" >/dev/null 2>&1
else
    fail "headless daemon started" "socket never appeared"
fi

# 8. A socket that accepts but never answers must not hang the CLI. On macOS
#    a hung `tabd browser` also blocks the wrapper app's event queue, so every
#    link clicked after it would stall too.
DBASE="$TMP/deaf"
mkdir -p "$DBASE"
cat > "$TMP/deaf.py" <<'DEAF'
import socket, sys, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.bind(sys.argv[1])
s.listen(8)
s.settimeout(1)
held = []
deadline = time.time() + 120
while time.time() < deadline:
    try:
        conn, _ = s.accept()
        held.append(conn)   # accept, read nothing, answer nothing
    except OSError:
        pass
DEAF
python3 "$TMP/deaf.py" "$DBASE/daemon.sock" &
DEAF_PID=$!
for _ in $(seq 1 40); do [[ -S "$DBASE/daemon.sock" ]] && break; sleep 0.2; done
START=$(date +%s)
OUT="$(TABD_NO_AUTO_SPAWN=1 "$BIN" browser --base-dir "$DBASE" 2>&1)"; RC=$?
ELAPSED=$(( $(date +%s) - START ))
kill "$DEAF_PID" 2>/dev/null
if [[ $RC -ne 0 && $ELAPSED -lt 30 ]]; then
    pass "a listening-but-silent daemon fails fast (${ELAPSED}s), not forever"
else
    fail "silent daemon fails fast" "rc=$RC elapsed=${ELAPSED}s out=$OUT"
fi

echo "== summary =="
echo "passed: $PASS"
echo "failed: $FAIL"
[[ "$FAIL" -eq 0 ]]
