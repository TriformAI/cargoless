//! Minimal LSP client over rust-analyzer's stdio (Epic 2 / CWDL #4, #21).
//!
//! Scope is exactly what the green/red model needs and no more:
//! `initialize`/`initialized`, `textDocument/didOpen|didChange|didSave`,
//! consuming `textDocument/publishDiagnostics`, and observing flycheck
//! progress. This is **not** a general LSP library — RA-specific, v0-shaped,
//! single workspace.
//!
//! ## #21 verdict-provenance (load-bearing for v0)
//!
//! rust-analyzer's *native* analysis is BLIND to the type/trait/method/macro
//! error class (e.g. E0599) — only `cargo check` (RA's *flycheck*) produces
//! it. So a diagnostic's authority depends on WHO produced it: RA tags
//! flycheck/cargo-check diagnostics with `source: "rustc"`, native ones with
//! `source: "rust-analyzer"`. [`PublishDiagnostics`] therefore splits error
//! counts into **authoritative** (rustc/cargo-check) vs **advisory**
//! (native). The authoritative *tree* verdict is only trustworthy at a
//! flycheck-pass boundary, so we also surface [`LspEvent::FlycheckEnded`]
//! from `$/progress`. The model gates GREEN on the authoritative tier; the
//! mapping/edge logic lives in `model`, not here.
//!
//! ## Testability
//!
//! Framing, diagnostics classification, and flycheck-end detection are pure
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
use tf_proto::{Diagnostic, Severity};

/// LSP `DiagnosticSeverity.Error`.
const SEVERITY_ERROR: i64 = 1;
/// LSP `DiagnosticSeverity.Warning`.
const SEVERITY_WARNING: i64 = 2;
/// LSP `DiagnosticSeverity.Information`.
const SEVERITY_INFO: i64 = 3;
/// LSP `DiagnosticSeverity.Hint`.
const SEVERITY_HINT: i64 = 4;

/// One `textDocument/publishDiagnostics` notification, reduced to what the
/// model cares about, split by **provenance** (#21).
///
/// The count fields (`authoritative_errors`/`advisory_errors`/`total`) are
/// the byte-frozen #21 surface the model's authoritative-vs-advisory logic
/// binds to; the `diagnostics` list is the FIELD FINDING #2 additive surface
/// the CLI uses to print actionable errors. Both are populated by the same
/// extraction so they stay consistent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishDiagnostics {
    /// The document URI exactly as RA sent it (`file://...`).
    pub uri: String,
    /// `severity == Error` diagnostics with `source == "rustc"` — produced by
    /// cargo-check/flycheck. These are AUTHORITATIVE for the verdict.
    pub authoritative_errors: usize,
    /// `severity == Error` diagnostics from any non-rustc source (chiefly
    /// `"rust-analyzer"` native). ADVISORY only — never asserts green.
    pub advisory_errors: usize,
    /// Total diagnostics (errors + warnings + hints, any source).
    pub total: usize,
    /// Full per-diagnostic detail (file/line/col/severity/code/message/source),
    /// in publish order. Additive — the model's count fields above remain the
    /// authority for green/red; this is what the CLI renders. May be empty on
    /// a "cleared" publish (RA's way of saying the file now has zero
    /// diagnostics).
    pub diagnostics: Vec<Diagnostic>,
}

impl PublishDiagnostics {
    /// Total error-severity diagnostics regardless of source.
    pub fn error_count(&self) -> usize {
        self.authoritative_errors + self.advisory_errors
    }

    /// No error of any source. (Not authority on its own — see module docs.)
    pub fn is_green(&self) -> bool {
        self.error_count() == 0
    }

    /// This file has a cargo-check (rustc) error — authoritative red.
    pub fn has_authoritative_error(&self) -> bool {
        self.authoritative_errors > 0
    }
}

/// What the reader thread streams to the model: a diagnostics notification,
/// the boundary of a completed flycheck (`cargo check`) pass, or the
/// boundary of RA's initial workspace indexing (FIELD FINDING #3a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspEvent {
    Diagnostics(PublishDiagnostics),
    /// RA reported a flycheck/`cargo check` `$/progress` `end`. The set of
    /// `source:"rustc"` diagnostics as of now is an AUTHORITATIVE snapshot.
    FlycheckEnded,
    /// FIELD FINDING #3a: RA reported `$/progress` `end` for its initial
    /// project-indexing (workspace scan / proc-macro server bring-up /
    /// dependency analysis). Before this signal, RA is publishing diagnostics
    /// from an *incomplete* model and the one-shot check loop must NOT
    /// settle-early on its quiet windows. This is what "project ready"
    /// means in LSP terms — the same signal a human IDE waits for before
    /// trusting "Go to Definition".
    IndexingEnded,
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
            if saw_any_header {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF mid-LSP-header",
                ));
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
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

/// True iff a diagnostic is `severity == Error` and produced by cargo-check
/// (`source == "rustc"`). Everything else (native `"rust-analyzer"`, etc.) is
/// advisory.
fn is_rustc_source(d: &Value) -> bool {
    d.get("source").and_then(Value::as_str) == Some("rustc")
}

/// Map an LSP `diagnostic.severity` integer to the typed [`Severity`].
/// `None` ⇒ the severity was missing or out of range; the caller folds that
/// into [`Severity::Info`] so a diagnostic is never silently dropped from
/// the CLI surface (the #21 count fields still skip unknowns).
fn severity_from_lsp(n: i64) -> Option<Severity> {
    match n {
        SEVERITY_ERROR => Some(Severity::Error),
        SEVERITY_WARNING => Some(Severity::Warning),
        SEVERITY_INFO => Some(Severity::Info),
        SEVERITY_HINT => Some(Severity::Hint),
        _ => None,
    }
}

/// Extract one [`Diagnostic`] from a single LSP `Diagnostic` JSON object
/// against a known `file_path`. Tolerant of malformed entries — anything
/// missing/unsorted is filled with a sensible default so the CLI still
/// surfaces *something* useful (the FIELD FINDING #2 contract: a red tree
/// always tells you *what*).
fn extract_one_diagnostic(d: &Value, file_path: &std::path::Path) -> Diagnostic {
    let sev_int = d.get("severity").and_then(Value::as_i64).unwrap_or(0);
    let severity = severity_from_lsp(sev_int).unwrap_or(Severity::Info);
    // LSP positions are 0-based; rustc/cargo display is 1-based — convert at
    // the boundary so every consumer sees the friendly convention.
    let lsp_line = d
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let lsp_col = d
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("character"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let line = (lsp_line as u32).saturating_add(1);
    let col = (lsp_col as u32).saturating_add(1);
    // `code` may be a string ("E0277") or a number; render either as a string.
    let code = d.get("code").and_then(|c| {
        c.as_str()
            .map(str::to_owned)
            .or_else(|| c.as_i64().map(|n| n.to_string()))
    });
    let message = d
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("(no message)")
        .to_string();
    let source = d.get("source").and_then(Value::as_str).map(str::to_owned);
    Diagnostic {
        file_path: file_path.to_path_buf(),
        line,
        col,
        severity,
        code,
        message,
        source,
    }
}

/// Pull a [`PublishDiagnostics`] out of a decoded JSON-RPC message, or `None`
/// if it is not a `textDocument/publishDiagnostics` notification. Splits
/// error counts by provenance (#21) **and** extracts the full per-diagnostic
/// detail (FIELD FINDING #2 additive surface).
pub fn extract_publish_diagnostics(v: &Value) -> Option<PublishDiagnostics> {
    if v.get("method")?.as_str()? != "textDocument/publishDiagnostics" {
        return None;
    }
    let params = v.get("params")?;
    let uri = params.get("uri")?.as_str()?.to_string();
    let diags = params.get("diagnostics")?.as_array()?;
    let mut authoritative_errors = 0usize;
    let mut advisory_errors = 0usize;
    // Pre-compute the per-publish file_path once. `path_from_uri` returns
    // `None` for non-`file:` schemes (e.g. `untitled:`); fall back to the raw
    // URI string so callers still get something stable.
    let file_path = path_from_uri(&uri)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(&uri));
    let mut rich = Vec::with_capacity(diags.len());
    for d in diags {
        if d.get("severity").and_then(Value::as_i64) == Some(SEVERITY_ERROR) {
            if is_rustc_source(d) {
                authoritative_errors += 1;
            } else {
                advisory_errors += 1;
            }
        }
        rich.push(extract_one_diagnostic(d, &file_path));
    }
    Some(PublishDiagnostics {
        uri,
        authoritative_errors,
        advisory_errors,
        total: diags.len(),
        diagnostics: rich,
    })
}

/// Common pre-check: `v` is a `$/progress` notification with `kind: "end"`.
/// Returns the lowercased (`token`, `title`) tuple for the caller's
/// generous matching, or `None` if `v` is not such a notification.
fn progress_end_token_title(v: &Value) -> Option<(String, String)> {
    if v.get("method").and_then(Value::as_str) != Some("$/progress") {
        return None;
    }
    let params = v.get("params")?;
    let value = params.get("value")?;
    if value.get("kind").and_then(Value::as_str) != Some("end") {
        return None;
    }
    let token = params
        .get("token")
        .map(|t| t.to_string())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    Some((token, title))
}

/// True iff `v` is a `$/progress` notification ending a flycheck /
/// `cargo check` pass. RA's flycheck progress carries `"check"` in its token
/// or title; matching generously on that (case-insensitive) is safe because
/// a missed end only degrades to the model's settle/timeout path — it can
/// never manufacture a false green (the model needs a *seen* end to upgrade
/// to authoritative).
pub fn extract_flycheck_end(v: &Value) -> bool {
    let Some((token, title)) = progress_end_token_title(v) else {
        return false;
    };
    // Guard: the indexing-end is also a `$/progress`/`end`; it is NOT a
    // flycheck end. Without this, RA's first indexing-end would falsely
    // trip the flycheck-end path and let the check loop upgrade to
    // Authoritative without a real cargo-check pass.
    if title.contains("indexing") || title.contains("scanning") || token.contains("indexing") {
        return false;
    }
    token.contains("check") || title.contains("check") || title.contains("flycheck")
}

/// FIELD FINDING #3a: true iff `v` is a `$/progress` notification ending RA's
/// initial workspace indexing (or the related roots-scanning / proc-macro
/// server bring-up phases that gate "the model is ready"). Generous matching
/// on the lowercased token/title so RA version bumps that rename
/// `rustAnalyzer/Indexing` → `rust-analyzer/indexing` (or similar) keep
/// working. False positives are SAFE here — the worst case is allowing the
/// check loop to settle-early as it did before the fix; false negatives are
/// the trust-broken case (cold-start false-red), so we accept slightly more
/// permissive matching than `extract_flycheck_end`.
pub fn extract_indexing_end(v: &Value) -> bool {
    let Some((token, title)) = progress_end_token_title(v) else {
        return false;
    };
    // Exclude flycheck/check ends — those are signalled separately and the
    // model treats them with very different authority.
    if title.contains("check") || token.contains("check") {
        return false;
    }
    token.contains("indexing")
        || token.contains("rootscanning")
        || token.contains("rootsscanned")
        || title.contains("indexing")
        || title.contains("roots scanned")
        || title.contains("scanning")
        || title.contains("loading")
}

/// `/abs/path` → `file:///abs/path`. v0: assumes an already-absolute,
/// space-free path; percent-encoding is a documented v1 refinement.
pub fn uri_from_path(abs_path: &str) -> String {
    if abs_path.starts_with('/') {
        format!("file://{abs_path}")
    } else {
        format!("file:///{abs_path}")
    }
}

/// Inverse of [`uri_from_path`] for the `file:` scheme; `None` for non-`file:`.
pub fn path_from_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    Some(rest.to_string())
}

// ---------------------------------------------------------------------------
// Live client
// ---------------------------------------------------------------------------

/// LSP client bound to one rust-analyzer process's stdio. Construction runs
/// the `initialize`/`initialized` handshake synchronously (enabling flycheck
/// via `checkOnSave`), then a reader thread streams [`LspEvent`]s.
pub struct LspClient {
    writer: Mutex<Box<dyn Write + Send>>,
    next_id: AtomicI64,
}

impl LspClient {
    /// Handshake against an RA speaking LSP over (`w` = its stdin, `r` = its
    /// stdout). `root_path` is the absolute workspace root. flycheck
    /// (`cargo check` on save) is enabled — it is the authoritative tier.
    pub fn initialize<W, R>(
        mut w: W,
        r: R,
        root_path: &str,
    ) -> io::Result<(Self, Receiver<LspEvent>)>
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
                // #21: flycheck IS the authoritative (cargo-check) tier.
                // Both the modern (`check.command`) and legacy
                // (`checkOnSave`) keys, since RA tolerates unknown keys and
                // versions differ across toolchains.
                "initializationOptions": {
                    "checkOnSave": true,
                    "check": { "command": "check" }
                },
                "capabilities": {
                    "window": { "workDoneProgress": true },
                    "textDocument": {
                        "publishDiagnostics": { "relatedInformation": false }
                    }
                }
            }
        });
        w.write_all(&encode_message(init.to_string().as_bytes()))?;
        w.flush()?;

        let mut br = BufReader::new(r);
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

        let (tx, rx): (Sender<LspEvent>, Receiver<LspEvent>) = channel();
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

    /// `textDocument/didChange` (full-document sync — v0 keeps it simple).
    pub fn did_change(&self, abs_path: &str, text: &str, version: i64) -> io::Result<()> {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri_from_path(abs_path), "version": version },
                "contentChanges": [ { "text": text } ]
            }),
        )
    }

    /// `textDocument/didSave` — triggers RA flycheck (`cargo check`), the
    /// authoritative tier.
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

fn reader_loop<R: BufRead>(mut br: R, tx: Sender<LspEvent>) {
    loop {
        match read_message(&mut br) {
            Ok(None) => break, // RA exited cleanly
            Err(_) => break,   // stream died / supervisor will restart RA
            Ok(Some(body)) => {
                let Ok(v) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                let ev = if let Some(pd) = extract_publish_diagnostics(&v) {
                    LspEvent::Diagnostics(pd)
                } else if extract_flycheck_end(&v) {
                    LspEvent::FlycheckEnded
                } else if extract_indexing_end(&v) {
                    LspEvent::IndexingEnded
                } else {
                    continue;
                };
                if tx.send(ev).is_err() {
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
    fn provenance_split_rustc_vs_native() {
        // E0599 (method-not-found) is rustc/cargo-check ONLY — the exact
        // class RA-native is blind to (#21). Native borrow error is advisory.
        let v: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///p/src/a.rs","diagnostics":[
                  {"severity":1,"source":"rustc","code":"E0599","message":"no method"},
                  {"severity":1,"source":"rust-analyzer","message":"native"},
                  {"severity":2,"source":"rustc","message":"warn"},
                  {"severity":1,"source":"rustc","code":"E0308","message":"mismatch"}]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&v).unwrap();
        assert_eq!(pd.uri, "file:///p/src/a.rs");
        assert_eq!(pd.authoritative_errors, 2, "two rustc errors");
        assert_eq!(pd.advisory_errors, 1, "one native error");
        assert_eq!(pd.total, 4);
        assert!(pd.has_authoritative_error());
        assert!(!pd.is_green());
        assert_eq!(pd.error_count(), 3);
        // FIELD FINDING #2 additive surface: the rich list mirrors the count
        // fields and carries everything the CLI needs to print.
        assert_eq!(pd.diagnostics.len(), 4, "rich list mirrors total");
        let codes: Vec<&str> = pd
            .diagnostics
            .iter()
            .filter_map(|d| d.code.as_deref())
            .collect();
        assert!(codes.contains(&"E0599"));
        assert!(codes.contains(&"E0308"));
        // Every diagnostic shares the publish file_path.
        for d in &pd.diagnostics {
            assert_eq!(d.file_path, std::path::PathBuf::from("/p/src/a.rs"));
        }
    }

    #[test]
    fn diagnostic_position_severity_and_code_extracted_one_based() {
        // FIELD FINDING #2: the CLI must print file:line:col + severity +
        // code + message; verify each piece round-trips and that LSP's
        // 0-based positions are converted to 1-based at the boundary.
        let v: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///r/src/lib.rs","diagnostics":[
                  {"severity":1,"source":"rustc","code":"E0277",
                   "message":"the trait bound `T: Foo` is not satisfied",
                   "range":{"start":{"line":41,"character":4},
                            "end":{"line":41,"character":11}}},
                  {"severity":2,"source":"rust-analyzer","code":"unused_imports",
                   "message":"unused import: `Bar`",
                   "range":{"start":{"line":0,"character":0},
                            "end":{"line":0,"character":11}}},
                  {"severity":3,"message":"hint-ish","source":"rustc"},
                  {"severity":4,"code":123,"message":"numeric code",
                   "range":{"start":{"line":9,"character":7},"end":{"line":9,"character":9}}}
                ]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&v).unwrap();
        assert_eq!(pd.diagnostics.len(), 4);

        let d0 = &pd.diagnostics[0];
        assert_eq!(d0.severity, Severity::Error);
        assert_eq!(d0.code.as_deref(), Some("E0277"));
        assert_eq!(d0.line, 42, "0-based LSP line 41 → 1-based 42");
        assert_eq!(d0.col, 5, "0-based LSP col 4 → 1-based 5");
        assert!(d0.message.contains("trait bound"));
        assert_eq!(d0.source.as_deref(), Some("rustc"));
        assert_eq!(d0.file_path, std::path::PathBuf::from("/r/src/lib.rs"));

        let d1 = &pd.diagnostics[1];
        assert_eq!(d1.severity, Severity::Warning);
        assert_eq!(d1.code.as_deref(), Some("unused_imports"));
        assert_eq!(d1.line, 1, "0-based line 0 → 1-based 1");
        assert_eq!(d1.col, 1);
        assert_eq!(d1.source.as_deref(), Some("rust-analyzer"));

        let d2 = &pd.diagnostics[2];
        assert_eq!(d2.severity, Severity::Info, "severity:3 → Info");
        assert_eq!(d2.line, 1, "missing range defaults to 1-based 1");
        assert_eq!(d2.col, 1);
        assert_eq!(d2.code, None);

        let d3 = &pd.diagnostics[3];
        assert_eq!(d3.severity, Severity::Hint, "severity:4 → Hint");
        assert_eq!(d3.code.as_deref(), Some("123"), "numeric code → string");
    }

    #[test]
    fn empty_publish_clears_the_rich_list_too() {
        // RA sends an empty `diagnostics: []` to "clear" a file once it goes
        // clean. The rich list must reflect that — Vec::is_empty is what the
        // model uses to drop stale per-file diagnostics.
        let v: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///r/src/lib.rs","diagnostics":[]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&v).unwrap();
        assert!(pd.diagnostics.is_empty());
        assert!(pd.is_green());
    }

    #[test]
    fn empty_diagnostics_is_green_no_authoritative() {
        let v: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///p/src/a.rs","diagnostics":[]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&v).unwrap();
        assert!(pd.is_green());
        assert!(!pd.has_authoritative_error());
        assert_eq!(pd.authoritative_errors, 0);
        assert_eq!(pd.advisory_errors, 0);
    }

    #[test]
    fn native_only_error_is_not_authoritative() {
        let v: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///x.rs","diagnostics":[
                  {"severity":1,"source":"rust-analyzer","message":"syntax"}]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&v).unwrap();
        assert!(!pd.has_authoritative_error());
        assert_eq!(pd.advisory_errors, 1);
        assert!(!pd.is_green());
    }

    // -----------------------------------------------------------------------
    // FIELD FINDING #3a — indexing-end detection + flycheck-end disambiguation
    // -----------------------------------------------------------------------

    #[test]
    fn indexing_end_detection_matches_ra_progress_tokens() {
        // The canonical RA indexing token + an `end` event.
        let v: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/Indexing",
                "value":{"kind":"end","title":"Indexing"}}}"#,
        )
        .unwrap();
        assert!(extract_indexing_end(&v), "canonical Indexing/end");

        // Roots-scanned (RA's pre-indexing phase) also gates project-ready.
        let v2: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/RootsScanned",
                "value":{"kind":"end","title":"Roots Scanned"}}}"#,
        )
        .unwrap();
        assert!(extract_indexing_end(&v2));

        // Begin/report on the same token is NOT an end.
        let v3: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/Indexing",
                "value":{"kind":"begin","title":"Indexing"}}}"#,
        )
        .unwrap();
        assert!(!extract_indexing_end(&v3));

        // A flycheck end is NOT an indexing end (they ride the same
        // `$/progress`/`end` shape; the cargo-check token must not leak).
        let v4: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/cargoCheck",
                "value":{"kind":"end","title":"cargo check"}}}"#,
        )
        .unwrap();
        assert!(!extract_indexing_end(&v4));
    }

    #[test]
    fn flycheck_end_does_not_match_indexing_end() {
        // The reverse direction: an indexing end must NOT be misread as a
        // flycheck end (the bug the #43 fix exists to prevent — RA's first
        // indexing-end firing before any real cargo-check pass would let the
        // check loop upgrade to Authoritative on incomplete data).
        let idx: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/Indexing",
                "value":{"kind":"end","title":"Indexing"}}}"#,
        )
        .unwrap();
        assert!(!extract_flycheck_end(&idx), "indexing end is NOT flycheck");
        // Roots-scanned likewise.
        let rs: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/RootsScanned",
                "value":{"kind":"end","title":"Roots Scanned"}}}"#,
        )
        .unwrap();
        assert!(!extract_flycheck_end(&rs));
    }

    #[test]
    fn flycheck_end_detection() {
        let end: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/cargoCheck",
                "value":{"kind":"end"}}}"#,
        )
        .unwrap();
        assert!(extract_flycheck_end(&end));

        let end_by_title: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"x",
                "value":{"kind":"end","title":"cargo check"}}}"#,
        )
        .unwrap();
        assert!(extract_flycheck_end(&end_by_title));

        // begin/report of the same is NOT an end
        let begin: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/cargoCheck",
                "value":{"kind":"begin","title":"cargo check"}}}"#,
        )
        .unwrap();
        assert!(!extract_flycheck_end(&begin));

        // unrelated progress (indexing) end is not a flycheck end
        let indexing: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/Indexing",
                "value":{"kind":"end","title":"Indexing"}}}"#,
        )
        .unwrap();
        assert!(!extract_flycheck_end(&indexing));

        // a publishDiagnostics is not a flycheck end
        let pd: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics","params":{"uri":"file:///a","diagnostics":[]}}"#,
        )
        .unwrap();
        assert!(!extract_flycheck_end(&pd));
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
    fn handshake_then_events_over_fakes() {
        // Scripted "RA": initialize response + a rustc diag + flycheck end.
        let mut server = encode_message(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        server.extend(encode_message(
            br#"{"method":"textDocument/publishDiagnostics","params":{"uri":"file:///r/src/lib.rs","diagnostics":[{"severity":1,"source":"rustc","code":"E0599"}]}}"#,
        ));
        server.extend(encode_message(
            br#"{"method":"$/progress","params":{"token":"rustAnalyzer/cargoCheck","value":{"kind":"end"}}}"#,
        ));
        let reader = Cursor::new(server);
        let writer: Vec<u8> = Vec::new();

        let (client, rx) = LspClient::initialize(writer, reader, "/r").expect("handshake");
        client.next_request_id();

        let e1 = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("first event");
        match e1 {
            LspEvent::Diagnostics(pd) => {
                assert_eq!(pd.uri, "file:///r/src/lib.rs");
                assert_eq!(pd.authoritative_errors, 1);
            }
            other => panic!("expected Diagnostics, got {other:?}"),
        }
        let e2 = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("second event");
        assert_eq!(e2, LspEvent::FlycheckEnded);
    }
}
