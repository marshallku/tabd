// Multi-tab APIs (TabInfo / ResolvedTarget::Explicit / create_tab / close_tab
// / list_tabs / activate_tab / send_to / reconcile) are called from phase 3c
// daemon handlers, which land in the next stage. Silence dead-code warnings
// at the module level until then so release builds stay quiet.
#![allow(dead_code)]

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Per-tab buffers populated by the reader task as chromium streams events.
/// Caps match TS `MAX_CONSOLE_ENTRIES` / `MAX_ERROR_ENTRIES` (`src/server/
/// runtimes/cdp.ts:100`). 3e2 will add network_log + network_index here.
const MAX_CONSOLE: usize = 100;
const MAX_PAGE_ERRORS: usize = 100;

#[derive(Debug, Clone, Serialize)]
pub struct ConsoleEntry {
    pub level: String,
    pub text: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorEntry {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u64>,
    pub timestamp: u64,
}

/// Per-tab state. sessionId + event-derived ring buffers. `Clone` removed —
/// 3e brings Vec<…> buffers that aren't free to copy and the only callsite
/// that previously needed clone (test helpers) just constructs literals.
#[derive(Debug)]
pub struct TabState {
    pub session_id: String,
    pub console_logs: Vec<ConsoleEntry>,
    pub page_errors: Vec<ErrorEntry>,
}

impl TabState {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            console_logs: Vec::new(),
            page_errors: Vec::new(),
        }
    }
}

/// Multi-tab registry. Single Mutex guards both fields so split-brain races
/// between `tabs` and `active` are impossible.
#[derive(Debug, Default)]
pub struct TabRegistry {
    pub tabs: HashMap<String, TabState>, // targetId → state
    pub active: Option<String>,          // currently focused targetId
}

#[derive(Debug)]
pub enum ResolveError {
    NoActiveTab,
    NoSessionFor(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoActiveTab => write!(f, "no active tab"),
            Self::NoSessionFor(t) => write!(f, "no session for targetId {t:?}"),
        }
    }
}

impl std::error::Error for ResolveError {}

impl TabRegistry {
    /// Resolve a target descriptor to a sessionId clone (for dispatch).
    /// Root → None (sessionId omitted from frame). Active/Explicit → Some.
    pub fn resolve(&self, target: &ResolvedTarget) -> Result<Option<String>, ResolveError> {
        match target {
            ResolvedTarget::Root => Ok(None),
            ResolvedTarget::Active => {
                let tid = self.active.as_ref().ok_or(ResolveError::NoActiveTab)?;
                let state = self
                    .tabs
                    .get(tid)
                    .ok_or_else(|| ResolveError::NoSessionFor(tid.clone()))?;
                Ok(Some(state.session_id.clone()))
            }
            ResolvedTarget::Explicit(tid) => {
                let state = self
                    .tabs
                    .get(tid)
                    .ok_or_else(|| ResolveError::NoSessionFor(tid.clone()))?;
                Ok(Some(state.session_id.clone()))
            }
        }
    }

    /// Drop tabs not present in the fresh chromium-reported set and clear
    /// `active` if it's gone. Used by `list_tabs()` to self-heal stale state
    /// without event subscription.
    pub fn reconcile(&mut self, fresh_ids: &HashSet<String>) {
        self.tabs.retain(|tid, _| fresh_ids.contains(tid));
        if let Some(a) = &self.active {
            if !fresh_ids.contains(a) {
                self.active = None;
            }
        }
    }
}

/// Tab info reported by `list_tabs()`. Every entry is a real chromium page
/// target at the moment of the call (reconciliation happens inside list_tabs).
#[derive(Debug, Clone, Serialize)]
pub struct TabInfo {
    pub target_id: String,
    pub url: String,
    pub title: String,
    pub active: bool,
}

/// Routing descriptor for `dispatch()`. Root = bootstrap calls (no sessionId);
/// Active = use whatever `registry.active` points at; Explicit = caller-named
/// targetId (must exist in registry).
#[derive(Debug, Clone)]
pub enum ResolvedTarget {
    Root,
    Active,
    Explicit(String),
}

/// CDP JSON-RPC client with a multi-tab registry. `send()` routes to the
/// active tab; `send_to(target_id, …)` routes to a named tab; `dispatch` is
/// the unified internal.
///
/// `close(&self)` is idempotent — it best-effort detaches every attached
/// session, then drops the writer mpsc so background tasks exit naturally.
pub struct CdpClient {
    next_id: AtomicU64,
    out_tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    // Arc so the reader task can clone-and-move it for event routing.
    registry: Arc<Mutex<TabRegistry>>,
}

#[derive(Deserialize, Debug)]
struct InboundFrame {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<Value>,
    // Events carry method/params/sessionId. Phase 3e1 routes
    // Runtime.consoleAPICalled + Runtime.exceptionThrown into per-tab ring
    // buffers; 3e2 will add Network.*.
    method: Option<String>,
    params: Option<Value>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

/// Current unix epoch in milliseconds (matches TS `Date.now()` field type).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Build a single-string text payload from Runtime.consoleAPICalled args.
/// Mirrors TS cdp.ts:582-602 — RemoteObject.value if present, else
/// .description, else "".
fn console_text_from_args(args: &Value) -> String {
    let arr = match args.as_array() {
        Some(a) => a,
        None => return String::new(),
    };
    arr.iter()
        .map(|arg| {
            if let Some(value) = arg.get("value") {
                match value {
                    Value::String(s) => s.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                }
            } else if let Some(desc) = arg.get("description").and_then(Value::as_str) {
                desc.to_owned()
            } else {
                String::new()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read a Runtime.exceptionThrown payload into an ErrorEntry. Returns None
/// if the payload is shaped unexpectedly (silently drops in that case).
fn error_entry_from_exception(params: &Value) -> Option<ErrorEntry> {
    let detail = params.get("exceptionDetails")?;
    let exception = detail.get("exception");
    let message = exception
        .and_then(|e| e.get("description"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            detail
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "Unknown error".to_string());
    let source = detail
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let line = detail.get("lineNumber").and_then(Value::as_u64);
    let column = detail.get("columnNumber").and_then(Value::as_u64);
    Some(ErrorEntry {
        message,
        source,
        line,
        column,
        timestamp: now_ms(),
    })
}

/// Trim a Vec ring buffer to `max` entries by dropping from the front.
fn trim_ring<T>(buf: &mut Vec<T>, max: usize) {
    if buf.len() > max {
        let excess = buf.len() - max;
        buf.drain(0..excess);
    }
}

impl CdpClient {
    /// Connect, create the first page target, flatten-attach, enable Page/
    /// Runtime/Network, and seat the new tab as the active one in the
    /// registry. The returned client is ready for `send()` calls.
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let (ws, _resp) = connect_async(ws_url)
            .await
            .with_context(|| format!("ws connect: {ws_url}"))?;
        let (mut sink, mut stream) = ws.split();

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_for_reader = pending.clone();
        let registry: Arc<Mutex<TabRegistry>> = Arc::new(Mutex::new(TabRegistry::default()));
        let registry_for_reader = registry.clone();

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if sink.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                let Ok(Message::Text(text)) = msg else {
                    continue;
                };
                let Ok(parsed) = serde_json::from_str::<InboundFrame>(text.as_str()) else {
                    continue;
                };
                if let Some(id) = parsed.id {
                    let mut map = pending_for_reader.lock().await;
                    if let Some(tx) = map.remove(&id) {
                        let value = match parsed.error {
                            Some(err) => Err(anyhow!("cdp error: {err}")),
                            None => Ok(parsed.result.unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(value);
                    }
                    continue;
                }
                // Events: route by sessionId into the matching TabState. RPC
                // calls from here are forbidden — they'd deadlock against
                // dispatch() (same registry mutex). Push/trim only.
                let (Some(method), Some(sid)) = (parsed.method, parsed.session_id) else {
                    continue;
                };
                let params = parsed.params.unwrap_or(Value::Null);
                let mut reg = registry_for_reader.lock().await;
                let Some(state) = reg.tabs.values_mut().find(|t| t.session_id == sid) else {
                    continue;
                };
                match method.as_str() {
                    "Runtime.consoleAPICalled" => {
                        let level = params
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("log")
                            .to_owned();
                        let text = console_text_from_args(
                            params.get("args").unwrap_or(&Value::Null),
                        );
                        state.console_logs.push(ConsoleEntry {
                            level,
                            text,
                            timestamp: now_ms(),
                        });
                        trim_ring(&mut state.console_logs, MAX_CONSOLE);
                    }
                    "Runtime.exceptionThrown" => {
                        if let Some(entry) = error_entry_from_exception(&params) {
                            state.page_errors.push(entry);
                            trim_ring(&mut state.page_errors, MAX_PAGE_ERRORS);
                        }
                    }
                    _ => {} // 3e2 will add Network.* arms here.
                }
            }
            // Stream closed → fail any still-pending requests so callers don't hang.
            let mut map = pending_for_reader.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(anyhow!("cdp websocket closed")));
            }
        });

        let client = CdpClient {
            next_id: AtomicU64::new(1),
            out_tx: Mutex::new(Some(out_tx)),
            pending,
            registry,
        };

        // 1. Fresh page target (about:blank — callers navigate later).
        let target = client
            .dispatch(
                "Target.createTarget",
                json!({ "url": "about:blank" }),
                ResolvedTarget::Root,
            )
            .await?;
        let target_id = target
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Target.createTarget missing targetId: {target:?}"))?
            .to_owned();

        // 2. Flatten attach (sessionId arrives in the response).
        let attach = client
            .dispatch(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                ResolvedTarget::Root,
            )
            .await?;
        let session_id = attach
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Target.attachToTarget missing sessionId: {attach:?}"))?
            .to_owned();

        // 3. Seat the initial tab as active before enabling domains so the
        // `Active` route resolves correctly for the enable calls below.
        {
            let mut reg = client.registry.lock().await;
            reg.tabs.insert(target_id.clone(), TabState::new(session_id));
            reg.active = Some(target_id);
        }

        // 4. Enable domains on this session.
        client.send("Page.enable", json!({})).await?;
        client.send("Runtime.enable", json!({})).await?;
        client.send("Network.enable", json!({})).await?;

        Ok(client)
    }

    /// Send a method call against the currently active tab.
    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        self.dispatch(method, params, ResolvedTarget::Active).await
    }

    /// Send a method call against an explicitly named tab.
    pub async fn send_to(&self, target_id: &str, method: &str, params: Value) -> Result<Value> {
        self.dispatch(method, params, ResolvedTarget::Explicit(target_id.to_owned()))
            .await
    }

    async fn dispatch(&self, method: &str, params: Value, target: ResolvedTarget) -> Result<Value> {
        // Resolve session at dispatch entry (not at response receive). If the
        // active tab flips mid-call, this request still completes on the
        // original session — matches TS chromium-cdp semantics.
        let session_id: Option<String> = {
            let reg = self.registry.lock().await;
            reg.resolve(&target).map_err(|e| anyhow!("{e}"))?
        };

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let text = build_frame(id, method, params, session_id.as_deref())?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let send_result = {
            let guard = self.out_tx.lock().await;
            match guard.as_ref() {
                Some(sender) => sender.send(text),
                None => {
                    drop(guard);
                    self.pending.lock().await.remove(&id);
                    return Err(anyhow!("cdp writer task closed"));
                }
            }
        };
        if let Err(err) = send_result {
            self.pending.lock().await.remove(&id);
            return Err(anyhow::Error::new(err).context("cdp writer task closed"));
        }

        rx.await.map_err(|_| anyhow!("cdp pending reply dropped"))?
    }

    /// Create a new page target, attach (flatten), enable domains, register
    /// the tab. Does NOT switch `active` — caller decides (3c open-tab spec
    /// sets active=true by default but other callers may differ).
    pub async fn create_tab(&self, url: &str) -> Result<String> {
        let target = self
            .dispatch(
                "Target.createTarget",
                json!({ "url": url }),
                ResolvedTarget::Root,
            )
            .await?;
        let target_id = target
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Target.createTarget missing targetId"))?
            .to_owned();

        let attach = self
            .dispatch(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                ResolvedTarget::Root,
            )
            .await?;
        let session_id = attach
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Target.attachToTarget missing sessionId"))?
            .to_owned();

        // Register before enabling domains so send_to() succeeds.
        {
            let mut reg = self.registry.lock().await;
            reg.tabs.insert(target_id.clone(), TabState::new(session_id));
        }

        self.send_to(&target_id, "Page.enable", json!({})).await?;
        self.send_to(&target_id, "Runtime.enable", json!({})).await?;
        self.send_to(&target_id, "Network.enable", json!({})).await?;

        Ok(target_id)
    }

    /// Close a tab. Best-effort CDP closeTarget; registry is cleaned up
    /// regardless of CDP outcome (tab is gone either way from the daemon's
    /// perspective once we drop it from the registry).
    #[allow(dead_code)] // called from phase 3c handlers
    pub async fn close_tab(&self, target_id: &str) -> Result<()> {
        let _ = self
            .dispatch(
                "Target.closeTarget",
                json!({ "targetId": target_id }),
                ResolvedTarget::Root,
            )
            .await;

        let mut reg = self.registry.lock().await;
        reg.tabs.remove(target_id);
        if reg.active.as_deref() == Some(target_id) {
            reg.active = None;
        }
        Ok(())
    }

    /// Refresh from chromium's `Target.getTargets` and return page targets.
    /// Self-heals stale state: removes registry entries no longer in chromium,
    /// clears `active` if it pointed at one of them.
    pub async fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        let response = self
            .dispatch("Target.getTargets", json!({}), ResolvedTarget::Root)
            .await?;
        let infos = response
            .get("targetInfos")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Target.getTargets missing targetInfos"))?;

        let mut fresh: Vec<(String, String, String)> = Vec::new();
        let mut fresh_ids: HashSet<String> = HashSet::new();
        for info in infos {
            let ty = info.get("type").and_then(Value::as_str).unwrap_or("");
            if ty != "page" {
                continue;
            }
            let tid = info
                .get("targetId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if tid.is_empty() {
                continue;
            }
            let url = info
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let title = info
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            fresh_ids.insert(tid.clone());
            fresh.push((tid, url, title));
        }

        let active = {
            let mut reg = self.registry.lock().await;
            reg.reconcile(&fresh_ids);
            reg.active.clone()
        };
        let active_id = active.as_deref();

        Ok(fresh
            .into_iter()
            .map(|(tid, url, title)| {
                let is_active = active_id == Some(tid.as_str());
                TabInfo {
                    target_id: tid,
                    url,
                    title,
                    active: is_active,
                }
            })
            .collect())
    }

    /// Run a closure with shared access to one tab's state — used by monitor
    /// handlers to snapshot console/error/network buffers without holding the
    /// registry lock past the read. Errors if the targetId isn't attached.
    pub async fn read_tab_state<R>(
        &self,
        target_id: &str,
        f: impl FnOnce(&TabState) -> R,
    ) -> Result<R, String> {
        let reg = self.registry.lock().await;
        let state = reg
            .tabs
            .get(target_id)
            .ok_or_else(|| format!("no session for targetId {target_id:?}"))?;
        Ok(f(state))
    }

    /// Registry-only active flip. Does NOT call CDP `Target.activateTarget`,
    /// so it's safe to use for internal bookkeeping (e.g. `tabs.open` setting
    /// the new tab as active without an OS-focus RPC that can no-op or fail
    /// on headless chromium). For user-driven `tabs.activate`, use
    /// `activate_tab` instead.
    pub async fn set_active(&self, target_id: &str) -> Result<()> {
        let mut reg = self.registry.lock().await;
        if !reg.tabs.contains_key(target_id) {
            return Err(anyhow!("no session for targetId {target_id:?}"));
        }
        reg.active = Some(target_id.to_owned());
        Ok(())
    }

    /// Switch the active tab. Refreshes once via `list_tabs()` if the targetId
    /// isn't in the registry (covers the case where chromium created the
    /// target but we haven't observed it yet). Internal active updates
    /// regardless of CDP `Target.activateTarget` outcome (OS focus is best-
    /// effort for headless daemons).
    pub async fn activate_tab(&self, target_id: &str) -> Result<()> {
        let exists = {
            let reg = self.registry.lock().await;
            reg.tabs.contains_key(target_id)
        };
        if !exists {
            self.list_tabs().await?;
            let still_missing = {
                let reg = self.registry.lock().await;
                !reg.tabs.contains_key(target_id)
            };
            if still_missing {
                return Err(anyhow!("no session for targetId {target_id:?}"));
            }
        }

        {
            let mut reg = self.registry.lock().await;
            reg.active = Some(target_id.to_owned());
        }

        let _ = self
            .dispatch(
                "Target.activateTarget",
                json!({ "targetId": target_id }),
                ResolvedTarget::Root,
            )
            .await;

        Ok(())
    }

    /// Idempotent shutdown. Best-effort detach every attached session (5s
    /// timeout per call to keep teardown bounded if chromium hangs), then
    /// drop the writer mpsc → background tasks exit naturally.
    pub async fn close(&self) -> Result<()> {
        let tabs: Vec<String> = {
            let reg = self.registry.lock().await;
            reg.tabs.keys().cloned().collect()
        };
        for tid in tabs {
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                self.dispatch(
                    "Target.detachFromTarget",
                    json!({ "targetId": tid }),
                    ResolvedTarget::Root,
                ),
            )
            .await;
        }
        let _ = self.out_tx.lock().await.take();
        Ok(())
    }
}

fn build_frame(id: u64, method: &str, params: Value, session_id: Option<&str>) -> Result<String> {
    let mut frame = Map::new();
    frame.insert("id".into(), Value::Number(id.into()));
    frame.insert("method".into(), Value::String(method.into()));
    frame.insert("params".into(), params);
    if let Some(sid) = session_id {
        frame.insert("sessionId".into(), Value::String(sid.into()));
    }
    serde_json::to_string(&Value::Object(frame)).context("serialize cdp frame")
}

// -- Unit tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry_with(active: Option<&str>, entries: &[(&str, &str)]) -> TabRegistry {
        let mut reg = TabRegistry::default();
        for (tid, sid) in entries {
            reg.tabs
                .insert((*tid).to_owned(), TabState::new((*sid).to_owned()));
        }
        reg.active = active.map(str::to_owned);
        reg
    }

    #[test]
    fn registry_resolve_root_returns_none() {
        let reg = registry_with(None, &[]);
        assert!(reg.resolve(&ResolvedTarget::Root).unwrap().is_none());
    }

    #[test]
    fn registry_resolve_active_returns_session() {
        let reg = registry_with(Some("t1"), &[("t1", "sess-A"), ("t2", "sess-B")]);
        let got = reg.resolve(&ResolvedTarget::Active).unwrap();
        assert_eq!(got.as_deref(), Some("sess-A"));
    }

    #[test]
    fn registry_resolve_active_without_active_errors() {
        let reg = registry_with(None, &[("t1", "sess-A")]);
        let err = reg.resolve(&ResolvedTarget::Active).err().unwrap();
        assert!(matches!(err, ResolveError::NoActiveTab));
    }

    #[test]
    fn registry_resolve_active_with_stale_pointer_errors() {
        let reg = registry_with(Some("ghost"), &[("t1", "sess-A")]);
        let err = reg.resolve(&ResolvedTarget::Active).err().unwrap();
        assert!(matches!(err, ResolveError::NoSessionFor(ref s) if s == "ghost"));
    }

    #[test]
    fn registry_resolve_explicit_hit() {
        let reg = registry_with(Some("t1"), &[("t1", "sess-A"), ("t2", "sess-B")]);
        let got = reg
            .resolve(&ResolvedTarget::Explicit("t2".to_owned()))
            .unwrap();
        assert_eq!(got.as_deref(), Some("sess-B"));
    }

    #[test]
    fn registry_resolve_explicit_miss_errors() {
        let reg = registry_with(Some("t1"), &[("t1", "sess-A")]);
        let err = reg
            .resolve(&ResolvedTarget::Explicit("nope".to_owned()))
            .err()
            .unwrap();
        assert!(matches!(err, ResolveError::NoSessionFor(ref s) if s == "nope"));
    }

    #[test]
    fn registry_reconcile_drops_gone_and_clears_active() {
        let mut reg = registry_with(Some("t1"), &[("t1", "sess-A"), ("t2", "sess-B")]);
        let fresh: HashSet<String> = ["t2".to_owned()].into_iter().collect();
        reg.reconcile(&fresh);
        assert!(!reg.tabs.contains_key("t1"));
        assert!(reg.tabs.contains_key("t2"));
        assert!(reg.active.is_none(), "active was on t1 which is gone");
    }

    #[test]
    fn registry_reconcile_keeps_active_when_present() {
        let mut reg = registry_with(Some("t1"), &[("t1", "sess-A"), ("t2", "sess-B")]);
        let fresh: HashSet<String> = ["t1".to_owned(), "t2".to_owned()].into_iter().collect();
        reg.reconcile(&fresh);
        assert_eq!(reg.active.as_deref(), Some("t1"));
        assert_eq!(reg.tabs.len(), 2);
    }

    #[test]
    fn resolve_error_display() {
        assert_eq!(format!("{}", ResolveError::NoActiveTab), "no active tab");
        assert_eq!(
            format!("{}", ResolveError::NoSessionFor("t9".to_owned())),
            "no session for targetId \"t9\""
        );
    }

    #[test]
    fn frame_includes_session_id_when_present() {
        let text = build_frame(7, "Page.navigate", json!({ "url": "https://x" }), Some("sess-1"))
            .unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["id"], json!(7));
        assert_eq!(parsed["method"], json!("Page.navigate"));
        assert_eq!(parsed["params"]["url"], json!("https://x"));
        assert_eq!(parsed["sessionId"], json!("sess-1"));
    }

    #[test]
    fn frame_omits_session_id_when_root() {
        let text = build_frame(1, "Target.createTarget", json!({ "url": "about:blank" }), None)
            .unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert!(parsed.get("sessionId").is_none(), "got: {parsed}");
    }

    #[test]
    fn frame_preserves_nested_params() {
        let text = build_frame(
            42,
            "Runtime.evaluate",
            json!({
                "expression": "1 + 1",
                "returnByValue": true,
                "awaitPromise": false,
            }),
            Some("s"),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["params"]["expression"], json!("1 + 1"));
        assert_eq!(parsed["params"]["returnByValue"], json!(true));
        assert_eq!(parsed["params"]["awaitPromise"], json!(false));
    }

    #[test]
    fn inbound_parses_success_result() {
        let raw = r#"{"id":3,"result":{"value":"hi"}}"#;
        let p: InboundFrame = serde_json::from_str(raw).unwrap();
        assert_eq!(p.id, Some(3));
        assert_eq!(p.result.unwrap()["value"], json!("hi"));
        assert!(p.error.is_none());
    }

    #[test]
    fn inbound_parses_error() {
        let raw = r#"{"id":4,"error":{"code":-32000,"message":"bad"}}"#;
        let p: InboundFrame = serde_json::from_str(raw).unwrap();
        assert_eq!(p.id, Some(4));
        assert!(p.result.is_none());
        assert_eq!(p.error.unwrap()["message"], json!("bad"));
    }

    #[test]
    fn inbound_parses_event_with_session_id() {
        let raw = r#"{"method":"Runtime.consoleAPICalled","sessionId":"s1","params":{"type":"log"}}"#;
        let p: InboundFrame = serde_json::from_str(raw).unwrap();
        assert!(p.id.is_none());
        assert_eq!(p.method.as_deref(), Some("Runtime.consoleAPICalled"));
        assert_eq!(p.session_id.as_deref(), Some("s1"));
    }

    // End-to-end smoke: spawn real chromium, exercise multi-tab paths.
    #[tokio::test]
    #[ignore = "requires real chromium; covers multi-tab create/activate/eval"]
    async fn cdp_multi_tab_roundtrip() {
        let browser = crate::browser::Browser::launch()
            .await
            .expect("launch chromium");
        let client = CdpClient::connect(browser.ws_endpoint())
            .await
            .expect("cdp connect");

        // The initial tab (about:blank) is already active. Open a second
        // with a distinguishable title.
        let t2 = client
            .create_tab("data:text/html,<title>Two</title>")
            .await
            .expect("create_tab");

        // First call still routes to original active (t1).
        let r1 = client
            .send(
                "Runtime.evaluate",
                json!({ "expression": "document.title", "returnByValue": true }),
            )
            .await
            .expect("eval on t1");
        let title1 = r1["result"]["value"].as_str().unwrap_or("").to_owned();

        // Flip to t2 and eval again — should yield the new title.
        client.activate_tab(&t2).await.expect("activate t2");
        let r2 = client
            .send(
                "Runtime.evaluate",
                json!({ "expression": "document.title", "returnByValue": true }),
            )
            .await
            .expect("eval on t2");
        let title2 = r2["result"]["value"].as_str().unwrap_or("").to_owned();

        assert_ne!(title1, title2, "expected different titles per tab");
        assert_eq!(title2, "Two");

        client.close().await.expect("close");
        browser.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    #[ignore = "requires real chromium; covers list_tabs reconciliation"]
    async fn cdp_list_tabs_reconciles_external_close() {
        let browser = crate::browser::Browser::launch()
            .await
            .expect("launch chromium");
        let client = CdpClient::connect(browser.ws_endpoint())
            .await
            .expect("cdp connect");

        let t2 = client
            .create_tab("data:text/html,<title>Two</title>")
            .await
            .expect("create_tab");
        let before = client.list_tabs().await.expect("list before");
        assert!(before.iter().any(|t| t.target_id == t2));

        // Drive Target.closeTarget from Root (no session) — simulates an
        // external close that bypasses our `close_tab()` registry cleanup.
        let _ = client
            .dispatch(
                "Target.closeTarget",
                json!({ "targetId": t2 }),
                ResolvedTarget::Root,
            )
            .await;

        let after = client.list_tabs().await.expect("list after");
        assert!(
            !after.iter().any(|t| t.target_id == t2),
            "expected t2 to be reconciled out: {after:?}"
        );

        client.close().await.expect("close");
        browser.shutdown().await.expect("shutdown");
    }
}
