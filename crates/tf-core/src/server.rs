//! The dev server (Epic 4 / CWDL-40..45).
//!
//! Serves the latest **green** WASM artifact bundle to the browser over
//! HTTP/1.1, and pushes a full-page reload over a WebSocket the instant a new
//! green build is available. Its one inviolable promise is **AC#4 — never
//! serve red**: once a green artifact has been served, a red tree
//! ([`StateEvent::BecameRed`]) or a failed build
//! ([`BuildOutcome::Failed`]) never replaces those bytes. The browser only
//! ever advances forward, green → green.
//!
//! ## Why std-only (no tokio/axum)
//!
//! Like [`crate::analyzer`], this module is **std process + threads only, no
//! external deps**. A v0 dev server is single-user, localhost, full-reload
//! (decision **D5**) — a thread-per-connection HTTP/1.1 server with a minimal
//! RFC 6455 WebSocket is entirely adequate and keeps the cold-build time that
//! AC#1/#2 are measured against from paying for a tokio/hyper/tower tree it
//! does not need. The WebSocket handshake's SHA-1 + base64 are implemented
//! in-crate (pure, unit-tested) for the same reason.
//!
//! ## Trunk compatibility (decision D3)
//!
//! The reload channel is a WebSocket at `/_trunk/ws`; a small autoreload shim
//! is injected before `</body>` of every served HTML document — exactly as
//! `trunk serve` does. An existing Trunk project therefore needs **no source
//! change**: its `index.html` is served verbatim with the shim appended, and
//! a JSON `{"reload":true}` frame triggers `location.reload()` (decision
//! **D5**: full reload, never hot-swap).
//!
//! ## Contract seam
//!
//! The server consumes only `tf_proto` types — [`StateEvent`] (verdict
//! stream) and [`BuildResult`] (what the build/CAS layer produced). How
//! artifact *bytes* are obtained from an [`ArtifactMeta`] is the CAS owner's
//! concern, abstracted behind [`ArtifactProvider`]; the default
//! [`CasArtifactProvider`] reads them from any [`tf_cas::ContentStore`]. The
//! multi-file artifact framing itself is the contract type [`tf_proto::Bundle`]
//! (build/CAS `pack`s, the server `unpack`s) — owned by the contract crate so
//! the two disjoint owners cannot silently diverge. The server never reaches
//! into the watcher/analyzer/model/build modules.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use tf_cas::ContentStore;
use tf_proto::{ArtifactMeta, BuildResult, Bundle, StateEvent};

// ---------------------------------------------------------------------------
// Artifact provider (CAS seam)
// ---------------------------------------------------------------------------

/// How the server turns an [`ArtifactMeta`] into servable [`Bundle`] bytes.
///
/// This is the only place the server touches "where artifacts live". The
/// production path is [`CasArtifactProvider`] over a
/// [`tf_cas::ContentStore`]; tests inject an in-memory provider so AC#4 is
/// proven by bytes, not by a real build.
pub trait ArtifactProvider: Send + Sync + 'static {
    /// Fetch the bundle for `meta`. An `Err` is treated as "not servable" —
    /// the server keeps the last green artifact (never serves red).
    fn fetch(&self, meta: &ArtifactMeta) -> io::Result<Bundle>;
}

/// Default provider: read the CAS blob stored under
/// `meta.input_hash` and [`Bundle::unpack`] it.
pub struct CasArtifactProvider<S: ContentStore + Send + Sync + 'static> {
    store: Arc<S>,
}

impl<S: ContentStore + Send + Sync + 'static> CasArtifactProvider<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self { store }
    }
}

impl<S: ContentStore + Send + Sync + 'static> ArtifactProvider for CasArtifactProvider<S> {
    fn fetch(&self, meta: &ArtifactMeta) -> io::Result<Bundle> {
        match self.store.get(&meta.input_hash)? {
            Some(bytes) => Bundle::unpack(&bytes),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("artifact {} not in CAS", meta.input_hash),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Served state — the AC#4 core
// ---------------------------------------------------------------------------

/// What the HTTP layer is currently allowed to serve. The state machine here
/// *is* the never-serve-red guarantee: bytes only ever move to a
/// successfully-fetched green artifact, and `tree_red` is status only — it
/// never gates or replaces the served bytes.
#[derive(Default)]
struct Served {
    /// `None` until the first green artifact is served (cold start → holding
    /// page). Once `Some`, only ever *replaced* by another green — never
    /// cleared.
    current: Option<(ArtifactMeta, Arc<Bundle>)>,
    /// Whether the tree is currently red. Surfaced for status/log only; the
    /// server keeps serving `current` regardless (AC#4).
    tree_red: bool,
}

// ---------------------------------------------------------------------------
// Server handle
// ---------------------------------------------------------------------------

/// A running dev server. Drop or [`ServerHandle::shutdown`] to stop it.
pub struct ServerHandle {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

struct Shared {
    served: Mutex<Served>,
    clients: Mutex<Vec<TcpStream>>,
    provider: Box<dyn ArtifactProvider>,
    running: AtomicBool,
}

impl ServerHandle {
    /// The address actually bound (use `127.0.0.1:0` to get an ephemeral
    /// port, then read it back here — what the integration tests do).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Feed a verdict-stream event. **AC#4**: `BecameRed` only flips the
    /// status flag — it never touches the served bytes. `BecameGreen` clears
    /// the flag (a build will follow; bytes change only on its
    /// [`BuildResult`]).
    pub fn notify_state(&self, ev: &StateEvent) {
        let mut s = self.shared.served.lock().unwrap();
        match ev {
            StateEvent::BecameRed => s.tree_red = true,
            StateEvent::BecameGreen { .. } => s.tree_red = false,
            StateEvent::FileVerdict { .. } => {}
        }
    }

    /// Feed a build result. The **only** path that can change served bytes:
    /// if the outcome is servable and carries an artifact, fetch it; on
    /// success swap atomically and push a reload. A `Failed` build, a missing
    /// artifact, or a fetch error keeps the last green artifact — the browser
    /// never sees red.
    pub fn notify_build(&self, result: &BuildResult) {
        if !result.outcome.is_servable() {
            return;
        }
        let Some(meta) = result.artifact.as_ref() else {
            return;
        };
        let bundle = match self.shared.provider.fetch(meta) {
            Ok(b) => Arc::new(b),
            Err(_) => return, // keep last green — never serve red
        };
        {
            let mut s = self.shared.served.lock().unwrap();
            s.current = Some((meta.clone(), bundle));
            s.tree_red = false;
        }
        self.shared.broadcast_reload();
    }

    /// Whether a green artifact has been served at least once.
    pub fn has_green(&self) -> bool {
        self.shared.served.lock().unwrap().current.is_some()
    }

    /// Whether the watched tree is currently red. **Status only** — this
    /// never gates what is served (AC#4: a red tree keeps the last green
    /// bytes). Exposed so the CLI/`status` can surface it and the AC#4 test
    /// can assert "red flag set, bytes unchanged" simultaneously.
    pub fn tree_is_red(&self) -> bool {
        self.shared.served.lock().unwrap().tree_red
    }

    /// Stop accepting connections and join the accept thread.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if self.shared.running.swap(false, Ordering::SeqCst) {
            // Unblock the accept loop with a throwaway self-connection.
            let _ = TcpStream::connect(self.addr);
            if let Some(h) = self.accept.take() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Shared {
    /// Push a trunk-compatible reload frame to every connected WS client,
    /// dropping any that error.
    fn broadcast_reload(&self) {
        let frame = ws_text_frame(br#"{"reload":true}"#);
        let mut clients = self.clients.lock().unwrap();
        clients.retain_mut(|c| c.write_all(&frame).and_then(|_| c.flush()).is_ok());
    }
}

// ---------------------------------------------------------------------------
// DevServer — construction & accept loop
// ---------------------------------------------------------------------------

/// Builder/entry point for the dev server.
pub struct DevServer {
    provider: Box<dyn ArtifactProvider>,
}

impl DevServer {
    /// New server backed by the default CAS-reading provider.
    pub fn new<S: ContentStore + Send + Sync + 'static>(store: Arc<S>) -> Self {
        Self {
            provider: Box::new(CasArtifactProvider::new(store)),
        }
    }

    /// New server with a custom [`ArtifactProvider`] (used by the AC#4 test).
    pub fn with_provider(provider: Box<dyn ArtifactProvider>) -> Self {
        Self { provider }
    }

    /// Bind `addr` and start serving on a background thread. Returns once the
    /// socket is bound (so `local_addr()` is immediately valid).
    pub fn spawn(self, addr: SocketAddr) -> io::Result<ServerHandle> {
        let listener = TcpListener::bind(addr)?;
        let bound = listener.local_addr()?;
        let shared = Arc::new(Shared {
            served: Mutex::new(Served::default()),
            clients: Mutex::new(Vec::new()),
            provider: self.provider,
            running: AtomicBool::new(true),
        });
        let accept = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("tf-devserver-accept".into())
                .spawn(move || accept_loop(listener, shared))?
        };
        Ok(ServerHandle {
            addr: bound,
            shared,
            accept: Some(accept),
        })
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    for stream in listener.incoming() {
        if !shared.running.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let shared = Arc::clone(&shared);
        // One thread per connection; trivial for a single-user dev server.
        let _ = thread::Builder::new()
            .name("tf-devserver-conn".into())
            .spawn(move || {
                let _ = handle_connection(stream, shared);
            });
    }
}

// ---------------------------------------------------------------------------
// HTTP / WebSocket connection handling
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
}

fn parse_request(reader: &mut BufReader<&TcpStream>) -> io::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None); // client closed
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || path.is_empty() {
        return Ok(None);
    }
    let mut headers = BTreeMap::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Ok(Some(Request {
        method,
        path,
        headers,
    }))
}

fn handle_connection(stream: TcpStream, shared: Arc<Shared>) -> io::Result<()> {
    // Parse in an inner scope so the `BufReader<&TcpStream>` borrow is
    // released before `stream` is moved into the WebSocket handler. The
    // client sends no body (GET) and waits for our 101 before sending WS
    // frames, so no post-header bytes are buffered away here.
    let req = {
        let mut reader = BufReader::new(&stream);
        match parse_request(&mut reader)? {
            Some(r) => r,
            None => return Ok(()),
        }
    };

    let is_ws = req
        .headers
        .get("upgrade")
        .map(|u| u.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    if is_ws && req.path == WS_PATH {
        return serve_websocket(stream, &req, shared);
    }

    // Plain HTTP/1.1 GET. Anything that is not a known asset still gets a
    // 200 holding page (cold start, decision D-A1) — never an error page,
    // never a red build.
    let mut wstream = stream.try_clone()?;
    let (status, ctype, body) = route(&req, &shared);
    let mut resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(&body);
    wstream.write_all(&resp)?;
    wstream.flush()
}

/// Resolve a request to `(status line, content-type, body)`.
fn route(req: &Request, shared: &Arc<Shared>) -> (&'static str, &'static str, Vec<u8>) {
    if req.method != "GET" && req.method != "HEAD" {
        return ("405 Method Not Allowed", "text/plain", b"405".to_vec());
    }
    let guard = shared.served.lock().unwrap();
    let Some((_, bundle)) = guard.current.as_ref() else {
        // No green artifact yet → holding page (200, with reload shim so the
        // page swaps itself in the moment the first green build lands).
        return ("200 OK", "text/html; charset=utf-8", holding_page());
    };
    let path = req.path.split('?').next().unwrap_or("/");
    if path == "/" || path == "/index.html" {
        return match bundle.document() {
            Some(html) => (
                "200 OK",
                "text/html; charset=utf-8",
                inject_reload_shim(html),
            ),
            None => ("200 OK", "text/html; charset=utf-8", holding_page()),
        };
    }
    match bundle.get(path) {
        Some(bytes) => {
            let ct = content_type(path);
            // Inject the shim into served HTML fragments too, so navigations
            // within the app keep the reload channel alive.
            let body = if ct.starts_with("text/html") {
                inject_reload_shim(bytes)
            } else {
                bytes.to_vec()
            };
            ("200 OK", ct, body)
        }
        None => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"404".to_vec(),
        ),
    }
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// Reload shim (trunk-compatible, decision D3 / D5)
// ---------------------------------------------------------------------------

const WS_PATH: &str = "/_trunk/ws";

/// The autoreload snippet injected before `</body>`. Reconnecting WebSocket
/// to [`WS_PATH`]; any frame carrying `reload` triggers a full
/// `location.reload()` (decision D5 — never a hot-swap). Mirrors what
/// `trunk serve` injects so an existing Trunk project needs no JS change
/// (decision D3).
fn reload_shim() -> &'static str {
    r#"<script>(function(){function c(){var w=new WebSocket((location.protocol==='https:'?'wss://':'ws://')+location.host+'/_trunk/ws');w.onmessage=function(e){try{var m=JSON.parse(e.data);if(m&&m.reload){location.reload()}}catch(_){location.reload()}};w.onclose=function(){setTimeout(c,1000)}}c()})();</script>"#
}

/// Insert the shim just before `</body>` (or append if absent). Idempotent
/// enough for v0: we only ever serve freshly-fetched bundle bytes.
fn inject_reload_shim(html: &[u8]) -> Vec<u8> {
    let s = String::from_utf8_lossy(html).into_owned();
    let shim = reload_shim();
    let injected = match s.rfind("</body>") {
        Some(i) => format!("{}{}{}", &s[..i], shim, &s[i..]),
        None => format!("{s}{shim}"),
    };
    injected.into_bytes()
}

/// Cold-start placeholder served before the first green build (decision
/// D-A1 / AC#1): a 200 page, not an error, that reloads itself into the app
/// as soon as the first green artifact is available.
///
/// Two independent swap mechanisms, by design:
/// * the [`reload_shim`] WebSocket — instant swap the moment `notify_build`
///   broadcasts on first green (the low-latency path);
/// * a `<meta http-equiv="refresh">` — a level-triggered safety net **on the
///   holding page only** (never on the served app, so the running app never
///   inherits a refresh loop). This covers the AC#5 dedupe path, where the
///   first build can be a sub-second `Deduplicated` hit that lands *before*
///   the holding page's WS has connected and would otherwise miss the
///   edge-triggered reload broadcast. With it, the holding page re-asks
///   `GET /` every 2s and `route()` serves the real app as soon as one
///   exists — bounded worst-case swap latency instead of a silent hang.
fn holding_page() -> Vec<u8> {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
<meta http-equiv=\"refresh\" content=\"2\"><title>building…</title>\
<style>body{{font:14px ui-monospace,monospace;display:grid;place-items:center;height:100vh;margin:0;background:#0b0b0b;color:#cfcfcf}}</style></head>\
<body><div>building — the page will load itself when the first green build is ready.</div>{}</body></html>",
        reload_shim()
    );
    body.into_bytes()
}

// ---------------------------------------------------------------------------
// Minimal RFC 6455 WebSocket (server push only)
// ---------------------------------------------------------------------------

// We deliberately do not reassemble client→server frames: the reload channel
// is entirely server-driven (decision D5, full reload), so a client frame is
// only ever interesting as "the peer is closing/gone". Partial reads are
// therefore irrelevant here — `unused_io_amount` does not apply.
#[allow(clippy::unused_io_amount)]
fn serve_websocket(stream: TcpStream, req: &Request, shared: Arc<Shared>) -> io::Result<()> {
    let Some(key) = req.headers.get("sec-websocket-key") else {
        return Ok(());
    };
    let accept = ws_accept_key(key);
    let mut handshake = stream.try_clone()?;
    handshake.write_all(
        format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .as_bytes(),
    )?;
    handshake.flush()?;

    // Register the writer end for broadcast.
    let writer = stream.try_clone()?;
    shared.clients.lock().unwrap().push(writer);

    // Drain client frames so the socket stays healthy; reply to ping, exit on
    // close/EOF. We never need client→server data (full reload is server
    // -driven), so the body is discarded.
    let mut reader = stream;
    let mut buf = [0u8; 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {
                // Opcode in the first byte's low nibble; 0x8 == close.
                if (buf[0] & 0x0f) == 0x08 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}

/// `base64( SHA1( key + RFC6455-GUID ) )` — the `Sec-WebSocket-Accept` value.
fn ws_accept_key(key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut input = key.as_bytes().to_vec();
    input.extend_from_slice(GUID.as_bytes());
    base64_encode(&sha1(&input))
}

/// A single unmasked server→client text frame (payload < 64 KiB, which every
/// control message here is).
fn ws_text_frame(payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 4);
    f.push(0x81); // FIN + opcode 0x1 (text)
    let n = payload.len();
    if n < 126 {
        f.push(n as u8);
    } else {
        f.push(126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    }
    f.extend_from_slice(payload);
    f
}

// --- SHA-1 (RFC 3174) -------------------------------------------------------

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let ml = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

// --- base64 (standard alphabet, padded) ------------------------------------

fn base64_encode(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_proto::{BuildIdentity, BuildOutcome, ContentHash, InputHash, Profile, TargetTriple};

    fn meta(tag: &str) -> ArtifactMeta {
        ArtifactMeta {
            input_hash: InputHash::new(tag),
            identity: BuildIdentity {
                source_tree: ContentHash::new(tag),
                cargo_lock: ContentHash::new("lock"),
                rust_toolchain: ContentHash::new("tc"),
                tf_config: ContentHash::new("cfg"),
                target: TargetTriple::new("wasm32-unknown-unknown"),
                profile: Profile::Dev,
            },
        }
    }

    /// In-memory provider keyed by input hash → bundle.
    struct MapProvider(std::collections::HashMap<String, Bundle>);
    impl ArtifactProvider for MapProvider {
        fn fetch(&self, m: &ArtifactMeta) -> io::Result<Bundle> {
            self.0
                .get(m.input_hash.as_str())
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing"))
        }
    }

    fn http_get(addr: SocketAddr, path: &str) -> (String, Vec<u8>) {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).unwrap();
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("has header terminator");
        let head = String::from_utf8_lossy(&raw[..split]).to_string();
        (head, raw[split + 4..].to_vec())
    }

    // Bundle pack/unpack + lookup tests live with the type in `tf_proto`
    // (it is the build↔server contract seam, owned by the contract crate).

    #[test]
    fn ws_accept_key_matches_rfc6455_example() {
        // The canonical example from RFC 6455 §1.3.
        assert_eq!(
            ws_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn shim_injected_before_body_close() {
        let out = inject_reload_shim(b"<html><body>hi</body></html>");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("/_trunk/ws"));
        assert!(s.find("/_trunk/ws").unwrap() < s.find("</body>").unwrap());
    }

    #[test]
    fn cold_start_serves_holding_page_not_error() {
        let srv = DevServer::with_provider(Box::new(MapProvider(Default::default())))
            .spawn("127.0.0.1:0".parse().unwrap())
            .unwrap();
        let (head, body) = http_get(srv.local_addr(), "/");
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "cold start is 200: {head}"
        );
        assert!(String::from_utf8_lossy(&body).contains("building"));
        assert!(!srv.has_green());
    }

    /// **AC#4 (CWDL-5) — never serve red.** Drive the server green, then red,
    /// then a failed build, and assert the *exact* prior green bytes are
    /// still served — verified by bytes, not visually.
    #[test]
    fn ac4_never_serves_red_after_going_green() {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "green1".to_string(),
            Bundle::from_entries([("index.html", b"<body>GREEN-ONE</body>".to_vec())]),
        );
        let srv = DevServer::with_provider(Box::new(MapProvider(map)))
            .spawn("127.0.0.1:0".parse().unwrap())
            .unwrap();
        let addr = srv.local_addr();

        // 1. Cold start: holding page, no green yet.
        let (h, _) = http_get(addr, "/");
        assert!(h.starts_with("HTTP/1.1 200"));
        assert!(!srv.has_green());

        // 2. Green build lands → that artifact is served.
        srv.notify_build(&BuildResult {
            outcome: BuildOutcome::Compiled,
            artifact: Some(meta("green1")),
        });
        assert!(srv.has_green());
        let (_, body) = http_get(addr, "/");
        assert!(
            String::from_utf8_lossy(&body).contains("GREEN-ONE"),
            "serves the green artifact"
        );

        // 3. Tree goes RED. The promise: the red flag flips, but the served
        //    bytes do NOT change — both observed in the same instant.
        srv.notify_state(&StateEvent::BecameRed);
        assert!(srv.tree_is_red(), "red is observable as status");
        let (_, body_after_red) = http_get(addr, "/");
        assert!(
            String::from_utf8_lossy(&body_after_red).contains("GREEN-ONE"),
            "AC#4: still serving the last green artifact after BecameRed"
        );

        // 4. A build FAILS (green verdict, broken build). Still last-green.
        srv.notify_build(&BuildResult {
            outcome: BuildOutcome::Failed {
                reason: "linker error".into(),
            },
            artifact: None,
        });
        let (_, body_after_fail) = http_get(addr, "/");
        assert!(
            String::from_utf8_lossy(&body_after_fail).contains("GREEN-ONE"),
            "AC#4: a failed build never replaces the last green artifact"
        );

        // 5. A servable result whose artifact is NOT fetchable → keep green.
        srv.notify_build(&BuildResult {
            outcome: BuildOutcome::Compiled,
            artifact: Some(meta("does-not-exist")),
        });
        let (_, body_after_missing) = http_get(addr, "/");
        assert!(
            String::from_utf8_lossy(&body_after_missing).contains("GREEN-ONE"),
            "AC#4: an unfetchable artifact never replaces the last green"
        );

        srv.shutdown();
    }

    #[test]
    fn advances_green_to_green() {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "g1".to_string(),
            Bundle::from_entries([("index.html", b"<body>ONE</body>".to_vec())]),
        );
        map.insert(
            "g2".to_string(),
            Bundle::from_entries([("index.html", b"<body>TWO</body>".to_vec())]),
        );
        let srv = DevServer::with_provider(Box::new(MapProvider(map)))
            .spawn("127.0.0.1:0".parse().unwrap())
            .unwrap();
        let addr = srv.local_addr();

        srv.notify_build(&BuildResult {
            outcome: BuildOutcome::Compiled,
            artifact: Some(meta("g1")),
        });
        let (_, b1) = http_get(addr, "/");
        assert!(String::from_utf8_lossy(&b1).contains("ONE"));

        srv.notify_state(&StateEvent::BecameGreen {
            identity: meta("g2").identity,
        });
        srv.notify_build(&BuildResult {
            outcome: BuildOutcome::Deduplicated,
            artifact: Some(meta("g2")),
        });
        let (_, b2) = http_get(addr, "/");
        assert!(
            String::from_utf8_lossy(&b2).contains("TWO"),
            "advances forward to the newer green artifact"
        );
        srv.shutdown();
    }

    #[test]
    fn serves_non_html_assets_with_content_type() {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "a".to_string(),
            Bundle::from_entries([
                ("index.html", b"<body>app</body>".to_vec()),
                ("app_bg.wasm", vec![0x00, 0x61, 0x73, 0x6d]),
            ]),
        );
        let srv = DevServer::with_provider(Box::new(MapProvider(map)))
            .spawn("127.0.0.1:0".parse().unwrap())
            .unwrap();
        let addr = srv.local_addr();
        srv.notify_build(&BuildResult {
            outcome: BuildOutcome::Compiled,
            artifact: Some(meta("a")),
        });
        let (head, body) = http_get(addr, "/app_bg.wasm");
        assert!(
            head.contains("application/wasm"),
            "wasm content-type: {head}"
        );
        assert_eq!(body, vec![0x00, 0x61, 0x73, 0x6d]);
        let (h404, _) = http_get(addr, "/missing.js");
        assert!(h404.starts_with("HTTP/1.1 404"));
        srv.shutdown();
    }
}
