//! A logging HTTP origin for the Fetch / auto-attach probes.
//!
//! `Fetch.requestPaused` firing proves interception happened; it does not
//! prove the request never went out, and its absence does not prove a block.
//! So the decisive evidence in every probe here is **this server's own request
//! log**, with CDP traces as corroboration.
//!
//! Three ports are bound so that same-origin traffic, cross-origin iframe
//! traffic and "must never be reached" traffic are distinguishable by port
//! alone.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct Record {
    pub port: u16,
    pub method: String,
    pub path: String,
    pub ts_ms: u128,
    pub sec_fetch_dest: Option<String>,
    pub sec_fetch_mode: Option<String>,
    pub sec_purpose: Option<String>,
    /// Present only when a service worker's navigation preload actually ran —
    /// the positive control for that case, which `Sec-Purpose` cannot give.
    pub sw_navigation_preload: Option<String>,
}

impl Record {
    pub fn to_json(&self) -> Value {
        json!({
            "port": self.port,
            "method": self.method,
            "path": self.path,
            "tsMs": self.ts_ms as u64,
            "secFetchDest": self.sec_fetch_dest,
            "secFetchMode": self.sec_fetch_mode,
            "secPurpose": self.sec_purpose,
            "swNavigationPreload": self.sw_navigation_preload,
        })
    }
}

#[derive(Default)]
pub struct Shared {
    log: Mutex<Vec<Record>>,
    pages: Mutex<HashMap<String, String>>,
}

/// Loopback addresses, one per fixture port.
///
/// Chromium's site isolation keys on scheme + site, and **ports are not part
/// of a site** — `127.0.0.1:A` and `127.0.0.1:B` are cross-origin but
/// same-site, so an iframe between them stays in the same renderer and never
/// becomes an OOPIF. Distinct loopback IPs are separate sites, which is what
/// Q10 and Q11 actually need. All of 127.0.0.0/8 is local on Linux.
const HOSTS: &[&str] = &["127.0.0.1", "127.0.0.2", "127.0.0.3"];

pub struct Fixture {
    hosts: Vec<String>,
    ports: Vec<u16>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
}

impl Fixture {
    /// Bind `count` loopback ports, each with its own accept thread.
    pub fn start(count: usize) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let mut ports = Vec::with_capacity(count);
        let mut hosts = Vec::with_capacity(count);

        for index in 0..count {
            // Linux has all of 127.0.0.0/8 bound; macOS only has 127.0.0.1
            // unless an alias was added, so fall back rather than fail. On a
            // fallback the cross-site probes degrade to same-site and say so.
            let preferred = HOSTS[index % HOSTS.len()];
            let (host, listener) = match TcpListener::bind((preferred, 0)) {
                Ok(listener) => (preferred.to_string(), listener),
                Err(_) => (
                    "127.0.0.1".to_string(),
                    TcpListener::bind(("127.0.0.1", 0))?,
                ),
            };
            hosts.push(host);
            let port = listener.local_addr()?.port();
            ports.push(port);
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let Ok(stream) = conn else { continue };
                    // One thread per connection: a `/slow` route must not
                    // serialize the unrelated requests of a timing-sensitive
                    // probe.
                    let shared = Arc::clone(&shared);
                    std::thread::spawn(move || {
                        let _ = handle(stream, port, &shared);
                    });
                }
            });
        }
        Ok(Fixture {
            hosts,
            ports,
            shared,
            stop,
        })
    }

    #[cfg(test)]
    pub fn port(&self, index: usize) -> u16 {
        self.ports[index]
    }

    pub fn origin(&self, index: usize) -> String {
        format!("http://{}:{}", self.hosts[index], self.ports[index])
    }

    #[cfg(test)]
    pub fn host(&self, index: usize) -> &str {
        &self.hosts[index]
    }

    /// Whether two fixture origins are genuinely **cross-site** (different
    /// hosts), rather than merely cross-origin. Only cross-site frames become
    /// OOPIFs, so probes that claim to measure OOPIF behaviour must check this
    /// before believing their own result.
    pub fn is_cross_site(&self, a: usize, b: usize) -> bool {
        self.hosts[a] != self.hosts[b]
    }

    pub fn url(&self, index: usize, path: &str) -> String {
        format!("{}{path}", self.origin(index))
    }

    /// Register the HTML served at `/page/<id>`.
    pub fn set_page(&self, id: &str, html: impl Into<String>) {
        self.shared
            .pages
            .lock()
            .unwrap()
            .insert(id.to_string(), html.into());
    }

    pub fn log(&self) -> Vec<Record> {
        self.shared.log.lock().unwrap().clone()
    }

    pub fn log_json(&self) -> Vec<Value> {
        self.log().iter().map(Record::to_json).collect()
    }

    /// Requests whose path contains `needle`, across every port.
    pub fn hits(&self, needle: &str) -> Vec<Record> {
        self.log()
            .into_iter()
            .filter(|r| r.path.contains(needle))
            .collect()
    }

    pub fn saw(&self, needle: &str) -> bool {
        !self.hits(needle).is_empty()
    }

    /// Wait up to `limit` for a request matching `needle`. Returns whether one
    /// arrived — used to give a "the server never saw it" conclusion a real
    /// observation window rather than an instantaneous check.
    pub fn wait_for(&self, needle: &str, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if self.saw(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock each `accept` so the threads observe the stop flag.
        for (host, port) in self.hosts.iter().zip(&self.ports) {
            let _ = TcpStream::connect((host.as_str(), *port));
        }
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn handle(mut stream: TcpStream, port: u16, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    // Read until the end of the header block.
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if raw.len() > 64 * 1024 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&raw).to_string();
    let mut lines = text.lines();
    let Some(request_line) = lines.next() else {
        return Ok(());
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let headers: Vec<(String, String)> = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();

    // Drain a request body if one was announced, so the client sees a clean
    // response rather than a reset (form POST navigations depend on this).
    if let Some(len) = header(&headers, "content-length").and_then(|v| v.parse::<usize>().ok()) {
        let already = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.len())
            .unwrap_or(0);
        let mut remaining = len.saturating_sub(already);
        while remaining > 0 {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            remaining = remaining.saturating_sub(n);
        }
    }

    shared.log.lock().unwrap().push(Record {
        port,
        method: method.clone(),
        path: path.clone(),
        ts_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        sec_fetch_dest: header(&headers, "sec-fetch-dest").map(str::to_string),
        sec_fetch_mode: header(&headers, "sec-fetch-mode").map(str::to_string),
        sec_purpose: header(&headers, "sec-purpose").map(str::to_string),
        sw_navigation_preload: header(&headers, "service-worker-navigation-preload")
            .map(str::to_string),
    });

    let response = route(&path, shared);
    stream.write_all(&response)?;
    stream.flush()
}

/// Build a full HTTP response for `path`.
///
/// Every response is `Access-Control-Allow-Origin: *`. Q12 reads a
/// cross-origin `fetch()` promise as its discriminator, and without permissive
/// CORS a *released* request would be logged by the server yet reject in the
/// page — inverting the outcome table. `Cache-Control: no-store` matters just
/// as much: an HTTP-cached response would hide a request from the log.
pub fn route(path: &str, shared: &Shared) -> Vec<u8> {
    let (route_path, query) = path.split_once('?').unwrap_or((path, ""));

    if route_path == "/redirect" {
        if let Some(target) = query_param(query, "to") {
            return raw_response(302, "text/html", b"", Some(&target));
        }
        return raw_response(400, "text/plain", b"missing to=", None);
    }
    if route_path == "/slow" {
        let ms = query_param(query, "ms")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(500);
        std::thread::sleep(Duration::from_millis(ms.min(10_000)));
        return raw_response(200, "text/html", b"<title>slow</title>slow", None);
    }
    if route_path == "/sw.js" {
        let body = br#"
self.addEventListener('install', e => e.waitUntil(self.skipWaiting()));
self.addEventListener('activate', e => e.waitUntil((async () => {
  if (self.registration.navigationPreload) {
    await self.registration.navigationPreload.enable();
  }
  await self.clients.claim();
})()));
self.addEventListener('fetch', e => {
  e.respondWith((async () => {
    const preload = await e.preloadResponse;
    if (preload) return preload;
    return fetch(e.request);
  })());
});
"#;
        return raw_response(200, "application/javascript", body, None);
    }
    if let Some(id) = route_path.strip_prefix("/page/") {
        let pages = shared.pages.lock().unwrap();
        return match pages.get(id) {
            Some(html) => raw_response(200, "text/html", html.as_bytes(), None),
            None => raw_response(404, "text/plain", b"no such page", None),
        };
    }
    raw_response(200, "text/html", b"<title>ok</title>ok", None)
}

pub fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn raw_response(status: u16, content_type: &str, body: &[u8], location: Option<&str>) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Cache-Control: no-store\r\n\
         Service-Worker-Allowed: /\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(location) = location {
        head.push_str(&format!("Location: {location}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_prefer_distinct_loopback_ips() {
        // The whole reason the fixture reaches for different IPs rather than
        // just different ports: ports are not part of a site, so a same-site
        // iframe never becomes an OOPIF. On a host where the alias cannot be
        // bound (macOS without `ifconfig lo0 alias 127.0.0.2`) this degrades
        // to 127.0.0.1 and the cross-site probes are same-site.
        let fixture = Fixture::start(2).expect("bind");
        assert_eq!(fixture.host(0), "127.0.0.1");
        assert!(fixture.origin(0).starts_with("http://127.0.0.1:"));
        if fixture.host(1) != "127.0.0.1" {
            assert_eq!(fixture.host(1), "127.0.0.2");
            assert!(fixture.origin(1).starts_with("http://127.0.0.2:"));
        }
    }

    #[test]
    fn distinct_indices_always_get_distinct_origins() {
        let fixture = Fixture::start(3).expect("bind");
        let origins: Vec<String> = (0..3).map(|i| fixture.origin(i)).collect();
        assert_ne!(origins[0], origins[1]);
        assert_ne!(origins[1], origins[2]);
        assert_ne!(origins[0], origins[2]);
    }

    #[test]
    fn query_params_are_decoded() {
        assert_eq!(
            query_param("to=http%3A%2F%2F127.0.0.1%3A8080%2Fx", "to").as_deref(),
            Some("http://127.0.0.1:8080/x")
        );
        assert_eq!(query_param("a=1&b=2", "b").as_deref(), Some("2"));
        assert_eq!(query_param("a=1", "missing"), None);
    }

    #[test]
    fn every_response_is_cors_open_and_uncacheable() {
        // Q12 reads a cross-origin fetch() promise, and an HTTP-cached
        // response would hide a request from the log — both are load-bearing.
        let shared = Shared::default();
        let head = String::from_utf8(route("/never", &shared)).unwrap();
        assert!(head.contains("Access-Control-Allow-Origin: *"));
        assert!(head.contains("Cache-Control: no-store"));
        assert!(head.contains("Content-Length:"));
    }

    #[test]
    fn redirect_route_sets_location() {
        let shared = Shared::default();
        let head = String::from_utf8(route("/redirect?to=http%3A%2F%2Fx%2Fy", &shared)).unwrap();
        assert!(head.starts_with("HTTP/1.1 302 "));
        assert!(head.contains("Location: http://x/y"));
    }

    #[test]
    fn registered_pages_are_served_and_unknown_ones_404() {
        let shared = Shared::default();
        shared
            .pages
            .lock()
            .unwrap()
            .insert("a".into(), "<h1>hello</h1>".into());
        let ok = String::from_utf8(route("/page/a", &shared)).unwrap();
        assert!(ok.starts_with("HTTP/1.1 200 "));
        assert!(ok.ends_with("<h1>hello</h1>"));
        let missing = String::from_utf8(route("/page/b", &shared)).unwrap();
        assert!(missing.starts_with("HTTP/1.1 404 "));
    }

    #[test]
    fn server_logs_a_real_request_with_its_headers() {
        let fixture = Fixture::start(1).expect("bind");
        fixture.set_page("x", "<title>x</title>");
        let mut stream = TcpStream::connect((fixture.host(0), fixture.port(0))).unwrap();
        stream
            .write_all(
                b"GET /page/x HTTP/1.1\r\nHost: localhost\r\nSec-Fetch-Dest: document\r\n\r\n",
            )
            .unwrap();
        let mut body = String::new();
        let _ = stream.read_to_string(&mut body);
        assert!(fixture.wait_for("/page/x", Duration::from_secs(2)));
        let hit = &fixture.hits("/page/x")[0];
        assert_eq!(hit.method, "GET");
        assert_eq!(hit.sec_fetch_dest.as_deref(), Some("document"));
        assert_eq!(hit.port, fixture.port(0));
    }
}
