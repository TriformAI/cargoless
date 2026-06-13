//! `l4proxy` — a per-instance, std-only TCP byte-splice proxy.
//!
//! One proxy is bound per app-serve instance at its fixed `app_bind` (the
//! address a k8s Service targets). Behind it, the daemon runs the actual app
//! child on a daemon-allocated loopback port and points the proxy's
//! **upstream slot** at it. A blue/green swap is then a single atomic store:
//! new connections splice to the new child; connections already in flight
//! keep splicing to the child they were accepted against, until they close.
//!
//! ## Why L4 (raw TCP), not L7 (HTTP)
//!
//! tf-multiverse serves WebSocket screencast and SSE chat — long-lived,
//! bidirectional, non-request/response traffic. A byte-splice proxy is
//! protocol-agnostic: it copies bytes each way until EOF, so upgrades,
//! chunked bodies, and infinite streams all "just work". The swap invariant
//! is correspondingly simple: **a connection is pinned to the upstream it was
//! accepted against**; flipping the slot only redirects *future* accepts.
//! An open SSE stream therefore rides its old child to completion — exactly
//! the drain semantics [`crate::appstate`] encodes with `StartDrain` /
//! `DrainComplete`.
//!
//! ## Shape (mirrors `transport/http.rs`'s accept loop)
//!
//! - nonblocking [`TcpListener`]; the accept thread polls a `stop`
//!   [`AtomicBool`] every 20 ms on `WouldBlock` (identical idiom to the read
//!   plane, so teardown latency and CPU profile match).
//! - per-connection: two splice threads (client→upstream, upstream→client),
//!   each doing a half-close ([`Shutdown::Write`]) on its EOF so the *other*
//!   direction can still drain — the SSE/WS case, where one side goes quiet
//!   for minutes while the other streams.
//! - an [`UpstreamSlot`] = an atomic `(generation, port)` pair (port 0 ⇒ "no
//!   green child yet", serve a tiny holding response and close).
//! - a per-generation connection gauge ([`ConnGauge`]) so the driver can ask
//!   "has the demoted child's last connection closed?" and complete a drain
//!   precisely instead of on a blind timer.
//!
//! Std-only: no async runtime, no proxy crate — the house ethos (the read
//! plane hand-rolls HTTP/1.1+SSE on `std::net`; this hand-rolls TCP splice).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How long the accept thread blocks before re-checking `stop` — matches the
/// read plane's 20 ms `WouldBlock` poll so the two loops behave identically.
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// Splice buffer per direction. 32 KiB is the usual sweet spot: large enough
/// that a screencast frame or SSE burst moves in one `read`/`write_all`, small
/// enough that thousands of idle-but-open streams don't pin much RAM.
const SPLICE_BUF: usize = 32 * 1024;

/// The current upstream for an instance: a loopback port plus the generation
/// that owns it. `port == 0` means "no green child is serving yet".
///
/// Lock-free because the splice hot path reads it on every new connection and
/// the driver writes it on every promote; an `AtomicU64` packs
/// `(generation << 16) | port` so a reader sees a *consistent* pair, never a
/// torn port-from-one-gen / gen-from-another.
#[derive(Debug, Default)]
pub struct UpstreamSlot {
    packed: AtomicU64,
}

/// A consistent read of the slot: which port, owned by which generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Upstream {
    pub port: u16,
    pub generation: u64,
}

impl UpstreamSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Point the proxy at `port`, owned by `generation`. The single promote
    /// site (the driver) calls this; it is the only writer.
    pub fn set(&self, port: u16, generation: u64) {
        self.packed
            .store((generation << 16) | port as u64, Ordering::Release);
    }

    /// Clear the slot (no upstream — new connections get the holding page).
    pub fn clear(&self) {
        self.packed.store(0, Ordering::Release);
    }

    /// A torn-free snapshot of the current upstream, or `None` when unset.
    pub fn get(&self) -> Option<Upstream> {
        let v = self.packed.load(Ordering::Acquire);
        let port = (v & 0xFFFF) as u16;
        if port == 0 {
            None
        } else {
            Some(Upstream {
                port,
                generation: v >> 16,
            })
        }
    }
}

/// Per-generation open-connection counters. A connection increments its
/// generation's gauge on accept and decrements when *both* splice directions
/// finish; the driver reads [`ConnGauge::count`] to decide a drain is done.
#[derive(Debug, Default)]
pub struct ConnGauge {
    counts: Mutex<BTreeMap<u64, usize>>,
}

impl ConnGauge {
    pub fn new() -> Self {
        Self::default()
    }

    fn inc(&self, generation: u64) {
        *self
            .counts
            .lock()
            .expect("gauge")
            .entry(generation)
            .or_insert(0) += 1;
    }

    fn dec(&self, generation: u64) {
        let mut g = self.counts.lock().expect("gauge");
        if let Some(n) = g.get_mut(&generation) {
            *n -= 1;
            if *n == 0 {
                g.remove(&generation);
            }
        }
    }

    /// Open connections still splicing to `generation` (0 ⇒ drained).
    pub fn count(&self, generation: u64) -> usize {
        self.counts
            .lock()
            .expect("gauge")
            .get(&generation)
            .copied()
            .unwrap_or(0)
    }

    /// Total open proxied connections across all generations.
    pub fn total(&self) -> usize {
        self.counts.lock().expect("gauge").values().sum()
    }
}

/// A running per-instance proxy. Dropping it stops the accept loop (existing
/// spliced connections are detached and end on their own EOF, like the read
/// plane's in-flight requests).
pub struct L4Proxy {
    bound: SocketAddr,
    slot: Arc<UpstreamSlot>,
    gauge: Arc<ConnGauge>,
    stop: Arc<AtomicBool>,
}

impl L4Proxy {
    /// Bind `addr` and start accepting. Connections splice to whatever
    /// [`UpstreamSlot::get`] returns *at accept time*; while the slot is empty
    /// they receive `holding` (a complete HTTP/1.1 response — the daemon
    /// passes a small "starting up" page) and close.
    pub fn bind(addr: SocketAddr, holding: Arc<HoldingResponse>) -> std::io::Result<Self> {
        let slot = Arc::new(UpstreamSlot::new());
        let gauge = Arc::new(ConnGauge::new());
        let stop = Arc::new(AtomicBool::new(false));
        let listener = TcpListener::bind(addr)?;
        let bound = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let (slot_t, gauge_t, stop_t) = (slot.clone(), gauge.clone(), stop.clone());
        thread::spawn(move || {
            accept_loop(listener, slot_t, gauge_t, stop_t, holding);
        });

        Ok(Self {
            bound,
            slot,
            gauge,
            stop,
        })
    }

    /// The actually-bound address (resolves an ephemeral `:0` test port).
    pub fn addr(&self) -> SocketAddr {
        self.bound
    }

    pub fn slot(&self) -> &Arc<UpstreamSlot> {
        &self.slot
    }

    pub fn gauge(&self) -> &Arc<ConnGauge> {
        &self.gauge
    }
}

impl Drop for L4Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A complete, pre-rendered HTTP/1.1 response the proxy serves verbatim when
/// no upstream is set. Built once by the daemon (holding page) and shared.
#[derive(Debug)]
pub struct HoldingResponse {
    bytes: Vec<u8>,
}

impl HoldingResponse {
    /// Wrap a ready-made HTTP/1.1 response (status line + headers + body).
    pub fn raw(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Build a minimal `503` holding page with `Connection: close` and an
    /// accurate `Content-Length` (so even a picky client renders it).
    pub fn page(status: u16, reason: &str, content_type: &str, body: &str) -> Self {
        let bytes = format!(
            "HTTP/1.1 {status} {reason}\r\n\
             Content-Type: {content_type}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             Cache-Control: no-store\r\n\
             \r\n\
             {body}",
            body.len()
        )
        .into_bytes();
        Self { bytes }
    }
}

fn accept_loop(
    listener: TcpListener,
    slot: Arc<UpstreamSlot>,
    gauge: Arc<ConnGauge>,
    stop: Arc<AtomicBool>,
    holding: Arc<HoldingResponse>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((client, _peer)) => {
                let upstream = slot.get();
                let (gauge_c, holding_c) = (gauge.clone(), holding.clone());
                thread::spawn(move || serve_conn(client, upstream, gauge_c, holding_c));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            // A genuine accept error (fd exhaustion, listener torn down):
            // end the loop. Drop already flips `stop`; this covers the rest.
            Err(_) => break,
        }
    }
}

/// Handle one accepted client: with no upstream, write the holding page and
/// close; otherwise connect to the pinned upstream port and splice both ways.
fn serve_conn(
    mut client: TcpStream,
    upstream: Option<Upstream>,
    gauge: Arc<ConnGauge>,
    holding: Arc<HoldingResponse>,
) {
    let Some(up) = upstream else {
        let _ = client.set_nonblocking(false);
        let _ = client.write_all(&holding.bytes);
        let _ = client.flush();
        let _ = client.shutdown(Shutdown::Both);
        return;
    };

    // Connect to the pinned child. A child that has died between promote and
    // this connect just fails here; the client sees a closed connection and
    // retries — the daemon's health loop will have already demoted it.
    let server = match TcpStream::connect(("127.0.0.1", up.port)) {
        Ok(s) => s,
        Err(_) => {
            let _ = client.shutdown(Shutdown::Both);
            return;
        }
    };
    let _ = client.set_nonblocking(false);
    let _ = server.set_nonblocking(false);

    // This connection belongs to `up.generation` for its whole life, even if
    // the slot flips mid-stream — that is the pin. The gauge entry lives until
    // both directions finish, so the driver's drain check is exact.
    gauge.inc(up.generation);

    let (c_read, s_write) = (client, server);
    let c_write = match c_read.try_clone() {
        Ok(c) => c,
        Err(_) => {
            let _ = c_read.shutdown(Shutdown::Both);
            gauge.dec(up.generation);
            return;
        }
    };
    let s_read = match s_write.try_clone() {
        Ok(s) => s,
        Err(_) => {
            let _ = c_read.shutdown(Shutdown::Both);
            let _ = s_write.shutdown(Shutdown::Both);
            gauge.dec(up.generation);
            return;
        }
    };

    // client → upstream on this thread; upstream → client on a helper. Each
    // half-closes its write end on EOF so the peer can still drain the other
    // direction (SSE: client goes silent, server streams for minutes).
    let pump = thread::spawn(move || splice(c_read, s_write));
    splice(s_read, c_write);
    let _ = pump.join();

    gauge.dec(up.generation);
}

/// Copy `from` → `to` until EOF, then half-close `to`'s write side so the
/// reverse direction can keep draining. Errors end the direction quietly —
/// a reset mid-stream is normal (the client closed the tab).
fn splice(mut from: TcpStream, mut to: TcpStream) {
    let mut buf = [0_u8; SPLICE_BUF];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    // Half-close: signal EOF downstream, leave the other direction open.
    let _ = to.shutdown(Shutdown::Write);
    let _ = from.shutdown(Shutdown::Read);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial blocking echo/sink upstream for splice tests: reads a line,
    /// writes a fixed banner + echoes. Returns its bound port and a stop flag.
    struct ToyUpstream {
        port: u16,
        stop: Arc<AtomicBool>,
    }

    impl ToyUpstream {
        /// Spawn an upstream that, per connection, writes `banner` then echoes
        /// whatever it reads back (so both splice directions are exercised).
        fn spawn(banner: &'static str) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_t = stop.clone();
            thread::spawn(move || {
                while !stop_t.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut conn, _)) => {
                            thread::spawn(move || {
                                let _ = conn.write_all(banner.as_bytes());
                                let _ = conn.flush();
                                let mut buf = [0_u8; 1024];
                                loop {
                                    match conn.read(&mut buf) {
                                        Ok(0) => break,
                                        Ok(n) => {
                                            if conn.write_all(&buf[..n]).is_err() {
                                                break;
                                            }
                                        }
                                        Err(_) => break,
                                    }
                                }
                                let _ = conn.shutdown(Shutdown::Both);
                            });
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self { port, stop }
        }
    }

    impl Drop for ToyUpstream {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    fn holding() -> Arc<HoldingResponse> {
        Arc::new(HoldingResponse::page(
            503,
            "Service Unavailable",
            "text/plain",
            "starting",
        ))
    }

    fn read_to_end(stream: &mut TcpStream) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0_u8; 1024];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        out
    }

    #[test]
    fn upstream_slot_is_torn_free_and_round_trips() {
        let slot = UpstreamSlot::new();
        assert_eq!(slot.get(), None, "unset slot ⇒ no upstream");
        slot.set(8090, 7);
        assert_eq!(
            slot.get(),
            Some(Upstream {
                port: 8090,
                generation: 7
            })
        );
        slot.set(8091, 8);
        assert_eq!(
            slot.get(),
            Some(Upstream {
                port: 8091,
                generation: 8
            })
        );
        slot.clear();
        assert_eq!(slot.get(), None);
        // A high generation never bleeds into the port field.
        slot.set(65535, u64::MAX >> 16);
        let up = slot.get().unwrap();
        assert_eq!(up.port, 65535);
        assert_eq!(up.generation, u64::MAX >> 16);
    }

    #[test]
    fn no_upstream_serves_holding_page() {
        let proxy = L4Proxy::bind("127.0.0.1:0".parse().unwrap(), holding()).unwrap();
        let mut c = TcpStream::connect(proxy.addr()).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let got = String::from_utf8_lossy(&read_to_end(&mut c)).to_string();
        assert!(got.starts_with("HTTP/1.1 503"), "got: {got:?}");
        assert!(got.contains("starting"));
        assert_eq!(proxy.gauge().total(), 0, "holding-page conns aren't gauged");
    }

    #[test]
    fn splices_bytes_both_directions() {
        let up = ToyUpstream::spawn("BANNER:");
        let proxy = L4Proxy::bind("127.0.0.1:0".parse().unwrap(), holding()).unwrap();
        proxy.slot().set(up.port, 1);

        let mut c = TcpStream::connect(proxy.addr()).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        // Send a payload; the toy upstream returns banner + echo.
        c.write_all(b"ping").unwrap();
        c.shutdown(Shutdown::Write).unwrap(); // EOF client->upstream
        let got = read_to_end(&mut c);
        assert_eq!(got, b"BANNER:ping", "banner + echoed payload: {got:?}");
    }

    #[test]
    fn flip_pins_existing_connection_to_its_original_upstream() {
        // Two distinguishable upstreams. A connection opened against A must
        // keep talking to A even after the slot flips to B mid-connection.
        let a = ToyUpstream::spawn("FROM-A:");
        let b = ToyUpstream::spawn("FROM-B:");
        let proxy = L4Proxy::bind("127.0.0.1:0".parse().unwrap(), holding()).unwrap();
        proxy.slot().set(a.port, 1);

        // Open against A and read A's banner (proves the conn reached A)…
        let mut c = TcpStream::connect(proxy.addr()).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut banner = [0_u8; 7];
        c.read_exact(&mut banner).unwrap();
        assert_eq!(&banner, b"FROM-A:");

        // …now FLIP the slot to B. The open connection is pinned to A.
        proxy.slot().set(b.port, 2);

        // The pinned connection still echoes via A.
        c.write_all(b"X").unwrap();
        c.shutdown(Shutdown::Write).unwrap();
        let rest = read_to_end(&mut c);
        assert_eq!(rest, b"X", "pinned conn still served by A after flip");

        // A brand-new connection gets B.
        let mut c2 = TcpStream::connect(proxy.addr()).unwrap();
        c2.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut banner2 = [0_u8; 7];
        c2.read_exact(&mut banner2).unwrap();
        assert_eq!(&banner2, b"FROM-B:", "new conn routed to the new upstream");
    }

    #[test]
    fn gauge_tracks_open_connections_per_generation() {
        let up = ToyUpstream::spawn("HI:");
        let proxy = L4Proxy::bind("127.0.0.1:0".parse().unwrap(), holding()).unwrap();
        proxy.slot().set(up.port, 42);

        let mut c = TcpStream::connect(proxy.addr()).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut banner = [0_u8; 3];
        c.read_exact(&mut banner).unwrap(); // connection is established & spliced
        assert_eq!(&banner, b"HI:");

        // Poll the gauge up — the inc happens just after accept. Budgets are
        // generous (~3s, early-exit on success) so a loaded CI runner sharing
        // cores across 5 jobs can't flake the timing.
        let mut saw_one = false;
        for _ in 0..600 {
            if proxy.gauge().count(42) == 1 {
                saw_one = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            saw_one,
            "an open spliced connection is gauged to its generation"
        );

        // Close the client; both directions end and the gauge returns to 0.
        c.shutdown(Shutdown::Both).unwrap();
        drop(c);
        let mut drained = false;
        for _ in 0..600 {
            if proxy.gauge().count(42) == 0 {
                drained = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(drained, "gauge returns to zero once the connection closes");
    }

    #[test]
    fn dropping_the_proxy_stops_accepting() {
        let proxy = L4Proxy::bind("127.0.0.1:0".parse().unwrap(), holding()).unwrap();
        let addr = proxy.addr();
        drop(proxy);
        // Give the accept loop a couple of poll cycles to observe `stop`.
        thread::sleep(Duration::from_millis(60));
        // The OS may still accept into the listen backlog briefly after the
        // thread exits, but a connect+read must not get a holding page (the
        // serving thread is gone). Tolerate connect success; require no data.
        if let Ok(mut c) = TcpStream::connect(addr) {
            c.set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let got = read_to_end(&mut c);
            assert!(got.is_empty(), "stopped proxy serves nothing: {got:?}");
        }
    }

    #[test]
    fn banner_only_stream_survives_until_close() {
        // The SSE shape: upstream sends a banner then stays open (no echo
        // needed). The half-close on the client->upstream EOF must NOT tear
        // down the upstream->client direction.
        let up = ToyUpstream::spawn("EVENT-STREAM-OPEN");
        let proxy = L4Proxy::bind("127.0.0.1:0".parse().unwrap(), holding()).unwrap();
        proxy.slot().set(up.port, 1);

        let mut c = TcpStream::connect(proxy.addr()).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        // Immediately half-close our write side (like a GET with no body that
        // then listens). The banner must still arrive.
        c.shutdown(Shutdown::Write).unwrap();
        let got = read_to_end(&mut c);
        assert_eq!(
            got, b"EVENT-STREAM-OPEN",
            "server→client survives a client-side half-close"
        );
    }
}
