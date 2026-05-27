use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::eval::{evaluate_value, unwrap_runtime_result};
use super::page;
use crate::cdp::CdpClient;

pub async fn run(
    url: &str,
    selector: Option<&str>,
    testid: Option<&str>,
    role: Option<&str>,
    name: Option<&str>,
    raw: bool,
    timeout_ms: u64,
) -> Result<()> {
    validate_target_flags(selector, testid, role, name)?;
    let (browser, client) = page::open(url, timeout_ms).await?;

    let result = if let Some(r) = role {
        ax_get_text(&client, r, name, raw).await
    } else {
        let expr = build_get_text_expr(selector, testid, raw)?;
        evaluate_value(&client, &expr).await
    };
    let _ = page::teardown(browser, client).await;

    match result? {
        Some(Value::String(s)) => println!("{s}"),
        Some(other) => {
            println!("{}", serde_json::to_string(&other)?);
        }
        None => bail!("get-text: Runtime returned undefined (unexpected)"),
    }
    Ok(())
}

/// Reject mutually exclusive target combinations and `--name` without `--role`.
/// CLI clap arg group also rejects selector/testid/role pairs, but the function
/// is the source of truth so non-CLI callers (and tests) stay safe.
fn validate_target_flags(
    selector: Option<&str>,
    testid: Option<&str>,
    role: Option<&str>,
    name: Option<&str>,
) -> Result<()> {
    let count = [selector.is_some(), testid.is_some(), role.is_some()]
        .iter()
        .filter(|&&x| x)
        .count();
    if count > 1 {
        bail!("--selector, --testid, --role are mutually exclusive");
    }
    if name.is_some() && role.is_none() {
        bail!("--name requires --role");
    }
    Ok(())
}

/// Single source of truth for the JS text-extraction body. Both the
/// Runtime.evaluate path (selector/testid/default) and the Runtime.callFunctionOn
/// path (AX role/name) wrap this body with their own `const target = ...;` prefix
/// so the extraction semantics cannot drift between paths.
pub(super) fn build_text_body(raw: bool) -> String {
    let raw_lit = serde_json::to_string(&raw).expect("bool serialization");
    format!(
        r#"if ({raw_lit}) return target.textContent ?? "";
const text = target.innerText ?? target.textContent ?? "";
return text.replace(/\n{{3,}}/g, "\n\n").trim();"#
    )
}

/// Build the JS expression that mirrors TS `dom.getText`
/// (src/server/runtimes/cdp.ts:854~872) byte-for-byte semantics, with an
/// extra `--testid` branch that resolves via JS string equality on
/// `el.dataset.testid` (avoids CSS attribute-value escape entirely).
fn build_get_text_expr(
    selector: Option<&str>,
    testid: Option<&str>,
    raw: bool,
) -> Result<String> {
    let target_expr = build_target_expr(selector, testid)?;
    let body = build_text_body(raw);
    Ok(format!(
        "(() => {{\n    const target = {target_expr};\n    {body}\n}})()"
    ))
}

fn build_target_expr(selector: Option<&str>, testid: Option<&str>) -> Result<String> {
    match (selector, testid) {
        (Some(s), None) => {
            let sel_lit = serde_json::to_string(s)?;
            Ok(format!(
                "document.querySelector({sel_lit}) ?? document.body"
            ))
        }
        (None, Some(t)) => {
            let testid_lit = serde_json::to_string(t)?;
            Ok(format!(
                "([...document.querySelectorAll('[data-testid]')].find(el => el.dataset.testid === {testid_lit})) ?? document.body"
            ))
        }
        (None, None) => Ok(
            r#"document.querySelector("main, article, body") ?? document.body"#.to_string(),
        ),
        (Some(_), Some(_)) => unreachable!("validated by CLI arg group and run()"),
    }
}

/// Accessibility-tree query path: enable the Accessibility domain, run
/// `queryAXTree` against the document root, take the first non-ignored match,
/// resolve its backend DOM node to an objectId, then `callFunctionOn` to apply
/// the shared text-extraction body. Returns the unwrapped runtime value the
/// same way `evaluate_value` does, so the caller's printing logic does not
/// have to branch on the source path.
async fn ax_get_text(
    client: &CdpClient,
    role: &str,
    name: Option<&str>,
    raw: bool,
) -> Result<Option<Value>> {
    // CDP says Accessibility.enable is idempotent — safe to call per query.
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

    if nodes.is_empty() {
        bail!(
            "no AX node matches role={role}{}",
            name.map(|n| format!(" name={n:?}")).unwrap_or_default()
        );
    }
    let visible = nodes.iter().find(|n| {
        n.get("ignored").and_then(Value::as_bool) != Some(true)
    });
    let node = visible.ok_or_else(|| {
        anyhow!(
            "no visible AX node matches role={role}{} (all {} match(es) are ignored)",
            name.map(|n| format!(" name={n:?}")).unwrap_or_default(),
            nodes.len()
        )
    })?;

    let backend_node_id = node
        .get("backendDOMNodeId")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("AX node missing backendDOMNodeId (virtual node?)"))?;

    let resolved = client
        .send("DOM.resolveNode", json!({ "backendNodeId": backend_node_id }))
        .await?;
    let object_id = resolved
        .get("object")
        .and_then(|o| o.get("objectId"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("DOM.resolveNode missing object.objectId"))?
        .to_owned();

    let body = build_text_body(raw);
    let fn_decl = format!("function() {{\n    const target = this;\n    {body}\n}}");
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
    unwrap_runtime_result(&r, "Runtime.callFunctionOn")
}

#[cfg(test)]
mod tests {
    use super::{build_get_text_expr, build_text_body, validate_target_flags};

    #[test]
    fn default_uses_main_article_body_chain() {
        let expr = build_get_text_expr(None, None, false).unwrap();
        assert!(
            expr.contains(r#"document.querySelector("main, article, body") ?? document.body"#),
            "got: {expr}"
        );
    }

    #[test]
    fn explicit_selector_embeds_as_json_literal() {
        let expr = build_get_text_expr(Some("h1.foo"), None, false).unwrap();
        assert!(
            expr.contains(r#"document.querySelector("h1.foo") ?? document.body"#),
            "got: {expr}"
        );
    }

    #[test]
    fn explicit_selector_quote_safely_embedded() {
        let expr = build_get_text_expr(Some(r#"a[href*='"']"#), None, false).unwrap();
        assert!(
            expr.contains(r#"document.querySelector("a[href*='\"']")"#),
            "got: {expr}"
        );
    }

    #[test]
    fn testid_uses_js_string_equality_not_css_attr() {
        let expr = build_get_text_expr(None, Some("my-btn"), false).unwrap();
        assert!(expr.contains("querySelectorAll('[data-testid]')"), "got: {expr}");
        assert!(expr.contains(r#"el.dataset.testid === "my-btn""#), "got: {expr}");
        assert!(expr.contains("?? document.body"), "got: {expr}");
    }

    #[test]
    fn testid_with_special_chars_uses_json_escape() {
        let weird = "a\"b\\c\nd";
        let expr = build_get_text_expr(None, Some(weird), false).unwrap();
        assert!(expr.contains(r#"=== "a\"b\\c\nd""#), "got: {expr}");
    }

    #[test]
    fn testid_empty_string_allowed() {
        let expr = build_get_text_expr(None, Some(""), false).unwrap();
        assert!(expr.contains(r#"=== """#), "got: {expr}");
    }

    #[test]
    fn raw_true_uses_early_return_textcontent() {
        let body = build_text_body(true);
        assert!(body.contains(r#"if (true) return target.textContent ?? """#), "got: {body}");
        assert!(body.contains(r#"replace(/\n{3,}/g"#), "got: {body}");
    }

    #[test]
    fn raw_false_uses_innertext_collapse_and_trim() {
        let body = build_text_body(false);
        assert!(body.contains(r#"if (false) return target.textContent"#), "got: {body}");
        assert!(body.contains("target.innerText ?? target.textContent"), "got: {body}");
        assert!(body.contains(r#"replace(/\n{3,}/g, "\n\n")"#), "got: {body}");
        assert!(body.contains(".trim()"), "got: {body}");
    }

    #[test]
    fn text_body_is_identical_across_paths() {
        // selector/testid path and AX path both wrap build_text_body. Verify
        // the body string is byte-identical for the same raw flag.
        let b1 = build_text_body(false);
        let expr = build_get_text_expr(Some("h1"), None, false).unwrap();
        assert!(expr.contains(&b1), "selector expr should embed body verbatim; expr: {expr}");
    }

    // -- validate_target_flags coverage --

    #[test]
    fn all_none_is_ok() {
        validate_target_flags(None, None, None, None).unwrap();
    }

    #[test]
    fn selector_only_is_ok() {
        validate_target_flags(Some("h1"), None, None, None).unwrap();
    }

    #[test]
    fn testid_only_is_ok() {
        validate_target_flags(None, Some("x"), None, None).unwrap();
    }

    #[test]
    fn role_only_is_ok() {
        validate_target_flags(None, None, Some("button"), None).unwrap();
    }

    #[test]
    fn role_with_name_is_ok() {
        validate_target_flags(None, None, Some("button"), Some("Click")).unwrap();
    }

    #[test]
    fn selector_plus_testid_rejected() {
        let err = validate_target_flags(Some("h1"), Some("x"), None, None).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn selector_plus_role_rejected() {
        let err = validate_target_flags(Some("h1"), None, Some("button"), None).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn testid_plus_role_rejected() {
        let err = validate_target_flags(None, Some("x"), Some("button"), None).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn name_without_role_rejected() {
        let err = validate_target_flags(None, None, None, Some("Click")).unwrap_err();
        assert!(err.to_string().contains("--name requires --role"), "got: {err}");
    }

    #[test]
    fn selector_plus_name_rejected_via_missing_role() {
        // --selector + --name (no --role) — name still requires role.
        let err = validate_target_flags(Some("h1"), None, None, Some("Click")).unwrap_err();
        assert!(err.to_string().contains("--name requires --role"), "got: {err}");
    }
}
