//! `cargoless verdict` — **A1**: the one-shot merge-gate verdict client
//! (push → await → attributed verdict → machine-readable output).
//!
//! The 0.4 wedge that lets gate wrappers collapse from ~1,700 lines of
//! shard-selection/retry/parsing bash to a single binary call:
//!
//! ```text
//! cargoless verdict \
//!   --output json \
//!   --header "X-Cargoless-Routing-Key: $routing_key" \
//!   --remote http://cargoless-pool.svc:8787 \
//!   -- "$repo"
//! ```
//!
//! What the subcommand owns (so caller bash does not):
//!
//! * **Routing headers (C1):** `--header` values ride EVERY request —
//!   the push and all status polls — because the pool ingress
//!   consistent-hashes `X-Cargoless-Routing-Key`; a poll that dropped
//!   the header would hash to a different shard than the push it is
//!   awaiting. Injection is client-wide by construction
//!   (`HttpClient::with_header`).
//! * **Failover ladder:** repeatable `--remote`, tried in order. A
//!   remote is skipped on transport failure, on `401`, or when it
//!   rejects the push (quiescing drain / payload guard); the await then
//!   stays PINNED to the remote that accepted — shards are
//!   verdict-equivalent for pushes, but only the accepting daemon owes
//!   us a fresh verdict for this overlay.
//! * **Verdict attribution (A2 consumer):** the await accepts a status
//!   echoing `base_sha == our resolved --base SHA`. Equal SHAs accept
//!   even a pre-push publication (idempotent re-run fast-path: same
//!   key + same SHA ⇒ same overlay content ⇒ same verdict). A status
//!   carrying no `base_sha` (older daemon, fs-watch verdict) falls back
//!   to the freshness guard; a MISMATCHED SHA never matches — that is
//!   another branch's verdict on a shared key.
//! * **Witness check-ids (B3 surface):** `--check-id` values travel as
//!   `PushOverlayOptions::check_ids` on the wire. Today's daemons
//!   store-and-ignore them; per-check witness selection consumes them
//!   server-side when B3 lands.
//! * **EX_TEMPFAIL honesty:** exit 0 = green, 1 = red, **75** = the
//!   infrastructure could not produce a verdict (await timeout, ladder
//!   exhausted, daemon said `unknown`) — callers escalate instead of
//!   treating infra trouble as a code red. 2 = setup/config error
//!   (bad flags, unauthorized everywhere, oversized payload).
//!
//! **Trivial-green short-circuit:** when the diff vs `--base` carries
//! no content-bearing files (empty diff, or excluded/metadata-only
//! paths only) the daemon has nothing to evaluate beyond the
//! already-gated base — the verdict is `green` with
//! `"source":"client"` + `"trivial_reason"` so consumers can tell it
//! apart from a daemon verdict. This mirrors the incumbent gate
//! workflow's own no-Rust-relevant-changes success arm.
//!
//! **Output contract** (`--output json`, the default): exactly one JSON
//! object on stdout — the `WorktreeStatus` wire shape plus two additive
//! keys: `remote` (which ladder entry answered) and `source`
//! (`"daemon"` = echoed from a daemon publication; `"client"` =
//! synthesized here: trivial green, ladder exhausted, await timeout).
//! All human diagnostics stay on stderr. `--output text` prints just
//! the verdict word (`green`/`red`/`unknown`) for `$(...)` capture.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use cargoless_core::transport::http::HttpClient;
use cargoless_core::transport::{
    PushOverlayOptions, TransportClient, TransportError, WorktreeStatus, status_to_json,
};

use crate::push::{
    AwaitFreshness, build_push_payload, emit_payload_diagnostics, git_changed_files,
    git_resolve_ref, push_overlay_request_body, validate_overlay_http_cap,
};

/// `--output` mode. JSON is the default: the subcommand exists for
/// machine consumers (gate workflows, thin wrappers); humans get the
/// stderr narration either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Json,
    Text,
}

impl OutputMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "json" => Some(OutputMode::Json),
            "text" => Some(OutputMode::Text),
            _ => None,
        }
    }
}

/// CLI-resolved verdict parameters (see module doc for the contract
/// each field serves).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictOpts {
    /// Failover ladder, tried in order; first entry is the primary.
    pub remotes: Vec<String>,
    /// Raw `--header "Name: value"` strings; parsed + validated before
    /// any network I/O (then re-validated by `with_header`).
    pub headers: Vec<String>,
    pub output: OutputMode,
    pub auth_token: Option<String>,
    pub repo: PathBuf,
    pub worktree: String,
    pub base: String,
    pub server_root: Option<PathBuf>,
    /// Witness-gated (Hard) verdict for this push.
    pub gate: bool,
    /// B3: requested witness check-ids (wire-attached, server-consumed
    /// when per-check gating lands).
    pub check_ids: Vec<String>,
    pub await_timeout_secs: u64,
}

/// `cargoless verdict` entry. Exit codes: 0 green, 1 red, 75 unknown /
/// infra-degraded (EX_TEMPFAIL), 2 setup error.
pub fn run(opts: &VerdictOpts) -> ExitCode {
    if opts.remotes.is_empty() {
        crate::ui::error(
            "verdict: --remote <url> is required (repeat the flag for a failover ladder)",
        );
        return ExitCode::from(2);
    }
    let headers = match parse_headers(&opts.headers) {
        Ok(headers) => headers,
        Err(message) => {
            crate::ui::error(message);
            return ExitCode::from(2);
        }
    };

    // 1. Enumerate + package the diff exactly like `push` (same payload
    //    discipline, same C6 canonical ordering, same 32MiB preflight).
    let changed = match git_changed_files(&opts.repo, &opts.base) {
        Ok(files) => files,
        Err(e) => {
            crate::ui::error(format!(
                "verdict: git diff against `{}` in `{}` failed: {e}",
                opts.base,
                opts.repo.display()
            ));
            return ExitCode::from(2);
        }
    };
    let repo_relative = opts.server_root.is_some();
    let mut payload = match build_push_payload(&opts.repo, &changed, repo_relative) {
        Ok(payload) => payload,
        Err(e) => {
            crate::ui::error(e.to_string());
            return ExitCode::from(2);
        }
    };
    payload.files.sort_by(|a, b| a.0.cmp(&b.0));

    let resolved_sha = git_resolve_ref(&opts.repo, &opts.base).ok();
    if resolved_sha.is_none() {
        crate::ui::warn(format!(
            "verdict: could not resolve `{}` to a commit SHA; verdict attribution \
             falls back to freshness-only",
            opts.base
        ));
    }

    // 2. Trivial-green short-circuit (module doc): nothing content-bearing
    //    to push ⇒ nothing the daemon could evaluate beyond the gated base.
    if payload.files.is_empty() {
        let detail = if changed.is_empty() {
            format!("empty diff vs {}", opts.base)
        } else {
            format!(
                "{} changed path(s) vs {} are all excluded or metadata-only — \
                 no content-bearing files to evaluate",
                changed.len(),
                opts.base
            )
        };
        return emit_client_verdict(opts, "green", &detail, None, resolved_sha.as_deref(), 0);
    }

    // 3. Wire options: gate + check-ids + attribution SHA (+ central-mode
    //    mapping when --server-root is set). base_sha is attached even in
    //    absolute-path mode — it costs one wire key and buys the A2
    //    attributed-await below.
    let mut options = PushOverlayOptions {
        changed_files: if payload.trigger_paths.is_empty() {
            None
        } else {
            Some(payload.trigger_paths.clone())
        },
        gate: opts.gate,
        check_ids: if opts.check_ids.is_empty() {
            None
        } else {
            Some(opts.check_ids.clone())
        },
        base_sha: resolved_sha.clone(),
        ..PushOverlayOptions::default()
    };
    if let Some(root) = opts.server_root.as_ref() {
        options.repo_relative = true;
        options.analysis_root = Some(root.to_string_lossy().into_owned());
    }
    let options = if options.is_empty() {
        None
    } else {
        Some(options)
    };
    let body = push_overlay_request_body(
        &opts.worktree,
        &opts.base,
        &payload.files,
        None,
        options.as_ref(),
    );
    emit_payload_diagnostics(&changed, &payload, body.len());
    if let Err(message) = validate_overlay_http_cap(&body, &payload.content_stats) {
        crate::ui::error(message);
        return ExitCode::from(2);
    }

    // 4. Build one client per ladder entry. Header/token validation
    //    failures are config errors (exit 2), not failover events — a
    //    malformed header would be malformed at every remote.
    let mut endpoints: Vec<(String, HttpClient)> = Vec::with_capacity(opts.remotes.len());
    for remote in &opts.remotes {
        match build_client(remote, opts.auth_token.as_deref(), &headers) {
            Ok(client) => endpoints.push((remote.clone(), client)),
            Err(e) => {
                crate::ui::error(format!("verdict: client init failed for `{remote}`: {e}"));
                return ExitCode::from(2);
            }
        }
    }

    // 5. Push down the ladder; pin the await to the accepting remote.
    let accepted = match push_with_failover(
        &endpoints,
        &opts.worktree,
        &opts.base,
        &payload.files,
        options.as_ref(),
    ) {
        Ok(accepted) => accepted,
        Err(exhausted) => {
            let reason = format!(
                "no remote accepted the push — {}",
                exhausted.describe_attempts()
            );
            // Unauthorized everywhere is a config problem (one shared
            // token), not transient infra: exit 2 so callers fix setup
            // instead of retrying.
            let exit = if exhausted.all_unauthorized() { 2 } else { 75 };
            return emit_client_verdict(
                opts,
                "unknown",
                &reason,
                None,
                resolved_sha.as_deref(),
                exit,
            );
        }
    };
    eprintln!(
        "[cargoless:verdict] push accepted by {} (applied_files={}); awaiting attributed verdict (timeout {}s)",
        accepted.remote, accepted.applied_files, opts.await_timeout_secs
    );

    // 6. Await the attributed verdict on the SAME client (routing-key
    //    affinity: polls must hash to the shard that took the push).
    match await_attributed_verdict(
        accepted.client,
        &opts.worktree,
        resolved_sha.as_deref(),
        accepted.freshness,
        opts.await_timeout_secs,
    ) {
        Some(status) => emit_daemon_verdict(opts, &status, accepted.remote),
        None => {
            let reason = format!(
                "timed out after {}s awaiting an attributed verdict from {} for {}",
                opts.await_timeout_secs, accepted.remote, opts.worktree
            );
            emit_client_verdict(
                opts,
                "unknown",
                &reason,
                Some(accepted.remote),
                resolved_sha.as_deref(),
                75,
            )
        }
    }
}

/// Parse raw `--header` strings into `(name, value)` pairs. Split on the
/// FIRST `:` only — header values legitimately contain colons (URLs).
/// Deep validation (token chars, reserved names, CRLF) is
/// `HttpClient::with_header`'s job; this is the shape check.
fn parse_headers(raw: &[String]) -> Result<Vec<(String, String)>, String> {
    raw.iter()
        .map(|header| {
            let (name, value) = header.split_once(':').ok_or_else(|| {
                format!("verdict: --header `{header}` is not of the form `Name: value`")
            })?;
            let name = name.trim();
            if name.is_empty() {
                return Err(format!("verdict: --header `{header}` has an empty name"));
            }
            Ok((name.to_string(), value.trim().to_string()))
        })
        .collect()
}

fn build_client(
    remote: &str,
    token: Option<&str>,
    headers: &[(String, String)],
) -> Result<HttpClient, TransportError> {
    let mut client = match token.map(str::trim).filter(|t| !t.is_empty()) {
        Some(token) => HttpClient::with_token(remote, token)?,
        None => HttpClient::new(remote)?,
    };
    for (name, value) in headers {
        client = client.with_header(name.clone(), value.clone())?;
    }
    Ok(client)
}

/// A push the ladder landed: which remote took it, the client pinned to
/// that remote for the await, and the freshness guard captured BEFORE
/// the push (so a pre-existing stale publication cannot satisfy the
/// freshness arm of the acceptance predicate).
struct AcceptedPush<'a, C> {
    remote: &'a str,
    client: &'a C,
    applied_files: u32,
    freshness: AwaitFreshness,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttemptFailure {
    Transport(String),
    Unauthorized,
    /// `accepted: false` ack. The ack wire shape carries no reason; the
    /// daemon's stderr has it (quiescing drain or a payload guard).
    Rejected,
}

impl AttemptFailure {
    fn describe(&self) -> String {
        match self {
            AttemptFailure::Transport(e) => format!("transport error: {e}"),
            AttemptFailure::Unauthorized => "unauthorized (401)".to_string(),
            AttemptFailure::Rejected => {
                "push rejected (quiescing daemon or payload guard)".to_string()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LadderExhausted {
    attempts: Vec<(String, AttemptFailure)>,
}

impl LadderExhausted {
    fn all_unauthorized(&self) -> bool {
        !self.attempts.is_empty()
            && self
                .attempts
                .iter()
                .all(|(_, failure)| *failure == AttemptFailure::Unauthorized)
    }

    fn describe_attempts(&self) -> String {
        self.attempts
            .iter()
            .map(|(remote, failure)| format!("{remote}: {}", failure.describe()))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Try each ladder entry in order until one ACCEPTS the push. Per-entry
/// freshness is captured from a pre-push status poll on that same
/// entry; a failed pre-poll degrades to push-time freshness (warn, not
/// failover — the push itself is the authoritative liveness probe).
fn push_with_failover<'a, C: TransportClient>(
    endpoints: &'a [(String, C)],
    worktree: &str,
    base_ref: &str,
    files: &[(String, String)],
    options: Option<&PushOverlayOptions>,
) -> Result<AcceptedPush<'a, C>, LadderExhausted> {
    let mut attempts = Vec::new();
    for (remote, client) in endpoints {
        let prior_published_at = match client.get_status(worktree) {
            Ok(Some(status)) => Some(status.published_at),
            Ok(None) => None,
            Err(e) => {
                crate::ui::warn(format!(
                    "verdict: pre-push status poll on `{remote}` failed ({e}); \
                     freshness falls back to push time"
                ));
                None
            }
        };
        let freshness = AwaitFreshness {
            prior_published_at,
            not_before_unix: crate::statusfile::now_unix(),
        };
        match client.push_overlay_with_options(worktree, base_ref, files, None, options) {
            Ok(ack) if ack.accepted => {
                return Ok(AcceptedPush {
                    remote: remote.as_str(),
                    client,
                    applied_files: ack.applied_files,
                    freshness,
                });
            }
            Ok(_) => {
                crate::ui::warn(format!(
                    "verdict: `{remote}` rejected the push (quiescing daemon or \
                     payload guard); trying next remote"
                ));
                attempts.push((remote.clone(), AttemptFailure::Rejected));
            }
            Err(TransportError::Unauthorized) => {
                crate::ui::warn(format!(
                    "verdict: `{remote}` refused the bearer token; trying next remote"
                ));
                attempts.push((remote.clone(), AttemptFailure::Unauthorized));
            }
            Err(e) => {
                crate::ui::warn(format!(
                    "verdict: push to `{remote}` failed ({e}); trying next remote"
                ));
                attempts.push((remote.clone(), AttemptFailure::Transport(e.to_string())));
            }
        }
    }
    Err(LadderExhausted { attempts })
}

/// The attribution acceptance predicate (module doc, A2 consumer):
///
/// * both sides carry a SHA and they MATCH ⇒ accept ONLY IF the status
///   is also FRESH (published after our push). The matching SHA proves
///   "this verdict is for our base"; it does NOT prove "this verdict
///   analyzed our overlay". A green published for the base BEFORE our
///   push — the central `--server-root` / foreign-worktree case, where
///   the daemon cannot map our overlay onto its served tree and so never
///   publishes an overlay-attributed verdict for our key — would match
///   the SHA while having analyzed nothing of ours (`crates: []`). The
///   freshness conjunct closes that base_sha-echo false-green (the CGLS-9
///   a8 residual: a planted type error went green 10/11 because a stale
///   matching-SHA base-green was accepted as the overlay's verdict). Cost
///   of dropping the old freshness-ignored fast-path: a genuine idempotent
///   re-run now waits for one fresh publication instead of accepting the
///   cached green — seconds, paid rarely. Soundness over the micro-opt for
///   a merge gate.
/// * both carry a SHA and they MISMATCH ⇒ never accept (another
///   branch's verdict on a shared key, or a stale prior publication
///   mid-replacement);
/// * either side lacks a SHA ⇒ freshness-only (legacy daemons that do
///   not echo `base_sha`, or an unresolvable local ref). Freshness
///   means "published after OUR accepted push", which on a single
///   per-key publication stream attributes the verdict to our overlay.
fn status_is_acceptable(
    status: &WorktreeStatus,
    resolved_sha: Option<&str>,
    freshness: AwaitFreshness,
) -> bool {
    match (resolved_sha, status.base_sha.as_deref()) {
        (Some(mine), Some(theirs)) => mine == theirs && freshness.is_fresh(status.published_at),
        _ => freshness.is_fresh(status.published_at),
    }
}

/// Poll `get_status` on the pinned client until the acceptance
/// predicate passes or the wall clock runs out. Poll errors warn and
/// keep polling — transient drops mid-await must not abandon a verdict
/// the daemon is still computing (and failing over mid-await would poll
/// a shard that never saw the push).
fn await_attributed_verdict<C: TransportClient>(
    client: &C,
    worktree: &str,
    resolved_sha: Option<&str>,
    freshness: AwaitFreshness,
    timeout_secs: u64,
) -> Option<WorktreeStatus> {
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let started = Instant::now();
    while started.elapsed() < timeout {
        match client.get_status(worktree) {
            Ok(Some(status)) if status_is_acceptable(&status, resolved_sha, freshness) => {
                return Some(status);
            }
            Ok(_) => {}
            Err(e) => {
                crate::ui::warn(format!("verdict: status poll failed ({e}); retrying"));
            }
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        let wait = remaining.min(Duration::from_millis(200));
        if wait.is_zero() {
            break;
        }
        std::thread::sleep(wait);
    }
    None
}

/// Stdout emit for a daemon-published verdict: the full status wire
/// shape + `remote` + `source:"daemon"`.
fn emit_daemon_verdict(opts: &VerdictOpts, status: &WorktreeStatus, remote: &str) -> ExitCode {
    eprintln!(
        "[cargoless:verdict] verdict={} worktree={} base_sha={} published_at={} via {}",
        status.verdict,
        status.worktree,
        status.base_sha.as_deref().unwrap_or("-"),
        status.published_at,
        remote
    );
    match opts.output {
        OutputMode::Json => println!("{}", daemon_verdict_json(status, remote)),
        OutputMode::Text => println!("{}", status.verdict),
    }
    ExitCode::from(exit_byte_for_verdict(&status.verdict))
}

fn daemon_verdict_json(status: &WorktreeStatus, remote: &str) -> String {
    let mut value: serde_json::Value =
        serde_json::from_str(&status_to_json(status)).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "remote".to_string(),
            serde_json::Value::String(remote.to_string()),
        );
        obj.insert(
            "source".to_string(),
            serde_json::Value::String("daemon".to_string()),
        );
    }
    value.to_string()
}

/// Stdout emit for a CLIENT-synthesized verdict (trivial green, ladder
/// exhausted, await timeout): same top-level keys as the daemon shape
/// where they exist, `source:"client"`, plus the reason under
/// `trivial_reason` (green) or `verdict_failure_reason` (unknown).
fn emit_client_verdict(
    opts: &VerdictOpts,
    verdict: &str,
    detail: &str,
    remote: Option<&str>,
    resolved_sha: Option<&str>,
    exit: u8,
) -> ExitCode {
    if verdict == "green" {
        eprintln!("[cargoless:verdict] trivial green: {detail}");
    } else {
        crate::ui::error(format!("verdict: {detail}"));
    }
    match opts.output {
        OutputMode::Json => println!(
            "{}",
            client_verdict_json(&opts.worktree, verdict, detail, remote, resolved_sha)
        ),
        OutputMode::Text => println!("{verdict}"),
    }
    ExitCode::from(exit)
}

fn client_verdict_json(
    worktree: &str,
    verdict: &str,
    detail: &str,
    remote: Option<&str>,
    resolved_sha: Option<&str>,
) -> String {
    let mut value = serde_json::json!({
        "worktree": worktree,
        "verdict": verdict,
        "source": "client",
    });
    let obj = value
        .as_object_mut()
        .expect("client_verdict_json constructed an object literal");
    let reason_key = if verdict == "green" {
        "trivial_reason"
    } else {
        "verdict_failure_reason"
    };
    obj.insert(
        reason_key.to_string(),
        serde_json::Value::String(detail.to_string()),
    );
    if let Some(remote) = remote {
        obj.insert(
            "remote".to_string(),
            serde_json::Value::String(remote.to_string()),
        );
    }
    if let Some(sha) = resolved_sha {
        obj.insert(
            "base_sha".to_string(),
            serde_json::Value::String(sha.to_string()),
        );
    }
    value.to_string()
}

/// 0 green / 1 red / 75 anything else (EX_TEMPFAIL: `unknown`,
/// `Indeterminate`-class strings — infra trouble, never a code red).
fn exit_byte_for_verdict(verdict: &str) -> u8 {
    match verdict {
        "green" => 0,
        "red" => 1,
        _ => 75,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargoless_core::transport::{
        CheckProfile, CrateVerdict, PushOverlayAck, TransitionEvent, WorktreeSummary,
    };
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, channel};

    fn status(verdict: &str, base_sha: Option<&str>, published_at: u64) -> WorktreeStatus {
        WorktreeStatus {
            worktree: "/wt".into(),
            verdict: verdict.into(),
            daemon_build_id: "test-build".into(),
            crates: vec![CrateVerdict {
                name: "core".into(),
                verdict: verdict.into(),
            }],
            red_diagnostics: u32::from(verdict == "red"),
            verdict_failure_reason: None,
            base_sha: base_sha.map(str::to_string),
            ra_blind_paths: false,
            heartbeat_age_secs: 1,
            published_at,
        }
    }

    #[test]
    fn output_mode_parses_json_text_and_rejects_garbage() {
        assert_eq!(OutputMode::parse("json"), Some(OutputMode::Json));
        assert_eq!(OutputMode::parse("text"), Some(OutputMode::Text));
        assert_eq!(OutputMode::parse("yaml"), None);
        assert_eq!(OutputMode::parse(""), None);
    }

    #[test]
    fn parse_headers_splits_on_first_colon_only() {
        let parsed = parse_headers(&[
            "X-Cargoless-Routing-Key: tf-mv-route-7".to_string(),
            "X-Callback: http://host:8080/path".to_string(),
        ])
        .unwrap();
        assert_eq!(
            parsed,
            vec![
                (
                    "X-Cargoless-Routing-Key".to_string(),
                    "tf-mv-route-7".to_string()
                ),
                (
                    "X-Callback".to_string(),
                    "http://host:8080/path".to_string()
                ),
            ]
        );
    }

    #[test]
    fn parse_headers_rejects_missing_colon_and_empty_name() {
        let err = parse_headers(&["NoColonHere".to_string()]).unwrap_err();
        assert!(err.contains("not of the form"), "{err}");
        let err = parse_headers(&[": value-without-name".to_string()]).unwrap_err();
        assert!(err.contains("empty name"), "{err}");
    }

    /// The A2 attribution matrix — the predicate the required merge
    /// check will trust. Each arm is a distinct correctness class:
    /// matching SHAs accept ONLY when also fresh (the base_sha-echo
    /// false-green fix — CGLS-9 a8 residual), mismatched SHAs NEVER
    /// accept (cross-branch verdict bleed — the false-attribution
    /// incident class A2 closes), and missing SHAs degrade to the
    /// freshness guard.
    #[test]
    fn attribution_predicate_matrix() {
        let fresh_after_100 = AwaitFreshness {
            prior_published_at: Some(100),
            not_before_unix: 100,
        };
        // Match + FRESH ⇒ accept (published after our push: a verdict
        // genuinely produced for our overlay on this base).
        assert!(status_is_acceptable(
            &status("green", Some("abc"), 101),
            Some("abc"),
            fresh_after_100
        ));
        // Match but STALE ⇒ REJECT. This is the a8 residual fix: a green
        // published for our base BEFORE our push (published_at=50 < 100)
        // is the daemon's pre-existing base-green, NOT a verdict that
        // analyzed our overlay (foreign-worktree / central --server-root
        // case). Accepting it false-greened planted type errors 10/11.
        assert!(!status_is_acceptable(
            &status("green", Some("abc"), 50),
            Some("abc"),
            fresh_after_100
        ));
        // Mismatch ⇒ never accept, even when fresh.
        assert!(!status_is_acceptable(
            &status("green", Some("other"), 999),
            Some("abc"),
            fresh_after_100
        ));
        // Status unattributed ⇒ freshness decides.
        assert!(!status_is_acceptable(
            &status("green", None, 100),
            Some("abc"),
            fresh_after_100
        ));
        assert!(status_is_acceptable(
            &status("green", None, 101),
            Some("abc"),
            fresh_after_100
        ));
        // Client SHA unresolved ⇒ freshness decides even when the
        // status carries one.
        assert!(status_is_acceptable(
            &status("green", Some("abc"), 101),
            None,
            fresh_after_100
        ));
        assert!(!status_is_acceptable(
            &status("green", Some("abc"), 100),
            None,
            fresh_after_100
        ));
    }

    #[test]
    fn exit_bytes_follow_the_fleet_convention() {
        assert_eq!(exit_byte_for_verdict("green"), 0);
        assert_eq!(exit_byte_for_verdict("red"), 1);
        assert_eq!(exit_byte_for_verdict("unknown"), 75);
        assert_eq!(exit_byte_for_verdict(""), 75);
        assert_eq!(exit_byte_for_verdict("Indeterminate"), 75);
    }

    #[test]
    fn daemon_verdict_json_carries_remote_and_source() {
        let json = daemon_verdict_json(&status("green", Some("abc"), 7), "http://a:8787");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["verdict"], "green");
        assert_eq!(value["base_sha"], "abc");
        assert_eq!(value["remote"], "http://a:8787");
        assert_eq!(value["source"], "daemon");
        // The status wire shape rides through intact.
        assert_eq!(value["published_at"], 7);
        assert_eq!(value["crates"][0]["name"], "core");
    }

    #[test]
    fn client_verdict_json_distinguishes_trivial_green_from_unknown() {
        let green: serde_json::Value = serde_json::from_str(&client_verdict_json(
            "/wt",
            "green",
            "empty diff vs HEAD",
            None,
            Some("abc"),
        ))
        .unwrap();
        assert_eq!(green["verdict"], "green");
        assert_eq!(green["source"], "client");
        assert_eq!(green["trivial_reason"], "empty diff vs HEAD");
        assert_eq!(green["base_sha"], "abc");
        assert!(green.get("verdict_failure_reason").is_none());
        assert!(green.get("remote").is_none());

        let unknown: serde_json::Value = serde_json::from_str(&client_verdict_json(
            "/wt",
            "unknown",
            "timed out after 180s",
            Some("http://a:8787"),
            None,
        ))
        .unwrap();
        assert_eq!(unknown["verdict"], "unknown");
        assert_eq!(unknown["source"], "client");
        assert_eq!(unknown["verdict_failure_reason"], "timed out after 180s");
        assert_eq!(unknown["remote"], "http://a:8787");
        assert!(unknown.get("base_sha").is_none());
    }

    // ── Ladder semantics against stub transports ──────────────────────

    /// Scripted `TransportClient`: a queue of push outcomes plus a fixed
    /// pre-poll status. Only the verbs the ladder exercises are
    /// meaningful; the rest satisfy the trait minimally.
    struct StubClient {
        pre_status: Option<WorktreeStatus>,
        push_outcomes: Mutex<Vec<Result<PushOverlayAck, TransportError>>>,
    }

    impl StubClient {
        fn new(
            pre_status: Option<WorktreeStatus>,
            outcome: Result<PushOverlayAck, TransportError>,
        ) -> Self {
            Self {
                pre_status,
                push_outcomes: Mutex::new(vec![outcome]),
            }
        }
    }

    fn accepted_ack() -> PushOverlayAck {
        PushOverlayAck {
            worktree: "/wt".into(),
            accepted: true,
            applied_files: 3,
        }
    }

    fn rejected_ack() -> PushOverlayAck {
        PushOverlayAck {
            worktree: "/wt".into(),
            accepted: false,
            applied_files: 0,
        }
    }

    impl TransportClient for StubClient {
        fn get_status(&self, _w: &str) -> Result<Option<WorktreeStatus>, TransportError> {
            Ok(self.pre_status.clone())
        }
        fn get_verdict(&self, _w: &str) -> Result<Option<String>, TransportError> {
            Ok(self.pre_status.as_ref().map(|s| s.verdict.clone()))
        }
        fn get_diagnostics(
            &self,
            _w: &str,
        ) -> Result<Vec<cargoless_core::Diagnostic>, TransportError> {
            Ok(Vec::new())
        }
        fn list_worktrees(&self) -> Result<Vec<WorktreeSummary>, TransportError> {
            Ok(Vec::new())
        }
        fn subscribe(&self) -> Result<Receiver<TransitionEvent>, TransportError> {
            Ok(channel().1)
        }
        fn push_overlay_with_options(
            &self,
            _worktree: &str,
            _base_ref: &str,
            _files: &[(String, String)],
            _check_profile: Option<&CheckProfile>,
            _options: Option<&PushOverlayOptions>,
        ) -> Result<PushOverlayAck, TransportError> {
            self.push_outcomes
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| Ok(accepted_ack()))
        }
    }

    fn files() -> Vec<(String, String)> {
        vec![("src/lib.rs".to_string(), "pub fn x() {}".to_string())]
    }

    #[test]
    fn ladder_fails_over_transport_error_and_rejection_then_pins_acceptor() {
        let endpoints = vec![
            (
                "http://down:8787".to_string(),
                StubClient::new(
                    None,
                    Err(TransportError::Io(std::io::Error::other("refused"))),
                ),
            ),
            (
                "http://draining:8787".to_string(),
                StubClient::new(None, Ok(rejected_ack())),
            ),
            (
                "http://healthy:8787".to_string(),
                StubClient::new(Some(status("green", None, 500)), Ok(accepted_ack())),
            ),
        ];
        let accepted = push_with_failover(&endpoints, "/wt", "HEAD", &files(), None).unwrap();
        assert_eq!(accepted.remote, "http://healthy:8787");
        assert_eq!(accepted.applied_files, 3);
        // Freshness was captured from the ACCEPTING endpoint's pre-poll:
        // a later verdict must publish after that endpoint's prior 500.
        assert_eq!(accepted.freshness.prior_published_at, Some(500));
        assert!(!accepted.freshness.is_fresh(500));
        assert!(accepted.freshness.is_fresh(501));
    }

    #[test]
    fn exhausted_ladder_reports_every_attempt_in_order() {
        let endpoints = vec![
            (
                "http://a:8787".to_string(),
                StubClient::new(
                    None,
                    Err(TransportError::Io(std::io::Error::other("refused"))),
                ),
            ),
            (
                "http://b:8787".to_string(),
                StubClient::new(None, Ok(rejected_ack())),
            ),
        ];
        let exhausted = match push_with_failover(&endpoints, "/wt", "HEAD", &files(), None) {
            Err(exhausted) => exhausted,
            Ok(_) => panic!("ladder of failing endpoints must exhaust"),
        };
        assert_eq!(exhausted.attempts.len(), 2);
        assert_eq!(exhausted.attempts[0].0, "http://a:8787");
        assert!(matches!(
            exhausted.attempts[0].1,
            AttemptFailure::Transport(_)
        ));
        assert_eq!(exhausted.attempts[1].1, AttemptFailure::Rejected);
        assert!(!exhausted.all_unauthorized());
        let described = exhausted.describe_attempts();
        assert!(
            described.contains("http://a:8787: transport error"),
            "{described}"
        );
        assert!(
            described.contains("http://b:8787: push rejected"),
            "{described}"
        );
    }

    #[test]
    fn all_unauthorized_is_a_config_class_not_tempfail() {
        let endpoints = vec![
            (
                "http://a:8787".to_string(),
                StubClient::new(None, Err(TransportError::Unauthorized)),
            ),
            (
                "http://b:8787".to_string(),
                StubClient::new(None, Err(TransportError::Unauthorized)),
            ),
        ];
        let exhausted = match push_with_failover(&endpoints, "/wt", "HEAD", &files(), None) {
            Err(exhausted) => exhausted,
            Ok(_) => panic!("all-unauthorized ladder must exhaust"),
        };
        assert!(exhausted.all_unauthorized());
        // Mixed failures are NOT the config class.
        let mixed = LadderExhausted {
            attempts: vec![
                ("http://a:8787".to_string(), AttemptFailure::Unauthorized),
                (
                    "http://b:8787".to_string(),
                    AttemptFailure::Transport("refused".into()),
                ),
            ],
        };
        assert!(!mixed.all_unauthorized());
        // Empty ladder result is never "all unauthorized".
        let empty = LadderExhausted { attempts: vec![] };
        assert!(!empty.all_unauthorized());
    }

    #[test]
    fn await_accepts_fresh_sha_match_and_times_out_on_stale_match_or_mismatch() {
        let guard = AwaitFreshness {
            prior_published_at: Some(1000),
            not_before_unix: 1000,
        };

        // SHA match + FRESH (published_at 1001 > prior 1000): instant accept.
        let fresh_match =
            StubClient::new(Some(status("red", Some("abc"), 1001)), Ok(accepted_ack()));
        let got = await_attributed_verdict(&fresh_match, "/wt", Some("abc"), guard, 5)
            .expect("fresh sha-matched status accepted");
        assert_eq!(got.verdict, "red");

        // SHA match but STALE (published_at 1 < prior 1000): NEVER accepted.
        // This is the a8 residual fix — a pre-existing base-green that
        // predates our push must not be attributed to our overlay. The
        // await honestly times out (1s floor) instead of false-greening.
        let stale_match = StubClient::new(Some(status("green", Some("abc"), 1)), Ok(accepted_ack()));
        assert!(
            await_attributed_verdict(&stale_match, "/wt", Some("abc"), guard, 1).is_none(),
            "stale matching base_sha must never satisfy the await (base_sha-echo false-green)"
        );

        // SHA mismatch: never accepted; the await honestly times out
        // (1s floor) instead of returning another branch's verdict.
        let mismatched = StubClient::new(
            Some(status("green", Some("other"), 9999)),
            Ok(accepted_ack()),
        );
        assert!(
            await_attributed_verdict(&mismatched, "/wt", Some("abc"), guard, 1).is_none(),
            "mismatched base_sha must never satisfy the await"
        );
    }

    // ── One real-wire ladder roundtrip (HttpServer + HttpClient) ──────

    /// Minimal accepting daemon modelling the real publish-after-analysis
    /// flow: NO attributed verdict exists until an overlay is pushed; the
    /// push triggers publication of a fresh green at `published_at: 2000`.
    /// Returning `None` pre-push is what makes the post-push status read as
    /// FRESH (the await captures `prior_published_at: None` pre-push, then
    /// 2000 > not_before). A stub that returned a constant timestamp both
    /// before and after the push could never satisfy the freshness conjunct
    /// — and would mask the very base_sha-echo bug the fix closes.
    struct GreenService {
        sha: String,
        pushed: std::sync::atomic::AtomicBool,
    }

    impl GreenService {
        fn new(sha: &str) -> Self {
            Self {
                sha: sha.to_string(),
                pushed: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    impl cargoless_core::transport::VerdictService for GreenService {
        fn get_status(&self, worktree: &str) -> Option<WorktreeStatus> {
            // Pre-push: no verdict attributed to this key yet.
            if !self.pushed.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            Some(WorktreeStatus {
                worktree: worktree.to_string(),
                verdict: "green".into(),
                daemon_build_id: "green-service".into(),
                crates: Vec::new(),
                red_diagnostics: 0,
                verdict_failure_reason: None,
                base_sha: Some(self.sha.clone()),
                ra_blind_paths: false,
                heartbeat_age_secs: 0,
                // Stamp wall-clock at publish (post-push) so the status is
                // genuinely fresher than the await's not_before_unix — the
                // real daemon publishes a new verdict after analyzing the
                // pushed overlay. A hardcoded past constant could never
                // satisfy the freshness conjunct in the None-prior branch.
                published_at: crate::statusfile::now_unix(),
            })
        }
        fn get_verdict(&self, _worktree: &str) -> Option<String> {
            Some("green".into())
        }
        fn get_diagnostics(&self, _worktree: &str) -> Vec<cargoless_core::Diagnostic> {
            Vec::new()
        }
        fn list_worktrees(&self) -> Vec<WorktreeSummary> {
            Vec::new()
        }
        fn subscribe(&self) -> Receiver<TransitionEvent> {
            channel().1
        }
        fn push_overlay(
            &self,
            worktree: &str,
            _base_ref: &str,
            files: &[(String, String)],
        ) -> PushOverlayAck {
            self.pushed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            PushOverlayAck {
                worktree: worktree.to_string(),
                accepted: true,
                applied_files: files.len() as u32,
            }
        }
    }

    /// Daemon that refuses ingest — the `VerdictService` trait default
    /// for `push_overlay` answers `accepted: false`, exactly the shape a
    /// quiescing/pre-push-era daemon puts on the wire.
    struct RefusingService;

    impl cargoless_core::transport::VerdictService for RefusingService {
        fn get_status(&self, _worktree: &str) -> Option<WorktreeStatus> {
            None
        }
        fn get_verdict(&self, _worktree: &str) -> Option<String> {
            None
        }
        fn get_diagnostics(&self, _worktree: &str) -> Vec<cargoless_core::Diagnostic> {
            Vec::new()
        }
        fn list_worktrees(&self) -> Vec<WorktreeSummary> {
            Vec::new()
        }
        fn subscribe(&self) -> Receiver<TransitionEvent> {
            channel().1
        }
    }

    /// End-to-end ladder over real HTTP: entry 1 refuses the push,
    /// entry 2 accepts; the await then resolves on entry 2 with the
    /// SHA-attributed green — proving headers/clients/ladder/await
    /// compose over the same wire the gate will use.
    #[test]
    fn wire_ladder_fails_over_to_accepting_daemon_and_awaits_attributed_green() {
        use cargoless_core::transport::AllowAll;
        use cargoless_core::transport::http::HttpServer;
        use std::sync::Arc;

        let refusing =
            HttpServer::bind("127.0.0.1:0", Arc::new(RefusingService), Arc::new(AllowAll)).unwrap();
        let green = HttpServer::bind(
            "127.0.0.1:0",
            Arc::new(GreenService::new("abc123")),
            Arc::new(AllowAll),
        )
        .unwrap();
        let url_refusing = format!("http://{}", refusing.addr());
        let url_green = format!("http://{}", green.addr());

        let headers = vec![(
            "X-Cargoless-Routing-Key".to_string(),
            "tf-mv-route-7".to_string(),
        )];
        let endpoints = vec![
            (
                url_refusing.clone(),
                build_client(&url_refusing, None, &headers).unwrap(),
            ),
            (
                url_green.clone(),
                build_client(&url_green, None, &headers).unwrap(),
            ),
        ];

        let accepted = push_with_failover(&endpoints, "/wt", "HEAD", &files(), None).unwrap();
        assert_eq!(accepted.remote, url_green);
        assert_eq!(accepted.applied_files, 1);

        let status = await_attributed_verdict(
            accepted.client,
            "/wt",
            Some("abc123"),
            accepted.freshness,
            5,
        )
        .expect("attributed green within timeout");
        assert_eq!(status.verdict, "green");
        assert_eq!(status.base_sha.as_deref(), Some("abc123"));

        let json = daemon_verdict_json(&status, accepted.remote);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["source"], "daemon");
        assert_eq!(value["remote"], url_green);
        assert_eq!(exit_byte_for_verdict(value["verdict"].as_str().unwrap()), 0);
    }
}
