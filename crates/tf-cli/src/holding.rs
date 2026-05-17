//! The AC#1 holding page — a std-only HTTP server brought up *immediately*
//! on `serve`, before any compile, so a zero-config user sees a live page in
//! well under 30s (decision **D-A1**: "daemon up + holding page", not a
//! finished app — a cold Leptos build is minutes).
//!
//! Scope boundary: this is a **bring-up placeholder**, not the real dev
//! server. The never-serve-red HTTP+WebSocket server (AC#4, decision **D3**
//! reload signalling) is owned by the `devserver` module in `tf-core` and
//! will supersede this once it lands. Until then this proves AC#1 end to end
//! and gives the user honest, auto-refreshing status. It is std-only
//! (`TcpListener`, one thread) — no async runtime, no HTTP crate.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// What the page tells the browser right now. The `serve` loop advances this
/// as the daemon learns the tree's state; the page auto-refreshes so the
/// human never has to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Daemon coming up (watcher/analyzer starting). The AC#1 first paint.
    Starting,
    /// Tree is green; the cold/initial build is running (minutes, honestly).
    Building,
    /// Tree is red — last-green is held (AC#4). Carries a one-liner.
    Red(String),
}

impl Phase {
    fn headline(&self) -> &str {
        match self {
            Phase::Starting => "Starting cargoless…",
            Phase::Building => "Compiling your app…",
            Phase::Red(_) => "Build is red — holding last green",
        }
    }

    fn detail(&self) -> String {
        match self {
            Phase::Starting => "The daemon is up and watching. The first build begins as \
                 soon as the tree is green."
                .to_string(),
            Phase::Building => "A cold Rust + WASM build takes a few minutes the first time. \
                 This page reloads itself when the app is ready."
                .to_string(),
            Phase::Red(why) => format!(
                "cargoless will not serve a broken build. Last-known-good is \
                 held until this is fixed:\n{why}"
            ),
        }
    }
}

/// Render the holding page. Pure (state in, HTML out) so the copy and the
/// auto-refresh contract are unit-tested without binding a socket.
pub fn render_page(phase: &Phase) -> String {
    let headline = phase.headline();
    let detail = phase.detail();
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta http-equiv=\"refresh\" content=\"2\">\
<title>cargoless — {headline}</title>\
<style>html{{color-scheme:dark light;font:16px/1.5 ui-sans-serif,system-ui,sans-serif}}\
body{{margin:0;min-height:100vh;display:grid;place-items:center}}\
main{{max-width:34rem;padding:2rem;text-align:center}}\
h1{{font-size:1.4rem;margin:0 0 .5rem}}p{{opacity:.75;white-space:pre-wrap}}\
.dot{{display:inline-block;width:.6rem;height:.6rem;border-radius:50%;\
background:currentColor;margin-right:.5rem;animation:b 1s infinite}}\
@keyframes b{{50%{{opacity:.2}}}}</style></head>\
<body><main><h1><span class=\"dot\"></span>{headline}</h1>\
<p>{detail}</p></main></body></html>"
    )
}

/// HTTP status line for a phase. Building/Starting are `200` (the page is the
/// intended content); Red is `503` so scripts/health-checks see "not ready"
/// while humans still get the explanatory page.
fn status_line(phase: &Phase) -> &'static str {
    match phase {
        Phase::Red(_) => "HTTP/1.1 503 Service Unavailable",
        _ => "HTTP/1.1 200 OK",
    }
}

/// A running holding-page server. Drop or [`HoldingServer::shutdown`] stops
/// the accept loop and joins the thread.
pub struct HoldingServer {
    phase: Arc<Mutex<Phase>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// The address actually bound (useful when port 0 / fallback is used).
    pub bound: std::net::SocketAddr,
}

impl HoldingServer {
    /// Bind `host:port` and start serving the holding page. The bind is the
    /// one operation that can legitimately fail fast (port in use) — it is
    /// surfaced as an `io::Error` so `serve` can give an actionable message.
    pub fn start(host: &str, port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind((host, port))?;
        listener.set_nonblocking(true)?;
        let bound = listener.local_addr()?;

        let phase = Arc::new(Mutex::new(Phase::Starting));
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s) = (Arc::clone(&phase), Arc::clone(&stop));

        let thread = thread::Builder::new()
            .name("tf-holding".into())
            .spawn(move || accept_loop(listener, p, s))
            .expect("spawn tf-holding thread");

        Ok(Self {
            phase,
            stop,
            thread: Some(thread),
            bound,
        })
    }

    /// Advance what the page reports. Cheap; safe to call on every state edge.
    pub fn set_phase(&self, phase: Phase) {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner()) = phase;
    }

    pub fn shutdown(mut self) {
        self.do_shutdown();
    }

    fn do_shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for HoldingServer {
    fn drop(&mut self) {
        self.do_shutdown();
    }
}

fn accept_loop(listener: TcpListener, phase: Arc<Mutex<Phase>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let snapshot = phase.lock().unwrap_or_else(|e| e.into_inner()).clone();
                // One connection misbehaving must never take the loop down.
                let _ = serve_one(stream, &snapshot);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Nonblocking listener with no pending connection: nap so the
                // stop flag is observed promptly without a busy-spin.
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn serve_one(mut stream: TcpStream, phase: &Phase) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    // Drain (a bounded prefix of) the request; we serve the same page for any
    // path, but a well-behaved server must consume the request line.
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf);

    let body = render_page(phase);
    let response = format!(
        "{}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        status_line(phase),
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_reflects_phase_and_auto_refreshes() {
        let s = render_page(&Phase::Starting);
        assert!(s.contains("Starting cargoless"));
        // The auto-refresh is the AC#1 contract: the user never reloads.
        assert!(s.contains("http-equiv=\"refresh\""));

        let b = render_page(&Phase::Building);
        assert!(b.contains("Compiling"));

        let r = render_page(&Phase::Red("E0432: unresolved import".into()));
        assert!(r.contains("E0432"));
        assert!(r.contains("last green"));
    }

    #[test]
    fn red_is_unavailable_others_ok() {
        assert!(status_line(&Phase::Starting).contains("200"));
        assert!(status_line(&Phase::Building).contains("200"));
        assert!(status_line(&Phase::Red("x".into())).contains("503"));
    }

    #[test]
    fn binds_serves_and_shuts_down() {
        // Port 0 = OS-assigned; proves the bind→serve→shutdown lifecycle on
        // the Linux CI box without a fixed-port collision.
        let srv = HoldingServer::start("127.0.0.1", 0).expect("bind");
        let addr = srv.bound;
        srv.set_phase(Phase::Building);

        let mut conn = TcpStream::connect(addr).expect("connect");
        conn.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("write req");
        let mut resp = String::new();
        conn.read_to_string(&mut resp).expect("read resp");
        assert!(resp.contains("200 OK"));
        assert!(resp.contains("Compiling"));

        srv.shutdown();
    }
}
