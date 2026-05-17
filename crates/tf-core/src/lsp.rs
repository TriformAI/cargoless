//! Minimal LSP client over rust-analyzer's stdio (Epic 2 / CWDL #4).
//!
//! Scope is exactly what the green/red model needs and no more:
//! `initialize`/`initialized`, `textDocument/didOpen|didChange|didSave`, and
//! consuming `textDocument/publishDiagnostics`. This is **not** a general LSP
//! library — RA-specific, v0-shaped, single workspace.
//!
//! ## Layering
//!
//! This module is pure transport + protocol: it turns RA's diagnostics into a
//! transport-level [`PublishDiagnostics`] (`uri`, error/total counts). The
//! mapping to `tf_proto::FileState` and the green/red edge logic live in
//! `model` — `lsp` deliberately does not depend on `tf-proto`, so the protocol
//! seam and the verdict seam can change independently.
//!
//! ## Testability
//!
//! Framing (`Content-Length` codec) and diagnostics extraction are pure
//! functions unit-tested over in-memory buffers — the CI `test` job (no
//! rust-analyzer in the image) exercises every parsing branch. The live
//! [`LspClient`] is generic over `Read`/`Write`, so the handshake is testable
//! against a scripted fake server too.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};

use serde_json::{Value, json};

/// LSP `DiagnosticSeverity.Error`.
const SEVERITY_ERROR: i64 = 1;

/// One `textDocument/publishDiagnostics` notification, reduced to what the
/// model cares about: which document, and whether it has compile errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishDiagnostics {
    /// The document URI exactly as RA sent it (`file://...`).
    pub uri: String,
    /// Number of `severity == Error` diagnostics.
    pub error_count: usize,
    /// Total diagnostics (errors + warnings + hints).
    pub total: usize,
}

impl PublishDiagnostics {
    /// File is green iff RA reported zero error-severity diagnostics for it.
    pub fn is_green(&self) -> bool {
        self.error_count == 0
    }
}

// ---------------------------------------------------------------------------
// Wire framing (pure)
// ---------------------------------------------------------------------------

/// Frame a JSON body with the LSP `Content-Length` header. Length is in
/// **bytes** (UTF-8), per the spec.
pub fn encode_message(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

/// Read exactly one LSP message body, or `Ok(None)` at clean EOF (the stream
/// ended on a frame boundary — RA exited). Malformed framing is an error.
pub fn read_message<R: BufRead>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut content_len: Option<usize> = None;
    let mut saw_any_header = false;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            // EOF. Clean iff it happened before any header of a new message.
            if saw_any_header {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF mid-LSP-header",
                ));
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if trimmed.is_empty() {
            break; // end of headers
        }
        saw_any_header = true;
        if let Some(v) = trimmed.strip_prefix("Content-Length:") {
            content_len = v.trim().parse::<usize>().ok();
        }
    }
    let len = content_len.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing/invalid Content-Length")
    })?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Pull a [`PublishDiagnostics`] out of a decoded JSON-RPC message, or `None`
/// if it is not a `textDocument/publishDiagnostics` notification.
pub fn extract_publish_diagnostics(v: &Value) -> Option<PublishDiagnostics> {
    if v.get("method")?.as_str()? != "textDocument/publishDiagnostics" {
        return None;
    }
    let params = v.get("params")?;
    let uri = params.get("uri")?.as_str()?.to_string();
    let diags = params.get("diagnostics")?.as_array()?;
    let error_count = diags
        .iter()
        .filter(|d| d.get("severity").and_then(Value::as_i64) == Some(SEVERITY_ERROR))
        .count();
    Some(PublishDiagnostics {
        uri,
        error_count,
        total: diags.len(),
    })
}

/// `/abs/path` → `file:///abs/path`. v0: assumes an already-absolute,
/// space-free path (cargoless watches a project root); percent-encoding is a
/// documented v1 refinement, not a contract change.
pub fn uri_from_path(abs_path: &str) -> String {
    if abs_path.starts_with('/') {
        format!("file://{abs_path}")
    } else {
        format!("file:///{abs_path}")
    }
}

/// Inverse of [`uri_from_path`] for the `file:` scheme; returns the URI
/// unchanged-stripped path or `None` for a non-`file:` URI.
pub fn path_from_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    Some(rest.to_string())
}

// ---------------------------------------------------------------------------
// Live client
// ---------------------------------------------------------------------------

/// LSP client bound to one rust-analyzer process's stdio. Construction runs
/// the `initialize`/`initialized` handshake synchronously, then a reader
/// thread streams [`PublishDiagnostics`] on the returned channel.
pub struct LspClient {
    writer: Mutex<Box<dyn Write + Send>>,
    next_id: AtomicI64,
}

impl LspClient {
    /// Handshake against an RA speaking LSP over (`w` = its stdin, `r` = its
    /// stdout). `root_path` is the absolute workspace root.
    pub fn initialize<W, R>(
        mut w: W,
        r: R,
        root_path: &str,
    ) -> io::Result<(Self, Receiver<PublishDiagnostics>)>
    where
        W: Write + Send + 'static,
        R: Read + Send + 'static,
    {
        let root_uri = uri_from_path(root_path);
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": { "relatedInformation": false }
                    }
                }
            }
        });
        w.write_all(&encode_message(init.to_string().as_bytes()))?;
        w.flush()?;

        let mut br = BufReader::new(r);
        // Drain until the initialize *response* (id == 1). RA may interleave
        // window/logMessage notifications before it; skip those.
        loop {
            match read_message(&mut br)? {
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "RA exited during initialize handshake",
                    ));
                }
                Some(body) => {
                    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
                        continue;
                    };
                    if v.get("id").and_then(Value::as_i64) == Some(1) && v.get("method").is_none() {
                        break;
                    }
                }
            }
        }

        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        w.write_all(&encode_message(initialized.to_string().as_bytes()))?;
        w.flush()?;

        let (tx, rx): (Sender<PublishDiagnostics>, Receiver<PublishDiagnostics>) = channel();
        // Detached: the reader ends on RA-stdout EOF (which the analyzer
        // Supervisor causes on restart), so there is no handle to join.
        let _reader: JoinHandle<()> = thread::Builder::new()
            .name("tf-lsp-reader".into())
            .spawn(move || reader_loop(br, tx))
            .expect("spawn tf-lsp-reader thread");

        Ok((
            Self {
                writer: Mutex::new(Box::new(w)),
                next_id: AtomicI64::new(2),
            },
            rx,
        ))
    }

    fn notify(&self, method: &str, params: Value) -> io::Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let bytes = encode_message(msg.to_string().as_bytes());
        let mut w = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.write_all(&bytes)?;
        w.flush()
    }

    /// `textDocument/didOpen`.
    pub fn did_open(&self, abs_path: &str, text: &str, version: i64) -> io::Result<()> {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri_from_path(abs_path),
                    "languageId": "rust",
                    "version": version,
                    "text": text
                }
            }),
        )
    }

    /// `textDocument/didChange` (full-document sync — v0 keeps it simple and
    /// correct rather than incremental).
    pub fn did_change(&self, abs_path: &str, text: &str, version: i64) -> io::Result<()> {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri_from_path(abs_path), "version": version },
                "contentChanges": [ { "text": text } ]
            }),
        )
    }

    /// `textDocument/didSave`.
    pub fn did_save(&self, abs_path: &str) -> io::Result<()> {
        self.notify(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": uri_from_path(abs_path) } }),
        )
    }

    /// Monotonic LSP id for any future request-style call.
    pub fn next_request_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }
}

fn reader_loop<R: BufRead>(mut br: R, tx: Sender<PublishDiagnostics>) {
    loop {
        match read_message(&mut br) {
            Ok(None) => break, // RA exited cleanly
            Err(_) => break,   // stream died / supervisor will restart RA
            Ok(Some(body)) => {
                let Ok(v) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                let Some(pd) = extract_publish_diagnostics(&v) else {
                    continue;
                };
                if tx.send(pd).is_err() {
                    break; // model gone
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn encode_then_read_roundtrips() {
        let body = br#"{"jsonrpc":"2.0","method":"x"}"#;
        let framed = encode_message(body);
        let mut cur = Cursor::new(framed);
        let got = read_message(&mut cur).unwrap().unwrap();
        assert_eq!(got, body);
        // next read is clean EOF
        assert!(read_message(&mut cur).unwrap().is_none());
    }

    #[test]
    fn read_handles_back_to_back_messages() {
        let mut stream = encode_message(b"AAAA");
        stream.extend(encode_message(b"BB"));
        let mut cur = Cursor::new(stream);
        assert_eq!(read_message(&mut cur).unwrap().unwrap(), b"AAAA");
        assert_eq!(read_message(&mut cur).unwrap().unwrap(), b"BB");
        assert!(read_message(&mut cur).unwrap().is_none());
    }

    #[test]
    fn missing_content_length_is_error() {
        let mut cur = Cursor::new(b"X-Foo: 1\r\n\r\n".to_vec());
        assert!(read_message(&mut cur).is_err());
    }

    #[test]
    fn extract_diagnostics_counts_errors_only() {
        let v: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///p/src/a.rs","diagnostics":[
                  {"severity":1,"message":"E0382"},
                  {"severity":2,"message":"unused"},
                  {"severity":1,"message":"E0277"}]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&v).unwrap();
        assert_eq!(pd.uri, "file:///p/src/a.rs");
        assert_eq!(pd.error_count, 2);
        assert_eq!(pd.total, 3);
        assert!(!pd.is_green());
    }

    #[test]
    fn extract_diagnostics_empty_is_green() {
        let v: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///p/src/a.rs","diagnostics":[]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&v).unwrap();
        assert_eq!(pd.error_count, 0);
        assert!(pd.is_green());
    }

    #[test]
    fn non_diagnostics_message_is_ignored() {
        let v: Value =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#)
                .unwrap();
        assert!(extract_publish_diagnostics(&v).is_none());
        let n: Value =
            serde_json::from_str(r#"{"method":"window/logMessage","params":{}}"#).unwrap();
        assert!(extract_publish_diagnostics(&n).is_none());
    }

    #[test]
    fn uri_path_roundtrip() {
        assert_eq!(uri_from_path("/abs/x.rs"), "file:///abs/x.rs");
        assert_eq!(
            path_from_uri("file:///abs/x.rs").as_deref(),
            Some("/abs/x.rs")
        );
        assert!(path_from_uri("http://x").is_none());
    }

    #[test]
    fn handshake_then_diagnostics_over_fakes() {
        // Scripted "RA": initialize response (id 1) + one publishDiagnostics.
        let mut server = encode_message(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        server.extend(encode_message(
            br#"{"method":"textDocument/publishDiagnostics","params":{"uri":"file:///r/src/lib.rs","diagnostics":[{"severity":1}]}}"#,
        ));
        let reader = Cursor::new(server);
        let writer: Vec<u8> = Vec::new();

        let (client, rx) = LspClient::initialize(writer, reader, "/r").expect("handshake");
        client.next_request_id(); // 2 -> 3, just exercising the counter

        let pd = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("diagnostics delivered");
        assert_eq!(pd.uri, "file:///r/src/lib.rs");
        assert_eq!(pd.error_count, 1);
    }
}
