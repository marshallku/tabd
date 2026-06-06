//! secrets.* + interaction.typeSecret handlers backed by the AES vault.

use super::*;

async fn vault_or_err(
    state: &DaemonState,
) -> Result<tokio::sync::MutexGuard<'_, Option<crate::secrets::VaultStore>>, String> {
    let mut guard = state.vault.lock().await;
    if guard.is_none() {
        let passphrase = std::env::var("TABD_VAULT_KEY")
            .map_err(|_| "TABD_VAULT_KEY env not set; secrets unavailable".to_string())?;
        let store = crate::secrets::VaultStore::open_or_create(&passphrase)
            .map_err(|e| format!("vault open failed: {e}"))?;
        *guard = Some(store);
    }
    Ok(guard)
}

pub(super) async fn handle_secrets_put(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let value = require_string(params, "value")?;
    let label = params
        .get("label")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut guard = vault_or_err(state).await?;
    let store = guard
        .as_mut()
        .ok_or_else(|| "vault not initialized".to_string())?;
    let resp = store
        .put(&value, label.as_deref())
        .map_err(|e| e.to_string())?;
    let json = serde_json::to_value(resp).map_err(|e| e.to_string())?;
    Ok(Some(json))
}

pub(super) async fn handle_secrets_list(
    state: &DaemonState,
    _params: &Value,
) -> Result<Option<Value>, String> {
    let guard = vault_or_err(state).await?;
    let store = guard
        .as_ref()
        .ok_or_else(|| "vault not initialized".to_string())?;
    let list = store.list();
    let json = serde_json::to_value(list).map_err(|e| e.to_string())?;
    Ok(Some(json))
}

pub(super) async fn handle_secrets_delete(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let id = params
        .get("id")
        .or_else(|| params.get("secretId"))
        .and_then(Value::as_str)
        .ok_or_else(|| "id is required".to_string())?
        .to_owned();
    let mut guard = vault_or_err(state).await?;
    let store = guard
        .as_mut()
        .ok_or_else(|| "vault not initialized".to_string())?;
    store.delete(&id).map_err(|e| e.to_string())?;
    Ok(Some(Value::Null))
}

pub(super) async fn handle_type_secret(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let selector = require_string(params, "selector")?;
    let secret_id = params
        .get("secretId")
        .and_then(Value::as_str)
        .ok_or_else(|| "secretId is required".to_string())?
        .to_owned();
    let clear = params.get("clear").and_then(Value::as_bool).unwrap_or(true);

    let plaintext = {
        let guard = vault_or_err(state).await?;
        let store = guard
            .as_ref()
            .ok_or_else(|| "vault not initialized".to_string())?;
        store.get(&secret_id).map_err(|e| e.to_string())?
    };

    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;
    wait_for_selector_visible(&client, &selector, 30_000).await?;

    let sel_lit = serde_json::to_string(&selector).map_err(|e| e.to_string())?;
    let value_lit = serde_json::to_string(&plaintext).map_err(|e| e.to_string())?;
    let clear_lit = if clear { "true" } else { "false" };
    let expr = format!(
        r#"
        (() => {{
            const el = document.querySelector({sel_lit});
            if (!el) throw new Error("type-secret: selector miss");
            const editable = (el instanceof HTMLInputElement)
                || (el instanceof HTMLTextAreaElement)
                || (el instanceof HTMLElement && el.isContentEditable);
            if (!editable) throw new Error("type-secret: element is not editable");
            el.scrollIntoView({{ block: "center", inline: "center" }});
            el.focus();
            const value = {value_lit};
            if ({clear_lit}) {{
                if ("value" in el) el.value = "";
                else el.textContent = "";
            }}
            if ("value" in el) el.value = value;
            else document.execCommand("insertText", false, value);
            el.dispatchEvent(new Event("input", {{ bubbles: true }}));
            el.dispatchEvent(new Event("change", {{ bubbles: true }}));
            return null;
        }})()
        "#
    );
    client
        .send_to(
            &tid,
            "Runtime.evaluate",
            json!({"expression": expr, "returnByValue": true}),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(Value::Null))
}
