# Architecture

This is the design doc — *why* `tabd` is shaped the way it is. For the user-
facing surface (commands, flags, install), see the top-level [README](../README.md)
and [INSTALL.md](../INSTALL.md). For running it as a service, see
[operations.md](operations.md).

## Goal

SSH-friendly headless browser controller for AI agents and humans. One Rust
binary, one long-running daemon per user, one Chromium shared across all calls
to that daemon. JSON over a Unix domain socket. No Node, no Python, no MCP, no
remote dependency.

## Process layout

```
shell → tabd CLI ──┐
                   ├── /tmp/…/daemon.sock ──> tabd daemon ──> chromium (CDP/WS)
shell → tabd CLI ──┘                                 │
                                                     ├── supervise task (Linux: /proc poll)
                                                     └── secrets vault (lazy-init)
```

- The **CLI** (`tabd <action>`) is short-lived. It parses argv, encodes the
  request, sends it over the daemon socket, decodes and renders the response,
  exits. It auto-spawns the daemon if no socket exists (detached fork — child
  inherits no stdio and is gated by `$TABD_NO_AUTO_SPAWN` so it can't loop).
- The **daemon** (`tabd daemon start`) owns the Chromium process and one
  `CdpClient` connected to it over `/json/version` → `webSocketDebuggerUrl`.
  It accepts newline-delimited JSON-RPC over its socket.
- One **Chromium** per daemon. All tabs are managed via CDP `Target.*` —
  `Target.createTarget` to open, `Target.closeTarget` to close,
  `Target.attachToTarget` to start a session per tab. There is no separate
  browser process per tab and no `BrowserContext` fan-out.

## Source layout

| File | Role |
|---|---|
| `crates/tabd/src/main.rs` | `clap` router. `daemon ...` subcommand + external-subcommand catch-all → `cli::run`. |
| `crates/tabd/src/cli.rs` | argv parser, dispatch table (38 actions + `secret-put` custom branch), daemon auto-spawn, render. |
| `crates/tabd/src/daemon.rs` | UDS server, shared state/helpers, request dispatch, supervisor, vault state. |
| `crates/tabd/src/daemon/` | Action handlers grouped by domain (`dom`, `interaction`, `tabs`, `storage`, `capture`, `monitor`, `waits`, `secrets`). |
| `crates/tabd/src/cdp.rs` | JSON-RPC over WS, multi-tab `TabRegistry`, reader task with event routing. |
| `crates/tabd/src/browser.rs` | Chromium discovery + launch with random debug port. |
| `crates/tabd/src/secrets.rs` | AES-256-GCM + PBKDF2-SHA256 file vault. |
| `crates/tabd/src/cmd/` | Page-injected helper expressions reused by the daemon (`eval`, `page` navigate, `get_text` body). |

## Key design decisions

### One Chromium per daemon, not per call

A `chromium --headless=new` cold start costs ~600 ms and ~150 MB RSS before the
first navigation. Running it per CLI call would dominate every command. The
daemon keeps one Chromium hot; subsequent calls reuse cookies, storage,
console history, and any tabs you opened.

The trade-off is that one user-space crash takes everything down, which is why
there is a supervisor task (below).

### Multi-tab via `TabRegistry`, not separate browsers

`TabRegistry` is `{ tabs: HashMap<targetId, TabState>, active: Option<targetId> }`
behind an `Arc<Mutex<…>>`. `TabState` carries the CDP `sessionId`, three ring
buffers (console, page-errors, network), and a `network_pending` counter for
`wait-network-idle`. Every action takes an optional `--tab N` (1-based index);
falsy values fall through to the active tab.

This sits at a lower level than Playwright's `BrowserContext`. We picked the
lower-level model because most real workflows want one logical session with
multiple tabs (multi-tab login flows, comparison browsing), not isolated
contexts per script.

### Reader task is read-only

The reader task owns the websocket read half and routes incoming CDP events
(`Runtime.consoleAPICalled`, `Runtime.exceptionThrown`, `Network.*`) into the
matching `TabState`. **It never calls `dispatch()`** — that would re-enter the
registry mutex it already holds, deadlocking. Anything that needs a CDP RPC
in response to an event (e.g. fetching response bodies for `networkLogs
--include-body`) has to spawn a separate task with an `Arc<CdpClient>` clone.
This is why body fetch is currently deferred.

### Native value setter for controlled inputs

React / Vue controlled inputs ignore `value =` assignment because their
re-render path overwrites the assignment. `type-secret` uses
`Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set`
plus a synthetic `input` event so the framework sees a "real" user input.
This is in `cmd/eval.rs`-adjacent helpers.

### Secrets vault is a single AES-GCM file

`secret-put` / `type-secret` / `secret-list` / `secret-delete` operate on a
JSON envelope at `$XDG_CONFIG_HOME/tabd/secrets.enc` (override with
`$TABD_VAULT_PATH`). The file is encrypted with AES-256-GCM; the key is
derived from `$TABD_VAULT_KEY` via PBKDF2-SHA256 (200 000 iterations, 16-byte
random salt). Each record has its own 12-byte IV and 16-byte auth tag.

- `secret-list` never decrypts — it returns the label and a fixed `****`
  preview so callers can enumerate without the key.
- Wrong passphrase is detected on reopen by decrypting the first record as a
  sanity check, before any subsequent action sees a corrupted plaintext.
- macOS Keychain / libsecret integration is deferred. Passphrase mode only.

Plaintext secret values never appear on argv. `secret-put` reads the value
from `--from-env VAR`, `--from-file PATH`, or `--stdin`.

### Daemon ↔ chromium supervisor (Linux)

A `tokio::spawn`'d task polls `/proc/{chromium_pid}/status` every 2 seconds.
It treats `State: Z` (zombie) and `State: X` (dead) as "not alive" so the
daemon doesn't sit forever holding a corpse. On crash:

1. Drop the dead `CdpClient` and `Browser`.
2. Bump `restart_attempts`.
3. Boot a new Chromium + CDP client with exponential backoff (200 ms → 2 s,
   5 attempts).
4. On success, flip `ready=true` and `notify_waiters` so blocked actions wake.
5. On failure (all 5 attempts), the supervisor loop continues — next poll
   tries again. The daemon process itself never exits from a chromium crash;
   it just stays in `ready=false`. `tabd daemon health` reports this.

## Concurrency model

- **Tokio multi-thread runtime.** Reader task, supervisor task, drain task,
  and each accepted client connection all run as `tokio::spawn`'d tasks.
- **`action_mutex`** on the daemon serializes action execution. Actions are
  not concurrent on the same Chromium because most CDP calls modify shared
  state (active tab, network log, console buffers). The mutex is fine-grained
  enough that `daemon.health` / `daemon.ping` skip it.
  - **Held for the whole action, including the polling sleeps inside waits**
    (`wait.selector` / `wait.url` / `wait.networkIdle`, and the implicit
    visible-wait before `interaction.click` / `type`). This is deliberate: it
    gives linear, predictable semantics — a `navigate`/`close` can't interleave
    into a tab another action is mid-wait on. The cost is that a long wait
    blocks every other client; that worst case is bounded by `MAX_WAIT_MS`
    (5 min) and the per-RPC timeout, and the daemon is single-user behind a
    `0700` dir / `0600` socket, so there is no untrusted client to starve.
  - Releasing the lock during waits, or moving to per-tab locks for cross-tab
    concurrency, was considered and rejected: it trades predictable
    serialization for interleaving-correctness risk that isn't worth it for
    this overwhelmingly-sequential single-user usage. Revisit if real
    concurrent-multi-tab demand shows up.
- **Registry mutex** is held only across the registry read/write itself,
  never across a CDP RPC. Long-held mutex would block the reader task from
  routing events into `TabState`.
- **Single ownership of WS read half** belongs to the reader task. Writes go
  through an mpsc channel; the writer half is owned by the dispatch path.
  This avoids the classic "two tasks calling `WebSocketStream::send`" race.

## Failure model

| What can fail | What happens | What the user sees |
|---|---|---|
| Chromium crash | Supervisor reboots within ~5 s | `restart_attempts` bumps; current in-flight actions error with `connection lost` |
| Daemon process crash | systemd / launchd restarts (see operations.md) | Next CLI call auto-spawns a fresh daemon |
| Bad request (bad selector, missing tab) | Daemon returns `{success: false, error: "..."}` | CLI exits 1 with the error string on stderr |
| Wrong `TABD_VAULT_KEY` | First decrypt sanity check fails | `vault open failed: ...`; daemon stays up |
| Chromium not found at boot | Daemon fails before binding the socket; CLI auto-spawn picks this up and reports it | `no Chromium binary found. Set $BROWSER_EXECUTABLE, …` |

## Constraints

- **Linux + macOS only.** Daemon talks over Unix domain sockets; Windows
  would need a different transport. Out of scope.
- **One Chromium per daemon, one daemon per `$TABD_BASE_DIR`.** Run multiple
  isolated daemons in parallel by setting different `TABD_BASE_DIR` values
  (CI uses this).
- **No screenshot diff baked in.** `screenshot` writes a PNG; comparison is
  caller's responsibility.
- **No body fetch in `network-logs --include-body`.** See "Reader task is
  read-only" above for why this is deferred.
- **No remote daemon.** Socket is local. SSH from another host into a box
  where `tabd daemon` is running gives the same shared Chromium — that is
  the SSH-friendly use case. There is no "tabd connect <host>".
