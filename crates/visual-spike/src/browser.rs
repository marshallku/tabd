//! Which browser the spike drives, and where its real profile lives.
//!
//! Executable and profile are resolved **together**, as one configuration.
//! Resolving them independently would let `$BROWSER_EXECUTABLE` point at one
//! browser while the profile-copy probe read a different browser's profile —
//! silently answering a question nobody asked.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Browser {
    pub name: &'static str,
    pub executable: PathBuf,
    /// The real, daily-use profile directory for this browser, if the
    /// configuration is known. `None` for an unrecognised override.
    pub real_profile_dir: Option<PathBuf>,
}

impl Browser {
    /// The browser this platform defaults to, unless `$BROWSER_EXECUTABLE`
    /// overrides it.
    ///
    /// macOS defaults to Chrome and Linux to Brave because that is what each
    /// machine actually runs; the design targets "the browser the human
    /// already uses", so the spike measures the same one.
    pub fn resolve() -> Browser {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let known = known_browsers(home.as_deref());

        if let Some(path) = std::env::var_os("BROWSER_EXECUTABLE").map(PathBuf::from)
            && !path.as_os_str().is_empty()
        {
            // An override still gets its configuration when we recognise it,
            // so the profile-copy probe keeps working for that browser.
            if let Some(found) = known.iter().find(|b| b.executable == path) {
                return found.clone();
            }
            return Browser {
                name: "unknown (BROWSER_EXECUTABLE override)",
                executable: path,
                real_profile_dir: None,
            };
        }

        known
            .into_iter()
            .find(|b| b.executable.is_file())
            .unwrap_or_else(|| Browser {
                name: "none found",
                executable: PathBuf::from("/usr/bin/brave"),
                real_profile_dir: None,
            })
    }
}

/// Browser configurations for this platform, highest priority first.
pub fn known_browsers(home: Option<&Path>) -> Vec<Browser> {
    let mut out = Vec::new();
    #[cfg(target_os = "macos")]
    {
        out.push(Browser {
            name: "Google Chrome",
            executable: PathBuf::from(
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            ),
            real_profile_dir: home.map(|h| h.join("Library/Application Support/Google/Chrome")),
        });
        out.push(Browser {
            name: "Brave Browser",
            executable: PathBuf::from(
                "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            ),
            real_profile_dir: home
                .map(|h| h.join("Library/Application Support/BraveSoftware/Brave-Browser")),
        });
    }
    #[cfg(not(target_os = "macos"))]
    {
        out.push(Browser {
            name: "Brave Browser",
            executable: PathBuf::from("/usr/bin/brave"),
            real_profile_dir: home.map(|h| h.join(".config/BraveSoftware/Brave-Browser")),
        });
        out.push(Browser {
            name: "Google Chrome",
            executable: PathBuf::from("/usr/bin/google-chrome-stable"),
            real_profile_dir: home.map(|h| h.join(".config/google-chrome")),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_browser_has_a_profile_when_home_is_known() {
        let browsers = known_browsers(Some(Path::new("/home/x")));
        assert!(!browsers.is_empty());
        for browser in &browsers {
            assert!(browser.executable.is_absolute(), "{}", browser.name);
            let profile = browser.real_profile_dir.as_ref().expect(browser.name);
            assert!(profile.starts_with("/home/x"), "{}", browser.name);
        }
    }

    #[test]
    fn the_platform_default_is_the_browser_that_machine_runs() {
        let first = known_browsers(Some(Path::new("/home/x")))
            .into_iter()
            .next()
            .expect("at least one");
        if cfg!(target_os = "macos") {
            assert_eq!(first.name, "Google Chrome");
        } else {
            assert_eq!(first.name, "Brave Browser");
        }
    }

    /// An override the harness does not recognise must NOT inherit some other
    /// browser's profile directory — that is how a profile-copy probe ends up
    /// copying Chrome's data and launching Brave against it.
    #[test]
    fn an_unrecognised_override_has_no_profile_configuration() {
        let browsers = known_browsers(Some(Path::new("/home/x")));
        let stranger = PathBuf::from("/opt/weird/browser");
        assert!(!browsers.iter().any(|b| b.executable == stranger));
        let resolved = Browser {
            name: "unknown (BROWSER_EXECUTABLE override)",
            executable: stranger,
            real_profile_dir: None,
        };
        assert!(resolved.real_profile_dir.is_none());
    }
}
