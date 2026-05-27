use anyhow::{Result, bail};
use serde_json::Value;

use super::eval::evaluate_value;
use super::page;

pub async fn run(
    url: &str,
    selector: Option<&str>,
    testid: Option<&str>,
    raw: bool,
    timeout_ms: u64,
) -> Result<()> {
    if selector.is_some() && testid.is_some() {
        bail!("--selector and --testid are mutually exclusive");
    }
    let expr = build_get_text_expr(selector, testid, raw)?;

    let (browser, client) = page::open(url, timeout_ms).await?;
    let value = evaluate_value(&client, &expr).await;
    let _ = page::teardown(browser, client).await;

    match value? {
        Some(Value::String(s)) => println!("{s}"),
        Some(other) => {
            // Defensive — the JS expression always returns a string (target.textContent
            // and .trim() guarantee it). This arm only fires if the page redefines
            // those, which is outside spike scope.
            println!("{}", serde_json::to_string(&other)?);
        }
        None => bail!("get-text: Runtime.evaluate returned undefined (unexpected)"),
    }
    Ok(())
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
    let raw_lit = serde_json::to_string(&raw)?;

    let target_expr = match (selector, testid) {
        (Some(s), None) => {
            let sel_lit = serde_json::to_string(s)?;
            format!("document.querySelector({sel_lit}) ?? document.body")
        }
        (None, Some(t)) => {
            let testid_lit = serde_json::to_string(t)?;
            format!(
                "([...document.querySelectorAll('[data-testid]')].find(el => el.dataset.testid === {testid_lit})) ?? document.body"
            )
        }
        (None, None) => {
            r#"document.querySelector("main, article, body") ?? document.body"#.to_string()
        }
        (Some(_), Some(_)) => unreachable!("validated by CLI arg group and run()"),
    };

    Ok(format!(
        r#"(() => {{
    const target = {target_expr};
    if ({raw_lit}) return target.textContent ?? "";
    const text = target.innerText ?? target.textContent ?? "";
    return text.replace(/\n{{3,}}/g, "\n\n").trim();
}})()"#
    ))
}

#[cfg(test)]
mod tests {
    use super::build_get_text_expr;

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
        // serde_json escapes the inner " to \"
        assert!(
            expr.contains(r#"document.querySelector("a[href*='\"']")"#),
            "got: {expr}"
        );
    }

    #[test]
    fn testid_uses_js_string_equality_not_css_attr() {
        let expr = build_get_text_expr(None, Some("my-btn"), false).unwrap();
        assert!(
            expr.contains("querySelectorAll('[data-testid]')"),
            "got: {expr}"
        );
        assert!(
            expr.contains(r#"el.dataset.testid === "my-btn""#),
            "got: {expr}"
        );
        assert!(expr.contains("?? document.body"), "got: {expr}");
    }

    #[test]
    fn testid_with_special_chars_uses_json_escape() {
        // Quote, backslash, newline must all be JSON-escaped.
        let weird = "a\"b\\c\nd";
        let expr = build_get_text_expr(None, Some(weird), false).unwrap();
        assert!(
            expr.contains(r#"=== "a\"b\\c\nd""#),
            "got: {expr}"
        );
    }

    #[test]
    fn testid_empty_string_allowed() {
        let expr = build_get_text_expr(None, Some(""), false).unwrap();
        assert!(expr.contains(r#"=== """#), "got: {expr}");
    }

    #[test]
    fn raw_true_uses_early_return_textcontent() {
        let expr = build_get_text_expr(None, None, true).unwrap();
        // Builder always emits both branches; raw=true makes the early-return fire first
        // at runtime because the `if (true)` literal short-circuits.
        assert!(
            expr.contains(r#"if (true) return target.textContent ?? """#),
            "got: {expr}"
        );
        // Collapse code is present in the source but unreachable under raw=true.
        assert!(expr.contains(r#"replace(/\n{3,}/g"#), "got: {expr}");
    }

    #[test]
    fn raw_false_uses_innertext_collapse_and_trim() {
        let expr = build_get_text_expr(None, None, false).unwrap();
        assert!(
            expr.contains(r#"if (false) return target.textContent"#),
            "got: {expr}"
        );
        assert!(
            expr.contains("target.innerText ?? target.textContent"),
            "got: {expr}"
        );
        assert!(
            expr.contains(r#"replace(/\n{3,}/g, "\n\n")"#),
            "got: {expr}"
        );
        assert!(expr.contains(".trim()"), "got: {expr}");
    }
}
