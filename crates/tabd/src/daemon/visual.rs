//! Visual-mode browser lifecycle: one browser, owned by this daemon, over the
//! debugging pipe.
//!
//! The rules that make this different from the headless supervisor:
//!
//! - **Never restart.** A browser that exited is a human who closed their
//!   window. The daemon stays up, serving control actions, so the next
//!   `tabd browser` can reopen it.
//! - **`Closed` means the child was reaped**, not merely that CDP dropped.
//!   Transport EOF only moves the state to `Closing`.
//! - **Every event is tagged with a generation**, so a late event from a
//!   previous browser cannot close its replacement.
//! - **The watcher calls `close()` on *every* transport wakeup**, not only a
//!   clean EOF. A reader-side failure (pipe read error, frame cap) ends the
//!   reader but leaves the writer holding the command pipe, so without this
//!   the browser would stay alive with nobody driving it.

use super::*;
use crate::browser::{LaunchSpec, VisualSpec};
use crate::cdp::{CdpClient, ConnectOptions};
use crate::platform;
use std::os::fd::AsRawFd;

/// How long the post-connect CDP handshake may take. This is what catches a
/// launch that the Chromium singleton handed off to an already-running
/// browser: our process exits immediately, but `connect_with` with
/// `bootstrap_tab: false` still returns `Ok` over the dead transport, and the
/// failure would otherwise surface only on some later action.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a graceful `Browser.close` gets before the transport is torn down
/// to force the EOF path (measured at 75-100 ms on Linux, 876 ms on macOS).
const GRACEFUL_CLOSE_WAIT: Duration = Duration::from_secs(10);
/// After the transport is gone, how long before escalating to SIGTERM.
const EOF_EXIT_WAIT: Duration = Duration::from_secs(5);
/// After SIGTERM, how long before SIGKILL. Reaching this loses the human's
/// session-restore data, so it is the last thing tried, never the first.
const SIGTERM_EXIT_WAIT: Duration = Duration::from_secs(5);

// -- Profile lock -----------------------------------------------------------

/// Exclusive ownership of one browser profile, as an advisory `flock` on
/// `<profile>.lock`.
///
/// Keyed on the profile rather than the daemon base dir on purpose: two
/// daemons started with different `$TABD_BASE_DIR` but the same profile would
/// otherwise both drive it. The lock is released when the file is closed,
/// which `Drop` does, and which the kernel does if the daemon dies.
#[derive(Debug)]
pub(super) struct ProfileLock {
    // Held only to keep the descriptor open; closing it releases the flock.
    _file: std::fs::File,
}

impl ProfileLock {
    pub(super) fn acquire(profile_dir: &Path) -> Result<Self> {
        let path = platform::profile_lock_path(profile_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open profile lock {}", path.display()))?;
        // SAFETY: `file` owns a live descriptor for the duration of the call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            bail!(
                "another tabd already owns profile {} ({}): {err}",
                profile_dir.display(),
                path.display()
            );
        }
        Ok(ProfileLock { _file: file })
    }
}

// -- Browser binding --------------------------------------------------------

/// Marker recording which browser owns this profile, kept **beside** the
/// profile like the lock so nothing of ours lives inside the user-data-dir.
fn binding_path(profile_dir: &Path) -> PathBuf {
    let Some(name) = profile_dir.file_name() else {
        return profile_dir.join(".tabd-browser");
    };
    let mut marker = name.to_os_string();
    marker.push(".browser");
    match profile_dir.parent() {
        Some(parent) => parent.join(marker),
        None => PathBuf::from(marker),
    }
}

/// Resolve the browser for this profile, binding the two together.
///
/// A Chromium profile is not portable between browsers: `Preferences`,
/// `Local State`, the extension set and the OSCrypt key are all
/// product-specific, and the cookie store is encrypted under a keyring entry
/// named after the *browser* ("Brave Safe Storage" / "Chrome Safe Storage").
/// Opening one browser's permanent profile with another therefore does not
/// merely fail — it rewrites it.
///
/// The profile path is fixed, but `discover_chromium` honors
/// `$BROWSER_EXECUTABLE` and a discovery order that changes when packages come
/// and go. So the first launch records the executable and every later launch
/// must match it.
fn resolve_browser(profile_dir: &Path) -> Result<PathBuf> {
    // `discover_chromium` returns `$BROWSER_EXECUTABLE` verbatim, so it can be
    // relative (`./browser`) or a name whose meaning moves with `$PATH`.
    // Neither is an identity: the marker would keep matching while a different
    // binary was launched. Resolve before both binding and launch.
    let executable = crate::browser::discover_chromium()?;
    let executable = executable
        .canonicalize()
        .with_context(|| format!("resolve browser executable {}", executable.display()))?;
    bind_browser(profile_dir, &executable)?;
    Ok(executable)
}

/// The check itself, split out so it can be tested without mutating
/// `$BROWSER_EXECUTABLE` in a process running tests in parallel.
fn bind_browser(profile_dir: &Path, executable: &Path) -> Result<()> {
    let marker = binding_path(profile_dir);
    match std::fs::read_to_string(&marker) {
        Ok(recorded) => {
            let recorded = recorded.trim();
            // Compare resolved forms: a marker written before the browser was
            // reinstalled at a symlinked path should still match itself.
            let recorded_path = Path::new(recorded);
            let resolved = recorded_path
                .canonicalize()
                .unwrap_or_else(|_| recorded_path.to_path_buf());
            if resolved != executable {
                bail!(
                    "profile {} is bound to browser {recorded}, but {} would be launched.                      Opening a Chromium profile with a different browser rewrites it.                      Point $BROWSER_EXECUTABLE at {recorded}, or delete {} to rebind                      (only safe if the path moved and the browser is the same one).",
                    profile_dir.display(),
                    executable.display(),
                    marker.display()
                );
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = marker.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            std::fs::write(&marker, executable.to_string_lossy().as_bytes())
                .with_context(|| format!("record browser binding {}", marker.display()))?;
        }
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("read browser binding {}", marker.display()));
        }
    }
    Ok(())
}

// -- State machine ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BrowserState {
    Closed,
    Starting,
    Running,
    Closing,
    Failed(String),
}

impl BrowserState {
    pub(super) fn as_str(&self) -> &str {
        match self {
            BrowserState::Closed => "closed",
            BrowserState::Starting => "starting",
            BrowserState::Running => "running",
            BrowserState::Closing => "closing",
            BrowserState::Failed(_) => "failed",
        }
    }
}

pub(super) struct Lifecycle {
    /// Serializes every transition. Held across the slow work (spawn,
    /// handshake, reap) so two `browser.ensure` calls cannot race a launch.
    /// The watcher takes it too; neither ever waits on the other while
    /// holding it, so there is no cycle.
    gate: Mutex<()>,
    /// The observable value. Never held across an await, so `browser.status`
    /// and `daemon.health` answer immediately even mid-launch.
    observed: std::sync::Mutex<Observed>,
}

struct Observed {
    generation: u64,
    state: BrowserState,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Lifecycle {
            gate: Mutex::new(()),
            observed: std::sync::Mutex::new(Observed {
                generation: 0,
                state: BrowserState::Closed,
            }),
        }
    }
}

impl Lifecycle {
    pub(super) fn state(&self) -> BrowserState {
        self.observed
            .lock()
            .map(|o| o.state.clone())
            .unwrap_or(BrowserState::Closed)
    }

    fn generation(&self) -> u64 {
        self.observed.lock().map(|o| o.generation).unwrap_or(0)
    }

    fn set(&self, state: BrowserState) {
        if let Ok(mut o) = self.observed.lock() {
            o.state = state;
        }
    }

    /// Begin a new browser generation. Returns its id; every event carries it
    /// so a late wakeup from the previous browser is ignored.
    fn begin_generation(&self) -> u64 {
        match self.observed.lock() {
            Ok(mut o) => {
                o.generation += 1;
                o.state = BrowserState::Starting;
                o.generation
            }
            Err(_) => 0,
        }
    }
}

// -- ensure / status --------------------------------------------------------

/// `browser.ensure { urls: [...] }` — idempotent. Brings the browser up if it
/// is down and hands the urls to it either way.
///
/// The urls are delivered **over the pipe this daemon already owns**, not by
/// spawning a second Chromium and trusting the singleton to forward them.
/// That removes the startup race entirely and makes the acknowledgement exact.
///
/// Per-url status is one of:
/// - `opened` — a target was created for it, with its `targetId`. Note that
///   `Target.createTarget` acknowledges **creation, not navigation**.
/// - `requested` — it was passed on the browser's command line during a
///   launch, so it was never separately confirmed.
/// - `failed` — with the error.
///
/// Partial success is a success response, and neither side retries an
/// ambiguous delivery: in a human's browser a duplicate tab is worse than a
/// missing one, and they can click the link again.
pub(super) async fn handle_ensure(
    state: &DaemonState,
    params: &Value,
) -> std::result::Result<Option<Value>, String> {
    let urls = parse_urls(params)?;

    // Shutdown is terminal. Checked before *and* inside the gate: `Closing ->
    // Closed` is not an invitation to reopen.
    if state.drain_started.load(Ordering::Acquire) {
        return Err("daemon is shutting down".to_string());
    }
    let _gate = state.lifecycle.gate.lock().await;
    if state.drain_started.load(Ordering::Acquire) {
        return Err("daemon is shutting down".to_string());
    }

    match state.lifecycle.state() {
        BrowserState::Running => {}
        // `Starting` / `Closing` cannot be observed here: both are only ever
        // set while the gate is held, and we hold it.
        _ => {
            launch(state, &urls)
                .await
                .map_err(|err| format!("browser launch failed: {err:#}"))?;
            // `requested`, not `opened`: these went on the browser's command
            // line, so nothing here observed a target being created for them.
            // Saying "opened" would be a claim we did not check — and opening
            // them a second time over CDP to get a targetId would duplicate
            // every tab.
            return Ok(Some(json!({
                "browserState": state.lifecycle.state().as_str(),
                "results": urls.iter()
                    .map(|u| json!({ "url": u, "status": "requested" }))
                    .collect::<Vec<_>>(),
            })));
        }
    }

    let client = match state.client.lock().await.clone() {
        Some(client) => client,
        None => return Err("browser is running but has no cdp client".to_string()),
    };
    let mut results = Vec::with_capacity(urls.len());
    for url in &urls {
        let outcome = client
            .send_browser("Target.createTarget", json!({ "url": url }))
            .await;
        results.push(match outcome {
            Ok(value) => json!({
                "url": url,
                "status": "opened",
                "targetId": value.get("targetId").and_then(Value::as_str),
            }),
            Err(err) => json!({ "url": url, "status": "failed", "error": err.to_string() }),
        });
    }
    Ok(Some(json!({
        "browserState": state.lifecycle.state().as_str(),
        "results": results,
    })))
}

pub(super) async fn handle_status(
    state: &DaemonState,
    _params: &Value,
) -> std::result::Result<Option<Value>, String> {
    Ok(Some(json!({
        "browserState": state.lifecycle.state().as_str(),
        "profileDir": state.profile_dir.display().to_string(),
        "sessionRestore": platform::session_restore(&state.profile_dir).as_str(),
    })))
}

/// Schemes a url handed to `browser.ensure` may use.
///
/// Deliberately short. These urls reach the browser's command line, and they
/// arrive from a `.desktop` `Exec=… %U` or a macOS `GURL` Apple Event — i.e.
/// from whatever the human clicked, which is not a trusted source.
const ALLOWED_URL_SCHEMES: &[&str] = &["http", "https", "file"];

/// `urls` may be absent (just bring the browser up), a single string, or an
/// array — a `.desktop` `Exec=… %U` hands over several at once.
fn parse_urls(params: &Value) -> std::result::Result<Vec<String>, String> {
    let raw: Vec<String> = match params.get("urls") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "urls must be strings".to_string())
            })
            .collect::<std::result::Result<_, _>>()?,
        Some(_) => return Err("urls must be a string or an array of strings".to_string()),
    };
    raw.into_iter().map(validate_url).collect()
}

/// A url is about to become a positional argument to the human's browser, so
/// "is it shaped like a switch" is a security question, not a tidiness one:
/// `--no-sandbox` would turn off the renderer sandbox, and `--user-data-dir=…`
/// would open a different profile from the one whose lock we hold. Chromium's
/// `--` terminator in `visual_args` is the second layer; this is the first.
fn validate_url(url: String) -> std::result::Result<String, String> {
    if url.is_empty() {
        return Err("invalid 'urls': empty url".to_string());
    }
    if url.starts_with('-') {
        return Err(format!(
            "invalid 'urls': {url:?} looks like a command-line switch"
        ));
    }
    // Control characters (a newline especially) have no place in a url and
    // would corrupt anything that logs or re-parses it.
    if url.chars().any(|c| c.is_control()) {
        return Err("invalid 'urls': url contains a control character".to_string());
    }
    let scheme = match url.split_once(':') {
        Some((scheme, _)) => scheme.to_ascii_lowercase(),
        None => return Err(format!("invalid 'urls': {url:?} has no scheme")),
    };
    if !ALLOWED_URL_SCHEMES.contains(&scheme.as_str()) {
        return Err(format!(
            "invalid 'urls': scheme {scheme:?} is not one of {ALLOWED_URL_SCHEMES:?}"
        ));
    }
    Ok(url)
}

// -- launch / close ---------------------------------------------------------

/// Caller must hold the lifecycle gate.
async fn launch(state: &DaemonState, urls: &[String]) -> Result<()> {
    let generation = state.lifecycle.begin_generation();
    // Resolved every launch, not once at startup: the profile can become a
    // symlink (or stop being one) between launches, and the lock, the browser
    // binding and `--user-data-dir` all have to agree on one identity.
    let profile_dir = match platform::canonical_profile_dir(state.profile_dir.as_ref()) {
        Ok(dir) => dir,
        Err(err) => {
            let reason = format!("{err:#}");
            state.lifecycle.set(BrowserState::Failed(reason.clone()));
            *state.not_ready_reason.lock().await = Some(reason);
            return Err(err);
        }
    };

    let result = launch_inner(state, &profile_dir, urls, generation).await;
    match &result {
        Ok(()) => {
            state.lifecycle.set(BrowserState::Running);
            *state.not_ready_reason.lock().await = None;
            state.ready.store(true, Ordering::Release);
            state.ready_notify.notify_waiters();
        }
        Err(err) => {
            let reason = format!("{err:#}");
            state.ready.store(false, Ordering::Release);
            // A half-launched browser gets the *same* escalation as a normal
            // teardown. `launch_inner` parks the browser and client in `state`
            // before anything that can fail, precisely so this can find them:
            // dropping them as locals would leave a browser that ignores EOF
            // running, unreaped, with its profile lock already released.
            stop_browser(state).await;
            state.lifecycle.set(BrowserState::Failed(reason.clone()));
            *state.not_ready_reason.lock().await = Some(reason);
        }
    }
    result
}

async fn launch_inner(
    state: &DaemonState,
    profile_dir: &Path,
    urls: &[String],
    generation: u64,
) -> Result<()> {
    // Before anything is spawned: one owner per profile, and one browser per
    // profile for the lifetime of that profile.
    let lock = ProfileLock::acquire(profile_dir)?;
    *state.profile_lock.lock().await = Some(lock);
    let executable = resolve_browser(profile_dir)?;

    let mut browser = Browser::launch(LaunchSpec::Visual(VisualSpec {
        profile_dir: profile_dir.to_path_buf(),
        executable,
        start_urls: urls.to_vec(),
        stderr_log: state.base_dir.join("browser-stderr.log"),
    }))
    .await?;
    let transport = browser.take_transport()?;

    // Park the browser in `state` before the first fallible step. From here
    // on every failure path goes through `stop_browser`, which reaps and
    // escalates; a `?` that dropped it as a local would not.
    *state.browser.lock().await = Some(browser);

    let client = Arc::new(
        CdpClient::connect_with(ConnectOptions {
            transport,
            bootstrap_tab: false,
        })
        .await?,
    );
    *state.client.lock().await = Some(client.clone());

    // The liveness handshake. Without it a singleton hand-off looks like a
    // successful connect over an already-dead pipe.
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        client.send_browser("Browser.getVersion", json!({})),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "browser did not answer Browser.getVersion within {}s — \
             it may have handed off to an instance already running on this profile \
             (see {})",
            HANDSHAKE_TIMEOUT.as_secs(),
            state.base_dir.join("browser-stderr.log").display()
        )
    })?
    .with_context(|| {
        format!(
            "browser did not answer Browser.getVersion (see {})",
            state.base_dir.join("browser-stderr.log").display()
        )
    })?;

    tokio::spawn(watch(state.clone(), client, generation));
    Ok(())
}

/// One per browser generation. Waits for the transport to end, then closes
/// the browser down and parks the daemon in `Closed` — it never restarts.
async fn watch(state: DaemonState, client: Arc<CdpClient>, generation: u64) {
    client.transport_closed().await;
    let _gate = state.lifecycle.gate.lock().await;
    if state.lifecycle.generation() != generation {
        return; // a later browser already replaced this one
    }
    if state.lifecycle.state() != BrowserState::Running {
        return; // a deliberate shutdown is already handling it
    }
    state.lifecycle.set(BrowserState::Closing);
    eprintln!("[tabd daemon] visual browser transport ended; closing (no restart)");
    teardown(&state, "browser_closed").await;
}

/// Bring the browser down and record why. Caller must hold the lifecycle gate.
///
/// The escalation is deliberate: `Browser.close` first (it writes the session
/// data), then transport teardown (the measured EOF path), then SIGTERM, and
/// SIGKILL only if all of that failed — a killed browser loses the human's
/// tabs.
pub(super) async fn teardown(state: &DaemonState, reason: &str) {
    state.ready.store(false, Ordering::Release);
    *state.not_ready_reason.lock().await = Some(reason.to_string());
    stop_browser(state).await;
    state.lifecycle.set(BrowserState::Closed);
}

/// Close whatever browser this daemon owns and release its profile lock.
/// Shared by the normal teardown and by a failed launch, so a half-started
/// browser cannot skip the escalation. Safe to call with nothing running.
async fn stop_browser(state: &DaemonState) {
    let client = state.client.lock().await.take();
    if let Some(client) = &client {
        let _ = tokio::time::timeout(
            GRACEFUL_CLOSE_WAIT,
            client.send_browser("Browser.close", json!({})),
        )
        .await;
    }

    let mut browser = state.browser.lock().await.take();
    if let Some(browser) = &mut browser {
        let _ = browser.wait_for_exit(GRACEFUL_CLOSE_WAIT).await;
    }

    // Always, even on a reader-side failure: the writer task still holds the
    // command pipe, and only `close()` releases it.
    if let Some(client) = client {
        let _ = client.close().await;
    }

    if let Some(mut browser) = browser
        && !browser.wait_for_exit(EOF_EXIT_WAIT).await
    {
        eprintln!("[tabd daemon] browser outlived the pipe; SIGTERM");
        browser.terminate();
        if !browser.wait_for_exit(SIGTERM_EXIT_WAIT).await {
            eprintln!("[tabd daemon] browser outlived SIGTERM; SIGKILL (session lost)");
            browser.kill().await;
        }
    }

    // Last: the lock outlives the browser it protects, so it is only released
    // once the process is actually gone.
    let _ = state.profile_lock.lock().await.take();
}

/// `daemon.shutdown` in visual mode. Takes the gate so it cannot interleave
/// with a launch.
pub(super) async fn shutdown(state: &DaemonState) {
    let _gate = state.lifecycle.gate.lock().await;
    if matches!(state.lifecycle.state(), BrowserState::Closed) {
        return;
    }
    state.lifecycle.set(BrowserState::Closing);
    teardown(state, "daemon_shutdown").await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn urls_of(value: Value) -> std::result::Result<Vec<String>, String> {
        parse_urls(&value)
    }

    #[test]
    fn urls_may_be_absent_a_string_or_an_array() {
        assert_eq!(urls_of(json!({})).unwrap(), Vec::<String>::new());
        assert_eq!(
            urls_of(json!({ "urls": null })).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            urls_of(json!({ "urls": "https://a.test/" })).unwrap(),
            vec!["https://a.test/"]
        );
        // `.desktop` Exec=… %U and a burst of GURL events both hand over more
        // than one at a time.
        assert_eq!(
            urls_of(json!({ "urls": ["https://a.test/", "file:///tmp/x.html"] })).unwrap(),
            vec!["https://a.test/", "file:///tmp/x.html"]
        );
    }

    #[test]
    fn switch_shaped_urls_are_refused() {
        // These reach the human's browser's command line. `--no-sandbox` would
        // turn off the renderer sandbox; `--user-data-dir` would open a
        // different profile from the one whose lock we hold.
        for hostile in [
            "--no-sandbox",
            "--user-data-dir=/tmp/elsewhere",
            "-remote-debugging-port=9222",
            "--",
        ] {
            let err = urls_of(json!({ "urls": [hostile] })).expect_err(hostile);
            assert!(err.contains("switch"), "{hostile}: {err}");
        }
    }

    #[test]
    fn only_browsable_schemes_are_accepted() {
        for ok in [
            "http://a.test/",
            "HTTPS://a.test/",
            "file:///tmp/x.html",
            "https://a.test/?q=--no-sandbox",
        ] {
            urls_of(json!({ "urls": [ok] })).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "javascript:alert(1)",
            "data:text/html,x",
            "chrome://settings",
            "a.test",
        ] {
            assert!(
                urls_of(json!({ "urls": [bad] })).is_err(),
                "{bad} was accepted"
            );
        }
    }

    #[test]
    fn empty_and_control_characters_are_refused() {
        assert!(urls_of(json!({ "urls": [""] })).is_err());
        assert!(urls_of(json!({ "urls": ["https://a.test/\nmore"] })).is_err());
        assert!(urls_of(json!({ "urls": [42] })).is_err());
    }

    #[test]
    fn a_binding_matches_through_a_symlinked_executable() {
        // `/usr/bin/brave -> /opt/brave-bin/brave` is the normal packaging
        // shape, so the two spellings must not read as two browsers.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let real = dir.path().join("real-browser");
        std::fs::write(&real, b"#!/bin/sh\n").expect("write");
        let alias = dir.path().join("alias-browser");
        std::os::unix::fs::symlink(&real, &alias).expect("symlink");
        let profile = dir.path().join("profile");

        let canonical = real.canonicalize().expect("canonical");
        bind_browser(&profile, &canonical).expect("bind");
        // The alias resolves to the same binary, so it is the same browser.
        bind_browser(&profile, &alias.canonicalize().expect("canonical alias"))
            .expect("symlinked spelling of the same browser");
    }

    #[test]
    fn binding_marker_sits_beside_the_profile() {
        assert_eq!(
            binding_path(Path::new("/data/tabd/profile")),
            PathBuf::from("/data/tabd/profile.browser")
        );
        // Same profile, same marker, trailing slash or not.
        assert_eq!(
            binding_path(Path::new("/data/tabd/profile/")),
            binding_path(Path::new("/data/tabd/profile"))
        );
    }

    #[test]
    fn a_profile_is_bound_to_one_browser() {
        // Opening a Chromium profile with a different browser rewrites it:
        // Preferences, Local State, the extension set and the OSCrypt keyring
        // entry are all product-specific. `discover_chromium` honors
        // $BROWSER_EXECUTABLE and a discovery order that shifts as packages
        // come and go, so the profile has to remember.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let profile = dir.path().join("profile");

        bind_browser(&profile, Path::new("/opt/browser-a")).expect("first launch binds");
        assert!(binding_path(&profile).exists());
        bind_browser(&profile, Path::new("/opt/browser-a")).expect("same browser is fine");

        let err = bind_browser(&profile, Path::new("/opt/browser-b"))
            .expect_err("a different browser must be refused");
        let msg = err.to_string();
        assert!(msg.contains("/opt/browser-a"), "{msg}");
        assert!(msg.contains("bound to browser"), "{msg}");
        // The refusal must not have rebound the marker.
        assert_eq!(
            std::fs::read_to_string(binding_path(&profile))
                .unwrap()
                .trim(),
            "/opt/browser-a"
        );
    }
}
