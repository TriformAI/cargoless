//! Unix-socket adapter (`D-FLEET-SHARED-DAEMON` §10.2, local-default
//! fleet mode). A long-running `serve --repo` listens on the conventional
//! socket ([`super::discovery::conventional_socket_path`]); many short
//! `cargoless status` / `events` CLI invocations connect, send one
//! newline-terminated JSON request, read one newline-terminated JSON
//! response (or, for `subscribe`, an unbounded NDJSON stream of
//! transition frames). One-request-per-line framing — the same
//! hand-rolled-framing posture the crate already uses for LSP JSON-RPC
//! (no RPC framework dependency).
//!
//! `#[cfg(unix)]`: the v0 targets are Linux + macOS. On a non-unix
//! target the server/client construct to a typed
//! [`TransportError::Protocol`] "unsupported" rather than failing to
//! compile — the discovery chain then falls through to file-read /
//! spawn-local, exactly as if no socket were present.

// `Receiver`/`channel` are consumed on the unix target via `imp`'s
// `use super::*`; the non-unix stub gets its own `Arc` (cfg'd below) so
// the un-cfg'd surface carries no import that is unused on the unix CI
// target.
use std::sync::mpsc::{Receiver, channel};

use cargoless_proto::Diagnostic;

use super::{
    PushOverlayAck, PushOverlayOptions, Request, TransitionEvent, TransportClient, TransportError,
    VerdictService, WorktreeStatus, WorktreeSummary, event_from_json, event_to_json,
    pushoverlayack_from_json, pushoverlayack_to_json, status_from_json, status_to_json,
    summaries_from_json, summaries_to_json,
};

/// Response framing shared with the request side. A logical reply is one
/// JSON line; `get_diagnostics` reuses the byte-identical
/// [`crate::diagnostics_store`] array codec (DRY — the retained-on-disk
/// and over-the-wire diagnostics are the same shape, so a consumer parses
/// one format). Only `imp` (the unix server) calls this, so it is
/// `cfg(unix)` — keeps the un-cfg'd surface free of non-unix dead code.
#[cfg(unix)]
fn dispatch_oneshot(svc: &dyn VerdictService, req: &Request) -> String {
    match req {
        Request::GetStatus(w) => match svc.get_status(w) {
            Some(s) => status_to_json(&s),
            None => "null".to_string(),
        },
        Request::GetVerdict(w) => match svc.get_verdict(w) {
            Some(v) => serde_json::Value::String(v).to_string(),
            None => "null".to_string(),
        },
        Request::GetDiagnostics(w) => crate::diagnostics_store::serialize(&svc.get_diagnostics(w)),
        Request::ListWorktrees => summaries_to_json(&svc.list_worktrees()),
        // Subscribe is not a one-shot; the server handles it as a stream
        // before reaching here. Defensive: never panic.
        Request::Subscribe => "null".to_string(),
        // Increment 2: `PushOverlay` IS a one-shot (write-ingest →
        // cheap ack; the verdict comes back via the read plane). One
        // NDJSON request line in, one ack line out.
        Request::PushOverlay {
            worktree,
            base_ref,
            files,
            check_profile,
        } => pushoverlayack_to_json(&svc.push_overlay_with_profile(
            worktree,
            base_ref,
            files,
            check_profile.as_ref(),
        )),
        Request::PushOverlayV2 {
            worktree,
            base_ref,
            files,
            check_profile,
            options,
        } => pushoverlayack_to_json(&svc.push_overlay_with_options(
            worktree,
            base_ref,
            files,
            check_profile.as_ref(),
            Some(options),
        )),
    }
}

#[cfg(unix)]
mod imp {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use super::*;

    /// A running Unix-socket server. Dropping it stops accepting (the
    /// listener closes); in-flight connection threads drain naturally.
    pub struct UnixServer {
        path: PathBuf,
        stop: Arc<AtomicBool>,
    }

    impl UnixServer {
        /// Bind `path` and serve `svc` until dropped. Unlinks a stale
        /// socket file first (a previous crashed daemon leaves one — the
        /// same SIGKILL'd-orphan class #128 handled for cli-status).
        pub fn bind(
            path: &Path,
            svc: Arc<dyn VerdictService>,
        ) -> Result<UnixServer, TransportError> {
            let _ = std::fs::remove_file(path); // stale-socket cleanup
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let listener = UnixListener::bind(path)?;
            listener.set_nonblocking(true)?;
            let stop = Arc::new(AtomicBool::new(false));
            let stop_t = stop.clone();
            let path_buf = path.to_path_buf();
            thread::spawn(move || {
                // Accept loop. Non-blocking + short park so `stop` is
                // observed promptly without a busy spin.
                while !stop_t.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            let svc_c = svc.clone();
                            thread::spawn(move || handle_conn(conn, svc_c));
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(std::time::Duration::from_millis(20));
                        }
                        Err(_) => break, // listener gone
                    }
                }
                let _ = std::fs::remove_file(&path_buf);
            });
            Ok(UnixServer {
                path: path.to_path_buf(),
                stop,
            })
        }

        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for UnixServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn handle_conn(conn: UnixStream, svc: Arc<dyn VerdictService>) {
        let mut reader = BufReader::new(match conn.try_clone() {
            Ok(c) => c,
            Err(_) => return,
        });
        let mut writer = conn;
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            return;
        }
        let Some(req) = Request::from_json(line.trim()) else {
            let _ = writeln!(writer, "{{\"error\":\"bad request\"}}");
            return;
        };
        if let Request::Subscribe = req {
            // Stream transitions as NDJSON until the peer disconnects
            // (write error) or the service drops the sender.
            let rx = svc.subscribe();
            for ev in rx {
                if writeln!(writer, "{}", event_to_json(&ev)).is_err() {
                    break;
                }
                if writer.flush().is_err() {
                    break;
                }
            }
            return;
        }
        let resp = dispatch_oneshot(svc.as_ref(), &req);
        let _ = writeln!(writer, "{resp}");
        let _ = writer.flush();
    }

    /// Connects to `path` for one request/response (or a subscribe
    /// stream). Cheap to construct — `cargoless status` makes one call
    /// and exits, so there is no persistent client object.
    pub struct UnixClient {
        path: PathBuf,
    }

    impl UnixClient {
        pub fn new(path: &Path) -> Self {
            Self {
                path: path.to_path_buf(),
            }
        }

        fn one_shot(&self, req: &Request) -> Result<String, TransportError> {
            let mut stream = UnixStream::connect(&self.path)?;
            writeln!(stream, "{}", req.to_json())?;
            stream.flush()?;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let line = line.trim().to_string();
            if line.is_empty() {
                return Err(TransportError::Protocol("empty response".into()));
            }
            Ok(line)
        }
    }

    impl TransportClient for UnixClient {
        fn get_status(&self, w: &str) -> Result<Option<WorktreeStatus>, TransportError> {
            let line = self.one_shot(&Request::GetStatus(w.to_string()))?;
            if line == "null" {
                return Ok(None);
            }
            Ok(status_from_json(&line))
        }

        fn get_verdict(&self, w: &str) -> Result<Option<String>, TransportError> {
            let line = self.one_shot(&Request::GetVerdict(w.to_string()))?;
            if line == "null" {
                return Ok(None);
            }
            match serde_json::from_str::<serde_json::Value>(&line) {
                Ok(serde_json::Value::String(s)) => Ok(Some(s)),
                _ => Err(TransportError::Protocol("verdict not a string".into())),
            }
        }

        fn get_diagnostics(&self, w: &str) -> Result<Vec<Diagnostic>, TransportError> {
            let line = self.one_shot(&Request::GetDiagnostics(w.to_string()))?;
            Ok(crate::diagnostics_store::deserialize(&line))
        }

        fn list_worktrees(&self) -> Result<Vec<WorktreeSummary>, TransportError> {
            let line = self.one_shot(&Request::ListWorktrees)?;
            Ok(summaries_from_json(&line))
        }

        fn subscribe(&self) -> Result<Receiver<TransitionEvent>, TransportError> {
            let stream = UnixStream::connect(&self.path)?;
            let mut w = stream.try_clone()?;
            writeln!(w, "{}", Request::Subscribe.to_json())?;
            w.flush()?;
            let (tx, rx) = channel();
            thread::spawn(move || {
                let reader = BufReader::new(stream);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Some(ev) = event_from_json(line.trim()) {
                        if tx.send(ev).is_err() {
                            break; // consumer dropped
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
            // One NDJSON request line out, one ack line back — the same
            // one-shot shape as get_status/get_verdict.
            let req = match options.filter(|o| !o.is_empty()) {
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
            };
            let line = self.one_shot(&req)?;
            pushoverlayack_from_json(&line)
                .ok_or_else(|| TransportError::Protocol("malformed push_overlay ack".into()))
        }
    }
}

#[cfg(unix)]
pub use imp::{UnixClient, UnixServer};

// ---- non-unix: typed-unsupported stubs (compile everywhere; discovery
// then falls through to file-read / spawn-local exactly as if absent) ----

// `Arc` is used only by the non-unix stub signatures (the unix path uses
// `imp`'s own `use std::sync::Arc`); cfg-scoped so it is never an unused
// import on the unix CI target.
#[cfg(not(unix))]
use std::sync::Arc;

#[cfg(not(unix))]
pub struct UnixServer;

#[cfg(not(unix))]
impl UnixServer {
    pub fn bind(
        _path: &std::path::Path,
        _svc: Arc<dyn VerdictService>,
    ) -> Result<UnixServer, TransportError> {
        Err(TransportError::Protocol(
            "unix sockets unsupported on this target".into(),
        ))
    }
}

#[cfg(not(unix))]
pub struct UnixClient;

#[cfg(not(unix))]
impl UnixClient {
    pub fn new(_path: &std::path::Path) -> Self {
        UnixClient
    }
}

#[cfg(not(unix))]
impl TransportClient for UnixClient {
    fn get_status(&self, _w: &str) -> Result<Option<WorktreeStatus>, TransportError> {
        Err(TransportError::Protocol("unix unsupported".into()))
    }
    fn get_verdict(&self, _w: &str) -> Result<Option<String>, TransportError> {
        Err(TransportError::Protocol("unix unsupported".into()))
    }
    fn get_diagnostics(&self, _w: &str) -> Result<Vec<Diagnostic>, TransportError> {
        Err(TransportError::Protocol("unix unsupported".into()))
    }
    fn list_worktrees(&self) -> Result<Vec<WorktreeSummary>, TransportError> {
        Err(TransportError::Protocol("unix unsupported".into()))
    }
    fn subscribe(&self) -> Result<Receiver<TransitionEvent>, TransportError> {
        Err(TransportError::Protocol("unix unsupported".into()))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::super::inproc::testmock::MockService;
    use super::*;

    fn tmp_sock(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cargoless-utest-{}-{}-{tag}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn server_client_roundtrip_all_oneshots() {
        let svc = Arc::new(MockService::new());
        let path = tmp_sock("rt");
        let srv = UnixServer::bind(&path, svc).expect("bind");
        // Tiny wait for the accept loop to be ready.
        std::thread::sleep(Duration::from_millis(50));
        let c = UnixClient::new(srv.path());

        assert_eq!(c.get_verdict("green-wt").unwrap(), Some("green".into()));
        assert_eq!(c.get_verdict("nope").unwrap(), None);
        let st = c.get_status("red-wt").unwrap().unwrap();
        assert_eq!(st.verdict, "red");
        assert_eq!(st.red_diagnostics, 1);
        assert!(
            st.crates.is_empty(),
            "honesty case survives the Unix wire (verdict alone)"
        );
        assert_eq!(c.get_status("nope").unwrap(), None);
        assert_eq!(c.get_diagnostics("red-wt").unwrap().len(), 1);
        assert!(c.get_diagnostics("green-wt").unwrap().is_empty());
        assert_eq!(c.list_worktrees().unwrap().len(), 2);
    }

    #[test]
    fn subscribe_streams_transitions_over_the_socket() {
        let svc = Arc::new(MockService::new());
        let path = tmp_sock("sub");
        let srv = UnixServer::bind(&path, svc.clone()).expect("bind");
        std::thread::sleep(Duration::from_millis(50));
        let c = UnixClient::new(srv.path());
        let rx = c.subscribe().unwrap();
        // Give the server thread a moment to register the subscriber.
        std::thread::sleep(Duration::from_millis(50));
        let ev = TransitionEvent {
            worktree: "red-wt".into(),
            verdict: "red".into(),
            red_diagnostics: 1,
            verdict_failure_reason: None,
            published_at: 99,
        };
        svc.emit(ev.clone());
        let got = rx.recv_timeout(Duration::from_secs(2)).expect("event");
        assert_eq!(got, ev);
    }

    #[test]
    fn client_errors_cleanly_when_no_server() {
        // Discovery relies on this: a dead/absent socket ⇒ Err, never a
        // panic, so the fallback chain proceeds.
        let c = UnixClient::new(std::path::Path::new("/nonexistent/cargoless.sock"));
        assert!(c.get_verdict("w").is_err());
    }
}
