//! Attach mode: drive the browser the human already has open, instead of a
//! second one on a copied profile.
//!
//! Chrome 144+ can expose a running instance through
//! `chrome://inspect/#remote-debugging` ("Allow remote debugging for this
//! browser instance"). It then writes `<user-data-dir>/DevToolsActivePort` and
//! asks the human to approve every connection, showing the "controlled by
//! automated test software" banner while one is attached. That is the whole
//! handshake, so this daemon:
//!
//! - **never launches, restarts or closes the browser.** It connects lazily on
//!   the first driver action, and `daemon.shutdown` only detaches.
//! - **sees only its own tabs.** The client runs in `TabScope::Owned`: tabs it
//!   opened (all in one agent window) and the tabs those open. The human's
//!   tabs are never listed, indexed, attached or closed.
//! - **refuses browser-global actions** whose effect cannot be confined to
//!   those tabs (download routing, the profile's cookie jar).
//!
//! A lost connection (Chrome quit, the human revoked it) is not retried in the
//! background: each attempt raises a dialog in front of the human, so the next
//! driver action reconnects, and the tabs from the previous connection are no
//! longer ours.

use super::*;
use crate::cdp::Transport;

/// How long a connection attempt may take, end to end: the human may be
/// reading the Allow dialog. `$TABD_ATTACH_TIMEOUT_MS` overrides it.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// A `DevToolsActivePort` whose port does not answer is stale — the browser
/// that wrote it is gone or has debugging switched off.
const PORT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

const ENABLE_HINT: &str = "open chrome://inspect/#remote-debugging in your browser and turn on \
                           \"Allow remote debugging for this browser instance\" (Chrome 144+)";

/// Actions an attach daemon refuses, with why. Everything else is served
/// against owned tabs exactly as on a headless daemon.
const REFUSED: &[(&str, &str)] = &[
    (
        "browser.setDownloadDir",
        "download routing is browser-wide and would redirect the human's own downloads",
    ),
    (
        "monitor.downloads",
        "the download log is browser-wide, not per tab",
    ),
    (
        "wait.download",
        "the download log is browser-wide, not per tab",
    ),
    (
        "cookies.get",
        "cookies are the real profile's, for any site, including HttpOnly session cookies",
    ),
    (
        "cookies.set",
        "cookies are the real profile's, for any site",
    ),
    (
        "cookies.delete",
        "cookies are the real profile's, for any site",
    ),
];

/// `Some(error message)` when `action` is not served in attach mode.
pub(super) fn refusal(action: &str) -> Option<String> {
    REFUSED
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(name, why)| format!("action '{name}' is not available on an attach daemon ({why})"))
}

#[derive(Default)]
pub(super) struct AttachState {
    /// Serializes connection attempts, publication and shutdown. Held across
    /// the slow part (the human's Allow dialog) so concurrent first actions
    /// raise one dialog, not several.
    gate: Mutex<()>,
    /// Where the current connection came from, for `daemon.health`.
    endpoint: std::sync::Mutex<Option<String>>,
}

impl AttachState {
    pub(super) fn endpoint(&self) -> Option<String> {
        self.endpoint.lock().ok().and_then(|e| e.clone())
    }

    fn set_endpoint(&self, value: Option<String>) {
        if let Ok(mut e) = self.endpoint.lock() {
            *e = value;
        }
    }
}

// -- Endpoint discovery -----------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct ActivePort {
    port: u16,
    path: String,
}

/// Parse `DevToolsActivePort`: the port on line one, the browser endpoint
/// path on line two.
fn parse_active_port(content: &str) -> std::result::Result<ActivePort, String> {
    let mut lines = content.lines().map(str::trim).filter(|l| !l.is_empty());
    let port = lines
        .next()
        .ok_or("empty DevToolsActivePort")?
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or("DevToolsActivePort has no valid port")?;
    let path = lines
        .next()
        .ok_or("DevToolsActivePort has no endpoint path")?;
    if !path.starts_with("/devtools/browser/") || path.chars().any(|c| c.is_whitespace()) {
        return Err(format!(
            "DevToolsActivePort has an unexpected path {path:?}"
        ));
    }
    Ok(ActivePort {
        port,
        path: path.to_owned(),
    })
}

/// `$TABD_ATTACH_WS` must point at this machine: attach mode is for the
/// human's own browser, and a remote endpoint would carry their session
/// across the network in the clear.
fn validate_ws_url(url: &str) -> std::result::Result<String, String> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| format!("TABD_ATTACH_WS must be a ws:// url, got {url:?}"))?;
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return Err(format!(
            "TABD_ATTACH_WS must be a loopback address (127.0.0.1, localhost, [::1]), got {host:?}"
        ));
    }
    Ok(url.to_owned())
}

struct Endpoint {
    ws_url: String,
    /// Human-readable origin of `ws_url` — the user-data-dir, or the env var.
    source: String,
}

async fn port_answers(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            PORT_PROBE_TIMEOUT,
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await,
        Ok(Ok(_))
    )
}

enum Probe {
    Live(Endpoint),
    Stale,
    Absent,
    Unreadable(String),
}

async fn probe_dir(dir: &Path) -> Probe {
    let file = dir.join("DevToolsActivePort");
    let content = match std::fs::read_to_string(&file) {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Probe::Absent,
        Err(err) => return Probe::Unreadable(format!("{}: {err}", file.display())),
    };
    let active = match parse_active_port(&content) {
        Ok(a) => a,
        Err(err) => return Probe::Unreadable(format!("{}: {err}", file.display())),
    };
    if !port_answers(active.port).await {
        return Probe::Stale;
    }
    Probe::Live(Endpoint {
        ws_url: format!("ws://127.0.0.1:{}{}", active.port, active.path),
        source: dir.display().to_string(),
    })
}

/// `$TABD_ATTACH_WS`, else `$TABD_ATTACH_USER_DATA_DIR`, else the first
/// candidate user-data-dir with a live `DevToolsActivePort`. Stale files are
/// skipped, so a Chrome that exited does not mask a running Brave.
async fn resolve_endpoint() -> std::result::Result<Endpoint, String> {
    if let Ok(ws) = std::env::var("TABD_ATTACH_WS") {
        return Ok(Endpoint {
            ws_url: validate_ws_url(&ws)?,
            source: "$TABD_ATTACH_WS".to_string(),
        });
    }
    let (dirs, explicit) = match std::env::var_os("TABD_ATTACH_USER_DATA_DIR") {
        Some(dir) => (vec![PathBuf::from(dir)], true),
        None => (
            platform::attach_candidate_dirs().map_err(|e| format!("{e:#}"))?,
            false,
        ),
    };
    let mut stale = Vec::new();
    let mut unreadable = Vec::new();
    for dir in &dirs {
        match probe_dir(dir).await {
            Probe::Live(endpoint) => return Ok(endpoint),
            Probe::Stale => stale.push(dir.display().to_string()),
            Probe::Unreadable(why) => unreadable.push(why),
            Probe::Absent => {}
        }
    }
    let mut message = if explicit {
        format!(
            "no running browser to attach to in $TABD_ATTACH_USER_DATA_DIR ({})",
            dirs[0].display()
        )
    } else {
        "no running browser to attach to".to_string()
    };
    if !stale.is_empty() {
        message.push_str(&format!(
            "; DevToolsActivePort in {} does not answer (browser closed or debugging turned off)",
            stale.join(", ")
        ));
    }
    if !unreadable.is_empty() {
        message.push_str(&format!("; {}", unreadable.join("; ")));
    }
    message.push_str(&format!(" — {ENABLE_HINT}"));
    Err(message)
}

fn connect_timeout() -> Duration {
    std::env::var("TABD_ATTACH_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT)
}

// -- Connection lifecycle ---------------------------------------------------

/// Connect, then wait for the browser to actually answer. Chrome may hold the
/// connection while the human looks at the Allow dialog, so the liveness call
/// is retried rather than failed on its first RPC timeout; the caller bounds
/// the whole thing with one deadline.
async fn connect(endpoint: &Endpoint) -> Result<CdpClient> {
    let client = CdpClient::connect_with(ConnectOptions {
        transport: Transport::WebSocket(endpoint.ws_url.clone()),
        bootstrap_tab: false,
        scope: TabScope::Owned,
    })
    .await
    .with_context(|| format!("connect to the browser at {}", endpoint.source))?;
    let ready = async {
        loop {
            match client.send_browser("Browser.getVersion", json!({})).await {
                Ok(_) => break,
                Err(err) if client.is_transport_closed() => return Err(err),
                // Most likely an RPC timeout while the dialog is up.
                Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
        client.enable_target_discovery().await
    };
    if let Err(err) = ready.await {
        let _ = client.close().await;
        return Err(
            err.context("the browser closed the connection — was remote debugging declined?")
        );
    }
    Ok(client)
}

async fn wait_for_shutdown(state: &DaemonState) {
    loop {
        let notified = state.drain_notify.notified();
        tokio::pin!(notified);
        if state.drain_started.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

async fn live_client(state: &DaemonState) -> Option<Arc<CdpClient>> {
    state
        .client
        .lock()
        .await
        .clone()
        .filter(|c| !c.is_transport_closed())
}

/// The connected client, connecting first if there is none. Every driver
/// action on an attach daemon passes through here.
pub(super) async fn ensure_connected(
    state: &DaemonState,
) -> std::result::Result<Arc<CdpClient>, String> {
    if let Some(client) = live_client(state).await {
        return Ok(client);
    }
    let _gate = state.attach.gate.lock().await;
    if state.drain_started.load(Ordering::Acquire) {
        return Err("daemon is shutting down (drain in progress)".to_string());
    }
    if let Some(client) = live_client(state).await {
        return Ok(client); // another request connected while we waited
    }
    // A dead client the watcher has not cleared yet.
    if let Some(dead) = state.client.lock().await.take() {
        let _ = dead.close().await;
    }

    let endpoint = resolve_endpoint().await?;
    let timeout = connect_timeout();
    let attempt = tokio::time::timeout(timeout, connect(&endpoint));
    let client = tokio::select! {
        outcome = attempt => match outcome {
            Ok(Ok(client)) => client,
            Ok(Err(err)) => {
                let message = format!("attach failed: {err:#}");
                *state.not_ready_reason.lock().await = Some(message.clone());
                return Err(message);
            }
            Err(_) => {
                let message = format!(
                    "attach timed out after {}s waiting for {} — approve the \
                     remote-debugging prompt in the browser, or raise $TABD_ATTACH_TIMEOUT_MS",
                    timeout.as_secs(),
                    endpoint.source
                );
                *state.not_ready_reason.lock().await = Some(message.clone());
                return Err(message);
            }
        },
        // Dropping the attempt drops its client, whose transport tasks abort.
        _ = wait_for_shutdown(state) => {
            return Err("daemon is shutting down (drain in progress)".to_string());
        }
    };

    // Publication is the last step and happens under the gate, after a final
    // shutdown check, so a connection that finished late cannot resurrect a
    // daemon that is draining.
    if state.drain_started.load(Ordering::Acquire) {
        let _ = client.close().await;
        return Err("daemon is shutting down (drain in progress)".to_string());
    }
    let client = Arc::new(client);
    *state.client.lock().await = Some(client.clone());
    state.attach.set_endpoint(Some(endpoint.source));
    *state.not_ready_reason.lock().await = None;
    state.ready.store(true, Ordering::Release);
    tokio::spawn(watch(state.clone(), client.clone()));
    Ok(client)
}

/// One per connection. Clears the published client when its transport ends,
/// unless a newer connection has replaced it.
async fn watch(state: DaemonState, client: Arc<CdpClient>) {
    client.transport_closed().await;
    let _gate = state.attach.gate.lock().await;
    let mut slot = state.client.lock().await;
    if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &client)) {
        slot.take();
        state.ready.store(false, Ordering::Release);
        *state.not_ready_reason.lock().await = Some("browser_disconnected".to_string());
        eprintln!("[tabd daemon] attached browser went away; next action reconnects");
    }
}

/// `daemon.shutdown` cleanup: detach from our tabs and leave everything else
/// — the browser, the agent window, the human's tabs — exactly as it is.
pub(super) async fn shutdown(state: &DaemonState) {
    let _gate = state.attach.gate.lock().await;
    state.ready.store(false, Ordering::Release);
    if let Some(client) = state.client.lock().await.take() {
        let _ = client.close().await;
    }
}

pub(super) async fn health_fields(state: &DaemonState, body: &mut Value) {
    let client = live_client(state).await;
    body["connected"] = json!(client.is_some());
    body["endpoint"] = json!(state.attach.endpoint());
    body["ownedTabs"] = match &client {
        Some(c) => json!(c.tab_count().await),
        None => json!(0),
    };
    body["notReadyReason"] = json!(state.not_ready_reason.lock().await.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_port_parses_port_and_path() {
        assert_eq!(
            parse_active_port("9222\n/devtools/browser/abc-123\n").unwrap(),
            ActivePort {
                port: 9222,
                path: "/devtools/browser/abc-123".to_string()
            }
        );
        // CRLF and surrounding blank lines are tolerated.
        assert_eq!(
            parse_active_port("\r\n55555\r\n/devtools/browser/x\r\n")
                .unwrap()
                .port,
            55555
        );
    }

    #[test]
    fn malformed_active_port_is_refused() {
        for bad in [
            "",
            "abc\n/devtools/browser/x",
            "0\n/devtools/browser/x",
            "70000\n/devtools/browser/x",
            "9222",
            "9222\n/devtools/page/x",
            "9222\n/devtools/browser/x y",
        ] {
            assert!(parse_active_port(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn ws_override_must_be_loopback() {
        for ok in [
            "ws://127.0.0.1:9222/devtools/browser/x",
            "ws://localhost:9222/devtools/browser/x",
            "ws://[::1]:9222/devtools/browser/x",
        ] {
            validate_ws_url(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "wss://127.0.0.1:9222/devtools/browser/x",
            "ws://10.0.0.5:9222/devtools/browser/x",
            "ws://127.0.0.1.evil.test:9222/x",
            "http://127.0.0.1:9222/",
        ] {
            assert!(validate_ws_url(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn browser_global_actions_are_refused() {
        for action in [
            "browser.setDownloadDir",
            "monitor.downloads",
            "wait.download",
            "cookies.get",
            "cookies.set",
            "cookies.delete",
        ] {
            let message = refusal(action).unwrap_or_else(|| panic!("{action} is served"));
            assert!(
                message.contains("is not available on an attach daemon"),
                "{message}"
            );
        }
        for action in [
            "tabs.navigate",
            "tabs.open",
            "dom.getText",
            "capture.screenshot",
        ] {
            assert!(refusal(action).is_none(), "{action} is refused");
        }
    }

    #[tokio::test]
    async fn a_stale_active_port_is_skipped() {
        // Bind and drop to get a port nobody is listening on.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("DevToolsActivePort"),
            format!("{port}\n/devtools/browser/gone\n"),
        )
        .unwrap();
        assert!(matches!(probe_dir(dir.path()).await, Probe::Stale));

        let live = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live_port = live.local_addr().unwrap().port();
        std::fs::write(
            dir.path().join("DevToolsActivePort"),
            format!("{live_port}\n/devtools/browser/here\n"),
        )
        .unwrap();
        match probe_dir(dir.path()).await {
            Probe::Live(e) => assert_eq!(
                e.ws_url,
                format!("ws://127.0.0.1:{live_port}/devtools/browser/here")
            ),
            _ => panic!("live port not detected"),
        }

        let empty = tempfile::TempDir::new().unwrap();
        assert!(matches!(probe_dir(empty.path()).await, Probe::Absent));
    }
}
