//! Small shells around the host tools the probes need (pgrep, hyprctl, grim,
//! sqlite3, notify-send). Kept in one place so a probe body stays readable.

use serde_json::Value;
use std::io;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

pub fn run(program: &str, args: &[&str]) -> io::Result<Output> {
    Command::new(program).args(args).output()
}

pub fn stdout_of(program: &str, args: &[&str]) -> String {
    match run(program, args) {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(err) => format!("<{program} failed: {err}>"),
    }
}

/// PIDs of Brave **browser** processes — the ones without a `--type=` switch.
/// Renderers, zygotes and GPU processes all share the executable name, so a
/// bare `pgrep -c brave` would count a single browser many times over.
pub fn brave_browser_pids() -> Vec<u32> {
    let Ok(out) = run("pgrep", &["-a", "brave"]) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| !line.contains("--type="))
        .filter_map(|line| line.split_whitespace().next())
        .filter_map(|pid| pid.parse::<u32>().ok())
        .collect()
}

pub fn cmdline_of(pid: u32) -> Vec<String> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    raw.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect()
}

pub fn hyprctl_clients() -> Option<Vec<Value>> {
    let out = run("hyprctl", &["clients", "-j"]).ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()?
        .as_array()
        .cloned()
}

/// Wait for a Hyprland client whose `pid` is `pid` (or a descendant of it —
/// Brave's window can belong to a child of the process we spawned).
pub fn wait_for_client(pid: u32, limit: Duration) -> Option<Value> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(clients) = hyprctl_clients() {
            let owned: Vec<&Value> = clients
                .iter()
                .filter(|c| {
                    c.get("pid")
                        .and_then(Value::as_u64)
                        .map(|p| p as u32 == pid || is_descendant_of(p as u32, pid))
                        .unwrap_or(false)
                })
                .collect();
            if let Some(found) = owned.first() {
                return Some((*found).clone());
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Walk `/proc/<pid>/stat`'s ppid chain (bounded) looking for `ancestor`.
pub fn is_descendant_of(mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..16 {
        let Some(ppid) = parent_pid(pid) else {
            return false;
        };
        if ppid == ancestor {
            return true;
        }
        if ppid <= 1 {
            return false;
        }
        pid = ppid;
    }
    false
}

fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `comm` can contain spaces and parens, so split at the LAST ')' — ppid is
    // then the second whitespace-separated field (state, ppid, ...).
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().nth(1)?.parse().ok()
}

pub fn active_workspace_id() -> Option<i64> {
    let out = run("hyprctl", &["activeworkspace", "-j"]).ok()?;
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()?
        .get("id")?
        .as_i64()
}

fn client_workspace_id(client: &Value) -> Option<i64> {
    client.pointer("/workspace/id")?.as_i64()
}

/// Wait until a client is both on the visible workspace and the active window,
/// and return its refreshed record.
///
/// This *verifies* rather than *enforces*: `grim -g` happily captures the
/// screen region a hidden or obscured window's geometry points at — which is
/// whatever is actually visible there, e.g. the terminal. Geometry alone is
/// not evidence that the right window was captured. A freshly launched browser
/// window is focused by the compositor anyway, so there is no need to move the
/// user's windows around; if it never becomes active, the capture is refused.
pub fn await_visible_active(client: &Value, limit: Duration) -> io::Result<Value> {
    let address = client
        .get("address")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "client has no address"))?
        .to_string();

    let deadline = Instant::now() + limit;
    loop {
        let found = hyprctl_clients()
            .unwrap_or_default()
            .into_iter()
            .find(|c| c.get("address").and_then(Value::as_str) == Some(address.as_str()));
        let last = match found {
            Some(refreshed) => {
                let active_ws = active_workspace_id();
                let on_visible =
                    active_ws.is_some() && client_workspace_id(&refreshed) == active_ws;
                let active_window = active_window_address();
                if on_visible && active_window.as_deref() == Some(address.as_str()) {
                    return Ok(refreshed);
                }
                format!(
                    "on workspace {:?} while {:?} is visible; active window is {:?}",
                    client_workspace_id(&refreshed),
                    active_ws,
                    active_window
                )
            }
            None => "the window disappeared".to_string(),
        };
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "{address} never became the visible, active window ({last}); a capture would show something else"
            )));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn active_window_address() -> Option<String> {
    let out = run("hyprctl", &["activewindow", "-j"]).ok()?;
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()?
        .get("address")?
        .as_str()
        .map(str::to_string)
}

/// Screenshot exactly the given Hyprland client's rectangle. `grim` has no
/// "capture this window" switch — the geometry has to come from hyprctl.
pub fn grim_client(client: &Value, out: &Path) -> io::Result<()> {
    let at = client
        .get("at")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "hypr client has no `at`"))?;
    let size = client
        .get("size")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "hypr client has no `size`"))?;
    let geometry = format!(
        "{},{} {}x{}",
        at[0].as_i64().unwrap_or(0),
        at[1].as_i64().unwrap_or(0),
        size[0].as_i64().unwrap_or(0),
        size[1].as_i64().unwrap_or(0),
    );
    let out_str = out.to_string_lossy().to_string();
    let result = run("grim", &["-g", &geometry, &out_str])?;
    if !result.status.success() {
        return Err(io::Error::other(format!(
            "grim -g '{geometry}' failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    Ok(())
}

/// Query a SQLite file **read-only**, leaving its `-wal`/`-shm` untouched.
/// No checkpoint, no `journal_mode` change: the WAL is part of the database's
/// persistent state and the browser has to consume it, not us.
pub fn sqlite_query(db: &Path, sql: &str) -> io::Result<Vec<Vec<String>>> {
    let uri = format!("file:{}?mode=ro", db.to_string_lossy());
    let out = run("sqlite3", &["-readonly", "-separator", "\u{1f}", &uri, sql])?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "sqlite3 {}: {}",
            db.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('\u{1f}').map(str::to_string).collect())
        .collect())
}

/// Chromium timestamps are microseconds since 1601-01-01; 0 means "session".
pub fn webkit_micros_now() -> i64 {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0);
    unix + 11_644_473_600_000_000
}

/// A one-shot HTTP collector on 127.0.0.1.
///
/// Some readings have to come from a browser we have **no** CDP connection to
/// (Q2's control launch), so the page reports them over the network instead:
/// `GET /report?tag=…&wd=…`. Requests are collected until `limit` elapses or
/// `expected` reports have arrived.
pub struct Reporter {
    pub port: u16,
    handle: Option<std::thread::JoinHandle<Vec<String>>>,
}

impl Reporter {
    pub fn start(expected: usize, limit: Duration) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let handle = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let deadline = Instant::now() + limit;
            let mut seen = Vec::new();
            while seen.len() < expected && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0u8; 2048];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let request = String::from_utf8_lossy(&buf[..n]).to_string();
                        if let Some(line) = request.lines().next()
                            && let Some(path) = line.split_whitespace().nth(1)
                        {
                            seen.push(path.to_string());
                        }
                        // CORS-open so a file:// page's fetch is not blocked.
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
            seen
        });
        Ok(Reporter {
            port,
            handle: Some(handle),
        })
    }

    /// Block until every expected report arrives or the deadline passes.
    pub fn collect(mut self) -> Vec<String> {
        self.handle
            .take()
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default()
    }
}

/// Pull one query parameter out of a collected `/report?...` path.
pub fn report_param(path: &str, key: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}
