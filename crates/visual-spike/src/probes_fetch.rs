//! Probes for the Fetch / auto-attach questions (Q4, Q9–Q12).
//!
//! These decide §V4's guarantee scope, so the bar for "pass" is the origin
//! server's own request log — see `fixture.rs`. Where a case's feature never
//! triggers, the verdict is `Inconclusive`, never `Pass`: a positive control
//! that fails cannot be read as enforcement working.

use crate::fixture::{Fixture, Record};
use crate::pipe::{PipeBrowser, visual_base_args};
use crate::probes::{Ctx, ProbeResult, Verdict, attach, nonce, target_infos};
use crate::scratch::Scratch;
use serde_json::{Value, json};
use std::io;
use std::time::{Duration, Instant};

/// How long a "the server never saw it" conclusion is given to be wrong.
const OBSERVE: Duration = Duration::from_secs(6);

struct Owned {
    browser: PipeBrowser,
    target: String,
    session: String,
}

/// Open a tab the way the design specifies for visual mode: create it blank,
/// attach, configure interception, and only then navigate — so the first
/// document request cannot precede `Fetch.enable`.
fn owned_tab(browser: PipeBrowser, fetch_patterns: Option<Value>) -> io::Result<Owned> {
    let created = browser.call("Target.createTarget", json!({ "url": "about:blank" }))?;
    let target = created["targetId"]
        .as_str()
        .ok_or_else(|| io::Error::other("createTarget returned no targetId"))?
        .to_string();
    let session = attach(&browser, &target)?;
    browser.call_session(&session, "Page.enable", json!({}))?;
    browser.call_session(&session, "Runtime.enable", json!({}))?;
    if let Some(patterns) = fetch_patterns {
        browser.call_session(&session, "Fetch.enable", json!({ "patterns": patterns }))?;
    }
    Ok(Owned {
        browser,
        target,
        session,
    })
}

fn document_patterns() -> Value {
    json!([{ "resourceType": "Document", "requestStage": "Request" }])
}

fn launch(ctx: &Ctx, tag: &str) -> io::Result<(Scratch, PipeBrowser)> {
    let scratch = Scratch::new(tag, ctx.keep)?;
    let mut args = visual_base_args();
    // Without this, Brave blocks on a modal "the login keyring did not get
    // unlocked" prompt before it will complete *any* network request: the
    // navigation commits, `Network.requestWillBeSent` fires, and then nothing
    // — no response, no failure, and the page session stops answering. It
    // shows up whenever the session's keyring is locked, which is the normal
    // state for a browser driven over ssh.
    //
    // These probes run on throwaway profiles with no real credentials, so the
    // plaintext store is the right call here. It is deliberately NOT used by
    // `q6-profile-copy`, whose whole question is whether the real profile's
    // OSCrypt key still works.
    args.push("--password-store=basic".into());
    let browser = PipeBrowser::launch(
        &ctx.exe,
        &scratch.child("profile"),
        &args,
        Some("about:blank"),
    )?;
    Ok((scratch, browser))
}

/// Drain events until `deadline`, handing each to `handler`. Returns when the
/// handler asks to stop or the deadline passes. Commands may be sent from
/// inside the handler — this is the spike's stand-in for the design's reader
/// task, which answers `Fetch.requestPaused` without awaiting a response.
fn pump<F>(browser: &PipeBrowser, cursor: &mut usize, deadline: Instant, mut handler: F)
where
    F: FnMut(&Value) -> bool,
{
    while Instant::now() < deadline {
        let events = browser.drain_events(cursor);
        if events.is_empty() {
            std::thread::sleep(Duration::from_millis(25));
            continue;
        }
        for event in events {
            if handler(&event) {
                return;
            }
        }
    }
}

fn method_of(event: &Value) -> &str {
    event.get("method").and_then(Value::as_str).unwrap_or("")
}

fn session_of(event: &Value) -> Option<&str> {
    event.get("sessionId").and_then(Value::as_str)
}

fn paused_url(event: &Value) -> &str {
    event
        .pointer("/params/request/url")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn paused_request_id(event: &Value) -> Option<&str> {
    event.pointer("/params/requestId").and_then(Value::as_str)
}

/// Smoke check: can the browser reach the fixture at all? Everything in this
/// module depends on it, and a failure here would otherwise surface as an
/// unexplained `Page.navigate` timeout inside a real probe.
fn http_smoke(ctx: &Ctx) -> ProbeResult {
    let fixture = Fixture::start(1)?;
    fixture.set_page("smoke", "<title>smoke</title>ok");
    let url = fixture.url(0, "/page/smoke");
    let (_scratch, browser) = launch(ctx, "smoke")?;
    let created = browser.call("Target.createTarget", json!({ "url": "about:blank" }))?;
    let target = created["targetId"].as_str().unwrap_or_default().to_string();
    let session = attach(&browser, &target)?;
    browser.call_session(&session, "Page.enable", json!({}))?;
    browser.call_session(&session, "Network.enable", json!({}))?;

    // Fire-and-forget: `Page.navigate` only returns once the navigation
    // commits, so a stalled request would hide the Network events that say
    // why. Those events are the diagnosis.
    let mut cursor = 0usize;
    browser.send_and_forget(
        Some(&session),
        "Page.navigate",
        json!({ "url": url.clone() }),
    )?;

    let wait_secs = std::env::var("TABD_SPIKE_SMOKE_WAIT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(wait_secs);
    let mut network: Vec<Value> = Vec::new();
    pump(&browser, &mut cursor, deadline, |event| {
        let method = method_of(event);
        if method.starts_with("Network.") {
            network.push(json!({
                "method": method,
                "url": event.pointer("/params/request/url"),
                "errorText": event.pointer("/params/errorText"),
                "canceled": event.pointer("/params/canceled"),
                "status": event.pointer("/params/response/status"),
                "blockedReason": event.pointer("/params/blockedReason"),
            }));
        }
        false
    });
    let saw = fixture.saw("/page/smoke");
    let targets = target_infos(&browser)
        .map(|t| {
            t.iter()
                .map(|i| json!({ "type": i.get("type"), "url": i.get("url"), "title": i.get("title") }))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok((
        if saw { Verdict::Pass } else { Verdict::Fail },
        format!(
            "server saw the request: {saw} after {:?}; {} Network events observed",
            started.elapsed(),
            network.len()
        ),
        json!({
            "url": url,
            "serverLog": fixture.log_json(),
            "networkEvents": network,
            "targets": targets,
            "extraArgs": std::env::var("TABD_SPIKE_EXTRA_ARGS").ok(),
        }),
    ))
}

// -------------------------------------------------------------------- Q4

const Q4_INNER: &str = r#"<!doctype html><title>inner</title>
<body style="margin:0"><button id="innerBtn" style="width:190px;height:60px">inner</button></body>"#;

/// Every target is laid out inside a small, non-scrolling viewport on purpose.
///
/// `DOM.getContentQuads` and `DOM.getNodeForLocation` only agree while the page
/// is unscrolled; scrolling a target into view between the two calls moves the
/// others and the hit-test then lands on whatever slid under the stale point.
/// Keeping the whole fixture above the fold removes that failure mode instead
/// of trying to compensate for it.
fn q4_page(cross_origin_iframe: &str) -> String {
    format!(
        r#"<!doctype html><title>q4</title><body style="margin:0;overflow:hidden">
<div style="display:flex;flex-wrap:wrap;width:420px">
  <button id="plain" style="width:200px;height:60px">plain</button>
  <div id="openHost"></div>
  <div id="closedHost"></div>
  <div style="position:relative;width:200px;height:60px">
    <button id="covered" style="position:absolute;inset:0">covered</button>
    <div id="overlay" style="position:absolute;inset:0;background:rgba(0,0,0,.2)"></div>
  </div>
  <iframe id="same" src="/page/q4-inner" style="width:200px;height:60px;border:0"></iframe>
  <iframe id="cross" src="{cross_origin_iframe}" style="width:200px;height:60px;border:0"></iframe>
</div>
<script>
  const open_ = document.getElementById('openHost').attachShadow({{mode:'open'}});
  open_.innerHTML = '<button id="openBtn" style="width:200px;height:60px">open</button>';
  const closed_ = document.getElementById('closedHost').attachShadow({{mode:'closed'}});
  closed_.innerHTML = '<button id="closedBtn" style="width:200px;height:60px">closed</button>';
  // A closed root cannot be traversed afterwards, so keep the reference now —
  // otherwise the probe would be testing fixture accessibility, not hit-testing.
  window.__closedShadowButton = closed_.getElementById('closedBtn');
</script></body>"#
    )
}

fn q4_hit_test(ctx: &Ctx) -> ProbeResult {
    let fixture = Fixture::start(2)?;
    fixture.set_page("q4-inner", Q4_INNER);
    fixture.set_page("q4", q4_page(&fixture.url(1, "/page/q4-inner")));

    let (_scratch, browser) = launch(ctx, "q4")?;
    let owned = owned_tab(browser, None)?;
    let (browser, session) = (&owned.browser, owned.session.as_str());
    // Pin the viewport. Hit-testing only resolves inside the *visual* viewport,
    // and a tiling compositor hands out whatever window height happens to be
    // free — which made every target below the first row fail intermittently.
    browser.call_session(
        session,
        "Emulation.setDeviceMetricsOverride",
        json!({ "width": 900, "height": 700, "deviceScaleFactor": 1, "mobile": false }),
    )?;
    browser.call_session(
        session,
        "Page.navigate",
        json!({ "url": fixture.url(0, "/page/q4") }),
    )?;
    // Wait for both iframes to actually be fetched instead of guessing with a
    // sleep; the cross-site one goes through a process swap and is slower.
    let both_frames = fixture.wait_for("/page/q4-inner", Duration::from_secs(10)) && {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen = fixture
                .log()
                .iter()
                .filter(|r| r.path.contains("/page/q4-inner"))
                .count();
            if seen >= 2 || Instant::now() >= deadline {
                break seen >= 2;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    };
    std::thread::sleep(Duration::from_millis(750));
    browser.call_session(session, "DOM.getDocument", json!({ "depth": 0 }))?;

    // (expression yielding the node, id we expect the hit-test to land on)
    let cases: &[(&str, &str, &str)] = &[
        ("plain", "document.getElementById('plain')", "plain"),
        (
            "open-shadow",
            "document.getElementById('openHost').shadowRoot.getElementById('openBtn')",
            "openBtn",
        ),
        ("closed-shadow", "window.__closedShadowButton", "closedBtn"),
        (
            "same-origin-iframe",
            "document.getElementById('same').contentDocument.getElementById('innerBtn')",
            "innerBtn",
        ),
        // The overlay is what a click must resolve to — that is exactly what
        // V3's trusted-click path relies on to report `obscured`.
        (
            "overlay-covered",
            "document.getElementById('covered')",
            "overlay",
        ),
    ];

    let mut results = serde_json::Map::new();
    let mut all_ok = true;
    for (name, expression, expected_id) in cases {
        match q4_case(browser, session, expression) {
            Ok((landed_on, point)) => {
                let ok = landed_on.as_deref() == Some(*expected_id);
                all_ok &= ok;
                results.insert(
                    (*name).to_string(),
                    json!({ "expected": expected_id, "landedOn": landed_on, "point": point, "ok": ok }),
                );
            }
            Err(err) => {
                all_ok = false;
                results.insert((*name).to_string(), json!({ "error": err.to_string() }));
            }
        }
    }

    // Cross-site: backend node ids from an OOPIF session are not comparable
    // with the parent's, so node *identity* across processes is not asserted.
    // What is asserted is the measured behaviour: against a real OOPIF the
    // parent session's hit-test resolves to the `<iframe>` ELEMENT itself,
    // carrying the main frame's id — it does not reach the child's button.
    // Encoding that as an expectation (rather than leaving it record-only)
    // means a future Chromium that starts piercing flips this probe to Fail
    // instead of passing silently, which is what V3 needs to hear about.
    let main_frame_id = browser
        .call_session(session, "Page.getFrameTree", json!({}))?
        .pointer("/frameTree/frame/id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cross_site = fixture.is_cross_site(0, 1);
    let (pierced, cross_ok, cross_json) =
        match q4_case(browser, session, "document.getElementById('cross')") {
            Ok((landed_on, point)) => {
                let frame_id = point
                    .get("frameId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let pierced = frame_id.is_some()
                    && frame_id != main_frame_id
                    && landed_on.as_deref() == Some("innerBtn");
                // Expected: the iframe element, in the main frame.
                let as_documented = cross_site
                    && !pierced
                    && landed_on.as_deref() == Some("cross")
                    && frame_id == main_frame_id;
                (
                    pierced,
                    as_documented,
                    json!({
                        "expected": "cross (the <iframe> element, main frame id)",
                        "landedOn": landed_on,
                        "point": point,
                        "mainFrameId": main_frame_id,
                        "crossSiteOrigins": cross_site,
                        "piercedTheBoundary": pierced,
                        "ok": as_documented,
                    }),
                )
            }
            Err(err) => (false, false, json!({ "error": err.to_string() })),
        };
    results.insert("cross-site-iframe".into(), cross_json);
    all_ok &= cross_ok;

    // `pierced` is a finding, not a pass condition — but the cross-site case
    // has to have been *established* before this probe may return a decisive
    // verdict at all. If the loopback alias fell back to 127.0.0.1 (same-site,
    // in-process) or the hit-test errored, no OOPIF boundary was exercised.
    let cross_established = cross_site
        && results["cross-site-iframe"].get("error").is_none()
        && results["cross-site-iframe"]["landedOn"].is_string();
    let verdict = if !both_frames {
        // Neither a pass nor a failure means anything if the fixture never
        // finished loading.
        Verdict::Inconclusive
    } else if !all_ok {
        Verdict::Fail
    } else if cross_established {
        Verdict::Pass
    } else {
        Verdict::Inconclusive
    };
    let answer = format!(
        "all six cases behaved as documented: {all_ok} (plain / open shadow / closed shadow / same-origin iframe / overlay land on the expected element; the cross-site OOPIF resolves to the <iframe> ELEMENT in the main frame). Cross-site case established: {cross_established}; parent session crossed into the child frame: {pierced} — it does not, which is why V3 cannot judge `obscured` inside a cross-site iframe from the parent session."
    );
    Ok((
        verdict,
        answer,
        json!({
            "cases": results,
            "bothIframesFetched": both_frames,
            "serverLog": fixture.log_json(),
        }),
    ))
}

/// Resolve `expression` to a node, take the centre of its first content quad,
/// hit-test there, and ask the page which element id the returned node belongs
/// to. Asking the page avoids comparing backend node ids across contexts.
fn q4_case(
    browser: &PipeBrowser,
    session: &str,
    expression: &str,
) -> io::Result<(Option<String>, Value)> {
    let object = browser.call_session(
        session,
        "Runtime.evaluate",
        json!({ "expression": expression }),
    )?;
    let object_id = object
        .pointer("/result/objectId")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other(format!("{expression} did not yield an object")))?
        .to_string();

    let quads = browser.call_session(
        session,
        "DOM.getContentQuads",
        json!({ "objectId": object_id }),
    )?;
    let quad = quads
        .pointer("/quads/0")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("no content quads"))?;
    let xs: Vec<f64> = quad.iter().step_by(2).filter_map(Value::as_f64).collect();
    let ys: Vec<f64> = quad
        .iter()
        .skip(1)
        .step_by(2)
        .filter_map(Value::as_f64)
        .collect();
    let x = xs.iter().sum::<f64>() / xs.len() as f64;
    let y = ys.iter().sum::<f64>() / ys.len() as f64;

    let hit = browser.call_session(
        session,
        "DOM.getNodeForLocation",
        json!({ "x": x.round() as i64, "y": y.round() as i64, "includeUserAgentShadowDOM": false }),
    )?;
    let backend = hit
        .get("backendNodeId")
        .and_then(Value::as_i64)
        .ok_or_else(|| io::Error::other("getNodeForLocation returned no backendNodeId"))?;

    let resolved = browser.call_session(
        session,
        "DOM.resolveNode",
        json!({ "backendNodeId": backend }),
    )?;
    let landed = match resolved.pointer("/object/objectId").and_then(Value::as_str) {
        Some(id) => browser
            .call_session(
                session,
                "Runtime.callFunctionOn",
                json!({
                    "objectId": id,
                    "functionDeclaration": "function(){ const el = this.nodeType === 1 ? this : this.parentElement; return el ? el.id : ''; }",
                    "returnByValue": true,
                }),
            )?
            .pointer("/result/value")
            .and_then(Value::as_str)
            .map(str::to_string),
        None => None,
    };
    Ok((
        landed,
        json!({ "x": x, "y": y, "frameId": hit.get("frameId") }),
    ))
}

// -------------------------------------------------------------------- Q9

fn q9_fetch_coverage(ctx: &Ctx) -> ProbeResult {
    let fixture = Fixture::start(2)?;
    let (_scratch, browser) = launch(ctx, "q9")?;
    let owned = owned_tab(browser, Some(document_patterns()))?;
    let (browser, session) = (&owned.browser, owned.session.as_str());
    let mut cursor = 0usize;
    let mut cases = serde_json::Map::new();

    // 1. Ordinary navigation, failed. The server must never see it.
    let tag = format!("q9-plain-{}", nonce());
    fixture.set_page(&tag, "<title>plain</title>");
    cases.insert(
        "plain-navigation".into(),
        fail_navigation(
            browser,
            session,
            &mut cursor,
            &fixture,
            &fixture.url(0, &format!("/page/{tag}")),
            &tag,
        ),
    );

    // 2a. Redirect, failing the FIRST hop.
    let dest_a = format!("q9-redir-dest-a-{}", nonce());
    fixture.set_page(&dest_a, "<title>dest</title>");
    let redirect_a = format!(
        "/redirect?to={}",
        percent_encode(&fixture.url(0, &format!("/page/{dest_a}")))
    );
    cases.insert(
        "redirect-fail-first-hop".into(),
        fail_navigation(
            browser,
            session,
            &mut cursor,
            &fixture,
            &fixture.url(0, &redirect_a),
            &dest_a,
        ),
    );

    // 2b. Redirect, letting the first hop through and failing the DESTINATION.
    // Failing only the first hop would prevent the redirect from existing and
    // would test nothing about redirect destinations.
    let dest_b = format!("q9-redir-dest-b-{}", nonce());
    fixture.set_page(&dest_b, "<title>dest</title>");
    let redirect_b = format!(
        "/redirect?to={}",
        percent_encode(&fixture.url(0, &format!("/page/{dest_b}")))
    );
    cases.insert(
        "redirect-fail-destination".into(),
        redirect_hop_case(
            browser,
            session,
            &mut cursor,
            &fixture,
            &fixture.url(0, &redirect_b),
            &dest_b,
        ),
    );

    // 3. Form POST navigation.
    let post_target = format!("q9-post-{}", nonce());
    fixture.set_page(&post_target, "<title>posted</title>");
    let form_page = format!("q9-form-{}", nonce());
    fixture.set_page(
        &form_page,
        format!(
            r#"<!doctype html><title>form</title><form id="f" method="post" action="/page/{post_target}"><input name="a" value="1"></form><script>document.getElementById('f').submit();</script>"#
        ),
    );
    let before = fixture.log().len();
    browser.send_and_forget(
        Some(session),
        "Page.navigate",
        json!({ "url": fixture.url(0, &format!("/page/{form_page}")) }),
    )?;
    // The form page itself is a document request and pauses first; continue it.
    let mut posted_pause: Option<Value> = None;
    let deadline = Instant::now() + Duration::from_secs(12);
    pump(browser, &mut cursor, deadline, |event| {
        if method_of(event) != "Fetch.requestPaused" || session_of(event) != Some(session) {
            return false;
        }
        let url = paused_url(event).to_string();
        let Some(request_id) = paused_request_id(event) else {
            return false;
        };
        if url.ends_with(&format!("/page/{post_target}")) {
            posted_pause = Some(event.clone());
            let _ = browser.send_and_forget(
                Some(session),
                "Fetch.failRequest",
                json!({ "requestId": request_id, "errorReason": "BlockedByClient" }),
            );
            return true;
        }
        let _ = browser.send_and_forget(
            Some(session),
            "Fetch.continueRequest",
            json!({ "requestId": request_id }),
        );
        false
    });
    let posted_method = posted_pause
        .as_ref()
        .and_then(|e| e.pointer("/params/request/method"))
        .cloned();
    let posted_leaked = fixture.wait_for(&format!("/page/{post_target}"), OBSERVE);
    cases.insert(
        "form-post-navigation".into(),
        json!({
            "paused": posted_pause.is_some(),
            "pausedMethod": posted_method,
            "serverSawItAfterFail": posted_leaked,
            "serverLogGrewBy": fixture.log().len() - before,
        }),
    );

    // 4. bfcache — is an owned tab even eligible? A page with an attached
    // DevTools session may be excluded, in which case the hole does not exist
    // for owned tabs. Two independent signals: pageshow.persisted reported by
    // the page, and whether the server logged a re-fetch.
    cases.insert(
        "bfcache".into(),
        bfcache_case(browser, session, &mut cursor, &fixture)?,
    );

    let decided: Vec<&str> = [
        "plain-navigation",
        "redirect-fail-first-hop",
        "redirect-fail-destination",
        "form-post-navigation",
    ]
    .into_iter()
    .collect();
    let all_enforced = decided.iter().all(|name| {
        cases[*name]
            .get("serverSawItAfterFail")
            .and_then(Value::as_bool)
            .map(|leaked| !leaked)
            .unwrap_or(false)
            && cases[*name]
                .get("paused")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    });
    // The bfcache case cannot be left out of the verdict: a real restore
    // bypasses Fetch entirely, and an unsettled positive control means we do
    // not know which of the two worlds we are in.
    let bfcache_restored = cases["bfcache"]["restoredFromBfcache"]
        .as_bool()
        .unwrap_or(false);
    // Only one reading means "no bfcache hole here": the page reported
    // pageshow.persisted === false AND the document was re-fetched. Any other
    // combination is a contradiction between the two signals, which
    // `bfcache_case` itself labels inconclusive — accepting a merely
    // *present* pageshowPersisted would let those through as a pass.
    let not_bfcache_eligible = cases["bfcache"]["pageshowPersisted"].as_str() == Some("false")
        && cases["bfcache"]["documentRefetched"].as_bool() == Some(true);
    let verdict = if !all_enforced {
        Verdict::Fail
    } else if bfcache_restored {
        // Interception held for every network path, but a cached document can
        // still come back without one.
        Verdict::Fail
    } else if !not_bfcache_eligible {
        Verdict::Inconclusive
    } else {
        Verdict::Pass
    };
    let answer = format!(
        "Document interception held for plain navigation, both redirect passes and form POST: {all_enforced}. bfcache: {}",
        cases["bfcache"]
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("?")
    );
    Ok((
        verdict,
        answer,
        json!({ "cases": cases, "serverLog": fixture.log_json() }),
    ))
}

/// Navigate to `url` with the policy failing the document request, then give
/// the server a real window to prove us wrong.
fn fail_navigation(
    browser: &PipeBrowser,
    session: &str,
    cursor: &mut usize,
    fixture: &Fixture,
    url: &str,
    // `must_not_be_requested` is a **path**, not a bare tag: a bare tag would
    // also match a redirect URL that carries it in a query parameter.
    must_not_be_requested: &str,
) -> Value {
    // Cannot be awaited: `Page.navigate` returns on commit, and the commit
    // cannot happen until the pause below is answered.
    let _ = browser.send_and_forget(Some(session), "Page.navigate", json!({ "url": url }));
    let mut paused = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    pump(browser, cursor, deadline, |event| {
        if method_of(event) != "Fetch.requestPaused" || session_of(event) != Some(session) {
            return false;
        }
        let Some(request_id) = paused_request_id(event) else {
            return false;
        };
        paused = Some(paused_url(event).to_string());
        let _ = browser.send_and_forget(
            Some(session),
            "Fetch.failRequest",
            json!({ "requestId": request_id, "errorReason": "BlockedByClient" }),
        );
        true
    });
    json!({
        "paused": paused.is_some(),
        "pausedUrl": paused,
        "serverSawItAfterFail": fixture.wait_for(must_not_be_requested, OBSERVE),
    })
}

/// Continue the first hop, fail the redirect destination.
///
/// The destination is matched by **suffix**, not by `contains`: the redirect
/// URL carries the destination in its `to=` query parameter, so a `contains`
/// test matches the first hop and silently turns this into a copy of the
/// fail-the-first-hop case.
fn redirect_hop_case(
    browser: &PipeBrowser,
    session: &str,
    cursor: &mut usize,
    fixture: &Fixture,
    url: &str,
    destination_tag: &str,
) -> Value {
    let destination_suffix = format!("/page/{destination_tag}");
    let _ = browser.send_and_forget(Some(session), "Page.navigate", json!({ "url": url }));
    let mut hops: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(12);
    pump(browser, cursor, deadline, |event| {
        if method_of(event) != "Fetch.requestPaused" || session_of(event) != Some(session) {
            return false;
        }
        let paused = paused_url(event).to_string();
        let Some(request_id) = paused_request_id(event) else {
            return false;
        };
        hops.push(paused.clone());
        if paused.ends_with(&destination_suffix) {
            let _ = browser.send_and_forget(
                Some(session),
                "Fetch.failRequest",
                json!({ "requestId": request_id, "errorReason": "BlockedByClient" }),
            );
            return true;
        }
        let _ = browser.send_and_forget(
            Some(session),
            "Fetch.continueRequest",
            json!({ "requestId": request_id }),
        );
        false
    });
    json!({
        "paused": hops.iter().any(|h| h.ends_with(&destination_suffix)),
        "hops": hops,
        "serverSawItAfterFail": fixture.wait_for(&destination_suffix, OBSERVE),
    })
}

fn bfcache_case(
    browser: &PipeBrowser,
    session: &str,
    cursor: &mut usize,
    fixture: &Fixture,
) -> io::Result<Value> {
    let a = format!("q9-bf-a-{}", nonce());
    let b = format!("q9-bf-b-{}", nonce());
    let report = fixture.origin(0);
    fixture.set_page(
        &a,
        format!(
            r#"<!doctype html><title>bfA</title><script>
addEventListener('pageshow', e => {{
  fetch('{report}/report?tag=bfA&persisted=' + e.persisted);
}});
</script>bfA"#
        ),
    );
    fixture.set_page(&b, "<title>bfB</title>bfB");

    let continue_all = |cursor: &mut usize, limit: Duration| {
        let deadline = Instant::now() + limit;
        pump(browser, cursor, deadline, |event| {
            if method_of(event) == "Fetch.requestPaused"
                && session_of(event) == Some(session)
                && let Some(request_id) = paused_request_id(event)
            {
                let _ = browser.send_and_forget(
                    Some(session),
                    "Fetch.continueRequest",
                    json!({ "requestId": request_id }),
                );
            }
            false
        });
    };

    browser.send_and_forget(
        Some(session),
        "Page.navigate",
        json!({ "url": fixture.url(0, &format!("/page/{a}")) }),
    )?;
    continue_all(cursor, Duration::from_secs(4));
    browser.send_and_forget(
        Some(session),
        "Page.navigate",
        json!({ "url": fixture.url(0, &format!("/page/{b}")) }),
    )?;
    continue_all(cursor, Duration::from_secs(4));

    let refetches_before = fixture.hits(&a).len();
    let reports_before = fixture.hits("tag=bfA").len();
    browser.call_session(
        session,
        "Runtime.evaluate",
        json!({ "expression": "history.back()" }),
    )?;
    continue_all(cursor, Duration::from_secs(5));

    let refetched = fixture.hits(&a).len() > refetches_before;
    let persisted = fixture
        .hits("tag=bfA")
        .into_iter()
        .skip(reports_before)
        .find_map(|r| crate::fixture::query_param(r.path.split_once('?')?.1, "persisted"));
    let restored_from_bfcache = persisted.as_deref() == Some("true") && !refetched;
    let summary = if restored_from_bfcache {
        "the owned tab WAS restored from bfcache with no request — Fetch cannot police history restoration"
    } else if persisted.as_deref() == Some("false") && refetched {
        "the owned tab is NOT bfcache-eligible; going back re-fetched the document, so Fetch still sees it"
    } else {
        "inconclusive: the positive control did not settle"
    };
    Ok(json!({
        "pageshowPersisted": persisted,
        "documentRefetched": refetched,
        "restoredFromBfcache": restored_from_bfcache,
        "summary": summary,
    }))
}

fn percent_encode(url: &str) -> String {
    url.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

// ------------------------------------------------------------------- Q10

fn q10_oopif_leak(ctx: &Ctx) -> ProbeResult {
    const TRIALS: usize = 20;
    let mut variants = serde_json::Map::new();
    // Control first. With no interception armed at all, the cross-site iframe
    // request MUST reach the origin — otherwise "the server never saw it"
    // means something unrelated is stopping it and the whole probe is void.
    variants.insert(
        "control-no-interception".into(),
        q10_variant(ctx, Q10Mode::Control, 3)?,
    );
    variants.insert(
        "A-parent-fetch-plus-autoattach".into(),
        q10_variant(ctx, Q10Mode::ParentFetchAndAutoAttach, TRIALS)?,
    );
    variants.insert(
        "B-autoattach-only".into(),
        q10_variant(ctx, Q10Mode::AutoAttachOnly, TRIALS)?,
    );

    let intercepted_b =
        variants["B-autoattach-only"]["trialsWhereTheRequestWasActuallyIntercepted"]
            .as_u64()
            .unwrap_or(0);
    let control_reached = variants["control-no-interception"]["iframeRequestsReachingTheServer"]
        .as_u64()
        .unwrap_or(0)
        > 0;
    let control_oopif = variants["control-no-interception"]["childSessionsAttached"]
        .as_u64()
        .unwrap_or(0);
    let leaked_b = variants["B-autoattach-only"]["leaks"]
        .as_u64()
        .unwrap_or(u64::MAX);

    let intercepted_a =
        variants["A-parent-fetch-plus-autoattach"]["trialsWhereTheRequestWasActuallyIntercepted"]
            .as_u64()
            .unwrap_or(0);
    let leaked_a = variants["A-parent-fetch-plus-autoattach"]["leaks"]
        .as_u64()
        .unwrap_or(u64::MAX);
    let children_b = variants["B-autoattach-only"]["childSessionsAttached"]
        .as_u64()
        .unwrap_or(0);
    let cross_site = variants["control-no-interception"]["crossSiteOrigins"]
        .as_bool()
        .unwrap_or(false);

    // The question is "can an OOPIF's first document request escape before its
    // auto-attached session is configured". Leaking in the auto-attach-only
    // variant answers it: yes, it can.
    // A control that reached the origin but never produced a child session
    // means the iframe stayed in-process, so nothing here is about OOPIFs.
    let control_valid = control_reached && control_oopif > 0 && cross_site;
    // The control proves an OOPIF is *possible*; it cannot prove the
    // separately launched B variant exercised one in every scored trial, and a
    // trial with no OOPIF says nothing either way.
    // Every trial of the auto-attach-only variant must be accounted for:
    // either the request was intercepted or it reached the origin. A trial
    // that is neither means the target sat frozen and proves nothing, and
    // counting it as "no leak" would be a false pass.
    let accounted_b = intercepted_b + leaked_b;
    let verdict = if !control_valid || children_b < TRIALS as u64 || accounted_b < TRIALS as u64 {
        Verdict::Inconclusive
    } else if leaked_b > 0 {
        Verdict::Fail
    } else if intercepted_a == TRIALS as u64 && leaked_a == 0 {
        Verdict::Pass
    } else {
        Verdict::Inconclusive
    };
    let answer = if !control_valid || children_b < TRIALS as u64 || accounted_b < TRIALS as u64 {
        format!(
            "VOID — the OOPIF case was not established in every scored trial: control request reached the origin: {control_reached}; control attached {control_oopif} child session(s); origins cross-site: {cross_site}; auto-attach-only variant attached {children_b}/{TRIALS} child sessions and accounted for {accounted_b}/{TRIALS} trials ({intercepted_b} intercepted + {leaked_b} leaked). (Without a distinct loopback IP the iframe is same-site and stays in-process.)"
        )
    } else {
        format!(
            "Control valid: with nothing armed the cross-site iframe request reached the origin and the frame went out-of-process ({control_oopif} child sessions).              AUTO-ATTACH ALONE IS NOT ENOUGH: with `waitForDebuggerOnStart` but no Fetch on the parent, {children_b}/{TRIALS} OOPIF sessions did attach, yet the iframe's FIRST document request reached the origin in {leaked_b}/{TRIALS} trials and was intercepted in {intercepted_b}/{TRIALS} — the request is already gone by the time the child session can be configured.              What does cover it is the PARENT page session's Fetch: {intercepted_a}/{TRIALS} intercepted, {leaked_a}/{TRIALS} escaped (and no child ever attaches there, because blocking the document means the OOPIF is never created).              No leak in N trials is bounded empirical evidence, not proof."
        )
    };
    Ok((verdict, answer, Value::Object(variants)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Q10Mode {
    /// Nothing armed — proves the request can reach the origin at all.
    Control,
    ParentFetchAndAutoAttach,
    AutoAttachOnly,
}

fn q10_variant(ctx: &Ctx, mode: Q10Mode, trials: usize) -> io::Result<Value> {
    let fixture = Fixture::start(2)?;
    let (_scratch, browser) = launch(ctx, "q10")?;
    let parent_fetch = mode == Q10Mode::ParentFetchAndAutoAttach;
    let patterns = parent_fetch.then(document_patterns);
    let owned = owned_tab(browser, patterns)?;
    let (browser, session) = (&owned.browser, owned.session.as_str());
    // The control still needs auto-attach: without it there is no way to
    // observe whether the iframe became its own target, which is the other
    // thing this control establishes.
    browser.call_session(
        session,
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": mode != Q10Mode::Control, "flatten": true }),
    )?;

    let mut leaks = 0usize;
    let mut attached_children = 0usize;
    let mut intercepted = 0usize;
    let mut child_sessions: Vec<String> = Vec::new();
    let mut marker_pause_sessions: Vec<String> = Vec::new();
    let mut catching_sessions: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    for _ in 0..trials {
        let marker = format!("never-oopif-{}", nonce());
        let marker_path = format!("/page/{marker}");
        // The iframe's OWN FIRST navigation is the decisive request. Having
        // the child navigate itself after it had loaded would only show that
        // interception works once configured — which is not the question.
        let inner = fixture.url(1, &format!("/page/{marker}"));
        let page = format!("q10-{}", nonce());
        fixture.set_page(
            &page,
            format!(r#"<!doctype html><title>q10</title><iframe src="{inner}"></iframe>"#),
        );
        fixture.set_page(&marker, "<title>leaked</title>");

        browser.send_and_forget(
            Some(session),
            "Page.navigate",
            json!({ "url": fixture.url(0, &format!("/page/{page}")) }),
        )?;

        // Pump for the whole trial instead of stopping at the first match.
        // Returning early left later auto-attached targets paused on
        // `waitForDebuggerOnStart` forever, so their requests never went out
        // and the trial would have scored as "intercepted" when it was really
        // "frozen" — a false pass on a security question.
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut caught_on: Option<String> = None;
        pump(browser, &mut cursor, deadline, |event| {
            match method_of(event) {
                "Target.attachedToTarget" => {
                    let Some(child) = event.pointer("/params/sessionId").and_then(Value::as_str)
                    else {
                        return false;
                    };
                    // Our own explicit `Target.attachToTarget` for the page
                    // also emits this event. Counting it as an auto-attached
                    // child inflated the child count and — far worse — made
                    // the "auto-attach only" variant call `Fetch.enable` on
                    // the PAGE session, collapsing it into the variant it was
                    // supposed to be contrasted with.
                    if child == session {
                        return false;
                    }
                    attached_children += 1;
                    child_sessions.push(child.to_string());
                    if mode == Q10Mode::Control {
                        // Nothing armed, and the target is not waiting for us.
                        return false;
                    }
                    // Configure before releasing the paused target: Fetch,
                    // then recursive auto-attach for nested OOPIFs, then run.
                    let _ = browser.send_and_forget(
                        Some(child),
                        "Fetch.enable",
                        json!({ "patterns": document_patterns() }),
                    );
                    let _ = browser.send_and_forget(
                        Some(child),
                        "Target.setAutoAttach",
                        json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
                    );
                    let _ = browser.send_and_forget(
                        Some(child),
                        "Runtime.runIfWaitingForDebugger",
                        json!({}),
                    );
                    false
                }
                "Fetch.requestPaused" => {
                    let url = paused_url(event).to_string();
                    let Some(request_id) = paused_request_id(event) else {
                        return false;
                    };
                    let on = session_of(event).unwrap_or("?").to_string();
                    if url.ends_with(&marker_path) {
                        let label = if on == session {
                            "parent".to_string()
                        } else if child_sessions.contains(&on) {
                            "child (auto-attached)".to_string()
                        } else {
                            format!("unknown session {on}")
                        };
                        if !catching_sessions.contains(&label) {
                            catching_sessions.push(label.clone());
                        }
                        if !marker_pause_sessions.contains(&on) {
                            marker_pause_sessions.push(on.clone());
                        }
                        caught_on = Some(label);
                        let _ = browser.send_and_forget(
                            Some(&on),
                            "Fetch.failRequest",
                            json!({ "requestId": request_id, "errorReason": "BlockedByClient" }),
                        );
                        return false;
                    }
                    let _ = browser.send_and_forget(
                        Some(&on),
                        "Fetch.continueRequest",
                        json!({ "requestId": request_id }),
                    );
                    false
                }
                _ => false,
            }
        });
        if caught_on.is_some() {
            intercepted += 1;
        }
        if fixture.wait_for(&format!("/page/{marker}"), Duration::from_secs(3)) {
            leaks += 1;
        }
    }

    let log = fixture.log();
    let iframe_hits = log
        .iter()
        .filter(|r| r.path.contains("never-oopif-"))
        .count();
    Ok(json!({
        "mode": match mode {
            Q10Mode::Control => "control (nothing armed)",
            Q10Mode::ParentFetchAndAutoAttach => "parent Fetch + auto-attach",
            Q10Mode::AutoAttachOnly => "auto-attach only",
        },
        "parentFetchEnabled": parent_fetch,
        "crossSiteIframeOrigin": fixture.origin(1),
        "crossSiteOrigins": fixture.is_cross_site(0, 1),
        "trials": trials,
        "leaks": leaks,
        "iframeRequestsReachingTheServer": iframe_hits,
        "trialsWhereTheRequestWasActuallyIntercepted": intercepted,
        "catchingSessions": catching_sessions,
        "pageSessionId": session,
        "childSessionIds": child_sessions,
        "markerPauseSessionIds": marker_pause_sessions,
        "childSessionsAttached": attached_children,
        "serverLogSize": log.len(),
        "serverLogSample": log.iter().take(6).map(Record::to_json).collect::<Vec<_>>(),
    }))
}

// ------------------------------------------------------------------- Q11

fn q11_popup(ctx: &Ctx) -> ProbeResult {
    const TRIALS: usize = 20;
    let mut variants = serde_json::Map::new();
    // A fresh browser per variant: otherwise variant A's auto-attach stays
    // active and would intercept variant B's popup, making the attribution
    // meaningless.
    for (name, root_level) in [
        ("A-page-session-autoattach", false),
        ("B-root-session-autoattach", true),
    ] {
        variants.insert(name.into(), q11_variant(ctx, root_level, TRIALS)?);
    }
    let best = ["A-page-session-autoattach", "B-root-session-autoattach"]
        .into_iter()
        .map(|k| {
            (
                k,
                variants[k]["startedPaused"].as_u64().unwrap_or(0),
                variants[k]["leaks"].as_u64().unwrap_or(u64::MAX),
                variants[k]["popupsCreated"].as_u64().unwrap_or(0),
            )
        })
        .collect::<Vec<_>>();
    let any_covered = best
        .iter()
        .any(|(_, paused, leaks, created)| *created > 0 && *paused == *created && *leaks == 0);
    let verdict = if any_covered {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    let answer = best
        .iter()
        .map(|(k, paused, leaks, created)| {
            format!("{k}: {created} popups created, {paused} started paused (waitingForDebugger), {leaks} first requests escaped")
        })
        .collect::<Vec<_>>()
        .join("; ");
    Ok((verdict, answer, Value::Object(variants)))
}

fn q11_variant(ctx: &Ctx, root_level: bool, trials: usize) -> io::Result<Value> {
    let fixture = Fixture::start(2)?;
    let (_scratch, browser) = launch(ctx, "q11")?;
    let owned = owned_tab(browser, Some(document_patterns()))?;
    let (browser, session) = (&owned.browser, owned.session.as_str());
    // Without discovery, `Target.targetCreated` is never emitted at all.
    browser.call("Target.setDiscoverTargets", json!({ "discover": true }))?;
    let auto_attach =
        json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true });
    if root_level {
        browser.call("Target.setAutoAttach", auto_attach.clone())?;
    } else {
        browser.call_session(session, "Target.setAutoAttach", auto_attach.clone())?;
    }

    let opener_page = format!("q11-{}", nonce());
    fixture.set_page(
        &opener_page,
        r#"<!doctype html><title>q11</title><button id="go" style="width:200px;height:60px">go</button>"#,
    );
    browser.send_and_forget(
        Some(session),
        "Page.navigate",
        json!({ "url": fixture.url(0, &format!("/page/{opener_page}")) }),
    )?;
    let mut cursor = 0usize;
    pump(
        browser,
        &mut cursor,
        Instant::now() + Duration::from_secs(6),
        |event| {
            if method_of(event) == "Fetch.requestPaused"
                && let Some(request_id) = paused_request_id(event)
            {
                let on = session_of(event).unwrap_or(session).to_string();
                let _ = browser.send_and_forget(
                    Some(&on),
                    "Fetch.continueRequest",
                    json!({ "requestId": request_id }),
                );
            }
            false
        },
    );

    let mut created = 0usize;
    let mut started_paused = 0usize;
    let mut leaks = 0usize;
    let mut blocked = 0usize;
    for index in 0..trials {
        let marker = format!("never-popup-{}", nonce());
        fixture.set_page(&marker, "<title>leaked</title>");
        let url = fixture.url(1, &format!("/page/{marker}"));
        // Alternate between a bare script call and one behind a real user
        // gesture — the popup blocker treats them differently.
        if index % 2 == 0 {
            browser.send_and_forget(
                Some(session),
                "Runtime.evaluate",
                json!({ "expression": format!("window.open({url:?})") }),
            )?;
        } else {
            browser.call_session(
                session,
                "Runtime.evaluate",
                json!({ "expression": format!("document.getElementById('go').onclick = () => window.open({url:?})") }),
            )?;
            click_center(browser, session, "go")?;
        }

        let mut saw_target = false;
        let mut saw_paused_start = false;
        let deadline = Instant::now() + Duration::from_secs(8);
        pump(browser, &mut cursor, deadline, |event| {
            match method_of(event) {
                "Target.targetCreated" | "Target.targetInfoChanged" => {
                    if event.to_string().contains(&marker) {
                        saw_target = true;
                    }
                    false
                }
                "Target.attachedToTarget" => {
                    let waiting = event
                        .pointer("/params/waitingForDebugger")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let Some(child) = event.pointer("/params/sessionId").and_then(Value::as_str)
                    else {
                        return false;
                    };
                    saw_target = true;
                    if waiting {
                        saw_paused_start = true;
                    }
                    let _ = browser.send_and_forget(
                        Some(child),
                        "Fetch.enable",
                        json!({ "patterns": document_patterns() }),
                    );
                    let _ = browser.send_and_forget(
                        Some(child),
                        "Runtime.runIfWaitingForDebugger",
                        json!({}),
                    );
                    false
                }
                "Fetch.requestPaused" => {
                    let Some(request_id) = paused_request_id(event) else {
                        return false;
                    };
                    let on = session_of(event).unwrap_or(session).to_string();
                    let url = paused_url(event).to_string();
                    let marker_path = format!("/page/{marker}");
                    let verb = if url.ends_with(&marker_path) {
                        "Fetch.failRequest"
                    } else {
                        "Fetch.continueRequest"
                    };
                    let params = if url.ends_with(&marker_path) {
                        json!({ "requestId": request_id, "errorReason": "BlockedByClient" })
                    } else {
                        json!({ "requestId": request_id })
                    };
                    let _ = browser.send_and_forget(Some(&on), verb, params);
                    url.ends_with(&marker_path)
                }
                _ => false,
            }
        });

        if saw_target {
            created += 1;
            if saw_paused_start {
                started_paused += 1;
            }
            if fixture.wait_for(&format!("/page/{marker}"), Duration::from_secs(2)) {
                leaks += 1;
            }
        } else {
            // A blocked popup means the popup blocker was exercised and
            // auto-attach was never tested — not an auto-attach success.
            blocked += 1;
        }

        // Close any popups so the next trial starts clean.
        for info in target_infos(browser)? {
            if info.get("targetId").and_then(Value::as_str) != Some(owned.target.as_str())
                && info.get("type").and_then(Value::as_str) == Some("page")
                && let Some(id) = info.get("targetId").and_then(Value::as_str)
            {
                let _ = browser.call("Target.closeTarget", json!({ "targetId": id }));
            }
        }
    }

    Ok(json!({
        "autoAttachAt": if root_level { "root session" } else { "page session" },
        "trials": trials,
        "popupsCreated": created,
        "popupsBlockedByBrowser": blocked,
        "startedPaused": started_paused,
        "leaks": leaks,
    }))
}

fn click_center(browser: &PipeBrowser, session: &str, element_id: &str) -> io::Result<()> {
    let rect = browser.call_session(
        session,
        "Runtime.evaluate",
        json!({
            "expression": format!("(() => {{ const r = document.getElementById({element_id:?}).getBoundingClientRect(); return {{x: r.x + r.width/2, y: r.y + r.height/2}}; }})()"),
            "returnByValue": true,
        }),
    )?;
    let x = rect
        .pointer("/result/value/x")
        .and_then(Value::as_f64)
        .unwrap_or(10.0);
    let y = rect
        .pointer("/result/value/y")
        .and_then(Value::as_f64)
        .unwrap_or(10.0);
    // Fire-and-forget: a click that opens a popup can block on the popup's
    // own paused navigation, and awaiting the dispatch would then time out.
    for kind in ["mousePressed", "mouseReleased"] {
        browser.send_and_forget(
            Some(session),
            "Input.dispatchMouseEvent",
            json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
        )?;
    }
    Ok(())
}

// ------------------------------------------------------------------- Q12

fn q12_detach_paused(ctx: &Ctx) -> ProbeResult {
    let fixture = Fixture::start(3)?;
    let (_scratch, browser) = launch(ctx, "q12")?;

    // Primary instrument, matching what §V4 actually pauses: a Document
    // request. Its only sound discriminator is the server log — "released" vs
    // "not released" — because a reattached session cannot distinguish a
    // canceled navigation from one still paused.
    let document = q12_document(&browser, &fixture)?;

    // Secondary instrument: a paused `fetch()`, whose promise gives a genuine
    // three-way signal. Not proven equivalent to Document teardown, so it is
    // reported as an analogue, not as the answer for Document requests.
    let subresource = q12_fetch(&browser, &fixture)?;

    let released = document["serverSawItAfterDetach"]
        .as_bool()
        .unwrap_or(false);
    let verdict = if document["paused"].as_bool().unwrap_or(false) {
        Verdict::Pass
    } else {
        Verdict::Inconclusive
    };
    // "Released" is decidable from the Document instrument alone: the origin
    // either got the request or it did not. Only the *not released* branch
    // leaves canceled-vs-still-paused open, and there the fetch() analogue is
    // the only signal available (and is not proven equivalent).
    let answer = if released {
        format!(
            "A Document request left paused by Fetch is RELEASED when its session detaches — the origin server received it. The fetch() analogue agrees: {}. §V4's teardown rule (failRequest every pending approval BEFORE detaching) is therefore mandatory, not merely prudent: detaching without answering lets the request through.",
            subresource["outcome"].as_str().unwrap_or("?")
        )
    } else {
        format!(
            "A Document request left paused by Fetch was NOT released on detach — the origin server never received it. Whether it was canceled or is still paused cannot be decided from the Document instrument; the fetch() analogue (not proven equivalent) says: {}.",
            subresource["outcome"].as_str().unwrap_or("?")
        )
    };
    Ok((
        verdict,
        answer,
        json!({
            "documentVariant": document,
            "fetchAnalogue": subresource,
            "serverLog": fixture.log_json(),
        }),
    ))
}

fn q12_document(browser: &PipeBrowser, fixture: &Fixture) -> io::Result<Value> {
    let created = browser.call("Target.createTarget", json!({ "url": "about:blank" }))?;
    let target = created["targetId"].as_str().unwrap_or_default().to_string();
    let session = attach(browser, &target)?;
    browser.call_session(&session, "Page.enable", json!({}))?;
    browser.call_session(
        &session,
        "Fetch.enable",
        json!({ "patterns": document_patterns() }),
    )?;

    let marker = format!("never-doc-{}", nonce());
    fixture.set_page(&marker, "<title>leaked</title>");
    browser.send_and_forget(
        Some(&session),
        "Page.navigate",
        json!({ "url": fixture.url(2, &format!("/page/{marker}")) }),
    )?;

    let mut cursor = 0usize;
    let paused = browser
        .wait_event_session(
            &mut cursor,
            Some(&session),
            "Fetch.requestPaused",
            Duration::from_secs(10),
        )
        .is_some();
    // Deliberately answer nothing, then detach.
    browser.call("Target.detachFromTarget", json!({ "sessionId": session }))?;
    let leaked = fixture.wait_for(&format!("/page/{marker}"), Duration::from_secs(10));
    let _ = browser.call("Target.closeTarget", json!({ "targetId": target }));

    Ok(json!({
        "paused": paused,
        "serverSawItAfterDetach": leaked,
        "note": "released vs canceled-or-still-paused is all this instrument can decide",
    }))
}

fn q12_fetch(browser: &PipeBrowser, fixture: &Fixture) -> io::Result<Value> {
    let marker = format!("never-fetch-{}", nonce());
    fixture.set_page(&marker, "<title>leaked</title>");
    let target_url = fixture.url(2, &format!("/page/{marker}"));
    let report_origin = fixture.origin(0);
    let page = format!("q12-{}", nonce());
    fixture.set_page(
        &page,
        format!(
            r#"<!doctype html><title>q12</title><script>
fetch({target_url:?})
  .then(() => fetch({report_origin:?} + '/report?tag=q12&outcome=resolved'))
  .catch(() => fetch({report_origin:?} + '/report?tag=q12&outcome=rejected'));
</script>"#
        ),
    );

    let created = browser.call("Target.createTarget", json!({ "url": "about:blank" }))?;
    let target = created["targetId"].as_str().unwrap_or_default().to_string();
    let session = attach(browser, &target)?;
    browser.call_session(&session, "Page.enable", json!({}))?;
    browser.call_session(
        &session,
        "Fetch.enable",
        json!({ "patterns": [
            { "resourceType": "Document", "requestStage": "Request" },
            { "resourceType": "Fetch", "requestStage": "Request" },
            { "resourceType": "XHR", "requestStage": "Request" },
        ]}),
    )?;
    browser.send_and_forget(
        Some(&session),
        "Page.navigate",
        json!({ "url": fixture.url(0, &format!("/page/{page}")) }),
    )?;

    let mut cursor = 0usize;
    let mut paused_the_target = false;
    pump(
        browser,
        &mut cursor,
        Instant::now() + Duration::from_secs(12),
        |event| {
            if method_of(event) != "Fetch.requestPaused" || session_of(event) != Some(&session) {
                return false;
            }
            let url = paused_url(event).to_string();
            let Some(request_id) = paused_request_id(event) else {
                return false;
            };
            if url.ends_with(&format!("/page/{marker}")) {
                // Answer nothing: this is the request we leave paused.
                paused_the_target = true;
                return true;
            }
            let _ = browser.send_and_forget(
                Some(&session),
                "Fetch.continueRequest",
                json!({ "requestId": request_id }),
            );
            false
        },
    );

    browser.call("Target.detachFromTarget", json!({ "sessionId": session }))?;
    let leaked = fixture.wait_for(&format!("/page/{marker}"), Duration::from_secs(10));
    let promise = fixture
        .hits("tag=q12")
        .into_iter()
        .find_map(|r| crate::fixture::query_param(r.path.split_once('?')?.1, "outcome"));
    let _ = browser.call("Target.closeTarget", json!({ "targetId": target }));

    let outcome = match (leaked, promise.as_deref()) {
        (true, Some("resolved")) => "released on detach",
        (false, Some("rejected")) => "canceled on detach",
        (false, None) => "still paused after detach (for at least 10 s)",
        _ => "inconclusive: discriminators disagree",
    };
    Ok(json!({
        "pausedTheTargetRequest": paused_the_target,
        "serverSawItAfterDetach": leaked,
        "fetchPromise": promise,
        "outcome": outcome,
    }))
}

// ------------------------------------------------------------------ table

pub fn run(id: &str, ctx: &Ctx) -> Option<ProbeResult> {
    Some(match id {
        "q0-http-smoke" => http_smoke(ctx),
        "q4-hit-test" => q4_hit_test(ctx),
        "q9-fetch-coverage" => q9_fetch_coverage(ctx),
        "q10-oopif-leak" => q10_oopif_leak(ctx),
        "q11-popup" => q11_popup(ctx),
        "q12-detach-paused" => q12_detach_paused(ctx),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_round_trips_through_the_fixture() {
        let url = "http://127.0.0.1:8080/page/a-b_c?x=1&y=2";
        let encoded = percent_encode(url);
        assert!(!encoded.contains(':'));
        assert!(!encoded.contains('&'));
        assert_eq!(
            crate::fixture::query_param(&format!("to={encoded}"), "to").as_deref(),
            Some(url)
        );
    }

    #[test]
    fn document_patterns_pause_before_the_request_goes_out() {
        let patterns = document_patterns();
        assert_eq!(patterns[0]["requestStage"], "Request");
        assert_eq!(patterns[0]["resourceType"], "Document");
    }

    #[test]
    fn q4_page_keeps_a_reference_to_the_closed_shadow_button() {
        let html = q4_page("http://example.invalid/x");
        assert!(html.contains("window.__closedShadowButton"));
        assert!(html.contains("mode:'closed'"));
    }
}
