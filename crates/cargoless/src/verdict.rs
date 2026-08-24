//! `cargoless verdict` — the outcome-v3 merge-gate client
//! (submit an exact attempt → await its typed outcome → obey its reaction).
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
//! What the subcommand owns:
//!
//! * **Routing headers (C1):** `--header` values ride EVERY request —
//!   the push and all status polls — because the pool ingress
//!   consistent-hashes `X-Cargoless-Routing-Key`; a poll that dropped
//!   the header would hash to a different shard than the push it is
//!   awaiting. Injection is client-wide by construction
//!   (`HttpClient::with_header`).
//! * **Exact identity:** request, attempt, execution, and trace identities
//!   are distinct from the overlay SHA. Polling is by `attempt_id`; equal
//!   SHAs in different retries or PRs can never satisfy one another.
//! * **Failover ladder:** repeatable `--remote`, tried in order for
//!   submission. Awaiting stays pinned to the daemon that accepted the
//!   attempt because that daemon owns its evidence and terminal outcome.
//! * **Witness check-ids (B3 surface):** `--check-id` values travel as
//!   `PushOverlayOptions::check_ids` on the wire. Today's daemons
//!   store-and-ignore them; per-check witness selection consumes them
//!   server-side when B3 lands.
//! * **One reaction mapping:** the validated outcome carries the required
//!   `pending`, `success`, `failure`, `error`, or `no_update` reaction.
//!   Exit 0 = success, 1 = code failure, 75 = retryable/persistent
//!   infrastructure indeterminacy, and 2 = local setup/protocol error.
//!
//! **Candidate rollout:** typed candidate transport and v2 result evidence are
//! explicitly enabled by `--candidate-snapshot`. Without it, the established
//! legacy text projection and v1/exact-Git-compatible behavior are preserved.
//! Typed mode also requires one or more explicit `--check-id` values. They are
//! normalized into a sorted unique set, and each must yield exactly one
//! sequential, authority-bound v2 evidence artifact before the terminal outcome
//! is accepted.
//!
//! **Typed trivial-green short-circuit:** when the complete candidate tree
//! equals `--base`, the daemon has nothing to evaluate beyond the already-gated
//! base — the verdict is `green` with
//! `"source":"client"` + `"trivial_reason"` so consumers can tell it
//! apart from a daemon verdict. Binary-only, mode-only, and delete-only
//! candidates are typed changes and never take this short-circuit.
//!
//! **Output contract** (`--output json`, the default): one validated
//! `cargoless.outcome/v3` envelope plus `remote`, `source`, and the same
//! contract-derived `reaction`. All human diagnostics stay on stderr.
//! `--output text` prints the reaction state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use cargoless_core::evidence::{
    ArtifactKind, EvidenceBundle, EvidenceClass, EvidenceInventoryEntry, EvidenceStore,
    canonical_evidence_bundle_digest,
};
use cargoless_core::outcome::{
    AttemptId, CheckState, Component, Conclusion, EvidenceAvailability, EvidenceRef,
    IndeterminateCause, NonEmptyText, OutcomeEnvelope, PassBasis, Phase, PhaseRecord, Producer,
    RequestId, RetryDirective, Subject, Surface, TraceId,
};
use cargoless_core::project_checks::{
    ProjectCheckOutcome, validate_structured_check_result_semantics,
};
use cargoless_core::transport::http::HttpClient;
use cargoless_core::transport::{AttemptContext, PushOverlayOptions, TransportError};
#[cfg(test)]
use cargoless_core::transport::{TransportClient, WorktreeStatus, status_to_json};
use cargoless_core::{CandidateSnapshot, CandidateSnapshotManifest};

#[cfg(test)]
use crate::push::AwaitFreshness;
use crate::push::{
    PushPayload, apply_candidate_snapshot_options, build_push_payload, candidate_changed_paths,
    emit_payload_diagnostics, git_changed_files, git_resolve_ref, push_overlay_request_body,
    push_payload_from_candidate_snapshot, validate_overlay_http_cap,
};

const CANDIDATE_V2_EVIDENCE_ARTIFACT_PATTERN: &str = "project-check-result-NNN.json";
const MAX_CANDIDATE_V2_EVIDENCE_ARTIFACTS: usize = 999;

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
    /// Opt in to the candidate-snapshot/v2 evidence protocol. Requires at least
    /// one explicit `check_ids` entry. Default CLI behavior remains the legacy
    /// text/exact-Git-compatible verdict path.
    pub candidate_snapshot: bool,
    pub await_timeout_secs: u64,
}

struct VerdictCandidateSubmission {
    manifest: CandidateSnapshotManifest,
    changed: Vec<String>,
    payload: PushPayload,
    options: PushOverlayOptions,
}

struct LegacyVerdictSubmission {
    changed: Vec<String>,
    payload: PushPayload,
    body: String,
}

fn canonical_candidate_check_ids(check_ids: &[String]) -> Vec<String> {
    check_ids
        .iter()
        .map(|check_id| check_id.trim())
        .filter(|check_id| !check_id.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn build_legacy_verdict_submission(
    opts: &VerdictOpts,
    resolved_sha: &str,
    semantic: &AttemptContext,
    changed: Vec<String>,
    payload: PushPayload,
) -> LegacyVerdictSubmission {
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
        base_sha: Some(resolved_sha.to_string()),
        semantic: Some(semantic.clone()),
        ..PushOverlayOptions::default()
    };
    if let Some(root) = opts.server_root.as_ref() {
        options.repo_relative = true;
        options.analysis_root = Some(root.to_string_lossy().into_owned());
    }
    let body = push_overlay_request_body(
        &opts.worktree,
        &opts.base,
        &payload.files,
        None,
        Some(&options),
    );
    LegacyVerdictSubmission {
        changed,
        payload,
        body,
    }
}

/// Build the verdict request from the same single typed snapshot, legacy
/// projection, and changed-path derivation as `cargoless push`.
fn build_verdict_candidate_submission(
    opts: &VerdictOpts,
    semantic: &AttemptContext,
) -> Result<Option<VerdictCandidateSubmission>, String> {
    let Some(built) = crate::candidate_snapshot_git::build_overlay_manifest(&opts.repo, &opts.base)
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let manifest = built.manifest;
    let changed = candidate_changed_paths(&manifest);
    let repo_relative = opts.server_root.is_some();
    let mut payload = push_payload_from_candidate_snapshot(&opts.repo, &manifest, repo_relative)?;
    payload.files.sort_by(|left, right| left.0.cmp(&right.0));
    let check_ids = canonical_candidate_check_ids(&opts.check_ids);

    let mut options = PushOverlayOptions {
        changed_files: (!changed.is_empty()).then(|| changed.clone()),
        gate: opts.gate,
        check_ids: (!check_ids.is_empty()).then_some(check_ids),
        // Legacy readers keep receiving comparison attribution. Candidate
        // identity remains separately bound by the typed manifest.
        base_sha: Some(manifest.comparison_base.commit_sha.clone()),
        semantic: Some(semantic.clone()),
        ..PushOverlayOptions::default()
    };
    apply_candidate_snapshot_options(&mut options, &manifest);
    if let Some(root) = opts.server_root.as_ref() {
        options.repo_relative = true;
        options.analysis_root = Some(root.to_string_lossy().into_owned());
    }

    Ok(Some(VerdictCandidateSubmission {
        manifest,
        changed,
        payload,
        options,
    }))
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

    if opts.candidate_snapshot {
        run_candidate_snapshot(opts, &headers)
    } else {
        run_legacy_verdict(opts, &headers)
    }
}

fn run_candidate_snapshot(opts: &VerdictOpts, headers: &[(String, String)]) -> ExitCode {
    let expected_check_ids = canonical_candidate_check_ids(&opts.check_ids);
    if expected_check_ids.is_empty() {
        crate::ui::error(
            "verdict: --candidate-snapshot requires at least one non-empty --check-id so every result artifact has an expected identity",
        );
        return ExitCode::from(2);
    }
    if expected_check_ids.len() > MAX_CANDIDATE_V2_EVIDENCE_ARTIFACTS {
        crate::ui::error(format!(
            "verdict: --candidate-snapshot accepts at most {MAX_CANDIDATE_V2_EVIDENCE_ARTIFACTS} distinct --check-id values"
        ));
        return ExitCode::from(2);
    }

    // Attempt identity is independent from source identity. The base text is
    // only a uniqueness seed when callers do not provide explicit IDs; the
    // typed manifest below is the comparison/candidate authority.
    let semantic = match attempt_context_from_env(&opts.base) {
        Ok(context) => context,
        Err(error) => {
            crate::ui::error(format!("verdict: invalid outcome-v3 identity: {error}"));
            return ExitCode::from(2);
        }
    };

    // 1. Construct exactly one canonical candidate snapshot. An absent
    // candidate means the complete tree equals the comparison base. A typed
    // candidate is never trivial merely because its legacy text projection
    // is empty (binary-only and mode-only candidates remain checkable).
    let submission = match build_verdict_candidate_submission(opts, &semantic) {
        Ok(submission) => submission,
        Err(error) => {
            crate::ui::error(format!(
                "verdict: could not build candidate snapshot against `{}` in `{}`: {error}",
                opts.base,
                opts.repo.display()
            ));
            return ExitCode::from(2);
        }
    };
    let Some(VerdictCandidateSubmission {
        manifest,
        changed,
        payload,
        options,
    }) = submission
    else {
        let resolved_sha = match git_resolve_ref(&opts.repo, &opts.base) {
            Ok(sha) => sha,
            Err(error) => {
                crate::ui::error(format!(
                    "verdict: outcome-v3 requires an immutable base SHA, but `{}` \
                     could not be resolved: {error}",
                    opts.base
                ));
                return ExitCode::from(2);
            }
        };
        let detail = format!("empty diff vs {}", opts.base);
        return match local_trivial_outcome_v3(opts, &semantic, &resolved_sha, &[], &detail) {
            Ok(outcome) => emit_local_outcome_v3(opts, &outcome),
            Err(error) => {
                crate::ui::error(format!(
                    "verdict: could not persist the trivial-pass evidence: {error}"
                ));
                ExitCode::from(75)
            }
        };
    };
    let resolved_sha = manifest.comparison_base.commit_sha.clone();

    // 2. Send the compatibility text projection and typed authority together.
    let options = Some(options);
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
        match build_client(remote, opts.auth_token.as_deref(), headers) {
            Ok(client) => endpoints.push((remote.clone(), client)),
            Err(e) => {
                crate::ui::error(format!("verdict: client init failed for `{remote}`: {e}"));
                return ExitCode::from(2);
            }
        }
    }

    // 5. Push down the ladder; pin the await to the accepting remote.
    let accepted = match submit_v3_with_failover(&endpoints, &body, &semantic) {
        Ok(accepted) => accepted,
        Err(exhausted) => {
            let reason = format!(
                "no remote accepted the push — {}",
                exhausted.describe_attempts()
            );
            // Unauthorized everywhere is a config problem (one shared
            // token), not transient infra: exit 2 so callers fix setup
            // instead of retrying.
            if exhausted.all_unauthorized() {
                crate::ui::error(format!("verdict: {reason}"));
                println!(
                    "{}",
                    serde_json::json!({
                        "schema": "cargoless.protocol-error/v3",
                        "code": "authentication.rejected",
                        "request_id": semantic.request_id.as_str(),
                        "attempt_id": semantic.attempt_id.as_str(),
                        "summary": reason,
                    })
                );
                return ExitCode::from(2);
            }
            return match local_indeterminate_outcome_v3(
                opts,
                &semantic,
                &resolved_sha,
                &changed,
                &reason,
                IndeterminateCause::DependencyUnavailable {
                    component: Component::Protocol,
                },
            ) {
                Ok(outcome) => emit_local_outcome_v3(opts, &outcome),
                Err(error) => {
                    crate::ui::error(format!(
                        "verdict: could not persist the indeterminate outcome: {error}"
                    ));
                    ExitCode::from(75)
                }
            };
        }
    };
    eprintln!(
        "[cargoless:verdict] attempt={} request={} accepted by {}; awaiting exact attempt outcome (timeout {}s)",
        semantic.attempt_id, semantic.request_id, accepted.remote, opts.await_timeout_secs
    );

    // 6. Await the exact attempt on the SAME client. Commit-addressed
    // status is intentionally not used by outcome-v3: a retry of the same
    // SHA is a different execution and therefore a different attempt.
    match await_attempt_v3(
        accepted.client,
        &semantic.attempt_id,
        accepted.initial,
        opts.await_timeout_secs,
    ) {
        Some(outcome) => {
            match retrieve_candidate_v2_evidence(
                accepted.client,
                &outcome,
                &manifest,
                &expected_check_ids,
            ) {
                Ok(_) => emit_daemon_outcome_v3(opts, &outcome, accepted.remote),
                Err(error) => {
                    emit_candidate_evidence_error(opts, &outcome, accepted.remote, &error)
                }
            }
        }
        None => {
            let reason = format!(
                "timed out after {}s awaiting exact attempt {} from {}",
                opts.await_timeout_secs, semantic.attempt_id, accepted.remote
            );
            match local_indeterminate_outcome_v3(
                opts,
                &semantic,
                &resolved_sha,
                &changed,
                &reason,
                IndeterminateCause::BudgetExhausted {
                    component: Component::ProjectCheck,
                    budget: NonEmptyText::new(format!(
                        "await_timeout_secs={}",
                        opts.await_timeout_secs
                    ))
                    .expect("timeout budget is non-empty"),
                },
            ) {
                Ok(outcome) => emit_local_outcome_v3(opts, &outcome),
                Err(error) => {
                    crate::ui::error(format!(
                        "verdict: could not persist the timeout outcome: {error}"
                    ));
                    ExitCode::from(75)
                }
            }
        }
    }
}

fn run_legacy_verdict(opts: &VerdictOpts, headers: &[(String, String)]) -> ExitCode {
    // This ordering deliberately mirrors the pre-candidate client: diff and
    // text projection errors surface before base/attempt identity errors.
    let changed = match git_changed_files(&opts.repo, &opts.base) {
        Ok(files) => files,
        Err(error) => {
            crate::ui::error(format!(
                "verdict: git diff against `{}` in `{}` failed: {error}",
                opts.base,
                opts.repo.display()
            ));
            return ExitCode::from(2);
        }
    };
    let repo_relative = opts.server_root.is_some();
    let mut payload = match build_push_payload(&opts.repo, &changed, repo_relative) {
        Ok(payload) => payload,
        Err(error) => {
            crate::ui::error(error.to_string());
            return ExitCode::from(2);
        }
    };
    payload.files.sort_by(|left, right| left.0.cmp(&right.0));

    let resolved_sha = match git_resolve_ref(&opts.repo, &opts.base) {
        Ok(sha) => sha,
        Err(error) => {
            crate::ui::error(format!(
                "verdict: outcome-v3 requires an immutable base SHA, but `{}` \
                 could not be resolved: {error}",
                opts.base
            ));
            return ExitCode::from(2);
        }
    };
    let semantic = match attempt_context_from_env(&resolved_sha) {
        Ok(context) => context,
        Err(error) => {
            crate::ui::error(format!("verdict: invalid outcome-v3 identity: {error}"));
            return ExitCode::from(2);
        }
    };

    // Preserve the pre-candidate rollout behavior exactly: legacy binary or
    // metadata-only changes have no text payload and retain the established
    // local trivial-pass decision until the caller explicitly opts into the
    // complete candidate protocol.
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
        return match local_trivial_outcome_v3(opts, &semantic, &resolved_sha, &changed, &detail) {
            Ok(outcome) => emit_local_outcome_v3(opts, &outcome),
            Err(error) => {
                crate::ui::error(format!(
                    "verdict: could not persist the trivial-pass evidence: {error}"
                ));
                ExitCode::from(75)
            }
        };
    }

    let LegacyVerdictSubmission {
        changed,
        payload,
        body,
    } = build_legacy_verdict_submission(opts, &resolved_sha, &semantic, changed, payload);

    emit_payload_diagnostics(&changed, &payload, body.len());
    if let Err(message) = validate_overlay_http_cap(&body, &payload.content_stats) {
        crate::ui::error(message);
        return ExitCode::from(2);
    }

    let mut endpoints: Vec<(String, HttpClient)> = Vec::with_capacity(opts.remotes.len());
    for remote in &opts.remotes {
        match build_client(remote, opts.auth_token.as_deref(), headers) {
            Ok(client) => endpoints.push((remote.clone(), client)),
            Err(error) => {
                crate::ui::error(format!(
                    "verdict: client init failed for `{remote}`: {error}"
                ));
                return ExitCode::from(2);
            }
        }
    }

    let accepted = match submit_v3_with_failover(&endpoints, &body, &semantic) {
        Ok(accepted) => accepted,
        Err(exhausted) => {
            let reason = format!(
                "no remote accepted the push — {}",
                exhausted.describe_attempts()
            );
            if exhausted.all_unauthorized() {
                crate::ui::error(format!("verdict: {reason}"));
                println!(
                    "{}",
                    serde_json::json!({
                        "schema": "cargoless.protocol-error/v3",
                        "code": "authentication.rejected",
                        "request_id": semantic.request_id.as_str(),
                        "attempt_id": semantic.attempt_id.as_str(),
                        "summary": reason,
                    })
                );
                return ExitCode::from(2);
            }
            return match local_indeterminate_outcome_v3(
                opts,
                &semantic,
                &resolved_sha,
                &changed,
                &reason,
                IndeterminateCause::DependencyUnavailable {
                    component: Component::Protocol,
                },
            ) {
                Ok(outcome) => emit_local_outcome_v3(opts, &outcome),
                Err(error) => {
                    crate::ui::error(format!(
                        "verdict: could not persist the indeterminate outcome: {error}"
                    ));
                    ExitCode::from(75)
                }
            };
        }
    };
    eprintln!(
        "[cargoless:verdict] attempt={} request={} accepted by {}; awaiting exact attempt outcome (timeout {}s)",
        semantic.attempt_id, semantic.request_id, accepted.remote, opts.await_timeout_secs
    );

    match await_attempt_v3(
        accepted.client,
        &semantic.attempt_id,
        accepted.initial,
        opts.await_timeout_secs,
    ) {
        Some(outcome) => emit_daemon_outcome_v3(opts, &outcome, accepted.remote),
        None => {
            let reason = format!(
                "timed out after {}s awaiting exact attempt {} from {}",
                opts.await_timeout_secs, semantic.attempt_id, accepted.remote
            );
            match local_indeterminate_outcome_v3(
                opts,
                &semantic,
                &resolved_sha,
                &changed,
                &reason,
                IndeterminateCause::BudgetExhausted {
                    component: Component::ProjectCheck,
                    budget: NonEmptyText::new(format!(
                        "await_timeout_secs={}",
                        opts.await_timeout_secs
                    ))
                    .expect("timeout budget is non-empty"),
                },
            ) {
                Ok(outcome) => emit_local_outcome_v3(opts, &outcome),
                Err(error) => {
                    crate::ui::error(format!(
                        "verdict: could not persist the timeout outcome: {error}"
                    ));
                    ExitCode::from(75)
                }
            }
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

pub(crate) fn attempt_context_from_env(base_sha: &str) -> Result<AttemptContext, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seed =
        cargoless_core::sha256_hex(format!("{base_sha}:{}:{now}", std::process::id()).as_bytes());
    let request_id = match std::env::var("CARGOLESS_REQUEST_ID") {
        Ok(value) => RequestId::new(value).map_err(|error| error.to_string())?,
        Err(_) => {
            RequestId::new(format!("req.{}", &seed[..24])).map_err(|error| error.to_string())?
        }
    };
    let attempt_id = match std::env::var("CARGOLESS_ATTEMPT_ID") {
        Ok(value) => AttemptId::new(value).map_err(|error| error.to_string())?,
        Err(_) => AttemptId::new(format!("attempt.{}", &seed[8..32]))
            .map_err(|error| error.to_string())?,
    };
    let trace_id = match std::env::var("CARGOLESS_TRACE_ID") {
        Ok(value) => TraceId::new(value).map_err(|error| error.to_string())?,
        Err(_) => TraceId::new(seed[..32].to_string()).map_err(|error| error.to_string())?,
    };
    let parse_u32 = |name: &str, default: u32| -> Result<u32, String> {
        match std::env::var(name) {
            Ok(value) => value
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("{name} must be an unsigned integer")),
            Err(_) => Ok(default),
        }
    };
    let parse_u64 = |name: &str, default: u64| -> Result<u64, String> {
        match std::env::var(name) {
            Ok(value) => value
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("{name} must be an unsigned integer")),
            Err(_) => Ok(default),
        }
    };
    let context = AttemptContext {
        request_id,
        attempt_id,
        trace_id,
        previous_attempt_id: std::env::var("CARGOLESS_PREVIOUS_ATTEMPT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(AttemptId::new)
            .transpose()
            .map_err(|error| error.to_string())?,
        attempt_number: parse_u32("CARGOLESS_ATTEMPT_NUMBER", 1)?,
        maximum_attempts: parse_u32("CARGOLESS_MAX_ATTEMPTS", 3)?,
        retry_after_ms: parse_u64("CARGOLESS_RETRY_AFTER_MS", 10_000)?,
    };
    context.validate().map_err(str::to_string)?;
    Ok(context)
}

fn local_trivial_outcome_v3(
    opts: &VerdictOpts,
    context: &AttemptContext,
    base_sha: &str,
    changed: &[String],
    detail: &str,
) -> Result<OutcomeEnvelope, String> {
    let text = |value: String| NonEmptyText::new(value).map_err(|error| error.to_string());
    let root = std::fs::canonicalize(&opts.repo).unwrap_or_else(|_| opts.repo.clone());
    let mut changed_files: Vec<String> = changed.to_vec();
    changed_files.sort();
    let check_plan = format!("gate={};check_ids={:?}", opts.gate, opts.check_ids);
    let subject = Subject::Overlay {
        repository: text(root.to_string_lossy().into_owned())?,
        worktree_key: text(opts.worktree.clone())?,
        base_ref: text(opts.base.clone())?,
        base_sha: text(base_sha.to_string())?,
        overlay_digest: text(cargoless_core::sha256_hex(b""))?,
        changed_files_digest: text(cargoless_core::sha256_hex(
            changed_files.join("\n").as_bytes(),
        ))?,
        check_plan_digest: text(cargoless_core::sha256_hex(check_plan.as_bytes()))?,
    };
    let now_ms: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let mut bundle = EvidenceBundle::default();
    bundle.push(
        ArtifactKind::Events,
        format!(
            "{{\"at_unix_ms\":{now_ms},\"event\":\"local.trivial_pass\",\
             \"attempt_id\":\"{}\",\"summary\":{}}}\n",
            context.attempt_id,
            serde_json::to_string(detail).map_err(|error| error.to_string())?
        ),
    );
    let store = EvidenceStore::new(root.join(".cargoless"));
    let evidence = store
        .reference_for(&context.attempt_id, &bundle)
        .map_err(|error| error.to_string())?;
    let mut outcome = OutcomeEnvelope::new(
        context.request_id.clone(),
        context.attempt_id.clone(),
        context.trace_id.clone(),
        Surface::Overlay,
        subject,
        Producer {
            daemon_build_id: text(cargoless_core::build_id().to_string())?,
            process_id: std::process::id(),
            process_generation: 1,
            pod_uid: None,
            rust_analyzer_generation: None,
        },
        Conclusion::Passed {
            basis: PassBasis::PolicySatisfied {
                policy: text("trivial.no_content_bearing_changes".to_string())?,
            },
            evidence,
            summary: text(detail.to_string())?,
        },
    );
    outcome.timeline = vec![
        PhaseRecord {
            phase: Phase::Accepted,
            started_at_unix_ms: now_ms,
            finished_at_unix_ms: Some(now_ms),
        },
        PhaseRecord {
            phase: Phase::Terminal,
            started_at_unix_ms: now_ms,
            finished_at_unix_ms: Some(now_ms),
        },
    ];
    store
        .persist(&outcome, EvidenceClass::Success, &bundle)
        .map_err(|error| error.to_string())?;
    Ok(outcome)
}

fn local_indeterminate_outcome_v3(
    opts: &VerdictOpts,
    context: &AttemptContext,
    base_sha: &str,
    changed: &[String],
    reason: &str,
    cause: IndeterminateCause,
) -> Result<OutcomeEnvelope, String> {
    let text = |value: String| NonEmptyText::new(value).map_err(|error| error.to_string());
    let root = std::fs::canonicalize(&opts.repo).unwrap_or_else(|_| opts.repo.clone());
    let mut changed_files: Vec<String> = changed.to_vec();
    changed_files.sort();
    let subject = Subject::Overlay {
        repository: text(root.to_string_lossy().into_owned())?,
        worktree_key: text(opts.worktree.clone())?,
        base_ref: text(opts.base.clone())?,
        base_sha: text(base_sha.to_string())?,
        overlay_digest: text(cargoless_core::sha256_hex(b"client-side-failure"))?,
        changed_files_digest: text(cargoless_core::sha256_hex(
            changed_files.join("\n").as_bytes(),
        ))?,
        check_plan_digest: text(cargoless_core::sha256_hex(
            format!("gate={};check_ids={:?}", opts.gate, opts.check_ids).as_bytes(),
        ))?,
    };
    let now_ms: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let mut bundle = EvidenceBundle::default();
    bundle.push(
        ArtifactKind::Events,
        format!(
            "{{\"at_unix_ms\":{now_ms},\"event\":\"client.indeterminate\",\
             \"attempt_id\":\"{}\",\"summary\":{}}}\n",
            context.attempt_id,
            serde_json::to_string(reason).map_err(|error| error.to_string())?
        ),
    );
    let store = EvidenceStore::new(root.join(".cargoless"));
    let evidence = store
        .reference_for(&context.attempt_id, &bundle)
        .map_err(|error| error.to_string())?;
    let mut outcome = OutcomeEnvelope::new(
        context.request_id.clone(),
        context.attempt_id.clone(),
        context.trace_id.clone(),
        Surface::Overlay,
        subject,
        Producer {
            daemon_build_id: text(cargoless_core::build_id().to_string())?,
            process_id: std::process::id(),
            process_generation: 1,
            pod_uid: None,
            rust_analyzer_generation: None,
        },
        Conclusion::Indeterminate {
            cause,
            retry: RetryDirective::Automatic {
                attempt: context.attempt_number,
                maximum_attempts: context.maximum_attempts,
                after_ms: context.retry_after_ms,
            },
            evidence,
            summary: text(reason.to_string())?,
        },
    );
    outcome.timeline = vec![
        PhaseRecord {
            phase: Phase::Accepted,
            started_at_unix_ms: now_ms,
            finished_at_unix_ms: Some(now_ms),
        },
        PhaseRecord {
            phase: Phase::Terminal,
            started_at_unix_ms: now_ms,
            finished_at_unix_ms: Some(now_ms),
        },
    ];
    store
        .persist(&outcome, EvidenceClass::Terminal, &bundle)
        .map_err(|error| error.to_string())?;
    Ok(outcome)
}

fn emit_local_outcome_v3(opts: &VerdictOpts, outcome: &OutcomeEnvelope) -> ExitCode {
    let reaction = outcome.reaction.clone();
    eprintln!(
        "[cargoless:verdict] attempt={} conclusion={} reaction={:?} source=client",
        outcome.attempt_id,
        outcome.conclusion.semantic_code(),
        reaction.state,
    );
    match opts.output {
        OutputMode::Json => {
            let mut value = serde_json::to_value(outcome)
                .unwrap_or_else(|_| serde_json::json!({"schema":"serialization-error"}));
            if let Some(object) = value.as_object_mut() {
                object.insert("source".into(), serde_json::json!("client"));
                object.insert(
                    "reaction".into(),
                    serde_json::to_value(&reaction)
                        .unwrap_or_else(|_| serde_json::json!({"state":"error"})),
                );
            }
            println!("{value}");
        }
        OutputMode::Text => println!(
            "{}:{}",
            outcome.conclusion.semantic_code(),
            reaction.code.as_str()
        ),
    }
    let exit = match reaction.state {
        CheckState::Success => 0,
        CheckState::Failure => 1,
        CheckState::Pending | CheckState::Error | CheckState::NoUpdate => 75,
    };
    ExitCode::from(exit)
}

struct AcceptedAttempt<'a> {
    remote: &'a str,
    client: &'a HttpClient,
    initial: OutcomeEnvelope,
}

fn submit_v3_with_failover<'a>(
    endpoints: &'a [(String, HttpClient)],
    body: &str,
    context: &AttemptContext,
) -> Result<AcceptedAttempt<'a>, LadderExhausted> {
    let mut attempts = Vec::new();
    for (remote, client) in endpoints {
        match client.submit_attempt_v3(body) {
            Ok(outcome)
                if outcome.request_id == context.request_id
                    && outcome.attempt_id == context.attempt_id
                    && outcome.trace_id == context.trace_id =>
            {
                return Ok(AcceptedAttempt {
                    remote,
                    client,
                    initial: outcome,
                });
            }
            Ok(outcome) => {
                let detail = format!(
                    "daemon returned mismatched identity request={} attempt={} trace={}",
                    outcome.request_id, outcome.attempt_id, outcome.trace_id
                );
                crate::ui::warn(format!("verdict: `{remote}` violated outcome-v3: {detail}"));
                attempts.push((remote.clone(), AttemptFailure::Transport(detail)));
            }
            Err(TransportError::Unauthorized) => {
                crate::ui::warn(format!(
                    "verdict: `{remote}` refused the bearer token; trying next remote"
                ));
                attempts.push((remote.clone(), AttemptFailure::Unauthorized));
            }
            Err(error) => {
                crate::ui::warn(format!(
                    "verdict: outcome-v3 submission to `{remote}` failed ({error}); \
                     trying next remote"
                ));
                attempts.push((remote.clone(), AttemptFailure::Transport(error.to_string())));
            }
        }
    }
    Err(LadderExhausted { attempts })
}

fn await_attempt_v3(
    client: &HttpClient,
    attempt_id: &AttemptId,
    initial: OutcomeEnvelope,
    timeout_secs: u64,
) -> Option<OutcomeEnvelope> {
    if !matches!(initial.conclusion, Conclusion::Pending { .. }) {
        return Some(initial);
    }
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let started = Instant::now();
    while started.elapsed() < timeout {
        match client.get_attempt_v3(attempt_id) {
            Ok(Some(outcome)) if !matches!(outcome.conclusion, Conclusion::Pending { .. }) => {
                return Some(outcome);
            }
            Ok(_) => {}
            Err(error) => {
                crate::ui::warn(format!(
                    "verdict: exact-attempt poll for {attempt_id} failed ({error}); retrying"
                ));
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

fn terminal_evidence_ref(conclusion: &Conclusion) -> Option<&EvidenceRef> {
    match conclusion {
        Conclusion::Pending { .. } => None,
        Conclusion::Passed { evidence, .. }
        | Conclusion::Failed { evidence, .. }
        | Conclusion::Indeterminate { evidence, .. }
        | Conclusion::Rejected { evidence, .. }
        | Conclusion::Cancelled { evidence, .. }
        | Conclusion::Superseded { evidence, .. } => Some(evidence),
    }
}

/// Structurally closed v2 evidence envelope used before the semantic authority
/// checks below. `serde_json::Value` is deliberately not enough here because it
/// erases duplicate keys with last-value-wins behavior.
#[allow(dead_code)]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedCandidateV2Evidence {
    schema: serde_json::Value,
    check_id: serde_json::Value,
    status: serde_json::Value,
    summary: serde_json::Value,
    subject: ClosedCandidateV2Subject,
    findings: serde_json::Value,
    #[serde(default)]
    degradation: Option<serde_json::Value>,
    #[serde(default)]
    metrics: Option<serde_json::Value>,
    #[serde(default)]
    artifacts: Option<serde_json::Value>,
}

#[allow(dead_code)]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedCandidateV2Subject {
    candidate_kind: serde_json::Value,
    candidate_snapshot_digest: serde_json::Value,
    candidate_tree_oid: serde_json::Value,
    #[serde(default)]
    candidate_sha: Option<serde_json::Value>,
    comparison_base_sha: serde_json::Value,
    manifest_digest: serde_json::Value,
    engine: serde_json::Value,
    engine_version: serde_json::Value,
    policy_hash: serde_json::Value,
    #[serde(default)]
    provider: Option<serde_json::Value>,
    #[serde(default)]
    model: Option<serde_json::Value>,
    #[serde(default)]
    model_revision: Option<serde_json::Value>,
    #[serde(default)]
    dimensions: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct CandidateEvidenceMeta {
    schema: String,
    attempt_id: String,
    artifact_digest: String,
    artifacts: Vec<EvidenceInventoryEntry>,
    bundle_artifacts: Vec<EvidenceInventoryEntry>,
    omitted_due_to_cap: Vec<String>,
}

type CandidateEvidenceStatus = ProjectCheckOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateTerminalStatus {
    Passed,
    Failed,
    Indeterminate,
    Rejected,
    Cancelled,
    Superseded,
}

fn candidate_terminal_status(conclusion: &Conclusion) -> Result<CandidateTerminalStatus, String> {
    match conclusion {
        Conclusion::Passed { .. } => Ok(CandidateTerminalStatus::Passed),
        Conclusion::Failed { .. } => Ok(CandidateTerminalStatus::Failed),
        Conclusion::Indeterminate { .. } => Ok(CandidateTerminalStatus::Indeterminate),
        Conclusion::Rejected { .. } => Ok(CandidateTerminalStatus::Rejected),
        Conclusion::Cancelled { .. } => Ok(CandidateTerminalStatus::Cancelled),
        Conclusion::Superseded { .. } => Ok(CandidateTerminalStatus::Superseded),
        Conclusion::Pending { .. } => {
            Err("candidate outcome remained pending before evidence validation".to_string())
        }
    }
}

fn validate_candidate_v2_status_aggregate(
    statuses: &[CandidateEvidenceStatus],
    terminal: CandidateTerminalStatus,
) -> Result<(), String> {
    if statuses.is_empty() {
        return Err("candidate outcome has no verified v2 result statuses".to_string());
    }
    let has_failed = statuses.contains(&CandidateEvidenceStatus::Failed);
    let has_degraded = statuses.contains(&CandidateEvidenceStatus::Degraded);
    let has_indeterminate = statuses.contains(&CandidateEvidenceStatus::Indeterminate);
    match terminal {
        CandidateTerminalStatus::Passed if has_failed || has_indeterminate => Err(
            "passed candidate outcome conflicts with failed or indeterminate v2 evidence"
                .to_string(),
        ),
        CandidateTerminalStatus::Passed => Ok(()),
        CandidateTerminalStatus::Failed if has_indeterminate => Err(
            "failed candidate outcome conflicts with indeterminate v2 evidence".to_string(),
        ),
        CandidateTerminalStatus::Failed if has_failed || has_degraded => Ok(()),
        CandidateTerminalStatus::Failed => Err(
            "failed candidate outcome requires failed or degraded v2 evidence".to_string(),
        ),
        CandidateTerminalStatus::Indeterminate | CandidateTerminalStatus::Rejected
            if has_indeterminate || has_degraded =>
        {
            Ok(())
        }
        CandidateTerminalStatus::Indeterminate | CandidateTerminalStatus::Rejected => Err(
            "indeterminate or rejected candidate outcome requires indeterminate or degraded v2 evidence"
                .to_string(),
        ),
        CandidateTerminalStatus::Cancelled | CandidateTerminalStatus::Superseded => Err(
            "cancelled or superseded candidate outcome cannot produce an accepted typed verdict"
                .to_string(),
        ),
    }
}

fn required_evidence_string<'a>(
    subject: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<&'a str, String> {
    subject
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("candidate v2 evidence field {field} must be a non-empty string"))
}

fn validate_candidate_v2_evidence(
    bytes: &[u8],
    manifest: &CandidateSnapshotManifest,
    expected_check_id: &str,
) -> Result<CandidateEvidenceStatus, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("candidate v2 evidence is not valid JSON: {error}"))?;
    serde_json::from_slice::<ClosedCandidateV2Evidence>(bytes)
        .map_err(|error| format!("candidate v2 evidence must have closed objects: {error}"))?;
    let root = value
        .as_object()
        .ok_or_else(|| "candidate v2 evidence root must be an object".to_string())?;
    if required_evidence_string(root, "schema")? != "cargoless.check-result/v2" {
        return Err("candidate v2 evidence schema must be cargoless.check-result/v2".to_string());
    }
    let check_id = required_evidence_string(root, "check_id")?;
    if check_id != expected_check_id {
        return Err(format!(
            "candidate v2 evidence check_id mismatch: expected {expected_check_id:?}, got {check_id:?}"
        ));
    }
    let semantics = validate_structured_check_result_semantics(&value)
        .map_err(|(code, message)| format!("candidate v2 evidence {code}: {message}"))?;
    let status = semantics.outcome;
    required_evidence_string(root, "summary")?;
    let subject = root
        .get("subject")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "candidate v2 evidence subject must be an object".to_string())?;

    for (field, expected) in [
        ("candidate_kind", manifest.candidate.kind()),
        (
            "candidate_snapshot_digest",
            manifest.candidate.snapshot_digest(),
        ),
        ("candidate_tree_oid", manifest.candidate.tree_oid()),
        (
            "comparison_base_sha",
            manifest.comparison_base.commit_sha.as_str(),
        ),
        ("manifest_digest", manifest.manifest_digest.as_str()),
    ] {
        let actual = required_evidence_string(subject, field)?;
        if actual != expected {
            return Err(format!(
                "candidate v2 evidence {field} mismatch: expected {expected:?}, got {actual:?}"
            ));
        }
    }

    for field in ["engine", "engine_version", "policy_hash"] {
        required_evidence_string(subject, field)?;
    }

    let expected_candidate_sha = match &manifest.candidate {
        CandidateSnapshot::Tree { commit_sha, .. } => Some(commit_sha.as_str()),
        CandidateSnapshot::Index { .. } | CandidateSnapshot::Overlay { .. } => None,
    };
    let actual_candidate_sha = match subject.get("candidate_sha") {
        None => None,
        Some(serde_json::Value::Null) => {
            return Err(
                "candidate v2 evidence candidate_sha must be absent when the candidate has no commit SHA"
                    .to_string(),
            );
        }
        Some(serde_json::Value::String(value)) if !value.trim().is_empty() => Some(value.as_str()),
        Some(_) => {
            return Err(
                "candidate v2 evidence candidate_sha must be a non-empty string or be absent"
                    .to_string(),
            );
        }
    };
    if actual_candidate_sha != expected_candidate_sha {
        return Err(format!(
            "candidate v2 evidence candidate_sha mismatch: expected {expected_candidate_sha:?}, got {actual_candidate_sha:?}"
        ));
    }
    Ok(status)
}

fn candidate_v2_evidence_artifact(sequence: usize) -> String {
    format!("project-check-result-{sequence:03}.json")
}

#[cfg(test)]
fn validate_candidate_v2_evidence_sequence<F>(
    manifest: &CandidateSnapshotManifest,
    expected_check_ids: &[String],
    terminal: CandidateTerminalStatus,
    mut fetch: F,
) -> Result<Vec<CandidateEvidenceStatus>, String>
where
    F: FnMut(&str) -> Result<Option<Vec<u8>>, String>,
{
    let canonical = canonical_candidate_check_ids(expected_check_ids);
    if canonical.is_empty() {
        return Err("candidate evidence requires at least one expected check_id".to_string());
    }
    if canonical.as_slice() != expected_check_ids {
        return Err(
            "candidate evidence check_ids must be sorted, unique, and non-empty".to_string(),
        );
    }
    if canonical.len() > MAX_CANDIDATE_V2_EVIDENCE_ARTIFACTS {
        return Err(format!(
            "candidate evidence exceeds the {MAX_CANDIDATE_V2_EVIDENCE_ARTIFACTS}-artifact limit"
        ));
    }

    let mut statuses = Vec::with_capacity(canonical.len());
    for (index, expected_check_id) in canonical.iter().enumerate() {
        let artifact = candidate_v2_evidence_artifact(index + 1);
        let bytes = fetch(&artifact)?.ok_or_else(|| {
            format!(
                "candidate outcome omitted required evidence artifact {artifact} for check_id {expected_check_id:?}"
            )
        })?;
        statuses.push(validate_candidate_v2_evidence(
            &bytes,
            manifest,
            expected_check_id,
        )?);
    }

    if canonical.len() < MAX_CANDIDATE_V2_EVIDENCE_ARTIFACTS {
        let extra = candidate_v2_evidence_artifact(canonical.len() + 1);
        if fetch(&extra)?.is_some() {
            return Err(format!(
                "candidate outcome contains extra unrequested evidence artifact {extra}"
            ));
        }
    }
    validate_candidate_v2_status_aggregate(&statuses, terminal)?;
    Ok(statuses)
}

fn candidate_v2_evidence_artifact_sequence(name: &str) -> Option<usize> {
    let digits = name
        .strip_prefix("project-check-result-")?
        .strip_suffix(".json")?;
    (digits.len() == 3 && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| digits.parse::<usize>().ok())
        .flatten()
        .filter(|sequence| *sequence > 0)
}

fn validate_candidate_v2_evidence_bundle<F>(
    meta_bytes: &[u8],
    evidence_sha: &str,
    attempt_id: &AttemptId,
    manifest: &CandidateSnapshotManifest,
    expected_check_ids: &[String],
    terminal: CandidateTerminalStatus,
    mut fetch: F,
) -> Result<Vec<CandidateEvidenceStatus>, String>
where
    F: FnMut(&str) -> Result<Option<Vec<u8>>, String>,
{
    let meta: CandidateEvidenceMeta = serde_json::from_slice(meta_bytes)
        .map_err(|error| format!("candidate evidence meta.json is invalid: {error}"))?;
    if meta.schema != "cargoless.evidence/v3" {
        return Err(format!(
            "candidate evidence meta.json schema mismatch: expected cargoless.evidence/v3, got {:?}",
            meta.schema
        ));
    }
    if meta.attempt_id != attempt_id.as_str() {
        return Err(format!(
            "candidate evidence meta.json attempt_id mismatch: expected {:?}, got {:?}",
            attempt_id.as_str(),
            meta.attempt_id
        ));
    }
    let mut bundle_by_name = BTreeMap::new();
    for artifact in &meta.bundle_artifacts {
        if bundle_by_name
            .insert(artifact.name.as_str(), artifact)
            .is_some()
        {
            return Err(format!(
                "candidate evidence meta.json contains duplicate bundle artifact {:?}",
                artifact.name
            ));
        }
    }
    let canonical_digest = canonical_evidence_bundle_digest(&meta.bundle_artifacts);
    if canonical_digest != meta.artifact_digest {
        return Err(format!(
            "candidate evidence meta.json canonical bundle digest mismatch: recomputed {:?}, declared {:?}",
            canonical_digest, meta.artifact_digest
        ));
    }
    if canonical_digest != evidence_sha {
        return Err(format!(
            "candidate evidence meta.json artifact_digest canonical value does not match EvidenceRef sha: expected {:?}, recomputed {:?}",
            evidence_sha, canonical_digest
        ));
    }

    let mut persisted_by_name = BTreeMap::new();
    for artifact in &meta.artifacts {
        if persisted_by_name
            .insert(artifact.name.as_str(), artifact)
            .is_some()
        {
            return Err(format!(
                "candidate evidence meta.json contains duplicate persisted artifact {:?}",
                artifact.name
            ));
        }
        if bundle_by_name.get(artifact.name.as_str()).copied() != Some(artifact) {
            return Err(format!(
                "candidate evidence persisted artifact {:?} does not match the canonical bundle inventory",
                artifact.name
            ));
        }
    }
    let mut omitted = BTreeSet::new();
    for name in &meta.omitted_due_to_cap {
        if !omitted.insert(name.as_str()) {
            return Err(format!(
                "candidate evidence meta.json contains duplicate omitted artifact {name:?}"
            ));
        }
        if name.starts_with("project-check-result-") {
            return Err(format!(
                "candidate evidence meta.json omitted required project result {name}"
            ));
        }
        if !bundle_by_name.contains_key(name.as_str())
            || persisted_by_name.contains_key(name.as_str())
        {
            return Err(format!(
                "candidate evidence omitted artifact {name:?} is not an omitted-only member of the canonical bundle inventory"
            ));
        }
    }
    for name in bundle_by_name.keys() {
        if !persisted_by_name.contains_key(name) && !omitted.contains(name) {
            return Err(format!(
                "candidate evidence canonical bundle artifact {name:?} is neither persisted nor declared omitted"
            ));
        }
    }

    let canonical = canonical_candidate_check_ids(expected_check_ids);
    if canonical.is_empty() {
        return Err("candidate evidence requires at least one expected check_id".to_string());
    }
    if canonical.as_slice() != expected_check_ids {
        return Err(
            "candidate evidence check_ids must be sorted, unique, and non-empty".to_string(),
        );
    }
    if canonical.len() > MAX_CANDIDATE_V2_EVIDENCE_ARTIFACTS {
        return Err(format!(
            "candidate evidence exceeds the {MAX_CANDIDATE_V2_EVIDENCE_ARTIFACTS}-artifact limit"
        ));
    }

    let expected_artifacts = (1..=canonical.len())
        .map(candidate_v2_evidence_artifact)
        .collect::<Vec<_>>();
    let mut actual_artifacts = Vec::new();
    for artifact in &meta.bundle_artifacts {
        if artifact.name.starts_with("project-check-result-") {
            candidate_v2_evidence_artifact_sequence(&artifact.name).ok_or_else(|| {
                format!(
                    "candidate evidence meta.json contains invalid project result artifact {:?}",
                    artifact.name
                )
            })?;
            actual_artifacts.push(artifact.name.clone());
        }
    }
    if actual_artifacts != expected_artifacts {
        return Err(format!(
            "candidate evidence project result sequence mismatch: expected {expected_artifacts:?}, got {actual_artifacts:?}"
        ));
    }

    let mut seen = BTreeSet::new();
    let mut statuses = Vec::with_capacity(canonical.len());
    for artifact in &meta.artifacts {
        if !seen.insert(artifact.name.as_str()) {
            return Err(format!(
                "candidate evidence meta.json contains duplicate artifact {:?}",
                artifact.name
            ));
        }
        let bytes = fetch(&artifact.name)?.ok_or_else(|| {
            format!(
                "candidate evidence meta.json enumerates missing artifact {:?}",
                artifact.name
            )
        })?;
        if bytes.len() as u64 != artifact.bytes {
            return Err(format!(
                "candidate evidence artifact {:?} byte length mismatch: expected {}, got {}",
                artifact.name,
                artifact.bytes,
                bytes.len()
            ));
        }
        let actual_sha = cargoless_core::sha256_hex(&bytes);
        if actual_sha != artifact.sha256 {
            return Err(format!(
                "candidate evidence artifact {:?} sha256 mismatch: expected {:?}, got {:?}",
                artifact.name, artifact.sha256, actual_sha
            ));
        }
        if let Some(sequence) = candidate_v2_evidence_artifact_sequence(&artifact.name) {
            statuses.push(validate_candidate_v2_evidence(
                &bytes,
                manifest,
                &canonical[sequence - 1],
            )?);
        }
    }
    validate_candidate_v2_status_aggregate(&statuses, terminal)?;
    Ok(statuses)
}

fn retrieve_candidate_v2_evidence(
    client: &HttpClient,
    outcome: &OutcomeEnvelope,
    manifest: &CandidateSnapshotManifest,
    expected_check_ids: &[String],
) -> Result<Vec<CandidateEvidenceStatus>, String> {
    let evidence = terminal_evidence_ref(&outcome.conclusion)
        .ok_or_else(|| "candidate outcome remained pending without durable evidence".to_string())?;
    if !matches!(&evidence.availability, EvidenceAvailability::Durable) {
        return Err("candidate outcome evidence is not durable".to_string());
    }
    let expected_id = format!("ev.{}", outcome.attempt_id);
    if evidence.evidence_id.as_str() != expected_id {
        return Err(format!(
            "candidate outcome evidence ID mismatch: expected {expected_id:?}, got {:?}",
            evidence.evidence_id.as_str()
        ));
    }
    let expected_uri = format!("/v3/attempts/{}/evidence", outcome.attempt_id);
    if evidence.relative_uri.as_str() != expected_uri.as_str() {
        return Err(format!(
            "candidate outcome evidence URI mismatch: expected {expected_uri:?}, got {:?}",
            evidence.relative_uri.as_str()
        ));
    }
    let meta = client
        .get_attempt_evidence_v3(&outcome.attempt_id, "meta.json")
        .map_err(|error| format!("candidate evidence meta.json retrieval failed: {error}"))?
        .ok_or_else(|| "candidate outcome omitted required evidence meta.json".to_string())?;
    let terminal = candidate_terminal_status(&outcome.conclusion)?;
    validate_candidate_v2_evidence_bundle(
        &meta,
        evidence.sha256.as_str(),
        &outcome.attempt_id,
        manifest,
        expected_check_ids,
        terminal,
        |artifact| {
            client
                .get_attempt_evidence_v3(&outcome.attempt_id, artifact)
                .map_err(|error| format!("candidate v2 evidence retrieval failed: {error}"))
        },
    )
}

fn emit_candidate_evidence_error(
    opts: &VerdictOpts,
    outcome: &OutcomeEnvelope,
    remote: &str,
    error: &str,
) -> ExitCode {
    crate::ui::error(format!(
        "verdict: exact candidate evidence is invalid: {error}"
    ));
    match opts.output {
        OutputMode::Json => println!(
            "{}",
            serde_json::json!({
                "schema": "cargoless.protocol-error/v3",
                "code": "candidate_snapshot.evidence_invalid",
                "request_id": outcome.request_id.as_str(),
                "attempt_id": outcome.attempt_id.as_str(),
                "remote": remote,
                "artifact": CANDIDATE_V2_EVIDENCE_ARTIFACT_PATTERN,
                "summary": error,
            })
        ),
        OutputMode::Text => println!("error:candidate_snapshot.evidence_invalid"),
    }
    ExitCode::from(75)
}

fn emit_daemon_outcome_v3(opts: &VerdictOpts, outcome: &OutcomeEnvelope, remote: &str) -> ExitCode {
    let reaction = outcome.reaction.clone();
    eprintln!(
        "[cargoless:verdict] attempt={} conclusion={} reaction={:?} code={} via {}",
        outcome.attempt_id,
        outcome.conclusion.semantic_code(),
        reaction.state,
        reaction.code.as_str(),
        remote
    );
    match opts.output {
        OutputMode::Json => {
            let mut value = serde_json::to_value(outcome)
                .unwrap_or_else(|_| serde_json::json!({"schema":"serialization-error"}));
            if let Some(object) = value.as_object_mut() {
                object.insert("source".into(), serde_json::json!("daemon"));
                object.insert("remote".into(), serde_json::json!(remote));
                object.insert(
                    "reaction".into(),
                    serde_json::to_value(&reaction)
                        .unwrap_or_else(|_| serde_json::json!({"state":"error"})),
                );
            }
            println!("{value}");
        }
        OutputMode::Text => println!(
            "{}:{}",
            outcome.conclusion.semantic_code(),
            reaction.code.as_str()
        ),
    }
    let exit = match reaction.state {
        CheckState::Success => 0,
        CheckState::Failure => 1,
        CheckState::Pending | CheckState::Error | CheckState::NoUpdate => 75,
    };
    ExitCode::from(exit)
}

/// A push the ladder landed: which remote took it, the client pinned to
/// that remote for the await, and the freshness guard captured BEFORE
/// the push (so a pre-existing stale publication cannot satisfy the
/// freshness arm of the acceptance predicate).
#[cfg(test)]
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
    #[cfg(test)]
    Rejected,
}

impl AttemptFailure {
    fn describe(&self) -> String {
        match self {
            AttemptFailure::Transport(e) => format!("transport error: {e}"),
            AttemptFailure::Unauthorized => "unauthorized (401)".to_string(),
            #[cfg(test)]
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
#[cfg(test)]
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
/// * both sides carry a SHA and they MATCH ⇒ accept, freshness ignored
///   (idempotent re-run fast-path — same key + same SHA ⇒ same overlay
///   content ⇒ same verdict);
/// * both carry a SHA and they MISMATCH ⇒ never accept (another
///   branch's verdict on a shared key, or a stale prior publication
///   mid-replacement);
/// * either side lacks a SHA ⇒ freshness-only (legacy daemons that do
///   not echo `base_sha`, or an unresolvable local ref). Freshness
///   means "published after OUR accepted push", which on a single
///   per-key publication stream attributes the verdict to our overlay.
#[cfg(test)]
fn status_is_acceptable(
    status: &WorktreeStatus,
    resolved_sha: Option<&str>,
    freshness: AwaitFreshness,
) -> bool {
    match (resolved_sha, status.base_sha.as_deref()) {
        (Some(mine), Some(theirs)) => mine == theirs,
        _ => freshness.is_fresh(status.published_at),
    }
}

/// Poll `get_status` on the pinned client until the acceptance
/// predicate passes or the wall clock runs out. Poll errors warn and
/// keep polling — transient drops mid-await must not abandon a verdict
/// the daemon is still computing (and failing over mid-await would poll
/// a shard that never saw the push).
#[cfg(test)]
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
        // Path D (addressing) — when we know the SHA we're asking about,
        // ask the daemon for THAT SHA's verdict via `verdict_history`
        // rather than the last-publisher cache. The `status_is_acceptable`
        // predicate is kept as belt-and-suspenders (harmless when the
        // server always echoes the requested sha).
        let poll = match resolved_sha {
            Some(_) => client.get_status_attributed(worktree, resolved_sha),
            None => client.get_status(worktree),
        };
        match poll {
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

#[cfg(test)]
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

#[cfg(test)]
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
#[cfg(test)]
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
    use cargoless_core::{CandidateSnapshot, OverlayOperation, decode_overlay_payload};
    use std::path::Path;
    use std::process::Command;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "cargoless-verdict-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            git(&root, &["init", "-q"]);
            git(&root, &["config", "user.name", "Verdict Candidate Test"]);
            git(
                &root,
                &["config", "user.email", "verdict-candidate@example.invalid"],
            );
            git(&root, &["config", "commit.gpgsign", "false"]);
            git(&root, &["config", "core.hooksPath", "/dev/null"]);
            git(&root, &["config", "core.autocrlf", "false"]);
            git(&root, &["config", "core.filemode", "true"]);
            Self(root)
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl std::ops::Deref for TempRepo {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn write(root: &Path, rel: &str, bytes: impl AsRef<[u8]>) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn candidate_context() -> AttemptContext {
        AttemptContext {
            request_id: RequestId::new("req.verdict-candidate").unwrap(),
            attempt_id: AttemptId::new("attempt.verdict-candidate.1").unwrap(),
            trace_id: TraceId::new("0123456789abcdef0123456789abcdef").unwrap(),
            previous_attempt_id: None,
            attempt_number: 1,
            maximum_attempts: 3,
            retry_after_ms: 10_000,
        }
    }

    fn candidate_evidence(
        manifest: &CandidateSnapshotManifest,
        check_id: &str,
        status: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "schema": "cargoless.check-result/v2",
            "check_id": check_id,
            "status": status,
            "summary": "candidate verified",
            "subject": {
                "candidate_kind": "overlay",
                "candidate_snapshot_digest": manifest.candidate.snapshot_digest(),
                "candidate_tree_oid": manifest.candidate.tree_oid(),
                "comparison_base_sha": manifest.comparison_base.commit_sha.as_str(),
                "manifest_digest": manifest.manifest_digest.as_str(),
                "engine": "test",
                "engine_version": "1",
                "policy_hash": "policy-1"
            },
            "findings": []
        })
    }

    fn candidate_evidence_meta(
        attempt_id: &AttemptId,
        artifact_digest: &str,
        documents: &std::collections::BTreeMap<String, Vec<u8>>,
        omitted_due_to_cap: &[&str],
    ) -> Vec<u8> {
        let artifacts = documents
            .iter()
            .map(|(name, bytes)| {
                serde_json::json!({
                    "name": name,
                    "bytes": bytes.len(),
                    "sha256": cargoless_core::sha256_hex(bytes),
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_vec(&serde_json::json!({
            "schema": "cargoless.evidence/v3",
            "attempt_id": attempt_id.as_str(),
            "class": "success",
            "created_at_unix": 1,
            "artifact_digest": artifact_digest,
            "bytes": documents.values().map(Vec::len).sum::<usize>(),
            "artifacts": artifacts.clone(),
            "bundle_artifacts": artifacts,
            "omitted_due_to_cap": omitted_due_to_cap,
        }))
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn verdict_candidate_submission_reuses_push_snapshot_projection_and_changed_hints() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempRepo::new("typed-overlay");
        write(&root, "text.txt", b"base text\n");
        write(&root, "delete.txt", b"delete me\n");
        write(&root, "mode.sh", b"#!/bin/sh\necho mode\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);

        write(&root, "text.txt", b"candidate text\n");
        std::fs::remove_file(root.join("delete.txt")).unwrap();
        write(&root, "binary.bin", [0x00, 0xff, b'B', 0x80]);
        let mode_path = root.join("mode.sh");
        let mut permissions = std::fs::metadata(&mode_path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&mode_path, permissions).unwrap();

        let direct = crate::candidate_snapshot_git::build_overlay_manifest(&root, "HEAD")
            .unwrap()
            .expect("fixture has a candidate")
            .manifest;
        let opts = VerdictOpts {
            remotes: vec!["http://127.0.0.1:8787".into()],
            headers: Vec::new(),
            output: OutputMode::Json,
            auth_token: None,
            repo: root.to_path_buf(),
            worktree: "candidate-wt".into(),
            base: "HEAD".into(),
            server_root: Some(PathBuf::from("/srv/tf-multiverse")),
            gate: true,
            check_ids: vec!["candidate-v2".into()],
            candidate_snapshot: true,
            await_timeout_secs: 30,
        };
        let context = candidate_context();

        let submission = build_verdict_candidate_submission(&opts, &context)
            .unwrap()
            .expect("typed candidate submission");

        assert_eq!(
            submission.manifest, direct,
            "push and verdict share one builder"
        );
        assert_eq!(
            submission.options.candidate_snapshot.as_ref(),
            Some(&submission.manifest)
        );
        assert_eq!(
            submission.options.comparison_base_sha.as_deref(),
            Some(submission.manifest.comparison_base.commit_sha.as_str())
        );
        assert_eq!(submission.options.semantic.as_ref(), Some(&context));
        assert_eq!(
            submission.options.base_sha,
            submission.options.comparison_base_sha
        );
        assert_eq!(submission.options.source_ref, None);
        assert_eq!(submission.options.source_sha, None);
        assert!(submission.options.repo_relative);
        assert_eq!(
            submission.options.analysis_root.as_deref(),
            Some("/srv/tf-multiverse")
        );

        for path in ["binary.bin", "delete.txt", "mode.sh", "text.txt"] {
            assert!(submission.changed.iter().any(|changed| changed == path));
        }
        assert_eq!(
            submission.options.changed_files.as_ref(),
            Some(&submission.changed)
        );
        assert_eq!(submission.payload.trigger_paths, submission.changed);

        let projected = submission
            .payload
            .files
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            projected.get("text.txt").map(String::as_str),
            Some("candidate text\n")
        );
        assert_eq!(projected.get("delete.txt").map(String::as_str), Some(""));
        assert_eq!(
            projected.get("mode.sh").map(String::as_str),
            Some("#!/bin/sh\necho mode\n")
        );
        assert!(!projected.contains_key("binary.bin"));

        let CandidateSnapshot::Overlay { operations, .. } = &submission.manifest.candidate else {
            panic!("verdict candidate must be an overlay");
        };
        assert!(operations.iter().any(|operation| matches!(
            operation,
            OverlayOperation::Delete { path, .. } if path == "delete.txt"
        )));
        assert!(operations.iter().any(|operation| matches!(
            operation,
            OverlayOperation::Upsert { path, mode, .. }
                if path == "mode.sh" && mode == "100755"
        )));
        let binary = operations
            .iter()
            .find(|operation| operation.path() == "binary.bin")
            .expect("binary upsert");
        let OverlayOperation::Upsert { payload, .. } = binary else {
            panic!("binary candidate must be an upsert");
        };
        assert_eq!(
            decode_overlay_payload(payload).unwrap(),
            [0x00, 0xff, b'B', 0x80]
        );
    }

    #[test]
    fn legacy_verdict_submission_preserves_the_v1_request_shape() {
        let root = TempRepo::new("legacy-request");
        write(&root, "text.txt", b"base\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);
        write(&root, "text.txt", b"candidate\n");
        let opts = VerdictOpts {
            remotes: vec!["http://127.0.0.1:8787".into()],
            headers: Vec::new(),
            output: OutputMode::Json,
            auth_token: None,
            repo: root.to_path_buf(),
            worktree: "legacy-wt".into(),
            base: "HEAD".into(),
            server_root: Some(PathBuf::from("/srv/tf-multiverse")),
            gate: true,
            check_ids: vec!["legacy-v1".into()],
            candidate_snapshot: false,
            await_timeout_secs: 30,
        };
        let resolved_sha = git_resolve_ref(&root, "HEAD").unwrap();
        let context = candidate_context();

        let expected_changed = crate::push::git_changed_files(&root, "HEAD").unwrap();
        let mut expected_payload =
            crate::push::build_push_payload(&root, &expected_changed, true).unwrap();
        expected_payload
            .files
            .sort_by(|left, right| left.0.cmp(&right.0));
        let expected_options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some("/srv/tf-multiverse".into()),
            changed_files: Some(expected_payload.trigger_paths.clone()),
            gate: true,
            check_ids: Some(vec!["legacy-v1".into()]),
            base_sha: Some(resolved_sha.clone()),
            semantic: Some(context.clone()),
            ..PushOverlayOptions::default()
        };
        let expected_body = push_overlay_request_body(
            "legacy-wt",
            "HEAD",
            &expected_payload.files,
            None,
            Some(&expected_options),
        );
        let submission = build_legacy_verdict_submission(
            &opts,
            &resolved_sha,
            &context,
            expected_changed.clone(),
            expected_payload.clone(),
        );

        assert_eq!(submission.changed, expected_changed);
        assert_eq!(submission.payload, expected_payload);
        assert_eq!(submission.body, expected_body);
    }

    #[test]
    fn candidate_v2_evidence_validation_is_bound_to_the_sent_manifest() {
        let root = TempRepo::new("v2-evidence");
        write(&root, "base.txt", b"base\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);
        write(&root, "candidate.txt", b"candidate\n");
        let manifest = crate::candidate_snapshot_git::build_overlay_manifest(&root, "HEAD")
            .unwrap()
            .expect("fixture has a candidate")
            .manifest;
        let evidence = candidate_evidence(&manifest, "candidate-v2", "passed");
        let bytes = serde_json::to_vec(&evidence).unwrap();
        validate_candidate_v2_evidence(&bytes, &manifest, "candidate-v2")
            .expect("matching v2 authority");

        let canonical = serde_json::to_string(&evidence).unwrap();
        let duplicate_root =
            canonical.replacen('{', r#"{"schema":"cargoless.check-result/v2","#, 1);
        let duplicate_subject = canonical.replacen(
            r#""subject":{"#,
            &format!(
                r#""subject":{{"manifest_digest":"{}","#,
                manifest.manifest_digest
            ),
            1,
        );
        let mut unknown_root = evidence.clone();
        unknown_root
            .as_object_mut()
            .unwrap()
            .insert("unexpected_root".into(), serde_json::json!(true));
        let mut unknown_subject = evidence.clone();
        unknown_subject["subject"]
            .as_object_mut()
            .unwrap()
            .insert("unexpected_subject".into(), serde_json::json!(true));

        for (case, bytes, expected) in [
            (
                "duplicate root",
                duplicate_root.into_bytes(),
                "duplicate field",
            ),
            (
                "duplicate subject",
                duplicate_subject.into_bytes(),
                "duplicate field",
            ),
            (
                "unknown root",
                serde_json::to_vec(&unknown_root).unwrap(),
                "unknown field",
            ),
            (
                "unknown subject",
                serde_json::to_vec(&unknown_subject).unwrap(),
                "unknown field",
            ),
        ] {
            let error = validate_candidate_v2_evidence(&bytes, &manifest, "candidate-v2")
                .expect_err("v2 root and subject objects must be closed");
            assert!(
                error.contains(expected),
                "{case} must report {expected:?}, got {error:?}"
            );
        }

        let mut null_candidate_sha = evidence.clone();
        null_candidate_sha["subject"]["candidate_sha"] = serde_json::Value::Null;
        let error = validate_candidate_v2_evidence(
            &serde_json::to_vec(&null_candidate_sha).unwrap(),
            &manifest,
            "candidate-v2",
        )
        .unwrap_err();
        assert!(error.contains("candidate_sha"), "{error}");

        let mut mismatched = evidence;
        mismatched["subject"]["manifest_digest"] = serde_json::json!(
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        );
        let error = validate_candidate_v2_evidence(
            &serde_json::to_vec(&mismatched).unwrap(),
            &manifest,
            "candidate-v2",
        )
        .unwrap_err();
        assert!(error.contains("manifest_digest"), "{error}");
    }

    #[test]
    fn candidate_v2_evidence_requires_protocol_and_producer_fields() {
        let root = TempRepo::new("v2-required");
        write(&root, "base.txt", b"base\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);
        write(&root, "candidate.txt", b"candidate\n");
        let manifest = crate::candidate_snapshot_git::build_overlay_manifest(&root, "HEAD")
            .unwrap()
            .expect("fixture has a candidate")
            .manifest;
        let evidence = candidate_evidence(&manifest, "candidate-v2", "passed");

        for field in [
            "schema", "check_id", "status", "summary", "subject", "findings",
        ] {
            let mut missing = evidence.clone();
            missing.as_object_mut().unwrap().remove(field);
            let error = validate_candidate_v2_evidence(
                &serde_json::to_vec(&missing).unwrap(),
                &manifest,
                "candidate-v2",
            )
            .expect_err("required v2 root field must not default");
            assert!(error.contains(field), "missing {field}: {error}");
        }

        for field in ["engine", "engine_version", "policy_hash"] {
            let mut missing = evidence.clone();
            missing["subject"].as_object_mut().unwrap().remove(field);
            let error = validate_candidate_v2_evidence(
                &serde_json::to_vec(&missing).unwrap(),
                &manifest,
                "candidate-v2",
            )
            .expect_err("required producer field must not default");
            assert!(error.contains(field), "missing {field}: {error}");

            let mut empty = evidence.clone();
            empty["subject"][field] = serde_json::json!("");
            let error = validate_candidate_v2_evidence(
                &serde_json::to_vec(&empty).unwrap(),
                &manifest,
                "candidate-v2",
            )
            .expect_err("producer field must be non-empty");
            assert!(error.contains(field), "empty {field}: {error}");
        }

        let mut wrong_check = evidence;
        wrong_check["check_id"] = serde_json::json!("different-check");
        let error = validate_candidate_v2_evidence(
            &serde_json::to_vec(&wrong_check).unwrap(),
            &manifest,
            "candidate-v2",
        )
        .expect_err("artifact check_id must match the requested check");
        assert!(error.contains("check_id"), "{error}");
    }

    #[test]
    fn candidate_v2_evidence_reuses_structured_result_semantics() {
        let root = TempRepo::new("v2-semantics");
        write(&root, "base.txt", b"base\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);
        write(&root, "candidate.txt", b"candidate\n");
        let manifest = crate::candidate_snapshot_git::build_overlay_manifest(&root, "HEAD")
            .unwrap()
            .expect("fixture has a candidate")
            .manifest;
        let valid_finding = serde_json::json!({
            "fingerprint": "finding-1",
            "blocking": true,
            "severity": "error",
            "code": "policy.blocked",
            "message": "the candidate violates policy"
        });

        for required in ["fingerprint", "code", "message"] {
            let mut malformed = candidate_evidence(&manifest, "candidate-v2", "failed");
            let mut finding = valid_finding.clone();
            finding.as_object_mut().unwrap().remove(required);
            malformed["findings"] = serde_json::json!([finding]);
            let error = validate_candidate_v2_evidence(
                &serde_json::to_vec(&malformed).unwrap(),
                &manifest,
                "candidate-v2",
            )
            .expect_err("every finding requires its stable identity and diagnostic fields");
            assert!(error.contains(required), "missing {required}: {error}");
        }

        let mut passed_blocking = candidate_evidence(&manifest, "candidate-v2", "passed");
        passed_blocking["findings"] = serde_json::json!([valid_finding]);
        let error = validate_candidate_v2_evidence(
            &serde_json::to_vec(&passed_blocking).unwrap(),
            &manifest,
            "candidate-v2",
        )
        .expect_err("passed cannot carry a blocking finding");
        assert!(error.contains("blocking"), "{error}");

        let failed_without_blocking = candidate_evidence(&manifest, "candidate-v2", "failed");
        let error = validate_candidate_v2_evidence(
            &serde_json::to_vec(&failed_without_blocking).unwrap(),
            &manifest,
            "candidate-v2",
        )
        .expect_err("failed requires at least one blocking finding");
        assert!(error.contains("blocking"), "{error}");

        let degraded_without_detail = candidate_evidence(&manifest, "candidate-v2", "degraded");
        let error = validate_candidate_v2_evidence(
            &serde_json::to_vec(&degraded_without_detail).unwrap(),
            &manifest,
            "candidate-v2",
        )
        .expect_err("degraded requires a valid degradation object");
        assert!(error.contains("degradation"), "{error}");
    }

    #[test]
    fn candidate_v2_artifacts_are_complete_ordered_and_terminal_consistent() {
        let root = TempRepo::new("v2-sequence");
        write(&root, "base.txt", b"base\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);
        write(&root, "candidate.txt", b"candidate\n");
        let manifest = crate::candidate_snapshot_git::build_overlay_manifest(&root, "HEAD")
            .unwrap()
            .expect("fixture has a candidate")
            .manifest;
        let expected = vec!["policy-a".to_string(), "policy-b".to_string()];
        let documents = [
            (
                "project-check-result-001.json".to_string(),
                serde_json::to_vec(&candidate_evidence(&manifest, "policy-a", "passed")).unwrap(),
            ),
            (
                "project-check-result-002.json".to_string(),
                serde_json::to_vec(&candidate_evidence(&manifest, "policy-b", "skipped")).unwrap(),
            ),
        ]
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();

        validate_candidate_v2_evidence_sequence(
            &manifest,
            &expected,
            CandidateTerminalStatus::Passed,
            |name| Ok(documents.get(name).cloned()),
        )
        .expect("every requested check has one ordered, success-consistent artifact");

        let missing = validate_candidate_v2_evidence_sequence(
            &manifest,
            &expected,
            CandidateTerminalStatus::Passed,
            |name| Ok((name.ends_with("001.json")).then(|| documents[name].clone())),
        )
        .expect_err("a gap in the verified artifact sequence must fail closed");
        assert!(missing.contains("002"), "{missing}");

        let mut extra = documents.clone();
        extra.insert(
            "project-check-result-003.json".to_string(),
            serde_json::to_vec(&candidate_evidence(&manifest, "policy-c", "passed")).unwrap(),
        );
        let error = validate_candidate_v2_evidence_sequence(
            &manifest,
            &expected,
            CandidateTerminalStatus::Passed,
            |name| Ok(extra.get(name).cloned()),
        )
        .expect_err("unrequested verified artifacts must fail closed");
        assert!(error.contains("extra"), "{error}");

        let mut wrong_order = documents.clone();
        wrong_order.insert(
            "project-check-result-001.json".to_string(),
            serde_json::to_vec(&candidate_evidence(&manifest, "policy-b", "passed")).unwrap(),
        );
        let error = validate_candidate_v2_evidence_sequence(
            &manifest,
            &expected,
            CandidateTerminalStatus::Passed,
            |name| Ok(wrong_order.get(name).cloned()),
        )
        .expect_err("artifact sequence must match sorted requested ids");
        assert!(error.contains("check_id"), "{error}");

        for (terminal, statuses, accepted) in [
            (
                CandidateTerminalStatus::Passed,
                vec![
                    CandidateEvidenceStatus::Passed,
                    CandidateEvidenceStatus::Degraded,
                ],
                true,
            ),
            (
                CandidateTerminalStatus::Passed,
                vec![CandidateEvidenceStatus::Failed],
                false,
            ),
            (
                CandidateTerminalStatus::Failed,
                vec![
                    CandidateEvidenceStatus::Passed,
                    CandidateEvidenceStatus::Failed,
                ],
                true,
            ),
            (
                CandidateTerminalStatus::Failed,
                vec![CandidateEvidenceStatus::Passed],
                false,
            ),
            (
                CandidateTerminalStatus::Failed,
                vec![
                    CandidateEvidenceStatus::Failed,
                    CandidateEvidenceStatus::Indeterminate,
                ],
                false,
            ),
            (
                CandidateTerminalStatus::Failed,
                vec![
                    CandidateEvidenceStatus::Degraded,
                    CandidateEvidenceStatus::Indeterminate,
                ],
                false,
            ),
            (
                CandidateTerminalStatus::Indeterminate,
                vec![CandidateEvidenceStatus::Indeterminate],
                true,
            ),
            (
                CandidateTerminalStatus::Rejected,
                vec![CandidateEvidenceStatus::Degraded],
                true,
            ),
            (
                CandidateTerminalStatus::Cancelled,
                vec![CandidateEvidenceStatus::Passed],
                false,
            ),
            (
                CandidateTerminalStatus::Superseded,
                vec![CandidateEvidenceStatus::Passed],
                false,
            ),
        ] {
            assert_eq!(
                validate_candidate_v2_status_aggregate(&statuses, terminal).is_ok(),
                accepted,
                "terminal={terminal:?} statuses={statuses:?}"
            );
        }
    }

    #[test]
    fn candidate_v2_bundle_is_meta_enumerated_and_hash_verified() {
        let root = TempRepo::new("v2-meta");
        write(&root, "base.txt", b"base\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);
        write(&root, "candidate.txt", b"candidate\n");
        let manifest = crate::candidate_snapshot_git::build_overlay_manifest(&root, "HEAD")
            .unwrap()
            .expect("fixture has a candidate")
            .manifest;
        let attempt_id = AttemptId::new("attempt.v2-meta.1").unwrap();
        let expected = vec!["policy-a".to_string(), "policy-b".to_string()];
        let mut documents = [
            (
                "project-check-result-001.json".to_string(),
                serde_json::to_vec(&candidate_evidence(&manifest, "policy-a", "passed")).unwrap(),
            ),
            (
                "project-check-result-002.json".to_string(),
                serde_json::to_vec(&candidate_evidence(&manifest, "policy-b", "skipped")).unwrap(),
            ),
            ("stdout.tail".to_string(), b"child output\n".to_vec()),
        ]
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
        let inventory = documents
            .iter()
            .map(
                |(name, bytes)| cargoless_core::evidence::EvidenceInventoryEntry {
                    name: name.clone(),
                    bytes: bytes.len() as u64,
                    sha256: cargoless_core::sha256_hex(bytes),
                },
            )
            .collect::<Vec<_>>();
        let evidence_digest =
            cargoless_core::evidence::canonical_evidence_bundle_digest(&inventory);
        let meta = candidate_evidence_meta(&attempt_id, &evidence_digest, &documents, &[]);
        let mut fetched = Vec::new();

        validate_candidate_v2_evidence_bundle(
            &meta,
            &evidence_digest,
            &attempt_id,
            &manifest,
            &expected,
            CandidateTerminalStatus::Passed,
            |name| {
                fetched.push(name.to_string());
                Ok(documents.get(name).cloned())
            },
        )
        .expect("meta binds and every enumerated artifact hash verifies");
        assert_eq!(fetched, documents.keys().cloned().collect::<Vec<_>>());

        let mismatch = validate_candidate_v2_evidence_bundle(
            &meta,
            "different-bundle-digest",
            &attempt_id,
            &manifest,
            &expected,
            CandidateTerminalStatus::Passed,
            |name| Ok(documents.get(name).cloned()),
        )
        .expect_err("EvidenceRef sha must match meta artifact_digest");
        assert!(mismatch.contains("artifact_digest"), "{mismatch}");

        let omitted = candidate_evidence_meta(
            &attempt_id,
            &evidence_digest,
            &documents,
            &["project-check-result-002.json"],
        );
        let error = validate_candidate_v2_evidence_bundle(
            &omitted,
            &evidence_digest,
            &attempt_id,
            &manifest,
            &expected,
            CandidateTerminalStatus::Passed,
            |name| Ok(documents.get(name).cloned()),
        )
        .expect_err("a meta-omitted project result must fail closed");
        assert!(error.contains("omitted"), "{error}");

        documents.insert("stdout.tail".to_string(), b"other output\n".to_vec());
        let error = validate_candidate_v2_evidence_bundle(
            &meta,
            &evidence_digest,
            &attempt_id,
            &manifest,
            &expected,
            CandidateTerminalStatus::Passed,
            |name| Ok(documents.get(name).cloned()),
        )
        .expect_err("every meta-enumerated artifact hash must be verified");
        assert!(
            error.contains("stdout.tail") && error.contains("sha256"),
            "{error}"
        );

        let mut jointly_mutated = documents.clone();
        jointly_mutated.insert("stdout.tail".to_string(), b"third output\n".to_vec());
        let rewritten_meta =
            candidate_evidence_meta(&attempt_id, &evidence_digest, &jointly_mutated, &[]);
        let error = validate_candidate_v2_evidence_bundle(
            &rewritten_meta,
            &evidence_digest,
            &attempt_id,
            &manifest,
            &expected,
            CandidateTerminalStatus::Passed,
            |name| Ok(jointly_mutated.get(name).cloned()),
        )
        .expect_err("rewriting an artifact and its meta hash cannot preserve EvidenceRef");
        assert!(
            error.contains("canonical") || error.contains("digest"),
            "{error}"
        );
    }

    #[test]
    fn candidate_check_ids_are_nonempty_sorted_and_unique() {
        assert_eq!(
            canonical_candidate_check_ids(&[
                " policy-b ".into(),
                "policy-a".into(),
                "policy-b".into(),
                " ".into(),
            ]),
            vec!["policy-a".to_string(), "policy-b".to_string()]
        );
        assert!(canonical_candidate_check_ids(&[" ".into()]).is_empty());
    }

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
            candidate_manifest_digest: None,
            candidate_snapshot_digest: None,
            candidate_tree_oid: None,
            ra_blind_paths: false,
            gated_checks_ran: Vec::new(),
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
    /// matching SHAs accept regardless of freshness (idempotent re-run
    /// fast-path), mismatched SHAs NEVER accept (cross-branch verdict
    /// bleed — the false-attribution incident class A2 closes), and
    /// missing SHAs degrade to the freshness guard.
    #[test]
    fn attribution_predicate_matrix() {
        let fresh_after_100 = AwaitFreshness {
            prior_published_at: Some(100),
            not_before_unix: 100,
        };
        // Match ⇒ accept even when stale (published before our push).
        assert!(status_is_acceptable(
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
            ..Default::default()
        }
    }

    fn rejected_ack() -> PushOverlayAck {
        PushOverlayAck {
            worktree: "/wt".into(),
            accepted: false,
            applied_files: 0,
            ..Default::default()
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

    /// Path D — a `TransportClient` that records which of `get_status` /
    /// `get_status_attributed` was called, so the await path's routing
    /// choice is observable in a unit test rather than only over the
    /// real wire.
    struct RoutingRecorder {
        // (worktree, base_sha) pairs, in call order.
        attributed_calls: Mutex<Vec<(String, Option<String>)>>,
        unattributed_calls: Mutex<Vec<String>>,
        answer: Option<WorktreeStatus>,
    }

    impl RoutingRecorder {
        fn new(answer: Option<WorktreeStatus>) -> Self {
            Self {
                attributed_calls: Mutex::new(Vec::new()),
                unattributed_calls: Mutex::new(Vec::new()),
                answer,
            }
        }
    }

    impl TransportClient for RoutingRecorder {
        fn get_status(&self, w: &str) -> Result<Option<WorktreeStatus>, TransportError> {
            self.unattributed_calls.lock().unwrap().push(w.to_string());
            Ok(self.answer.clone())
        }
        fn get_status_attributed(
            &self,
            w: &str,
            base_sha: Option<&str>,
        ) -> Result<Option<WorktreeStatus>, TransportError> {
            self.attributed_calls
                .lock()
                .unwrap()
                .push((w.to_string(), base_sha.map(str::to_string)));
            Ok(self.answer.clone())
        }
        fn get_verdict(&self, _w: &str) -> Result<Option<String>, TransportError> {
            Ok(None)
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
    }

    #[test]
    fn await_routes_through_attributed_when_resolved_sha_is_some() {
        // Path D wiring — with a resolved base_sha, the await MUST call
        // `get_status_attributed` (which reaches the verdict_history ring
        // via `&base_sha=` on the wire) instead of the bare `get_status`
        // that only sees the last-publisher cache. This is the whole
        // point of the addressing fix: a poller for a superseded commit
        // must find its own verdict, not another SHA's.
        let matching = RoutingRecorder::new(Some(status("green", Some("abc"), 1)));
        let guard = AwaitFreshness {
            prior_published_at: Some(1000),
            not_before_unix: 1000,
        };
        let got = await_attributed_verdict(&matching, "/wt", Some("abc"), guard, 5)
            .expect("attributed path returns the matching status");
        assert_eq!(got.verdict, "green");
        assert_eq!(
            matching.attributed_calls.lock().unwrap().as_slice(),
            &[("/wt".to_string(), Some("abc".to_string()))],
            "resolved_sha=Some ⇒ MUST call get_status_attributed with that sha"
        );
        assert!(
            matching.unattributed_calls.lock().unwrap().is_empty(),
            "resolved_sha=Some ⇒ MUST NOT fall through to get_status"
        );

        // Symmetric case: no resolved_sha ⇒ historical get_status path.
        let unattr = RoutingRecorder::new(Some(status("green", None, 5000)));
        let got = await_attributed_verdict(&unattr, "/wt", None, guard, 5)
            .expect("unattributed path returns the freshness-passed status");
        assert_eq!(got.verdict, "green");
        assert_eq!(
            unattr.unattributed_calls.lock().unwrap().as_slice(),
            &["/wt".to_string()],
            "resolved_sha=None ⇒ MUST call get_status, not the attributed variant"
        );
        assert!(
            unattr.attributed_calls.lock().unwrap().is_empty(),
            "resolved_sha=None ⇒ MUST NOT call get_status_attributed"
        );
    }

    #[test]
    fn await_accepts_sha_match_instantly_and_times_out_on_mismatch() {
        // SHA match: instant accept, no freshness needed.
        let matching = StubClient::new(Some(status("red", Some("abc"), 1)), Ok(accepted_ack()));
        let guard = AwaitFreshness {
            prior_published_at: Some(1000),
            not_before_unix: 1000,
        };
        let got = await_attributed_verdict(&matching, "/wt", Some("abc"), guard, 5)
            .expect("sha-matched status accepted");
        assert_eq!(got.verdict, "red");

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

    /// Minimal accepting daemon: takes any push, publishes a green
    /// status attributed to a fixed SHA.
    struct GreenService {
        sha: String,
    }

    impl cargoless_core::transport::VerdictService for GreenService {
        fn get_status(&self, worktree: &str) -> Option<WorktreeStatus> {
            Some(WorktreeStatus {
                worktree: worktree.to_string(),
                verdict: "green".into(),
                daemon_build_id: "green-service".into(),
                crates: Vec::new(),
                red_diagnostics: 0,
                verdict_failure_reason: None,
                base_sha: Some(self.sha.clone()),
                candidate_manifest_digest: None,
                candidate_snapshot_digest: None,
                candidate_tree_oid: None,
                ra_blind_paths: false,
                gated_checks_ran: Vec::new(),
                heartbeat_age_secs: 0,
                published_at: 2000,
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
            PushOverlayAck {
                worktree: worktree.to_string(),
                accepted: true,
                applied_files: files.len() as u32,
                ..Default::default()
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
            Arc::new(GreenService {
                sha: "abc123".into(),
            }),
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
