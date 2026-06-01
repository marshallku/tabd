# Installation

`tabd` is a single Rust binary. Linux and macOS only — Windows is permanently
unsupported because the daemon talks over Unix domain sockets.

## Requirements

- A Chromium build available on disk
- 64-bit Linux (x86_64) or macOS (Intel / Apple Silicon)
- ~600 MB free for Chromium itself (we do not bundle it)
- Rust 1.85+ (edition 2024) *only* if installing from source

## Install

### Pre-built binary (recommended)

One line — detects your platform, downloads the matching binary, verifies its
SHA256, and installs to `~/.local/bin/tabd`:

```bash
curl -fsSL https://raw.githubusercontent.com/marshallku/tabd/master/install.sh | sh
```

Environment overrides:

| Variable | Default | Purpose |
|---|---|---|
| `TABD_VERSION` | latest release | Pin a tag, e.g. `TABD_VERSION=v0.1.0` |
| `TABD_INSTALL_DIR` | `~/.local/bin` | Where to drop the `tabd` binary |
| `TABD_NO_VERIFY` | unset | Skip SHA256 checksum verification |

Make sure the install dir is on `PATH` (the script warns if it is not). On macOS
the script clears the quarantine flag for you, since the binaries are unsigned.

Each tag push runs `.github/workflows/binary-release.yml`, which publishes
`tabd-linux-x64`, `tabd-darwin-x64`, `tabd-darwin-arm64`, and a `SHA256SUMS`
file to the GitHub Release. To install manually instead of the script:

```bash
gh release download v0.1.0 --pattern 'tabd-linux-x64'
chmod +x tabd-linux-x64
mv tabd-linux-x64 ~/.local/bin/tabd
```

### From source

```bash
git clone https://github.com/marshallku/tabd.git
cd tabd
cargo install --path crates/tabd
```

This produces `~/.cargo/bin/tabd`. Make sure `~/.cargo/bin` is on `PATH`.

## Chromium

The daemon launches its own Chromium with `--headless=new`. It looks for one in this
order:

1. `$BROWSER_EXECUTABLE` (explicit override)
2. `google-chrome` / `google-chrome-stable` / `chromium` / `chromium-browser` on `PATH`
3. The highest-version Chromium in `~/.cache/ms-playwright/chromium-*/chrome-linux64/chrome`
   (set up by `npx playwright install chromium` from any other project)

If none exists the daemon refuses to start. On a fresh Linux box without a desktop
environment, install the runtime libs too — `apt install libnss3 libatk-bridge2.0-0
libdrm2 libxkbcommon0 libgbm1 libasound2` covers the usual gaps. On Arch they are
mostly already present.

## First run

```bash
# Boot daemon in the background.
tabd daemon start &
# (or skip this; the first action below auto-spawns it.)

# Sanity check.
tabd daemon health
# → {"pid":12345, "ready":true, "driver":{"chromiumPid":12346, ...}, ...}

# Do something.
tabd navigate https://example.com
tabd get-text --selector h1
tabd daemon stop
```

If `daemon health` shows `"ready": false`, Chromium is still booting — try again in
~2 seconds.

## Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `BROWSER_EXECUTABLE` | auto-discover | Override Chromium binary path |
| `TABD_BASE_DIR` | `$XDG_RUNTIME_DIR/tabd` (or `~/.cache/tabd`) | Daemon socket + pid file directory |
| `TABD_NO_AUTO_SPAWN` | unset | Disable CLI auto-spawn of the daemon |
| `TABD_VAULT_KEY` | unset | Required for any `secrets.*` action |
| `TABD_VAULT_PATH` | `$XDG_CONFIG_HOME/tabd/secrets.enc` (or `~/.config/...`) | Secrets file location |

## Uninstall

```bash
tabd daemon stop || true
rm -f "$(command -v tabd)"
rm -rf "$XDG_RUNTIME_DIR/tabd"
rm -f "$XDG_CONFIG_HOME/tabd/secrets.enc"
```
