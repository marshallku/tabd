//! Scratch profile directories.
//!
//! The only way a probe can obtain a `--user-data-dir` is [`Scratch::new`],
//! which creates a fresh private directory via `mkdtemp(3)`. There is no
//! "point the spike at an existing directory" flag, so the whole class of
//! "launched a browser inside, or recursively deleted, someone else's tree"
//! is removed by construction rather than by a path check. The path check in
//! [`assert_safe_user_data_dir`] is defence in depth for that invariant.

use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// The real, daily-use profile of the browser being driven. The spike must
/// never launch a browser with it, nor delete anything under it; the
/// profile-copy probe only ever copies out of it.
pub fn real_profile_dir() -> Option<PathBuf> {
    crate::browser::Browser::resolve().real_profile_dir
}

/// Every real profile directory this platform knows about, so the guard
/// protects the ones we are *not* driving too — an override that points at
/// Chrome must not make Brave's profile fair game for deletion.
pub fn all_real_profile_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    crate::browser::known_browsers(home.as_deref())
        .into_iter()
        .filter_map(|b| b.real_profile_dir)
        .collect()
}

/// Reject a user-data-dir that is the real profile, an ancestor of it, or a
/// descendant of it. Both sides are resolved as far as they exist so a symlink
/// alias cannot slip past; a path that does not exist yet is compared by its
/// nearest existing ancestor plus the remaining components.
pub fn assert_safe_user_data_dir(dir: &Path) -> io::Result<()> {
    let candidate = resolve_as_far_as_possible(dir);
    for real in all_real_profile_dirs() {
        // A missing real profile means nothing to protect.
        let Ok(real) = real.canonicalize() else {
            continue;
        };
        let bad = if candidate == real {
            Some("is a real browser profile")
        } else if candidate.starts_with(&real) {
            Some("is inside a real browser profile")
        } else if real.starts_with(&candidate) {
            Some("is an ancestor of a real browser profile")
        } else {
            None
        };
        if let Some(why) = bad {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing to use {}: it {why} ({})",
                    candidate.display(),
                    real.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Canonicalize the longest existing prefix of `path` and re-append the rest,
/// so a not-yet-created directory still compares correctly against a symlinked
/// real profile.
fn resolve_as_far_as_possible(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            let mut out = resolved;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// A private directory owned by this process. `Drop` removes only what this
/// type created — never a caller-supplied path, because there is no way to
/// construct one from a caller-supplied path.
pub struct Scratch {
    path: PathBuf,
    keep: bool,
}

impl Scratch {
    /// `mkdtemp(3)` creates the directory with mode 0700 and fails if it
    /// already exists, so a pre-existing directory, a symlink swap or a
    /// concurrent spike run cannot be raced into.
    pub fn new(tag: &str, keep: bool) -> io::Result<Self> {
        let template = std::env::temp_dir().join(format!("visual-spike-{tag}-XXXXXX"));
        let c_template = CString::new(template.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut buf = c_template.into_bytes_with_nul();

        // SAFETY: `buf` is a NUL-terminated, writable, correctly-sized template.
        let res = unsafe { libc::mkdtemp(buf.as_mut_ptr().cast()) };
        if res.is_null() {
            return Err(io::Error::last_os_error());
        }
        buf.pop(); // drop the NUL
        let path = PathBuf::from(OsString::from_vec(buf));
        assert_safe_user_data_dir(&path)?;
        Ok(Scratch { path, keep })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.keep {
            eprintln!("[scratch] kept {}", self.path.display());
            return;
        }
        // Re-check before a recursive delete: cheap, and the one operation
        // where being wrong is unrecoverable.
        if assert_safe_user_data_dir(&self.path).is_err() {
            eprintln!("[scratch] refusing to remove {}", self.path.display());
            return;
        }
        if let Err(err) = std::fs::remove_dir_all(&self.path) {
            eprintln!("[scratch] remove {} failed: {err}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_is_private_and_self_removing() {
        let path = {
            let scratch = Scratch::new("unit", false).expect("mkdtemp");
            let path = scratch.path().to_path_buf();
            assert!(path.is_dir());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700, "mkdtemp must create 0700");
            }
            path
        };
        assert!(!path.exists(), "Drop must remove what it created");
    }

    #[test]
    fn scratch_paths_are_unique() {
        let a = Scratch::new("unit", false).unwrap();
        let b = Scratch::new("unit", false).unwrap();
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn guard_rejects_real_profile_self_ancestor_and_descendant() {
        let Some(real) = real_profile_dir() else {
            return;
        };
        if real.canonicalize().is_err() {
            return; // no real profile on this machine; nothing to guard
        }
        assert!(assert_safe_user_data_dir(&real).is_err(), "self");
        assert!(
            assert_safe_user_data_dir(&real.join("spike")).is_err(),
            "descendant"
        );
        assert!(
            assert_safe_user_data_dir(&real.join("Default/Network")).is_err(),
            "deep descendant"
        );
        assert!(
            assert_safe_user_data_dir(real.parent().unwrap()).is_err(),
            "ancestor"
        );
    }

    #[test]
    fn guard_allows_unrelated_and_nonexistent_paths() {
        let scratch = Scratch::new("unit", false).unwrap();
        assert!(assert_safe_user_data_dir(scratch.path()).is_ok());
        assert!(assert_safe_user_data_dir(&scratch.child("nested/deep")).is_ok());
    }

    #[test]
    fn guard_sees_through_a_symlink_alias_to_the_real_profile() {
        let Some(real) = real_profile_dir() else {
            return;
        };
        if real.canonicalize().is_err() {
            return;
        }
        let scratch = Scratch::new("unit", false).unwrap();
        let alias = scratch.child("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        assert!(
            assert_safe_user_data_dir(&alias).is_err(),
            "a symlink to the real profile must be rejected"
        );
    }
}
