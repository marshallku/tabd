use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::path::PathBuf;
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
                        let _ = s.send(Err(anyhow::Error::new(err).context("read chromium stderr")));
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

fn discover_chromium() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("BROWSER_EXECUTABLE")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }
    for candidate in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ] {
        if let Some(path) = which(candidate) {
            return Ok(path);
        }
    }
    if let Some(path) = playwright_cache_chromium() {
        return Ok(path);
    }
    Err(anyhow!(
        "no Chromium binary found. Set $BROWSER_EXECUTABLE, install chromium via your system package manager, or run `npx playwright install chromium`"
    ))
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

fn playwright_cache_chromium() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let cache_root = home.join(".cache/ms-playwright");
    let entries = std::fs::read_dir(&cache_root).ok()?;

    let mut versions: Vec<(u32, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let rest = name.to_string_lossy().strip_prefix("chromium-")?.to_owned();
            let ver = rest.parse::<u32>().ok()?;
            let chrome = e.path().join("chrome-linux64/chrome");
            chrome.is_file().then_some((ver, chrome))
        })
        .collect();
    versions.sort_by_key(|(v, _)| std::cmp::Reverse(*v));
    versions.into_iter().next().map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::parse_devtools_port;

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
