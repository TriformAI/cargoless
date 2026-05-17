//! cargoless S1 / AC#2 rust-analyzer latency harness.
//!
//! Drives a real `rust-analyzer` over LSP (JSON-RPC on stdio, hand-rolled,
//! std-only) against the committed Leptos reference fixture. For each
//! scenario it warms the daemon, then repeatedly: applies an edit that
//! introduces a known error, `didSave`s, and measures wall-clock time until
//! `textDocument/publishDiagnostics` reports the new error for that file.
//! It reports the **median** save→verdict latency per scenario and an
//! overall AC#2 (<1s) PASS/FAIL plus a D-A2 go/no-go recommendation.
//!
//! Two scenarios, chosen to straddle rust-analyzer's documented fidelity
//! cliff:
//!
//! * `trait-error`  — an unresolved-method error in plain domain code. RA
//!   detects this from its **own** analysis; expected fast path.
//! * `view!-macro-error` — an error that only exists *after* Leptos
//!   `view!` proc-macro expansion. RA's documented weak spot; the crux of
//!   the S1 / D-A2 question. A *miss* here (no diagnostic at all) is the
//!   single most important honest finding this harness can produce.
//!
//! Exit code is always 0: this is an evidence-gathering spike, not a CI
//! gate. The verdict text — not a red build — is the deliverable.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
fn env_ms(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Cfg {
    ra_bin: String,
    fixture: PathBuf,
    reps: usize,
    edit_timeout: Duration,
    warm_timeout: Duration,
    settle: Duration,
    ac2_budget_ms: u64,
}

fn main() {
    let cfg = Cfg {
        ra_bin: env_or("RA_BIN", "rust-analyzer"),
        fixture: PathBuf::from(env_or("FIXTURE_DIR", "bench/fixture")),
        reps: env_ms("REPS", 7) as usize,
        edit_timeout: Duration::from_millis(env_ms("EDIT_TIMEOUT_MS", 8000)),
        warm_timeout: Duration::from_millis(env_ms("WARM_TIMEOUT_MS", 600_000)),
        settle: Duration::from_millis(env_ms("SETTLE_MS", 2500)),
        ac2_budget_ms: env_ms("AC2_BUDGET_MS", 1000),
    };

    let fixture = match cfg.fixture.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            blocker(
                "fixture-dir-missing",
                &format!(
                    "fixture dir {:?} not found ({e}). Cannot measure.",
                    cfg.fixture
                ),
            );
            return;
        }
    };

    println!("=== cargoless S1 / AC#2 rust-analyzer latency harness ===");
    println!("fixture:        {}", fixture.display());
    println!("rust-analyzer:  {}", ra_version(&cfg.ra_bin));
    println!(
        "config:         reps={} edit_timeout={}ms warm_timeout={}ms ac2_budget={}ms",
        cfg.reps,
        cfg.edit_timeout.as_millis(),
        cfg.warm_timeout.as_millis(),
        cfg.ac2_budget_ms
    );
    println!();

    let mut ra = match spawn_ra(&cfg.ra_bin, &fixture) {
        Ok(c) => c,
        Err(e) => {
            blocker(
                "rust-analyzer-spawn-failed",
                &format!(
                    "could not spawn rust-analyzer ({e}). \
                     RA is required and must be available in CI."
                ),
            );
            return;
        }
    };

    let stdin = ra.child.stdin.take().expect("stdin");
    let mut lsp = Lsp {
        stdin,
        rx: ra.rx,
        next_id: 1,
    };

    // ---- LSP handshake -------------------------------------------------
    // Built by concatenation (not format!) so there is no brace-escaping
    // class of bug in the one message that must be perfectly valid JSON.
    let root_uri = path_to_uri(&fixture);
    let root_path = json_escape(&fixture.to_string_lossy());
    let mut init = String::new();
    init.push_str(r#"{"processId":null,"#);
    init.push_str(&format!(r#""rootUri":"{root_uri}","#));
    init.push_str(&format!(r#""rootPath":"{root_path}","#));
    init.push_str(&format!(
        r#""workspaceFolders":[{{"uri":"{root_uri}","name":"fixture"}}],"#
    ));
    init.push_str(r#""capabilities":{"window":{"workDoneProgress":true},"#);
    init.push_str(r#""textDocument":{"synchronization":{"didSave":true},"#);
    init.push_str(r#""publishDiagnostics":{"relatedInformation":false}}},"#);
    // NOTE: we deliberately do NOT advertise `workspace.configuration`.
    // A bare initialize (proven to work in a standalone LSP probe against
    // this exact RA + fixture) avoids RA pulling config and keeps the
    // session minimal; initializationOptions below are honored regardless.
    init.push_str(r#""initializationOptions":{"checkOnSave":false,"#);
    init.push_str(r#""cargo":{"buildScripts":{"enable":true},"features":["csr"]},"#);
    init.push_str(r#""procMacro":{"enable":true},"#);
    init.push_str(r#""diagnostics":{"enable":true}}}"#);
    let dbg = std::env::var("RA_DEBUG").is_ok();
    if dbg {
        eprintln!("[dbg] initialize sent ({} bytes)", init.len());
    }
    lsp.request("initialize", &init);

    // Drain until the initialize *response* (id echoed) comes back.
    let init_ok = lsp.await_response(1, Duration::from_secs(60));
    if dbg {
        eprintln!("[dbg] initialize response received = {init_ok}");
    }
    lsp.notify("initialized", "{}");

    // ---- targets -------------------------------------------------------
    let trait_file = fixture.join("src/domain/model.rs");
    let macro_file = fixture.join("src/components/metrics.rs");

    let trait_doc = Doc::open(&mut lsp, &trait_file);
    let macro_doc = Doc::open(&mut lsp, &macro_file);
    // Open a couple more so RA's crate graph / proc-macro server is fully
    // primed the way a real editing session would have it.
    let _ = Doc::open(&mut lsp, &fixture.join("src/app.rs"));
    let _ = Doc::open(&mut lsp, &fixture.join("src/components/todo.rs"));

    // ---- warm ----------------------------------------------------------
    let warm = warm_up(&mut lsp, cfg.warm_timeout, cfg.settle);
    println!(
        "warm:           {:.1}s to quiescent ({})\n",
        warm.0.as_secs_f64(),
        warm.1
    );

    // ---- scenarios -----------------------------------------------------
    let trait_res = run_scenario(
        &mut lsp,
        "trait-error (RA-native, no macro expansion)",
        Doc::clone_meta(&trait_doc),
        "self.entries.len() /* BENCH_TRAIT_ANCHOR */",
        "self.entries.len_oops() /* BENCH_TRAIT_ANCHOR */",
        &cfg,
    );

    let macro_res = run_scenario(
        &mut lsp,
        "view!-macro-error (post-Leptos-expansion)",
        Doc::clone_meta(&macro_doc),
        "count.get() /* BENCH_MACRO_ANCHOR */",
        "count.get_oops() /* BENCH_MACRO_ANCHOR */",
        &cfg,
    );

    let _ = lsp.request("shutdown", "null");
    lsp.notify("exit", "");
    let _ = ra.child.kill();

    report(&cfg, &trait_res, &macro_res);
}

// ======================================================================
// rust-analyzer process + reader thread
// ======================================================================

struct Ra {
    child: Child,
    rx: Receiver<String>,
}

fn spawn_ra(bin: &str, cwd: &Path) -> std::io::Result<Ra> {
    let mut child = Command::new(bin)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        while let Some(msg) = read_message(&mut r) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });

    // Drain stderr so RA never blocks on a full pipe. Normally discarded;
    // if $RA_STDERR_LOG is set, tee it to that file for diagnosis.
    if let Some(err) = child.stderr.take() {
        let log = std::env::var("RA_STDERR_LOG").ok();
        thread::spawn(move || {
            let mut r = BufReader::new(err);
            let mut sink = String::new();
            let _ = r.read_to_string(&mut sink);
            if let Some(path) = log {
                let _ = std::fs::write(path, sink.as_bytes());
            }
        });
    }

    Ok(Ra { child, rx })
}

fn ra_version(bin: &str) -> String {
    Command::new(bin)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "<unknown>".into())
}

fn read_message<R: BufRead>(r: &mut R) -> Option<String> {
    let mut content_len: usize = 0;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line).ok()?;
        if n == 0 {
            return None; // EOF
        }
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some(v) = t.strip_prefix("Content-Length:") {
            content_len = v.trim().parse().ok()?;
        }
    }
    if content_len == 0 {
        return Some(String::new());
    }
    let mut buf = vec![0u8; content_len];
    r.read_exact(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

// ======================================================================
// Minimal LSP client
// ======================================================================

struct Lsp {
    stdin: ChildStdin,
    rx: Receiver<String>,
    next_id: i64,
}

impl Lsp {
    fn write_frame(&mut self, payload: &str) {
        let _ = write!(
            self.stdin,
            "Content-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        let _ = self.stdin.flush();
    }

    fn notify(&mut self, method: &str, params: &str) {
        let p = if params.is_empty() { "null" } else { params };
        self.write_frame(&format!(
            r#"{{"jsonrpc":"2.0","method":"{method}","params":{p}}}"#
        ));
    }

    fn request(&mut self, method: &str, params: &str) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let p = if params.is_empty() { "null" } else { params };
        self.write_frame(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{p}}}"#
        ));
        id
    }

    /// Pull the next server message, transparently answering any
    /// server→client request so RA never stalls waiting on us.
    fn recv(&mut self, timeout: Duration) -> Option<String> {
        loop {
            match self.rx.recv_timeout(timeout) {
                Ok(raw) => {
                    if is_server_request(&raw) {
                        self.answer_server_request(&raw);
                        continue;
                    }
                    return Some(raw);
                }
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    fn answer_server_request(&mut self, raw: &str) {
        let id = extract_id(raw).unwrap_or_else(|| "null".to_string());
        if raw.contains("\"workspace/configuration\"") {
            // One result entry per requested item; nulls ⇒ "use defaults".
            let n = raw.matches("\"scopeUri\"").count().max(
                raw.matches("\"section\"")
                    .count()
                    .max(if raw.contains("\"items\"") { 1 } else { 1 }),
            );
            let arr = std::iter::repeat("null")
                .take(n.max(1))
                .collect::<Vec<_>>()
                .join(",");
            self.write_frame(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":[{arr}]}}"#
            ));
        } else {
            self.write_frame(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#));
        }
    }

    fn await_response(&mut self, id: i64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.recv(left) {
                Some(raw) => {
                    if is_response_to(&raw, id) {
                        return true;
                    }
                }
                None => return false,
            }
        }
        false
    }
}

// ======================================================================
// Documents
// ======================================================================

struct Doc {
    uri: String,
    path: PathBuf,
    clean: String,
    version: i64,
}

impl Doc {
    fn open(lsp: &mut Lsp, path: &Path) -> Doc {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {:?}: {e}", path));
        let uri = path_to_uri(path);
        lsp.notify(
            "textDocument/didOpen",
            &format!(
                r#"{{"textDocument":{{"uri":"{uri}","languageId":"rust","version":1,"text":"{t}"}}}}"#,
                t = json_escape(&text)
            ),
        );
        Doc {
            uri,
            path: path.to_path_buf(),
            clean: text,
            version: 1,
        }
    }

    fn clone_meta(d: &Doc) -> Doc {
        Doc {
            uri: d.uri.clone(),
            path: d.path.clone(),
            clean: d.clean.clone(),
            version: d.version,
        }
    }

    fn set_text(&mut self, lsp: &mut Lsp, text: &str) {
        self.version += 1;
        lsp.notify(
            "textDocument/didChange",
            &format!(
                r#"{{"textDocument":{{"uri":"{uri}","version":{v}}},
"contentChanges":[{{"text":"{t}"}}]}}"#,
                uri = self.uri,
                v = self.version,
                t = json_escape(text)
            ),
        );
        lsp.notify(
            "textDocument/didSave",
            &format!(
                r#"{{"textDocument":{{"uri":"{uri}"}},"text":"{t}"}}"#,
                uri = self.uri,
                t = json_escape(text)
            ),
        );
    }
}

// ======================================================================
// Warm-up + scenario loop
// ======================================================================

fn warm_up(lsp: &mut Lsp, max: Duration, settle: Duration) -> (Duration, &'static str) {
    let start = Instant::now();
    let deadline = start + max;
    let mut saw_index_end = false;
    let mut saw_diag = false;

    // Phase 1: a leptos workspace (≈197 crates) primes cold for ~45-60s,
    // emitting thousands of $/progress notifications. RA is "ready" when
    // it has (a) ENDed an Indexing/cachePriming/Building/Loading progress
    // AND (b) published diagnostics for an opened doc at least once
    // (empirically verified against this RA + fixture). That pair is the
    // honest warm gate — NOT global silence (RA may stay chatty), and NOT
    // a fixed sleep. Cold prime time is setup, not the AC#2 metric.
    while Instant::now() < deadline {
        match lsp.recv(deadline.saturating_duration_since(Instant::now())) {
            Some(raw) => {
                if raw.contains("\"$/progress\"")
                    && raw.contains("\"kind\":\"end\"")
                    && (raw.contains("ndex")
                        || raw.contains("rime")
                        || raw.contains("uild")
                        || raw.contains("oad"))
                {
                    saw_index_end = true;
                }
                if raw.contains("\"textDocument/publishDiagnostics\"") {
                    saw_diag = true;
                }
                if saw_index_end && saw_diag {
                    break;
                }
            }
            None => break, // RA fell silent
        }
    }

    // Phase 2: brief grace drain so any trailing post-index analysis
    // flushes before we start measuring (bounded; not the readiness gate).
    while Instant::now() < deadline {
        match lsp.recv(settle) {
            Some(_) => continue,
            None => break,
        }
    }

    let why = if saw_index_end && saw_diag {
        "indexing-end + diagnostics seen"
    } else if saw_index_end {
        "indexing-end seen (no diagnostics)"
    } else if Instant::now() >= deadline {
        "warm timeout hit"
    } else {
        "RA fell silent early"
    };
    (start.elapsed(), why)
}

struct ScenarioResult {
    name: String,
    samples: Vec<u64>, // measured latencies (ms)
    detected: usize,   // reps where a new diagnostic actually appeared
    attempted: usize,
    timeout_ms: u64,
}

impl ScenarioResult {
    fn median(&self) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut s = self.samples.clone();
        s.sort_unstable();
        Some(s[s.len() / 2])
    }
    fn fidelity(&self) -> bool {
        self.attempted > 0 && self.detected == self.attempted
    }
}

#[allow(clippy::too_many_arguments)]
fn run_scenario(
    lsp: &mut Lsp,
    name: &str,
    mut doc: Doc,
    find: &str,
    replace: &str,
    cfg: &Cfg,
) -> ScenarioResult {
    let clean = doc.clean.clone();
    assert!(
        clean.contains(find),
        "anchor {find:?} missing from {:?} — fixture drifted",
        doc.path
    );
    let broken = clean.replacen(find, replace, 1);

    // Baseline: force a fresh publish for the *clean* file and record how
    // many diagnostics RA reports for it, so the error-delta is honest
    // even if the fixture has incidental warnings.
    doc.set_text(lsp, &clean);
    let baseline = first_diag_count(lsp, &doc.uri, cfg.settle * 3).unwrap_or(0);

    let mut samples: Vec<u64> = Vec::new();
    let mut detected = 0usize;
    let mut attempted = 0usize;
    let total = cfg.reps + 1; // first rep is discarded (cold)
    let mut consecutive_miss = 0usize;

    for rep in 0..total {
        // ---- break + save, measure to first new diagnostic ----
        doc.set_text(lsp, &broken);
        let t0 = Instant::now();
        let hit = wait_for_count(lsp, &doc.uri, baseline + 1, cfg.edit_timeout);
        let elapsed_ms = t0.elapsed().as_millis() as u64;

        // ---- revert + save, let RA go green again before next rep ----
        doc.set_text(lsp, &clean);
        let _ = wait_for_count_le(lsp, &doc.uri, baseline, cfg.edit_timeout);

        if rep == 0 {
            continue; // discard cold rep
        }
        attempted += 1;
        if hit {
            detected += 1;
            samples.push(elapsed_ms);
            consecutive_miss = 0;
        } else {
            // No diagnostic within the budget: record the ceiling so the
            // median honestly reflects "you did not get a verdict".
            samples.push(cfg.edit_timeout.as_millis() as u64);
            consecutive_miss += 1;
            // Conclusive fidelity miss — stop burning CI time.
            if consecutive_miss >= 3 {
                break;
            }
        }
    }

    ScenarioResult {
        name: name.to_string(),
        samples,
        detected,
        attempted,
        timeout_ms: cfg.edit_timeout.as_millis() as u64,
    }
}

/// First publishDiagnostics count seen for `uri` within `timeout`, if any.
fn first_diag_count(lsp: &mut Lsp, uri: &str, timeout: Duration) -> Option<usize> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match lsp.recv(deadline.saturating_duration_since(Instant::now())) {
            Some(raw) => {
                if is_diag_for(&raw, uri) {
                    return Some(diag_count(&raw));
                }
            }
            None => return None,
        }
    }
    None
}

fn wait_for_count(lsp: &mut Lsp, uri: &str, want_at_least: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match lsp.recv(deadline.saturating_duration_since(Instant::now())) {
            Some(raw) => {
                if is_diag_for(&raw, uri) && diag_count(&raw) >= want_at_least {
                    return true;
                }
            }
            None => return false,
        }
    }
    false
}

fn wait_for_count_le(lsp: &mut Lsp, uri: &str, want_at_most: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match lsp.recv(deadline.saturating_duration_since(Instant::now())) {
            Some(raw) => {
                if is_diag_for(&raw, uri) && diag_count(&raw) <= want_at_most {
                    return true;
                }
            }
            None => return false,
        }
    }
    false
}

// ======================================================================
// Message classification (substring-based; std-only, no JSON dep)
// ======================================================================

fn is_diag_for(raw: &str, uri: &str) -> bool {
    raw.contains("\"textDocument/publishDiagnostics\"") && raw.contains(uri)
}

/// Count diagnostics in a publishDiagnostics payload. RA emits one
/// `"severity":` per diagnostic; `"diagnostics":[]` ⇒ zero.
fn diag_count(raw: &str) -> usize {
    if raw.contains("\"diagnostics\":[]") {
        return 0;
    }
    raw.matches("\"severity\"").count()
}

// JSON-RPC discrimination. A RESPONSE carries `result`/`error`; a server→
// client REQUEST carries `method` + `id` and NO result/error. RA's
// initialize *result* embeds capability strings that can contain the
// substring "method" (e.g. command names), so keying "is this a request?"
// purely on a "method" substring mis-fired and made the harness reply a
// bogus Response to RA's initialize result — RA then aborted with
// `expected initialized notification, got: Response`. The result/error
// guard is the fix: a message with result/error is NEVER a request.
fn is_response(raw: &str) -> bool {
    (raw.contains("\"result\"") || raw.contains("\"error\"")) && has_top_level_id(raw)
}

fn is_server_request(raw: &str) -> bool {
    raw.contains("\"method\"") && has_top_level_id(raw) && !is_response(raw)
}

fn is_response_to(raw: &str, id: i64) -> bool {
    is_response(raw) && extract_id(raw).as_deref() == Some(&id.to_string())
}

fn has_top_level_id(raw: &str) -> bool {
    extract_id(raw).is_some()
}

/// Pull the first `"id":` value (number or quoted string). RA's compact
/// JSON puts id right after the envelope, so first-match is reliable here.
fn extract_id(raw: &str) -> Option<String> {
    let i = raw.find("\"id\"")?;
    let after = &raw[i + 4..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else {
        let end = rest
            .find(|c: char| c == ',' || c == '}')
            .unwrap_or(rest.len());
        let v = rest[..end].trim();
        if v.is_empty() {
            None
        } else {
            Some(v.to_string())
        }
    }
}

// ======================================================================
// JSON / URI helpers
// ======================================================================

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn path_to_uri(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::from("file://");
    for b in s.bytes() {
        match b {
            b'/' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ======================================================================
// Reporting + verdict
// ======================================================================

fn line(res: &ScenarioResult) {
    println!("scenario: {}", res.name);
    let detected = format!("{}/{}", res.detected, res.attempted.max(1));
    match res.median() {
        Some(m) if res.fidelity() => {
            println!("  diagnostic reported: yes ({detected})");
            println!("  samples ms: {:?}", res.samples);
            println!("  median: {m} ms");
        }
        Some(m) => {
            println!(
                "  diagnostic reported: PARTIAL/NONE ({detected}) — \
                 misses recorded at the {}ms ceiling",
                res.timeout_ms
            );
            println!("  samples ms: {:?}", res.samples);
            println!("  median: {m} ms  <-- NOT a real verdict latency");
        }
        None => println!("  diagnostic reported: NONE — no samples"),
    }
    println!();
}

fn report(cfg: &Cfg, t: &ScenarioResult, m: &ScenarioResult) {
    println!("---- results ----");
    line(t);
    line(m);

    let budget = cfg.ac2_budget_ms;
    let tv = verdict(t, budget);
    let mv = verdict(m, budget);

    println!("AC#2 budget: median save->publishDiagnostics < {budget} ms");
    println!("  {:<42} {}", "trait-error:", tv.0);
    println!("  {:<42} {}", "view!-macro-error:", mv.0);
    println!();

    println!("==== D-A2 GO/NO-GO ====");
    let go = tv.1 && mv.1;
    if go {
        println!("VERDICT: GO — RA-native diagnostics meet AC#2 (<{budget}ms median)");
        println!("for BOTH plain-Rust and post-`view!`-expansion errors on a");
        println!("realistically-sized Leptos project. AC#2 wording stands.");
    } else {
        println!("VERDICT: NO-GO for the blanket AC#2 wording.");
        println!();
        println!("Finding: rust-analyzer native diagnostics are {} for", tv.2);
        println!(
            "RA-native (plain trait/type) errors, but {} for errors",
            mv.2
        );
        println!("that only exist after Leptos `view!` proc-macro expansion —");
        println!("exactly the cliff S1 (CWDL-22) was raised to de-risk.");
        println!();
        println!("Recommended D-A2 renegotiation of AC#2:");
        println!("  \"Median save->verdict < {budget}ms for RA-native-detectable");
        println!("   errors. Errors requiring full macro/cargo-check fidelity");
        println!("   (incl. Leptos `view!`) fall back to debounced flycheck:");
        println!("   slower, but correct — with an honest, separately-reported");
        println!("   latency, never silently folded into the <1s claim.\"");
        println!();
        println!("This is the S1 gate input for Sprint 2 / Epic 2's state model:");
        println!("the green/red model MUST distinguish fast-native verdicts from");
        println!("slow-but-authoritative flycheck verdicts.");
    }
    println!();
    println!("(harness exit 0 by design — evidence, not a CI gate)");

    // MUST be the final stdout line: run.sh / the lead lift this single
    // line into a Forgejo commit status (the only S1 channel readable
    // via API on this build). <=200 chars, prefix `S1_VERDICT:`.
    println!("{}", s1_verdict_line(cfg, t, m));
}

/// The one machine-parseable line. Self-contained, grep-able as
/// `^S1_VERDICT:`, <=200 chars.
fn s1_verdict_line(cfg: &Cfg, t: &ScenarioResult, m: &ScenarioResult) -> String {
    let b = cfg.ac2_budget_ms;
    let tv = verdict(t, b);
    let mv = verdict(m, b);

    let trait_part = match t.median() {
        Some(ms) => format!("trait_err={ms}ms:{}", if tv.1 { "PASS" } else { "FAIL" }),
        None => "trait_err=NA:FAIL".to_string(),
    };
    let view_part = if !m.fidelity() {
        "view_macro=MISS".to_string()
    } else {
        match m.median() {
            Some(ms) => format!("view_macro={ms}ms"),
            None => "view_macro=MISS".to_string(),
        }
    };
    let pass = tv.1 && mv.1;
    let ac2 = if pass { "PASS" } else { "FAIL" };
    let da2 = if pass { "GO" } else { "NO-GO" };
    let reword = if pass {
        "AC#2 stands: median save->verdict <1s on the reference project"
    } else {
        "<1s median for RA-native errors; view!/macro errors use slower-but-correct flycheck"
    };
    format!("S1_VERDICT: {trait_part} {view_part} AC2={ac2} D-A2={da2} reword=\"{reword}\"")
}

/// -> (status string, meets_budget, adjective)
fn verdict(r: &ScenarioResult, budget: u64) -> (String, bool, &'static str) {
    match r.median() {
        None => (
            "NO DATA (RA produced no diagnostics)".into(),
            false,
            "unusable",
        ),
        Some(m) => {
            if !r.fidelity() {
                (
                    format!(
                        "FAIL — fidelity gap: only {}/{} edits ever got a \
                         diagnostic (median {}ms is a timeout floor)",
                        r.detected,
                        r.attempted.max(1),
                        m
                    ),
                    false,
                    "unreliable",
                )
            } else if m < budget {
                (format!("PASS — median {m}ms < {budget}ms"), true, "fast")
            } else {
                (
                    format!("FAIL — median {m}ms >= {budget}ms"),
                    false,
                    "too slow",
                )
            }
        }
    }
}

fn blocker(token: &str, msg: &str) {
    println!("=== cargoless S1 / AC#2 latency harness ===");
    println!();
    println!("BLOCKER: {msg}");
    println!();
    println!("D-A2 GO/NO-GO: BLOCKED — the spike could not run, so AC#2's");
    println!("sub-1s wording remains UNPROVEN. This must be resolved before");
    println!("Sprint 2 (it is the S1 gate, Plane CWDL-22).");
    println!();
    println!("(harness exit 0 by design — evidence, not a CI gate)");
    // Final stdout line — see s1_verdict_line.
    println!("S1_VERDICT: BLOCKER={token} AC2=UNKNOWN D-A2=BLOCKED");
}
