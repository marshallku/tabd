# tabd

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
  one within seconds.
- **AES-256-GCM secrets vault** — `secret-put` / `type-secret` for login
  automation; plaintext never goes on argv.
- **`--json` everywhere** — every action accepts `--json` for scriptable output;
  `--out FILE` decodes binary payloads (e.g. PNG screenshots) to disk.

## Install

See [INSTALL.md](./INSTALL.md). Short version — no build from source needed:

```bash
curl -fsSL https://raw.githubusercontent.com/marshallku/tabd/master/install.sh | sh
```

Downloads the right pre-built binary for your platform (Linux x64, macOS
Intel/Apple Silicon), verifies its SHA256, and installs to `~/.local/bin/tabd`.
From source instead: `cargo install --path crates/tabd`.

### AI agent skill (optional)

If you use Claude Code or Codex CLI, install the embedded skill so the agent
automatically uses `tabd` for browser-automation requests:

```bash
tabd skill install            # auto-detects Claude / Codex, installs to both
tabd skill install --target codex --force   # explicit one-target, overwrite
tabd skill install --path .claude/skills/tabd   # project-local
```

Writes `SKILL.md` + the four reference docs (`.claude/skills/tabd/*.md`) into
`~/.claude/skills/tabd/` and/or `~/.codex/skills/tabd/`. Restart the client
afterwards so it reloads skill metadata.

## Surface

47 action subcommands + 4 daemon controls.

| Category | Commands |
|---|---|
| **Tabs** | `navigate`, `open-tab`, `close-tab`, `list-tabs`, `activate-tab`, `back`, `forward`, `reload` |
| **DOM** | `get-html`, `get-text`, `query`, `summary` |
| **Interaction** | `click`, `type`, `hover`, `mouse-move`, `scroll`, `press-key`, `select-option`, `check`, `upload` |
| **Capture** | `screenshot`, `metrics` |
| **Emulation** | `set-viewport` |
| **Execution** | `eval` |
| **Wait** | `wait-selector`, `wait-url`, `wait-text`, `wait-network-idle`, `wait-download` |
| **Cookies** | `cookies-get`, `cookies-set`, `cookies-delete` |
| **Storage** | `storage-get`, `storage-set`, `storage-clear` |
| **Monitor** | `console-logs`, `page-errors`, `network-logs`, `dialogs`, `downloads` |
| **Dialogs** | `dialog-policy` (JS dialogs auto-handled by the daemon — never wedge automation) |
| **Downloads** | `download-dir` (opt-in capture → known dir), `wait-download` |
| **Secrets** | `secret-put`, `secret-list`, `secret-delete`, `type-secret` |
| **Daemon** | `daemon start`, `daemon stop`, `daemon ping`, `daemon health` |
| **Visual (owner)** | `browser`, `service install/uninstall/status`, `profile import/cutover/discard` — see [Visual mode](#visual-mode) |

Every action that targets a specific tab accepts `--tab N` (1-based index).
Defaults to the active tab.

## Quick start

```bash
# 1. Boot daemon (auto-spawn on first action also works).
tabd daemon start &

# 2. Drive it.
tabd navigate https://example.com
tabd get-text --selector h1                    # → "Example Domain"
tabd screenshot --out /tmp/example.png
tabd daemon health                             # daemon + chromium pids, RSS, restart count

# 3. Multi-tab.
tabd open-tab https://news.ycombinator.com     # returns {tabId, targetId, url}
tabd list-tabs --json                          # all open tabs with active flag
tabd activate-tab --tab 1
tabd back

# 4. Monitor what just happened.
tabd console-logs --json
tabd network-logs --method GET --status 2xx --limit 20

# 5. Login automation (passphrase-mode secrets vault).
export TABD_VAULT_KEY="$(pass show tabd/vault 2>/dev/null || echo 'change-me')"
echo -n "$GITHUB_PASSWORD" | tabd secret-put --label github --stdin
# → {"secretId":"a1b2c3...", "label":"github", "preview":"****", ...}
tabd navigate https://github.com/login
tabd type     '#login_field' marshallku
tabd type-secret '#password' --secret-id a1b2c3...
tabd click    '[name=commit]'
tabd wait-url 'https://github.com/*' --pattern-type glob

# 6. Stop when done.
tabd daemon stop
```

## Visual mode

Everything above drives a **throwaway headless Chromium**. Visual mode is the
other thing tabd can do: drive **your everyday browser** — your real profile,
your logins, your extensions — over a pipe no other process can reach, so you
can click a link and have it open there.

It is owner-only today. A visual daemon serves `daemon.*` and `browser.*` and
rejects every driver action with `visual_mode_unsupported`, so an agent cannot
reach your browser through it. Connecting agents to it safely is a later
phase.

### Platforms

| | Linux | macOS | Windows |
|---|---|---|---|
| `tabd browser`, `service install` | yes | yes | no — tabd is Unix-only (unix sockets, `fork`/`exec`) |
| Launcher it installs | `~/.local/share/applications/tabd.desktop` | `~/Applications/tabd.app` (generated with `osacompile`, ad-hoc signed) |  |
| Background service (`--enable-service`) | systemd user unit | LaunchAgent (`Aqua` only) | |
| Set as default browser | `--set-default` (needs `xdg-settings`) | manual, in System Settings — macOS has no supported non-interactive way | |

Honouring `$XDG_DATA_HOME` / `$XDG_CONFIG_HOME` on Linux. `--enable-service`
needs systemd; without it the files still install and `tabd browser` starts the
daemon on demand.

### Move your existing profile in — before you launch it

Visual mode uses its own profile directory, empty to begin with. If you want
your real one — cookies, logins, extensions — bring it across **first**:
`cutover` publishes with an atomic rename onto an empty destination, so once
you have launched the visual browser even once there is a profile in the way.

**Quit your browser completely**, then:

```bash
tabd profile import      # copies to <profile>.staging; never touches the original
# …follow the printed command to open the copy and check your logins…
tabd profile cutover     # publishes it with one atomic rename
tabd profile discard     # or throw the copy away
```

The original is only ever read, so a bad import costs a wasted copy. `import`
refuses while a browser is running, aborts if one opens the profile mid-copy,
and `cutover` refuses if the original changed in between.

Already launched and now want to import? The destination is not empty, so
`cutover` will refuse. Stop the visual daemon, move that profile aside, and
import into the free slot:

```bash
tabd daemon stop --visual   # closes the browser too — they share a lifetime
tabd service status         # prints the profile path; move it aside by hand
tabd profile import && tabd profile cutover
```

### Use it

```bash
# 1. Register tabd with the OS. Does NOT change your default browser and does
#    NOT start anything in the background — both are separate opt-ins.
tabd service install
tabd service status

# 2. Bring your real profile across first, if you want it (previous section).

# 3. Open something. Starts the visual daemon and the browser if needed.
tabd browser https://example.com
tabd browser                      # just bring the browser up
```

To have *clicked links* land in it, make tabd your default browser — Linux
`tabd service install --set-default`, macOS System Settings → Desktop & Dock →
Default web browser → tabd. Undo by picking your previous browser the same way.

Check URL delivery works **without** changing your default, using a private
scheme the installed launcher also claims:

```bash
gio open tabd://hello     # Linux  (not xdg-open — see below)
open    tabd://hello      # macOS
tabd service status       # shows the last delivery
```

On Linux this is deliberately `gio open`: measured on xdg-utils 1.2.1,
`xdg-open` routes any URL whose scheme it does not recognise straight to
`$BROWSER` / `x-www-browser` and never consults the scheme handler. http/https
are unaffected — those go through the default-browser setting.

### Two things worth knowing

- **The daemon owns the browser.** They live and die together: if the daemon
  stops, the browser closes.
- **Turn on "Continue where you left off"** in the browser's startup settings.
  Measured: without it, a browser that exits comes back with a single new-tab
  page and no restore prompt — so a daemon crash loses your tabs. tabd cannot
  set this for you (Chromium protects the preference); `tabd service status`
  reports whether it is on.

`navigator.webdriver` is `true` browser-wide while tabd drives it — an accepted
trade-off of the pipe transport. Keep sensitive sites on your ordinary browser
profile.

## Architecture

```
tabd CLI ──┐
           ├── /tmp/…/daemon.sock ──> tabd daemon ──> chromium (CDP/WS)
tabd CLI ──┘                              │
                                          ├── supervise task
                                          └── secrets vault (AES-256-GCM)
```

- **Daemon** owns one Chromium and a `TabRegistry` (targetId → sessionId + per-tab
  ring buffers for console/page-errors/network).
- **Reader task** routes CDP events into the matching `TabState` — no RPC calls
  from inside the reader (would self-deadlock the registry mutex).
- **Supervise task** checks Chromium liveness every 2 s (`try_wait()` on the
  owned child — cross-platform); on crash it rebuilds the Chromium + CDP
  client with exponential backoff (5 attempts).
- **CLI dispatcher** auto-spawns the daemon if no socket exists, then routes the
  subcommand to the matching daemon action over UDS.
- **Secrets vault** is a single AES-256-GCM file at
  `$XDG_CONFIG_HOME/tabd/secrets.enc`, key derived from
  `$TABD_VAULT_KEY` via PBKDF2-SHA256 (200 000 iters). `secret-list`
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
cargo build --release --manifest-path crates/tabd/Cargo.toml

# Test
cargo test --bins --manifest-path crates/tabd/Cargo.toml         # 120 unit
bash tests/cli-direct-smoke.sh                                          # CLI smoke (auto-spawn, render, error contracts)
bash tests/spike-daemon-compat.sh                                       # 39 cases (real Chromium)
```

`crates/tabd/src/`:

- `main.rs` — clap router for `daemon ...` + external_subcommand → `cli::run`
- `cli.rs` — argv parser, dispatch table, daemon auto-spawn, render
- `daemon.rs` — UDS server, action handlers, supervisor, vault state
- `cdp.rs` — JSON-RPC over WS, multi-tab registry, event routing
- `browser.rs` — Chromium launch + DevTools port discovery
- `secrets.rs` — AES-GCM + PBKDF2 file vault
- `cmd/` — helper expressions (text/AX/find-all) used by the daemon handlers

## Docs

- [`commands.md`](.claude/skills/tabd/commands.md) — per-action reference: positional
  args, every `--flag`, return shapes, error strings. The thing you'll
  actually keep open while writing a script.
- [`cookbook.md`](.claude/skills/tabd/cookbook.md) — full scenarios stitched
  together: 2FA login + data extract, three patterns for capturing API
  responses, session save/restore, infinite scroll, isolated CI daemon,
  gotchas.
- [`architecture.md`](.claude/skills/tabd/architecture.md) — why `tabd` is shaped
  this way (daemon, multi-tab registry, reader task, supervisor, secrets
  vault).
- [Visual mode](#visual-mode) — driving your everyday browser instead of a
  throwaway one: `tabd browser`, `service install`, `profile import`.
- [`operations.md`](.claude/skills/tabd/operations.md) — running `tabd` as a
  long-lived service: systemd user unit, launchd LaunchAgent, shell-rc
  fallback, drain semantics, health watchdog, troubleshooting.
