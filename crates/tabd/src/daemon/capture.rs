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

/// `emulation.setViewport` — Emulation.setDeviceMetricsOverride on the tab's
/// session. Persists for the tab until the daemon (or chromium) restarts;
/// screenshots capture the emulated viewport. Lives here rather than a new
/// emulation module (one handler — Rule of Three).
pub(super) async fn handle_set_viewport(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let width = loose_u64(params, "width").ok_or_else(|| "missing 'width' (number)".to_string())?;
    let height =
        loose_u64(params, "height").ok_or_else(|| "missing 'height' (number)".to_string())?;
    if width == 0 || height == 0 {
        return Err("invalid 'width'/'height' (must be >= 1)".to_string());
    }
    let scale = loose_f64(params, "scale").unwrap_or(1.0);
    let mobile = params
        .get("mobile")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;
    client
        .send_to(
            &tid,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": width,
                "height": height,
                "deviceScaleFactor": scale,
                "mobile": mobile,
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(json!({
        "width": width,
        "height": height,
        "scale": scale,
        "mobile": mobile,
    })))
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
