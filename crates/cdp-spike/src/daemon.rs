//! TS-protocol-compatible daemon: newline-delimited JSON-RPC over a Unix
//! domain socket. Spawns one Chromium for the daemon's lifetime, serializes
//! all driver actions through a single mutex, supports `daemon.ping`,
//! `daemon.health`, and `daemon.shutdown` plus three driver actions
//! (`tabs.navigate`, `execution.executeJs`, `dom.getText`).
//!
//! Phase 2 scope: TS CLI compat for `navigate` / `eval` / `get-text` only.
//! MCP and other driver actions are out of scope.

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
    } else if let Ok(d) = std::env::var("AI_BROWSER_BASE_DIR") {
        PathBuf::from(d)
    } else if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(d).join("ai-browser-rs")
    } else {
        let home = std::env::var("HOME").context("HOME not set")?;
        PathBuf::from(home).join(".cache/ai-browser-rs")
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
}

fn success_response(id: &str, data: Value) -> String {
    serde_json::to_string(&Response {
        id,
        success: true,
        data: Some(data),
        error: None,
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
    })
    .unwrap_or_else(|_| r#"{"id":"","success":false,"error":"serialization failed"}"#.into())
}

fn error_response(id: &str, message: &str) -> String {
    serde_json::to_string(&Response {
        id,
        success: false,
        data: None,
        error: Some(message.to_owned()),
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
        // Driver-level health (codex round 1 C4) — phase 2c populates chromium
        // pid + RSS when the browser is up. restartAttempt is always 0 because
        // spike does not implement crash-restart supervisor (phase 2c+).
        let driver = match self.browser.lock().await.as_ref().and_then(|b| b.pid()) {
            Some(pid) => json!({
                "chromiumPid": pid,
                "chromiumRssBytes": read_process_rss_bytes(pid),
                "restartAttempt": 0,
                "restarting": false,
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
        let timeout_ms = std::env::var("AI_BROWSER_DRAIN_TIMEOUT_MS")
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

// -- Action handlers --------------------------------------------------------
//
// Each handler returns Result<Value, String> where Ok is the data field and
// Err is the error message string. process_request packages them into the
// wire response.

async fn handle_navigate(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "tabs.navigate: missing 'url' (string)".to_string())?
        .to_owned();
    let client = state
        .client
        .lock()
        .await
        .as_ref()
        .cloned()
        .ok_or_else(|| "cdp client not initialized".to_string())?;
    page::navigate_existing(&client, &url, 30_000)
        .await
        .map(|()| Some(json!({ "url": url })))
        .map_err(|e| e.to_string())
}

async fn handle_eval(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let code = params
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| "execution.executeJs: missing 'code' (string)".to_string())?;
    let client = state
        .client
        .lock()
        .await
        .as_ref()
        .cloned()
        .ok_or_else(|| "cdp client not initialized".to_string())?;
    // None (CDP `undefined`) propagates as None → wire response omits `data`,
    // matching TS chromium-cdp byte-exact (codex round 1 C1).
    crate::cmd::eval::evaluate_value(&client, code)
        .await
        .map_err(|e| e.to_string())
}

async fn handle_get_text(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let selector = params
        .get("selector")
        .and_then(Value::as_str)
        .unwrap_or("main, article, body")
        .to_owned();
    let raw = params.get("raw").and_then(Value::as_bool).unwrap_or(false);
    let client = state
        .client
        .lock()
        .await
        .as_ref()
        .cloned()
        .ok_or_else(|| "cdp client not initialized".to_string())?;

    let body = crate::cmd::get_text::build_text_body(raw);
    let sel_lit =
        serde_json::to_string(&selector).map_err(|e| format!("selector encode: {e}"))?;
    let expr = format!(
        "(() => {{ const target = document.querySelector({sel_lit}) ?? document.body; {body} }})()"
    );

    // dom.getText always returns a string (TS wraps with String(...)). Map
    // None → "" so the wire shape stays consistent.
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|opt| Some(opt.unwrap_or(Value::String(String::new()))))
        .map_err(|e| e.to_string())
}

// -- Phase 2c: driver health helpers ---------------------------------------

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

// -- Phase 2b: interaction + wait helpers ----------------------------------

fn require_string<'a>(params: &'a Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing '{key}' (string)"))
        .map(|s| s.to_owned())
}

fn optional_u64(params: &Value, key: &str, default: u64) -> u64 {
    params
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or(default)
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
        match crate::cmd::eval::evaluate_value(client, &probe).await {
            Ok(Some(Value::Bool(true))) => return Ok(()),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "selector {selector} not visible after {timeout_ms}ms"
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Compile a URL matcher from (pattern, patternType). Mirrors TS's
/// src/shared/urlMatch.ts behavior:
///   - exact: u == pattern
///   - glob: pattern with `*` becoming `.*`, anchored, other special chars escaped
///   - regex: pattern compiled directly
fn compile_url_matcher(
    pattern: &str,
    pattern_type: &str,
) -> Result<Box<dyn Fn(&str) -> bool + Send + Sync>, String> {
    match pattern_type {
        "exact" => {
            let p = pattern.to_owned();
            Ok(Box::new(move |u: &str| u == p))
        }
        "regex" => {
            let re = regex::Regex::new(pattern)
                .map_err(|e| format!("invalid regex pattern: {e}"))?;
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

async fn handle_click(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let timeout_ms = optional_u64(params, "timeout", 30_000);
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, timeout_ms).await?;
    let sel_lit = serde_json::to_string(&selector).unwrap();
    let expr = format!(
        "(() => {{
    const el = document.querySelector({sel_lit});
    if (!el) throw new Error('Selector not found: ' + {sel_lit});
    el.click();
    return {{ ok: true }};
}})()"
    );
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map_err(|e| e.to_string())
}

async fn handle_type(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let text = require_string(params, "text")?;
    let timeout_ms = optional_u64(params, "timeout", 30_000);
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, timeout_ms).await?;
    let sel_lit = serde_json::to_string(&selector).unwrap();
    let text_lit = serde_json::to_string(&text).unwrap();
    // JS-based type (spike scope) — sets .value + fires input/change events.
    // Plain HTML forms work; some React/Vue controlled inputs may need the
    // native setter trick, which is phase 2c (real CDP Input.dispatchKeyEvent).
    let expr = format!(
        "(() => {{
    const el = document.querySelector({sel_lit});
    if (!el) throw new Error('Selector not found: ' + {sel_lit});
    el.focus();
    el.value = {text_lit};
    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
    return {{ ok: true }};
}})()"
    );
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map_err(|e| e.to_string())
}

async fn handle_wait_selector(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let timeout_ms = optional_u64(params, "timeout", 30_000);
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, timeout_ms).await?;
    Ok(Some(json!({ "found": true })))
}

/// `dom.getHtml` — TS chromium-cdp parity (src/server/runtimes/cdp.ts:823).
/// Params: selector (default "body"), outer (default true), clean (default true).
/// `clean=true` strips script/style/svg, comments, and data-* attrs from a
/// deep clone before serializing.
async fn handle_get_html(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let selector = params
        .get("selector")
        .and_then(Value::as_str)
        .unwrap_or("body")
        .to_owned();
    let outer = params
        .get("outer")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let clean = params
        .get("clean")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let client = client_or_err(state).await?;
    let sel_lit = serde_json::to_string(&selector).unwrap();
    let outer_lit = serde_json::to_string(&outer).unwrap();
    let clean_lit = serde_json::to_string(&clean).unwrap();

    let expr = format!(
        r#"(() => {{
    const node = document.querySelector({sel_lit});
    if (!node) throw new Error('Selector not found: ' + {sel_lit});
    const clone = node.cloneNode(true);
    if ({clean_lit}) {{
        clone.querySelectorAll("script,style,svg").forEach((el) => el.remove());
        const walker = document.createTreeWalker(clone, NodeFilter.SHOW_COMMENT);
        const comments = [];
        while (walker.nextNode()) comments.push(walker.currentNode);
        comments.forEach((node) => node.remove());
        clone.querySelectorAll("*").forEach((el) => {{
            [...el.attributes]
                .filter((attr) => attr.name.startsWith("data-"))
                .forEach((attr) => el.removeAttribute(attr.name));
        }});
    }}
    return {outer_lit} ? clone.outerHTML : clone.innerHTML;
}})()"#
    );

    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|opt| Some(opt.unwrap_or(Value::String(String::new()))))
        .map_err(|e| e.to_string())
}

/// `dom.querySelector` — TS chromium-cdp parity (src/server/runtimes/cdp.ts:874).
/// Params: selector (string, required-ish — "" returns []), limit (default 20),
/// visibleOnly (default false).
async fn handle_query_selector(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = params
        .get("selector")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(20);
    let visible_only = params
        .get("visibleOnly")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let client = client_or_err(state).await?;
    let sel_lit = serde_json::to_string(&selector).unwrap();
    let visible_lit = serde_json::to_string(&visible_only).unwrap();

    let expr = format!(
        r#"(() => {{
    return [...document.querySelectorAll({sel_lit})]
        .filter((el) => {{
            if (!{visible_lit}) return true;
            const rect = el.getBoundingClientRect();
            const style = getComputedStyle(el);
            return rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none";
        }})
        .slice(0, {limit})
        .map((el, index) => {{
            const rect = el.getBoundingClientRect();
            return {{
                index,
                tag: el.tagName.toLowerCase(),
                id: el.id || null,
                classes: [...el.classList],
                text: (el.innerText || el.textContent || "").trim().slice(0, 200),
                attributes: Object.fromEntries([...el.attributes].map((attr) => [attr.name, attr.value])),
                rect: {{ x: rect.x, y: rect.y, width: rect.width, height: rect.height }}
            }};
        }});
}})()"#
    );

    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|opt| Some(opt.unwrap_or(Value::Array(vec![]))))
        .map_err(|e| e.to_string())
}

/// `cookies.get` — CDP `Network.getCookies {urls:[url]}` → cookies array.
/// Network.* CDP calls are wrapped in a 5s timeout because some chromium
/// builds hang the request without ever responding (observed on the
/// Playwright-bundled chromium-1217 used in spike testing). Timeout surfaces
/// as a clear error rather than a stuck action lock + drain.
async fn handle_cookies_get(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let url = require_string(params, "url")?;
    let client = client_or_err(state).await?;
    let send_fut = client.send("Network.getCookies", json!({ "urls": [url] }));
    let resp = match tokio::time::timeout(Duration::from_secs(5), send_fut).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("Network.getCookies timed out after 5s".to_string()),
    };
    Ok(Some(
        resp.get("cookies").cloned().unwrap_or(Value::Array(vec![])),
    ))
}

/// `cookies.set` — CDP `Network.setCookie`. Returns null on success (no data
/// field on wire). Throws if CDP rejects the cookie.
async fn handle_cookies_set(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let url = require_string(params, "url")?;
    let name = require_string(params, "name")?;
    let value = require_string(params, "value")?;
    let mut p = json!({ "url": url, "name": name, "value": value });
    for k in &["domain", "path"] {
        if let Some(v) = params.get(*k).and_then(Value::as_str) {
            p[*k] = Value::String(v.to_owned());
        }
    }
    for k in &["secure", "httpOnly"] {
        if let Some(v) = params.get(*k).and_then(Value::as_bool) {
            p[*k] = Value::Bool(v);
        }
    }
    if let Some(v) = params.get("expirationDate").and_then(Value::as_f64) {
        p["expires"] = json!(v);
    }
    let client = client_or_err(state).await?;
    let send_fut = client.send("Network.setCookie", p);
    let resp = match tokio::time::timeout(Duration::from_secs(5), send_fut).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("Network.setCookie timed out after 5s".to_string()),
    };
    let success = resp.get("success").and_then(Value::as_bool).unwrap_or(false);
    if !success {
        return Err(format!("CDP rejected the cookie: {resp}"));
    }
    Ok(None)
}

/// `cookies.delete` — CDP `Network.deleteCookies`. Returns null.
async fn handle_cookies_delete(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let url = require_string(params, "url")?;
    let name = require_string(params, "name")?;
    let client = client_or_err(state).await?;
    let send_fut = client.send("Network.deleteCookies", json!({ "url": url, "name": name }));
    match tokio::time::timeout(Duration::from_secs(5), send_fut).await {
        Ok(Ok(_)) => Ok(None),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("Network.deleteCookies timed out after 5s".to_string()),
    }
}

/// `storage.get` — evaluate-based wrapper over local/sessionStorage.
/// If `key` is provided, returns the single value. Otherwise dumps the whole
/// storage as `{ key: value }` object.
async fn handle_storage_get(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let storage_type = if params.get("type").and_then(Value::as_str) == Some("session") {
        "sessionStorage"
    } else {
        "localStorage"
    };
    let client = client_or_err(state).await?;
    let expr = match params.get("key").and_then(Value::as_str) {
        Some(key) => {
            let key_lit = serde_json::to_string(key).unwrap();
            format!("{storage_type}.getItem({key_lit})")
        }
        None => format!(
            "Object.fromEntries(Object.keys({storage_type}).map((k) => [k, {storage_type}.getItem(k) ?? \"\"]))"
        ),
    };
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|opt| Some(opt.unwrap_or(Value::Null)))
        .map_err(|e| e.to_string())
}

async fn handle_storage_set(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let key = require_string(params, "key")?;
    let value = require_string(params, "value")?;
    let storage_type = if params.get("type").and_then(Value::as_str) == Some("session") {
        "sessionStorage"
    } else {
        "localStorage"
    };
    let client = client_or_err(state).await?;
    let key_lit = serde_json::to_string(&key).unwrap();
    let value_lit = serde_json::to_string(&value).unwrap();
    let expr = format!("{storage_type}.setItem({key_lit}, {value_lit}); null;");
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|_| None)
        .map_err(|e| e.to_string())
}

async fn handle_storage_clear(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let storage_type = if params.get("type").and_then(Value::as_str) == Some("session") {
        "sessionStorage"
    } else {
        "localStorage"
    };
    let client = client_or_err(state).await?;
    let expr = format!("{storage_type}.clear(); null;");
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|_| None)
        .map_err(|e| e.to_string())
}

async fn handle_wait_url(state: &DaemonState, params: &Value) -> Result<Option<Value>, String> {
    let pattern = require_string(params, "pattern")?;
    let pattern_type = params
        .get("patternType")
        .and_then(Value::as_str)
        .unwrap_or("exact")
        .to_owned();
    let timeout_ms = optional_u64(params, "timeout", 30_000);
    let client = client_or_err(state).await?;
    let matcher = compile_url_matcher(&pattern, &pattern_type)?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Ok(Some(Value::String(url))) =
            crate::cmd::eval::evaluate_value(&client, "document.location.href").await
            && matcher(&url)
        {
            return Ok(Some(json!({ "url": url })));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "wait-url timed out after {timeout_ms}ms (pattern={pattern} type={pattern_type})"
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
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
    let _action_lock = state.action_mutex.lock().await;

    let result = match req.action.as_str() {
        "tabs.navigate" => handle_navigate(state, &req.params).await,
        "execution.executeJs" => handle_eval(state, &req.params).await,
        "dom.getText" => handle_get_text(state, &req.params).await,
        "dom.getHtml" => handle_get_html(state, &req.params).await,
        "dom.querySelector" => handle_query_selector(state, &req.params).await,
        "cookies.get" => handle_cookies_get(state, &req.params).await,
        "cookies.set" => handle_cookies_set(state, &req.params).await,
        "cookies.delete" => handle_cookies_delete(state, &req.params).await,
        "storage.get" => handle_storage_get(state, &req.params).await,
        "storage.set" => handle_storage_set(state, &req.params).await,
        "storage.clear" => handle_storage_clear(state, &req.params).await,
        "interaction.click" => handle_click(state, &req.params).await,
        "interaction.type" => handle_type(state, &req.params).await,
        "wait.selector" => handle_wait_selector(state, &req.params).await,
        "wait.url" => handle_wait_url(state, &req.params).await,
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

async fn bind_listener_with_lock(paths: &DaemonPaths) -> Result<UnixListener> {
    std::fs::create_dir_all(&paths.base_dir).context("create daemon base dir")?;
    for attempt in 0..2 {
        match UnixListener::bind(&paths.socket_path) {
            Ok(listener) => return Ok(listener),
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
        let errno = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(0);
        errno == libc::EPERM
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

async fn probe_socket_alive(socket_path: &Path) -> bool {
    match tokio::time::timeout(Duration::from_millis(1500), UnixStream::connect(socket_path)).await
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
                eprintln!("[cdp-spike daemon] boot failed: {err:#}");
                let _ = std::fs::remove_file(&boot_paths.socket_path);
                let _ = std::fs::remove_file(&boot_paths.pid_path);
                std::process::exit(1);
            }
        }
    });

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
        "[cdp-spike daemon] listening on {} (pid={})",
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
                        eprintln!("[cdp-spike daemon] accept error: {err}");
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
        let req: Request =
            serde_json::from_str(r#"{"id":"x","action":"daemon.ping"}"#).unwrap();
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
        assert!(v.get("data").is_none());
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
}
