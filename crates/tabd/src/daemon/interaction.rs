//! interaction.* handlers (click, type, hover, scroll, keys, select, check).

use super::*;

pub(super) async fn handle_click(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let timeout_ms = clamped_wait_ms(params, 30_000);
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, timeout_ms).await?;
    let sel_lit = serde_json::to_string(&selector).unwrap();
    let expr = format!(
        "(() => {{
    const el = document.querySelector({sel_lit});
    if (!el) throw new Error('Selector not found: ' + {sel_lit});
    el.click();
    return {{ ok: true }};
}})()"
    );
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map_err(|e| e.to_string())
}

pub(super) async fn handle_type(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let text = require_string(params, "text")?;
    let timeout_ms = clamped_wait_ms(params, 30_000);
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, timeout_ms).await?;
    let sel_lit = serde_json::to_string(&selector).unwrap();
    let text_lit = serde_json::to_string(&text).unwrap();
    // JS-based type (spike scope) — sets .value + fires input/change events.
    // Plain HTML forms work; some React/Vue controlled inputs may need the
    // native setter trick, which is phase 2c (real CDP Input.dispatchKeyEvent).
    let expr = format!(
        "(() => {{
    const el = document.querySelector({sel_lit});
    if (!el) throw new Error('Selector not found: ' + {sel_lit});
    el.focus();
    el.value = {text_lit};
    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
    return {{ ok: true }};
}})()"
    );
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map_err(|e| e.to_string())
}

pub(super) async fn handle_hover(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let offset_x = params.get("x").and_then(Value::as_f64);
    let offset_y = params.get("y").and_then(Value::as_f64);
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, 30_000).await?;

    let sel_lit = serde_json::to_string(&selector).map_err(|e| e.to_string())?;
    let ox = offset_x
        .map(|n| n.to_string())
        .unwrap_or_else(|| "null".to_string());
    let oy = offset_y
        .map(|n| n.to_string())
        .unwrap_or_else(|| "null".to_string());
    let expr = format!(
        r#"
        (() => {{
            const el = document.querySelector({sel_lit});
            if (!el) throw new Error("hover: selector miss");
            const r = el.getBoundingClientRect();
            const ox = {ox};
            const oy = {oy};
            return [r.x + (ox !== null ? ox : r.width / 2), r.y + (oy !== null ? oy : r.height / 2)];
        }})()
        "#
    );
    let rect = crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "hover: rect computation returned undefined".to_string())?;
    let arr = rect
        .as_array()
        .ok_or_else(|| "hover: rect not an array".to_string())?;
    let x = arr
        .first()
        .and_then(Value::as_f64)
        .ok_or_else(|| "hover: missing x".to_string())?;
    let y = arr
        .get(1)
        .and_then(Value::as_f64)
        .ok_or_else(|| "hover: missing y".to_string())?;

    client
        .send(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y, "button": "none"}),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(Value::Null))
}

pub(super) async fn handle_mouse_move(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let x = params
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| "x is required".to_string())?;
    let y = params
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| "y is required".to_string())?;
    let client = client_or_err(state).await?;
    client
        .send(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y, "button": "none"}),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(Value::Null))
}

pub(super) async fn handle_scroll(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = params.get("selector").and_then(Value::as_str);
    let x = params.get("x").and_then(Value::as_f64).unwrap_or(0.0);
    let y = params.get("y").and_then(Value::as_f64).unwrap_or(0.0);
    let client = client_or_err(state).await?;
    let expr = if let Some(sel) = selector {
        let sel_lit = serde_json::to_string(sel).map_err(|e| e.to_string())?;
        format!(
            r#"
            (() => {{
                const el = document.querySelector({sel_lit});
                if (!el) throw new Error("scroll: selector miss");
                el.scrollIntoView({{ block: "center", inline: "center" }});
                return null;
            }})()
            "#
        )
    } else {
        format!("window.scrollBy({x}, {y}); null")
    };
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(Value::Null))
}

pub(super) async fn handle_press_key(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let key = require_string(params, "key")?;
    let selector = params
        .get("selector")
        .and_then(Value::as_str)
        .map(str::to_string);
    let client = client_or_err(state).await?;
    if let Some(sel) = selector {
        let sel_lit = serde_json::to_string(&sel).map_err(|e| e.to_string())?;
        let focus_expr = format!(
            r#"
            (() => {{
                const el = document.querySelector({sel_lit});
                if (el && typeof el.focus === "function") el.focus();
                return null;
            }})()
            "#
        );
        crate::cmd::eval::evaluate_value(&client, &focus_expr)
            .await
            .map_err(|e| e.to_string())?;
    }
    let kd = resolve_key(&key);
    let mut down = json!({
        "type": "keyDown",
        "key": kd.key,
        "code": kd.code,
        "windowsVirtualKeyCode": kd.key_code,
        "nativeVirtualKeyCode": kd.key_code,
    });
    if let Some(text) = &kd.text {
        down["text"] = json!(text);
    }
    let up = json!({
        "type": "keyUp",
        "key": kd.key,
        "code": kd.code,
        "windowsVirtualKeyCode": kd.key_code,
        "nativeVirtualKeyCode": kd.key_code,
    });
    client
        .send("Input.dispatchKeyEvent", down)
        .await
        .map_err(|e| e.to_string())?;
    client
        .send("Input.dispatchKeyEvent", up)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(Value::Null))
}

pub(super) async fn handle_select_option(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, 30_000).await?;

    let value_lit = params
        .get("value")
        .and_then(Value::as_str)
        .map(|s| serde_json::to_string(s).unwrap())
        .unwrap_or_else(|| "null".to_string());
    let label_lit = params
        .get("label")
        .and_then(Value::as_str)
        .map(|s| serde_json::to_string(s).unwrap())
        .unwrap_or_else(|| "null".to_string());
    let index_lit = params
        .get("index")
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "null".to_string());
    let sel_lit = serde_json::to_string(&selector).map_err(|e| e.to_string())?;
    let expr = format!(
        r#"
        (() => {{
            const sel = document.querySelector({sel_lit});
            if (!sel || sel.tagName !== "SELECT") throw new Error("selectOption: not a SELECT");
            const targetValue = {value_lit};
            const targetLabel = {label_lit};
            const targetIndex = {index_lit};
            const opts = Array.from(sel.options);
            let chosen = null;
            if (targetValue !== null) chosen = opts.find(o => o.value === targetValue);
            else if (targetLabel !== null) chosen = opts.find(o => o.label === targetLabel);
            else if (targetIndex !== null) chosen = opts[targetIndex];
            if (!chosen) throw new Error("Requested option was not found");
            sel.value = chosen.value;
            sel.dispatchEvent(new Event("input", {{ bubbles: true }}));
            sel.dispatchEvent(new Event("change", {{ bubbles: true }}));
            return null;
        }})()
        "#
    );
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(Value::Null))
}

pub(super) async fn handle_check(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let checked = params
        .get("checked")
        .and_then(Value::as_bool)
        .unwrap_or(true); // TS: checked !== false
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, 30_000).await?;
    let sel_lit = serde_json::to_string(&selector).map_err(|e| e.to_string())?;
    let expr = format!(
        r#"
        (() => {{
            const el = document.querySelector({sel_lit});
            if (!el) throw new Error("check: selector miss");
            el.checked = {checked};
            el.dispatchEvent(new Event("input", {{ bubbles: true }}));
            el.dispatchEvent(new Event("change", {{ bubbles: true }}));
            return null;
        }})()
        "#
    );
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(Value::Null))
}
