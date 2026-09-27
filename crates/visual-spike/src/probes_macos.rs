//! macOS OS-integration probes (Q14, Q16, Q17, Q18).
//!
//! Everything here touches system state, so every probe is bounded and
//! reversible: artifacts live in the scratch dir, a LaunchAgent gets a unique
//! label and is booted out again, the Keychain items are created by this probe
//! and deleted by it, and **the user's default browser is never changed**.
//! A probe that cannot clean up says so in its evidence rather than leaving
//! the mess implicit.

#![cfg(target_os = "macos")]

use crate::pipe::{PipeBrowser, visual_base_args};
use crate::probes::{Ctx, ProbeResult, Verdict, nonce};
use crate::scratch::Scratch;
use crate::sys;
use serde_json::{Value, json};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// Stable prefix for everything this module registers with the system, so a
/// later run can find residue from an earlier *failed* run. Checking only the
/// current nonce would never notice an interrupted run's leftovers.
const SPIKE_PREFIX: &str = "dev.tabd.spike";

const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

pub fn run(id: &str, ctx: &Ctx) -> Option<ProbeResult> {
    Some(match id {
        "q14-launchservices" => q14_launchservices(ctx),
        "q16-launchagent-ui" => q16_launchagent_ui(ctx),
        "q17-keychain-rebuild" => q17_keychain_rebuild(ctx),
        "q18-seatbelt" => q18_seatbelt(ctx),
        _ => return None,
    })
}

// ------------------------------------------------------------------- Q14

fn q14_launchservices(ctx: &Ctx) -> ProbeResult {
    let scratch = Scratch::new("q14", ctx.keep)?;
    let marker = nonce();

    // (a) Handoff. The decisive evidence is the nonce arriving on the pipe we
    // already hold — a process-count comparison cannot show that the URL went
    // to *this* instance, because unrelated browser processes start and exit
    // on their own.
    let page = scratch.child(&format!("q14-{marker}.html"));
    std::fs::write(&page, format!("<title>q14-{marker}</title>handoff"))?;
    let url = format!("file://{}", page.display());

    let mut browser = PipeBrowser::launch(
        &ctx.exe,
        &scratch.child("profile"),
        &visual_base_args(),
        Some("about:blank"),
    )?;
    browser.call("Target.setDiscoverTargets", json!({ "discover": true }))?;
    let mut cursor = 0usize;
    let _ = browser.drain_events(&mut cursor);

    let pids_before = sys::browser_pids(&ctx.exe);
    let open_result = sys::run("open", &[&url])?;
    let arrived = browser
        .wait_event(&mut cursor, Duration::from_secs(12), |ev| {
            ev.to_string().contains(&marker)
        })
        .is_some();
    let pids_after = sys::browser_pids(&ctx.exe);
    browser.shutdown();

    // (b) Registration. Build a wrapper .app declaring http/https, ad-hoc sign
    // it, register it, and ask LaunchServices whether it is now a claimant —
    // without touching which handler is the default.
    let registration = q14_register_wrapper(&scratch, &marker);
    // Residue check runs at the END too: a record left by this run is exactly
    // what the start-of-run check cannot see.
    let stale = stale_launchservices_bundles();

    let arrived_text = if arrived {
        "arrived on the existing pipe connection"
    } else {
        "did NOT arrive on the existing pipe connection"
    };
    let verdict = match &registration {
        Ok(reg) if reg.registered && reg.unregistered => Verdict::Pass,
        Ok(_) => Verdict::Inconclusive,
        Err(_) => Verdict::Fail,
    };
    let answer = format!(
        "(a) a link opened through LaunchServices {arrived_text} — that is the decisive signal; browser-process count {} → {} is corroborative only. (b) wrapper .app registration: {}",
        pids_before.len(),
        pids_after.len(),
        match &registration {
            Ok(reg) => format!(
                "registered as an http/https claimant: {}, and unregistered again: {}",
                reg.registered, reg.unregistered
            ),
            Err(err) => format!("failed: {err}"),
        }
    );
    Ok((
        verdict,
        answer,
        json!({
            "handoff": {
                "markerArrivedOnExistingPipe": arrived,
                "openExitStatus": open_result.status.to_string(),
                "browserPidsBefore": pids_before,
                "browserPidsAfter": pids_after,
                "note": "one run against the default handler as configured right now; not a general law",
            },
            "configuredHttpHandler": configured_http_handler(),
            "registration": match &registration {
                Ok(reg) => json!({
                    "bundleId": reg.bundle_id,
                    "registered": reg.registered,
                    "unregistered": reg.unregistered,
                    "claimedSchemes": reg.claimed_schemes,
                }),
                Err(err) => json!({ "error": err.to_string() }),
            },
            "staleSpikeBundlesFound": stale,
            "defaultBrowserChanged": false,
        }),
    ))
}

struct WrapperRegistration {
    bundle_id: String,
    registered: bool,
    unregistered: bool,
    claimed_schemes: Vec<String>,
}

fn q14_register_wrapper(scratch: &Scratch, marker: &str) -> io::Result<WrapperRegistration> {
    let bundle_id = format!("{SPIKE_PREFIX}.wrapper.{marker}");
    let app = scratch.child(&format!("tabd-spike-{marker}.app"));
    let macos_dir = app.join("Contents/MacOS");
    std::fs::create_dir_all(&macos_dir)?;
    std::fs::write(
        app.join("Contents/Info.plist"),
        wrapper_info_plist(&bundle_id),
    )?;
    let binary = macos_dir.join("tabd-spike");
    std::fs::write(&binary, "#!/bin/sh\nexit 0\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))?;
    }
    // Ad-hoc signature: enough for LaunchServices to accept the bundle.
    let sign = sys::run(
        "codesign",
        &["--force", "--sign", "-", &app.to_string_lossy()],
    )?;
    if !sign.status.success() {
        return Err(io::Error::other(format!(
            "codesign failed: {}",
            String::from_utf8_lossy(&sign.stderr)
        )));
    }

    let _ = sys::run(LSREGISTER, &["-f", &app.to_string_lossy()]);
    std::thread::sleep(Duration::from_secs(2));
    let (registered, claimed_schemes) = lsregister_claims(&bundle_id);

    // `lsregister -dump` is eventually consistent: checking once, two seconds
    // after `-u`, reported the bundle gone while a record for it was still in
    // the database minutes later. Poll until it really disappears, and report
    // honestly if it does not.
    let _ = sys::run(LSREGISTER, &["-u", &app.to_string_lossy()]);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut still_there = true;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        still_there = lsregister_claims(&bundle_id).0;
        if !still_there {
            // Confirm it stays gone rather than trusting one reading.
            std::thread::sleep(Duration::from_secs(3));
            still_there = lsregister_claims(&bundle_id).0;
            if !still_there {
                break;
            }
        }
    }

    Ok(WrapperRegistration {
        bundle_id,
        registered,
        unregistered: !still_there,
        claimed_schemes,
    })
}

fn wrapper_info_plist(bundle_id: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>{bundle_id}</string>
  <key>CFBundleName</key><string>tabd spike</string>
  <key>CFBundleExecutable</key><string>tabd-spike</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleShortVersionString</key><string>0.0</string>
  <key>LSUIElement</key><true/>
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key><string>Web site URL</string>
      <key>CFBundleURLSchemes</key>
      <array><string>http</string><string>https</string></array>
    </dict>
  </array>
</dict>
</plist>
"#
    )
}

/// Ask LaunchServices whether `bundle_id` is registered and which schemes it
/// claims. `lsregister -dump` is not a stable interface, so a parse failure is
/// reported rather than guessed at.
fn lsregister_claims(bundle_id: &str) -> (bool, Vec<String>) {
    let Ok(out) = sys::run(LSREGISTER, &["-dump"]) else {
        return (false, Vec::new());
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(at) = text.find(bundle_id) else {
        return (false, Vec::new());
    };
    // Look at the record around the hit for the schemes it binds.
    let window = &text[at.saturating_sub(4000)..(at + 4000).min(text.len())];
    let mut schemes = Vec::new();
    for scheme in ["http:", "https:"] {
        if window.contains(scheme) {
            schemes.push(scheme.trim_end_matches(':').to_string());
        }
    }
    (true, schemes)
}

/// Residue from an earlier interrupted run, found by the stable prefix rather
/// than the current nonce.
fn stale_launchservices_bundles() -> Vec<String> {
    let Ok(out) = sys::run(LSREGISTER, &["-dump"]) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut found: Vec<String> = text
        .split_whitespace()
        .filter(|tok| tok.starts_with(SPIKE_PREFIX))
        .map(|tok| {
            tok.trim_matches(|c: char| !c.is_ascii_graphic())
                .to_string()
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

fn configured_http_handler() -> Value {
    let out = sys::stdout_of(
        "defaults",
        &[
            "read",
            "com.apple.LaunchServices/com.apple.launchservices.secure",
            "LSHandlers",
        ],
    );
    let handler = out
        .split('}')
        .find(|block| block.contains("LSHandlerURLScheme = https"))
        .and_then(|block| {
            block
                .lines()
                .find(|l| l.contains("LSHandlerRoleAll"))
                .map(|l| l.trim().to_string())
        });
    json!(handler)
}

// ------------------------------------------------------------------- Q16

fn q16_launchagent_ui(ctx: &Ctx) -> ProbeResult {
    let scratch = Scratch::new("q16", ctx.keep)?;
    let marker = nonce();
    let label = format!("{SPIKE_PREFIX}.q16.{marker}");
    let results = scratch.child("agent-results.json");
    let ran_marker = scratch.child("agent-ran");
    let script = scratch.child("agent.sh");

    // The agent tries BOTH paths the question asks about. A notification and a
    // modal dialog are delivered and authorized differently, so a result for
    // one says nothing about the other. The dialog is bounded with `giving up
    // after` so it cannot sit on screen waiting for a human.
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
echo ran > "{ran}"
osascript -e 'display notification "tabd spike q16" with title "tabd spike"' 2>"{dir}/notify.err"
notify_status=$?
osascript -e 'display dialog "tabd spike q16 (closes itself)" giving up after 3' 2>"{dir}/dialog.err" >/dev/null
dialog_status=$?
printf '{{"notify":%d,"dialog":%d}}\n' "$notify_status" "$dialog_status" > "{results}"
"#,
            ran = ran_marker.display(),
            dir = scratch.path().display(),
            results = results.display(),
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    }

    let plist_path = scratch.child("agent.plist");
    std::fs::write(&plist_path, agent_plist(&label, &script, scratch.path()))?;

    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}");
    let bootstrap = sys::run(
        "launchctl",
        &["bootstrap", &domain, &plist_path.to_string_lossy()],
    )?;
    let bootstrapped = bootstrap.status.success();

    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline && !results.exists() {
        std::thread::sleep(Duration::from_millis(500));
    }
    let agent_ran = ran_marker.exists();
    let statuses: Value = std::fs::read_to_string(&results)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);

    let target = format!("{domain}/{label}");
    let bootout = sys::run("launchctl", &["bootout", &target])?;
    let booted_out = bootout.status.success()
        || String::from_utf8_lossy(&bootout.stderr).contains("No such process");
    let stale = stale_launch_agents();

    // "osascript exited 0" is NOT "a notification appeared on screen", and
    // there is no dependable oracle for delivery, so the stronger claim is
    // never made.
    let notify_ok = statuses.get("notify").and_then(Value::as_i64) == Some(0);
    let dialog_ok = statuses.get("dialog").and_then(Value::as_i64) == Some(0);
    let verdict = if !bootstrapped || !agent_ran {
        Verdict::Fail
    } else if notify_ok && dialog_ok {
        Verdict::Pass
    } else {
        Verdict::Inconclusive
    };
    let answer = format!(
        "LaunchAgent bootstrapped: {bootstrapped}; it ran: {agent_ran}. From inside it, `display notification` exited {:?} and a bounded `display dialog` exited {:?}. Exit 0 means the Apple Event was accepted, NOT that anything appeared on screen — there is no dependable delivery oracle, so that stronger claim is not made. Booted out again: {booted_out}.",
        statuses.get("notify"),
        statuses.get("dialog")
    );
    Ok((
        verdict,
        answer,
        json!({
            "label": label,
            "bootstrapped": bootstrapped,
            "bootstrapStderr": String::from_utf8_lossy(&bootstrap.stderr).trim(),
            "agentRan": agent_ran,
            "statuses": statuses,
            "notifyStderr": read_trimmed(&scratch.child("notify.err")),
            "dialogStderr": read_trimmed(&scratch.child("dialog.err")),
            "agentStdout": read_trimmed(&scratch.child("agent.out")),
            "agentStderr": read_trimmed(&scratch.child("agent.err")),
            "bootedOut": booted_out,
            "staleSpikeAgentsFound": stale,
            "focusState": focus_state(),
            "deliveryObserved": false,
        }),
    ))
}

fn agent_plist(label: &str, script: &Path, dir: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>/bin/sh</string><string>{script}</string></array>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>{dir}/agent.out</string>
  <key>StandardErrorPath</key><string>{dir}/agent.err</string>
  <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#,
        label = label,
        script = script.display(),
        dir = dir.display(),
    )
}

/// Agents left behind by an earlier interrupted run, found by the stable
/// prefix rather than this run's label.
fn stale_launch_agents() -> Vec<String> {
    sys::stdout_of("launchctl", &["list"])
        .lines()
        .filter(|l| l.contains(&format!("{SPIKE_PREFIX}.q16")))
        .map(|l| l.split_whitespace().last().unwrap_or(l).to_string())
        .collect()
}

/// Focus / Do Not Disturb can suppress a notification silently, which would
/// look like a failure but is a settings artifact. Best effort only.
fn focus_state() -> Value {
    let out = sys::stdout_of(
        "defaults",
        &[
            "read",
            "com.apple.controlcenter",
            "NSStatusItem Visible FocusModes",
        ],
    );
    if out.is_empty() || out.starts_with('<') {
        Value::Null
    } else {
        json!(out)
    }
}

fn read_trimmed(path: &Path) -> Value {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => json!(text.trim()),
        _ => Value::Null,
    }
}

// ------------------------------------------------------------------- Q17

/// A tiny Security-framework reader. The Keychain ACL binds to the *reading
/// binary's* code identity, so the reader has to be a real binary that we can
/// rebuild — `security(1)` would only ever test `security`'s own identity.
const KEYCHAIN_READER_C: &str = r#"
#include <stdio.h>
#include <string.h>
#include <Security/Security.h>
/* Must affect the EMITTED binary, not just the source: a tag inside a comment
   is stripped by the compiler and the rebuilt binary has an identical CDHash,
   which makes the whole rebuild experiment vacuous. */
volatile const char *tabd_build_tag = "tabd-spike-build-%TAG%";

int main(int argc, char **argv) {
    fprintf(stderr, "build %s\n", (const char *)tabd_build_tag);
    if (argc < 3) { fprintf(stderr, "usage: reader <service> <account>\n"); return 2; }
    UInt32 length = 0; void *data = NULL;
    /* Both service AND account are required: passing a NULL account yields
       errSecParam (-50), which looks like a denial but is a bad call. */
    OSStatus status = SecKeychainFindGenericPassword(
        NULL, (UInt32)strlen(argv[1]), argv[1],
        (UInt32)strlen(argv[2]), argv[2], &length, &data, NULL);

    if (status == errSecSuccess) {
        if (data) SecKeychainItemFreeContent(NULL, data);
        printf("ok %u\n", (unsigned)length);
        return 0;
    }
    fprintf(stderr, "status %d\n", (int)status);
    return 1;
}
"#;

fn build_keychain_reader(scratch: &Scratch, tag: &str) -> io::Result<std::path::PathBuf> {
    let binary = scratch.child(&format!("reader-{tag}"));
    build_keychain_reader_at(scratch, tag, &binary)?;
    Ok(binary)
}

/// Compile the reader to an explicit path, so a rebuild can overwrite the
/// previous binary rather than landing beside it under a new name.
fn build_keychain_reader_at(scratch: &Scratch, tag: &str, binary: &Path) -> io::Result<()> {
    let source = scratch.child(&format!("reader-{tag}.c"));
    std::fs::write(&source, KEYCHAIN_READER_C.replace("%TAG%", tag))?;
    let out = sys::run(
        "clang",
        &[
            // The SecKeychain API is deprecated but is the one that exercises
            // the ACL this question is about; the warning is not useful here.
            "-w",
            "-framework",
            "Security",
            "-framework",
            "CoreFoundation",
            "-o",
            &binary.to_string_lossy(),
            &source.to_string_lossy(),
        ],
    )?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "clang failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// The binary's code-directory hash, so "we really did change the code
/// identity" is asserted rather than assumed.
fn code_hash(binary: &Path) -> Option<String> {
    let out = sys::run("codesign", &["-dvvv", &binary.to_string_lossy()]).ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    text.lines()
        .find(|l| l.starts_with("CDHash="))
        .map(|l| l.trim().to_string())
}

/// Run a command with a wall-clock bound, killing it if it overruns.
///
/// A Security call can open a modal authorization dialog and block forever, so
/// nothing here is ever awaited unbounded.
fn run_bounded(program: &Path, args: &[&str], limit: Duration) -> (Option<i32>, String, bool) {
    let Ok(mut child) = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    else {
        return (None, "spawn failed".into(), false);
    };
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut err = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    use std::io::Read;
                    let _ = stderr.read_to_string(&mut err);
                }
                return (status.code(), err.trim().to_string(), false);
            }
            Ok(None) => {}
            Err(_) => return (None, "wait failed".into(), false),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return (None, "timed out".into(), true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Run a Keychain-touching command, but give up the moment macOS puts up an
/// authorization dialog rather than waiting out the timeout.
///
/// Killing the requesting process can leave that dialog orphaned on the user's
/// screen — which this probe did once before this guard existed. The pid set
/// is snapshotted first so a SecurityAgent that was already running is not
/// mistaken for ours.
fn run_keychain_bounded(
    program: &Path,
    args: &[&str],
    limit: Duration,
    // `abort_on_prompt = false` waits the prompt out instead of abandoning it:
    // that is the interactive case, where a human is there to answer and
    // killing the requester would defeat the measurement.
    abort_on_prompt: bool,
) -> (Option<i32>, String, bool, bool) {
    let before = security_agent_pids();
    let Ok(mut child) = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    else {
        return (None, "spawn failed".into(), false, false);
    };
    let deadline = Instant::now() + limit;
    let mut prompted = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut err = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    use std::io::Read;
                    let _ = stderr.read_to_string(&mut err);
                }
                return (status.code(), err.trim().to_string(), false, prompted);
            }
            Ok(None) => {}
            Err(_) => return (None, "wait failed".into(), false, prompted),
        }
        let prompt_now = security_agent_pids().iter().any(|p| !before.contains(p));
        if prompt_now {
            prompted = true;
            if abort_on_prompt {
                let _ = child.kill();
                let _ = child.wait();
                return (
                    None,
                    "a Keychain authorization dialog appeared; the read was abandoned immediately so the dialog is not left waiting on a killed process".into(),
                    false,
                    true,
                );
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return (None, "timed out".into(), true, prompted);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Deletes the probe's own Keychain item when it goes out of scope, including
/// on the `?` error paths that previously bypassed cleanup entirely.
struct KeychainItem {
    service: String,
    deleted: std::cell::Cell<bool>,
}

impl KeychainItem {
    fn create(service: &str, value: &str, trusted: &Path) -> io::Result<Self> {
        let add = sys::run(
            "security",
            &[
                "add-generic-password",
                "-s",
                service,
                "-a",
                "tabd-spike",
                "-w",
                value,
                "-T",
                &trusted.to_string_lossy(),
                "-U",
            ],
        )?;
        if !add.status.success() {
            return Err(io::Error::other(format!(
                "could not create the test Keychain item: {}",
                String::from_utf8_lossy(&add.stderr)
            )));
        }
        Ok(KeychainItem {
            service: service.to_string(),
            deleted: std::cell::Cell::new(false),
        })
    }

    /// Delete now and report whether it worked, so the evidence can say so.
    fn delete(&self) -> bool {
        if self.deleted.get() {
            return true;
        }
        let ok = sys::run(
            "security",
            &["delete-generic-password", "-s", &self.service],
        )
        .map(|o| o.status.success())
        .unwrap_or(false);
        self.deleted.set(ok);
        ok
    }
}

impl Drop for KeychainItem {
    fn drop(&mut self) {
        if !self.deleted.get() && !self.delete() {
            eprintln!(
                "[visual-spike] WARNING: could not delete test Keychain item {}",
                self.service
            );
        }
    }
}

fn security_agent_pids() -> Vec<u32> {
    sys::stdout_of("pgrep", &["-x", "SecurityAgent"])
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

fn q17_keychain_rebuild(ctx: &Ctx) -> ProbeResult {
    // This question is *about* whether macOS prompts, so running it
    // necessarily risks putting a modal dialog on the user's screen. It
    // therefore only runs when a human is there to answer — the same rule the
    // other human-in-the-loop checks follow.
    if !ctx.interactive {
        return Ok((
            Verdict::Inconclusive,
            "needs a human: this measures whether macOS puts up a Keychain authorization dialog, so it must not run unattended. Re-run with --interactive and answer (or deny) the prompt if one appears.".into(),
            json!({ "ran": false, "reason": "requires --interactive" }),
        ));
    }
    let scratch = Scratch::new("q17", ctx.keep)?;
    let marker = nonce();
    let service = format!("tabd-spike-q17-{marker}");

    // The rebuild has to land at the SAME path, or this measures "a different
    // binary at a different path" and says nothing about code identity.
    let reader_path = scratch.child("reader");
    build_keychain_reader_at(&scratch, "a", &reader_path)?;
    // The reader must be named in the item's ACL, or its very first read would
    // prompt and the rebuild would prove nothing. `security` alone would make
    // `security` the trusted application, not our binary.
    let item = KeychainItem::create(&service, &format!("value-{marker}"), &reader_path)?;

    let agents_before = security_agent_pids();
    let (first_code, first_err, first_timeout, first_prompted) = run_keychain_bounded(
        &reader_path,
        &[&service, "tabd-spike"],
        Duration::from_secs(90),
        false,
    );
    let first_hash = code_hash(&reader_path);
    // Rebuild IN PLACE: a new tag changes the code hash while the path stays
    // the same, which is the only way this measures code identity rather than
    // "a different binary somewhere else".
    build_keychain_reader_at(&scratch, "b", &reader_path)?;
    let second_hash = code_hash(&reader_path);
    let (second_code, second_err, second_timeout, second_prompted) = run_keychain_bounded(
        &reader_path,
        &[&service, "tabd-spike"],
        Duration::from_secs(90),
        false,
    );
    let agents_after = security_agent_pids();
    let new_agents: Vec<u32> = agents_after
        .iter()
        .copied()
        .filter(|p| !agents_before.contains(p))
        .collect();

    let cleaned = item.delete();

    let baseline_ok = first_code == Some(0);
    let hash_changed = first_hash.is_some() && second_hash.is_some() && first_hash != second_hash;
    let verdict = if !baseline_ok || !hash_changed {
        // Without a working baseline, or without the code hash actually
        // changing, the rebuild tells us nothing.
        Verdict::Inconclusive
    } else if second_code == Some(0) && !second_prompted {
        // "Succeeded" is not enough: succeeding *after a second prompt* is the
        // opposite answer to this question.
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    let answer = format!(
        "baseline read by the ACL-named binary at a fixed path: {} (a prompt appeared: {first_prompted}). After rebuilding IN PLACE at that same path — code hash changed: {hash_changed} — the read: {} (a prompt appeared again: {second_prompted}). A SecurityAgent process appeared during the run: {} — attributed by comparing the pid set before and after, not by merely finding one. Test item deleted: {cleaned}.{}",
        describe_read(first_code, &first_err, first_timeout),
        describe_read(second_code, &second_err, second_timeout),
        !new_agents.is_empty(),
        if new_agents.is_empty() {
            String::new()
        } else {
            " A Keychain dialog may still be on screen; dismiss it if so.".to_string()
        }
    );
    Ok((
        verdict,
        answer,
        json!({
            "service": service,
            "createdWith": "security add-generic-password -T <reader> -U (ACL names the reader; NOT -A)",
            "readerPath": reader_path.to_string_lossy(),
            "codeHashBefore": first_hash,
            "codeHashAfter": second_hash,
            "codeHashChanged": hash_changed,
            "baseline": { "exitCode": first_code, "stderr": first_err, "timedOut": first_timeout, "promptAppeared": first_prompted },
            "afterRebuild": { "exitCode": second_code, "stderr": second_err, "timedOut": second_timeout, "promptAppeared": second_prompted },
            "securityAgentPidsBefore": agents_before,
            "securityAgentPidsAfter": agents_after,
            "securityAgentPidsNew": new_agents,
            "testItemDeleted": cleaned,
            "note": "this is a test item's ACL; Chrome's own Safe Storage item is a different ACL and nothing here is extrapolated to it",
        }),
    ))
}

fn describe_read(code: Option<i32>, err: &str, timed_out: bool) -> String {
    if timed_out {
        "BLOCKED (killed at the timeout — consistent with a modal prompt)".to_string()
    } else {
        match code {
            Some(0) => "succeeded".to_string(),
            Some(c) => format!("failed with exit {c} ({err})"),
            None => format!("did not run ({err})"),
        }
    }
}

// ------------------------------------------------------------------- Q18

const SEATBELT_PROFILE: &str = r#"(version 1)
(allow default)
;; The three things the design wants an agent sandbox to deny on macOS.
(deny mach-lookup (global-name "com.apple.SecurityServer"))
(deny mach-lookup (global-name "com.apple.securityd"))
(deny mach-lookup (global-name "com.apple.security.agent"))
(deny mach-lookup (global-name "com.apple.coreservices.appleevents"))
(deny mach-lookup (global-name "com.apple.windowserver.active"))
(deny mach-lookup (global-name "com.apple.screencapture"))
"#;

/// One capability: the same command run outside and inside the sandbox.
struct Capability {
    name: &'static str,
    /// Unsandboxed first. Without a passing baseline a sandboxed failure is
    /// uninterpretable — a missing binary, a TCC denial or a bad argument all
    /// look identical to a Seatbelt denial.
    baseline: (Option<i32>, String, bool),
    sandboxed: (Option<i32>, String, bool),
}

impl Capability {
    fn denied_by_sandbox(&self) -> bool {
        self.baseline.0 == Some(0) && self.sandboxed.0 != Some(0)
    }
    fn to_json(&self) -> Value {
        json!({
            "baselineExit": self.baseline.0,
            "baselineStderr": self.baseline.1,
            "baselineTimedOut": self.baseline.2,
            "sandboxedExit": self.sandboxed.0,
            "sandboxedStderr": self.sandboxed.1,
            "sandboxedTimedOut": self.sandboxed.2,
            "baselineWorked": self.baseline.0 == Some(0),
            "deniedBySandbox": self.denied_by_sandbox(),
        })
    }
}

fn q18_seatbelt(ctx: &Ctx) -> ProbeResult {
    let scratch = Scratch::new("q18", ctx.keep)?;
    let marker = nonce();
    let profile = scratch.child("deny.sb");
    std::fs::write(&profile, SEATBELT_PROFILE)?;

    // This probe creates and reads its OWN Keychain item. Borrowing q17's
    // would make "item missing" and "sandbox denied" the same nonzero status.
    let service = format!("tabd-spike-q18-{marker}");
    let reader = build_keychain_reader(&scratch, "q18")?;
    // Guarded: every later `?` in this probe would otherwise leak the item.
    let item = KeychainItem::create(&service, &format!("value-{marker}"), &reader)?;

    let sandbox = Path::new("/usr/bin/sandbox-exec");
    let profile_arg = profile.to_string_lossy().to_string();
    let reader_arg = reader.to_string_lossy().to_string();
    let shot = scratch.child("shot.png").to_string_lossy().to_string();
    let control_file = scratch.child("control.txt");
    std::fs::write(&control_file, "control\n")?;
    let control_arg = control_file.to_string_lossy().to_string();
    // A loopback socket is a deterministic network control; DNS would add
    // cache and resolver-service noise that Seatbelt itself can perturb.
    let net_script = scratch.child("loopback.py");
    std::fs::write(
        &net_script,
        "import socket\ns=socket.socket();s.bind(('127.0.0.1',0));s.listen(1)\nc=socket.create_connection(s.getsockname());s.accept();print('ok')\n",
    )?;
    let net_arg = net_script.to_string_lossy().to_string();

    let bound = Duration::from_secs(12);
    let mut keychain_prompted = false;
    // Reading a Keychain item from a freshly built, ad-hoc binary makes macOS
    // ask the user, and a probe must not put a modal dialog on someone's
    // screen unattended. So this capability runs only under --interactive,
    // exactly like the other human-in-the-loop checks.
    let mut capabilities = Vec::new();
    if ctx.interactive {
        let (base_code, base_err, base_to, base_prompted) =
            // A human is present in this branch, so the prompt is waited on
            // rather than abandoned — aborting would defeat the measurement.
            run_keychain_bounded(&reader, &[&service, "tabd-spike"], Duration::from_secs(90), false);
        let (sand_code, sand_err, sand_to, _) = run_keychain_bounded(
            sandbox,
            &["-f", &profile_arg, &reader_arg, &service, "tabd-spike"],
            Duration::from_secs(90),
            false,
        );
        keychain_prompted = base_prompted;
        capabilities.push(Capability {
            name: "keychain-read",
            baseline: (base_code, base_err, base_to),
            sandboxed: (sand_code, sand_err, sand_to),
        });
    }
    // `osascript -e "return 1+1"` is NOT an Apple Event to another app — it is
    // a local script evaluation, and calling this "apple-events" claimed far
    // more than it measures.
    capabilities.push(Capability {
        name: "osascript-eval",
        baseline: run_bounded(
            Path::new("/usr/bin/osascript"),
            &["-e", "return 1 + 1"],
            bound,
        ),
        sandboxed: run_bounded(
            sandbox,
            &[
                "-f",
                &profile_arg,
                "/usr/bin/osascript",
                "-e",
                "return 1 + 1",
            ],
            bound,
        ),
    });

    // The real question is whether a *cross-application* Apple Event can be
    // blocked. That needs an Automation (TCC) grant, and asking for one puts a
    // modal prompt on the user's screen — so the sandboxed half runs only when
    // the unsandboxed baseline already succeeds, i.e. the grant exists.
    let cross_script = "tell application \"Finder\" to get name";
    let cross_baseline = run_bounded(
        Path::new("/usr/bin/osascript"),
        &["-e", cross_script],
        Duration::from_secs(8),
    );
    let cross_sandboxed = if cross_baseline.0 == Some(0) {
        run_bounded(
            sandbox,
            &["-f", &profile_arg, "/usr/bin/osascript", "-e", cross_script],
            bound,
        )
    } else {
        (
            None,
            "skipped: no unsandboxed baseline, so Automation permission is probably not granted; this probe will not trigger a TCC prompt to get one".to_string(),
            false,
        )
    };
    capabilities.push(Capability {
        name: "cross-app-apple-event",
        baseline: cross_baseline,
        sandboxed: cross_sandboxed,
    });
    capabilities.push(Capability {
        name: "screen-capture",
        baseline: run_bounded(
            Path::new("/usr/sbin/screencapture"),
            &["-x", "-R", "0,0,10,10", &shot],
            bound,
        ),
        sandboxed: run_bounded(
            sandbox,
            &[
                "-f",
                &profile_arg,
                "/usr/sbin/screencapture",
                "-x",
                "-R",
                "0,0,10,10",
                &shot,
            ],
            bound,
        ),
    });
    // Controls: ordinary work must still succeed INSIDE the sandbox, otherwise
    // the profile is simply breaking everything.
    capabilities.push(Capability {
        name: "control-file-read",
        baseline: run_bounded(Path::new("/bin/cat"), &[&control_arg], bound),
        sandboxed: run_bounded(
            sandbox,
            &["-f", &profile_arg, "/bin/cat", &control_arg],
            bound,
        ),
    });
    capabilities.push(Capability {
        name: "control-loopback-socket",
        baseline: run_bounded(Path::new("/usr/bin/python3"), &[&net_arg], bound),
        sandboxed: run_bounded(
            sandbox,
            &["-f", &profile_arg, "/usr/bin/python3", &net_arg],
            bound,
        ),
    });

    let cleaned = item.delete();

    // The three capabilities the design actually needs denied. A capability
    // that was never measured is NOT evidence of denial, so it can only make
    // the result inconclusive — never a pass.
    const REQUIRED: [&str; 3] = ["keychain-read", "screen-capture", "cross-app-apple-event"];
    let measured_required: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|name| {
            capabilities
                .iter()
                .any(|c| c.name == *name && c.baseline.0 == Some(0))
        })
        .collect();
    let unmeasured: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|name| !measured_required.contains(name))
        .collect();
    let denied: Vec<&str> = capabilities
        .iter()
        .filter(|c| !c.name.starts_with("control-") && c.denied_by_sandbox())
        .map(|c| c.name)
        .collect();
    let survived_the_sandbox: Vec<&str> = measured_required
        .iter()
        .copied()
        .filter(|name| !denied.contains(name))
        .collect();
    let controls_survive = capabilities
        .iter()
        .filter(|c| c.name.starts_with("control-"))
        .all(|c| c.sandboxed.0 == Some(0));
    let cross_app = capabilities
        .iter()
        .find(|c| c.name == "cross-app-apple-event");
    let cross_app_measured = measured_required.contains(&"cross-app-apple-event");

    let verdict = if !controls_survive {
        // A profile that breaks ordinary work has not answered the question.
        Verdict::Fail
    } else if !survived_the_sandbox.is_empty() {
        // Something we could measure got through.
        Verdict::Fail
    } else if !unmeasured.is_empty() {
        Verdict::Inconclusive
    } else {
        Verdict::Pass
    };
    let answer = format!(
        "every capability was measured unsandboxed first, so a sandboxed failure is interpretable. Denied by the profile: {denied:?}. Required but NOT measured on this run (so not evidence of anything): {unmeasured:?}. Measured and still got through the sandbox: {survived_the_sandbox:?}. Cross-application Apple Events were measurable: {cross_app_measured}, and the sandbox stopped them: {cross_app_stopped}. Ordinary work still worked inside the sandbox: {controls_survive}. Keychain check ran: {ran} (it needs --interactive, because reading an item from a freshly built binary makes macOS ask the user and a probe must not leave a modal dialog on screen unattended). Test Keychain item deleted: {cleaned}. This shows a profile CAN deny these; it does not show Claude Code still works under it.",
        ran = ctx.interactive,
        cross_app_stopped = cross_app.is_some_and(|c| c.denied_by_sandbox())
    );
    Ok((
        verdict,
        answer,
        json!({
            "profile": SEATBELT_PROFILE,
            "capabilities": capabilities
                .iter()
                .map(|c| (c.name.to_string(), c.to_json()))
                .collect::<serde_json::Map<String, Value>>(),
            "testItemDeleted": cleaned,
            "deniedCapabilities": denied,
            "crossAppAppleEventMeasured": cross_app_measured,
            "crossAppAppleEventDenied": cross_app.is_some_and(|c| c.denied_by_sandbox()),
            "keychainCheckRan": ctx.interactive,
            "keychainPromptAppeared": keychain_prompted,
            "unmeasuredCapabilities": unmeasured,
            "claudeCodeUnderSandboxTested": false,
        }),
    ))
}
