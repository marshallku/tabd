use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::eval::{evaluate_value, unwrap_runtime_result};
use super::get_text::build_text_body;
use super::page;
use crate::cdp::CdpClient;

pub async fn run(
    url: &str,
    selector: Option<&str>,
    testid: Option<&str>,
    role: Option<&str>,
    name: Option<&str>,
    raw: bool,
    limit: u32,
    timeout_ms: u64,
) -> Result<()> {
    validate_target_flags_strict(selector, testid, role, name)?;
    let (browser, client) = page::open(url, timeout_ms).await?;

    // Materialise into Result first so page::teardown always runs even on error.
    let result: Result<Vec<String>> = if let Some(r) = role {
        ax_query_all(&client, r, name, raw, limit).await
    } else {
        match build_query_all_expr(selector, testid, raw, limit) {
            Ok(expr) => eval_to_string_array(&client, &expr).await,
            Err(e) => Err(e),
        }
    };
    let _ = page::teardown(browser, client).await;

    let texts = result?;
    println!("{}", serde_json::to_string(&texts)?);
    Ok(())
}

/// Strict variant of validate_target_flags — requires exactly one of
/// selector/testid/role. (get-text's variant allows all-none → default chain.)
fn validate_target_flags_strict(
    selector: Option<&str>,
    testid: Option<&str>,
    role: Option<&str>,
    name: Option<&str>,
) -> Result<()> {
    let count = [selector.is_some(), testid.is_some(), role.is_some()]
        .iter()
        .filter(|&&x| x)
        .count();
    if count == 0 {
        bail!("query-all requires --selector, --testid, or --role");
    }
    if count > 1 {
        bail!("--selector, --testid, --role are mutually exclusive");
    }
    if name.is_some() && role.is_none() {
        bail!("--name requires --role");
    }
    Ok(())
}

fn build_els_expr(selector: Option<&str>, testid: Option<&str>) -> Result<String> {
    match (selector, testid) {
        (Some(s), None) => {
            let sel_lit = serde_json::to_string(s)?;
            Ok(format!("document.querySelectorAll({sel_lit})"))
        }
        (None, Some(t)) => {
            let testid_lit = serde_json::to_string(t)?;
            Ok(format!(
                "[...document.querySelectorAll('[data-testid]')].filter(el => el.dataset.testid === {testid_lit})"
            ))
        }
        _ => unreachable!("validate_target_flags_strict requires exactly one"),
    }
}

fn build_query_all_expr(
    selector: Option<&str>,
    testid: Option<&str>,
    raw: bool,
    limit: u32,
) -> Result<String> {
    let els_expr = build_els_expr(selector, testid)?;
    let body = build_text_body(raw);
    Ok(format!(
        r#"(() => {{
    const els = {els_expr};
    return [...els].slice(0, {limit}).map(target => {{
        {body}
    }});
}})()"#
    ))
}

async fn eval_to_string_array(client: &CdpClient, expr: &str) -> Result<Vec<String>> {
    let value = evaluate_value(client, expr)
        .await?
        .ok_or_else(|| anyhow!("query-all: evaluate returned undefined"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| anyhow!("query-all: expected array, got: {value}"))?;
    arr.iter()
        .map(|v| {
            v.as_str()
                .map(String::from)
                .ok_or_else(|| anyhow!("query-all: array element is not a string: {v}"))
        })
        .collect()
}

async fn ax_query_all(
    client: &CdpClient,
    role: &str,
    name: Option<&str>,
    raw: bool,
    limit: u32,
) -> Result<Vec<String>> {
    client.send("Accessibility.enable", json!({})).await?;

    let doc = client.send("DOM.getDocument", json!({ "depth": 0 })).await?;
    let root_node_id = doc
        .get("root")
        .and_then(|r| r.get("nodeId"))
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("DOM.getDocument missing root.nodeId"))?;

    let mut params = json!({ "nodeId": root_node_id, "role": role });
    if let Some(n) = name {
        params["accessibleName"] = Value::String(n.to_owned());
    }
    let q = client.send("Accessibility.queryAXTree", params).await?;
    let nodes = q
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Accessibility.queryAXTree missing nodes"))?;

    let body = build_text_body(raw);
    let fn_decl = format!("function() {{\n    const target = this;\n    {body}\n}}");

    let mut texts: Vec<String> = Vec::new();
    for node in nodes {
        // Limit is a cap on successful results, not inspected nodes.
        if texts.len() as u32 >= limit {
            break;
        }
        if node.get("ignored").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let backend_id = match node.get("backendDOMNodeId").and_then(Value::as_u64) {
            Some(id) => id,
            None => continue, // virtual AX node, no DOM peer
        };
        let resolved = match client
            .send("DOM.resolveNode", json!({ "backendNodeId": backend_id }))
            .await
        {
            Ok(r) => r,
            Err(_) => continue, // detached node between queryAXTree and now
        };
        let object_id = match resolved
            .get("object")
            .and_then(|o| o.get("objectId"))
            .and_then(Value::as_str)
        {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let r = client
            .send(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": fn_decl,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        if let Some(s) = unwrap_runtime_result(&r, "Runtime.callFunctionOn")?
            .and_then(|v| v.as_str().map(String::from))
        {
            texts.push(s);
        }
    }
    Ok(texts)
}

#[cfg(test)]
mod tests {
    use super::{build_els_expr, build_query_all_expr, validate_target_flags_strict};

    // -- validate_target_flags_strict --

    #[test]
    fn strict_rejects_all_none() {
        let err = validate_target_flags_strict(None, None, None, None).unwrap_err();
        assert!(
            err.to_string().contains("requires --selector"),
            "got: {err}"
        );
    }

    #[test]
    fn strict_selector_only_is_ok() {
        validate_target_flags_strict(Some("li"), None, None, None).unwrap();
    }

    #[test]
    fn strict_testid_only_is_ok() {
        validate_target_flags_strict(None, Some("x"), None, None).unwrap();
    }

    #[test]
    fn strict_role_only_is_ok() {
        validate_target_flags_strict(None, None, Some("button"), None).unwrap();
    }

    #[test]
    fn strict_role_plus_name_is_ok() {
        validate_target_flags_strict(None, None, Some("button"), Some("Click")).unwrap();
    }

    #[test]
    fn strict_selector_plus_testid_rejected() {
        let err =
            validate_target_flags_strict(Some("li"), Some("x"), None, None).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn strict_selector_plus_role_rejected() {
        let err =
            validate_target_flags_strict(Some("li"), None, Some("button"), None).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn strict_testid_plus_role_rejected() {
        let err =
            validate_target_flags_strict(None, Some("x"), Some("button"), None).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn strict_name_without_role_rejected() {
        let err =
            validate_target_flags_strict(Some("li"), None, None, Some("Click")).unwrap_err();
        assert!(
            err.to_string().contains("--name requires --role"),
            "got: {err}"
        );
    }

    // -- build_els_expr --

    #[test]
    fn els_selector_uses_querySelectorAll() {
        let e = build_els_expr(Some("li.foo"), None).unwrap();
        assert_eq!(e, r#"document.querySelectorAll("li.foo")"#);
    }

    #[test]
    fn els_selector_quote_safely_escaped() {
        let e = build_els_expr(Some(r#"a[href*='"']"#), None).unwrap();
        assert!(
            e.contains(r#"querySelectorAll("a[href*='\"']")"#),
            "got: {e}"
        );
    }

    #[test]
    fn els_testid_filters_by_dataset() {
        let e = build_els_expr(None, Some("item")).unwrap();
        assert!(
            e.contains(r#"[...document.querySelectorAll('[data-testid]')].filter"#),
            "got: {e}"
        );
        assert!(e.contains(r#"el.dataset.testid === "item""#), "got: {e}");
    }

    #[test]
    fn els_testid_special_chars_json_escaped() {
        let e = build_els_expr(None, Some("a\"b\\c\nd")).unwrap();
        assert!(e.contains(r#"=== "a\"b\\c\nd""#), "got: {e}");
    }

    // -- build_query_all_expr --

    #[test]
    fn query_all_expr_includes_slice_limit() {
        let e = build_query_all_expr(Some("li"), None, false, 50).unwrap();
        assert!(e.contains("slice(0, 50)"), "got: {e}");
    }

    #[test]
    fn query_all_expr_includes_text_body() {
        let e = build_query_all_expr(Some("li"), None, false, 10).unwrap();
        // The shared body must appear verbatim — confirms the SoT chain
        // (build_query_all_expr → build_text_body) is intact.
        assert!(
            e.contains("target.innerText ?? target.textContent"),
            "got: {e}"
        );
        assert!(e.contains(r#"replace(/\n{3,}/g"#), "got: {e}");
    }

    #[test]
    fn query_all_expr_raw_uses_textcontent_only() {
        let e = build_query_all_expr(Some("li"), None, true, 10).unwrap();
        assert!(
            e.contains(r#"if (true) return target.textContent ?? """#),
            "got: {e}"
        );
    }

    #[test]
    fn query_all_expr_maps_with_arrow() {
        let e = build_query_all_expr(Some("li"), None, false, 10).unwrap();
        assert!(e.contains(".map(target =>"), "got: {e}");
    }
}
