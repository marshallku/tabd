# Operations guide

How to run `tabd` as a long-lived service for a user account, watch it stay
healthy, and recover when Chromium dies. Covers Linux (systemd user units),
macOS (launchd user agents), and minimal shell-rc setups for the gap in
between.

For the system surface itself (commands, daemon shape), see
[architecture.md](architecture.md). For install, see
[INSTALL.md](../INSTALL.md).

---

## Two ways to use the daemon

The CLI **auto-spawns** the daemon on its first call, so for one-off SSH
sessions you don't have to do anything — `tabd navigate …` from a cold shell
just works. The daemon will live as long as the parent shell tree.

You want a **service-managed daemon** when:

- the daemon should outlive any single SSH session,
- the daemon should auto-restart if it crashes,
- a cron / CI / automation job needs the daemon to be already running.

That's the rest of this doc.

---

## Linux — systemd user unit (recommended)

Run the daemon as the user. No root, no system unit needed.

`~/.config/systemd/user/tabd.service`:

```ini
[Unit]
Description=tabd — shared Chromium controller (CDP daemon)
After=default.target
# If the daemon process itself dies 5 times within 60s, systemd gives up.
# (Chromium crashes are handled inside the daemon by its own supervisor
# with exp-backoff up to 5 attempts — see architecture.md.)
StartLimitBurst=5
StartLimitIntervalSec=60

[Service]
Type=simple
ExecStart=%h/.local/bin/tabd daemon start
Restart=on-failure
RestartSec=5

# Stop semantics: SIGTERM triggers a graceful drain (see "Drain" below).
# Default 10s drain timeout is enough for most actions.
KillSignal=SIGTERM
TimeoutStopSec=15

# Where to find Chromium. Uncomment if it's not on PATH and not in the
# Playwright cache.
# Environment=BROWSER_EXECUTABLE=/usr/bin/chromium

# Persistent secrets vault. The passphrase MUST come from a file (so it
# doesn't end up in the systemd unit / journal). Create with mode 0600.
# EnvironmentFile=%h/.config/tabd/vault.env
#   The file should contain a single line: TABD_VAULT_KEY=<your-passphrase>

StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
```

Then:

```bash
systemctl --user daemon-reload
systemctl --user enable --now tabd
journalctl --user -u tabd -f         # follow logs
tabd daemon health                   # quick health snapshot
```

To restart cleanly after a tabd upgrade:

```bash
systemctl --user restart tabd
```

### Linger for "after logout" daemons

By default a systemd user instance dies when you log out. If you want the
daemon to keep running across SSH disconnects (so the next session reuses
the same warm Chromium):

```bash
sudo loginctl enable-linger "$USER"    # one-time, persistent
```

---

## macOS — launchd LaunchAgent

`~/Library/LaunchAgents/dev.marshallku.tabd.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.marshallku.tabd</string>

  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/tabd</string>
    <string>daemon</string>
    <string>start</string>
  </array>

  <key>RunAtLoad</key>
  <true/>

  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>

  <key>ThrottleInterval</key>
  <integer>10</integer>

  <key>StandardOutPath</key>
  <string>/tmp/tabd.out.log</string>
  <key>StandardErrorPath</key>
  <string>/tmp/tabd.err.log</string>

  <key>EnvironmentVariables</key>
  <dict>
    <!-- Uncomment as needed -->
    <!-- <key>BROWSER_EXECUTABLE</key>
         <string>/Applications/Chromium.app/Contents/MacOS/Chromium</string> -->
  </dict>
</dict>
</plist>
```

Load it:

```bash
launchctl load -w ~/Library/LaunchAgents/dev.marshallku.tabd.plist
launchctl list | grep tabd
tabd daemon health
tail -f /tmp/tabd.err.log
```

`KeepAlive.SuccessfulExit=false` restarts only on non-zero exit. `tabd
daemon stop` exits cleanly, so it won't loop after a deliberate shutdown.

---

## Shell-rc fallback (no init system)

When you can't or don't want to install a service unit:

```bash
# ~/.bashrc or ~/.zshrc
if ! pgrep -x tabd >/dev/null 2>&1; then
  nohup tabd daemon start >/tmp/tabd.log 2>&1 &
  disown
fi
```

The daemon dies when the user's last login session dies. For SSH-only boxes
where you log in to do something and log out, this is acceptable — the
auto-spawn on the next CLI call will rebuild it.

---

## First-run validation

Whatever you used to start the daemon, sanity check it:

```bash
tabd daemon ping
# {"pid":12345,"ready":true}

tabd daemon health
# {
#   "pid": 12345,
#   "uptimeMs": 3600000,
#   "ready": true,
#   "accepting": true,
#   "inflight": 0,
#   "totalRequests": 482,
#   "lastError": null,
#   "driver": {
#     "chromiumPid": 12389,
#     "chromiumRssBytes": 482344960,
#     "restartAttempts": 0,
#     "restartAttempt": 0,
#     "restarting": false
#   }
# }

tabd navigate https://example.com
tabd get-text --selector h1
# "Example Domain"
```

If `daemon health` shows `"ready": false`, Chromium is still booting (or has
just crashed and the supervisor is rebooting it). Wait ~5 s and re-poll.

---

## Watching the daemon

`tabd daemon health` returns a JSON snapshot — small enough to poll from
shell scripts. The fields that matter:

| Field | What it tells you |
|---|---|
| `ready` | `true` once chromium is attached and CDP is responsive. Goes `false` during a chromium reboot. |
| `accepting` | `false` after `daemon stop` / SIGTERM. Listener stays open during drain. |
| `inflight` | Number of actions currently executing. Stuck non-zero with no clients = hung action. |
| `totalRequests` | Lifetime counter. Useful for sanity ("did anything reach this daemon?"). |
| `lastError` | `null` or `{action, message, at}`. Populated by any failed daemon-side request. |
| `driver.chromiumPid` | Current chromium pid. Changes after every supervisor restart. |
| `driver.chromiumRssBytes` | Most recent RSS read from `/proc`. Watch for unbounded growth. |
| `driver.restartAttempts` | How many times the supervisor has rebooted chromium. Non-zero = something is unhappy. |
| `driver.restarting` | `true` while a restart is in progress. |

A simple polling watchdog:

```bash
while sleep 60; do
  if ! out=$(tabd daemon health 2>/dev/null); then
    echo "$(date -Is) daemon unreachable" >&2
    continue
  fi
  ready=$(printf '%s' "$out" | jq -r '.ready')
  attempts=$(printf '%s' "$out" | jq -r '.driver.restartAttempts // 0')
  echo "$(date -Is) ready=$ready restart_attempts=$attempts"
done
```

(Requires `jq`.)

---

## Shutdown / drain semantics

`tabd daemon stop` (or systemd's SIGTERM) does:

1. `accepting = false` immediately. New requests are rejected with `daemon
   not accepting`.
2. Listener stays open during the drain — `daemon.ping` and `daemon.health`
   still answer, so you can watch the drain progress.
3. In-flight actions get up to **`$TABD_DRAIN_TIMEOUT_MS`** (default 10000)
   to finish.
4. On timeout, the daemon closes the chromium connection and exits; any
   stuck client sees a connection-lost error on its socket.
5. Final teardown: WS close → server socket close → unlink pid + sock files.

If you want a longer drain (e.g. an in-flight `screenshot` of a slow page),
set `Environment=TABD_DRAIN_TIMEOUT_MS=30000` in the systemd unit.

---

## Automated boot for CI / one-shot jobs

For a clean isolated daemon that goes away at the end of a job (so it
doesn't fight with a user's long-running daemon):

```bash
#!/usr/bin/env bash
set -euo pipefail
BASE="$(mktemp -d -t tabd-ci.XXXX)"
export TABD_BASE_DIR="$BASE"
trap 'tabd daemon stop --base-dir "$BASE" >/dev/null 2>&1; rm -rf "$BASE"' EXIT

tabd daemon start --base-dir "$BASE" &
# Wait for ready
for _ in $(seq 1 30); do
  tabd daemon ping --base-dir "$BASE" >/dev/null 2>&1 && break
  sleep 0.5
done

tabd navigate https://example.com
tabd screenshot --out /tmp/example.png
# … rest of the job …
```

`tests/cli-direct-smoke.sh` and `tests/spike-daemon-compat.sh` use the same
pattern.

---

## Troubleshooting

**`daemon not running at … and TABD_NO_AUTO_SPAWN is set`**
Auto-spawn is disabled and there's no daemon. Either unset
`TABD_NO_AUTO_SPAWN` or start the daemon yourself first.

**`no Chromium binary found. Set $BROWSER_EXECUTABLE …`**
See [INSTALL.md § Chromium](../INSTALL.md). The daemon refuses to start
without a Chromium binary. Easiest fix on Linux:
`sudo pacman -S chromium` (Arch) / `sudo apt install chromium` (Debian).

**`/json/version not ready within 60s on port N`**
Chromium started but its DevTools port is not responding. Usually a missing
runtime library — see the `apt install libnss3 …` line in INSTALL.md.
Check `journalctl --user -u tabd` for the actual chromium stderr.

**`vault open failed: invalid passphrase`**
`$TABD_VAULT_KEY` does not match the key that encrypted
`$XDG_CONFIG_HOME/tabd/secrets.enc`. Either fix the env file or, if the
secrets are disposable, `rm` the vault and re-`secret-put` everything.

**`daemon.health` shows `restartAttempts` climbing**
Chromium is crashing in a loop. Look at chromium stderr (in
`journalctl --user -u tabd` on Linux or `/tmp/tabd.err.log` on macOS). The
most common causes:
- A flag chromium no longer recognizes after a version bump.
- RSS pressure killing chromium from outside (cgroup / OOM).
- A bad `--user-data-dir` path (we use a TempDir, so this is rare).

**`daemon health` says `ready: false` for more than ~30 s after start**
Either chromium is still booting (`/json/version` polling) or the supervisor
ran out of restart attempts and is sleeping between rounds. Check
`lastError` — `supervisor.restart` errors point at chromium boot issues.

**Daemon won't shut down (systemctl restart hangs)**
A long-running action is blocking the drain. Either wait, or
`TABD_DRAIN_TIMEOUT_MS` is set too high. systemd's `TimeoutStopSec` is the
hard cutoff — after it, systemd `SIGKILL`s the daemon.
