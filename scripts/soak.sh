#!/usr/bin/env bash
# soak.sh — long-running stress test for the ai-browser daemon.
# Drives a steady mix of navigate / get-text / eval / screenshot operations
# from N concurrent clients while polling daemon.health for RSS,
# restartAttempt, and inflight. CSV logged to soak-<timestamp>.csv.
#
# Usage:
#   scripts/soak.sh [--duration 6h] [--workers 2] [--out soak.csv]
#
# Examples:
#   scripts/soak.sh --duration 10m    # quick smoke
#   scripts/soak.sh --duration 6h     # the recommended pre-release gate
#
# Exit code 0 if no daemon crash or persistent restart loop was observed.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ROOT_DIR}/bin/ai-browser.js"
NODE="${NODE:-node}"

DURATION="1h"
WORKERS=2
OUT="${ROOT_DIR}/soak-$(date +%Y%m%d-%H%M%S).csv"
POLL_SEC=15

while [[ $# -gt 0 ]]; do
  case "$1" in
    --duration) DURATION="$2"; shift 2 ;;
    --workers) WORKERS="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --poll) POLL_SEC="$2"; shift 2 ;;
    -h|--help)
      grep -E "^# " "$0" | sed 's/^# //; s/^#//'
      exit 0
      ;;
    *) echo "Unknown arg: $1" >&2; exit 2 ;;
  esac
done

# Convert duration like 10m / 2h / 30s into seconds.
to_seconds() {
  local s="$1"
  case "$s" in
    *s) echo "${s%s}" ;;
    *m) echo $(( ${s%m} * 60 )) ;;
    *h) echo $(( ${s%h} * 3600 )) ;;
    *)  echo "$s" ;;
  esac
}
DURATION_SEC=$(to_seconds "$DURATION")
DEADLINE=$(( $(date +%s) + DURATION_SEC ))

# Make sure the daemon is up. The warmup call is timeout-bounded so a
# pre-existing hung daemon does not block the script before it can run
# the verdict gates that are supposed to catch exactly that condition.
if ! timeout 30 "${NODE}" "${BIN}" navigate about:blank >/dev/null 2>&1; then
  echo "[soak] FAIL — daemon warmup timed out or failed" >&2
  exit 1
fi

echo "ts,uptimeMs,inflight,totalRequests,chromiumRssMB,restartAttempt,restarting,lastErrorMsg" > "${OUT}"

# CSV poller — runs daemon.health every $POLL_SEC. Requires `jq` for clean
# JSON parsing; jq is part of the standard install on most distros and is
# already in the operations.md prereqs.
poll_health() {
  if ! command -v jq >/dev/null 2>&1; then
    echo "[soak] jq not found in PATH — please install jq" >&2
    return 1
  fi
  while [[ $(date +%s) -lt ${DEADLINE} ]]; do
    local raw ts
    # A hung daemon (accepts the socket but never replies) would block the
    # CLI forever. Wrap every health call in `timeout` so a stuck daemon
    # surfaces as an empty/missing sample and trips the >25% gate.
    raw="$(timeout 5 "${NODE}" "${BIN}" daemon health 2>/dev/null || echo '{}')"
    ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    # `daemon health` emits multi-line pretty JSON; jq -c folds it to one
    # line so we can field-extract reliably.
    local one
    one="$(echo "${raw}" | jq -c '.' 2>/dev/null || echo '{}')"
    local up inflight total rss attempt restarting err rss_mb
    up=$(echo "${one}" | jq -r '.uptimeMs // ""')
    inflight=$(echo "${one}" | jq -r '.inflight // ""')
    total=$(echo "${one}" | jq -r '.totalRequests // ""')
    rss=$(echo "${one}" | jq -r '.driver.chromiumRssBytes // 0')
    attempt=$(echo "${one}" | jq -r '.driver.restartAttempt // ""')
    restarting=$(echo "${one}" | jq -r '.driver.restarting // ""')
    err=$(echo "${one}" | jq -r '.lastError.message // ""' | tr ',' ';')
    rss_mb=$(( ${rss:-0} / 1024 / 1024 ))
    echo "${ts},${up},${inflight},${total},${rss_mb},${attempt},${restarting},${err}" >> "${OUT}"
    sleep "${POLL_SEC}"
  done
}

# Per-worker counters. Each worker maintains running ok/fail counts in
# memory and flushes them to a sidecar file at the end of every action
# loop. The parent reads them once, after all workers have exited, so the
# per-loop flush is sufficient — atomicity matters only against process
# kill, which the verdict step also tolerates (missing file → 0/0).
WORKER_STATS_DIR="$(mktemp -d -t ai-browser-soak-XXXX)"

worker_loop() {
  local i="$1"
  local url1="https://example.com"
  local url2="https://example.org"
  local ok=0 fail=0
  local stats_file="${WORKER_STATS_DIR}/worker-${i}.stats"
  while [[ $(date +%s) -lt ${DEADLINE} ]]; do
    for action in \
      "navigate ${url1}" \
      "get-text --selector h1" \
      "eval 1+${i} --json" \
      "navigate ${url2}" \
      "screenshot --out /tmp/soak-worker-${i}.png" \
    ; do
      # 30s ceiling per action — fronts a hung daemon as a fail rather
      # than a hang and lets the verdict step run on schedule.
      if timeout 30 "${NODE}" "${BIN}" $action >/dev/null 2>&1; then
        ok=$((ok+1))
      else
        fail=$((fail+1))
      fi
    done
    printf "%s %s\n" "${ok}" "${fail}" > "${stats_file}"
    sleep 1
  done
}

echo "[soak] duration=${DURATION} (${DURATION_SEC}s) workers=${WORKERS} out=${OUT}"

# Spawn poller + workers in background; wait for all to finish.
poll_health &
POLL_PID=$!
WORKER_PIDS=()
for ((i=1; i<=WORKERS; i++)); do
  worker_loop "$i" &
  WORKER_PIDS+=($!)
done

wait "${POLL_PID}" "${WORKER_PIDS[@]}" 2>/dev/null || true

# Verdict has three independent gates — soak fails if ANY trips:
#   1. Restart loop: any row with restartAttempt > 2
#   2. Daemon unresponsive: >25% of poll samples have empty uptimeMs
#      (daemon health failed during the soak)
#   3. Worker error rate: across all workers, fails/(ok+fails) > 25%
#      (the daemon was up but actions were failing)
bad=0

# Gate 1: restart loop
if awk -F, 'NR>1 && $6+0 > 2 { found=1 } END { exit found?1:0 }' "${OUT}"; then
  :
else
  echo "[soak] FAIL — restartAttempt > 2 observed (restart loop)" >&2
  bad=1
fi

# Gate 2: daemon unresponsive ratio. Zero samples means the poller never
# wrote anything (e.g. jq missing, daemon never came up) — that is also a
# fail because the gate cannot be evaluated otherwise.
total_samples=$(awk -F, 'NR>1 {n++} END {print n+0}' "${OUT}")
empty_samples=$(awk -F, 'NR>1 && $2 == "" {n++} END {print n+0}' "${OUT}")
if [[ ${total_samples} -eq 0 ]]; then
  echo "[soak] FAIL — no daemon.health samples were recorded (poller crashed?)" >&2
  bad=1
else
  # ratio = empty/total >= 25%? use awk for floating-point compare
  if awk -v e="${empty_samples}" -v t="${total_samples}" \
       'BEGIN { exit !(e * 4 > t) }'; then
    echo "[soak] FAIL — daemon.health unresponsive in ${empty_samples}/${total_samples} samples (>25%)" >&2
    bad=1
  fi
fi

# Gate 3: worker error rate
total_ok=0
total_fail=0
for f in "${WORKER_STATS_DIR}"/worker-*.stats; do
  [[ -e "$f" ]] || continue
  read -r w_ok w_fail < "$f"
  total_ok=$(( total_ok + w_ok ))
  total_fail=$(( total_fail + w_fail ))
done
total_actions=$(( total_ok + total_fail ))
if [[ ${total_actions} -eq 0 ]]; then
  echo "[soak] FAIL — workers issued zero actions (daemon never accepted work)" >&2
  bad=1
else
  if awk -v f="${total_fail}" -v t="${total_actions}" \
       'BEGIN { exit !(f * 4 > t) }'; then
    echo "[soak] FAIL — worker error rate ${total_fail}/${total_actions} (>25%)" >&2
    bad=1
  fi
fi

# Cleanup worker stats dir
rm -rf "${WORKER_STATS_DIR}"

if [[ ${bad} -eq 0 ]]; then
  echo "[soak] PASS — ${total_ok}/${total_actions} actions ok, ${total_samples} health samples, no restart loop. CSV: ${OUT}"
  exit 0
fi
echo "[soak] FAIL — see CSV: ${OUT}" >&2
exit 1
