# AI Browser

SSH-friendly headless browser controller for AI agents (and humans). Single Rust
binary, daemon-shared Chromium session, JSON over Unix domain socket. Replaces the
earlier TypeScript MCP server (retired in phase 3i) with a smaller, faster,
dependency-free CLI surface.

## Highlights

- **Single static Rust binary** (~7 MB). No Node, no Python.
- **One long-running daemon per user.** Multiple CLI calls share the same
  Chromium and its cookies, storage, console history, and tabs.
- **Auto-spawn** — the first CLI call boots the daemon if none is running.
- **Crash-restart supervisor** — if Chromium dies the daemon brings up a fresh
  one within seconds (Linux).
- **AES-256-GCM secrets vault** — `secret-put` / `type-secret` for login
  automation; plaintext never goes on argv.
- **`--json` everywhere** — every action accepts `--json` for scriptable output;
  `--out FILE` decodes binary payloads (e.g. PNG screenshots) to disk.

## Install

See [INSTALL.md](./INSTALL.md). Short version:

```bash
cargo install --path crates/ai-browser   # from source
# or
gh release download v0.x.y --pattern 'ai-browser-linux-x64'
```

## Surface

39 action subcommands + 4 daemon controls.

| Category | Commands |
|---|---|
| **Tabs** | `navigate`, `open-tab`, `close-tab`, `list-tabs`, `activate-tab`, `back`, `forward`, `reload` |
| **DOM** | `get-html`, `get-text`, `query`, `summary` |
| **Interaction** | `click`, `type`, `hover`, `mouse-move`, `scroll`, `press-key`, `select-option`, `check` |
| **Capture** | `screenshot`, `metrics` |
| **Execution** | `eval` |
| **Wait** | `wait-selector`, `wait-url`, `wait-network-idle` |
| **Cookies** | `cookies-get`, `cookies-set`, `cookies-delete` |
| **Storage** | `storage-get`, `storage-set`, `storage-clear` |
| **Monitor** | `console-logs`, `page-errors`, `network-logs` |
| **Secrets** | `secret-put`, `secret-list`, `secret-delete`, `type-secret` |
| **Daemon** | `daemon start`, `daemon stop`, `daemon ping`, `daemon health` |

Every action that targets a specific tab accepts `--tab N` (1-based index).
Defaults to the active tab.

## Quick start

```bash
# 1. Boot daemon (auto-spawn on first action also works).
ai-browser daemon start &

# 2. Drive it.
ai-browser navigate https://example.com
ai-browser get-text --selector h1                    # → "Example Domain"
ai-browser screenshot --out /tmp/example.png
ai-browser daemon health                             # daemon + chromium pids, RSS, restart count

# 3. Multi-tab.
ai-browser open-tab https://news.ycombinator.com     # returns {tabId, targetId, url}
ai-browser list-tabs --json                          # all open tabs with active flag
ai-browser activate-tab --tab 1
ai-browser back

# 4. Monitor what just happened.
ai-browser console-logs --json
ai-browser network-logs --method GET --status 2xx --limit 20

# 5. Login automation (passphrase-mode secrets vault).
export AI_BROWSER_VAULT_KEY="$(pass show ai-browser/vault 2>/dev/null || echo 'change-me')"
echo -n "$GITHUB_PASSWORD" | ai-browser secret-put --label github --stdin
# → {"secretId":"a1b2c3...", "label":"github", "preview":"****", ...}
ai-browser navigate https://github.com/login
ai-browser type     '#login_field' marshallku
ai-browser type-secret '#password' --secret-id a1b2c3...
ai-browser click    '[name=commit]'
ai-browser wait-url 'https://github.com/*' --pattern-type glob

# 6. Stop when done.
ai-browser daemon stop
```

## Architecture

```
        ┌──────────────────┐
ai-browser CLI ──┐
                ├── /tmp/…/daemon.sock ──> ai-browser daemon ──> chromium (CDP/WS)
ai-browser CLI ──┘                                  │
                                                    └── supervise task (Linux)
                                                    └── secrets vault (AES-256-GCM)
```

- **Daemon** owns one Chromium and a `TabRegistry` (targetId → sessionId + per-tab
  ring buffers for console/page-errors/network).
- **Reader task** routes CDP events into the matching `TabState` — no RPC calls
  from inside the reader (would self-deadlock the registry mutex).
- **Supervise task** polls `/proc/{pid}/status` every 2 s; on crash it rebuilds
  the Chromium + CDP client with exponential backoff (5 attempts).
- **CLI dispatcher** auto-spawns the daemon if no socket exists, then routes the
  subcommand to the matching daemon action over UDS.
- **Secrets vault** is a single AES-256-GCM file at
  `$XDG_CONFIG_HOME/ai-browser/secrets.enc`, key derived from
  `$AI_BROWSER_VAULT_KEY` via PBKDF2-SHA256 (200 000 iters). `secret-list`
  never decrypts.

## `--json` and `--out`

Every dispatched subcommand accepts:

- `--json` — emit the daemon response payload as compact JSON instead of the
  default pretty rendering. String results become quoted JSON literals; null
  becomes `null`; objects/arrays serialize compactly.
- `--out FILE` — for actions that return a base64 data URL or
  `{base64,mimeType}` object (`screenshot`), decode the bytes and write the file.
  No stdout payload.

## Development

```bash
# Build
cargo build --release --manifest-path crates/ai-browser/Cargo.toml

# Test
cargo test --bins --manifest-path crates/ai-browser/Cargo.toml         # 120 unit
bash tests/cli-direct-smoke.sh                                          # 4 cases
bash tests/spike-daemon-compat.sh                                       # 39 cases (real Chromium)
```

`crates/ai-browser/src/`:

- `main.rs` — clap router for `daemon ...` + external_subcommand → `cli::run`
- `cli.rs` — argv parser, dispatch table, daemon auto-spawn, render
- `daemon.rs` — UDS server, action handlers, supervisor, vault state
- `cdp.rs` — JSON-RPC over WS, multi-tab registry, event routing
- `browser.rs` — Chromium launch + DevTools port discovery
- `secrets.rs` — AES-GCM + PBKDF2 file vault
- `cmd/` — helper expressions (text/AX/find-all) used by the daemon handlers

## Phase history

`docs/` keeps the design plans from each migration phase
(`rust-chromium-spike-plan.md`, `spike-phase-*-plan.md`) for context. Phase 3
(2026-03~05) migrated the entire TypeScript surface to this Rust binary; the
TypeScript implementation was removed in phase 3i.
