//! TS-protocol-compatible daemon: newline-delimited JSON-RPC over a Unix
//! domain socket. Spawns one Chromium for the daemon's lifetime and serializes
//! all driver actions through a single mutex. Control actions (`daemon.ping`,
//! `daemon.health`, `daemon.shutdown`) and request dispatch live here; the
//! driver-action handlers are grouped by domain under `src/daemon/` (dom,
//! interaction, tabs, storage, capture, monitor, waits, secrets), each reusing
//! the shared state and helpers in this module via `use super::*`.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};

use crate::browser::Browser;
use crate::cdp::CdpClient;
use crate::cmd::page;

// Domain handler submodules. Each accesses shared state/helpers via `use
// super::*` and exposes its handlers as `pub(super)` for process_request.
mod capture;
mod dom;
pub(crate) mod error;
mod interaction;
mod monitor;
mod secrets;
mod storage;
mod tabs;
mod waits;

// -- Path resolution --------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DaemonPaths {
    pub base_dir: PathBuf,
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
}

pub fn resolve_paths(override_base: Option<&str>) -> Result<DaemonPaths> {
    let base_dir: PathBuf = if let Some(p) = override_base {
        PathBuf::from(p)
    } else if let Ok(d) = std::env::var("TABD_BASE_DIR") {
        PathBuf::from(d)
    } else if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(d).join("tabd")
    } else {
        let home = std::env::var("HOME").context("HOME not set")?;
        PathBuf::from(home).join(".cache/tabd")
    };
    Ok(DaemonPaths {
        socket_path: base_dir.join("daemon.sock"),
        pid_path: base_dir.join("daemon.pid"),
        base_dir,
    })
}

// -- Wire format ------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct Request {
    #[serde(default)]
    id: String,
    action: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct Response<'a> {
    id: &'a str,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Stable machine-parseable code (see `daemon/error.rs`). Additive field —
    /// absent on success and on responses from older daemons.
    #[serde(rename = "errorCode", skip_serializing_if = "Option::is_none")]
    error_code: Option<&'static str>,
}

fn success_response(id: &str, data: Value) -> String {
    serde_json::to_string(&Response {
        id,
        success: true,
        data: Some(data),
        error: None,
        error_code: None,
    })
    .unwrap_or_else(|_| r#"{"id":"","success":false,"error":"serialization failed"}"#.into())
}

/// TS chromium-cdp returns `data: undefined` for executeJs of `void 0`, which
/// `JSON.stringify` strips out. Wire shape becomes `{"id":..,"success":true}`
/// with no data field. This helper produces that byte-exact response.
fn success_response_no_data(id: &str) -> String {
    serde_json::to_string(&Response {
        id,
        success: true,
        data: None,
        error: None,
        error_code: None,
    })
    .unwrap_or_else(|_| r#"{"id":"","success":false,"error":"serialization failed"}"#.into())
}

fn error_response(id: &str, message: &str) -> String {
    serde_json::to_string(&Response {
        id,
        success: false,
        data: None,
        error: Some(message.to_owned()),
        error_code: Some(error::classify_error_code(message).as_str()),
    })
    .unwrap_or_else(|_| r#"{"id":"","success":false,"error":"serialization failed"}"#.into())
}

// -- Daemon state -----------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct LastError {
    action: String,
    message: String,
    #[serde(rename = "at")]
    at_epoch_ms: u128,
}

#[derive(Clone)]
struct DaemonState {
    ready: Arc<AtomicBool>,
    ready_notify: Arc<Notify>,
    accepting: Arc<AtomicBool>,
    inflight: Arc<AtomicU32>,
    total_requests: Arc<AtomicU64>,
    drain_complete: Arc<AtomicBool>,
    drain_started: Arc<AtomicBool>,
    drain_notify: Arc<Notify>,
    last_error: Arc<Mutex<Option<LastError>>>,
    client: Arc<Mutex<Option<Arc<CdpClient>>>>,
    browser: Arc<Mutex<Option<Browser>>>,
    action_mutex: Arc<Mutex<()>>,
    /// Lazy-init secrets vault. None until the first secrets.* call.
    /// Phase 3f: passphrase mode only ($TABD_VAULT_KEY required).
    vault: Arc<Mutex<Option<crate::secrets::VaultStore>>>,
    /// Cumulative chromium restart count for this daemon process (3g).
    restart_attempts: Arc<AtomicU32>,
    started_at: Instant,
    pid: u32,
}

struct InflightGuard {
    inflight: Arc<AtomicU32>,
    notify: Arc<Notify>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.inflight.fetch_sub(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }
}

impl DaemonState {
    fn new() -> Self {
        DaemonState {
            ready: Arc::new(AtomicBool::new(false)),
            ready_notify: Arc::new(Notify::new()),
            accepting: Arc::new(AtomicBool::new(true)),
            inflight: Arc::new(AtomicU32::new(0)),
            total_requests: Arc::new(AtomicU64::new(0)),
            drain_complete: Arc::new(AtomicBool::new(false)),
            drain_started: Arc::new(AtomicBool::new(false)),
            drain_notify: Arc::new(Notify::new()),
            last_error: Arc::new(Mutex::new(None)),
            client: Arc::new(Mutex::new(None)),
            browser: Arc::new(Mutex::new(None)),
            action_mutex: Arc::new(Mutex::new(())),
            vault: Arc::new(Mutex::new(None)),
            restart_attempts: Arc::new(AtomicU32::new(0)),
            started_at: Instant::now(),
            pid: std::process::id(),
        }
    }

    fn try_admit(&self) -> Option<InflightGuard> {
        if !self.accepting.load(Ordering::Acquire) {
            return None;
        }
        self.inflight.fetch_add(1, Ordering::AcqRel);
        if !self.accepting.load(Ordering::Acquire) {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            self.drain_notify.notify_waiters();
            return None;
        }
        self.total_requests.fetch_add(1, Ordering::AcqRel);
        Some(InflightGuard {
            inflight: self.inflight.clone(),
            notify: self.drain_notify.clone(),
        })
    }

    async fn wait_ready(&self) {
        loop {
            let notified = self.ready_notify.notified();
            tokio::pin!(notified);
            if self.ready.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    async fn wait_drain_complete(&self) {
        loop {
            let notified = self.drain_notify.notified();
            tokio::pin!(notified);
            if self.drain_complete.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn ping(&self, id: &str) -> String {
        success_response(
            id,
            json!({ "pid": self.pid, "ready": self.ready.load(Ordering::Acquire) }),
        )
    }

    async fn health(&self, id: &str) -> String {
        let last_err = self.last_error.lock().await.clone();
        let restart_attempts = self.restart_attempts.load(Ordering::Acquire);
        let ready = self.ready.load(Ordering::Acquire);
        let driver = match self.browser.lock().await.as_ref().and_then(|b| b.pid()) {
            Some(pid) => json!({
                "chromiumPid": pid,
                "chromiumRssBytes": read_process_rss_bytes(pid),
                "restartAttempts": restart_attempts,
                "restartAttempt": restart_attempts, // legacy field for spike-daemon-compat
                "restarting": !ready && restart_attempts > 0,
            }),
            None => Value::Null,
        };
        let body = json!({
            "pid": self.pid,
            "uptimeMs": self.started_at.elapsed().as_millis() as u64,
            "ready": self.ready.load(Ordering::Acquire),
            "accepting": self.accepting.load(Ordering::Acquire),
            "inflight": self.inflight.load(Ordering::Acquire),
            "totalRequests": self.total_requests.load(Ordering::Acquire),
            "lastError": last_err,
            "driver": driver,
        });
        success_response(id, body)
    }

    async fn shutdown(&self, id: &str) -> String {
        let was_accepting = self.accepting.swap(false, Ordering::AcqRel);
        self.drain_notify.notify_waiters();
        if was_accepting && !self.drain_started.swap(true, Ordering::AcqRel) {
            let drain_state = self.clone();
            tokio::spawn(async move {
                drain_state.run_drain().await;
            });
        }
        success_response(id, json!({ "stopping": true }))
    }

    async fn run_drain(&self) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let timeout_ms = std::env::var("TABD_DRAIN_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(10_000);
        let _ = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
            loop {
                let notified = self.drain_notify.notified();
                tokio::pin!(notified);
                if self.inflight.load(Ordering::Acquire) == 0 {
                    return;
                }
                notified.await;
            }
        })
        .await;
        self.drain_complete.store(true, Ordering::Release);
        self.drain_notify.notify_waiters();
    }

    async fn record_failure(&self, action: &str, error_message: &str) {
        let mut last = self.last_error.lock().await;
        *last = Some(LastError {
            action: action.to_owned(),
            message: error_message.to_owned(),
            at_epoch_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        });
    }
}

// -- Shared handler helpers -------------------------------------------------
//
// The domain handlers themselves live in src/daemon/<domain>.rs (dom,
// interaction, tabs, storage, capture, monitor, waits, secrets); each pulls
// these helpers in via `use super::*`. Every handler returns
// Result<Option<Value>, String> and process_request packages it into the wire
// response.

/// Read VmRSS from /proc/<pid>/status on Linux. Returns 0 on any failure
/// (file missing, not Linux, parsing problems). The TS daemon also returns
/// 0 when the procfs read fails.
fn read_process_rss_bytes(pid: u32) -> u64 {
    #[cfg(target_os = "linux")]
    {
        let path = format!("/proc/{pid}/status");
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return 0;
        };
        for line in contents.lines() {
            // Lines look like: "VmRSS:	   12345 kB"
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb_str = rest
                    .trim()
                    .trim_end_matches(" kB")
                    .trim_end_matches("kB")
                    .trim();
                if let Ok(kb) = kb_str.parse::<u64>() {
                    return kb.saturating_mul(1024);
                }
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        0
    }
}

// Param extraction + the shared visible-wait poll, used by several handlers.

fn require_string(params: &Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing '{key}' (string)"))
        .map(|s| s.to_owned())
}

fn optional_u64(params: &Value, key: &str, default: u64) -> u64 {
    params.get(key).and_then(Value::as_u64).unwrap_or(default)
}

/// Upper bound on a user-supplied `timeout` for any polling wait. Driver actions
/// hold a global action lock for their whole duration, so an unclamped wait lets
/// one request pin the daemon against every other client. 5 minutes is generous
/// for legitimate waits while bounding the worst case.
const MAX_WAIT_MS: u64 = 300_000;

/// Default `--max-chars` for get-html / get-text / eval payloads. Generous for
/// any legitimate page read, but stops a single call from dumping tens of MB
/// into an AI agent's context window. `--max-chars 0` disables the clamp.
const DEFAULT_MAX_CHARS: u64 = 500_000;

/// Read the user-supplied `maxChars` clamp (0 = unlimited).
fn max_chars(params: &Value) -> u64 {
    optional_u64(params, "maxChars", DEFAULT_MAX_CHARS)
}

/// Truncate `s` to `max` chars with a visible marker so the agent knows the
/// payload is partial (a silent clamp would read as "that's the whole page").
fn clamp_chars(s: String, max: u64) -> String {
    if max == 0 {
        return s;
    }
    let max = max as usize;
    let total = s.chars().count();
    if total <= max {
        return s;
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…[truncated: {max} of {total} chars; pass --max-chars 0 for full output]")
}

/// Apply [`clamp_chars`] to a string payload, passing other shapes through.
fn clamp_value_chars(v: Value, max: u64) -> Value {
    match v {
        Value::String(s) => Value::String(clamp_chars(s, max)),
        other => other,
    }
}

/// Read a user-supplied `timeout` (ms) for a wait, clamped to [`MAX_WAIT_MS`].
fn clamped_wait_ms(params: &Value, default: u64) -> u64 {
    optional_u64(params, "timeout", default).min(MAX_WAIT_MS)
}

async fn client_or_err(state: &DaemonState) -> Result<Arc<CdpClient>, String> {
    state
        .client
        .lock()
        .await
        .as_ref()
        .cloned()
        .ok_or_else(|| "cdp client not initialized".to_string())
}

/// Poll until the selector matches a visible element, or fail with a clear
/// timeout error. Visibility = non-zero rect + style.visibility != hidden
/// + display != none.
async fn wait_for_selector_visible(
    client: &Arc<CdpClient>,
    selector: &str,
    timeout_ms: u64,
) -> Result<(), String> {
    let sel_lit = serde_json::to_string(selector).map_err(|e| e.to_string())?;
    let probe = format!(
        "(() => {{
    const el = document.querySelector({sel_lit});
    if (!el) return false;
    const rect = el.getBoundingClientRect();
    const style = getComputedStyle(el);
    return rect.width > 0 && rect.height > 0 && style.visibility !== 'hidden' && style.display !== 'none';
}})()"
    );
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Ok(Some(Value::Bool(true))) = crate::cmd::eval::evaluate_value(client, &probe).await
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "selector {selector} not visible after {timeout_ms}ms"
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Predicate that tests whether a URL string matches a compiled pattern.
type UrlMatcher = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// Compile a URL matcher from (pattern, patternType). Mirrors TS's
/// src/shared/urlMatch.ts behavior:
///   - exact: u == pattern
///   - glob: pattern with `*` becoming `.*`, anchored, other special chars escaped
///   - regex: pattern compiled directly
fn compile_url_matcher(pattern: &str, pattern_type: &str) -> Result<UrlMatcher, String> {
    match pattern_type {
        "exact" => {
            let p = pattern.to_owned();
            Ok(Box::new(move |u: &str| u == p))
        }
        "regex" => {
            let re =
                regex::Regex::new(pattern).map_err(|e| format!("invalid regex pattern: {e}"))?;
            Ok(Box::new(move |u: &str| re.is_match(u)))
        }
        "glob" => {
            // Escape regex metacharacters, then turn `*` into `.*`. Anchor with ^…$.
            let mut out = String::from("^");
            for c in pattern.chars() {
                match c {
                    '*' => out.push_str(".*"),
                    '.' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$'
                    | '\\' => {
                        out.push('\\');
                        out.push(c);
                    }
                    _ => out.push(c),
                }
            }
            out.push('$');
            let re = regex::Regex::new(&out)
                .map_err(|e| format!("invalid glob → regex compile: {e}"))?;
            Ok(Box::new(move |u: &str| re.is_match(u)))
        }
        other => Err(format!(
            "unsupported patternType '{other}' (expected exact|glob|regex)"
        )),
    }
}

// Tab resolution helpers, shared by the tabs/monitor/waits/secrets handlers.

/// Resolve a 1-based tabId to a chromium targetId. Mirrors TS
/// `resolveTargetId` in `src/server/runtimes/cdp.ts:1948` — when no tabId is
/// passed, prefer the active tab; otherwise fall back to the first listed.
/// Error strings are byte-exact with TS so `spike-daemon-compat` checks pass.
async fn resolve_target_id(client: &Arc<CdpClient>, tab_id: Option<u32>) -> Result<String, String> {
    let tabs = client.list_tabs().await.map_err(|e| e.to_string())?;
    match tab_id {
        Some(n) => {
            let idx = (n as usize)
                .checked_sub(1)
                .ok_or_else(|| format!("Tab not found: {n}"))?;
            tabs.get(idx)
                .map(|t| t.target_id.clone())
                .ok_or_else(|| format!("Tab not found: {n}"))
        }
        None => {
            if tabs.is_empty() {
                return Err("No browser tabs are open".to_string());
            }
            Ok(tabs
                .iter()
                .find(|t| t.active)
                .map(|t| t.target_id.clone())
                .unwrap_or_else(|| tabs[0].target_id.clone()))
        }
    }
}

fn require_tab_id(params: &Value) -> Result<u32, String> {
    params
        .get("tabId")
        .and_then(Value::as_u64)
        .ok_or_else(|| "tabId is required".to_string())
        .map(|n| n as u32)
}

// Key resolution helper, used by the interaction.pressKey handler.

/// CDP Input.dispatchKeyEvent fields for one logical keypress. Matches the
/// shape of TS `resolveKey()` output in `src/server/runtimes/cdp.ts:2031`.
/// `text` is Some for printable characters (drives input/change events),
/// None for special keys (Arrow, F-keys, Escape, etc.).
#[derive(Debug, Clone, PartialEq)]
struct KeyDef {
    key: String,
    code: String,
    text: Option<String>,
    key_code: u32,
}

/// Map a CLI key string to a KeyDef. Mirrors TS resolveKey: 20-ish special
/// keys + single-char fallback. Chord parsing ("Control+A") is intentionally
/// not implemented — TS doesn't either, and adding it here would diverge.
fn resolve_key(input: &str) -> KeyDef {
    let special: &[(&str, &str, &str, u32)] = &[
        ("Enter", "Enter", "Enter", 13),
        ("Tab", "Tab", "Tab", 9),
        ("Escape", "Escape", "Escape", 27),
        ("Backspace", "Backspace", "Backspace", 8),
        ("Delete", "Delete", "Delete", 46),
        ("ArrowLeft", "ArrowLeft", "ArrowLeft", 37),
        ("ArrowUp", "ArrowUp", "ArrowUp", 38),
        ("ArrowRight", "ArrowRight", "ArrowRight", 39),
        ("ArrowDown", "ArrowDown", "ArrowDown", 40),
        ("Home", "Home", "Home", 36),
        ("End", "End", "End", 35),
        ("PageUp", "PageUp", "PageUp", 33),
        ("PageDown", "PageDown", "PageDown", 34),
        ("Space", " ", "Space", 32),
        ("F1", "F1", "F1", 112),
        ("F2", "F2", "F2", 113),
        ("F3", "F3", "F3", 114),
        ("F4", "F4", "F4", 115),
        ("F5", "F5", "F5", 116),
        ("F6", "F6", "F6", 117),
        ("F7", "F7", "F7", 118),
        ("F8", "F8", "F8", 119),
        ("F9", "F9", "F9", 120),
        ("F10", "F10", "F10", 121),
        ("F11", "F11", "F11", 122),
        ("F12", "F12", "F12", 123),
    ];
    for (name, key, code, kc) in special {
        if input == *name {
            // Space's "key" field is " " (single space); no text dispatch
            // because the platform event already carries it via key.
            return KeyDef {
                key: (*key).to_string(),
                code: (*code).to_string(),
                text: None,
                key_code: *kc,
            };
        }
    }
    // Single-char fallback: emit text so the page sees an `input` event.
    let lower = input.to_lowercase();
    let upper = input.to_uppercase();
    let first_upper_char = upper.chars().next().unwrap_or('\0');
    KeyDef {
        key: lower.clone(),
        code: format!("Key{}", first_upper_char),
        text: Some(lower.clone()),
        key_code: first_upper_char as u32,
    }
}

// -- Connection / request loop ----------------------------------------------

async fn process_request(state: &DaemonState, line: &str) -> String {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(_) => return error_response("", "invalid JSON in request"),
    };

    // Control actions: no admit, no action lock.
    match req.action.as_str() {
        "daemon.ping" => return state.ping(&req.id),
        "daemon.health" => return state.health(&req.id).await,
        "daemon.shutdown" => return state.shutdown(&req.id).await,
        _ => {}
    }

    // Driver actions: admit gate → wait_ready → action_mutex → handler.
    let _guard = match state.try_admit() {
        Some(g) => g,
        None => {
            return error_response(&req.id, "daemon is shutting down (drain in progress)");
        }
    };
    state.wait_ready().await;
    // Held for the entire handler, including polling sleeps inside waits. This is
    // a deliberate choice for linear, predictable action semantics (no action
    // interleaving into a tab another is mid-wait on); the long-wait worst case
    // is bounded by MAX_WAIT_MS + the per-RPC timeout, and the daemon is
    // single-user behind a 0700 dir / 0600 socket. See docs/architecture.md
    // "Concurrency model" for the rejected per-tab-lock alternative.
    let _action_lock = state.action_mutex.lock().await;

    let result = match req.action.as_str() {
        "tabs.navigate" => dom::handle_navigate(state, &req.params).await,
        "execution.executeJs" => dom::handle_eval(state, &req.params).await,
        "dom.getText" => dom::handle_get_text(state, &req.params).await,
        "dom.getHtml" => dom::handle_get_html(state, &req.params).await,
        "dom.querySelector" => dom::handle_query_selector(state, &req.params).await,
        "cookies.get" => storage::handle_cookies_get(state, &req.params).await,
        "cookies.set" => storage::handle_cookies_set(state, &req.params).await,
        "cookies.delete" => storage::handle_cookies_delete(state, &req.params).await,
        "storage.get" => storage::handle_storage_get(state, &req.params).await,
        "storage.set" => storage::handle_storage_set(state, &req.params).await,
        "storage.clear" => storage::handle_storage_clear(state, &req.params).await,
        "capture.screenshot" => capture::handle_screenshot(state, &req.params).await,
        "interaction.click" => interaction::handle_click(state, &req.params).await,
        "interaction.type" => interaction::handle_type(state, &req.params).await,
        "wait.selector" => waits::handle_wait_selector(state, &req.params).await,
        "wait.url" => waits::handle_wait_url(state, &req.params).await,
        "wait.text" => waits::handle_wait_text(state, &req.params).await,
        "tabs.list" => tabs::handle_tabs_list(state, &req.params).await,
        "tabs.open" => tabs::handle_tabs_open(state, &req.params).await,
        "tabs.close" => tabs::handle_tabs_close(state, &req.params).await,
        "tabs.activate" => tabs::handle_tabs_activate(state, &req.params).await,
        "tabs.goBack" => {
            tabs::handle_tabs_history(state, &req.params, "history.back(); null;").await
        }
        "tabs.goForward" => {
            tabs::handle_tabs_history(state, &req.params, "history.forward(); null;").await
        }
        "tabs.reload" => tabs::handle_tabs_reload(state, &req.params).await,
        "interaction.hover" => interaction::handle_hover(state, &req.params).await,
        "interaction.mouseMove" => interaction::handle_mouse_move(state, &req.params).await,
        "interaction.scroll" => interaction::handle_scroll(state, &req.params).await,
        "interaction.pressKey" => interaction::handle_press_key(state, &req.params).await,
        "interaction.selectOption" => interaction::handle_select_option(state, &req.params).await,
        "interaction.check" => interaction::handle_check(state, &req.params).await,
        "monitor.consoleLogs" => monitor::handle_console_logs(state, &req.params).await,
        "monitor.pageErrors" => monitor::handle_page_errors(state, &req.params).await,
        "capture.metrics" => capture::handle_metrics(state, &req.params).await,
        "dom.contentSummary" => dom::handle_content_summary(state, &req.params).await,
        "monitor.networkLogs" => monitor::handle_network_logs(state, &req.params).await,
        "monitor.dialogs" => monitor::handle_dialogs(state, &req.params).await,
        "browser.setDialogPolicy" => monitor::handle_set_dialog_policy(state, &req.params).await,
        "wait.networkIdle" => waits::handle_wait_network_idle(state, &req.params).await,
        "secrets.put" => secrets::handle_secrets_put(state, &req.params).await,
        "secrets.list" => secrets::handle_secrets_list(state, &req.params).await,
        "secrets.delete" => secrets::handle_secrets_delete(state, &req.params).await,
        "interaction.typeSecret" => secrets::handle_type_secret(state, &req.params).await,
        other => Err(format!("unsupported action: {other}")),
    };

    match result {
        Ok(Some(data)) => success_response(&req.id, data),
        Ok(None) => success_response_no_data(&req.id),
        Err(msg) => {
            state.record_failure(&req.action, &msg).await;
            error_response(&req.id, &msg)
        }
    }
}

async fn handle_connection(stream: UnixStream, state: DaemonState) {
    let (reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.is_empty() {
            continue;
        }
        let state = state.clone();
        let writer = writer.clone();
        tokio::spawn(async move {
            let response = process_request(&state, &line).await;
            let mut w = writer.lock().await;
            let _ = w.write_all(response.as_bytes()).await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;
        });
    }
}

// -- Boot / lock ------------------------------------------------------------

/// chmod `path` to `mode`. Unix-only (the daemon is Unix-socket based).
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("set mode {mode:o} on {}", path.display()))
}

async fn bind_listener_with_lock(paths: &DaemonPaths) -> Result<UnixListener> {
    std::fs::create_dir_all(&paths.base_dir).context("create daemon base dir")?;
    // 0o700 so other local users can't traverse into the runtime dir and reach
    // the daemon socket (which would let them drive the browser, read cookies,
    // or trigger type-secret). Critical for the ~/.cache/tabd fallback used over
    // SSH when XDG_RUNTIME_DIR is unset. set_permissions also fixes up an
    // already-existing dir created before this hardening.
    set_mode(&paths.base_dir, 0o700)?;
    for attempt in 0..2 {
        match UnixListener::bind(&paths.socket_path) {
            Ok(listener) => {
                // Defense-in-depth on top of the 0o700 dir: the socket itself is
                // owner-only. (bind() honors umask, so make the intent explicit.)
                set_mode(&paths.socket_path, 0o600)?;
                return Ok(listener);
            }
            Err(err) => {
                if err.kind() != std::io::ErrorKind::AddrInUse || attempt == 1 {
                    return Err(err)
                        .with_context(|| format!("bind {}", paths.socket_path.display()));
                }
                if pid_alive(&paths.pid_path) {
                    bail!(
                        "another daemon is running (pid file {}); refusing to start",
                        paths.pid_path.display()
                    );
                }
                if probe_socket_alive(&paths.socket_path).await {
                    bail!(
                        "another process is listening on {}; refusing to start",
                        paths.socket_path.display()
                    );
                }
                let _ = std::fs::remove_file(&paths.socket_path);
                let _ = std::fs::remove_file(&paths.pid_path);
            }
        }
    }
    unreachable!("bind loop exits via return/bail");
}

fn pid_alive(pid_path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(pid_path) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<i32>() else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // kill(pid, 0) returns 0 if the signal could be delivered (process exists
    // and signal allowed). On -1, ESRCH means the process is gone, EPERM means
    // it exists but we can't signal it — still ALIVE. Treating EPERM as dead
    // (codex round 2 C2) would let stale-socket recovery unlink another
    // user's live socket/pid files.
    #[cfg(unix)]
    {
        let r = unsafe { libc::kill(pid, 0) };
        if r == 0 {
            return true;
        }
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        errno == libc::EPERM
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

async fn probe_socket_alive(socket_path: &Path) -> bool {
    match tokio::time::timeout(
        Duration::from_millis(1500),
        UnixStream::connect(socket_path),
    )
    .await
    {
        Ok(Ok(_)) => true,
        Ok(Err(_)) => false,
        Err(_) => true, // conservative — slow probe means probably alive
    }
}

fn write_pid_file(paths: &DaemonPaths) -> Result<()> {
    std::fs::write(&paths.pid_path, std::process::id().to_string())
        .with_context(|| format!("write pid file {}", paths.pid_path.display()))
}

// -- Entry ------------------------------------------------------------------

pub async fn run(override_base: Option<&str>) -> Result<()> {
    let paths = resolve_paths(override_base)?;
    let listener = bind_listener_with_lock(&paths).await?;

    let state = DaemonState::new();
    let boot_state = state.clone();
    let boot_paths = paths.clone();
    tokio::spawn(async move {
        match boot_browser_and_cdp(&boot_state, &boot_paths).await {
            Ok(()) => {
                boot_state.ready.store(true, Ordering::Release);
                boot_state.ready_notify.notify_waiters();
            }
            Err(err) => {
                eprintln!("[tabd daemon] boot failed: {err:#}");
                let _ = std::fs::remove_file(&boot_paths.socket_path);
                let _ = std::fs::remove_file(&boot_paths.pid_path);
                std::process::exit(1);
            }
        }
    });

    // Phase 3g: chromium liveness supervisor.
    tokio::spawn(supervise(state.clone(), paths.clone()));

    // SIGTERM/SIGINT → graceful shutdown
    let sig_state = state.clone();
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
        }
        // Simulate daemon.shutdown locally.
        let _ = sig_state.shutdown("").await;
    });

    eprintln!(
        "[tabd daemon] listening on {} (pid={})",
        paths.socket_path.display(),
        std::process::id()
    );

    loop {
        tokio::select! {
            r = listener.accept() => {
                match r {
                    Ok((stream, _addr)) => {
                        let st = state.clone();
                        tokio::spawn(handle_connection(stream, st));
                    }
                    Err(err) => {
                        eprintln!("[tabd daemon] accept error: {err}");
                    }
                }
            }
            _ = state.wait_drain_complete() => break,
        }
    }

    // Cleanup
    if let Some(client) = state.client.lock().await.take() {
        let _ = client.close().await;
    }
    if let Some(browser) = state.browser.lock().await.take() {
        let _ = browser.shutdown().await;
    }
    let _ = std::fs::remove_file(&paths.socket_path);
    let _ = std::fs::remove_file(&paths.pid_path);
    Ok(())
}

async fn boot_browser_and_cdp(state: &DaemonState, paths: &DaemonPaths) -> Result<()> {
    let browser = Browser::launch().await?;
    let client = CdpClient::connect(browser.ws_endpoint()).await?;
    write_pid_file(paths)?;
    *state.client.lock().await = Some(Arc::new(client));
    *state.browser.lock().await = Some(browser);
    Ok(())
}

/// Phase 3g supervisor: polls chromium liveness every 2s and rebuilds the
/// browser + cdp client if it died. Liveness is `Browser::is_alive()` —
/// `try_wait()` on the owned child — which works on every platform (the
/// earlier `/proc/{pid}/status` State parse was Linux-only and silently
/// disabled supervision on macOS), reaps the zombie the State parse existed
/// to detect, and can't be fooled by PID reuse. The browser mutex is held
/// only for the non-blocking check, same as the previous pid read.
async fn supervise(state: DaemonState, paths: DaemonPaths) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if state.drain_started.load(Ordering::Acquire) {
            return;
        }
        if !state.ready.load(Ordering::Acquire) {
            continue;
        }
        let checked = {
            let mut guard = state.browser.lock().await;
            guard.as_mut().map(|b| (b.pid(), b.is_alive()))
        };
        let Some((pid, alive)) = checked else {
            continue;
        };
        if alive {
            continue;
        }
        let pid = pid.unwrap_or(0);
        // Crash detected — flip ready off and rebuild.
        eprintln!("[tabd daemon] chromium pid={pid} disappeared; restarting");
        state.ready.store(false, Ordering::Release);
        state.restart_attempts.fetch_add(1, Ordering::AcqRel);
        // Drop the dead client/browser before booting a new one so resources
        // are freed and the writer mpsc closes.
        if let Some(client) = state.client.lock().await.take() {
            let _ = client.close().await;
        }
        let _ = state.browser.lock().await.take();
        let mut delay_ms: u64 = 200;
        for attempt in 0..5u32 {
            match boot_browser_and_cdp(&state, &paths).await {
                Ok(()) => {
                    state.ready.store(true, Ordering::Release);
                    state.ready_notify.notify_waiters();
                    eprintln!(
                        "[tabd daemon] chromium recovered after {} attempt(s)",
                        attempt + 1
                    );
                    break;
                }
                Err(err) => {
                    state
                        .record_failure("supervisor.restart", &err.to_string())
                        .await;
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = (delay_ms * 2).min(2000);
                }
            }
        }
    }
}

// -- Daemon control client (Rust CLI side) ----------------------------------

pub async fn send_control_action(socket_path: &Path, action: &str) -> Result<Value> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connect {}", socket_path.display()))?;
    let (reader, mut writer) = stream.into_split();
    let req = json!({ "id": "cli", "action": action }).to_string() + "\n";
    writer.write_all(req.as_bytes()).await?;
    writer.flush().await?;
    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("daemon closed without response"))?;
    let v: Value = serde_json::from_str(&line).context("daemon response not JSON")?;
    Ok(v)
}

// -- Unit tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_basic() {
        let req: Request = serde_json::from_str(r#"{"id":"x","action":"daemon.ping"}"#).unwrap();
        assert_eq!(req.id, "x");
        assert_eq!(req.action, "daemon.ping");
        assert!(req.params.is_null());
    }

    #[test]
    fn parse_request_with_params() {
        let req: Request = serde_json::from_str(
            r#"{"id":"y","action":"tabs.navigate","params":{"url":"data:,"}}"#,
        )
        .unwrap();
        assert_eq!(req.action, "tabs.navigate");
        assert_eq!(req.params["url"], json!("data:,"));
    }

    #[test]
    fn parse_request_missing_id_defaults_to_empty() {
        let req: Request = serde_json::from_str(r#"{"action":"daemon.ping"}"#).unwrap();
        assert_eq!(req.id, "");
    }

    #[test]
    fn parse_request_invalid_json() {
        let r = serde_json::from_str::<Request>("not json");
        assert!(r.is_err());
    }

    #[test]
    fn success_response_shape() {
        let s = success_response("a", json!({"k": 1}));
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["id"], json!("a"));
        assert_eq!(v["success"], json!(true));
        assert_eq!(v["data"]["k"], json!(1));
        assert!(v.get("error").is_none());
    }

    #[test]
    fn error_response_shape() {
        let s = error_response("b", "boom");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["id"], json!("b"));
        assert_eq!(v["success"], json!(false));
        assert_eq!(v["error"], json!("boom"));
        assert_eq!(v["errorCode"], json!("internal"));
        assert!(v.get("data").is_none());
    }

    #[test]
    fn error_response_classifies_known_messages() {
        let s = error_response("b", "Tab not found: 9");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["errorCode"], json!("tab_not_found"));

        let s = error_response("b", "selector .x not visible after 30000ms");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["errorCode"], json!("selector_not_found"));
    }

    #[test]
    fn clamp_chars_boundaries() {
        // Under and exactly-at the limit pass through untouched.
        assert_eq!(clamp_chars("abc".into(), 4), "abc");
        assert_eq!(clamp_chars("abcd".into(), 4), "abcd");
        // Over the limit truncates with a visible marker.
        let out = clamp_chars("abcde".into(), 4);
        assert!(
            out.starts_with("abcd…[truncated: 4 of 5 chars"),
            "got: {out}"
        );
        // 0 disables the clamp entirely.
        assert_eq!(clamp_chars("abcde".into(), 0), "abcde");
    }

    #[test]
    fn clamp_value_chars_only_touches_strings() {
        assert_eq!(clamp_value_chars(json!({"a": 1}), 1), json!({"a": 1}));
        let clamped = clamp_value_chars(json!("xy"), 1);
        let s = clamped.as_str().unwrap();
        assert!(s.starts_with("x…[truncated: 1 of 2 chars"), "got: {s}");
    }

    #[test]
    fn max_chars_default_and_override() {
        assert_eq!(max_chars(&json!({})), DEFAULT_MAX_CHARS);
        assert_eq!(max_chars(&json!({"maxChars": 100})), 100);
        assert_eq!(max_chars(&json!({"maxChars": 0})), 0);
    }

    #[test]
    fn success_response_has_no_error_code() {
        let s = success_response("a", json!(null));
        let v: Value = serde_json::from_str(&s).unwrap();
        assert!(v.get("errorCode").is_none());
        let s = success_response_no_data("a");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert!(v.get("errorCode").is_none());
    }

    #[test]
    fn resolve_paths_override_wins() {
        let p = resolve_paths(Some("/tmp/test-base")).unwrap();
        assert_eq!(p.base_dir, PathBuf::from("/tmp/test-base"));
        assert_eq!(p.socket_path, PathBuf::from("/tmp/test-base/daemon.sock"));
        assert_eq!(p.pid_path, PathBuf::from("/tmp/test-base/daemon.pid"));
    }

    #[test]
    fn try_admit_basic_flow() {
        let state = DaemonState::new();
        // accepting=true initially
        let g1 = state.try_admit();
        assert!(g1.is_some());
        assert_eq!(state.inflight.load(Ordering::Acquire), 1);
        assert_eq!(state.total_requests.load(Ordering::Acquire), 1);
        drop(g1);
        assert_eq!(state.inflight.load(Ordering::Acquire), 0);
    }

    #[test]
    fn try_admit_rejects_after_accepting_false() {
        let state = DaemonState::new();
        state.accepting.store(false, Ordering::Release);
        assert!(state.try_admit().is_none());
        assert_eq!(state.inflight.load(Ordering::Acquire), 0);
    }

    // -- Phase 2b: compile_url_matcher --

    #[test]
    fn url_matcher_exact() {
        let m = compile_url_matcher("https://example.com/x", "exact").unwrap();
        assert!(m("https://example.com/x"));
        assert!(!m("https://example.com/x/"));
        assert!(!m("https://example.com/y"));
    }

    #[test]
    fn url_matcher_glob_wildcard() {
        let m = compile_url_matcher("https://*.example.com/*", "glob").unwrap();
        assert!(m("https://api.example.com/foo"));
        assert!(m("https://app.example.com/"));
        assert!(!m("https://example.org/foo"));
        assert!(!m("http://api.example.com/foo")); // scheme differs
    }

    #[test]
    fn url_matcher_glob_anchored() {
        // Implicit anchor — substring without anchors must not match.
        let m = compile_url_matcher("https://example.com/x", "glob").unwrap();
        assert!(m("https://example.com/x"));
        assert!(!m("https://example.com/x/y"));
        assert!(!m("prefix-https://example.com/x"));
    }

    #[test]
    fn url_matcher_glob_escapes_regex_metas() {
        // dots / + / ? must be treated as literals when in a glob pattern.
        let m = compile_url_matcher("https://example.com/foo.bar?baz=1", "glob").unwrap();
        assert!(m("https://example.com/foo.bar?baz=1"));
        // dot is literal — `foo!bar` must NOT match
        assert!(!m("https://example.com/foo!bar?baz=1"));
    }

    #[test]
    fn url_matcher_regex() {
        let m = compile_url_matcher(r"^https://example\.com/\d+$", "regex").unwrap();
        assert!(m("https://example.com/42"));
        assert!(!m("https://example.com/x"));
    }

    #[test]
    fn url_matcher_unsupported_pattern_type() {
        // Closure type isn't Debug, so use `.err()` instead of `.unwrap_err()`.
        let err = compile_url_matcher("x", "weird").err().unwrap();
        assert!(err.contains("unsupported patternType"), "got: {err}");
    }

    #[test]
    fn url_matcher_invalid_regex() {
        let err = compile_url_matcher("[bad", "regex").err().unwrap();
        assert!(err.contains("invalid regex pattern"), "got: {err}");
    }

    // -- Phase 2c: driver health --

    #[test]
    fn rss_returns_nonzero_for_self_pid_on_linux() {
        // The current process should always have a measurable RSS on Linux.
        // On other OSes the helper returns 0 by design.
        let rss = read_process_rss_bytes(std::process::id());
        #[cfg(target_os = "linux")]
        assert!(rss > 0, "expected non-zero RSS on Linux; got {rss}");
        #[cfg(not(target_os = "linux"))]
        assert_eq!(rss, 0);
    }

    #[test]
    fn rss_returns_zero_for_bogus_pid() {
        // Very unlikely a process with this PID exists; even if it does it
        // would belong to another user and procfs read fails → 0.
        let rss = read_process_rss_bytes(99_999_999);
        assert_eq!(rss, 0);
    }

    // -- Phase 3d: resolve_key --

    #[test]
    fn resolve_key_enter_is_special() {
        let kd = resolve_key("Enter");
        assert_eq!(kd.key, "Enter");
        assert_eq!(kd.code, "Enter");
        assert_eq!(kd.key_code, 13);
        assert!(kd.text.is_none(), "Enter is special; no text dispatch");
    }

    #[test]
    fn resolve_key_arrow_left() {
        let kd = resolve_key("ArrowLeft");
        assert_eq!(kd.key, "ArrowLeft");
        assert_eq!(kd.code, "ArrowLeft");
        assert_eq!(kd.key_code, 37);
        assert!(kd.text.is_none());
    }

    #[test]
    fn resolve_key_printable_letter() {
        let kd = resolve_key("a");
        assert_eq!(kd.key, "a");
        assert_eq!(kd.code, "KeyA");
        assert_eq!(kd.text.as_deref(), Some("a"));
        assert_eq!(kd.key_code, 'A' as u32);
    }

    #[test]
    fn resolve_key_chord_treated_as_literal() {
        // Mirrors TS behavior — chord support is intentionally NOT implemented.
        let kd = resolve_key("Control+A");
        assert_eq!(kd.key, "control+a");
        assert_eq!(kd.code, "KeyC");
        assert_eq!(kd.text.as_deref(), Some("control+a"));
    }

    // -- Phase 3g: supervisor --
    // Liveness tests live in browser.rs (Browser::is_alive on the owned Child).

    #[test]
    fn resolve_key_f5() {
        let kd = resolve_key("F5");
        assert_eq!(kd.key, "F5");
        assert_eq!(kd.code, "F5");
        assert_eq!(kd.key_code, 116);
        assert!(kd.text.is_none());
    }
}
