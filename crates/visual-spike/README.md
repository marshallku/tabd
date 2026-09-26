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

## The Fetch / auto-attach probes

`q4` / `q9` / `q10` / `q11` / `q12` measure against `fixture.rs`, a logging HTTP
origin, because `Fetch.requestPaused` firing proves interception happened — not
that the request stayed in. The decisive evidence is always the origin's own
request log, and `q0-http-smoke` exists to prove the browser can reach it at all
before any of them is believed.

The fixture binds **distinct loopback IPs** (127.0.0.1/.2/.3) rather than just
distinct ports: Chromium's site isolation does not treat the port as part of a
site, so a "cross-origin" iframe between two ports stays in the same renderer
and never becomes an OOPIF — which would quietly make `q10` and `q11` test
nothing. Where the alias cannot be bound (macOS without
`sudo ifconfig lo0 alias 127.0.0.2`) it falls back to 127.0.0.1 and those two
probes degrade to same-site.

These probes pass `--password-store=basic`. Without it, a locked session keyring
makes Brave put up a modal "the login keyring did not get unlocked" prompt and
then complete **no network request at all**: the navigation commits,
`Network.requestWillBeSent` fires, and nothing further happens — no response, no
failure, and the page session stops answering CDP. That is the normal state for
a browser driven over ssh. The flag is deliberately not used by
`q6-profile-copy`, whose question is whether the real profile's OSCrypt key
still works.

Navigations in these probes are sent fire-and-forget: `Page.navigate` only
returns once the navigation commits, and under interception the commit cannot
happen until the pause is answered — awaiting it deadlocks against our own
policy.
