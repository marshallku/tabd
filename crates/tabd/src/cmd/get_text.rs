/// Single source of truth for the JS text-extraction body. The daemon's DOM
/// handlers wrap this body with their own `const target = ...;` prefix so the
/// extraction semantics cannot drift between the selector/testid and AX paths.
pub(crate) fn build_text_body(raw: bool) -> String {
    let raw_lit = serde_json::to_string(&raw).expect("bool serialization");
    format!(
        r#"if ({raw_lit}) return target.textContent ?? "";
const text = target.innerText ?? target.textContent ?? "";
return text.replace(/\n{{3,}}/g, "\n\n").trim();"#
    )
}

#[cfg(test)]
mod tests {
    use super::build_text_body;

    #[test]
    fn raw_true_uses_early_return_textcontent() {
        let body = build_text_body(true);
        assert!(
            body.contains(r#"if (true) return target.textContent ?? """#),
            "got: {body}"
        );
        assert!(body.contains(r#"replace(/\n{3,}/g"#), "got: {body}");
    }

    #[test]
    fn raw_false_uses_innertext_collapse_and_trim() {
        let body = build_text_body(false);
        assert!(
            body.contains(r#"if (false) return target.textContent"#),
            "got: {body}"
        );
        assert!(
            body.contains("target.innerText ?? target.textContent"),
            "got: {body}"
        );
        assert!(
            body.contains(r#"replace(/\n{3,}/g, "\n\n")"#),
            "got: {body}"
        );
        assert!(body.contains(".trim()"), "got: {body}");
    }
}
