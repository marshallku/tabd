# Commands reference

Per-action signatures, options, return shapes, and error strings. Cross-
reference doc — for narrative scenarios see [cookbook.md](cookbook.md), for
why-it-is-shaped-this-way see [architecture.md](architecture.md), for running
the daemon see [operations.md](operations.md).

## Contents

- [Conventions](#conventions) — global flags, argv parsing, tab semantics
- [Daemon control](#daemon-control) — `daemon start/stop/ping/health`
- Actions (39 total, grouped):
  - [Tabs](#tabs) (8) · [DOM](#dom) (4) · [Interaction](#interaction) (8)
  - [Capture](#capture) (2) · [Execution](#execution) (1) · [Wait](#wait) (3)
  - [Cookies](#cookies) (3) · [Storage](#storage) (3) · [Monitor](#monitor) (3)
  - [Secrets](#secrets) (4)

---

## Conventions

### Global flags (every action accepts)

| Flag | Effect |
|---|---|
| `--json` | Emit the daemon response payload as compact JSON instead of pretty-rendered text. Strings come back quoted, `null` as the literal `null`, objects/arrays serialized compactly. |
| `--out FILE` | For actions returning base64 data (`screenshot`), decode the bytes and write the file. Suppresses stdout payload. |
| `--tab N` | Target tab by 1-based index. Most actions default to the active tab if omitted; a few require it (see [Tab semantics](#tab-semantics)). |
| `--base-dir DIR` | Override `$TABD_BASE_DIR` for the socket/pid path. Useful for isolated daemons in CI. |

### argv parsing

Mirrors the original TS CLI for tooling compatibility:

- `--flag VALUE` and `--flag=VALUE` both accepted
- `--no-flag` ⇒ `flag: false`
- Bare `--flag` (no value, no `=`) ⇒ `flag: true`
- Positionals: action-specific (see each entry's signature)
- Coercion: `true`/`false`/`null` → typed; integer literals → `i64`; decimal literals → `f64`; anything else → `string`
- kebab-case flag names get camelCased before reaching the daemon:
  `--pattern-type` ⇒ `patternType`, `--include-body` ⇒ `includeBody`,
  `--max-headings` ⇒ `maxHeadings`, etc.

### Tab semantics

`--tab N` is 1-based and maps to `tabId` in the wire protocol.

**Require `--tab` explicitly** (error: `"tabId is required"`):
- `close-tab`, `activate-tab`

**Default to active tab** if `--tab` omitted: every other tab-scoped action.

**Tab-less** (operate on the daemon/browser globally, not a specific tab):
`screenshot`, `eval`, `list-tabs`, `open-tab`, all `cookies-*`, all `storage-*`,
all `secret-*`.

### Return shape

All daemon responses come back as `{ id, success, data }`. The CLI unwraps:
- on `success: true` → prints `data` (string raw, object/array via `--json`)
- on `success: false` → prints `error: <message> [<errorCode>]` to stderr and
  exits nonzero (see [Errors & exit codes](#errors--exit-codes)); with `--json`
  the full failure envelope `{ id, success, error, errorCode }` is printed to
  stdout instead

In the action tables below, "Returns" is the `data` payload only.

### Errors & exit codes

Failures carry a stable machine-parseable `errorCode` so scripts and AI agents
can branch without regex-matching the error prose. The exit code maps from it:

| `errorCode` | Exit | Meaning / typical reaction |
|---|---|---|
| `selector_not_found` | 5 | Selector never matched **or never became visible** before the deadline → fix the selector, or wait/retry if content loads late |
| `tab_not_found` | 5 | 1-based tab index out of range, or no tabs open → `list-tabs` and re-resolve |
| `timeout` | 4 | `wait-url` / `wait-network-idle` / a CDP RPC hit its deadline → retry or raise `--timeout` |
| `daemon_unreachable` | 3 | Socket unreachable, auto-spawn failed, or daemon draining → check `daemon health`, restart |
| `cdp_not_ready` | 1 | Chromium is (re)starting → brief wait, then retry |
| `eval_error` | 1 | JS threw inside `eval` or an injected expression → inspect the message |
| `vault_error` | 1 | Secrets vault locked / `TABD_VAULT_KEY` unset / unknown secret id |
| `invalid_request` | 1 | Unknown action, malformed JSON, missing/invalid params → fix the call |
| `internal` | 1 | Anything else |

Exit `0` is success; exit `2` is reserved for client-side usage errors
(`secret-put` source-flag validation). Scripts that only check "nonzero =
failure" are unaffected by the 3/4/5 split.

```bash
tabd click '#login' --json || case $? in
  5) echo "selector/tab gone — re-query the page" ;;
  4) echo "still loading — retry" ;;
  3) echo "daemon down — tabd daemon start" ;;
esac
```

### `secret-put` is special

It does **not** go through the generic DISPATCH table. The CLI reads the
plaintext locally (from `--from-env` / `--from-file` / `--stdin`), then sends
only the resolved value to the daemon — plaintext never appears on argv.

---

## Daemon control

`tabd daemon` is a clap subcommand (separate from the action dispatcher).
Standard `--help` works on these.

### daemon start

```bash
tabd daemon start [--base-dir DIR]
```

Run the daemon in the foreground (blocks until SIGTERM, SIGINT, or
`daemon.shutdown`). Auto-spawn happens detached, never via this command.

Boots Chromium, opens the UDS at `$base_dir/daemon.sock`, writes
`$base_dir/daemon.pid`. Default base_dir per [operations.md](operations.md).

### daemon stop

```bash
tabd daemon stop [--base-dir DIR]
```

Triggers graceful drain (`accepting=false`, in-flight actions get
`$TABD_DRAIN_TIMEOUT_MS` to finish, default 10000ms), then closes.
Prints the shutdown response payload.

### daemon ping

```bash
tabd daemon ping [--base-dir DIR]
```

**Returns**: `{ "pid": number, "ready": bool }`.

### daemon health

```bash
tabd daemon health [--base-dir DIR]
```

**Returns**: see [operations.md § Watching the daemon](operations.md#watching-the-daemon).

---

## Tabs

### navigate

```bash
tabd navigate <url> [--tab N]
```

| Positional | Type | Meaning |
|---|---|---|
| `url` | string | URL to navigate to |

**Returns**: `{ "url": string }` — the requested URL after navigation completes.

**Errors**: `"tabs.navigate: missing 'url' (string)"`.

### open-tab

```bash
tabd open-tab <url>
```

**Returns**: `{ "tabId": number, "targetId": string, "url": string }`.

The new tab becomes active. Use `list-tabs` to see all tabs with active flag.

### close-tab

```bash
tabd close-tab --tab N
```

`--tab` is **required**. **Returns**: `null`.

**Errors**: `"tabId is required"`, `"Tab not found: N"`.

### list-tabs

```bash
tabd list-tabs
```

**Returns**:
```json
[
  {"tabId": 1, "targetId": "ABCD...", "title": "Example", "url": "...", "active": true},
  ...
]
```

### activate-tab

```bash
tabd activate-tab --tab N
```

`--tab` is **required**. **Returns**: `null`.

### back / forward / reload

```bash
tabd back [--tab N]
tabd forward [--tab N]
tabd reload [--tab N]
```

**Returns**: `null`.

`back`/`forward` call `history.back()`/`history.forward()` in page; no error
if there is nothing to go back to.

---

## DOM

### get-html

```bash
tabd get-html [--selector SELECTOR] [--no-outer] [--no-clean] [--tab N]
```

| Option | Type | Default | Meaning |
|---|---|---|---|
| `--selector` | string | `body` | CSS selector to extract |
| `--outer` | bool | `true` | Include outer HTML (use `--no-outer` for innerHTML) |
| `--clean` | bool | `true` | Strip `<script>`, `<style>`, `<svg>`, comments, `data-*` attrs |

**Returns**: `string` — the HTML.

**Errors**: `"Selector not found: SELECTOR"`.

### get-text

```bash
tabd get-text [--selector SELECTOR] [--raw] [--tab N]
```

| Option | Type | Default | Meaning |
|---|---|---|---|
| `--selector` | string | `main, article, body` (first hit) | CSS selector |
| `--raw` | bool | `false` | Return raw `textContent` instead of whitespace-normalized text |

**Returns**: `string` — the text. Cleaned form strips repeated whitespace and
empty lines.

### query

```bash
tabd query <selector> [--limit N] [--visible-only] [--tab N]
```

| Option | Type | Default | Meaning |
|---|---|---|---|
| `--limit` | number | `20` | Max matched elements to return |
| `--visible-only` | bool | `false` | Filter to elements with non-zero bounding rect and visible computed style |

**Returns**:
```json
[
  {
    "index": 0,
    "tag": "li",
    "id": null,
    "classes": ["row"],
    "text": "...",
    "attributes": {"data-role": "...", ...},
    "rect": {"x": 10, "y": 100, "width": 200, "height": 24}
  },
  ...
]
```

### summary

```bash
tabd summary [--selector SELECTOR] [--max-headings N] [--max-links N] [--max-text-length N] [--tab N]
```

LLM-friendly page summary. Strips noise (`nav`, `footer`, `script`, `style`,
ARIA-hidden, cookie banners, ads), normalizes whitespace.

| Option | Type | Default | Meaning |
|---|---|---|---|
| `--selector` | string | first hit of `main, article, [role=main], body` | Scope element |
| `--max-headings` | number | `20` | |
| `--max-links` | number | `20` | |
| `--max-text-length` | number | `4000` | Truncates `text` field at this many chars |

**Returns**:
```json
{
  "url": "...",
  "title": "...",
  "selector": "body",
  "headings": [{"level": "h1", "text": "..."}, ...],
  "links": [{"text": "...", "href": "..."}, ...],
  "forms": [{"index": 0, "fields": [{"name": "u", "type": "text", "id": null}, ...]}],
  "text": "...cleaned page text..."
}
```

---

## Interaction

All interaction actions auto-wait for the selector to become visible (default
30000 ms; override with `--timeout`).

### click

```bash
tabd click <selector> [--timeout MS] [--tab N]
```

**Returns**: result of `element.click()` evaluation, typically `null` or `undefined`.

**Errors**: `"selector SELECTOR not visible after N ms"`.

### type

```bash
tabd type <selector> <text> [--timeout MS] [--tab N]
```

Types `text` into an `<input>` / `<textarea>` / `contentEditable` element.
Sets `.value` directly **and** dispatches `input` event — works on most
controlled inputs (React/Vue). For sensitive values use [type-secret](#type-secret).

### hover

```bash
tabd hover <selector> [--x OFFSET] [--y OFFSET] [--tab N]
```

| Option | Type | Default | Meaning |
|---|---|---|---|
| `--x` | number | rect center | X offset *within* the element |
| `--y` | number | rect center | Y offset *within* the element |

**Returns**: `null`. Dispatches a CDP `Input.dispatchMouseEvent` of type `mouseMoved` at the resolved coordinates.

### mouse-move

```bash
tabd mouse-move --x N --y N [--tab N]
```

Move mouse to absolute viewport coordinates. Both `--x` and `--y` required.

### scroll

```bash
tabd scroll [--selector SELECTOR] [--x PX] [--y PX] [--tab N]
```

Two modes:
- with `--selector`: scrolls the element into view (`scrollIntoView({block: "center"})`)
- without `--selector`: scrolls the viewport by `(x, y)` pixels (relative)

**Returns**: `null`.

### press-key

```bash
tabd press-key <key> [--selector SELECTOR] [--tab N]
```

If `--selector` is given, focuses that element first.

**Recognized special key names** (case-sensitive, matched verbatim):
`Enter`, `Tab`, `Escape`, `Backspace`, `Delete`,
`ArrowLeft`, `ArrowUp`, `ArrowRight`, `ArrowDown`,
`Home`, `End`, `PageUp`, `PageDown`, `Space`,
`F1` … `F12`.

Anything else: treated as a single character (lowercased for the `key` field;
`text` event also fired so `<input>` sees it as typed).

**Returns**: `null`.

### select-option

```bash
tabd select-option <selector> {--value V | --label L | --index N} [--tab N]
```

Selects an `<option>` inside a `<select>`. Exactly one of `--value` / `--label` /
`--index` should be set. Tried in that order if multiple.

**Returns**: `null`.

**Errors**: `"selectOption: not a SELECT"`, `"Requested option was not found"`.

### check

```bash
tabd check <selector> [--no-checked] [--tab N]
```

Sets a checkbox or radio's `checked` state. Default: `true`. Use `--no-checked`
to uncheck.

**Returns**: `null`.

---

## Capture

### screenshot

```bash
tabd screenshot [--out FILE]
```

Captures the **active tab** (no `--tab` option).

**Returns**: `"data:image/png;base64,..."` data URL string. With `--out FILE`,
the PNG bytes are decoded and written to the file; nothing is printed.

**Errors**: `"Page.captureScreenshot timed out after 10s"`.

### metrics

```bash
tabd metrics [--tab N]
```

**Returns**:
```json
{
  "url": "...",
  "title": "...",
  "readyState": "complete",
  "domNodes": 1234,
  "resources": 42,
  "navigation": {
    "type": "navigate",
    "domContentLoaded": 250,
    "loadEventEnd": 800
  }
}
```

`navigation` may be `null` if `performance.getEntriesByType("navigation")` is
empty (e.g. very fresh tab).

---

## Execution

### eval

```bash
tabd eval <code>
```

Run JavaScript in the active tab's main world. The expression's value is
returned via CDP `returnByValue: true, awaitPromise: true` — so an async
function expression (`async () => …`) or a top-level `await` (single `await`
expression — not statement-block-level) works directly.

The result must be JSON-serializable. Returning `undefined` results in no
`data` field (CLI prints empty).

**Patterns**:
```bash
# Plain expression
tabd eval '1 + 1'

# Async fetch in browser context (cookies + CSRF + session auto-applied)
tabd eval 'await fetch("/api/data").then(r => r.json())' --json

# Wrap multi-statement code in an IIFE so the final value is returned
tabd eval '(() => { const xs = [...document.querySelectorAll("li")]; return xs.length; })()'
```

**Returns**: the evaluated value (any JSON type).

**Errors**: `"Runtime.evaluate failed"` (wrapped with the JS exception text).

---

## Wait

### wait-selector

```bash
tabd wait-selector <selector> [--timeout MS] [--tab N]
```

Polls until the selector resolves to a visible element. Default timeout 30000ms.

**Returns**: `{ "found": true }`.

**Errors**: `"selector SELECTOR not visible after N ms"`.

### wait-url

```bash
tabd wait-url <pattern> [--pattern-type TYPE] [--timeout MS] [--tab N]
```

Polls the tab's current URL until it matches the pattern.

| `--pattern-type` | Meaning |
|---|---|
| `exact` (default) | literal string equality |
| `glob` | shell-style wildcards; `*` → `.*`, other regex metachars escaped, anchored `^…$` |
| `regex` | JavaScript regex (raw) |

Default timeout 30000ms.

**Returns**: `{ "url": string }` — the URL at the moment of match.

**Errors**: `"wait-url timed out after N ms (pattern=... type=...)"`,
`"invalid regex pattern: ..."`, `"invalid glob → regex compile: ..."`,
`"unsupported patternType 'X' (expected exact|glob|regex)"`.

### wait-network-idle

```bash
tabd wait-network-idle [--idle-time MS] [--timeout MS] [--tab N]
```

| Option | Type | Default | Meaning |
|---|---|---|---|
| `--idle-time` | number | `500` | Network must be quiet for this many ms |
| `--timeout` | number | `10000` | Total timeout |

Counts in-flight CDP `Network.*` requests on the tab; when the count is zero
for `--idle-time` consecutive ms, returns successfully.

**Returns**: `null`.

**Errors**: `"Timed out waiting for network idle (N pending requests)"`.

---

## Cookies

### cookies-get

```bash
tabd cookies-get <url>
```

**Returns**: array of CDP cookie objects:
```json
[
  {"name": "...", "value": "...", "domain": "...", "path": "/", "expires": ..., "size": ..., "httpOnly": false, "secure": true, "session": false, "sameSite": "Lax"},
  ...
]
```

### cookies-set

```bash
tabd cookies-set --url URL --name NAME --value VALUE \
  [--domain D] [--path P] [--secure] [--http-only] \
  [--same-site SAMESITE] [--expiration-date EPOCH_SECS]
```

| Option | Type | Default | Meaning |
|---|---|---|---|
| `--url` | string | **required** | URL for cookie scope; CDP infers domain/path if not given |
| `--name` | string | **required** | Cookie name |
| `--value` | string | **required** | Cookie value |
| `--domain` | string | inferred from URL | |
| `--path` | string | `/` | |
| `--secure` | bool | from URL | use `--secure` / `--no-secure` |
| `--http-only` | bool | `false` | |
| `--same-site` | string | none | one of `Strict` / `Lax` / `None` |
| `--expiration-date` | number | session | Unix epoch seconds |

**Returns**: `null`.

**Errors**: `"Network.setCookie timed out after 5s"`, `"CDP rejected the cookie: ..."`.

### cookies-delete

```bash
tabd cookies-delete <name> --url URL
```

`--url` is required.

**Returns**: `null`.

---

## Storage

`localStorage` / `sessionStorage` for the **active tab's current origin**.

### storage-get

```bash
tabd storage-get [--key KEY] [--type local|session] [--tab N]
```

| Option | Type | Default | Meaning |
|---|---|---|---|
| `--key` | string | — | Get one key only |
| `--type` | string | `local` | `local` (default) or `session` |

**Returns**:
- with `--key`: `string | null` (the value, or null if missing)
- without `--key`: `{ "k1": "v1", "k2": "v2", ... }`

### storage-set

```bash
tabd storage-set --key KEY --value VALUE [--type local|session] [--tab N]
```

**Returns**: `null`.

### storage-clear

```bash
tabd storage-clear [--type local|session] [--tab N]
```

**Returns**: `null`. Clears the entire storage for the current origin.

---

## Monitor

The daemon keeps per-tab ring buffers for console (100), page errors (100),
and network (500). These actions tail those buffers.

### console-logs

```bash
tabd console-logs [--level LEVEL] [--limit N] [--tab N]
```

| Option | Type | Default | Meaning |
|---|---|---|---|
| `--level` | string | (all) | Filter by `log` / `warn` / `error` / `info` / `debug` |
| `--limit` | number | `100` | Tail size |

**Returns**:
```json
[
  {"level": "log", "message": "...", "timestamp": 1701337200000, "stackTrace": "..."?},
  ...
]
```

### page-errors

```bash
tabd page-errors [--limit N] [--tab N]
```

**Returns**:
```json
[
  {"url": "...", "lineNumber": 42, "columnNumber": 10, "text": "Uncaught TypeError: ...", "level": "error"},
  ...
]
```

Captures both `Runtime.exceptionThrown` and unhandled promise rejections.

### network-logs

```bash
tabd network-logs [--method M] [--status S] [--url-pattern REGEX] \
  [--url-contains SUBSTR] [--limit N] [--include-body] [--tab N]
```

| Option | Type | Meaning |
|---|---|---|
| `--method` | string | HTTP method, case-insensitive (`GET`, `POST`, …) |
| `--status` | number\|string | Exact (`404`) or bucket (`2xx`, `4xx`, `5xx`) |
| `--url-pattern` | regex string | Anchored regex against URL |
| `--url-contains` | string | Substring match against URL |
| `--limit` | number | Tail size (default 100) |
| `--include-body` | bool | Include response body — **deferred** (reader task can't dispatch CDP RPC; see [architecture.md § Reader task is read-only](architecture.md#reader-task-is-read-only)). Returns metadata only regardless. |

**Returns**:
```json
[
  {
    "method": "GET",
    "url": "https://app/api/x",
    "status": 200,
    "type": "fetch",
    "startTime": 1701337200000,
    "endTime": 1701337200245,
    "durationMs": 245,
    "fromCache": false,
    "failed": false,
    "responseBodySize": 1234
  },
  ...
]
```

Failed/cancelled requests have `failed: true` and a `failureText` field.

---

## Secrets

All `secret-*` actions require `$TABD_VAULT_KEY` to be set in the daemon's
environment. The vault file lives at `$XDG_CONFIG_HOME/tabd/secrets.enc`
(override with `$TABD_VAULT_PATH`).

### secret-put

```bash
tabd secret-put {--from-env VAR | --from-file PATH | --stdin} [--label TEXT]
```

| Option | Type | Meaning |
|---|---|---|
| `--from-env` | string | Read secret value from named environment variable |
| `--from-file` | string | Read secret value from file at path |
| `--stdin` | bool | Read secret value from stdin until EOF |
| `--label` | string | Optional human-readable label stored alongside |

Exactly one of `--from-env` / `--from-file` / `--stdin` must be set. The CLI
reads the plaintext locally; the daemon receives only the resolved value.
The trailing newline is stripped before storage.

**Returns**:
```json
{"secretId": "a1b2c3...", "label": "github", "createdAt": 1701337200000, "preview": "****"}
```

**Errors**: `"secret-put: provide --from-env VAR, --from-file PATH, or --stdin"`,
`"secret-put: choose exactly one ..."`, `"secret-put: env var VAR is not set"`,
`"value is empty"`, `"TABD_VAULT_KEY env not set; secrets unavailable"`.

### secret-list

```bash
tabd secret-list
```

**Returns**: array of `{ secretId, label, createdAt, preview: "****" }`. Never
decrypts plaintext.

### secret-delete

```bash
tabd secret-delete <secretId>
```

**Returns**: `null`.

**Errors**: `"secret not found"`.

### type-secret

```bash
tabd type-secret <selector> --secret-id ID [--no-clear] [--tab N]
```

Decrypts the named secret in-daemon and types it into the selector via the
native value setter (works on controlled inputs that ignore plain `.value =`
assignment).

| Option | Type | Default | Meaning |
|---|---|---|---|
| `--secret-id` | string | **required** | Vault ID from `secret-put` |
| `--clear` | bool | `true` | Clear field before typing (use `--no-clear` to append) |

**Returns**: `null`. Plaintext never leaves the daemon process.

**Errors**: `"secret not found"`, `"type-secret: element is not editable"`.
