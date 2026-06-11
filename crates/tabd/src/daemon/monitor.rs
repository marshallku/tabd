//! monitor.* handlers (console logs, page errors, network logs).

use super::*;

pub(super) async fn handle_console_logs(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let level_filter = params
        .get("level")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;

    let entries = client
        .read_tab_state(&tid, |state| {
            let mut filtered: Vec<crate::cdp::ConsoleEntry> = match &level_filter {
                Some(l) if l != "all" && !l.is_empty() => state
                    .console_logs
                    .iter()
                    .filter(|e| e.level == *l)
                    .cloned()
                    .collect(),
                _ => state.console_logs.clone(),
            };
            if filtered.len() > limit {
                let excess = filtered.len() - limit;
                filtered.drain(0..excess);
            }
            filtered
        })
        .await?;
    let json = serde_json::to_value(entries).map_err(|e| e.to_string())?;
    Ok(Some(json))
}

pub(super) async fn handle_page_errors(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;

    let entries = client
        .read_tab_state(&tid, |state| {
            let mut copy = state.page_errors.clone();
            if copy.len() > limit {
                let excess = copy.len() - limit;
                copy.drain(0..excess);
            }
            copy
        })
        .await?;
    let json = serde_json::to_value(entries).map_err(|e| e.to_string())?;
    Ok(Some(json))
}

pub(super) async fn handle_dialogs(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;

    let entries = client
        .read_tab_state(&tid, |state| {
            let mut copy = state.dialogs.clone();
            if copy.len() > limit {
                let excess = copy.len() - limit;
                copy.drain(0..excess);
            }
            copy
        })
        .await?;
    let json = serde_json::to_value(entries).map_err(|e| e.to_string())?;
    Ok(Some(json))
}

/// Parse the `action` param into the policy's accept flag.
fn parse_dialog_action(action: &str) -> Result<bool, String> {
    match action {
        "accept" => Ok(true),
        "dismiss" => Ok(false),
        other => Err(format!(
            "invalid 'action' (expected accept|dismiss, got '{other}')"
        )),
    }
}

/// Pre-configures how FUTURE dialogs are auto-answered. It cannot rescue an
/// already-open dialog — that action holds the global action mutex, which is
/// exactly why the event reader auto-handles dialogs in the first place.
pub(super) async fn handle_set_dialog_policy(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let action = require_string(params, "action")?;
    let accept = parse_dialog_action(&action)?;
    let prompt_text = params
        .get("promptText")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let client = client_or_err(state).await?;
    client.set_dialog_policy(accept, prompt_text.clone()).await;
    Ok(Some(json!({ "action": action, "promptText": prompt_text })))
}

pub(super) async fn handle_network_logs(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
    let method_filter = params
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let status_filter: Option<(Option<u16>, Option<u16>)> = match params.get("status") {
        Some(Value::Number(n)) => n.as_u64().map(|v| (Some(v as u16), Some(v as u16))),
        Some(Value::String(s)) => {
            // "2xx" → (200, 299); fall through to None (no filter) on parse failure.
            if let Some(first) = s.chars().next().and_then(|c| c.to_digit(10)) {
                let lo = (first * 100) as u16;
                let hi = lo + 99;
                Some((Some(lo), Some(hi)))
            } else {
                None
            }
        }
        _ => None,
    };
    let url_pattern_re = match params.get("urlPattern").and_then(Value::as_str) {
        Some(p) => Some(regex::Regex::new(p).map_err(|e| e.to_string())?),
        None => None,
    };
    let include_body = params
        .get("includeBody")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;
    let entries = client
        .read_tab_state(&tid, |state| state.network_log.clone())
        .await?;

    let mut filtered: Vec<crate::cdp::NetworkEntry> = entries
        .into_iter()
        .filter(|e| match &method_filter {
            Some(m) => e.method.to_ascii_lowercase() == *m,
            None => true,
        })
        .filter(|e| match status_filter {
            Some((Some(lo), Some(hi))) => e.status.map(|s| s >= lo && s <= hi).unwrap_or(false),
            _ => true,
        })
        .filter(|e| match &url_pattern_re {
            Some(re) => re.is_match(&e.url),
            None => true,
        })
        .collect();

    if !include_body {
        for entry in filtered.iter_mut() {
            entry.response_body = None;
        }
    }

    if filtered.len() > limit {
        let excess = filtered.len() - limit;
        filtered.drain(0..excess);
    }

    let json = serde_json::to_value(filtered).map_err(|e| e.to_string())?;
    Ok(Some(json))
}

#[cfg(test)]
mod tests {
    use super::parse_dialog_action;

    #[test]
    fn parse_dialog_action_accept_dismiss() {
        assert_eq!(parse_dialog_action("accept"), Ok(true));
        assert_eq!(parse_dialog_action("dismiss"), Ok(false));
    }

    #[test]
    fn parse_dialog_action_rejects_unknown_as_invalid_request() {
        let err = parse_dialog_action("maybe").unwrap_err();
        assert!(err.contains("expected accept|dismiss"), "got: {err}");
        // Wording is load-bearing: the classifier maps it to invalid_request.
        assert_eq!(
            crate::daemon::error::classify_error_code(&err),
            crate::daemon::error::ErrorCode::InvalidRequest
        );
    }
}
