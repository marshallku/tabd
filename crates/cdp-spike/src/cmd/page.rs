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
    let client = match CdpClient::connect(browser.ws_endpoint()).await {
        Ok(c) => c,
        Err(err) => {
            // Browser launched but CDP attach failed → run the documented
            // teardown explicitly rather than leaning on Drop+kill_on_drop.
            let _ = browser.shutdown().await;
            return Err(err);
        }
    };

    match navigate_and_wait(&client, url, timeout_ms).await {
        Ok(()) => Ok((browser, client)),
        Err(err) => {
            // Same here: teardown the well-formed handles so callers get a
            // clean state even when navigation itself fails or times out.
            let _ = client.close().await;
            let _ = browser.shutdown().await;
            Err(err)
        }
    }
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

pub async fn teardown(browser: Browser, client: CdpClient) -> Result<()> {
    client.close().await?;
    browser.shutdown().await
}
