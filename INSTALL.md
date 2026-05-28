# Installation

`tabd` is a single Rust binary. Linux and macOS only — Windows is permanently
unsupported because the daemon talks over Unix domain sockets.

## Requirements

- Rust 1.85+ (edition 2024) if installing from source
- A Chromium build available on disk
- 64-bit Linux (x86_64) or macOS (Intel / Apple Silicon)
- ~600 MB free for Chromium itself (we do not bundle it)

## Install

### From source (recommended)

```bash
git clone https://github.com/marshallku/browser.git
cd browser
cargo install --path crates/tabd
```

This produces `~/.cargo/bin/tabd`. Make sure `~/.cargo/bin` is on `PATH`.

### Pre-built binary

Each tag push runs `.github/workflows/binary-release.yml`, which uploads:

- `tabd-linux-x64`
- `tabd-darwin-x64`
- `tabd-darwin-arm64`

```bash
gh release download v0.x.y --pattern 'tabd-linux-x64'
chmod +x tabd-linux-x64
mv tabd-linux-x64 ~/.local/bin/tabd
```

macOS binaries are unsigned — `xattr -dr com.apple.quarantine tabd-darwin-arm64`
on first run.

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
