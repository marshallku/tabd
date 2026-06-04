use anyhow::{Result, bail};
use serde_json::Value;

use super::eval::evaluate_value;
use super::page;

pub async fn run(url: &str, selector: &str, timeout_ms: u64) -> Result<()> {
    let expr = build_text_expr(selector)?;

    let (browser, client) = page::open(url, timeout_ms).await?;
    let value = evaluate_value(&client, &expr).await;
    let _ = page::teardown(browser, client).await;

    match value? {
        Some(Value::String(s)) => println!("{s}"),
        Some(other) => {
            // The `?? ''` fallback guarantees a string in the happy path; this
            // arm only fires if the page redefined textContent to a non-string.
            println!("{}", serde_json::to_string(&other)?);
        }
        None => bail!("fetch-text: Runtime.evaluate returned undefined (unexpected with `?? ''`)"),
    }
    Ok(())
}

/// Build the JS expression that returns `textContent` of the matched node,
/// or `""` when nothing matches. Selector is JSON-encoded so quotes,
/// backslashes, and other special chars cannot break out of the string literal
/// (spike plan codex C3).
fn build_text_expr(selector: &str) -> Result<String> {
    let lit = serde_json::to_string(selector)?;
    Ok(format!(
        "(document.querySelector({lit})?.textContent) ?? ''"
    ))
}

#[cfg(test)]
mod tests {
    use super::build_text_expr;

    #[test]
    fn embeds_plain_selector() {
        let expr = build_text_expr("h1").unwrap();
        assert_eq!(expr, r#"(document.querySelector("h1")?.textContent) ?? ''"#);
    }

    #[test]
    fn escapes_double_quote_in_selector() {
        let expr = build_text_expr(r#"a[href*='"']"#).unwrap();
        // serde_json encodes the inner " as \"
        assert!(expr.contains(r#""a[href*='\"']""#), "got: {expr}");
    }

    #[test]
    fn escapes_backslash_in_selector() {
        let expr = build_text_expr(r"div\.foo").unwrap();
        assert!(expr.contains(r#""div\\.foo""#), "got: {expr}");
    }

    #[test]
    fn escapes_newline_in_selector() {
        let expr = build_text_expr("a\nb").unwrap();
        assert!(expr.contains(r#""a\nb""#), "got: {expr}");
    }

    #[test]
    fn embeds_unicode_selector() {
        let expr = build_text_expr("section[data-name='한글']").unwrap();
        // JSON keeps the unicode verbatim by default.
        assert!(expr.contains("한글"), "got: {expr}");
    }
}
