//! tabs.* handlers (multi-tab list/open/close/activate/history/reload).

use super::*;

pub(super) async fn handle_tabs_list(
    state: &DaemonState,
    _params: &Value,
) -> Result<Option<Value>, String> {
    let client = client_or_err(state).await?;
    let tabs = client.list_tabs().await.map_err(|e| e.to_string())?;
    let arr: Vec<Value> = tabs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            json!({
                "tabId": i + 1,
                "targetId": t.target_id,
                "title": t.title,
                "url": t.url,
                "active": t.active,
            })
        })
        .collect();
    Ok(Some(Value::Array(arr)))
}

pub(super) async fn handle_tabs_open(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let url = require_string(params, "url")?;
    let client = client_or_err(state).await?;
    let new_tid = client.create_tab(&url).await.map_err(|e| e.to_string())?;
    // Registry-only flip — no Target.activateTarget RPC (TS parity).
    client
        .set_active(&new_tid)
        .await
        .map_err(|e| e.to_string())?;
    let tabs = client.list_tabs().await.map_err(|e| e.to_string())?;
    let tab_id = tabs
        .iter()
        .enumerate()
        .find(|(_, t)| t.target_id == new_tid)
        .map(|(i, _)| i + 1)
        .unwrap_or(1); // dead path — TS does the same `?? 1`.
    Ok(Some(json!({
        "tabId": tab_id,
        "targetId": new_tid,
        "url": url,
    })))
}

pub(super) async fn handle_tabs_close(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let tab_id = require_tab_id(params)?;
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, Some(tab_id)).await?;
    client.close_tab(&tid).await.map_err(|e| e.to_string())?;
    Ok(Some(Value::Null))
}

pub(super) async fn handle_tabs_activate(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let tab_id = require_tab_id(params)?;
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, Some(tab_id)).await?;
    client.activate_tab(&tid).await.map_err(|e| e.to_string())?;
    Ok(Some(Value::Null))
}

pub(super) async fn handle_tabs_history(
    state: &DaemonState,
    params: &Value,
    expr: &str,
) -> Result<Option<Value>, String> {
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;
    client
        .send_to(
            &tid,
            "Runtime.evaluate",
            json!({
                "expression": expr,
                "returnByValue": true,
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(Value::Null))
}

pub(super) async fn handle_tabs_reload(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;
    client
        .send_to(&tid, "Page.reload", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(Value::Null))
}
