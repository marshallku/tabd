//! capture.* handlers (screenshot + perf metrics).

use super::*;

pub(super) async fn handle_screenshot(
    state: &DaemonState,
    _params: &Value,
) -> Result<Option<Value>, String> {
    let client = client_or_err(state).await?;
    let send_fut = client.send("Page.captureScreenshot", json!({ "format": "png" }));
    let resp = match tokio::time::timeout(Duration::from_secs(10), send_fut).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("Page.captureScreenshot timed out after 10s".to_string()),
    };
    let data = resp
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| "Page.captureScreenshot response missing 'data'".to_string())?;
    Ok(Some(Value::String(format!("data:image/png;base64,{data}"))))
}

pub(super) async fn handle_metrics(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;
    // Pure JS — no Performance.enable RPC needed (TS chromium-cdp identical).
    let code = r#"
        (() => {
            const nav = performance.getEntriesByType("navigation")[0];
            return {
                url: location.href,
                title: document.title,
                readyState: document.readyState,
                domNodes: document.getElementsByTagName("*").length,
                resources: performance.getEntriesByType("resource").length,
                navigation: nav ? {
                    type: nav.type,
                    domContentLoaded: nav.domContentLoadedEventEnd,
                    loadEventEnd: nav.loadEventEnd,
                } : null,
            };
        })()
    "#;
    let resp = client
        .send_to(
            &tid,
            "Runtime.evaluate",
            json!({"expression": code, "returnByValue": true}),
        )
        .await
        .map_err(|e| e.to_string())?;
    let value = resp
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(Some(value))
}
