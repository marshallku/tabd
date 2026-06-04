use anyhow::{Result, anyhow};
use serde_json::Value;

use super::ax::traverse_visible_nodes;
use super::eval::evaluate_value;
use super::get_text::build_meta_js_body;
use super::page;
use super::query_all::{build_els_expr, validate_target_flags_strict};
use crate::cdp::CdpClient;

// TARGET flags (selector/testid/role/name) + raw/limit/timeout are passed
// positionally to mirror the CLI arg surface; grouping them into a shared
// struct is tracked as a follow-up cleanup.
#[allow(clippy::too_many_arguments)]
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

    let result: Result<Vec<Value>> = if let Some(r) = role {
        ax_find_all(&client, r, name, raw, limit).await
    } else {
        match build_find_all_expr(selector, testid, raw, limit) {
            Ok(expr) => eval_to_value_array(&client, &expr).await,
            Err(e) => Err(e),
        }
    };
    let _ = page::teardown(browser, client).await;

    let objects = result?;
    println!("{}", serde_json::to_string(&objects)?);
    Ok(())
}

fn build_find_all_expr(
    selector: Option<&str>,
    testid: Option<&str>,
    raw: bool,
    limit: u32,
) -> Result<String> {
    let els_expr = build_els_expr(selector, testid)?;
    let meta_body = build_meta_js_body(raw);
    Ok(format!(
        r#"(() => {{
{meta_body}
    const els = {els_expr};
    return [...els].slice(0, {limit}).map(metaOf).filter(x => x !== null);
}})()"#
    ))
}

async fn eval_to_value_array(client: &CdpClient, expr: &str) -> Result<Vec<Value>> {
    let value = evaluate_value(client, expr)
        .await?
        .ok_or_else(|| anyhow!("find-all: evaluate returned undefined"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| anyhow!("find-all: expected array, got: {value}"))?;
    Ok(arr.clone())
}

async fn ax_find_all(
    client: &CdpClient,
    role: &str,
    name: Option<&str>,
    raw: bool,
    limit: u32,
) -> Result<Vec<Value>> {
    let meta_body = build_meta_js_body(raw);
    // metaOf returns null for non-Element AX targets (static text etc.).
    // traverse_visible_nodes skips JSON null so they never reach the output.
    let fn_decl = format!("function() {{\n{meta_body}\nreturn metaOf(this);\n}}");
    traverse_visible_nodes(client, role, name, limit, &fn_decl).await
}

#[cfg(test)]
mod tests {
    use super::build_find_all_expr;

    #[test]
    fn expr_includes_meta_helpers_and_attr_keys() {
        let e = build_find_all_expr(Some("li"), None, false, 100).unwrap();
        assert!(e.contains("const ATTR_KEYS ="), "got: {e}");
        assert!(e.contains(r#""role""#), "got: {e}");
        assert!(e.contains(r#""aria-label""#), "got: {e}");
        assert!(e.contains(r#""name""#), "got: {e}");
        assert!(e.contains(r#""href""#), "got: {e}");
        assert!(e.contains(r#""value""#), "got: {e}");
        assert!(e.contains("const textOf"), "got: {e}");
        assert!(e.contains("const liveValueOf"), "got: {e}");
        assert!(e.contains("const metaOf"), "got: {e}");
    }

    #[test]
    fn expr_filters_null_meta_results() {
        let e = build_find_all_expr(Some("li"), None, false, 100).unwrap();
        assert!(e.contains(".filter(x => x !== null)"), "got: {e}");
    }

    #[test]
    fn expr_includes_slice_limit() {
        let e = build_find_all_expr(Some("li"), None, false, 50).unwrap();
        assert!(e.contains("slice(0, 50)"), "got: {e}");
    }

    #[test]
    fn expr_includes_required_meta_fields() {
        let e = build_find_all_expr(Some("li"), None, false, 100).unwrap();
        assert!(e.contains("tag: el.tagName.toLowerCase()"), "got: {e}");
        assert!(e.contains("text: textOf(el)"), "got: {e}");
        assert!(e.contains("id: el.id || null"), "got: {e}");
        assert!(e.contains("classes: [...el.classList]"), "got: {e}");
        assert!(e.contains("rect:"), "got: {e}");
    }

    #[test]
    fn expr_input_uses_live_value() {
        // The liveValueOf branch should reference HTMLInputElement etc.
        let e = build_find_all_expr(Some("input"), None, false, 100).unwrap();
        assert!(e.contains("HTMLInputElement"), "got: {e}");
        assert!(e.contains("HTMLTextAreaElement"), "got: {e}");
        assert!(e.contains("HTMLSelectElement"), "got: {e}");
        assert!(e.contains("el.value"), "got: {e}");
    }

    #[test]
    fn expr_metaof_checks_instance_of_element() {
        let e = build_find_all_expr(Some("li"), None, false, 100).unwrap();
        assert!(
            e.contains("if (!(el instanceof Element)) return null"),
            "got: {e}"
        );
    }

    #[test]
    fn expr_testid_uses_dataset_filter() {
        let e = build_find_all_expr(None, Some("item"), false, 100).unwrap();
        assert!(
            e.contains(r#"[...document.querySelectorAll('[data-testid]')].filter"#),
            "got: {e}"
        );
        assert!(e.contains(r#"el.dataset.testid === "item""#), "got: {e}");
    }

    #[test]
    fn expr_raw_passes_through_text_body() {
        let e_raw = build_find_all_expr(Some("li"), None, true, 100).unwrap();
        let e_norm = build_find_all_expr(Some("li"), None, false, 100).unwrap();
        // raw vs not changes the text_body branch — verify both forms appear
        // in their respective outputs.
        assert!(
            e_raw.contains(r#"if (true) return target.textContent ?? """#),
            "raw got: {e_raw}"
        );
        assert!(
            e_norm.contains(r#"if (false) return target.textContent ?? """#),
            "norm got: {e_norm}"
        );
    }
}
