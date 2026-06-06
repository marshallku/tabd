use anyhow::{Result, bail};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::cdp::CdpClient;

const READY_POLL: Duration = Duration::from_millis(200);

/// Navigate using an already-connected CdpClient (no browser launch) and wait
/// for readyState. Used by the daemon (one Chromium reused across requests).
pub async fn navigate_existing(client: &CdpClient, url: &str, timeout_ms: u64) -> Result<()> {
    navigate_and_wait(client, url, timeout_ms).await
}

async fn navigate_and_wait(client: &CdpClient, url: &str, timeout_ms: u64) -> Result<()> {
    let nav = client.send("Page.navigate", json!({ "url": url })).await?;
    if let Some(err) = nav.get("errorText").and_then(|v| v.as_str())
        && !err.is_empty()
    {
        bail!("Page.navigate failed: {err}");
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        // Mirror TS waitForReadyState (src/server/runtimes/cdp.ts:1925): swallow
        // transient evaluate errors during navigation (e.g. context destroyed
        // while the new document is loading) and keep polling until deadline.
        if let Ok(resp) = client
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": "document.readyState",
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await
            && let Some(state) = resp
                .get("result")
                .and_then(|r| r.get("value"))
                .and_then(|v| v.as_str())
            && (state == "interactive" || state == "complete")
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("navigate {url} timed out after {timeout_ms}ms waiting for readyState");
        }
        sleep(READY_POLL).await;
    }
}
