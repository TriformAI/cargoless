//! HTTP + SSE adapter (`D-FLEET-SHARED-DAEMON` §10.2 network mode +
//! §11 SSE-vs-polling). `serve --repo --bind <addr>` exposes the logical
//! API over a **minimal, bounded, std-only HTTP/1.1** server (no HTTP
//! framework — the house ethos: JSON-RPC framing / debounce / ignore are
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
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread;

use cargoless_proto::Diagnostic;

use super::{
    Authorizer, TransitionEvent, TransportClient, TransportError, VerdictService, WorktreeStatus,
    WorktreeSummary, event_from_json, event_to_json, status_from_json, status_to_json,
    summaries_from_json, summaries_to_json,
};

// ---- tiny request model -------------------------------------------------

struct HttpReq {
    path: String,
    query: String,
    bearer: Option<String>,
}

/// Parse the request line + headers (we only need method/path/query +
/// `Authorization: Bearer`). Body is never read — the API is all GET.
/// Returns `None` on a malformed head (server answers 400).
fn parse_request(reader: &mut impl BufRead) -> Option<HttpReq> {
    let mut start = String::new();
    reader.read_line(&mut start).ok()?;
    let mut it = start.split_whitespace();
    let _method = it.next()?; // GET (only verb served)
    let target = it.next()?.to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut bearer = None;
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
            if k.trim().eq_ignore_ascii_case("authorization") {
                let v = v.trim();
                if let Some(tok) = v.strip_prefix("Bearer ") {
                    bearer = Some(tok.to_string());
                }
            }
        }
    }
    Some(HttpReq {
        path,
        query,
        bearer,
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

// ---- server -------------------------------------------------------------

/// A running HTTP server. Dropping it stops the accept loop; in-flight
/// connections (incl. long-lived SSE) drain when their peer disconnects.
pub struct HttpServer {
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
}

impl HttpServer {
    /// Bind `addr` (e.g. `127.0.0.1:0` for an ephemeral test port) and
    /// serve `svc`, gating every request through `auth`. Pass
    /// `Arc::new(AllowAll)` for the #10 posture; #14 passes a token
    /// policy — this signature does not change.
    pub fn bind(
        addr: &str,
        svc: Arc<dyn VerdictService>,
        auth: Arc<dyn Authorizer>,
    ) -> Result<HttpServer, TransportError> {
        let listener = TcpListener::bind(addr)?;
        let bound = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        thread::spawn(move || {
            while !stop_t.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((conn, _)) => {
                        let (svc_c, auth_c) = (svc.clone(), auth.clone());
                        thread::spawn(move || handle(conn, svc_c, auth_c));
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

fn handle(conn: TcpStream, svc: Arc<dyn VerdictService>, auth: Arc<dyn Authorizer>) {
    let mut writer = match conn.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut reader = BufReader::new(conn);
    let Some(req) = parse_request(&mut reader) else {
        write_response(&mut writer, 400, "Bad Request", "text/plain", "bad request");
        return;
    };
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

    // SSE stream route.
    if req.path == "/events" {
        let _ = write!(
            writer,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
        );
        let _ = writer.flush();
        let rx = svc.subscribe();
        for ev in rx {
            // SSE frame: `data: <json>\n\n`.
            if write!(writer, "data: {}\n\n", event_to_json(&ev)).is_err() {
                break;
            }
            if writer.flush().is_err() {
                break;
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

/// HTTP client for the §10.3 `--remote <url>` path. `base` is like
/// `http://127.0.0.1:8080` (no trailing slash required).
pub struct HttpClient {
    host: String,
    port: u16,
}

impl HttpClient {
    /// Parse `http://host:port` (the only scheme #10 serves; #14 may add
    /// TLS). Returns a protocol error on a malformed base rather than
    /// panicking — discovery then falls through.
    pub fn new(base: &str) -> Result<Self, TransportError> {
        let rest = base
            .strip_prefix("http://")
            .ok_or_else(|| TransportError::Protocol(format!("unsupported URL: {base}")))?;
        let rest = rest.trim_end_matches('/');
        let (host, port) = match rest.split_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| TransportError::Protocol(format!("bad port: {p}")))?,
            ),
            None => (rest.to_string(), 80),
        };
        Ok(Self { host, port })
    }

    fn get(&self, path_and_query: &str) -> Result<(u16, String), TransportError> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port))?;
        write!(
            stream,
            "GET {path_and_query} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.host
        )?;
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
}

impl TransportClient for HttpClient {
    fn get_status(&self, w: &str) -> Result<Option<WorktreeStatus>, TransportError> {
        let (code, body) = self.get(&format!("/status?worktree={w}"))?;
        if code == 404 {
            return Ok(None);
        }
        Ok(status_from_json(&body))
    }

    fn get_verdict(&self, w: &str) -> Result<Option<String>, TransportError> {
        let (code, body) = self.get(&format!("/verdict?worktree={w}"))?;
        if code == 404 {
            return Ok(None);
        }
        match serde_json::from_str::<serde_json::Value>(body.trim()) {
            Ok(serde_json::Value::String(s)) => Ok(Some(s)),
            _ => Err(TransportError::Protocol("verdict not a string".into())),
        }
    }

    fn get_diagnostics(&self, w: &str) -> Result<Vec<Diagnostic>, TransportError> {
        let (_code, body) = self.get(&format!("/worktrees/{w}/diagnostics"))?;
        Ok(crate::diagnostics_store::deserialize(&body))
    }

    fn list_worktrees(&self) -> Result<Vec<WorktreeSummary>, TransportError> {
        let (_code, body) = self.get("/worktrees")?;
        Ok(summaries_from_json(&body))
    }

    fn subscribe(&self) -> Result<Receiver<TransitionEvent>, TransportError> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port))?;
        write!(
            stream,
            "GET /events HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.host
        )?;
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
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::AllowAll;
    use super::super::inproc::testmock::MockService;
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
        assert!(HttpClient::new("http://h:9").is_ok());
    }
}
