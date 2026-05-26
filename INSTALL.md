# Installation guide

End-to-end setup for `ai-browser`. The repo runs on Linux and macOS; Windows is not supported natively (the daemon uses Unix domain sockets) — use WSL 2 instead.

For day-2 operations (systemd / launchd, health observation, drain semantics, migration notes), see [`docs/operations.md`](./docs/operations.md).

## System requirements

| Requirement | Notes                                                                                                          |
| ----------- | -------------------------------------------------------------------------------------------------------------- |
| Node.js     | 20.x or newer (24.x recommended). No `engines` field is enforced — use a current LTS.                          |
| OS          | Linux (x64/arm64) or macOS (Intel/Apple Silicon). Windows users: install inside WSL 2.                         |
| Disk        | ~600 MB free for the Playwright Chromium download (`~/.cache/ms-playwright`).                                  |
| Network     | Required during the initial install (npm registry + Playwright CDN). The daemon itself only reaches localhost. |

Optional, only if you want a persistent secret vault backed by the OS keychain:

- macOS: `security` (preinstalled).
- Linux: `secret-tool` (`libsecret-tools` on Debian/Ubuntu, `libsecret` on Arch, `libsecret` on Fedora). Skip if you prefer the passphrase mode.

## Platform prerequisites

Playwright's bundled Chromium pulls in a small set of native libraries. The "Quick install" path below calls `npx playwright install chromium` which downloads the browser binary but does **not** install OS packages — you may also need `npx playwright install-deps chromium` on a fresh Linux box.

### Debian / Ubuntu

```bash
apt update
apt install -y nodejs npm git
# Playwright runtime deps (covers Chromium):
npx playwright install-deps chromium
# Optional, for OS-keychain-backed secrets:
apt install -y libsecret-tools
```

### Arch / Manjaro

```bash
pacman -S --needed nodejs npm git
# Playwright Chromium runtime libs (most are usually already in a desktop install):
pacman -S --needed nss libxss alsa-lib gtk3 libdrm libxcomposite libxdamage \
                          libxrandr libgbm pango cairo
# Optional, for OS-keychain-backed secrets:
pacman -S --needed libsecret
```

### Fedora / RHEL

```bash
dnf install -y nodejs npm git
dnf install -y nss libXScrnSaver alsa-lib gtk3 libdrm libXcomposite libXdamage \
                    libXrandr mesa-libgbm pango cairo
# Optional:
dnf install -y libsecret
```

### macOS

```bash
# Node via Homebrew (skip if you already have it via nvm/asdf/volta)
brew install node git
# No extra native deps needed — Playwright downloads a self-contained Chromium,
# and the `security` keychain CLI ships with macOS.
```

### Windows (WSL 2)

Install Ubuntu 22.04+ in WSL, then follow the Debian/Ubuntu instructions above. The daemon's Unix socket lives in `$XDG_RUNTIME_DIR` (created by `systemd --user` in WSL) or `~/.cache/ai-browser/`. Native Windows is not supported.

## Quick install (recommended)

One script handles `npm install` → Playwright Chromium → `npm run build` → patches MCP client configs:

```bash
git clone https://github.com/marshallku/ai-browser
cd ai-browser
./scripts/install.sh
```

By default this registers `ai-browser` in **both** Claude Code (`~/.claude.json`) and Codex CLI (`~/.codex/config.toml`) when their config files (or parent directories) exist. The script is idempotent — re-running updates the existing entry and saves the previous config as `<file>.bak.<timestamp>`.

### Common flags

```bash
./scripts/install.sh --target claude       # only Claude Code
./scripts/install.sh --target codex        # only Codex CLI
./scripts/install.sh --dry-run             # print plan, no writes
./scripts/install.sh --headless 0          # launch Chromium with a visible window
./scripts/install.sh --user-data-dir ~/.cache/ai-browser/profile  # persistent profile
./scripts/install.sh --name browser        # MCP server key in the client config
```

| Flag              | Default      | Purpose                                          |
| ----------------- | ------------ | ------------------------------------------------ |
| `--target`        | `both`       | `claude`, `codex`, or `both`                     |
| `--runtime`       | `playwright` | `playwright` or `chromium-cdp`                   |
| `--headless`      | `1`          | `0` to show the browser window                   |
| `--executable`    | auto         | Override the Chromium binary path                |
| `--user-data-dir` | (temp)       | Persistent profile directory                     |
| `--name`          | `ai-browser` | MCP server key written into the client config    |
| `--skip-install`  | —            | Skip `npm install` (use existing `node_modules`) |
| `--skip-build`    | —            | Skip `npm run build` (use existing `dist/`)      |
| `--dry-run`       | —            | Print what would change without writing          |

After install, restart your MCP client (Claude Code, Codex, etc.) so it re-reads the config.

## Manual install (step by step)

If you prefer to drive each step yourself:

```bash
git clone https://github.com/marshallku/ai-browser
cd ai-browser

npm install                                  # 1. install dependencies
npx playwright install chromium              # 2. download bundled Chromium (~250 MB)
# On a fresh Linux box you may also need:
# npx playwright install-deps chromium
npm run build                                # 3. compile TypeScript -> dist/
```

Then register the MCP server in your client (see "MCP client registration" below).

## MCP client registration

`scripts/install.sh` does this for you. If you want to wire it up by hand:

### Claude Code (`~/.claude.json`)

```jsonc
{
  "mcpServers": {
    "ai-browser": {
      "type": "stdio",
      "command": "/absolute/path/to/ai-browser/scripts/run-mcp.sh",
      "args": [],
      "env": {
        "BROWSER_RUNTIME": "playwright",
        "BROWSER_HEADLESS": "1"
      }
    }
  }
}
```

### Codex CLI (`~/.codex/config.toml`)

```toml
[mcp_servers.ai-browser]
command = "/absolute/path/to/ai-browser/scripts/run-mcp.sh"

[mcp_servers.ai-browser.env]
BROWSER_RUNTIME = "playwright"
BROWSER_HEADLESS = "1"
```

### Once published to npm

After the package is on npm, clients can spawn it via `npx` without a local checkout:

```jsonc
{
  "mcpServers": {
    "ai-browser": {
      "command": "npx",
      "args": ["-y", "@marshallku/ai-browser"]
    }
  }
}
```

Until then, the `run-mcp.sh` path is the working option.

## Verification

After install, confirm the daemon spawns and Chromium responds:

```bash
# 1. Auto-spawn the daemon on first command.
./bin/ai-browser.js navigate https://example.com
./bin/ai-browser.js get-text --selector h1
# Expected output: "Example Domain"

# 2. Inspect daemon health.
./bin/ai-browser.js daemon health
# Look for: "ready": true, "accepting": true, driver.chromiumPid populated.

# 3. (Optional) Smoke test against a data: URL — no network.
npm run smoke:playwright
```

If you installed `ai-browser` on `$PATH` (via `install.sh` or a future `npx`), drop `./bin/` from the commands above.

## Persistent secret store (optional)

The daemon keeps secrets in memory by default — they evaporate on restart. To keep them across restarts, see the full setup in [the main README's "Persistent secret store" section](./README.md#persistent-secret-store). Quick version:

**Passphrase mode** (simplest, works in CI):

```bash
export AI_BROWSER_SECRET_STORE=persistent
export AI_BROWSER_VAULT_KEY='a-strong-passphrase'
ai-browser daemon restart
ai-browser secret-put --from-env GMAIL_PW --label gmail --json
```

**OS keychain mode** (no env var to manage):

```bash
unset AI_BROWSER_VAULT_KEY                    # forces keychain fallback
export AI_BROWSER_SECRET_STORE=persistent
ai-browser daemon restart
# First put auto-creates a random 32-byte key in:
#   macOS — Keychain entry "ai-browser-vault" (service "ai-browser")
#   Linux — libsecret entry {service=ai-browser, key=vault}
```

If neither `security` (macOS) nor `secret-tool` (Linux) is available and no passphrase is set, the store init fails fast with an actionable error.

## Long-running daemon (systemd / launchd)

For an always-on daemon supervised by the OS, see [`docs/operations.md`](./docs/operations.md). Templates for `systemd --user` and `launchd` are included there along with restart-rate gates that match the daemon's own three-strikes policy.

## Upgrading

```bash
cd ai-browser
git pull
npm install
npm run build
ai-browser daemon restart
```

If a new Playwright version bundles a different Chromium revision, also re-run `npx playwright install chromium`. The daemon's crash-restore path replays open URLs and `storageState` into the new browser, so an in-place restart usually preserves your session.

## Uninstall

```bash
# 1. Stop the daemon and any one-off ephemeral processes.
ai-browser daemon stop

# 2. Remove the MCP entries (install.sh saved the previous config as *.bak.* — restore those).
# Claude Code: edit ~/.claude.json and drop mcpServers.ai-browser
# Codex CLI:   edit ~/.codex/config.toml and drop the [mcp_servers.ai-browser] section.

# 3. Clean up runtime state.
rm -rf "${XDG_RUNTIME_DIR:-$HOME/.cache}/ai-browser"  # socket + pid file
rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/ai-browser"  # persistent secrets, if any

# 4. Remove the checkout itself.
rm -rf /path/to/ai-browser

# 5. Optional: drop the Playwright Chromium cache (~250 MB).
rm -rf "$HOME/.cache/ms-playwright"
```

To remove the keychain-stored master key (only if you used keychain mode):

```bash
# macOS
security delete-generic-password -a ai-browser -s ai-browser-vault
# Linux
secret-tool clear service ai-browser key vault
```

## Troubleshooting

**`chromium: cannot find Chromium binary`**
Playwright did not download the bundled browser, or the cache path is non-standard. Fix:

```bash
npx playwright install chromium
# Or pin the path:
export BROWSER_EXECUTABLE=/path/to/chrome
```

**`Failed to launch: error while loading shared libraries: libnss3.so`** (or similar `lib*.so` errors on Linux)
You're missing Chromium's native deps. Run:

```bash
npx playwright install-deps chromium
```

or install the system packages listed under "Platform prerequisites" for your distro.

**`bind EADDRINUSE` / `daemon already running`**
A previous daemon left a stale socket. The single-instance check should clean this up automatically; if it does not:

```bash
ai-browser daemon status      # confirms whether one is alive
ai-browser daemon stop        # graceful shutdown
# As a last resort (only if status reports stopped but the socket file exists):
rm "${XDG_RUNTIME_DIR:-$HOME/.cache}/ai-browser/daemon.sock"
```

**`No vault key available`** when enabling persistent secrets
Either set `AI_BROWSER_VAULT_KEY` (passphrase mode) or install the OS keychain CLI for your platform (`security` on macOS — preinstalled; `secret-tool` on Linux). See "Platform prerequisites".

**MCP client doesn't see the tools**

1. Restart the client — MCP configs are read on startup.
2. Confirm the registered command is executable: `ls -l scripts/run-mcp.sh`.
3. Run `scripts/run-mcp.sh` by hand — it should hang waiting for stdio (that's correct). If it exits immediately, the error on stderr explains why.

**Two MCP sessions see different tabs / cookies**
Both sessions are supposed to share one daemon. If they appear isolated, you're probably running an outdated build that still honors `AI_BROWSER_MCP_MODE=standalone`. Pull the latest, `npm run build`, restart the client. The flag is accepted-but-ignored in current builds.
