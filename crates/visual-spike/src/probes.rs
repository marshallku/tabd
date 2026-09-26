//! One function per open question in `docs/visual-mode-plan.md` §9.
//!
//! Every probe builds its own scratch profile and its own browser, so a
//! failure tears down its own state instead of cascading into the next probe.

use crate::pipe::{PipeBrowser, PlainBrowser, visual_base_args};
use crate::scratch::{Scratch, real_profile_dir};
use crate::sys;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    /// Answered only by a human looking at the evidence.
    Inconclusive,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::Inconclusive => "inconclusive",
        }
    }
}

pub struct Ctx {
    pub exe: PathBuf,
    pub keep: bool,
    pub interactive: bool,
    /// A site the user is already logged into, for Q6's functional check.
    pub login_url: Option<String>,
    /// Where screenshots land (outside any scratch dir, so they survive).
    pub out_dir: PathBuf,
}

pub struct Outcome {
    pub id: &'static str,
    pub question: &'static str,
    pub verdict: Verdict,
    pub answer: String,
    pub evidence: Value,
}

impl Outcome {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "question": self.question,
            "verdict": self.verdict.as_str(),
            "answer": self.answer,
            "evidence": self.evidence,
        })
    }
}

pub type ProbeResult = io::Result<(Verdict, String, Value)>;

/// `q6` is last on purpose: it aborts if any Brave is running, so it must not
/// see a browser an earlier probe leaked.
pub const PROBE_IDS: &[&str] = &[
    "q0-http-smoke",
    "q1-pipe-launch",
    "q2-webdriver",
    "q3-infobar",
    "q4-hit-test",
    "q5-singleton",
    "q7-app-id",
    "q8-dunst-actions",
    "q9-fetch-coverage",
    "q10-oopif-leak",
    "q11-popup",
    "q12-detach-paused",
    "q6-profile-copy",
];

fn question_for(id: &str) -> &'static str {
    match id {
        "q0-http-smoke" => {
            "Smoke: can the browser reach the local fixture server at all? (Every Fetch probe depends on it.)"
        }
        "q1-pipe-launch" => {
            "Q1: does Brave 154 + --remote-debugging-pipe + a persistent profile work, and does the browser exit when the pipe closes?"
        }
        "q2-webdriver" => "Q2: is navigator.webdriver false in a tab we never attach to?",
        "q3-infobar" => "Q3: does pipe mode show an infobar or any 'being debugged' UI?",
        "q5-singleton" => {
            "Q5: does a second `brave --user-data-dir=<same>` hand the URL to the running instance rather than starting a second browser?"
        }
        "q6-profile-copy" => {
            "Q6: do cookies and saved passwords still decrypt in a copy of the default profile?"
        }
        "q4-hit-test" => {
            "Q4: does DOM.getNodeForLocation hit-testing behave as expected across shadow DOM and iframe boundaries?"
        }
        "q7-app-id" => "Q7: does --class reach the Wayland app_id?",
        "q9-fetch-coverage" => {
            "Q9: with Fetch.enable on Document/Request, is there a navigation path that escapes interception?"
        }
        "q10-oopif-leak" => {
            "Q10: can an OOPIF's first document request escape before its auto-attached session is configured?"
        }
        "q11-popup" => {
            "Q11: is a popup from an owned tab stopped before its first navigation request?"
        }
        "q12-detach-paused" => {
            "Q12: what happens to a request left paused by Fetch when its session detaches?"
        }
        "q8-dunst-actions" => "Q8: can a dunst notification action be selected?",
        _ => "unknown",
    }
}

pub fn run_probe(id: &'static str, ctx: &Ctx) -> Outcome {
    if let Some(result) = crate::probes_fetch::run(id, ctx) {
        return finish(id, result);
    }
    let result = match id {
        "q1-pipe-launch" => q1_pipe_launch(ctx),
        "q2-webdriver" => q2_webdriver(ctx),
        "q3-infobar" => q3_infobar(ctx),
        "q5-singleton" => q5_singleton(ctx),
        "q6-profile-copy" => q6_profile_copy(ctx),
        "q7-app-id" => q7_app_id(ctx),
        "q8-dunst-actions" => q8_dunst_actions(ctx),
        other => Err(io::Error::other(format!("unknown probe {other}"))),
    };
    finish(id, result)
}

fn finish(id: &'static str, result: ProbeResult) -> Outcome {
    match result {
        Ok((verdict, answer, evidence)) => Outcome {
            id,
            question: question_for(id),
            verdict,
            answer,
            evidence,
        },
        Err(err) => Outcome {
            id,
            question: question_for(id),
            verdict: Verdict::Fail,
            answer: format!("probe errored: {err}"),
            evidence: json!({ "error": err.to_string() }),
        },
    }
}

// ---------------------------------------------------------------- helpers

pub fn nonce() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

pub fn attach(browser: &PipeBrowser, target_id: &str) -> io::Result<String> {
    let res = browser.call(
        "Target.attachToTarget",
        json!({ "targetId": target_id, "flatten": true }),
    )?;
    res.get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| io::Error::other("attachToTarget returned no sessionId"))
}

pub fn target_infos(browser: &PipeBrowser) -> io::Result<Vec<Value>> {
    Ok(browser
        .call("Target.getTargets", json!({}))?
        .get("targetInfos")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Poll `Target.getTargets` (which needs no session) until the target's title
/// carries `prefix`. Polling on the nonce is what makes the reading sound: a
/// title read straight after createTarget is routinely empty or stale.
fn poll_title(
    browser: &PipeBrowser,
    target_id: &str,
    prefix: &str,
    limit: Duration,
) -> io::Result<Option<String>> {
    let deadline = Instant::now() + limit;
    loop {
        for info in target_infos(browser)? {
            if info.get("targetId").and_then(Value::as_str) != Some(target_id) {
                continue;
            }
            if let Some(title) = info.get("title").and_then(Value::as_str)
                && title.starts_with(prefix)
            {
                return Ok(Some(title.to_string()));
            }
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn screenshot_of_pid(pid: u32, out: &Path, wait: Duration) -> (Option<Value>, Option<String>) {
    let Some(client) = sys::wait_for_client(pid, wait) else {
        return (
            None,
            Some(format!("no Hyprland client found for pid {pid}")),
        );
    };
    // The window has to be visible AND on top before its geometry means
    // anything to grim; otherwise the capture silently shows whatever else is
    // on screen there.
    let focused = match sys::await_visible_active(&client, Duration::from_secs(10)) {
        Ok(focused) => focused,
        Err(err) => return (Some(client), Some(err.to_string())),
    };
    std::thread::sleep(Duration::from_millis(600));
    match sys::grim_client(&focused, out) {
        Ok(()) => (Some(focused), None),
        Err(err) => (Some(focused), Some(err.to_string())),
    }
}

// ------------------------------------------------------------------- Q1

fn q1_pipe_launch(ctx: &Ctx) -> ProbeResult {
    let scratch = Scratch::new("q1", ctx.keep)?;
    let profile = scratch.child("profile");
    let mut steps = serde_json::Map::new();

    let mut browser =
        PipeBrowser::launch(&ctx.exe, &profile, &visual_base_args(), Some("about:blank"))?;

    let version = browser.call("Browser.getVersion", json!({}))?;
    steps.insert("browserGetVersion".into(), version.clone());
    steps.insert(
        "targetsAtStartup".into(),
        json!(target_infos(&browser)?.len()),
    );

    let created = browser.call("Target.createTarget", json!({ "url": "about:blank" }))?;
    let target_id = created["targetId"]
        .as_str()
        .ok_or_else(|| io::Error::other("createTarget returned no targetId"))?
        .to_string();
    let session = attach(&browser, &target_id)?;
    browser.call_session(&session, "Page.enable", json!({}))?;
    let tag = format!("q1-{}", nonce());
    browser.call_session(
        &session,
        "Page.navigate",
        json!({ "url": format!("data:text/html,<title>{tag}</title>ok") }),
    )?;
    let title = poll_title(&browser, &target_id, &tag, Duration::from_secs(5))?;
    steps.insert("navigatedTitle".into(), json!(title));
    let eval = browser.call_session(
        &session,
        "Runtime.evaluate",
        json!({ "expression": "1+1", "returnByValue": true }),
    )?;
    steps.insert("runtimeEvaluate".into(), eval);

    // The measurement: every parent-side descriptor of both pipes is gone and
    // this process is still alive.
    browser.disconnect();
    let started = Instant::now();
    let exit = browser.wait_bounded(Duration::from_secs(15));
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let exited = exit.is_some();
    steps.insert(
        "afterPipeClose".into(),
        json!({
            "exited": exited,
            "elapsedMs": elapsed_ms,
            "exitStatus": exit.map(|s| s.to_string()),
        }),
    );
    browser.shutdown();
    drop(browser);

    // Relaunch on the *same* profile dir: a persistent profile has to reopen.
    let mut second = PipeBrowser::launch(&ctx.exe, &profile, &visual_base_args(), None)?;
    let second_version = second.call("Browser.getVersion", json!({}))?;
    steps.insert("relaunchSameProfile".into(), second_version);
    steps.insert(
        "profileHasPreferences".into(),
        json!(profile.join("Default/Preferences").exists()),
    );
    steps.insert("scratchDir".into(), json!(scratch.path().to_string_lossy()));
    steps.insert(
        "browserStderrTail".into(),
        json!(tail_of(&second.stderr_log, 20)),
    );
    second.shutdown();

    // Only a *positive* observation earns a pass. If the browser was still
    // running when the window closed, we cannot tell "Brave survives a pipe
    // disconnect" from "our teardown measurement is wrong", and that
    // distinction decides whether the daemon can own the browser's lifetime.
    let verdict = if exited {
        Verdict::Pass
    } else {
        Verdict::Inconclusive
    };
    let answer = format!(
        "pipe transport works on Brave {}; after closing both parent pipe ends the browser {}",
        version
            .get("product")
            .and_then(Value::as_str)
            .unwrap_or("?"),
        if exited {
            format!("exited on its own after {elapsed_ms} ms")
        } else {
            "was STILL RUNNING after 15 s — needs corroboration before being read as 'survives a pipe disconnect'".to_string()
        }
    );
    Ok((verdict, answer, Value::Object(steps)))
}

// ------------------------------------------------------------------- Q2

fn q2_webdriver(ctx: &Ctx) -> ProbeResult {
    // Four readings are needed, and one of them comes from a browser we have
    // no CDP connection to, so the page reports out of band over HTTP.
    let reporter = sys::Reporter::start(4, Duration::from_secs(60))?;
    let port = reporter.port;
    let pages = Scratch::new("q2-pages", ctx.keep)?;
    let page_for = |tag: &str| -> io::Result<String> {
        let path = pages.child(&format!("{tag}.html"));
        std::fs::write(
            &path,
            format!(
                "<title>{tag}:pending</title><script>\
                 document.title='{tag}:'+navigator.webdriver;\
                 fetch('http://127.0.0.1:{port}/report?tag={tag}&wd='+navigator.webdriver);\
                 </script>"
            ),
        )?;
        Ok(format!("file://{}", path.display()))
    };

    // (a) Control: an ordinary Brave with no debugging transport at all.
    let control_scratch = Scratch::new("q2-control", ctx.keep)?;
    let control_url = page_for("control")?;
    {
        let _control = PlainBrowser::launch(
            &ctx.exe,
            &control_scratch.child("profile"),
            &visual_base_args(),
            Some(&control_url),
        )?;
        std::thread::sleep(Duration::from_secs(8));
    }

    // (b)-(d) the pipe browser.
    let scratch = Scratch::new("q2", ctx.keep)?;
    let profile = scratch.child("profile");
    let mut browser =
        PipeBrowser::launch(&ctx.exe, &profile, &visual_base_args(), Some("about:blank"))?;
    browser.call("Target.setDiscoverTargets", json!({ "discover": true }))?;

    // (b) a tab CDP created but never attached to.
    let unattached_url = page_for("cdp-unattached")?;
    let created = browser.call("Target.createTarget", json!({ "url": unattached_url }))?;
    let unattached_id = created["targetId"]
        .as_str()
        .ok_or_else(|| io::Error::other("no targetId"))?
        .to_string();
    let unattached_title = poll_title(
        &browser,
        &unattached_id,
        "cdp-unattached:",
        Duration::from_secs(8),
    )?;

    // (c) a tab opened the way a human opens one: a second `brave` process on
    // the same profile, forwarded by the Chromium singleton. Nothing about
    // this tab went through CDP.
    let human_url = page_for("human-opened")?;
    let _ = std::process::Command::new(&ctx.exe)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(&human_url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()?;
    std::thread::sleep(Duration::from_secs(5));
    let human_title = target_infos(&browser)?.into_iter().find_map(|info| {
        info.get("title")
            .and_then(Value::as_str)
            .filter(|t| t.starts_with("human-opened:"))
            .map(str::to_string)
    });

    // (d) the same CDP tab, now attached.
    let session = attach(&browser, &unattached_id)?;
    let attached_url = page_for("cdp-attached")?;
    browser.call_session(&session, "Page.navigate", json!({ "url": attached_url }))?;
    let attached_title = poll_title(
        &browser,
        &unattached_id,
        "cdp-attached:",
        Duration::from_secs(8),
    )?;
    browser.shutdown();

    let reports = reporter.collect();
    let reported = |tag: &str| -> Option<String> {
        reports
            .iter()
            .find(|p| sys::report_param(p, "tag").as_deref() == Some(tag))
            .and_then(|p| sys::report_param(p, "wd"))
    };
    let control = reported("control");
    let cdp_unattached = reported("cdp-unattached");
    let human_opened = reported("human-opened");
    let cdp_attached = reported("cdp-attached");

    // The design assumes an unattached tab looks like an ordinary browser.
    // A reading is only meaningful against a validated control: if the plain
    // launch did not report `false`, the control is contaminated and pipe mode
    // has not been shown to change anything either way.
    let verdict = match (control.as_deref(), cdp_unattached.as_deref()) {
        (Some("false"), Some("false")) => Verdict::Pass,
        (Some("false"), Some(_)) => Verdict::Fail,
        _ => Verdict::Inconclusive,
    };
    let answer = format!(
        "navigator.webdriver — plain launch, no debugging transport: {control:?}; pipe browser, CDP-created tab never attached: {cdp_unattached:?}; pipe browser, tab opened by a second `brave` process (the human path): {human_opened:?}; same tab after attaching: {cdp_attached:?}"
    );
    Ok((
        verdict,
        answer,
        json!({
            "control": control,
            "cdpUnattached": cdp_unattached,
            "humanOpened": human_opened,
            "cdpAttached": cdp_attached,
            "titles": {
                "cdpUnattached": unattached_title,
                "humanOpened": human_title,
                "cdpAttached": attached_title,
            },
            "rawReports": reports,
        }),
    ))
}

// ------------------------------------------------------------------- Q3

fn q3_infobar(ctx: &Ctx) -> ProbeResult {
    std::fs::create_dir_all(&ctx.out_dir)?;
    let window_args = || {
        let mut args = visual_base_args();
        args.push("--window-size=1280,800".into());
        args.push("--window-position=80,80".into());
        args
    };
    let start_url = "data:text/html,<title>q3</title><h1>q3</h1>";

    // Control: an ordinary launch, no debugging transport at all. Adding a
    // second transport to measure the control would change the control.
    let control_scratch = Scratch::new("q3-control", ctx.keep)?;
    let control_png = ctx.out_dir.join("q3-control.png");
    let (control_client, control_err, control_cmdline) = {
        let control = PlainBrowser::launch(
            &ctx.exe,
            &control_scratch.child("profile"),
            &window_args(),
            Some(start_url),
        )?;
        let (client, err) = screenshot_of_pid(control.pid, &control_png, Duration::from_secs(20));
        (client, err, sys::cmdline_of(control.pid))
    };

    // Pipe launch.
    let pipe_scratch = Scratch::new("q3-pipe", ctx.keep)?;
    let pipe_png = ctx.out_dir.join("q3-pipe.png");
    let mut pipe = PipeBrowser::launch(
        &ctx.exe,
        &pipe_scratch.child("profile"),
        &window_args(),
        Some(start_url),
    )?;
    let pipe_cmdline = sys::cmdline_of(pipe.pid());
    let (pipe_client, pipe_err) = screenshot_of_pid(pipe.pid(), &pipe_png, Duration::from_secs(20));
    pipe.shutdown();

    let automation_flag = pipe_cmdline
        .iter()
        .any(|a| a.contains("--enable-automation"));
    // Both windows are the same size showing the same page, so any extra
    // browser chrome in pipe mode changes the pixels. Byte-identical captures
    // are therefore a machine-checkable "no difference"; anything else falls
    // back to a human look.
    let identical = match (std::fs::read(&control_png), std::fs::read(&pipe_png)) {
        (Ok(a), Ok(b)) => Some(a == b),
        _ => None,
    };
    let verdict = if identical == Some(true) && control_err.is_none() && pipe_err.is_none() {
        Verdict::Pass
    } else {
        Verdict::Inconclusive
    };
    let answer = format!(
        "--enable-automation present in the pipe launch: {automation_flag}; captures byte-identical: {identical:?}. Screenshots: {}, {}",
        control_png.display(),
        pipe_png.display()
    );
    Ok((
        verdict,
        answer,
        json!({
            "controlPng": control_png.to_string_lossy(),
            "pipePng": pipe_png.to_string_lossy(),
            "controlClient": control_client,
            "pipeClient": pipe_client,
            "controlCaptureError": control_err,
            "pipeCaptureError": pipe_err,
            "controlCmdline": control_cmdline,
            "pipeCmdline": pipe_cmdline,
            "enableAutomationPresent": automation_flag,
            "capturesByteIdentical": identical,
        }),
    ))
}

// ------------------------------------------------------------------- Q5

fn q5_singleton(ctx: &Ctx) -> ProbeResult {
    let scratch = Scratch::new("q5", ctx.keep)?;
    let profile = scratch.child("profile");
    let marker = nonce();
    let page = scratch.child(&format!("singleton-{marker}.html"));
    std::fs::write(
        &page,
        format!("<title>singleton-{marker}</title><h1>singleton</h1>"),
    )?;
    let url = format!("file://{}", page.display());

    let mut browser =
        PipeBrowser::launch(&ctx.exe, &profile, &visual_base_args(), Some("about:blank"))?;
    // Discovery must be on *and acknowledged* before the second process runs,
    // or the target event we are looking for may never be delivered.
    browser.call("Target.setDiscoverTargets", json!({ "discover": true }))?;
    let mut cursor = 0usize;
    let _ = browser.drain_events(&mut cursor);

    let before = sys::brave_browser_pids();
    let started = Instant::now();
    let second = std::process::Command::new(&ctx.exe)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(&url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()?;
    let second_elapsed_ms = started.elapsed().as_millis() as u64;

    // A target is often created before its URL settles, so accept the marker
    // from targetCreated, a later targetInfoChanged, or a bounded poll.
    let needle = format!("singleton-{marker}");
    let event = browser.wait_event(&mut cursor, Duration::from_secs(10), |ev| {
        let method = ev.get("method").and_then(Value::as_str).unwrap_or("");
        (method == "Target.targetCreated" || method == "Target.targetInfoChanged")
            && ev.to_string().contains(&needle)
    });
    let polled = if event.is_none() {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let hit = target_infos(&browser)?.into_iter().find(|info| {
                info.get("url")
                    .and_then(Value::as_str)
                    .map(|u| u.contains(&needle))
                    .unwrap_or(false)
            });
            if hit.is_some() || Instant::now() >= deadline {
                break hit;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    } else {
        None
    };
    let after = sys::brave_browser_pids();
    browser.shutdown();

    let forwarded = event.is_some() || polled.is_some();
    let same_process_count = before.len() == after.len();
    let verdict = if forwarded && same_process_count && second.status.success() {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    let answer = format!(
        "second `brave --user-data-dir=<same>` exited in {second_elapsed_ms} ms (status {}); browser-process count {} → {}; the URL {} on the existing pipe connection",
        second.status,
        before.len(),
        after.len(),
        if forwarded {
            "appeared"
        } else {
            "did NOT appear"
        }
    );
    Ok((
        verdict,
        answer,
        json!({
            "secondProcessStatus": second.status.to_string(),
            "secondProcessElapsedMs": second_elapsed_ms,
            "browserPidsBefore": before,
            "browserPidsAfter": after,
            "matchedEvent": event,
            "matchedByPolling": polled,
        }),
    ))
}

// ------------------------------------------------------------------- Q7

fn q7_app_id(ctx: &Ctx) -> ProbeResult {
    let scratch = Scratch::new("q7", ctx.keep)?;
    let mut args = visual_base_args();
    args.push("--class=tabd-visual".into());
    let mut browser = PipeBrowser::launch(
        &ctx.exe,
        &scratch.child("profile"),
        &args,
        Some("data:text/html,<title>q7</title>q7"),
    )?;
    let client = sys::wait_for_client(browser.pid(), Duration::from_secs(20));
    browser.shutdown();

    let Some(client) = client else {
        return Ok((
            Verdict::Inconclusive,
            "no Hyprland client could be correlated to the launched pid".into(),
            json!({ "clients": sys::hyprctl_clients() }),
        ));
    };
    let class = client.get("class").and_then(Value::as_str).unwrap_or("");
    let initial = client
        .get("initialClass")
        .and_then(Value::as_str)
        .unwrap_or("");
    // Hyprland reports `xwayland: true` for X11 clients. `--class` setting
    // WM_CLASS on an XWayland window says nothing about the Wayland app_id.
    let xwayland = client
        .get("xwayland")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let matched = class == "tabd-visual" || initial == "tabd-visual";
    let verdict = if matched && !xwayland {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    let answer = format!(
        "client class={class:?} initialClass={initial:?} xwayland={xwayland} — {}",
        if matched && !xwayland {
            "--class reached the native Wayland app_id"
        } else if matched {
            "the flag only set WM_CLASS on an XWayland window; app_id is NOT set"
        } else {
            "--class did not reach the window at all"
        }
    );
    Ok((verdict, answer, json!({ "client": client })))
}

// ------------------------------------------------------------------- Q8

fn q8_dunst_actions(ctx: &Ctx) -> ProbeResult {
    let version = sys::stdout_of("dunst", &["--version"])
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    let rc = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".config/dunst/dunstrc"));
    let binding = rc
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|text| {
            text.lines()
                .filter(|l| l.contains("do_action"))
                .map(str::trim)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if !ctx.interactive {
        return Ok((
            Verdict::Inconclusive,
            format!(
                "needs one human click; re-run with --interactive. dunst: {version}. do_action bindings in dunstrc: {}",
                if binding.is_empty() {
                    "none".to_string()
                } else {
                    binding.join(" | ")
                }
            ),
            json!({ "dunstVersion": version, "doActionBindings": binding, "ran": false }),
        ));
    }

    let out = sys::run(
        "notify-send",
        &[
            "-A",
            "approve=Approve",
            "-A",
            "deny=Deny",
            "--wait",
            "-t",
            "20000",
            "tabd visual spike",
            "Click Approve or Deny to answer Q8",
        ],
    )?;
    let chosen = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let verdict = if chosen.is_empty() {
        Verdict::Fail
    } else {
        Verdict::Pass
    };
    Ok((
        verdict,
        format!(
            "notify-send --wait returned {:?} (empty = no action could be selected). dunst: {version}",
            chosen
        ),
        json!({
            "dunstVersion": version,
            "doActionBindings": binding,
            "ran": true,
            "stdout": chosen,
            "status": out.status.to_string(),
        }),
    ))
}

// ------------------------------------------------------------------- Q6

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CookieId {
    host: String,
    name: String,
    path: String,
    port: i64,
    /// `is_secure` in SQLite, `secure` over CDP. Part of the identity because
    /// (host, name, path, port) alone is not unique.
    secure: bool,
}

fn normalize_host(host: &str) -> String {
    host.trim_start_matches('.').to_ascii_lowercase()
}

fn q6_profile_copy(ctx: &Ctx) -> ProbeResult {
    let running = sys::brave_browser_pids();
    if !running.is_empty() {
        return Err(io::Error::other(format!(
            "refusing to copy the profile while Brave is running (pids {running:?})"
        )));
    }
    let real = real_profile_dir().ok_or_else(|| io::Error::other("no $HOME"))?;
    if !real.is_dir() {
        return Err(io::Error::other(format!(
            "real profile {} not found",
            real.display()
        )));
    }
    let singletons: Vec<String> = std::fs::read_dir(&real)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("Singleton"))
        .collect();
    if !singletons.is_empty() {
        return Err(io::Error::other(format!(
            "real profile still has {singletons:?} — Brave may not have exited cleanly"
        )));
    }

    let scratch = Scratch::new("q6", ctx.keep)?;
    let staging = scratch.child("profile");
    let copy = sys::run(
        "cp",
        &["-a", &real.to_string_lossy(), &staging.to_string_lossy()],
    )?;
    if !copy.status.success() {
        return Err(io::Error::other(format!(
            "cp -a failed: {}",
            String::from_utf8_lossy(&copy.stderr)
        )));
    }
    let pruned = prune_copy(&staging)?;

    // Which profile directory is actually in use? Brave may be on "Profile 1".
    let local_state: Value = std::fs::read_to_string(staging.join("Local State"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let profiles: Vec<String> = local_state
        .pointer("/profile/info_cache")
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_else(|| vec!["Default".to_string()]);
    let last_used = local_state
        .pointer("/profile/last_used")
        .and_then(Value::as_str)
        .unwrap_or("Default")
        .to_string();

    // Expectations, read from the pristine copy before any browser touches it.
    let mut per_profile = serde_json::Map::new();
    let mut expected: HashMap<String, Vec<CookieId>> = HashMap::new();
    let mut ambiguous_total = 0usize;
    for profile in &profiles {
        let dir = staging.join(profile);
        let cookies_db = ["Network/Cookies", "Cookies"]
            .iter()
            .map(|rel| dir.join(rel))
            .find(|p| p.is_file());
        let mut ids: Vec<CookieId> = Vec::new();
        let mut expired = 0usize;
        let mut session_scoped = 0usize;
        let now = sys::webkit_micros_now();
        if let Some(db) = &cookies_db {
            for row in sys::sqlite_query(
                db,
                "SELECT host_key, name, path, source_port, expires_utc, is_secure FROM cookies WHERE length(encrypted_value) > 0",
            )? {
                if row.len() < 6 {
                    continue;
                }
                let expires: i64 = row[4].parse().unwrap_or(0);
                if expires == 0 {
                    // `expires_utc = 0` is a session cookie. Chromium persists
                    // it but only restores it when session restore is on, so
                    // its absence says nothing about decryption. Counted, not
                    // expected.
                    session_scoped += 1;
                    continue;
                }
                if expires <= now {
                    expired += 1;
                    continue;
                }
                ids.push(CookieId {
                    host: normalize_host(&row[0]),
                    name: row[1].clone(),
                    path: row[2].clone(),
                    port: row[3].parse().unwrap_or(-1),
                    secure: row[5] == "1",
                });
            }
        }
        let unique: HashSet<&CookieId> = ids.iter().collect();
        let ambiguous = ids.len() - unique.len();
        ambiguous_total += ambiguous;

        let logins_db = dir.join("Login Data");
        let stored_passwords = if logins_db.is_file() {
            sys::sqlite_query(
                &logins_db,
                "SELECT count(*) FROM logins WHERE length(password_value) > 0",
            )
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.first().cloned()))
            .and_then(|v| v.parse::<i64>().ok())
        } else {
            None
        };

        let extension_ids: Vec<String> = std::fs::read_dir(dir.join("Extensions"))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();

        per_profile.insert(
            profile.clone(),
            json!({
                "cookiesDb": cookies_db.as_ref().map(|p| p.strip_prefix(&staging).unwrap_or(p).to_string_lossy()),
                "encryptedPersistentUnexpiredCookies": ids.len(),
                "encryptedExpiredCookiesSkipped": expired,
                "encryptedSessionCookiesNotExpected": session_scoped,
                "ambiguousIdentityRows": ambiguous,
                "storedPasswordRows": stored_passwords,
                "extensionIdsOnDisk": extension_ids.len(),
            }),
        );
        expected.insert(profile.clone(), ids);
    }

    // Now launch the copy and see which of those identities come back.
    let mut browser =
        PipeBrowser::launch(&ctx.exe, &staging, &visual_base_args(), Some("about:blank"))?;
    let cookies = browser
        .call("Storage.getCookies", json!({}))?
        .get("cookies")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let live: HashSet<CookieId> = cookies
        .iter()
        .map(|c| CookieId {
            host: normalize_host(c.get("domain").and_then(Value::as_str).unwrap_or("")),
            name: c
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            path: c
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            port: c.get("sourcePort").and_then(Value::as_i64).unwrap_or(-1),
            secure: c.get("secure").and_then(Value::as_bool).unwrap_or(false),
        })
        .collect();

    // The launched browser only opens the last-used profile, so that is the
    // only profile whose cookies `Storage.getCookies` can be expected to hold.
    let want = expected.get(&last_used).cloned().unwrap_or_default();
    // `live` is a set, so two database rows that collapse to the same
    // `CookieId` would both count as matched against a single live cookie —
    // one decrypted row masking another that never came back. Deduplicate the
    // expectation and carry the collision count into the verdict instead.
    let want_unique: Vec<CookieId> = {
        let mut seen = HashSet::new();
        want.iter()
            .filter(|id| seen.insert((*id).clone()))
            .cloned()
            .collect()
    };
    let want_ambiguous = want.len() - want_unique.len();
    let matched = want_unique.iter().filter(|id| live.contains(id)).count();
    let missing_hosts: Vec<String> = want_unique
        .iter()
        .filter(|id| !live.contains(id))
        .map(|id| format!("{}|{}", id.host, id.name))
        .take(25)
        .collect();

    let mut screenshots = serde_json::Map::new();
    std::fs::create_dir_all(&ctx.out_dir)?;
    let password_png = ctx.out_dir.join("q6-password-manager.png");
    let pm_target = browser.call(
        "Target.createTarget",
        json!({ "url": "brave://password-manager/passwords" }),
    )?;
    if let Some(id) = pm_target["targetId"].as_str() {
        let session = attach(&browser, id)?;
        std::thread::sleep(Duration::from_secs(3));
        if let Ok(shot) = browser.call_session(&session, "Page.captureScreenshot", json!({}))
            && let Some(data) = shot.get("data").and_then(Value::as_str)
        {
            write_base64_png(data, &password_png)?;
            screenshots.insert(
                "passwordManagerPng".into(),
                json!(password_png.to_string_lossy()),
            );
        }
    }
    if let Some(url) = &ctx.login_url {
        let login_png = ctx.out_dir.join("q6-login-check.png");
        let target = browser.call("Target.createTarget", json!({ "url": url }))?;
        if let Some(id) = target["targetId"].as_str() {
            let session = attach(&browser, id)?;
            std::thread::sleep(Duration::from_secs(6));
            if let Ok(shot) = browser.call_session(&session, "Page.captureScreenshot", json!({}))
                && let Some(data) = shot.get("data").and_then(Value::as_str)
            {
                write_base64_png(data, &login_png)?;
                screenshots.insert("loginCheckPng".into(), json!(login_png.to_string_lossy()));
            }
        }
    }
    let loaded_extension_targets = target_infos(&browser)?
        .iter()
        .filter(|t| {
            t.get("url")
                .and_then(Value::as_str)
                .map(|u| u.starts_with("chrome-extension://"))
                .unwrap_or(false)
        })
        .count();
    browser.shutdown();

    let total = want_unique.len();
    // Chromium *skips* cookie rows it cannot decrypt, so absence — not an
    // empty value — is the decryption signal.
    let verdict = if total == 0 || want_ambiguous > 0 {
        Verdict::Inconclusive
    } else if matched == total {
        Verdict::Pass
    } else if matched == 0 {
        Verdict::Fail
    } else {
        Verdict::Inconclusive
    };
    let answer = format!(
        "profile {last_used:?}: {matched}/{total} distinct persistent unexpired encrypted cookies from the pristine copy came back from Storage.getCookies ({want_ambiguous} colliding identity rows in this profile, {ambiguous_total} across all profiles; any collision forces an inconclusive verdict because one decrypted row could mask another). Saved passwords are NOT established by this run — the password-manager list shows plaintext metadata even when values cannot be decrypted, so that sub-question stays inconclusive until a human reveals a known password. Loaded extension targets ({loaded_extension_targets}) are a lower bound on extensions, not proof all loaded."
    );
    Ok((
        verdict,
        answer,
        json!({
            "realProfile": real.to_string_lossy(),
            "stagingProfile": staging.to_string_lossy(),
            "prunedFromCopy": pruned,
            "profiles": profiles,
            "lastUsedProfile": last_used,
            "perProfile": Value::Object(per_profile),
            "liveCookieCount": cookies.len(),
            "expectedPersistentUnexpiredEncrypted": total,
            "matched": matched,
            "ambiguousIdentityRowsInLastUsed": want_ambiguous,
            "missingSample": missing_hosts,
            "loadedExtensionTargets": loaded_extension_targets,
            "passwordSubVerdict": "inconclusive",
            "screenshots": Value::Object(screenshots),
        }),
    ))
}

/// Remove the files that must not travel with a profile copy. Operates only on
/// the copy — the real profile is never touched.
fn prune_copy(staging: &Path) -> io::Result<Vec<String>> {
    crate::scratch::assert_safe_user_data_dir(staging)?;
    let mut removed = Vec::new();
    let mut stack = vec![staging.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            let is_junk = name.starts_with("Singleton")
                || name == "lockfile"
                || name == "Crashpad"
                || name.contains("Cache");
            if is_junk {
                let res = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                if res.is_ok() {
                    removed.push(
                        path.strip_prefix(staging)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .to_string(),
                    );
                }
                continue;
            }
            if path.is_dir() && !path.is_symlink() {
                stack.push(path);
            }
        }
    }
    removed.sort();
    Ok(removed)
}

/// Last `lines` lines of a browser stderr log, for evidence.
fn tail_of(path: &Path, lines: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn write_base64_png(data: &str, out: &Path) -> io::Result<()> {
    let bytes = base64_decode(data)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad base64 screenshot"))?;
    std::fs::write(out, bytes)
}

/// Minimal standard-alphabet base64 decoder — the spike has no base64 dep and
/// only ever decodes CDP screenshots.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        } as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_params_are_parsed_out_of_a_collected_path() {
        let path = "/report?tag=control&wd=false";
        assert_eq!(sys::report_param(path, "tag").as_deref(), Some("control"));
        assert_eq!(sys::report_param(path, "wd").as_deref(), Some("false"));
        assert_eq!(sys::report_param(path, "missing"), None);
        assert_eq!(sys::report_param("/report", "tag"), None);
    }

    #[test]
    fn host_normalization_matches_sqlite_and_cdp_spellings() {
        assert_eq!(normalize_host(".Example.COM"), "example.com");
        assert_eq!(normalize_host("example.com"), "example.com");
    }

    #[test]
    fn base64_round_trips_known_vectors() {
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("aGk=").unwrap(), b"hi");
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(
            base64_decode("YW55IGNhcm5hbCBwbGVhcw==").unwrap(),
            b"any carnal pleas"
        );
        assert!(base64_decode("!!!").is_none());
    }

    #[test]
    fn q6_runs_last_so_a_leaked_browser_cannot_corrupt_it() {
        assert_eq!(*PROBE_IDS.last().unwrap(), "q6-profile-copy");
    }

    #[test]
    fn every_probe_id_has_a_question() {
        for id in PROBE_IDS {
            assert_ne!(question_for(id), "unknown", "{id}");
        }
    }
}
