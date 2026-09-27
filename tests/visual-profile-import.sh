#!/usr/bin/env bash
# visual-profile-import.sh — V1 WU-D end-to-end for `tabd profile import`.
#
# Runs against a SYNTHETIC profile tree, never the human's real one: the whole
# point of this command is that it must not endanger a real profile, and a test
# that copies 9 GiB of live cookies to prove it would be its own hazard.
#
# Asserts what the design promises: the original is never modified, disposable
# caches are dropped while real storage is kept, symlinks are refused rather
# than copied, an open profile is refused, a browser opening the source mid-copy
# aborts it, and publishing is one atomic rename onto an empty destination.

set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ROOT_DIR}/crates/tabd/target/release/tabd"
[[ -x "$BIN" ]] || { echo "Missing tabd binary. cargo build --release --manifest-path crates/tabd/Cargo.toml" >&2; exit 2; }

PASS=0; FAIL=0
pass() { echo "PASS  $1"; PASS=$((PASS+1)); }
fail() { echo "FAIL  $1"; echo "      $2"; FAIL=$((FAIL+1)); }

TMP="$(mktemp -d -t tabd-profile.XXXX)"
trap 'rm -rf "$TMP"' EXIT

SRC="$TMP/real"
DST="$TMP/tabd/profile"
STAGING="$TMP/tabd/profile.staging"

# A browser that is never running, so the quiescence checks pass.
mkdir -p "$TMP/bin"
printf '#!/bin/sh\nexit 0\n' > "$TMP/bin/never-running-browser"
chmod +x "$TMP/bin/never-running-browser"
# tabd records the *canonical* executable and prints canonical paths, and on
# macOS $TMPDIR lives under /var -> /private/var. Compare like for like.
FAKE_BROWSER="$(cd "$TMP/bin" && pwd -P)/never-running-browser"
export BROWSER_EXECUTABLE="$FAKE_BROWSER"

build_source() {
    rm -rf "$SRC"
    mkdir -p "$SRC/Default/Cache" \
             "$SRC/Default/Code Cache/js" \
             "$SRC/Default/GPUCache" \
             "$SRC/Default/Service Worker/CacheStorage" \
             "$SRC/Default/Service Worker/ScriptCache" \
             "$SRC/Default/IndexedDB" \
             "$SRC/Default/Local Storage/leveldb" \
             "$SRC/Default/Extensions/abc"
    echo real-cookies    > "$SRC/Default/Cookies"
    echo real-logins     > "$SRC/Default/Login Data"
    echo real-history    > "$SRC/Default/History"
    echo '{"session":{}}' > "$SRC/Default/Preferences"
    echo local-state     > "$SRC/Local State"
    echo site-data       > "$SRC/Default/Service Worker/CacheStorage/entry"
    echo sw-script       > "$SRC/Default/Service Worker/ScriptCache/script"
    echo idb             > "$SRC/Default/IndexedDB/db"
    echo ls              > "$SRC/Default/Local Storage/leveldb/000001.log"
    echo manifest        > "$SRC/Default/Extensions/abc/manifest.json"
    echo junk            > "$SRC/Default/Cache/junk"
    echo junk            > "$SRC/Default/Code Cache/js/junk"
    echo junk            > "$SRC/Default/GPUCache/junk"
}

profile_cmd() { "$BIN" profile "$1" --from "$SRC" --to "$DST" 2>&1; }

# Filenames here contain spaces ("Login Data", "Local State"), so the checksum
# must not word-split them.
checksum_tree() { (cd "$1" && find . -type f -print0 | sort -z | xargs -0 shasum | shasum); }

echo "== visual-profile-import =="
build_source
SRC_CANONICAL="$(cd "$SRC" && pwd -P)"
SRC_BEFORE="$(checksum_tree "$SRC")"

# 1. An open profile is refused. SingletonLock points at <host>-<pid> and
#    usually does not resolve, so a naive exists() check would miss it.
ln -s "somehost-99999" "$SRC/SingletonLock"
OUT="$(profile_cmd import)"; RC=$?
{ [[ $RC -ne 0 ]] && grep -q "SingletonLock" <<<"$OUT"; } \
    && pass "refuses a profile that is open in a browser" || fail "refuses an open profile" "rc=$RC $OUT"
rm "$SRC/SingletonLock"

# 2. A symlink is refused, not copied: Chromium follows a copied link during
#    the verification launch, straight back into the original.
ln -s /etc/passwd "$SRC/Default/sneaky"
OUT="$(profile_cmd import)"; RC=$?
{ [[ $RC -ne 0 ]] && grep -q "Default/sneaky" <<<"$OUT"; } \
    && pass "refuses a symlink inside the profile" || fail "refuses a symlink" "rc=$RC $OUT"
[[ ! -e "$STAGING" ]] && pass "no staging left behind by a refused import" \
    || fail "no staging after refusal" "$STAGING exists"
rm "$SRC/Default/sneaky"

# 3. The happy path.
OUT="$(profile_cmd import)"; RC=$?
[[ $RC -eq 0 ]] && pass "import staged the profile" || fail "import" "rc=$RC $OUT"

for keep in "Default/Cookies" "Default/Login Data" "Local State" \
            "Default/Service Worker/CacheStorage/entry" \
            "Default/Service Worker/ScriptCache/script" \
            "Default/IndexedDB/db" "Default/Local Storage/leveldb/000001.log" \
            "Default/Extensions/abc/manifest.json"; do
    [[ -f "$STAGING/$keep" ]] || { fail "kept $keep" "missing from staging"; continue; }
done
pass "kept cookies, logins, service-worker data, IndexedDB, storage and extensions"

for drop in "Default/Cache" "Default/Code Cache" "Default/GPUCache"; do
    [[ -e "$STAGING/$drop" ]] && fail "dropped $drop" "present in staging"
done
pass "dropped the disposable caches"

# The distinction the name list exists for.
{ [[ -f "$STAGING/Default/Service Worker/CacheStorage/entry" ]] \
  && [[ ! -e "$STAGING/Default/Cache" ]]; } \
    && pass "CacheStorage kept while Cache dropped (not a *Cache* glob)" \
    || fail "CacheStorage vs Cache" "exclusion is too broad or too narrow"

# 4. The original is byte-for-byte untouched. This is the promise that makes
#    a failed import cost nothing.
SRC_AFTER="$(checksum_tree "$SRC")"
[[ "$SRC_BEFORE" == "$SRC_AFTER" ]] && pass "the original profile is unmodified" \
    || fail "original unmodified" "checksums differ"

# 5. A second import cannot interleave into the same staging tree.
OUT="$(profile_cmd import)"; RC=$?
[[ $RC -ne 0 ]] && pass "a second import refuses an existing staging tree" \
    || fail "second import refused" "rc=$RC $OUT"

# 6. Publish.
OUT="$(profile_cmd cutover)"; RC=$?
[[ $RC -eq 0 ]] && pass "cutover published the staged copy" || fail "cutover" "rc=$RC $OUT"
[[ -f "$DST/Default/Cookies" && ! -e "$STAGING" ]] \
    && pass "the rename moved staging onto the destination" || fail "rename" "dst/staging wrong"
[[ "$(cat "$DST.browser" 2>/dev/null)" == "$FAKE_BROWSER" ]] \
    && pass "the published profile is bound to its browser" \
    || fail "browser binding" "$(cat "$DST.browser" 2>/dev/null)"
grep -q "untouched at $SRC_CANONICAL" <<<"$OUT" \
    && pass "cutover prints the rollback path" || fail "rollback instructions" "$OUT"

# 7. Publishing over an existing profile is refused.
build_source
OUT="$(profile_cmd import)"; RC=$?
[[ $RC -ne 0 ]] && grep -qi "already has a profile" <<<"$OUT" \
    && pass "import refuses a non-empty destination" || fail "non-empty destination" "rc=$RC $OUT"

# 8. discard removes exactly the staging tree.
rm -rf "$DST" "$DST.browser"
profile_cmd import >/dev/null
[[ -d "$STAGING" ]] || fail "staged for discard" "no staging"
OUT="$(profile_cmd discard)"; RC=$?
{ [[ $RC -eq 0 ]] && [[ ! -e "$STAGING" ]] && [[ -d "$SRC" ]]; } \
    && pass "discard removes staging and leaves the original" || fail "discard" "rc=$RC $OUT"

# 9. Overlapping paths are refused before anything is touched.
OUT="$("$BIN" profile import --from "$SRC" --to "$SRC/nested" 2>&1)"; RC=$?
{ [[ $RC -ne 0 ]] && grep -qi "overlap" <<<"$OUT"; } \
    && pass "refuses a destination inside the source" || fail "overlap refused" "rc=$RC $OUT"

# 9b. The source being used *between* import and cutover must block the
#     publish: the staged copy is internally consistent but stale, and
#     publishing it would silently throw away whatever the human just did.
rm -rf "$DST" "$DST.browser" "$DST.import" "$STAGING"
build_source
profile_cmd import >/dev/null 2>&1
# Deliberately Bookmarks, not one of the seven databases an earlier version
# tracked: bookmarks, IndexedDB, extension storage and a second profile were
# all invisible to that list, so a stale copy of them published silently.
echo "a-bookmark-added-after-import" > "$SRC/Default/Bookmarks"
OUT="$(profile_cmd cutover)"; RC=$?
{ [[ $RC -ne 0 ]] && grep -qi "changed since it was imported" <<<"$OUT"; } \
    && pass "cutover refuses a stale copy, including changes outside the main databases" \
    || fail "stale copy refused" "rc=$RC $OUT"

# 9b2. Staging is bound to the importing browser at import time, so a
#      verification launch cannot open it with a different one and rewrite it
#      before cutover ever gets a chance to notice.
rm -rf "$DST" "$DST.browser" "$DST.import" "$STAGING" "$STAGING.browser"
build_source
profile_cmd import >/dev/null 2>&1
[[ "$(cat "$STAGING.browser" 2>/dev/null)" == "$FAKE_BROWSER" ]] \
    && pass "staging is bound to the importing browser before verification" \
    || fail "staging bound at import" "$(cat "$STAGING.browser" 2>/dev/null)"
OUT="$(profile_cmd import 2>&1; true)"
profile_cmd discard >/dev/null 2>&1

# 9b3. The printed verification command pins the browser, so pasting it cannot
#      discover a different one.
rm -rf "$DST" "$DST.browser" "$DST.import" "$STAGING" "$STAGING.browser"
build_source
OUT="$(profile_cmd import 2>&1)"
grep -q "BROWSER_EXECUTABLE='$FAKE_BROWSER'" <<<"$OUT" \
    && pass "the verification command pins the importing browser" \
    || fail "verification command pins the browser" "$OUT"
profile_cmd discard >/dev/null 2>&1

# 9c. Cutover binds the browser that IMPORTED the data, not whatever
#     $BROWSER_EXECUTABLE says at cutover time. Opening a Chromium profile
#     with a different browser rewrites it.
rm -rf "$DST" "$DST.browser" "$DST.import" "$STAGING"
build_source
profile_cmd import >/dev/null 2>&1
printf '#!/bin/sh\nexit 0\n' > "$TMP/bin/other-browser"
chmod +x "$TMP/bin/other-browser"
OTHER="$(cd "$TMP/bin" && pwd -P)/other-browser"
OUT="$(BROWSER_EXECUTABLE="$OTHER" "$BIN" profile cutover --from "$SRC" --to "$DST" 2>&1)"; RC=$?
if [[ $RC -eq 0 && "$(cat "$DST.browser")" == "$FAKE_BROWSER" ]]; then
    pass "cutover binds the importing browser, not the one set at cutover time"
else
    fail "binding uses the importing browser" "rc=$RC binding=$(cat "$DST.browser" 2>/dev/null)"
fi

# 9d. A staging tree tabd did not create has no import record, so it cannot be
#     verified as a copy of anything.
rm -rf "$DST" "$DST.browser" "$DST.import" "$STAGING"
mkdir -p "$STAGING/Default"
echo forged > "$STAGING/Default/Cookies"
OUT="$(profile_cmd cutover)"; RC=$?
{ [[ $RC -ne 0 ]] && grep -qi "no import record" <<<"$OUT"; } \
    && pass "cutover refuses a staging tree it has no record of" \
    || fail "unrecorded staging refused" "rc=$RC $OUT"
rm -rf "$STAGING"

# 9e. A staging *symlink* must never be published: the rename would publish an
#     alias to the original rather than an independent copy.
rm -rf "$DST" "$DST.browser" "$DST.import"
ln -s "$SRC" "$STAGING"
OUT="$(profile_cmd cutover)"; RC=$?
{ [[ $RC -ne 0 ]] && grep -qi "not a directory" <<<"$OUT"; } \
    && pass "cutover refuses a staging symlink" || fail "staging symlink refused" "rc=$RC $OUT"
[[ -d "$SRC" ]] && pass "the original survived the symlink attempt" || fail "original survived" "gone"
rm -f "$STAGING"

# 9f. Staging is owner-only while it fills with cookies and passwords.
rm -rf "$DST" "$DST.browser" "$DST.import"
chmod 700 "$SRC"
build_source 2>/dev/null || true
chmod 700 "$SRC"
profile_cmd import >/dev/null 2>&1
# GNU coreutils may shadow BSD stat on macOS (homebrew), and there `-f` means
# --file-system rather than a format string. Try GNU first, then BSD.
MODE="$(stat -c '%a' "$STAGING" 2>/dev/null || stat -f '%Lp' "$STAGING" 2>/dev/null)"
MODE="${MODE%%$'\n'*}"
[[ "$MODE" == "700" ]] && pass "staging root is owner-only ($MODE)" \
    || fail "staging root permissions" "mode=$MODE"
profile_cmd discard >/dev/null 2>&1

# 9g. Resolving paths must not create anything inside the source.
BEFORE_ENTRIES="$(ls -A "$SRC" | wc -l | tr -d ' ')"
"$BIN" profile import --from "$SRC" --to "$SRC/deep/nested/profile" >/dev/null 2>&1
AFTER_ENTRIES="$(ls -A "$SRC" | wc -l | tr -d ' ')"
{ [[ "$BEFORE_ENTRIES" == "$AFTER_ENTRIES" ]] && [[ ! -e "$SRC/deep" ]]; } \
    && pass "a rejected --to creates nothing inside the source" \
    || fail "rejected --to is non-mutating" "$SRC/deep exists or entry count changed"

# 9h. The verification base dir is validated too. The printed instructions
#     start a daemon there, and a daemon chmods its base dir and writes a
#     socket and pid file into it — so landing it on the source would mean
#     following our own advice wrote into the original.
VERIFY_COLLIDE="$TMP/collide"
mkdir -p "$VERIFY_COLLIDE/profile.staging.verify/Default"
echo c > "$VERIFY_COLLIDE/profile.staging.verify/Default/Cookies"
OUT="$("$BIN" profile import --from "$VERIFY_COLLIDE/profile.staging.verify" \
        --to "$VERIFY_COLLIDE/profile" 2>&1)"; RC=$?
{ [[ $RC -ne 0 ]] && grep -qi "verification directory" <<<"$OUT"; } \
    && pass "refuses a layout where the verification dir is the source" \
    || fail "verification dir overlap refused" "rc=$RC $OUT"

# 9i. A `<staging>.lock` symlink must not make us create its target — that
#     target can be inside the original profile.
rm -rf "$DST" "$DST.browser" "$DST.import" "$STAGING" "$STAGING.browser" "$STAGING.lock"
build_source
ln -s "$SRC/Default/created-by-following-a-symlink" "$STAGING.lock"
OUT="$(profile_cmd import 2>&1)"; RC=$?
{ [[ $RC -ne 0 ]] && [[ ! -e "$SRC/Default/created-by-following-a-symlink" ]]; } \
    && pass "a lock-file symlink is refused without creating its target" \
    || fail "lock symlink refused" "rc=$RC created=$(ls "$SRC/Default" | tr '\n' ' ')"
rm -f "$STAGING.lock"

# 10. The strong interlock: a browser opening the source *during* the copy must
#     abort it. A before/after comparison would miss a browser whose whole
#     lifetime fell inside the window, so the copy watches for Singleton*
#     throughout. Needs a source big enough that the copy is still running
#     when the marker appears.
#     A real 9 GiB profile takes minutes to copy, but a synthetic one is done
#     in milliseconds on a fast disk (the Linux box did 20k files in under a
#     second), so the race cannot be made deterministic by size. The copy has
#     a test-only per-file delay for exactly this.
BIGSRC="$TMP/slow"
mkdir -p "$BIGSRC/Default/IndexedDB"
echo c > "$BIGSRC/Default/Cookies"
for i in $(seq 1 200); do echo x > "$BIGSRC/Default/IndexedDB/f$i"; done
BIGDST="$TMP/tabd/slow-profile"
( sleep 1; ln -s "somehost-424242" "$BIGSRC/SingletonLock" ) &
RACER=$!
START=$(date +%s)
OUT="$(TABD_PROFILE_TEST_SLOW_COPY_MS=15 "$BIN" profile import --from "$BIGSRC" --to "$BIGDST" 2>&1)"; RC=$?
ELAPSED=$(( $(date +%s) - START ))
wait $RACER 2>/dev/null
if [[ $RC -ne 0 ]] && grep -qi "during the copy\|while it was being copied" <<<"$OUT"; then
    pass "a browser opening the source mid-copy aborts the import (${ELAPSED}s)"
else
    fail "mid-copy abort" "rc=$RC elapsed=${ELAPSED}s out=$OUT"
fi
[[ ! -e "$BIGDST.staging" ]] && pass "an aborted copy leaves no staging tree behind" \
    || fail "aborted copy cleans up" "$BIGDST.staging exists"
rm -f "$BIGSRC/SingletonLock"

echo "== summary =="
echo "passed: $PASS"
echo "failed: $FAIL"
[[ "$FAIL" -eq 0 ]]
