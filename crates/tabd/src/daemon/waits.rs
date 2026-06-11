//! wait.* handlers (selector visible, URL match, network idle).

use super::*;

pub(super) async fn handle_wait_selector(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let timeout_ms = clamped_wait_ms(params, 30_000);
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, timeout_ms).await?;
    Ok(Some(json!({ "found": true })))
}

pub(super) async fn handle_wait_text(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let text = require_string(params, "text")?;
    let selector = params
        .get("selector")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let timeout_ms = clamped_wait_ms(params, 30_000);
    let client = client_or_err(state).await?;

    let text_lit = serde_json::to_string(&text).map_err(|e| e.to_string())?;
    // Missing scope keeps polling (it may mount later) — expiry then lands on
    // the same `timeout` classification as a never-appearing text. Conscious
    // tradeoff: an invalid selector is indistinguishable from slow content.
    let scope_expr = match &selector {
        Some(sel) => {
            let sel_lit = serde_json::to_string(sel).map_err(|e| e.to_string())?;
            format!("document.querySelector({sel_lit})")
        }
        None => "document.body".to_string(),
    };
    let probe = format!(
        "(() => {{
    const el = {scope_expr};
    if (!el) return false;
    return (el.innerText || el.textContent || \"\").includes({text_lit});
}})()"
    );
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Ok(Some(Value::Bool(true))) = crate::cmd::eval::evaluate_value(&client, &probe).await
        {
            return Ok(Some(json!({ "found": true })));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "wait-text timed out after {timeout_ms}ms (text={text})"
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub(super) async fn handle_wait_url(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let pattern = require_string(params, "pattern")?;
    let pattern_type = params
        .get("patternType")
        .and_then(Value::as_str)
        .unwrap_or("exact")
        .to_owned();
    let timeout_ms = clamped_wait_ms(params, 30_000);
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

pub(super) async fn handle_wait_network_idle(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let timeout_ms = clamped_wait_ms(params, 10_000);
    let idle_ms = params
        .get("idleTime")
        .and_then(Value::as_u64)
        .unwrap_or(500);
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut idle_since: Option<Instant> = None;
    loop {
        let pending = client
            .read_tab_state(&tid, |state| state.network_pending)
            .await?;
        if pending == 0 {
            let mark = idle_since.get_or_insert_with(Instant::now);
            if mark.elapsed() >= Duration::from_millis(idle_ms) {
                return Ok(Some(Value::Null));
            }
        } else {
            idle_since = None;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Timed out waiting for network idle ({pending} pending requests)"
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
