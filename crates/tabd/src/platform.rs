//! Platform-specific paths and probes for visual mode.
//!
//! This is the `ProfilePaths` role from the platform table in
//! `docs/visual-mode-plan.md` §3. The other roles (`HumanUi`, `VaultUnlock`,
//! `ServiceInstall`, `DefaultBrowser`, `WindowOps`) arrive in the phases that
//! first need them; putting them here now would be five empty traits.
//!
//! Headless paths are deliberately untouched: `daemon::resolve_paths` keeps
//! its `$XDG_RUNTIME_DIR/tabd` / `~/.cache/tabd` behavior, and visual mode
//! gets its own tree so the two daemons coexist.

use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME not set")
}

/// `$VAR` if it names an absolute path, else `home/<fallback>`.
#[cfg(not(target_os = "macos"))]
fn env_dir_or(var: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(v) = std::env::var_os(var) {
        let p = PathBuf::from(v);
        if p.is_absolute() {
            return Ok(p);
        }
    }
    Ok(home()?.join(fallback))
}

/// Where the visual daemon keeps its own state: `daemon.sock`, `daemon.pid`,
/// and the browser's stderr log. Not the browser profile — that is
/// [`visual_profile_dir`], and it lives elsewhere because it is much larger,
/// must survive a `$XDG_RUNTIME_DIR` wipe, and gets backed up.
pub fn visual_base_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Ok(home()?.join("Library/Application Support/tabd/visual"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(env_dir_or("XDG_STATE_HOME", ".local/state")?.join("tabd/visual"))
    }
}

/// The `--user-data-dir` visual mode drives. `$TABD_VISUAL_PROFILE_DIR`
/// overrides it; tests need that, and so does anyone keeping the profile on
/// another disk.
pub fn visual_profile_dir() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os("TABD_VISUAL_PROFILE_DIR") {
        let p = PathBuf::from(v);
        if p.is_absolute() {
            return Ok(p);
        }
    }
    #[cfg(target_os = "macos")]
    {
        Ok(home()?.join("Library/Application Support/tabd/profile"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(env_dir_or("XDG_DATA_HOME", ".local/share")?.join("tabd/profile"))
    }
}

/// Lock file for "who owns this profile". Derived from the **profile**, not
/// from the daemon base dir, so two daemons started with different
/// `$TABD_BASE_DIR` but the same profile still contend on one file.
///
/// Built from `parent` + `file_name` rather than by appending to the path
/// string: appending turns a trailing slash into `<profile>/.lock`, which puts
/// our lock *inside* the user-data-dir Chromium owns, and makes `/a/b` and
/// `/a/b/` — the same profile — take two different locks.
pub fn profile_lock_path(profile_dir: &Path) -> PathBuf {
    let Some(name) = profile_dir.file_name() else {
        // A path with no final component (`/`, `..`) is not a usable profile;
        // the caller will fail on it anyway. Keep the lock beside it.
        return profile_dir.join(".tabd-profile.lock");
    };
    let mut lock = name.to_os_string();
    lock.push(".lock");
    match profile_dir.parent() {
        Some(parent) => parent.join(lock),
        None => PathBuf::from(lock),
    }
}

/// Resolve a profile path to one canonical identity.
///
/// Everything that protects the profile — the `flock` and the browser binding
/// — is keyed on its path, so two paths that name the same directory must
/// resolve to the same string. With `/data/alias` a symlink to
/// `/data/profile`, the unresolved paths take two different locks on one
/// user-data-dir, and the alias also escapes the binding recorded under the
/// real name.
///
/// The directory may not exist yet on a first launch, and `canonicalize`
/// requires an existing path, so the nearest existing ancestor is resolved and
/// the remaining components appended.
///
/// **Creates nothing.** An earlier version created the missing parents so it
/// could canonicalize them, which meant `tabd profile import --to
/// <source>/nested/x` made directories *inside the source profile* before the
/// overlap check rejected it — in a command whose whole promise is that it
/// does not touch the original.
pub fn canonical_profile_dir(profile_dir: &Path) -> Result<PathBuf> {
    if let Ok(resolved) = profile_dir.canonicalize() {
        return Ok(resolved);
    }
    let absolute = if profile_dir.is_absolute() {
        profile_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve the current directory")?
            .join(profile_dir)
    };

    let mut existing = absolute.as_path();
    let mut trailing: Vec<&OsStr> = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().with_context(|| {
            format!(
                "no existing ancestor of {} could be resolved",
                absolute.display()
            )
        })?;
        trailing.push(name);
        existing = existing.parent().with_context(|| {
            format!(
                "no existing ancestor of {} could be resolved",
                absolute.display()
            )
        })?;
    }
    let mut resolved = existing
        .canonicalize()
        .with_context(|| format!("resolve {}", existing.display()))?;
    for name in trailing.iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

/// Where a given browser keeps the profile the human uses every day.
///
/// Keyed on the executable's file name, which is stable across the
/// distro-specific paths (`/usr/bin/brave` vs `/opt/brave-bin/brave`) that
/// `discover_chromium` can return. `None` for a browser we do not recognize —
/// the caller then requires an explicit `--from`, because guessing would mean
/// copying one browser's data and opening it with another.
pub fn real_profile_dir(executable: &Path) -> Option<PathBuf> {
    let name = executable
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();
    let home = home().ok()?;
    let relative = match name.as_str() {
        "google chrome" | "google-chrome" | "google-chrome-stable" | "chrome" => {
            if cfg!(target_os = "macos") {
                "Google/Chrome"
            } else {
                "google-chrome"
            }
        }
        "google chrome canary" | "google-chrome-canary" => {
            if cfg!(target_os = "macos") {
                "Google/Chrome Canary"
            } else {
                "google-chrome-canary"
            }
        }
        "brave browser" | "brave" | "brave-browser" => "BraveSoftware/Brave-Browser",
        "chromium" | "chromium-browser" => {
            if cfg!(target_os = "macos") {
                "Chromium"
            } else {
                "chromium"
            }
        }
        "microsoft edge" | "microsoft-edge" | "microsoft-edge-stable" => {
            if cfg!(target_os = "macos") {
                "Microsoft Edge"
            } else {
                "microsoft-edge"
            }
        }
        "vivaldi" | "vivaldi-stable" => {
            if cfg!(target_os = "macos") {
                "Vivaldi"
            } else {
                "vivaldi"
            }
        }
        _ => return None,
    };
    #[cfg(target_os = "macos")]
    {
        Some(home.join("Library/Application Support").join(relative))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(home.join(".config").join(relative))
    }
}

/// Exclusive ownership of one browser profile, as an advisory `flock` on
/// `<profile>.lock`.
///
/// Keyed on the profile rather than the daemon base dir on purpose: two
/// daemons started with different `$TABD_BASE_DIR` but the same profile would
/// otherwise both drive it. The lock is released when the file is closed,
/// which `Drop` does, and which the kernel does if the holder dies.
#[derive(Debug)]
pub struct ProfileLock {
    // Held only to keep the descriptor open; closing it releases the flock.
    _file: std::fs::File,
}

impl ProfileLock {
    pub fn acquire(profile_dir: &Path) -> Result<Self> {
        use std::os::fd::AsRawFd;

        let path = profile_lock_path(profile_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        // `O_NOFOLLOW`: without it, a `<profile>.lock` that is a dangling
        // symlink into somewhere else gets *created there*. For
        // `<staging>.lock` that somewhere else can be inside the original
        // profile, which `tabd profile import` exists to leave untouched.
        // The atomic-sidecar path does not cover this one — locks are opened,
        // not renamed into place.
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("open profile lock {}", path.display()))?;
        if !file
            .metadata()
            .with_context(|| format!("stat profile lock {}", path.display()))?
            .is_file()
        {
            anyhow::bail!("profile lock {} is not a regular file", path.display());
        }
        // SAFETY: `file` owns a live descriptor for the duration of the call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!(
                "another tabd already owns profile {} ({}): {err}",
                profile_dir.display(),
                path.display()
            );
        }
        Ok(ProfileLock { _file: file })
    }
}

/// Write a small sidecar file beside a profile, replacing whatever is there.
///
/// Through an exclusively-created temporary and a rename, for two reasons. A
/// plain write **follows a symlink and truncates its target**: a
/// `<destination>.import` that happens to be a link to the source's `History`
/// would have the original database overwritten with JSON. And a partial write
/// would leave an unparsable record behind. `rename` replaces the link itself,
/// and is atomic.
pub fn write_sidecar(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp = parent.join(format!(".tabd-tmp-{}-{stamp}", std::process::id()));

    let write = || -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true) // never opens something that already exists
            .write(true)
            .mode(0o600)
            .open(&temp)
            .with_context(|| format!("create {}", temp.display()))?;
        file.write_all(contents)
            .with_context(|| format!("write {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("flush {}", temp.display()))?;
        std::fs::rename(&temp, path).with_context(|| format!("publish {}", path.display()))
    };
    let result = write();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Whether the profile reopens its tabs on startup.
///
/// This matters more than it looks. Measured on 2026-09-27 (macOS, Chrome
/// 153): with the default setting, a browser that exits — gracefully via
/// SIGTERM, which is the same path a pipe EOF takes, or after `kill -9` —
/// comes back with a single new-tab page and no "Restore pages?" prompt. The
/// daemon owns the pipe, so a daemon crash closes the browser; without this
/// setting the human silently loses their tabs.
///
/// tabd cannot turn it on: `session.restore_on_startup` is a MAC-protected
/// preference (startup pages are a hijacking target), so an edit to
/// `Preferences` is reverted on the next launch, and the managed-policy route
/// is browser-wide. So this is a *probe*, and the caller warns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRestore {
    /// "Continue where you left off".
    On,
    /// New tab page or a fixed URL list — tabs are lost on exit.
    Off,
    /// No readable `Preferences` yet (a profile that has never been launched).
    Unknown,
}

impl SessionRestore {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionRestore::On => "on",
            SessionRestore::Off => "off",
            SessionRestore::Unknown => "unknown",
        }
    }
}

/// Chromium's `SessionStartupPref`: 1 = restore the last session, 4 = a fixed
/// URL list, 5 = new tab page (the default). Absent means the default.
const RESTORE_LAST_SESSION: i64 = 1;

pub fn session_restore(profile_dir: &Path) -> SessionRestore {
    // tabd owns a single-profile user-data-dir, so `Default` is the only
    // profile there is. A user who adds more in the browser UI gets `Unknown`
    // for those, which is the honest answer.
    let prefs = profile_dir.join("Default/Preferences");
    let Ok(text) = std::fs::read_to_string(&prefs) else {
        return SessionRestore::Unknown;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return SessionRestore::Unknown;
    };
    match value
        .pointer("/session/restore_on_startup")
        .and_then(|v| v.as_i64())
    {
        Some(RESTORE_LAST_SESSION) => SessionRestore::On,
        // Present-but-other and absent both mean "tabs are not coming back".
        _ => SessionRestore::Off,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn lock_path_is_a_sibling_of_the_profile() {
        let lock = profile_lock_path(Path::new("/data/tabd/profile"));
        assert_eq!(lock, PathBuf::from("/data/tabd/profile.lock"));
        // Deriving from the profile — not from a base dir — is what makes two
        // daemons with different base dirs contend on the same file.
        assert_eq!(lock.parent(), Some(Path::new("/data/tabd")));
    }

    #[test]
    fn lock_path_is_stable_across_trailing_slashes() {
        // `/a/b` and `/a/b/` name the same profile, so they must take the same
        // lock — and it must sit beside the profile, never inside it (a file
        // inside the user-data-dir is Chromium's business, not ours).
        assert_eq!(
            profile_lock_path(Path::new("/a/b/")),
            profile_lock_path(Path::new("/a/b"))
        );
        assert_eq!(
            profile_lock_path(Path::new("/a/b/")),
            PathBuf::from("/a/b.lock")
        );
    }

    #[test]
    fn visual_dirs_are_absolute_and_distinct() {
        let base = visual_base_dir().expect("base dir");
        let profile = visual_profile_dir().expect("profile dir");
        assert!(base.is_absolute(), "got: {}", base.display());
        assert!(profile.is_absolute(), "got: {}", profile.display());
        // The daemon's state and the browser's profile must not be the same
        // tree: one is disposable, the other is the human's browser.
        assert_ne!(base, profile);
    }

    #[test]
    fn canonical_profile_dir_collapses_aliases() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let real = dir.path().join("profile");
        std::fs::create_dir_all(&real).expect("mkdir");
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("symlink");

        let a = canonical_profile_dir(&real).expect("real");
        let b = canonical_profile_dir(&alias).expect("alias");
        assert_eq!(
            a, b,
            "an alias must not be a second identity for one profile"
        );
        // …and therefore neither may the files that protect it.
        assert_eq!(profile_lock_path(&a), profile_lock_path(&b));
    }

    #[test]
    fn canonical_profile_dir_works_before_the_profile_exists() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let fresh = dir.path().join("not-yet");
        let resolved = canonical_profile_dir(&fresh).expect("fresh profile");
        assert!(resolved.is_absolute(), "got: {}", resolved.display());
        assert_eq!(resolved.file_name(), fresh.file_name());
        assert!(!resolved.exists());
    }

    #[test]
    fn canonical_profile_dir_creates_nothing() {
        // `tabd profile import --to <source>/a/b/c` must not make directories
        // inside the source while working out whether the paths overlap.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let deep = dir.path().join("a/b/c");
        let resolved = canonical_profile_dir(&deep).expect("deep path");
        assert!(resolved.ends_with("a/b/c"), "got: {}", resolved.display());
        assert!(!dir.path().join("a").exists(), "nothing may be created");
    }

    #[test]
    fn real_profile_dirs_are_per_browser_and_absolute() {
        // Distro spellings of the same browser land on one profile.
        let brave_a = real_profile_dir(Path::new("/usr/bin/brave")).expect("brave");
        let brave_b = real_profile_dir(Path::new("/opt/brave-bin/brave-browser")).expect("brave");
        assert_eq!(brave_a, brave_b);
        assert!(brave_a.is_absolute());

        // Different browsers must never share one — copying Chrome's data and
        // opening it with Brave rewrites it.
        let chrome = real_profile_dir(Path::new("/usr/bin/google-chrome-stable")).expect("chrome");
        assert_ne!(chrome, brave_a);

        // macOS bundle binary names carry spaces.
        assert!(
            real_profile_dir(Path::new(
                "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"
            ))
            .is_some()
        );

        // An unrecognized browser has no configuration — guessing is worse
        // than making the caller pass --from.
        assert_eq!(real_profile_dir(Path::new("/opt/weird/browser")), None);
    }

    #[test]
    fn profile_lock_is_exclusive_and_releasable() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let profile = dir.path().join("profile");
        let first = ProfileLock::acquire(&profile).expect("first");
        let err = ProfileLock::acquire(&profile).expect_err("second must fail");
        assert!(
            err.to_string().contains("already owns profile"),
            "got: {err}"
        );
        drop(first);
        ProfileLock::acquire(&profile).expect("reacquire after release");
    }

    #[test]
    fn profile_lock_refuses_to_follow_a_symlink() {
        // A dangling `<profile>.lock` symlink must not make us create its
        // target — that target can be inside the profile an import is meant
        // to leave alone.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let victim = dir.path().join("inside-the-original");
        let profile = dir.path().join("profile");
        std::os::unix::fs::symlink(&victim, profile_lock_path(&profile)).expect("symlink");

        assert!(ProfileLock::acquire(&profile).is_err());
        assert!(!victim.exists(), "the symlink target must not be created");
    }

    #[test]
    fn write_sidecar_replaces_a_symlink_instead_of_its_target() {
        // A plain write would follow the link and truncate the victim — and
        // the victim here is the kind of file this whole command exists to
        // protect.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let victim = dir.path().join("precious");
        std::fs::write(&victim, b"do not destroy").expect("write");
        let sidecar = dir.path().join("profile.import");
        std::os::unix::fs::symlink(&victim, &sidecar).expect("symlink");

        write_sidecar(&sidecar, b"{}").expect("write sidecar");
        assert_eq!(std::fs::read(&victim).unwrap(), b"do not destroy");
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"{}");
        assert!(!sidecar.symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[test]
    fn write_sidecar_leaves_no_temporary_behind() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        write_sidecar(&dir.path().join("x.import"), b"a").expect("write");
        write_sidecar(&dir.path().join("x.import"), b"bb").expect("overwrite");
        assert_eq!(std::fs::read(dir.path().join("x.import")).unwrap(), b"bb");
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tabd-tmp-"))
            .collect();
        assert!(strays.is_empty(), "temporary left behind");
    }

    #[test]
    fn session_restore_reads_the_pref() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let default = dir.path().join("Default");
        std::fs::create_dir_all(&default).expect("mkdir");
        let prefs = default.join("Preferences");

        // No file at all — a profile that has never been launched.
        assert_eq!(session_restore(dir.path()), SessionRestore::Unknown);

        std::fs::write(&prefs, r#"{"session":{"restore_on_startup":1}}"#).unwrap();
        assert_eq!(session_restore(dir.path()), SessionRestore::On);

        // 5 is Chromium's default (new tab page) and 4 is a fixed URL list.
        // Neither brings the tabs back, so both are Off.
        for other in [4, 5] {
            std::fs::write(
                &prefs,
                format!(r#"{{"session":{{"restore_on_startup":{other}}}}}"#),
            )
            .unwrap();
            assert_eq!(session_restore(dir.path()), SessionRestore::Off);
        }

        // Absent key = Chromium's default = tabs are lost.
        std::fs::write(&prefs, r#"{"session":{}}"#).unwrap();
        assert_eq!(session_restore(dir.path()), SessionRestore::Off);

        // Unparseable is Unknown, not Off — we did not learn anything.
        std::fs::write(&prefs, "not json").unwrap();
        assert_eq!(session_restore(dir.path()), SessionRestore::Unknown);
    }
}
