//! emulation.* handlers (viewport, User-Agent, init scripts). Extracted from
//! capture.rs once a third handler arrived (Rule of Three).

use super::*;

/// `emulation.setViewport` — Emulation.setDeviceMetricsOverride on the tab's
/// session. Persists for the tab until the daemon (or chromium) restarts;
/// screenshots capture the emulated viewport.
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

/// `emulation.setUserAgent` — Network.setUserAgentOverride on the tab's
/// session. Persists for the tab until the daemon (or chromium) restarts.
/// Chromium's `--headless=new` still reports `HeadlessChrome/<ver>` in the
/// default UA on some builds, a common bot-detection signal; this lets
/// callers replace it (e.g. with the equivalent non-headless `Chrome/<ver>`
/// string) before navigating.
pub(super) async fn handle_set_user_agent(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let user_agent = require_string(params, "userAgent")?;
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;
    client
        .send_to(
            &tid,
            "Network.setUserAgentOverride",
            json!({ "userAgent": user_agent }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(json!({ "userAgent": user_agent })))
}

/// `emulation.addInitScript` — Page.addScriptToEvaluateOnNewDocument on the
/// tab's session. Unlike a plain `eval` (which only runs after the current
/// document has already loaded and executed its own scripts), this runs
/// BEFORE every subsequent navigation's scripts do — the only way to patch
/// automation tells like `navigator.webdriver` (which Chromium sets to
/// `true` on the `Navigator` prototype as soon as CDP attaches, regardless
/// of the `--headless` flag or User-Agent) before a page's own fraud/bot
/// detection can read it. Applies to future navigations on this tab, not
/// retroactively to the current document — call before navigating anywhere
/// that matters.
pub(super) async fn handle_add_init_script(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let source = require_string(params, "source")?;
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;
    let resp = client
        .send_to(
            &tid,
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": source }),
        )
        .await
        .map_err(|e| e.to_string())?;
    let identifier = resp
        .get("identifier")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "Page.addScriptToEvaluateOnNewDocument response missing 'identifier'".to_string()
        })?
        .to_owned();
    Ok(Some(json!({ "identifier": identifier })))
}
