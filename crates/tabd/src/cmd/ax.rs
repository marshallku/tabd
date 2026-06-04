use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use super::eval::unwrap_runtime_result;
use crate::cdp::CdpClient;

/// Shared AX traversal: enable Accessibility, query the AX tree for role/name,
/// then for each visible (non-ignored) match resolve the DOM node and invoke
/// `fn_decl` on it via `Runtime.callFunctionOn`. Returns the (non-null) results
/// in document order, capped at `limit`.
///
/// `fn_decl` must be a JS function expression that takes `this` as the matched
/// DOM node and returns the per-node value. Null returns (e.g. metaOf on a
/// non-Element AX target) are skipped — they do not count toward `limit`.
///
/// Used by get-text (limit=1), query-all, and find-all so the AX query loop
/// lives in a single place (Rule of Three).
pub(super) async fn traverse_visible_nodes(
    client: &CdpClient,
    role: &str,
    name: Option<&str>,
    limit: u32,
    fn_decl: &str,
) -> Result<Vec<Value>> {
    client.send("Accessibility.enable", json!({})).await?;

    let doc = client
        .send("DOM.getDocument", json!({ "depth": 0 }))
        .await?;
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

    let mut results: Vec<Value> = Vec::new();
    for node in nodes {
        // limit caps successful results; skipped nodes do not count.
        if results.len() as u32 >= limit {
            break;
        }
        if node.get("ignored").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let backend_id = match node.get("backendDOMNodeId").and_then(Value::as_u64) {
            Some(id) => id,
            None => continue, // virtual AX node (text alternative, etc.)
        };
        let resolved = match client
            .send("DOM.resolveNode", json!({ "backendNodeId": backend_id }))
            .await
        {
            Ok(r) => r,
            Err(_) => continue, // detached between query and resolve
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
        match unwrap_runtime_result(&r, "Runtime.callFunctionOn")? {
            Some(v) if !v.is_null() => results.push(v),
            // Some(Value::Null) from metaOf on a non-Element node, or None from
            // `undefined` return — skip in either case.
            _ => continue,
        }
    }
    Ok(results)
}
