# Operations guide

How to run `ai-browser` daemon as a long-lived service on Linux and macOS, plus health observation and migration notes.

## Linux — systemd user unit

Recommended for an always-on daemon for the current user. Put this at `~/.config/systemd/user/ai-browser.service`:

```ini
[Unit]
Description=ai-browser daemon (shared Chromium for CLI + MCP)
After=default.target
# Restart-rate gate: if the daemon fails 3 times within 60s, systemd gives
# up. This matches the daemon's own MAX_RESTART_ATTEMPTS=3 supervisor cap.
StartLimitBurst=3
StartLimitIntervalSec=60

[Service]
Type=simple
ExecStart=%h/.local/bin/ai-browser daemon --foreground
Restart=on-failure
RestartSec=10
# Optional knobs — uncomment + tune as needed:
# Environment=BROWSER_MAX_RSS_MB=1500
# Environment=BROWSER_RSS_POLL_MS=15000
# Environment=AI_BROWSER_DRAIN_TIMEOUT_MS=10000
# Environment=AI_BROWSER_SECRET_STORE=persistent
# For persistent secrets, pass the passphrase via EnvironmentFile (so it
# does not appear in argv / systemd unit). The file should be 0600:
# EnvironmentFile=%h/.config/ai-browser/vault.env  # must define AI_BROWSER_VAULT_KEY=...
# Environment=BROWSER_USER_DATA_DIR=%h/.cache/ai-browser/profile
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
```

Then:

```bash
systemctl --user daemon-reload
systemctl --user enable --now ai-browser
journalctl --user -u ai-browser -f       # follow logs
ai-browser daemon health                 # quick health JSON
```

`Restart=on-failure` + the StartLimit knobs match the daemon's own "die loudly after 3 consecutive failed restarts" policy — if the daemon dies three times in a minute, systemd will stop trying and the operator gets a clear failed state.

## macOS — launchd user agent

Put this at `~/Library/LaunchAgents/dev.marshallku.ai-browser.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.marshallku.ai-browser</string>

  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/ai-browser</string>
    <string>daemon</string>
    <string>--foreground</string>
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
  <string>/tmp/ai-browser.out.log</string>
  <key>StandardErrorPath</key>
  <string>/tmp/ai-browser.err.log</string>

  <key>EnvironmentVariables</key>
  <dict>
    <!-- Tune as needed -->
    <key>BROWSER_MAX_RSS_MB</key>
    <string>1500</string>
  </dict>
</dict>
</plist>
```

Then:

```bash
launchctl load -w ~/Library/LaunchAgents/dev.marshallku.ai-browser.plist
launchctl list | grep ai-browser
ai-browser daemon health
tail -f /tmp/ai-browser.err.log
```

`KeepAlive.SuccessfulExit=false` only restarts on non-zero exit (which is what the daemon emits after 3 failed restart attempts — matches the same intent).

## Observing health

`ai-browser daemon health` returns a JSON snapshot. Sample:

```json
{
  "pid": 12345,
  "uptimeMs": 3600000,
  "ready": true,
  "accepting": true,
  "inflight": 0,
  "totalRequests": 482,
  "lastError": null,
  "driver": {
    "chromiumPid": 12389,
    "chromiumRssBytes": 482344960,
    "rssCheckedAt": 1701337200000,
    "rssMaxMb": 1500,
    "restartAttempt": 0,
    "restarting": false
  }
}
```

Watch for:
- `restartAttempt > 0` — supervisor is dealing with crashes; check journal/log for the root cause
- `chromiumRssBytes / 1024 / 1024` trending up over hours without RSS-triggered restart → consider tightening `BROWSER_MAX_RSS_MB`
- `lastError` populated repeatedly with the same message → likely a real bug, not a flake
- `inflight` stuck at a non-zero value while no clients are issuing requests → indicates a hung Playwright action (file an issue with the relevant action+selector)

## Drain semantics during shutdown

Shutdown order:
1. `accepting=false` immediately on `daemon.shutdown` (or SIGTERM)
2. Listener stays open during drain — `daemon.health` can still be polled
3. In-flight actions get up to `AI_BROWSER_DRAIN_TIMEOUT_MS` (default 10s) to finish
4. If drain times out, `context.close()` is called — Playwright rejects all pending Promises with a connection-closed error. Clients see `request cancelled: daemon connection lost mid-request`.
5. Browser → server → socket close → unlink pid/sock files

The bridge never auto-replays a request that disconnected mid-send because the daemon may have already partially executed it. Long-lived MCP sessions transparently reconnect on the *next* request via `ensureDaemon` (which auto-spawns if needed).

## Pre-release soak test

Before tagging a release (or after any meaningful supervisor change), run a long soak against the daemon to surface slow leaks, restart-storm regressions, and cross-client races that short E2E tests miss.

```bash
# Recommended pre-release gate — 6h with 2 concurrent clients
scripts/soak.sh --duration 6h --workers 2 --out /tmp/soak.csv

# Quick smoke (~1 min) before a longer run
scripts/soak.sh --duration 1m --workers 2 --out /tmp/soak-smoke.csv --poll 5
```

The script spawns N workers that drive a mix of `navigate`, `get-text`, `eval`, and `screenshot` against the local daemon while polling `daemon.health` every 15s (`--poll` overrides). Output CSV columns:

```
ts, uptimeMs, inflight, totalRequests, chromiumRssMB, restartAttempt, restarting, lastErrorMsg
```

Three independent gates must all pass for `exit 0`:

1. No row with `restartAttempt > 2` (no persistent restart loop).
2. ≤25% empty `uptimeMs` samples (daemon stayed responsive to `daemon.health`).
3. ≤25% worker error rate (the daemon stayed *useful*, not just alive).

Zero health samples = FAIL (the poller crashed; verdict cannot be evaluated).

Requires `jq` in `$PATH` (used to parse `daemon.health` JSON).

Look at the CSV afterwards: a stable RSS line with occasional small bumps means the daemon is healthy; a steady upward slope is a slow leak and warrants tightening `BROWSER_MAX_RSS_MB`.

## One-shot CI usage

When a job just needs a clean browser for a single command:

```bash
ai-browser run-once navigate https://example.com
ai-browser run-once screenshot --out /tmp/x.png
```

The ephemeral daemon spawns in an isolated socket path (via `AI_BROWSER_BASE_DIR=<tmp>`), runs the single subcommand, then tears itself down. Any long-running user daemon is unaffected. This is the recommended pattern for CI; do not start/stop the regular daemon between tests.

## Migration notes (≤ v0.2 → v0.3)

- **MCP no longer offers per-session isolated browsers.** Every MCP client and CLI invocation now shares one Chromium via the daemon. If you relied on separate browsers per MCP session, that mode is gone. (See `docs/architecture.md`.)
- `AI_BROWSER_MCP_MODE=standalone|daemon` env var and `--standalone`/`--daemon` CLI flags are accepted but ignored — slated for removal in a future release.
- The `cookies-set` CLI now correctly handles either `--url URL` (Playwright derives domain/path) or `--domain D --path P` (explicit). Previously it sent both, and Playwright rejected the call.
- New env vars: `AI_BROWSER_BASE_DIR`, `AI_BROWSER_DRAIN_TIMEOUT_MS`, `BROWSER_MAX_RSS_MB`, `BROWSER_RSS_POLL_MS`.
- New CLI: `daemon health`, `run-once <subcmd>`.
