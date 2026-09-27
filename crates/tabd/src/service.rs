//! `tabd service install` — make the OS able to reach `tabd browser`.
//!
//! Two platform roles from the table in `docs/visual-mode-plan.md` §3:
//! `ServiceInstall` (a systemd user unit / a LaunchAgent) and `DefaultBrowser`
//! (a `.desktop` entry / a wrapper `.app`).
//!
//! ## What this deliberately does not do
//!
//! **It never makes tabd your default browser on its own.** On Linux that
//! needs an explicit `--set-default`; on macOS it is a user gesture in System
//! Settings that this command only prints instructions for. Silently taking
//! over every link on someone's machine is not something an install step gets
//! to decide.
//!
//! **It does not start the service either**, unless asked with
//! `--enable-service`. `tabd browser` already starts the daemon on demand and
//! inherits the graphical environment from whatever launched it; a
//! system-managed service has to be handed that environment separately, and
//! getting that wrong means a browser that cannot open a window.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Where the installed files go. `$HOME` in normal use; a scratch directory in
/// tests, which is why it is a parameter rather than a lookup.
#[derive(Debug, Clone)]
pub struct Prefix(PathBuf);

impl Prefix {
    pub fn home() -> Result<Self> {
        Ok(Prefix(
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME not set")?,
        ))
    }

    pub fn at(dir: &str) -> Self {
        Prefix(PathBuf::from(dir))
    }

    fn join(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }

    /// Where user data files go. `$XDG_DATA_HOME` for a real install, so the
    /// desktop entry lands inside the desktop's application search path; a
    /// scratch prefix keeps the layout relative so tests stay hermetic.
    fn data_home(&self) -> PathBuf {
        self.xdg("XDG_DATA_HOME", ".local/share")
    }

    /// Where user config files go. `$XDG_CONFIG_HOME` for a real install —
    /// `systemctl --user` reads units from there, not from a hardcoded
    /// `~/.config`.
    fn config_home(&self) -> PathBuf {
        self.xdg("XDG_CONFIG_HOME", ".config")
    }

    fn xdg(&self, var: &str, fallback: &str) -> PathBuf {
        if self.is_home()
            && let Some(value) = std::env::var_os(var)
        {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                return path;
            }
        }
        self.0.join(fallback)
    }

    /// Whether this prefix is the real `$HOME`.
    ///
    /// Service-manager commands (`systemctl --user`, `launchctl`) are global:
    /// they act on the user's one session regardless of where the unit file
    /// came from. Running them for a scratch prefix would stop the user's real
    /// installation from a test that claims to be isolated.
    fn is_home(&self) -> bool {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|home| home.canonicalize().ok())
            .zip(self.0.canonicalize().ok())
            .is_some_and(|(home, here)| home == here)
    }
}

pub struct Options {
    /// Register tabd as the handler for http/https. Never the default.
    pub set_default: bool,
    /// Enable and start the background service.
    pub enable_service: bool,
}

/// A private scheme the wrapper also claims, so that URL delivery can be
/// tested — and sanity-checked by the user — without anyone having to change
/// their default browser first.
pub const TEST_SCHEME: &str = "tabd";

fn tabd_executable() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe")?;
    let exe = exe
        .canonicalize()
        .with_context(|| format!("resolve {}", exe.display()))?;
    // Rejected once here rather than in each of the four generators. A newline
    // terminates an `Exec=` / `ExecStart=` line and turns the remainder into a
    // new key — line injection, and the one case that produces a *dangerous*
    // artifact rather than a merely broken one. Same rule `validate_url`
    // applies to urls.
    if exe.to_string_lossy().chars().any(char::is_control) {
        bail!(
            "refusing to generate launcher files for an executable path containing a control \
             character: {:?}",
            exe.to_string_lossy()
        );
    }
    Ok(exe)
}

// -- Linux ------------------------------------------------------------------

/// Quote a path for a desktop entry's `Exec=`.
///
/// Two layers, applied in order, because `Exec` is a shell-ish word list
/// living inside a desktop-entry string: the spec requires reserved characters
/// to be escaped inside double quotes, and then the desktop-entry format
/// itself treats a backslash as an escape, so every backslash produced by the
/// first layer has to be doubled by the second. Without this,
/// `/home/a/My Apps/tabd` runs `/home/a/My`.
#[cfg(not(target_os = "macos"))]
pub(crate) fn desktop_exec_quote(path: &Path) -> String {
    let inner: String = path
        .to_string_lossy()
        .chars()
        .flat_map(|c| {
            // Reserved inside a quoted Exec argument.
            let escape = matches!(c, '"' | '`' | '$' | '\\');
            escape.then_some('\\').into_iter().chain(std::iter::once(c))
        })
        .collect::<String>()
        // Field codes are expanded *after* unquoting, so a literal percent has
        // to be written `%%`. Otherwise `/home/a/50%off/tabd` launches
        // `/home/a/50ff/tabd` (`%o` eaten as an unknown code) — and a segment
        // containing `%f` or `%U` gets the url list spliced into the
        // executable path. Done after the backslash pass because it
        // introduces none.
        .replace('%', "%%");
    // Desktop-entry string escaping, over the top.
    format!("\"{}\"", inner.replace('\\', "\\\\"))
}

/// Quote a path for a systemd `ExecStart=`. Same idea, one layer: inside
/// double quotes, `\` and `"` are escaped with a backslash.
#[cfg(not(target_os = "macos"))]
pub(crate) fn systemd_exec_quote(path: &Path) -> String {
    let inner = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        // Quoting does NOT disable systemd's specifier expansion, so a
        // literal percent has to be doubled here too — `/opt/%n/tabd` would
        // otherwise become the unit name and the service would never start.
        // Same hazard as the desktop entry's field codes; I fixed that one
        // first and missed this one.
        .replace('%', "%%");
    format!("\"{inner}\"")
}

/// A systemd user unit. `PartOf` + `After` tie it to the graphical session, so
/// it stops when the session does — the browser is a window on that session.
#[cfg(not(target_os = "macos"))]
pub(crate) fn systemd_unit(executable: &Path, base_dir: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=tabd visual browser daemon\n\
         Documentation=https://github.com/marshallku/tabd\n\
         PartOf=graphical-session.target\n\
         After=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} daemon start --visual\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         # The daemon must not try to spawn a second copy of itself.\n\
         Environment=TABD_NO_AUTO_SPAWN=1\n\
         # Pinned, not inherited: the guard that refuses to enable this while\n\
         # an on-demand daemon is running probes one base dir, and without\n\
         # this the service could come up on a different one.\n\
         Environment=TABD_BASE_DIR={}\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n",
        systemd_exec_quote(executable),
        systemd_exec_quote(base_dir)
    )
}

/// The desktop entry that makes tabd selectable as a browser. `%U` hands over
/// every url at once, which `tabd browser` accepts.
#[cfg(not(target_os = "macos"))]
pub(crate) fn desktop_entry(executable: &Path, base_dir: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=tabd\n\
         GenericName=Web Browser\n\
         Comment=Your everyday browser, driven by tabd\n\
         Exec={} browser --base-dir {} %U\n\
         Terminal=false\n\
         NoDisplay=false\n\
         Categories=Network;WebBrowser;\n\
         MimeType=x-scheme-handler/http;x-scheme-handler/https;x-scheme-handler/{TEST_SCHEME};text/html;\n",
        desktop_exec_quote(executable),
        desktop_exec_quote(base_dir)
    )
}

/// Refuse the global flags for a prefix that is not the real `$HOME`, **before
/// any filesystem work**.
///
/// `xdg-settings`, `systemctl --user`, `launchctl` and `lsregister` all act on
/// the user's one session and one database, whatever directory the files came
/// from. Checking late meant a scratch-prefix install had already rebuilt the
/// bundle and mutated LaunchServices by the time it said "only makes sense for
/// the real $HOME".
fn assert_global_flags_allowed(prefix: &Prefix, options: &Options) -> Result<()> {
    if !options.set_default && !options.enable_service {
        return Ok(());
    }
    if !prefix.is_home() {
        bail!(
            "--set-default and --enable-service only make sense for the real $HOME: they change \
             session-wide state that has nothing to do with {}",
            prefix.join("").display()
        );
    }
    if options.enable_service
        && let Some(socket) = visual_daemon_socket_in_use()?
    {
        // The managed copy would lose the socket bind and exit, and the
        // restart policy would then retry it forever while the on-demand
        // daemon carries on unmanaged. Refuse with the way out rather than
        // tearing down a daemon that may be driving the human's browser.
        bail!(
            "a visual daemon is already listening on {}. Stop it first, then enable the \
             service:\n\x20   tabd daemon stop --base-dir {}",
            socket.display(),
            socket.parent().unwrap_or_else(|| Path::new("")).display()
        );
    }
    Ok(())
}

/// The base dir every generated artifact uses: the **platform default**,
/// always.
///
/// `$TABD_BASE_DIR` is deliberately ignored here. Honouring it meant the
/// value had to be pinned identically into four places — the systemd unit,
/// the LaunchAgent, the desktop entry and the AppleScript applet — and kept in
/// step across reinstalls, `--enable-service` being passed or not, and the
/// running-daemon probe. Four review rounds each found a different pair of
/// those that had drifted apart, the last being a reinstall with a new value
/// that updated the wrapper but not the LaunchAgent. Rather than patch the
/// fourth instance, the configurability is gone: an OS-level install uses the
/// OS-level location, so the artifacts agree by construction and a reinstall
/// is idempotent.
///
/// Ad-hoc use is unaffected — `tabd daemon start --visual --base-dir …` and
/// `tabd browser --base-dir …` still take one.
fn managed_base_dir() -> Result<PathBuf> {
    let base = crate::platform::visual_base_dir()?;
    let base = if base.is_absolute() {
        base
    } else {
        std::env::current_dir()
            .context("resolve the current directory")?
            .join(base)
    };
    // Interpolated into `Environment=`, into plist XML and into an
    // AppleScript literal, where a newline ends the line and starts something
    // else. Same check the executable path gets.
    if base.to_string_lossy().chars().any(char::is_control) {
        bail!(
            "refusing to generate service files for a base directory containing a control \
             character: {:?}",
            base.to_string_lossy()
        );
    }
    Ok(base)
}

/// Say so if the environment suggests a different base dir than the one the
/// install will use, so nobody is left wondering why their variable had no
/// effect.
fn warn_if_base_dir_overridden() -> Result<()> {
    let managed = managed_base_dir()?;
    let effective = crate::daemon::resolve_paths_for(None, crate::daemon::DaemonMode::Visual)?;
    if effective.base_dir != managed {
        eprintln!(
            "note: $TABD_BASE_DIR points at {}, but an installed service always uses {} so the \
             service and the launchers cannot drift apart. Ad-hoc `tabd daemon start --visual \
             --base-dir` and `tabd browser --base-dir` still honour it.",
            effective.base_dir.display(),
            managed.display()
        );
    }
    Ok(())
}

/// The visual daemon's socket, if something is listening on it./// The visual daemon's socket, if something is listening on it.
///
/// A plain `connect`: a live daemon accepts, and a stale socket file refuses
/// with `ECONNREFUSED`, which is exactly the distinction needed here.
fn visual_daemon_socket_in_use() -> Result<Option<PathBuf>> {
    // Derived from `managed_base_dir`, not resolved independently, so the
    // probe and the generated service always name the same socket.
    let socket = managed_base_dir()?.join("daemon.sock");
    match std::os::unix::net::UnixStream::connect(&socket) {
        Ok(_) => Ok(Some(socket)),
        Err(_) => Ok(None),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn install(prefix: &Prefix, options: &Options) -> Result<()> {
    assert_global_flags_allowed(prefix, options)?;
    warn_if_base_dir_overridden()?;
    let executable = tabd_executable()?;

    let unit_path = prefix
        .config_home()
        .join("systemd/user/tabd-visual.service");
    write_file(
        &unit_path,
        systemd_unit(&executable, &managed_base_dir()?).as_bytes(),
    )?;
    eprintln!("wrote {}", unit_path.display());

    let desktop_path = prefix.data_home().join("applications/tabd.desktop");
    write_file(
        &desktop_path,
        desktop_entry(&executable, &managed_base_dir()?).as_bytes(),
    )?;
    eprintln!("wrote {}", desktop_path.display());
    run_optional(
        "update-desktop-database",
        &[prefix
            .data_home()
            .join("applications")
            .display()
            .to_string()],
    );

    if options.enable_service {
        run_required("systemctl", &["--user", "daemon-reload"])?;
        run_required(
            "systemctl",
            &["--user", "enable", "--now", "tabd-visual.service"],
        )?;
        eprintln!("enabled tabd-visual.service");
    } else {
        eprintln!(
            "\nThe service is installed but not enabled. `tabd browser` starts the daemon on\n\
             demand anyway, and it inherits the graphical environment from whatever launched\n\
             it. To run it as a service instead:\n\
             \x20   systemctl --user daemon-reload && systemctl --user enable --now tabd-visual.service\n\
             (its environment then comes from the session, so make sure your desktop runs\n\
             `systemctl --user import-environment` or the equivalent.)"
        );
    }

    if options.set_default {
        run_required(
            "xdg-settings",
            &["set", "default-web-browser", "tabd.desktop"],
        )?;
        eprintln!("tabd is now the default web browser");
    } else {
        eprintln!(
            "\ntabd is NOT your default browser. To make it one:\n\
             \x20   xdg-settings set default-web-browser tabd.desktop\n\
             \x20   (or re-run this command with --set-default)\n\
             To undo: xdg-settings set default-web-browser <your browser>.desktop"
        );
    }
    eprintln!(
        "\nCheck url delivery without changing anything:\n\
         \x20   xdg-open {TEST_SCHEME}://hello"
    );
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn uninstall(prefix: &Prefix) -> Result<()> {
    if prefix.is_home() && is_default_browser() {
        bail!(
            "tabd is currently your default web browser. Point it at another browser first, \
             or removing this leaves you with no handler for links:\n\
             \x20   xdg-settings set default-web-browser <your browser>.desktop"
        );
    }
    // Only for the real installation. `systemctl --user` is global, so doing
    // this for a scratch prefix would stop the user's actual service from a
    // test that claims to touch nothing.
    if prefix.is_home() {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", "tabd-visual.service"])
            .status();
    }
    for path in [
        prefix
            .config_home()
            .join("systemd/user/tabd-visual.service"),
        prefix.data_home().join("applications/tabd.desktop"),
    ] {
        remove_reporting(&path);
    }
    run_optional(
        "update-desktop-database",
        &[prefix
            .data_home()
            .join("applications")
            .display()
            .to_string()],
    );
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn is_default_browser() -> bool {
    std::process::Command::new("xdg-settings")
        .args(["get", "default-web-browser"])
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "tabd.desktop")
        .unwrap_or(false)
}

// -- macOS ------------------------------------------------------------------

/// The wrapper app's handler script.
///
/// An `osacompile` applet rather than a Swift binary or Carbon FFI, for one
/// measured reason: LaunchServices delivers urls as a `GURL` **Apple Event**,
/// not as argv, so a shell script as `CFBundleExecutable` never sees them at
/// all. `on open location` is the handler for exactly that event, `osacompile`
/// ships with every macOS, and delivery was measured working — including a
/// burst of urls arriving while an earlier handler was still blocked, which
/// the applet's own event queue serializes.
///
/// Two levels of escaping, which are different things: `applescript_literal`
/// makes the path safe inside AppleScript source, and `quoted form of` makes
/// it safe crossing into the shell.
///
/// The `try` is load-bearing, not belt-and-braces: a nonzero exit from
/// `do shell script` raises an AppleScript error, and an error dialog on a
/// background applet blocks every url delivered after it.
#[cfg(target_os = "macos")]
pub(crate) fn applet_source(executable: &Path, base_dir: &Path) -> String {
    format!(
        "on tabd()\n\
         \treturn \"{}\"\n\
         end tabd\n\
         \n\
         on tabdBase()\n\
         \treturn \"{}\"\n\
         end tabdBase\n\
         \n\
         on tabdCommand()\n\
         \treturn quoted form of tabd() & \" browser --base-dir \" & quoted form of tabdBase()\n\
         end tabdCommand\n\
         \n\
         on open location theURL\n\
         \ttry\n\
         \t\tdo shell script tabdCommand() & \" \" & quoted form of theURL\n\
         \tend try\n\
         end open location\n\
         \n\
         on run\n\
         \ttry\n\
         \t\tdo shell script tabdCommand()\n\
         \tend try\n\
         end run\n",
        applescript_literal(&executable.to_string_lossy()),
        applescript_literal(&base_dir.to_string_lossy())
    )
}

/// Escape a string for use inside an AppleScript `"…"` literal.
#[cfg(target_os = "macos")]
pub(crate) fn applescript_literal(value: &str) -> String {
    value.replace('\\', r"\\").replace('"', "\\\"")
}

/// Escape a string for XML character data.
///
/// A path containing `&` or `<` otherwise produces a plist that will not
/// parse — and because the install does not load it, it reports success and
/// fails silently at the next login.
#[cfg(target_os = "macos")]
pub(crate) fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// A LaunchAgent tied to the Aqua session, so it does not run for ssh logins
/// or at the login window — there is no screen to put a browser window on.
#[cfg(target_os = "macos")]
pub(crate) fn launch_agent_plist(executable: &Path, base_dir: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>dev.tabd.visual</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>daemon</string>
        <string>start</string>
        <string>--visual</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>TABD_NO_AUTO_SPAWN</key><string>1</string>
        <!-- Pinned, not inherited: the guard that refuses to enable this
             while an on-demand daemon is running probes one base dir. -->
        <key>TABD_BASE_DIR</key><string>{}</string>
    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key>
    <dict><key>SuccessfulExit</key><false/></dict>
    <key>LimitLoadToSessionType</key><string>Aqua</string>
    <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#,
        xml_escape(&executable.to_string_lossy()),
        xml_escape(&base_dir.to_string_lossy())
    )
}

#[cfg(target_os = "macos")]
const APP_BUNDLE_ID: &str = "dev.tabd.browser";

#[cfg(target_os = "macos")]
pub fn install(prefix: &Prefix, options: &Options) -> Result<()> {
    assert_global_flags_allowed(prefix, options)?;
    warn_if_base_dir_overridden()?;
    let executable = tabd_executable()?;

    let app_path = prefix.join("Applications/tabd.app");
    build_wrapper_app(
        &app_path,
        &executable,
        &managed_base_dir()?,
        prefix.is_home(),
    )?;
    eprintln!("built {}", app_path.display());

    let agent_path = prefix.join("Library/LaunchAgents/dev.tabd.visual.plist");
    if options.enable_service {
        write_file(
            &agent_path,
            launch_agent_plist(&executable, &managed_base_dir()?).as_bytes(),
        )?;
        eprintln!("wrote {}", agent_path.display());
        let uid = unsafe { libc::getuid() };
        // launchd rejects importing a label it already knows, so a service
        // that was enabled before and then had its daemon stopped could never
        // be re-enabled — including via the "stop it first, then enable"
        // recovery this command prints. Boot it out first; failing means it
        // was not loaded, which is the state we want anyway.
        run_optional(
            "launchctl",
            &["bootout".to_string(), format!("gui/{uid}/dev.tabd.visual")],
        );
        run_required(
            "launchctl",
            &[
                "bootstrap",
                &format!("gui/{uid}"),
                &agent_path.display().to_string(),
            ],
        )?;
        eprintln!("bootstrapped dev.tabd.visual");
    } else {
        // The plist is deliberately NOT written. Unlike a systemd unit, which
        // does nothing until it is enabled, anything in
        // `~/Library/LaunchAgents` is loaded at the next login — so writing it
        // "without starting it" would still be opting the user in, one logout
        // later.
        eprintln!(
            "\nNo LaunchAgent was installed: on macOS anything in ~/Library/LaunchAgents starts\n\
             at the next login, so installing one *is* the opt-in. `tabd browser` starts the\n\
             daemon on demand anyway. To run it as a service:\n\
             \x20   tabd service install --enable-service"
        );
    }

    if options.set_default {
        // There is no supported non-interactive way to set the default
        // browser on macOS, and the unsupported ways poke at a
        // MAC-protected LaunchServices database.
        eprintln!(
            "\nnote: --set-default does nothing on macOS. The default browser is a user gesture:"
        );
    }
    eprintln!(
        "\ntabd is NOT your default browser. To make it one:\n\
         \x20   System Settings -> Desktop & Dock -> Default web browser -> tabd\n\
         To undo, pick your previous browser in the same place.\n\n\
         Check url delivery without changing anything:\n\
         \x20   open {TEST_SCHEME}://hello"
    );
    Ok(())
}

/// Generate, patch and ad-hoc sign the wrapper bundle.
///
/// Built somewhere else first and only swapped in once it is complete and
/// signed. Compiling straight over the destination means a failure halfway
/// through leaves the user with no working handler — and if tabd is their
/// default browser, no handler for links at all.
#[cfg(target_os = "macos")]
fn build_wrapper_app(
    app_path: &Path,
    executable: &Path,
    base_dir: &Path,
    register: bool,
) -> Result<()> {
    let parent = app_path
        .parent()
        .context("wrapper app has no parent directory")?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    // Never replace a bundle that is not ours. `tabd.app` is a plausible name
    // for someone else's build, and `remove_dir_all` is not a guess to make.
    if app_path.exists() && !is_our_bundle(app_path) {
        bail!(
            "{} exists but is not a tabd wrapper (its CFBundleIdentifier is not {APP_BUNDLE_ID}).              Move it aside if you want tabd to take that name.",
            app_path.display()
        );
    }

    // The `.app` suffix is load-bearing: `osacompile` picks its output format
    // from the extension, and without it produces a plain compiled script
    // rather than an application bundle.
    let staging = parent.join(".tabd-app-staging.app");
    let _ = std::fs::remove_dir_all(&staging);
    let source = parent.join(".tabd-app.applescript");
    let build = || -> Result<()> {
        // `write_file`, not `std::fs::write`: a plain write follows a symlink
        // and truncates its target, which is the hazard
        // `platform::write_sidecar` exists for.
        write_file(&source, applet_source(executable, base_dir).as_bytes())?;
        run_required(
            "osacompile",
            &[
                "-o",
                &staging.display().to_string(),
                &source.display().to_string(),
            ],
        )?;
        patch_bundle_plist(&staging)?;
        run_required(
            "codesign",
            &["--force", "-s", "-", &staging.display().to_string()],
        )
    };
    let built = build();
    let _ = std::fs::remove_file(&source);
    if let Err(err) = built {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(err);
    }

    // Only now is the existing bundle disturbed.
    if app_path.exists() {
        let previous = parent.join(".tabd-app-previous.app");
        let _ = std::fs::remove_dir_all(&previous);
        if let Err(err) = std::fs::rename(app_path, &previous) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("move aside {}", app_path.display()));
        }
        if let Err(err) = std::fs::rename(&staging, app_path) {
            // Put the old one back rather than leaving nothing there, and
            // clean up the staging tree — `uninstall` does not know about it,
            // so anything left here is left forever.
            let _ = std::fs::rename(&previous, app_path);
            let _ = std::fs::remove_dir_all(&staging);
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("install {}", app_path.display()));
        }
        let _ = std::fs::remove_dir_all(&previous);
    } else if let Err(err) = std::fs::rename(&staging, app_path) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(anyhow::Error::new(err))
            .with_context(|| format!("install {}", app_path.display()));
    }

    // Only for the real installation: LaunchServices is a per-user global
    // database, so registering a scratch bundle leaves a dangling http/https
    // handler behind the moment that directory is deleted.
    if register {
        run_required(LSREGISTER, &["-f", &app_path.display().to_string()])?;
    }
    Ok(())
}

/// Whether this bundle is one we wrote, by its identifier.
#[cfg(target_os = "macos")]
fn is_our_bundle(app_path: &Path) -> bool {
    std::process::Command::new("/usr/libexec/PlistBuddy")
        .args([
            "-c",
            "Print :CFBundleIdentifier",
            &app_path.join("Contents/Info.plist").display().to_string(),
        ])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == APP_BUNDLE_ID)
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn patch_bundle_plist(app_path: &Path) -> Result<()> {
    let plist = app_path.join("Contents/Info.plist");
    let plist_arg = plist.display().to_string();
    for (key, kind, value) in [
        ("CFBundleIdentifier", "string", APP_BUNDLE_ID),
        ("LSUIElement", "bool", "true"),
        ("CFBundleName", "string", "tabd"),
    ] {
        set_plist_value(&plist_arg, key, kind, value)?;
    }

    // The template may or may not already have URL types, so the Delete is
    // allowed to fail — but nothing after it is. A half-written scheme list
    // still signs and registers, and then silently does not handle links.
    let _ = std::process::Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Delete :CFBundleURLTypes", &plist_arg])
        .output();

    let mut commands = vec![
        "Add :CFBundleURLTypes array".to_string(),
        "Add :CFBundleURLTypes:0 dict".to_string(),
        "Add :CFBundleURLTypes:0:CFBundleURLName string tabd browser".to_string(),
        "Add :CFBundleURLTypes:0:CFBundleURLSchemes array".to_string(),
    ];
    for (index, scheme) in ["http", "https", TEST_SCHEME].iter().enumerate() {
        commands.push(format!(
            "Add :CFBundleURLTypes:0:CFBundleURLSchemes:{index} string {scheme}"
        ));
    }
    for command in commands {
        let output = std::process::Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", &command, &plist_arg])
            .output()
            .with_context(|| format!("run PlistBuddy: {command}"))?;
        if !output.status.success() {
            bail!(
                "PlistBuddy `{command}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn set_plist_value(plist: &str, key: &str, kind: &str, value: &str) -> Result<()> {
    for command in [
        format!("Set :{key} {value}"),
        format!("Add :{key} {kind} {value}"),
    ] {
        let ok = std::process::Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", &command, plist])
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if ok {
            return Ok(());
        }
    }
    bail!("could not set {key} in {plist}")
}

#[cfg(target_os = "macos")]
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

#[cfg(target_os = "macos")]
pub fn uninstall(prefix: &Prefix) -> Result<()> {
    let app_path = prefix.join("Applications/tabd.app");
    if app_path.exists() {
        // The same ownership check `install` makes. Refusing to *replace*
        // someone else's `tabd.app` while happily *deleting* it would be the
        // worse half of the pair.
        if !is_our_bundle(&app_path) {
            bail!(
                "{} is not a tabd wrapper (its CFBundleIdentifier is not {APP_BUNDLE_ID}); \
                 leaving it alone",
                app_path.display()
            );
        }
        // Gated exactly like the `-f` in `build_wrapper_app`: LaunchServices
        // is a per-user global database, and a scratch prefix has no business
        // touching it in either direction.
        if prefix.is_home() {
            run_optional(
                LSREGISTER,
                &["-u".to_string(), app_path.display().to_string()],
            );
        }
        remove_reporting(&app_path);
    }
    let agent_path = prefix.join("Library/LaunchAgents/dev.tabd.visual.plist");
    if agent_path.exists() {
        // `launchctl` is global, so only for the real installation.
        let uid = unsafe { libc::getuid() };
        if prefix.is_home() {
            run_optional(
                "launchctl",
                &["bootout".to_string(), format!("gui/{uid}/dev.tabd.visual")],
            );
        }
        remove_reporting(&agent_path);
    }
    eprintln!(
        "\nIf tabd was your default browser, macOS has no handler for links now — pick one in\n\
         System Settings -> Desktop & Dock -> Default web browser."
    );
    Ok(())
}

// -- Shared helpers ---------------------------------------------------------

fn write_file(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    crate::platform::write_sidecar(path, contents)
}

fn remove_reporting(path: &Path) {
    if !path.exists() {
        return;
    }
    let outcome = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match outcome {
        Ok(()) => eprintln!("removed {}", path.display()),
        Err(err) => eprintln!("warning: could not remove {}: {err}", path.display()),
    }
}

/// Run a command whose failure is fatal.
fn run_required<S: AsRef<std::ffi::OsStr>>(program: &str, args: &[S]) -> Result<()> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("run {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Run a command whose absence or failure is not fatal — a desktop-database
/// refresh, an unregister during cleanup.
fn run_optional<S: AsRef<std::ffi::OsStr>>(program: &str, args: &[S]) {
    let _ = std::process::Command::new(program).args(args).output();
}

/// What is installed, and what is not.
pub fn status(prefix: &Prefix, base_dir: Option<&str>) -> Result<()> {
    let entries: Vec<(&str, PathBuf)> = if cfg!(target_os = "macos") {
        vec![
            ("wrapper app", prefix.join("Applications/tabd.app")),
            (
                "launch agent",
                prefix.join("Library/LaunchAgents/dev.tabd.visual.plist"),
            ),
        ]
    } else {
        vec![
            (
                "desktop entry",
                prefix.data_home().join("applications/tabd.desktop"),
            ),
            (
                "systemd unit",
                prefix
                    .config_home()
                    .join("systemd/user/tabd-visual.service"),
            ),
        ]
    };
    for (label, path) in entries {
        let mark = if path.exists() {
            "installed"
        } else {
            "missing"
        };
        println!("{mark:>9}  {label}: {}", path.display());
    }
    // The same resolution the writer uses, `$TABD_BASE_DIR` included —
    // `platform::visual_base_dir()` ignores it, so with that variable set the
    // two looked at different files and status always said "none yet".
    let base =
        crate::daemon::resolve_paths_for(base_dir, crate::daemon::DaemonMode::Visual)?.base_dir;
    let delivery_log = base.join("url-delivery.log");
    match std::fs::read_to_string(&delivery_log) {
        Ok(text) => match text.lines().next_back() {
            Some(last) => println!("{:>9}  last url delivery: {last}", "seen"),
            None => println!("{:>9}  last url delivery: (log is empty)", "none"),
        },
        Err(_) => println!(
            "{:>9}  last url delivery: none yet — try `open {TEST_SCHEME}://hello`",
            "none"
        ),
    }
    let profile = crate::platform::visual_profile_dir()?;
    println!(
        "{:>9}  visual profile: {} (session restore: {})",
        if profile.exists() {
            "present"
        } else {
            "absent"
        },
        profile.display(),
        crate::platform::session_restore(&profile).as_str()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_desktop_entry_takes_every_url_at_once() {
        let entry = desktop_entry(
            Path::new("/usr/local/bin/tabd"),
            Path::new("/run/u/tabd-visual"),
        );
        // `%U`, not `%u`: a click that opens several links hands them over in
        // one invocation, and `tabd browser` accepts a list.
        assert!(
            entry.contains(
                "Exec=\"/usr/local/bin/tabd\" browser --base-dir \"/run/u/tabd-visual\" %U"
            ),
            "{entry}"
        );
        // An absolute path — the desktop launches this with no useful $PATH.
        assert!(!entry.contains("Exec=\"tabd\""), "{entry}");
        for scheme in ["x-scheme-handler/http", "x-scheme-handler/https"] {
            assert!(entry.contains(scheme), "{entry}");
        }
        // The private scheme is what makes delivery checkable without anyone
        // changing their default browser.
        assert!(
            entry.contains(&format!("x-scheme-handler/{TEST_SCHEME}")),
            "{entry}"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn exec_quoting_survives_spaces_and_reserved_characters() {
        // The desktop-entry format escapes twice: once for the quoted Exec
        // word, then again because a backslash is itself an escape in the
        // string. A single pass silently produces a broken launcher.
        let quoted = desktop_exec_quote(Path::new("/home/a/My Apps/ta$bd"));
        assert!(quoted.starts_with('"') && quoted.ends_with('"'), "{quoted}");
        assert!(quoted.contains("My Apps"), "{quoted}");
        assert!(quoted.contains("\\\\$bd"), "got: {quoted}");

        // Field codes are expanded after unquoting, so a literal percent must
        // be doubled. `/home/a/50%off/tabd` otherwise launches
        // `/home/a/50ff/tabd` — `%o` eaten as an unknown code — and a segment
        // containing `%U` would get the url list spliced into the path.
        assert_eq!(
            desktop_exec_quote(Path::new("/home/a/50%off/tabd")),
            "\"/home/a/50%%off/tabd\""
        );
        assert_eq!(
            desktop_exec_quote(Path::new("/home/a/%U/tabd")),
            "\"/home/a/%%U/tabd\""
        );

        // systemd: backslash and quote escaped inside quotes — and percent
        // doubled, because quoting does not disable specifier expansion.
        // `/opt/%n/tabd` would otherwise become the unit name.
        assert_eq!(
            systemd_exec_quote(Path::new("/opt/a b/tabd")),
            "\"/opt/a b/tabd\""
        );
        assert_eq!(
            systemd_exec_quote(Path::new("/opt/%n/tabd")),
            "\"/opt/%%n/tabd\""
        );
        assert_eq!(
            systemd_exec_quote(Path::new(r#"/opt/a"b/tabd"#)),
            r#""/opt/a\"b/tabd""#
        );
    }

    #[test]
    fn xdg_overrides_apply_only_to_a_real_install() {
        // A scratch prefix must stay hermetic — otherwise a test would write
        // the desktop entry into the user's real $XDG_DATA_HOME.
        let scratch = Prefix::at("/tmp/scratch-prefix");
        assert_eq!(
            scratch.data_home(),
            std::path::PathBuf::from("/tmp/scratch-prefix/.local/share")
        );
        assert_eq!(
            scratch.config_home(),
            std::path::PathBuf::from("/tmp/scratch-prefix/.config")
        );
    }

    #[test]
    fn the_pinned_base_dir_is_the_platform_default() {
        // Every generated artifact reads this one function, and it ignores
        // `$TABD_BASE_DIR`, so the unit, the plist, the desktop entry, the
        // applet and the running-daemon probe cannot drift apart — including
        // across a reinstall with a different environment.
        let base = managed_base_dir().expect("base dir");
        assert!(base.is_absolute(), "got: {}", base.display());
        assert!(!base.to_string_lossy().chars().any(char::is_control));
        assert_eq!(
            base,
            crate::platform::visual_base_dir().expect("platform default")
        );
    }

    #[test]
    fn global_flags_are_refused_for_a_scratch_prefix() {
        // Before any filesystem work: `xdg-settings`, `systemctl --user`,
        // `launchctl` and `lsregister` all act on the one session, so a late
        // check meant the damage was already done by the time it fired.
        let scratch = Prefix::at("/tmp/definitely-not-home");
        for options in [
            Options {
                set_default: true,
                enable_service: false,
            },
            Options {
                set_default: false,
                enable_service: true,
            },
        ] {
            assert!(assert_global_flags_allowed(&scratch, &options).is_err());
        }
        // Without those flags a scratch prefix is just files, so it is fine.
        assert!(
            assert_global_flags_allowed(
                &scratch,
                &Options {
                    set_default: false,
                    enable_service: false
                }
            )
            .is_ok()
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_unit_dies_with_the_graphical_session() {
        let unit = systemd_unit(
            Path::new("/usr/local/bin/tabd"),
            Path::new("/run/user/1000/tabd-visual"),
        );
        // The browser is a window on that session; outliving it is pointless
        // and leaves a daemon holding a profile lock.
        assert!(unit.contains("PartOf=graphical-session.target"), "{unit}");
        assert!(unit.contains("After=graphical-session.target"), "{unit}");
        assert!(
            unit.contains("ExecStart=\"/usr/local/bin/tabd\" daemon start --visual"),
            "{unit}"
        );
        // Otherwise the service spawns a daemon that spawns a daemon.
        assert!(unit.contains("TABD_NO_AUTO_SPAWN=1"), "{unit}");
        // Pinned so the service and the "is one already running" probe can
        // never end up on different base dirs.
        assert!(
            unit.contains("Environment=TABD_BASE_DIR=\"/run/user/1000/tabd-visual\""),
            "{unit}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_applet_escapes_both_boundaries() {
        // Two different escapes for two different boundaries: the AppleScript
        // source literal, then `quoted form of` for the shell.
        let source = applet_source(
            Path::new(r#"/Users/a b/"weird"\path/tabd"#),
            Path::new("/tmp/base"),
        );
        assert!(
            source.contains(r#"/Users/a b/\"weird\"\\path/tabd"#),
            "{source}"
        );
        assert!(source.contains("quoted form of tabd()"), "{source}");
        // The handler for the GURL Apple Event, which is how LaunchServices
        // actually delivers a url — argv never carries it.
        assert!(source.contains("on open location theURL"), "{source}");
        // A nonzero exit must not raise a dialog: a modal on a background
        // applet blocks every url after it.
        assert!(source.contains("\ttry\n"), "{source}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_literal_escapes_backslash_before_quote() {
        // Order matters: escaping quotes first would then double the
        // backslashes it just introduced.
        assert_eq!(applescript_literal(r#"a\b"c"#), r#"a\\b\"c"#);
        assert_eq!(applescript_literal("plain"), "plain");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_launch_agent_escapes_xml() {
        // A path with `&` otherwise produces a plist that will not parse —
        // and since the install does not load it, it reports success and
        // fails at the next login instead.
        let plist = launch_agent_plist(Path::new("/opt/a&b/<tabd>"), Path::new("/tmp/b&se"));
        assert!(
            plist.contains("<string>/opt/a&amp;b/&lt;tabd&gt;</string>"),
            "{plist}"
        );
        assert_eq!(
            xml_escape(r#"a&b<c>"d'e"#),
            "a&amp;b&lt;c&gt;&quot;d&apos;e"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_launch_agent_is_aqua_only() {
        let plist = launch_agent_plist(Path::new("/usr/local/bin/tabd"), Path::new("/tmp/base"));
        // No screen at the login window or over ssh, so no browser.
        assert!(
            plist.contains("<key>LimitLoadToSessionType</key><string>Aqua</string>"),
            "{plist}"
        );
        assert!(
            plist.contains("<string>/usr/local/bin/tabd</string>"),
            "{plist}"
        );
        assert!(plist.contains("TABD_NO_AUTO_SPAWN"), "{plist}");
    }
}
