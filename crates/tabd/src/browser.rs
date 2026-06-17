use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};

const STDERR_WAIT: Duration = Duration::from_secs(10);
const JSON_VERSION_WAIT: Duration = Duration::from_secs(5);
const JSON_VERSION_POLL: Duration = Duration::from_millis(200);
const GRACEFUL_WAIT: Duration = Duration::from_secs(2);

pub struct Browser {
    child: Child,
    ws_endpoint: String,
    // Kept alive to defer tempdir cleanup until Browser is dropped.
    _user_data_dir: TempDir,
}

#[derive(Deserialize)]
struct JsonVersion {
    #[serde(rename = "webSocketDebuggerUrl")]
    web_socket_debugger_url: String,
}

impl Browser {
    pub async fn launch() -> Result<Self> {
        let executable = discover_chromium()?;
        let user_data_dir = TempDir::new().context("create tempdir for --user-data-dir")?;

        let mut child = Command::new(&executable)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-first-run",
                "--no-default-browser-check",
                "--no-sandbox",
                "--disable-dev-shm-usage",
                "--disable-background-networking",
                "--disable-sync",
                "--disable-extensions",
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
            ])
            .arg(format!(
                "--user-data-dir={}",
                user_data_dir.path().display()
            ))
            .arg("about:blank")
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn chromium: {}", executable.display()))?;

        let stderr = child.stderr.take().expect("stderr piped");
        let port = match timeout(STDERR_WAIT, spawn_stderr_scanner(stderr)).await {
            Ok(Ok(p)) => p,
            Ok(Err(err)) => {
                let _ = child.kill().await;
                return Err(err);
            }
            Err(_) => {
                let _ = child.kill().await;
                bail!(
                    "failed to capture 'DevTools listening on ws://...' from chromium stderr within {}s",
                    STDERR_WAIT.as_secs()
                );
            }
        };

        let ws_endpoint = match probe_json_version(port).await {
            Ok(url) => url,
            Err(err) => {
                let _ = child.kill().await;
                return Err(err);
            }
        };

        Ok(Browser {
            child,
            ws_endpoint,
            _user_data_dir: user_data_dir,
        })
    }

    pub fn ws_endpoint(&self) -> &str {
        &self.ws_endpoint
    }

    /// PID of the spawned Chromium process. `None` if the child has already
    /// been reaped or was never assigned a PID by the OS (shouldn't happen
    /// post-launch, but std::process::Child::id returns Option).
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Liveness check for the supervisor, tied to the owned `Child` (works on
    /// every platform, immune to PID reuse, and reaps the zombie that the old
    /// Linux-only /proc State parse existed to detect). Non-blocking.
    /// tokio caches the exit status, so once this returns false it stays false.
    pub fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(Some(_status)) => false,
            Ok(None) => true,
            Err(err) => {
                // Conservative: don't trigger restart storms on a wait error,
                // but leave a trace so persistent failures are diagnosable.
                eprintln!("[tabd daemon] warn: chromium try_wait failed: {err}");
                true
            }
        }
    }

    /// Graceful shutdown: SIGTERM → wait up to `GRACEFUL_WAIT` → SIGKILL fallback.
    /// `kill_on_drop(true)` covers panic / early-drop paths separately.
    /// Once cdp.rs lands (task #16) this can additionally send CDP `Browser.close`
    /// before the signal escalation, but SIGTERM is already a clean exit for chromium.
    pub async fn shutdown(mut self) -> Result<()> {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // SAFETY: pid is valid until the child is reaped; we only deliver a signal.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        match timeout(GRACEFUL_WAIT, self.child.wait()).await {
            Ok(_) => Ok(()),
            Err(_) => {
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
                Ok(())
            }
        }
    }
}

async fn spawn_stderr_scanner<R>(reader: R) -> Result<u16>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = oneshot::channel::<Result<u16>>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        let mut sender = Some(tx);
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if sender.is_some()
                        && let Some(port) = parse_devtools_port(&line)
                        && let Some(s) = sender.take()
                    {
                        let _ = s.send(Ok(port));
                    }
                    // Keep draining after match so chromium's stderr buffer
                    // never fills and blocks the child.
                }
                Ok(None) => {
                    if let Some(s) = sender.take() {
                        let _ = s.send(Err(anyhow!(
                            "chromium stderr closed before 'DevTools listening on ws://...' appeared"
                        )));
                    }
                    return;
                }
                Err(err) => {
                    if let Some(s) = sender.take() {
                        let _ =
                            s.send(Err(anyhow::Error::new(err).context("read chromium stderr")));
                    }
                    return;
                }
            }
        }
    });
    rx.await
        .map_err(|_| anyhow!("stderr scanner task dropped"))?
}

fn parse_devtools_port(line: &str) -> Option<u16> {
    // Strict match: "DevTools listening on ws://HOST:PORT/devtools/browser/UUID".
    // Anchoring on the prefix avoids picking up any earlier ws://-containing log line.
    let after_prefix = line
        .split_once("DevTools listening on ws://")
        .map(|(_, r)| r)?;
    let (host_port, _) = after_prefix.split_once('/')?;
    let (_, port_str) = host_port.rsplit_once(':')?;
    port_str.parse::<u16>().ok()
}

async fn probe_json_version(port: u16) -> Result<String> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("build reqwest client")?;

    let deadline = Instant::now() + JSON_VERSION_WAIT;
    let mut last_err: Option<anyhow::Error> = None;
    while Instant::now() < deadline {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: JsonVersion = resp.json().await.context("parse /json/version body")?;
                if body.web_socket_debugger_url.is_empty() {
                    bail!("/json/version returned empty webSocketDebuggerUrl");
                }
                return Ok(body.web_socket_debugger_url);
            }
            Ok(resp) => {
                last_err = Some(anyhow!("/json/version returned status {}", resp.status()));
            }
            Err(err) => {
                last_err = Some(anyhow::Error::new(err).context("GET /json/version"));
            }
        }
        sleep(JSON_VERSION_POLL).await;
    }
    Err(last_err.unwrap_or_else(|| {
        anyhow!(
            "/json/version not ready within {}s on port {port}",
            JSON_VERSION_WAIT.as_secs()
        )
    }))
}

/// Chromium-based executables to probe on `$PATH`, highest priority first.
/// Chrome / Chromium proper come before Edge and Brave, which also speak CDP
/// and work as a last resort. These are the common Linux package names; macOS
/// ships `.app` bundles instead (see [`app_bundle_candidates`]).
const PATH_CANDIDATES: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "chrome",
    "microsoft-edge",
    "brave-browser",
];

/// Locate a launchable Chromium-based browser. Resolution order, first hit wins:
///   1. `$BROWSER_EXECUTABLE` (explicit override)
///   2. a [`PATH_CANDIDATES`] name on `$PATH` (the Linux path)
///   3. a standard macOS `.app` bundle ([`app_bundle_candidates`])
///   4. the most recent Playwright-cached Chromium ([`playwright_cache_chromium`])
fn discover_chromium() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("BROWSER_EXECUTABLE")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }
    for &candidate in PATH_CANDIDATES {
        if let Some(path) = which(candidate) {
            return Ok(path);
        }
    }
    // macOS keeps browsers in .app bundles that aren't on $PATH.
    for candidate in app_bundle_candidates() {
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    if let Some(path) = playwright_cache_chromium() {
        return Ok(path);
    }
    Err(anyhow!(
        "no Chromium-based browser found. Set $BROWSER_EXECUTABLE, install Chrome/Chromium via your system package manager, or run `npx playwright install chromium`"
    ))
}

/// Standard macOS application-bundle binaries for Chromium-based browsers, in
/// priority order, under both `/Applications` and `~/Applications`. Empty on
/// non-macOS targets, which rely on `$PATH` + the Playwright cache instead.
fn app_bundle_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        // (bundle dir, binary name inside Contents/MacOS).
        const BUNDLES: &[(&str, &str)] = &[
            ("Google Chrome.app", "Google Chrome"),
            ("Chromium.app", "Chromium"),
            ("Google Chrome Canary.app", "Google Chrome Canary"),
            ("Microsoft Edge.app", "Microsoft Edge"),
            ("Brave Browser.app", "Brave Browser"),
        ];
        let mut roots: Vec<PathBuf> = vec![PathBuf::from("/Applications")];
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join("Applications"));
        }
        let mut out = Vec::with_capacity(BUNDLES.len() * roots.len());
        for (bundle, binary) in BUNDLES {
            for root in &roots {
                out.push(root.join(bundle).join("Contents/MacOS").join(binary));
            }
        }
        out
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| is_executable_file(p))
}

#[cfg(unix)]
fn is_executable_file(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(p: &std::path::Path) -> bool {
    p.is_file()
}

/// Relative path from a `chromium-<rev>` cache dir to the launchable binary,
/// per OS, tried in order. Newer Playwright uses `chrome-linux64`; older builds
/// used `chrome-linux`. macOS ships the binary inside a `.app` bundle.
#[cfg(target_os = "macos")]
const PLAYWRIGHT_REL_CANDIDATES: &[&str] = &["chrome-mac/Chromium.app/Contents/MacOS/Chromium"];
#[cfg(not(target_os = "macos"))]
const PLAYWRIGHT_REL_CANDIDATES: &[&str] = &["chrome-linux64/chrome", "chrome-linux/chrome"];

/// Most recent Chromium from the Playwright browser cache, if present. Handles
/// the per-OS cache root and bundle layout.
fn playwright_cache_chromium() -> Option<PathBuf> {
    let cache_root = playwright_cache_root()?;
    let entries = std::fs::read_dir(&cache_root).ok()?;

    let mut versions: Vec<(u32, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let ver = parse_chromium_dir_version(&e.file_name().to_string_lossy())?;
            let base = e.path();
            // First layout whose binary actually exists on disk.
            let chrome = PLAYWRIGHT_REL_CANDIDATES
                .iter()
                .map(|rel| base.join(rel))
                .find(|p| p.is_file())?;
            Some((ver, chrome))
        })
        .collect();
    versions.sort_by_key(|(v, _)| std::cmp::Reverse(*v));
    versions.into_iter().next().map(|(_, p)| p)
}

/// Resolve the Playwright browser cache root from the environment.
fn playwright_cache_root() -> Option<PathBuf> {
    resolve_cache_root(
        std::env::var_os("PLAYWRIGHT_BROWSERS_PATH").as_deref(),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
    )
}

/// Pure resolver for the cache root. `$PLAYWRIGHT_BROWSERS_PATH` wins when set
/// to a real path (CI often relocates the cache there); `"0"` is Playwright's
/// "install next to the package" sentinel we can't resolve, so it falls back to
/// the per-OS default under `$HOME`.
fn resolve_cache_root(custom: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(custom) = custom
        && !custom.is_empty()
        && custom != OsStr::new("0")
    {
        return Some(PathBuf::from(custom));
    }
    let home = home?;
    #[cfg(target_os = "macos")]
    {
        Some(home.join("Library/Caches/ms-playwright"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(home.join(".cache/ms-playwright"))
    }
}

/// Parse the chromium revision from a Playwright cache dir name
/// (`chromium-1217` → `1217`). Rejects other browsers and the
/// `chromium_headless_shell-*` variant (full Chromium only).
fn parse_chromium_dir_version(dir_name: &str) -> Option<u32> {
    dir_name.strip_prefix("chromium-")?.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::{
        PATH_CANDIDATES, PLAYWRIGHT_REL_CANDIDATES, parse_chromium_dir_version,
        parse_devtools_port, resolve_cache_root,
    };
    use std::ffi::OsStr;
    use std::path::Path;

    // -- Browser discovery (cross-platform) --

    #[test]
    fn chromium_dir_version_parses_and_rejects() {
        assert_eq!(parse_chromium_dir_version("chromium-1217"), Some(1217));
        // Other browsers / non-numeric / empty revision are rejected.
        assert_eq!(parse_chromium_dir_version("firefox-1487"), None);
        assert_eq!(parse_chromium_dir_version("chromium-"), None);
        assert_eq!(parse_chromium_dir_version("chromium-beta"), None);
        // The headless-shell variant must NOT match (we want full Chromium).
        assert_eq!(
            parse_chromium_dir_version("chromium_headless_shell-1217"),
            None
        );
    }

    #[test]
    fn cache_root_prefers_real_custom_path() {
        let root = resolve_cache_root(Some(OsStr::new("/opt/pw")), Some(Path::new("/home/x")));
        assert_eq!(root, Some(std::path::PathBuf::from("/opt/pw")));
    }

    #[test]
    fn cache_root_ignores_sentinel_and_empty_custom() {
        // "0" (install-next-to-package) and "" fall back to the $HOME default.
        for custom in [Some(OsStr::new("0")), Some(OsStr::new("")), None] {
            let root =
                resolve_cache_root(custom, Some(Path::new("/home/x"))).expect("home-based root");
            assert!(root.starts_with("/home/x"), "got: {}", root.display());
            assert!(root.ends_with("ms-playwright"), "got: {}", root.display());
        }
    }

    #[test]
    fn cache_root_none_without_home() {
        assert_eq!(resolve_cache_root(None, None), None);
    }

    #[test]
    fn path_candidates_prioritize_chrome_over_alternatives() {
        // Chrome/Chromium proper must be probed before Edge/Brave fallbacks.
        let chrome = PATH_CANDIDATES.iter().position(|c| *c == "google-chrome");
        let edge = PATH_CANDIDATES.iter().position(|c| *c == "microsoft-edge");
        assert!(chrome < edge, "chrome should rank before edge");
        assert!(!PLAYWRIGHT_REL_CANDIDATES.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_app_bundles_are_absolute_and_chrome_first() {
        let candidates = super::app_bundle_candidates();
        assert!(!candidates.is_empty(), "macOS must offer .app candidates");
        // Every candidate is an absolute path into a Chromium-based .app bundle.
        for c in &candidates {
            assert!(c.is_absolute(), "got: {}", c.display());
            assert!(c.to_string_lossy().contains("Contents/MacOS"));
        }
        // Google Chrome is the first thing tried.
        assert!(
            candidates[0].to_string_lossy().contains("Google Chrome"),
            "got: {}",
            candidates[0].display()
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_has_no_app_bundles() {
        assert!(super::app_bundle_candidates().is_empty());
    }

    #[test]
    fn parses_devtools_line() {
        let line = "DevTools listening on ws://127.0.0.1:54321/devtools/browser/abc-def";
        assert_eq!(parse_devtools_port(line), Some(54321));
    }

    #[test]
    fn rejects_unrelated_line() {
        assert_eq!(parse_devtools_port("[INFO] something else"), None);
    }

    #[test]
    fn handles_high_port() {
        let line = "DevTools listening on ws://127.0.0.1:65535/devtools/browser/x";
        assert_eq!(parse_devtools_port(line), Some(65535));
    }

    #[test]
    fn rejects_overflow_port() {
        let line = "DevTools listening on ws://127.0.0.1:65536/devtools/browser/x";
        assert_eq!(parse_devtools_port(line), None);
    }

    #[test]
    fn rejects_stray_ws_url_without_prefix() {
        // Earlier log lines that happen to contain ws:// must not be mistaken
        // for the actual DevTools endpoint announcement (codex I1).
        let line = "[1234:567:0123/abc:ERROR:foo.cc(99)] connected via ws://10.0.0.1:9999/api";
        assert_eq!(parse_devtools_port(line), None);
    }

    /// Wrap an arbitrary spawned child in a Browser so is_alive() can be
    /// exercised without a real chromium.
    fn browser_for_child(child: tokio::process::Child) -> super::Browser {
        super::Browser {
            child,
            ws_endpoint: String::new(),
            _user_data_dir: tempfile::TempDir::new().expect("tempdir"),
        }
    }

    #[tokio::test]
    async fn is_alive_true_for_running_child() {
        let child = tokio::process::Command::new("sleep")
            .arg("5")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep");
        let mut browser = browser_for_child(child);
        assert!(browser.is_alive());
    }

    #[tokio::test]
    async fn is_alive_false_after_exit_and_stays_false() {
        let mut child = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let _ = child.wait().await; // ensure it has exited (also reaps)
        let mut browser = browser_for_child(child);
        assert!(!browser.is_alive());
        // tokio caches the exit status — repeated checks stay false.
        assert!(!browser.is_alive());
    }

    // Real-chromium smoke. Skipped by default; run with:
    //   cargo test --bin cdp-spike -- --ignored launch_smoke
    #[tokio::test]
    #[ignore = "requires real chromium; covers spawn + stderr scan + /json/version probe"]
    async fn launch_smoke() {
        let browser = super::Browser::launch().await.expect("launch chromium");
        assert!(
            browser.ws_endpoint().starts_with("ws://"),
            "ws_endpoint should start with ws://, got: {}",
            browser.ws_endpoint()
        );
        browser.shutdown().await.expect("shutdown");
    }
}
