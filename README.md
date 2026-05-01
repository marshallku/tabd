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
| **Wait** | selector, navigation, networkIdle | - | full |
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

**As an MCP server (for AI agents)** — default. Claude Code / Codex / etc. invoke `ai-browser` (no args) on stdio and call tools. By default each MCP session owns its own browser instance. Pass `--daemon` (or set `AI_BROWSER_MCP_MODE=daemon`) to attach to the shared daemon instead — AI and CLI then share one Chromium and one set of tabs/cookies/logs.

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

To share one Chromium between MCP and CLI, run the MCP server in daemon-attach mode — both will hit the same browser instance:

```jsonc
{
  "mcpServers": {
    "browser": {
      "command": "ai-browser",
      "args": ["mcp", "--daemon"]
    }
  }
}
```

Equivalent: set `AI_BROWSER_MCP_MODE=daemon` in the env block. Default (`standalone`) preserves the original isolated-browser behavior.

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

## CLI

```
ai-browser <subcommand> [args] [--option value] [--json] [--tab N]
ai-browser daemon [start|stop|status|restart|--foreground]
ai-browser repl
ai-browser mcp                  # explicit MCP server mode
ai-browser help
```

### Daemon lifecycle

The daemon listens on `$XDG_RUNTIME_DIR/ai-browser/daemon.sock` (fallback `~/.cache/ai-browser/daemon.sock`). The OS-level socket bind itself acts as the single-instance lock — a second startup is rejected unless both the recorded PID is dead *and* a ping to the socket fails. `daemon.pid` is a stale-owner hint, not the lock.

| Command | Effect |
|---------|--------|
| `ai-browser daemon start` | Start a detached daemon (no-op if already running) |
| `ai-browser daemon stop` | Send shutdown to the daemon |
| `ai-browser daemon status` | Print socket path, PID, and running state. Exit codes: `0` = running (ready), `1` = stopped, `2` = starting/mid-init |
| `ai-browser daemon restart` | Stop + start |
| `ai-browser daemon --foreground` | Run the daemon in the foreground (used internally and for debugging) |

Any subcommand auto-spawns the daemon if it is not running.

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
