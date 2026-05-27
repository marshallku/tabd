use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::page;
use crate::cdp::CdpClient;

pub async fn run(url: &str, expr: &str, as_json: bool, timeout_ms: u64) -> Result<()> {
    let (browser, client) = page::open(url, timeout_ms).await?;
    let result = evaluate_value(&client, expr).await;
    let _ = page::teardown(browser, client).await;
    let value = result?;

    if as_json {
        let serialized = serde_json::to_string(&value.unwrap_or(Value::Null))?;
        println!("{serialized}");
    } else {
        match value {
            Some(Value::String(s)) => println!("{s}"),
            Some(other) => println!("{}", serde_json::to_string(&other)?),
            // Runtime.evaluate of `undefined` returns no `.result.value` —
            // emit an empty line to keep CLI piping behaviour predictable.
            None => println!(),
        }
    }
    Ok(())
}

/// Run `Runtime.evaluate(expr, returnByValue: true)` on the attached session
/// and return the inner `result.value` (or `None` when the expression evaluates
/// to `undefined`). Propagates `exceptionDetails` as a Rust error.
pub async fn evaluate_value(client: &CdpClient, expr: &str) -> Result<Option<Value>> {
    // `awaitPromise: true` matches TS chromium-cdp (src/server/runtimes/cdp.ts:1893):
    // resolves `Promise.resolve(x)` to `x` instead of returning the raw promise object.
    let raw = client
        .send(
            "Runtime.evaluate",
            json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )
        .await?;
    unwrap_runtime_result(&raw, "Runtime.evaluate")
}

/// Shared unwrap of a Runtime.evaluate / Runtime.callFunctionOn response body.
/// `op` is the human-readable operation label used in error messages so that
/// callers (`Runtime.evaluate` vs `Runtime.callFunctionOn`) get accurate
/// diagnostics.
///
/// CDP semantics for the inner RemoteObject:
///   - `type=="undefined"`               → no value field; treat as `None`
///   - `unserializableValue` present     → NaN / Infinity / -0 / 1n etc.;
///                                         surface the literal as a string so
///                                         callers see the same form DevTools shows
///   - `value` present                   → the serializable value
///   - otherwise (object/function w/o by-value) → bail with the type
pub fn unwrap_runtime_result(raw: &Value, op: &str) -> Result<Option<Value>> {
    if let Some(exc) = raw.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(Value::as_str)
            .or_else(|| exc.get("text").and_then(Value::as_str))
            .unwrap_or("evaluation threw");
        bail!("{op}: {msg}");
    }

    let result_obj = raw
        .get("result")
        .ok_or_else(|| anyhow!("{op} response missing 'result'"))?;

    if matches!(
        result_obj.get("type").and_then(Value::as_str),
        Some("undefined")
    ) {
        return Ok(None);
    }
    if let Some(unser) = result_obj
        .get("unserializableValue")
        .and_then(Value::as_str)
    {
        return Ok(Some(Value::String(unser.to_owned())));
    }
    if let Some(value) = result_obj.get("value") {
        return Ok(Some(value.clone()));
    }
    let type_str = result_obj
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("<no type>");
    bail!("{op} returned a non-serializable {type_str}; pass returnByValue-friendly expression or serialize on the JS side");
}

#[cfg(test)]
mod tests {
    use super::unwrap_runtime_result;
    use serde_json::json;

    #[test]
    fn unwrap_returns_string_value() {
        let raw = json!({ "result": { "type": "string", "value": "hello" } });
        let v = unwrap_runtime_result(&raw, "Runtime.evaluate").unwrap();
        assert_eq!(v.unwrap(), json!("hello"));
    }

    #[test]
    fn unwrap_undefined_returns_none() {
        let raw = json!({ "result": { "type": "undefined" } });
        assert!(unwrap_runtime_result(&raw, "Runtime.evaluate").unwrap().is_none());
    }

    #[test]
    fn unwrap_unserializable_becomes_string() {
        let raw = json!({ "result": { "type": "number", "unserializableValue": "NaN" } });
        let v = unwrap_runtime_result(&raw, "Runtime.evaluate").unwrap();
        assert_eq!(v.unwrap(), json!("NaN"));
    }

    #[test]
    fn unwrap_exception_uses_operation_label() {
        let raw = json!({
            "result": { "type": "undefined" },
            "exceptionDetails": { "exception": { "description": "boom" } }
        });
        let err = unwrap_runtime_result(&raw, "Runtime.callFunctionOn").unwrap_err();
        assert!(err.to_string().starts_with("Runtime.callFunctionOn:"), "got: {err}");
        assert!(err.to_string().contains("boom"), "got: {err}");
    }
}
