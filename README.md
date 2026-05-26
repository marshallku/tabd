# AI Browser

SSH-friendly headless browser MCP server for AI agents.

Replaces `browser-control`'s WebExtension bridge with direct control. No display, no extension, no interactive session required.

The intended long-term engine is Playwright. The browser engine itself should not be reinvented here; this repo should focus on the AI-facing layer on top of it.

## Features

The MCP surface keeps the `browser-control` shape and adds secret-safe input helpers.

| Category | Tools | http-fetch | CDP |
|----------|-------|:----------:|:---:|
| **Tabs** | list, open, close, navigate, activate, back, forward, reload | partial | full |
| **DOM** | getHtml, getText, querySelector, formValues, accessibilityTree | partial | full |
| **Interaction** | click, type, scroll, pressKey (chords), hover, mouseMove, selectOption, check, annotations | - | full |
| **Capture** | screenshot, computedStyles, elementRect, metrics, annotate, highlight | metrics only | full |
| **Execution** | executeJs | - | full |
| **Wait** | selector, navigation, networkIdle, url (pattern-based) | - | full |
| **Cookies** | get, set, delete | full | full |
| **Storage** | get, set, clear + session save/restore | full | full |
| **Dialog** | setBehavior, getLast | - | full |
| **Monitor** | consoleLogs, pageErrors, networkLogs | - | full |

## Architecture

```
MCP Client (Claude Code, etc.)
    | stdio
AI Browser MCP Server
    | BrowserDriver interface
    |
    +-- FetchBrowserDriver (http-fetch)      -- just fetch() + HTML parsing
    +-- PlaywrightBrowserDriver              -- preferred full browser runtime
    +-- CdpBrowserDriver (chromium-cdp)      -- lower-level fallback/runtime experiment
```

### Preferred Playwright runtime

- `page.goto`, `locator.click`, `locator.fill`, `page.screenshot`
- browser context level cookie/session control
- automatic waiting and more robust selector handling than raw CDP
- easier support for Chromium, Firefox, and WebKit when needed
- extra MCP tools for secrets: `secret_store_put`, `secret_store_delete`, `type_secret`
- bulk secret import: `secret_import_csv`

### AI-specific features to layer on top

- secret handles for passwords and tokens so the model never has to echo raw values back
- redacted logging for `fill`/`type` operations on sensitive fields
- session import/export that can include cookies and selected storage keys
- DOM/CSS inspection helpers that normalize output for model consumption

### CDP runtime specifics

- **Persistent sessions** per tab (attach once, keep Runtime/Page/Network/DOM enabled)
- **Event capture** — console logs, JS errors, dialog events, network activity tracked per tab
- **Key synthesis** via `Input.dispatchKeyEvent`
- **Accessibility tree** built from DOM walk with ref-based annotation system

## Two ways to use it

**As an MCP server (for AI agents)** — default. Claude Code / Codex / etc. invoke `ai-browser` (no args) on stdio and call tools. The MCP server always attaches to (or auto-spawns) the shared daemon, so AI and CLI sessions share one Chromium and one set of tabs/cookies/secrets. Standalone per-MCP-session browsers are no longer offered.

**As a CLI (for humans + scripts)** — a long-lived daemon owns one Chromium; CLI subcommands talk to it over a Unix socket. Subsequent commands are <100ms (no cold start) and share state across CLI invocations (cookies, tabs, network logs).

```bash
ai-browser navigate https://example.com
ai-browser get-text --selector h1
ai-browser eval "document.title" --json
ai-browser network-logs --status 4xx --json
ai-browser screenshot --out /tmp/page.png
ai-browser repl                                # interactive shell
ai-browser daemon status                       # is the browser running?
ai-browser daemon stop                         # quit the browser
```

The daemon auto-spawns on the first CLI command. State persists across CLI calls.

The MCP server and the CLI already share one Chromium: both attach to the same daemon socket, which is auto-spawned on first use. No flag is required.

```jsonc
{
  "mcpServers": {
    "browser": {
      "command": "ai-browser"
    }
  }
}
```

(The legacy `--daemon` / `--standalone` MCP flags are accepted but ignored for one release.)

Run `ai-browser help` for the full subcommand list. See [CLI section](#cli) below.

## Setup

### Quick install (recommended)

After cloning, one script handles npm install → Playwright Chromium → build → MCP config patch:

```bash
git clone https://github.com/marshallku/ai-browser && cd ai-browser
./scripts/install.sh                     # registers in both Claude Code (~/.claude.json)
                                         # and Codex CLI (~/.codex/config.toml) if present
./scripts/install.sh --target claude     # only Claude Code
./scripts/install.sh --target codex      # only Codex CLI
./scripts/install.sh --dry-run           # print plan, no writes
```

Useful flags:

| Flag | Default | Description |
|------|---------|-------------|
| `--target` | `both` | `claude`, `codex`, or `both` |
| `--runtime` | `playwright` | `playwright` or `chromium-cdp` |
| `--headless` | `1` | `0` to show the window |
| `--executable` | auto | Override Chromium binary path |
| `--user-data-dir` | (temp) | Persistent profile directory |
| `--name` | `ai-browser` | MCP server key in the client config |
| `--skip-install` / `--skip-build` | — | Skip step 1 / step 3 |
| `--dry-run` | — | Print what would change without writing |

The script is idempotent — re-running it updates the existing entry and creates a `.bak.<timestamp>` of the previous config.

After install, restart your client so it re-reads the MCP config.

### Manual install

```bash
npm install && npm run build
```

## Simpler install path

For users, the cleaner path is to publish a prebuilt package and run it with `npx`:

```bash
npx -y @marshallku/ai-browser
```

That avoids both local `npm install` and `npm run build` for consumers. To support that flow, this repo now exposes a package bin entrypoint and runs `npm run build` automatically during `npm pack` / `npm publish` via `prepack`.

Current limitation:

- a raw git checkout still needs `npm install && npm run build`
- `npx @marshallku/ai-browser` only works after this package is published to npm (or another registry)

## Usage

### MCP server config (Claude Code)

```json
{
    "mcpServers": {
        "browser": {
            "command": "npx",
            "args": ["-y", "@marshallku/ai-browser"],
            "env": {
                "BROWSER_RUNTIME": "playwright",
                "BROWSER_NAME": "chromium",
                "BROWSER_HEADLESS": "1",
                "BROWSER_EXECUTABLE": "/home/marshall/.cache/ms-playwright/chromium-1208/chrome-linux64/chrome"
            }
        }
    }
}
```

For a local checkout that has already been built, keep using:
[scripts/run-mcp.sh](/home/marshall/dev/browser/scripts/run-mcp.sh)

## Release

This repo uses Changesets for versioning and npm publishing.

- Add a release note with `npm run changeset`
- Merge to `main`
- GitHub Actions opens or updates a release PR
- Merging that PR publishes `@marshallku/ai-browser` to npm

Required GitHub secret:

- `NPM_TOKEN`: npm automation token with publish access for `@marshallku/ai-browser`

Full example:
[docs/mcp-config.json](/home/marshall/dev/browser/docs/mcp-config.json)

### Development

```bash
npm run dev
npm run build
npm run smoke:playwright
npm run smoke:fill-and-submit
./scripts/run-mcp.sh
```

## Modes

| Mode | `BROWSER_RUNTIME` | Description |
|------|-------------------|-------------|
| Playwright | `playwright` | Preferred full browser runtime |
| HTTP fetch | `http-fetch` | No browser process — fetch pages via HTTP, inspect HTML |
| Local Chromium | `chromium-cdp` | Launch headless Chromium with full browser control |
| External CDP | `external-cdp` | Connect to an already-running browser |

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `BROWSER_RUNTIME` | `playwright` | Runtime mode |
| `BROWSER_NAME` | `chromium` | Playwright browser type: chromium, firefox, webkit |
| `BROWSER_EXECUTABLE` | auto-detect | Path to Chromium binary |
| `BROWSER_DEBUG_PORT` | `9222` | CDP port |
| `BROWSER_HEADLESS` | `1` | Set `0` to show the browser window |
| `BROWSER_USER_DATA_DIR` | (temp) | Persistent profile directory |
| `BROWSER_STARTUP_TIMEOUT_MS` | `30000` | Browser launch timeout |
| `AI_BROWSER_SECRET_STORE` | `memory` | `persistent` to keep secrets across daemon restarts |
| `AI_BROWSER_VAULT_KEY` | (unset) | Passphrase for the persistent vault (PBKDF2 → AES-256-GCM). Unset → OS keychain fallback |
| `AI_BROWSER_SECRETS_FILE` | `$XDG_CONFIG_HOME/ai-browser/secrets.enc` | Override the persistent secrets file path |
| `AI_BROWSER_BASE_DIR` | `$XDG_RUNTIME_DIR/ai-browser` | Override the socket/pid directory. Used by `run-once` to isolate the ephemeral daemon. |
| `AI_BROWSER_DRAIN_TIMEOUT_MS` | `10000` | How long `daemon.shutdown` waits for in-flight actions to finish before force-closing the context (real cancel, not a fake one). |
| `BROWSER_MAX_RSS_MB` | (unset) | Chromium-tree RSS cap. When exceeded, the daemon gracefully restarts with the URL list + storageState replayed. Persistent-context mode skips the check (PID unavailable). |
| `BROWSER_RSS_POLL_MS` | `15000` | RSS poll interval. Lower for tighter reaction; min 500ms. |

## CLI

```
ai-browser <subcommand> [args] [--option value] [--json] [--tab N]
ai-browser daemon [start|stop|status|health|restart|--foreground]
ai-browser run-once <subcommand> [args]    # isolated ephemeral daemon
ai-browser repl
ai-browser mcp                             # explicit MCP server mode
ai-browser help
```

### Daemon lifecycle

The daemon listens on `$XDG_RUNTIME_DIR/ai-browser/daemon.sock` (fallback `~/.cache/ai-browser/daemon.sock`). The OS-level socket bind itself acts as the single-instance lock — a second startup is rejected unless both the recorded PID is dead *and* a ping to the socket fails. `daemon.pid` is a stale-owner hint, not the lock.

| Command | Effect |
|---------|--------|
| `ai-browser daemon start` | Start a detached daemon (no-op if already running) |
| `ai-browser daemon stop` | Send shutdown to the daemon (graceful drain — waits for in-flight actions up to `AI_BROWSER_DRAIN_TIMEOUT_MS`, then force-closes the context to release stuck Playwright work) |
| `ai-browser daemon status` | Print socket path, PID, and running state. Exit codes: `0` = running (ready), `1` = stopped, `2` = starting/mid-init |
| `ai-browser daemon health` | JSON snapshot: uptime, accepting flag, in-flight count, last error, Chromium PID, tree RSS, supervisor state. Works during drain. |
| `ai-browser daemon restart` | Stop + start |
| `ai-browser daemon --foreground` | Run the daemon in the foreground (used internally and for debugging) |

Any subcommand auto-spawns the daemon if it is not running.

### Reliability

The daemon supervises Chromium with three layered mechanisms so it stays up across crashes, leaks, and disconnects:

- **Crash restore.** Every action's URL is tracked in real time (via `framenavigated`) and `storageState` (cookies + localStorage) is refreshed in memory every 5s. When the Chromium process exits unexpectedly (signal, OOM, hard kill), the supervisor relaunches with exponential backoff (1s → 2s → 4s, capped at 30s) and replays URLs + storageState into the new context. Persistent-profile mode relies on the user-data-dir for state and only restores URLs (plus cleans up `SingletonLock`/`SingletonCookie`/`SingletonSocket` so the relaunch can take ownership).
- **Memory reclaim.** With `BROWSER_MAX_RSS_MB` set, the daemon polls the Chromium process tree every `BROWSER_RSS_POLL_MS` (default 15s) and triggers a graceful restart through the same supervisor path when the threshold is exceeded. The old browser is closed before the new one is launched — the restart is actual reclaim, not a duplicate process.
- **Drain on shutdown.** `daemon.shutdown` / `SIGTERM` stops accepting new non-control requests, lets in-flight actions finish for up to `AI_BROWSER_DRAIN_TIMEOUT_MS`, and then force-closes the context. Clients with mid-send disconnects receive an explicit `request cancelled` error — the bridge never silently replays an action that may have been partially executed.

Three consecutive failed restarts cause the daemon to exit with code 1 so a process supervisor (systemd, launchd) can do a fresh boot.

For long-running deployments (systemd unit, launchd plist, health observation, drain semantics, migration notes), see [`docs/operations.md`](docs/operations.md).

### Ephemeral daemon for one-off commands (`run-once`)

For CI scripts that want a clean browser per invocation without affecting the user's long-running daemon:

```bash
ai-browser run-once navigate https://example.com
ai-browser run-once screenshot --out /tmp/page.png
```

`run-once` spawns an isolated daemon in a temporary directory (via `AI_BROWSER_BASE_DIR`), runs the single subcommand, then tears the daemon down. Meta subcommands (`daemon`, `run-once`, `mcp`, `repl`, `help`) are refused. The main daemon, if any, is untouched.

### Output format

- Default — pretty JSON for object results, plain text for strings, `ok` for null.
- `--json` — single-line compact JSON of `result.data` (machine-readable; the wrapper success/error envelope is dropped on success).
- `--out FILE` — write base64 binary results (e.g. `screenshot`) to FILE.

### Common subcommands

```bash
# Navigation
ai-browser navigate https://example.com
ai-browser open-tab https://other.example
ai-browser list-tabs --json
ai-browser activate-tab --tab 2
ai-browser back        # also: forward, reload

# DOM
ai-browser get-text --selector main
ai-browser get-html --selector "#app"
ai-browser query "button[type=submit]" --json
ai-browser summary

# Interaction
ai-browser click "button.primary"
ai-browser type "#email" "user@example.com"
ai-browser hover "[data-tooltip]"
ai-browser press-key "Control+A"
ai-browser select-option "select#country" --value KR
ai-browser check "#agree"

# Capture
ai-browser screenshot --out /tmp/page.png
ai-browser metrics --json

# Waits
ai-browser wait-selector "#app-ready"
ai-browser wait-network-idle
ai-browser wait-url "https://example.com/dashboard*" --pattern-type glob --timeout 120000

# Secrets (credentials stay inside the daemon; plaintext never crosses argv)
ai-browser secret-put --from-env LOGIN_PW --label gmail --json
ai-browser secret-put --from-file /tmp/pw.txt --label github --json
echo -n "$PW" | ai-browser secret-put --stdin --label paypal --json
ai-browser secret-list --json
ai-browser type-secret "#password" --secret-id <id>
ai-browser secret-delete <id>

# Monitoring
ai-browser console-logs --json
ai-browser page-errors --json
ai-browser network-logs --method POST --url-pattern "/api/" --json
ai-browser network-logs --status 4xx --include-body true --json

# State
ai-browser cookies-get https://example.com --json
ai-browser cookies-set --url https://example.com --name session --value abc
ai-browser storage-get --type local --json
ai-browser eval "document.querySelectorAll('a').length" --json
```

### REPL

```
$ ai-browser repl
ai-browser repl — type 'help' for commands, 'exit' to quit
syntax: <action> [json-params]   e.g.  tabs.navigate {"url":"https://example.com"}
> tabs.navigate {"url":"https://example.com"}
> dom.contentSummary
> monitor.networkLogs {"status":"4xx"}
> exit
```

REPL accepts the raw bridge actions (e.g. `tabs.navigate`, `monitor.networkLogs`) with a JSON params object. Tab-completion offers action names. History persists at `$XDG_DATA_HOME/ai-browser/repl_history`.

## Monitoring tools for AI agents

`get_console_logs`, `get_page_errors`, and `get_network_logs` expose per-tab event buffers captured automatically by the Playwright/CDP runtimes. Useful when the model needs to debug a flow rather than just control the page.

### `get_network_logs`

Returns HTTP activity seen by the tab (navigation, XHR, fetch, subresources).

| Parameter | Type | Description |
|-----------|------|-------------|
| `tabId` | number? | Defaults to the active tab. |
| `method` | string? | Filter by method (case-insensitive). |
| `status` | number \| "2xx" \| "3xx" \| "4xx" \| "5xx" | Exact code or bucket. |
| `urlPattern` | string? | Regex matched against the URL (e.g. `/api/users`). |
| `limit` | number? | Default 100, cap 500. |
| `includeBody` | boolean? | Include request/response bodies (default false). |

Returned entry shape (one per request):

```
{
  url, method, resourceType, status, statusText,
  requestHeaders, responseHeaders,
  requestBody, responseBody, responseBodyTruncated, responseBodySize,
  startTime, endTime, durationMs,
  fromCache, failed, failureText
}
```

Limits & safety:

- **500 entries per tab** (FIFO). Tab close clears everything.
- **Bodies only captured for text-ish content types** (`text/*`, `application/json`, `application/xml`, `application/javascript`, form/graphql) and **truncated at 100 KB**.
- **Sensitive headers redacted**: `authorization`, `cookie`, `set-cookie`, `proxy-authorization` become `[redacted]` before the buffer is populated.
- Bodies are omitted from responses unless `includeBody=true` — keeps the default output small for LLM context.

Typical recipes for an AI agent:

```jsonc
// 1. Find failing API calls after clicking submit
{ "urlPattern": "/api/", "status": "5xx" }

// 2. Inspect a specific JSON response
{ "urlPattern": "/api/users/42$", "includeBody": true, "limit": 5 }

// 3. Check whether the page hit the backend at all
{ "method": "POST", "urlPattern": "/graphql" }
```

## Notes

- In this Codex sandbox, Chromium launch required escalated permissions to pass the runtime smoke test.
- The checked smoke path used the cached Playwright Chromium binary at `~/.cache/ms-playwright/chromium-1208/chrome-linux64/chrome`.
- A runnable MCP entrypoint is provided at [scripts/run-mcp.sh](/home/marshall/dev/browser/scripts/run-mcp.sh).

## Persistent secret store

By default, secrets live in an in-memory AES-GCM vault that evaporates when the daemon stops. Set `AI_BROWSER_SECRET_STORE=persistent` to keep them across restarts in an encrypted file (default `$XDG_CONFIG_HOME/ai-browser/secrets.enc`, mode `0600`).

The master key resolves in this order:

1. **`AI_BROWSER_VAULT_KEY` env** (passphrase) — derived to a 32-byte key via PBKDF2-SHA256 (200k iters, per-vault random salt stored in the file header). Best for CI, scripts, and shared shells where keychain access is awkward.
2. **OS keychain auto-fallback** — only if `AI_BROWSER_VAULT_KEY` is unset:
   - macOS: `security add-generic-password` (key fed through `security -i` so it never appears in process argv)
   - Linux: `secret-tool` (libsecret)
3. **Neither available** → store init fails with an actionable error.

Once unlocked, every record is sealed with its own IV + auth tag. `secret-list` (CLI) and `secret_list` (MCP) return metadata only — id, label, createdAt, opaque preview — and never decrypt.

```bash
# One-time setup (passphrase mode)
export AI_BROWSER_SECRET_STORE=persistent
export AI_BROWSER_VAULT_KEY='your-strong-passphrase'

ai-browser secret-put --from-env GMAIL_PW --label gmail --json
# { "id": "ab12...", "label": "gmail", ... } — id is the only handle you reuse
```

Restart the daemon, restart the host — the handle still resolves. Switching modes (passphrase ↔ keychain) requires a new vault file because the master keys are independent by design.

## Login + 2FA recipe

The three new pieces (`wait-url`, persistent secrets, CLI `type-secret`) compose into a single shell script that handles credential-protected logins with manual 2FA:

```bash
#!/usr/bin/env bash
set -euo pipefail

# Daemon shares cookies/storage across calls — log in once, reuse forever.
ai-browser navigate https://accounts.example.com/signin
ai-browser type "#email" "me@example.com"
ai-browser type-secret "#password" --secret-id "$EXAMPLE_SECRET_ID"
ai-browser click "button[type=submit]"

# User taps the 2FA prompt on their phone. The script just waits.
ai-browser wait-url "https://app.example.com/home*" \
    --pattern-type glob \
    --timeout 120000

# Now logged in. Persist cookies to disk so the next run skips 2FA.
ai-browser cookies-get https://app.example.com --json > ~/.cache/example-cookies.json
```

`wait-url` supports three `--pattern-type` modes:

| Mode | Use when | Example |
|------|----------|---------|
| `exact` (default) | the target URL is fully predictable | `https://app.example.com/home` |
| `glob` | path varies but host/prefix is stable | `https://app.example.com/u/*/home` |
| `regex` | you need alternation or anchored matching | `^https://(app\|m)\\.example\\.com/home$` |

The same primitives are available as MCP tools (`wait_for_url`, `secret_store_put`, `secret_list`, `type_secret`, `secret_store_delete`) so an AI agent can drive the same flow without ever seeing plaintext credentials.

## Secret Import

`secret_import_csv` reads a header-based CSV and stores one secret handle per row.

- default value column detection: `password`, then `value`, then the first column
- default label columns: any of `label`, `name`, `title`, `site`, `url`, `username`, `email`
- output includes `secretId`, row number, label, and a redacted preview

## Scripted Automation

For fixed workflows, use [fill-and-submit.sh](/home/marshall/dev/browser/scripts/fill-and-submit.sh) instead of MCP.

Examples:

```bash
./scripts/fill-and-submit.sh \
  --url https://example.com/login \
  --fill '#email=user@example.com' \
  --fill-secret '#password=env:LOGIN_PASSWORD' \
  --click 'button[type=submit]' \
  --screenshot /tmp/login-result.png
```

```bash
./scripts/fill-and-submit.sh \
  --url https://example.com/form \
  --fill '#name=Marshall' \
  --fill '#team=Browser' \
  --wait-for '#submit' \
  --click '#submit'
```
