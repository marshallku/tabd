//! interaction.* handlers (click, type, hover, scroll, keys, select, check).

use super::*;

/// Default candidate set for `click --text` when no selector narrows the scope.
const CLICKABLE_SELECTOR: &str = "a,button,[role=\"button\"],input[type=\"button\"],input[type=\"submit\"],label,summary,[onclick]";

pub(super) async fn handle_click(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    // Path split happens before any require_* so `click --text` works without
    // a selector (codex unit-5 plan C4).
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    let selector = params
        .get("selector")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let timeout_ms = clamped_wait_ms(params, 30_000);
    let frame = frame_param(params);
    let client = client_or_err(state).await?;

    let Some(text) = text else {
        let Some(selector) = selector else {
            return Err("'selector' or 'text' is required".to_owned());
        };
        wait_for_selector_visible(&client, &selector, timeout_ms, frame.as_deref()).await?;
        let sel_lit = serde_json::to_string(&selector).unwrap();
        let prelude = doc_prelude(frame.as_deref())?;
        let expr = format!(
            "(() => {{
    {prelude}
    const el = __doc.querySelector({sel_lit});
    if (!el) throw new Error('Selector not found: ' + {sel_lit});
    el.click();
    return {{ ok: true }};
}})()"
        );
        return crate::cmd::eval::evaluate_value(&client, &expr)
            .await
            .map_err(|e| e.to_string());
    };

    // Text path: find-and-click in one evaluation (no TOCTOU between probe
    // and click), polled until the deadline like the selector wait.
    let scope = selector.as_deref().unwrap_or(CLICKABLE_SELECTOR);
    let scope_lit = serde_json::to_string(scope).map_err(|e| e.to_string())?;
    let text_lit = serde_json::to_string(&text.to_lowercase()).map_err(|e| e.to_string())?;
    let prelude = doc_prelude(frame.as_deref())?;
    let probe = format!(
        r#"(() => {{
    {prelude}
    const wanted = {text_lit};
    const labelOf = (el) =>
        ((el.innerText || "").trim()
            || el.value
            || el.getAttribute("aria-label")
            || "").trim();
    const candidates = [...__doc.querySelectorAll({scope_lit})]
        .filter((el) => {{
            const rect = el.getBoundingClientRect();
            const style = getComputedStyle(el);
            return rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none";
        }})
        .map((el) => ({{ el, label: labelOf(el).toLowerCase() }}))
        .filter((c) => c.label.includes(wanted));
    if (!candidates.length) return false;
    const exact = candidates.find((c) => c.label === wanted);
    (exact || candidates[0]).el.click();
    return true;
}})()"#
    );
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match crate::cmd::eval::evaluate_value(&client, &probe).await {
            Ok(Some(Value::Bool(true))) => return Ok(Some(json!({ "ok": true }))),
            // Frame failures (missing frame / cross-origin) fail fast —
            // the probe's inner lookup returns false rather than throwing.
            Err(e)
                if e.to_string().contains("cross-origin or not a frame")
                    || e.to_string().contains("Selector not found:") =>
            {
                return Err(e.to_string());
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no element with text {text:?} found after {timeout_ms}ms"
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// `interaction.uploadFile` — set a local file on an `<input type=file>` via
/// CDP `DOM.setFileInputFiles` (the DevTools-native path; no synthetic
/// DataTransfer fragility). Deliberately no visibility wait: file inputs are
/// routinely hidden behind styled labels, and the CDP call doesn't need a
/// rendered element. The CLI already canonicalized the path against the
/// caller's cwd; the existence re-check here is defense in depth.
pub(super) async fn handle_upload_file(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let path = require_string(params, "path")?;
    if !std::path::Path::new(&path).is_file() {
        return Err(format!("file not found: {path}"));
    }
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;

    let doc = client
        .send_to(&tid, "DOM.getDocument", json!({ "depth": 0 }))
        .await
        .map_err(|e| e.to_string())?;
    let root_id = doc
        .get("root")
        .and_then(|r| r.get("nodeId"))
        .and_then(Value::as_u64)
        .ok_or_else(|| "DOM.getDocument response missing root nodeId".to_string())?;
    let node = client
        .send_to(
            &tid,
            "DOM.querySelector",
            json!({ "nodeId": root_id, "selector": selector }),
        )
        .await
        .map_err(|e| e.to_string())?;
    let node_id = node.get("nodeId").and_then(Value::as_u64).unwrap_or(0);
    if node_id == 0 {
        return Err(format!("Selector not found: {selector}"));
    }
    client
        .send_to(
            &tid,
            "DOM.setFileInputFiles",
            json!({ "files": [path], "nodeId": node_id }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(json!({ "ok": true, "path": path })))
}

pub(super) async fn handle_type(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = require_string(params, "selector")?;
    let text = require_string(params, "text")?;
    let timeout_ms = clamped_wait_ms(params, 30_000);
    let frame = frame_param(params);
    let client = client_or_err(state).await?;
    wait_for_selector_visible(&client, &selector, timeout_ms, frame.as_deref()).await?;
    let sel_lit = serde_json::to_string(&selector).unwrap();
    let text_lit = serde_json::to_string(&text).unwrap();
    let prelude = doc_prelude(frame.as_deref())?;
    // JS-based type (spike scope) — sets .value + fires input/change events.
    // Plain HTML forms work; some React/Vue controlled inputs may need the
    // native setter trick, which is phase 2c (real CDP Input.dispatchKeyEvent).
    let expr = format!(
        "(() => {{
    {prelude}
    const el = __doc.querySelector({sel_lit});
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
    wait_for_selector_visible(&client, &selector, 30_000, None).await?;

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
    wait_for_selector_visible(&client, &selector, 30_000, None).await?;

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
    wait_for_selector_visible(&client, &selector, 30_000, None).await?;
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
