# Cookbook

Working scenarios that combine multiple tabd actions. For per-action
reference see [commands.md](commands.md).

## Contents

1. [Login with 2FA and extract data](#1-login-with-2fa-and-extract-data)
2. [Three patterns for capturing API responses](#2-three-patterns-for-capturing-api-responses)
3. [Save and restore a session](#3-save-and-restore-a-session)
4. [Multi-tab compare](#4-multi-tab-compare)
5. [Scroll-driven infinite list](#5-scroll-driven-infinite-list)
6. [CI / one-shot isolated daemon](#6-ci--one-shot-isolated-daemon)
7. [Wait until a single network call completes](#7-wait-until-a-single-network-call-completes)
8. [Gotchas](#gotchas)

---

## 1. Login with 2FA and extract data

The full pattern: log in, pass 2FA, wait for the destination page, pull data
via in-browser fetch.

### One-time setup (run once, secrets persist in the vault)

```bash
export TABD_VAULT_KEY="$(pass show tabd/vault 2>/dev/null || echo 'change-me')"

# Password
echo -n "$EXAMPLE_PASSWORD" | tabd secret-put --label example-password --stdin
# → {"secretId":"PW_ID","label":"example-password","createdAt":...,"preview":"****"}

# TOTP secret (the base32 string from the QR code, NOT the 6-digit code)
echo -n "$EXAMPLE_TOTP_SECRET" | tabd secret-put --label example-totp --stdin
# → {"secretId":"TOTP_ID",...}
```

Save the printed IDs somewhere outside the vault — they're not secrets, but
you'll need them in the script.

### The script

```bash
#!/usr/bin/env bash
set -euo pipefail

: "${TABD_VAULT_KEY:?set TABD_VAULT_KEY}"
PW_ID="${PW_ID:?set PW_ID from secret-put}"
TOTP_ID="${TOTP_ID:?set TOTP_ID from secret-put}"

# 1. Login page
tabd navigate https://app.example.com/login
tabd wait-selector '#username' --timeout 10000

# 2. Credentials. type-secret keeps plaintext inside the daemon.
tabd type '#username' 'me@example.com'
tabd type-secret '#password' --secret-id "$PW_ID"
tabd click 'button[type=submit]'

# 3. Wait for the OTP page
tabd wait-url 'https://app.example.com/otp*' --pattern-type glob --timeout 30000

# 4. Generate TOTP code (oathtool or pyotp). secret-list won't decrypt it
#    so we keep the raw base32 in an env file:
#    ~/.config/tabd/totp.env contains:  EXAMPLE_TOTP_RAW=JBSWY3DPEHPK3PXP
TOTP_CODE=$(oathtool --totp --base32 "$EXAMPLE_TOTP_RAW")
tabd type '#otp' "$TOTP_CODE"
tabd click 'button[name=verify]'

# 5. Dashboard arrived
tabd wait-url 'https://app.example.com/dashboard*' --pattern-type glob --timeout 30000
tabd wait-network-idle --idle-time 1500 --timeout 15000

# 6. Pull data — pattern 1 (in-browser fetch) from cookbook §2
tabd eval 'await fetch("/api/transactions?limit=500").then(r => r.json())' --json \
  > "${1:-/tmp/transactions.json}"
```

### Variations

- **SMS / push 2FA** (can't TOTP-automate): block on user input.

  ```bash
  read -r -p "OTP code from your phone: " OTP
  tabd type '#otp' "$OTP"
  tabd click 'button[name=verify]'
  ```

  Chromium is headless so the user can't see the page — but they don't need
  to; the OTP arrives on their phone.

- **Persistent login** (no 2FA after the first): save cookies + storage after
  the first run, restore on subsequent runs. See [§3](#3-save-and-restore-a-session).

---

## 2. Three patterns for capturing API responses

When a UI flow triggers a backend API call, there are three ways to get the
response — and they have **different trade-offs**.

### Pattern A — `eval` + `fetch` (recommended)

Run `fetch` *inside the page* so cookies, CSRF tokens, SameSite rules, and
custom headers from the site's own code all apply automatically:

```bash
# Just the JSON body
tabd eval 'await fetch("/api/transactions?from=2024-01-01").then(r => r.json())' --json

# Body + status + headers
tabd eval '(async () => {
  const r = await fetch("/api/transactions", {
    method: "POST",
    headers: {"Content-Type": "application/json"},
    body: JSON.stringify({from: "2024-01-01"})
  });
  return {
    status: r.status,
    headers: Object.fromEntries(r.headers.entries()),
    body: await r.json()
  };
})()' --json
```

**Strengths**: zero session reconstruction work; CORS / preflight all
handled by the browser; whatever JS the site does (signing, CSRF refresh)
also applies.

**Limitations**: requires the daemon to stay attached to the page that
provides the right origin. Same-origin policy applies (a Google-origin
fetch from an example.com page won't carry Google cookies).

### Pattern B — click + `network-logs`

When the UI button calls an API you can't easily call directly (multiple
calls, intermediate state, server-only endpoint), let the page do its thing
and inspect what happened:

```bash
tabd click '#load-transactions'
sleep 0.5
tabd wait-network-idle --idle-time 500

tabd network-logs --url-contains '/api/transactions' --limit 1 --json | jq '.[]'
```

**Returns** request method, URL, status, timing — but **not body**. Body
fetch is deferred (see [commands.md § network-logs](commands.md#network-logs)).

For body, do the click + read pattern, then re-issue via Pattern A using the
URL/method you observed.

### Pattern C — extract cookies + curl

When you need to drive requests from a different process / host, or you
want to remove tabd from the runtime path after login:

```bash
# After login, snapshot the session
tabd cookies-get https://app.example.com/ --json > /tmp/cookies.json

# Reconstruct a Cookie header for curl
COOKIE_HDR=$(jq -r '[.[] | "\(.name)=\(.value)"] | join("; ")' /tmp/cookies.json)
UA=$(tabd eval 'navigator.userAgent' --json | jq -r '.')

curl -sS \
  -H "Cookie: $COOKIE_HDR" \
  -H "User-Agent: $UA" \
  -H "Accept: application/json" \
  https://app.example.com/api/transactions
```

**Caveats**:
- Anti-bot checks may require additional `sec-ch-ua-*` / `Accept-Language`
  headers matching the chromium build. Extract via
  `tabd eval 'fetch("/__echo").then(r => r.headers)'` or just observe a
  real request via Pattern B and copy headers.
- CSRF tokens, request signatures, OAuth bearer tokens in non-cookie storage
  are not auto-included; pull them with `tabd storage-get` or
  `tabd eval 'document.querySelector("meta[name=csrf-token]").content'`.

### Choosing

| Need | Use |
|---|---|
| JSON API response, post-login | **Pattern A** |
| Drive a UI flow, observe what backend got hit | **Pattern B** (then A for body) |
| Run from CI without re-running login each time | **Pattern C** (cookie reuse) |

---

## 3. Save and restore a session

Daemon-resident cookies/storage survive across CLI calls but die if the
daemon (or chromium) is restarted. Persist them yourself:

### Save (right after a successful login)

```bash
tabd cookies-get https://app.example.com/ --json > ~/.cache/example-cookies.json

# localStorage + sessionStorage from the current origin
tabd storage-get --type local --json  > ~/.cache/example-local.json
tabd storage-get --type session --json > ~/.cache/example-session.json
```

### Restore (new daemon, fresh chromium)

```bash
# Need to be on the origin first, otherwise cookies-set / storage-set are scoped wrong
tabd navigate https://app.example.com/

# Cookies — one set call per cookie
jq -c '.[]' ~/.cache/example-cookies.json | while read -r c; do
  name=$(echo "$c" | jq -r '.name')
  value=$(echo "$c" | jq -r '.value')
  domain=$(echo "$c" | jq -r '.domain')
  path=$(echo "$c" | jq -r '.path')
  secure=$(echo "$c" | jq -r 'if .secure then "--secure" else "" end')
  tabd cookies-set --url https://app.example.com/ \
                   --name "$name" --value "$value" \
                   --domain "$domain" --path "$path" $secure
done

# localStorage
jq -r 'to_entries[] | "\(.key)\t\(.value)"' ~/.cache/example-local.json | \
  while IFS=$'\t' read -r k v; do
    tabd storage-set --key "$k" --value "$v" --type local
  done

# Navigate again so the restored cookies+storage take effect
tabd navigate https://app.example.com/dashboard

# Test
tabd wait-selector 'nav.user-menu' --timeout 5000 && echo "still logged in"
```

If the site uses HttpOnly session cookies, they survive this round-trip
(cookies-get reads them; cookies-set restores them). Tokens stored in
IndexedDB don't — IndexedDB is out of scope.

---

## 4. Multi-tab compare

Open two pages side-by-side, extract the same data from each, compare:

```bash
tabd navigate https://store-a.example.com/product/widget
tabd open-tab https://store-b.example.com/product/widget  # tabId 1

tabd list-tabs --json
# [{"tabId":0,...,"active":false},{"tabId":1,...,"active":true}]

# Extract from each, no need to activate-tab — every action accepts --tab N
PRICE_A=$(tabd get-text --selector '.price' --tab 0)
PRICE_B=$(tabd get-text --selector '.price' --tab 1)

echo "A: $PRICE_A  vs  B: $PRICE_B"
```

The active tab matters only for actions that don't take `--tab`
(`screenshot`, `eval`, all `cookies-*`, all `storage-*`). For those, switch
explicitly:

```bash
tabd activate-tab --tab 0
tabd screenshot --out /tmp/a.png
tabd activate-tab --tab 1
tabd screenshot --out /tmp/b.png
```

Close tabs you don't need (keeps RSS down):

```bash
tabd close-tab --tab 1
```

---

## 5. Scroll-driven infinite list

Pages that load more content as you scroll need a poll-scroll-poll loop:

```bash
#!/usr/bin/env bash
set -euo pipefail

tabd navigate https://news.example.com/feed
tabd wait-selector '.feed-item' --timeout 10000

prev=0
for _ in $(seq 1 30); do
  # Scroll to bottom of the document
  tabd eval 'window.scrollTo(0, document.body.scrollHeight); null'
  tabd wait-network-idle --idle-time 1000 --timeout 5000 || true

  count=$(tabd eval 'document.querySelectorAll(".feed-item").length' --json)
  echo "loaded: $count"

  # Stop when no new items appeared
  if [[ "$count" == "$prev" ]]; then
    break
  fi
  prev=$count
done

# Dump everything
tabd eval '[...document.querySelectorAll(".feed-item")].map(el => ({
  id: el.dataset.id,
  title: el.querySelector(".title")?.innerText,
  url: el.querySelector("a")?.href,
}))' --json > /tmp/feed.json
```

`wait-network-idle` is the key: it stops you from polling before the lazy
load actually resolved. `--idle-time 1000` is conservative for jittery SPAs.

---

## 6. CI / one-shot isolated daemon

Don't fight a user's long-running daemon — spin up a private one:

```bash
#!/usr/bin/env bash
set -euo pipefail

BASE="$(mktemp -d -t tabd-ci.XXXX)"
export TABD_BASE_DIR="$BASE"
trap 'tabd daemon stop --base-dir "$BASE" >/dev/null 2>&1 || true; rm -rf "$BASE"' EXIT

# Boot daemon; first action auto-spawns, but explicit start makes timing tractable
tabd daemon start --base-dir "$BASE" &
for _ in $(seq 1 30); do
  tabd daemon ping --base-dir "$BASE" >/dev/null 2>&1 && break
  sleep 0.5
done

# ... your scraping logic ...
tabd navigate https://example.com
tabd screenshot --out artifact.png
```

Because the daemon runs in a tempdir, it can coexist with the user's
default daemon without socket conflicts. The trap cleans up on any exit.

---

## 7. Wait until a single network call completes

`wait-network-idle` is generic. When you only care about *one specific
endpoint*, watch its log directly:

```bash
tabd click '#submit-order'

# Poll network-logs until the target request appears with a final status
for _ in $(seq 1 30); do
  result=$(tabd network-logs --url-contains '/api/orders' --method POST --limit 1 --json)
  status=$(printf '%s' "$result" | jq -r '.[0].status // empty')
  failed=$(printf '%s' "$result" | jq -r '.[0].failed // empty')
  if [[ -n "$status" || "$failed" == "true" ]]; then
    echo "$result" | jq '.[0]'
    break
  fi
  sleep 0.2
done
```

For body, follow with Pattern A from §2 to re-issue the request.

---

## Gotchas

- **`type-secret` doesn't always satisfy every framework.** It fires the
  native value setter + `input` event, which covers React / Vue / Svelte.
  If a site's submit handler also checks for `keydown` / `keyup` per
  character (rare, usually anti-bot), fall back to per-character `press-key`
  with the focused element.

- **`wait-network-idle --idle-time` of 500 ms is fine for static pages but
  short for SPAs with debounced fetchers.** Bump to 1500–2000 ms.

- **`type` clears the input first.** If you want to append to an existing
  value, use `tabd eval` with the native value setter, or use `type-secret
  --no-clear` for vault-resident appends.

- **`eval` returning `undefined` produces no `data` field.** That's why the
  pattern `return false; (nothing useful); null` is common. If you need a
  concrete return value, end with `null` or wrap in an IIFE returning a
  value.

- **`cookies-set` requires the URL or domain to already be "known" to the
  browser.** Always `navigate` to the origin first before cookies-set,
  otherwise the cookie may be silently rejected.

- **Daemon restart loses everything.** Cookies, storage, open tabs, and
  visited URLs are all in the chromium TempDir, which is destroyed on
  shutdown. Use §3 to persist what you need.

- **TOTP secrets belong in the vault, not in env vars.** Once `secret-put`
  has it, the daemon decrypts on-demand. Don't `export TABD_VAULT_KEY` in
  a shared shell — load it from a 0600 env file
  (`source ~/.config/tabd/vault.env` with `TABD_VAULT_KEY=...` inside).

- **`screenshot` captures the active tab.** No `--tab` option — switch
  with `activate-tab` first.

- **`network-logs --include-body` is deferred.** It's accepted as a flag
  but the daemon's reader task can't issue a CDP `Network.getResponseBody`
  RPC without deadlocking the registry mutex (see [architecture.md](architecture.md#reader-task-is-read-only)).
  Workaround: use Pattern A.

- **Long actions can race the daemon drain timeout.** If you call
  `tabd daemon stop` while a slow `screenshot` is in flight, it'll error
  after `$TABD_DRAIN_TIMEOUT_MS` (default 10s). Either wait, or increase
  the env var in your systemd unit (see [operations.md § Shutdown / drain
  semantics](operations.md#shutdown--drain-semantics)).
