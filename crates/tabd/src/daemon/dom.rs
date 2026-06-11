//! dom.* + tabs.navigate handlers (navigate, eval, text/html, query, summary).

use super::*;

pub(super) async fn handle_navigate(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "tabs.navigate: missing 'url' (string)".to_string())?
        .to_owned();
    let client = state
        .client
        .lock()
        .await
        .as_ref()
        .cloned()
        .ok_or_else(|| "cdp client not initialized".to_string())?;
    page::navigate_existing(&client, &url, 30_000)
        .await
        .map(|()| Some(json!({ "url": url })))
        .map_err(|e| e.to_string())
}

pub(super) async fn handle_eval(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let code = params
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| "execution.executeJs: missing 'code' (string)".to_string())?;
    let client = state
        .client
        .lock()
        .await
        .as_ref()
        .cloned()
        .ok_or_else(|| "cdp client not initialized".to_string())?;
    let max = max_chars(params);
    // None (CDP `undefined`) propagates as None → wire response omits `data`,
    // matching TS chromium-cdp byte-exact (codex round 1 C1). The clamp must
    // never coerce that into null/"" — only Some values are touched.
    let result = crate::cmd::eval::evaluate_value(&client, code)
        .await
        .map_err(|e| e.to_string())?;
    match result {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(Value::String(clamp_chars(s, max)))),
        Some(v) => {
            // Truncated JSON would be syntactically corrupt — error instead.
            if max > 0 {
                let n = serde_json::to_string(&v)
                    .map_err(|e| e.to_string())?
                    .chars()
                    .count() as u64;
                if n > max {
                    return Err(format!(
                        "eval result too large ({n} chars > {max}); narrow the expression or pass --max-chars 0"
                    ));
                }
            }
            Ok(Some(v))
        }
    }
}

pub(super) async fn handle_get_text(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = params
        .get("selector")
        .and_then(Value::as_str)
        .unwrap_or("main, article, body")
        .to_owned();
    let raw = params.get("raw").and_then(Value::as_bool).unwrap_or(false);
    let client = state
        .client
        .lock()
        .await
        .as_ref()
        .cloned()
        .ok_or_else(|| "cdp client not initialized".to_string())?;

    let body = crate::cmd::get_text::build_text_body(raw);
    let sel_lit = serde_json::to_string(&selector).map_err(|e| format!("selector encode: {e}"))?;
    let expr = format!(
        "(() => {{ const target = document.querySelector({sel_lit}) ?? document.body; {body} }})()"
    );

    // dom.getText always returns a string (TS wraps with String(...)). Map
    // None → "" so the wire shape stays consistent.
    let max = max_chars(params);
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|opt| {
            Some(clamp_value_chars(
                opt.unwrap_or(Value::String(String::new())),
                max,
            ))
        })
        .map_err(|e| e.to_string())
}

pub(super) async fn handle_get_html(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = params
        .get("selector")
        .and_then(Value::as_str)
        .unwrap_or("body")
        .to_owned();
    let outer = params.get("outer").and_then(Value::as_bool).unwrap_or(true);
    let clean = params.get("clean").and_then(Value::as_bool).unwrap_or(true);
    let client = client_or_err(state).await?;
    let sel_lit = serde_json::to_string(&selector).unwrap();
    let outer_lit = serde_json::to_string(&outer).unwrap();
    let clean_lit = serde_json::to_string(&clean).unwrap();

    let expr = format!(
        r#"(() => {{
    const node = document.querySelector({sel_lit});
    if (!node) throw new Error('Selector not found: ' + {sel_lit});
    const clone = node.cloneNode(true);
    if ({clean_lit}) {{
        clone.querySelectorAll("script,style,svg").forEach((el) => el.remove());
        const walker = document.createTreeWalker(clone, NodeFilter.SHOW_COMMENT);
        const comments = [];
        while (walker.nextNode()) comments.push(walker.currentNode);
        comments.forEach((node) => node.remove());
        clone.querySelectorAll("*").forEach((el) => {{
            [...el.attributes]
                .filter((attr) => attr.name.startsWith("data-"))
                .forEach((attr) => el.removeAttribute(attr.name));
        }});
    }}
    return {outer_lit} ? clone.outerHTML : clone.innerHTML;
}})()"#
    );

    let max = max_chars(params);
    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|opt| {
            Some(clamp_value_chars(
                opt.unwrap_or(Value::String(String::new())),
                max,
            ))
        })
        .map_err(|e| e.to_string())
}

pub(super) async fn handle_query_selector(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let selector = params
        .get("selector")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(20);
    let visible_only = params
        .get("visibleOnly")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let client = client_or_err(state).await?;
    let sel_lit = serde_json::to_string(&selector).unwrap();
    let visible_lit = serde_json::to_string(&visible_only).unwrap();

    let expr = format!(
        r#"(() => {{
    return [...document.querySelectorAll({sel_lit})]
        .filter((el) => {{
            if (!{visible_lit}) return true;
            const rect = el.getBoundingClientRect();
            const style = getComputedStyle(el);
            return rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none";
        }})
        .slice(0, {limit})
        .map((el, index) => {{
            const rect = el.getBoundingClientRect();
            return {{
                index,
                tag: el.tagName.toLowerCase(),
                id: el.id || null,
                classes: [...el.classList],
                text: (el.innerText || el.textContent || "").trim().slice(0, 200),
                attributes: Object.fromEntries([...el.attributes].map((attr) => [attr.name, attr.value])),
                rect: {{ x: rect.x, y: rect.y, width: rect.width, height: rect.height }}
            }};
        }});
}})()"#
    );

    crate::cmd::eval::evaluate_value(&client, &expr)
        .await
        .map(|opt| Some(opt.unwrap_or(Value::Array(vec![]))))
        .map_err(|e| e.to_string())
}

pub(super) async fn handle_content_summary(
    state: &DaemonState,
    params: &Value,
) -> Result<Option<Value>, String> {
    let tab_id = params
        .get("tabId")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let selector_lit = params
        .get("selector")
        .and_then(Value::as_str)
        .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "null".to_string()))
        .unwrap_or_else(|| "null".to_string());
    let max_headings = params
        .get("maxHeadings")
        .and_then(Value::as_u64)
        .unwrap_or(20);
    let max_links = params.get("maxLinks").and_then(Value::as_u64).unwrap_or(20);
    let max_text_length = params
        .get("maxTextLength")
        .and_then(Value::as_u64)
        .unwrap_or(4000);
    let client = client_or_err(state).await?;
    let tid = resolve_target_id(&client, tab_id).await?;

    // Port of TS Playwright src/server/runtimes/playwright.ts contentSummary JS
    // body. CDP runtime (TS) marks this unsupported; we wire it via Runtime.
    // evaluate so the CLI/MCP wire shape is identical to the Playwright path.
    // Template-replace 4 params instead of format! so JS object literals
    // (`{a: 1}`) don't collide with format! placeholder syntax. Two `#`s
    // because the body has `"#cookie-banner"` which would otherwise close
    // an `r#"..."#` raw string prematurely.
    let template = r##"
        (() => {
            const selector = __SELECTOR__;
            const maxHeadings = __MAX_HEADINGS__;
            const maxLinks = __MAX_LINKS__;
            const maxTextLength = __MAX_TEXT_LENGTH__;
            const noiseSelectors = [
                "script","style","svg","noscript","nav","footer","header","aside",
                "[role='navigation']","[aria-hidden='true']",".sr-only",
                ".visually-hidden",".hidden","#cookie-banner","#cookies",
                ".cookie-banner",".cookie-notice",".advertisement",".ads"
            ];
            const pickRoot = () => {
                if (selector) return document.querySelector(selector);
                return document.querySelector("main")
                    || document.querySelector("article")
                    || document.querySelector("[role='main']")
                    || document.body;
            };
            const root = pickRoot();
            if (!root) {
                return { url: location.href, title: document.title, selector: selector ?? null, headings: [], links: [], forms: [], text: "" };
            }
            const clone = root.cloneNode(true);
            clone.querySelectorAll(noiseSelectors.join(",")).forEach(el => el.remove());
            const cleanText = (t) => (t || "")
                .replace(/ /g, " ")
                .replace(/[ \t]+\n/g, "\n")
                .replace(/\n{3,}/g, "\n\n")
                .replace(/[ \t]{2,}/g, " ")
                .trim();
            const headings = Array.from(clone.querySelectorAll("h1,h2,h3,h4,h5,h6"))
                .slice(0, maxHeadings)
                .map(el => ({ level: el.tagName.toLowerCase(), text: cleanText(el.textContent).slice(0, 200) }))
                .filter(item => item.text);
            const links = Array.from(clone.querySelectorAll("a[href]"))
                .slice(0, maxLinks)
                .map(el => ({ text: cleanText(el.textContent).slice(0, 160), href: el.getAttribute("href") }))
                .filter(item => item.text || item.href);
            const forms = Array.from(clone.querySelectorAll("form")).map((form, index) => {
                const fields = Array.from(form.querySelectorAll("input,textarea,select")).map(el => ({
                    name: el.getAttribute("name"),
                    type: el.getAttribute("type") || el.tagName.toLowerCase(),
                    id: el.id || null,
                }));
                return { index, fields };
            });
            const text = cleanText(clone.innerText || clone.textContent || "").slice(0, maxTextLength);
            return {
                url: location.href,
                title: document.title,
                selector: selector ?? null,
                headings,
                links,
                forms,
                text,
            };
        })()
        "##;
    let code = template
        .replace("__SELECTOR__", &selector_lit)
        .replace("__MAX_HEADINGS__", &max_headings.to_string())
        .replace("__MAX_LINKS__", &max_links.to_string())
        .replace("__MAX_TEXT_LENGTH__", &max_text_length.to_string());
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
