use anyhow::{Result, bail};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::browser::Browser;
use crate::cdp::CdpClient;

const READY_POLL: Duration = Duration::from_millis(200);

/// Launch chromium, attach CDP, navigate, and block until `document.readyState`
/// is `interactive` or `complete`. Mirrors `waitForReadyState` in the TS
/// `chromium-cdp` runtime (src/server/runtimes/cdp.ts:1914).
///
/// The brief race between Page.navigate's ack and the first readyState query
/// is shared with the TS implementation and is acceptable in spike scope.
pub async fn open(url: &str, timeout_ms: u64) -> Result<(Browser, CdpClient)> {
    let browser = Browser::launch().await?;
    let client = CdpClient::connect(browser.ws_endpoint()).await?;

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
                json!({ "expression": "document.readyState", "returnByValue": true }),
            )
            .await
            && let Some(state) = resp
                .get("result")
                .and_then(|r| r.get("value"))
                .and_then(|v| v.as_str())
            && (state == "interactive" || state == "complete")
        {
            return Ok((browser, client));
        }
        if Instant::now() >= deadline {
            bail!("navigate {url} timed out after {timeout_ms}ms waiting for readyState");
        }
        sleep(READY_POLL).await;
    }
}

pub async fn teardown(browser: Browser, client: CdpClient) -> Result<()> {
    client.close().await?;
    browser.shutdown().await
}
