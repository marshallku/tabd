//! CDP over `--remote-debugging-pipe` (fd 3 in, fd 4 out), blocking std I/O.
//!
//! This is deliberately *not* a prototype of the eventual tokio transport in
//! `crates/tabd/src/cdp.rs`. It exists to answer whether the OS-level
//! mechanism behaves the way `docs/visual-mode-plan.md` assumes.

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const REAP_POLL: Duration = Duration::from_millis(25);

/// The visual-mode flag set from the design doc. What is *absent* is the
/// point: no `--no-sandbox`, no `--disable-extensions`, no `--disable-sync`,
/// no `--disable-background-networking`, no `--enable-automation`, no
/// `--headless`.
pub fn visual_base_args() -> Vec<String> {
    let mut args: Vec<String> = ["--no-first-run", "--no-default-browser-check"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // On this Hyprland box Brave picks the X11 ozone backend by default and
    // dies with "Missing X server or $DISPLAY" when launched from a plain ssh
    // shell. The hint makes it select Wayland when a compositor is there and
    // fall back to X11 otherwise — and it is also what makes Q7's app_id
    // question meaningful, since `--class` only reaches `app_id` on a native
    // Wayland window.
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        args.push("--ozone-platform-hint=auto".into());
    }
    // Escape hatch for bisecting launch flags against a real browser (e.g.
    // "is the network service failing because the sandbox cannot start?").
    if let Ok(extra) = std::env::var("TABD_SPIKE_EXTRA_ARGS") {
        args.extend(extra.split_whitespace().map(str::to_string));
    }
    args
}

pub fn browser_executable() -> PathBuf {
    crate::browser::Browser::resolve().executable
}

#[derive(Default)]
struct State {
    responses: HashMap<u64, Value>,
    /// Ids sent fire-and-forget. The reader stores every id-bearing frame in
    /// `responses` and there is no pending registry, so without this set an
    /// un-awaited response would sit there forever and the map would grow with
    /// every Fetch decision.
    forgotten: HashSet<u64>,
    events: Vec<Value>,
    /// Set when the browser's end of the response pipe hit EOF, or the reader
    /// was told to stop. Waiters must wake up instead of hanging.
    closed: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

/// A browser launched by us, driven over the debugging pipe.
pub struct PipeBrowser {
    child: Child,
    pid: u32,
    /// Parent's write end (the peer of the browser's fd 3). `None` once
    /// disconnected.
    write_fd: Mutex<Option<OwnedFd>>,
    wake_write: Option<OwnedFd>,
    reader: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    disconnected: bool,
    pub stderr_log: PathBuf,
}

impl PipeBrowser {
    /// Spawn `exe` with `--remote-debugging-pipe` and the given user-data-dir.
    /// The caller is responsible for having obtained `user_data_dir` from
    /// `Scratch` — `launch` re-asserts the safety guard regardless.
    pub fn launch(
        exe: &Path,
        user_data_dir: &Path,
        extra_args: &[String],
        start_url: Option<&str>,
    ) -> io::Result<Self> {
        crate::scratch::assert_safe_user_data_dir(user_data_dir)?;

        let (cmd_r, cmd_w) = pipe2_cloexec()?;
        let (res_r, res_w) = pipe2_cloexec()?;
        let (wake_r, wake_w) = pipe2_cloexec()?;

        let stderr_log = user_data_dir.join("spike-browser-stderr.log");
        std::fs::create_dir_all(user_data_dir)?;
        let stderr_file = std::fs::File::create(&stderr_log)?;

        let mut cmd = Command::new(exe);
        cmd.arg("--remote-debugging-pipe")
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .args(extra_args);
        if let Some(url) = start_url {
            cmd.arg(url);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file));

        let cmd_r_raw = cmd_r.as_raw_fd();
        let res_w_raw = res_w.as_raw_fd();
        // SAFETY: the closure runs between fork and exec and calls only
        // async-signal-safe functions (dup2/fcntl/close).
        unsafe {
            cmd.pre_exec(move || {
                // Move both ends out of the 0-9 range first: the descriptors
                // we were handed could themselves already be 3 or 4, and
                // `dup2(n, n)` is a no-op that leaves FD_CLOEXEC set.
                let tmp_in = libc::fcntl(cmd_r_raw, libc::F_DUPFD, 10);
                if tmp_in < 0 {
                    return Err(io::Error::last_os_error());
                }
                let tmp_out = libc::fcntl(res_w_raw, libc::F_DUPFD, 10);
                if tmp_out < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::dup2(tmp_in, 3) < 0 || libc::dup2(tmp_out, 4) < 0 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(tmp_in);
                libc::close(tmp_out);
                // dup2 clears FD_CLOEXEC on the new descriptor, but be explicit
                // — this is the one thing that must be true after exec.
                for fd in [3, 4] {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }

        let mut child = cmd.spawn()?;
        let pid = child.id();
        drop(cmd_r);
        drop(res_w);

        // Everything from here on can fail, and a bare `Child` drop neither
        // kills nor reaps. Leaving a live browser behind would be worse than
        // the error itself: the caller's `Scratch` would then recursively
        // delete the profile directory out from under a running browser.
        let setup = || -> io::Result<(Arc<Shared>, JoinHandle<()>)> {
            set_nonblocking(cmd_w.as_raw_fd())?;
            let shared = Arc::new(Shared::default());
            let reader_shared = Arc::clone(&shared);
            let reader = std::thread::Builder::new()
                .name("cdp-pipe-reader".into())
                .spawn(move || reader_loop(res_r, wake_r, reader_shared))?;
            Ok((shared, reader))
        };
        let (shared, reader) = match setup() {
            Ok(pair) => pair,
            Err(err) => {
                kill_and_reap(&mut child, pid);
                return Err(err);
            }
        };

        Ok(PipeBrowser {
            child,
            pid,
            write_fd: Mutex::new(Some(cmd_w)),
            wake_write: Some(wake_w),
            reader: Some(reader),
            shared,
            next_id: AtomicU64::new(1),
            disconnected: false,
            stderr_log,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Liveness of the browser we own. Uses `try_wait`, never `/proc/<pid>`:
    /// an exited-but-unreaped child still has a `/proc` entry and would read
    /// as alive (the same trap `crates/tabd/src/browser.rs` documents).
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn wait_bounded(&mut self, limit: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + limit;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(REAP_POLL);
        }
    }

    pub fn call(&self, method: &str, params: Value) -> io::Result<Value> {
        self.call_inner(method, params, None)
    }

    pub fn call_session(&self, session: &str, method: &str, params: Value) -> io::Result<Value> {
        self.call_inner(method, params, Some(session))
    }

    fn call_inner(&self, method: &str, params: Value, session: Option<&str>) -> io::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut frame = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            frame["sessionId"] = json!(session);
        }
        self.send(&frame.to_string())?;

        let deadline = Instant::now() + CALL_TIMEOUT;
        let mut guard = self.shared.state.lock().unwrap();
        loop {
            if let Some(resp) = guard.responses.remove(&id) {
                if let Some(err) = resp.get("error") {
                    return Err(io::Error::other(format!("{method} failed: {err}")));
                }
                return Ok(resp.get("result").cloned().unwrap_or(Value::Null));
            }
            if guard.closed {
                return Err(io::Error::other(format!(
                    "{method}: pipe closed before a response arrived"
                )));
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{method}: no response within {CALL_TIMEOUT:?}"),
                ));
            };
            let (next, _) = self.shared.cv.wait_timeout(guard, remaining).unwrap();
            guard = next;
        }
    }

    fn send(&self, json: &str) -> io::Result<()> {
        let guard = self.write_fd.lock().unwrap();
        let Some(fd) = guard.as_ref() else {
            return Err(io::Error::other("send after disconnect"));
        };
        let mut buf = Vec::with_capacity(json.len() + 1);
        buf.extend_from_slice(json.as_bytes());
        buf.push(0);
        write_all_bounded(fd.as_raw_fd(), &buf, WRITE_TIMEOUT)
    }

    /// Send a command and never wait for its response.
    ///
    /// A CDP command **must** carry an `id` — a frame without one can be
    /// rejected outright, which for `Fetch.continueRequest`/`failRequest`
    /// would silently leave the request paused. So an id is allocated as
    /// usual, and it is the *response* that is discarded. This mirrors the
    /// dialog path in `crates/tabd/src/cdp.rs` and is the fire-and-forget
    /// shape the visual-mode design specifies for its reader task.
    pub fn send_and_forget(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> io::Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut frame = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            frame["sessionId"] = json!(session);
        }
        self.shared.state.lock().unwrap().forgotten.insert(id);
        match self.send(&frame.to_string()) {
            Ok(()) => Ok(()),
            Err(err) => {
                // A failed write means no response will ever arrive, so the
                // id has to be reclaimed here.
                self.shared.state.lock().unwrap().forgotten.remove(&id);
                Err(err)
            }
        }
    }

    /// Wait for an event with `method` on a specific session.
    pub fn wait_event_session(
        &self,
        cursor: &mut usize,
        session: Option<&str>,
        method: &str,
        limit: Duration,
    ) -> Option<Value> {
        self.wait_event(cursor, limit, |ev| {
            ev.get("method").and_then(Value::as_str) == Some(method)
                && match session {
                    Some(want) => ev.get("sessionId").and_then(Value::as_str) == Some(want),
                    None => true,
                }
        })
    }

    /// Events observed so far, from `*cursor` onward; advances the cursor.
    pub fn drain_events(&self, cursor: &mut usize) -> Vec<Value> {
        let guard = self.shared.state.lock().unwrap();
        let out = guard.events[(*cursor).min(guard.events.len())..].to_vec();
        *cursor = guard.events.len();
        out
    }

    /// Block until an event matching `pred` arrives (from `*cursor` onward).
    pub fn wait_event<F>(&self, cursor: &mut usize, limit: Duration, pred: F) -> Option<Value>
    where
        F: Fn(&Value) -> bool,
    {
        let deadline = Instant::now() + limit;
        let mut guard = self.shared.state.lock().unwrap();
        loop {
            while *cursor < guard.events.len() {
                let ev = guard.events[*cursor].clone();
                *cursor += 1;
                if pred(&ev) {
                    return Some(ev);
                }
            }
            if guard.closed {
                return None;
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (next, _) = self.shared.cv.wait_timeout(guard, remaining).unwrap();
            guard = next;
        }
    }

    /// Close **every** parent-side descriptor of both pipes, leaving this
    /// process alive. The reader thread owns the browser-response descriptor,
    /// so it has to be woken and joined before that descriptor is gone — a
    /// plain `drop` of a handle would not close it.
    ///
    /// Order matters for what Q1 measures: the parent's read end goes first,
    /// then the write end, which is the peer of the browser's fd 3 and the
    /// endpoint whose EOF Chromium reacts to.
    pub fn disconnect(&mut self) {
        if self.disconnected {
            return;
        }
        self.disconnected = true;
        if let Some(wake) = self.wake_write.take() {
            let byte = [1u8];
            // SAFETY: valid fd, valid buffer. Best effort: if the reader is
            // already gone the join below returns immediately anyway.
            unsafe {
                libc::write(wake.as_raw_fd(), byte.as_ptr().cast(), 1);
            }
        }
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
        *self.write_fd.lock().unwrap() = None;
    }

    /// Graceful close, then signal escalation. Returns once the child is
    /// reaped, so a caller may safely delete the profile directory after.
    pub fn shutdown(&mut self) {
        if !self.disconnected && self.is_alive() {
            let _ = self.call("Browser.close", json!({}));
            let _ = self.wait_bounded(Duration::from_secs(2));
        }
        self.disconnect();
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        // SAFETY: the child is not reaped yet, so the pid is still ours.
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGTERM);
        }
        if self.wait_bounded(Duration::from_secs(3)).is_some() {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PipeBrowser {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// SIGTERM → bounded wait → SIGKILL → reap. Used on the launch error path,
/// where there is no `PipeBrowser` to carry the usual RAII teardown.
fn kill_and_reap(child: &mut Child, pid: u32) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    // SAFETY: the child is not reaped yet, so the pid is still ours.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(REAP_POLL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn reader_loop(res_r: OwnedFd, wake_r: OwnedFd, shared: Arc<Shared>) {
    let mut buf = Vec::<u8>::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: res_r.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_r.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: both descriptors are owned by this thread and alive.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, 500) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[1].revents != 0 {
            break; // asked to stop
        }
        if fds[0].revents == 0 {
            continue;
        }
        // SAFETY: `chunk` is a valid writable buffer of the given length.
        let n = unsafe { libc::read(res_r.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if n == 0 {
            break; // browser closed its end
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        let frames = split_frames(&mut buf);
        if frames.is_empty() {
            continue;
        }
        let mut guard = shared.state.lock().unwrap();
        for frame in frames {
            match serde_json::from_slice::<Value>(&frame) {
                Ok(value) => {
                    if let Some(id) = value.get("id").and_then(Value::as_u64) {
                        if guard.forgotten.remove(&id) {
                            // Fire-and-forget: nobody is waiting, so drop it
                            // rather than letting `responses` grow unbounded.
                        } else {
                            guard.responses.insert(id, value);
                        }
                    } else {
                        guard.events.push(value);
                    }
                }
                Err(err) => guard.events.push(json!({
                    "spikeParseError": err.to_string(),
                    "raw": String::from_utf8_lossy(&frame),
                })),
            }
        }
        drop(guard);
        shared.cv.notify_all();
    }

    // Dropping these here is the whole point of the wake pipe: after the join
    // in `disconnect`, no descriptor of either pipe is left in this process.
    drop(res_r);
    drop(wake_r);
    {
        let mut guard = shared.state.lock().unwrap();
        guard.closed = true;
        // Nothing can arrive any more, so held ids would never be reclaimed.
        guard.forgotten.clear();
    }
    shared.cv.notify_all();
}

/// Split `buf` on NUL, leaving any trailing partial frame in place.
pub fn split_frames(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut start = 0usize;
    for (i, byte) in buf.iter().enumerate() {
        if *byte == 0 {
            if i > start {
                frames.push(buf[start..i].to_vec());
            }
            start = i + 1;
        }
    }
    if start > 0 {
        buf.drain(..start);
    }
    frames
}

/// A close-on-exec pipe. Linux gets it atomically from `pipe2`; macOS has no
/// `pipe2`, so the flag is set right after `pipe` (the spike is single-threaded
/// at this point, so the window is not a practical concern there).
fn pipe2_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    #[cfg(target_os = "linux")]
    // SAFETY: `fds` is a valid 2-element array.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(target_os = "linux"))]
    // SAFETY: as above.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe/pipe2 just handed us two fresh, owned descriptors.
    let pair = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    #[cfg(not(target_os = "linux"))]
    for fd in [pair.0.as_raw_fd(), pair.1.as_raw_fd()] {
        // SAFETY: both descriptors are live and owned by `pair`.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(pair)
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is a live descriptor owned by the caller.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Write everything within `limit`. The descriptor is non-blocking, so a frame
/// larger than the pipe's capacity cannot wedge the caller: `poll` gates each
/// retry and the monotonic deadline bounds the whole loop. (`poll(POLLOUT)`
/// followed by a *blocking* write would not be bounded — readiness can lapse,
/// and a large write can block after a partial transfer.)
pub fn write_all_bounded(fd: RawFd, buf: &[u8], limit: Duration) -> io::Result<()> {
    let deadline = Instant::now() + limit;
    let mut written = 0usize;
    while written < buf.len() {
        // SAFETY: writing `len - written` bytes from inside `buf`.
        let n = unsafe { libc::write(fd, buf[written..].as_ptr().cast(), buf.len() - written) };
        if n > 0 {
            written += n as usize;
            continue;
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => {}
            _ => return Err(err),
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("pipe write stalled after {written}/{} bytes", buf.len()),
            ));
        };
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: single valid pollfd.
        let rc = unsafe {
            libc::poll(
                &mut pfd,
                1,
                remaining.as_millis().min(i32::MAX as u128) as libc::c_int,
            )
        };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if rc == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("pipe write stalled after {written}/{} bytes", buf.len()),
            ));
        }
    }
    Ok(())
}

/// A plain (non-CDP) browser launch, used as Q3's control. Killed on drop.
pub struct PlainBrowser {
    child: Child,
    pub pid: u32,
}

impl PlainBrowser {
    pub fn launch(
        exe: &Path,
        user_data_dir: &Path,
        extra_args: &[String],
        start_url: Option<&str>,
    ) -> io::Result<Self> {
        crate::scratch::assert_safe_user_data_dir(user_data_dir)?;
        std::fs::create_dir_all(user_data_dir)?;
        let mut cmd = Command::new(exe);
        cmd.arg(format!("--user-data-dir={}", user_data_dir.display()))
            .args(extra_args);
        if let Some(url) = start_url {
            cmd.arg(url);
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(
                user_data_dir.join("spike-browser-stderr.log"),
            )?))
            .spawn()?;
        let pid = child.id();
        Ok(PlainBrowser { child, pid })
    }
}

impl Drop for PlainBrowser {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        // SAFETY: not reaped yet, so the pid is still ours.
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(REAP_POLL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames_of(input: &[u8], carry: &mut Vec<u8>) -> Vec<String> {
        carry.extend_from_slice(input);
        split_frames(carry)
            .into_iter()
            .map(|f| String::from_utf8(f).unwrap())
            .collect()
    }

    #[test]
    fn splits_multiple_frames_in_one_read() {
        let mut carry = Vec::new();
        let got = frames_of(b"{\"a\":1}\0{\"b\":2}\0", &mut carry);
        assert_eq!(got, vec!["{\"a\":1}", "{\"b\":2}"]);
        assert!(carry.is_empty());
    }

    #[test]
    fn keeps_a_frame_split_across_reads() {
        let mut carry = Vec::new();
        assert!(frames_of(b"{\"a\":", &mut carry).is_empty());
        assert!(frames_of(b"1}", &mut carry).is_empty());
        assert_eq!(frames_of(b"\0", &mut carry), vec!["{\"a\":1}"]);
        assert!(carry.is_empty());
    }

    #[test]
    fn keeps_a_trailing_partial_after_complete_frames() {
        let mut carry = Vec::new();
        assert_eq!(
            frames_of(b"{\"a\":1}\0{\"b\"", &mut carry),
            vec!["{\"a\":1}"]
        );
        assert_eq!(carry, b"{\"b\"");
        assert_eq!(frames_of(b":2}\0", &mut carry), vec!["{\"b\":2}"]);
    }

    #[test]
    fn ignores_empty_frames_from_consecutive_nuls() {
        let mut carry = Vec::new();
        assert_eq!(
            frames_of(b"\0\0{\"a\":1}\0\0", &mut carry),
            vec!["{\"a\":1}"]
        );
        assert!(carry.is_empty());
    }

    #[test]
    fn write_all_bounded_survives_a_frame_larger_than_the_pipe_buffer() {
        let (r, w) = pipe2_cloexec().unwrap();
        set_nonblocking(w.as_raw_fd()).unwrap();
        // Comfortably larger than Linux's default 64 KiB pipe capacity, so the
        // write can only complete if the loop drains against a live reader.
        let payload = vec![b'x'; 512 * 1024];
        let expected = payload.len();
        let reader = std::thread::spawn(move || {
            let mut total = 0usize;
            let mut chunk = [0u8; 8192];
            while total < expected {
                // SAFETY: valid fd and buffer.
                let n =
                    unsafe { libc::read(r.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
                if n <= 0 {
                    break;
                }
                total += n as usize;
            }
            total
        });
        write_all_bounded(w.as_raw_fd(), &payload, Duration::from_secs(5)).unwrap();
        assert_eq!(reader.join().unwrap(), expected);
    }

    /// A fire-and-forget command must not leave an entry behind: the reader
    /// stores every id-bearing frame, so without the forgotten-id set the
    /// response map would grow with every Fetch decision.
    #[test]
    fn forgotten_responses_are_dropped_not_accumulated() {
        let shared = Arc::new(Shared::default());
        {
            let mut guard = shared.state.lock().unwrap();
            guard.forgotten.insert(7);
        }
        // Simulate what the reader does for a response to a forgotten id.
        {
            let mut guard = shared.state.lock().unwrap();
            let id = 7u64;
            if guard.forgotten.remove(&id) {
                // dropped
            } else {
                guard.responses.insert(id, json!({"id": id}));
            }
            assert!(
                guard.responses.is_empty(),
                "forgotten id must not be stored"
            );
            assert!(guard.forgotten.is_empty(), "the id must be reclaimed");
        }
        // An id nobody forgot is still delivered.
        {
            let mut guard = shared.state.lock().unwrap();
            let id = 8u64;
            if !guard.forgotten.remove(&id) {
                guard.responses.insert(id, json!({"id": id}));
            }
            assert_eq!(guard.responses.len(), 1);
        }
    }

    #[test]
    fn write_all_bounded_times_out_instead_of_hanging() {
        let (_r, w) = pipe2_cloexec().unwrap();
        set_nonblocking(w.as_raw_fd()).unwrap();
        // Nobody reads `_r`, so the pipe fills and the write must give up.
        let payload = vec![b'x'; 4 * 1024 * 1024];
        let started = Instant::now();
        let err = write_all_bounded(w.as_raw_fd(), &payload, Duration::from_millis(200))
            .expect_err("must not block forever");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
