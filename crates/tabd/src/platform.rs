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
/// requires an existing path, so the parent is resolved and the final
/// component appended.
pub fn canonical_profile_dir(profile_dir: &Path) -> Result<PathBuf> {
    if let Ok(resolved) = profile_dir.canonicalize() {
        return Ok(resolved);
    }
    let parent = profile_dir.parent().unwrap_or_else(|| Path::new("."));
    let name = profile_dir.file_name().with_context(|| {
        format!(
            "profile path has no final component: {}",
            profile_dir.display()
        )
    })?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create profile parent {}", parent.display()))?;
    let parent = parent
        .canonicalize()
        .with_context(|| format!("resolve profile parent {}", parent.display()))?;
    Ok(parent.join(name))
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
        // Resolving must not create the profile itself — only its parent.
        assert!(!resolved.exists());
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
