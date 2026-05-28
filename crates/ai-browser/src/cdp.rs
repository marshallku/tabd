use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// CDP JSON-RPC client. Attaches to a fresh page target with flatten mode,
/// auto-routes responses by id, and exposes `send()` for method calls on
/// the attached session (sessionId is added automatically).
///
/// `close(&self)` is idempotent — drops the writer mpsc sender so background
/// reader/writer tasks exit naturally (sink close → reader EOF). No JoinHandle
/// tracking; tasks are expected to terminate on process exit if not before.
pub struct CdpClient {
    next_id: AtomicU64,
    // Mutex<Option<Sender>> so close() can take + drop the sender. Subsequent
    // dispatch attempts see None and return a clear error.
    out_tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    session_id: String,
}

#[derive(Deserialize, Debug)]
struct InboundFrame {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<Value>,
    // method/params present on events; not used by spike but kept for forward-compat.
    #[allow(dead_code)]
    method: Option<String>,
    #[allow(dead_code)]
    params: Option<Value>,
}

impl CdpClient {
    /// Connect, create a new page target, flatten-attach, and enable Page + Runtime.
    /// Returns a client ready for method calls scoped to the new session.
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let (ws, _resp) = connect_async(ws_url)
            .await
            .with_context(|| format!("ws connect: {ws_url}"))?;
        let (mut sink, mut stream) = ws.split();

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_for_reader = pending.clone();

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
                }
                // Events (no `id`) ignored — spike scope does not need event subscription.
            }
            // Stream closed → fail any still-pending requests so callers don't hang.
            let mut map = pending_for_reader.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(anyhow!("cdp websocket closed")));
            }
        });

        let mut client = CdpClient {
            next_id: AtomicU64::new(1),
            out_tx: Mutex::new(Some(out_tx)),
            pending,
            session_id: String::new(),
        };

        // 1. Fresh page target (about:blank — real URL comes via Page.navigate later).
        let target = client
            .call_root("Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        let target_id = target
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Target.createTarget missing targetId: {target:?}"))?
            .to_owned();

        // 2. Flatten attach (sessionId arrives in the response, not as an event).
        let attach = client
            .call_root(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        client.session_id = attach
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Target.attachToTarget missing sessionId: {attach:?}"))?
            .to_owned();

        // 3. Enable domains on the session. Network domain is required for
        // cookies.* actions to receive responses on some chromium builds
        // (phase 2d found Network.setCookie hanging without it).
        client.send("Page.enable", json!({})).await?;
        client.send("Runtime.enable", json!({})).await?;
        client.send("Network.enable", json!({})).await?;

        Ok(client)
    }

    /// Send a method call against the attached session.
    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        self.dispatch(method, params, Some(&self.session_id)).await
    }

    /// Root-scoped (no sessionId) call. Used for Target.* during bootstrap.
    async fn call_root(&self, method: &str, params: Value) -> Result<Value> {
        self.dispatch(method, params, None).await
    }

    async fn dispatch(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let text = build_frame(id, method, params, session_id)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        // Grab the sender under lock. If close() already took it, fail fast
        // and reclaim the pending entry.
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

        rx.await
            .map_err(|_| anyhow!("cdp pending reply dropped"))?
    }

    /// Drop the writer mpsc sender. Background tasks (writer/reader) exit
    /// naturally on sender drop → mpsc closed → sink close → reader EOF.
    /// Idempotent — calling close twice is safe.
    pub async fn close(&self) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn inbound_parses_event_without_id() {
        let raw = r#"{"method":"Page.loadEventFired","params":{"timestamp":12.3}}"#;
        let p: InboundFrame = serde_json::from_str(raw).unwrap();
        assert!(p.id.is_none());
        assert_eq!(p.method.as_deref(), Some("Page.loadEventFired"));
    }

    // End-to-end smoke: spawn real chromium, connect over CDP, run an evaluate.
    // Validates Target.createTarget → flatten attach → Page/Runtime.enable wiring.
    #[tokio::test]
    #[ignore = "requires real chromium; covers cdp client attach + Runtime.evaluate"]
    async fn cdp_evaluate_roundtrip() {
        let browser = crate::browser::Browser::launch()
            .await
            .expect("launch chromium");
        let client = CdpClient::connect(browser.ws_endpoint())
            .await
            .expect("cdp connect");

        let result = client
            .send(
                "Runtime.evaluate",
                json!({ "expression": "1 + 1", "returnByValue": true }),
            )
            .await
            .expect("evaluate");
        assert_eq!(result["result"]["value"], json!(2), "got: {result:?}");

        client.close().await.expect("cdp close");
        browser.shutdown().await.expect("browser shutdown");
    }
}
