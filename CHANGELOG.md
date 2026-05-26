# @marshallku/ai-browser

## Unreleased

### Major Changes

- **Daemon-only architecture.** MCP server and CLI always attach to a shared daemon — per-session isolated browsers are no longer supported. All clients see the same Chromium / tabs / cookies / secrets. Legacy `AI_BROWSER_MCP_MODE=standalone` env and `--standalone`/`--daemon` MCP flags are accepted but ignored, slated for removal.

### Reliability

- **Crash supervisor.** Chromium process exit and persistent context close trigger automatic restart with exponential backoff (1s/2s/4s, capped at 30s). Three consecutive failures `process.exit(1)` so systemd/launchd can do a fresh boot.
- **SnapshotKeeper.** Per-page URLs are tracked in real time via `framenavigated`; `storageState` (cookies + localStorage) is refreshed every 5s in memory. Crash restart replays both into the new context. Persistent profiles restore URLs only (state lives in `userDataDir`).
- **RSS monitor.** `BROWSER_MAX_RSS_MB` triggers a graceful restart (close old browser → relaunch with snapshot) when the Chromium process tree exceeds the cap. `BROWSER_RSS_POLL_MS` tunes the poll interval (default 15s).
- **In-flight drain.** `daemon.shutdown` / SIGTERM stops accepting new non-control requests, lets in-flight actions finish for up to `AI_BROWSER_DRAIN_TIMEOUT_MS` (default 10s), then force-closes the context for a real cancel (not Promise.race fake-cancel).
- **Stable page identity.** Internal page UUIDs ride along queued actions so a tab close mid-queue can no longer redirect a request to a different tab.
- **ActionQueue.** Per-tab serialization with global lock so multiple MCP/CLI clients can drive the daemon concurrently without race on shared browser state.
- **Safe disconnect handling.** Mid-send daemon disconnects surface a clean `request cancelled: daemon connection lost mid-request` error instead of silently replaying a possibly-partially-executed action. The next request transparently reconnects.

### New CLI / API

- `ai-browser daemon health` — JSON snapshot: uptime, accepting, inflight, totalRequests, lastError, driver (chromiumPid, chromiumRssBytes, rssCheckedAt, rssMaxMb, restartAttempt, restarting).
- `ai-browser run-once <subcmd>` — ephemeral isolated daemon for one-off CI commands. Does not collide with the long-running daemon. Refuses to wrap meta subcommands (daemon/run-once/mcp/repl/help).
- `wait_for_url` MCP tool + `wait-url <pattern> --pattern-type exact|glob|regex --timeout ms` CLI subcommand.
- `secret-{put,list,delete}` and `type-secret` CLI subcommands. `secret-put` accepts plaintext only via `--from-env`, `--from-file`, or `--stdin` (never argv).
- `secret_list` MCP tool — metadata only, never plaintext.

### Persistent secret store

- `AI_BROWSER_SECRET_STORE=persistent` enables an AES-256-GCM file vault at `$XDG_CONFIG_HOME/ai-browser/secrets.enc` (mode 0600).
- Master key: `AI_BROWSER_VAULT_KEY` env (PBKDF2-SHA256, 200k iters) takes precedence; otherwise OS keychain (macOS `security`, Linux `secret-tool`) is consulted with auto-create.
- macOS keychain write goes through `security -i` interactive mode so the key never appears in process argv.

### Environment variables (new)

- `AI_BROWSER_BASE_DIR` — override socket/pid base directory (used by `run-once`).
- `AI_BROWSER_DRAIN_TIMEOUT_MS` — shutdown drain budget.
- `BROWSER_MAX_RSS_MB` / `BROWSER_RSS_POLL_MS` — RSS monitor.
- `AI_BROWSER_SECRET_STORE` / `AI_BROWSER_VAULT_KEY` / `AI_BROWSER_SECRETS_FILE` — persistent vault.

### Fixes

- `cookies.set` no longer fails with `"Cookie should have either url or domain"` when both `url` and a default domain/path were inferred — the call now uses url-mode by default and domain-mode when an explicit domain is given.
- `launchServer` is pinned to `127.0.0.1` with a randomized `wsPath` so the Chromium control WebSocket is not reachable from the LAN (default Playwright binds to `0.0.0.0`).

### Docs

- `docs/operations.md` — systemd / launchd templates, health observation, drain semantics, migration notes.

## 0.2.0

### Minor Changes

- 6fba9b0: Rename the package to `@marshallku/ai-browser` and add Changesets-based automated publishing through GitHub Actions.
