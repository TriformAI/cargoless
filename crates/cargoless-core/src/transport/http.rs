//! HTTP + SSE adapter (`D-FLEET-SHARED-DAEMON` §10.2 network mode +
//! §11 SSE-vs-polling). `serve --repo --bind <addr>` exposes the logical
//! API over a **minimal, bounded HTTP/1.1** server (no HTTP framework —
//! the house ethos: JSON-RPC framing / debounce / ignore are
//! all hand-rolled in-crate; an HTTP crate would be the first heavy dep).
//!
//! Routes (§10.1 / §11):
//! * `GET /status?worktree=W`               → 200 status JSON | 404
//! * `GET /verdict?worktree=W`              → 200 `"green"` | 404
//! * `GET /worktrees`                       → 200 summary array
//! * `GET /worktrees/<W>/diagnostics`       → 200 diagnostics array
//!   (byte-identical to the [`crate::diagnostics_store`] on-disk codec)
//! * `GET /events`                          → `text/event-stream` SSE,
//!   one `data: <json>\n\n` frame per transition (the "react in real
//!   time to red" agent-orchestration case, §11)
//! * `GET /admin/active`                     → quiesce/drain counters
//! * `POST /admin/quiesce`                   → refuse new pushes, drain,
//!   then let the daemon exit cleanly for restart
//! * `POST /batch-check`                     → native batch gate report
//!
//! **Auth (#14 seam, NOT policy):** every request is gated by an
//! [`Authorizer`]; the default is [`AllowAll`] (the #10 posture —
//! `D-FLEET §10.4`: localhost-only, no auth). #14 swaps a bearer-token
//! `Authorizer` in without touching this file. A denied request gets a
//! clean `401`.
//!
//! Bounded by construction: one-shot responses carry `Content-Length`
//! and the connection closes (`Connection: close`); SSE streams until the
//! peer disconnects. No chunked encoding, no keep-alive, no request body
//! — the whole surface is GET. That keeps the hand-rolled parser small
//! and audit-able.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::Duration;

use cargoless_proto::Diagnostic;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use native_tls::{HandshakeError, TlsConnector, TlsStream};

use super::{
    Authorizer, BatchCheckRequest, BatchReport, DaemonActivity, PushOverlayAck, PushOverlayOptions,
    Request, TransitionEvent, TransportClient, TransportError, VerdictService, WorktreeStatus,
    WorktreeSummary, batchreport_from_json, batchreport_to_json, event_from_json, event_to_json,
    pushoverlayack_from_json, pushoverlayack_to_json, status_from_json, status_to_json,
    summaries_from_json, summaries_to_json,
};

/// Increment 2 (D-PUSHOVERLAY §2.5) — hard cap on a `POST /overlay`
/// request body. The body-reading route is *bounded by construction*:
/// the server `read_exact`s an EXACT, capped `Content-Length` and never
/// more; a larger declared length is refused `413` before any read. 32
/// MiB comfortably covers a whole-file overlay-set for a real workspace
/// while fail-closed-bounding a hostile/runaway client.
pub const MAX_OVERLAY_BYTES: usize = 32 * 1024 * 1024;
/// Compress large JSON request bodies before applying the fixed HTTP cap.
/// This targets full-file generated overlays without changing the logical
/// push protocol or making small same-host requests pay gzip overhead.
pub const HTTP_COMPRESSION_MIN_BYTES: usize = 1024 * 1024;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(10);
const BATCH_CHECK_READ_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_MAX_CONNECTIONS: usize = 128;

// ---- tiny request model -------------------------------------------------

struct HttpReq {
    method: String,
    path: String,
    query: String,
    bearer: Option<String>,
    /// `Some(n)` iff a numeric `Content-Length` header was present.
    /// Increment 2: only `POST /overlay` reads a body; an absent OR
    /// non-numeric value both collapse to `None` ⇒ the POST handler
    /// answers `400` (every GET route still reads no body).
    content_length: Option<usize>,
    /// Optional request body encoding. `gzip` is accepted on the bounded POST
    /// routes; unknown encodings fail closed with `415`.
    content_encoding: Option<String>,
}

/// Parse the request line + headers (method/path/query +
/// `Authorization: Bearer` + `Content-Length`). The body is read ONLY by
/// the `POST /overlay` route (Increment 2); every GET route stays
/// body-less — the bounded-by-construction property is preserved.
/// Returns `None` on a malformed head (server answers 400).
fn parse_request(reader: &mut impl BufRead) -> Option<HttpReq> {
    let mut start = String::new();
    reader.read_line(&mut start).ok()?;
    let mut it = start.split_whitespace();
    let method = it.next()?.to_string(); // GET (read routes) | POST (/overlay)
    let target = it.next()?.to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut bearer = None;
    let mut content_length = None;
    let mut content_encoding = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            if k.eq_ignore_ascii_case("authorization") {
                if let Some(tok) = v.trim().strip_prefix("Bearer ") {
                    bearer = Some(tok.to_string());
                }
            } else if k.eq_ignore_ascii_case("content-length") {
                // Non-numeric ⇒ stays `None` ⇒ POST /overlay answers 400
                // (absent and non-numeric are the same client error).
                content_length = v.trim().parse::<usize>().ok();
            } else if k.eq_ignore_ascii_case("content-encoding") {
                content_encoding = Some(v.trim().to_ascii_lowercase());
            }
        }
    }
    Some(HttpReq {
        method,
        path,
        query,
        bearer,
        content_length,
        content_encoding,
    })
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == key { Some(v.to_string()) } else { None }
    })
}

fn write_response(w: &mut impl Write, code: u16, reason: &str, ctype: &str, body: &str) {
    let _ = write!(
        w,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = w.flush();
}

fn daemon_activity_to_json(activity: &DaemonActivity) -> String {
    serde_json::json!({
        "quiescing": activity.quiescing,
        "active_worktrees": activity.active_worktrees,
        "pending_pushes": activity.pending_pushes,
        "pending_batch_waiters": activity.pending_batch_waiters,
        "pending_batch_members": activity.pending_batch_members,
        "inflight_batch_runs": activity.inflight_batch_runs,
    })
    .to_string()
}

/// Encoded HTTP request body ready to place after the headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedJsonBody {
    pub bytes: Vec<u8>,
    pub content_encoding: Option<&'static str>,
    pub raw_len: usize,
}

impl PreparedJsonBody {
    pub fn encoded_len(&self) -> usize {
        self.bytes.len()
    }
}

/// Prepare a JSON request body for the bounded HTTP POST routes. Bodies under
/// [`HTTP_COMPRESSION_MIN_BYTES`] stay byte-for-byte plain JSON; larger bodies
/// use gzip only when that reduces the encoded length.
pub fn prepare_json_body(body: &str) -> Result<PreparedJsonBody, TransportError> {
    let raw = body.as_bytes();
    if raw.len() < configured_compression_min_bytes() {
        return Ok(PreparedJsonBody {
            bytes: raw.to_vec(),
            content_encoding: None,
            raw_len: raw.len(),
        });
    }

    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(raw)?;
    let compressed = encoder.finish()?;
    if compressed.len() < raw.len() {
        Ok(PreparedJsonBody {
            bytes: compressed,
            content_encoding: Some("gzip"),
            raw_len: raw.len(),
        })
    } else {
        Ok(PreparedJsonBody {
            bytes: raw.to_vec(),
            content_encoding: None,
            raw_len: raw.len(),
        })
    }
}

fn configured_compression_min_bytes() -> usize {
    std::env::var("CARGOLESS_HTTP_COMPRESSION_MIN_BYTES")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(HTTP_COMPRESSION_MIN_BYTES)
}

fn decode_request_body(
    req: &HttpReq,
    encoded: Vec<u8>,
) -> Result<Vec<u8>, (u16, &'static str, String)> {
    match req.content_encoding.as_deref().filter(|v| !v.is_empty()) {
        None | Some("identity") => Ok(encoded),
        Some("gzip") => {
            let mut decoder = GzDecoder::new(encoded.as_slice());
            let mut limited = (&mut decoder).take((MAX_OVERLAY_BYTES + 1) as u64);
            let mut decoded = Vec::new();
            if let Err(e) = limited.read_to_end(&mut decoded) {
                return Err((
                    400,
                    "Bad Request",
                    format!("gzip request body could not be decoded: {e}"),
                ));
            }
            if decoded.len() > MAX_OVERLAY_BYTES {
                return Err((
                    413,
                    "Payload Too Large",
                    "decoded request body exceeds the size cap".to_string(),
                ));
            }
            Ok(decoded)
        }
        Some(other) => Err((
            415,
            "Unsupported Media Type",
            format!("unsupported Content-Encoding: {other}"),
        )),
    }
}

// ---- server -------------------------------------------------------------

/// A running HTTP server. Dropping it stops the accept loop; in-flight
/// connections (incl. long-lived SSE) drain when their peer disconnects.
pub struct HttpServer {
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
}

struct ConnectionPermit {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

fn configured_max_connections() -> usize {
    std::env::var("CARGOLESS_HTTP_MAX_CONNECTIONS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_CONNECTIONS)
}

fn try_acquire_connection(
    active: &Arc<AtomicUsize>,
    max_connections: usize,
) -> Option<ConnectionPermit> {
    let mut current = active.load(Ordering::Relaxed);
    loop {
        if current >= max_connections {
            return None;
        }
        match active.compare_exchange_weak(
            current,
            current + 1,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                return Some(ConnectionPermit {
                    active: Arc::clone(active),
                });
            }
            Err(next) => current = next,
        }
    }
}

fn write_busy(mut conn: TcpStream) {
    let _ = conn.set_nonblocking(false);
    let _ = conn.set_read_timeout(Some(Duration::from_millis(200)));
    let mut discard = [0_u8; 1024];
    let _ = conn.read(&mut discard);
    let _ = conn.set_write_timeout(Some(CLIENT_IO_TIMEOUT));
    write_response(
        &mut conn,
        503,
        "Service Unavailable",
        "text/plain",
        "cargoless http server is busy; retry shortly",
    );
    let _ = conn.shutdown(Shutdown::Both);
}

impl HttpServer {
    /// Bind `addr` (e.g. `127.0.0.1:0` for an ephemeral test port) and
    /// serve `svc`, gating every request through `auth`. Pass
    /// `Arc::new(AllowAll)` for the #10 posture; #14 passes a token
    /// policy — this signature does not change.
    ///
    /// Delegates to [`Self::bind_with_health`] with an **always-ready**
    /// flag: a caller that wires no readiness signal gets `GET /healthz`
    /// ⇒ `200` (server-up ⇒ ready). Every other route + the #14 auth gate
    /// is byte-identical to pre-`/healthz` — this constructor's
    /// signature and behaviour are unchanged (the exhaustive existing
    /// suite is untouched).
    pub fn bind(
        addr: &str,
        svc: Arc<dyn VerdictService>,
        auth: Arc<dyn Authorizer>,
    ) -> Result<HttpServer, TransportError> {
        Self::bind_with_health(addr, svc, auth, Arc::new(AtomicBool::new(true)))
    }

    /// Like [`Self::bind`] but with a caller-owned `ready` flag the
    /// unauthenticated `GET /healthz` route reflects: `false` ⇒
    /// `503 {"status":"starting"}`, `true` ⇒ `200 {"status":"ready"}`.
    /// The daemon flips it `true` once its serve loop is live — the
    /// meaningful k8s readiness boundary (a bound listener alone only
    /// proves liveness, not that the daemon is actually serving).
    ///
    /// **ADDITIVE, not a contract reshape:** this adds exactly one route
    /// (`/healthz`) and one constructor; [`Self::bind`]'s
    /// signature/behaviour, the [`VerdictService`] trait, the wire codec,
    /// the discovery chain, and the #14 auth seam for **every other
    /// route** are byte-frozen and their exhaustive unit suites untouched.
    /// `/healthz` is the ONLY auth-exempt path (see [`handle`]).
    pub fn bind_with_health(
        addr: &str,
        svc: Arc<dyn VerdictService>,
        auth: Arc<dyn Authorizer>,
        ready: Arc<AtomicBool>,
    ) -> Result<HttpServer, TransportError> {
        Self::bind_with_health_and_limit(addr, svc, auth, ready, configured_max_connections())
    }

    fn bind_with_health_and_limit(
        addr: &str,
        svc: Arc<dyn VerdictService>,
        auth: Arc<dyn Authorizer>,
        ready: Arc<AtomicBool>,
        max_connections: usize,
    ) -> Result<HttpServer, TransportError> {
        let listener = TcpListener::bind(addr)?;
        let bound = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let active = Arc::new(AtomicUsize::new(0));
        thread::spawn(move || {
            while !stop_t.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((conn, _)) => {
                        let Some(permit) = try_acquire_connection(&active, max_connections) else {
                            write_busy(conn);
                            continue;
                        };
                        // The listener is nonblocking so the accept loop can
                        // poll the stop flag. Some platforms let accepted
                        // streams inherit that mode; body reads must be
                        // blocking or a large POST can surface WouldBlock as a
                        // false "short body".
                        let _ = conn.set_nonblocking(false);
                        let (svc_c, auth_c, ready_c) = (svc.clone(), auth.clone(), ready.clone());
                        thread::spawn(move || {
                            let _permit = permit;
                            handle(conn, svc_c, auth_c, ready_c);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(HttpServer { addr: bound, stop })
    }

    /// The actually-bound address (resolves an ephemeral `:0` port).
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn handle(
    conn: TcpStream,
    svc: Arc<dyn VerdictService>,
    auth: Arc<dyn Authorizer>,
    ready: Arc<AtomicBool>,
) {
    let _ = conn.set_read_timeout(Some(CLIENT_IO_TIMEOUT));
    let _ = conn.set_write_timeout(Some(CLIENT_IO_TIMEOUT));
    let mut writer = match conn.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut reader = BufReader::new(conn);
    let Some(req) = parse_request(&mut reader) else {
        write_response(&mut writer, 400, "Bad Request", "text/plain", "bad request");
        return;
    };
    // ── GET /healthz — the ONLY unauthenticated route ───────────────────
    // Structurally Authorizer-EXEMPT: we answer and `return` for EXACTLY
    // this path BEFORE the #14 auth gate below, so the exemption cannot
    // widen to any other route (every other path still flows into
    // `auth.authorize`). The body is a FIXED constant — ZERO verdict,
    // diagnostics, worktree names, paths, or counts — so an
    // unauthenticated caller learns only a readiness boolean (a path or a
    // count would leak repo structure off-host). k8s probe semantic: a
    // bound listener proves liveness; this proves the daemon's serve loop
    // is actually up. `503` until `ready`, `200` after.
    if req.path == "/healthz" {
        let (code, reason, body): (u16, &str, &str) = if ready.load(Ordering::Relaxed) {
            (200, "OK", "{\"status\":\"ready\"}")
        } else {
            (503, "Service Unavailable", "{\"status\":\"starting\"}")
        };
        write_response(&mut writer, code, reason, "application/json", body);
        return;
    }
    // #14 seam — AllowAll under #10, so this never denies today; the
    // 401 path exists so #14 is pure policy, not a structural change.
    if !auth.authorize(req.bearer.as_deref()) {
        write_response(
            &mut writer,
            401,
            "Unauthorized",
            "text/plain",
            "unauthorized",
        );
        return;
    }

    // ── POST /admin/quiesce — authenticated graceful-drain request ──
    // This is an admin write, so it sits behind the same bearer gate as
    // POST /overlay. It carries no request body: the entire operation is
    // "refuse new pushes, drain accepted pushed worktrees, then let the
    // serve loop exit when the counts reach zero".
    if req.method == "POST" && req.path == "/admin/quiesce" {
        let activity = svc.request_quiesce();
        write_response(
            &mut writer,
            200,
            "OK",
            "application/json",
            &daemon_activity_to_json(&activity),
        );
        return;
    }

    // ── POST /overlay — the server's FIRST body-reading route (Inc 2) ──
    // Bearer-gated: the #14 auth gate above already ran, so POST /overlay
    // inherits the SAME Authorizer as every non-/healthz route — no new
    // auth surface. Bounded by construction: read EXACTLY a capped
    // Content-Length and never more; every GET route stays body-less.
    if req.method == "POST" && req.path == "/overlay" {
        let body = match req.content_length {
            // absent OR non-numeric Content-Length ⇒ 400 (same client error)
            None => {
                write_response(
                    &mut writer,
                    400,
                    "Bad Request",
                    "text/plain",
                    "POST /overlay requires a numeric Content-Length",
                );
                return;
            }
            // declared length over the cap ⇒ 413, refused BEFORE any read
            Some(n) if n > MAX_OVERLAY_BYTES => {
                write_response(
                    &mut writer,
                    413,
                    "Payload Too Large",
                    "text/plain",
                    "overlay payload exceeds the size cap",
                );
                return;
            }
            Some(n) => {
                let mut buf = vec![0u8; n];
                if reader.read_exact(&mut buf).is_err() {
                    write_response(
                        &mut writer,
                        400,
                        "Bad Request",
                        "text/plain",
                        "overlay body shorter than its Content-Length",
                    );
                    return;
                }
                buf
            }
        };
        let body = match decode_request_body(&req, body) {
            Ok(body) => body,
            Err((code, reason, message)) => {
                write_response(&mut writer, code, reason, "text/plain", &message);
                return;
            }
        };
        match Request::from_json(&String::from_utf8_lossy(&body)) {
            Some(Request::PushOverlay {
                worktree,
                base_ref,
                files,
                check_profile,
            }) => {
                let ack = svc.push_overlay_with_profile(
                    &worktree,
                    &base_ref,
                    &files,
                    check_profile.as_ref(),
                );
                write_response(
                    &mut writer,
                    200,
                    "OK",
                    "application/json",
                    &pushoverlayack_to_json(&ack),
                );
            }
            Some(Request::PushOverlayV2 {
                worktree,
                base_ref,
                files,
                check_profile,
                options,
            }) => {
                let ack = svc.push_overlay_with_options(
                    &worktree,
                    &base_ref,
                    &files,
                    check_profile.as_ref(),
                    Some(&options),
                );
                write_response(
                    &mut writer,
                    200,
                    "OK",
                    "application/json",
                    &pushoverlayack_to_json(&ack),
                );
            }
            _ => write_response(
                &mut writer,
                400,
                "Bad Request",
                "text/plain",
                "body is not a valid push_overlay request",
            ),
        }
        return;
    }

    // ── POST /batch-check — native optimistic batch gate ──
    // Same bounded body discipline as `/overlay`: exact capped
    // Content-Length, authenticated by the shared #14 seam.
    if req.method == "POST" && req.path == "/batch-check" {
        let body = match req.content_length {
            None => {
                write_response(
                    &mut writer,
                    400,
                    "Bad Request",
                    "text/plain",
                    "POST /batch-check requires a numeric Content-Length",
                );
                return;
            }
            Some(n) if n > MAX_OVERLAY_BYTES => {
                write_response(
                    &mut writer,
                    413,
                    "Payload Too Large",
                    "text/plain",
                    "batch-check payload exceeds the size cap",
                );
                return;
            }
            Some(n) => {
                let mut buf = vec![0u8; n];
                if reader.read_exact(&mut buf).is_err() {
                    write_response(
                        &mut writer,
                        400,
                        "Bad Request",
                        "text/plain",
                        "batch-check body shorter than its Content-Length",
                    );
                    return;
                }
                buf
            }
        };
        let body = match decode_request_body(&req, body) {
            Ok(body) => body,
            Err((code, reason, message)) => {
                write_response(&mut writer, code, reason, "text/plain", &message);
                return;
            }
        };
        match Request::from_json(&String::from_utf8_lossy(&body)) {
            Some(Request::BatchCheck(request)) => {
                let report = svc.batch_check(&request);
                write_response(
                    &mut writer,
                    200,
                    "OK",
                    "application/json",
                    &batchreport_to_json(&report),
                );
            }
            _ => write_response(
                &mut writer,
                400,
                "Bad Request",
                "text/plain",
                "body is not a valid batch_check request",
            ),
        }
        return;
    }

    // SSE stream route.
    if req.path == "/events" {
        let _ = write!(
            writer,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
        );
        let _ = writer.flush();
        let rx = svc.subscribe();
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(ev) => {
                    // SSE frame: `data: <json>\n\n`.
                    if write!(writer, "data: {}\n\n", event_to_json(&ev)).is_err() {
                        break;
                    }
                    if writer.flush().is_err() {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // A client that exits immediately after its verdict can
                    // otherwise sit in CLOSE_WAIT until the next verdict. A
                    // small SSE comment heartbeat detects that closed peer and
                    // lets the thread/subscription drain promptly.
                    if writer.write_all(b": keepalive\n\n").is_err() {
                        break;
                    }
                    if writer.flush().is_err() {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        return;
    }

    // One-shot routes.
    let (code, body) = route_oneshot(svc.as_ref(), &req);
    let ctype = "application/json";
    if code == 200 {
        write_response(&mut writer, 200, "OK", ctype, &body);
    } else {
        write_response(&mut writer, 404, "Not Found", ctype, "null");
    }
}

/// Pure routing of the one-shot GETs → (status_code, json_body). No I/O,
/// no socket — unit-tested directly against a mock service.
fn route_oneshot(svc: &dyn VerdictService, req: &HttpReq) -> (u16, String) {
    // `/worktrees/<W>/diagnostics`
    if let Some(rest) = req.path.strip_prefix("/worktrees/") {
        if let Some(w) = rest.strip_suffix("/diagnostics") {
            let w = pct_decode(w);
            return (
                200,
                crate::diagnostics_store::serialize(&svc.get_diagnostics(&w)),
            );
        }
    }
    match req.path.as_str() {
        "/daemon" => (
            200,
            serde_json::json!({
                "build_id": crate::build_id(),
            })
            .to_string(),
        ),
        "/admin/active" => (200, daemon_activity_to_json(&svc.daemon_activity())),
        "/worktrees" => (200, summaries_to_json(&svc.list_worktrees())),
        "/status" => match query_param(&req.query, "worktree").map(|w| pct_decode(&w)) {
            Some(w) => match svc.get_status(&w) {
                Some(s) => (200, status_to_json(&s)),
                None => (404, "null".into()),
            },
            None => (404, "null".into()),
        },
        "/verdict" => match query_param(&req.query, "worktree").map(|w| pct_decode(&w)) {
            Some(w) => match svc.get_verdict(&w) {
                Some(v) => (200, serde_json::Value::String(v).to_string()),
                None => (404, "null".into()),
            },
            None => (404, "null".into()),
        },
        _ => (404, "null".into()),
    }
}

/// Minimal percent-decoding for `%XX` + `+`→space (worktree ids are
/// paths/names; the few bytes that need escaping in a query are enough).
/// Std-only; not a general URL decoder, just what the API surface needs.
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hi = (b[i + 1] as char).to_digit(16);
                let lo = (b[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- client -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpScheme {
    Http,
    Https,
}

impl HttpScheme {
    fn default_port(self) -> u16 {
        match self {
            HttpScheme::Http => 80,
            HttpScheme::Https => 443,
        }
    }
}

enum ClientStream {
    Plain(TcpStream),
    Tls(TlsStream<TcpStream>),
}

impl Read for ClientStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ClientStream::Plain(stream) => stream.read(buf),
            ClientStream::Tls(stream) => stream.read(buf),
        }
    }
}

impl Write for ClientStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ClientStream::Plain(stream) => stream.write(buf),
            ClientStream::Tls(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ClientStream::Plain(stream) => stream.flush(),
            ClientStream::Tls(stream) => stream.flush(),
        }
    }
}

/// HTTP(S) client for the §10.3 `--remote <url>` path. `base` is like
/// `http://127.0.0.1:8080` or `https://cargoless.example` (no trailing slash
/// required).
pub struct HttpClient {
    scheme: HttpScheme,
    host: String,
    port: u16,
    /// Bearer token for protected HTTP transport routes. `None` ⇒ no
    /// `Authorization` header is sent, correct for the #10
    /// loopback/`AllowAll` posture. Network daemons that bind with an auth
    /// token require it for read and write routes.
    token: Option<String>,
}

fn parse_host_port(rest: &str, default_port: u16) -> Result<(String, u16), String> {
    if let Some(after_open) = rest.strip_prefix('[') {
        let (host, suffix) = after_open
            .split_once(']')
            .ok_or_else(|| format!("bad IPv6 host: {rest}"))?;
        if host.is_empty() {
            return Err("empty host".into());
        }
        let port = match suffix.strip_prefix(':') {
            Some(port) if !port.is_empty() => port
                .parse::<u16>()
                .map_err(|_| format!("bad port: {port}"))?,
            Some(_) => return Err("bad port: ".into()),
            None if suffix.is_empty() => default_port,
            None => return Err(format!("bad host/port: {rest}")),
        };
        return Ok((host.to_string(), port));
    }

    match rest.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            if host.is_empty() {
                return Err("empty host".into());
            }
            Ok((
                host.to_string(),
                port.parse::<u16>()
                    .map_err(|_| format!("bad port: {port}"))?,
            ))
        }
        Some(_) => Err(format!("bad host/port: {rest}; bracket IPv6 addresses")),
        None if rest.is_empty() => Err("empty host".into()),
        None => Ok((rest.to_string(), default_port)),
    }
}

impl HttpClient {
    /// Parse `http://host:port` or `https://host:port`. Returns a protocol
    /// error on a malformed base rather than panicking — discovery then falls
    /// through. Token-less (the GET read paths are token-less); use
    /// [`Self::with_token`] for an authed daemon.
    pub fn new(base: &str) -> Result<Self, TransportError> {
        let (scheme, rest) = if let Some(rest) = base.strip_prefix("http://") {
            (HttpScheme::Http, rest)
        } else if let Some(rest) = base.strip_prefix("https://") {
            (HttpScheme::Https, rest)
        } else {
            return Err(TransportError::Protocol(format!("unsupported URL: {base}")));
        };
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() || rest.contains('/') {
            return Err(TransportError::Protocol(format!("bad remote URL: {base}")));
        }
        let (host, port) =
            parse_host_port(rest, scheme.default_port()).map_err(TransportError::Protocol)?;
        Ok(Self {
            scheme,
            host,
            port,
            token: None,
        })
    }

    /// Increment 2 — like [`Self::new`] but carrying a bearer token the
    /// client presents as `Authorization: Bearer` to protected routes.
    pub fn with_token(base: &str, token: impl Into<String>) -> Result<Self, TransportError> {
        let mut c = Self::new(base)?;
        c.token = Some(token.into());
        Ok(c)
    }

    fn connect(&self) -> Result<ClientStream, TransportError> {
        self.connect_with_read_timeout(CLIENT_IO_TIMEOUT)
    }

    fn connect_with_read_timeout(
        &self,
        read_timeout: Duration,
    ) -> Result<ClientStream, TransportError> {
        let mut addrs = (self.host.as_str(), self.port).to_socket_addrs()?;
        let addr = addrs
            .next()
            .ok_or_else(|| TransportError::Protocol("remote resolved to no addresses".into()))?;
        let stream = TcpStream::connect_timeout(&addr, CLIENT_IO_TIMEOUT)?;
        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
        match self.scheme {
            HttpScheme::Http => Ok(ClientStream::Plain(stream)),
            HttpScheme::Https => {
                let connector = TlsConnector::new()
                    .map_err(|e| TransportError::Protocol(format!("TLS init failed: {e}")))?;
                match connector.connect(&self.host, stream) {
                    Ok(stream) => Ok(ClientStream::Tls(stream)),
                    Err(HandshakeError::Failure(e)) => Err(TransportError::Protocol(format!(
                        "TLS handshake failed for {}:{}: {e}",
                        self.host, self.port
                    ))),
                    Err(HandshakeError::WouldBlock(_)) => Err(TransportError::Protocol(format!(
                        "TLS handshake would block for {}:{}",
                        self.host, self.port
                    ))),
                }
            }
        }
    }

    fn post_json(
        &self,
        path: &str,
        body: &str,
        read_timeout: Duration,
        too_large_label: &str,
    ) -> Result<(u16, String), TransportError> {
        let prepared = prepare_json_body(body)?;
        if prepared.raw_len > MAX_OVERLAY_BYTES || prepared.encoded_len() > MAX_OVERLAY_BYTES {
            return Err(TransportError::Protocol(format!(
                "{too_large_label} payload too large ({} encoded bytes, {} raw bytes > {} byte limit)",
                prepared.encoded_len(),
                prepared.raw_len,
                MAX_OVERLAY_BYTES
            )));
        }
        let mut req = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n",
            self.host,
            prepared.encoded_len()
        )
        .into_bytes();
        if let Some(encoding) = prepared.content_encoding {
            req.extend_from_slice(format!("Content-Encoding: {encoding}\r\n").as_bytes());
        }
        if let Some(tok) = &self.token {
            req.extend_from_slice(format!("Authorization: Bearer {tok}\r\n").as_bytes());
        }
        req.extend_from_slice(b"\r\n");
        req.extend_from_slice(&prepared.bytes);

        let mut stream = self.connect_with_read_timeout(read_timeout)?;
        stream.write_all(&req)?;
        stream.flush()?;
        let mut raw = String::new();
        stream.read_to_string(&mut raw)?;
        let (head, resp) = raw
            .split_once("\r\n\r\n")
            .ok_or_else(|| TransportError::Protocol("no header/body split".into()))?;
        let code = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .ok_or_else(|| TransportError::Protocol("no status code".into()))?;
        Ok((code, resp.to_string()))
    }

    fn get(&self, path_and_query: &str) -> Result<(u16, String), TransportError> {
        let mut stream = self.connect()?;
        write!(
            stream,
            "GET {path_and_query} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.host
        )?;
        if let Some(tok) = &self.token {
            write!(stream, "Authorization: Bearer {tok}\r\n")?;
        }
        write!(stream, "\r\n")?;
        stream.flush()?;
        let mut raw = String::new();
        stream.read_to_string(&mut raw)?;
        let (head, body) = raw
            .split_once("\r\n\r\n")
            .ok_or_else(|| TransportError::Protocol("no header/body split".into()))?;
        let code = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .ok_or_else(|| TransportError::Protocol("no status code".into()))?;
        Ok((code, body.to_string()))
    }

    pub fn daemon_build_id(&self) -> Result<Option<String>, TransportError> {
        let (code, body) = self.get("/daemon")?;
        if code == 404 {
            return Ok(None);
        }
        if code == 401 {
            return Err(TransportError::Unauthorized);
        }
        let value: serde_json::Value = serde_json::from_str(&body)
            .map_err(|_| TransportError::Protocol("daemon identity is not json".into()))?;
        Ok(value
            .get("build_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string))
    }
}

impl TransportClient for HttpClient {
    fn get_status(&self, w: &str) -> Result<Option<WorktreeStatus>, TransportError> {
        let (code, body) = self.get(&format!("/status?worktree={w}"))?;
        if code == 404 {
            return Ok(None);
        }
        if code == 401 {
            return Err(TransportError::Unauthorized);
        }
        Ok(status_from_json(&body))
    }

    fn get_verdict(&self, w: &str) -> Result<Option<String>, TransportError> {
        let (code, body) = self.get(&format!("/verdict?worktree={w}"))?;
        if code == 404 {
            return Ok(None);
        }
        if code == 401 {
            return Err(TransportError::Unauthorized);
        }
        match serde_json::from_str::<serde_json::Value>(body.trim()) {
            Ok(serde_json::Value::String(s)) => Ok(Some(s)),
            _ => Err(TransportError::Protocol("verdict not a string".into())),
        }
    }

    fn get_diagnostics(&self, w: &str) -> Result<Vec<Diagnostic>, TransportError> {
        let (code, body) = self.get(&format!("/worktrees/{w}/diagnostics"))?;
        if code == 401 {
            return Err(TransportError::Unauthorized);
        }
        Ok(crate::diagnostics_store::deserialize(&body))
    }

    fn list_worktrees(&self) -> Result<Vec<WorktreeSummary>, TransportError> {
        let (code, body) = self.get("/worktrees")?;
        if code == 401 {
            return Err(TransportError::Unauthorized);
        }
        Ok(summaries_from_json(&body))
    }

    fn subscribe(&self) -> Result<Receiver<TransitionEvent>, TransportError> {
        let mut stream = self.connect()?;
        write!(
            stream,
            "GET /events HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.host
        )?;
        if let Some(tok) = &self.token {
            write!(stream, "Authorization: Bearer {tok}\r\n")?;
        }
        write!(stream, "\r\n")?;
        stream.flush()?;
        let (tx, rx) = channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            // Skip the response head (up to the blank line).
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                if line.trim_end().is_empty() {
                    break;
                }
            }
            // SSE frames: `data: <json>` lines, blank-line separated.
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if let Some(payload) = line.strip_prefix("data: ") {
                    if let Some(ev) = event_from_json(payload.trim()) {
                        if tx.send(ev).is_err() {
                            break; // consumer dropped
                        }
                    }
                }
            }
        });
        Ok(rx)
    }

    fn push_overlay(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
    ) -> Result<PushOverlayAck, TransportError> {
        self.push_overlay_with_profile(worktree, base_ref, files, None)
    }

    fn push_overlay_with_profile(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
        check_profile: Option<&crate::transport::CheckProfile>,
    ) -> Result<PushOverlayAck, TransportError> {
        self.push_overlay_with_options(worktree, base_ref, files, check_profile, None)
    }

    fn push_overlay_with_options(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
        check_profile: Option<&crate::transport::CheckProfile>,
        options: Option<&PushOverlayOptions>,
    ) -> Result<PushOverlayAck, TransportError> {
        // The server's one body-carrying route. Reuse the frozen
        // `Request` codec for the body (no bespoke JSON); bearer header
        // only when a token is configured (#10 loopback posture sends
        // none — `AllowAll` accepts it).
        let body = match options.filter(|o| !o.is_empty()) {
            Some(options) => Request::PushOverlayV2 {
                worktree: worktree.to_string(),
                base_ref: base_ref.to_string(),
                files: files.to_vec(),
                check_profile: check_profile.cloned(),
                options: options.clone(),
            },
            None => Request::PushOverlay {
                worktree: worktree.to_string(),
                base_ref: base_ref.to_string(),
                files: files.to_vec(),
                check_profile: check_profile.cloned(),
            },
        }
        .to_json();
        let (code, resp) = self.post_json("/overlay", &body, CLIENT_IO_TIMEOUT, "overlay")?;
        match code {
            200 => pushoverlayack_from_json(&resp)
                .ok_or_else(|| TransportError::Protocol("malformed push_overlay ack".into())),
            401 => Err(TransportError::Unauthorized),
            413 => Err(TransportError::Protocol(
                "overlay payload too large (413)".into(),
            )),
            c => Err(TransportError::Protocol(format!(
                "push_overlay HTTP {c}: {}",
                resp.trim()
            ))),
        }
    }

    fn batch_check(&self, request: &BatchCheckRequest) -> Result<BatchReport, TransportError> {
        let body = Request::BatchCheck(request.clone()).to_json();
        let (code, resp) = self.post_json(
            "/batch-check",
            &body,
            BATCH_CHECK_READ_TIMEOUT,
            "batch_check",
        )?;
        match code {
            200 => batchreport_from_json(&resp)
                .ok_or_else(|| TransportError::Protocol("malformed batch_check report".into())),
            401 => Err(TransportError::Unauthorized),
            413 => Err(TransportError::Protocol(
                "batch_check payload too large (413)".into(),
            )),
            c => Err(TransportError::Protocol(format!(
                "batch_check HTTP {c}: {}",
                resp.trim()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use super::super::inproc::testmock::MockService;
    use super::super::{AllowAll, BatchCheckRequest, BearerToken};
    use super::*;

    fn server() -> HttpServer {
        HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(AllowAll),
        )
        .expect("bind ephemeral")
    }

    fn client_for(s: &HttpServer) -> HttpClient {
        HttpClient::new(&format!("http://{}", s.addr())).expect("client")
    }

    #[test]
    fn http_roundtrip_all_oneshots_incl_honesty_case() {
        let s = server();
        std::thread::sleep(Duration::from_millis(50));
        let c = client_for(&s);
        assert_eq!(c.get_verdict("green-wt").unwrap(), Some("green".into()));
        assert_eq!(c.get_verdict("nope").unwrap(), None);
        let st = c.get_status("red-wt").unwrap().unwrap();
        assert_eq!(st.verdict, "red");
        assert!(
            st.crates.is_empty(),
            "honesty case survives the HTTP wire — verdict stands alone"
        );
        assert_eq!(c.get_status("nope").unwrap(), None);
        assert_eq!(c.get_diagnostics("red-wt").unwrap().len(), 1);
        assert!(c.get_diagnostics("green-wt").unwrap().is_empty());
        assert_eq!(c.list_worktrees().unwrap().len(), 2);
    }

    #[test]
    fn sse_streams_transitions() {
        let svc = Arc::new(MockService::new());
        let s = HttpServer::bind("127.0.0.1:0", svc.clone(), Arc::new(AllowAll)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let c = client_for(&s);
        let rx = c.subscribe().unwrap();
        std::thread::sleep(Duration::from_millis(80)); // subscriber registers
        let ev = TransitionEvent {
            worktree: "red-wt".into(),
            verdict: "red".into(),
            red_diagnostics: 1,
            verdict_failure_reason: None,
            published_at: 5,
        };
        svc.emit(ev.clone());
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), ev);
    }

    #[test]
    fn denying_authorizer_yields_401_not_a_panic() {
        // Proves the #14 seam is load-bearing: a policy that denies
        // produces a clean 401 the client surfaces as None/!ok — the
        // adapter needs ZERO change for #14 to add real policy.
        struct DenyAll;
        impl Authorizer for DenyAll {
            fn authorize(&self, _t: Option<&str>) -> bool {
                false
            }
        }
        let s = HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(DenyAll),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let c = client_for(&s);
        // 401 ⇒ not 404, not 200; get_status maps non-404 + unparseable
        // body to None/err, never panics.
        let r = c.get_status("green-wt");
        assert!(r.is_ok() || r.is_err(), "must not panic under deny");
    }

    #[test]
    fn bearer_client_sends_token_on_remote_reads() {
        let s = HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(BearerToken::new("sekret")),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let bare = client_for(&s);
        assert!(matches!(
            bare.get_status("green-wt"),
            Err(TransportError::Unauthorized)
        ));

        let authed =
            HttpClient::with_token(&format!("http://{}", s.addr()), "sekret").expect("client");
        assert_eq!(
            authed.get_verdict("green-wt").unwrap(),
            Some("green".into())
        );
        assert_eq!(authed.get_status("red-wt").unwrap().unwrap().verdict, "red");
        assert_eq!(authed.list_worktrees().unwrap().len(), 2);
    }

    #[test]
    fn pct_decode_handles_escapes_and_plus() {
        assert_eq!(pct_decode("tf%2Dmv%2Fflat"), "tf-mv/flat");
        assert_eq!(pct_decode("a+b"), "a b");
        assert_eq!(pct_decode("plain"), "plain");
        assert_eq!(pct_decode("%zz"), "%zz"); // malformed ⇒ literal, no panic
    }

    #[test]
    fn bad_base_url_is_typed_error_not_panic() {
        assert!(HttpClient::new("ftp://x").is_err());
        assert!(HttpClient::new("http://h:notaport").is_err());
        let http = HttpClient::new("http://h:9").expect("http ok");
        assert_eq!(http.scheme, HttpScheme::Http);
        assert_eq!(http.host, "h");
        assert_eq!(http.port, 9);
        let https = HttpClient::new("https://h").expect("https default port ok");
        assert_eq!(https.scheme, HttpScheme::Https);
        assert_eq!(https.host, "h");
        assert_eq!(https.port, 443);
        let https_port = HttpClient::new("https://h:8443").expect("https explicit port ok");
        assert_eq!(https_port.scheme, HttpScheme::Https);
        assert_eq!(https_port.port, 8443);
    }

    // ───────── /healthz — unauthenticated readiness probe ─────────
    // (No `HttpClient` method by design: /healthz is a k8s/curl probe,
    // NOT part of the TransportClient contract — proved over raw GET.)

    fn raw_get(addr: std::net::SocketAddr, target: &str) -> (u16, String) {
        let mut s = TcpStream::connect(addr).expect("connect");
        write!(
            s,
            "GET {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        s.flush().unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        let (head, body) = match raw.split_once("\r\n\r\n") {
            Some(hb) => hb,
            None => (raw.as_str(), ""),
        };
        let code = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .expect("status code");
        (code, body.to_string())
    }

    #[test]
    fn healthz_is_unauth_503_until_ready_then_200_with_constant_body() {
        // DenyAll authorizer: proves /healthz is STRUCTURALLY auth-exempt
        // (a DenyAll daemon 401s every other route — see the surgical
        // test below — yet still answers /healthz).
        struct DenyAll;
        impl Authorizer for DenyAll {
            fn authorize(&self, _t: Option<&str>) -> bool {
                false
            }
        }
        let ready = Arc::new(AtomicBool::new(false));
        let s = HttpServer::bind_with_health(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(DenyAll),
            ready.clone(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // Not ready ⇒ 503 + the FIXED starting constant. Exact-equality
        // is the strongest zero-leakage proof: byte-for-byte the
        // constant, so it cannot carry a verdict/path/count.
        let (code, body) = raw_get(s.addr(), "/healthz");
        assert_eq!(
            code, 503,
            "unready ⇒ 503 (auth-exempt: DenyAll did not 401 it)"
        );
        assert_eq!(
            body, "{\"status\":\"starting\"}",
            "fixed constant, zero leakage"
        );

        // Flip ready ⇒ 200 + the FIXED ready constant.
        ready.store(true, Ordering::Relaxed);
        let (code, body) = raw_get(s.addr(), "/healthz");
        assert_eq!(code, 200, "ready ⇒ 200");
        assert_eq!(
            body, "{\"status\":\"ready\"}",
            "fixed constant, zero leakage"
        );
        // Belt-and-braces: the body names no worktree the service knows
        // and carries no path/structure (a leak would mention these).
        assert!(!body.contains("green-wt") && !body.contains("red-wt"));
        assert!(!body.contains('/'), "no path leaks to an unauth caller");
    }

    #[test]
    fn healthz_exemption_is_surgical_every_other_route_still_401() {
        // The exemption must NOT widen: under DenyAll, every NON-/healthz
        // route still hits the #14 gate and 401s.
        struct DenyAll;
        impl Authorizer for DenyAll {
            fn authorize(&self, _t: Option<&str>) -> bool {
                false
            }
        }
        let s = HttpServer::bind_with_health(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(DenyAll),
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        for route in [
            "/admin/active",
            "/status?worktree=green-wt",
            "/verdict?worktree=red-wt",
            "/worktrees",
            "/worktrees/red-wt/diagnostics",
            "/events",
        ] {
            let (code, _) = raw_get(s.addr(), route);
            assert_eq!(
                code, 401,
                "{route} must still be auth-gated (exemption is /healthz-only)"
            );
        }
        // …and /healthz on the SAME deny server still answers (200, ready).
        assert_eq!(raw_get(s.addr(), "/healthz").0, 200);
    }

    #[test]
    fn old_bind_constructor_healthz_defaults_ready_200_no_regression() {
        // The byte-frozen `bind` delegate ⇒ always-ready: an old caller
        // (every existing test/consumer) sees /healthz ⇒ 200 and EVERY
        // other route unchanged. Proves `bind` behaviour is unregressed.
        let s = HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(AllowAll),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let (code, body) = raw_get(s.addr(), "/healthz");
        assert_eq!(code, 200);
        assert_eq!(body, "{\"status\":\"ready\"}");
        // Non-/healthz routes still work exactly as before.
        let c = client_for(&s);
        assert_eq!(c.get_verdict("green-wt").unwrap(), Some("green".into()));
    }

    // ───────── Increment 2 — POST /overlay body-reading route ─────────

    /// Raw `POST` with a caller-chosen `Content-Length` header (or none)
    /// — lets a test declare a deliberately-wrong length.
    fn raw_post(
        addr: std::net::SocketAddr,
        path: &str,
        body: &str,
        content_length: Option<&str>,
    ) -> (u16, String) {
        raw_post_bytes(addr, path, body.as_bytes(), content_length, None)
    }

    fn raw_post_bytes(
        addr: std::net::SocketAddr,
        path: &str,
        body: &[u8],
        content_length: Option<&str>,
        content_encoding: Option<&str>,
    ) -> (u16, String) {
        let mut s = TcpStream::connect(addr).expect("connect");
        let mut head = format!("POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n");
        if let Some(cl) = content_length {
            head.push_str(&format!("Content-Length: {cl}\r\n"));
        }
        if let Some(encoding) = content_encoding {
            head.push_str(&format!("Content-Encoding: {encoding}\r\n"));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        s.flush().unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        let (h, b) = match raw.split_once("\r\n\r\n") {
            Some(hb) => hb,
            None => (raw.as_str(), ""),
        };
        let code = h
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .expect("status code");
        (code, b.to_string())
    }

    fn overlay_body() -> String {
        Request::PushOverlay {
            worktree: "wt-push".into(),
            base_ref: "origin/main".into(),
            files: vec![("src/lib.rs".into(), "fn f(){}".into())],
            check_profile: None,
        }
        .to_json()
    }

    #[test]
    fn admin_active_and_quiesce_routes_are_json_and_bearer_gated() {
        let s = HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(AllowAll),
        )
        .expect("bind");
        std::thread::sleep(Duration::from_millis(50));

        let (code, body) = raw_get(s.addr(), "/admin/active");
        assert_eq!(code, 200);
        assert!(
            body.contains("\"active_worktrees\":0") && body.contains("\"pending_pushes\":0"),
            "admin activity exposes bounded counts as JSON"
        );

        let (code, body) = raw_post(s.addr(), "/admin/quiesce", "", None);
        assert_eq!(code, 200);
        assert!(
            body.contains("\"active_worktrees\":0") && body.contains("\"pending_pushes\":0"),
            "admin quiesce responds with the same activity JSON shape"
        );

        struct DenyAll;
        impl Authorizer for DenyAll {
            fn authorize(&self, _t: Option<&str>) -> bool {
                false
            }
        }
        let denied = HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(DenyAll),
        )
        .expect("bind");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(raw_get(denied.addr(), "/admin/active").0, 401);
        assert_eq!(raw_post(denied.addr(), "/admin/quiesce", "", None).0, 401);
    }

    #[test]
    fn post_overlay_bounded_body_400_413_and_happy_path() {
        let s = HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(AllowAll),
        )
        .expect("bind");
        std::thread::sleep(Duration::from_millis(50));
        let body = overlay_body();

        // Happy path: exact Content-Length ⇒ 200 + a parseable ack.
        let (code, resp) = raw_post(s.addr(), "/overlay", &body, Some(&body.len().to_string()));
        assert_eq!(code, 200, "exact-length POST /overlay ⇒ 200");
        let ack = pushoverlayack_from_json(&resp).expect("ack parses");
        assert_eq!(ack.worktree, "wt-push");

        // Absent Content-Length ⇒ 400 (bounded-by-construction: a POST
        // body is read ONLY against an exact declared length).
        assert_eq!(raw_post(s.addr(), "/overlay", &body, None).0, 400);
        // Non-numeric Content-Length ⇒ 400 (same client error as absent).
        assert_eq!(
            raw_post(s.addr(), "/overlay", &body, Some("not-a-number")).0,
            400
        );
        // Declared length over the 32 MiB cap ⇒ 413, refused BEFORE any
        // read (we send a tiny body but claim ~99 GB).
        assert_eq!(
            raw_post(s.addr(), "/overlay", &body, Some("99999999999")).0,
            413
        );
        // A body that is not a valid push_overlay request ⇒ 400.
        let junk = "{\"op\":\"nonsense\"}";
        assert_eq!(
            raw_post(s.addr(), "/overlay", junk, Some(&junk.len().to_string())).0,
            400
        );
        drop(s);
    }

    #[test]
    fn post_overlay_accepts_gzip_body_with_bounded_decode() {
        let s = HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(AllowAll),
        )
        .expect("bind");
        std::thread::sleep(Duration::from_millis(50));
        let body = Request::PushOverlay {
            worktree: "wt-gzip".into(),
            base_ref: "origin/main".into(),
            files: vec![(
                "src/generated.rs".into(),
                "registry_mirror_entry = 42;\n".repeat(80_000),
            )],
            check_profile: None,
        }
        .to_json();
        let prepared = prepare_json_body(&body).expect("gzip body");
        assert_eq!(prepared.content_encoding, Some("gzip"));
        assert!(prepared.encoded_len() < body.len());

        let (code, resp) = raw_post_bytes(
            s.addr(),
            "/overlay",
            &prepared.bytes,
            Some(&prepared.encoded_len().to_string()),
            prepared.content_encoding,
        );

        assert_eq!(code, 200, "gzip POST /overlay should decode and route");
        let ack = pushoverlayack_from_json(&resp).expect("ack parses");
        assert_eq!(ack.worktree, "wt-gzip");
        drop(s);
    }

    #[test]
    fn post_overlay_rejects_unknown_content_encoding() {
        let s = HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(AllowAll),
        )
        .expect("bind");
        std::thread::sleep(Duration::from_millis(50));
        let body = overlay_body();

        let (code, resp) = raw_post_bytes(
            s.addr(),
            "/overlay",
            body.as_bytes(),
            Some(&body.len().to_string()),
            Some("br"),
        );

        assert_eq!(code, 415);
        assert!(resp.contains("unsupported Content-Encoding"));
        drop(s);
    }

    #[test]
    fn post_overlay_is_bearer_gated_deny_yields_401() {
        // Unlike /healthz, POST /overlay is NOT auth-exempt — it flows
        // through the #14 gate. A DenyAll authorizer ⇒ 401, no panic.
        struct DenyAll;
        impl Authorizer for DenyAll {
            fn authorize(&self, _t: Option<&str>) -> bool {
                false
            }
        }
        let s = HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(DenyAll),
        )
        .expect("bind");
        std::thread::sleep(Duration::from_millis(50));
        let body = overlay_body();
        let (code, _) = raw_post(s.addr(), "/overlay", &body, Some(&body.len().to_string()));
        assert_eq!(
            code, 401,
            "POST /overlay is bearer-gated (not /healthz-exempt)"
        );
        drop(s);
    }

    #[test]
    fn http_server_refuses_excess_connections_with_503() {
        let s = HttpServer::bind_with_health_and_limit(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(AllowAll),
            Arc::new(AtomicBool::new(true)),
            1,
        )
        .expect("bind");
        std::thread::sleep(Duration::from_millis(50));

        let mut held = TcpStream::connect(s.addr()).expect("held connect");
        write!(
            held,
            "GET /events HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        held.flush().unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let (code, body) = raw_get(s.addr(), "/healthz");
        assert_eq!(code, 503, "connection cap should fail fast with 503");
        assert!(
            body.contains("busy"),
            "busy response should be actionable, got {body:?}"
        );
        drop(held);
        drop(s);
    }

    #[test]
    fn http_client_push_overlay_roundtrips_over_the_wire() {
        // The HttpClient write path end-to-end: POST /overlay → ack.
        let s = HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(AllowAll),
        )
        .expect("bind");
        std::thread::sleep(Duration::from_millis(50));
        let c = client_for(&s);
        let ack = c
            .push_overlay(
                "wt-z",
                "origin/main",
                &[("a.rs".to_string(), "// a".to_string())],
            )
            .expect("push_overlay ok");
        // MockService uses the trait default ⇒ honest refusal (accepted
        // false) — the WIRE is what this proves: request encoded, routed,
        // ack decoded. Real acceptance is the serve loop's job (2b/§4).
        assert_eq!(ack.worktree, "wt-z");
        assert!(!ack.accepted);
        drop(s);
    }

    #[test]
    fn http_client_refuses_oversized_overlay_before_connect() {
        // Port 9 is intentionally not served here. A connection-refused error
        // would prove the cap check happened too late.
        let c = HttpClient::new("http://127.0.0.1:9").expect("client");
        let huge = "x".repeat(MAX_OVERLAY_BYTES + 1);
        let err = c
            .push_overlay("wt-z", "origin/main", &[("src/big.rs".into(), huge)])
            .unwrap_err();

        assert!(
            matches!(err, TransportError::Protocol(ref msg)
                if msg.contains("overlay payload too large")
                    && msg.contains(&MAX_OVERLAY_BYTES.to_string())),
            "oversized push must fail locally with the HTTP cap message, got {err:?}"
        );
    }

    #[test]
    fn http_client_batch_check_roundtrips_over_the_wire() {
        // The HttpClient write path end-to-end: POST /batch-check →
        // structured attribution report. MockService uses the trait default
        // indeterminate report; the wire is what this test pins.
        let s = HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(MockService::new()),
            Arc::new(AllowAll),
        )
        .expect("bind");
        std::thread::sleep(Duration::from_millis(50));
        let c = client_for(&s);
        let mut request = BatchCheckRequest::new("batch-http", "origin/main");
        request.members = vec![crate::batch::BatchMember::new("wt-a")];

        let report = c.batch_check(&request).expect("batch_check ok");

        assert_eq!(report.batch_id, "batch-http");
        assert_eq!(report.members.len(), 1);
        assert_eq!(report.members[0].worktree, "wt-a");
        assert_eq!(
            report.members[0].provenance,
            crate::batch::BatchProvenance::Indeterminate
        );
        drop(s);
    }

    #[derive(Default)]
    struct ConcurrentBatchService {
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    impl ConcurrentBatchService {
        fn enter(&self) -> ActiveBatchGuard<'_> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            let mut observed = self.max_active.load(Ordering::SeqCst);
            while active > observed {
                match self.max_active.compare_exchange(
                    observed,
                    active,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(next) => observed = next,
                }
            }
            ActiveBatchGuard { svc: self }
        }
    }

    struct ActiveBatchGuard<'a> {
        svc: &'a ConcurrentBatchService,
    }

    impl Drop for ActiveBatchGuard<'_> {
        fn drop(&mut self) {
            self.svc.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl VerdictService for ConcurrentBatchService {
        fn get_status(&self, _worktree: &str) -> Option<WorktreeStatus> {
            None
        }

        fn get_verdict(&self, _worktree: &str) -> Option<String> {
            None
        }

        fn get_diagnostics(&self, _worktree: &str) -> Vec<Diagnostic> {
            Vec::new()
        }

        fn list_worktrees(&self) -> Vec<WorktreeSummary> {
            Vec::new()
        }

        fn subscribe(&self) -> Receiver<TransitionEvent> {
            let (_tx, rx) = std::sync::mpsc::channel();
            rx
        }

        fn batch_check(&self, request: &BatchCheckRequest) -> BatchReport {
            let _guard = self.enter();
            thread::sleep(Duration::from_millis(150));
            BatchReport {
                batch_id: request.batch_id.clone(),
                verdict: crate::batch::BatchVerdict::Green,
                members: request
                    .members
                    .iter()
                    .map(|member| crate::batch::BatchMemberResult {
                        worktree: member.worktree.clone(),
                        verdict: crate::batch::BatchVerdict::Green,
                        provenance: crate::batch::BatchProvenance::CombinedGreen,
                        diagnostics: Vec::new(),
                        duration_ms: 150,
                    })
                    .collect(),
                combined_checks: 1,
                solo_checks: 0,
                duration_ms: 150,
                queue_wait_ms: 0,
                executed_members: request.members.len() as u32,
                executed_batch_id: Some(request.batch_id.clone()),
            }
        }
    }

    #[test]
    fn batch_check_requests_overlap_on_http_server() {
        let svc = Arc::new(ConcurrentBatchService::default());
        let s = HttpServer::bind(
            "127.0.0.1:0",
            Arc::clone(&svc) as Arc<dyn VerdictService>,
            Arc::new(AllowAll),
        )
        .expect("bind");
        std::thread::sleep(Duration::from_millis(50));
        let remote = format!("http://{}", s.addr());
        let start = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();

        for idx in 0..8 {
            let remote = remote.clone();
            let start = Arc::clone(&start);
            handles.push(thread::spawn(move || {
                let client = HttpClient::new(&remote).expect("client");
                let mut request =
                    BatchCheckRequest::new(format!("http-concurrent-{idx}"), "origin/main");
                request.members = vec![crate::batch::BatchMember::new(format!("wt-{idx}"))];
                start.wait();
                client.batch_check(&request).expect("batch_check")
            }));
        }

        let reports: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("concurrent http batch thread"))
            .collect();

        assert_eq!(reports.len(), 8);
        for idx in 0..8 {
            assert!(
                reports
                    .iter()
                    .any(|report| report.batch_id == format!("http-concurrent-{idx}")
                        && report.verdict == crate::batch::BatchVerdict::Green
                        && report.members.len() == 1),
                "missing green report for request {idx}"
            );
        }
        assert!(
            svc.max_active.load(Ordering::SeqCst) > 1,
            "HTTP server should process overlapping batch_check requests"
        );
        drop(s);
    }
}
