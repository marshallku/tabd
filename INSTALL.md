# Installation

`ai-browser` is a single Rust binary. Linux and macOS only — Windows is permanently
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
cargo install --path crates/ai-browser
```

This produces `~/.cargo/bin/ai-browser`. Make sure `~/.cargo/bin` is on `PATH`.

### Pre-built binary

Each tag push runs `.github/workflows/binary-release.yml`, which uploads:

- `ai-browser-linux-x64`
- `ai-browser-darwin-x64`
- `ai-browser-darwin-arm64`

```bash
gh release download v0.x.y --pattern 'ai-browser-linux-x64'
chmod +x ai-browser-linux-x64
mv ai-browser-linux-x64 ~/.local/bin/ai-browser
```

macOS binaries are unsigned — `xattr -dr com.apple.quarantine ai-browser-darwin-arm64`
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
ai-browser daemon start &
# (or skip this; the first action below auto-spawns it.)

# Sanity check.
ai-browser daemon health
# → {"pid":12345, "ready":true, "driver":{"chromiumPid":12346, ...}, ...}

# Do something.
ai-browser navigate https://example.com
ai-browser get-text --selector h1
ai-browser daemon stop
```

If `daemon health` shows `"ready": false`, Chromium is still booting — try again in
~2 seconds.

## Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `BROWSER_EXECUTABLE` | auto-discover | Override Chromium binary path |
| `AI_BROWSER_BASE_DIR` | `$XDG_RUNTIME_DIR/ai-browser-rs` (or `~/.cache/ai-browser-rs`) | Daemon socket + pid file directory |
| `AI_BROWSER_NO_AUTO_SPAWN` | unset | Disable CLI auto-spawn of the daemon |
| `AI_BROWSER_VAULT_KEY` | unset | Required for any `secrets.*` action |
| `AI_BROWSER_VAULT_PATH` | `$XDG_CONFIG_HOME/ai-browser/secrets.enc` (or `~/.config/...`) | Secrets file location |

## Uninstall

```bash
ai-browser daemon stop || true
rm -f "$(command -v ai-browser)"
rm -rf "$XDG_RUNTIME_DIR/ai-browser-rs"
rm -f "$XDG_CONFIG_HOME/ai-browser/secrets.enc"
```
