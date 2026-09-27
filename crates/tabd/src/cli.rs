//! CLI dispatcher: argv parsing, dispatch table, daemon auto-spawn, result
//! rendering. Reads `tabd <subcommand>` argv, dispatches the matching daemon
//! action, and renders the response. The shape mirrors the long-retired TS
//! CLI for tooling that still parses the JSON output.
//!
//! Why one file: per the original phase-3a plan, render/dispatch/args/daemon-
//! client stay together until the Rule of Three triggers a split.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::ffi::OsString;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
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
    m.insert(
        "navigate",
        Spec {
            action: "tabs.navigate",
            positional: &["url"],
        },
    );
    m.insert(
        "eval",
        Spec {
            action: "execution.executeJs",
            positional: &["code"],
        },
    );
    m.insert(
        "get-text",
        Spec {
            action: "dom.getText",
            positional: &[],
        },
    );
    m.insert(
        "get-html",
        Spec {
            action: "dom.getHtml",
            positional: &[],
        },
    );
    m.insert(
        "query",
        Spec {
            action: "dom.querySelector",
            positional: &["selector"],
        },
    );
    m.insert(
        "screenshot",
        Spec {
            action: "capture.screenshot",
            positional: &[],
        },
    );
    m.insert(
        "click",
        Spec {
            action: "interaction.click",
            positional: &["selector"],
        },
    );
    m.insert(
        "type",
        Spec {
            action: "interaction.type",
            positional: &["selector", "text"],
        },
    );
    m.insert(
        "wait-selector",
        Spec {
            action: "wait.selector",
            positional: &["selector"],
        },
    );
    m.insert(
        "wait-url",
        Spec {
            action: "wait.url",
            positional: &["pattern"],
        },
    );
    m.insert(
        "wait-text",
        Spec {
            action: "wait.text",
            positional: &["text"],
        },
    );
    // Audit unit 6 — download interception.
    m.insert(
        "wait-download",
        Spec {
            action: "wait.download",
            positional: &[],
        },
    );
    m.insert(
        "download-dir",
        Spec {
            action: "browser.setDownloadDir",
            positional: &["dir"],
        },
    );
    m.insert(
        "downloads",
        Spec {
            action: "monitor.downloads",
            positional: &[],
        },
    );
    m.insert(
        "dialogs",
        Spec {
            action: "monitor.dialogs",
            positional: &[],
        },
    );
    m.insert(
        "dialog-policy",
        Spec {
            action: "browser.setDialogPolicy",
            positional: &["action"],
        },
    );
    m.insert(
        "cookies-get",
        Spec {
            action: "cookies.get",
            positional: &["url"],
        },
    );
    m.insert(
        "cookies-set",
        Spec {
            action: "cookies.set",
            positional: &[],
        },
    );
    m.insert(
        "cookies-delete",
        Spec {
            action: "cookies.delete",
            positional: &["name"],
        },
    );
    m.insert(
        "storage-get",
        Spec {
            action: "storage.get",
            positional: &[],
        },
    );
    m.insert(
        "storage-set",
        Spec {
            action: "storage.set",
            positional: &[],
        },
    );
    m.insert(
        "storage-clear",
        Spec {
            action: "storage.clear",
            positional: &[],
        },
    );
    // Phase 3c — Tier 3 multi-tab actions.
    m.insert(
        "open-tab",
        Spec {
            action: "tabs.open",
            positional: &["url"],
        },
    );
    m.insert(
        "close-tab",
        Spec {
            action: "tabs.close",
            positional: &[],
        },
    );
    m.insert(
        "list-tabs",
        Spec {
            action: "tabs.list",
            positional: &[],
        },
    );
    m.insert(
        "activate-tab",
        Spec {
            action: "tabs.activate",
            positional: &[],
        },
    );
    m.insert(
        "back",
        Spec {
            action: "tabs.goBack",
            positional: &[],
        },
    );
    m.insert(
        "forward",
        Spec {
            action: "tabs.goForward",
            positional: &[],
        },
    );
    m.insert(
        "reload",
        Spec {
            action: "tabs.reload",
            positional: &[],
        },
    );
    // Audit unit 5b — file upload via DOM.setFileInputFiles.
    m.insert(
        "upload",
        Spec {
            action: "interaction.uploadFile",
            positional: &["selector", "path"],
        },
    );
    // Phase 3d — Tier 4 interaction extras.
    m.insert(
        "hover",
        Spec {
            action: "interaction.hover",
            positional: &["selector"],
        },
    );
    m.insert(
        "mouse-move",
        Spec {
            action: "interaction.mouseMove",
            positional: &[],
        },
    );
    m.insert(
        "scroll",
        Spec {
            action: "interaction.scroll",
            positional: &[],
        },
    );
    m.insert(
        "press-key",
        Spec {
            action: "interaction.pressKey",
            positional: &["key"],
        },
    );
    m.insert(
        "select-option",
        Spec {
            action: "interaction.selectOption",
            positional: &["selector"],
        },
    );
    m.insert(
        "check",
        Spec {
            action: "interaction.check",
            positional: &["selector"],
        },
    );
    // Phase 3e1 — Tier 5 monitor/diagnostic.
    m.insert(
        "console-logs",
        Spec {
            action: "monitor.consoleLogs",
            positional: &[],
        },
    );
    m.insert(
        "page-errors",
        Spec {
            action: "monitor.pageErrors",
            positional: &[],
        },
    );
    m.insert(
        "metrics",
        Spec {
            action: "capture.metrics",
            positional: &[],
        },
    );
    // Audit unit 5c — viewport emulation.
    m.insert(
        "set-viewport",
        Spec {
            action: "emulation.setViewport",
            positional: &["width", "height"],
        },
    );
    m.insert(
        "set-user-agent",
        Spec {
            action: "emulation.setUserAgent",
            positional: &["userAgent"],
        },
    );
    m.insert(
        "add-init-script",
        Spec {
            action: "emulation.addInitScript",
            positional: &["source"],
        },
    );
    m.insert(
        "summary",
        Spec {
            action: "dom.contentSummary",
            positional: &[],
        },
    );
    // Phase 3e2 — network-logs (event-stitching, body fetch deferred).
    m.insert(
        "network-logs",
        Spec {
            action: "monitor.networkLogs",
            positional: &[],
        },
    );
    // Phase 3f — Tier 2 (login automation). `secret-put` is handled outside
    // this table (custom branch in run()) because it must keep plaintext
    // off argv via --from-env/--from-file/--stdin.
    m.insert(
        "wait-network-idle",
        Spec {
            action: "wait.networkIdle",
            positional: &[],
        },
    );
    m.insert(
        "secret-list",
        Spec {
            action: "secrets.list",
            positional: &[],
        },
    );
    m.insert(
        "secret-delete",
        Spec {
            action: "secrets.delete",
            positional: &["id"],
        },
    );
    m.insert(
        "type-secret",
        Spec {
            action: "interaction.typeSecret",
            positional: &["selector"],
        },
    );
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

/// TS coerce: true/false/null/number/string. Integers stay i64 so daemon
/// handlers using `.as_u64()` / `.as_i64()` see numbers correctly; floats
/// (with a `.`) fall through to f64 (matches TS Number wire shape, since
/// integer JSON tokens have no decimal point either).
fn coerce(value: &str) -> Value {
    if value == "true" {
        return Value::Bool(true);
    }
    if value == "false" {
        return Value::Bool(false);
    }
    if value == "null" {
        return Value::Null;
    }
    static NUM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-?\d+(\.\d+)?$").unwrap());
    // A numeric string with a leading zero in its integer part (007,
    // 01099998888) is an identifier, not a quantity: JSON numbers can't
    // round-trip the leading zero, so coercing it would silently corrupt the
    // value. Keep it a string. This is what lets `type --text 01012345678`
    // survive intact (the Toss sign-in phone field, and OTPs / zip codes
    // generally). A bare "0" and fractionals like "0.5" are still numbers.
    let int_part = value.strip_prefix('-').unwrap_or(value);
    let has_leading_zero =
        int_part.len() > 1 && int_part.starts_with('0') && int_part.as_bytes()[1].is_ascii_digit();
    if NUM_RE.is_match(value) && !has_leading_zero {
        if !value.contains('.')
            && let Ok(n) = value.parse::<i64>()
        {
            return Value::Number(serde_json::Number::from(n));
        }
        if let Ok(n) = value.parse::<f64>()
            && let Some(num) = serde_json::Number::from_f64(n)
        {
            return Value::Number(num);
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
                match argv.get(i) {
                    // Bare flag (end of argv, or the next token is another
                    // flag) ⇒ true — the behavior commands.md always
                    // documented. Values that genuinely start with `--` go
                    // through the `--flag=VALUE` form.
                    None => {
                        p.options.insert(key, Value::Bool(true));
                    }
                    Some(next) if next.starts_with("--") => {
                        p.options.insert(key, Value::Bool(true));
                    }
                    Some(next) => {
                        p.options.insert(key, coerce(next));
                        i += 1;
                    }
                }
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

/// Map a wire `errorCode` to a process exit code so scripts can branch without
/// parsing stderr: 3 daemon unreachable, 4 timeout, 5 selector/tab not found,
/// 1 anything else. 0 (success) and 2 (usage error) are assigned elsewhere.
fn exit_code_for_error(code: Option<&str>) -> i32 {
    match code {
        Some("daemon_unreachable") => 3,
        Some("timeout") => 4,
        Some("selector_not_found") | Some("tab_not_found") => 5,
        _ => 1,
    }
}

/// Returns the exit code (0 success, nonzero error — see `exit_code_for_error`).
/// Side-effect: writes to stdout/stderr and (on `--out`) to the file path.
async fn render_result(resp: &Value, parsed: &ParsedArgs) -> Result<i32> {
    let success = resp
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let data = resp.get("data");
    let error = resp.get("error").and_then(Value::as_str);

    if !success {
        let code = resp.get("errorCode").and_then(Value::as_str);
        if parsed.json {
            println!("{}", serde_json::to_string(resp)?);
        } else {
            match code {
                Some(c) => eprintln!("error: {} [{c}]", error.unwrap_or("unknown")),
                None => eprintln!("error: {}", error.unwrap_or("unknown")),
            }
        }
        return Ok(exit_code_for_error(code));
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
            Some(Value::Object(o)) => o.get("base64").and_then(Value::as_str).and_then(|b64| {
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
// `tabd browser` — the owner entry point
// ---------------------------------------------------------------------------

/// Open the human's browser, and the given urls in it.
///
/// This is what the default-browser registration calls: a `.desktop` file's
/// `Exec=tabd browser %U` on Linux, the wrapper `.app`'s `on open location`
/// handler on macOS. `%U` can hand over several urls at once, and zero urls
/// means "just make sure the browser is up".
///
/// All the work happens in the daemon, over the pipe it already owns. The CLI
/// deliberately does not spawn a browser itself: letting two `tabd browser`
/// invocations race a launch, or relying on the Chromium singleton to forward
/// argv to a running instance, is exactly what `browser.ensure`'s serialized
/// lifecycle exists to avoid.
pub async fn run_browser(urls: Vec<String>, base_dir: Option<&str>, json: bool) -> Result<i32> {
    // Partitioned, not all-or-nothing: `%U` can hand over a mixed list, and
    // sending one `tabd:` url along with the rest made `parse_urls` fail the
    // whole request — so `tabd browser tabd://x https://example.com` opened
    // nothing when the user expected example.com.
    let (checks, urls) = split_delivery_checks(&urls);
    if !checks.is_empty() {
        record_delivery_checks(&checks, base_dir)?;
        if urls.is_empty() {
            return Ok(0);
        }
    }

    let paths = ensure_visual_daemon(base_dir).await?;
    let resp = send_action(
        &paths.socket_path,
        "browser.ensure",
        json!({ "urls": urls }),
    )
    .await?;

    if json {
        println!("{}", serde_json::to_string(&resp)?);
    }

    if !resp
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let code = resp.get("errorCode").and_then(Value::as_str);
        if !json {
            let message = resp
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            match code {
                Some(c) => eprintln!("error: {message} [{c}]"),
                None => eprintln!("error: {message}"),
            }
        }
        return Ok(exit_code_for_error(code));
    }

    let data = resp.get("data").cloned().unwrap_or(Value::Null);
    let launched = data
        .get("launched")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let results = data
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut failed = 0usize;
    for result in &results {
        let url = result.get("url").and_then(Value::as_str).unwrap_or("?");
        match result.get("status").and_then(Value::as_str) {
            Some("failed") => {
                failed += 1;
                let why = result
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                eprintln!("error: could not open {url}: {why}");
            }
            // "requested" is not "opened": the url went on the browser's
            // command line during a launch, and nothing confirmed a target
            // was created for it.
            Some(status) if !json => println!("{status} {url}"),
            _ => {}
        }
    }

    if launched {
        // Once per browser start, not once per clicked link. Measured:
        // without this setting a browser that exits comes back with a single
        // new-tab page and no restore prompt — and the daemon owns the pipe,
        // so a daemon crash closes the browser. tabd cannot turn it on
        // (Chromium MAC-protects the pref), so all it can do is say so.
        //
        // `unknown` warns too, and is the *normal* answer on a first launch:
        // the profile has no `Preferences` yet because Chromium has not
        // written one. Staying quiet there would mean never warning the one
        // person who most needs it — someone setting the profile up.
        match data.get("sessionRestore").and_then(Value::as_str) {
            Some("off") => eprintln!(
                "warning: this browser profile does not reopen its tabs on startup, so if the \
                 tabd daemon stops your tabs are lost with no prompt. Turn on \
                 \"Continue where you left off\" in the browser's startup settings."
            ),
            Some("unknown") => eprintln!(
                "warning: could not read this profile's startup setting (a profile that has \
                 never been launched has none yet). If the tabd daemon stops, the browser \
                 closes with it — turn on \"Continue where you left off\" in the browser's \
                 startup settings so your tabs come back."
            ),
            _ => {}
        }
        if !json && results.is_empty() {
            println!("browser started");
        }
    } else if !json && results.is_empty() {
        println!("browser already running");
    }

    Ok(if failed > 0 { 1 } else { 0 })
}

/// How long one health probe may take. A daemon can be listening and still
/// never answer (wedged mid-launch, stopped in a debugger), and an unbounded
/// probe would hang `tabd browser` — which, on macOS, also blocks the wrapper
/// app's event queue and therefore every link clicked after it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Overall budget for getting a visual daemon to answer, spawn included.
const VISUAL_START_DEADLINE: Duration = Duration::from_secs(12);

/// What is listening on a daemon socket, as far as `tabd browser` cares.
enum Listening {
    Visual,
    /// A daemon answered, but it is not one we may drive.
    WrongMode(String),
    /// Nothing answered in time.
    Nothing,
}

/// Probe a daemon socket. Reads `daemon.health` rather than `daemon.ping`
/// because the question is "is this the *right* daemon" — `ping` would happily
/// accept a headless one squatting on the same base dir.
async fn probe_visual_daemon(socket_path: &Path) -> Listening {
    let resp = match tokio::time::timeout(
        PROBE_TIMEOUT,
        daemon::send_control_action(socket_path, "daemon.health"),
    )
    .await
    {
        Ok(Ok(resp)) => resp,
        _ => return Listening::Nothing,
    };
    match resp.pointer("/data/mode").and_then(Value::as_str) {
        Some("visual") => Listening::Visual,
        Some(other) => Listening::WrongMode(other.to_owned()),
        // Answered, but not in a shape we recognize. Refusing is the safe
        // reading: driving an unknown daemon is worse than saying so.
        None => Listening::WrongMode("unrecognized".to_owned()),
    }
}

fn wrong_mode_error(mode: &str, socket_path: &Path) -> anyhow::Error {
    anyhow!(
        "a {mode} daemon is listening on {} — `tabd browser` needs a visual one. \
         Stop it, or pass --base-dir to use a different directory.",
        socket_path.display()
    )
}

/// Split urls into delivery checks and real urls.
///
/// Case-insensitive: LaunchServices matches `CFBundleURLSchemes` without
/// regard to case, so `open TABD://hello` reaches the wrapper — and a
/// case-sensitive test here would let it fall through to the daemon, be
/// rejected as an unknown scheme, and be swallowed by the wrapper's `try`.
/// That is exactly the silent-no-op failure this check was added to remove.
fn split_delivery_checks(urls: &[String]) -> (Vec<String>, Vec<String>) {
    let scheme = crate::service::TEST_SCHEME;
    urls.iter().cloned().partition(|url| {
        url.split_once(':')
            .is_some_and(|(prefix, _)| prefix.eq_ignore_ascii_case(scheme))
    })
}

/// Record delivery checks for the private scheme.
///
/// Answered here, before the daemon is involved, because the daemon only
/// accepts `http`/`https`/`file`. Recorded rather than only printed: the macOS
/// wrapper runs this with no terminal attached, so the log is the evidence.
fn record_delivery_checks(checks: &[String], base_dir: Option<&str>) -> Result<()> {
    // The same hygiene `validate_url` applies, and for the same reason: this
    // goes into a log that `tabd service status` prints verbatim to a
    // terminal, so a newline forges a log line and an escape sequence reaches
    // the terminal.
    for url in checks {
        if url.chars().any(char::is_control) {
            bail!("invalid url: contains a control character");
        }
    }
    let paths = daemon::resolve_paths_for(base_dir, daemon::DaemonMode::Visual)?;
    std::fs::create_dir_all(&paths.base_dir)
        .with_context(|| format!("create {}", paths.base_dir.display()))?;
    let log = paths.base_dir.join("url-delivery.log");
    let line = format!("{} {}\n", unix_timestamp(), checks.join(" "));

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("open {}", log.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("write {}", log.display()))?;

    for url in checks {
        println!("url delivery ok: {url}");
    }
    eprintln!("recorded in {}", log.display());
    Ok(())
}

/// Seconds since the epoch. A real timestamp would mean a date dependency for
/// one log line.
fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reach the visual daemon, starting it if it is not there.
///
/// Unlike [`ensure_daemon`] this waits only for *reachable*, never for
/// `ready`: a visual daemon whose browser is closed is **intentionally** not
/// ready, and `browser.ensure` is the thing that makes it ready again. Gating
/// on readiness would mean a closed browser could never be reopened.
async fn ensure_visual_daemon(base_dir: Option<&str>) -> Result<daemon::DaemonPaths> {
    let paths = daemon::resolve_paths_for(base_dir, daemon::DaemonMode::Visual)?;

    match probe_visual_daemon(&paths.socket_path).await {
        Listening::Visual => return Ok(paths),
        Listening::WrongMode(mode) => return Err(wrong_mode_error(&mode, &paths.socket_path)),
        Listening::Nothing => {}
    }

    if std::env::var("TABD_NO_AUTO_SPAWN").is_ok() {
        bail!(
            "no visual daemon at {} and TABD_NO_AUTO_SPAWN is set",
            paths.socket_path.display()
        );
    }

    let exe = std::env::current_exe().context("current_exe")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("daemon").arg("start").arg("--visual");
    if let Some(b) = base_dir {
        cmd.arg("--base-dir").arg(b);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env("TABD_NO_AUTO_SPAWN", "1");
    drop(cmd.spawn().context("spawn visual daemon")?); // detached; init reaps it

    // The mode check repeats inside the loop, not just before the spawn: a
    // headless daemon can bind the socket in the gap, and handing it
    // `browser.ensure` would drive the wrong browser.
    let deadline = Instant::now() + VISUAL_START_DEADLINE;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        match probe_visual_daemon(&paths.socket_path).await {
            Listening::Visual => return Ok(paths),
            Listening::WrongMode(mode) => return Err(wrong_mode_error(&mode, &paths.socket_path)),
            Listening::Nothing => {}
        }
        if Instant::now() >= deadline {
            bail!(
                "visual daemon did not answer at {} within {}s",
                paths.socket_path.display(),
                VISUAL_START_DEADLINE.as_secs()
            );
        }
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::split_delivery_checks;

    fn split(urls: &[&str]) -> (Vec<String>, Vec<String>) {
        let owned: Vec<String> = urls.iter().map(|u| (*u).to_string()).collect();
        split_delivery_checks(&owned)
    }

    #[test]
    fn the_private_scheme_is_matched_case_insensitively() {
        // LaunchServices matches CFBundleURLSchemes without regard to case, so
        // `open TABD://x` reaches the wrapper. A case-sensitive test here let
        // it fall through to the daemon, be rejected, and be swallowed by the
        // wrapper's `try` — silently doing nothing while looking fine.
        for spelling in ["tabd://x", "TABD://x", "TaBd://x"] {
            let (checks, rest) = split(&[spelling]);
            assert_eq!(checks.len(), 1, "{spelling}");
            assert!(rest.is_empty(), "{spelling}");
        }
    }

    #[test]
    fn a_mixed_list_is_partitioned_not_rejected() {
        // `%U` can hand over both at once, and the entry claims both schemes.
        // Sending the lot to the daemon made `parse_urls` fail the whole
        // request, so the real url opened nothing.
        let (checks, rest) = split(&["tabd://probe", "https://example.com", "file:///tmp/x"]);
        assert_eq!(checks, vec!["tabd://probe"]);
        assert_eq!(rest, vec!["https://example.com", "file:///tmp/x"]);
    }

    #[test]
    fn ordinary_urls_are_untouched() {
        let (checks, rest) = split(&["https://example.com"]);
        assert!(checks.is_empty());
        assert_eq!(rest, vec!["https://example.com"]);
        // A host that merely starts with the scheme name is not the scheme.
        let (checks, rest) = split(&["https://tabd.example.com"]);
        assert!(checks.is_empty());
        assert_eq!(rest.len(), 1);
    }
}

/// Every daemon action the CLI can reach, for tests that must stay in step
/// with the dispatch table rather than hard-coding a list that rots.
#[cfg(test)]
pub(crate) fn dispatch_actions() -> Vec<&'static str> {
    DISPATCH.values().map(|spec| spec.action).collect()
}

// ---------------------------------------------------------------------------
// Daemon RPC + auto-spawn
// ---------------------------------------------------------------------------

/// ensure_daemon + send_action, with connection-level failures folded into a
/// synthesized error envelope so `render_result` stays the single rendering
/// path (`--json` keeps emitting JSON even when the daemon is unreachable, and
/// the exit code maps from `errorCode: "daemon_unreachable"`).
async fn dispatch_action(base_dir: Option<&str>, action: &str, params: Value) -> Value {
    let result = async {
        let paths = ensure_daemon(base_dir).await?;
        send_action(&paths.socket_path, action, params).await
    }
    .await;
    result.unwrap_or_else(|err| {
        json!({
            "id": "cli",
            "success": false,
            "error": format!("{err:#}"),
            "errorCode": "daemon_unreachable",
        })
    })
}

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
/// and `TABD_NO_AUTO_SPAWN` is unset, spawn one in detached mode and poll
/// until it's ready (or the deadline elapses).
async fn ensure_daemon(base_dir: Option<&str>) -> Result<daemon::DaemonPaths> {
    let paths = daemon::resolve_paths(base_dir)?;

    if ping(&paths.socket_path).await.is_ok() {
        return Ok(paths);
    }

    if std::env::var("TABD_NO_AUTO_SPAWN").is_ok() {
        bail!(
            "daemon not running at {} and TABD_NO_AUTO_SPAWN is set",
            paths.socket_path.display()
        );
    }

    // Detached spawn: child inherits no stdio (avoids zombie/SIGPIPE), and
    // carries TABD_NO_AUTO_SPAWN so it cannot recursively respawn.
    let exe = std::env::current_exe().context("current_exe")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("daemon").arg("start");
    if let Some(b) = base_dir {
        cmd.arg("--base-dir").arg(b);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env("TABD_NO_AUTO_SPAWN", "1");
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
    let argv: Vec<String> = args
        .iter()
        .map(|os| os.to_string_lossy().into_owned())
        .collect();
    let Some(name) = argv.first() else {
        bail!("missing subcommand");
    };

    // Phase 3f: `secret-put` keeps plaintext off argv via --from-env /
    // --from-file / --stdin. Routed through a custom branch instead of the
    // generic DISPATCH so the source is read locally before forwarding the
    // value via the daemon's secrets.put action.
    if name == "secret-put" {
        return run_secret_put(&argv[1..]).await;
    }

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

    // `upload`: resolve the file path against the CALLER's cwd before it
    // crosses to the daemon (whose cwd is wherever it was first spawned).
    if name == "upload" {
        let Some(raw) = parsed.options.get("path").and_then(Value::as_str) else {
            eprintln!("upload: usage: tabd upload <selector> <file>");
            return Ok(2);
        };
        match std::fs::canonicalize(raw) {
            Ok(abs) => {
                // canonicalize proves existence, not readability — open it so
                // an unreadable file fails here (exit 2), not in chromium.
                if let Err(err) = std::fs::File::open(&abs) {
                    eprintln!("upload: cannot read {raw}: {err}");
                    return Ok(2);
                }
                parsed.options.insert(
                    "path".to_string(),
                    Value::String(abs.to_string_lossy().into_owned()),
                );
            }
            Err(err) => {
                eprintln!("upload: cannot resolve {raw}: {err}");
                return Ok(2);
            }
        }
    }

    // `download-dir`: resolve against the CALLER's cwd and verify it's an
    // existing, writable directory before the daemon ever sees it — chromium
    // writes downloads there, so a non-writable dir would fail silently later.
    if name == "download-dir" {
        let Some(raw) = parsed.options.get("dir").and_then(Value::as_str) else {
            eprintln!("download-dir: usage: tabd download-dir <dir>");
            return Ok(2);
        };
        let abs = match std::fs::canonicalize(raw) {
            Ok(p) => p,
            Err(err) => {
                eprintln!("download-dir: cannot resolve {raw}: {err}");
                return Ok(2);
            }
        };
        if !abs.is_dir() {
            eprintln!("download-dir: not a directory: {raw}");
            return Ok(2);
        }
        // Unique-named probe (never a fixed filename) so a same-named user
        // file in the download dir can't be truncated/deleted. Auto-removed.
        if let Err(err) = tempfile::Builder::new()
            .prefix(".tabd-probe-")
            .tempfile_in(&abs)
        {
            eprintln!("download-dir: directory is not writable: {raw} ({err})");
            return Ok(2);
        }
        parsed.options.insert(
            "dir".to_string(),
            Value::String(abs.to_string_lossy().into_owned()),
        );
    }

    // `--base-dir` is consumed by ensure_daemon, not forwarded as a param.
    let base_dir = parsed
        .options
        .remove("baseDir")
        .and_then(|v| v.as_str().map(str::to_string));

    let params = Value::Object(parsed.options.clone());
    let resp = dispatch_action(base_dir.as_deref(), spec.action, params).await;
    render_result(&resp, &parsed).await
}

/// Custom handler for `secret-put`. Refuses plaintext via argv; pulls the
/// value from one of `--from-env VAR`, `--from-file PATH`, or `--stdin`.
async fn run_secret_put(args: &[String]) -> Result<i32> {
    // Treat bare `--stdin` like `--stdin=true` so parse_args doesn't swallow
    // the next flag as its value. Mirrors the TS `secret-put` CLI handler.
    let normalized: Vec<String> = args
        .iter()
        .map(|a| {
            if a == "--stdin" {
                "--stdin=true".to_string()
            } else {
                a.clone()
            }
        })
        .collect();
    let parsed = parse_args(&normalized);
    let label = parsed
        .options
        .get("label")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let from_env = parsed
        .options
        .get("fromEnv")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let from_file = parsed
        .options
        .get("fromFile")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let from_stdin = parsed
        .options
        .get("stdin")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut picked = 0;
    if from_env.is_some() {
        picked += 1;
    }
    if from_file.is_some() {
        picked += 1;
    }
    if from_stdin {
        picked += 1;
    }
    if picked == 0 {
        eprintln!("secret-put: provide --from-env VAR, --from-file PATH, or --stdin");
        return Ok(2);
    }
    if picked > 1 {
        eprintln!("secret-put: choose exactly one of --from-env, --from-file, --stdin");
        return Ok(2);
    }

    let value: String = if let Some(var) = from_env {
        match std::env::var(&var) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("secret-put: env var {var} is not set");
                return Ok(2);
            }
        }
    } else if let Some(path) = from_file {
        let raw = std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
        raw.trim_end_matches(['\r', '\n']).to_string()
    } else {
        let mut buf = String::new();
        use std::io::Read;
        std::io::stdin().read_to_string(&mut buf)?;
        buf.trim_end_matches(['\r', '\n']).to_string()
    };

    if value.is_empty() {
        eprintln!("secret-put: value is empty");
        return Ok(2);
    }

    let base_dir = parsed
        .options
        .get("baseDir")
        .and_then(|v| v.as_str().map(str::to_string));
    let mut params = serde_json::Map::new();
    params.insert("value".to_string(), Value::String(value));
    if let Some(lbl) = label {
        params.insert("label".to_string(), Value::String(lbl));
    }
    let resp = dispatch_action(base_dir.as_deref(), "secrets.put", Value::Object(params)).await;
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
    fn coerce_numbers_are_integer_or_float() {
        // Integers stay i64 so daemon `.as_u64()` works on filter/timeout
        // params. Floats keep their decimal precision via f64.
        assert_eq!(coerce("42"), json!(42));
        assert_eq!(coerce("-7"), json!(-7));
        assert_eq!(coerce("1.5"), json!(1.5));
        // Sanity: integer token serializes without a decimal point.
        assert_eq!(serde_json::to_string(&coerce("42")).unwrap(), "42");
    }

    #[test]
    fn coerce_strings_otherwise() {
        assert_eq!(coerce("hello"), json!("hello"));
        assert_eq!(coerce("True"), json!("True")); // case-sensitive
        assert_eq!(coerce("1e5"), json!("1e5")); // regex doesn't match scientific
        assert_eq!(coerce(""), json!(""));
    }

    #[test]
    fn coerce_leading_zero_stays_string() {
        // Identifiers (phone numbers, OTPs, zip codes) keep their leading zero —
        // coercing to a number would drop it. Toss's phone field is the driver:
        // `type --text 01099998888` must reach the daemon as a string.
        assert_eq!(coerce("01099998888"), json!("01099998888"));
        assert_eq!(coerce("007"), json!("007"));
        assert_eq!(coerce("-07"), json!("-07"));
        assert_eq!(coerce("00"), json!("00"));
        // But a bare zero and fractionals are still real numbers.
        assert_eq!(coerce("0"), json!(0));
        assert_eq!(coerce("0.5"), json!(0.5));
        assert_eq!(coerce("-0.5"), json!(-0.5));
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
    fn parse_bare_flag_at_end_is_true() {
        let p = parse_args(&args(&["--mobile"]));
        assert_eq!(p.options.get("mobile"), Some(&Value::Bool(true)));
    }

    #[test]
    fn parse_bare_flag_before_another_flag_is_true() {
        // The documented `--flag` ⇒ true contract: a bare flag must not
        // swallow the following flag as its value.
        let p = parse_args(&args(&["--visible-only", "--limit", "5"]));
        assert_eq!(p.options.get("visibleOnly"), Some(&Value::Bool(true)));
        assert_eq!(p.options.get("limit"), Some(&json!(5)));
    }

    #[test]
    fn parse_flag_still_consumes_plain_value() {
        let p = parse_args(&args(&["--text", "Sign in", "--timeout", "1000"]));
        assert_eq!(p.options.get("text"), Some(&json!("Sign in")));
        assert_eq!(p.options.get("timeout"), Some(&json!(1000)));
    }

    #[test]
    fn parse_no_flag() {
        let p = parse_args(&args(&["--no-clear"]));
        assert_eq!(p.options.get("clear"), Some(&Value::Bool(false)));
    }

    #[test]
    fn parse_equals_form() {
        let p = parse_args(&args(&["--timeout=5000"]));
        assert_eq!(p.options.get("timeout"), Some(&json!(5000)));
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
        assert_eq!(p.options.get("timeout"), Some(&json!(1000)));
        assert_eq!(p.options.get("raw"), Some(&Value::Bool(false)));
        assert_eq!(p.options.get("limit"), Some(&json!(50)));
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
    async fn render_error_maps_error_code_to_exit_code() {
        let parsed = ParsedArgs::default();
        for (code_str, expected) in [
            ("daemon_unreachable", 3),
            ("timeout", 4),
            ("selector_not_found", 5),
            ("tab_not_found", 5),
            ("eval_error", 1),
            ("internal", 1),
        ] {
            let resp = json!({"id":"x","success":false,"error":"boom","errorCode":code_str});
            let code = render_result(&resp, &parsed).await.unwrap();
            assert_eq!(code, expected, "errorCode {code_str}");
        }
    }

    #[test]
    fn exit_code_unknown_or_missing_code_is_one() {
        assert_eq!(exit_code_for_error(None), 1);
        assert_eq!(exit_code_for_error(Some("not_a_real_code")), 1);
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
    fn dispatch_table_has_all_tiers() {
        // Tier 1 (16) + Tier 3 (7) + Tier 4 (6) + Tier 5 (5) + Tier 2 partial
        // (4: wait-network-idle, secret-list, secret-delete, type-secret).
        // `secret-put` is a custom branch outside DISPATCH so the table
        // length excludes it.
        let tier_1 = [
            "navigate",
            "eval",
            "get-text",
            "get-html",
            "query",
            "screenshot",
            "click",
            "type",
            "wait-selector",
            "wait-url",
            "cookies-get",
            "cookies-set",
            "cookies-delete",
            "storage-get",
            "storage-set",
            "storage-clear",
        ];
        let tier_3 = [
            "open-tab",
            "close-tab",
            "list-tabs",
            "activate-tab",
            "back",
            "forward",
            "reload",
        ];
        let tier_4 = [
            "hover",
            "mouse-move",
            "scroll",
            "press-key",
            "select-option",
            "check",
        ];
        let tier_5 = [
            "console-logs",
            "page-errors",
            "metrics",
            "summary",
            "network-logs",
        ];
        let tier_2 = [
            "wait-network-idle",
            "secret-list",
            "secret-delete",
            "type-secret",
        ];
        // Audit units 3+5+6: dialogs, wait-text, upload, viewport, downloads.
        let tier_6 = [
            "wait-text",
            "dialogs",
            "dialog-policy",
            "upload",
            "set-viewport",
            "set-user-agent",
            "add-init-script",
            "wait-download",
            "download-dir",
            "downloads",
        ];
        for name in tier_1
            .iter()
            .chain(tier_3.iter())
            .chain(tier_4.iter())
            .chain(tier_5.iter())
            .chain(tier_2.iter())
            .chain(tier_6.iter())
        {
            assert!(DISPATCH.contains_key(name), "missing: {name}");
        }
        assert_eq!(
            DISPATCH.len(),
            tier_1.len() + tier_3.len() + tier_4.len() + tier_5.len() + tier_2.len() + tier_6.len()
        );
        // Ensure secret-put is NOT in the table (custom branch only).
        assert!(!DISPATCH.contains_key("secret-put"));
    }

    #[test]
    fn apply_tab_rewrites_tab_to_tab_id() {
        // Mirrors TS `applyTab` in src/cli/index.ts.
        let mut p = parse_args(&args(&["--tab", "2"]));
        if let Some(tab) = p.options.remove("tab") {
            p.options.entry("tabId".to_string()).or_insert(tab);
        }
        assert!(p.options.get("tab").is_none());
        assert_eq!(p.options.get("tabId"), Some(&json!(2)));
    }
}
