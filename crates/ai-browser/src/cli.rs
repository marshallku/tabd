//! TS-CLI-compatible dispatcher. Mirrors `src/cli/index.ts` semantics so a
//! Rust `ai-browser <subcommand>` invocation behaves byte-identically to the
//! TS one: same argv parsing rules, same render-result branches, same daemon
//! auto-spawn behavior. Tier 1 (16 daemon actions) is the only surface; new
//! subcommands land in phase 3b~3f.
//!
//! Why one file: per phase-3a plan, render/dispatch/args/daemon-client stay
//! together until the Rule of Three triggers a split in later phases.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::ffi::OsString;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::daemon;

// ---------------------------------------------------------------------------
// Subcommand dispatch table — Tier 1 only (16 daemon actions)
// ---------------------------------------------------------------------------

struct Spec {
    action: &'static str,
    positional: &'static [&'static str],
}

static DISPATCH: LazyLock<std::collections::HashMap<&'static str, Spec>> = LazyLock::new(|| {
    let mut m = std::collections::HashMap::new();
    m.insert("navigate", Spec { action: "tabs.navigate", positional: &["url"] });
    m.insert("eval", Spec { action: "execution.executeJs", positional: &["code"] });
    m.insert("get-text", Spec { action: "dom.getText", positional: &[] });
    m.insert("get-html", Spec { action: "dom.getHtml", positional: &[] });
    m.insert("query", Spec { action: "dom.querySelector", positional: &["selector"] });
    m.insert("screenshot", Spec { action: "capture.screenshot", positional: &[] });
    m.insert("click", Spec { action: "interaction.click", positional: &["selector"] });
    m.insert("type", Spec { action: "interaction.type", positional: &["selector", "text"] });
    m.insert("wait-selector", Spec { action: "wait.selector", positional: &["selector"] });
    m.insert("wait-url", Spec { action: "wait.url", positional: &["pattern"] });
    m.insert("cookies-get", Spec { action: "cookies.get", positional: &["url"] });
    m.insert("cookies-set", Spec { action: "cookies.set", positional: &[] });
    m.insert("cookies-delete", Spec { action: "cookies.delete", positional: &["name"] });
    m.insert("storage-get", Spec { action: "storage.get", positional: &[] });
    m.insert("storage-set", Spec { action: "storage.set", positional: &[] });
    m.insert("storage-clear", Spec { action: "storage.clear", positional: &[] });
    // Phase 3c — Tier 3 multi-tab actions.
    m.insert("open-tab", Spec { action: "tabs.open", positional: &["url"] });
    m.insert("close-tab", Spec { action: "tabs.close", positional: &[] });
    m.insert("list-tabs", Spec { action: "tabs.list", positional: &[] });
    m.insert("activate-tab", Spec { action: "tabs.activate", positional: &[] });
    m.insert("back", Spec { action: "tabs.goBack", positional: &[] });
    m.insert("forward", Spec { action: "tabs.goForward", positional: &[] });
    m.insert("reload", Spec { action: "tabs.reload", positional: &[] });
    // Phase 3d — Tier 4 interaction extras.
    m.insert("hover", Spec { action: "interaction.hover", positional: &["selector"] });
    m.insert("mouse-move", Spec { action: "interaction.mouseMove", positional: &[] });
    m.insert("scroll", Spec { action: "interaction.scroll", positional: &[] });
    m.insert("press-key", Spec { action: "interaction.pressKey", positional: &["key"] });
    m.insert("select-option", Spec { action: "interaction.selectOption", positional: &["selector"] });
    m.insert("check", Spec { action: "interaction.check", positional: &["selector"] });
    m
});

// ---------------------------------------------------------------------------
// argv parsing — TS parseArgs port
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ParsedArgs {
    positional: Vec<String>,
    options: Map<String, Value>,
    json: bool,
    output: Option<String>,
}

/// kebab-case → camelCase. Matches TS `camel()` helper.
fn camel(kebab: &str) -> String {
    let mut out = String::with_capacity(kebab.len());
    let mut upper = false;
    for ch in kebab.chars() {
        if ch == '-' {
            upper = true;
        } else if upper {
            out.push(ch.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

/// TS coerce: true/false/null/number/string. Numbers are emitted as f64 so the
/// wire shape matches TS Number (no i64/f64 split).
fn coerce(value: &str) -> Value {
    if value == "true" { return Value::Bool(true); }
    if value == "false" { return Value::Bool(false); }
    if value == "null" { return Value::Null; }
    static NUM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-?\d+(\.\d+)?$").unwrap());
    if NUM_RE.is_match(value) {
        if let Ok(n) = value.parse::<f64>() {
            if let Some(num) = serde_json::Number::from_f64(n) {
                return Value::Number(num);
            }
        }
    }
    Value::String(value.to_string())
}

fn parse_args(argv: &[String]) -> ParsedArgs {
    let mut p = ParsedArgs::default();
    let mut i = 0usize;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--json" {
            p.json = true;
            i += 1;
            continue;
        }
        if a == "--out" {
            i += 1;
            p.output = argv.get(i).cloned();
            i += 1;
            continue;
        }
        if let Some(rest) = a.strip_prefix("--no-") {
            let key = camel(rest);
            p.options.insert(key, Value::Bool(false));
            i += 1;
            continue;
        }
        if let Some(rest) = a.strip_prefix("--") {
            if let Some(eq) = rest.find('=') {
                let key = camel(&rest[..eq]);
                let raw = &rest[eq + 1..];
                p.options.insert(key, coerce(raw));
                i += 1;
            } else {
                let key = camel(rest);
                i += 1;
                let raw = argv.get(i).cloned().unwrap_or_default();
                p.options.insert(key, coerce(&raw));
                i += 1;
            }
            continue;
        }
        p.positional.push(a.clone());
        i += 1;
    }
    p
}

// ---------------------------------------------------------------------------
// Render result — TS renderResult port
// ---------------------------------------------------------------------------

/// Returns the exit code (0 success, 1 error). Side-effect: writes to stdout/
/// stderr and (on `--out`) to the file path.
async fn render_result(resp: &Value, parsed: &ParsedArgs) -> Result<i32> {
    let success = resp.get("success").and_then(Value::as_bool).unwrap_or(false);
    let data = resp.get("data");
    let error = resp.get("error").and_then(Value::as_str);

    if !success {
        if parsed.json {
            println!("{}", serde_json::to_string(resp)?);
        } else {
            eprintln!("error: {}", error.unwrap_or("unknown"));
        }
        return Ok(1);
    }

    // --out: extract bytes from data URL or { base64 } payload.
    if let Some(out_path) = &parsed.output {
        let bytes: Option<Vec<u8>> = match data {
            Some(Value::String(s)) => {
                // /^data:[^;,]+;base64,(.+)$/ — extract base64 segment.
                static DATA_URL: LazyLock<Regex> =
                    LazyLock::new(|| Regex::new(r"^data:[^;,]+;base64,(.+)$").unwrap());
                DATA_URL
                    .captures(s)
                    .and_then(|caps| caps.get(1).map(|m| m.as_str()))
                    .and_then(|b64| {
                        general_purpose::STANDARD
                            .decode(b64)
                            .or_else(|_| general_purpose::STANDARD_NO_PAD.decode(b64))
                            .ok()
                    })
            }
            Some(Value::Object(o)) => o
                .get("base64")
                .and_then(Value::as_str)
                .and_then(|b64| {
                    general_purpose::STANDARD
                        .decode(b64)
                        .or_else(|_| general_purpose::STANDARD_NO_PAD.decode(b64))
                        .ok()
                }),
            _ => None,
        };
        let Some(bytes) = bytes else {
            eprintln!(
                "--out expected a base64 data URL or {{ base64 }} payload; got something else. Use --json to inspect."
            );
            return Ok(1);
        };
        std::fs::write(out_path, &bytes).with_context(|| format!("write {out_path}"))?;
        if !parsed.json {
            println!("wrote {} bytes to {}", bytes.len(), out_path);
        }
        return Ok(0);
    }

    if parsed.json {
        let payload = data.cloned().unwrap_or(Value::Null);
        println!("{}", serde_json::to_string(&payload)?);
        return Ok(0);
    }

    match data {
        None | Some(Value::Null) => println!("ok"),
        Some(Value::String(s)) => println!("{s}"),
        Some(v) => println!("{}", serde_json::to_string_pretty(v)?),
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Daemon RPC + auto-spawn
// ---------------------------------------------------------------------------

/// Connect to an already-running daemon and send one action. Newline-delimited
/// JSON over UDS, matching the protocol that `daemon.rs` implements.
async fn send_action(socket_path: &Path, action: &str, params: Value) -> Result<Value> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connect {}", socket_path.display()))?;
    let (reader, mut writer) = stream.into_split();
    let req = json!({ "id": "cli", "action": action, "params": params }).to_string() + "\n";
    writer.write_all(req.as_bytes()).await?;
    writer.flush().await?;
    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("daemon closed without response"))?;
    serde_json::from_str(&line).context("daemon response not JSON")
}

/// Try `daemon.ping`. Returns Ok if the daemon is reachable.
async fn ping(socket_path: &Path) -> Result<()> {
    daemon::send_control_action(socket_path, "daemon.ping")
        .await
        .map(|_| ())
}

/// Make sure a daemon is reachable at the given base_dir. If none is running
/// and `AI_BROWSER_NO_AUTO_SPAWN` is unset, spawn one in detached mode and
/// poll until it's ready (or the deadline elapses).
async fn ensure_daemon(base_dir: Option<&str>) -> Result<daemon::DaemonPaths> {
    let paths = daemon::resolve_paths(base_dir)?;

    if ping(&paths.socket_path).await.is_ok() {
        return Ok(paths);
    }

    if std::env::var("AI_BROWSER_NO_AUTO_SPAWN").is_ok() {
        bail!(
            "daemon not running at {} and AI_BROWSER_NO_AUTO_SPAWN is set",
            paths.socket_path.display()
        );
    }

    // Detached spawn: child inherits no stdio (avoids zombie/SIGPIPE), and
    // carries AI_BROWSER_NO_AUTO_SPAWN so it cannot recursively respawn.
    let exe = std::env::current_exe().context("current_exe")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("daemon").arg("start");
    if let Some(b) = base_dir {
        cmd.arg("--base-dir").arg(b);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env("AI_BROWSER_NO_AUTO_SPAWN", "1");
    let child = cmd.spawn().context("spawn daemon")?;
    drop(child); // detach — init/PID 1 reaps it on exit.

    // Poll for readiness. ~12s total worst case (200ms * 60).
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if ping(&paths.socket_path).await.is_ok() {
            return Ok(paths);
        }
    }
    bail!(
        "daemon failed to become ready at {} within 12s",
        paths.socket_path.display()
    )
}

// ---------------------------------------------------------------------------
// Entry point — invoked from main.rs for `external_subcommand` argv
// ---------------------------------------------------------------------------

/// `args[0]` is the subcommand name (e.g. "navigate"), `args[1..]` are its
/// arguments. Returns the process exit code.
pub async fn run(args: Vec<OsString>) -> Result<i32> {
    let argv: Vec<String> = args.iter().map(|os| os.to_string_lossy().into_owned()).collect();
    let Some(name) = argv.first() else {
        bail!("missing subcommand");
    };
    let Some(spec) = DISPATCH.get(name.as_str()) else {
        bail!("unknown subcommand: {name}");
    };

    let mut parsed = parse_args(&argv[1..]);
    // Map positional args onto their named keys per spec.
    for (idx, key) in spec.positional.iter().enumerate() {
        if let Some(value) = parsed.positional.get(idx) {
            parsed
                .options
                .insert((*key).to_string(), Value::String(value.clone()));
        }
    }

    // TS parity: `--tab N` is a CLI shorthand for `--tabId N` (TS's
    // `applyTab` helper in src/cli/index.ts). Rewrite before sending.
    if let Some(tab) = parsed.options.remove("tab") {
        parsed.options.entry("tabId".to_string()).or_insert(tab);
    }

    // `--base-dir` is consumed by ensure_daemon, not forwarded as a param.
    let base_dir = parsed
        .options
        .remove("baseDir")
        .and_then(|v| v.as_str().map(str::to_string));

    let paths = ensure_daemon(base_dir.as_deref()).await?;

    let params = Value::Object(parsed.options.clone());
    let resp = send_action(&paths.socket_path, spec.action, params).await?;
    render_result(&resp, &parsed).await
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn camel_kebab_to_camel() {
        assert_eq!(camel("url"), "url");
        assert_eq!(camel("user-data-dir"), "userDataDir");
        assert_eq!(camel("pattern-type"), "patternType");
        assert_eq!(camel("a-b-c"), "aBC");
    }

    #[test]
    fn coerce_booleans_null() {
        assert_eq!(coerce("true"), Value::Bool(true));
        assert_eq!(coerce("false"), Value::Bool(false));
        assert_eq!(coerce("null"), Value::Null);
    }

    #[test]
    fn coerce_numbers_are_f64() {
        // Matches TS Number — integer is still f64 on the wire.
        assert_eq!(coerce("42"), json!(42.0));
        assert_eq!(coerce("-7"), json!(-7.0));
        assert_eq!(coerce("3.14"), json!(3.14));
    }

    #[test]
    fn coerce_strings_otherwise() {
        assert_eq!(coerce("hello"), json!("hello"));
        assert_eq!(coerce("True"), json!("True")); // case-sensitive
        assert_eq!(coerce("1e5"), json!("1e5")); // regex doesn't match scientific
        assert_eq!(coerce(""), json!(""));
    }

    #[test]
    fn parse_json_flag() {
        let p = parse_args(&args(&["--json"]));
        assert!(p.json);
        assert!(p.options.is_empty());
    }

    #[test]
    fn parse_out_consumes_next() {
        let p = parse_args(&args(&["--out", "shot.png"]));
        assert_eq!(p.output.as_deref(), Some("shot.png"));
    }

    #[test]
    fn parse_no_flag() {
        let p = parse_args(&args(&["--no-clear"]));
        assert_eq!(p.options.get("clear"), Some(&Value::Bool(false)));
    }

    #[test]
    fn parse_equals_form() {
        let p = parse_args(&args(&["--timeout=5000"]));
        assert_eq!(p.options.get("timeout"), Some(&json!(5000.0)));
    }

    #[test]
    fn parse_space_form() {
        let p = parse_args(&args(&["--selector", "h1"]));
        assert_eq!(p.options.get("selector"), Some(&json!("h1")));
    }

    #[test]
    fn parse_positional() {
        let p = parse_args(&args(&["https://x", "1+1"]));
        assert_eq!(p.positional, vec!["https://x", "1+1"]);
    }

    #[test]
    fn parse_kebab_to_camel_in_flags() {
        let p = parse_args(&args(&["--pattern-type", "glob"]));
        assert_eq!(p.options.get("patternType"), Some(&json!("glob")));
    }

    #[test]
    fn parse_mixed() {
        let p = parse_args(&args(&[
            "https://x",
            "--timeout=1000",
            "--json",
            "--no-raw",
            "--limit",
            "50",
        ]));
        assert_eq!(p.positional, vec!["https://x"]);
        assert!(p.json);
        assert_eq!(p.options.get("timeout"), Some(&json!(1000.0)));
        assert_eq!(p.options.get("raw"), Some(&Value::Bool(false)));
        assert_eq!(p.options.get("limit"), Some(&json!(50.0)));
    }

    #[tokio::test]
    async fn render_null_data_prints_ok_text_mode() {
        // Smoke: just verify no panic and exit code = 0. stdout capture is
        // harder under cargo test; behavior is verified e2e in cli-direct-smoke.
        let resp = json!({"id":"x","success":true});
        let parsed = ParsedArgs::default();
        let code = render_result(&resp, &parsed).await.unwrap();
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn render_error_returns_one() {
        let resp = json!({"id":"x","success":false,"error":"boom"});
        let parsed = ParsedArgs::default();
        let code = render_result(&resp, &parsed).await.unwrap();
        assert_eq!(code, 1);
    }

    #[tokio::test]
    async fn render_out_writes_png_bytes() {
        // base64 of a 4-byte PNG magic header (89 50 4E 47)
        let resp = json!({
            "id":"x","success":true,
            "data":"data:image/png;base64,iVBORw=="
        });
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        let parsed = ParsedArgs {
            output: Some(path.clone()),
            json: true, // suppress stdout chatter
            ..Default::default()
        };
        let code = render_result(&resp, &parsed).await.unwrap();
        assert_eq!(code, 0);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, vec![0x89, 0x50, 0x4E, 0x47]);
    }

    #[test]
    fn dispatch_table_has_tier_1_3_4() {
        // Tier 1 (16 actions from phase 3a) + Tier 3 (7 multi-tab from 3c) +
        // Tier 4 (6 interaction extras from 3d). Tier 5/2 land in later phases.
        let tier_1 = [
            "navigate", "eval", "get-text", "get-html", "query", "screenshot",
            "click", "type", "wait-selector", "wait-url",
            "cookies-get", "cookies-set", "cookies-delete",
            "storage-get", "storage-set", "storage-clear",
        ];
        let tier_3 = [
            "open-tab", "close-tab", "list-tabs", "activate-tab",
            "back", "forward", "reload",
        ];
        let tier_4 = [
            "hover", "mouse-move", "scroll", "press-key", "select-option", "check",
        ];
        for name in tier_1.iter().chain(tier_3.iter()).chain(tier_4.iter()) {
            assert!(DISPATCH.contains_key(name), "missing: {name}");
        }
        assert_eq!(
            DISPATCH.len(),
            tier_1.len() + tier_3.len() + tier_4.len()
        );
    }

    #[test]
    fn apply_tab_rewrites_tab_to_tab_id() {
        // Mirrors TS `applyTab` in src/cli/index.ts.
        let mut p = parse_args(&args(&["--tab", "2"]));
        if let Some(tab) = p.options.remove("tab") {
            p.options.entry("tabId".to_string()).or_insert(tab);
        }
        assert!(p.options.get("tab").is_none());
        assert_eq!(p.options.get("tabId"), Some(&json!(2.0)));
    }
}
