//! cookies.* + storage.* handlers.

use super::*;

pub(super) async fn handle_cookies_get(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let url = require_string(params, "url")?;
    let client = client_or_err(state).await?;
    let send_fut = client.send("Network.getCookies", json!({ "urls": [url] }));
    let resp = match tokio::time::timeout(Duration::from_secs(5), send_fut).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("Network.getCookies timed out after 5s".to_string()),
    };
    Ok(Some(
        resp.get("cookies").cloned().unwrap_or(Value::Array(vec![])),
    ))
}

pub(super) async fn handle_cookies_set(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let url = require_string(params, "url")?;
    let name = require_string(params, "name")?;
    let value = require_string(params, "value")?;
    let mut p = json!({ "url": url, "name": name, "value": value });
    for k in &["domain", "path"] {
        if let Some(v) = params.get(*k).and_then(Value::as_str) {
            p[*k] = Value::String(v.to_owned());
        }
    }
    for k in &["secure", "httpOnly"] {
        if let Some(v) = params.get(*k).and_then(Value::as_bool) {
            p[*k] = Value::Bool(v);
        }
    }
    if let Some(v) = params.get("expirationDate").and_then(Value::as_f64) {
        p["expires"] = json!(v);
    }
    let client = client_or_err(state).await?;
    let send_fut = client.send("Network.setCookie", p);
    let resp = match tokio::time::timeout(Duration::from_secs(5), send_fut).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("Network.setCookie timed out after 5s".to_string()),
    };
    let success = resp
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !success {
        return Err(format!("CDP rejected the cookie: {resp}"));
    }
    Ok(None)
}

pub(super) async fn handle_cookies_delete(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let url = require_string(params, "url")?;
    let name = require_string(params, "name")?;
    let client = client_or_err(state).await?;
    let send_fut = client.send("Network.deleteCookies", json!({ "url": url, "name": name }));
    match tokio::time::timeout(Duration::from_secs(5), send_fut).await {
        Ok(Ok(_)) => Ok(None),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("Network.deleteCookies timed out after 5s".to_string()),
    }
}

pub(super) async fn handle_storage_get(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let storage_type = if params.get("type").and_then(Value::as_str) == Some("session") {
        "sessionStorage"
    } else {
        "localStorage"
    };
    let client = client_or_err(state).await?;
    let expr = match params.get("key").and_then(Value::as_str) {
        Some(key) => {
            let key_lit = serde_json::to_string(key).unwrap();
            format!("{storage_type}.getItem({key_lit})")
        }
        None => format!(
            "Object.fromEntries(Object.keys({storage_type}).map((k) => [k, {storage_type}.getItem(k) ?? \"\"]))"
        ),
    };
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|opt| Some(opt.unwrap_or(Value::Null)))
        .map_err(|e| e.to_string())
}

pub(super) async fn handle_storage_set(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let key = require_string(params, "key")?;
    let value = require_string(params, "value")?;
    let storage_type = if params.get("type").and_then(Value::as_str) == Some("session") {
        "sessionStorage"
    } else {
        "localStorage"
    };
    let client = client_or_err(state).await?;
    let key_lit = serde_json::to_string(&key).unwrap();
    let value_lit = serde_json::to_string(&value).unwrap();
    let expr = format!("{storage_type}.setItem({key_lit}, {value_lit}); null;");
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|_| None)
        .map_err(|e| e.to_string())
}

pub(super) async fn handle_storage_clear(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let storage_type = if params.get("type").and_then(Value::as_str) == Some("session") {
        "sessionStorage"
    } else {
        "localStorage"
    };
    let client = client_or_err(state).await?;
    let expr = format!("{storage_type}.clear(); null;");
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|_| None)
        .map_err(|e| e.to_string())
}
