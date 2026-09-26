# visual-spike

V0 verification spike for **visual mode** (`docs/visual-mode-plan.md` §9).

This is not shipped code. It is a separate crate — not a `[[bin]]` of
`crates/tabd` — so the released binary, `install.sh` and both CI workflows stay
untouched by it. It talks CDP over `--remote-debugging-pipe` with blocking std
I/O and two dependencies (`serde_json`, `libc`), because the point is to check
what the OS and the browser actually do, not to prototype the eventual async
transport in `crates/tabd/src/cdp.rs`.

## Safety

The only way a probe obtains a `--user-data-dir` is `Scratch::new`, which
`mkdtemp`s a fresh 0700 directory. There is no flag that points the spike at an
existing directory, and `Drop` removes only what the type created. A launch is
additionally refused if the resolved path is the real Brave profile, an
ancestor of it, or a descendant of it — symlinks included. Q6 reads the real
profile with `cp -a` and never writes to it.

## Running

Probes drive a visible browser and (on Linux) talk to Hyprland, so they need a
graphical session. Over ssh that means exporting it explicitly:

```sh
export XDG_RUNTIME_DIR=/run/user/1000 \
       WAYLAND_DISPLAY=wayland-1 \
       DISPLAY=:1 \
       XDG_SESSION_TYPE=wayland \
       DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
       HYPRLAND_INSTANCE_SIGNATURE=$(ls "$XDG_RUNTIME_DIR/hypr" | head -1)

cargo run --release -- run --all --out-dir ./visual-spike-out
```

`XDG_SESSION_TYPE=wayland` is not optional: without it Brave picks the X11
ozone backend, and from a bare ssh shell it then dies with `Missing X server or
$DISPLAY`.

Options: `--all`, individual probe ids, `--interactive` (probes needing a human
click), `--keep` (leave scratch profiles behind), `--login-url <url>` (a site
you are already signed into, for Q6's functional check), `--out-dir <dir>`.

Output is one JSON object per probe on stdout; a readable summary goes to
stderr. Exit code 1 if any probe returned `fail`.

`q6-profile-copy` always runs last and refuses to start while any Brave is
running, so a browser leaked by an earlier probe makes it abort loudly instead
of producing a wrong answer.
