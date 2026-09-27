// Multi-tab APIs (TabInfo / ResolvedTarget::Explicit / create_tab / close_tab
// / list_tabs / activate_tab / send_to / reconcile) are called from phase 3c
// daemon handlers, which land in the next stage. Silence dead-code warnings
// at the module level until then so release builds stay quiet.
#![allow(dead_code)]

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Per-tab buffers populated by the reader task as chromium streams events.
/// Caps match TS `MAX_CONSOLE_ENTRIES` / `MAX_ERROR_ENTRIES` (`src/server/
/// runtimes/cdp.ts:100`). 3e2 will add network_log + network_index here.
const MAX_CONSOLE: usize = 100;
const MAX_PAGE_ERRORS: usize = 100;
const MAX_NETWORK: usize = 500;
const MAX_DIALOGS: usize = 50;
/// Cap on the registry-global download history. In-progress entries are NEVER
/// evicted (only terminal completed/canceled entries are dropped over cap), so
/// a burst of downloads can't make an active `wait-download` impossible.
const MAX_DOWNLOADS: usize = 100;

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

/// One stitched request/response cycle for `monitor.networkLogs`. Fields are
/// camelCase on the wire to match TS NetworkEntry; optional fields are
/// skipped from JSON when None so consumers see a clean null vs missing
/// boundary. responseBody is always None for now (body fetch deferred — see
/// phase 3e2 plan; would need Arc<CdpClient> + spawn task to avoid reader-
/// task self-deadlock against the registry mutex).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkEntry {
    pub request_id: String,
    pub url: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_headers: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    pub response_body_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body_size: Option<u64>,
    pub start_time: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub from_cache: bool,
    pub failed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_text: Option<String>,
}

/// Find the most recent entry with `request_id` (reverse iter — Network.*
/// events generally arrive close to their requestWillBeSent so the hit lands
/// in the last few entries). Returns None if missing — happens when the
/// stub was evicted by ring trim before the response arrived.
pub fn find_network_entry_mut<'a>(
    log: &'a mut [NetworkEntry],
    request_id: &str,
) -> Option<&'a mut NetworkEntry> {
    log.iter_mut().rev().find(|e| e.request_id == request_id)
}

/// Per-tab state. sessionId + event-derived ring buffers. `Clone` removed —
/// 3e brings Vec<…> buffers that aren't free to copy and the only callsite
/// that previously needed clone (test helpers) just constructs literals.
#[derive(Debug)]
pub struct TabState {
    pub session_id: String,
    pub console_logs: Vec<ConsoleEntry>,
    pub page_errors: Vec<ErrorEntry>,
    pub network_log: Vec<NetworkEntry>,
    /// Inflight count for `wait.networkIdle`. requestWillBeSent inc,
    /// loadingFinished/loadingFailed dec (saturating). Reader task writes,
    /// handler reads.
    pub network_pending: u32,
    /// Auto-handled JS dialogs (newest last). Reader task writes on
    /// Page.javascriptDialogOpening; `monitor.dialogs` reads.
    pub dialogs: Vec<DialogEntry>,
}

impl TabState {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            console_logs: Vec::new(),
            page_errors: Vec::new(),
            network_log: Vec::new(),
            network_pending: 0,
            dialogs: Vec::new(),
        }
    }
}

/// A JS dialog the daemon auto-handled. `action` records what was done
/// ("accept" / "dismiss") so agents can audit what happened to e.g. an
/// auto-accepted beforeunload (potential unsaved-state loss).
#[derive(Debug, Clone, Serialize)]
pub struct DialogEntry {
    #[serde(rename = "dialogType")]
    pub dialog_type: String,
    pub message: String,
    pub action: String,
    #[serde(rename = "promptText", skip_serializing_if = "Option::is_none")]
    pub prompt_text: Option<String>,
    pub timestamp: u64,
}

/// Global auto-response policy for JS dialogs, applied by the event reader the
/// moment a dialog opens. A pending dialog blocks the page's JS — and the
/// action that triggered it holds the daemon's global action mutex — so
/// responding later via a command is structurally impossible; this policy is
/// pre-configuration, not live rescue. `beforeunload` always accepts
/// regardless (a dismissed beforeunload silently blocks navigation).
#[derive(Debug, Clone, Default)]
pub struct DialogPolicy {
    /// false (default) = dismiss alert/confirm/prompt; true = accept.
    pub accept: bool,
    /// Text typed into `prompt()` when accepting.
    pub prompt_text: Option<String>,
}

/// A captured browser download. `saved_path` is `<download_dir>/<guid>`
/// (Browser.setDownloadBehavior `allowAndName` names files by guid, so there
/// are no collisions); the original name lives in `suggested_filename`. The
/// agent owns the file afterwards — tabd never deletes it.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadEntry {
    pub guid: String,
    pub url: String,
    #[serde(rename = "suggestedFilename")]
    pub suggested_filename: String,
    /// "inProgress" | "completed" | "canceled".
    pub state: String,
    #[serde(rename = "totalBytes", skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(rename = "receivedBytes")]
    pub received_bytes: u64,
    #[serde(rename = "savedPath")]
    pub saved_path: String,
    /// Epoch ms when downloadWillBegin arrived — orders "most recent".
    #[serde(rename = "startedAt")]
    pub started_at: u64,
}

impl DownloadEntry {
    fn is_terminal(&self) -> bool {
        self.state == "completed" || self.state == "canceled"
    }
}

/// Apply a `downloadWillBegin` to the store: insert a fresh in-progress entry,
/// then ring-cap by dropping only the OLDEST terminal entries (in-progress
/// downloads are never evicted — see [`MAX_DOWNLOADS`]). Pure for testability.
pub(crate) fn download_begin(store: &mut Vec<DownloadEntry>, entry: DownloadEntry, max: usize) {
    store.push(entry);
    while store.len() > max {
        match store.iter().position(|e| e.is_terminal()) {
            Some(i) => {
                store.remove(i);
            }
            None => break, // all in-progress — keep them all
        }
    }
}

/// Apply a `downloadProgress` to the store: update bytes/state of the matching
/// guid. Unknown guids are ignored (the willBegin may have been evicted or the
/// download predates enablement). Pure for testability.
pub(crate) fn download_progress(
    store: &mut [DownloadEntry],
    guid: &str,
    state: &str,
    total_bytes: Option<u64>,
    received_bytes: Option<u64>,
) {
    if let Some(e) = store.iter_mut().find(|e| e.guid == guid) {
        if !state.is_empty() {
            e.state = state.to_owned();
        }
        if let Some(t) = total_bytes {
            e.total_bytes = Some(t);
        }
        if let Some(r) = received_bytes {
            e.received_bytes = r;
        }
    }
}

/// Multi-tab registry. Single Mutex guards all fields so split-brain races are
/// impossible.
#[derive(Debug, Default)]
pub struct TabRegistry {
    pub tabs: HashMap<String, TabState>, // targetId → state
    pub active: Option<String>,          // currently focused targetId
    pub dialog_policy: DialogPolicy,
    /// Download interception target dir; None until `download-dir` enables it.
    pub download_dir: Option<std::path::PathBuf>,
    /// Browser-global download history (newest last).
    pub downloads: Vec<DownloadEntry>,
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
        if let Some(a) = &self.active
            && !fresh_ids.contains(a)
        {
            self.active = None;
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
/// Backstop timeout for a single CDP RPC. Every method call funnels through
/// `dispatch`, so this bounds how long a wedged Chromium (crash mid-flight,
/// an infinite-loop `Runtime.evaluate`, a hung renderer) can stall the caller.
/// The daemon holds a global action lock per request, so an unbounded RPC would
/// otherwise block every other client forever. 30s is generous for any real
/// single round-trip; it does cap a deliberately long `awaitPromise` evaluate.
const RPC_TIMEOUT_MS: u64 = 30_000;

/// In-flight CDP requests, keyed by frame id. A `std::sync::Mutex` (not tokio)
/// so a `PendingGuard` can clean up synchronously from `Drop` — the lock is
/// never held across an `.await`.
type PendingMap = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// Removes a pending entry on drop, so a `dispatch` future that is cancelled
/// (e.g. an outer `tokio::time::timeout` fires before the reply) or times out
/// internally never leaks its slot in the pending map.
struct PendingGuard {
    pending: PendingMap,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&self.id);
        }
    }
}

pub struct CdpClient {
    // Arc so the reader task shares the id space for its fire-and-forget
    // dialog responses (no collisions with dispatch()-issued ids).
    next_id: Arc<AtomicU64>,
    out_tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending: PendingMap,
    // Arc so the reader task can clone-and-move it for event routing.
    registry: Arc<Mutex<TabRegistry>>,
    // Taken by `close()`. The reader task holds a clone of `out_tx` for its
    // fire-and-forget dialog replies, so dropping the client's sender alone
    // never ends the writer — the tasks have to be aborted and joined, and
    // for the pipe transport that join is what releases the descriptors the
    // browser is waiting on.
    tasks: Mutex<Option<TransportTasks>>,
    transport_closed: Arc<TransportClosed>,
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
    let source = detail.get("url").and_then(Value::as_str).map(str::to_owned);
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

/// How long teardown waits for the transport tasks to end on their own before
/// aborting them. For the pipe transport the fds live inside those tasks, so a
/// join that never completes would keep the browser alive indefinitely.
const TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound on a partially-received pipe frame. Chromium's own frames are
/// well under this; the cap exists so a transport that never sends a NUL
/// cannot grow the accumulator without bound.
const MAX_PENDING_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// How the client talks to Chromium.
///
/// `WebSocket` is the headless path (`--remote-debugging-port`), unchanged.
/// `Pipe` is visual mode (`--remote-debugging-pipe`): the browser reads
/// commands on fd 3 and writes responses on fd 4, with `\0`-delimited JSON
/// frames in both directions. The pipe is not reachable from another local
/// process, which is the whole point — see the A3 row of the threat model in
/// `docs/visual-mode-plan.md`.
pub enum Transport {
    WebSocket(String),
    Pipe {
        /// Parent's write end; peer of the browser's fd 3.
        to_browser: OwnedFd,
        /// Parent's read end; peer of the browser's fd 4.
        from_browser: OwnedFd,
    },
}

pub struct ConnectOptions {
    pub transport: Transport,
    /// Create + attach an `about:blank` tab and seat it as active. True for
    /// headless (callers expect a tab to exist); false for visual.
    pub bootstrap_tab: bool,
}

/// One-shot "the transport ended" flag with a wakeup. Set by whichever reader
/// loop is running, exactly once, as it exits.
#[derive(Default)]
struct TransportClosed {
    flag: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl TransportClosed {
    fn set(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_closed(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            if self.is_closed() {
                return;
            }
            notified.await;
        }
    }
}

/// Everything a reader loop needs to route one inbound frame. Identical for
/// both transports — the transport-specific code ends at "I have a frame".
struct ReaderCtx {
    pending: PendingMap,
    registry: Arc<Mutex<TabRegistry>>,
    next_id: Arc<AtomicU64>,
    out_tx: mpsc::UnboundedSender<String>,
    closed: Arc<TransportClosed>,
}

/// The spawned writer + reader pair. Held by the client so teardown can wait
/// for them: for the pipe transport, the browser only exits once both tasks
/// have dropped their descriptors.
struct TransportTasks {
    // `Option` so `shutdown` can consume the handles and leave `Drop` with
    // nothing to do.
    writer: Option<tokio::task::JoinHandle<()>>,
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl TransportTasks {
    fn new(writer: tokio::task::JoinHandle<()>, reader: tokio::task::JoinHandle<()>) -> Self {
        TransportTasks {
            writer: Some(writer),
            reader: Some(reader),
        }
    }

    /// Abort both tasks and wait for them to actually stop.
    ///
    /// Abort rather than "wait for a natural exit": the writer does end when
    /// the outbound channel drops, but the reader only ends on EOF, and at
    /// teardown the browser is usually still alive — waiting for it would add
    /// a fixed stall to every shutdown. The caller has already drained the
    /// pending map, so nothing in flight is worth flushing. The join is what
    /// matters: the tasks own the pipe descriptors, and the browser does not
    /// see EOF until they have been dropped.
    async fn shutdown(mut self) {
        // Per-handle rather than a two-slot destructure: an `else` arm that
        // dropped a tuple holding one `Some` would *detach* that task instead
        // of aborting it. Both slots are always set and cleared together
        // today, so that is unreachable — but it costs nothing to make the
        // unreachable case behave.
        let writer = self.writer.take();
        let reader = self.reader.take();
        if let Some(writer) = &writer {
            writer.abort();
        }
        if let Some(reader) = &reader {
            reader.abort();
        }
        let _ = tokio::time::timeout(TASK_JOIN_TIMEOUT, async {
            if let Some(writer) = writer {
                let _ = writer.await;
            }
            if let Some(reader) = reader {
                let _ = reader.await;
            }
        })
        .await;
    }
}

impl Drop for TransportTasks {
    /// Backstop for every path that never reaches `close()`: an early return
    /// during bootstrap, a cancelled future, a panicking task. A detached
    /// reader keeps its clone of the outbound sender alive, which keeps the
    /// writer alive, which keeps the command pipe open — and with visual
    /// mode's `kill_on_drop(false)`, that leaves a browser running with
    /// nobody owning it. `Drop` cannot await, so this aborts without joining;
    /// `close()` still does the bounded join when it is reached.
    fn drop(&mut self) {
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
    }
}

/// Fail every in-flight request. Idempotent: a second call finds an empty map.
fn drain_pending(pending: &PendingMap, reason: &str) {
    let Ok(mut map) = pending.lock() else {
        return;
    };
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(anyhow!("{reason}")));
    }
}

/// Reassembles `\0`-delimited frames from arbitrary pipe reads. A frame can
/// straddle any number of reads and several frames can arrive in one.
#[derive(Default)]
struct FrameAccumulator {
    buf: Vec<u8>,
}

impl FrameAccumulator {
    /// Append `chunk` and drain every complete frame into `out`. Errors only
    /// when the *unterminated* remainder exceeds `max`.
    fn push(&mut self, chunk: &[u8], max: usize, out: &mut Vec<String>) -> Result<()> {
        let mut rest = chunk;
        while let Some(pos) = rest.iter().position(|b| *b == 0) {
            self.buf.extend_from_slice(&rest[..pos]);
            rest = &rest[pos + 1..];
            let frame = std::mem::take(&mut self.buf);
            if frame.is_empty() {
                continue;
            }
            match String::from_utf8(frame) {
                Ok(text) => out.push(text),
                // Chromium only ever emits UTF-8 JSON here. Dropping the one
                // malformed frame costs a single RPC (its caller times out)
                // instead of tearing down the whole transport.
                Err(_) => eprintln!("[tabd cdp] dropped a non-UTF-8 pipe frame"),
            }
        }
        self.buf.extend_from_slice(rest);
        if self.buf.len() > max {
            self.buf.clear();
            bail!("cdp pipe frame exceeded {max} bytes with no NUL terminator");
        }
        Ok(())
    }
}

async fn spawn_transport_tasks(
    transport: Transport,
    out_rx: mpsc::UnboundedReceiver<String>,
    ctx: ReaderCtx,
) -> Result<TransportTasks> {
    match transport {
        Transport::WebSocket(url) => {
            let (ws, _resp) = connect_async(&url)
                .await
                .with_context(|| format!("ws connect: {url}"))?;
            let (sink, stream) = ws.split();
            Ok(TransportTasks::new(
                tokio::spawn(writer_ws(sink, out_rx)),
                tokio::spawn(reader_ws(stream, ctx)),
            ))
        }
        Transport::Pipe {
            to_browser,
            from_browser,
        } => {
            // Both conversions happen before either spawn, so a failure here
            // drops both descriptors instead of stranding one in a live task.
            let tx = pipe::Sender::from_owned_fd(to_browser)
                .context("wrap cdp pipe write end (fd 3 peer)")?;
            let rx = pipe::Receiver::from_owned_fd(from_browser)
                .context("wrap cdp pipe read end (fd 4 peer)")?;
            Ok(TransportTasks::new(
                tokio::spawn(writer_pipe(tx, out_rx)),
                tokio::spawn(reader_pipe(rx, ctx)),
            ))
        }
    }
}

async fn writer_ws<S>(mut sink: S, mut out_rx: mpsc::UnboundedReceiver<String>)
where
    S: futures_util::Sink<Message> + Unpin,
{
    while let Some(msg) = out_rx.recv().await {
        if sink.send(Message::Text(msg)).await.is_err() {
            break;
        }
    }
    let _ = sink.close().await;
}

async fn writer_pipe(mut tx: pipe::Sender, mut out_rx: mpsc::UnboundedReceiver<String>) {
    while let Some(msg) = out_rx.recv().await {
        // Payload then terminator. This is the only writer on the pipe, so
        // the two writes cannot interleave with another frame.
        if tx.write_all(msg.as_bytes()).await.is_err() || tx.write_all(b"\0").await.is_err() {
            break;
        }
    }
}

async fn reader_ws<S, E>(mut stream: S, ctx: ReaderCtx)
where
    S: futures_util::Stream<Item = std::result::Result<Message, E>> + Unpin,
{
    while let Some(msg) = stream.next().await {
        let Ok(Message::Text(text)) = msg else {
            continue;
        };
        route_frame(text.as_str(), &ctx).await;
    }
    ctx.closed.set();
    drain_pending(&ctx.pending, "cdp websocket closed");
}

async fn reader_pipe(mut rx: pipe::Receiver, ctx: ReaderCtx) {
    let mut acc = FrameAccumulator::default();
    let mut buf = vec![0u8; 64 * 1024];
    let mut frames: Vec<String> = Vec::new();
    loop {
        let n = match rx.read(&mut buf).await {
            Ok(0) => break, // browser closed fd 4
            Ok(n) => n,
            Err(err) => {
                eprintln!("[tabd cdp] pipe read failed: {err}");
                break;
            }
        };
        if let Err(err) = acc.push(&buf[..n], MAX_PENDING_FRAME_BYTES, &mut frames) {
            eprintln!("[tabd cdp] {err}");
            break;
        }
        for frame in frames.drain(..) {
            route_frame(&frame, &ctx).await;
        }
    }
    ctx.closed.set();
    drain_pending(&ctx.pending, "cdp pipe closed");
}

/// Route one inbound CDP frame into the pending map or the per-tab registry.
/// RPC calls from here are forbidden — they would deadlock against
/// `dispatch()` on the same registry mutex. Push/trim only.
async fn route_frame(text: &str, ctx: &ReaderCtx) {
    let Ok(parsed) = serde_json::from_str::<InboundFrame>(text) else {
        return;
    };
    if let Some(id) = parsed.id {
        let mut map = ctx.pending.lock().unwrap();
        if let Some(tx) = map.remove(&id) {
            let value = match parsed.error {
                Some(err) => Err(anyhow!("cdp error: {err}")),
                None => Ok(parsed.result.unwrap_or(Value::Null)),
            };
            let _ = tx.send(value);
        }
        return;
    }
    // Events: RPC calls from here are forbidden — they'd deadlock
    // against dispatch() (same registry mutex). Push/trim only.
    let Some(method) = parsed.method else {
        return;
    };
    let params = parsed.params.unwrap_or(Value::Null);
    let mut reg = ctx.registry.lock().await;

    // Browser-level download events carry NO sessionId and are
    // browser-global — handle them against the registry store
    // before the per-tab sessionId gate below.
    if method == "Browser.downloadWillBegin" {
        if let Some(dir) = reg.download_dir.clone() {
            let guid = params
                .get("guid")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if !guid.is_empty() {
                let saved = dir.join(&guid).to_string_lossy().into_owned();
                let entry = DownloadEntry {
                    url: params
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    suggested_filename: params
                        .get("suggestedFilename")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    guid,
                    state: "inProgress".to_owned(),
                    total_bytes: None,
                    received_bytes: 0,
                    saved_path: saved,
                    started_at: now_ms(),
                };
                download_begin(&mut reg.downloads, entry, MAX_DOWNLOADS);
            }
        }
        return;
    }
    if method == "Browser.downloadProgress" {
        let guid = params.get("guid").and_then(Value::as_str).unwrap_or("");
        if !guid.is_empty() {
            let state = params.get("state").and_then(Value::as_str).unwrap_or("");
            let total = params.get("totalBytes").and_then(Value::as_f64);
            let received = params.get("receivedBytes").and_then(Value::as_f64);
            download_progress(
                &mut reg.downloads,
                guid,
                state,
                total.map(|t| t as u64),
                received.map(|r| r as u64),
            );
        }
        return;
    }

    // Per-tab events: require a sessionId to route into a TabState.
    let Some(sid) = parsed.session_id else {
        return;
    };
    // Cloned before `state` mutably borrows reg.tabs below.
    let dialog_policy = if method == "Page.javascriptDialogOpening" {
        Some(reg.dialog_policy.clone())
    } else {
        None
    };
    let Some(state) = reg.tabs.values_mut().find(|t| t.session_id == sid) else {
        return;
    };
    match method.as_str() {
        "Page.javascriptDialogOpening" => {
            // A pending dialog blocks the page's JS while the
            // triggering action holds the global action mutex, so
            // this is the only point where it can be answered.
            // Fire-and-forget enqueue — awaiting an RPC here would
            // deadlock (see comment above); the reply carries no
            // payload and is dropped by the no-pending-entry path.
            let dialog_type = params
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("alert")
                .to_owned();
            let message = params
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let policy = dialog_policy.unwrap_or_default();
            // beforeunload always accepts: dismissing it silently
            // blocks navigation, which is worse for automation
            // than the (recorded) unsaved-state loss.
            let accept = dialog_type == "beforeunload" || policy.accept;
            let prompt_text = if accept && dialog_type == "prompt" {
                policy.prompt_text.clone()
            } else {
                None
            };
            state.dialogs.push(DialogEntry {
                dialog_type,
                message,
                action: if accept { "accept" } else { "dismiss" }.to_owned(),
                prompt_text: prompt_text.clone(),
                timestamp: now_ms(),
            });
            trim_ring(&mut state.dialogs, MAX_DIALOGS);
            let id = ctx.next_id.fetch_add(1, Ordering::SeqCst);
            let mut reply_params = json!({ "accept": accept });
            if let Some(text) = prompt_text {
                reply_params["promptText"] = json!(text);
            }
            let frame = json!({
                "id": id,
                "sessionId": sid,
                "method": "Page.handleJavaScriptDialog",
                "params": reply_params,
            })
            .to_string();
            let _ = ctx.out_tx.send(frame);
        }
        "Runtime.consoleAPICalled" => {
            let level = params
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("log")
                .to_owned();
            let text = console_text_from_args(params.get("args").unwrap_or(&Value::Null));
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
        "Network.requestWillBeSent" => {
            let request_id = params
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if request_id.is_empty() {
                return;
            }
            let request = params.get("request").cloned().unwrap_or(Value::Null);
            let url = request
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_owned();
            let resource_type = params
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let request_headers = request.get("headers").cloned();
            let request_body = request
                .get("postData")
                .and_then(Value::as_str)
                .map(str::to_owned);
            state.network_log.push(NetworkEntry {
                request_id,
                url,
                method,
                resource_type,
                status: None,
                status_text: None,
                request_headers,
                response_headers: None,
                request_body,
                response_body: None,
                response_body_truncated: false,
                response_body_size: None,
                start_time: now_ms(),
                end_time: None,
                duration_ms: None,
                from_cache: false,
                failed: false,
                failure_text: None,
            });
            trim_ring(&mut state.network_log, MAX_NETWORK);
            state.network_pending = state.network_pending.saturating_add(1);
        }
        "Network.responseReceived" => {
            let request_id = params
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or("");
            if request_id.is_empty() {
                return;
            }
            let response = params.get("response").cloned().unwrap_or(Value::Null);
            let resource_type = params
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let status = response
                .get("status")
                .and_then(Value::as_u64)
                .map(|n| n as u16);
            let status_text = response
                .get("statusText")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let response_headers = response.get("headers").cloned();
            let from_cache = response
                .get("fromDiskCache")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Some(entry) = find_network_entry_mut(&mut state.network_log, request_id) {
                entry.status = status;
                entry.status_text = status_text;
                entry.response_headers = response_headers;
                entry.from_cache = from_cache;
                if resource_type.is_some() {
                    entry.resource_type = resource_type;
                }
            }
        }
        "Network.loadingFinished" => {
            let request_id = params
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or("");
            if request_id.is_empty() {
                return;
            }
            let encoded_length = params.get("encodedDataLength").and_then(Value::as_u64);
            let now = now_ms();
            if let Some(entry) = find_network_entry_mut(&mut state.network_log, request_id) {
                entry.end_time = Some(now);
                entry.duration_ms = Some(now.saturating_sub(entry.start_time));
                entry.response_body_size = encoded_length;
            }
            state.network_pending = state.network_pending.saturating_sub(1);
        }
        "Network.loadingFailed" => {
            let request_id = params
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or("");
            if request_id.is_empty() {
                return;
            }
            let error_text = params
                .get("errorText")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let now = now_ms();
            if let Some(entry) = find_network_entry_mut(&mut state.network_log, request_id) {
                entry.failed = true;
                entry.failure_text = error_text;
                entry.end_time = Some(now);
                entry.duration_ms = Some(now.saturating_sub(entry.start_time));
            }
            state.network_pending = state.network_pending.saturating_sub(1);
        }
        _ => {} // other domain events silently dropped
    }
}

impl CdpClient {
    /// Connect over a WebSocket endpoint with the historical defaults: a
    /// bootstrap `about:blank` tab, created and seated as active. This is the
    /// headless entry point and its behavior is unchanged.
    pub async fn connect(ws_url: &str) -> Result<Self> {
        Self::connect_with(ConnectOptions {
            transport: Transport::WebSocket(ws_url.to_owned()),
            bootstrap_tab: true,
        })
        .await
    }

    /// Connect over an arbitrary [`Transport`]. Visual mode passes
    /// `bootstrap_tab: false` — the human already has windows open, and
    /// manufacturing an `about:blank` tab in their browser would be rude.
    pub async fn connect_with(opts: ConnectOptions) -> Result<Self> {
        let ConnectOptions {
            transport,
            bootstrap_tab,
        } = opts;

        let pending: PendingMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let registry: Arc<Mutex<TabRegistry>> = Arc::new(Mutex::new(TabRegistry::default()));
        let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
        // Shared with the reader task: id allocation for fire-and-forget
        // frames + the outbound channel itself (cloned before the spawn).
        let next_id: Arc<AtomicU64> = Arc::new(AtomicU64::new(1));
        let transport_closed = Arc::new(TransportClosed::default());

        let ctx = ReaderCtx {
            pending: pending.clone(),
            registry: registry.clone(),
            next_id: next_id.clone(),
            out_tx: out_tx.clone(),
            closed: transport_closed.clone(),
        };
        let tasks = spawn_transport_tasks(transport, out_rx, ctx).await?;

        let client = CdpClient {
            next_id,
            out_tx: Mutex::new(Some(out_tx)),
            pending,
            registry,
            tasks: Mutex::new(Some(tasks)),
            transport_closed,
        };

        // From here on the tasks are owned by `client`, so every failure path
        // must run `close()` — dropping the client would leave them detached,
        // and for the pipe transport that means the browser never sees EOF.
        if bootstrap_tab && let Err(err) = client.bootstrap_tab().await {
            let _ = client.close().await;
            return Err(err);
        }

        Ok(client)
    }

    /// Create the first page target, flatten-attach, seat it as active, and
    /// enable Page/Runtime/Network on it.
    async fn bootstrap_tab(&self) -> Result<()> {
        // 1. Fresh page target (about:blank — callers navigate later).
        let target = self
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
            .ok_or_else(|| anyhow!("Target.attachToTarget missing sessionId: {attach:?}"))?
            .to_owned();

        // 3. Seat the initial tab as active before enabling domains so the
        // `Active` route resolves correctly for the enable calls below.
        {
            let mut reg = self.registry.lock().await;
            reg.tabs
                .insert(target_id.clone(), TabState::new(session_id));
            reg.active = Some(target_id);
        }

        // 4. Enable domains on this session.
        self.send("Page.enable", json!({})).await?;
        self.send("Runtime.enable", json!({})).await?;
        self.send("Network.enable", json!({})).await?;
        Ok(())
    }

    /// Resolves once the transport has ended — the browser closed its end of
    /// the pipe / websocket, or the reader hit an unrecoverable error. The
    /// visual lifecycle coordinator waits on this instead of polling.
    pub async fn transport_closed(&self) {
        self.transport_closed.wait().await
    }

    /// Non-blocking form of [`Self::transport_closed`].
    pub fn is_transport_closed(&self) -> bool {
        self.transport_closed.is_closed()
    }

    /// Send a method call against the currently active tab.
    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        self.dispatch(method, params, ResolvedTarget::Active).await
    }

    /// Send a method call against an explicitly named tab.
    pub async fn send_to(&self, target_id: &str, method: &str, params: Value) -> Result<Value> {
        self.dispatch(
            method,
            params,
            ResolvedTarget::Explicit(target_id.to_owned()),
        )
        .await
    }

    /// Send a browser-level (`Browser.*`) method call — no sessionId. `send()`
    /// routes through the active tab session, which the browser domain rejects;
    /// download behavior and other browser-scoped commands go here.
    pub async fn send_browser(&self, method: &str, params: Value) -> Result<Value> {
        self.dispatch(method, params, ResolvedTarget::Root).await
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
        self.pending.lock().unwrap().insert(id, tx);
        // From here, `_pending` removes the entry on every exit path — early
        // return, internal timeout, or the future being dropped by an outer
        // `tokio::time::timeout`. The reader removes it first on a normal reply;
        // a second remove is a harmless no-op.
        let _pending = PendingGuard {
            pending: self.pending.clone(),
            id,
        };

        let send_result = {
            let guard = self.out_tx.lock().await;
            match guard.as_ref() {
                Some(sender) => sender.send(text),
                None => return Err(anyhow!("cdp writer task closed")),
            }
        };
        if let Err(err) = send_result {
            return Err(anyhow::Error::new(err).context("cdp writer task closed"));
        }

        match tokio::time::timeout(Duration::from_millis(RPC_TIMEOUT_MS), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(anyhow!("cdp pending reply dropped")),
            Err(_) => Err(anyhow!(
                "cdp rpc '{method}' timed out after {RPC_TIMEOUT_MS}ms"
            )),
        }
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
            reg.tabs
                .insert(target_id.clone(), TabState::new(session_id));
        }

        self.send_to(&target_id, "Page.enable", json!({})).await?;
        self.send_to(&target_id, "Runtime.enable", json!({}))
            .await?;
        self.send_to(&target_id, "Network.enable", json!({}))
            .await?;

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

    /// Set the global dialog auto-response policy (applies to dialogs that
    /// open AFTER this call — see `DialogPolicy` for why it can't be live).
    pub async fn set_dialog_policy(&self, accept: bool, prompt_text: Option<String>) {
        let mut reg = self.registry.lock().await;
        reg.dialog_policy = DialogPolicy {
            accept,
            prompt_text,
        };
    }

    /// Record the download target dir on the registry so the reader can compute
    /// `saved_path` for incoming downloads. Call ONLY after
    /// `Browser.setDownloadBehavior` succeeds — otherwise the registry would
    /// claim downloads are enabled for a behavior chromium never accepted.
    pub async fn set_download_dir(&self, dir: std::path::PathBuf) {
        let mut reg = self.registry.lock().await;
        reg.download_dir = Some(dir);
    }

    /// Snapshot the download history (newest last).
    pub async fn downloads_snapshot(&self) -> Vec<DownloadEntry> {
        let reg = self.registry.lock().await;
        reg.downloads.clone()
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
    /// tear the transport down.
    ///
    /// The order matters. Dropping the outbound sender is not enough on its
    /// own (the reader holds a clone of it), and `abort()` skips the reader's
    /// own pending-map drain, so this fails the in-flight requests itself
    /// before aborting. Only once both tasks have been joined are the pipe
    /// descriptors released — which is the event the browser reacts to.
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
        drain_pending(&self.pending, "cdp client closed");
        if let Some(tasks) = self.tasks.lock().await.take() {
            tasks.shutdown().await;
        }
        self.transport_closed.set();
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

    fn dl(guid: &str, started: u64, state: &str) -> DownloadEntry {
        DownloadEntry {
            guid: guid.to_owned(),
            url: "http://x/f".to_owned(),
            suggested_filename: "f".to_owned(),
            state: state.to_owned(),
            total_bytes: None,
            received_bytes: 0,
            saved_path: format!("/dl/{guid}"),
            started_at: started,
        }
    }

    // -- Pipe frame accumulator --

    fn drain(acc: &mut FrameAccumulator, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        acc.push(chunk, MAX_PENDING_FRAME_BYTES, &mut out)
            .expect("under cap");
        out
    }

    #[test]
    fn accumulator_splits_several_frames_in_one_read() {
        let mut acc = FrameAccumulator::default();
        assert_eq!(
            drain(&mut acc, b"{\"id\":1}\0{\"id\":2}\0"),
            vec![r#"{"id":1}"#, r#"{"id":2}"#]
        );
    }

    #[test]
    fn accumulator_joins_a_frame_split_across_reads() {
        let mut acc = FrameAccumulator::default();
        assert!(drain(&mut acc, b"{\"id\"").is_empty());
        assert!(drain(&mut acc, b":1}").is_empty(), "no NUL yet");
        assert_eq!(drain(&mut acc, b"\0"), vec![r#"{"id":1}"#]);
    }

    #[test]
    fn accumulator_holds_a_trailing_partial_frame() {
        let mut acc = FrameAccumulator::default();
        assert_eq!(drain(&mut acc, b"{\"a\":1}\0{\"b\""), vec![r#"{"a":1}"#]);
        assert_eq!(drain(&mut acc, b":2}\0"), vec![r#"{"b":2}"#]);
    }

    #[test]
    fn accumulator_skips_empty_frames() {
        let mut acc = FrameAccumulator::default();
        assert_eq!(drain(&mut acc, b"\0\0{\"a\":1}\0\0"), vec![r#"{"a":1}"#]);
    }

    #[test]
    fn accumulator_survives_a_codepoint_split_across_reads() {
        // A multi-byte UTF-8 sequence cut in half by the read boundary must
        // not be decoded until the frame is complete.
        let mut acc = FrameAccumulator::default();
        let payload = "{\"t\":\"한\"}".as_bytes().to_vec();
        let (head, tail) = payload.split_at(6);
        assert!(drain(&mut acc, head).is_empty());
        let mut rest = tail.to_vec();
        rest.push(0);
        assert_eq!(drain(&mut acc, &rest), vec![r#"{"t":"한"}"#]);
    }

    #[test]
    fn accumulator_rejects_an_unterminated_frame_over_cap() {
        let mut acc = FrameAccumulator::default();
        let mut out = Vec::new();
        let err = acc.push(&[b'x'; 65], 64, &mut out).expect_err("over cap");
        assert!(err.to_string().contains("NUL"), "got: {err}");
        assert!(out.is_empty());
        // The buffer is cleared, so a later well-formed frame still parses.
        assert_eq!(drain(&mut acc, b"{\"a\":1}\0"), vec![r#"{"a":1}"#]);
    }

    #[test]
    fn accumulator_allows_a_frame_exactly_at_cap() {
        // The cap is on the *unterminated* remainder, so a frame whose payload
        // fills the cap and is then terminated must still come through. (This
        // one never reaches the cap check at all — the NUL drains the buffer
        // first — which is itself the property being asserted.)
        let mut acc = FrameAccumulator::default();
        let mut out = Vec::new();
        let mut chunk = vec![b'x'; 64];
        chunk.push(0);
        acc.push(&chunk, 64, &mut out).expect("terminated at cap");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 64);
    }

    #[test]
    fn accumulator_holds_an_unterminated_remainder_exactly_at_cap() {
        // The real boundary: `len() > max` errors, `len() == max` does not.
        // One more byte without a terminator must tip it over.
        let mut acc = FrameAccumulator::default();
        let mut out = Vec::new();
        acc.push(&[b'x'; 64], 64, &mut out)
            .expect("at cap, not over");
        assert!(out.is_empty(), "nothing is complete without a NUL");
        assert!(acc.push(b"x", 64, &mut out).is_err(), "65 bytes is over");
    }

    #[tokio::test]
    async fn dropping_transport_tasks_aborts_them() {
        // The failure this guards: a detached reader keeps its clone of the
        // outbound sender alive, which keeps the writer alive, which keeps the
        // pipe open — and a visual browser is then left running with nobody
        // owning it.
        let make = || tokio::spawn(std::future::pending::<()>());
        let (writer, reader) = (make(), make());
        let (w_probe, r_probe) = (writer.abort_handle(), reader.abort_handle());
        drop(TransportTasks::new(writer, reader));
        // `abort` is asynchronous; yield until the runtime has processed it.
        for _ in 0..100 {
            if w_probe.is_finished() && r_probe.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(w_probe.is_finished(), "writer task outlived its owner");
        assert!(r_probe.is_finished(), "reader task outlived its owner");
    }

    #[tokio::test]
    async fn shutdown_aborts_and_joins_both_tasks() {
        let make = || tokio::spawn(std::future::pending::<()>());
        let (writer, reader) = (make(), make());
        let (w_probe, r_probe) = (writer.abort_handle(), reader.abort_handle());
        // `shutdown` joins, so both tasks are finished by the time it returns
        // — no yielding needed, unlike the `Drop` path.
        TransportTasks::new(writer, reader).shutdown().await;
        assert!(w_probe.is_finished());
        assert!(r_probe.is_finished());
    }

    #[tokio::test]
    async fn shutdown_tolerates_already_emptied_slots() {
        let make = || tokio::spawn(std::future::pending::<()>());
        let mut tasks = TransportTasks::new(make(), make());
        tasks.writer.take().unwrap().abort();
        tasks.reader.take().unwrap().abort();
        tasks.shutdown().await; // must not panic
    }

    #[test]
    fn download_progress_updates_matching_guid() {
        let mut store = vec![dl("a", 1, "inProgress")];
        download_progress(&mut store, "a", "completed", Some(100), Some(100));
        assert_eq!(store[0].state, "completed");
        assert_eq!(store[0].total_bytes, Some(100));
        assert_eq!(store[0].received_bytes, 100);
        // Unknown guid is a no-op.
        download_progress(&mut store, "zzz", "canceled", None, None);
        assert_eq!(store[0].state, "completed");
    }

    #[test]
    fn download_begin_never_evicts_in_progress() {
        let mut store = Vec::new();
        // Fill past cap with in-progress entries — none may be evicted.
        for i in 0..5 {
            download_begin(&mut store, dl(&format!("p{i}"), i, "inProgress"), 3);
        }
        assert_eq!(store.len(), 5, "in-progress entries must not be evicted");
        // A terminal entry IS evictable; adding past cap drops the oldest
        // terminal one (front), keeping in-progress intact.
        let mut store2 = vec![
            dl("done0", 0, "completed"),
            dl("done1", 1, "completed"),
            dl("live", 2, "inProgress"),
        ];
        download_begin(&mut store2, dl("new", 3, "inProgress"), 3);
        assert!(
            !store2.iter().any(|e| e.guid == "done0"),
            "oldest terminal evicted"
        );
        assert!(store2.iter().any(|e| e.guid == "live"));
        assert!(store2.iter().any(|e| e.guid == "new"));
        assert_eq!(store2.len(), 3);
    }

    #[test]
    fn pending_guard_removes_entry_on_drop() {
        let pending: PendingMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (tx, _rx) = oneshot::channel::<Result<Value>>();
        pending.lock().unwrap().insert(7, tx);
        {
            let _guard = PendingGuard {
                pending: pending.clone(),
                id: 7,
            };
            assert!(pending.lock().unwrap().contains_key(&7));
        }
        // Guard dropped → entry gone, so a cancelled/timed-out dispatch can't leak.
        assert!(!pending.lock().unwrap().contains_key(&7));
        // A second remove (e.g. reader already handled the reply) is a no-op.
        assert!(pending.lock().unwrap().remove(&7).is_none());
    }

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
        let text = build_frame(
            7,
            "Page.navigate",
            json!({ "url": "https://x" }),
            Some("sess-1"),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["id"], json!(7));
        assert_eq!(parsed["method"], json!("Page.navigate"));
        assert_eq!(parsed["params"]["url"], json!("https://x"));
        assert_eq!(parsed["sessionId"], json!("sess-1"));
    }

    #[test]
    fn frame_omits_session_id_when_root() {
        let text = build_frame(
            1,
            "Target.createTarget",
            json!({ "url": "about:blank" }),
            None,
        )
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
        let raw =
            r#"{"method":"Runtime.consoleAPICalled","sessionId":"s1","params":{"type":"log"}}"#;
        let p: InboundFrame = serde_json::from_str(raw).unwrap();
        assert!(p.id.is_none());
        assert_eq!(p.method.as_deref(), Some("Runtime.consoleAPICalled"));
        assert_eq!(p.session_id.as_deref(), Some("s1"));
    }

    /// Visual-mode transport smoke: launch a real browser with
    /// `--remote-debugging-pipe` against a throwaway profile, drive it over
    /// the inherited descriptors, and confirm the V0 finding that closing the
    /// parent's ends is what stops the browser.
    ///
    /// Needs a graphical session (a visual launch is by definition not
    /// headless). Over ssh on Linux, export `WAYLAND_DISPLAY`, `DISPLAY`,
    /// `XDG_SESSION_TYPE=wayland`, `XDG_RUNTIME_DIR` and
    /// `DBUS_SESSION_BUS_ADDRESS` first, or the browser dies on startup.
    ///
    ///   cargo test --manifest-path crates/tabd/Cargo.toml -- --ignored visual_pipe
    #[tokio::test]
    #[ignore = "requires a real browser and a graphical session"]
    async fn visual_pipe_transport_roundtrip() {
        use crate::browser::{LaunchSpec, VisualSpec};

        let scratch = tempfile::TempDir::new().expect("tempdir");
        let profile_dir = scratch.path().join("profile");
        let mut browser = crate::browser::Browser::launch(LaunchSpec::Visual(VisualSpec {
            profile_dir: profile_dir.clone(),
            executable: crate::browser::discover_chromium().expect("a browser"),
            start_urls: Vec::new(),
            stderr_log: scratch.path().join("browser-stderr.log"),
        }))
        .await
        .expect("launch visual browser");

        let client = CdpClient::connect_with(ConnectOptions {
            transport: browser.take_transport().expect("pipe transport"),
            bootstrap_tab: false,
        })
        .await
        .expect("cdp connect over pipe");

        // Taking the transport twice must fail — the descriptors moved.
        assert!(browser.take_transport().is_err());

        // `bootstrap_tab: false` means we did not manufacture a tab. The
        // browser's own startup tab exists, but nothing is in OUR registry.
        assert!(
            client.registry.lock().await.tabs.is_empty(),
            "visual mode must start with an empty tab registry"
        );

        // A browser-domain round trip proves both directions of the pipe.
        let version = client
            .send_browser("Browser.getVersion", json!({}))
            .await
            .expect("Browser.getVersion");
        assert!(
            version.get("product").and_then(Value::as_str).is_some(),
            "got: {version:?}"
        );

        // A page session round trip proves frame routing, not just framing.
        let tab = client.create_tab("about:blank").await.expect("create_tab");
        // `send_to`, not `send`: with no bootstrap tab there is no active tab
        // for the `Active` route to resolve, which is exactly the point.
        assert!(
            client
                .send("Runtime.evaluate", json!({ "expression": "1" }))
                .await
                .is_err(),
            "the Active route must have nothing to resolve to"
        );
        let evaluated = client
            .send_to(
                &tab,
                "Runtime.evaluate",
                json!({ "expression": "1 + 1", "returnByValue": true }),
            )
            .await
            .expect("Runtime.evaluate");
        assert_eq!(
            evaluated.pointer("/result/value").and_then(Value::as_i64),
            Some(2)
        );
        client.close_tab(&tab).await.expect("close_tab");

        // V0 Q1: with every parent-side descriptor gone, the browser exits by
        // itself (75-100 ms on Linux, 876 ms on macOS). `close()` is what
        // releases them, by joining the transport tasks that own them.
        client.close().await.expect("close");
        assert!(client.is_transport_closed());
        assert!(
            browser.wait_for_exit(Duration::from_secs(10)).await,
            "browser must exit once the parent closes the pipe"
        );
    }

    // End-to-end smoke: spawn real chromium, exercise multi-tab paths.
    #[tokio::test]
    #[ignore = "requires real chromium; covers multi-tab create/activate/eval"]
    async fn cdp_multi_tab_roundtrip() {
        let mut browser = crate::browser::Browser::launch(crate::browser::LaunchSpec::Headless)
            .await
            .expect("launch chromium");
        let client = CdpClient::connect_with(ConnectOptions {
            transport: browser.take_transport().expect("transport"),
            bootstrap_tab: true,
        })
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
        let mut browser = crate::browser::Browser::launch(crate::browser::LaunchSpec::Headless)
            .await
            .expect("launch chromium");
        let client = CdpClient::connect_with(ConnectOptions {
            transport: browser.take_transport().expect("transport"),
            bootstrap_tab: true,
        })
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
