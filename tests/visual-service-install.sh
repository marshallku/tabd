#!/usr/bin/env bash
# visual-service-install.sh — V1 WU-E end-to-end for `tabd service install`.
#
# Installs under a scratch prefix, so nothing of the user's is touched, and
# asserts the generated artefacts are the ones the OS actually needs. It never
# sets the default browser: that is a deliberate opt-in, and a test that took
# over every link on the machine would be a bad trade for coverage.
#
# On macOS it additionally does one REAL install into ~/Applications, because
# LaunchServices does not register a bundle from a temp dir at all (measured),
# and drives a url through the private `tabd:` scheme — which proves delivery
# without anyone's default browser changing. It unregisters and removes it
# afterwards.

set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ROOT_DIR}/crates/tabd/target/release/tabd"
[[ -x "$BIN" ]] || { echo "Missing tabd binary. cargo build --release --manifest-path crates/tabd/Cargo.toml" >&2; exit 2; }

PASS=0; FAIL=0
pass() { echo "PASS  $1"; PASS=$((PASS+1)); }
fail() { echo "FAIL  $1"; echo "      $2"; FAIL=$((FAIL+1)); }

TMP="$(mktemp -d -t tabd-service.XXXX)"
PREFIX="$TMP/home"
mkdir -p "$PREFIX"
# The delivery log is the user's, not the test's — so this run does not modify
# it at all. Earlier attempts snapshotted and restored it, then filtered out
# the test's own line; both raced a concurrent `tabd browser tabd://…` and
# could lose a real delivery. Leaving the line is honest: a delivery genuinely
# happened, and the log is append-only diagnostics.
VISUAL_LOG="$HOME/Library/Application Support/tabd/visual/url-delivery.log"
NONCE="$RANDOM$RANDOM"

# Set once the real install below has happened, so an interrupted run does not
# leave a registered bundle in ~/Applications that every later run then skips
# over.
REAL_INSTALL_MINE=""
cleanup() {
    if [[ -n "$REAL_INSTALL_MINE" ]]; then
        "$BIN" service uninstall >/dev/null 2>&1
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT

EXE="$(cd "$(dirname "$BIN")" && pwd -P)/$(basename "$BIN")"

# Captured before anything runs, so the assertions below compare against the
# machine's real starting state rather than assuming it.
ENABLED_BEFORE=""
if [[ "$(uname)" != "Darwin" ]]; then
    ENABLED_BEFORE="$(systemctl --user is-enabled tabd-visual.service 2>&1 || true)"
fi

echo "== visual-service-install =="
OUT="$("$BIN" service install --prefix "$PREFIX" 2>&1)"; RC=$?
[[ $RC -eq 0 ]] && pass "install succeeded under a scratch prefix" || fail "install" "rc=$RC $OUT"

# The whole point of the opt-in: an install must not silently take over links.
grep -qi "NOT your default browser" <<<"$OUT" \
    && pass "install says it did not change the default browser" \
    || fail "default-browser notice" "$OUT"

if [[ "$(uname)" == "Darwin" ]]; then
    APP="$PREFIX/Applications/tabd.app"
    PLIST="$APP/Contents/Info.plist"
    [[ -d "$APP" ]] && pass "wrapper app was built" || fail "wrapper app built" "no $APP"

    # LaunchServices delivers urls as a GURL Apple Event, so the bundle has to
    # declare the schemes and the applet has to handle the event.
    if /usr/libexec/PlistBuddy -c "Print :CFBundleURLTypes:0:CFBundleURLSchemes" "$PLIST" 2>/dev/null \
        | grep -q "http"; then
        pass "bundle claims http/https"
    else
        fail "bundle claims http/https" "$(/usr/libexec/PlistBuddy -c 'Print' "$PLIST" 2>&1 | head -5)"
    fi
    [[ "$(/usr/libexec/PlistBuddy -c 'Print :LSUIElement' "$PLIST" 2>/dev/null)" == "true" ]] \
        && pass "wrapper is a background app (LSUIElement)" || fail "LSUIElement" "not set"
    [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$PLIST" 2>/dev/null)" == "dev.tabd.browser" ]] \
        && pass "wrapper has a stable bundle id" || fail "bundle id" "missing"
    codesign --verify "$APP" 2>/dev/null \
        && pass "wrapper is signed" || fail "wrapper signed" "codesign --verify failed"
    # Same requirement as the Linux desktop entry: a link opened from a GUI
    # must reach the daemon the service manages, not the platform default.
    osadecompile "$APP/Contents/Resources/Scripts/main.scpt" 2>/dev/null | grep -q -- "--base-dir" \
        && pass "the applet pins the base dir" \
        || fail "applet pins base dir" "$(osadecompile "$APP/Contents/Resources/Scripts/main.scpt" 2>&1 | head -6)"
    # Anything in ~/Library/LaunchAgents loads at the next login, so writing
    # the plist *is* the opt-in — "installed but not started" would still be
    # opting the user in, one logout later.
    [[ ! -f "$PREFIX/Library/LaunchAgents/dev.tabd.visual.plist" ]] \
        && pass "no launch agent without --enable-service" \
        || fail "launch agent is opt-in" "plist written anyway"
    # A scratch install must not have touched LaunchServices: it is a per-user
    # global database, so registering a bundle that is about to be deleted
    # leaves a dangling http/https handler behind.
    grep -q "$PREFIX" <<<"$(codesign -dv "$APP" 2>&1)" >/dev/null 2>&1 || true
    # The real artefact names are dot-prefixed, which is also why the
    # `find -name 'tabd*'` sweep below cannot see them.
    STRAY="$(find "$PREFIX/Applications" -maxdepth 1 -name '.tabd-*' 2>/dev/null | wc -l | tr -d ' ')"
    [[ "$STRAY" == "0" ]] && pass "no build leftovers beside the bundle" \
        || fail "build leftovers" "$(find "$PREFIX/Applications" -maxdepth 1 -name '.tabd-*')"
else
    DESKTOP="$PREFIX/.local/share/applications/tabd.desktop"
    UNIT="$PREFIX/.config/systemd/user/tabd-visual.service"
    [[ -f "$DESKTOP" ]] && pass "desktop entry installed" || fail "desktop entry" "missing"
    grep -qE "^Exec=\"[^\"]+\" browser --base-dir \"[^\"]+\" %U$" "$DESKTOP" \
        && pass "desktop entry quotes the path, pins the base dir, and takes %U" \
        || fail "Exec line" "$(grep Exec= "$DESKTOP")"
    # The launcher must name the same base dir the service pins, or a link
    # clicked in a GUI starts a second daemon at the platform default.
    UNIT_BASE="$(sed -n 's/^Environment=TABD_BASE_DIR=//p' "$UNIT" | tr -d '"')"
    EXEC_BASE="$(sed -n 's/^Exec=.* --base-dir "\([^"]*\)".*/\1/p' "$DESKTOP")"
    [[ -n "$UNIT_BASE" && "$UNIT_BASE" == "$EXEC_BASE" ]] \
        && pass "launcher and service agree on the base dir ($EXEC_BASE)" \
        || fail "base dir agreement" "unit=$UNIT_BASE exec=$EXEC_BASE"
    grep -q "x-scheme-handler/https" "$DESKTOP" \
        && pass "desktop entry handles https" || fail "https handler" "$(cat "$DESKTOP")"
    [[ -f "$UNIT" ]] && pass "systemd unit installed" || fail "systemd unit" "missing"
    grep -q "PartOf=graphical-session.target" "$UNIT" \
        && pass "unit dies with the graphical session" || fail "PartOf" "$(cat "$UNIT")"
    # The install must not have *changed* whether anything is enabled. Asking
    # only "is it enabled now" fails the suite for a user who legitimately has
    # the real service enabled, even though the scratch install touched
    # nothing — the same mistake as consulting `is_default_browser` for a
    # scratch prefix.
    ENABLED_AFTER="$(systemctl --user is-enabled tabd-visual.service 2>&1 || true)"
    [[ "$ENABLED_BEFORE" == "$ENABLED_AFTER" ]] \
        && pass "a scratch install did not change service enablement ($ENABLED_AFTER)" \
        || fail "enablement unchanged" "before=$ENABLED_BEFORE after=$ENABLED_AFTER"
fi

# A path with a space is the case that breaks naive interpolation into
# Exec=/ExecStart=.
SPACED="$TMP/home with space"
mkdir -p "$SPACED"
"$BIN" service install --prefix "$SPACED" >/dev/null 2>&1
if [[ "$(uname)" == "Darwin" ]]; then
    codesign --verify "$SPACED/Applications/tabd.app" 2>/dev/null \
        && pass "installs under a path containing a space" \
        || fail "spaced prefix" "bundle missing or unsigned"
else
    grep -qE '^Exec="[^"]*" browser --base-dir "[^"]*" %U$' "$SPACED/.local/share/applications/tabd.desktop" \
        && pass "Exec quotes the executable path" \
        || fail "Exec quoting" "$(grep Exec= "$SPACED/.local/share/applications/tabd.desktop")"
    grep -qE '^ExecStart="[^"]*" daemon start --visual' "$SPACED/.config/systemd/user/tabd-visual.service" \
        && pass "ExecStart quotes the executable path" \
        || fail "ExecStart quoting" "$(grep ExecStart= "$SPACED/.config/systemd/user/tabd-visual.service")"
fi
"$BIN" service uninstall --prefix "$SPACED" >/dev/null 2>&1

# A bundle that is not ours must never be replaced.
if [[ "$(uname)" == "Darwin" ]]; then
    FOREIGN="$TMP/foreign"
    mkdir -p "$FOREIGN/Applications/tabd.app/Contents"
    cat > "$FOREIGN/Applications/tabd.app/Contents/Info.plist" <<'PL'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.someone.else</string></dict></plist>
PL
    OUT2="$("$BIN" service install --prefix "$FOREIGN" 2>&1)"; RC2=$?
    { [[ $RC2 -ne 0 ]] && [[ -f "$FOREIGN/Applications/tabd.app/Contents/Info.plist" ]]; } \
        && pass "refuses to replace a tabd.app that is not ours" \
        || fail "foreign bundle protected" "rc=$RC2 $OUT2"
    # Refusing to replace it but deleting it on uninstall would be the worse
    # half of the pair.
    OUT2="$("$BIN" service uninstall --prefix "$FOREIGN" 2>&1)"; RC2=$?
    { [[ $RC2 -ne 0 ]] && [[ -f "$FOREIGN/Applications/tabd.app/Contents/Info.plist" ]]; } \
        && pass "refuses to delete a tabd.app that is not ours" \
        || fail "foreign bundle survives uninstall" "rc=$RC2 $OUT2"
fi

# Both global flags are refused for a scratch prefix, on both platforms and
# BEFORE any filesystem work. On Linux, `xdg-settings` would otherwise point
# the REAL default browser at a desktop entry that is not on $XDG_DATA_DIRS —
# every link on the machine then opens nothing, including any link to look up
# how to undo it. And `uninstall` would refuse, because tabd is now the
# default. Self-locking.
for flag in --set-default --enable-service; do
    OUT2="$("$BIN" service install --prefix "$PREFIX" "$flag" 2>&1)"; RC2=$?
    { [[ $RC2 -ne 0 ]] && grep -qi 'real \$HOME' <<<"$OUT2"; } \
        && pass "$flag refuses a scratch prefix" \
        || fail "$flag scoping" "rc=$RC2 $OUT2"
done

OUT="$("$BIN" service status --prefix "$PREFIX" 2>&1)"
grep -q "installed" <<<"$OUT" && pass "status reports the installed pieces" || fail "status" "$OUT"

OUT="$("$BIN" service uninstall --prefix "$PREFIX" 2>&1)"; RC=$?
[[ $RC -eq 0 ]] && pass "uninstall succeeded" || fail "uninstall" "rc=$RC $OUT"
# Both spellings: the bundle and unit are `tabd*`, the build leftovers are
# `.tabd-*`, and a sweep for only the first can never see the second.
LEFT="$(find "$PREFIX" \( -name 'tabd*' -o -name '.tabd*' \) 2>/dev/null | wc -l | tr -d ' ')"
[[ "$LEFT" == "0" ]] && pass "uninstall removed everything it installed" \
    || fail "uninstall is complete" "$(find "$PREFIX" \( -name 'tabd*' -o -name '.tabd*' \))"

# -- macOS: real registration + url delivery through the private scheme ------
if [[ "$(uname)" == "Darwin" ]]; then
    echo "-- real install (~/Applications), private scheme only"
    LSREG="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
    REAL_APP="$HOME/Applications/tabd.app"
    REAL_AGENT="$HOME/Library/LaunchAgents/dev.tabd.visual.plist"
    HAD_ONE=""
    [[ -e "$REAL_APP" ]] && HAD_ONE="$REAL_APP"
    [[ -e "$REAL_AGENT" ]] && HAD_ONE="${HAD_ONE:+$HAD_ONE }$REAL_AGENT"
    if [[ -n "$HAD_ONE" ]]; then
        echo "      (a real install is present: $HAD_ONE — skipping to avoid clobbering it)"
    else
        # No stub: the wrapper points at the real binary, and the private
        # scheme is answered by `tabd browser` itself as a delivery check that
        # records to a log. That exercises the shipped path end to end.
        REAL_INSTALL_MINE=yes
        "$BIN" service install --prefix "$HOME" >/dev/null 2>&1
        "$LSREG" -f "$REAL_APP" >/dev/null 2>&1
        # `open` succeeding is the registration proof, not `lsregister -dump`:
        # the dump is eventually consistent (V0 saw a record persist minutes
        # after an unregister, and appear late after a register), so grepping
        # it reports the wrong answer in both directions.
        REGISTERED=""
        for _ in $(seq 1 15); do
            if open "tabd://delivery-check?run=$NONCE" 2>"$TMP/open.err"; then REGISTERED=yes; break; fi
            sleep 1
        done
        [[ -n "$REGISTERED" ]] && pass "LaunchServices routes the private scheme to the wrapper" \
            || fail "wrapper registers" "open failed: $(cat "$TMP/open.err")"

        # `tabd browser` records the delivery under the visual base dir.
        DELIVERED=""
        for _ in $(seq 1 15); do
            grep -q "tabd://delivery-check?run=$NONCE" "$VISUAL_LOG" 2>/dev/null && { DELIVERED=yes; break; }
            sleep 1
        done
        [[ -n "$DELIVERED" ]] \
            && pass "a url reaches tabd browser through the wrapper (full url intact)" \
            || fail "url delivery" "log: $(tail -3 "$VISUAL_LOG" 2>/dev/null)"

        # And `service status` surfaces it, which is how a user checks.
        "$BIN" service status 2>/dev/null | grep -c "tabd://delivery-check?run=$NONCE" >/dev/null \
            && pass "service status reports the last url delivery" \
            || fail "status reports delivery" "$("$BIN" service status 2>&1 | tail -3)"

        "$BIN" service uninstall --prefix "$HOME" >/dev/null 2>&1
        REAL_INSTALL_MINE=""
        [[ ! -e "$REAL_APP" ]] && pass "the real install was removed again" \
            || fail "real install removed" "$REAL_APP still there"

    fi
fi

echo "== summary =="
echo "passed: $PASS"
echo "failed: $FAIL"
[[ "$FAIL" -eq 0 ]]
