//! Increment 0 (Model R #10 read-plane wiring) — the live serve-loop's
//! [`VerdictService`].
//!
//! v0.2.0 shipped a **complete, exhaustively-unit-tested transport library**
//! ([`cargoless_core::transport`]: the logical [`VerdictService`] +
//! in-proc/Unix/HTTP adapters + the `--remote` discovery chain + the #14
//! auth seam) that **nothing in the binary wires**. This module is the
//! missing wire on the *server* side: a [`VerdictService`] backed by the
//! serve-loop's live per-worktree verdict state, so `serve --repo --bind
//! <addr>` actually exposes the shipped HTTP+SSE surface.
//!
//! ## Faithful-composition discipline (NOT a transport reshape)
//!
//! The transport contract (`transport/{mod,http,discovery,inproc}.rs`) is
//! frozen and unit-tested; this is *wiring*, not redesign. The load-bearing
//! property is reused, not weakened:
//!
//! * **Single verdict site preserved (Judgment B as composed).** servedrv
//!   already attributes a verdict at EXACTLY ONE site —
//!   `servedrv::publish_verdict`, the sole `ClusterAction::EmitVerdict`
//!   arm. [`ServeVerdictState::publish`] is called *from that same one
//!   site*, alongside the existing durable `statusfile::write`. We do NOT
//!   introduce a second verdict-attribution path — the in-memory service
//!   and the SSE bus are a faithful *mirror* of the one authoritative
//!   write-plane, so the proven `#189`/`#198` composition story is intact.
//! * **Subscribe-emit from the same one site (0b).** The transition-event
//!   fan-out happens in `publish` too — one event per real verdict,
//!   never a fabricated one.
//!
//! ## Honest Increment-0 boundary (stated, not papered over)
//!
//! `red_diagnostics` is `0` and `crates` is empty here — *exactly* as the
//! existing `statusfile`/`publish_verdict` v0 path already writes them
//! (servedrv's `Status` carries `red_diagnostics: 0, crates: Vec::new()`).
//! Per-crate roll-up (#9 `cratemap`) and queryable diagnostics retention
//! (#11 `diagnostics_store`) are real surfaces but their *serve-loop
//! wiring* is a later increment; mirroring the same zeros the durable path
//! already emits keeps the read-plane consistent with the write-plane
//! rather than fabricating detail the loop does not yet compute.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cargoless_core::analyzer::RaStderrSnapshot;
use cargoless_core::batch::{BatchChecker, BatchMember, BatchReport, BatchVerdict, run_batch};
use cargoless_core::corun::CorunPolicy;
use cargoless_core::evidence::{ArtifactKind, EvidenceBundle, EvidenceClass, EvidenceStore};
use cargoless_core::lane::{LaneMember, LaneState};
use cargoless_core::lanehost::LaneHost;
use cargoless_core::outcome::{
    AttemptId, Authority, Component as OutcomeComponent, Conclusion, DiagnosticLocation,
    DiagnosticOrigin, DiagnosticRecord, DiagnosticSeverity, EvidenceAvailability, EvidenceRef,
    ExecutionId, FailureCause, IndeterminateCause, NonEmptyDiagnostics, NonEmptyText,
    OutcomeEnvelope, PassBasis, PathOverlap, Phase, PhaseRecord, Producer, Relation, RelationKind,
    RetryDirective, Subject, Surface,
};
use cargoless_core::project_checks::{
    CandidateSnapshotCheckContext, ProjectCheckReport, plan_dev_with_changes,
};
use cargoless_core::sha256_hex;
use cargoless_core::transport::{
    AttemptContext, BatchCheckRequest, CheckProfile, DaemonActivity, LaneEnqueueRequest,
    PushOverlayAck, PushOverlayOptions, TransitionEvent, VerdictService, WorktreeStatus,
    WorktreeSummary, batchreport_to_json,
};
use cargoless_core::{
    CandidateSnapshot, CandidateSnapshotManifest, Diagnostic, OverlayOperation, Severity,
    TreeState, canonical_manifest_json, decode_overlay_payload,
};

/// Poison-tolerant lock (same discipline as `model::poisoned` /
/// `inproc::testmock`): a panicked verdict path must not wedge the read
/// plane — recover the guard and carry on (best-effort transport ethos).
fn poisoned<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// #A4.3 — global hard-witness generation source. Monotonic and never
/// recycled across worktrees, so `finish_hard_witness`'s equality check
/// can never be fooled by a reused value.
static HARD_WITNESS_SEQ: AtomicU64 = AtomicU64::new(0);
/// Every accepted typed candidate is a distinct execution, even when a client
/// retries byte-identical manifest content. The manifest digest addresses the
/// retained result; this process-local sequence keeps concurrent hard-witness
/// claims from superseding one another before either result is published.
static CANDIDATE_EXECUTION_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct HardWitnessClaim {
    generation: u64,
    attempt_id: Option<AttemptId>,
}
/// Fallback cap on the per-worktree base_sha-addressable verdict ring
/// ([`ServeVerdictState::verdict_history`]) — used only when
/// `CARGOLESS_WITNESS_HISTORY_CAP` is unset or unparseable. Raised from
/// 16 to 64 by Path D (addressing): with the wire now actually threading
/// `&base_sha=`, the ring must survive one CI's poll window (~5100s) of
/// fan-in, which under a ~20-PR landing flood reliably exceeds 16.
/// Override via env for post-arming tuning.
const HARD_WITNESS_HISTORY_CAP_DEFAULT: usize = 64;
/// Fallback cap on the per-worktree [`ServeVerdictState::pushed`] queue.
/// The queue is unbounded historically (CGLS-25 introduced the queue for
/// the clobber fix); this cap is the R3 mitigation for a stuck-consumer
/// pathology (RA reap loop stranding the queue's head). At the cap,
/// distinct-base_sha pushes are REJECTED with HTTP 429 so the client
/// backs off instead of silently accumulating overlay bodies (each up to
/// 128 MiB per `transport::http`'s cap). Same-base_sha still latest-wins
/// (replace in place) even at the cap. Override via env.
const PUSHED_MAX_PER_WT_DEFAULT: usize = 8;
const OUTCOME_V3_MEMORY_CAP: usize = 1024;
const PROJECT_CHECK_MANIFEST_NAME: &str = "cargoless.checks.yaml";
/// CGLS-26 — bump when the warm shared-target-dir layout or keying changes,
/// so a daemon rolling a new image never reuses an incompatible warm tree
/// (it gets a fresh keyed subdir and one cold rebuild, then warm).
const WARM_TARGET_SCHEMA_TAG: &str = "warm-v1";
/// CGLS-26 — number of newest warm-target keyed dirs to retain; older
/// siblings are pruned (a toolchain/Cargo.lock bump otherwise leaks a
/// multi-GB target tree per key on the bounded shard PVC).
const WARM_TARGET_KEEP: usize = 2;

/// A pushed overlay set carried in `ServeVerdictState::pushed`. Stored
/// pair-shape (`Vec<(String, String)>`) instead of [`OverlaySet`] so the
/// consumer in servedrv.rs's `SwitchOverlay` arm can re-build with
/// `OverlaySet::from_pairs(pushed.files)` — byte-identical to the FS
/// path's construction (the composing-equivalence assertion 2b's
/// load-bearing test pins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushedOverlay {
    /// Client-supplied base_ref (typically e.g. `origin/main`). v0.2.x:
    /// stored for diagnostics + future "diff vs base_ref" features;
    /// server does NOT act on it in 2b (spike open-question #2 default).
    pub base_ref: String,
    /// Whole-file `(path, content)` pairs — the same shape the FS path
    /// builds via `std::fs::read_to_string` per file.
    pub files: Vec<(String, String)>,
    /// Server-side root for central-daemon pushes. When set, the serve loop
    /// uses this as the rust-analyzer workspace root while keeping `worktree`
    /// as the client-visible status key.
    pub analysis_root: Option<PathBuf>,
    /// Client's resolved base SHA, diagnostics-only. The server fetch/reset
    /// result remains authoritative.
    pub base_sha: Option<String>,
    /// Advertised remote ref used to fetch the exact candidate.
    pub source_ref: Option<String>,
    /// Verified candidate commit used by git-native project checks.
    pub source_sha: Option<String>,
    /// Digest-bound complete candidate. Present only for typed overlay pushes;
    /// exact-Git pushes continue to use source_ref/source_sha unchanged.
    pub candidate_snapshot: Option<CandidateSnapshotManifest>,
    /// Unix timestamp of the push receipt. Diagnostics-only for 2b;
    /// future idle-evict policy (Wave-2) reads this.
    pub last_push_unix: u64,
    /// Repo-relative files changed by the client diff. Project-check
    /// trigger filtering uses this instead of the overlay file list because
    /// overlays include extra workspace config files for cluster routing.
    pub changed_files: Option<Vec<String>>,
    /// Optional per-push Cargo check profile. This lets a single
    /// repo-scoped daemon accept tf-multiverse's per-invocation
    /// `check-remote` selectors without restarting RA per package.
    pub check_profile: Option<CheckProfile>,
    /// Merge-gate push: promote THIS push's project-check mode from Warn
    /// to Hard (witness-gated verdict). Wire default `false`.
    pub gate: bool,
    /// Requested witness check-ids for a gate push. When `gate` is set and
    /// this is a non-empty set, the Hard lane runs ONLY these checks (the
    /// compile witnesses — ssr/wasm/isolator-vsock) instead of the whole
    /// `dev` profile, so the merge gate proves the requested witnesses ran
    /// without dragging the ~97-check governance profile (and its
    /// environmental reds) through a gating verdict. `None`/empty ⇒ the
    /// gated lane runs the full profile (prior behavior).
    pub check_ids: Option<Vec<String>>,
    /// Exact v3 attempt identity. Legacy/local pushes omit it and retain the
    /// historical status-only behavior.
    pub semantic: Option<AttemptContext>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectCheckRunContext {
    pub root: PathBuf,
    pub changed_files: Option<Vec<String>>,
    pub base_ref: String,
    pub base_sha: Option<String>,
    pub source_ref: Option<String>,
    pub source_sha: Option<String>,
    pub candidate_snapshot: Option<CandidateSnapshotManifest>,
    pub overlay_files: Vec<(String, String)>,
    pub materialize_overlay: bool,
    /// Carried from [`PushedOverlay::gate`]: the EmitVerdict arm promotes
    /// Warn → Hard for this push when set.
    pub gate: bool,
    /// Carried from [`PushedOverlay::check_ids`]: the witness-only run filter
    /// for the gated lane. Only consulted when `gate` is true.
    pub check_ids: Option<Vec<String>>,
}

/// Owned handoff from HTTP ingest to the hard-witness dispatcher. Gated
/// checks do not enter the rust-analyzer transaction queue.
#[derive(Debug, Clone)]
pub(crate) struct DirectGateRequest {
    pub wt: PathBuf,
    pub context: ProjectCheckRunContext,
    pub attribution: PushAttribution,
}

/// Exact bytes from one successfully verified candidate result-v2 document.
///
/// Construction happens only from a [`ProjectCheckReport`] whose structured
/// subject was bound to the verified candidate run context. The evidence
/// store deliberately persists the original bytes, not a reserialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedProjectCheckEvidence {
    pub check_id: String,
    pub bytes: Vec<u8>,
}

type AttributedDiagnostics = BTreeMap<(String, Option<String>), Vec<Diagnostic>>;
type CandidateDiagnostics = BTreeMap<(String, String), Vec<Diagnostic>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateVerdictIdentity {
    pub manifest_digest: String,
    pub snapshot_digest: String,
    pub tree_oid: String,
    pub execution_id: u64,
}

impl CandidateVerdictIdentity {
    fn from_manifest(manifest: &CandidateSnapshotManifest) -> Self {
        Self {
            manifest_digest: manifest.manifest_digest.clone(),
            snapshot_digest: manifest.candidate.snapshot_digest().to_string(),
            tree_oid: manifest.candidate.tree_oid().to_string(),
            execution_id: CANDIDATE_EXECUTION_SEQ.fetch_add(1, Ordering::Relaxed) + 1,
        }
    }

    fn witness_key(&self) -> String {
        format!("candidate:{}:{}", self.manifest_digest, self.execution_id)
    }
}

/// Verdict-attribution record for one consumed push (#A2/#A7). Captured by
/// the serve loop's SwitchOverlay arm at the moment a [`PushedOverlay`] is
/// actually applied to rust-analyzer, consumed by [`ServeVerdictState::
/// publish`] when the resulting verdict lands. Recorded at *consume* time
/// (not push receipt) so a replacing second push can never leave its
/// `base_sha` stamped on the first push's verdict — the loop's
/// record→publish pairs are properly nested per worktree key.
#[derive(Debug, Clone)]
pub(crate) struct PushAttribution {
    /// Client-resolved base SHA from the push, echoed on the published
    /// [`WorktreeStatus`] so a poller sharing a status key with other
    /// branches accepts only verdicts stamped with its own commit.
    pub base_sha: Option<String>,
    /// Typed candidate execution identity. This is deliberately separate
    /// from legacy `base_sha`: comparison attribution is not candidate
    /// content identity and must never drive coalescing or result lookup.
    pub candidate: Option<CandidateVerdictIdentity>,
    /// #A8 — `true` iff the push's `changed_files` matched the daemon's
    /// macro-blind path globs (`CARGOLESS_MACRO_BLIND_PATHS`) at consume
    /// time. Rides the attribution so it stays paired with `base_sha`
    /// through the record→pop lifecycle (incl. the Hard-mode supervisor
    /// thread), is echoed as the additive `ra_blind_paths` wire key, and
    /// — with `CARGOLESS_MACRO_BLIND_ESCALATE=1` — promotes this push's
    /// project-check mode Warn → Hard at the EmitVerdict dispatch.
    pub macro_blind_hit: bool,
    /// `PushedOverlay::last_push_unix` — wall-clock push receipt (seconds).
    pub push_received_unix: u64,
    /// Wall-clock + monotonic pair captured together at overlay-apply, so
    /// publish-time latency = coarse queue wait (receipt→consume, second
    /// granularity) + exact analysis time (consume→publish, monotonic ms).
    pub consumed_unix: u64,
    pub consumed_at: Instant,
    /// Kept on the consumed push so final publication can update exactly the
    /// attempt that caused it, even when the same SHA is retried.
    pub semantic: Option<AttemptContext>,
}

impl PushAttribution {
    pub(crate) fn witness_key(&self) -> Option<String> {
        self.candidate
            .as_ref()
            .map(CandidateVerdictIdentity::witness_key)
            .or_else(|| self.base_sha.clone().filter(|value| !value.is_empty()))
    }
}

impl PushAttribution {
    /// Push-receipt → verdict-publish latency in milliseconds (#A7).
    pub(crate) fn verdict_latency_ms(&self) -> u64 {
        latency_ms(
            self.push_received_unix,
            self.consumed_unix,
            self.consumed_at.elapsed(),
        )
    }
}

/// #A7 latency composition: coarse queue wait (unix-second receipt →
/// consume; `now_unix` is the only clock the push receipt has) plus exact
/// monotonic analysis time (consume → publish). Saturating throughout —
/// wall-clock skew (NTP step between receipt and consume) degrades to a
/// smaller-but-sane number, never a panic or a u64 wrap.
fn latency_ms(push_received_unix: u64, consumed_unix: u64, analysis: Duration) -> u64 {
    consumed_unix
        .saturating_sub(push_received_unix)
        .saturating_mul(1000)
        .saturating_add(u64::try_from(analysis.as_millis()).unwrap_or(u64::MAX))
}

fn text_v3(value: impl Into<String>) -> NonEmptyText {
    NonEmptyText::new(value).expect("v3 producer must construct non-empty semantic text")
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn producer_v3() -> Producer {
    Producer {
        daemon_build_id: text_v3(cargoless_core::build_id()),
        process_id: std::process::id(),
        process_generation: 1,
        pod_uid: std::env::var("POD_UID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(text_v3),
        rust_analyzer_generation: None,
    }
}

fn reaction_state_name(state: cargoless_core::outcome::CheckState) -> &'static str {
    match state {
        cargoless_core::outcome::CheckState::Pending => "pending",
        cargoless_core::outcome::CheckState::Success => "success",
        cargoless_core::outcome::CheckState::Failure => "failure",
        cargoless_core::outcome::CheckState::Error => "error",
        cargoless_core::outcome::CheckState::NoUpdate => "no_update",
    }
}

fn evidence_mut_v3(conclusion: &mut Conclusion) -> Option<&mut EvidenceRef> {
    match conclusion {
        Conclusion::Passed { evidence, .. }
        | Conclusion::Failed { evidence, .. }
        | Conclusion::Indeterminate { evidence, .. }
        | Conclusion::Rejected { evidence, .. }
        | Conclusion::Cancelled { evidence, .. }
        | Conclusion::Superseded { evidence, .. } => Some(evidence),
        Conclusion::Pending { .. } => None,
    }
}

/// Evidence durability is part of the result, not an out-of-band warning.
/// A real code failure remains a failure if harvesting breaks; every other
/// conclusion becomes an operational error so a missing bundle can never be
/// mistaken for a proven pass.
fn mark_evidence_unavailable_v3(outcome: &mut OutcomeEnvelope, explanation: String) {
    let explanation_text = text_v3(explanation);
    let mut conclusion = outcome.conclusion.clone();
    let Some(evidence) = evidence_mut_v3(&mut conclusion) else {
        return;
    };
    evidence.availability = EvidenceAvailability::Unavailable {
        explanation: explanation_text.clone(),
    };
    let evidence = evidence.clone();
    match conclusion {
        Conclusion::Failed {
            cause,
            path_overlap,
            ..
        } => outcome.conclude(Conclusion::Failed {
            cause,
            path_overlap,
            evidence,
            summary: text_v3(format!(
                "code failure retained, but its durable evidence bundle is unavailable: {}",
                explanation_text.as_str()
            )),
        }),
        _ => outcome.conclude(Conclusion::Rejected {
            cause: IndeterminateCause::DependencyUnavailable {
                component: OutcomeComponent::EvidenceStore,
            },
            retry: RetryDirective::OperatorRequired,
            evidence,
            summary: text_v3(format!(
                "result cannot be accepted because durable evidence is unavailable: {}",
                explanation_text.as_str()
            )),
        }),
    }
}

fn canonical_pairs_digest(files: &[(String, String)]) -> String {
    let mut files = files.to_vec();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut bytes = Vec::new();
    for (path, content) in files {
        bytes.extend_from_slice(path.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(content.len().to_string().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(sha256_hex(content.as_bytes()).as_bytes());
        bytes.push(b'\n');
    }
    sha256_hex(&bytes)
}

fn canonical_strings_digest(values: &[String]) -> String {
    let mut values = values.to_vec();
    values.sort();
    values.dedup();
    sha256_hex(values.join("\n").as_bytes())
}

fn overlay_subject_v3(
    worktree: &str,
    base_ref: &str,
    files: &[(String, String)],
    profile: Option<&CheckProfile>,
    options: &PushOverlayOptions,
) -> Result<Subject, &'static str> {
    let base_sha = options
        .base_sha
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or("v3 overlay requires base_sha")?;
    let repository = options
        .analysis_root
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(worktree);
    if repository.trim().is_empty() || worktree.trim().is_empty() || base_ref.trim().is_empty() {
        return Err("v3 overlay requires repository, worktree, and base_ref");
    }
    let changed_files = options.changed_files.as_deref().unwrap_or_default();
    let plan = format!(
        "profile={profile:?};gate={};check_ids={:?}",
        options.gate, options.check_ids
    );
    let overlay_digest = match options.candidate_snapshot.as_ref() {
        Some(manifest) => {
            let legacy_digest = canonical_pairs_digest(files);
            let mut preimage = b"cargoless-overlay-subject\0v1\0".to_vec();
            for value in [
                legacy_digest.as_str(),
                manifest.manifest_digest.as_str(),
                manifest.candidate.snapshot_digest(),
                manifest.candidate.tree_oid(),
            ] {
                preimage.extend_from_slice(value.len().to_string().as_bytes());
                preimage.push(0);
                preimage.extend_from_slice(value.as_bytes());
                preimage.push(0);
            }
            sha256_hex(&preimage)
        }
        None => canonical_pairs_digest(files),
    };
    Ok(Subject::Overlay {
        repository: text_v3(repository),
        worktree_key: text_v3(worktree),
        base_ref: text_v3(base_ref),
        base_sha: text_v3(base_sha),
        overlay_digest: text_v3(overlay_digest),
        changed_files_digest: text_v3(canonical_strings_digest(changed_files)),
        check_plan_digest: text_v3(sha256_hex(plan.as_bytes())),
    })
}

/// #A8 — the operator's proc-macro-blind path globs, comma-separated in
/// `CARGOLESS_MACRO_BLIND_PATHS` (e.g. `portal/**,chemistry/shell/**`).
/// Empty / unset ⇒ no globs ⇒ the annotation never fires (the feature is
/// inert until the deployment opts in). Read per consume, not cached:
/// pushes are seconds-apart events and a fleet env edit must not require
/// a daemon restart reasoning step during an incident.
fn macro_blind_globs() -> Vec<String> {
    parse_macro_blind_globs(&std::env::var("CARGOLESS_MACRO_BLIND_PATHS").unwrap_or_default())
}

/// Env-free parse body of [`macro_blind_globs`] (testable without
/// process-env mutation under parallel test threads). Tolerant of
/// spaces around commas and stray empty segments (`a/**,,b/**` ⇒ two
/// globs) — a fleet env edit must not silently disable the annotation
/// over a formatting slip.
fn parse_macro_blind_globs(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// #CGLS-12 — the operator's proc-macro call-signature names, comma-separated
/// in `CARGOLESS_MACRO_BLIND_MACROS` (e.g. `view,html` — WITHOUT the trailing
/// `!`; the scanner adds it). Used by
/// [`compute_macro_blind_hit`] to narrow glob hits via content scanning.
/// Empty / unset ⇒ macro names unconfigured ⇒ content scan is skipped and
/// behavior is byte-identical to the pre-CGLS-12 pure path-glob path. Read
/// per consume (same policy as `macro_blind_globs`).
fn macro_blind_macros() -> Vec<String> {
    parse_macro_blind_macros(&std::env::var("CARGOLESS_MACRO_BLIND_MACROS").unwrap_or_default())
}

/// Env-free parse body of [`macro_blind_macros`]. Same tolerant split as
/// [`parse_macro_blind_globs`]: spaces around commas, stray empty segments
/// ignored. Each entry is a macro name WITHOUT the trailing `!` (e.g.
/// `"view"`, not `"view!"`).
fn parse_macro_blind_macros(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// #CGLS-12 — does `content` contain an invocation of any macro in
/// `macro_names`? Scans for `<name>!` immediately followed by optional
/// ASCII whitespace and then `{`, `(`, or `[` — the three legal
/// delimiters for a macro invocation. No regex crate: a simple two-pass
/// byte scan (find the `!`, walk back to the name, walk forward to the
/// delimiter) keeps the crate dep-free and allocation-free per call.
///
/// Deliberately conservative: unusual formatting (e.g. a comment between
/// the `!` and the `{`) may be missed, which is fine — the caller's
/// fail-safe (no content found ⇒ glob hit stands) means this can only
/// produce false negatives (treat as blind), never false positives (miss
/// a real blind file).
fn content_has_macro_call(content: &str, macro_names: &[String]) -> bool {
    if macro_names.is_empty() {
        return false;
    }
    let bytes = content.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    while i < len {
        if bytes[i] != b'!' {
            i += 1;
            continue;
        }
        // Walk forward past optional ASCII whitespace to find the delimiter.
        let mut j = i + 1;
        while j < len
            && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n' || bytes[j] == b'\r')
        {
            j += 1;
        }
        if j < len && (bytes[j] == b'{' || bytes[j] == b'(' || bytes[j] == b'[') {
            // Found `!<ws>*[{(\[]`. Now walk backward from i to extract the
            // identifier before the `!`. Identifiers: ASCII alphanumeric + `_`.
            let name_end = i; // exclusive: the char at i is `!`
            let mut k = i;
            while k > 0 && (bytes[k - 1].is_ascii_alphanumeric() || bytes[k - 1] == b'_') {
                k -= 1;
            }
            if k < name_end {
                let name = &content[k..name_end];
                if macro_names.iter().any(|m| m == name) {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// Resolve the content of `changed_path` (a repo-relative path such as
/// `"portal/src/app.rs"`) from the push overlay's file pairs. Overlay
/// paths may be absolute (after `map_repo_relative_files` joins them with
/// `analysis_root`) or repo-relative (direct push). A suffix match handles
/// both: the repo-relative path is always a suffix of the absolute form.
fn overlay_content_for<'a>(
    changed_path: &str,
    overlay_files: &'a [(String, String)],
) -> Option<&'a str> {
    overlay_files
        .iter()
        .find(|(overlay_path, _)| {
            // Exact match first (repo-relative push, paths are identical).
            overlay_path == changed_path
                // Suffix match for absolute overlay paths produced by
                // map_repo_relative_files: `/root/portal/src/app.rs` ends with
                // `/portal/src/app.rs` which is `"/" + changed_path`.
                || overlay_path.ends_with(&format!("/{changed_path}"))
        })
        .map(|(_, content)| content.as_str())
}

/// #A8 — does this push touch a macro-blind path? Matches the push's
/// repo-relative `changed_files` (the same list project-check triggers
/// filter on — NOT the overlay file list, which carries extra workspace
/// config files for cluster routing) against the operator globs with the
/// manifest-trigger matcher, so one pattern language serves both.
///
/// `None`/empty `changed_files` ⇒ `false`: with no attributable change
/// list there is no evidence the push touches a blind path, and the
/// annotation must never fire on absence of evidence (the same posture
/// as `base_sha: None` ⇒ unattributed, never a match).
///
/// #CGLS-12 — content-narrowing (fail-safe): when `macro_names` is
/// non-empty, glob-matched files are additionally scanned for a macro
/// call invocation (`<name>!\s*[{(\[]`). A glob-matched file whose
/// content is available AND contains no such call is NOT classified as
/// blind (reduces ~37% over-fire). Fail-safe: if the file's content is
/// NOT in the overlay (unreadable edge case), the glob hit stands — a
/// real blind file must never be missed. When `macro_names` is empty the
/// content scan is skipped entirely and behavior is byte-identical to the
/// pre-CGLS-12 pure path-glob path.
fn compute_macro_blind_hit(
    changed_files: Option<&[String]>,
    blind_globs: &[String],
    overlay_files: &[(String, String)],
    macro_names: &[String],
) -> bool {
    if blind_globs.is_empty() {
        return false;
    }
    changed_files.is_some_and(|files| {
        files.iter().any(|path| {
            let glob_hit = blind_globs
                .iter()
                .any(|pattern| cargoless_core::project_checks::glob_match_path(pattern, path));
            if !glob_hit {
                return false;
            }
            // Glob matched. If macro names are configured, try to narrow via
            // content scan. Fail-safe: absent content ⇒ keep the glob hit.
            if macro_names.is_empty() {
                return true;
            }
            match overlay_content_for(path, overlay_files) {
                Some(content) => content_has_macro_call(content, macro_names),
                None => {
                    // Content not in overlay (e.g. the overlay only contains
                    // workspace config files for cluster routing, not the
                    // Rust source). Fall back to the glob hit — conservative.
                    true
                }
            }
        })
    })
}

/// The serve-loop's live verdict state, presented as the shipped logical
/// [`VerdictService`]. `Send + Sync` (the trait demands it so the
/// HTTP/Unix adapters can share one service across connection threads):
/// the `Mutex`-guarded fields satisfy that by construction.
#[derive(Default)]
pub struct ServeVerdictState {
    /// worktree-key → last published status. Keyed by the SAME string
    /// `servedrv::publish_verdict` uses (`wt.to_string_lossy()`), so a
    /// remote `get_status(<wt>)` resolves the exact tree the loop
    /// attributed.
    statuses: Mutex<BTreeMap<String, WorktreeStatus>>,
    /// Full diagnostics keyed by the same `(worktree, base_sha)` identity as
    /// attributed verdict history. This prevents simultaneous PRs sharing a
    /// worktree key from reading each other's compiler output.
    diagnostics: Mutex<AttributedDiagnostics>,
    /// Typed-candidate diagnostics keyed by immutable manifest identity,
    /// never by the mutable shared worktree slot or comparison base.
    candidate_diagnostics: Mutex<CandidateDiagnostics>,
    /// Live transition-event subscribers (the SSE / in-proc fan-out).
    /// Retain-on-send like `model`'s buses so a dropped subscriber never
    /// stalls the (single) producer.
    subs: Mutex<Vec<Sender<TransitionEvent>>>,
    /// #240/2b — pushed-overlay store. worktree-key → FIFO queue of
    /// [`PushedOverlay`]. Populated by `push_overlay` (the
    /// [`VerdictService`] write-plane ingest, `push_back`), consumed one
    /// at a time by `take_overlay_for` (the serve loop's SwitchOverlay
    /// arm, `pop_front`). The `take` is **pop-on-consume semantic** (spike
    /// open-question #3 default): once the queue empties the WT falls back
    /// to the FS path until a fresh push arrives.
    ///
    /// **CGLS-25 — the value is a QUEUE, not a single slot.** The witness
    /// hardcodes ONE worktree key for every PR, so two concurrent PR
    /// pushes land on the same key. The historical single-slot `insert`
    /// let PR-B's push OVERWRITE PR-A's pending overlay before the serve
    /// loop consumed it — PR-A's witness never ran, its poller starved to
    /// the CI timeout (the "attributed to X, want Y" clobber class). A
    /// per-WT FIFO queue makes distinct pushes independent at the map
    /// level: each survives to its own SwitchOverlay→witness cycle. The
    /// clobber window is ONLY here — `project_check_context` /
    /// `push_attribution` are recorded-then-consumed strictly on the
    /// single serve-loop thread (SwitchOverlay records, EmitVerdict pops,
    /// alternating per WT), so they never cross-clobber. `take_overlay_for`
    /// re-signals the serve loop when the queue is still non-empty so a
    /// second queued push is not starved by the wake-dedup in
    /// `drain_unique_push_keys`.
    pushed: Mutex<BTreeMap<String, VecDeque<PushedOverlay>>>,
    /// Serializes central-daemon mirror fetch/reset operations. The HTTP
    /// adapter can accept several requests concurrently; the checked-out
    /// mirror is one mutable filesystem and must move one base at a time.
    sync_lock: Mutex<()>,
    /// #240/2b — push-arrival signal channel. The serve loop drains
    /// this alongside ctrl_rx; each received worktree-key is the
    /// wakeup signal that a push needs servicing. `Option<Sender>`
    /// because `new()` constructs without a channel; the loop wires
    /// one in via [`Self::attach_push_signal`] at startup, BEFORE
    /// `HttpServer::bind` exposes `push_overlay` to clients (so no
    /// push can race the channel-not-yet-attached window).
    push_signal: Mutex<Option<Sender<String>>>,
    /// Direct gated-check handoff, wired before HTTP bind just like
    /// `push_signal`. A gate push is accepted only when this dispatcher is
    /// available; it is never queued behind rust-analyzer.
    direct_gate_signal: Mutex<Option<Sender<DirectGateRequest>>>,
    /// Admin drain state. A restart requests quiesce through HTTP; after
    /// that, new pushes are refused while accepted pushed worktrees stay
    /// active until their next authoritative verdict is published.
    drain: Mutex<DrainState>,
    /// Project-check context captured when a pushed overlay is consumed.
    /// The verdict arm runs later, after rust-analyzer settles, so the
    /// changed-file trigger set and central-daemon analysis root need a
    /// small handoff store keyed by the client-visible worktree.
    project_check_context: Mutex<BTreeMap<String, ProjectCheckRunContext>>,
    /// #A2/#A7 — attribution handoff parallel to `project_check_context`:
    /// captured at SwitchOverlay consume, popped by `publish`. Worktrees
    /// whose verdict came from the FS-watch path simply have no entry
    /// (their status carries `base_sha: None`, no latency line).
    push_attribution: Mutex<BTreeMap<String, PushAttribution>>,
    /// A6 — RA-warm readiness latch, the `GET /readyz` source of truth.
    /// `false` (the `Default`) until servedrv flips it via
    /// [`Self::mark_ready`] at the first completed rust-analyzer LSP
    /// handshake — distinct from the `/healthz` serve-loop-entered flag,
    /// which goes `true` before RA can produce any verdict. One-way
    /// monotonic latch ⇒ `Relaxed` ordering suffices.
    ready: AtomicBool,
    /// Optional server-side coalescing for explicit `coalesce_key`
    /// batch-check requests. Absent key keeps historical immediate behavior.
    batch_coalescer: BatchCoalescer,
    /// CGLS-25 — global concurrency gate for the Hard-witness compile.
    /// Default off (`CARGOLESS_WITNESS_MAX_INFLIGHT=0`, unbounded); set to N
    /// to cap concurrent witness compiles once the overlay-queue fix lets
    /// N distinct-SHA survivors through. Acquired in the witness worker only.
    witness_gate: WitnessInflightGate,
    /// Server-local state directory used for transient project-check
    /// scratch worktrees. `None` keeps the in-root v0 path for unit tests
    /// and embedded callers that do not have a resolved fleet config.
    project_check_state_dir: Option<PathBuf>,
    /// Exact attempt-keyed semantic state. This is intentionally independent
    /// of the last-writer-wins worktree status slot.
    outcomes_v3: Mutex<BTreeMap<cargoless_core::outcome::AttemptId, OutcomeEnvelope>>,
    outcome_order_v3: Mutex<VecDeque<cargoless_core::outcome::AttemptId>>,
    /// Durable proof store, configured alongside the daemon state directory.
    evidence_store_v3: Option<EvidenceStore>,
    ra_evidence_v3: Mutex<BTreeMap<cargoless_core::outcome::AttemptId, RaStderrSnapshot>>,
    outcome_metrics_v3: Mutex<OutcomeMetricsV3>,
    /// Per-`(worktree, base_sha)` Hard-witness generation counter. The latest
    /// generation for each key is the only witness that may publish; stale
    /// witnesses (from a prior push whose EmitVerdict fired while a newer
    /// push's witness is already running) are detected by
    /// `finish_hard_witness` and dropped. The counter values are sourced from
    /// the module-level `HARD_WITNESS_SEQ` atomic, which is globally monotonic
    /// and never recycled, so a recycled match is structurally impossible.
    ///
    /// **The base_sha is part of the key** (was: worktree-only). The witness
    /// hardcodes one worktree string for every PR, so a worktree-only latch
    /// let a *newer commit's* push supersede an *older commit's* in-flight
    /// witness — the older witness's correct GREEN was dropped
    /// ("stale-witness-dropped") and the superseded SHA's poller timed out
    /// (the `<absent>` attribution bug). Keying by `(worktree, base_sha)`
    /// makes two distinct commits independent: each publishes on its own
    /// merit; only a *re-push of the same commit* supersedes. FS-watch /
    /// unattributed witnesses (`base_sha: None`) still share one key per
    /// worktree, matching their pre-existing semantics.
    hard_witness_generation: Mutex<BTreeMap<(String, Option<String>), HardWitnessClaim>>,
    /// #A2-keystone — base_sha-addressable verdict ring per worktree. Because
    /// the witness shares one worktree key across all PRs, the single
    /// `statuses` slot can only hold the *last* publisher's verdict; a poller
    /// for a superseded commit would never see its own verdict echoed even
    /// when that verdict was correctly computed. This ring retains the last
    /// [`Self::witness_history_cap`] *attributed* (base_sha = `Some`) verdicts
    /// per worktree so `get_status_attributed(wt, Some(sha))` resolves the
    /// exact commit the poller asked about, independent of what landed in the
    /// `statuses` slot afterward. Unattributed (FS-watch) verdicts never enter
    /// the ring — they have no SHA to address.
    verdict_history: Mutex<BTreeMap<String, VecDeque<WorktreeStatus>>>,
    /// C1 observability — the resolved RA config JSON
    /// ([`cargoless_core::lsp::InitOpts::resolved_summary`]) this daemon
    /// runs under, surfaced on `GET /daemon`. `None` until the serve loop
    /// wires it via [`Self::set_resolved_config`] at startup (same
    /// attach-at-startup pattern as `push_signal`); a daemon that never
    /// sets it simply omits `ra_config` from `/daemon`.
    resolved_config: Mutex<Option<serde_json::Value>>,
    /// Path D (addressing) — cap on [`Self::verdict_history`], resolved
    /// once at construction via [`witness_history_cap_from`]. Stored
    /// (rather than env-read per publish) so unit tests set an
    /// explicit cap without env mutation, and env drift mid-daemon-life
    /// can't confuse the eviction invariant.
    witness_history_cap: usize,
    /// R3 mitigation — cap on [`Self::pushed`] queue depth per worktree
    /// key, resolved once at construction via [`pushed_max_per_wt_from`].
    /// Same rationale as `witness_history_cap`.
    pushed_max_per_wt: usize,
    /// CGLS-26 — per-warm-key in-process busy flag for the shared witness
    /// target dir. A non-blocking compare-and-swap is the primary interlock:
    /// if a warm key is already in use (flag true), the caller falls back to
    /// a cold per-run dir rather than sharing an in-flight target (the
    /// CGLS-24 corruption hazard). An `AtomicBool` (not a `Mutex<()>`) so the
    /// RAII release needs no lifetime gymnastics: the guard holds the `Arc`
    /// and stores `false` on drop. Keyed by the warm-dir key.
    warm_target_locks: Mutex<BTreeMap<String, Arc<std::sync::atomic::AtomicBool>>>,
    /// CGLS-26 — cumulative warm-target outcomes, keyed by the same
    /// `mode:reason` pair the obs line carries (`warm:hit`,
    /// `cold-fallback:contended:in-proc`, …). Surfaced on `GET /daemon` as
    /// `warm_target`, which is what makes a `cold-fallback` alertable: the
    /// eprintln alone was never consumed, and on these pods it cannot be —
    /// both witness instances sit on triform-5, whose log-shipping agent has
    /// been crash-looping for weeks. A pull-based counter needs no log
    /// pipeline. Monotonic for the process lifetime; a restart zeroes it,
    /// which a `increase()`-style query handles correctly.
    warm_target_stats: Mutex<BTreeMap<String, u64>>,
    /// The build lane, when this daemon runs one.
    ///
    /// `None` on every daemon that has not been configured for it, which is
    /// why `GET /lane` answers **404** rather than an empty lane: "no lane
    /// here" and "a lane with nothing in it" are different answers, and
    /// conflating them leaves an operator waiting on a queue that does not
    /// exist. For the same reason `lane_enqueue` on a laneless daemon returns
    /// `Err` rather than silently accepting — a caller told "queued" would
    /// wait forever for a build that is never going to run.
    lane: Option<LaneHost>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectCheckAuthority {
    CandidateSnapshot,
    ExactGit,
    LegacyOverlay,
}

impl ProjectCheckAuthority {
    fn from_context(context: &ProjectCheckRunContext) -> Self {
        if context.candidate_snapshot.is_some() {
            Self::CandidateSnapshot
        } else if context.source_sha.is_some() {
            Self::ExactGit
        } else {
            Self::LegacyOverlay
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::CandidateSnapshot => "candidate-snapshot",
            Self::ExactGit => "exact-git",
            Self::LegacyOverlay => "legacy-overlay",
        }
    }
}

struct ProjectCheckRunCleanup<'a> {
    api: &'a ServeVerdictState,
    root: &'a Path,
    authority: ProjectCheckAuthority,
    scratch_run: ProtectedRunDirectory,
    candidate_run: Option<ProtectedRunDirectory>,
    cleaned: bool,
}

impl ProjectCheckRunCleanup<'_> {
    fn cleanup(&mut self) -> (Result<(), String>, Result<(), String>) {
        if self.cleaned {
            return (Ok(()), Ok(()));
        }
        self.cleaned = true;
        let scratch = {
            let _guard = poisoned(&self.api.sync_lock);
            cleanup_protected_project_check_scratch(self.root, &self.scratch_run)
        };
        let manifest = self
            .candidate_run
            .as_ref()
            .map(cleanup_protected_candidate_manifest_run)
            .unwrap_or(Ok(()));
        (scratch, manifest)
    }
}

impl Drop for ProjectCheckRunCleanup<'_> {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let (scratch, manifest) = self.cleanup();
        for (scope, result) in [("scratch", scratch), ("candidate-manifest", manifest)] {
            if let Err(error) = result {
                eprintln!(
                    "[cargoless:obs] project-check-panic-cleanup authority={} scope={scope} root={} error={error}",
                    self.authority.label(),
                    self.root.display()
                );
            }
        }
    }
}

fn finish_project_check_run<T>(
    authority: ProjectCheckAuthority,
    result: Result<T, String>,
    scratch_cleanup: Result<(), String>,
    manifest_cleanup: Result<(), String>,
) -> Result<T, String> {
    let cleanup_errors = [scratch_cleanup.err(), manifest_cleanup.err()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if cleanup_errors.is_empty() {
        return result;
    }
    if authority == ProjectCheckAuthority::CandidateSnapshot {
        return Err(format!(
            "candidate_snapshot.cleanup_failed: {}",
            cleanup_errors.join("; ")
        ));
    }
    eprintln!(
        "[cargoless:obs] project-check-cleanup-failed authority={} errors={}",
        authority.label(),
        cleanup_errors.join("; ")
    );
    result
}

#[derive(Default)]
struct DrainState {
    quiescing: bool,
    active_worktrees: BTreeSet<String>,
}

#[derive(Default)]
struct OutcomeMetricsV3 {
    terminal_by_code: BTreeMap<String, u64>,
    reactions_by_state: BTreeMap<String, u64>,
    ra_storm_outcomes: u64,
    evidence_persist_failures: u64,
    last_ra_error_lines: u64,
    last_ra_suppressed_lines: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct BatchCoalesceKey {
    coalesce_key: String,
    base_ref: String,
    analysis_root: Option<String>,
    repo_relative: bool,
    check_profile: String,
    corun: bool,
    /// Gate + witness-only filter partition the coalesce space. Without these
    /// a gated witness-only push (runs only ssr/wasm/isolator-vsock) and a
    /// concurrent advisory full-profile push to the same root+base — which
    /// compute an IDENTICAL plan coalesce token (planned with `only_id=None`)
    /// — would share ONE physical run, and a waiter would receive the wrong
    /// scope's verdict. Keying on them keeps the two runs distinct.
    gate: bool,
    check_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy)]
struct BatchCoalesceConfig {
    /// Anti-thundering-herd grace period: if > 0, the leader waits this long
    /// after the first arrival before draining, allowing simultaneous arrivals
    /// to land in the same batch. Default 0 = off (drain immediately once
    /// inflight == 0). Formerly `CARGOLESS_BATCH_DEBOUNCE_MS`; env var kept
    /// for backward compatibility.
    coalesce_grace: Duration,
    /// Kept for backward compatibility; the env var is parsed but the value is
    /// no longer used as a primary flush trigger. Drain-on-completion supersedes
    /// the max-wait timer.
    #[allow(dead_code)]
    max_wait: Duration,
    /// Hard cap on members per physical run (overflow backstop). Still enforced
    /// by `drain_group`.
    max_members: usize,
    /// Maximum number of physical runs allowed in-flight simultaneously across
    /// ALL keys. Default 1 = strict serial (one checker globally at a time).
    /// Set to 0 to use per-key isolation only (different bases may run in
    /// parallel while each key still drains-on-completion).
    global_inflight_limit: u32,
    /// Number of drain rounds a SoloRed member is held out of after it causes
    /// a fallback. Default 1 = skip the immediately-next drain. 0 = disabled.
    eject_cooldown_rounds: u64,
}

impl Default for BatchCoalesceConfig {
    fn default() -> Self {
        // CARGOLESS_BATCH_MAX_WAIT_MS is parsed but inert (drain-on-completion
        // supersedes the timer). Log nothing here — only at runtime if the env
        // var is set, to avoid spamming tests.
        Self {
            // Small cold-start grace: when NO run is in flight and several
            // submitters arrive at once, the leader waits this brief window so
            // they coalesce into one batch instead of the first running solo.
            // This is NOT the rejected large T/2 window (which taxed every
            // check); steady-state bursts coalesce for free via the inflight
            // gate (arrivals during an active run queue and drain together), so
            // this only adds latency on a genuinely-idle first check.
            coalesce_grace: configured_batch_duration("CARGOLESS_BATCH_DEBOUNCE_MS", 250),
            max_wait: configured_batch_duration("CARGOLESS_BATCH_MAX_WAIT_MS", 1000),
            max_members: configured_batch_usize("CARGOLESS_BATCH_MAX_MEMBERS", 40),
            global_inflight_limit: configured_batch_u32("CARGOLESS_BATCH_GLOBAL_INFLIGHT", 1),
            eject_cooldown_rounds: configured_batch_u64("CARGOLESS_BATCH_EJECT_COOLDOWN_ROUNDS", 1),
        }
    }
}

#[cfg(test)]
type AfterFastPathHook = Arc<dyn Fn(&BatchCheckRequest) + Send + Sync>;

#[derive(Default)]
struct BatchCoalescer {
    state: Mutex<BatchCoalescerState>,
    cv: Condvar,
    #[cfg(test)]
    after_fast_path: Option<AfterFastPathHook>,
    config: BatchCoalesceConfig,
}

/// RAII guard: on Drop, decrements `inflight_runs` and calls `cv.notify_all()`
/// so any cross-key leader blocked in the global-inflight gate wakes up.
/// Constructed immediately after incrementing `inflight_runs`; panic-safe.
struct InflightGuard<'a> {
    coalescer: &'a BatchCoalescer,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        let mut s = poisoned(&self.coalescer.state);
        s.inflight_runs = s.inflight_runs.saturating_sub(1);
        drop(s);
        self.coalescer.cv.notify_all();
    }
}

/// CGLS-25 — standalone global concurrency gate for the Hard-witness
/// compile. A DEDICATED instance of the same discipline the
/// [`BatchCoalescer`] uses for its cross-key inflight gate (Mutex counter +
/// Condvar + RAII decrement/notify), kept separate so the witness lane can
/// be soaked with its own env knob independently of the batch lane.
///
/// `limit == 0` = OFF = unbounded (today's behavior: every distinct-SHA
/// witness runs its own detached compile thread). `limit == N` caps the
/// number of witness compiles running at once across the daemon — the
/// survivors that CGLS-25's overlay-queue fix now lets through no longer
/// thrash a single 240Gi pod. Acquired ONLY inside the witness worker
/// thread; never the serve loop or the supervisor (both must stay
/// non-blocking so verdict latency is never gated by the compile queue).
struct WitnessInflightGate {
    state: Mutex<u32>,
    cv: Condvar,
    /// Witness workers currently PARKED waiting for a slot, distinct from
    /// `state` (workers currently holding one). Observability only — never
    /// read by `acquire`, so it cannot affect admission.
    ///
    /// This counter exists because its absence hid the real serialization
    /// point. `/admin/active` reported only the BatchCoalescer's queue, which
    /// sits DOWNSTREAM of this gate: `acquire_witness_slot()` is taken in the
    /// witness worker before `run_project_checks_and_log` reaches the
    /// coalescer, so with `limit = 1` the coalescer sees one member at a time
    /// and reports a near-empty queue while N witnesses are in fact stacked up
    /// HERE. An operator reading the old snapshot concluded the batcher was
    /// starved of arrivals; it was starved by this gate.
    ///
    /// An `AtomicU64` rather than a second `Mutex` deliberately: `acquire`
    /// mutates it while holding `state`, so a second lock would introduce a
    /// lock ORDER between the two, and a reader taking them the other way
    /// round would deadlock the witness lane. An atomic has no such order,
    /// and a purely-observational counter does not need to be consistent with
    /// `state` under one lock. `Relaxed` suffices — nothing branches on it.
    waiting: AtomicU64,
    limit: u32,
    /// Interval after which a still-queued witness emits an observability line.
    /// This used to be a fail-open budget. That made `limit = 1` stop being a
    /// limit during the exact long compile it exists to protect: the waiter ran
    /// ungated, collided with the holder's warm target, and started a full cold
    /// compile. Holders are RAII-bounded and their compiler subprocesses have
    /// their own deadlines, so a configured limit now remains authoritative.
    queue_budget: Duration,
}

impl Default for WitnessInflightGate {
    fn default() -> Self {
        Self {
            state: Mutex::new(0),
            cv: Condvar::new(),
            waiting: AtomicU64::new(0),
            limit: configured_batch_u32("CARGOLESS_WITNESS_MAX_INFLIGHT", 0),
            queue_budget: Duration::from_millis(configured_batch_u64(
                "CARGOLESS_WITNESS_QUEUE_WAIT_MS",
                600_000,
            )),
        }
    }
}

/// RAII slot: decrements the inflight counter + notifies the next waiter on
/// Drop, on BOTH normal return and worker panic (thread unwind drops the
/// stack guard). `counted == false` (the gate-disabled no-op grant) skips
/// the decrement so the counter never underflows below a slot it never took.
pub(crate) struct WitnessInflightGuard<'a> {
    gate: &'a WitnessInflightGate,
    counted: bool,
}

impl Drop for WitnessInflightGuard<'_> {
    fn drop(&mut self) {
        if !self.counted {
            return;
        }
        let mut s = poisoned(&self.gate.state);
        *s = s.saturating_sub(1);
        drop(s);
        self.gate.cv.notify_all();
    }
}

impl WitnessInflightGate {
    /// Acquire a compile slot. `limit == 0` grants immediately (uncounted
    /// no-op). A configured positive limit never returns an uncounted grant:
    /// the queue interval only produces a progress line, then the waiter keeps
    /// parking until a holder releases. Claim-under-lock: the counter is
    /// incremented in the SAME lock hold that observed it free, so two waiters
    /// cannot both pass (mirrors the BatchCoalescer inflight gate).
    fn acquire(&self) -> WitnessInflightGuard<'_> {
        if self.limit == 0 {
            return WitnessInflightGuard {
                gate: self,
                counted: false,
            };
        }
        let observation_interval = self.queue_budget.max(Duration::from_millis(1));
        let mut deadline = Instant::now() + observation_interval;
        let mut s = poisoned(&self.state);
        // Observational only. Counted from BEFORE the first admission test so a
        // worker that parks is visible for its whole park; `WaitingTicket`'s Drop
        // decrements on EVERY exit — immediate grant or a panic unwinding
        // through here — so the gauge cannot leak upward and strand a phantom
        // queue on `/admin/active`.
        let _waiting = WaitingTicket::new(&self.waiting);
        loop {
            if *s < self.limit {
                *s = s.saturating_add(1);
                return WitnessInflightGuard {
                    gate: self,
                    counted: true,
                };
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                eprintln!(
                    "[cargoless:obs] witness-gate-still-waiting limit={} interval_ms={} — concurrency limit remains enforced",
                    self.limit,
                    observation_interval.as_millis(),
                );
                deadline = Instant::now() + observation_interval;
                continue;
            }
            let (guard, timed_out) = self
                .cv
                .wait_timeout(s, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s = guard;
            if timed_out.timed_out() && *s >= self.limit {
                eprintln!(
                    "[cargoless:obs] witness-gate-still-waiting limit={} interval_ms={} — concurrency limit remains enforced",
                    self.limit,
                    observation_interval.as_millis(),
                );
                deadline = Instant::now() + observation_interval;
            }
        }
    }

    /// Snapshot `(holding, waiting)` for `/admin/active`.
    ///
    /// Takes `state` only momentarily. That is safe despite this being served
    /// on the HTTP thread: `acquire`'s park is `Condvar::wait_timeout`, which
    /// RELEASES the mutex while a witness is queued, so `state` is only ever
    /// held across the short admission test — never across a compile. The two
    /// values are read under different synchronisation and may be momentarily
    /// inconsistent with each other, which is correct for a gauge.
    fn counts(&self) -> (u32, u32) {
        let holding = *poisoned(&self.state);
        let waiting = self.waiting.load(Ordering::Relaxed).min(u32::MAX as u64) as u32;
        (holding, waiting)
    }
}

/// RAII gauge for [`WitnessInflightGate::waiting`]. Increment on construction,
/// decrement on Drop — including on panic — so the counter can never drift up.
struct WaitingTicket<'a> {
    counter: &'a AtomicU64,
}

impl<'a> WaitingTicket<'a> {
    fn new(counter: &'a AtomicU64) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self { counter }
    }
}

impl Drop for WaitingTicket<'_> {
    fn drop(&mut self) {
        // `fetch_update` rather than `fetch_sub`: saturating at zero keeps a
        // stray double-drop from wrapping the gauge to u64::MAX.
        let _ = self
            .counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }
}

/// Round-based ejection mark. A member is held out until
/// `next_run_seq > release_at_run_seq` (strict — anti-starvation).
#[derive(Debug, Clone, Copy)]
struct EjectMark {
    release_at_run_seq: u64,
}

#[derive(Default)]
struct BatchCoalescerState {
    queues: BTreeMap<BatchCoalesceKey, BatchQueue>,
    inflight_runs: u32,
    next_run_seq: u64,
    /// Cross-run cooldown: worktree keys held out of the immediately-next drain
    /// after returning SoloRed. Purged lazily; never starved.
    ejected_until: BTreeMap<String, EjectMark>,
}

#[derive(Default)]
struct BatchQueue {
    waiters: VecDeque<Arc<BatchWaiter>>,
    leader_active: bool,
    first_at: Option<Instant>,
    last_at: Option<Instant>,
}

struct BatchWaiter {
    request: BatchCheckRequest,
    enqueued_at: Instant,
    result: Mutex<Option<BatchReport>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct BatchQueueCounts {
    waiters: u32,
    members: u32,
    inflight_runs: u32,
}

impl BatchCoalescer {
    fn submit(
        &self,
        key: BatchCoalesceKey,
        request: &BatchCheckRequest,
        run: impl Fn(&BatchCheckRequest) -> BatchReport,
    ) -> BatchReport {
        let waiter = Arc::new(BatchWaiter {
            request: request.clone(),
            enqueued_at: Instant::now(),
            result: Mutex::new(None),
        });

        {
            let mut state = poisoned(&self.state);
            let queue = state.queues.entry(key.clone()).or_default();
            let now = Instant::now();
            if queue.waiters.is_empty() {
                queue.first_at = Some(now);
            }
            queue.last_at = Some(now);
            queue.waiters.push_back(Arc::clone(&waiter));
            self.cv.notify_all();
        }

        loop {
            // Optimistic fast path: another leader already produced our result.
            if let Some(report) = poisoned(&waiter.result).clone() {
                return report;
            }

            #[cfg(test)]
            if let Some(hook) = &self.after_fast_path {
                hook(request);
            }

            let mut state = poisoned(&self.state);
            // Re-check while holding the queue-state lock. A leader publishes
            // every drained waiter's result before `finish_leader` removes an
            // empty queue and notifies followers. Without this second check, a
            // follower can read None above, get descheduled, then acquire the
            // lock after queue removal and wait forever on a notification that
            // already happened.
            if let Some(report) = poisoned(&waiter.result).clone() {
                drop(state);
                return report;
            }
            let Some(queue) = state.queues.get_mut(&key) else {
                state = self
                    .cv
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                drop(state);
                continue;
            };

            if !queue.leader_active {
                // Win the leader election for this key.
                queue.leader_active = true;

                // Optional anti-thundering-herd grace: if coalesce_grace > 0,
                // wait briefly so simultaneous arrivals land in the same batch.
                // Default is 0 (off) — lone submitter on a quiet trunk starts
                // with zero added latency.
                if !self.config.coalesce_grace.is_zero() {
                    let grace = self.config.coalesce_grace;
                    let (grace_state, _timeout) = self
                        .cv
                        .wait_timeout(state, grace)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Release the lock before any re-acquisition to avoid
                    // deadlock: finish_leader and poisoned(&self.state) both
                    // acquire self.state, so grace_state must be dropped first.
                    drop(grace_state);
                    // Re-check result after wait (another leader may have done it).
                    if let Some(report) = poisoned(&waiter.result).clone() {
                        // We hold leader_active; give it up cleanly before returning.
                        self.finish_leader(&key);
                        return report;
                    }
                    state = poisoned(&self.state);
                } else {
                    drop(state);
                    state = poisoned(&self.state);
                }

                // Global-inflight gate + CLAIM, atomically. We must reserve the
                // inflight slot in the SAME lock hold that observed it free —
                // otherwise two leaders on different keys both see inflight==0,
                // both pass, and both run concurrently (the serialisation bug).
                // So: wait until inflight < limit, then increment IMMEDIATELY
                // before releasing the lock. (limit==0 disables the gate:
                // per-key isolation only, different bases may run in parallel —
                // we still claim a run_seq for ejection bookkeeping.)
                loop {
                    let gate_open = self.config.global_inflight_limit == 0
                        || state.inflight_runs < self.config.global_inflight_limit;
                    if gate_open {
                        // Claim the slot + bump run_seq under THIS lock hold.
                        state.inflight_runs = state.inflight_runs.saturating_add(1);
                        state.next_run_seq = state.next_run_seq.saturating_add(1);
                        break;
                    }
                    state = self
                        .cv
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Re-check: another leader may have produced our result while
                    // we were parked on the inflight gate. Drop `state` (releases
                    // the lock) before finish_leader, which re-acquires it.
                    if poisoned(&waiter.result).is_some() {
                        drop(state);
                        self.finish_leader(&key);
                        return poisoned(&waiter.result)
                            .clone()
                            .expect("result was Some before finish_leader");
                    }
                }
                // Slot is claimed; arm the RAII guard NOW so any early return /
                // panic from here on decrements inflight + notifies cross-key
                // leaders. `run_seq` is the seq we just bumped.
                let run_seq = state.next_run_seq;
                drop(state);
                let _inflight_guard = InflightGuard { coalescer: self };

                // Drain whatever is queued for this key RIGHT NOW (no timer).
                // `max_members` is enforced inside drain_group as an overflow
                // backstop; any remaining waiters will be picked up next drain.
                // NOTE: drain_group peeks next_run_seq+1 for ejection re-admission;
                // we already bumped next_run_seq above, so an ejected member is
                // re-admitted once a LATER run_seq is reached — consistent.
                let group = self.drain_group(&key);
                if group.is_empty() {
                    // Nothing to run (e.g. all waiters ejected this pass). Release
                    // the claimed slot via the guard drop, give up leadership.
                    drop(_inflight_guard);
                    self.finish_leader(&key);
                    continue;
                }

                let run_start = Instant::now();
                let queue_wait_ms: Vec<u128> = group
                    .iter()
                    .map(|w| run_start.duration_since(w.enqueued_at).as_millis())
                    .collect();

                let combined = combined_request_for(&key, &group, run_seq);
                // A panic in the physical run (e.g. OOM compiling the union)
                // must NOT leave the already-drained non-leader waiters parked
                // forever. Catch it, fan out an indeterminate report to the whole
                // group, and still release the leader slot so the queue recovers.
                // `_inflight_guard` drop fires on both the normal path and the
                // panic path — decrement + notify_all is always guaranteed.
                let combined_report =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&combined)))
                        .unwrap_or_else(|_| {
                            batch_indeterminate(
                                &combined,
                                "coalesced batch run panicked; resubmit to retry",
                            )
                        });

                // Record SoloRed ejections AFTER the run, BEFORE distributing
                // results. The guard hasn't dropped yet here — inflight is still
                // counted — but that's fine: ejection recording only mutates
                // ejected_until, which is separate from the inflight gate.
                self.record_solo_red_ejections(&combined_report, run_seq);

                // Drop the inflight guard here explicitly: decrement + notify_all
                // fires BEFORE distribute so cross-key leaders wake as soon as
                // possible. `distribute_combined_report` does not need the lock.
                drop(_inflight_guard);

                distribute_combined_report(&group, &combined_report, &queue_wait_ms);
                self.finish_leader(&key);
                continue;
            }

            // Follower path: park until woken by finish_leader or InflightGuard.
            let state = self
                .cv
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(state);
        }
    }

    /// Record ejections for every member that returned SoloRed. Called after
    /// a physical run completes, under a fresh lock acquisition inside.
    fn record_solo_red_ejections(&self, report: &BatchReport, run_seq: u64) {
        use cargoless_core::batch::BatchProvenance;
        let cooldown = self.config.eject_cooldown_rounds;
        if cooldown == 0 {
            return; // Feature disabled.
        }
        let solo_reds: Vec<String> = report
            .members
            .iter()
            .filter(|m| m.provenance == BatchProvenance::SoloRed)
            .map(|m| m.worktree.clone())
            .collect();
        if solo_reds.is_empty() {
            return;
        }
        let mut state = poisoned(&self.state);
        for worktree in solo_reds {
            state.ejected_until.insert(
                worktree,
                EjectMark {
                    release_at_run_seq: run_seq.saturating_add(cooldown),
                },
            );
        }
    }

    /// Drain waiters for `key` into a group, respecting `max_members` and
    /// skipping any waiter whose sole member is in the SoloRed cooldown set.
    /// Skipped waiters stay in `queue.waiters` so they are picked up by the
    /// next drain (anti-starvation: admission is strict `next_run_seq >
    /// release_at_run_seq`).
    fn drain_group(&self, key: &BatchCoalesceKey) -> Vec<Arc<BatchWaiter>> {
        let mut state = poisoned(&self.state);
        // The caller already bumped `next_run_seq` to THIS run's seq before
        // draining (under the inflight-gate lock), so `next_run_seq` here is the
        // current run's seq. An ejected waiter stays held while this seq is
        // `<= release_at_run_seq` and is re-admitted once a strictly-later run
        // reaches it (anti-starvation).
        let next_run_seq = state.next_run_seq;

        if !state.queues.contains_key(key) {
            return Vec::new();
        }

        // Phase 1 (read-only): decide which indices to admit vs skip.
        // We separate the read phase (touching both state.queues and
        // state.ejected_until immutably) from the mutation phase to
        // satisfy the borrow checker.
        let queue_len = state.queues[key].waiters.len();
        let mut admit_indices: Vec<usize> = Vec::new();
        let mut member_count = 0usize;
        let mut ejection_purges: Vec<String> = Vec::new();
        // Indices of single-member waiters held out THIS pass because their
        // cooldown is still active. Tracked so that if the cooldown skip would
        // otherwise leave the drain EMPTY, we admit the oldest held one rather
        // than spin (a skipped-into-empty drain never advances next_run_seq, so
        // the cooldown would never elapse → starvation).
        let mut cooldown_held: Vec<usize> = Vec::new();

        'outer: for idx in 0..queue_len {
            let next = &state.queues[key].waiters[idx];
            let next_members = next.request.members.len().max(1);

            // max_members overflow backstop: once the group has at least one
            // member, stop before adding another that would exceed the cap.
            if !admit_indices.is_empty() && member_count + next_members > self.config.max_members {
                break 'outer;
            }

            // Cross-run culprit ejection (single-member push-path only). Hold a
            // just-SoloRed culprit out of the next SHARED batch so it can't
            // force a solo-fallback that slows innocent members.
            if next_members == 1 {
                let worktree = &next.request.members[0].worktree;
                if let Some(&mark) = state.ejected_until.get(worktree) {
                    if next_run_seq <= mark.release_at_run_seq {
                        // Cooldown still active — defer this waiter for now.
                        cooldown_held.push(idx);
                        continue;
                    }
                    // Cooldown expired — schedule lazy purge, then admit below.
                    ejection_purges.push(worktree.clone());
                }
            }

            admit_indices.push(idx);
            member_count += next_members;
        }

        // Anti-starvation / anti-spin: if cooldown skips left the drain empty
        // (the culprit is alone — there is no batch to protect), admit the
        // OLDEST held waiter so the run makes forward progress. Its mark is
        // purged so it isn't re-held next pass.
        if admit_indices.is_empty() {
            if let Some(&oldest_held) = cooldown_held.first() {
                if let Some(member) = state.queues[key].waiters[oldest_held]
                    .request
                    .members
                    .first()
                {
                    ejection_purges.push(member.worktree.clone());
                }
                admit_indices.push(oldest_held);
            }
        }

        // Phase 2 (mutation): purge expired/forced ejections, then pop admitted.
        for worktree in ejection_purges {
            state.ejected_until.remove(&worktree);
        }

        // Remove admitted waiters in REVERSE index order so earlier indices remain
        // valid across each VecDeque::remove call.
        let mut group: Vec<Arc<BatchWaiter>> = Vec::with_capacity(admit_indices.len());
        let queue = state
            .queues
            .get_mut(key)
            .expect("key present, checked above");
        for &idx in admit_indices.iter().rev() {
            let waiter = queue.waiters.remove(idx).expect("index valid");
            group.push(waiter);
        }
        // Reverse-pop produced reverse insertion order; restore FIFO order.
        group.reverse();

        if queue.waiters.is_empty() {
            queue.first_at = None;
            queue.last_at = None;
        } else {
            let now = Instant::now();
            queue.first_at = Some(now);
            queue.last_at = Some(now);
        }
        group
    }

    fn finish_leader(&self, key: &BatchCoalesceKey) {
        let mut state = poisoned(&self.state);
        let should_remove = if let Some(queue) = state.queues.get_mut(key) {
            queue.leader_active = false;
            queue.waiters.is_empty()
        } else {
            false
        };
        if should_remove {
            state.queues.remove(key);
        }
        self.cv.notify_all();
    }

    fn counts(&self) -> BatchQueueCounts {
        let state = poisoned(&self.state);
        let mut counts = BatchQueueCounts {
            inflight_runs: state.inflight_runs,
            ..BatchQueueCounts::default()
        };
        for queue in state.queues.values() {
            counts.waiters += queue.waiters.len() as u32;
            counts.members += queue_member_count(queue) as u32;
        }
        counts
    }
}

fn configured_batch_duration(name: &str, default_ms: u64) -> Duration {
    Duration::from_millis(
        std::env::var(name)
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(default_ms),
    )
}

fn configured_batch_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn configured_batch_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(default)
}

fn configured_batch_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(default)
}

/// Path D (addressing) — resolve `CARGOLESS_WITNESS_HISTORY_CAP` (default
/// [`HARD_WITNESS_HISTORY_CAP_DEFAULT`]). Pure over its input so the
/// default / override / invalid arms are unit-testable without env
/// mutation. `Some("0")` and unparseable input both fall back to the
/// default (a zero-cap ring would immediately evict every publish, which
/// is never the operator intent — matches CGLS-28's "typo doesn't arm a
/// dangerous knob" pattern).
pub(crate) fn witness_history_cap_from(env: Option<&str>) -> usize {
    env.and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(HARD_WITNESS_HISTORY_CAP_DEFAULT)
}

/// R3 mitigation — resolve `CARGOLESS_PUSHED_MAX_PER_WT` (default
/// [`PUSHED_MAX_PER_WT_DEFAULT`]). Same fallback rules as
/// [`witness_history_cap_from`]. A zero cap would reject every push, so
/// it degrades to the default.
pub(crate) fn pushed_max_per_wt_from(env: Option<&str>) -> usize {
    env.and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(PUSHED_MAX_PER_WT_DEFAULT)
}

fn batch_coalesce_key(request: &BatchCheckRequest) -> Option<BatchCoalesceKey> {
    let coalesce_key = request
        .coalesce_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())?
        .to_string();
    Some(BatchCoalesceKey {
        coalesce_key,
        base_ref: request.base_ref.clone(),
        analysis_root: request.options.analysis_root.clone(),
        repo_relative: request.options.repo_relative,
        check_profile: format!("{:?}", request.check_profile),
        corun: request.corun,
        gate: request.options.gate,
        check_ids: request.options.check_ids.clone(),
    })
}

/// Pick the coalesce token for a project-check push. The granularity is
/// deliberately different for the two modes:
///
///   * **Non-gated** (warn/advisory) project-checks use the fine-grained
///     per-plan fingerprint (`project-check-plan:{fp}`), so pushes that select
///     DIFFERENT check subsets do not co-run and pollute each other's advisory
///     diagnostics.
///
///   * **Gated** (hard) witness pushes take a COARSE key keyed only on the base
///     ref (`witness-gate:{base_ref}`). Every gated push on a given base runs
///     the same physical `cargo check --release` against the base-tip mirror, so
///     grouping them by exact changed-file set is pure fragmentation: on a hot
///     trunk each PR's file set differs slightly → a distinct fingerprint → a
///     distinct coalesce queue → N serialized 15–20 min release compiles while
///     `GLOBAL_INFLIGHT=1` holds the single mirror. Keying by base instead lets
///     all pending gated pushes flatten into ONE `run_batch`, which already
///     returns `CombinedGreen` to every member on a green union and falls back
///     to per-member solo checks to attribute a red honestly
///     (`batch.rs::run_batch`). This trades exactness — a member is checked
///     against base-tip + the batch's union overlay, not its own base — for the
///     throughput the witness lane needs under a push storm.
///
/// **Manifest safety (both modes):** a push whose overlay edits
/// `cargoless.checks.yaml` changes the plan itself, so it must NOT share a
/// physical run with pushes computed against the un-edited manifest. The gated
/// branch preserves the same guard the fine token applies (return `None` ⇒ the
/// caller falls back to a solo run) before keying by base.
fn gated_or_plan_coalesce_token(
    root: &Path,
    gate: bool,
    request: &BatchCheckRequest,
) -> Option<String> {
    if gate {
        if request_overlay_touches_project_check_manifest(root, request) {
            eprintln!(
                "[cargoless:obs] witness-gate root={} coalesce=false reason={} overlay changed",
                root.display(),
                PROJECT_CHECK_MANIFEST_NAME
            );
            return None;
        }
        let base_ref = request.base_ref.trim();
        eprintln!(
            "[cargoless:obs] witness-gate root={} coalesce=true base_ref={}",
            root.display(),
            base_ref
        );
        return Some(format!("witness-gate:{}", base_ref));
    }
    project_check_plan_coalesce_token(root, request)
}

fn project_check_plan_coalesce_token(root: &Path, request: &BatchCheckRequest) -> Option<String> {
    if request_overlay_touches_project_check_manifest(root, request) {
        eprintln!(
            "[cargoless:obs] project-check-plan root={} coalesce=false reason={} overlay changed",
            root.display(),
            PROJECT_CHECK_MANIFEST_NAME
        );
        return None;
    }

    let changed_files = union_changed_files(&request.members);
    let changed_files = (!changed_files.is_empty()).then_some(changed_files);
    let plan = match plan_dev_with_changes(root, changed_files.as_deref()) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!(
                "[cargoless:obs] project-check-plan root={} coalesce=false error={}",
                root.display(),
                e
            );
            return None;
        }
    };
    if !plan.coalesceable {
        eprintln!(
            "[cargoless:obs] project-check-plan root={} coalesce=false reason={}",
            root.display(),
            plan.non_coalesce_reason
                .as_deref()
                .unwrap_or("plan marked non-coalesceable")
        );
        return None;
    }
    Some(format!("project-check-plan:{}", plan.fingerprint))
}

fn request_overlay_touches_project_check_manifest(
    root: &Path,
    request: &BatchCheckRequest,
) -> bool {
    request.members.iter().any(|member| {
        member
            .files
            .iter()
            .any(|(path, _)| overlay_path_matches_project_check_manifest(root, Path::new(path)))
    })
}

fn overlay_path_matches_project_check_manifest(root: &Path, path: &Path) -> bool {
    let manifest = Path::new(PROJECT_CHECK_MANIFEST_NAME);
    if path.is_absolute() {
        return path.strip_prefix(root).is_ok_and(|rel| rel == manifest);
    }
    safe_repo_relative_path(&path.to_string_lossy()).is_ok_and(|rel| rel == manifest)
}

fn queue_member_count(queue: &BatchQueue) -> usize {
    queue
        .waiters
        .iter()
        .map(|waiter| waiter.request.members.len().max(1))
        .sum()
}

fn combined_request_for(
    key: &BatchCoalesceKey,
    group: &[Arc<BatchWaiter>],
    run_seq: u64,
) -> BatchCheckRequest {
    let first = &group[0].request;
    let mut request = first.clone();
    request.batch_id = format!("coalesced:{}:run-{}", key.coalesce_key, run_seq);
    request.coalesce_key = None;
    request.members = group
        .iter()
        .flat_map(|waiter| waiter.request.members.clone())
        .collect();
    request
}

fn distribute_combined_report(
    group: &[Arc<BatchWaiter>],
    combined: &BatchReport,
    queue_wait_ms: &[u128],
) {
    let mut offset = 0usize;
    let executed_members = combined.members.len() as u32;
    for (idx, waiter) in group.iter().enumerate() {
        let count = waiter.request.members.len();
        let end = offset.saturating_add(count).min(combined.members.len());
        let members = combined.members[offset..end].to_vec();
        offset = end;
        let verdict = verdict_for_members(&members);
        let report = BatchReport {
            batch_id: waiter.request.batch_id.clone(),
            verdict,
            members,
            combined_checks: combined.combined_checks,
            solo_checks: combined.solo_checks,
            duration_ms: combined.duration_ms,
            queue_wait_ms: queue_wait_ms.get(idx).copied().unwrap_or(0),
            executed_members,
            executed_batch_id: Some(combined.batch_id.clone()),
        };
        *poisoned(&waiter.result) = Some(report);
    }
}

fn verdict_for_members(members: &[cargoless_core::batch::BatchMemberResult]) -> BatchVerdict {
    if members
        .iter()
        .any(|member| member.verdict == cargoless_core::batch::BatchVerdict::Indeterminate)
    {
        BatchVerdict::Indeterminate
    } else if members
        .iter()
        .any(|member| member.verdict == cargoless_core::batch::BatchVerdict::Red)
    {
        BatchVerdict::Red
    } else {
        BatchVerdict::Green
    }
}

impl ServeVerdictState {
    /// Construct empty. Returns `Self` (NOT `Arc<Self>`) on purpose —
    /// `fn new() -> Arc<Self>` trips `clippy::new_ret_no_self` under the
    /// `-D warnings` gate; callers wrap in `Arc` (the house pattern, cf.
    /// `inproc::testmock::MockService`).
    ///
    /// Path D + R3 caps are resolved from env HERE, once per state —
    /// tests that need explicit caps use
    /// [`Self::with_caps_for_testing`].
    pub fn new() -> Self {
        Self {
            witness_history_cap: witness_history_cap_from(
                std::env::var("CARGOLESS_WITNESS_HISTORY_CAP")
                    .ok()
                    .as_deref(),
            ),
            pushed_max_per_wt: pushed_max_per_wt_from(
                std::env::var("CARGOLESS_PUSHED_MAX_PER_WT").ok().as_deref(),
            ),
            ..Self::default()
        }
    }

    /// Turn on the build lane for `repo`, building candidates on `base_ref`
    /// and running the named `cargoless.checks.yaml` profile as its legs.
    ///
    /// Opt-in, and off unless the operator asks: a lane merges and publishes,
    /// so a daemon must never acquire one as a side effect of a default. The
    /// caller supplies the profile because the legs are the *project's* — that
    /// is what makes the lane reusable rather than a tf-multiverse feature.
    ///
    /// `artifact_path` (relative to the candidate root) is what the lander
    /// publishes on green. `None` is a check-only lane: it proves the merged
    /// tree compiles and deliberately leaves the pointer alone rather than
    /// advancing it to nothing.
    ///
    /// `dispatch` selects WHERE the legs run. `None` compiles them in this
    /// process; `Some((argv, remote, ref_prefix))` publishes the candidate and
    /// hands it to an external builder.
    ///
    /// `intergeneration_yield` keeps the lane observably idle after a blocking
    /// generation so a cooperative external trunk writer cannot be starved by
    /// an always-nonempty queue. It delays no in-flight build or land.
    ///
    /// In-process is the default because it is the zero-config one — a single
    /// developer's laptop has no builder to dispatch to. It is NOT the safe one
    /// for a multi-tenant daemon: `cargo` executes `build.rs` and proc-macros
    /// from the candidate, which is unreviewed code, so a daemon whose
    /// container can reach a credential (tf-multiverse's can reach a
    /// push-capable forge token via `.git/config` on its shared volume, checked
    /// 2026-07-31) must dispatch instead. See `DispatchLegRunner`.
    #[must_use]
    pub fn with_lane(
        mut self,
        repo: &Path,
        state_dir: &Path,
        base_ref: &str,
        plan: cargoless_core::lanedrv::LegPlan,
        land_command: Option<Vec<String>>,
        intergeneration_yield: Duration,
    ) -> Self {
        let tree = cargoless_core::lanetree::GitCandidateTree::new(
            repo,
            state_dir.join("lane-candidates"),
            base_ref,
        );
        // The lander follows the PLAN, so the two cannot disagree. Only an
        // in-process plan with an artifact path produces a local file to
        // publish; both remote plans build elsewhere and report `artifact:
        // None`, so pairing either with `PointerLander` would leave it taking
        // its "green with nothing to publish" branch forever — no error, no
        // pointer movement, and an operator watching a publishing lane publish
        // nothing. The caller refuses that combination at boot; this makes it
        // unrepresentable here.
        let publishing = plan.publishes_locally();
        // The description is the caller's to log — `servedrv` announces it in
        // the boot line beside the profile and base, where an operator reads it.
        let (legs, _where) = plan.into_runner();
        // The lander follows the artifact setting so the two cannot disagree.
        //
        // No artifact ⇒ report-only: the lane proves the merged tree builds and
        // ships nothing. That is the shape to shadow-run in, and it makes the
        // safe configuration also the DEFAULT one — an operator who omits
        // `CARGOLESS_LANE_ARTIFACT` gets a lane that cannot touch the pointer,
        // rather than one that publishes an empty payload.
        //
        // Pairing them here rather than exposing two independent knobs removes
        // the state where a check-only lane still holds a publishing lander:
        // it would take the "green with no artifact" branch every time, which
        // is harmless today but is one refactor away from advancing a pointer
        // to nothing.
        //
        // Two spawn calls, one per LANDER. The runner is boxed above (see the
        // `Box<dyn LegRunner>` impl) because it has two variants of its own, and
        // monomorphising both dimensions would mean four `spawn` bodies here.
        //
        // The verdict trail, beside the witness's own `witness-legs.log` in the
        // same state dir. A lane build is tens of minutes and the candidate
        // worktree (with its target dir) is destroyed the moment it ends, so
        // without this there is nothing left to read afterwards — measured on
        // the first shadow run, which compiled for 76 minutes and left no
        // recoverable verdict.
        let trail = state_dir.join("lane-runs.log");
        // Boxed, one `spawn` body. Three landers × three runners would be nine
        // monomorphised branches here; landing runs once per green build, so
        // the vtable hop costs nothing measurable.
        let lander: Box<dyn cargoless_core::lanedrv::LaneLander + Send> = match land_command {
            // AUTO-MERGE. Deliberately last-resort in this match order so the
            // safe shapes win by default: an operator gets report-only unless
            // they explicitly name a lander command.
            Some(cmd) => Box::new(cargoless_core::lanedrv::CommandLander::new(cmd)),
            None if publishing => Box::new(cargoless_core::lanedrv::PointerLander::new(repo)),
            None => Box::new(cargoless_core::lanedrv::ReportOnlyLander),
        };
        // CLOSE ANY GENERATION THE PREVIOUS PROCESS LEFT OPEN.
        //
        // `run_build` writes `lane-build-start` before it compiles and the
        // matching `lane-build … outcome=…` only after, so a daemon killed
        // mid-build leaves a start with no end — and the next process begins at
        // generation 0, never mentioning it again. Measured 2026-08-03/04: 15
        // such orphans across 200 starts, one per lane-shadow roll, each one a
        // build that burned real minutes and reported nothing. Worse, the
        // record is INDISTINGUISHABLE from a build still running, so the log
        // cannot answer "is the lane working?" after the fact.
        //
        // Deliberately NOT persisted state: the trail already records what was
        // in flight, so reading it back is enough and there is no new file to
        // keep consistent. Same instinct that makes enrollment survive a
        // restart — reconstruct rather than persist.
        //
        // Best-effort by construction. An unreadable or absent trail must never
        // stop the lane from starting; the worst case is the status quo.
        cargoless_core::lanedrv::close_abandoned_generations(&trail);

        self.lane = Some(LaneHost::spawn_with_intergeneration_yield(
            LaneState::new(repo),
            cargoless_core::lanedrv::LaneDriver::new(tree, legs, lander).with_trail(trail),
            intergeneration_yield,
        ));
        self
    }

    /// Test hook: set the addressing + push queue caps explicitly, without
    /// Advance the build lane's clock. No-op when no lane is configured.
    ///
    /// **The lane does not run without this.** Its capture window and ejection
    /// TTLs are both measured in ticks, and `LaneState`'s clock moves ONLY on
    /// `LaneEvent::Tick` — so with nothing driving it `now` stays 0 forever, the
    /// default 60-tick window never elapses, and a build only ever starts when
    /// the queue happens to reach `max_members`. A lane that accepts
    /// submissions, reports "queued", and silently never builds is the worst
    /// possible failure: it looks like a transport or auth problem, not a
    /// missing heartbeat.
    ///
    /// Ejection TTLs lapse on the same signal, so without it an ejection is
    /// permanent — the backstop that is supposed to guarantee nothing is stuck
    /// forever would itself never fire.
    pub fn lane_tick(&self, now: u64) {
        if let Some(lane) = self.lane.as_ref() {
            lane.tick(now);
        }
    }

    /// mutating process env. Used by the cap unit tests so the shipped
    /// [`Self::new`] env-read path stays untouched. Retains the historical
    /// stored value for the other cap so a test only overrides what it
    /// cares about.
    #[cfg(test)]
    pub(crate) fn with_caps_for_testing(
        mut self,
        witness_history_cap: usize,
        pushed_max_per_wt: usize,
    ) -> Self {
        self.witness_history_cap = witness_history_cap;
        self.pushed_max_per_wt = pushed_max_per_wt;
        self
    }

    /// Path D — the resolved cap this daemon runs under, for the boot
    /// obs line and any future `/daemon` surface.
    pub fn witness_history_cap(&self) -> usize {
        self.witness_history_cap
    }

    /// R3 — the resolved cap this daemon runs under, for the boot
    /// obs line and any future `/daemon` surface.
    pub fn pushed_max_per_wt(&self) -> usize {
        self.pushed_max_per_wt
    }

    /// Use the daemon's resolved state directory for transient
    /// project-check scratch worktrees. This keeps slow advisory/project
    /// checks out of the shared mutable analysis root.
    pub fn with_project_check_state_dir(mut self, state_dir: PathBuf) -> Self {
        self.evidence_store_v3 = Some(EvidenceStore::new(&state_dir));
        self.project_check_state_dir = Some(state_dir);
        self
    }

    fn remember_outcome_v3(&self, outcome: OutcomeEnvelope) {
        let attempt_id = outcome.attempt_id.clone();
        let mut outcomes = poisoned(&self.outcomes_v3);
        let mut order = poisoned(&self.outcome_order_v3);
        if outcomes.insert(attempt_id.clone(), outcome).is_some() {
            order.retain(|existing| existing != &attempt_id);
        }
        order.push_back(attempt_id);
        while order.len() > OUTCOME_V3_MEMORY_CAP {
            if let Some(expired) = order.pop_front() {
                outcomes.remove(&expired);
            }
        }
    }

    fn begin_outcome_v3(
        &self,
        context: &AttemptContext,
        surface: Surface,
        subject: Subject,
        phase: Phase,
        summary: impl Into<String>,
    ) -> OutcomeEnvelope {
        let now = now_unix_ms();
        let mut outcome = OutcomeEnvelope::new(
            context.request_id.clone(),
            context.attempt_id.clone(),
            context.trace_id.clone(),
            surface,
            subject,
            producer_v3(),
            Conclusion::Pending {
                phase,
                retry: None,
                summary: text_v3(summary),
            },
        );
        let attempt_digest = sha256_hex(context.attempt_id.as_str().as_bytes());
        let execution_id = ExecutionId::new(format!(
            "execution.{}.{}.{}",
            std::process::id(),
            now,
            &attempt_digest[..16]
        ))
        .expect("generated execution identity is contract-safe");
        outcome.execution_id = Some(execution_id.clone());
        outcome.relations.push(Relation {
            kind: RelationKind::ExecutedBy,
            attempt_id: None,
            execution_id: Some(execution_id),
        });
        outcome.timeline = vec![
            PhaseRecord {
                phase: Phase::Accepted,
                started_at_unix_ms: now,
                finished_at_unix_ms: Some(now),
            },
            PhaseRecord {
                phase,
                started_at_unix_ms: now,
                finished_at_unix_ms: None,
            },
        ];
        if let Some(previous_attempt_id) = context.previous_attempt_id.clone() {
            outcome.relations.push(Relation {
                kind: RelationKind::RetriedFrom,
                attempt_id: Some(previous_attempt_id),
                execution_id: None,
            });
        }
        self.remember_outcome_v3(outcome.clone());
        outcome
    }

    fn forget_outcome_v3(&self, attempt_id: &AttemptId) {
        poisoned(&self.outcomes_v3).remove(attempt_id);
        poisoned(&self.outcome_order_v3).retain(|existing| existing != attempt_id);
    }

    pub(crate) fn record_ra_evidence_v3(
        &self,
        context: Option<&AttemptContext>,
        snapshot: RaStderrSnapshot,
    ) {
        let Some(context) = context else {
            return;
        };
        poisoned(&self.ra_evidence_v3).insert(context.attempt_id.clone(), snapshot);
    }

    fn finish_outcome_v3(
        &self,
        context: &AttemptContext,
        payload: &crate::statusfile::VerdictPayload,
        gated_checks_ran: &[String],
        verified_project_checks: &[VerifiedProjectCheckEvidence],
        worktree: &str,
    ) {
        let Some(mut outcome) = poisoned(&self.outcomes_v3)
            .get(&context.attempt_id)
            .cloned()
        else {
            tracing::error!(
                attempt_id = %context.attempt_id,
                request_id = %context.request_id,
                "outcome-v3 publish has no accepted attempt"
            );
            return;
        };

        let published_at = now_unix_ms();
        if let Some(queued) = outcome
            .timeline
            .iter_mut()
            .rev()
            .find(|record| record.finished_at_unix_ms.is_none())
        {
            queued.finished_at_unix_ms = Some(published_at);
        }
        outcome.timeline.push(PhaseRecord {
            phase: Phase::Publishing,
            started_at_unix_ms: published_at,
            finished_at_unix_ms: Some(published_at),
        });

        let mut evidence = EvidenceBundle::default();
        let ra_snapshot = poisoned(&self.ra_evidence_v3).remove(&context.attempt_id);
        evidence.push(
            ArtifactKind::Events,
            format!(
                "{{\"at_unix_ms\":{published_at},\"event\":\"verdict.publish\",\
                 \"attempt_id\":\"{}\",\"request_id\":\"{}\",\"trace_id\":\"{}\",\
                 \"worktree\":{},\"verdict\":\"{}\",\"red_diagnostics\":{},\
                 \"failure_reason\":{}}}\n",
                context.attempt_id,
                context.request_id,
                context.trace_id,
                serde_json::to_string(worktree).expect("worktree JSON"),
                payload.verdict.as_str(),
                payload.red_diagnostics,
                serde_json::to_string(&payload.analysis_failure_reason).expect("reason JSON"),
            ),
        );
        let project_check_evidence_error = if verified_project_checks.len() > 999 {
            Some(format!(
                "{} verified project-check results exceed the 999-artifact evidence limit",
                verified_project_checks.len()
            ))
        } else {
            for (index, result) in verified_project_checks.iter().enumerate() {
                let sequence = u32::try_from(index + 1)
                    .expect("the verified project-check evidence limit fits u32");
                tracing::debug!(
                    attempt_id = %context.attempt_id,
                    check_id = %result.check_id,
                    sequence,
                    "retaining verified candidate result evidence"
                );
                evidence.push(
                    ArtifactKind::ProjectCheckResult(sequence),
                    result.bytes.clone(),
                );
            }
            None
        };
        let (
            ra_process_generation,
            ra_pid,
            ra_total_lines,
            ra_error_lines,
            ra_suppressed_lines,
            ra_overflow_fingerprints,
            ra_fingerprints,
        ) = match ra_snapshot.as_ref() {
            Some(snapshot) => (
                snapshot.process_generation,
                snapshot.pid,
                snapshot.total_lines,
                snapshot.error_lines,
                snapshot.suppressed_lines,
                snapshot.overflow_fingerprints,
                snapshot
                    .fingerprints
                    .iter()
                    .map(|fingerprint| {
                        serde_json::json!({
                            "fingerprint": fingerprint.fingerprint,
                            "count": fingerprint.count,
                            "level": fingerprint.level,
                            "sample": fingerprint.sample,
                        })
                    })
                    .collect::<Vec<_>>(),
            ),
            None => (0, None, 0, 0, 0, 0, Vec::new()),
        };
        let ra_internal_error = ra_snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .fingerprints
                .iter()
                .filter(|fingerprint| fingerprint.level == "error")
                .max_by_key(|fingerprint| fingerprint.count)
                .cloned()
        });
        let ra_storm = ra_internal_error
            .as_ref()
            .filter(|fingerprint| fingerprint.count >= 1000)
            .cloned();
        evidence.push(
            ArtifactKind::RustAnalyzerSummary,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": "cargoless.rust-analyzer-summary/v3",
                "classification": if gated_checks_ran.is_empty() {
                    "rust_analyzer_flycheck"
                } else {
                    "project_check"
                },
                "process_generation": ra_process_generation,
                "pid": ra_pid,
                "stderr": {
                    "total_lines": ra_total_lines,
                    "error_lines": ra_error_lines,
                    "duplicates_suppressed": ra_suppressed_lines,
                    "overflow_fingerprints": ra_overflow_fingerprints,
                    "fingerprints": ra_fingerprints,
                },
                "reported_error_diagnostics": payload.red_diagnostics,
                "item_level_diagnostics_retained": false,
                "gated_checks_executed": gated_checks_ran,
            }))
            .expect("summary JSON"),
        );
        if let Some(snapshot) = ra_snapshot {
            if !snapshot.tail.is_empty() {
                evidence.push(
                    ArtifactKind::StderrTail,
                    format!("{}\n", snapshot.tail.join("\n")),
                );
            }
            for (index, stack) in snapshot.stack_captures.into_iter().enumerate() {
                evidence.push(ArtifactKind::Stack(index as u32 + 1), stack);
            }
        }
        let reference_store = self
            .evidence_store_v3
            .clone()
            .unwrap_or_else(|| EvidenceStore::new("."));
        let evidence_ref = match reference_store.reference_for(&context.attempt_id, &evidence) {
            Ok(reference) => reference,
            Err(error) => {
                tracing::error!(
                    attempt_id = %context.attempt_id,
                    error = %error,
                    "could not construct outcome-v3 evidence reference"
                );
                return;
            }
        };

        let origin = if gated_checks_ran.is_empty() {
            DiagnosticOrigin::RustAnalyzerFlycheck
        } else {
            DiagnosticOrigin::ProjectCheck
        };
        let conclusion = match payload.verdict {
            crate::statusfile::Verdict::Green => Conclusion::Passed {
                basis: if gated_checks_ran.is_empty() {
                    PassBasis::DiagnosticsClear { origin }
                } else {
                    PassBasis::ChecksPassed {
                        requested_check_ids: gated_checks_ran.iter().map(text_v3).collect(),
                        executed_check_ids: gated_checks_ran.iter().map(text_v3).collect(),
                    }
                },
                evidence: evidence_ref,
                summary: text_v3(if gated_checks_ran.is_empty() {
                    "rust-analyzer flycheck completed with no blocking diagnostics"
                } else {
                    "every requested blocking project check executed and passed"
                }),
            },
            crate::statusfile::Verdict::Red
                if gated_checks_ran.is_empty() && ra_storm.is_some() =>
            {
                let storm = ra_storm.as_ref().expect("guarded by is_some");
                Conclusion::Indeterminate {
                    cause: cargoless_core::outcome::IndeterminateCause::AnalyzerPathology {
                        component: OutcomeComponent::RustAnalyzer,
                        signature: text_v3(&storm.fingerprint),
                        repeated_events: storm.count,
                    },
                    retry: RetryDirective::OperatorRequired,
                    evidence: evidence_ref,
                    summary: text_v3(format!(
                        "rust-analyzer emitted the same internal error {} times; this is an analyzer pathology, not a compiler diagnostic",
                        storm.count
                    )),
                }
            }
            crate::statusfile::Verdict::Red => {
                let count = std::num::NonZeroU32::new(payload.red_diagnostics)
                    .expect("VerdictPayload makes red-with-zero unrepresentable");
                Conclusion::Failed {
                    cause: cargoless_core::outcome::FailureCause::UnlocatedDiagnosticReport {
                        origin,
                        authority: Authority::Blocking,
                        reported_count: count,
                        producer: text_v3("legacy_verdict_payload"),
                        raw_report_digest: text_v3(sha256_hex(
                            format!(
                                "{}:{}:{:?}",
                                payload.verdict.as_str(),
                                payload.red_diagnostics,
                                payload.analysis_failure_reason
                            )
                            .as_bytes(),
                        )),
                    },
                    path_overlap: PathOverlap::NotComputable,
                    evidence: evidence_ref,
                    summary: text_v3(format!(
                        "producer reported {} blocking diagnostic(s), but item-level file, line, \
                         code, and message records were not retained",
                        payload.red_diagnostics
                    )),
                }
            }
            crate::statusfile::Verdict::Unknown => {
                let reason = payload
                    .analysis_failure_reason
                    .as_deref()
                    .filter(|reason| !reason.trim().is_empty())
                    .unwrap_or("producer returned unknown without a reason");
                let (cause, retry) = if reason == "ra_native_attempt_stderr_error" {
                    let (signature, repeated_events) = ra_internal_error
                        .as_ref()
                        .map(|fingerprint| (fingerprint.fingerprint.as_str(), fingerprint.count))
                        .unwrap_or(("rust-analyzer-stderr-error", ra_error_lines.max(1)));
                    (
                        cargoless_core::outcome::IndeterminateCause::AnalyzerPathology {
                            component: OutcomeComponent::RustAnalyzer,
                            signature: text_v3(signature),
                            repeated_events,
                        },
                        RetryDirective::OperatorRequired,
                    )
                } else if matches!(
                    reason,
                    "ra_blind_path_green_unwitnessed"
                        | "ra_native_timer_settled_no_flycheck_activity"
                        | "ra_native_unattributed_error"
                ) {
                    (
                        cargoless_core::outcome::IndeterminateCause::CompilerWitnessRequired {
                            component: OutcomeComponent::RustAnalyzer,
                            limitation: text_v3(
                                "rust-analyzer did not produce an attributable authoritative compiler result for this input",
                            ),
                        },
                        RetryDirective::NewInputRequired,
                    )
                } else if reason.starts_with("ra_respawn_") {
                    (
                        cargoless_core::outcome::IndeterminateCause::ProcessLost {
                            component: OutcomeComponent::RustAnalyzer,
                            respawned: true,
                        },
                        RetryDirective::Automatic {
                            attempt: context.attempt_number,
                            maximum_attempts: context.maximum_attempts,
                            after_ms: context.retry_after_ms,
                        },
                    )
                } else if reason.starts_with("ra_spawn_failed") {
                    (
                        cargoless_core::outcome::IndeterminateCause::ProcessLost {
                            component: OutcomeComponent::RustAnalyzer,
                            respawned: false,
                        },
                        RetryDirective::Automatic {
                            attempt: context.attempt_number,
                            maximum_attempts: context.maximum_attempts,
                            after_ms: context.retry_after_ms,
                        },
                    )
                } else if reason.contains("timeout") {
                    (
                        cargoless_core::outcome::IndeterminateCause::BudgetExhausted {
                            component: OutcomeComponent::ProjectCheck,
                            budget: text_v3(reason),
                        },
                        RetryDirective::Automatic {
                            attempt: context.attempt_number,
                            maximum_attempts: context.maximum_attempts,
                            after_ms: context.retry_after_ms,
                        },
                    )
                } else {
                    (
                        cargoless_core::outcome::IndeterminateCause::InternalContractViolation {
                            invariant: text_v3(reason),
                        },
                        RetryDirective::OperatorRequired,
                    )
                };
                Conclusion::Indeterminate {
                    cause,
                    retry,
                    evidence: evidence_ref,
                    summary: text_v3(format!(
                        "evaluation was indeterminate because {reason}; this is not a code failure"
                    )),
                }
            }
        };
        outcome.conclude(conclusion);
        outcome.timeline.push(PhaseRecord {
            phase: Phase::Terminal,
            started_at_unix_ms: published_at,
            finished_at_unix_ms: Some(published_at),
        });
        if ra_process_generation > 0 {
            outcome.producer.rust_analyzer_generation = Some(ra_process_generation);
        }
        let evidence_error = if project_check_evidence_error.is_some() {
            project_check_evidence_error
        } else if let Some(store) = self.evidence_store_v3.as_ref() {
            let class = if matches!(outcome.conclusion, Conclusion::Passed { .. }) {
                EvidenceClass::Success
            } else {
                EvidenceClass::Terminal
            };
            store
                .persist(&outcome, class, &evidence)
                .err()
                .map(|error| error.to_string())
        } else {
            Some("durable evidence store is not configured".to_string())
        };
        if let Some(error) = evidence_error {
            mark_evidence_unavailable_v3(&mut outcome, error.clone());
            let mut metrics = poisoned(&self.outcome_metrics_v3);
            metrics.evidence_persist_failures = metrics.evidence_persist_failures.saturating_add(1);
            drop(metrics);
            tracing::error!(
                attempt_id = %context.attempt_id,
                error = %error,
                "outcome-v3 durable evidence persistence failed"
            );
        }
        {
            let reaction_state = reaction_state_name(outcome.reaction.state);
            let mut metrics = poisoned(&self.outcome_metrics_v3);
            *metrics
                .terminal_by_code
                .entry(outcome.conclusion.semantic_code().to_string())
                .or_insert(0) += 1;
            *metrics
                .reactions_by_state
                .entry(reaction_state.to_string())
                .or_insert(0) += 1;
            if ra_storm.is_some() {
                metrics.ra_storm_outcomes = metrics.ra_storm_outcomes.saturating_add(1);
            }
            metrics.last_ra_error_lines = ra_error_lines;
            metrics.last_ra_suppressed_lines = ra_suppressed_lines;
        }
        self.remember_outcome_v3(outcome);
    }

    /// Resolve an accepted semantic attempt when a newer attempt for the
    /// same `(worktree, base_sha)` replaces its in-flight Hard witness.
    ///
    /// The generation latch prevents the stale witness from publishing a
    /// verdict, but that suppression must not leave its exact-attempt
    /// outcome in `Pending` forever. Record the replacement as a first-class
    /// terminal `Superseded` outcome without touching the worktree's
    /// last-writer-wins verdict slot.
    fn supersede_outcome_v3(
        &self,
        attempt_id: &AttemptId,
        successor_attempt_id: &AttemptId,
        worktree: &str,
    ) {
        let Some(mut outcome) = poisoned(&self.outcomes_v3).get(attempt_id).cloned() else {
            return;
        };
        if !matches!(outcome.conclusion, Conclusion::Pending { .. }) {
            return;
        }

        let terminal_at = now_unix_ms();
        if let Some(active) = outcome
            .timeline
            .iter_mut()
            .rev()
            .find(|record| record.finished_at_unix_ms.is_none())
        {
            active.finished_at_unix_ms = Some(terminal_at);
        }
        poisoned(&self.ra_evidence_v3).remove(attempt_id);

        let mut evidence = EvidenceBundle::default();
        evidence.push(
            ArtifactKind::Events,
            serde_json::to_vec(&serde_json::json!({
                "at_unix_ms": terminal_at,
                "event": "witness.superseded",
                "attempt_id": attempt_id.as_str(),
                "successor_attempt_id": successor_attempt_id.as_str(),
                "worktree": worktree,
            }))
            .expect("supersession event JSON"),
        );
        let reference_store = self
            .evidence_store_v3
            .clone()
            .unwrap_or_else(|| EvidenceStore::new("."));
        let Ok(evidence_ref) = reference_store.reference_for(attempt_id, &evidence) else {
            tracing::error!(
                attempt_id = %attempt_id,
                successor_attempt_id = %successor_attempt_id,
                "could not construct superseded outcome-v3 evidence reference"
            );
            return;
        };

        outcome.relations.push(Relation {
            kind: RelationKind::SupersededBy,
            attempt_id: Some(successor_attempt_id.clone()),
            execution_id: None,
        });
        outcome.conclude(Conclusion::Superseded {
            successor_attempt_id: successor_attempt_id.clone(),
            evidence: evidence_ref,
            summary: text_v3("a newer attempt for the same tree replaced this in-flight witness"),
        });
        outcome.timeline.push(PhaseRecord {
            phase: Phase::Terminal,
            started_at_unix_ms: terminal_at,
            finished_at_unix_ms: Some(terminal_at),
        });

        let evidence_error = if let Some(store) = self.evidence_store_v3.as_ref() {
            store
                .persist(&outcome, EvidenceClass::Terminal, &evidence)
                .err()
                .map(|error| error.to_string())
        } else {
            Some("durable evidence store is not configured".to_string())
        };
        if let Some(error) = evidence_error {
            mark_evidence_unavailable_v3(&mut outcome, error.clone());
            let mut metrics = poisoned(&self.outcome_metrics_v3);
            metrics.evidence_persist_failures = metrics.evidence_persist_failures.saturating_add(1);
            drop(metrics);
            tracing::error!(
                attempt_id = %attempt_id,
                error = %error,
                "superseded outcome-v3 durable evidence persistence failed"
            );
        }
        {
            let reaction_state = reaction_state_name(outcome.reaction.state);
            let mut metrics = poisoned(&self.outcome_metrics_v3);
            *metrics
                .terminal_by_code
                .entry(outcome.conclusion.semantic_code().to_string())
                .or_insert(0) += 1;
            *metrics
                .reactions_by_state
                .entry(reaction_state.to_string())
                .or_insert(0) += 1;
        }
        self.remember_outcome_v3(outcome);
    }

    /// CGLS-25 — acquire a Hard-witness compile slot from the global gate.
    /// Called at the top of the witness WORKER thread (never the serve loop
    /// or supervisor). The returned RAII guard releases the slot on drop —
    /// on normal return or panic — waking the next queued witness. With the
    /// gate off (default) this is an uncounted no-op grant, so the worker
    /// runs immediately exactly as today.
    pub(crate) fn acquire_witness_slot(&self) -> WitnessInflightGuard<'_> {
        self.witness_gate.acquire()
    }

    /// A6 — flip the RA-warm readiness latch. Called by servedrv once the
    /// daemon is first able to produce a meaningful verdict (the first
    /// cluster's RA handshake completed). One-way: never un-set; a
    /// respawning RA mid-flight is a liveness concern, not readiness.
    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Relaxed);
    }

    /// Unattributed convenience wrapper over [`Self::publish_attributed`]
    /// (`base_sha: None`) — the entry point for callers without a push to
    /// attribute (tests, embedded use). servedrv's one `publish_verdict`
    /// (the `ClusterAction::EmitVerdict` arm, Judgment B as composed) calls
    /// `publish_attributed` directly, right after the durable
    /// `statusfile::write`. Updates
    /// the in-memory status map AND fans out one [`TransitionEvent`]
    /// (subscribe-emit, plan 0b). One real verdict ⇒ one map update ⇒ one
    /// event; never a fabricated transition.
    ///
    /// **INFRA-36:** payload-shaped (was `authoritative_error: bool`).
    /// The SSE mirror now reflects the same honest verdict + diagnostic
    /// count + failure reason that `publish_verdict` writes to the
    /// statusfile — a remote `subscribe` client sees what a local
    /// `status` reader sees, instead of every error condition
    /// collapsing into `verdict=red, red_diagnostics=0`.
    // Non-test builds have no caller (servedrv's sole publish site calls
    // `publish_attributed`); the wrapper is kept as the unattributed
    // entry point for tests/embedded use, so allow it dead there.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn publish(&self, wt: &Path, payload: crate::statusfile::VerdictPayload) {
        self.publish_attributed(wt, payload, None, false);
    }

    /// [`Self::publish`] with verdict attribution (#A2): `base_sha` is the
    /// client-resolved commit from the overlay push this verdict answers,
    /// popped by servedrv's `publish_verdict` via
    /// [`Self::take_push_attribution`] at the sole attribution site.
    /// `None` ⇒ FS-watch / legacy verdict — the wire key stays absent.
    ///
    /// `ra_blind_paths` (#A8) travels the same pop: `true` iff the push's
    /// attribution classified its `changed_files` as macro-blind; `false`
    /// for FS-watch verdicts (no push ⇒ no blind evidence) and the key
    /// stays absent on the wire.
    pub fn publish_attributed(
        &self,
        wt: &Path,
        payload: crate::statusfile::VerdictPayload,
        base_sha: Option<String>,
        ra_blind_paths: bool,
    ) {
        self.publish_attributed_with_checks(
            wt,
            payload,
            base_sha,
            ra_blind_paths,
            Vec::new(),
            None,
        );
    }

    /// [`Self::publish_attributed`] carrying the ids of the project checks
    /// that actually RAN for this verdict (`gated_checks_ran`). Additive-
    /// default variant (mirrors `push_overlay` → `push_overlay_with_profile`)
    /// so the unattributed wrapper and every test caller stay on the 4-arg
    /// form; only servedrv's Hard-witness publish path — the one with an
    /// enumerated `ProjectCheckReport` — threads the ran ids through here.
    ///
    /// The witness asserts a specific check id (e.g. `wasm-compiler-witness`)
    /// is present before accepting an attributed green; an empty list ⇒ the
    /// witness falls back to plain base_sha attribution (transition-safe).
    /// The coalesced/batched path and the RA-native path pass an empty Vec
    /// (they "cannot enumerate"), and the wire key stays absent.
    pub fn publish_attributed_with_checks(
        &self,
        wt: &Path,
        payload: crate::statusfile::VerdictPayload,
        base_sha: Option<String>,
        ra_blind_paths: bool,
        gated_checks_ran: Vec<String>,
        semantic: Option<AttemptContext>,
    ) {
        self.publish_attributed_with_candidate_checks(
            wt,
            payload,
            base_sha,
            None,
            ra_blind_paths,
            gated_checks_ran,
            Vec::new(),
            semantic,
        );
    }

    /// Candidate-addressable form of [`Self::publish_attributed_with_checks`].
    /// Legacy callers pass no candidate and retain base-SHA addressing.
    pub(crate) fn publish_attributed_with_candidate_checks(
        &self,
        wt: &Path,
        payload: crate::statusfile::VerdictPayload,
        base_sha: Option<String>,
        candidate: Option<CandidateVerdictIdentity>,
        ra_blind_paths: bool,
        gated_checks_ran: Vec<String>,
        verified_project_checks: Vec<VerifiedProjectCheckEvidence>,
        semantic: Option<AttemptContext>,
    ) {
        let worktree = wt.to_string_lossy().into_owned();
        let verdict_color = payload.verdict.as_str().to_string();
        let red_diagnostics = payload.red_diagnostics;
        let failure_reason = payload.analysis_failure_reason.clone();
        let published_at = crate::statusfile::now_unix();
        let status = WorktreeStatus {
            worktree: worktree.clone(),
            verdict: verdict_color.clone(),
            daemon_build_id: cargoless_core::build_id().to_string(),
            // Per-crate roll-up is still empty here (the publish path
            // doesn't have the cratemap context — that lives in
            // `build.rs::write_status`); the load-bearing change is
            // that `red_diagnostics` and `verdict_failure_reason` are
            // now honest scalars from the payload, NOT hardcoded zeros.
            crates: Vec::new(),
            red_diagnostics,
            verdict_failure_reason: failure_reason.clone(),
            base_sha: base_sha.clone(),
            candidate_manifest_digest: candidate
                .as_ref()
                .map(|identity| identity.manifest_digest.clone()),
            candidate_snapshot_digest: candidate
                .as_ref()
                .map(|identity| identity.snapshot_digest.clone()),
            candidate_tree_oid: candidate.as_ref().map(|identity| identity.tree_oid.clone()),
            ra_blind_paths,
            // The witness's positive "the gated check ran" proof. Empty for
            // FS-watch / coalesced / RA-native verdicts (no enumerated
            // report) ⇒ absent on the wire (additive, same as base_sha).
            gated_checks_ran: gated_checks_ran.clone(),
            // Freshly published ⇒ age computed at read time (get_status)
            // from `published_at` so a remote reader sees an honest age.
            heartbeat_age_secs: 0,
            published_at,
        };
        // #A2-keystone — retain every ATTRIBUTED verdict in the
        // base_sha-addressable ring. The witness shares one worktree key
        // across all PRs, so the single `statuses` slot only ever holds the
        // last publisher's verdict; a poller for a commit that has since been
        // superseded in the slot can still retrieve its own verdict here.
        // Unattributed (FS-watch) verdicts carry no SHA to address by and
        // never enter the ring.
        if candidate.is_some() || base_sha.as_deref().is_some_and(|sha| !sha.is_empty()) {
            let mut hist = poisoned(&self.verdict_history);
            let ring = hist.entry(worktree.clone()).or_default();
            if let Some(identity) = candidate.as_ref() {
                // Manifest digests are immutable result addresses. A retry
                // may execute independently but its latest published result
                // remains addressable at the same content identity.
                ring.retain(|status| {
                    status.candidate_manifest_digest.as_deref()
                        != Some(identity.manifest_digest.as_str())
                });
            } else if let Some(sha) = base_sha.as_deref().filter(|s| !s.is_empty()) {
                ring.retain(|status| status.base_sha.as_deref() != Some(sha));
            }
            ring.push_back(status.clone());
            while ring.len() > self.witness_history_cap {
                ring.pop_front();
            }
            let retained_shas: BTreeSet<String> = ring
                .iter()
                .filter_map(|entry| entry.base_sha.clone())
                .collect();
            let retained_manifests: BTreeSet<String> = ring
                .iter()
                .filter_map(|entry| entry.candidate_manifest_digest.clone())
                .collect();
            drop(hist);
            poisoned(&self.diagnostics).retain(|(key_wt, key_sha), _| {
                key_wt != &worktree
                    || key_sha.is_none()
                    || key_sha
                        .as_ref()
                        .is_some_and(|candidate| retained_shas.contains(candidate))
            });
            poisoned(&self.candidate_diagnostics).retain(|(key_wt, manifest_digest), _| {
                key_wt != &worktree || retained_manifests.contains(manifest_digest)
            });
        }
        // The slot is last-writer-wins, but never regresses to a STRICTLY
        // staler timestamp — two Hard-witness supervisor threads can publish
        // out of order, and a plain `get_status` reader (e.g. `cargoless
        // status`) should see the freshest verdict, not whichever thread won
        // the lock race. `>=` (not `>`) preserves the clear-on-unattributed
        // contract: an FS-watch `None` publish in the same wall-clock second
        // as the prior attributed publish must still clear the stale SHA.
        {
            let mut slot = poisoned(&self.statuses);
            let fresher = slot
                .get(&worktree)
                .is_none_or(|prev| published_at >= prev.published_at);
            if fresher {
                slot.insert(worktree.clone(), status);
            }
        }
        let ev = TransitionEvent {
            worktree: worktree.clone(),
            verdict: verdict_color,
            red_diagnostics,
            verdict_failure_reason: failure_reason,
            base_sha,
            candidate_manifest_digest: candidate
                .as_ref()
                .map(|identity| identity.manifest_digest.clone()),
            candidate_snapshot_digest: candidate
                .as_ref()
                .map(|identity| identity.snapshot_digest.clone()),
            candidate_tree_oid: candidate.map(|identity| identity.tree_oid),
            ra_blind_paths,
            published_at,
        };
        poisoned(&self.subs).retain(|s| s.send(ev.clone()).is_ok());
        self.mark_worktree_published(&worktree);
        if let Some(context) = semantic.as_ref() {
            self.finish_outcome_v3(
                context,
                &payload,
                &gated_checks_ran,
                &verified_project_checks,
                &worktree,
            );
        }
    }

    /// #240/2b — wire the push-arrival signal channel. Called ONCE by
    /// the serve loop at startup, BEFORE `HttpServer::bind` exposes the
    /// `push_overlay` ingest route. After this, every `push_overlay`
    /// call sends the WT key on `tx`; the serve loop's drain wakes up
    /// and synthesizes a `DriverEvent::RoutedBatch` for that WT.
    ///
    /// **Best-effort by construction:** a wedged `tx` (closed receiver)
    /// produces a silent send-error; the push is still STORED in
    /// `pushed`, only the wakeup is lost. The next push or activity
    /// tick will eventually surface the stored overlay — the
    /// fail-soft transport ethos applied to the write-plane wakeup.
    pub fn attach_push_signal(&self, tx: Sender<String>) {
        *poisoned(&self.push_signal) = Some(tx);
    }

    /// Wire the direct hard-witness dispatcher before exposing HTTP ingest.
    pub(crate) fn attach_direct_gate_signal(&self, tx: Sender<DirectGateRequest>) {
        *poisoned(&self.direct_gate_signal) = Some(tx);
    }

    /// C1 observability — record the resolved RA config JSON
    /// (`InitOpts::resolved_summary()`) for `GET /daemon`. Called by the
    /// serve loop at startup, same attach-at-startup pattern as
    /// `attach_push_signal`. Idempotent; last writer wins (the config is
    /// env-derived and identical across clusters in a single daemon).
    pub fn set_resolved_config(&self, config: serde_json::Value) {
        *poisoned(&self.resolved_config) = Some(config);
    }

    /// #240/2b — consume-semantic reader for the SwitchOverlay arm.
    /// Returns the pushed overlay for `wt_key` (matching
    /// `wt.to_string_lossy()` from servedrv) AND removes it from the
    /// store. If no push is pending, returns `None` and the SwitchOverlay
    /// arm falls through to the FS-read path. The pop-on-consume
    /// semantic (spike open-question #3 default) means each push
    /// services exactly one SwitchOverlay cycle; FS path resumes if no
    /// fresh push arrives.
    pub fn take_overlay_for(&self, wt_key: &str) -> Option<PushedOverlay> {
        let popped = {
            let mut store = poisoned(&self.pushed);
            let queue = store.get_mut(wt_key)?;
            let front = queue.pop_front();
            // Drop the now-empty queue so `is_empty`/`len`/peek see no
            // phantom key, and so the FS-fallback discriminant
            // (take → None) holds once drained.
            let still_pending = if queue.is_empty() {
                store.remove(wt_key);
                false
            } else {
                true
            };
            (front, still_pending)
        };
        // CGLS-25 — the serve loop's `drain_unique_push_keys` dedups wake
        // signals per WT key, so a single wake services exactly one
        // SwitchOverlay cycle (one `pop_front`). If more pushes are queued
        // for this WT, re-signal so the next loop iteration routes the next
        // one; without this the tail of a same-WT burst would starve until
        // an unrelated push happened to wake the loop.
        if popped.1 {
            if let Some(tx) = poisoned(&self.push_signal).as_ref() {
                let _ = tx.send(wt_key.to_string());
            }
        }
        popped.0
    }

    /// Terminalize the next accepted overlay when its rust-analyzer cluster
    /// cannot be created. Accepted exact attempts must never remain queued
    /// after the adapter has already abandoned their execution.
    pub(crate) fn fail_next_pushed_overlay(&self, wt_key: &str, reason: &str) -> bool {
        let Some(pushed) = self.take_overlay_for(wt_key) else {
            return false;
        };
        let macro_blind_hit = compute_macro_blind_hit(
            pushed.changed_files.as_deref(),
            &macro_blind_globs(),
            &pushed.files,
            &macro_blind_macros(),
        );
        self.publish_attributed_with_candidate_checks(
            Path::new(wt_key),
            crate::statusfile::VerdictPayload::unknown(reason),
            pushed.base_sha,
            pushed
                .candidate_snapshot
                .as_ref()
                .map(CandidateVerdictIdentity::from_manifest),
            macro_blind_hit,
            Vec::new(),
            Vec::new(),
            pushed.semantic,
        );
        true
    }

    /// #240/2b — non-consuming peek. Used by the serve loop's first-push
    /// cluster-hash derivation (`cluster_hash_from_pushed`) which needs
    /// to read the pushed workspace-config files WITHOUT consuming the
    /// overlay (the consume happens later in the SwitchOverlay arm via
    /// `take_overlay_for`). Returns a clone; the store is unchanged.
    pub fn peek_overlay_for(&self, wt_key: &str) -> Option<PushedOverlay> {
        // Front of the queue = the next overlay `take_overlay_for` will
        // consume. Cluster-hash derivation reads workspace-config files,
        // which are stable across a worktree's pushes, so peeking the front
        // (vs any other queued push) is correct.
        poisoned(&self.pushed)
            .get(wt_key)
            .and_then(|q| q.front().cloned())
    }

    /// Server-side analysis root for a pending pushed overlay, if the client
    /// supplied one. The serve loop uses this before consuming the overlay so
    /// first-push cluster spawn uses the daemon's mirror path, not the
    /// client's pod-local worktree key.
    pub fn analysis_root_for(&self, wt_key: &str) -> Option<PathBuf> {
        // Front of the queue = the next overlay to be consumed; its
        // analysis_root drives the cluster-spawn mirror path.
        poisoned(&self.pushed)
            .get(wt_key)
            .and_then(|q| q.front())
            .and_then(|p| p.analysis_root.clone())
    }

    /// Struct-param form (was six positional params): adding `gate` made
    /// the positional list 8 args counting `&self`, which trips
    /// `clippy::too_many_arguments`; the literal at the sole call site is
    /// also simply more readable.
    pub(crate) fn record_project_check_context(&self, worktree: &str, ctx: ProjectCheckRunContext) {
        poisoned(&self.project_check_context).insert(worktree.to_string(), ctx);
    }

    pub(crate) fn take_project_check_context(
        &self,
        worktree: &str,
    ) -> Option<ProjectCheckRunContext> {
        poisoned(&self.project_check_context).remove(worktree)
    }

    pub(crate) fn retain_diagnostics(
        &self,
        worktree: &str,
        base_sha: Option<&str>,
        diagnostics: Vec<Diagnostic>,
    ) {
        let mut retained = poisoned(&self.diagnostics);
        let key = (
            worktree.to_string(),
            base_sha.filter(|sha| !sha.is_empty()).map(str::to_string),
        );
        if diagnostics.is_empty() {
            retained.remove(&key);
        } else {
            retained.insert(key, diagnostics);
        }
    }

    pub(crate) fn retain_candidate_diagnostics(
        &self,
        worktree: &str,
        candidate: &CandidateVerdictIdentity,
        diagnostics: Vec<Diagnostic>,
    ) {
        let mut retained = poisoned(&self.candidate_diagnostics);
        let key = (worktree.to_string(), candidate.manifest_digest.clone());
        if diagnostics.is_empty() {
            retained.remove(&key);
        } else {
            retained.insert(key, diagnostics);
        }
    }

    /// #A2/#A7 — stamp the attribution for the push just consumed by the
    /// SwitchOverlay arm. Same lifecycle as `record_project_check_context`:
    /// recorded at consume, popped at publish; a replacing push for the
    /// same key overwrites (the verdict that eventually publishes belongs
    /// to the LAST consumed push, so its attribution must win too).
    ///
    /// #A8 — also classifies the push's `changed_files` against the
    /// operator's macro-blind globs at this same consume instant, so the
    /// blind bit and the `base_sha` travel as one record and can never be
    /// stamped onto a different push's verdict.
    pub(crate) fn record_push_attribution(&self, worktree: &str, pushed: &PushedOverlay) {
        self.record_push_attribution_with_globs(
            worktree,
            pushed,
            &macro_blind_globs(),
            &macro_blind_macros(),
        );
    }

    /// Env-free body of [`Self::record_push_attribution`] (the
    /// `_with_timeout` injection discipline): tests pass globs and macro
    /// names explicitly instead of mutating process env under parallel
    /// test threads.
    pub(crate) fn record_push_attribution_with_globs(
        &self,
        worktree: &str,
        pushed: &PushedOverlay,
        blind_globs: &[String],
        macro_names: &[String],
    ) {
        poisoned(&self.push_attribution).insert(
            worktree.to_string(),
            PushAttribution {
                base_sha: pushed.base_sha.clone(),
                candidate: pushed
                    .candidate_snapshot
                    .as_ref()
                    .map(CandidateVerdictIdentity::from_manifest),
                macro_blind_hit: compute_macro_blind_hit(
                    pushed.changed_files.as_deref(),
                    blind_globs,
                    &pushed.files,
                    macro_names,
                ),
                push_received_unix: pushed.last_push_unix,
                consumed_unix: crate::statusfile::now_unix(),
                consumed_at: Instant::now(),
                semantic: pushed.semantic.clone(),
            },
        );
    }

    pub(crate) fn take_push_attribution(&self, worktree: &str) -> Option<PushAttribution> {
        poisoned(&self.push_attribution).remove(worktree)
    }

    /// CGLS-27 — drain the attributions of pushes STRANDED by a
    /// rust-analyzer respawn, restricted to `worktrees` (the caller's
    /// respawned cluster).
    ///
    /// ## Why a lingering entry *is* the stranded set
    ///
    /// An attribution is recorded at EXACTLY ONE site (the SwitchOverlay
    /// exec arm, at overlay-consume) and removed at EXACTLY ONE site
    /// ([`Self::take_push_attribution`], at EmitVerdict dispatch). So an
    /// entry that is still present means: this push's overlay was
    /// consumed, but its verdict never dispatched. When RA dies mid-check,
    /// `ClusterDriver::reset_after_respawn` drops the in-flight txn to keep
    /// the #247 no-false-GREEN invariant — correct, but it leaves that
    /// consumed push with nothing to re-drive it. The entry is the only
    /// remaining evidence the push ever existed.
    ///
    /// A worktree sitting in the driver's deliberately-retained `pending`
    /// queue has NO attribution (it never reached SwitchOverlay), so this
    /// drain cannot steal a push that is still legitimately queued. And
    /// since a driver holds at most ONE in-flight txn, this returns 0 or 1
    /// entry per respawned cluster in practice — not a crowd.
    ///
    /// ## Why `worktrees`-scoped and not a global drain
    ///
    /// `reset_after_respawn` strands only the respawned cluster's own
    /// in-flight txn; other clusters' drivers are untouched and their
    /// in-flight pushes are still going to publish normally. A global
    /// drain would publish a spurious `unknown` for those — resolving a
    /// healthy push's CI early at exit 75 when it was about to go green.
    /// The caller passes the worktree keys belonging to the respawned
    /// cluster only.
    ///
    /// ## Drain, not peek
    ///
    /// Entries are REMOVED. A second respawn (the reap loop this exists
    /// for is sustained, not one-shot) therefore cannot re-publish a push
    /// that was already resolved by the first — publish-once survives a
    /// tight kill/respawn cycle by construction rather than by timing.
    pub(crate) fn drain_push_attributions_for(
        &self,
        worktrees: &BTreeSet<String>,
    ) -> Vec<(String, PushAttribution)> {
        let mut map = poisoned(&self.push_attribution);
        let keys: Vec<String> = map
            .keys()
            .filter(|k| worktrees.contains(*k))
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|k| map.remove(&k).map(|a| (k, a)))
            .collect()
    }

    /// #A4.3 — claim the hard-witness slot for `(wt_key, base_sha)`. Returns
    /// the new generation; a previously claimed (still-running) witness for
    /// the same key is implicitly invalidated (its `finish_hard_witness` will
    /// return `false`). Generations come from a global never-recycled
    /// sequence, so an ABA match is structurally impossible.
    ///
    /// `base_sha` is part of the key (the `<absent>` fix): two distinct
    /// commits sharing one worktree key each get an independent latch, so a
    /// newer commit's push no longer supersedes an older commit's in-flight
    /// witness. `None` (FS-watch / unattributed) keeps the historical
    /// one-latch-per-worktree behavior.
    pub(crate) fn begin_hard_witness(
        &self,
        wt_key: &str,
        base_sha: Option<&str>,
        semantic: Option<&AttemptContext>,
    ) -> u64 {
        let generation = HARD_WITNESS_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        let key = (wt_key.to_string(), base_sha.map(str::to_string));
        let attempt_id = semantic.map(|context| context.attempt_id.clone());
        let previous = poisoned(&self.hard_witness_generation).insert(
            key,
            HardWitnessClaim {
                generation,
                attempt_id: attempt_id.clone(),
            },
        );
        if let (Some(previous_attempt_id), Some(successor_attempt_id)) =
            (previous.and_then(|claim| claim.attempt_id), attempt_id)
        {
            if previous_attempt_id != successor_attempt_id {
                self.supersede_outcome_v3(&previous_attempt_id, &successor_attempt_id, wt_key);
            }
        }
        generation
    }

    /// `true` iff `generation` is still the latest claim for
    /// `(wt_key, base_sha)` — the caller may publish. Consumes the claim on
    /// success so a duplicate finish (watchdog already published, worker
    /// completes later) reports `false` and stays silent.
    pub(crate) fn finish_hard_witness(
        &self,
        wt_key: &str,
        base_sha: Option<&str>,
        generation: u64,
    ) -> bool {
        let key = (wt_key.to_string(), base_sha.map(str::to_string));
        let mut map = poisoned(&self.hard_witness_generation);
        if map
            .get(&key)
            .is_some_and(|claim| claim.generation == generation)
        {
            map.remove(&key);
            true
        } else {
            false
        }
    }

    pub(crate) fn with_project_check_overlay<T>(
        &self,
        context: &ProjectCheckRunContext,
        f: impl FnOnce(&Path, Option<&Path>, Option<&CandidateManifestSidecar>) -> T,
    ) -> Result<T, String> {
        #[cfg(not(target_os = "linux"))]
        if context.candidate_snapshot.is_some() {
            return Err(candidate_environment_unsafe(
                "typed candidate execution requires Linux sealed child authority",
            ));
        }
        if !context.materialize_overlay {
            if context.candidate_snapshot.is_some() {
                return Err(
                    "candidate_snapshot.materialization_required: typed candidates must run in an isolated scratch"
                        .to_string(),
                );
            }
            // No overlay to materialize ⇒ no scratch ⇒ no warm dir; run in
            // place with the historical cold per-run target.
            return Ok(f(&context.root, None, None));
        }

        if let Some(state_dir) = self.project_check_state_dir.as_deref() {
            return self.with_project_check_scratch_overlay(context, state_dir, f);
        }
        if context.candidate_snapshot.is_some() {
            return Err(
                "candidate_snapshot.environment_unsafe: typed candidates require a configured external daemon state directory"
                    .to_string(),
            );
        }
        if context.source_sha.is_some() {
            let fallback_state = context.root.join(".cargoless");
            return self.with_project_check_scratch_overlay(context, &fallback_state, f);
        }

        self.with_project_check_locked_overlay(context, f)
    }

    fn with_project_check_locked_overlay<T>(
        &self,
        context: &ProjectCheckRunContext,
        f: impl FnOnce(&Path, Option<&Path>, Option<&CandidateManifestSidecar>) -> T,
    ) -> Result<T, String> {
        let authority = ProjectCheckAuthority::from_context(context);
        let _guard = poisoned(&self.sync_lock);
        reset_analysis_root(&context.root, &context.base_ref)?;
        materialize_overlay_files(&context.root, &context.overlay_files)?;
        // The local (no-state-dir) path always runs cold: warm caching is a
        // central-daemon-only optimization.
        let result = Ok(f(&context.root, None, None));
        let cleanup = reset_analysis_root(&context.root, &context.base_ref);
        finish_project_check_run(authority, result, cleanup, Ok(()))
    }

    fn with_project_check_scratch_overlay<T>(
        &self,
        context: &ProjectCheckRunContext,
        state_dir: &Path,
        f: impl FnOnce(&Path, Option<&Path>, Option<&CandidateManifestSidecar>) -> T,
    ) -> Result<T, String> {
        let authority = ProjectCheckAuthority::from_context(context);
        let protected_state = ProtectedStateRoot::open(
            &context.root,
            state_dir,
            authority == ProjectCheckAuthority::CandidateSnapshot,
        )?;
        let scratch_namespace = if authority == ProjectCheckAuthority::CandidateSnapshot {
            protected_state.namespace("candidate-project-check-runs", true)?
        } else {
            protected_state.legacy_scratch_namespace(true)?
        }
        .expect("created namespace is present");
        let candidate_namespace = if authority == ProjectCheckAuthority::CandidateSnapshot {
            protected_state.namespace("candidate-snapshots", true)?
        } else {
            None
        };
        let run_name = unpredictable_project_check_run_name()?;
        let scratch_run_dir = scratch_namespace.join(&run_name);
        match create_candidate_private_directory(&scratch_run_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(candidate_environment_unsafe(format!(
                    "unpredictable project-check run path `{}` already exists",
                    scratch_run_dir.display()
                )));
            }
            Err(error) => {
                return Err(candidate_environment_unsafe(format!(
                    "could not create protected project-check run `{}`: {error}",
                    scratch_run_dir.display()
                )));
            }
        }
        let scratch_run =
            match ProtectedRunDirectory::capture(scratch_run_dir.clone(), &scratch_namespace) {
                Ok(run) => run,
                Err(error) => {
                    return Err(candidate_environment_unsafe(format!(
                        "could not bind scratch run identity; preserving `{}`: {error}",
                        scratch_run_dir.display()
                    )));
                }
            };
        let scratch_root = scratch_run.path.join("worktree");
        let candidate_run_dir = candidate_namespace
            .as_ref()
            .map(|namespace| namespace.join(&run_name));

        let mut cleanup = ProjectCheckRunCleanup {
            api: self,
            root: &context.root,
            authority,
            scratch_run,
            candidate_run: None,
            cleaned: false,
        };

        let prepare = {
            let _guard = poisoned(&self.sync_lock);
            let checkout_ref = if let Some(source_sha) = context.source_sha.as_deref() {
                if !local_commit_exists(&context.root, source_sha) {
                    Err(format!(
                        "verified source commit {source_sha} is no longer present locally"
                    ))
                } else {
                    Ok(source_sha.to_string())
                }
            } else if let Some(manifest) = context.candidate_snapshot.as_ref() {
                manifest.candidate.base().ok_or_else(|| {
                    "candidate_snapshot.kind_unsupported: project-check overlay requires an overlay base"
                        .to_string()
                }).and_then(|base| {
                    if local_commit_exists(&context.root, &base.commit_sha) {
                        Ok(base.commit_sha.clone())
                    } else {
                        Err(format!(
                            "candidate_snapshot.base_commit_missing: verified base commit {} is no longer present locally",
                            base.commit_sha
                        ))
                    }
                })
            } else {
                sync_analysis_root(&context.root, &context.base_ref)
                    .map(|()| context.base_ref.clone())
            };
            checkout_ref.and_then(|checkout_ref| {
                prepare_new_protected_project_check_scratch(
                    &context.root,
                    &scratch_root,
                    &checkout_ref,
                )
            })
        };
        if let Err(error) = prepare {
            let (scratch_cleanup, manifest_cleanup) = cleanup.cleanup();
            return finish_project_check_run(
                authority,
                Err(error),
                scratch_cleanup,
                manifest_cleanup,
            );
        }
        if let Err(error) = set_private_directory_mode(&scratch_root) {
            let (scratch_cleanup, manifest_cleanup) = cleanup.cleanup();
            return finish_project_check_run(
                authority,
                Err(error),
                scratch_cleanup,
                manifest_cleanup,
            );
        }

        let candidate_manifest = if let Some(manifest) = context.candidate_snapshot.as_ref() {
            let candidate_run_dir = candidate_run_dir
                .as_deref()
                .expect("candidate run directory paired with manifest");
            let setup = materialize_candidate_snapshot(&scratch_root, manifest)
                .and_then(|()| create_candidate_manifest_run(candidate_run_dir));
            if let Err(error) = setup {
                let (scratch_cleanup, manifest_cleanup) = cleanup.cleanup();
                return finish_project_check_run(
                    authority,
                    Err(error),
                    scratch_cleanup,
                    manifest_cleanup,
                );
            }
            let candidate_run = match ProtectedRunDirectory::capture(
                candidate_run_dir.to_path_buf(),
                candidate_namespace
                    .as_deref()
                    .expect("candidate run has a protected namespace"),
            ) {
                Ok(run) => run,
                Err(error) => {
                    let (scratch_cleanup, _) = cleanup.cleanup();
                    let preserved = Err(candidate_environment_unsafe(format!(
                        "could not bind candidate run identity; preserving `{}`: {error}",
                        candidate_run_dir.display()
                    )));
                    return finish_project_check_run(
                        authority,
                        Err(error),
                        scratch_cleanup,
                        preserved,
                    );
                }
            };
            cleanup.candidate_run = Some(candidate_run);
            match write_candidate_manifest(candidate_run_dir, manifest) {
                Ok(path) => Some(path),
                Err(error) => {
                    let (scratch_cleanup, manifest_cleanup) = cleanup.cleanup();
                    return finish_project_check_run(
                        authority,
                        Err(error),
                        scratch_cleanup,
                        manifest_cleanup,
                    );
                }
            }
        } else {
            None
        };

        // CGLS-26 — resolve a WARM shared target dir (or None = cold per-run).
        // The returned guard holds the in-process + flock locks for the whole
        // compile and fails closed to cold on ANY doubt. `warm` is the dir to
        // hand the compile; the guard's lifetime keeps the locks held.
        let warm = self.resolve_warm_target(&protected_state.canonical, &scratch_root);
        let warm_dir = warm.as_ref().map(|w| w.dir.as_path());

        let result = if context.source_sha.is_some() || context.candidate_snapshot.is_some() {
            Ok(f(&scratch_root, warm_dir, candidate_manifest.as_ref()))
        } else {
            match materialize_overlay_files_from_root(
                &context.root,
                &scratch_root,
                &context.overlay_files,
            ) {
                Ok(()) => Ok(f(&scratch_root, warm_dir, None)),
                Err(e) => Err(e),
            }
        };
        // Warm-lock guard drops here (after the compile), releasing both
        // layers. Explicit for clarity — the locks must outlive `f`.
        drop(warm);

        let (scratch_cleanup, manifest_cleanup) = cleanup.cleanup();
        finish_project_check_run(authority, result, scratch_cleanup, manifest_cleanup)
    }

    /// CGLS-26 — resolve a WARM, persistent, shared `CARGO_TARGET_DIR` for
    /// this witness compile, or `None` to run cold in the per-run scratch
    /// (today's behavior). Fails CLOSED to cold on ANY doubt: flag off, key
    /// unresolvable, in-process lock contended, cross-process flock
    /// contended, or dir/lock create error. The returned guard holds both
    /// lock layers for the caller-scoped compile; dropping it releases them.
    ///
    /// Warmth is safe only because witness compiles are serialized (CGLS-25):
    /// a shared target dir can be corrupted by two concurrent `cargo`s
    /// (CGLS-24), so the locks are a hard interlock — if anything else is in
    /// the warm dir, this run goes cold rather than share it.
    ///
    /// ## There are FOUR interlocks, not two — know all of them before tuning
    ///
    /// The two locks below (in-process CAS, cross-process flock) are the ones
    /// this function owns, and they guard the dir at RUN granularity. They are
    /// not the whole story:
    ///
    /// | # | Interlock | Granularity | Owner |
    /// |---|---|---|---|
    /// | 1 | key = sha256(schema, toolchain, `Cargo.lock`) | dep-graph | this fn |
    /// | 2 | per-key in-process `AtomicBool` CAS, non-blocking | run | this fn |
    /// | 3 | `flock(2)` `LOCK_NB` on `<warm>/.witness-lock` | run/process | this fn |
    /// | 4 | cargo's `<target>/<layout>/.cargo-lock`, **blocking** | check | cargo |
    ///
    /// **#4 is the one carrying intra-run load, and nothing here enforces it.**
    /// `MAX_INFLIGHT` and the two locks above gate witness *runs*; within one
    /// run `project_checks::run_parallel` fans out to the profile's
    /// `max_parallel`, and every `command` check inherits the SAME
    /// `CARGO_TARGET_DIR`. Observed live 2026-07-30 with three witnesses on one
    /// warm dir — one builder, two parked in `locks_lock_inode_wait`:
    ///
    /// ```text
    /// FLOCK ADVISORY WRITE 91447 …2767534        <- wasm holds release/
    ///    -> FLOCK ADVISORY WRITE 91467 …2767534  <- csr blocked
    ///    -> FLOCK ADVISORY WRITE 91489 …2767534  <- ssr blocked
    /// ```
    ///
    /// Two consequences worth writing down:
    ///
    /// - Interlocks #2/#3 are *quiet* about this. They are per-run and
    ///   uncontended in the fast path, so a clean `mode=warm reason=hit` log
    ///   says nothing about how many `cargo`s are queued inside that run.
    /// - The intra-run sharing PREDATES the warm dir. Cold pins
    ///   `<root>/.cargoless-target` — per-*run*, not per-*check* — so those
    ///   same checks always shared a dir. Warm did not create it; warm
    ///   extended it across runs. Reverting warm would not remove it.
    ///
    /// `project_checks::run_parallel` now holds shared-warm-dir checks in a
    /// serial class so they queue in `pending` instead of in the kernel. That
    /// is a resource fix layered ON TOP of #4, **not a replacement for it** —
    /// #4 is still the correctness backstop. Anything that raises effective
    /// concurrency against one target dir (per-check target split, a higher
    /// `max_parallel`, a second daemon on the same PVC) is removing a CGLS-24
    /// guard, so pair it with evidence that #4 still covers the gap.
    /// Emit the warm-target obs line AND bump the matching counter.
    ///
    /// One call site for both so the two can never disagree — the failure
    /// this repo keeps hitting is an optimisation whose telemetry silently
    /// stops matching its behaviour. Every `resolve_warm_target` exit goes
    /// through here.
    fn record_warm_obs(&self, warm_dir: &Path, mode: &str, reason: &str) {
        eprintln!(
            "[cargoless:obs] witness-warm-target dir={} mode={mode} reason={reason}",
            warm_dir.display()
        );
        *poisoned(&self.warm_target_stats)
            .entry(format!("{mode}:{}", warm_obs_bucket(reason)))
            .or_insert(0) += 1;
    }

    /// Snapshot of [`Self::warm_target_stats`] for `GET /daemon`. `None` when
    /// nothing has resolved yet, so a daemon that runs no witness compiles
    /// omits the field entirely rather than publishing a misleading zero.
    pub(crate) fn warm_target_stats_json(&self) -> Option<serde_json::Value> {
        let stats = poisoned(&self.warm_target_stats);
        if stats.is_empty() {
            return None;
        }
        let mut warm: u64 = 0;
        let mut cold: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        for (k, v) in stats.iter() {
            match k.split_once(':') {
                Some(("warm", _)) => warm += v,
                Some(("cold-fallback", reason)) => {
                    cold.insert(reason.to_string(), serde_json::json!(v));
                }
                _ => {}
            }
        }
        Some(serde_json::json!({ "warm": warm, "cold_fallback": cold }))
    }

    fn resolve_warm_target(
        &self,
        state_dir: &Path,
        scratch_root: &Path,
    ) -> Option<WarmTargetGuard> {
        // 1. Feature gate — default OFF ⇒ compute nothing, take no lock.
        if !warm_target_enabled() {
            return None;
        }
        // 2. Key on (schema, toolchain, Cargo.lock). Base_sha is deliberately
        //    NOT in the key — cargo's own fingerprinting handles the file diff
        //    between bases; keying per (toolchain, lock) only gives an
        //    incompatible toolchain/dep-graph a fresh cold subdir. Any input
        //    unresolvable ⇒ cold.
        let key = warm_target_key(scratch_root)?;
        let warm_dir = state_dir.join("witness-target-warm").join(&key);
        if let Err(e) = std::fs::create_dir_all(&warm_dir) {
            self.record_warm_obs(&warm_dir, "cold-fallback", &format!("mkdir:{e}"));
            return None;
        }

        // 3a. In-process per-key busy flag (primary). Non-blocking CAS —
        //     never wait: a wedged prior witness must not wedge this one;
        //     already-busy ⇒ cold. The guard stores `false` on drop.
        let busy = {
            let mut map = poisoned(&self.warm_target_locks);
            Arc::clone(map.entry(key.clone()).or_default())
        };
        if busy
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            self.record_warm_obs(&warm_dir, "cold-fallback", "contended:in-proc");
            return None;
        }
        let in_proc = InProcWarmGuard {
            busy,
            released: false,
        };

        // 3b. Cross-process advisory flock (insurance for a future
        //     multi-daemon topology; today serve is single-replica). LOCK_NB
        //     ⇒ contended ⇒ cold.
        let lock_path = warm_dir.join(".witness-lock");
        let flock = match WarmFlock::acquire_nb(&lock_path) {
            Ok(Some(fl)) => fl,
            Ok(None) => {
                self.record_warm_obs(&warm_dir, "cold-fallback", "contended:flock");
                return None;
            }
            Err(e) => {
                self.record_warm_obs(&warm_dir, "cold-fallback", &format!("flock-open:{e}"));
                return None;
            }
        };

        // 4. Stamp LRU recency, then GC older keyed dirs (best-effort; never
        //    blocks the compile). The stamp makes THIS dir newest so prune's
        //    ordering can't select it; the explicit `active` skip protects it
        //    even if the stamp write fails (disk-full) — pruning the dir we
        //    hold locks on mid-compile would be CGLS-24 by another road.
        let _ = std::fs::write(warm_dir.join(".last-used"), "");
        prune_warm_target_dirs(state_dir, &warm_dir);

        // 5. Disk-pressure rung, checked AFTER prune so a reclaimable stale
        //    key counts as free space rather than tripping the guard.
        //
        //    Why this rung exists: prune keeps WARM_TARGET_KEEP=2 and runs
        //    after create, so a `Cargo.lock` change transiently holds THREE
        //    keyed dirs. Measured 2026-07-30 on witness-b, whose warm keys are
        //    ~16G and ~6G on a 59G volume — 3 keys would not fit. And running
        //    cargo out of space is the bad failure: it surfaces as a compile
        //    error on a tree that compiles fine, i.e. a false RED on a
        //    required check, which is exactly the class the per-run dirs were
        //    introduced to stop.
        //
        //    Going cold instead costs a slow compile and nothing else, so
        //    this is the same fail-CLOSED trade every rung above makes.
        if let Some(reason) = warm_dir_disk_pressure(&warm_dir) {
            self.record_warm_obs(&warm_dir, "cold-fallback", &reason);
            return None;
        }

        self.record_warm_obs(&warm_dir, "warm", "hit");
        Some(WarmTargetGuard {
            dir: warm_dir,
            in_proc,
            flock: Some(flock),
        })
    }

    /// Route a single-WT push-path project-check through the shared
    /// [`BatchCoalescer`] so that N concurrent pushers against the same
    /// server-derived project-check plan share ONE physical
    /// `run_batch_check_now` call instead of N serialised overlay runs.
    ///
    /// ## Coalesce key
    /// `"project-check-plan:<fingerprint>"` where the fingerprint is
    /// computed from the daemon's current `cargoless.checks.yaml`, engine
    /// version, profile, and selected check configs for this changed-file
    /// set. Manifest edits deliberately return `None` and fall back to the
    /// direct path so the overlaid manifest is evaluated after materialize.
    ///
    /// ## overlay_files path convention
    /// The push path already converts repo-relative paths to absolute
    /// analysis-root paths inside `push_overlay_with_options` (via
    /// `map_repo_relative_files`). By the time `ProjectCheckRunContext` is
    /// constructed the files are absolute. We therefore set
    /// `repo_relative = false` on the batch request so `run_batch_check_now`
    /// does NOT re-join them under the root a second time.
    ///
    /// ## Empty vs Green distinction
    /// The batch path returns `BatchVerdict::Green` for both "checks ran and
    /// passed" and "no checks were selected (empty profile)". The `Empty`
    /// distinction is NOT preserved through the coalesced path — callers
    /// receive `ProjectCheckSummary::Green` in both cases. This is
    /// conservative (green-is-green at verdict time) and documented here as
    /// an explicit known limitation.
    ///
    /// ## Off-path (no context / no overlay)
    /// When the context has an empty `base_ref` or the `analysis_root` would
    /// be empty (WT-local check, no central-daemon overlay), `None` is
    /// returned and the caller falls back to the direct
    /// `with_project_check_overlay` path.
    pub(crate) fn coalesced_project_check(
        &self,
        wt: &Path,
        context: &ProjectCheckRunContext,
    ) -> Option<(crate::servedrv::ProjectCheckSummary, Vec<String>)> {
        // Git-native and typed candidates are complete immutable identities.
        // Until the batch API carries one exact identity per member, never
        // union either form into a base-ref overlay batch: exactness outranks
        // this optimization.
        if context.source_sha.is_some() || context.candidate_snapshot.is_some() {
            return None;
        }
        let base_ref = context.base_ref.trim();
        let root_str = context.root.to_string_lossy();
        if base_ref.is_empty() || root_str.trim().is_empty() {
            return None;
        }

        let wt_key = wt.to_string_lossy().into_owned();
        let member = cargoless_core::batch::BatchMember {
            worktree: wt_key.clone(),
            files: context.overlay_files.clone(),
            changed_files: context.changed_files.clone().unwrap_or_default(),
        };

        let mut request = BatchCheckRequest::new(format!("pushpath:{wt_key}"), base_ref);
        // overlay_files are already absolute analysis-root paths (the push
        // path converted them in push_overlay_with_options via
        // map_repo_relative_files). repo_relative = false so run_batch_check_now
        // does not re-join them.
        request.options = cargoless_core::transport::PushOverlayOptions {
            repo_relative: false,
            analysis_root: Some(root_str.into_owned()),
            base_sha: None,
            source_ref: None,
            source_sha: None,
            comparison_base_sha: None,
            candidate_snapshot: None,
            changed_files: None, // changed_files live on the member, not the options
            // Carry the push's gate + witness-only filter through the coalesced
            // lane. `run_batch_check_now` reads these off `request.options` to
            // build the `ServeBatchChecker`, so a gated witness push runs ONLY
            // its requested check_ids instead of the full dev profile. These
            // are ALSO part of `BatchCoalesceKey` (see `batch_coalesce_key`),
            // so a gated witness-only run never coalesces into — and returns a
            // waiter the verdict of — a concurrent advisory full-profile run
            // on the same root+base.
            gate: context.gate,
            check_ids: context.check_ids.clone(),
            semantic: None,
        };
        request.members = vec![member];
        request.corun = true;
        request.coalesce_key = Some(gated_or_plan_coalesce_token(
            &context.root,
            context.gate,
            &request,
        )?);

        // coalesce_key was set above, so this is always Some; `?` keeps the
        // defensive None-path (empty-after-trim) without a clippy::question_mark lint.
        let key = batch_coalesce_key(&request)?;

        let report = self
            .batch_coalescer
            .submit(key, &request, |combined| self.run_batch_check_now(combined));

        // The coalescing HIT, not the eligibility. `witness-gate ... coalesce=true`
        // (emitted by `gated_or_plan_coalesce_token`) says only that this push
        // *could* share a run; it is emitted identically whether the physical run
        // ended up with 1 member or 40. These three fields are the outcome, and
        // they are the only way to tell a working coalescer from an inert one:
        //
        //   executed_members — members in the PHYSICAL run this verdict came from.
        //                      1 means we did not coalesce, whatever the plan said.
        //   batch_id         — `coalesced:<key>:run-<seq>`; two pushes reporting the
        //                      SAME id provably shared one compile.
        //   queue_wait_ms    — time this member sat in the coalescer queue, which
        //                      separates "compiling for 40min" from "queued for 40min".
        //
        // Read off the report before `members` is consumed below. This is the
        // measurement the CGLS by-base fix (1e33c2d) has to be judged on: it must
        // be a measured hit rate, never an assumed one — the documented failure
        // mode in this fleet is an optimisation that silently no-ops while
        // exiting 0.
        eprintln!(
            "[cargoless:obs] witness-batch wt={} executed_members={} batch_id={} queue_wait_ms={} combined_checks={} solo_checks={}",
            wt_key,
            report.executed_members,
            report.executed_batch_id.as_deref().unwrap_or("-"),
            report.queue_wait_ms,
            report.combined_checks,
            report.solo_checks,
        );

        // Find this WT's slice in the returned report.
        let member_result = report.members.into_iter().find(|m| m.worktree == wt_key);

        Some(match member_result {
            None => {
                // Coalescer returned a report without our member — treat as
                // indeterminate (should not happen in practice). No member ⇒
                // no ran ids to report.
                (
                    crate::servedrv::ProjectCheckSummary::Indeterminate {
                        reason: "project_check_batch_missing_member",
                        detail: format!("coalesced report did not include member {wt_key}"),
                    },
                    Vec::new(),
                )
            }
            Some(m) => {
                // The witness's gate proof, carried through the coalesced path
                // exactly as the direct path carries it (servedrv
                // run_project_checks_and_log): the ids of the checks that ran
                // in the shared physical run. Green and Red both keep it (a red
                // still proves the cone compiled); only a true batch-level
                // Indeterminate or the red-without-diagnostics downgrade drops
                // it, matching the direct path's Empty/Indeterminate arms.
                let ran_check_ids = m.ran_check_ids;
                match m.verdict {
                    cargoless_core::batch::BatchVerdict::Green => {
                        // CombinedGreen and SoloGreen both map to Green.
                        // Empty is indistinguishable at this layer (documented above).
                        (crate::servedrv::ProjectCheckSummary::Green, ran_check_ids)
                    }
                    cargoless_core::batch::BatchVerdict::Red => {
                        let error_count = m
                            .diagnostics
                            .iter()
                            .filter(|d| d.severity == cargoless_core::Severity::Error)
                            .count() as u32;
                        // Defensive: if error_count is 0 despite Red verdict, route
                        // to Indeterminate (mirrors the same guard in run_project_checks_and_log).
                        if error_count == 0 {
                            (
                                crate::servedrv::ProjectCheckSummary::Indeterminate {
                                    reason: "project_check_red_without_diagnostics",
                                    detail: format!(
                                        "batch member {wt_key} red but 0 error-severity diagnostics"
                                    ),
                                },
                                ran_check_ids,
                            )
                        } else {
                            (
                                crate::servedrv::ProjectCheckSummary::Red {
                                    error_count,
                                    diagnostics: m.diagnostics.clone(),
                                },
                                ran_check_ids,
                            )
                        }
                    }
                    cargoless_core::batch::BatchVerdict::Indeterminate => {
                        let detail = m
                            .diagnostics
                            .first()
                            .map(|d| d.message.clone())
                            .unwrap_or_else(|| "batch indeterminate (no detail)".to_string());
                        (
                            crate::servedrv::ProjectCheckSummary::Indeterminate {
                                reason: "project_check_batch_indeterminate",
                                detail,
                            },
                            ran_check_ids,
                        )
                    }
                }
            }
        })
    }

    pub fn quiescing(&self) -> bool {
        poisoned(&self.drain).quiescing
    }

    pub fn drain_complete(&self) -> bool {
        let drain = poisoned(&self.drain);
        let batch_counts = self.batch_coalescer.counts();
        let (witness_inflight, witness_waiting) = self.witness_gate.counts();
        drain.quiescing
            && drain.active_worktrees.is_empty()
            && poisoned(&self.pushed).is_empty()
            && batch_counts.waiters == 0
            && batch_counts.inflight_runs == 0
            && witness_waiting == 0
            && witness_inflight == 0
    }

    fn mark_push_active(&self, worktree: &str) -> bool {
        let mut drain = poisoned(&self.drain);
        if drain.quiescing {
            return false;
        }
        drain.active_worktrees.insert(worktree.to_string());
        true
    }

    fn mark_worktree_published(&self, worktree: &str) {
        poisoned(&self.drain).active_worktrees.remove(worktree);
    }

    fn activity_snapshot(&self) -> DaemonActivity {
        let drain = poisoned(&self.drain);
        let batch_counts = self.batch_coalescer.counts();
        let (witness_inflight, witness_waiting) = self.witness_gate.counts();
        DaemonActivity {
            quiescing: drain.quiescing,
            active_worktrees: drain.active_worktrees.len() as u32,
            // Sum queue depths, not key count: the witness shares one WT
            // key, so a same-WT burst lives as N entries under one key.
            pending_pushes: poisoned(&self.pushed)
                .values()
                .map(VecDeque::len)
                .sum::<usize>() as u32,
            pending_batch_waiters: batch_counts.waiters,
            pending_batch_members: batch_counts.members,
            inflight_batch_runs: batch_counts.inflight_runs,
            inflight_witness_compiles: witness_inflight,
            waiting_witness_compiles: witness_waiting,
        }
    }

    fn run_batch_check_now(&self, request: &BatchCheckRequest) -> BatchReport {
        if self.quiescing() {
            return batch_indeterminate(request, "daemon is quiescing");
        }

        let Some(root) = request
            .options
            .analysis_root
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
        else {
            return batch_indeterminate(request, "batch_check v1 requires a shared analysis_root");
        };
        let base_ref = request.base_ref.trim();
        if base_ref.is_empty() {
            return batch_indeterminate(request, "batch_check requires a non-empty base_ref");
        }
        if !root.join(".git").exists() {
            return batch_indeterminate(
                request,
                format!("analysis_root `{}` is not a git checkout", root.display()),
            );
        }

        let members =
            match map_batch_members(&root, request.options.repo_relative, &request.members) {
                Ok(members) => members,
                Err(e) => return batch_indeterminate(request, e),
            };

        // #A3 — per-member truncation guard. Suspect members are withheld
        // from execution and stitched back as Indeterminate (escalate, not
        // green, not whole-batch failure): one truncated member must
        // neither pass on a bare-base check nor poison its batch-mates'
        // honest results.
        let suspect_reasons: Vec<Option<String>> =
            members.iter().map(member_truncation_suspect).collect();
        for (member, reason) in members.iter().zip(&suspect_reasons) {
            if let Some(why) = reason {
                eprintln!(
                    "[cargoless:batch] member-rejected worktree={}: {why} (#A3)",
                    member.worktree
                );
            }
        }
        let clean_members: Vec<BatchMember> = members
            .iter()
            .zip(&suspect_reasons)
            .filter(|(_, reason)| reason.is_none())
            .map(|(member, _)| member.clone())
            .collect();

        let inner = if clean_members.is_empty() && !members.is_empty() {
            // Every member suspect ⇒ nothing executes; skip the fetch (no
            // point spending the sync_lock on a batch that cannot run).
            BatchReport {
                batch_id: request.batch_id.clone(),
                verdict: BatchVerdict::Green,
                members: Vec::new(),
                combined_checks: 0,
                solo_checks: 0,
                duration_ms: 0,
                queue_wait_ms: 0,
                executed_members: 0,
                executed_batch_id: Some(request.batch_id.clone()),
            }
        } else {
            {
                let _guard = poisoned(&self.sync_lock);
                if let Err(e) = sync_analysis_root(&root, base_ref) {
                    return batch_indeterminate(request, e);
                }
            }

            let checker = ServeBatchChecker {
                api: self,
                root,
                base_ref: base_ref.to_string(),
                gate: request.options.gate,
                check_ids: request.options.check_ids.clone(),
            };
            run_batch(
                request.batch_id.clone(),
                &clean_members,
                &checker,
                if request.corun {
                    CorunPolicy::Corun
                } else {
                    CorunPolicy::NoCorun
                },
            )
        };

        if suspect_reasons.iter().all(Option::is_none) {
            // No suspects ⇒ `clean_members == members`; the executed
            // report passes through byte-identical to the pre-#A3 path.
            return inner;
        }
        stitch_suspect_members(inner, &members, &suspect_reasons)
    }

    fn execute_batch_report(&self, request: &BatchCheckRequest) -> BatchReport {
        if request.options.candidate_snapshot.is_some() {
            return batch_indeterminate(
                request,
                "candidate_snapshot.coalescing_forbidden: typed candidates require one exact per-source execution and cannot use BatchCheckRequest",
            );
        }
        if let Some(key) = batch_coalesce_key(request) {
            self.batch_coalescer
                .submit(key, request, |combined| self.run_batch_check_now(combined))
        } else {
            self.run_batch_check_now(request)
        }
    }

    fn execute_batch_outcome_v3(&self, request: &BatchCheckRequest) -> Option<OutcomeEnvelope> {
        let context = request.options.semantic.as_ref()?;
        context.validate().ok()?;
        if let Some(existing) = self.get_outcome_v3(&context.attempt_id) {
            return Some(existing);
        }

        let mut member_identity = Vec::new();
        for member in &request.members {
            member_identity.extend_from_slice(member.worktree.as_bytes());
            member_identity.push(0);
            member_identity.extend_from_slice(canonical_pairs_digest(&member.files).as_bytes());
            member_identity.push(0);
            for path in &member.changed_files {
                member_identity.extend_from_slice(path.as_bytes());
                member_identity.push(0);
            }
        }
        let check_plan = serde_json::json!({
            "check_profile": request.check_profile.as_ref().map(|profile| format!("{profile:?}")),
            "check_ids": request.options.check_ids,
            "corun": request.corun,
        });
        let subject = Subject::Batch {
            batch_id: NonEmptyText::new(request.batch_id.clone()).ok()?,
            base_sha: text_v3(
                request
                    .options
                    .base_sha
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(&request.base_ref),
            ),
            ordered_member_digest: text_v3(sha256_hex(&member_identity)),
            check_plan_digest: text_v3(sha256_hex(check_plan.to_string().as_bytes())),
        };
        let mut outcome = self.begin_outcome_v3(
            context,
            Surface::Batch,
            subject,
            Phase::WaitingForExecutionSlot,
            "batch accepted and waiting for its physical execution",
        );
        let started = now_unix_ms();
        if let Some(queued) = outcome.timeline.last_mut() {
            queued.finished_at_unix_ms = Some(started);
        }
        outcome.timeline.push(PhaseRecord {
            phase: Phase::Executing,
            started_at_unix_ms: started,
            finished_at_unix_ms: None,
        });
        self.remember_outcome_v3(outcome.clone());

        let report = self.execute_batch_report(request);
        let report_json = batchreport_to_json(&report);
        let mut evidence = EvidenceBundle::default();
        evidence.push(ArtifactKind::BatchReport, report_json.clone());
        evidence.push(
            ArtifactKind::Events,
            format!(
                "{}\n",
                serde_json::json!({
                    "event": "batch_terminal",
                    "attempt_id": context.attempt_id.as_str(),
                    "execution_id": outcome.execution_id.as_ref().map(ToString::to_string),
                    "batch_id": report.batch_id,
                    "executed_batch_id": report.executed_batch_id,
                    "combined_checks": report.combined_checks,
                    "solo_checks": report.solo_checks,
                    "executed_members": report.executed_members,
                    "duration_ms": report.duration_ms,
                    "queue_wait_ms": report.queue_wait_ms,
                })
            ),
        );
        let store = self
            .evidence_store_v3
            .clone()
            .unwrap_or_else(|| EvidenceStore::new("."));
        let evidence_ref = store.reference_for(&context.attempt_id, &evidence).ok()?;

        let requested_check_ids = request
            .options
            .check_ids
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(text_v3)
            .collect();
        let mut executed_check_ids: BTreeSet<String> = BTreeSet::new();
        for member in &report.members {
            executed_check_ids.extend(member.ran_check_ids.iter().cloned());
        }
        let executed_check_ids = executed_check_ids.into_iter().map(text_v3).collect();

        let conclusion = match report.verdict {
            BatchVerdict::Green => Conclusion::Passed {
                basis: PassBasis::ChecksPassed {
                    requested_check_ids,
                    executed_check_ids,
                },
                evidence: evidence_ref,
                summary: text_v3(format!(
                    "batch passed: {} submitted member(s), {} physical member(s), {} combined and {} solo check(s)",
                    request.members.len(),
                    report.executed_members,
                    report.combined_checks,
                    report.solo_checks
                )),
            },
            BatchVerdict::Red => {
                let mut diagnostics = Vec::new();
                for member in &report.members {
                    diagnostics.extend(
                        member
                            .diagnostics
                            .iter()
                            .filter(|diagnostic| diagnostic.severity == Severity::Error)
                            .map(diagnostic_record_v3),
                    );
                }
                let cause = if let Some(first) = diagnostics.first().cloned() {
                    FailureCause::Diagnostics {
                        diagnostics: NonEmptyDiagnostics::new(
                            first,
                            diagnostics.into_iter().skip(1).collect(),
                        ),
                    }
                } else {
                    let red_members = report
                        .members
                        .iter()
                        .filter(|member| member.verdict == BatchVerdict::Red)
                        .count()
                        .max(1);
                    FailureCause::UnlocatedDiagnosticReport {
                        origin: DiagnosticOrigin::ProjectCheck,
                        authority: Authority::Blocking,
                        reported_count: std::num::NonZeroU32::new(
                            u32::try_from(red_members).unwrap_or(u32::MAX),
                        )
                        .expect("red member count is clamped to at least one"),
                        producer: text_v3("batch_report_v1"),
                        raw_report_digest: text_v3(sha256_hex(report_json.as_bytes())),
                    }
                };
                Conclusion::Failed {
                    cause,
                    path_overlap: batch_path_overlap(&report, request),
                    evidence: evidence_ref,
                    summary: text_v3(format!(
                        "batch failed: {} of {} returned member result(s) are red; see batch-report.json for every member and provenance",
                        report
                            .members
                            .iter()
                            .filter(|member| member.verdict == BatchVerdict::Red)
                            .count(),
                        report.members.len()
                    )),
                }
            }
            BatchVerdict::Indeterminate => Conclusion::Indeterminate {
                cause: IndeterminateCause::AttributionUnavailable {
                    producer: text_v3("batch_report_v1"),
                },
                retry: RetryDirective::Automatic {
                    attempt: context.attempt_number,
                    maximum_attempts: context.maximum_attempts,
                    after_ms: context.retry_after_ms,
                },
                evidence: evidence_ref,
                summary: text_v3(
                    "batch execution did not produce a trustworthy code conclusion; legacy detail is retained in batch-report.json",
                ),
            },
        };
        outcome.conclude(conclusion);
        let terminal = now_unix_ms();
        if let Some(executing) = outcome.timeline.last_mut() {
            executing.finished_at_unix_ms = Some(terminal);
        }
        outcome.timeline.push(PhaseRecord {
            phase: Phase::Terminal,
            started_at_unix_ms: terminal,
            finished_at_unix_ms: Some(terminal),
        });

        let evidence_error = if let Some(durable) = self.evidence_store_v3.as_ref() {
            let class = if matches!(outcome.conclusion, Conclusion::Passed { .. }) {
                EvidenceClass::Success
            } else {
                EvidenceClass::Terminal
            };
            durable
                .persist(&outcome, class, &evidence)
                .err()
                .map(|error| error.to_string())
        } else {
            Some("durable evidence store is not configured".to_string())
        };
        if let Some(error) = evidence_error {
            mark_evidence_unavailable_v3(&mut outcome, error);
            let mut metrics = poisoned(&self.outcome_metrics_v3);
            metrics.evidence_persist_failures = metrics.evidence_persist_failures.saturating_add(1);
        }
        {
            let mut metrics = poisoned(&self.outcome_metrics_v3);
            *metrics
                .terminal_by_code
                .entry(outcome.conclusion.semantic_code().to_string())
                .or_insert(0) += 1;
            *metrics
                .reactions_by_state
                .entry(reaction_state_name(outcome.reaction.state).to_string())
                .or_insert(0) += 1;
        }
        self.remember_outcome_v3(outcome.clone());
        Some(outcome)
    }
}

fn diagnostic_record_v3(diagnostic: &Diagnostic) -> DiagnosticRecord {
    let path = diagnostic.file_path.to_string_lossy();
    let origin = match diagnostic.source.as_deref() {
        Some("rustc") => DiagnosticOrigin::Rustc,
        Some(source) if source.contains("rust-analyzer") => DiagnosticOrigin::RustAnalyzerNative,
        Some("cargoless") => DiagnosticOrigin::SyntheticCheck,
        _ => DiagnosticOrigin::ProjectCheck,
    };
    let severity = match diagnostic.severity {
        Severity::Error => DiagnosticSeverity::Error,
        Severity::Warning => DiagnosticSeverity::Warning,
        Severity::Info => DiagnosticSeverity::Information,
        Severity::Hint => DiagnosticSeverity::Hint,
    };
    let location = if diagnostic.line > 0
        && !path.trim().is_empty()
        && !path.starts_with("<cargoless-")
    {
        DiagnosticLocation::Located {
            file: text_v3(path.as_ref()),
            line: diagnostic.line,
            column: diagnostic.col,
        }
    } else {
        DiagnosticLocation::Unlocated {
            explanation: text_v3("the producing project check did not retain a source location"),
        }
    };
    let message = if diagnostic.message.trim().is_empty() {
        "producer emitted an empty diagnostic message"
    } else {
        diagnostic.message.as_str()
    };
    let fingerprint = sha256_hex(
        format!(
            "{origin:?}|{:?}|{}|{}|{}|{}|{}",
            diagnostic.severity,
            path,
            diagnostic.line,
            diagnostic.col,
            diagnostic.code.as_deref().unwrap_or("-"),
            message
        )
        .as_bytes(),
    );
    DiagnosticRecord {
        origin,
        severity,
        authority: if diagnostic.severity == Severity::Error {
            Authority::Blocking
        } else {
            Authority::Advisory
        },
        location,
        code: diagnostic
            .code
            .as_deref()
            .filter(|code| !code.trim().is_empty())
            .map(text_v3),
        message: text_v3(message),
        fingerprint: text_v3(fingerprint),
    }
}

fn batch_path_overlap(report: &BatchReport, request: &BatchCheckRequest) -> PathOverlap {
    let changed: Vec<&str> = request
        .members
        .iter()
        .flat_map(|member| member.changed_files.iter().map(String::as_str))
        .collect();
    let diagnostics: Vec<&Diagnostic> = report
        .members
        .iter()
        .flat_map(|member| member.diagnostics.iter())
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .collect();
    if diagnostics.is_empty() || changed.is_empty() {
        return PathOverlap::NotComputable;
    }
    if diagnostics.iter().any(|diagnostic| {
        diagnostic.line == 0
            || diagnostic
                .file_path
                .to_string_lossy()
                .starts_with("<cargoless-")
    }) {
        return PathOverlap::NotComputable;
    }
    let overlaps = diagnostics
        .iter()
        .filter(|diagnostic| {
            let path = diagnostic.file_path.to_string_lossy();
            let normalized = path.trim_start_matches("./");
            changed.iter().any(|changed_path| {
                let changed_path = changed_path.trim_start_matches("./");
                normalized == changed_path || normalized.ends_with(&format!("/{changed_path}"))
            })
        })
        .count();
    match overlaps {
        0 => PathOverlap::NoPathsOverlap,
        count if count == diagnostics.len() => PathOverlap::AllPathsOverlap,
        _ => PathOverlap::SomePathsOverlap,
    }
}

impl VerdictService for ServeVerdictState {
    /// A6 — `GET /readyz` reads this. Overrides the default-`true` trait
    /// body with the honest RA-warm latch: `false` until servedrv calls
    /// [`ServeVerdictState::mark_ready`] at the first completed RA
    /// handshake.
    fn ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    fn get_status(&self, worktree: &str) -> Option<WorktreeStatus> {
        self.get_status_attributed(worktree, None)
    }

    /// Submit a candidate to the build lane.
    ///
    /// The trait default *fails* on a laneless daemon and that is preserved
    /// here: answering "queued" when no lane exists would leave the caller
    /// waiting forever for a build that is never going to run.
    fn lane_enqueue(&self, request: &LaneEnqueueRequest) -> Result<String, String> {
        let Some(lane) = self.lane.as_ref() else {
            return Err("build lane not enabled on this daemon".to_string());
        };
        // `id` and `head` are required and are NOT defaulted. A member with no
        // identity cannot be attributed, ejected or reported on, and an
        // anonymous candidate must never enter a queue that can move the
        // trunk. `changed_files` IS optional — a caller that cannot compute a
        // diff still gets queued and accepts that its reds are unattributable.
        if request.id.trim().is_empty() {
            return Err("`id` is required — an anonymous member cannot be attributed".to_string());
        }
        if request.head.trim().is_empty() {
            return Err("`head` is required — without it staleness cannot be detected".to_string());
        }
        lane.enqueue(
            LaneMember::new(request.id.clone(), request.head.clone())
                .with_changed_files(request.changed_files.clone()),
        )
    }

    /// Lift an ejection by hand.
    fn lane_readmit(&self, id: &str) -> Result<String, String> {
        let Some(lane) = self.lane.as_ref() else {
            return Err("build lane not enabled on this daemon".to_string());
        };
        lane.readmit(id)
    }

    /// Take a member out of the lane permanently.
    ///
    /// The escape hatch for a member the lane will never finish with: a closed
    /// or superseded PR, or one blocked on something outside the lane's view —
    /// a required check it cannot pass, so the forge refuses the merge however
    /// green the candidate builds. Without this the member rebuilds forever and
    /// consumes the whole queue, because the lane's own ejection only fires on
    /// a red the member CAUSED.
    fn lane_withdraw(&self, id: &str) -> Result<String, String> {
        let Some(lane) = self.lane.as_ref() else {
            return Err("build lane not enabled on this daemon".to_string());
        };
        if id.trim().is_empty() {
            return Err("`id` is required — refusing to withdraw an unnamed member".to_string());
        }
        lane.withdraw(id)
    }

    /// The lane's product surface: queue depth, the running build, and every
    /// live ejection WITH its reason. An author whose change stopped moving
    /// needs to see which errors hold it, who else is implicated, and what
    /// will clear it — so the ejections carry `kind` (attributed vs not,
    /// because the two are cleared by different things) and the failing files.
    ///
    /// `why` carries the author-facing sentence itself, not just the fields it
    /// is derived from. The enum tags alone are not readable: `files: []` means
    /// "could not identify them" for `unattributed` and "nothing was compiled"
    /// for `infrastructure`; `shared_with` always names other implicated
    /// members, while their relationship differs by kind; and the re-admission
    /// rule differs per kind. The daemon already computes that sentence —
    /// withholding it here is what forced every downstream consumer to
    /// re-derive it, and they drifted.
    ///
    /// `now` is the lane's clock on the same scale as `expires_at_tick`. A
    /// deadline published without the clock it is measured against cannot
    /// answer "how long until this clears".
    ///
    /// `queue_depth` counts members accepted by `POST /lane` that the worker
    /// has not stepped yet, as well as the lane's own queue. During a build the
    /// worker is blocked inside the compile and the lane's queue cannot grow,
    /// so without that a member submitted mid-build reads as `queue_depth: 0`
    /// for the whole build after being told "queued" — observed in production,
    /// and an author who sees it reasonably re-submits. `queued` names them.
    ///
    /// `activity` is what the driver is BLOCKED ON, which `phase` cannot say.
    /// The lane is legitimately `idle` while a green candidate is landing — the
    /// build is over and the roster is empty — and reporting only `idle` at
    /// that moment invites an operator to roll the daemon mid-merge.
    fn lane_snapshot(&self) -> Option<serde_json::Value> {
        let lane = self.lane.as_ref()?;
        let s = lane.snapshot();
        Some(serde_json::json!({
            "phase": s.phase,
            "activity": s.activity,
            "landing": s.landing,
            "queue_depth": s.queue_depth,
            "queued": s.queued,
            "generation": s.generation,
            "now": s.now,
            "in_flight": s.in_flight,
            "members": s.members.iter().map(|m| serde_json::json!({
                "id": m.id,
                "head": m.head,
                "state": m.state,
            })).collect::<Vec<_>>(),
            "ejections": s.ejections.iter().map(|e| serde_json::json!({
                "id": e.id,
                "head": e.head,
                "cause": e.cause,
                "kind": e.kind,
                "files": e.files,
                "shared_with": e.shared_with,
                "expires_at_tick": e.expires_at_tick,
                "why": e.why,
            })).collect::<Vec<_>>(),
        }))
    }

    /// Resolution rule (the `<absent>` fix's read half):
    /// - `base_sha = Some(sha)` → the ring entry attributed to exactly that
    ///   commit, if retained; else the live slot IFF the slot is itself that
    ///   commit; else `None`. A poll for commit X NEVER returns commit Y's
    ///   verdict — that cross-attribution is the bug the strict witness
    ///   exists to refuse.
    /// - `base_sha = None`/empty → the current live slot (plain
    ///   `cargoless status`, unattributed readers).
    ///
    /// Age is derived at read time from the publish timestamp regardless of
    /// which source answered, so a remote reader always sees an honest age.
    fn get_status_attributed(
        &self,
        worktree: &str,
        base_sha: Option<&str>,
    ) -> Option<WorktreeStatus> {
        let now = crate::statusfile::now_unix();
        let stamp_age = |mut s: WorktreeStatus| {
            s.heartbeat_age_secs = now.saturating_sub(s.published_at);
            s
        };
        match base_sha.filter(|s| !s.is_empty()) {
            Some(want) => {
                // Ring first: the exact commit, even if superseded in the slot.
                if let Some(hit) = poisoned(&self.verdict_history)
                    .get(worktree)
                    .and_then(|ring| {
                        ring.iter()
                            .rev()
                            .find(|s| {
                                s.candidate_manifest_digest.is_none()
                                    && s.base_sha.as_deref() == Some(want)
                            })
                            .cloned()
                    })
                {
                    return Some(stamp_age(hit));
                }
                // Fall back to the live slot ONLY when it is that same commit
                // (a verdict published before the ring existed, or one that
                // is still current) — never cross-attribute another commit.
                poisoned(&self.statuses)
                    .get(worktree)
                    .filter(|s| {
                        s.candidate_manifest_digest.is_none() && s.base_sha.as_deref() == Some(want)
                    })
                    .cloned()
                    .map(stamp_age)
            }
            None => poisoned(&self.statuses)
                .get(worktree)
                .cloned()
                .map(stamp_age),
        }
    }

    fn get_status_candidate_attributed(
        &self,
        worktree: &str,
        manifest_digest: &str,
    ) -> Option<WorktreeStatus> {
        if manifest_digest.is_empty() {
            return None;
        }
        let now = crate::statusfile::now_unix();
        let stamp_age = |mut status: WorktreeStatus| {
            status.heartbeat_age_secs = now.saturating_sub(status.published_at);
            status
        };
        if let Some(hit) = poisoned(&self.verdict_history)
            .get(worktree)
            .and_then(|ring| {
                ring.iter()
                    .rev()
                    .find(|status| {
                        status.candidate_manifest_digest.as_deref() == Some(manifest_digest)
                    })
                    .cloned()
            })
        {
            return Some(stamp_age(hit));
        }
        poisoned(&self.statuses)
            .get(worktree)
            .filter(|status| status.candidate_manifest_digest.as_deref() == Some(manifest_digest))
            .cloned()
            .map(stamp_age)
    }

    fn get_verdict(&self, worktree: &str) -> Option<String> {
        poisoned(&self.statuses)
            .get(worktree)
            .map(|s| s.verdict.clone())
    }

    fn get_diagnostics(&self, worktree: &str) -> Vec<Diagnostic> {
        let status = poisoned(&self.statuses).get(worktree).cloned();
        if let Some(manifest_digest) = status
            .as_ref()
            .and_then(|status| status.candidate_manifest_digest.as_ref())
        {
            return poisoned(&self.candidate_diagnostics)
                .get(&(worktree.to_string(), manifest_digest.clone()))
                .cloned()
                .unwrap_or_default();
        }
        let current_sha = status.and_then(|status| status.base_sha);
        poisoned(&self.diagnostics)
            .get(&(worktree.to_string(), current_sha))
            .cloned()
            .unwrap_or_default()
    }

    fn get_diagnostics_attributed(
        &self,
        worktree: &str,
        base_sha: Option<&str>,
    ) -> Vec<Diagnostic> {
        let Some(base_sha) = base_sha.filter(|sha| !sha.is_empty()) else {
            return self.get_diagnostics(worktree);
        };
        poisoned(&self.diagnostics)
            .get(&(worktree.to_string(), Some(base_sha.to_string())))
            .cloned()
            .unwrap_or_default()
    }

    fn get_diagnostics_candidate_attributed(
        &self,
        worktree: &str,
        manifest_digest: &str,
    ) -> Vec<Diagnostic> {
        if manifest_digest.is_empty() {
            return Vec::new();
        }
        poisoned(&self.candidate_diagnostics)
            .get(&(worktree.to_string(), manifest_digest.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    fn get_outcome_v3(
        &self,
        attempt_id: &cargoless_core::outcome::AttemptId,
    ) -> Option<OutcomeEnvelope> {
        if let Some(outcome) = poisoned(&self.outcomes_v3).get(attempt_id).cloned() {
            return Some(outcome);
        }
        self.evidence_store_v3
            .as_ref()
            .and_then(|store| store.read_outcome(attempt_id).ok().flatten())
    }

    fn outcome_metrics_v3(&self) -> Option<serde_json::Value> {
        let metrics = poisoned(&self.outcome_metrics_v3);
        let pending_attempts = poisoned(&self.outcomes_v3)
            .values()
            .filter(|outcome| matches!(outcome.conclusion, Conclusion::Pending { .. }))
            .count();
        Some(serde_json::json!({
            "schema": "cargoless.metrics/v3",
            "pending_attempts": pending_attempts,
            "terminal_by_code": &metrics.terminal_by_code,
            "reactions_by_state": &metrics.reactions_by_state,
            "ra_storm_outcomes": metrics.ra_storm_outcomes,
            "evidence_persist_failures": metrics.evidence_persist_failures,
            "last_ra_error_lines": metrics.last_ra_error_lines,
            "last_ra_duplicates_suppressed": metrics.last_ra_suppressed_lines,
        }))
    }

    fn get_evidence_v3(
        &self,
        attempt_id: &cargoless_core::outcome::AttemptId,
        artifact: &str,
    ) -> Option<Vec<u8>> {
        self.evidence_store_v3
            .as_ref()
            .and_then(|store| store.read_named(attempt_id, artifact).ok().flatten())
    }

    fn list_worktrees(&self) -> Vec<WorktreeSummary> {
        poisoned(&self.statuses)
            .values()
            .map(|s| WorktreeSummary {
                worktree: s.worktree.clone(),
                verdict: s.verdict.clone(),
                daemon_build_id: s.daemon_build_id.clone(),
                red_diagnostics: s.red_diagnostics,
            })
            .collect()
    }

    fn subscribe(&self) -> Receiver<TransitionEvent> {
        let (tx, rx) = channel();
        poisoned(&self.subs).push(tx);
        rx
    }

    /// #240/2b — overlay-push ingest. The WRITE-PLANE entry for the
    /// pushed-mode central-daemon topology (D-PUSHOVERLAY §2.4 / §4).
    ///
    /// 1. Plain pushes record `(base_ref, files)` in the per-WT queue
    ///    and wake the rust-analyzer serve loop.
    /// 2. Gated pushes are handed directly to the isolated Cargo witness
    ///    dispatcher and never enter the rust-analyzer overlay queue.
    /// 3. Return an ack: `accepted=true` + `applied_files` count. The
    ///    ack does NOT block on the verdict; the client uses the
    ///    already-shipped subscribe (SSE) or `get_status` for the
    ///    verdict (D-PUSHOVERLAY §2.3 — no new verdict-egress surface).
    fn push_overlay(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
    ) -> PushOverlayAck {
        self.push_overlay_with_profile(worktree, base_ref, files, None)
    }

    fn push_overlay_with_profile(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
        check_profile: Option<&CheckProfile>,
    ) -> PushOverlayAck {
        self.push_overlay_with_options(worktree, base_ref, files, check_profile, None)
    }

    fn push_overlay_with_options(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
        check_profile: Option<&CheckProfile>,
        options: Option<&PushOverlayOptions>,
    ) -> PushOverlayAck {
        let semantic = options.and_then(|options| options.semantic.clone());
        if let Some(context) = semantic.as_ref() {
            if let Err(reason) = context.validate() {
                return rejected_push(worktree, reason);
            }
            if poisoned(&self.outcomes_v3).contains_key(&context.attempt_id) {
                // Attempt submission is idempotent. A network retry must not
                // enqueue the same execution twice or overwrite its terminal
                // result; callers poll the existing exact attempt.
                return PushOverlayAck {
                    worktree: worktree.to_string(),
                    accepted: true,
                    applied_files: files.len() as u32,
                    ..Default::default()
                };
            }
        }
        let semantic_subject = match (semantic.as_ref(), options) {
            (Some(_), Some(options)) => {
                match overlay_subject_v3(worktree, base_ref, files, check_profile, options) {
                    Ok(subject) => Some(subject),
                    Err(reason) => return rejected_push(worktree, reason),
                }
            }
            (Some(_), None) => {
                return rejected_push(worktree, "v3 overlay requires semantic options");
            }
            (None, _) => None,
        };
        if self.quiescing() {
            return rejected_push(worktree, "daemon is quiescing");
        }
        let mut mapped_files = files.to_vec();
        let mut analysis_root = None;
        let mut base_sha = None;
        let mut source_ref = None;
        let mut source_sha = None;
        let mut candidate_snapshot = None;
        let mut typed_legacy_files = None;
        let mut changed_files = None;
        let mut gate = false;
        let mut check_ids = None;
        if let Some(options) = options {
            match typed_candidate_overlay(options, files) {
                Ok(Some(candidate)) => {
                    candidate_snapshot = Some(candidate.manifest);
                    changed_files = Some(candidate.changed_files);
                    typed_legacy_files = Some(candidate.legacy_files);
                }
                Ok(None) => changed_files = options.changed_files.clone(),
                Err(error) => return rejected_push(worktree, &error),
            }
            gate = options.gate;
            check_ids = options.check_ids.clone();
            analysis_root = options
                .analysis_root
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from);
            base_sha = options
                .base_sha
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            source_ref = options
                .source_ref
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            source_sha = options
                .source_sha
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if source_ref.is_some() != source_sha.is_some() {
                return rejected_push(
                    worktree,
                    "source_ref and source_sha must be supplied together",
                );
            }
            if let Some(source_ref) = source_ref.as_deref() {
                if let Err(e) = validate_source_ref(source_ref) {
                    return rejected_push(worktree, &e);
                }
            }
            if let Some(source_sha) = source_sha.as_deref() {
                if !is_commit_hash(source_sha) {
                    return rejected_push(
                        worktree,
                        "source_sha must be a full 40- or 64-hex object id",
                    );
                }
                if !files.is_empty() {
                    return rejected_push(
                        worktree,
                        "exact-Git push must carry an empty files array",
                    );
                }
                if base_sha.as_deref() != Some(source_sha) {
                    return rejected_push(
                        worktree,
                        "base_sha must equal source_sha for exact-Git attribution",
                    );
                }
                if !gate {
                    return rejected_push(worktree, "exact-Git source requires gate=true");
                }
                if analysis_root.is_none() {
                    return rejected_push(
                        worktree,
                        "exact-Git source requires an analysis_root checkout",
                    );
                }
            }

            if options.repo_relative {
                let Some(root) = analysis_root.as_ref() else {
                    return rejected_push(worktree, "repo-relative push missing analysis_root");
                };
                let files = typed_legacy_files.as_deref().unwrap_or(files);
                mapped_files = match map_repo_relative_files(root, files) {
                    Ok(files) => files,
                    Err(e) => return rejected_push(worktree, &e),
                };
            }

            // #A3 — empty-overlay false-green guard. Keyed on file COUNT,
            // never content: deletions arrive deliberately as empty-content
            // entries (push.rs carries them so RA stops seeing the dead
            // file) and must pass. Two truncation signatures are fatal:
            // a push *claiming* changed files while carrying none, and a
            // central-daemon (analysis_root) push with nothing to apply —
            // both would make the daemon check the bare base and publish
            // a verdict attributed to changes it never saw (the known
            // 32MiB-payload false-green incident class). Plain optionless
            // empty pushes stay accepted: locally that is the legitimate
            // "revert RA to the on-disk tree" operation. Placed BEFORE
            // `ensure_analysis_root` so a doomed push never spends the
            // sync_lock on a fetch.
            if files.is_empty() && source_sha.is_none() && candidate_snapshot.is_none() {
                if let Some(changed) = changed_files.as_ref().filter(|c| !c.is_empty()) {
                    return rejected_push(
                        worktree,
                        &format!(
                            "push claims {} changed file(s) but carries 0 overlay files; \
                             suspect payload truncation — refusing to check the bare base",
                            changed.len()
                        ),
                    );
                }
                if analysis_root.is_some() {
                    return rejected_push(
                        worktree,
                        "central-daemon push (analysis_root set) carries 0 overlay files; \
                         refusing to publish a base-tree verdict as if it covered the push",
                    );
                }
            }

            if let Some(root) = analysis_root.as_ref() {
                if let Some(manifest) = candidate_snapshot.as_ref() {
                    let _guard = poisoned(&self.sync_lock);
                    if let Err(error) = ensure_candidate_snapshot_base(root, base_ref, manifest) {
                        return rejected_push(worktree, &error);
                    }
                } else if let (Some(source_ref), Some(source_sha)) =
                    (source_ref.as_deref(), source_sha.as_deref())
                {
                    let _guard = poisoned(&self.sync_lock);
                    if let Err(e) = fetch_verified_source(root, source_ref, source_sha) {
                        return rejected_push(worktree, &e);
                    }
                } else {
                    let base = base_ref.trim();
                    if !base.is_empty() {
                        let _guard = poisoned(&self.sync_lock);
                        if let Err(e) = ensure_analysis_root(root, base, base_sha.as_deref()) {
                            return rejected_push(worktree, &e);
                        }
                    }
                }
            }
        }

        if !self.mark_push_active(worktree) {
            return rejected_push(worktree, "daemon is quiescing");
        }

        let applied_files = candidate_snapshot
            .as_ref()
            .map(|manifest| manifest.candidate.operation_count())
            .unwrap_or(files.len() as u64) as u32;
        let pushed = PushedOverlay {
            base_ref: base_ref.to_string(),
            files: mapped_files,
            analysis_root,
            base_sha,
            source_ref,
            source_sha,
            candidate_snapshot,
            last_push_unix: crate::statusfile::now_unix(),
            changed_files,
            check_profile: check_profile.cloned(),
            gate,
            check_ids,
            semantic: semantic.clone(),
        };
        if pushed.gate {
            let Some(tx) = poisoned(&self.direct_gate_signal).as_ref().cloned() else {
                self.mark_worktree_published(worktree);
                return rejected_push(worktree, "direct gate dispatcher is unavailable");
            };
            let now = crate::statusfile::now_unix();
            let attribution = PushAttribution {
                base_sha: pushed.base_sha.clone(),
                candidate: pushed
                    .candidate_snapshot
                    .as_ref()
                    .map(CandidateVerdictIdentity::from_manifest),
                macro_blind_hit: compute_macro_blind_hit(
                    pushed.changed_files.as_deref(),
                    &macro_blind_globs(),
                    &pushed.files,
                    &macro_blind_macros(),
                ),
                push_received_unix: pushed.last_push_unix,
                consumed_unix: now,
                consumed_at: Instant::now(),
                semantic: pushed.semantic.clone(),
            };
            let project_root = pushed
                .analysis_root
                .clone()
                .unwrap_or_else(|| PathBuf::from(worktree));
            let request = DirectGateRequest {
                wt: PathBuf::from(worktree),
                context: ProjectCheckRunContext {
                    root: project_root,
                    changed_files: pushed.changed_files.clone(),
                    base_ref: pushed.base_ref.clone(),
                    base_sha: pushed.base_sha.clone(),
                    source_ref: pushed.source_ref.clone(),
                    source_sha: pushed.source_sha.clone(),
                    candidate_snapshot: pushed.candidate_snapshot.clone(),
                    overlay_files: pushed.files.clone(),
                    materialize_overlay: pushed.analysis_root.is_some(),
                    gate: true,
                    check_ids: pushed.check_ids.clone(),
                },
                attribution,
            };
            if let (Some(context), Some(subject)) = (semantic.as_ref(), semantic_subject) {
                self.begin_outcome_v3(
                    context,
                    Surface::Overlay,
                    subject,
                    Phase::Queued,
                    "gated overlay accepted and queued for compiler witness",
                );
            }
            if tx.send(request).is_err() {
                if let Some(context) = semantic.as_ref() {
                    self.forget_outcome_v3(&context.attempt_id);
                }
                self.mark_worktree_published(worktree);
                return rejected_push(worktree, "direct gate dispatcher disconnected");
            }
            eprintln!(
                "[cargoless:obs] witness-direct-dispatch wt={} source_sha={} files={}",
                worktree,
                pushed.source_sha.as_deref().unwrap_or("overlay"),
                pushed.files.len()
            );
            return PushOverlayAck {
                worktree: worktree.to_string(),
                accepted: true,
                applied_files,
                ..Default::default()
            };
        }
        // CGLS-25 — base_sha-keyed enqueue: a concurrent PR pushing on the
        // same hardcoded worktree key must not destroy this one's pending
        // overlay before the serve loop consumes it. But a rapid re-push of
        // the SAME commit (same base_sha — an FS save-storm or a retried
        // push) SHOULD still coalesce to latest-wins, exactly as the
        // `hard_witness_generation` latch supersedes only same-(wt,base_sha).
        // So: replace an already-queued entry with a matching base_sha in
        // place (latest content wins for that commit); otherwise append, so
        // a DISTINCT commit gets its own SwitchOverlay→witness cycle (drained
        // one-per-wake, with `take_overlay_for` re-signalling the tail).
        // base_sha == None (FS-watch / unattributed) keeps the historical
        // single-slot coalesce: all None-keyed pushes collapse to the last.
        //
        // R3 mitigation — at `pushed_max_per_wt`, a DISTINCT-base_sha push
        // is REJECTED with 429 instead of appending. Same-base_sha still
        // replaces in place regardless of depth — a legitimate retry of an
        // already-queued commit must not be starved by a stuck consumer.
        // The reject is loud (obs line at [`rejected_push_queue_full`])
        // so its hit rate is measurable per the "no dead machinery" rule.
        {
            let mut store = poisoned(&self.pushed);
            let queue = store.entry(worktree.to_string()).or_default();
            let replace = if pushed.candidate_snapshot.is_some() {
                None
            } else {
                queue
                    .iter_mut()
                    .find(|queued| match (&queued.semantic, &pushed.semantic) {
                        (Some(left), Some(right)) => left.attempt_id == right.attempt_id,
                        (None, None) => queued.base_sha == pushed.base_sha,
                        _ => false,
                    })
            };
            match replace {
                Some(existing) => *existing = pushed,
                None => {
                    if queue.len() >= self.pushed_max_per_wt {
                        let depth = queue.len();
                        let cap = self.pushed_max_per_wt;
                        drop(store);
                        return rejected_push_queue_full(worktree, cap, depth);
                    }
                    queue.push_back(pushed);
                }
            }
        }
        // Publish the pending identity before waking the serve loop. The loop
        // can fail an initial rust-analyzer spawn immediately; waking first
        // lets that failure race ahead of the accepted outcome and strand the
        // later pending record forever.
        if let (Some(context), Some(subject)) = (semantic.as_ref(), semantic_subject) {
            self.begin_outcome_v3(
                context,
                Surface::Overlay,
                subject,
                Phase::Queued,
                "overlay accepted and queued for analysis",
            );
        }
        // Wake the serve loop (best-effort — see attach_push_signal doc).
        if let Some(tx) = poisoned(&self.push_signal).as_ref() {
            let _ = tx.send(worktree.to_string());
        }
        PushOverlayAck {
            worktree: worktree.to_string(),
            accepted: true,
            applied_files,
            ..Default::default()
        }
    }

    fn batch_check(&self, request: &BatchCheckRequest) -> BatchReport {
        self.execute_batch_report(request)
    }

    fn submit_batch_v3(&self, request: &BatchCheckRequest) -> Option<OutcomeEnvelope> {
        self.execute_batch_outcome_v3(request)
    }

    fn daemon_activity(&self) -> DaemonActivity {
        self.activity_snapshot()
    }

    fn resolved_config(&self) -> Option<serde_json::Value> {
        poisoned(&self.resolved_config).clone()
    }

    fn warm_target_stats(&self) -> Option<serde_json::Value> {
        self.warm_target_stats_json()
    }

    fn request_quiesce(&self) -> DaemonActivity {
        {
            let mut drain = poisoned(&self.drain);
            drain.quiescing = true;
        }
        self.batch_coalescer.cv.notify_all();
        self.activity_snapshot()
    }
}

struct ServeBatchChecker<'a> {
    api: &'a ServeVerdictState,
    root: PathBuf,
    base_ref: String,
    /// Carried from `request.options.gate`: true ⇒ this is a merge-gate run.
    gate: bool,
    /// Carried from `request.options.check_ids`: the witness-only filter for
    /// a gated run. Only consulted when `gate` is true.
    check_ids: Option<Vec<String>>,
}

impl BatchChecker for ServeBatchChecker<'_> {
    fn check_combined(&self, members: &[BatchMember]) -> Result<ProjectCheckReport, String> {
        let overlay_files = match union_overlay_files(members) {
            Ok(files) => files,
            Err(conflict) => return Ok(batch_red_project_report(&conflict)),
        };
        let changed_files = union_changed_files(members);
        self.run_overlay(overlay_files, changed_files)
    }

    fn check_solo(&self, member: &BatchMember) -> Result<ProjectCheckReport, String> {
        let changed_files = member_changed_files(member);
        self.run_overlay(member.files.clone(), changed_files)
    }
}

/// The witness-only run filter shared by BOTH project-check run lanes.
/// `Some(ids)` iff `gate` is set AND `check_ids` is a non-empty set after
/// dropping blank entries; else `None` (advisory lane, or a gated push that
/// requested no specific ids ⇒ full profile).
///
/// Load-bearing that this is ONE function used by both lanes: a gated push
/// normally runs witness-only through the COALESCED lane
/// (`ServeBatchChecker::run_overlay`), but when its overlay touches the
/// project-check manifest the plan is non-coalesceable, so
/// `coalesced_project_check` returns `None` and the push falls through to the
/// DIRECT lane (`run_project_checks_and_log`). Both lanes MUST filter by the
/// same witness ids, or a manifest-touching gated PR would silently run the
/// full ~97-check profile on the direct lane and publish its environmental
/// governance reds as a gating RED.
pub(crate) fn gated_witness_ids(
    gate: bool,
    check_ids: Option<&Vec<String>>,
) -> Option<Vec<String>> {
    if !gate {
        return None;
    }
    let filtered: Vec<String> = check_ids?
        .iter()
        .filter(|id| !id.trim().is_empty())
        .cloned()
        .collect();
    (!filtered.is_empty()).then_some(filtered)
}

impl ServeBatchChecker<'_> {
    /// The witness-only filter for this gated run — see [`gated_witness_ids`].
    /// `None` when not a gated witness run (advisory lane, or a gated push
    /// that requested no specific ids) — the caller then runs the full profile.
    fn gated_run_ids(&self) -> Option<Vec<String>> {
        gated_witness_ids(self.gate, self.check_ids.as_ref())
    }

    fn run_overlay(
        &self,
        overlay_files: Vec<(String, String)>,
        changed_files: Vec<String>,
    ) -> Result<ProjectCheckReport, String> {
        let changed_files = (!changed_files.is_empty()).then_some(changed_files);
        let gated_ids = self.gated_run_ids();
        let context = ProjectCheckRunContext {
            root: self.root.clone(),
            changed_files: changed_files.clone(),
            base_ref: self.base_ref.clone(),
            base_sha: None,
            source_ref: None,
            source_sha: None,
            candidate_snapshot: None,
            overlay_files,
            materialize_overlay: true,
            gate: self.gate,
            check_ids: self.check_ids.clone(),
        };
        self.api
            .with_project_check_overlay(&context, |root, warm, _candidate_manifest_path| {
                // Both arms thread the CGLS-26 warm target dir (or None=cold).
                match gated_ids.as_deref() {
                    // GATED, witness-only. Run EXACTLY the requested witness
                    // ids (their intersection with the dev profile). This is
                    // the merge-gate lane: it proves the compile witnesses
                    // (ssr/wasm/isolator-vsock) ran, WITHOUT dragging the ~97
                    // governance/coverage checks (and their environmental
                    // reds inside the scratch overlay) into a gating verdict.
                    // `only_ids` restricts the profile to these ids; the
                    // no-vacuous-green invariant holds because every witness
                    // id is `tier:dev` and so is a member of the dev profile
                    // (verified against tf-multiverse cargoless.checks.yaml —
                    // the AND of id-match ∧ profile-match selects exactly the
                    // witnesses). `changed_files=None` so trigger-globbing
                    // never additionally skips a requested witness.
                    Some(ids) => cargoless_core::project_checks::run_profile_with_ids_in(
                        root, "dev", ids, None, warm,
                    ),
                    // ADVISORY lane (or a gated push with no id filter): run
                    // the FULL `dev` profile with NO trigger-filtering
                    // (`only_ids=None`, `changed_files=None`). The compiler
                    // witnesses are in the dev profile, so this guarantees
                    // they run on the batch lane — AND so do every other
                    // dev-profile check the dev-merge gate depends on
                    // (element-agnostic, hydration-gate, the audits, …).
                    // Passing `changed_files=None` is the key:
                    // `run_dev_with_changes` would pass the real changed-file
                    // list, letting `select_for_changes` SKIP the witness (and
                    // others) whose trigger globs the changes don't match —
                    // exactly the gap this path closes. `None` means "no
                    // change-filter → run the whole profile".
                    None => cargoless_core::project_checks::run_profile_with_ids_in(
                        root,
                        "dev",
                        &[],
                        None,
                        warm,
                    ),
                }
            })
            .and_then(|report| report.map_err(|e| format!("project checks failed: {e}")))
    }
}

fn map_batch_members(
    root: &Path,
    repo_relative: bool,
    members: &[BatchMember],
) -> Result<Vec<BatchMember>, String> {
    members
        .iter()
        .map(|member| {
            let files = if repo_relative {
                map_repo_relative_files(root, &member.files)?
            } else {
                member.files.clone()
            };
            Ok(BatchMember {
                worktree: member.worktree.clone(),
                files,
                changed_files: member.changed_files.clone(),
            })
        })
        .collect()
}

/// #A3 — the per-member truncation signature: a member *claiming* changed
/// files while carrying zero overlay files. Such a member would execute
/// against the bare base and return a verdict attributed to changes the
/// daemon never saw (the 32MiB-payload false-green incident class). A
/// member with empty `changed_files` AND empty `files` stays legal — that
/// is an honest "no diff vs base" entry, and a bare-base check is exactly
/// its verdict. Keyed on file COUNT, never content (deletions are carried
/// as empty-content entries and must pass).
fn member_truncation_suspect(member: &BatchMember) -> Option<String> {
    if member.files.is_empty() && !member.changed_files.is_empty() {
        return Some(format!(
            "member claims {} changed file(s) but carries 0 overlay files; \
             suspect payload truncation",
            member.changed_files.len()
        ));
    }
    None
}

/// #A3 — rebuild the report in request-member order, splicing executed
/// results (from `inner`, which ran only the clean members, in order)
/// around Indeterminate placeholders for the suspects. Request order is
/// load-bearing: `distribute_combined_report` slices a coalesced report
/// by per-waiter member offsets.
fn stitch_suspect_members(
    inner: BatchReport,
    members: &[BatchMember],
    suspect_reasons: &[Option<String>],
) -> BatchReport {
    // Destructure (not `..inner` after moving `members` out — E0382):
    // every counter passes through from the executed run, so the report
    // stays honest about what physically ran (`executed_members` counts
    // only clean members; suspects never executed).
    let BatchReport {
        batch_id,
        verdict: _,
        members: executed_results,
        combined_checks,
        solo_checks,
        duration_ms,
        queue_wait_ms,
        executed_members,
        executed_batch_id,
    } = inner;
    let mut executed = executed_results.into_iter();
    let stitched: Vec<cargoless_core::batch::BatchMemberResult> = members
        .iter()
        .zip(suspect_reasons)
        .map(|(member, reason)| {
            let why = match reason {
                Some(why) => why.as_str(),
                // Total by construction: `run_batch` returns one result
                // per input member in order, so this branch is
                // unreachable today — but a short executed report must
                // surface as an honest Indeterminate, never a member
                // silently missing from the report.
                None => match executed.next() {
                    Some(result) => return result,
                    None => "internal: executed batch report ran short of members",
                },
            };
            cargoless_core::batch::BatchMemberResult {
                worktree: member.worktree.clone(),
                verdict: BatchVerdict::Indeterminate,
                provenance: cargoless_core::batch::BatchProvenance::Indeterminate,
                diagnostics: vec![batch_diagnostic(why)],
                duration_ms: 0,
                // Suspect placeholder — never executed, so no ran ids.
                ran_check_ids: Vec::new(),
            }
        })
        .collect();
    BatchReport {
        batch_id,
        verdict: verdict_for_members(&stitched),
        members: stitched,
        combined_checks,
        solo_checks,
        duration_ms,
        queue_wait_ms,
        executed_members,
        executed_batch_id,
    }
}

fn union_overlay_files(members: &[BatchMember]) -> Result<Vec<(String, String)>, String> {
    let mut by_path: BTreeMap<String, String> = BTreeMap::new();
    for member in members {
        for (path, content) in &member.files {
            match by_path.get(path) {
                Some(existing) if existing != content => {
                    return Err(format!(
                        "batch members carry different content for `{path}`; \
                         rerun/merge serially or resolve the overlay conflict"
                    ));
                }
                Some(_) => {}
                None => {
                    by_path.insert(path.clone(), content.clone());
                }
            }
        }
    }
    Ok(by_path.into_iter().collect())
}

fn union_changed_files(members: &[BatchMember]) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for member in members {
        for path in member_changed_files(member) {
            paths.insert(path);
        }
    }
    paths.into_iter().collect()
}

fn member_changed_files(member: &BatchMember) -> Vec<String> {
    // Empty changed_files means "unknown" (run all checks). Do not fall back
    // to mapped overlay paths here: central-daemon overlays are absolute
    // analysis-root paths, while project-check trigger rules expect the
    // caller's repo-relative changed-file list.
    member.changed_files.clone()
}

fn batch_indeterminate(request: &BatchCheckRequest, why: impl Into<String>) -> BatchReport {
    let why = why.into();
    BatchReport {
        batch_id: request.batch_id.clone(),
        verdict: BatchVerdict::Indeterminate,
        members: request
            .members
            .iter()
            .map(|member| cargoless_core::batch::BatchMemberResult {
                worktree: member.worktree.clone(),
                verdict: BatchVerdict::Indeterminate,
                provenance: cargoless_core::batch::BatchProvenance::Indeterminate,
                diagnostics: vec![batch_diagnostic(&why)],
                duration_ms: 0,
                // Batch-level indeterminate — nothing ran.
                ran_check_ids: Vec::new(),
            })
            .collect(),
        combined_checks: 0,
        solo_checks: 0,
        duration_ms: 0,
        queue_wait_ms: 0,
        executed_members: request.members.len() as u32,
        executed_batch_id: Some(request.batch_id.clone()),
    }
}

fn batch_diagnostic(message: &str) -> Diagnostic {
    Diagnostic {
        file_path: PathBuf::from("<cargoless-batch>"),
        line: 0,
        col: 0,
        severity: Severity::Error,
        code: Some("cargoless.batch".into()),
        message: message.to_string(),
        source: Some("cargoless".into()),
    }
}

fn batch_red_project_report(message: &str) -> ProjectCheckReport {
    ProjectCheckReport {
        tree: TreeState::Red,
        diagnostics: vec![batch_diagnostic(message)],
        results: Vec::new(),
        skipped: Vec::new(),
        duration_ms: 0,
    }
}

fn rejected_push(worktree: &str, why: &str) -> PushOverlayAck {
    eprintln!("[cargoless:push] rejected worktree={worktree}: {why}");
    PushOverlayAck {
        worktree: worktree.to_string(),
        accepted: false,
        applied_files: 0,
        reject_http_status: Some(409),
        reject_body: Some(why.to_string()),
    }
}

/// R3 mitigation — pushed-queue-full backpressure. Distinct from
/// [`rejected_push`] in that this carries an explicit HTTP status +
/// structured body pair that the HTTP handler surfaces as `429 Too Many
/// Requests` so a client can back off intelligently rather than treating
/// the queue-full case as an ordinary server-side refusal.
fn rejected_push_queue_full(worktree: &str, cap: usize, depth: usize) -> PushOverlayAck {
    eprintln!("[cargoless:obs] pushed-queue-reject wt={worktree} cap={cap} depth={depth}");
    let body = serde_json::json!({
        "error": "pushed_queue_full",
        "cap": cap,
        "wt": worktree,
    })
    .to_string();
    PushOverlayAck {
        worktree: worktree.to_string(),
        accepted: false,
        applied_files: 0,
        reject_http_status: Some(429),
        reject_body: Some(body),
    }
}

fn map_repo_relative_files(
    root: &Path,
    files: &[(String, String)],
) -> Result<Vec<(String, String)>, String> {
    files
        .iter()
        .map(|(path, content)| {
            let rel = safe_repo_relative_path(path)?;
            Ok((
                root.join(rel).to_string_lossy().into_owned(),
                content.clone(),
            ))
        })
        .collect()
}

struct TypedCandidateOverlay {
    manifest: CandidateSnapshotManifest,
    changed_files: Vec<String>,
    legacy_files: Vec<(String, String)>,
}

fn typed_candidate_overlay(
    options: &PushOverlayOptions,
    legacy_files: &[(String, String)],
) -> Result<Option<TypedCandidateOverlay>, String> {
    let comparison_base_sha = options
        .comparison_base_sha
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let manifest = options.candidate_snapshot.as_ref();
    match (comparison_base_sha, manifest) {
        (None, None) => return Ok(None),
        (Some(_), None) => {
            return Err(
                "candidate_snapshot.manifest_missing: comparison base requires a typed manifest"
                    .to_string(),
            );
        }
        (None, Some(_)) => {
            return Err(
                "candidate_snapshot.comparison_base_missing: typed manifest requires comparison_base_sha"
                    .to_string(),
            );
        }
        (Some(_), Some(_)) => {}
    }
    let (Some(comparison_base_sha), Some(manifest)) = (comparison_base_sha, manifest) else {
        return Err(
            "candidate_snapshot.pairing_invalid: typed candidate identity is incomplete"
                .to_string(),
        );
    };
    cargoless_core::validate_candidate_snapshot_manifest(manifest)
        .map_err(|error| error.to_string())?;
    if comparison_base_sha != manifest.comparison_base.commit_sha {
        return Err(format!(
            "candidate_snapshot.comparison_base_mismatch: option {comparison_base_sha} differs from manifest {}",
            manifest.comparison_base.commit_sha
        ));
    }
    if !options.repo_relative || options.analysis_root.is_none() {
        return Err(
            "candidate_snapshot.analysis_root_missing: typed overlays require repo_relative analysis_root"
                .to_string(),
        );
    }
    if options.source_ref.is_some() || options.source_sha.is_some() {
        return Err(
            "candidate_snapshot.identity_conflict: typed overlay and exact-Git source are mutually exclusive"
                .to_string(),
        );
    }
    let CandidateSnapshot::Overlay { operations, .. } = &manifest.candidate else {
        return Err(
            "candidate_snapshot.kind_unsupported: push_overlay requires kind=overlay".to_string(),
        );
    };

    let changed_files = operations
        .iter()
        .map(|operation| operation.path().to_string())
        .collect::<Vec<_>>();
    if let Some(advertised) = options.changed_files.as_ref() {
        if advertised != &changed_files {
            return Err(
                "candidate_snapshot.changed_files_mismatch: hint differs from typed operations"
                    .to_string(),
            );
        }
    }

    let mut projection = Vec::new();
    for operation in operations {
        match operation {
            OverlayOperation::Delete { path, .. } => {
                projection.push((path.clone(), String::new()));
            }
            OverlayOperation::Upsert { path, payload, .. } => {
                let bytes = decode_overlay_payload(payload).map_err(|error| error.to_string())?;
                if let Ok(content) = String::from_utf8(bytes) {
                    projection.push((path.clone(), content));
                }
            }
        }
    }
    if projection != legacy_files {
        return Err(
            "candidate_snapshot.legacy_projection_mismatch: files differ from typed operations"
                .to_string(),
        );
    }
    Ok(Some(TypedCandidateOverlay {
        manifest: manifest.clone(),
        changed_files,
        legacy_files: projection,
    }))
}

fn safe_repo_relative_path(path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(format!("repo-relative push carried absolute path `{path}`"));
    }
    let mut out = PathBuf::new();
    for component in p.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("repo-relative path escapes repo root: `{path}`"));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err("repo-relative push carried an empty path".to_string());
    }
    Ok(out)
}

fn validate_source_ref(source_ref: &str) -> Result<(), String> {
    let allowed_namespace =
        source_ref.starts_with("refs/heads/") || source_ref.starts_with("refs/pull/");
    if !allowed_namespace
        || source_ref.contains("..")
        || source_ref.contains("@{")
        || source_ref.ends_with('/')
        || source_ref.bytes().any(|b| {
            b.is_ascii_control()
                || matches!(b, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
    {
        return Err(format!(
            "source_ref `{source_ref}` is not an allowed heads/pull ref"
        ));
    }
    let valid = Command::new("git")
        .args(["check-ref-format", source_ref])
        .status()
        .map_err(|e| format!("failed to validate source_ref with git: {e}"))?
        .success();
    if !valid {
        return Err(format!("source_ref `{source_ref}` is not a valid Git ref"));
    }
    Ok(())
}

/// Fetch one advertised ref, then prove the requested immutable candidate is
/// a commit reachable from it. The checkout itself is not moved: project
/// checks create a detached scratch worktree at `source_sha`.
fn fetch_verified_source(root: &Path, source_ref: &str, source_sha: &str) -> Result<(), String> {
    if !root.join(".git").exists() {
        return Err(format!(
            "analysis_root `{}` is not a git checkout",
            root.display()
        ));
    }
    validate_source_ref(source_ref)?;
    if !is_commit_hash(source_sha) {
        return Err("source_sha must be a full 40- or 64-hex object id".to_string());
    }
    retry_with_sleeps(
        &[Duration::from_secs(1), Duration::from_secs(3)],
        |attempt| {
            if attempt > 0 {
                eprintln!(
                    "[cargoless:git] source fetch retry attempt={attempt} root={}",
                    root.display()
                );
            }
            run_git(root, &["fetch", "--no-tags", "origin", source_ref])
        },
    )?;
    if !local_commit_exists(root, source_sha) {
        return Err(format!(
            "source_sha {source_sha} is not a commit after fetching {source_ref}"
        ));
    }
    if !run_git_success(
        root,
        &["merge-base", "--is-ancestor", source_sha, "FETCH_HEAD"],
    )? {
        return Err(format!(
            "source_sha {source_sha} is not reachable from fetched {source_ref}"
        ));
    }
    Ok(())
}

/// Run `op` up to `1 + sleeps.len()` times, sleeping `sleeps[n]` after
/// failed attempt `n`. `op` receives the 0-based attempt index so call
/// sites can log retries. First success wins; otherwise the last error
/// propagates.
fn retry_with_sleeps<T>(
    sleeps: &[Duration],
    mut op: impl FnMut(usize) -> Result<T, String>,
) -> Result<T, String> {
    let mut attempt = 0;
    loop {
        match op(attempt) {
            Ok(value) => return Ok(value),
            Err(e) => {
                if attempt >= sleeps.len() {
                    return Err(e);
                }
                std::thread::sleep(sleeps[attempt]);
                attempt += 1;
            }
        }
    }
}

fn sync_analysis_root(root: &Path, base_ref: &str) -> Result<(), String> {
    if !root.join(".git").exists() {
        return Err(format!(
            "analysis_root `{}` is not a git checkout",
            root.display()
        ));
    }
    let fetch_ref = base_ref
        .strip_prefix("origin/")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(base_ref);
    // Skip the network fetch when `base_ref` is a bare commit hash that is
    // ALREADY in the local object store. A push pins `base_sha` to the exact
    // commit it diffed against — almost never a branch tip, because dev moves
    // constantly — and `git fetch origin <sha>` asks the remote to serve that
    // commit *by hash*. Forgejo/GitHub `upload-pack` refuse a non-advertised
    // object by default (`uploadpack.allowAnySHA1InWant` off), returning
    // `fatal: remote error: upload-pack: not our ref <sha>` and failing the
    // whole push — EVEN THOUGH the serve shard's repo-sync sidecar keeps deep
    // `origin/dev` history precisely so these bases resolve locally. So if the
    // object is present, trust the mirror and go straight to reset/clean; only
    // hit the network when the base is genuinely absent. A symbolic ref
    // (`origin/dev`, a branch/tag name) is NOT short-circuited — it must fetch
    // to observe upstream advances.
    let base_present_locally = is_commit_hash(base_ref) && local_commit_exists(root, base_ref);
    if !base_present_locally {
        // The fetch is the only network step here: transient hiccups get 2
        // retries (1s then 3s) before the error fails the whole push/batch.
        // The reset/clean below stay single-shot — they are local-only.
        retry_with_sleeps(
            &[Duration::from_secs(1), Duration::from_secs(3)],
            |attempt| {
                if attempt > 0 {
                    eprintln!(
                        "[cargoless:git] fetch retry attempt={attempt} worktree-root={}",
                        root.display()
                    );
                }
                run_git(root, &["fetch", "--prune", "origin", fetch_ref])
            },
        )?;
    }
    reset_analysis_root(root, base_ref)?;
    Ok(())
}

fn ensure_candidate_snapshot_base(
    root: &Path,
    base_ref: &str,
    manifest: &CandidateSnapshotManifest,
) -> Result<(), String> {
    sync_analysis_root(root, base_ref)?;
    let comparison = crate::candidate_snapshot_git::resolve_commit_snapshot(
        root,
        &manifest.comparison_base.commit_sha,
    )
    .map_err(|error| {
        format!(
            "candidate_snapshot.comparison_base_invalid: could not resolve comparison base {}: {error}",
            manifest.comparison_base.commit_sha
        )
    })?;
    if comparison.git_object_format != manifest.git_object_format
        || comparison.reference != manifest.comparison_base
    {
        return Err(format!(
            "candidate_snapshot.comparison_base_invalid: resolved {:?} with {} differs from advertised {:?} with {}",
            comparison.reference,
            comparison.git_object_format.as_str(),
            manifest.comparison_base,
            manifest.git_object_format.as_str(),
        ));
    }

    let advertised_base = manifest.candidate.base().ok_or_else(|| {
        "candidate_snapshot.kind_unsupported: project-check overlay requires an overlay base"
            .to_string()
    })?;
    if !local_commit_exists(root, &advertised_base.commit_sha) {
        return Err(format!(
            "candidate_snapshot.base_commit_missing: candidate base {} is absent after fetching {base_ref}",
            advertised_base.commit_sha
        ));
    }
    let operation_base = crate::candidate_snapshot_git::resolve_commit_snapshot(
        root,
        &advertised_base.commit_sha,
    )
    .map_err(|error| {
        format!(
            "candidate_snapshot.base_commit_missing: could not resolve candidate base {}: {error}",
            advertised_base.commit_sha
        )
    })?;
    if operation_base.git_object_format != manifest.git_object_format {
        return Err(format!(
            "candidate_snapshot.object_format_mismatch: repository uses {} but manifest uses {}",
            operation_base.git_object_format.as_str(),
            manifest.git_object_format.as_str()
        ));
    }
    if operation_base.reference != *advertised_base {
        return Err(format!(
            "candidate_snapshot.base_tree_mismatch: resolved {:?} differs from advertised {:?}",
            operation_base.reference, advertised_base
        ));
    }
    if !run_git_success(
        root,
        &[
            "merge-base",
            "--is-ancestor",
            &manifest.comparison_base.commit_sha,
            &advertised_base.commit_sha,
        ],
    )? {
        return Err(format!(
            "candidate_snapshot.comparison_base_invalid: comparison base {} is not an ancestor of candidate base {}",
            manifest.comparison_base.commit_sha, advertised_base.commit_sha
        ));
    }
    if !run_git_success(
        root,
        &[
            "merge-base",
            "--is-ancestor",
            &advertised_base.commit_sha,
            "HEAD",
        ],
    )? {
        return Err(format!(
            "candidate_snapshot.base_unreachable: candidate base {} is not reachable from fetched {base_ref}",
            advertised_base.commit_sha
        ));
    }
    // The RA compatibility overlay is a delta from this exact base, not from
    // the moving symbolic ref used only to fetch/reachability-check it.
    reset_analysis_root(root, &advertised_base.commit_sha)?;
    let candidate_entries = manifest
        .candidate
        .entries()
        .iter()
        .cloned()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    cargoless_core::validate_manifest_against_entry_maps(
        manifest,
        Some(&operation_base.entries),
        &candidate_entries,
    )
    .map_err(|error| error.to_string())
}

/// `true` when `s` is a full git object hash (40-hex SHA-1 or 64-hex
/// SHA-256) — the only shape we trust the local mirror for. A symbolic
/// ref (branch/tag name, `origin/dev`, an abbreviated hash) returns
/// `false` so [`sync_analysis_root`] still fetches it: a name must hit the
/// network to observe upstream advances, and an abbreviation can't be
/// safely round-tripped through `reset --hard` without resolution.
fn is_commit_hash(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `true` when `sha` names a commit object already present in `root`'s
/// local object store. Used to skip a doomed `git fetch <sha>` (Forgejo
/// `upload-pack: not our ref`) when the repo-sync sidecar's deep history
/// already has the base. `^{commit}` forces commit-peeling so a stray blob
/// or tree sharing the hex can never be mistaken for a usable base.
fn local_commit_exists(root: &Path, sha: &str) -> bool {
    run_git_success(root, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).unwrap_or(false)
}

fn ensure_analysis_root(
    root: &Path,
    base_ref: &str,
    expected_base_sha: Option<&str>,
) -> Result<(), String> {
    if !root.join(".git").exists() {
        return Err(format!(
            "analysis_root `{}` is not a git checkout",
            root.display()
        ));
    }
    if let Some(sha) = expected_base_sha.map(str::trim).filter(|s| !s.is_empty()) {
        if analysis_root_clean_at_sha(root, sha)? {
            return Ok(());
        }
    }
    sync_analysis_root(root, base_ref)
}

fn analysis_root_clean_at_sha(root: &Path, expected_sha: &str) -> Result<bool, String> {
    let head = git_stdout(root, &["rev-parse", "HEAD"])?;
    if head.trim() != expected_sha {
        return Ok(false);
    }
    Ok(run_git_success(root, &["diff", "--quiet"])?
        && run_git_success(root, &["diff", "--cached", "--quiet"])?)
}

fn reset_analysis_root(root: &Path, base_ref: &str) -> Result<(), String> {
    run_git(root, &["reset", "--hard", base_ref])?;
    run_git(root, &["clean", "-fd", "-e", ".cargoless"])?;
    Ok(())
}

fn prepare_project_check_scratch(
    root: &Path,
    scratch_root: &Path,
    base_ref: &str,
) -> Result<(), String> {
    cleanup_project_check_scratch(root, scratch_root)?;
    if let Some(parent) = scratch_root.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "could not create project-check scratch parent `{}`: {e}",
                parent.display()
            )
        })?;
    }
    let scratch = scratch_root.to_string_lossy().into_owned();
    run_git(root, &["worktree", "add", "--detach", &scratch, base_ref])
}

fn prepare_new_protected_project_check_scratch(
    root: &Path,
    scratch_root: &Path,
    base_ref: &str,
) -> Result<(), String> {
    match std::fs::symlink_metadata(scratch_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(candidate_environment_unsafe(format!(
                "unpredictable project-check run path `{}` already exists",
                scratch_root.display()
            )));
        }
        Err(error) => {
            return Err(candidate_environment_unsafe(format!(
                "could not inspect new project-check run path `{}`: {error}",
                scratch_root.display()
            )));
        }
    }
    let scratch = scratch_root.to_string_lossy().into_owned();
    run_git(root, &["worktree", "add", "--detach", &scratch, base_ref])
}

fn set_private_directory_mode(_path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(_path, std::fs::Permissions::from_mode(0o700)).map_err(
            |error| {
                candidate_environment_unsafe(format!(
                    "could not protect run directory `{}`: {error}",
                    _path.display()
                ))
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
fn cleanup_incomplete_project_check_scratch(
    root: &Path,
    scratch_root: &Path,
    _canonical_parent: &Path,
) -> Result<(), String> {
    match std::fs::symlink_metadata(scratch_root) {
        Ok(_) => Err(candidate_environment_unsafe(format!(
            "incomplete project-check run `{}` has no pre-recorded identity; preserving it",
            scratch_root.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            run_git(root, &["worktree", "prune", "--expire", "now"])
        }
        Err(error) => Err(candidate_environment_unsafe(format!(
            "could not inspect incomplete project-check run `{}`: {error}",
            scratch_root.display()
        ))),
    }
}

fn cleanup_protected_project_check_scratch(
    root: &Path,
    protected: &ProtectedRunDirectory,
) -> Result<(), String> {
    cleanup_protected_project_check_scratch_with_after_quarantine(root, protected, |_| {})
}

fn cleanup_protected_project_check_scratch_with_after_quarantine(
    root: &Path,
    protected: &ProtectedRunDirectory,
    after_quarantine: impl FnOnce(&Path),
) -> Result<(), String> {
    let quarantine = protected.quarantine()?;
    after_quarantine(&quarantine.path);
    ensure_protected_original_absent(protected)?;
    quarantine.verify()?;
    remove_bound_protected_run(&quarantine)?;
    run_git(root, &["worktree", "prune", "--expire", "now"])
}

fn cleanup_project_check_scratch(root: &Path, scratch_root: &Path) -> Result<(), String> {
    let scratch = scratch_root.to_string_lossy().into_owned();
    match run_git(root, &["worktree", "remove", "--force", &scratch]) {
        Ok(()) => Ok(()),
        Err(git_err) => {
            if scratch_root.exists() {
                std::fs::remove_dir_all(scratch_root).map_err(|e| {
                    format!(
                        "{git_err}; fallback remove_dir_all `{}` failed: {e}",
                        scratch_root.display()
                    )
                })?;
            }
            // If the directory was unregistered, the remove above fails even
            // though there is nothing else to do. If registration metadata is
            // damaged or stale, pruning after filesystem cleanup removes the
            // prunable entry from the persistent repository volume.
            run_git(root, &["worktree", "prune", "--expire", "now"])
        }
    }
}

/// Reclaim project-check worktrees left by a prior daemon process.
///
/// This is a startup-only operation: the new process has not accepted work
/// yet, so every directory under the legacy `project-check-runs` and strict
/// `candidate-project-check-runs` namespaces belongs to a dead predecessor.
/// The warm target and durable evidence live beside these directories and are
/// deliberately preserved.
pub(crate) fn recover_project_check_scratch(
    root: &Path,
    state_dir: &Path,
) -> Result<usize, String> {
    let source = if state_dir.starts_with(root) {
        cargoless_core::config::Source::Default
    } else {
        cargoless_core::config::Source::Cli
    };
    recover_project_check_scratch_for_source(root, state_dir, source)
}

pub(crate) fn recover_project_check_scratch_for_source(
    root: &Path,
    state_dir: &Path,
    source: cargoless_core::config::Source,
) -> Result<usize, String> {
    let canonical_repo = std::fs::canonicalize(root).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not resolve repository for startup recovery `{}`: {error}",
            root.display()
        ))
    })?;
    if source == cargoless_core::config::Source::Default {
        let metadata = match std::fs::symlink_metadata(state_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(candidate_environment_unsafe(format!(
                    "could not inspect default state root `{}`: {error}",
                    state_dir.display()
                )));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            // Default state never enables typed candidates. A hostile or stale
            // default path is therefore not an authority to canonicalize or
            // recover through; leave it untouched for operator inspection.
            return Ok(0);
        }
        let canonical_state = std::fs::canonicalize(state_dir).map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not resolve default state root `{}`: {error}",
                state_dir.display()
            ))
        })?;
        if canonical_state != canonical_repo && !canonical_state.starts_with(&canonical_repo) {
            return Ok(0);
        }
        return recover_legacy_project_check_scratch_only(root, &canonical_state);
    }

    let protected = ProtectedStateRoot::open(root, state_dir, true)?;
    // Validate every protected namespace before deleting anything. The
    // scratch namespace predates typed candidates and may safely retain its
    // historical 0755 mode, but it must remain owner-controlled and not
    // group/world-writable. Candidate sidecars retain the exact 0700 policy.
    let scratch_parent = protected.legacy_scratch_namespace(false)?;
    let candidate_scratch_parent = protected.namespace("candidate-project-check-runs", false)?;
    let candidate_parent = protected.namespace("candidate-snapshots", false)?;
    let scratch_runs = scratch_parent
        .as_deref()
        .map(legacy_scratch_recovery_runs)
        .transpose()?
        .unwrap_or_default();
    let candidate_runs = candidate_parent
        .as_deref()
        .map(protected_recovery_runs)
        .transpose()?
        .unwrap_or_default();
    let candidate_scratch_runs = candidate_scratch_parent
        .as_deref()
        .map(protected_recovery_runs)
        .transpose()?
        .unwrap_or_default();
    let recovered = scratch_runs.len() + candidate_scratch_runs.len() + candidate_runs.len();
    for run in &scratch_runs {
        cleanup_protected_project_check_scratch(root, run)?;
    }
    for run in &candidate_scratch_runs {
        cleanup_protected_project_check_scratch(root, run)?;
    }
    for run in &candidate_runs {
        cleanup_protected_candidate_manifest_run(run)?;
    }
    run_git(root, &["worktree", "prune", "--expire", "now"])?;
    Ok(recovered)
}

fn protected_recovery_runs(namespace: &Path) -> Result<Vec<ProtectedRunDirectory>, String> {
    let entries = std::fs::read_dir(namespace).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not enumerate protected recovery namespace `{}`: {error}",
            namespace.display()
        ))
    })?;
    let mut runs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not enumerate protected recovery namespace `{}`: {error}",
                namespace.display()
            ))
        })?;
        runs.push(ProtectedRunDirectory::capture(entry.path(), namespace)?);
    }
    Ok(runs)
}

fn legacy_scratch_recovery_runs(namespace: &Path) -> Result<Vec<ProtectedRunDirectory>, String> {
    let entries = std::fs::read_dir(namespace).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not enumerate legacy scratch recovery namespace `{}`: {error}",
            namespace.display()
        ))
    })?;
    let mut runs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not enumerate legacy scratch recovery namespace `{}`: {error}",
                namespace.display()
            ))
        })?;
        #[cfg(unix)]
        let required_mode = {
            use std::os::unix::fs::PermissionsExt as _;
            match std::fs::symlink_metadata(entry.path())
                .map_err(|error| {
                    candidate_environment_unsafe(format!(
                        "could not inspect legacy scratch run `{}`: {error}",
                        entry.path().display()
                    ))
                })?
                .permissions()
                .mode()
                & 0o777
            {
                0o700 => 0o700,
                0o755 => 0o755,
                mode => {
                    return Err(candidate_environment_unsafe(format!(
                        "legacy scratch run `{}` has unsupported mode {mode:04o}",
                        entry.path().display()
                    )));
                }
            }
        };
        #[cfg(not(unix))]
        let required_mode = 0o700;
        runs.push(ProtectedRunDirectory::capture_with_mode(
            entry.path(),
            namespace,
            required_mode,
        )?);
    }
    Ok(runs)
}

fn recover_legacy_project_check_scratch_only(
    root: &Path,
    state_dir: &Path,
) -> Result<usize, String> {
    let scratch_parent = state_dir.join("project-check-runs");
    let mut recovered = 0usize;
    match std::fs::read_dir(&scratch_parent) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|error| {
                    format!(
                        "could not enumerate project-check scratch `{}`: {error}",
                        scratch_parent.display()
                    )
                })?;
                if !entry
                    .file_type()
                    .map_err(|error| {
                        format!(
                            "could not inspect project-check scratch `{}`: {error}",
                            entry.path().display()
                        )
                    })?
                    .is_dir()
                {
                    continue;
                }
                cleanup_project_check_scratch(root, &entry.path())?;
                recovered += 1;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not inspect project-check scratch `{}`: {error}",
                scratch_parent.display()
            ));
        }
    }
    run_git(root, &["worktree", "prune", "--expire", "now"])?;
    Ok(recovered)
}

fn materialize_overlay_files(root: &Path, files: &[(String, String)]) -> Result<(), String> {
    materialize_overlay_files_from_root(root, root, files)
}

fn materialize_candidate_snapshot(
    scratch_root: &Path,
    manifest: &CandidateSnapshotManifest,
) -> Result<(), String> {
    let CandidateSnapshot::Overlay { operations, .. } = &manifest.candidate else {
        return Err(
            "candidate_snapshot.kind_unsupported: project-check materialization requires kind=overlay"
                .to_string(),
        );
    };
    for operation in operations {
        let relative = safe_repo_relative_path(operation.path())?;
        match operation {
            OverlayOperation::Delete { path, .. } => {
                ensure_candidate_parent(scratch_root, &relative, false)?;
                let target = scratch_root.join(&relative);
                let metadata = std::fs::symlink_metadata(&target).map_err(|error| {
                    format!(
                        "candidate_snapshot.delete_missing: could not inspect {path:?}: {error}"
                    )
                })?;
                if !metadata.file_type().is_file() {
                    return Err(format!(
                        "candidate_snapshot.overlay_mode_unsupported: delete target {path:?} is not a regular file"
                    ));
                }
                std::fs::remove_file(&target).map_err(|error| {
                    format!("candidate_snapshot.materialize_failed: delete {path:?}: {error}")
                })?;
            }
            OverlayOperation::Upsert {
                path,
                mode,
                payload,
                ..
            } => {
                ensure_candidate_parent(scratch_root, &relative, true)?;
                let target = scratch_root.join(&relative);
                if let Ok(metadata) = std::fs::symlink_metadata(&target) {
                    if !metadata.file_type().is_file() {
                        return Err(format!(
                            "candidate_snapshot.overlay_mode_unsupported: upsert target {path:?} is not a regular file"
                        ));
                    }
                }
                let bytes = decode_overlay_payload(payload).map_err(|error| error.to_string())?;
                std::fs::write(&target, bytes).map_err(|error| {
                    format!("candidate_snapshot.materialize_failed: upsert {path:?}: {error}")
                })?;
                set_candidate_mode(&target, mode)?;
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
pub(crate) struct CandidateManifestSidecar {
    path: PathBuf,
    bytes: Vec<u8>,
    dev: u64,
    ino: u64,
}

impl std::ops::Deref for CandidateManifestSidecar {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<Path> for CandidateManifestSidecar {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

fn write_candidate_manifest(
    candidate_run_dir: &Path,
    manifest: &CandidateSnapshotManifest,
) -> Result<CandidateManifestSidecar, String> {
    validate_candidate_directory(candidate_run_dir, Some(0o700))?;
    let manifest_path = candidate_run_dir.join("manifest.json");
    let canonical = canonical_manifest_json(manifest).map_err(|error| error.to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&manifest_path).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not create exclusive manifest `{}`: {error}",
            manifest_path.display()
        ))
    })?;
    use std::io::Write as _;
    file.write_all(canonical.as_bytes()).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not write manifest `{}`: {error}",
            manifest_path.display()
        ))
    })?;
    file.sync_all().map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not sync manifest `{}`: {error}",
            manifest_path.display()
        ))
    })?;
    let written_metadata = file.metadata().map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not inspect written manifest `{}`: {error}",
            manifest_path.display()
        ))
    })?;
    validate_candidate_manifest_file(&manifest_path, &written_metadata)?;
    drop(file);

    let path_metadata = std::fs::symlink_metadata(&manifest_path).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not re-inspect manifest path `{}`: {error}",
            manifest_path.display()
        ))
    })?;
    if path_metadata.file_type().is_symlink() {
        return Err(candidate_environment_unsafe(format!(
            "manifest path `{}` became a symlink",
            manifest_path.display()
        )));
    }
    validate_candidate_manifest_file(&manifest_path, &path_metadata)?;

    let mut reopened = std::fs::File::open(&manifest_path).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not reopen manifest `{}`: {error}",
            manifest_path.display()
        ))
    })?;
    let reopened_metadata = reopened.metadata().map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not inspect reopened manifest `{}`: {error}",
            manifest_path.display()
        ))
    })?;
    validate_candidate_manifest_file(&manifest_path, &reopened_metadata)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if written_metadata.dev() != reopened_metadata.dev()
            || written_metadata.ino() != reopened_metadata.ino()
        {
            return Err(candidate_environment_unsafe(format!(
                "manifest `{}` changed identity before execution",
                manifest_path.display()
            )));
        }
    }
    let mut reopened_bytes = Vec::new();
    reopened.read_to_end(&mut reopened_bytes).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not read reopened manifest `{}`: {error}",
            manifest_path.display()
        ))
    })?;
    if reopened_bytes != canonical.as_bytes() {
        return Err(candidate_environment_unsafe(format!(
            "manifest `{}` changed bytes before execution",
            manifest_path.display()
        )));
    }
    #[cfg(unix)]
    let (dev, ino) = {
        use std::os::unix::fs::MetadataExt as _;
        (written_metadata.dev(), written_metadata.ino())
    };
    #[cfg(not(unix))]
    let (dev, ino) = (0, 0);
    Ok(CandidateManifestSidecar {
        path: manifest_path,
        bytes: canonical.into_bytes(),
        dev,
        ino,
    })
}

fn create_candidate_manifest_run(candidate_run_dir: &Path) -> Result<(), String> {
    let parent = candidate_run_dir.parent().ok_or_else(|| {
        candidate_environment_unsafe("candidate run directory has no state parent")
    })?;
    let canonical_parent = ensure_candidate_manifest_parent(parent)?;
    create_candidate_private_directory(candidate_run_dir).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not create exclusive run directory `{}`: {error}",
            candidate_run_dir.display()
        ))
    })?;
    let protect = validate_candidate_directory(candidate_run_dir, Some(0o700));
    let protected_identity = protect.and_then(|()| {
        let canonical_run = std::fs::canonicalize(candidate_run_dir).map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not resolve run directory `{}`: {error}",
                candidate_run_dir.display()
            ))
        })?;
        if canonical_run.parent() != Some(canonical_parent.as_path()) {
            return Err(candidate_environment_unsafe(format!(
                "run directory `{}` escaped manifest namespace `{}`",
                canonical_run.display(),
                canonical_parent.display()
            )));
        }
        Ok(())
    });
    if let Err(error) = protected_identity {
        return Err(format!(
            "{error}; preserving unbound run directory `{}`",
            candidate_run_dir.display()
        ));
    }
    Ok(())
}

fn candidate_environment_unsafe(detail: impl std::fmt::Display) -> String {
    format!("candidate_snapshot.environment_unsafe: {detail}")
}

#[derive(Debug, Clone)]
struct ProtectedStateRoot {
    canonical: PathBuf,
    dev: u64,
    ino: u64,
}

#[derive(Debug, Clone)]
struct ProtectedRunDirectory {
    path: PathBuf,
    canonical_parent: PathBuf,
    required_mode: u32,
    dev: u64,
    ino: u64,
    parent_dev: u64,
    parent_ino: u64,
}

impl ProtectedStateRoot {
    fn open(repo_root: &Path, state_dir: &Path, require_external: bool) -> Result<Self, String> {
        if !require_external {
            match std::fs::symlink_metadata(state_dir) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    create_candidate_private_directory(state_dir).map_err(|create_error| {
                        candidate_environment_unsafe(format!(
                            "could not create daemon state directory `{}`: {create_error}",
                            state_dir.display()
                        ))
                    })?;
                }
                Err(error) => {
                    return Err(candidate_environment_unsafe(format!(
                        "could not inspect daemon state directory `{}`: {error}",
                        state_dir.display()
                    )));
                }
            }
        }
        validate_candidate_directory(state_dir, None)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            let metadata = std::fs::symlink_metadata(state_dir).map_err(|error| {
                candidate_environment_unsafe(format!(
                    "could not inspect daemon state directory `{}`: {error}",
                    state_dir.display()
                ))
            })?;
            if metadata.uid() != effective_user_id() {
                return Err(candidate_environment_unsafe(format!(
                    "daemon state directory `{}` is not owned by the effective user",
                    state_dir.display()
                )));
            }
            if metadata.permissions().mode() & 0o022 != 0 {
                return Err(candidate_environment_unsafe(format!(
                    "daemon state directory `{}` is group- or world-writable",
                    state_dir.display()
                )));
            }
        }
        let canonical = std::fs::canonicalize(state_dir).map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not resolve daemon state directory `{}`: {error}",
                state_dir.display()
            ))
        })?;
        if require_external {
            let canonical_repo = std::fs::canonicalize(repo_root).map_err(|error| {
                candidate_environment_unsafe(format!(
                    "could not resolve candidate repository `{}`: {error}",
                    repo_root.display()
                ))
            })?;
            if canonical == canonical_repo || canonical.starts_with(&canonical_repo) {
                return Err(candidate_environment_unsafe(format!(
                    "daemon state directory `{}` is inside candidate repository `{}`",
                    canonical.display(),
                    canonical_repo.display()
                )));
            }
        }
        #[cfg(unix)]
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = std::fs::metadata(&canonical).map_err(|error| {
                candidate_environment_unsafe(format!(
                    "could not inspect canonical state root `{}`: {error}",
                    canonical.display()
                ))
            })?;
            (metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let (dev, ino) = (0, 0);
        Ok(Self {
            canonical,
            dev,
            ino,
        })
    }

    fn namespace(&self, name: &str, create: bool) -> Result<Option<PathBuf>, String> {
        self.verify()?;
        let path = self.canonical.join(name);
        if create {
            match create_candidate_private_directory(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(candidate_environment_unsafe(format!(
                        "could not create protected namespace `{}`: {error}",
                        path.display()
                    )));
                }
            }
        } else {
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(candidate_environment_unsafe(format!(
                        "could not inspect protected namespace `{}`: {error}",
                        path.display()
                    )));
                }
            }
        }
        validate_candidate_directory(&path, Some(0o700))?;
        let canonical = std::fs::canonicalize(&path).map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not resolve protected namespace `{}`: {error}",
                path.display()
            ))
        })?;
        if canonical.parent() != Some(self.canonical.as_path()) {
            return Err(candidate_environment_unsafe(format!(
                "protected namespace `{}` escaped state root `{}`",
                canonical.display(),
                self.canonical.display()
            )));
        }
        Ok(Some(canonical))
    }

    fn legacy_scratch_namespace(&self, create: bool) -> Result<Option<PathBuf>, String> {
        self.verify()?;
        let path = self.canonical.join("project-check-runs");
        if create {
            match create_candidate_private_directory(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(candidate_environment_unsafe(format!(
                        "could not create legacy scratch namespace `{}`: {error}",
                        path.display()
                    )));
                }
            }
        } else {
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(candidate_environment_unsafe(format!(
                        "could not inspect legacy scratch namespace `{}`: {error}",
                        path.display()
                    )));
                }
            }
        }
        validate_candidate_directory(&path, None)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                candidate_environment_unsafe(format!(
                    "could not inspect legacy scratch namespace `{}`: {error}",
                    path.display()
                ))
            })?;
            if metadata.permissions().mode() & 0o022 != 0 {
                return Err(candidate_environment_unsafe(format!(
                    "legacy scratch namespace `{}` is group- or world-writable",
                    path.display()
                )));
            }
        }
        let canonical = std::fs::canonicalize(&path).map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not resolve legacy scratch namespace `{}`: {error}",
                path.display()
            ))
        })?;
        if canonical.parent() != Some(self.canonical.as_path()) {
            return Err(candidate_environment_unsafe(format!(
                "legacy scratch namespace `{}` escaped state root `{}`",
                canonical.display(),
                self.canonical.display()
            )));
        }
        Ok(Some(canonical))
    }

    fn verify(&self) -> Result<(), String> {
        validate_candidate_directory(&self.canonical, None)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            let metadata = std::fs::metadata(&self.canonical).map_err(|error| {
                candidate_environment_unsafe(format!(
                    "could not inspect protected state root `{}`: {error}",
                    self.canonical.display()
                ))
            })?;
            if metadata.dev() != self.dev || metadata.ino() != self.ino {
                return Err(candidate_environment_unsafe(format!(
                    "protected state root `{}` changed file identity",
                    self.canonical.display()
                )));
            }
            if metadata.uid() != effective_user_id() || metadata.permissions().mode() & 0o022 != 0 {
                return Err(candidate_environment_unsafe(format!(
                    "protected state root `{}` changed owner or mode",
                    self.canonical.display()
                )));
            }
        }
        Ok(())
    }
}

impl ProtectedRunDirectory {
    fn capture(path: PathBuf, canonical_parent: &Path) -> Result<Self, String> {
        Self::capture_with_mode(path, canonical_parent, 0o700)
    }

    fn capture_with_mode(
        path: PathBuf,
        canonical_parent: &Path,
        required_mode: u32,
    ) -> Result<Self, String> {
        validate_candidate_directory(&path, Some(required_mode))?;
        let canonical = std::fs::canonicalize(&path).map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not resolve protected run `{}`: {error}",
                path.display()
            ))
        })?;
        if canonical.parent() != Some(canonical_parent) {
            return Err(candidate_environment_unsafe(format!(
                "protected run `{}` escaped namespace `{}`",
                canonical.display(),
                canonical_parent.display()
            )));
        }
        #[cfg(unix)]
        let (dev, ino, parent_dev, parent_ino) = {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = std::fs::metadata(&canonical).map_err(|error| {
                candidate_environment_unsafe(format!(
                    "could not inspect protected run `{}`: {error}",
                    canonical.display()
                ))
            })?;
            let parent_metadata = std::fs::metadata(canonical_parent).map_err(|error| {
                candidate_environment_unsafe(format!(
                    "could not inspect protected run parent `{}`: {error}",
                    canonical_parent.display()
                ))
            })?;
            (
                metadata.dev(),
                metadata.ino(),
                parent_metadata.dev(),
                parent_metadata.ino(),
            )
        };
        #[cfg(not(unix))]
        let (dev, ino, parent_dev, parent_ino) = (0, 0, 0, 0);
        Ok(Self {
            path: canonical,
            canonical_parent: canonical_parent.to_path_buf(),
            required_mode,
            dev,
            ino,
            parent_dev,
            parent_ino,
        })
    }

    fn verify(&self) -> Result<(), String> {
        validate_candidate_directory(&self.path, Some(self.required_mode))?;
        let canonical = std::fs::canonicalize(&self.path).map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not resolve protected run `{}` during cleanup: {error}",
                self.path.display()
            ))
        })?;
        if canonical.parent() != Some(self.canonical_parent.as_path()) {
            return Err(candidate_environment_unsafe(format!(
                "protected run `{}` escaped namespace `{}` during cleanup",
                canonical.display(),
                self.canonical_parent.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = std::fs::metadata(&canonical).map_err(|error| {
                candidate_environment_unsafe(format!(
                    "could not inspect protected run `{}` during cleanup: {error}",
                    canonical.display()
                ))
            })?;
            if metadata.dev() != self.dev || metadata.ino() != self.ino {
                return Err(candidate_environment_unsafe(format!(
                    "protected run `{}` changed file identity before cleanup",
                    canonical.display()
                )));
            }
        }
        Ok(())
    }

    fn quarantine(&self) -> Result<Self, String> {
        self.verify()?;
        let quarantine_name =
            unpredictable_project_check_run_name()?.replacen("run-", ".cleanup-", 1);
        let quarantine = self.canonical_parent.join(quarantine_name);
        std::fs::rename(&self.path, &quarantine).map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not atomically quarantine protected run `{}`: {error}",
                self.path.display()
            ))
        })?;
        let quarantined =
            Self::capture_with_mode(quarantine, &self.canonical_parent, self.required_mode)?;
        if quarantined.dev != self.dev || quarantined.ino != self.ino {
            return Err(candidate_environment_unsafe(format!(
                "quarantined run `{}` does not match the recorded file identity; preserving it",
                quarantined.path.display()
            )));
        }
        Ok(quarantined)
    }
}

fn unpredictable_project_check_run_name() -> Result<String, String> {
    let mut random = [0_u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not acquire unpredictable project-check run identity: {error}"
            ))
        })?;
    let mut name = String::with_capacity(4 + random.len() * 2);
    name.push_str("run-");
    for byte in random {
        use std::fmt::Write as _;
        write!(&mut name, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(name)
}

pub(crate) fn validate_candidate_state_dir(
    repo_root: &Path,
    state_dir: &Path,
) -> Result<PathBuf, String> {
    ProtectedStateRoot::open(repo_root, state_dir, true).map(|root| root.canonical)
}

fn ensure_candidate_manifest_parent(parent: &Path) -> Result<PathBuf, String> {
    match create_candidate_private_directory(parent) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(candidate_environment_unsafe(format!(
                "could not create manifest namespace `{}`: {error}",
                parent.display()
            )));
        }
    }
    validate_candidate_directory(parent, Some(0o700))?;
    std::fs::canonicalize(parent).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not resolve manifest namespace `{}`: {error}",
            parent.display()
        ))
    })
}

fn create_candidate_private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn validate_candidate_directory(path: &Path, required_mode: Option<u32>) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not inspect directory `{}`: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(candidate_environment_unsafe(format!(
            "path `{}` is not a real directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != effective_user_id() {
            return Err(candidate_environment_unsafe(format!(
                "directory `{}` is not owned by the effective user",
                path.display()
            )));
        }
        if required_mode.is_some_and(|mode| metadata.permissions().mode() & 0o777 != mode) {
            return Err(candidate_environment_unsafe(format!(
                "directory `{}` does not have required mode {:04o}",
                path.display(),
                required_mode.expect("required mode checked")
            )));
        }
    }
    #[cfg(not(unix))]
    let _ = required_mode;
    Ok(())
}

fn validate_candidate_manifest_file(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), String> {
    if !metadata.is_file() {
        return Err(candidate_environment_unsafe(format!(
            "manifest `{}` is not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != effective_user_id() {
            return Err(candidate_environment_unsafe(format!(
                "manifest `{}` is not owned by the effective user",
                path.display()
            )));
        }
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(candidate_environment_unsafe(format!(
                "manifest `{}` does not have required mode 0600",
                path.display()
            )));
        }
        if metadata.nlink() != 1 {
            return Err(candidate_environment_unsafe(format!(
                "manifest `{}` has an unsafe hard-link count",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn effective_user_id() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid(2) has no arguments or failure mode and only returns
    // the effective process identity used by filesystem permission checks.
    unsafe { geteuid() }
}

fn cleanup_candidate_manifest_run(candidate_run_dir: &Path) -> Result<(), String> {
    let Some(parent) = candidate_run_dir.parent() else {
        return Err(candidate_environment_unsafe(
            "candidate run directory has no cleanup parent",
        ));
    };
    match std::fs::symlink_metadata(parent) {
        Ok(_) => validate_candidate_directory(parent, Some(0o700))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(candidate_environment_unsafe(format!(
                "could not inspect manifest namespace `{}` during cleanup: {error}",
                parent.display()
            )));
        }
    }
    match std::fs::symlink_metadata(candidate_run_dir) {
        Ok(_) => validate_candidate_directory(candidate_run_dir, Some(0o700))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(candidate_environment_unsafe(format!(
                "could not inspect run directory `{}` during cleanup: {error}",
                candidate_run_dir.display()
            )));
        }
    }
    let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not resolve manifest namespace `{}` during cleanup: {error}",
            parent.display()
        ))
    })?;
    let canonical_run = std::fs::canonicalize(candidate_run_dir).map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not resolve run directory `{}` during cleanup: {error}",
            candidate_run_dir.display()
        ))
    })?;
    if canonical_run.parent() != Some(canonical_parent.as_path()) {
        return Err(candidate_environment_unsafe(format!(
            "run directory `{}` escaped manifest namespace `{}` during cleanup",
            canonical_run.display(),
            canonical_parent.display()
        )));
    }
    match std::fs::remove_dir_all(candidate_run_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "remove candidate manifest run `{}`: {error}",
            candidate_run_dir.display()
        )),
    }
}

fn cleanup_protected_candidate_manifest_run(
    protected: &ProtectedRunDirectory,
) -> Result<(), String> {
    cleanup_protected_candidate_manifest_run_with_after_quarantine(protected, |_| {})
}

fn cleanup_protected_candidate_manifest_run_with_after_quarantine(
    protected: &ProtectedRunDirectory,
    after_quarantine: impl FnOnce(&Path),
) -> Result<(), String> {
    let quarantine = protected.quarantine()?;
    after_quarantine(&quarantine.path);
    ensure_protected_original_absent(protected)?;
    quarantine.verify()?;
    remove_bound_protected_run(&quarantine)
}

fn ensure_protected_original_absent(protected: &ProtectedRunDirectory) -> Result<(), String> {
    match std::fs::symlink_metadata(&protected.path) {
        Ok(_) => Err(candidate_environment_unsafe(format!(
            "protected run path `{}` was replaced during cleanup; preserving both paths",
            protected.path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(candidate_environment_unsafe(format!(
            "could not inspect original protected run path `{}` during cleanup: {error}",
            protected.path.display()
        ))),
    }
}

#[cfg(target_os = "linux")]
fn remove_bound_protected_run(protected: &ProtectedRunDirectory) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    const O_DIRECTORY: core::ffi::c_int = 0x0001_0000;
    const O_NOFOLLOW: core::ffi::c_int = 0x0002_0000;
    const O_CLOEXEC: core::ffi::c_int = 0x0008_0000;
    const AT_REMOVEDIR: core::ffi::c_int = 0x0200;
    unsafe extern "C" {
        fn unlinkat(
            dirfd: core::ffi::c_int,
            pathname: *const core::ffi::c_char,
            flags: core::ffi::c_int,
        ) -> core::ffi::c_int;
    }

    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        .open(&protected.path)
        .map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not bind quarantined run `{}` for deletion: {error}",
                protected.path.display()
            ))
        })?;
    let metadata = directory.metadata().map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not inspect bound quarantined run `{}`: {error}",
            protected.path.display()
        ))
    })?;
    if metadata.dev() != protected.dev || metadata.ino() != protected.ino {
        return Err(candidate_environment_unsafe(format!(
            "quarantined run `{}` changed identity before bound deletion",
            protected.path.display()
        )));
    }
    let parent = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        .open(&protected.canonical_parent)
        .map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not bind protected namespace `{}` for deletion: {error}",
                protected.canonical_parent.display()
            ))
        })?;
    let parent_metadata = parent.metadata().map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not inspect bound protected namespace `{}`: {error}",
            protected.canonical_parent.display()
        ))
    })?;
    if parent_metadata.dev() != protected.parent_dev
        || parent_metadata.ino() != protected.parent_ino
    {
        return Err(candidate_environment_unsafe(format!(
            "protected namespace `{}` changed identity before bound deletion",
            protected.canonical_parent.display()
        )));
    }
    fn remove_contents_at(directory: &std::fs::File) -> Result<(), String> {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::MetadataExt as _;

        const O_DIRECTORY: core::ffi::c_int = 0x0001_0000;
        const O_NOFOLLOW: core::ffi::c_int = 0x0002_0000;
        const O_CLOEXEC: core::ffi::c_int = 0x0008_0000;
        const AT_REMOVEDIR: core::ffi::c_int = 0x0200;
        unsafe extern "C" {
            fn openat(
                dirfd: core::ffi::c_int,
                pathname: *const core::ffi::c_char,
                flags: core::ffi::c_int,
            ) -> core::ffi::c_int;
            fn unlinkat(
                dirfd: core::ffi::c_int,
                pathname: *const core::ffi::c_char,
                flags: core::ffi::c_int,
            ) -> core::ffi::c_int;
        }

        let descriptor_root = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let entries = std::fs::read_dir(&descriptor_root).map_err(|error| {
            candidate_environment_unsafe(format!(
                "could not enumerate bound run directory `{}`: {error}",
                descriptor_root.display()
            ))
        })?;
        for entry in entries {
            let name = entry
                .map_err(|error| {
                    candidate_environment_unsafe(format!(
                        "could not enumerate bound run directory `{}`: {error}",
                        descriptor_root.display()
                    ))
                })?
                .file_name();
            let name = CString::new(name.as_bytes()).map_err(|_| {
                candidate_environment_unsafe("protected run entry name contains NUL")
            })?;

            // SAFETY: directory is a live directory descriptor and name is a
            // live NUL-terminated single component. O_NOFOLLOW prevents a
            // swapped symlink from redirecting recursion.
            let child_fd = unsafe {
                openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC,
                )
            };
            if child_fd >= 0 {
                // SAFETY: openat returned a new owned descriptor on success.
                let child = unsafe { std::fs::File::from_raw_fd(child_fd) };
                let child_metadata = child.metadata().map_err(|error| {
                    candidate_environment_unsafe(format!(
                        "could not inspect bound nested run directory: {error}"
                    ))
                })?;
                remove_contents_at(&child)?;

                // Re-open through the held parent descriptor after recursion
                // and compare identity. A name substitution is preserved and
                // fails closed instead of being recursively deleted.
                let current_fd = unsafe {
                    openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC,
                    )
                };
                if current_fd < 0 {
                    return Err(candidate_environment_unsafe(format!(
                        "nested protected run directory changed before unlink: {}",
                        std::io::Error::last_os_error()
                    )));
                }
                // SAFETY: openat returned a new owned descriptor on success.
                let current = unsafe { std::fs::File::from_raw_fd(current_fd) };
                let current_metadata = current.metadata().map_err(|error| {
                    candidate_environment_unsafe(format!(
                        "could not re-inspect nested protected run directory: {error}"
                    ))
                })?;
                if current_metadata.dev() != child_metadata.dev()
                    || current_metadata.ino() != child_metadata.ino()
                {
                    return Err(candidate_environment_unsafe(
                        "nested protected run directory changed identity; preserving it",
                    ));
                }
                // SAFETY: directory and name are still live; AT_REMOVEDIR
                // removes only this directory entry and never follows it.
                if unsafe { unlinkat(directory.as_raw_fd(), name.as_ptr(), AT_REMOVEDIR) } != 0 {
                    return Err(candidate_environment_unsafe(format!(
                        "could not unlink bound nested run directory: {}",
                        std::io::Error::last_os_error()
                    )));
                }
            } else {
                // A non-directory (including a symlink) is removed relative
                // to the held directory descriptor. If it races into a
                // directory, unlinkat fails rather than traversing it.
                if unsafe { unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                    return Err(candidate_environment_unsafe(format!(
                        "could not unlink bound run entry: {}",
                        std::io::Error::last_os_error()
                    )));
                }
            }
        }
        Ok(())
    }

    remove_contents_at(&directory)?;
    // The held directory descriptor binds recursive traversal. Re-check the
    // namespace entry after traversal, then unlink it relative to the held
    // parent descriptor so a parent-path swap cannot redirect deletion.
    let current = std::fs::symlink_metadata(
        PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(
            protected
                .path
                .file_name()
                .expect("protected run has a name"),
        ),
    )
    .map_err(|error| {
        candidate_environment_unsafe(format!(
            "could not re-inspect quarantined run entry `{}`: {error}",
            protected.path.display()
        ))
    })?;
    if current.dev() != protected.dev || current.ino() != protected.ino {
        return Err(candidate_environment_unsafe(format!(
            "quarantined run `{}` changed identity before unlink; preserving it",
            protected.path.display()
        )));
    }
    let name = CString::new(
        protected
            .path
            .file_name()
            .expect("protected run has a name")
            .as_bytes(),
    )
    .map_err(|_| candidate_environment_unsafe("protected run name contains NUL"))?;
    // SAFETY: parent is a live directory descriptor and name is a live,
    // NUL-terminated single path component. AT_REMOVEDIR never follows it.
    if unsafe { unlinkat(parent.as_raw_fd(), name.as_ptr(), AT_REMOVEDIR) } != 0 {
        return Err(candidate_environment_unsafe(format!(
            "could not unlink bound quarantined run `{}`: {}",
            protected.path.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn remove_bound_protected_run(protected: &ProtectedRunDirectory) -> Result<(), String> {
    remove_bound_protected_run_with_after_verify(protected, |_| {})
}

#[cfg(not(target_os = "linux"))]
fn remove_bound_protected_run_with_after_verify(
    protected: &ProtectedRunDirectory,
    after_verify: impl FnOnce(&Path),
) -> Result<(), String> {
    protected.verify()?;
    after_verify(&protected.path);
    Err(candidate_environment_unsafe(format!(
        "atomic bound cleanup is unsupported on this platform; preserving protected run `{}`",
        protected.path.display()
    )))
}

fn ensure_candidate_parent(
    root: &Path,
    relative: &Path,
    create_missing: bool,
) -> Result<(), String> {
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut cursor = root.to_path_buf();
    for component in parent.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(format!(
                "candidate_snapshot.path_noncanonical: invalid path `{}`",
                relative.display()
            ));
        };
        cursor.push(component);
        match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "candidate_snapshot.symlink_traversal: parent `{}` is a symlink",
                    cursor.display()
                ));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "candidate_snapshot.path_conflict: parent `{}` is not a directory",
                    cursor.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_missing => {
                std::fs::create_dir(&cursor).map_err(|error| {
                    format!(
                        "candidate_snapshot.materialize_failed: create parent `{}`: {error}",
                        cursor.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "candidate_snapshot.materialize_failed: inspect parent `{}`: {error}",
                    cursor.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_candidate_mode(path: &Path, mode: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let permissions = match mode {
        "100644" => std::fs::Permissions::from_mode(0o644),
        "100755" => std::fs::Permissions::from_mode(0o755),
        _ => {
            return Err(format!(
                "candidate_snapshot.overlay_mode_unsupported: unsupported mode {mode:?}"
            ));
        }
    };
    std::fs::set_permissions(path, permissions).map_err(|error| {
        format!(
            "candidate_snapshot.materialize_failed: chmod `{}`: {error}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn set_candidate_mode(_path: &Path, mode: &str) -> Result<(), String> {
    if matches!(mode, "100644" | "100755") {
        Ok(())
    } else {
        Err(format!(
            "candidate_snapshot.overlay_mode_unsupported: unsupported mode {mode:?}"
        ))
    }
}

pub(crate) fn candidate_snapshot_check_context(
    sidecar: &CandidateManifestSidecar,
    manifest: &CandidateSnapshotManifest,
) -> CandidateSnapshotCheckContext {
    CandidateSnapshotCheckContext {
        manifest_path: sidecar.path.clone(),
        manifest_bytes: sidecar.bytes.clone(),
        manifest_dev: sidecar.dev,
        manifest_ino: sidecar.ino,
        candidate_kind: manifest.candidate.kind().to_string(),
        snapshot_digest: manifest.candidate.snapshot_digest().to_string(),
        candidate_tree_oid: manifest.candidate.tree_oid().to_string(),
        candidate_sha: match &manifest.candidate {
            CandidateSnapshot::Tree { commit_sha, .. } => Some(commit_sha.clone()),
            CandidateSnapshot::Index { .. } | CandidateSnapshot::Overlay { .. } => None,
        },
        manifest_digest: manifest.manifest_digest.clone(),
        comparison_base_sha: manifest.comparison_base.commit_sha.clone(),
    }
}

// ── CGLS-26: warm shared witness target dir ──────────────────────────────

/// Feature gate. Default OFF: `CARGOLESS_WITNESS_WARM_TARGET` unset or not
/// exactly "1" ⇒ cold per-run behavior, byte-identical to before.
fn warm_target_enabled() -> bool {
    std::env::var("CARGOLESS_WITNESS_WARM_TARGET").is_ok_and(|v| v == "1")
}

/// Compute the warm-dir key = short sha256 of (schema, toolchain, lockhash).
/// Base_sha is intentionally absent (see `resolve_warm_target`). Returns
/// `None` (⇒ cold) if the toolchain is unresolvable or Cargo.lock is missing
/// / unreadable — a warm dir keyed on unknown inputs could serve a
/// schema-incompatible tree, so fail closed.
fn warm_target_key(scratch_root: &Path) -> Option<String> {
    let toolchain = warm_toolchain_id()?;
    // Cargo.lock lives at the scratch (base) root; a missing lock ⇒ cold.
    let lock_path = scratch_root.join("Cargo.lock");
    let lock_bytes = std::fs::read(&lock_path).ok()?;
    let lockhash = sha256_hex(&lock_bytes);
    let material = format!("{WARM_TARGET_SCHEMA_TAG}\0{toolchain}\0{lockhash}");
    let full = sha256_hex(material.as_bytes());
    Some(full[..16].to_string())
}

/// Resolve a stable toolchain identity. Prefer `RUSTUP_TOOLCHAIN` (set in the
/// serve deploy env); else a bounded hash of `rustc -vV`. `None` if neither
/// is obtainable.
fn warm_toolchain_id() -> Option<String> {
    if let Ok(tc) = std::env::var("RUSTUP_TOOLCHAIN") {
        if !tc.trim().is_empty() {
            return Some(tc);
        }
    }
    let out = Command::new("rustc").arg("-vV").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(sha256_hex(&out.stdout)[..16].to_string())
}

/// Best-effort GC: keep the newest [`WARM_TARGET_KEEP`] keyed dirs under
/// `<state_dir>/witness-target-warm/`, remove older ones. Never blocks or
/// fails the compile — a leaked dir is a disk cost, not a correctness bug.
///
/// Two in-use protections (removing a LIVE target dir mid-compile would be
/// CGLS-24 by another road): `active` (the caller's own dir) is skipped
/// unconditionally, and every other candidate is removed only after ITS
/// `.witness-lock` flock is acquired non-blocking — contended = some other
/// compile owns it = skip this round.
fn prune_warm_target_dirs(state_dir: &Path, active: &Path) {
    let root = state_dir.join("witness-target-warm");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let path = e.path();
            // Recency = the `.last-used` stamp (rewritten every warm hit;
            // rewriting a file does NOT bump the parent dir's mtime, so the
            // dir mtime alone would under-count reuse). Dir mtime is the
            // fallback for dirs predating the stamp.
            let m = std::fs::metadata(path.join(".last-used"))
                .and_then(|m| m.modified())
                .or_else(|_| e.metadata().and_then(|m| m.modified()))
                .ok()?;
            Some((m, path))
        })
        .collect();
    if dirs.len() <= WARM_TARGET_KEEP {
        return;
    }
    // Newest first; drop everything past the keep count.
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in dirs.into_iter().skip(WARM_TARGET_KEEP) {
        if path == active {
            continue;
        }
        match WarmFlock::acquire_nb(&path.join(".witness-lock")) {
            // Holding the lock: nobody is compiling in there; safe to remove.
            // The flock dies with the guard whether or not removal succeeds.
            Ok(Some(_guard)) => {
                let _ = std::fs::remove_dir_all(&path);
            }
            // Contended or unreadable: in use / indeterminate — leak it, a
            // later prune with more evidence gets another chance.
            Ok(None) | Err(_) => {}
        }
    }
}

/// Headroom multiple required to keep using the warm dir: free space must be
/// at least `WARM_DISK_HEADROOM_X` times the dir's current size. 1× would be
/// the naive "it fits" test, but the dir is about to GROW during the compile
/// it is protecting, and a `Cargo.lock` change can transiently add a whole
/// second key before prune reclaims the old one.
const WARM_DISK_HEADROOM_X: u64 = 2;

/// Below this size a warm dir is not worth protecting and the ratio check is
/// skipped outright. 1 GiB: two orders of magnitude under the ~16 GiB live
/// witness dirs this rung exists for, and far above any unit-test fixture or
/// freshly-created key. See `warm_dir_disk_pressure` for why a bare ratio
/// without this floor misfires on a disk-pressured CI runner.
const WARM_DISK_MIN_INTERESTING_BYTES: u64 = 1 << 30;

/// `None` ⇒ enough headroom, keep the warm dir. `Some(reason)` ⇒ go cold.
///
/// Fails **OPEN** (returns `None`) when free space or dir size can't be
/// measured. That is the opposite polarity from the rest of the ladder, and
/// deliberate: an unmeasurable disk is not evidence of a full one, and this
/// rung is a resource optimisation. Every rung above guards CORRECTNESS
/// (clobber ⇒ false RED), so those fail closed; blanking the cache because
/// `df` is unavailable would trade a real cost for a hypothetical one.
/// Genuine exhaustion still surfaces — cargo reports ENOSPC and the compile
/// reds honestly.
fn warm_dir_disk_pressure(warm_dir: &Path) -> Option<String> {
    let used = dir_size_bytes(warm_dir)?;
    // Below the floor there is no cache worth protecting, so skip the check
    // entirely — including the `df` subprocess.
    //
    // The floor is load-bearing, not a micro-optimisation. Without it the
    // ratio alone misfires on a small dir sitting on a nearly-full volume:
    // 2 x 4 KiB is trivially unavailable at 99% used, so a first-run warm dir
    // would go cold on a disk with gigabytes free. That is exactly what the
    // CI runner is (`ci.yml` header: "a disk-pressured Forgejo runner"), and
    // it turned the CGLS-26 warm-target unit tests RED on the first push of
    // this rung. The hazard being guarded is a MULTI-GIGABYTE target dir
    // (~16 GiB live on witness-b); anything this side of the floor cannot
    // fill a volume no matter how the ratio comes out.
    let floor = WARM_DISK_MIN_INTERESTING_BYTES;
    if used < floor {
        return None;
    }
    let free = free_bytes_at(warm_dir)?;
    let need = used.saturating_mul(WARM_DISK_HEADROOM_X);
    if free < need {
        return Some(format!(
            "disk-pressure:free={}MiB,warm={}MiB,need={}MiB",
            free >> 20,
            used >> 20,
            need >> 20
        ));
    }
    None
}

/// Free bytes on the filesystem holding `path`, via `df -Pk`.
///
/// `df` rather than `statvfs(2)`: this crate carries no `libc` dependency, and
/// the `statvfs` struct layout is platform-specific — a hand-rolled `extern
/// "C"` binding with a wrong field offset would return a plausible-looking
/// wrong number and silently mis-gate. A subprocess is measurably slower and
/// completely unambiguous, and this runs once per witness compile (minutes),
/// not per file. Same precedent as `warm_toolchain_id`'s `rustc -vV`.
///
/// `-P` is POSIX output: exactly one line per filesystem, so a long device
/// name cannot wrap and shift the column index (verified on the deploy image,
/// whose Longhorn device paths are 53 chars).
fn free_bytes_at(path: &Path) -> Option<u64> {
    let out = Command::new("df").arg("-Pk").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Field 3 (0-indexed) of the data line is 1024-byte blocks available.
    let kb: u64 = text
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()?;
    Some(kb.saturating_mul(1024))
}

/// Recursive apparent size of `dir` in bytes. `None` if the walk hits an
/// error, so the caller fails open rather than acting on a partial total —
/// an undercount here would defeat the guard exactly when the tree is
/// largest. Symlinks are not followed (`symlink_metadata`), so a link into
/// the repo cannot inflate the number or escape the subtree.
fn dir_size_bytes(dir: &Path) -> Option<u64> {
    let mut total: u64 = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).ok()? {
            let entry = entry.ok()?;
            let meta = std::fs::symlink_metadata(entry.path()).ok()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Some(total)
}

/// Collapse a warm-obs `reason` to a BOUNDED label for the counter.
///
/// Several reasons embed an errno or byte counts (`mkdir:<io error>`,
/// `disk-pressure:free=…,warm=…`). Keying the map on the raw string would let
/// it grow without bound and would fragment the very signal an alert needs to
/// sum. Keying on the whole prefix before the first `:` would over-collapse
/// the opposite way — `contended:in-proc` and `contended:flock` are DIFFERENT
/// interlocks (in-process CAS vs cross-process flock) and which one fired is
/// the diagnostic.
///
/// So: keep `contended:*` two-deep, collapse everything else to its head.
fn warm_obs_bucket(reason: &str) -> &str {
    if reason.starts_with("contended:") {
        return reason;
    }
    reason.split([':', ',']).next().unwrap_or(reason)
}

/// RAII holder for the resolved warm dir + its two locks. Dropping it releases
/// the cross-process flock *before* publishing the in-process key as idle.
/// That ordering is load-bearing: another thread may acquire the in-process
/// flag as soon as it becomes false, so exposing false while the prior flock
/// is still held creates a spurious `contended:flock` cold fallback.
struct WarmTargetGuard {
    dir: PathBuf,
    in_proc: InProcWarmGuard,
    flock: Option<WarmFlock>,
}

impl Drop for WarmTargetGuard {
    fn drop(&mut self) {
        // `WarmFlock::drop` performs an explicit LOCK_UN and then closes the
        // fd. Complete that transition before a racing resolver can observe
        // the in-process key as available.
        if let Some(flock) = self.flock.take() {
            drop(flock);
        }
        self.in_proc.release();
    }
}

/// Clears the per-key busy flag on drop (normal return OR panic-unwind), so a
/// finished/aborted witness never leaves the warm key permanently marked
/// busy. Holds the `Arc` so the flag outlives the daemon map entry if pruned.
struct InProcWarmGuard {
    busy: Arc<std::sync::atomic::AtomicBool>,
    released: bool,
}

impl InProcWarmGuard {
    fn release(&mut self) {
        if !self.released {
            self.busy.store(false, std::sync::atomic::Ordering::Release);
            self.released = true;
        }
    }
}

impl Drop for InProcWarmGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Cross-process advisory lock on `<warm>/.witness-lock` via `flock(2)`,
/// `LOCK_NB`. Insurance for a future multi-daemon topology; today serve is
/// single-replica. Holds the open `File` (fd) for the compile; closing it at
/// Drop releases the lock.
struct WarmFlock {
    file: std::fs::File,
}

impl WarmFlock {
    fn acquire_nb(lock_path: &Path) -> std::io::Result<Option<WarmFlock>> {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path)?;
        // LOCK_EX | LOCK_NB = 2 | 4 = 6 on Linux.
        const LOCK_EX_NB: i32 = 6;
        // SAFETY: fd is valid for the call; flock is a well-defined syscall.
        let rc = unsafe {
            unsafe extern "C" {
                fn flock(fd: i32, operation: i32) -> i32;
            }
            flock(file.as_raw_fd(), LOCK_EX_NB)
        };
        if rc == 0 {
            Ok(Some(WarmFlock { file }))
        } else {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                // EWOULDBLOCK/EAGAIN ⇒ contended, not an error. 11 on Linux
                // (the deploy target), 35 on macOS (dev/test machines).
                Some(11) | Some(35) => Ok(None),
                _ => Err(err),
            }
        }
    }
}

impl Drop for WarmFlock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // LOCK_UN = 8 on Linux and macOS. Closing the fd is also specified to
        // release flock, but doing that implicitly left the lock transition
        // unobservable and produced rare post-drop contention in CI. Make the
        // release synchronous and explicit; close remains the final backstop.
        const LOCK_UN: i32 = 8;
        // SAFETY: the File owns a valid fd for the duration of this Drop call.
        let _ = unsafe {
            unsafe extern "C" {
                fn flock(fd: i32, operation: i32) -> i32;
            }
            flock(self.file.as_raw_fd(), LOCK_UN)
        };
    }
}

fn materialize_overlay_files_from_root(
    source_root: &Path,
    target_root: &Path,
    files: &[(String, String)],
) -> Result<(), String> {
    for (path, content) in files {
        let path = Path::new(path);
        let abs = if path.is_absolute() {
            let rel = path.strip_prefix(source_root).map_err(|_| {
                format!(
                    "overlay path `{}` escapes analysis_root `{}`",
                    path.display(),
                    source_root.display()
                )
            })?;
            target_root.join(rel)
        } else {
            target_root.join(safe_repo_relative_path(&path.to_string_lossy())?)
        };
        if !abs.starts_with(target_root) {
            return Err(format!(
                "overlay path `{}` escapes analysis_root `{}`",
                abs.display(),
                target_root.display()
            ));
        }
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "could not create overlay parent `{}`: {e}",
                    parent.display()
                )
            })?;
        }
        std::fs::write(&abs, content)
            .map_err(|e| format!("could not materialize overlay `{}`: {e}", abs.display()))?;
    }
    Ok(())
}

/// Output of a bounded child run: `Command::output()` shape with the
/// streams pre-decoded (lossy) — every consumer here wants strings.
/// Debug is load-bearing for `unwrap_err` in the deadline tests.
#[derive(Debug)]
struct BoundedOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// `Command::output()` with a deadline: spawn with piped stdout/stderr
/// drained by two reader threads, poll `try_wait` (~50ms) until `timeout`
/// elapses, then kill the child and fail. Mirrors the proven
/// spawn/deadline/kill + reader-thread pattern in
/// `cargoless_core::project_checks::check_command`. Every git op here
/// runs under `sync_lock`, so an unbounded wait on one wedged `git
/// fetch` would hold the lock — and every push ack behind it — forever.
fn run_command_bounded(cmd: &mut Command, timeout: Duration) -> Result<BoundedOutput, String> {
    // `Command::output()` nulls stdin; preserve that so a credential
    // prompt can never wedge the child on terminal input.
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start: {e}"))?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || read_pipe(&mut stdout));
    let err_thread = std::thread::spawn(move || read_pipe(&mut stderr));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                // Join the readers briefly, then detach: a grandchild
                // (ssh / git-remote-https) that inherited the pipe write
                // end can hold it open past the kill, and the bound on
                // THIS call is the contract. Detached threads exit when
                // the pipe finally closes.
                let join_deadline = Instant::now() + Duration::from_millis(250);
                while !(out_thread.is_finished() && err_thread.is_finished())
                    && Instant::now() < join_deadline
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                return Err(format!("timed out after {}ms", timeout.as_millis()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not wait: {e}"));
            }
        }
    };
    Ok(BoundedOutput {
        status,
        stdout: out_thread.join().unwrap_or_default(),
        stderr: err_thread.join().unwrap_or_default(),
    })
}

fn read_pipe(pipe: &mut Option<impl Read>) -> String {
    let mut out = String::new();
    if let Some(pipe) = pipe {
        let _ = pipe.read_to_string(&mut out);
    }
    out
}

/// Deadline for one git invocation: `CARGOLESS_GIT_TIMEOUT_MS` overrides
/// everything when set (ops escape hatch); otherwise network fetches get
/// 120s and local-only git ops 60s.
fn git_timeout(args: &[&str]) -> Duration {
    let env_ms = std::env::var("CARGOLESS_GIT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok());
    git_timeout_from(env_ms, args)
}

fn git_timeout_from(env_ms: Option<u64>, args: &[&str]) -> Duration {
    if let Some(ms) = env_ms {
        return Duration::from_millis(ms);
    }
    if matches!(args.first(), Some(&"fetch")) {
        Duration::from_millis(120_000)
    } else {
        Duration::from_millis(60_000)
    }
}

fn git_command(root: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    for selector in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
        "GIT_QUARANTINE_PATH",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        cmd.env_remove(selector);
    }
    for (name, _) in std::env::vars_os() {
        let name_lossy = name.to_string_lossy();
        if name_lossy.starts_with("GIT_CONFIG_KEY_") || name_lossy.starts_with("GIT_CONFIG_VALUE_")
        {
            cmd.env_remove(name);
        }
    }
    cmd.env("LC_ALL", "C")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1");
    cmd
}

fn run_git(root: &Path, args: &[&str]) -> Result<(), String> {
    let out = run_command_bounded(&mut git_command(root, args), git_timeout(args))
        .map_err(|e| format!("git {:?} in `{}` {e}", args, root.display()))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "git {:?} in `{}` exited {:?}: {}",
        args,
        root.display(),
        out.status.code(),
        out.stderr.trim()
    ))
}

fn run_git_success(root: &Path, args: &[&str]) -> Result<bool, String> {
    let out = run_command_bounded(&mut git_command(root, args), git_timeout(args))
        .map_err(|e| format!("git {:?} in `{}` {e}", args, root.display()))?;
    Ok(out.status.success())
}

fn git_stdout(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = run_command_bounded(&mut git_command(root, args), git_timeout(args))
        .map_err(|e| format!("git {:?} in `{}` {e}", args, root.display()))?;
    if out.status.success() {
        return Ok(out.stdout.trim().to_string());
    }
    Err(format!(
        "git {:?} in `{}` exited {:?}: {}",
        args,
        root.display(),
        out.status.code(),
        out.stderr.trim()
    ))
}

#[cfg(test)]
mod git_bounds_tests {
    use super::*;

    #[test]
    fn git_command_removes_hostile_repository_and_object_selectors() {
        let command = git_command(Path::new("/authoritative/repository"), &["status"]);
        let env = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
            "GIT_QUARANTINE_PATH",
            "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            assert_eq!(
                env.get(key),
                Some(&None),
                "{key} must not override the explicit repository authority"
            );
        }
        assert_eq!(
            env.get("GIT_NO_REPLACE_OBJECTS"),
            Some(&Some("1".to_string()))
        );
        assert_eq!(env.get("GIT_NO_LAZY_FETCH"), Some(&Some("1".to_string())));
        assert_eq!(env.get("LC_ALL"), Some(&Some("C".to_string())));
    }

    #[test]
    fn run_command_bounded_kills_on_deadline() {
        let start = Instant::now();
        let err = run_command_bounded(Command::new("sleep").arg("30"), Duration::from_millis(300))
            .unwrap_err();
        assert!(err.contains("timed out after 300ms"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "deadline must bound the wait far under the child's 30s sleep; took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn run_command_bounded_captures_output_within_deadline() {
        let out = run_command_bounded(Command::new("echo").arg("bounded"), Duration::from_secs(30))
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.trim(), "bounded");
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn run_git_fails_fast_without_consuming_the_timeout() {
        let start = Instant::now();
        let err = run_git(
            Path::new("/cargoless-no-such-dir"),
            &["definitely-not-a-git-subcommand"],
        )
        .unwrap_err();
        assert!(!err.contains("timed out"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "a fast git failure must not wait out the deadline; took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn retry_with_sleeps_retries_then_succeeds() {
        let mut calls = 0;
        let result = retry_with_sleeps(&[Duration::ZERO, Duration::ZERO], |attempt| {
            calls += 1;
            if attempt < 2 {
                Err(format!("transient {attempt}"))
            } else {
                Ok(attempt)
            }
        });
        assert_eq!(result.unwrap(), 2);
        assert_eq!(calls, 3, "fail, fail, succeed = 3 invocations");
    }

    #[test]
    fn retry_with_sleeps_propagates_last_error_after_exhaustion() {
        let mut calls = 0;
        let err = retry_with_sleeps(&[Duration::ZERO], |_| -> Result<(), String> {
            calls += 1;
            Err(format!("fail {calls}"))
        })
        .unwrap_err();
        assert_eq!(calls, 2, "one retry sleep = two attempts");
        assert_eq!(err, "fail 2");
    }

    #[test]
    fn git_timeout_env_overrides_then_fetch_and_local_defaults_split() {
        assert_eq!(
            git_timeout_from(Some(5_000), &["fetch", "origin", "main"]),
            Duration::from_millis(5_000)
        );
        assert_eq!(
            git_timeout_from(None, &["fetch", "--prune", "origin", "main"]),
            Duration::from_millis(120_000)
        );
        assert_eq!(
            git_timeout_from(None, &["reset", "--hard", "origin/main"]),
            Duration::from_millis(60_000)
        );
    }

    #[test]
    fn cleanup_failure_is_authority_specific() {
        let result = finish_project_check_run(
            ProjectCheckAuthority::CandidateSnapshot,
            Ok::<_, String>("verified"),
            Err("scratch cleanup denied".to_string()),
            Ok(()),
        )
        .unwrap_err();
        assert!(
            result.contains("candidate_snapshot.cleanup_failed"),
            "{result}"
        );
        assert!(result.contains("scratch cleanup denied"), "{result}");

        for authority in [
            ProjectCheckAuthority::ExactGit,
            ProjectCheckAuthority::LegacyOverlay,
        ] {
            assert_eq!(
                finish_project_check_run(
                    authority,
                    Ok::<_, String>("verified"),
                    Err("scratch cleanup denied".to_string()),
                    Ok(()),
                )
                .unwrap(),
                "verified",
                "noncandidate cleanup is operational telemetry and must not replace the verdict"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::Path;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use cargoless_core::batch::{BatchMember, BatchProvenance, BatchReport, BatchVerdict};
    use cargoless_core::transport::http::{HttpClient, HttpServer};
    use cargoless_core::transport::{
        AllowAll, AttemptContext, BatchCheckRequest, CargoSubcommand, PushOverlayOptions,
        TransportClient, VerdictService,
    };
    use cargoless_core::{
        CandidateSnapshot, CandidateSnapshotManifest, GitObjectFormat, GitTreeRef,
        OverlayOperation, OverlayPayload, SnapshotEntry, compute_candidate_tree_oid,
        compute_manifest_digest, compute_snapshot_digest, parse_and_validate_manifest_json,
        sha256_hex,
    };

    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cargoless-serveapi-{label}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    const CANDIDATE_SNAPSHOT_GOLDEN: &str = r#"{
      "schema":"cargoless-candidate-snapshot/1",
      "git_object_format":"sha1",
      "comparison_base":{"commit_sha":"de16c5f7dd233165813ffa72719869e3181c554b","tree_oid":"4b825dc642cb6eb9a060e54bf8d69288fbee4904"},
      "candidate":{"kind":"overlay","base":{"commit_sha":"de16c5f7dd233165813ffa72719869e3181c554b","tree_oid":"4b825dc642cb6eb9a060e54bf8d69288fbee4904"},"tree_oid":"08d60034cad9ce340c4d42748bf0bc1b2e34d830","entry_count":2,"entries":[{"path":"empty.bin","mode":"100644","blob_oid":"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391","size":0,"sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"},{"path":"script.sh","mode":"100755","blob_oid":"9766475a4185a151dc9d56d614ffb9aaea3bfd42","size":3,"sha256":"dc51b8c96c2d745df3bd5590d990230a482fd247123599548e0632fdbf97fc22"}],"snapshot_digest":"sha256:365cc276607bc3209bd7346f8de4f765e42e68bba8fdaf1b22687b6a169118ed","operation_count":2,"operations":[{"op":"upsert","path":"empty.bin","mode":"100644","blob_oid":"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391","size":0,"sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","payload":{"encoding":"base64","data":""}},{"op":"upsert","path":"script.sh","mode":"100755","blob_oid":"9766475a4185a151dc9d56d614ffb9aaea3bfd42","size":3,"sha256":"dc51b8c96c2d745df3bd5590d990230a482fd247123599548e0632fdbf97fc22","payload":{"encoding":"base64","data":"b2sK"}}]},
      "manifest_digest":"sha256:a363a22a9ab3317a8d7d616ecb4ac66ef7d0f2d7dd46d8a1010f44a601b8377c"
    }"#;

    fn candidate_snapshot_golden() -> CandidateSnapshotManifest {
        parse_and_validate_manifest_json(CANDIDATE_SNAPSHOT_GOLDEN).unwrap()
    }

    #[test]
    fn candidate_check_context_maps_overlay_index_and_tree_identity_without_synthesis() {
        let manifest_path = PathBuf::from("/state/candidate-snapshots/run-1/manifest.json");
        let sidecar = CandidateManifestSidecar {
            path: manifest_path.clone(),
            bytes: CANDIDATE_SNAPSHOT_GOLDEN.as_bytes().to_vec(),
            dev: 7,
            ino: 11,
        };
        let overlay = candidate_snapshot_golden();
        let overlay_context = candidate_snapshot_check_context(&sidecar, &overlay);
        assert_eq!(overlay_context.manifest_path, manifest_path);
        assert_eq!(overlay_context.manifest_bytes, sidecar.bytes);
        assert_eq!(overlay_context.manifest_dev, 7);
        assert_eq!(overlay_context.manifest_ino, 11);
        assert_eq!(overlay_context.candidate_kind, "overlay");
        assert_eq!(
            overlay_context.snapshot_digest,
            overlay.candidate.snapshot_digest()
        );
        assert_eq!(
            overlay_context.candidate_tree_oid,
            overlay.candidate.tree_oid()
        );
        assert_eq!(overlay_context.candidate_sha, None);

        let entries = overlay.candidate.entries().to_vec();
        let snapshot_digest = overlay.candidate.snapshot_digest().to_string();
        let tree_oid = overlay.candidate.tree_oid().to_string();
        let mut index = overlay.clone();
        index.candidate = CandidateSnapshot::Index {
            base: overlay.comparison_base.clone(),
            tree_oid: tree_oid.clone(),
            entry_count: entries.len() as u64,
            entries: entries.clone(),
            snapshot_digest: snapshot_digest.clone(),
        };
        let index_context = candidate_snapshot_check_context(&sidecar, &index);
        assert_eq!(index_context.candidate_kind, "index");
        assert_eq!(index_context.candidate_sha, None);

        let commit_sha = "1".repeat(40);
        let mut tree = overlay;
        tree.candidate = CandidateSnapshot::Tree {
            commit_sha: commit_sha.clone(),
            tree_oid,
            entry_count: entries.len() as u64,
            entries,
            snapshot_digest,
        };
        let tree_context = candidate_snapshot_check_context(&sidecar, &tree);
        assert_eq!(tree_context.candidate_kind, "tree");
        assert_eq!(
            tree_context.candidate_sha.as_deref(),
            Some(commit_sha.as_str())
        );
    }

    fn attempt_context(id: &str, attempt_number: u32) -> AttemptContext {
        AttemptContext {
            request_id: cargoless_core::outcome::RequestId::new("request-1").unwrap(),
            attempt_id: cargoless_core::outcome::AttemptId::new(id).unwrap(),
            trace_id: cargoless_core::outcome::TraceId::new("0123456789abcdef").unwrap(),
            previous_attempt_id: (attempt_number > 1)
                .then(|| cargoless_core::outcome::AttemptId::new("attempt-one").unwrap()),
            attempt_number,
            maximum_attempts: 3,
            retry_after_ms: 5000,
        }
    }

    #[test]
    fn candidate_result_evidence_is_exact_and_retry_attempt_scoped() {
        let state_dir = temp_root("candidate-result-evidence-retries");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let first = attempt_context("attempt-one", 1);
        let second = attempt_context("attempt-two", 2);
        let subject = || Subject::Overlay {
            repository: text_v3("/repo"),
            worktree_key: text_v3("/client/candidate"),
            base_ref: text_v3("base-ref"),
            base_sha: text_v3("base-sha"),
            overlay_digest: text_v3("overlay-digest"),
            changed_files_digest: text_v3("changed-files-digest"),
            check_plan_digest: text_v3("check-plan-digest"),
        };
        api.begin_outcome_v3(
            &first,
            Surface::Overlay,
            subject(),
            Phase::Queued,
            "first candidate attempt",
        );
        api.begin_outcome_v3(
            &second,
            Surface::Overlay,
            subject(),
            Phase::Queued,
            "retried candidate attempt",
        );

        let tree_oid = "1".repeat(40);
        let candidate_sha = "2".repeat(40);
        let comparison_base_sha = "3".repeat(40);
        let snapshot_digest = format!("sha256:{}", "4".repeat(64));
        let manifest_digest = format!("sha256:{}", "5".repeat(64));
        let result = |policy_hash: &str| {
            format!(
                "{{\n  \"schema\": \"cargoless.check-result/v2\",\n  \"check_id\": \"candidate-policy\",\n  \"status\": \"passed\",\n  \"summary\": \"candidate policy satisfied\",\n  \"subject\": {{\n    \"candidate_kind\": \"tree\",\n    \"candidate_snapshot_digest\": \"{snapshot_digest}\",\n    \"candidate_tree_oid\": \"{tree_oid}\",\n    \"candidate_sha\": \"{candidate_sha}\",\n    \"comparison_base_sha\": \"{comparison_base_sha}\",\n    \"manifest_digest\": \"{manifest_digest}\",\n    \"engine\": \"migration-burndown\",\n    \"engine_version\": \"1\",\n    \"policy_hash\": \"{policy_hash}\"\n  }},\n  \"findings\": []\n}}\n"
            )
            .into_bytes()
        };
        let first_bytes = result("policy-first");
        let second_bytes = result("policy-second");
        let candidate = |execution_id| CandidateVerdictIdentity {
            manifest_digest: manifest_digest.clone(),
            snapshot_digest: snapshot_digest.clone(),
            tree_oid: tree_oid.clone(),
            execution_id,
        };

        api.publish_attributed_with_candidate_checks(
            Path::new("/client/candidate"),
            crate::statusfile::VerdictPayload::green(),
            Some(comparison_base_sha.clone()),
            Some(candidate(1)),
            false,
            vec!["candidate-policy".to_string()],
            vec![VerifiedProjectCheckEvidence {
                check_id: "candidate-policy".to_string(),
                bytes: first_bytes.clone(),
            }],
            Some(first.clone()),
        );
        api.publish_attributed_with_candidate_checks(
            Path::new("/client/candidate"),
            crate::statusfile::VerdictPayload::green(),
            Some(comparison_base_sha),
            Some(candidate(2)),
            false,
            vec!["candidate-policy".to_string()],
            vec![VerifiedProjectCheckEvidence {
                check_id: "candidate-policy".to_string(),
                bytes: second_bytes.clone(),
            }],
            Some(second.clone()),
        );

        assert_eq!(
            api.get_evidence_v3(&first.attempt_id, "project-check-result-001.json")
                .unwrap(),
            first_bytes
        );
        assert_eq!(
            api.get_evidence_v3(&second.attempt_id, "project-check-result-001.json")
                .unwrap(),
            second_bytes
        );
        assert_ne!(
            api.get_evidence_v3(&first.attempt_id, "project-check-result-001.json"),
            api.get_evidence_v3(&second.attempt_id, "project-check-result-001.json"),
            "a retry must never overwrite the predecessor's result evidence"
        );
        let retried = api.get_outcome_v3(&second.attempt_id).unwrap();
        assert!(retried.relations.iter().any(|relation| {
            relation.kind == RelationKind::RetriedFrom
                && relation.attempt_id.as_ref() == Some(&first.attempt_id)
        }));

        let reopened = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        assert_eq!(
            reopened
                .get_evidence_v3(&first.attempt_id, "project-check-result-001.json")
                .unwrap(),
            first_bytes
        );
        assert_eq!(
            reopened
                .get_evidence_v3(&second.attempt_id, "project-check-result-001.json")
                .unwrap(),
            second_bytes
        );
        let first_json: serde_json::Value = serde_json::from_slice(
            &reopened
                .get_evidence_v3(&first.attempt_id, "project-check-result-001.json")
                .unwrap(),
        )
        .unwrap();
        let result_subject = first_json["subject"].as_object().unwrap();
        for field in [
            "candidate_kind",
            "candidate_snapshot_digest",
            "candidate_tree_oid",
            "candidate_sha",
            "comparison_base_sha",
            "manifest_digest",
            "engine",
            "engine_version",
            "policy_hash",
        ] {
            assert!(result_subject.contains_key(field), "missing {field}");
        }
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn ordinary_overlay_rejection_preserves_the_exact_reason() {
        let ack = rejected_push(
            "/workspace/tf-multiverse",
            "git fetch origin local-only-sha failed: upload-pack: not our ref",
        );

        assert!(!ack.accepted);
        assert_eq!(ack.reject_http_status, Some(409));
        assert_eq!(
            ack.reject_body.as_deref(),
            Some("git fetch origin local-only-sha failed: upload-pack: not our ref")
        );
    }

    #[test]
    fn outcome_v3_keeps_retries_distinct_and_classes_ra_storm_as_analyzer_pathology() {
        let state_dir = temp_root("outcome-v3");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let files = vec![("src/lib.rs".to_string(), "pub fn broken() {}".to_string())];
        let options = |attempt_id: &str, attempt_number: u32| PushOverlayOptions {
            base_sha: Some("same-commit".into()),
            changed_files: Some(vec!["src/lib.rs".into()]),
            semantic: Some(attempt_context(attempt_id, attempt_number)),
            ..PushOverlayOptions::default()
        };

        assert!(
            api.push_overlay_with_options(
                "/client/wt",
                "origin/main",
                &files,
                None,
                Some(&options("attempt-1", 1)),
            )
            .accepted
        );
        assert!(
            api.push_overlay_with_options(
                "/client/wt",
                "origin/main",
                &files,
                None,
                Some(&options("attempt-2", 2)),
            )
            .accepted
        );
        assert_eq!(
            poisoned(&api.pushed).get("/client/wt").unwrap().len(),
            2,
            "same commit retries remain distinct execution attempts"
        );

        let first = api.take_overlay_for("/client/wt").expect("first attempt");
        api.record_push_attribution("/client/wt", &first);
        let attribution = api
            .take_push_attribution("/client/wt")
            .expect("attempt attribution");
        let context = attribution.semantic.clone().expect("v3 identity");
        api.record_ra_evidence_v3(
            Some(&context),
            RaStderrSnapshot {
                process_generation: 7,
                pid: Some(4242),
                total_lines: 1_500,
                error_lines: 1_500,
                suppressed_lines: 1_499,
                overflow_fingerprints: 0,
                fingerprints: vec![cargoless_core::analyzer::RaStderrFingerprint {
                    fingerprint: "storm-fingerprint".into(),
                    count: 1_500,
                    level: "error".into(),
                    sample: "ERROR inference diagnostic in desugared expr".into(),
                }],
                tail: vec!["ERROR inference diagnostic in desugared expr".into()],
                stack_captures: vec![b"thread apply all bt".to_vec()],
            },
        );
        api.publish_attributed_with_checks(
            Path::new("/client/wt"),
            crate::statusfile::VerdictPayload::red(1),
            attribution.base_sha,
            false,
            Vec::new(),
            Some(context.clone()),
        );

        let first_outcome = api
            .get_outcome_v3(&context.attempt_id)
            .expect("terminal exact attempt");
        match &first_outcome.conclusion {
            Conclusion::Indeterminate {
                cause:
                    cargoless_core::outcome::IndeterminateCause::AnalyzerPathology {
                        component,
                        signature,
                        repeated_events,
                    },
                summary,
                ..
            } => {
                assert_eq!(*component, OutcomeComponent::RustAnalyzer);
                assert_eq!(signature.as_str(), "storm-fingerprint");
                assert_eq!(*repeated_events, 1_500);
                assert!(
                    summary.as_str().contains("analyzer pathology"),
                    "the semantic conclusion must name the analyzer pathology"
                );
            }
            other => panic!("expected analyzer pathology, got {other:?}"),
        }
        assert_eq!(
            first_outcome.reaction.state,
            cargoless_core::outcome::CheckState::Error
        );
        assert_eq!(
            first_outcome.reaction.code.as_str(),
            "indeterminate.analyzer_pathology"
        );
        assert!(matches!(
            api.get_outcome_v3(&cargoless_core::outcome::AttemptId::new("attempt-2").unwrap())
                .unwrap()
                .conclusion,
            Conclusion::Pending { .. }
        ));
        assert!(
            state_dir
                .join("evidence-v3/attempt-1/ra-summary.json")
                .is_file()
        );
        assert!(
            state_dir
                .join("evidence-v3/attempt-1/stack-001.txt")
                .is_file()
        );

        let reopened = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        assert_eq!(
            reopened
                .get_outcome_v3(&context.attempt_id)
                .expect("durable outcome after process-state loss"),
            first_outcome
        );
        let metrics = api.outcome_metrics_v3().unwrap();
        assert_eq!(metrics["ra_storm_outcomes"], 1);
        assert_eq!(metrics["reactions_by_state"]["error"], 1);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn outcome_v3_classes_one_attempt_local_ra_error_as_analyzer_pathology() {
        let state_dir = temp_root("outcome-v3-one-ra-error");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let files = vec![("src/lib.rs".to_string(), "pub fn broken() {}".to_string())];
        let options = PushOverlayOptions {
            base_sha: Some("same-commit".into()),
            changed_files: Some(vec!["src/lib.rs".into()]),
            semantic: Some(attempt_context("attempt-one-ra-error", 1)),
            ..PushOverlayOptions::default()
        };
        assert!(
            api.push_overlay_with_options(
                "/client/wt",
                "origin/main",
                &files,
                None,
                Some(&options),
            )
            .accepted
        );
        let pushed = api.take_overlay_for("/client/wt").expect("attempt");
        api.record_push_attribution("/client/wt", &pushed);
        let attribution = api
            .take_push_attribution("/client/wt")
            .expect("attempt attribution");
        let context = attribution.semantic.clone().expect("v3 identity");
        api.record_ra_evidence_v3(
            Some(&context),
            RaStderrSnapshot {
                process_generation: 8,
                pid: Some(4343),
                total_lines: 1,
                error_lines: 1,
                fingerprints: vec![cargoless_core::analyzer::RaStderrFingerprint {
                    fingerprint: "attempt-error-fingerprint".into(),
                    count: 1,
                    level: "error".into(),
                    sample: "ERROR inference diagnostic in desugared expr".into(),
                }],
                tail: vec!["ERROR inference diagnostic in desugared expr".into()],
                ..RaStderrSnapshot::default()
            },
        );
        api.publish_attributed_with_checks(
            Path::new("/client/wt"),
            crate::statusfile::VerdictPayload::unknown("ra_native_attempt_stderr_error"),
            attribution.base_sha,
            false,
            Vec::new(),
            Some(context.clone()),
        );

        let outcome = api
            .get_outcome_v3(&context.attempt_id)
            .expect("terminal exact attempt");
        match outcome.conclusion {
            Conclusion::Indeterminate {
                cause:
                    cargoless_core::outcome::IndeterminateCause::AnalyzerPathology {
                        component,
                        signature,
                        repeated_events,
                    },
                ..
            } => {
                assert_eq!(component, OutcomeComponent::RustAnalyzer);
                assert_eq!(signature.as_str(), "attempt-error-fingerprint");
                assert_eq!(repeated_events, 1);
            }
            other => panic!("expected attempt-local analyzer pathology, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn evidence_failure_preserves_code_red_but_marks_bundle_unavailable() {
        let api = ServeVerdictState::new();
        let files = vec![("src/lib.rs".to_string(), "broken".to_string())];
        let options = PushOverlayOptions {
            base_sha: Some("same-commit".into()),
            changed_files: Some(vec!["src/lib.rs".into()]),
            semantic: Some(attempt_context("attempt-no-store", 1)),
            ..PushOverlayOptions::default()
        };
        assert!(
            api.push_overlay_with_options(
                "/client/wt",
                "origin/main",
                &files,
                None,
                Some(&options),
            )
            .accepted
        );
        let pushed = api.take_overlay_for("/client/wt").unwrap();
        api.record_push_attribution("/client/wt", &pushed);
        let attribution = api.take_push_attribution("/client/wt").unwrap();
        let context = attribution.semantic.clone().unwrap();
        api.publish_attributed_with_checks(
            Path::new("/client/wt"),
            crate::statusfile::VerdictPayload::red(1),
            attribution.base_sha,
            false,
            Vec::new(),
            Some(context.clone()),
        );
        let outcome = api.get_outcome_v3(&context.attempt_id).unwrap();
        assert_eq!(
            outcome.reaction.state,
            cargoless_core::outcome::CheckState::Failure,
            "an evidence-store outage must not erase a real code failure"
        );
        let Conclusion::Failed {
            evidence, summary, ..
        } = outcome.conclusion
        else {
            panic!("code red was not retained");
        };
        assert!(matches!(
            evidence.availability,
            EvidenceAvailability::Unavailable { .. }
        ));
        assert!(summary.as_str().contains("code failure retained"));
        assert_eq!(
            api.outcome_metrics_v3().unwrap()["evidence_persist_failures"],
            1
        );
    }

    #[test]
    fn analyzer_blind_result_requires_a_compiler_witness_by_typed_code() {
        let state_dir = temp_root("outcome-v3-blind");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let options = PushOverlayOptions {
            base_sha: Some("same-commit".into()),
            changed_files: Some(vec!["src/view.rs".into()]),
            semantic: Some(attempt_context("attempt-blind", 1)),
            ..PushOverlayOptions::default()
        };
        assert!(
            api.push_overlay_with_options(
                "/client/wt",
                "origin/main",
                &[("src/view.rs".into(), "view! { <div/> }".into())],
                None,
                Some(&options),
            )
            .accepted
        );
        let pushed = api.take_overlay_for("/client/wt").unwrap();
        api.record_push_attribution("/client/wt", &pushed);
        let attribution = api.take_push_attribution("/client/wt").unwrap();
        let context = attribution.semantic.clone().unwrap();
        api.publish_attributed_with_checks(
            Path::new("/client/wt"),
            crate::statusfile::VerdictPayload::unknown(
                "ra_native_timer_settled_no_flycheck_activity",
            ),
            attribution.base_sha,
            true,
            Vec::new(),
            Some(context.clone()),
        );

        let outcome = api.get_outcome_v3(&context.attempt_id).unwrap();
        assert!(matches!(
            outcome.conclusion,
            Conclusion::Indeterminate {
                cause: IndeterminateCause::CompilerWitnessRequired { .. },
                retry: RetryDirective::NewInputRequired,
                ..
            }
        ));
        assert_eq!(
            outcome.reaction.state,
            cargoless_core::outcome::CheckState::Error
        );
        assert_eq!(
            outcome.reaction.code.as_str(),
            "indeterminate.compiler_witness_required"
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn analyzer_unattributed_error_requires_a_compiler_witness_by_typed_code() {
        let state_dir = temp_root("outcome-v3-unattributed-error");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let options = PushOverlayOptions {
            base_sha: Some("same-commit".into()),
            changed_files: Some(vec!["src/view.rs".into()]),
            semantic: Some(attempt_context("attempt-unattributed-error", 1)),
            ..PushOverlayOptions::default()
        };
        assert!(
            api.push_overlay_with_options(
                "/client/wt",
                "origin/main",
                &[("src/view.rs".into(), "view! { <div/> }".into())],
                None,
                Some(&options),
            )
            .accepted
        );
        let pushed = api.take_overlay_for("/client/wt").unwrap();
        api.record_push_attribution("/client/wt", &pushed);
        let attribution = api.take_push_attribution("/client/wt").unwrap();
        let context = attribution.semantic.clone().unwrap();
        api.publish_attributed_with_checks(
            Path::new("/client/wt"),
            crate::statusfile::VerdictPayload::unknown("ra_native_unattributed_error"),
            attribution.base_sha,
            false,
            Vec::new(),
            Some(context.clone()),
        );

        let outcome = api.get_outcome_v3(&context.attempt_id).unwrap();
        assert!(matches!(
            outcome.conclusion,
            Conclusion::Indeterminate {
                cause: IndeterminateCause::CompilerWitnessRequired { .. },
                retry: RetryDirective::NewInputRequired,
                ..
            }
        ));
        assert_eq!(
            outcome.reaction.state,
            cargoless_core::outcome::CheckState::Error
        );
        assert_eq!(
            outcome.reaction.code.as_str(),
            "indeterminate.compiler_witness_required"
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn batch_outcome_v3_is_typed_persisted_and_retry_bounded() {
        let state_dir = temp_root("batch-outcome-v3");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let request = |attempt_number| {
            let mut request = BatchCheckRequest::new("physical-batch-42", "origin/dev");
            request.members = vec![BatchMember::new("member-a")];
            request.options.semantic = Some(attempt_context(
                &format!("batch-attempt-{attempt_number}"),
                attempt_number,
            ));
            request
        };

        let first = api.submit_batch_v3(&request(1)).unwrap();
        assert_eq!(first.surface, Surface::Batch);
        assert!(matches!(
            first.conclusion,
            Conclusion::Indeterminate {
                cause: IndeterminateCause::AttributionUnavailable { .. },
                ..
            }
        ));
        assert_eq!(
            first.reaction.state,
            cargoless_core::outcome::CheckState::Pending
        );
        assert!(
            api.get_evidence_v3(&first.attempt_id, "batch-report.json")
                .is_some()
        );

        let final_attempt = api.submit_batch_v3(&request(3)).unwrap();
        assert_eq!(
            final_attempt.reaction.state,
            cargoless_core::outcome::CheckState::Error
        );
        assert_eq!(
            final_attempt.reaction.code.as_str(),
            "indeterminate.attribution"
        );
        let reopened = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        assert_eq!(
            reopened
                .get_outcome_v3(&final_attempt.attempt_id)
                .unwrap()
                .reaction,
            final_attempt.reaction
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    fn git_capture(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn git_hash_blob(root: &Path, bytes: &[u8]) -> String {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["hash-object", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.as_mut().unwrap().write_all(bytes).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git hash-object --stdin failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn overlay_manifest_with_delete_empty_executable_and_binary(
        root: &Path,
    ) -> CandidateSnapshotManifest {
        let base_commit = git_capture(root, &["rev-parse", "HEAD"]);
        let base_tree = git_capture(root, &["rev-parse", "HEAD^{tree}"]);
        let removed_blob = git_capture(root, &["rev-parse", "HEAD:remove.txt"]);
        let binary = [0_u8, 0xff, 0x80, b'\n'];
        let script = b"#!/bin/sh\nexit 0\n";
        let entries = vec![
            SnapshotEntry {
                path: "binary.bin".into(),
                mode: "100644".into(),
                blob_oid: git_hash_blob(root, &binary),
                size: binary.len() as u64,
                sha256: sha256_hex(&binary),
            },
            SnapshotEntry {
                path: "empty.bin".into(),
                mode: "100644".into(),
                blob_oid: git_hash_blob(root, b""),
                size: 0,
                sha256: sha256_hex(b""),
            },
            SnapshotEntry {
                path: "script.sh".into(),
                mode: "100755".into(),
                blob_oid: git_hash_blob(root, script),
                size: script.len() as u64,
                sha256: sha256_hex(script),
            },
        ];
        let operations = vec![
            OverlayOperation::Upsert {
                path: "binary.bin".into(),
                mode: "100644".into(),
                blob_oid: entries[0].blob_oid.clone(),
                size: binary.len() as u64,
                sha256: entries[0].sha256.clone(),
                payload: OverlayPayload {
                    encoding: "base64".into(),
                    data: "AP+ACg==".into(),
                },
            },
            OverlayOperation::Upsert {
                path: "empty.bin".into(),
                mode: "100644".into(),
                blob_oid: entries[1].blob_oid.clone(),
                size: 0,
                sha256: entries[1].sha256.clone(),
                payload: OverlayPayload {
                    encoding: "base64".into(),
                    data: String::new(),
                },
            },
            OverlayOperation::Delete {
                path: "remove.txt".into(),
                base_mode: "100644".into(),
                base_blob_oid: removed_blob,
            },
            OverlayOperation::Upsert {
                path: "script.sh".into(),
                mode: "100755".into(),
                blob_oid: entries[2].blob_oid.clone(),
                size: script.len() as u64,
                sha256: entries[2].sha256.clone(),
                payload: OverlayPayload {
                    encoding: "base64".into(),
                    data: "IyEvYmluL3NoCmV4aXQgMAo=".into(),
                },
            },
        ];
        let base = GitTreeRef {
            commit_sha: base_commit,
            tree_oid: base_tree,
        };
        let mut manifest = CandidateSnapshotManifest {
            schema: "cargoless-candidate-snapshot/1".into(),
            git_object_format: GitObjectFormat::Sha1,
            comparison_base: base.clone(),
            candidate: CandidateSnapshot::Overlay {
                base,
                tree_oid: "0".repeat(40),
                entry_count: entries.len() as u64,
                entries,
                snapshot_digest: format!("sha256:{}", "0".repeat(64)),
                operation_count: operations.len() as u64,
                operations,
            },
            manifest_digest: format!("sha256:{}", "0".repeat(64)),
        };
        let tree_oid = compute_candidate_tree_oid(&manifest).unwrap();
        let CandidateSnapshot::Overlay {
            tree_oid: advertised_tree,
            ..
        } = &mut manifest.candidate
        else {
            unreachable!()
        };
        *advertised_tree = tree_oid;
        let snapshot_digest = compute_snapshot_digest(&manifest).unwrap();
        let CandidateSnapshot::Overlay {
            snapshot_digest: advertised_snapshot,
            ..
        } = &mut manifest.candidate
        else {
            unreachable!()
        };
        *advertised_snapshot = snapshot_digest;
        manifest.manifest_digest = compute_manifest_digest(&manifest).unwrap();
        manifest
    }

    /// Build a repo whose `origin` remote advertises only `main`, but which
    /// locally holds a SECOND commit reachable from no remote ref — exactly
    /// the shape of a push pinning `base_sha` to a non-tip dev commit the
    /// remote will not serve by hash.
    fn repo_with_unreferenced_local_commit(label: &str) -> (PathBuf, PathBuf, String, String) {
        let root = temp_root(label);
        let remote = temp_root(&format!("{label}-remote"));
        git(&remote, &["init", "--bare"]);
        git(&root, &["init"]);
        git(&root, &["config", "user.email", "c@example.invalid"]);
        git(&root, &["config", "user.name", "Cargoless Test"]);
        git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );

        std::fs::write(root.join("marker.txt"), "base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        // Publish ONLY this first commit as origin/main (the advertised tip).
        git(&root, &["push", "origin", "HEAD:main"]);
        let base_sha = git_capture(&root, &["rev-parse", "HEAD"]);

        // A second commit that exists locally but is pushed to NO remote ref.
        std::fs::write(root.join("marker.txt"), "advanced\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "advanced (local only)"]);
        let local_only_sha = git_capture(&root, &["rev-parse", "HEAD"]);

        // Reset the worktree back to base so a later reset-to(local_only_sha)
        // is observable via the marker file content.
        git(&root, &["reset", "--hard", &base_sha]);
        (root, remote, base_sha, local_only_sha)
    }

    #[test]
    fn is_commit_hash_only_matches_full_object_hashes() {
        // 40-hex SHA-1 and 64-hex SHA-256 are the trusted shapes.
        assert!(is_commit_hash(&"a".repeat(40)));
        assert!(is_commit_hash(&"0".repeat(64)));
        assert!(is_commit_hash("e0f8f9396117d2214946199d0b5e63adb9ec6132"));
        // Symbolic refs and abbreviations must NOT short-circuit the fetch.
        assert!(!is_commit_hash("origin/dev"));
        assert!(!is_commit_hash("dev"));
        assert!(!is_commit_hash("HEAD"));
        assert!(!is_commit_hash("e0f8f93")); // abbreviated
        assert!(!is_commit_hash(&"a".repeat(41))); // wrong length
        assert!(!is_commit_hash(&"g".repeat(40))); // non-hex
        assert!(!is_commit_hash(""));
    }

    #[test]
    fn sync_analysis_root_uses_local_base_without_fetching_unadvertised_sha() {
        // THE production bug (serve-shard `not our ref`): a base_sha that is
        // present locally but advertised by no remote ref. The old code ran
        // `git fetch origin <sha>`, which a real Forgejo/GitHub upload-pack
        // rejects; here `origin` is a bare repo that likewise has never seen
        // the commit. The fix must short-circuit on the local object and
        // reset to it WITHOUT consulting the remote.
        let (root, remote, base_sha, local_only_sha) =
            repo_with_unreferenced_local_commit("sync-local-base");

        // Precondition: the unadvertised commit is genuinely local-only.
        assert!(local_commit_exists(&root, &local_only_sha));
        assert!(is_commit_hash(&local_only_sha));

        // Sync to the local-only SHA. Pre-fix this errored with the remote's
        // equivalent of `upload-pack: not our ref`; post-fix it must succeed
        // off the local object store alone.
        sync_analysis_root(&root, &local_only_sha)
            .unwrap_or_else(|e| panic!("sync to local-only base must not fetch: {e}"));

        // And it must have actually moved the tree to that commit.
        assert_eq!(git_capture(&root, &["rev-parse", "HEAD"]), local_only_sha);
        assert_eq!(
            std::fs::read_to_string(root.join("marker.txt")).unwrap(),
            "advanced\n",
            "tree must be reset to the local-only base content"
        );
        assert_ne!(local_only_sha, base_sha);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote);
    }

    #[test]
    fn batch_check_without_shared_analysis_root_is_indeterminate_per_member() {
        let api = ServeVerdictState::new();
        let mut request = BatchCheckRequest::new("batch-no-root", "origin/main");
        request.members = vec![
            BatchMember {
                worktree: "/client/a".into(),
                files: vec![("src/a.rs".into(), "pub fn a() {}".into())],
                changed_files: vec!["src/a.rs".into()],
            },
            BatchMember {
                worktree: "/client/b".into(),
                files: vec![("src/b.rs".into(), "pub fn b() {}".into())],
                changed_files: vec!["src/b.rs".into()],
            },
        ];

        let report = api.batch_check(&request);

        assert_eq!(report.verdict, BatchVerdict::Indeterminate);
        assert_eq!(report.members.len(), 2);
        assert_eq!(report.combined_checks, 0);
        assert_eq!(report.solo_checks, 0);
        for member in report.members {
            assert_eq!(member.verdict, BatchVerdict::Indeterminate);
            assert_eq!(member.provenance, BatchProvenance::Indeterminate);
            assert!(
                member.diagnostics[0]
                    .message
                    .contains("requires a shared analysis_root")
            );
        }
    }

    #[test]
    fn batch_member_mapping_keeps_repo_relative_paths_inside_analysis_root() {
        let root = temp_root("batch-map");
        let members = vec![BatchMember {
            worktree: "/client/a".into(),
            files: vec![("src/a.rs".into(), "pub fn a() {}".into())],
            changed_files: vec!["src/a.rs".into()],
        }];

        let mapped = map_batch_members(&root, true, &members).unwrap();

        assert_eq!(mapped[0].worktree, "/client/a");
        assert_eq!(mapped[0].changed_files, vec!["src/a.rs".to_string()]);
        assert_eq!(
            mapped[0].files,
            vec![(
                root.join("src/a.rs").to_string_lossy().into_owned(),
                "pub fn a() {}".to_string(),
            )]
        );

        let escaping = vec![BatchMember {
            worktree: "/client/b".into(),
            files: vec![("../outside.rs".into(), "bad".into())],
            changed_files: vec![],
        }];
        assert!(
            map_batch_members(&root, true, &escaping)
                .unwrap_err()
                .contains("escapes repo root")
        );
    }

    #[test]
    fn batch_overlay_union_dedupes_same_content_and_rejects_conflicts() {
        let same = vec![
            BatchMember {
                worktree: "a".into(),
                files: vec![("src/lib.rs".into(), "same".into())],
                changed_files: vec![],
            },
            BatchMember {
                worktree: "b".into(),
                files: vec![("src/lib.rs".into(), "same".into())],
                changed_files: vec![],
            },
        ];
        assert_eq!(
            union_overlay_files(&same).unwrap(),
            vec![("src/lib.rs".into(), "same".into())]
        );

        let conflicting = vec![
            BatchMember {
                worktree: "a".into(),
                files: vec![("src/lib.rs".into(), "one".into())],
                changed_files: vec![],
            },
            BatchMember {
                worktree: "b".into(),
                files: vec![("src/lib.rs".into(), "two".into())],
                changed_files: vec![],
            },
        ];
        assert!(
            union_overlay_files(&conflicting)
                .unwrap_err()
                .contains("different content")
        );
    }

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    struct BatchProject {
        root: PathBuf,
        remote: PathBuf,
    }

    impl Drop for BatchProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
            let _ = std::fs::remove_dir_all(&self.remote);
        }
    }

    fn setup_batch_project(label: &str) -> BatchProject {
        let root = temp_root(label);
        let remote = temp_root(&format!("{label}-remote"));

        git(&remote, &["init", "--bare"]);
        git(&root, &["init"]);
        git(
            &root,
            &["config", "user.email", "cargoless@example.invalid"],
        );
        git(&root, &["config", "user.name", "Cargoless Test"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn base() {}\n").unwrap();
        std::fs::write(
            root.join("cargoless.checks.yaml"),
            r#"
version: 1
checks:
  - id: no-fail-token
    kind: forbidden_patterns
    inputs: ["src/*.rs"]
    # The HTTP batch tests exercise attribution and corun policy, not the
    # engine's timeout policy.  Forty no-corun members intentionally repeat
    # this scan serially; give each tiny synthetic scan enough headroom that a
    # loaded CI volume cannot turn this fixture into a project-check timeout.
    timeout_ms: 12000
    patterns:
      - code: batch.fail_token
        literal: FAIL_BATCH
        message: failing batch token present
"#,
        )
        .unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&root, &["push", "-u", "origin", "HEAD:main"]);

        BatchProject { root, remote }
    }

    fn batch_member(name: &str, rel_path: &str, content: &str) -> BatchMember {
        BatchMember {
            worktree: format!("/client/{name}"),
            files: vec![(rel_path.to_string(), content.to_string())],
            changed_files: vec![rel_path.to_string()],
        }
    }

    fn batch_request(batch_id: &str, root: &Path, members: Vec<BatchMember>) -> BatchCheckRequest {
        let mut request = BatchCheckRequest::new(batch_id, "origin/main");
        request.options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some(root.to_string_lossy().into_owned()),
            base_sha: None,
            source_ref: None,
            source_sha: None,
            comparison_base_sha: None,
            candidate_snapshot: None,
            changed_files: None,
            gate: false,
            check_ids: None,
            semantic: None,
        };
        request.members = members;
        request
    }

    fn http_batch_check_with_client(remote: &str, request: &BatchCheckRequest) -> BatchReport {
        let client = HttpClient::new(remote).expect("client for batch_check remote");
        client.batch_check(request).expect("remote batch_check")
    }

    fn http_batch_check(request: &BatchCheckRequest) -> BatchReport {
        let api = Arc::new(ServeVerdictState::new());
        let srv = HttpServer::bind(
            "127.0.0.1:0",
            Arc::clone(&api) as Arc<dyn VerdictService>,
            Arc::new(AllowAll),
        )
        .expect("bind ephemeral");
        let remote = format!("http://{}", srv.addr());
        let mut last_err = None;
        let report = (0..20)
            .find_map(|_| {
                let client = match HttpClient::new(&remote) {
                    Ok(client) => client,
                    Err(err) => {
                        last_err = Some(err.to_string());
                        std::thread::sleep(Duration::from_millis(25));
                        return None;
                    }
                };
                match client.batch_check(request) {
                    Ok(report) => Some(report),
                    Err(err) => {
                        last_err = Some(err.to_string());
                        std::thread::sleep(Duration::from_millis(25));
                        None
                    }
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "remote batch_check did not become ready: {}",
                    last_err.unwrap_or_else(|| "no attempts made".into())
                )
            });
        drop(srv);
        report
    }

    fn assert_overlay_paths_cleaned(root: &Path, rel_paths: &[String]) {
        for rel_path in rel_paths {
            assert!(
                !root.join(rel_path).exists(),
                "overlay path `{rel_path}` should be removed after batch_check cleanup"
            );
        }
    }

    fn member_result<'a>(
        report: &'a BatchReport,
        worktree: &str,
    ) -> &'a cargoless_core::batch::BatchMemberResult {
        report
            .members
            .iter()
            .find(|member| member.worktree == worktree)
            .unwrap_or_else(|| panic!("missing batch result for {worktree}"))
    }

    fn test_coalescer() -> BatchCoalescer {
        BatchCoalescer {
            state: Mutex::new(BatchCoalescerState::default()),
            cv: Condvar::new(),
            after_fast_path: None,
            config: BatchCoalesceConfig {
                // Small cold-start grace (50ms): lets simultaneously-launched
                // same-key submitters enqueue before the leader drains, so they
                // coalesce into ONE batch (the production default is 250ms; the
                // shorter window keeps tests fast). Steady-state coalescing
                // rides the inflight gate and needs no grace.
                coalesce_grace: Duration::from_millis(50),
                max_wait: Duration::from_millis(300),
                max_members: 40,
                global_inflight_limit: 1,
                eject_cooldown_rounds: 1,
            },
        }
    }

    fn test_batch_key(name: &str) -> BatchCoalesceKey {
        BatchCoalesceKey {
            coalesce_key: name.to_string(),
            base_ref: "origin/main".into(),
            analysis_root: Some("/workspace/repo".into()),
            repo_relative: true,
            check_profile: "None".into(),
            corun: true,
            gate: false,
            check_ids: None,
        }
    }

    fn coalescer_request(batch_id: &str, member: &str) -> BatchCheckRequest {
        let mut request = BatchCheckRequest::new(batch_id, "origin/main");
        request.options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some("/workspace/repo".into()),
            base_sha: None,
            source_ref: None,
            source_sha: None,
            comparison_base_sha: None,
            candidate_snapshot: None,
            changed_files: None,
            gate: false,
            check_ids: None,
            semantic: None,
        };
        request.members = vec![BatchMember::new(member)];
        request
    }

    fn green_report_for(request: &BatchCheckRequest) -> BatchReport {
        BatchReport {
            batch_id: request.batch_id.clone(),
            verdict: BatchVerdict::Green,
            members: request
                .members
                .iter()
                .map(|member| cargoless_core::batch::BatchMemberResult {
                    worktree: member.worktree.clone(),
                    verdict: BatchVerdict::Green,
                    provenance: BatchProvenance::CombinedGreen,
                    diagnostics: Vec::new(),
                    duration_ms: 1,
                    ran_check_ids: Vec::new(),
                })
                .collect(),
            combined_checks: 1,
            solo_checks: 0,
            duration_ms: 1,
            queue_wait_ms: 0,
            executed_members: request.members.len() as u32,
            executed_batch_id: Some(request.batch_id.clone()),
        }
    }

    #[test]
    fn batch_coalescer_groups_same_key_requests() {
        // Two simultaneously-released same-key submitters must COALESCE into a
        // single physical run. `test_coalescer()` carries a 50ms cold-start
        // grace, so the elected leader waits briefly for the follower to enqueue
        // before draining — both land in ONE group. (No barrier inside `run`:
        // the follower coalesces in as a non-leader and never invokes `run`, so
        // a 2-party rendezvous there would deadlock. The grace window is what
        // guarantees the coalescing the test asserts.)
        let coalescer = Arc::new(test_coalescer());
        let key = test_batch_key("same");
        let start = Arc::new(Barrier::new(2));
        let runs = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let mut handles = Vec::new();

        for (batch_id, member) in [("batch-a", "member-a"), ("batch-b", "member-b")] {
            let coalescer = Arc::clone(&coalescer);
            let key = key.clone();
            let start = Arc::clone(&start);
            let runs = Arc::clone(&runs);
            let request = coalescer_request(batch_id, member);
            handles.push(thread::spawn(move || {
                start.wait();
                coalescer.submit(key, &request, |combined| {
                    poisoned(&runs).push(
                        combined
                            .members
                            .iter()
                            .map(|member| member.worktree.clone())
                            .collect(),
                    );
                    green_report_for(combined)
                })
            }));
        }

        let reports: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("coalescer thread"))
            .collect();

        // Exactly ONE physical run, containing BOTH members (the coalescing).
        let runs_snapshot = poisoned(&runs).clone();
        assert_eq!(
            runs_snapshot.len(),
            1,
            "the cold-start grace must coalesce both same-key submitters into ONE run; got {runs_snapshot:?}"
        );
        let mut ran_members = runs_snapshot[0].clone();
        ran_members.sort();
        assert_eq!(ran_members, vec!["member-a", "member-b"]);
        // Each submitter still gets its own member sliced back.
        assert!(
            reports
                .iter()
                .any(|report| report.batch_id == "batch-a"
                    && report.members[0].worktree == "member-a")
        );
        assert!(
            reports
                .iter()
                .any(|report| report.batch_id == "batch-b"
                    && report.members[0].worktree == "member-b")
        );
    }

    #[test]
    fn batch_coalescer_keeps_different_keys_separate() {
        let coalescer = Arc::new(test_coalescer());
        let start = Arc::new(Barrier::new(2));
        let runs = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let mut handles = Vec::new();

        for (key_name, batch_id, member) in [
            ("key-a", "batch-a", "member-a"),
            ("key-b", "batch-b", "member-b"),
        ] {
            let coalescer = Arc::clone(&coalescer);
            let key = test_batch_key(key_name);
            let start = Arc::clone(&start);
            let runs = Arc::clone(&runs);
            let request = coalescer_request(batch_id, member);
            handles.push(thread::spawn(move || {
                start.wait();
                coalescer.submit(key, &request, |combined| {
                    poisoned(&runs).push(
                        combined
                            .members
                            .iter()
                            .map(|member| member.worktree.clone())
                            .collect(),
                    );
                    green_report_for(combined)
                })
            }));
        }

        for handle in handles {
            handle.join().expect("coalescer thread");
        }
        let mut runs = poisoned(&runs).clone();
        runs.sort();
        assert_eq!(runs, vec![vec!["member-a"], vec!["member-b"]]);
    }

    #[test]
    fn batch_coalescer_splits_at_max_members_without_losing_waiters() {
        let coalescer = Arc::new(BatchCoalescer {
            state: Mutex::new(BatchCoalescerState::default()),
            cv: Condvar::new(),
            after_fast_path: None,
            config: BatchCoalesceConfig {
                coalesce_grace: Duration::ZERO,
                max_wait: Duration::from_millis(300),
                max_members: 2,
                global_inflight_limit: 1,
                eject_cooldown_rounds: 1,
            },
        });
        let key = test_batch_key("max-members");
        let start = Arc::new(Barrier::new(3));
        let runs = Arc::new(Mutex::new(Vec::<usize>::new()));
        let mut handles = Vec::new();

        for idx in 0..3 {
            let coalescer = Arc::clone(&coalescer);
            let key = key.clone();
            let start = Arc::clone(&start);
            let runs = Arc::clone(&runs);
            let request = coalescer_request(&format!("batch-{idx}"), &format!("member-{idx}"));
            handles.push(thread::spawn(move || {
                start.wait();
                coalescer.submit(key, &request, |combined| {
                    poisoned(&runs).push(combined.members.len());
                    green_report_for(combined)
                })
            }));
        }

        let reports: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("coalescer thread"))
            .collect();
        // Invariants robust to scheduler timing (the exact run PARTITION — e.g.
        // [1,2] vs [2,1] vs [1,1,1] when followers miss the leader's drain
        // window — is inherently racy under parallel test load). What must hold:
        let run_sizes = poisoned(&runs).clone();
        // (1) max_members is NEVER exceeded — the overflow backstop is the point.
        assert!(
            run_sizes.iter().all(|&n| n <= 2),
            "no physical run may exceed max_members=2; got {run_sizes:?}"
        );
        // (2) every member ran exactly once across all flushes (none lost,
        // none double-run): total members == 3.
        assert_eq!(
            run_sizes.iter().sum::<usize>(),
            3,
            "all 3 members must run exactly once across the flushes; got {run_sizes:?}"
        );
        // (3) at least 2 flushes (3 members, cap 2 ⇒ cannot fit in one).
        assert!(
            run_sizes.len() >= 2,
            "3 members with max_members=2 require ≥2 flushes; got {run_sizes:?}"
        );
        assert_eq!(reports.len(), 3);
        // Distinct flushes carry distinct executed_batch_id values.
        let mut executed_ids: Vec<_> = reports
            .iter()
            .filter_map(|report| report.executed_batch_id.clone())
            .collect();
        executed_ids.sort();
        executed_ids.dedup();
        assert!(
            executed_ids.len() >= 2,
            "≥2 physical flushes should have distinct executed_batch_id values; got {executed_ids:?}"
        );
        assert!(
            reports
                .iter()
                .all(|report| report.verdict == BatchVerdict::Green && report.members.len() == 1)
        );
    }

    #[test]
    fn batch_coalescer_completed_waiter_cannot_miss_queue_removal_notification() {
        let (follower_arrived_tx, follower_arrived_rx) = std::sync::mpsc::channel();
        let (release_follower_tx, release_follower_rx) = std::sync::mpsc::channel();
        let release_follower_rx = Arc::new(Mutex::new(release_follower_rx));
        let hook_release = Arc::clone(&release_follower_rx);
        let coalescer = Arc::new(BatchCoalescer {
            state: Mutex::new(BatchCoalescerState::default()),
            cv: Condvar::new(),
            after_fast_path: Some(Arc::new(move |request| {
                if request.batch_id == "follower" {
                    follower_arrived_tx
                        .send(())
                        .expect("announce follower fast-path pause");
                    hook_release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(2))
                        .expect("release follower after leader removes queue");
                }
            })),
            config: BatchCoalesceConfig {
                coalesce_grace: Duration::ZERO,
                max_wait: Duration::from_millis(300),
                max_members: 40,
                global_inflight_limit: 1,
                eject_cooldown_rounds: 1,
            },
        });
        let key = test_batch_key("missed-removal-notification");
        let follower_request = coalescer_request("follower", "follower-member");
        let follower_coalescer = Arc::clone(&coalescer);
        let follower_key = key.clone();
        let (follower_result_tx, follower_result_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let report =
                follower_coalescer.submit(follower_key, &follower_request, green_report_for);
            let _ = follower_result_tx.send(report);
        });

        follower_arrived_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("follower reached the pre-lock interleaving");

        // The follower is already queued but paused after its optimistic result
        // check. This submitter becomes leader, drains both waiters, publishes
        // both results, and removes the now-empty queue before we release it.
        let leader_request = coalescer_request("leader", "leader-member");
        let leader_report = coalescer.submit(key, &leader_request, green_report_for);
        assert_eq!(leader_report.members[0].worktree, "leader-member");

        release_follower_tx
            .send(())
            .expect("release paused follower");
        let follower_report = follower_result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("completed follower must observe its published result");
        assert_eq!(follower_report.members[0].worktree, "follower-member");
    }

    #[test]
    fn batch_coalescer_panic_in_run_does_not_wedge_group() {
        // GAP-1 regression: if the leader's physical run panics, every
        // already-drained non-leader waiter must still get a result instead of
        // parking on the condvar forever. Without the catch_unwind in submit(),
        // this test deadlocks (the two non-leaders never wake). Three same-key
        // submitters coalesce into one group; the leader's closure panics.
        let coalescer = Arc::new(test_coalescer());
        let key = test_batch_key("panic-group");
        let start = Arc::new(Barrier::new(3));
        let panics = Arc::new(Mutex::new(0u32));
        let mut handles = Vec::new();

        for idx in 0..3 {
            let coalescer = Arc::clone(&coalescer);
            let key = key.clone();
            let start = Arc::clone(&start);
            let panics = Arc::clone(&panics);
            let request = coalescer_request(&format!("batch-{idx}"), &format!("member-{idx}"));
            handles.push(thread::spawn(move || {
                start.wait();
                coalescer.submit(key, &request, |_combined| {
                    // Only the elected leader ever invokes `run`; one panic must
                    // fan out an indeterminate result to the whole drained group.
                    *poisoned(&panics) += 1;
                    panic!("simulated heavy-run crash (e.g. OOM compiling the union)");
                })
            }));
        }

        let reports: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("coalescer thread must not panic out"))
            .collect();

        // At least one physical run was attempted and panicked. Under
        // drain-on-completion the burst MAY form one coalesced group (all three
        // in one run) or, if a submitter misses the leader's cold-start grace
        // window, split across a couple of runs — either way every panic must
        // fan an Indeterminate out to its whole drained group, with NO wedge.
        // The load-bearing GAP-1 contract is "no waiter hangs after a panic",
        // not an exact physical-run count (which is inherently scheduler-racy).
        let panic_count = *poisoned(&panics);
        assert!(
            (1..=3).contains(&panic_count),
            "between 1 and 3 physical runs expected (coalescing is timing-dependent); got {panic_count}"
        );
        assert_eq!(reports.len(), 3);
        assert!(
            reports
                .iter()
                .all(|report| report.verdict == BatchVerdict::Indeterminate),
            "every submitter must see indeterminate after a run panic, not hang"
        );
        // Each submitter still gets its own member sliced back, in order.
        for (idx, report) in reports.iter().enumerate() {
            assert_eq!(report.members.len(), 1, "report {idx} keeps its own member");
            assert_eq!(report.members[0].provenance, BatchProvenance::Indeterminate);
        }
        // The coalescer is reusable after a panic: a fresh green submit works.
        let request = coalescer_request("after-panic", "member-after");
        let recovered = coalescer.submit(key, &request, green_report_for);
        assert_eq!(recovered.verdict, BatchVerdict::Green);
    }

    // ── Helpers for new tests ──────────────────────────────────────────────

    /// Build a coalescer with ejection disabled (cooldown=0) for tests that
    /// do not want ejection side-effects.
    fn test_coalescer_no_eject() -> BatchCoalescer {
        BatchCoalescer {
            state: Mutex::new(BatchCoalescerState::default()),
            cv: Condvar::new(),
            after_fast_path: None,
            config: BatchCoalesceConfig {
                coalesce_grace: Duration::ZERO,
                max_wait: Duration::from_millis(300),
                max_members: 40,
                global_inflight_limit: 1,
                eject_cooldown_rounds: 0, // ejection off
            },
        }
    }

    /// Build a solo-red report for a single-member request (mimics the
    /// SoloRed provenance returned by `run_batch` after combined-red fallback).
    fn solo_red_report_for(request: &BatchCheckRequest) -> BatchReport {
        BatchReport {
            batch_id: request.batch_id.clone(),
            verdict: BatchVerdict::Red,
            members: request
                .members
                .iter()
                .map(|member| cargoless_core::batch::BatchMemberResult {
                    worktree: member.worktree.clone(),
                    verdict: BatchVerdict::Red,
                    provenance: BatchProvenance::SoloRed,
                    diagnostics: Vec::new(),
                    duration_ms: 1,
                    ran_check_ids: Vec::new(),
                })
                .collect(),
            combined_checks: 0,
            solo_checks: 1,
            duration_ms: 1,
            queue_wait_ms: 0,
            executed_members: request.members.len() as u32,
            executed_batch_id: Some(request.batch_id.clone()),
        }
    }

    // ── Change 1: drain-on-completion tests ───────────────────────────────

    /// A lone submitter on a quiet trunk (inflight==0) must start with zero
    /// added latency: no timer wait, drain fires immediately.
    #[test]
    fn lone_submitter_quiet_trunk_starts_immediately() {
        let coalescer = Arc::new(test_coalescer_no_eject());
        let key = test_batch_key("lone");
        let request = coalescer_request("lone-batch", "lone-member");

        let run_entry = Arc::new(Mutex::new(None::<std::time::Instant>));
        let enqueued_at = std::time::Instant::now();

        let run_entry_clone = Arc::clone(&run_entry);
        let report = coalescer.submit(key, &request, move |combined| {
            *poisoned(&run_entry_clone) = Some(std::time::Instant::now());
            green_report_for(combined)
        });

        assert_eq!(report.verdict, BatchVerdict::Green);
        let elapsed = poisoned(&run_entry)
            .expect("run was invoked")
            .duration_since(enqueued_at);
        // With coalesce_grace=0, the run closure must start within a generous
        // bound (500ms); in practice it is sub-millisecond on a healthy host.
        assert!(
            elapsed < Duration::from_millis(500),
            "run started after {elapsed:?}; expected near-immediate start on quiet trunk"
        );
    }

    /// Arrivals during a run must all drain as ONE next batch (not one per
    /// arrival): while the leader is inside `run`, K more submitters enqueue;
    /// when the run finishes and inflight drops to 0, they all drain together.
    #[test]
    fn arrivals_during_run_drain_as_one_next_batch() {
        let coalescer = Arc::new(test_coalescer_no_eject());
        let key = test_batch_key("arrivals");

        // Channel: leader signals when it enters `run`; we enqueue K followers.
        let (in_run_tx, in_run_rx) = std::sync::mpsc::channel::<()>();
        // Channel: test unblocks the leader.
        let (unblock_tx, unblock_rx) = std::sync::mpsc::channel::<()>();

        let in_run_tx = Arc::new(std::sync::Mutex::new(Some(in_run_tx)));
        let unblock_rx = Arc::new(std::sync::Mutex::new(unblock_rx));

        let runs = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));

        // Submit the first member (becomes leader).
        let coalescer_a = Arc::clone(&coalescer);
        let key_a = key.clone();
        let in_run_tx_a = Arc::clone(&in_run_tx);
        let unblock_rx_a = Arc::clone(&unblock_rx);
        let runs_a = Arc::clone(&runs);
        let req_a = coalescer_request("batch-first", "member-first");
        let h_first = thread::spawn(move || {
            coalescer_a.submit(key_a, &req_a, move |combined| {
                // Signal that we are now inside the run.
                if let Some(tx) = poisoned(&in_run_tx_a).take() {
                    let _ = tx.send(());
                }
                // Block until the test says go.
                let _ = poisoned(&unblock_rx_a).recv();
                poisoned(&runs_a).push(
                    combined
                        .members
                        .iter()
                        .map(|m| m.worktree.clone())
                        .collect(),
                );
                green_report_for(combined)
            })
        });

        // Wait until the leader is inside `run`, then enqueue 3 more.
        in_run_rx.recv().expect("leader entered run");

        const K: usize = 3;
        let mut followers = Vec::new();
        for idx in 0..K {
            let coalescer_f = Arc::clone(&coalescer);
            let key_f = key.clone();
            let runs_f = Arc::clone(&runs);
            let req_f = coalescer_request(
                &format!("batch-follower-{idx}"),
                &format!("member-follower-{idx}"),
            );
            followers.push(thread::spawn(move || {
                coalescer_f.submit(key_f, &req_f, move |combined| {
                    poisoned(&runs_f).push(
                        combined
                            .members
                            .iter()
                            .map(|m| m.worktree.clone())
                            .collect(),
                    );
                    green_report_for(combined)
                })
            }));
        }

        // Give followers time to enqueue, then unblock the leader.
        thread::sleep(Duration::from_millis(50));
        unblock_tx.send(()).expect("unblock");
        h_first.join().expect("first submitter");
        for h in followers {
            h.join().expect("follower submitter");
        }

        let run_sizes: Vec<usize> = poisoned(&runs).iter().map(|g| g.len()).collect();
        assert_eq!(
            run_sizes.len(),
            2,
            "expected exactly 2 physical runs; got run sizes {run_sizes:?}"
        );
        assert_eq!(run_sizes[0], 1, "first run: just the leader's member");
        assert_eq!(
            run_sizes[1], K,
            "second run: all {K} followers drained together; got {run_sizes:?}"
        );
    }

    /// Two DIFFERENT keys submitted concurrently must NOT run simultaneously:
    /// with global_inflight_limit=1 they run disjointly (Variant A).
    #[test]
    fn global_inflight_gate_serializes_across_keys() {
        let coalescer = Arc::new(test_coalescer_no_eject());
        let key_a = test_batch_key("inflight-key-a");
        let key_b = test_batch_key("inflight-key-b");

        // Barrier: both threads start submitting at the same time.
        let start = Arc::new(Barrier::new(2));
        // Each run records its (enter, exit) wall-clock time.
        let timeline = Arc::new(Mutex::new(
            Vec::<(std::time::Instant, std::time::Instant)>::new(),
        ));

        let mut handles = Vec::new();
        for (key, batch_id, member) in [
            (key_a, "batch-ka", "member-ka"),
            (key_b, "batch-kb", "member-kb"),
        ] {
            let coalescer = Arc::clone(&coalescer);
            let start = Arc::clone(&start);
            let timeline = Arc::clone(&timeline);
            let request = coalescer_request(batch_id, member);
            handles.push(thread::spawn(move || {
                start.wait();
                coalescer.submit(key, &request, move |combined| {
                    let enter = std::time::Instant::now();
                    // Simulate a non-trivial run so timelines are measurable.
                    thread::sleep(Duration::from_millis(30));
                    let exit = std::time::Instant::now();
                    poisoned(&timeline).push((enter, exit));
                    green_report_for(combined)
                })
            }));
        }
        for h in handles {
            h.join().expect("inflight gate thread");
        }

        let tl = poisoned(&timeline).clone();
        assert_eq!(tl.len(), 2, "both runs must complete");
        let (e0, x0) = tl[0];
        let (e1, x1) = tl[1];
        // Disjoint intervals: one must start after the other exits.
        let disjoint = x0 <= e1 || x1 <= e0;
        assert!(
            disjoint,
            "global_inflight_limit=1: runs must be disjoint; \
             run0={e0:?}..{x0:?} run1={e1:?}..{x1:?}"
        );
    }

    // ── CGLS-25: WitnessInflightGate tests ────────────────────────────────

    fn test_witness_gate(limit: u32, budget_ms: u64) -> WitnessInflightGate {
        WitnessInflightGate {
            state: Mutex::new(0),
            cv: Condvar::new(),
            waiting: AtomicU64::new(0),
            limit,
            queue_budget: Duration::from_millis(budget_ms),
        }
    }

    #[test]
    fn witness_gate_limit_1_serializes_two_compiles() {
        // Two workers acquire the gate (limit=1) and hold it for a
        // measurable window; their intervals must be disjoint — one runs
        // only after the other releases. Mirrors
        // global_inflight_gate_serializes_across_keys.
        let gate = Arc::new(test_witness_gate(1, 60_000));
        let start = Arc::new(Barrier::new(2));
        let timeline = Arc::new(Mutex::new(Vec::<(Instant, Instant)>::new()));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let gate = Arc::clone(&gate);
            let start = Arc::clone(&start);
            let timeline = Arc::clone(&timeline);
            handles.push(thread::spawn(move || {
                start.wait();
                let _slot = gate.acquire();
                let enter = Instant::now();
                thread::sleep(Duration::from_millis(30));
                let exit = Instant::now();
                poisoned(&timeline).push((enter, exit));
            }));
        }
        for h in handles {
            h.join().expect("witness gate thread");
        }
        let tl = poisoned(&timeline).clone();
        assert_eq!(tl.len(), 2, "both compiles must complete");
        let (e0, x0) = tl[0];
        let (e1, x1) = tl[1];
        assert!(
            x0 <= e1 || x1 <= e0,
            "limit=1: compiles must be disjoint; run0={e0:?}..{x0:?} run1={e1:?}..{x1:?}"
        );
        // Counter fully released.
        assert_eq!(*poisoned(&gate.state), 0, "all slots released");
    }

    #[test]
    fn witness_gate_disabled_is_unbounded_concurrency() {
        // limit=0 = OFF = today's behavior: N workers run concurrently, no
        // serialization. Grants are uncounted so the counter never moves.
        // `overlap`/`max_overlap` are Mutex<usize> (no new atomic import).
        let gate = Arc::new(test_witness_gate(0, 60_000));
        let start = Arc::new(Barrier::new(3));
        let overlap = Arc::new(Mutex::new(0usize));
        let max_overlap = Arc::new(Mutex::new(0usize));
        let mut handles = Vec::new();
        for _ in 0..3 {
            let gate = Arc::clone(&gate);
            let start = Arc::clone(&start);
            let overlap = Arc::clone(&overlap);
            let max_overlap = Arc::clone(&max_overlap);
            handles.push(thread::spawn(move || {
                start.wait();
                let _slot = gate.acquire();
                {
                    let mut cur = poisoned(&overlap);
                    *cur += 1;
                    let mut mx = poisoned(&max_overlap);
                    *mx = (*mx).max(*cur);
                }
                thread::sleep(Duration::from_millis(25));
                *poisoned(&overlap) -= 1;
            }));
        }
        for h in handles {
            h.join().expect("witness gate thread");
        }
        assert!(
            *poisoned(&max_overlap) >= 2,
            "gate off ⇒ concurrent compiles allowed (unbounded, today's behavior)"
        );
        assert_eq!(
            *poisoned(&gate.state),
            0,
            "no-op grants never touch counter"
        );
    }

    #[test]
    fn witness_gate_wait_interval_never_bypasses_limit() {
        // A holder keeps the only slot past multiple queue intervals. The
        // waiter must remain queued until release: `limit=1` is an invariant,
        // not a best-effort hint that disappears during a slow compile.
        let gate = Arc::new(test_witness_gate(1, 50)); // 50ms budget
        let hold_start = Arc::new(Barrier::new(2));
        let gate_h = Arc::clone(&gate);
        let hs = Arc::clone(&hold_start);
        let holder = thread::spawn(move || {
            let _slot = gate_h.acquire();
            hs.wait(); // signal the slot is taken
            thread::sleep(Duration::from_millis(300)); // hold well past budget
        });
        hold_start.wait();
        // Waiter: acquire must outlive the 50ms interval and wait for release.
        let t0 = Instant::now();
        {
            let _slot = gate.acquire();
        }
        let waited = t0.elapsed();
        holder.join().expect("holder");
        assert!(
            waited >= Duration::from_millis(250),
            "waiter must remain queued until the 300ms holder releases (waited {waited:?})"
        );
    }

    #[test]
    fn witness_gate_panicking_holder_releases_slot() {
        // A panicking guarded section must still release the slot (RAII drop
        // on unwind), so the next witness is not permanently blocked.
        // Mirrors batch_coalescer_panic_cross_key_proceeds_after_inflight_guard.
        let gate = Arc::new(test_witness_gate(1, 60_000));
        let gate_p = Arc::clone(&gate);
        let panicked = thread::spawn(move || {
            let _slot = gate_p.acquire();
            assert_eq!(*poisoned(&gate_p.state), 1, "slot claimed");
            panic!("witness worker panicked mid-compile");
        })
        .join();
        assert!(panicked.is_err(), "the worker did panic");
        assert_eq!(
            *poisoned(&gate.state),
            0,
            "panic must release the slot (RAII drop on unwind)"
        );
        // A fresh acquire still works (not permanently blocked).
        let _slot = gate.acquire();
        assert_eq!(*poisoned(&gate.state), 1, "next witness acquires cleanly");
    }

    /// THE observability regression test. A witness parked on the gate must be
    /// COUNTED as waiting, because its absence is exactly what hid the real
    /// serialization point: `/admin/active` reported only the BatchCoalescer's
    /// queue, which sits DOWNSTREAM of this gate, so N stacked-up witnesses
    /// showed as a near-empty batch queue and read as "the batcher has nothing
    /// to coalesce" when in truth this gate was admitting one at a time.
    #[test]
    fn witness_gate_waiting_gauge_exposes_the_upstream_queue() {
        let gate = Arc::new(test_witness_gate(1, 60_000));
        assert_eq!(
            gate.counts(),
            (0, 0),
            "idle gate: nothing held, nothing queued"
        );

        let hold_start = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let gate_h = Arc::clone(&gate);
        let hs = Arc::clone(&hold_start);
        let rel = Arc::clone(&release);
        let holder = thread::spawn(move || {
            let _slot = gate_h.acquire();
            hs.wait();
            rel.wait(); // hold until the assertions below have run
        });
        hold_start.wait();
        assert_eq!(gate.counts().0, 1, "holder occupies the only slot");

        // A second witness parks. It must become VISIBLE as waiting.
        let gate_w = Arc::clone(&gate);
        let waiter = thread::spawn(move || {
            let _slot = gate_w.acquire();
        });
        let mut observed_waiting = 0;
        for _ in 0..200 {
            observed_waiting = gate.counts().1;
            if observed_waiting >= 1 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            observed_waiting >= 1,
            "a parked witness MUST appear in the waiting gauge — this is the \
             field whose absence made the upstream gate invisible (got {observed_waiting})"
        );

        release.wait();
        holder.join().expect("holder");
        waiter.join().expect("waiter");

        // And it must drain back to zero: a gauge that leaks upward would
        // strand a phantom queue on /admin/active forever.
        assert_eq!(
            gate.counts(),
            (0, 0),
            "every acquire path must decrement the waiting gauge on exit"
        );
    }

    /// Crossing one or more observation intervals must not leak or temporarily
    /// drop the waiting gauge. The waiter remains queued until a real slot is
    /// available, then the ticket drains normally.
    #[test]
    fn witness_gate_repeated_wait_intervals_preserve_waiting_gauge() {
        let gate = Arc::new(test_witness_gate(1, 50));
        let hold_start = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let gate_h = Arc::clone(&gate);
        let hs = Arc::clone(&hold_start);
        let rel = Arc::clone(&release);
        let holder = thread::spawn(move || {
            let _slot = gate_h.acquire();
            hs.wait();
            rel.wait();
        });
        hold_start.wait();

        let gate_w = Arc::clone(&gate);
        let waiter = thread::spawn(move || {
            let _slot = gate_w.acquire();
        });
        thread::sleep(Duration::from_millis(125)); // cross two 50ms intervals
        assert_eq!(
            gate.counts(),
            (1, 1),
            "the holder stays admitted and the waiter stays visibly queued"
        );

        release.wait();
        holder.join().expect("holder");
        waiter.join().expect("waiter");
        assert_eq!(gate.counts(), (0, 0), "gate returns fully idle");
    }

    /// A panic unwinding through `acquire`'s parked section must not leak the
    /// gauge either (RAII drop on unwind), mirroring
    /// `witness_gate_panicking_holder_releases_slot` for the slot counter.
    #[test]
    fn witness_gate_panicking_holder_releases_waiting_gauge() {
        let gate = Arc::new(test_witness_gate(1, 60_000));
        let gate_p = Arc::clone(&gate);
        let panicked = thread::spawn(move || {
            let _slot = gate_p.acquire();
            panic!("witness worker panicked mid-compile");
        })
        .join();
        assert!(panicked.is_err(), "the worker did panic");
        assert_eq!(
            gate.counts(),
            (0, 0),
            "panic must release BOTH the slot and the waiting gauge"
        );
    }

    // ── Change 2: cross-run culprit ejection tests ────────────────────────

    /// A member that returned SoloRed must be held out of the immediately-next
    /// drain (cooldown=1), then admitted and given a real verdict in the next
    /// drain after that.
    #[test]
    fn solo_red_member_is_held_out_of_next_drain() {
        // Coalescer with cooldown=1. Use global_inflight_limit=0 (per-key only)
        // to avoid serialisation interference in this single-key test.
        let coalescer = Arc::new(BatchCoalescer {
            state: Mutex::new(BatchCoalescerState::default()),
            cv: Condvar::new(),
            after_fast_path: None,
            config: BatchCoalesceConfig {
                coalesce_grace: Duration::ZERO,
                max_wait: Duration::from_millis(300),
                max_members: 40,
                global_inflight_limit: 0,
                eject_cooldown_rounds: 1,
            },
        });
        let key = test_batch_key("eject-solo-red");

        // ---- Round 1: submit "red-member" alone; it returns SoloRed. ----
        let req_red = coalescer_request("round1", "red-member");
        let run_sizes = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let runs_1 = Arc::clone(&run_sizes);
        let report_r1 = coalescer.submit(key.clone(), &req_red, move |combined| {
            runs_1.lock().unwrap().push(
                combined
                    .members
                    .iter()
                    .map(|m| m.worktree.clone())
                    .collect(),
            );
            solo_red_report_for(combined)
        });
        assert_eq!(report_r1.verdict, BatchVerdict::Red);
        assert_eq!(
            report_r1.members[0].provenance,
            BatchProvenance::SoloRed,
            "round 1 must be SoloRed"
        );

        // ---- Round 2: re-submit "red-member" + a healthy "green-member". ----
        // "red-member" is in cooldown; it must be SKIPPED this drain.
        // Only "green-member" should appear in round-2's group.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let barrier_r2 = Arc::new(Barrier::new(2));

        let coalescer_r2 = Arc::clone(&coalescer);
        let key_r2 = key.clone();
        let runs_r2 = Arc::clone(&run_sizes);
        let barrier_r2_t = Arc::clone(&barrier_r2);
        // Submit the green member first so it wins the leader race.
        let req_green = coalescer_request("round2-green", "green-member");
        let h_green = {
            let coalescer_r2 = Arc::clone(&coalescer_r2);
            let key_r2 = key_r2.clone();
            let runs_r2 = Arc::clone(&runs_r2);
            let barrier_r2_t = Arc::clone(&barrier_r2_t);
            thread::spawn(move || {
                coalescer_r2.submit(key_r2, &req_green, move |combined| {
                    barrier_r2_t.wait(); // let "red-member" enqueue first
                    let members: Vec<String> = combined
                        .members
                        .iter()
                        .map(|m| m.worktree.clone())
                        .collect();
                    runs_r2.lock().unwrap().push(members.clone());
                    // "red-member" must NOT appear in this run.
                    assert!(
                        !members.contains(&"red-member".to_string()),
                        "red-member should be held out of round-2 drain; got {members:?}"
                    );
                    green_report_for(combined)
                })
            })
        };
        // Submit "red-member" concurrently; it should sit in the queue.
        let coalescer_red2 = Arc::clone(&coalescer);
        let key_red2 = key.clone();
        let runs_red2 = Arc::clone(&run_sizes);
        let done_tx_clone = done_tx.clone();
        let req_red2 = coalescer_request("round2-red", "red-member");
        let h_red = thread::spawn(move || {
            // Slight delay so green-member wins leader election.
            thread::sleep(Duration::from_millis(5));
            let r = coalescer_red2.submit(key_red2, &req_red2, move |combined| {
                let members: Vec<String> = combined
                    .members
                    .iter()
                    .map(|m| m.worktree.clone())
                    .collect();
                runs_red2.lock().unwrap().push(members);
                green_report_for(combined)
            });
            drop(done_tx_clone);
            r
        });
        // Signal green leader to start its run (red-member is enqueued by now).
        barrier_r2.wait();
        h_green.join().expect("green round-2");
        h_red.join().expect("red-member round-3");
        drop(done_tx);
        let _ = done_rx.recv(); // wait for red to complete (round 3).

        let all_runs = run_sizes.lock().unwrap().clone();
        // Should be 3 physical runs total:
        // run[0] = ["red-member"]  (round 1 — SoloRed, sets ejection)
        // run[1] = ["green-member"] (round 2 — red-member held out)
        // run[2] = ["red-member"]  (round 3 — cooldown expired, admitted)
        assert_eq!(
            all_runs.len(),
            3,
            "expected 3 physical runs; got {all_runs:?}"
        );
        assert_eq!(all_runs[0], vec!["red-member"], "run 1");
        assert_eq!(all_runs[1], vec!["green-member"], "run 2 (red held out)");
        assert_eq!(all_runs[2], vec!["red-member"], "run 3 (red admitted)");
    }

    /// Ejected member is never starved: no matter how many fresh arrivals
    /// pile in, the ejected member must be admitted within 2 drains.
    #[test]
    fn ejected_member_is_never_starved() {
        let coalescer = Arc::new(BatchCoalescer {
            state: Mutex::new(BatchCoalescerState::default()),
            cv: Condvar::new(),
            after_fast_path: None,
            config: BatchCoalesceConfig {
                // Small cold-start grace so round-2's fresh greens batch.
                coalesce_grace: Duration::from_millis(50),
                max_wait: Duration::from_millis(300),
                max_members: 40,
                global_inflight_limit: 0,
                eject_cooldown_rounds: 1,
            },
        });
        let key = test_batch_key("no-starvation");

        // Round 1: eject "persistent-red".
        let req_red = coalescer_request("r1", "persistent-red");
        let run_log = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let rl = Arc::clone(&run_log);
        let _ = coalescer.submit(key.clone(), &req_red, move |combined| {
            rl.lock().unwrap().push(
                combined
                    .members
                    .iter()
                    .map(|m| m.worktree.clone())
                    .collect(),
            );
            solo_red_report_for(combined)
        });

        // Round 2: submit "persistent-red" (ejected) + 3 fresh greens. The
        // ejected member must NOT appear in round-2's drain. But with
        // `next_run_seq > release_at_run_seq` strict, it IS admitted in round 3.
        //
        // We serialise this deterministically: submit red first (it will be
        // skipped), then 3 greens, then check that red is admitted in run[2].
        let req_red2 = coalescer_request("r2-red", "persistent-red");
        let rl2 = Arc::clone(&run_log);
        let coalescer2 = Arc::clone(&coalescer);
        let key2 = key.clone();

        // Use a channel to serialise round 2 vs 3.
        let (r2_done_tx, r2_done_rx) = std::sync::mpsc::channel::<()>();

        // Launch red2 (will sit in queue, skipped once).
        let h_red2 = {
            let rl_r = Arc::clone(&rl2);
            thread::spawn(move || {
                coalescer2.submit(key2, &req_red2, move |combined| {
                    rl_r.lock().unwrap().push(
                        combined
                            .members
                            .iter()
                            .map(|m| m.worktree.clone())
                            .collect(),
                    );
                    green_report_for(combined) // red admitted in round 3 → green verdict
                })
            })
        };

        // Slight delay so red2 is enqueued first.
        thread::sleep(Duration::from_millis(5));

        // Submit 3 greens; they will be round-2 run.
        for idx in 0..3usize {
            let coalescer_g = Arc::clone(&coalescer);
            let key_g = key.clone();
            let rl_g = Arc::clone(&run_log);
            let r2_tx = r2_done_tx.clone();
            let req_g = coalescer_request(&format!("r2-g{idx}"), &format!("green-{idx}"));
            thread::spawn(move || {
                let _ = coalescer_g.submit(key_g, &req_g, move |combined| {
                    rl_g.lock().unwrap().push(
                        combined
                            .members
                            .iter()
                            .map(|m| m.worktree.clone())
                            .collect(),
                    );
                    green_report_for(combined)
                });
                drop(r2_tx);
            });
        }
        drop(r2_done_tx);
        // Wait for all round-2 greens to finish.
        while r2_done_rx.recv().is_ok() {}
        h_red2.join().expect("red admitted in round 3");

        let log = run_log.lock().unwrap().clone();
        // run[0] = round 1 (SoloRed ejection)
        // run[1] = round 2 (greens; red skipped)
        // run[2] = round 3 (red admitted — within 2 drains of ejection)
        assert!(
            log.len() >= 2,
            "at least 2 physical runs expected; got {log:?}"
        );
        // The ejected member must appear in one of the last runs (round 2 or 3),
        // proving it was admitted within cooldown_rounds + 1 = 2 drains.
        let last_two: Vec<_> = log.iter().rev().take(2).collect();
        let admitted = last_two
            .iter()
            .any(|run| run.contains(&"persistent-red".to_string()));
        assert!(
            admitted,
            "persistent-red must be admitted within 2 drains; log={log:?}"
        );
    }

    /// Ejection does not disturb positional attribution: when a member is held
    /// out mid-drain, the remaining members' results are still sliced correctly
    /// by `distribute_combined_report` (offsets stay aligned).
    #[test]
    fn ejection_preserves_positional_attribution() {
        // Coalescer: eject after SoloRed, cooldown=1, per-key gate. Small
        // cold-start grace so round-2's alpha+beta enqueue together and
        // coalesce into ONE batch (the behaviour under test) while culprit is
        // held out.
        let coalescer = Arc::new(BatchCoalescer {
            state: Mutex::new(BatchCoalescerState::default()),
            cv: Condvar::new(),
            after_fast_path: None,
            config: BatchCoalesceConfig {
                coalesce_grace: Duration::from_millis(50),
                max_wait: Duration::from_millis(300),
                max_members: 40,
                global_inflight_limit: 0,
                eject_cooldown_rounds: 1,
            },
        });
        let key = test_batch_key("positional");

        // Round 1: eject "culprit" (SoloRed).
        let req_culprit = coalescer_request("r1-culprit", "culprit");
        let rl = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let rl1 = Arc::clone(&rl);
        let _ = coalescer.submit(key.clone(), &req_culprit, move |combined| {
            rl1.lock().unwrap().push(
                combined
                    .members
                    .iter()
                    .map(|m| m.worktree.clone())
                    .collect(),
            );
            solo_red_report_for(combined)
        });

        // Round 2: submit "alpha", "culprit" (ejected), "beta" concurrently.
        // "culprit" is held out; only "alpha" and "beta" run.
        // Each must receive its own result (not the other's).
        let mut round2_handles = Vec::new();
        let round2_results = Arc::new(Mutex::new(
            std::collections::BTreeMap::<String, BatchVerdict>::new(),
        ));
        let rl2 = Arc::clone(&rl);

        for member in ["alpha", "culprit", "beta"] {
            let c = Arc::clone(&coalescer);
            let k = key.clone();
            let rr = Arc::clone(&round2_results);
            let rl_t = Arc::clone(&rl2);
            let req = coalescer_request(&format!("r2-{member}"), member);
            let member_str = member.to_string();
            round2_handles.push(thread::spawn(move || {
                // Stagger by member to make alpha/beta submit before culprit.
                if member_str == "culprit" {
                    thread::sleep(Duration::from_millis(5));
                }
                let report = c.submit(k, &req, move |combined| {
                    rl_t.lock().unwrap().push(
                        combined
                            .members
                            .iter()
                            .map(|m| m.worktree.clone())
                            .collect(),
                    );
                    green_report_for(combined)
                });
                poisoned(&rr).insert(member_str, report.members[0].verdict);
            }));
        }
        for h in round2_handles {
            h.join().expect("round-2 member");
        }

        let results = poisoned(&round2_results).clone();
        // "alpha" and "beta" get real green verdicts in round 2.
        assert_eq!(
            results.get("alpha"),
            Some(&BatchVerdict::Green),
            "alpha must be green"
        );
        assert_eq!(
            results.get("beta"),
            Some(&BatchVerdict::Green),
            "beta must be green"
        );
        // "culprit" was admitted in a separate drain (round 3) and also green.
        assert_eq!(
            results.get("culprit"),
            Some(&BatchVerdict::Green),
            "culprit must eventually get a real verdict"
        );

        // Verify that the drain for "alpha"+"beta" did not include "culprit"
        // (positional check: the run that had 2 members had only alpha+beta).
        let log = rl.lock().unwrap().clone();
        let two_member_runs: Vec<_> = log.iter().filter(|r| r.len() == 2).collect();
        assert_eq!(
            two_member_runs.len(),
            1,
            "exactly one 2-member run expected (alpha+beta); got {log:?}"
        );
        let names: std::collections::BTreeSet<_> =
            two_member_runs[0].iter().map(String::as_str).collect();
        assert_eq!(names, ["alpha", "beta"].iter().copied().collect());
    }

    /// A panic during the physical run must still decrement inflight (via
    /// InflightGuard) and wake cross-key leaders. After the panic, a
    /// different-key submit must still proceed.
    #[test]
    fn batch_coalescer_panic_cross_key_proceeds_after_inflight_guard() {
        let coalescer = Arc::new(test_coalescer_no_eject());
        let key_panic = test_batch_key("panic-inflight");
        let key_other = test_batch_key("other-inflight");

        // Submit to the panic key; run closure will panic.
        let coalescer_p = Arc::clone(&coalescer);
        let key_panic2 = key_panic.clone();
        let req_panic = coalescer_request("panic-batch", "panic-member");
        let h_panic = thread::spawn(move || {
            coalescer_p.submit(key_panic2, &req_panic, |_combined| {
                panic!("simulated run panic");
            })
        });
        let report_panic = h_panic.join().expect("panic thread must not propagate");
        assert_eq!(report_panic.verdict, BatchVerdict::Indeterminate);

        // After the panic the InflightGuard should have decremented inflight to 0.
        // A fresh submit on a DIFFERENT key must succeed immediately.
        let req_other = coalescer_request("other-batch", "other-member");
        let report_other = coalescer.submit(key_other, &req_other, green_report_for);
        assert_eq!(
            report_other.verdict,
            BatchVerdict::Green,
            "cross-key submit must succeed after panic releases InflightGuard"
        );
        // Inflight must be 0 now.
        assert_eq!(
            coalescer.counts().inflight_runs,
            0,
            "inflight must be 0 after both runs complete"
        );
    }

    /// TDD gate for Phase 2 (push-path coalescing).
    ///
    /// Proves the core coalescing property at the coalescer level:
    /// N concurrent submitters using the push-path key format
    /// (`"pushpath:<base_ref>:<root>"`) share exactly ONE physical run
    /// closure invocation, and each submitter receives its own per-WT
    /// slice of the combined report.
    ///
    /// This is the FAILING-FIRST test: it will fail until
    /// `coalesced_project_check` is wired to the push-path coalescer.
    /// Once the method exists and emits the correct key, the
    /// `batch_coalescer.submit` machinery (already proven by
    /// `batch_coalescer_groups_same_key_requests`) does the rest.
    ///
    /// A separate integration test (`coalesced_project_check_green_real_project`)
    /// proves the type conversion + real-project end-to-end.
    #[test]
    fn coalesced_project_check_routes_n_pushers_through_one_physical_run() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let project = setup_batch_project("pushpath-coalesce");
        let api = Arc::new(ServeVerdictState::new());

        // We test the coalescing key derivation by wiring a counting closure
        // directly into the coalescer using the SAME server-derived
        // project-check plan token that `coalesced_project_check` will use.
        // This validates the key format without requiring a real daemon loop.
        let base_ref = "origin/main";
        let root_str = project.root.to_string_lossy().into_owned();

        let run_count = Arc::new(AtomicU32::new(0));
        let start = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();

        // Build all requests up front (before borrowing api for threads).
        let mut thread_args: Vec<(BatchCoalesceKey, BatchCheckRequest, String)> = Vec::new();
        for idx in 0..3usize {
            let wt = format!("/client/agent-{idx:02}");
            let mut request = BatchCheckRequest::new(format!("pushpath:{wt}"), base_ref);
            request.options = cargoless_core::transport::PushOverlayOptions {
                repo_relative: false,
                analysis_root: Some(root_str.clone()),
                base_sha: None,
                source_ref: None,
                source_sha: None,
                comparison_base_sha: None,
                candidate_snapshot: None,
                changed_files: None,
                gate: false,
                check_ids: None,
                semantic: None,
            };
            request.members = vec![cargoless_core::batch::BatchMember {
                worktree: wt.clone(),
                files: vec![(
                    project
                        .root
                        .join(format!("src/agent_{idx:02}.rs"))
                        .to_string_lossy()
                        .into_owned(),
                    format!("pub fn agent_{idx:02}() {{}}\n"),
                )],
                changed_files: vec![format!("src/agent_{idx:02}.rs")],
            }];
            request.corun = true;
            request.coalesce_key = Some(
                project_check_plan_coalesce_token(&project.root, &request)
                    .expect("selected project-check plan should be coalesceable"),
            );
            let key = batch_coalesce_key(&request).expect("coalesce_key should be present");
            thread_args.push((key, request, wt));
        }

        for (key, request, _wt) in thread_args {
            let run_count = Arc::clone(&run_count);
            let start = Arc::clone(&start);
            let api_clone = Arc::clone(&api);
            handles.push(thread::spawn(move || {
                start.wait();
                api_clone.batch_coalescer.submit(key, &request, |combined| {
                    run_count.fetch_add(1, Ordering::SeqCst);
                    // Return a green BatchReport covering all combined members.
                    let members: Vec<cargoless_core::batch::BatchMemberResult> = combined
                        .members
                        .iter()
                        .map(|m| cargoless_core::batch::BatchMemberResult {
                            worktree: m.worktree.clone(),
                            verdict: BatchVerdict::Green,
                            provenance: BatchProvenance::CombinedGreen,
                            diagnostics: Vec::new(),
                            duration_ms: 1,
                            ran_check_ids: Vec::new(),
                        })
                        .collect();
                    let executed_members = members.len() as u32;
                    BatchReport {
                        batch_id: combined.batch_id.clone(),
                        verdict: BatchVerdict::Green,
                        members,
                        combined_checks: 1,
                        solo_checks: 0,
                        duration_ms: 1,
                        queue_wait_ms: 0,
                        executed_members,
                        executed_batch_id: Some(combined.batch_id.clone()),
                    }
                })
            }));
        }

        let reports: Vec<BatchReport> = handles
            .into_iter()
            .map(|h| h.join().expect("pushpath coalescer thread"))
            .collect();

        // KEY ASSERTION: the 3 concurrent same-(base_ref,analysis_root) pushers
        // COALESCE — far fewer physical runs than submitters. In the steady
        // state they share exactly ONE run; under heavy parallel-test scheduler
        // jitter a straggler that misses the leader's cold-start grace window
        // may form a second run, so the robust contract is "strictly fewer runs
        // than pushers" (coalescing happened) rather than a brittle exact-1 that
        // flakes only when 60+ other tests contend for cores. Each submitter
        // still gets its own correct per-WT slice (asserted below).
        let final_run_count = run_count.load(Ordering::SeqCst);
        assert!(
            (1..3).contains(&final_run_count),
            "3 concurrent pushers sharing the same (base_ref, analysis_root) must \
             coalesce into fewer than 3 physical runs — got {final_run_count}"
        );

        // Each submitter gets its own per-WT member slice back.
        assert_eq!(reports.len(), 3, "every submitter must receive a report");
        for report in &reports {
            assert_eq!(
                report.members.len(),
                1,
                "each submitter's report must carry exactly 1 member slice"
            );
            assert_eq!(
                report.verdict,
                BatchVerdict::Green,
                "coalesced green run: every submitter report should be green"
            );
            assert_eq!(
                report.combined_checks, 1,
                "every submitter's report must reflect the shared combined_checks=1"
            );
        }
        // Verify all three distinct WT slices are present.
        let mut observed_wts: Vec<String> = reports
            .iter()
            .map(|r| r.members[0].worktree.clone())
            .collect();
        observed_wts.sort();
        assert_eq!(
            observed_wts,
            vec![
                "/client/agent-00".to_string(),
                "/client/agent-01".to_string(),
                "/client/agent-02".to_string(),
            ],
            "each coalesced submitter must receive its own WT member slice, not a neighbour's"
        );
        // project drops here → Drop removes root + remote dirs.
    }

    #[test]
    fn project_check_plan_coalesce_token_skips_manifest_edits() {
        let project = setup_batch_project("pushpath-manifest-edit");
        let mut request = batch_request(
            "manifest-edit",
            &project.root,
            vec![BatchMember {
                worktree: "/client/manifest-edit".to_string(),
                files: vec![(
                    project
                        .root
                        .join("cargoless.checks.yaml")
                        .to_string_lossy()
                        .into_owned(),
                    "version: 1\nchecks: []\n".to_string(),
                )],
                changed_files: vec!["cargoless.checks.yaml".to_string()],
            }],
        );
        request.options.repo_relative = false;

        assert!(
            project_check_plan_coalesce_token(&project.root, &request).is_none(),
            "manifest edits must evaluate after overlay materialization, not via a stale base plan"
        );
    }

    /// Gated (hard witness) pushes coalesce by BASE, not by changed-file plan.
    /// Two gated pushes with DIFFERENT changed files must land in the SAME
    /// coalesce token so they flatten into one physical `run_batch` — the fix
    /// for witness-lane serialization under a hot trunk (each PR's file set
    /// differs → the per-plan fingerprint fragments → N serial release compiles).
    #[test]
    fn gated_witness_pushes_coalesce_by_base_across_different_files() {
        let project = setup_batch_project("gated-coalesce-by-base");

        let mk = |wt: &str, file: &str| {
            let mut request = batch_request(
                "gated",
                &project.root,
                vec![BatchMember {
                    worktree: wt.to_string(),
                    files: vec![(
                        project.root.join(file).to_string_lossy().into_owned(),
                        format!("pub fn f() {{}} // {file}\n"),
                    )],
                    changed_files: vec![file.to_string()],
                }],
            );
            request.options.repo_relative = false;
            request
        };

        let req_a = mk("/client/a", "src/alpha.rs");
        let req_b = mk("/client/b", "src/beta.rs");

        // Sanity: the OLD (non-gated) per-plan token CAN differ across distinct
        // file sets — that fragmentation is exactly what defeated coalescing.
        // (We do not assert they differ — select_for_changes may pick the same
        // plan — only that the GATED token is base-stable regardless.)
        let gated_a = gated_or_plan_coalesce_token(&project.root, true, &req_a)
            .expect("clean gated push should be coalesceable");
        let gated_b = gated_or_plan_coalesce_token(&project.root, true, &req_b)
            .expect("clean gated push should be coalesceable");
        assert_eq!(
            gated_a, gated_b,
            "two gated pushes on the same base must share one coalesce token \
             regardless of their changed-file sets (got {gated_a:?} vs {gated_b:?})"
        );
        assert!(
            gated_a.starts_with("witness-gate:"),
            "gated token must be the coarse base-keyed form, got {gated_a:?}"
        );

        // …and they produce equal BatchCoalesceKeys (the actual queue key).
        let mut keyed_a = req_a.clone();
        keyed_a.coalesce_key = Some(gated_a);
        let mut keyed_b = req_b.clone();
        keyed_b.coalesce_key = Some(gated_b);
        assert_eq!(
            batch_coalesce_key(&keyed_a),
            batch_coalesce_key(&keyed_b),
            "same gated token ⇒ same BatchCoalesceKey ⇒ same coalescer queue"
        );
    }

    /// Manifest safety survives the gated coarse-key path: a gated push whose
    /// overlay edits `cargoless.checks.yaml` must NOT get a shared base key
    /// (it changes the plan itself), so it returns None → solo fallback.
    #[test]
    fn gated_witness_push_touching_manifest_does_not_coalesce() {
        let project = setup_batch_project("gated-manifest-edit");
        let mut request = batch_request(
            "gated-manifest",
            &project.root,
            vec![BatchMember {
                worktree: "/client/manifest".to_string(),
                files: vec![(
                    project
                        .root
                        .join("cargoless.checks.yaml")
                        .to_string_lossy()
                        .into_owned(),
                    "version: 1\nchecks: []\n".to_string(),
                )],
                changed_files: vec!["cargoless.checks.yaml".to_string()],
            }],
        );
        request.options.repo_relative = false;

        assert!(
            gated_or_plan_coalesce_token(&project.root, true, &request).is_none(),
            "a gated push editing the check manifest must fall back to a solo run"
        );
    }

    /// CGLS-27 × W4 interaction: when N gated pushes coalesce into ONE
    /// physical run and that run is stranded by an RA respawn, every
    /// co-run member must independently publish `unknown` — the coarse
    /// base-keyed coalescing must not swallow per-member attribution.
    ///
    /// Attributions are recorded per-WT at overlay-consume (one entry per
    /// gated pusher, keyed by its own worktree path), and the CGLS-27
    /// drain scoped to the respawned cluster's worktrees returns ONE
    /// entry per stranded member. This test pins that shape so the
    /// coarse-key coalescing cannot regress it silently.
    #[test]
    fn stranded_witness_publishes_unknown_to_every_coalesced_member() {
        let api = ServeVerdictState::new();

        // Three gated pushes on the same base_ref land in the same
        // coalescer queue (proven by gated_witness_pushes_coalesce_by_base_
        // across_different_files above); each pusher's SwitchOverlay arm
        // records its OWN worktree's attribution.
        let base_sha = "shared-base-sha-abc";
        let wts = ["/client/gated-a", "/client/gated-b", "/client/gated-c"];
        for wt in wts {
            api.record_push_attribution(wt, &stranded_pushed(base_sha));
        }

        // The physical run this coalesced trio would have shared is
        // stranded by a respawn (RA reset drops the in-flight txn per
        // #247). `reset_after_respawn` drains attributions scoped to the
        // respawned cluster's worktrees — i.e., all three co-run members.
        let cluster_keys: BTreeSet<String> = wts.iter().map(|s| s.to_string()).collect();
        let drained = api.drain_push_attributions_for(&cluster_keys);

        // The load-bearing shape: one drained attribution per coalesced
        // member — not one for the whole batch. `publish_stranded_unknown`
        // is called once per drained pair, so each member-WT publishes
        // its own honest `unknown` (ra_respawn_stranded_push, exit 75).
        assert_eq!(
            drained.len(),
            wts.len(),
            "every coalesced gated member must strand independently — a coarse \
             base-keyed batch must not swallow per-member attribution"
        );
        let mut drained_wts: Vec<String> = drained.iter().map(|(k, _)| k.clone()).collect();
        drained_wts.sort();
        let mut expected: Vec<String> = wts.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(
            drained_wts, expected,
            "every co-run member must appear in the drain — none lost to the coarse key"
        );
        for (_, attribution) in &drained {
            assert_eq!(
                attribution.base_sha.as_deref(),
                Some(base_sha),
                "the shared base survives per-member attribution"
            );
        }
        // Publish-once: a second respawn cannot re-publish the same batch.
        assert!(
            api.drain_push_attributions_for(&cluster_keys).is_empty(),
            "a second respawn must find nothing to re-publish for the coalesced set"
        );
    }

    /// Integration test: `coalesced_project_check` on a real git project
    /// returns `Green` for a clean overlay and correctly maps the per-WT
    /// member slice to `ProjectCheckSummary`. This validates the type
    /// conversion path independently of the coalescing count test.
    #[test]
    fn coalesced_project_check_green_real_project() {
        use crate::servedrv::ProjectCheckSummary;

        let project = setup_batch_project("coalesce-type-conv");
        let api = Arc::new(ServeVerdictState::new());

        let wt = Path::new("/client/wt-type-conv");
        let context = ProjectCheckRunContext {
            root: project.root.clone(),
            changed_files: Some(vec!["src/added.rs".into()]),
            base_ref: "origin/main".to_string(),
            base_sha: None,
            source_ref: None,
            source_sha: None,
            candidate_snapshot: None,
            overlay_files: vec![(
                project
                    .root
                    .join("src/added.rs")
                    .to_string_lossy()
                    .into_owned(),
                "pub fn added() {}\n".to_string(),
            )],
            materialize_overlay: true,
            gate: false,
            check_ids: None,
        };

        let result = api.coalesced_project_check(wt, &context);

        assert!(
            result.is_some(),
            "non-empty base_ref + materialize_overlay=true should engage the coalesced path"
        );
        let (summary, ran_check_ids) = result.unwrap();
        assert_eq!(
            summary,
            ProjectCheckSummary::Green,
            "clean overlay over a clean project should yield ProjectCheckSummary::Green"
        );
        // Commit-D core proof: the coalesced path now returns the REAL ids of
        // the checks that ran in the shared physical run (setup_batch_project
        // configures exactly one — `no-fail-token`), instead of the historical
        // empty "cannot enumerate" list. This is the witness's `gated_checks_ran`
        // proof at the method boundary, over a genuine `report.results[].id`
        // (not a mock). The empty return was the root cause: a requested witness
        // id could ride a coalesced green without ever being proven-ran.
        assert!(
            ran_check_ids.iter().any(|id| id == "no-fail-token"),
            "coalesced green must enumerate the check that actually ran, not return empty: {ran_check_ids:?}"
        );
        // project drops here → Drop removes root + remote dirs.
    }

    #[test]
    fn batch_check_http_combined_green_uses_real_project_checks() {
        let project = setup_batch_project("batch-http-green");
        let request = batch_request(
            "http-green",
            &project.root,
            vec![
                batch_member("a", "src/a.rs", "pub fn a() {}\n"),
                batch_member("b", "src/b.rs", "pub fn b() {}\n"),
            ],
        );

        let report = http_batch_check(&request);

        assert_eq!(report.verdict, BatchVerdict::Green);
        assert_eq!(report.combined_checks, 1);
        assert_eq!(report.solo_checks, 0);
        assert_eq!(report.members.len(), 2);
        assert!(report.members.iter().all(|member| {
            member.verdict == BatchVerdict::Green
                && member.provenance == BatchProvenance::CombinedGreen
                && member.diagnostics.is_empty()
        }));
    }

    #[test]
    fn batch_check_http_combined_red_falls_back_and_attributes_bad_member() {
        let project = setup_batch_project("batch-http-attribution");
        let overlay_paths = vec!["src/good.rs".to_string(), "src/bad.rs".to_string()];
        let request = batch_request(
            "http-attribution",
            &project.root,
            vec![
                batch_member("good", "src/good.rs", "pub fn good() {}\n"),
                batch_member("bad", "src/bad.rs", "pub fn bad() { /* FAIL_BATCH */ }\n"),
            ],
        );

        let report = http_batch_check(&request);

        assert_eq!(report.verdict, BatchVerdict::Red);
        assert_eq!(report.combined_checks, 1);
        assert_eq!(report.solo_checks, 2);
        let good = member_result(&report, "/client/good");
        assert_eq!(good.verdict, BatchVerdict::Green);
        assert_eq!(good.provenance, BatchProvenance::SoloGreen);
        assert!(good.diagnostics.is_empty());
        let bad = member_result(&report, "/client/bad");
        assert_eq!(bad.verdict, BatchVerdict::Red);
        assert_eq!(bad.provenance, BatchProvenance::SoloRed);
        assert!(
            bad.diagnostics
                .iter()
                .any(|diag| diag.code.as_deref() == Some("batch.fail_token"))
        );
        assert_overlay_paths_cleaned(&project.root, &overlay_paths);
    }

    #[test]
    fn batch_check_http_combined_red_attributes_multiple_bad_members() {
        let project = setup_batch_project("batch-http-multi-red");
        let overlay_paths = vec![
            "src/good_a.rs".to_string(),
            "src/bad_a.rs".to_string(),
            "src/good_b.rs".to_string(),
            "src/bad_b.rs".to_string(),
        ];
        let request = batch_request(
            "http-multi-red",
            &project.root,
            vec![
                batch_member("good-a", "src/good_a.rs", "pub fn good_a() {}\n"),
                batch_member(
                    "bad-a",
                    "src/bad_a.rs",
                    "pub fn bad_a() { /* FAIL_BATCH */ }\n",
                ),
                batch_member("good-b", "src/good_b.rs", "pub fn good_b() {}\n"),
                batch_member(
                    "bad-b",
                    "src/bad_b.rs",
                    "pub fn bad_b() { /* FAIL_BATCH */ }\n",
                ),
            ],
        );

        let report = http_batch_check(&request);

        assert_eq!(report.verdict, BatchVerdict::Red);
        assert_eq!(report.combined_checks, 1);
        assert_eq!(report.solo_checks, 4);
        for worktree in ["/client/good-a", "/client/good-b"] {
            let result = member_result(&report, worktree);
            assert_eq!(result.verdict, BatchVerdict::Green);
            assert_eq!(result.provenance, BatchProvenance::SoloGreen);
            assert!(result.diagnostics.is_empty());
        }
        for worktree in ["/client/bad-a", "/client/bad-b"] {
            let result = member_result(&report, worktree);
            assert_eq!(result.verdict, BatchVerdict::Red);
            assert_eq!(result.provenance, BatchProvenance::SoloRed);
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|diag| diag.code.as_deref() == Some("batch.fail_token")),
                "{worktree} should carry the forbidden-pattern diagnostic"
            );
        }
        assert_overlay_paths_cleaned(&project.root, &overlay_paths);
    }

    #[test]
    fn batch_check_http_overlay_conflict_reports_interaction_red_not_false_culprit() {
        let project = setup_batch_project("batch-http-interaction");
        let request = batch_request(
            "http-interaction",
            &project.root,
            vec![
                batch_member("one", "src/shared.rs", "pub fn one() {}\n"),
                batch_member("two", "src/shared.rs", "pub fn two() {}\n"),
            ],
        );

        let report = http_batch_check(&request);

        assert_eq!(report.verdict, BatchVerdict::Red);
        assert_eq!(report.combined_checks, 1);
        assert_eq!(report.solo_checks, 2);
        assert!(report.members.iter().all(|member| {
            member.verdict == BatchVerdict::Red
                && member.provenance == BatchProvenance::InteractionRed
                && member
                    .diagnostics
                    .iter()
                    .any(|diag| diag.message.contains("different content"))
        }));
    }

    #[test]
    fn batch_check_http_forty_member_green_batch_stays_one_combined_check() {
        let project = setup_batch_project("batch-http-forty");
        let members = (0..40)
            .map(|idx| {
                batch_member(
                    &format!("agent-{idx:02}"),
                    &format!("src/agent_{idx:02}.rs"),
                    &format!("pub fn agent_{idx:02}() {{}}\n"),
                )
            })
            .collect();
        let request = batch_request("http-forty", &project.root, members);

        let report = http_batch_check(&request);

        assert_eq!(report.verdict, BatchVerdict::Green);
        assert_eq!(report.members.len(), 40);
        assert_eq!(
            report.combined_checks, 1,
            "a 40-agent green batch should amortize to one combined check"
        );
        assert_eq!(report.solo_checks, 0);
        assert!(report.members.iter().all(|member| {
            member.verdict == BatchVerdict::Green
                && member.provenance == BatchProvenance::CombinedGreen
                && member.diagnostics.is_empty()
        }));
    }

    #[test]
    fn batch_check_http_no_corun_forty_member_batch_runs_all_solos() {
        let project = setup_batch_project("batch-http-forty-no-corun");
        let members = (0..40)
            .map(|idx| {
                batch_member(
                    &format!("solo-agent-{idx:02}"),
                    &format!("src/solo_agent_{idx:02}.rs"),
                    &format!("pub fn solo_agent_{idx:02}() {{}}\n"),
                )
            })
            .collect();
        let mut request = batch_request("http-forty-no-corun", &project.root, members);
        request.corun = false;

        let report = http_batch_check(&request);

        assert_eq!(
            report.verdict,
            BatchVerdict::Green,
            "clean no-corun members must remain green: {report:#?}"
        );
        assert_eq!(report.members.len(), 40);
        assert_eq!(report.combined_checks, 0);
        assert_eq!(
            report.solo_checks, 40,
            "no-corun mode should prove every member independently"
        );
        assert!(report.members.iter().all(|member| {
            member.verdict == BatchVerdict::Green
                && member.provenance == BatchProvenance::SoloGreen
                && member.diagnostics.is_empty()
        }));
    }

    #[test]
    fn batch_check_http_concurrent_same_root_batches_are_isolated_and_cleaned() {
        let project = setup_batch_project("batch-http-concurrent");
        let api = Arc::new(ServeVerdictState::new());
        let srv = HttpServer::bind(
            "127.0.0.1:0",
            Arc::clone(&api) as Arc<dyn VerdictService>,
            Arc::new(AllowAll),
        )
        .expect("bind ephemeral");
        std::thread::sleep(Duration::from_millis(50));
        let remote = format!("http://{}", srv.addr());
        let start = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        let mut overlay_paths = Vec::new();

        for request_idx in 0..8 {
            let members: Vec<BatchMember> = (0..5)
                .map(|member_idx| {
                    let rel_path = format!("src/concurrent_{request_idx}_{member_idx}.rs");
                    overlay_paths.push(rel_path.clone());
                    batch_member(
                        &format!("concurrent-{request_idx}-{member_idx}"),
                        &rel_path,
                        &format!(
                            "pub fn concurrent_{request_idx}_{member_idx}() -> usize {{ {} }}\n",
                            request_idx * 10 + member_idx
                        ),
                    )
                })
                .collect();
            let request = batch_request(
                &format!("http-concurrent-{request_idx}"),
                &project.root,
                members,
            );
            let remote = remote.clone();
            let start = Arc::clone(&start);
            handles.push(thread::spawn(move || {
                start.wait();
                http_batch_check_with_client(&remote, &request)
            }));
        }

        let reports: Vec<BatchReport> = handles
            .into_iter()
            .map(|handle| handle.join().expect("concurrent batch thread"))
            .collect();

        assert_eq!(reports.len(), 8);
        for report in reports {
            assert_eq!(report.verdict, BatchVerdict::Green);
            assert_eq!(report.members.len(), 5);
            assert_eq!(report.combined_checks, 1);
            assert_eq!(report.solo_checks, 0);
            assert!(report.members.iter().all(|member| {
                member.verdict == BatchVerdict::Green
                    && member.provenance == BatchProvenance::CombinedGreen
                    && member.diagnostics.is_empty()
            }));
        }
        assert_overlay_paths_cleaned(&project.root, &overlay_paths);
        drop(srv);
    }

    #[test]
    fn batch_check_coalesces_same_key_requests_and_slices_reports() {
        let project = setup_batch_project("batch-coalesce-same-key");
        let api = Arc::new(ServeVerdictState::new());
        let srv = HttpServer::bind(
            "127.0.0.1:0",
            Arc::clone(&api) as Arc<dyn VerdictService>,
            Arc::new(AllowAll),
        )
        .expect("bind ephemeral");
        std::thread::sleep(Duration::from_millis(50));
        let remote = format!("http://{}", srv.addr());
        let start = Arc::new(Barrier::new(2));

        let mut request_a = batch_request(
            "request-a",
            &project.root,
            vec![batch_member("a", "src/coalesce_a.rs", "pub fn a() {}\n")],
        );
        request_a.coalesce_key = Some("same-key".into());
        let mut request_b = batch_request(
            "request-b",
            &project.root,
            vec![batch_member("b", "src/coalesce_b.rs", "pub fn b() {}\n")],
        );
        request_b.coalesce_key = Some("same-key".into());

        let remote_a = remote.clone();
        let start_a = Arc::clone(&start);
        let handle_a = thread::spawn(move || {
            start_a.wait();
            http_batch_check_with_client(&remote_a, &request_a)
        });
        let remote_b = remote.clone();
        let start_b = Arc::clone(&start);
        let handle_b = thread::spawn(move || {
            start_b.wait();
            http_batch_check_with_client(&remote_b, &request_b)
        });

        let report_a = handle_a.join().expect("request a thread");
        let report_b = handle_b.join().expect("request b thread");

        assert_eq!(report_a.batch_id, "request-a");
        assert_eq!(report_b.batch_id, "request-b");
        assert_eq!(report_a.verdict, BatchVerdict::Green);
        assert_eq!(report_b.verdict, BatchVerdict::Green);
        assert_eq!(report_a.members.len(), 1);
        assert_eq!(report_b.members.len(), 1);
        assert_eq!(report_a.members[0].worktree, "/client/a");
        assert_eq!(report_b.members[0].worktree, "/client/b");
        assert_eq!(
            report_a.members[0].provenance,
            BatchProvenance::CombinedGreen
        );
        assert_eq!(
            report_b.members[0].provenance,
            BatchProvenance::CombinedGreen
        );
        assert_eq!(report_a.executed_members, 2);
        assert_eq!(report_b.executed_members, 2);
        assert_eq!(
            report_a.executed_batch_id, report_b.executed_batch_id,
            "both submitters should point at the same physical coalesced run"
        );
        assert!(
            report_a
                .executed_batch_id
                .as_deref()
                .is_some_and(|id| id.starts_with("coalesced:same-key:run-")),
            "executed_batch_id should be unique per physical run, not just per key"
        );
        assert_eq!(
            report_a.combined_checks, 1,
            "request A should see the shared combined run"
        );
        assert_eq!(
            report_b.combined_checks, 1,
            "request B should see the shared combined run"
        );
        assert_eq!(report_a.solo_checks, 0);
        assert_eq!(report_b.solo_checks, 0);
        drop(srv);
    }

    /// THE Increment-0 GATE differential test: a **remote** read of the
    /// real [`ServeVerdictState`] (over the shipped HTTP+SSE adapter) is
    /// byte-equivalent to the **local** in-proc read for the SAME tree
    /// state — across a GREEN→RED transition — AND the subscribe-emit
    /// (0b) delivers identical [`TransitionEvent`]s on both the in-proc
    /// receiver and the HTTP SSE receiver. Run against the production
    /// `ServeVerdictState`, not a mock — this proves the *wire*, which is
    /// what Increment 0 ships.
    #[test]
    fn remote_verdict_equiv_local_for_same_tree_state_and_subscribe_emits() {
        let api = Arc::new(ServeVerdictState::new());
        let wt = Path::new("/repo/wt-a");
        let key = wt.to_string_lossy().into_owned();

        // Local (in-proc) subscriber, registered before any publish.
        let local_rx = api.subscribe();

        // Real HTTP server over the real ServeVerdictState (#10 posture:
        // AllowAll — the auth seam is exercised separately in transport's
        // own unit suite; here we prove the verdict wire).
        let srv = HttpServer::bind(
            "127.0.0.1:0",
            Arc::clone(&api) as Arc<dyn VerdictService>,
            Arc::new(AllowAll),
        )
        .expect("bind ephemeral");
        std::thread::sleep(Duration::from_millis(50));
        let client =
            HttpClient::new(&format!("http://{}", srv.addr())).expect("client for ephemeral addr");
        // Remote SSE subscriber (server-side svc.subscribe()).
        let remote_rx = client.subscribe().expect("remote subscribe");
        std::thread::sleep(Duration::from_millis(80)); // subscriber registers

        // ── tree state 1: GREEN ──────────────────────────────────────
        api.publish(wt, crate::statusfile::VerdictPayload::green());
        let local_v = api.get_verdict(&key);
        let remote_v = client.get_verdict(&key).expect("remote get_verdict");
        assert_eq!(local_v.as_deref(), Some("green"), "local sees GREEN");
        assert_eq!(
            remote_v, local_v,
            "remote verdict ≡ local verdict for the same tree state (GREEN)"
        );
        let lev = local_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("local transition event");
        let rev = remote_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("remote SSE transition event");
        assert_eq!(lev.verdict, "green");
        assert_eq!(
            rev, lev,
            "remote TransitionEvent ≡ local TransitionEvent (subscribe-emit, 0b)"
        );

        // ── tree state 2: RED (same wt — a real transition) ───────────
        // INFRA-36: red MUST be backed by a real diagnostic count; the
        // test publishes 1 to exercise the non-empty path.
        api.publish(wt, crate::statusfile::VerdictPayload::red(1));
        let local_s = api.get_status(&key).map(|s| s.verdict);
        let remote_s = client
            .get_status(&key)
            .expect("remote get_status")
            .map(|s| s.verdict);
        assert_eq!(local_s.as_deref(), Some("red"), "local sees RED");
        assert_eq!(
            remote_s, local_s,
            "remote status verdict ≡ local for the same tree state (RED)"
        );
        let lev2 = local_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let rev2 = remote_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(lev2.verdict, "red");
        assert_eq!(
            rev2, lev2,
            "the GREEN→RED transition is mirrored remote ≡ local"
        );

        // Unknown worktree resolves identically (None) on both transports
        // — the 404/None path is part of "remote ≡ local".
        assert_eq!(api.get_verdict("nope"), None);
        assert_eq!(client.get_verdict("nope").unwrap(), None);

        // list_worktrees agrees across the wire.
        let local_list = api.list_worktrees();
        let remote_list = client.list_worktrees().expect("remote list");
        assert_eq!(local_list, remote_list, "list_worktrees remote ≡ local");
        assert_eq!(local_list.len(), 1);
        assert_eq!(local_list[0].verdict, "red");

        drop(srv);
    }

    #[test]
    fn get_diagnostics_retains_full_red_details_and_clears_on_success() {
        let api = ServeVerdictState::new();
        let diagnostic = Diagnostic {
            file_path: PathBuf::from("/r/wt/src/lib.rs"),
            line: 12,
            col: 7,
            severity: Severity::Error,
            code: Some("E0308".to_string()),
            message: "mismatched types".to_string(),
            source: Some("rustc".to_string()),
        };
        api.retain_diagnostics("/r/wt", None, vec![diagnostic.clone()]);
        api.publish(
            Path::new("/r/wt"),
            crate::statusfile::VerdictPayload::red(1),
        );
        assert_eq!(api.get_diagnostics("/r/wt"), vec![diagnostic]);
        let status = api.get_status("/r/wt").expect("status present");
        assert_eq!(status.verdict, "red");
        assert_eq!(status.red_diagnostics, 1);
        assert!(
            status.verdict_failure_reason.is_none(),
            "a real Red verdict carries its concrete diagnostics, not an infra reason"
        );

        api.retain_diagnostics("/r/wt", None, Vec::new());
        api.publish(
            Path::new("/r/wt"),
            crate::statusfile::VerdictPayload::green(),
        );
        assert!(
            api.get_diagnostics("/r/wt").is_empty(),
            "a later successful run must not expose stale RED diagnostics"
        );
    }

    #[test]
    fn attributed_diagnostics_do_not_cross_between_shared_worktree_prs() {
        let api = ServeVerdictState::new();
        let make = |path: &str, message: &str| Diagnostic {
            file_path: PathBuf::from(path),
            line: 1,
            col: 1,
            severity: Severity::Error,
            code: Some("E0308".to_string()),
            message: message.to_string(),
            source: Some("rustc".to_string()),
        };
        let a = make("/repo/a.rs", "PR A");
        let b = make("/repo/b.rs", "PR B");
        api.retain_diagnostics("/shared", Some("sha-a"), vec![a.clone()]);
        api.publish_attributed(
            Path::new("/shared"),
            crate::statusfile::VerdictPayload::red(1),
            Some("sha-a".to_string()),
            false,
        );
        api.retain_diagnostics("/shared", Some("sha-b"), vec![b.clone()]);
        api.publish_attributed(
            Path::new("/shared"),
            crate::statusfile::VerdictPayload::red(1),
            Some("sha-b".to_string()),
            false,
        );

        assert_eq!(
            api.get_diagnostics_attributed("/shared", Some("sha-a")),
            vec![a]
        );
        assert_eq!(
            api.get_diagnostics_attributed("/shared", Some("sha-b")),
            vec![b.clone()]
        );
        assert_eq!(
            api.get_diagnostics("/shared"),
            vec![b],
            "unattributed readers see the current live slot only"
        );
    }

    #[test]
    fn publish_unknown_payload_carries_reason_on_wire() {
        // **INFRA-36 invariant test:** the new `Unknown` verdict path
        // — what the daemon publishes when project-checks couldn't
        // evaluate, or when RA-native reported an unattributed error
        // — must surface on the SSE-mirror state with both the
        // verdict color and the reason classifier. SigNoz dashboards
        // / a remote `subscribe` client both depend on these being
        // honest.
        let api = ServeVerdictState::new();
        api.publish(
            Path::new("/r/wt-broken"),
            crate::statusfile::VerdictPayload::unknown("project_check_setup_error: oops"),
        );
        let status = api.get_status("/r/wt-broken").expect("status present");
        assert_eq!(status.verdict, "unknown");
        assert_eq!(status.red_diagnostics, 0);
        assert_eq!(
            status.verdict_failure_reason.as_deref(),
            Some("project_check_setup_error: oops"),
            "INFRA-36: the SSE-mirror state MUST carry the reason \
             classifier so a remote subscriber sees the same honest \
             answer the local `cargoless status` reader sees"
        );
    }

    // ──────────── #240/2b — overlay-push ingest tests ────────────

    #[test]
    fn push_overlay_stores_files_signals_and_acks() {
        let api = ServeVerdictState::new();
        let (tx, rx) = channel::<String>();
        api.attach_push_signal(tx);

        let files = vec![
            ("/wt-a/src/lib.rs".to_string(), "pub fn x() {}".to_string()),
            (
                "/wt-a/Cargo.toml".to_string(),
                "[package]\nname=\"x\"\n".to_string(),
            ),
        ];
        let ack = api.push_overlay("/wt-a", "origin/main", &files);

        // Ack: accepted=true + applied_files=N.
        assert_eq!(ack.worktree, "/wt-a");
        assert!(
            ack.accepted,
            "VerdictService override returns accepted=true"
        );
        assert_eq!(ack.applied_files, 2);

        // Store contains the overlay (peek doesn't consume).
        let peeked = api.peek_overlay_for("/wt-a").expect("stored");
        assert_eq!(peeked.base_ref, "origin/main");
        assert_eq!(peeked.files.len(), 2);
        assert_eq!(peeked.files, files);
        assert_eq!(peeked.check_profile, None);

        // Signal fired with the WT key.
        let signal = rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .expect("push_signal wakeup");
        assert_eq!(signal, "/wt-a");
    }

    #[test]
    fn push_overlay_with_profile_stores_per_request_cargo_profile() {
        let api = ServeVerdictState::new();
        let profile = CheckProfile {
            subcommand: CargoSubcommand::Check,
            package: Some("alchemy".into()),
            target: Some("wasm32-unknown-unknown".into()),
            features: vec!["hydrate".into()],
            no_default_features: true,
            release: true,
            extra_args: vec!["--tests".into()],
        };
        let files = vec![("/wt/Cargo.toml".to_string(), "[workspace]\n".to_string())];

        let ack = api.push_overlay_with_profile("/wt", "origin/dev", &files, Some(&profile));

        assert!(ack.accepted);
        let pushed = api.peek_overlay_for("/wt").expect("stored");
        assert_eq!(pushed.check_profile, Some(profile));
    }

    #[test]
    fn push_overlay_with_options_maps_repo_relative_paths_to_analysis_root() {
        let api = ServeVerdictState::new();
        let files = vec![("src/lib.rs".to_string(), "pub fn x() {}".to_string())];
        let options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some("/workspace/tf-multiverse".into()),
            base_sha: Some("abc123".into()),
            source_ref: None,
            source_sha: None,
            comparison_base_sha: None,
            candidate_snapshot: None,
            changed_files: Some(vec!["src/lib.rs".into()]),
            gate: false,
            check_ids: None,
            semantic: None,
        };

        let ack = api.push_overlay_with_options("/client/wt", "", &files, None, Some(&options));

        assert!(ack.accepted);
        let pushed = api.peek_overlay_for("/client/wt").expect("stored");
        assert_eq!(
            pushed.files,
            vec![(
                "/workspace/tf-multiverse/src/lib.rs".to_string(),
                "pub fn x() {}".to_string()
            )]
        );
        assert_eq!(
            pushed.analysis_root.as_deref(),
            Some(Path::new("/workspace/tf-multiverse"))
        );
        assert_eq!(pushed.base_sha.as_deref(), Some("abc123"));
        assert_eq!(pushed.changed_files, Some(vec!["src/lib.rs".into()]));
    }

    #[test]
    fn push_overlay_skips_fetch_reset_when_analysis_root_already_at_base_sha() {
        let root = temp_root("sync-skip");
        git(&root, &["init"]);
        git(
            &root,
            &["config", "user.email", "cargoless@example.invalid"],
        );
        git(&root, &["config", "user.name", "Cargoless Test"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn base() {}\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        let head = git_stdout(&root, &["rev-parse", "HEAD"]).unwrap();

        let api = ServeVerdictState::new();
        let files = vec![(
            "src/lib.rs".to_string(),
            "pub fn changed() {}\n".to_string(),
        )];
        let options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some(root.to_string_lossy().into_owned()),
            base_sha: Some(head),
            source_ref: None,
            source_sha: None,
            comparison_base_sha: None,
            candidate_snapshot: None,
            changed_files: Some(vec!["src/lib.rs".into()]),
            gate: false,
            check_ids: None,
            semantic: None,
        };

        let ack = api.push_overlay_with_options(
            "/client/wt",
            "origin/main",
            &files,
            None,
            Some(&options),
        );

        assert!(
            ack.accepted,
            "matching base_sha should avoid `git fetch origin main`; this test repo has no origin"
        );
        assert!(api.peek_overlay_for("/client/wt").is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn exact_git_gate_fetches_verified_sha_and_checks_it_in_isolated_scratch() {
        let root = temp_root("exact-git-gate");
        let remote = temp_root("exact-git-gate-remote");
        git(&remote, &["init", "--bare"]);
        git(&root, &["init"]);
        git(
            &root,
            &["config", "user.email", "cargoless@example.invalid"],
        );
        git(&root, &["config", "user.name", "Cargoless Test"]);
        git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn base() {}\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        let base_sha = git_capture(&root, &["rev-parse", "HEAD"]);

        std::fs::write(root.join("src/lib.rs"), "pub fn candidate() {}\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "candidate"]);
        let source_sha = git_capture(&root, &["rev-parse", "HEAD"]);
        git(&root, &["push", "origin", "HEAD:main"]);
        git(&root, &["reset", "--hard", &base_sha]);

        let api = ServeVerdictState::new();
        let (direct_tx, direct_rx) = channel();
        api.attach_direct_gate_signal(direct_tx);
        let options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some(root.to_string_lossy().into_owned()),
            base_sha: Some(source_sha.clone()),
            source_ref: Some("refs/heads/main".to_string()),
            source_sha: Some(source_sha.clone()),
            comparison_base_sha: None,
            candidate_snapshot: None,
            changed_files: Some(vec!["src/lib.rs".to_string()]),
            gate: true,
            check_ids: Some(vec!["ssr-compiler-witness".to_string()]),
            semantic: None,
        };

        let ack = api.push_overlay_with_options(
            "/workspace/tf-multiverse",
            "origin/dev",
            &[],
            None,
            Some(&options),
        );
        assert!(ack.accepted, "{:?}", ack.reject_body);
        assert_eq!(ack.applied_files, 0);
        assert!(api.peek_overlay_for("/workspace/tf-multiverse").is_none());

        let request = direct_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("direct gate dispatch");
        assert_eq!(
            request.context.source_sha.as_deref(),
            Some(source_sha.as_str())
        );
        let seen = api
            .with_project_check_overlay(
                &request.context,
                |scratch, _warm, _candidate_manifest_path| {
                    (
                        git_capture(scratch, &["rev-parse", "HEAD"]),
                        std::fs::read_to_string(scratch.join("src/lib.rs")).unwrap(),
                    )
                },
            )
            .unwrap();
        assert_eq!(seen.0, source_sha);
        assert_eq!(seen.1, "pub fn candidate() {}\n");
        assert_eq!(
            git_capture(&root, &["rev-parse", "HEAD"]),
            base_sha,
            "the shared analysis root must not move"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            "pub fn base() {}\n"
        );

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(remote);
    }

    #[cfg(unix)]
    #[test]
    fn exact_git_accepts_preexisting_legacy_0755_scratch_namespace() {
        use std::os::unix::fs::PermissionsExt as _;

        let project = setup_batch_project("exact-git-legacy-scratch-mode");
        let source_sha = git_capture(&project.root, &["rev-parse", "HEAD"]);
        let state_dir = temp_root("exact-git-legacy-scratch-mode-state");
        let scratch_namespace = state_dir.join("project-check-runs");
        std::fs::create_dir(&scratch_namespace).unwrap();
        std::fs::set_permissions(&scratch_namespace, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let context = ProjectCheckRunContext {
            root: project.root.clone(),
            changed_files: Some(vec!["src/lib.rs".to_string()]),
            base_ref: "origin/main".to_string(),
            base_sha: Some(source_sha.clone()),
            source_ref: Some("refs/heads/main".to_string()),
            source_sha: Some(source_sha.clone()),
            candidate_snapshot: None,
            overlay_files: Vec::new(),
            materialize_overlay: true,
            gate: true,
            check_ids: Some(vec!["ssr-compiler-witness".to_string()]),
        };

        let seen = api
            .with_project_check_overlay(&context, |scratch, _warm, sidecar| {
                assert!(sidecar.is_none(), "exact Git has no typed sidecar");
                assert_eq!(git_capture(scratch, &["rev-parse", "HEAD"]), source_sha);
                scratch.to_path_buf()
            })
            .expect("a safe pre-existing legacy namespace remains compatible");

        assert!(!seen.exists(), "the isolated exact-Git checkout is cleaned");
        assert_eq!(
            std::fs::metadata(&scratch_namespace)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "compatibility must not mutate the legacy namespace mode"
        );
        let _ = std::fs::remove_dir_all(state_dir);
        drop(project);
    }

    #[test]
    fn exact_git_gate_rejects_source_body_or_cross_sha_attribution() {
        let api = ServeVerdictState::new();
        let sha = "a".repeat(40);
        let other = "b".repeat(40);
        let base = PushOverlayOptions {
            source_ref: Some("refs/heads/main".to_string()),
            source_sha: Some(sha.clone()),
            base_sha: Some(other),
            ..Default::default()
        };
        let mismatch = api.push_overlay_with_options("/wt", "", &[], None, Some(&base));
        assert!(!mismatch.accepted);

        let mut with_body = base;
        with_body.base_sha = Some(sha.clone());
        let body = vec![("src/lib.rs".to_string(), "ignored".to_string())];
        let rejected = api.push_overlay_with_options("/wt", "", &body, None, Some(&with_body));
        assert!(!rejected.accepted);

        let missing_gate = PushOverlayOptions {
            analysis_root: Some("/workspace/repo".to_string()),
            source_ref: Some("refs/heads/main".to_string()),
            source_sha: Some(sha.clone()),
            base_sha: Some(sha.clone()),
            gate: false,
            ..Default::default()
        };
        let rejected = api.push_overlay_with_options("/wt", "", &[], None, Some(&missing_gate));
        assert!(!rejected.accepted);

        let missing_root = PushOverlayOptions {
            source_ref: Some("refs/heads/main".to_string()),
            source_sha: Some(sha.clone()),
            base_sha: Some(sha),
            gate: true,
            ..Default::default()
        };
        let rejected = api.push_overlay_with_options("/wt", "", &[], None, Some(&missing_root));
        assert!(!rejected.accepted);
    }

    #[test]
    fn source_ref_accepts_only_valid_heads_and_pull_refs() {
        assert!(validate_source_ref("refs/heads/dev").is_ok());
        assert!(validate_source_ref("refs/pull/123/head").is_ok());
        for invalid in [
            "dev",
            "refs/tags/v1",
            "refs/heads/../main",
            "refs/heads/bad name",
            "refs/heads/.hidden",
        ] {
            assert!(
                validate_source_ref(invalid).is_err(),
                "{invalid} must be rejected"
            );
        }
    }

    #[test]
    fn typed_overlay_keeps_attribution_comparison_and_operation_bases_distinct() {
        let root = temp_root("candidate-distinct-bases");
        git(&root, &["init"]);
        git(
            &root,
            &["config", "user.email", "cargoless@example.invalid"],
        );
        git(&root, &["config", "user.name", "Cargoless Test"]);

        std::fs::write(root.join("comparison.txt"), b"comparison\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "comparison base"]);
        let comparison_commit = git_capture(&root, &["rev-parse", "HEAD"]);
        let comparison_tree = git_capture(&root, &["rev-parse", "HEAD^{tree}"]);

        std::fs::write(root.join("overlay-base-only.txt"), b"overlay base\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "overlay base"]);
        let overlay_base_commit = git_capture(&root, &["rev-parse", "HEAD"]);

        std::fs::write(root.join("attribution-only.txt"), b"legacy attribution\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "legacy attribution"]);
        let legacy_base_sha = git_capture(&root, &["rev-parse", "HEAD"]);

        git(&root, &["reset", "--hard", &overlay_base_commit]);
        std::fs::write(root.join("candidate.txt"), b"candidate bytes\n").unwrap();
        let mut manifest =
            crate::candidate_snapshot_git::build_overlay_manifest(&root, &overlay_base_commit)
                .unwrap()
                .expect("candidate fixture has an overlay")
                .manifest;
        manifest.comparison_base = GitTreeRef {
            commit_sha: comparison_commit.clone(),
            tree_oid: comparison_tree.clone(),
        };
        manifest.manifest_digest = compute_manifest_digest(&manifest).unwrap();
        std::fs::remove_file(root.join("candidate.txt")).unwrap();
        git(&root, &["reset", "--hard", &legacy_base_sha]);

        let CandidateSnapshot::Overlay {
            base: operation_base,
            ..
        } = &manifest.candidate
        else {
            panic!("fixture must remain an overlay");
        };
        assert_ne!(legacy_base_sha, comparison_commit);
        assert_ne!(legacy_base_sha, operation_base.commit_sha);
        assert_ne!(comparison_commit, operation_base.commit_sha);

        let options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some(root.to_string_lossy().into_owned()),
            base_sha: Some(legacy_base_sha.clone()),
            comparison_base_sha: Some(comparison_commit.clone()),
            candidate_snapshot: Some(manifest.clone()),
            changed_files: Some(vec!["candidate.txt".into()]),
            ..Default::default()
        };
        let typed = typed_candidate_overlay(
            &options,
            &[("candidate.txt".into(), "candidate bytes\n".into())],
        )
        .expect("legacy attribution is independent from comparison authority")
        .expect("typed candidate is present");
        assert_eq!(typed.manifest, manifest);

        ensure_candidate_snapshot_base(&root, &legacy_base_sha, &manifest)
            .expect("operations validate against candidate.base, not comparison_base");
        assert_eq!(
            git_capture(&root, &["rev-parse", "HEAD"]),
            operation_base.commit_sha,
            "analysis root resets to the operation base before scratch materialization"
        );

        let mut wrong_tree = manifest.clone();
        let CandidateSnapshot::Overlay { base, .. } = &mut wrong_tree.candidate else {
            unreachable!()
        };
        base.tree_oid = comparison_tree.clone();
        wrong_tree.manifest_digest = compute_manifest_digest(&wrong_tree).unwrap();
        let error = ensure_candidate_snapshot_base(&root, &legacy_base_sha, &wrong_tree)
            .expect_err("advertised candidate.base tree must match the resolved commit");
        assert!(
            error.contains("candidate_snapshot.base_tree_mismatch"),
            "unexpected wrong-tree taxonomy: {error}"
        );

        let unrelated_comparison = git_capture(
            &root,
            &[
                "commit-tree",
                &comparison_tree,
                "-m",
                "unrelated comparison",
            ],
        );
        let mut unrelated = manifest.clone();
        unrelated.comparison_base.commit_sha = unrelated_comparison;
        unrelated.manifest_digest = compute_manifest_digest(&unrelated).unwrap();
        let error = ensure_candidate_snapshot_base(&root, &legacy_base_sha, &unrelated)
            .expect_err("comparison base must be an ancestor of candidate.base");
        assert!(
            error.contains("candidate_snapshot.comparison_base_invalid"),
            "unexpected ancestry taxonomy: {error}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn candidate_backed_batch_is_rejected_before_analysis_root_touch() {
        let api = ServeVerdictState::new();
        let forbidden_root = temp_root("candidate-batch-must-not-touch");
        assert!(!forbidden_root.exists());

        let manifest = candidate_snapshot_golden();
        let mut request = BatchCheckRequest::new("candidate-batch", "origin/main");
        request.coalesce_key = Some("must-not-coalesce".to_string());
        request.options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some(forbidden_root.to_string_lossy().into_owned()),
            base_sha: Some("e".repeat(40)),
            source_ref: None,
            source_sha: None,
            comparison_base_sha: Some(manifest.comparison_base.commit_sha.clone()),
            candidate_snapshot: Some(manifest),
            changed_files: Some(vec!["empty.bin".to_string()]),
            gate: true,
            check_ids: Some(vec!["candidate-policy".to_string()]),
            semantic: None,
        };
        request.members = vec![BatchMember {
            worktree: "/client/candidate".to_string(),
            files: vec![("empty.bin".to_string(), String::new())],
            changed_files: vec!["empty.bin".to_string()],
        }];

        let report = api.batch_check(&request);
        assert_eq!(report.verdict, BatchVerdict::Indeterminate);
        assert_eq!(report.combined_checks, 0);
        assert_eq!(report.solo_checks, 0);
        assert!(
            report
                .members
                .iter()
                .flat_map(|member| &member.diagnostics)
                .any(|diagnostic| diagnostic
                    .message
                    .contains("candidate_snapshot.coalescing_forbidden")),
            "typed candidate batches must fail with the stable code before any legacy union path: {report:?}"
        );
        assert!(
            !forbidden_root.exists(),
            "candidate-backed batch rejection must happen before analysis-root access"
        );
        let counts = api.batch_coalescer.counts();
        assert_eq!(counts.waiters, 0);
        assert_eq!(counts.members, 0);
        assert_eq!(
            counts.inflight_runs, 0,
            "candidate-backed batches may never enter the coalescer"
        );
    }

    #[test]
    fn typed_candidate_subject_binds_manifest_snapshot_and_tree_identity() {
        let files = vec![("empty.bin".to_string(), String::new())];
        let mut options = PushOverlayOptions {
            analysis_root: Some("/server/repository".to_string()),
            base_sha: Some("legacy-attribution".to_string()),
            changed_files: Some(vec!["empty.bin".to_string()]),
            candidate_snapshot: Some(candidate_snapshot_golden()),
            ..PushOverlayOptions::default()
        };
        let digest = |options: &PushOverlayOptions| {
            let Subject::Overlay { overlay_digest, .. } =
                overlay_subject_v3("/client/worktree", "origin/main", &files, None, options)
                    .unwrap()
            else {
                unreachable!()
            };
            overlay_digest.as_str().to_string()
        };
        let original = digest(&options);

        let manifest = options.candidate_snapshot.as_mut().unwrap();
        manifest.manifest_digest = format!("sha256:{}", "a".repeat(64));
        assert_ne!(
            digest(&options),
            original,
            "manifest digest is subject identity"
        );
        *options.candidate_snapshot.as_mut().unwrap() = candidate_snapshot_golden();

        let CandidateSnapshot::Overlay {
            snapshot_digest, ..
        } = &mut options.candidate_snapshot.as_mut().unwrap().candidate
        else {
            unreachable!()
        };
        *snapshot_digest = format!("sha256:{}", "b".repeat(64));
        assert_ne!(
            digest(&options),
            original,
            "snapshot digest is subject identity"
        );
        *options.candidate_snapshot.as_mut().unwrap() = candidate_snapshot_golden();

        let CandidateSnapshot::Overlay { tree_oid, .. } =
            &mut options.candidate_snapshot.as_mut().unwrap().candidate
        else {
            unreachable!()
        };
        *tree_oid = "c".repeat(40);
        assert_ne!(
            digest(&options),
            original,
            "candidate tree is subject identity"
        );
    }

    #[test]
    fn identical_typed_candidates_never_replace_queue_or_hard_witness() {
        let root = temp_root("candidate-noncoalescing");
        git(&root, &["init"]);
        git(
            &root,
            &["config", "user.email", "cargoless@example.invalid"],
        );
        git(&root, &["config", "user.name", "Cargoless Test"]);
        std::fs::write(root.join("remove.txt"), b"remove me\n").unwrap();
        std::fs::create_dir_all(root.join(".cargoless")).unwrap();
        std::fs::write(
            root.join(".cargoless/candidate-snapshot.json"),
            b"tracked candidate content\n",
        )
        .unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "candidate base"]);
        let manifest = overlay_manifest_with_delete_empty_executable_and_binary(&root);
        let legacy_files = vec![
            ("empty.bin".to_string(), String::new()),
            ("remove.txt".to_string(), String::new()),
            ("script.sh".to_string(), "#!/bin/sh\nexit 0\n".to_string()),
        ];
        let options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some(root.to_string_lossy().into_owned()),
            base_sha: Some("same-legacy-attribution".to_string()),
            comparison_base_sha: Some(manifest.comparison_base.commit_sha.clone()),
            candidate_snapshot: Some(manifest.clone()),
            changed_files: Some(
                manifest
                    .candidate
                    .operations()
                    .iter()
                    .map(|operation| operation.path().to_string())
                    .collect(),
            ),
            ..PushOverlayOptions::default()
        };
        let api = ServeVerdictState::new();
        assert!(
            api.push_overlay_with_options(
                "/client/candidate",
                &manifest.comparison_base.commit_sha,
                &legacy_files,
                None,
                Some(&options),
            )
            .accepted
        );
        assert!(
            api.push_overlay_with_options(
                "/client/candidate",
                &manifest.comparison_base.commit_sha,
                &legacy_files,
                None,
                Some(&options),
            )
            .accepted
        );

        let first = api
            .take_overlay_for("/client/candidate")
            .expect("first candidate remains queued");
        let second = api
            .take_overlay_for("/client/candidate")
            .expect("identical candidate is a separate execution");
        assert!(api.take_overlay_for("/client/candidate").is_none());
        api.record_push_attribution("/candidate-first", &first);
        api.record_push_attribution("/candidate-second", &second);
        let first = api.take_push_attribution("/candidate-first").unwrap();
        let second = api.take_push_attribution("/candidate-second").unwrap();
        assert_eq!(
            first.candidate.as_ref().unwrap().manifest_digest,
            second.candidate.as_ref().unwrap().manifest_digest
        );
        let first_key = first.witness_key().unwrap();
        let second_key = second.witness_key().unwrap();
        assert_ne!(
            first_key, second_key,
            "even identical manifests never coalesce"
        );
        let first_generation = api.begin_hard_witness(
            "/client/candidate",
            Some(&first_key),
            first.semantic.as_ref(),
        );
        let second_generation = api.begin_hard_witness(
            "/client/candidate",
            Some(&second_key),
            second.semantic.as_ref(),
        );
        assert!(api.finish_hard_witness("/client/candidate", Some(&first_key), first_generation));
        assert!(api.finish_hard_witness("/client/candidate", Some(&second_key), second_generation));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn candidate_status_events_and_diagnostics_are_manifest_addressable() {
        let api = ServeVerdictState::new();
        let events = api.subscribe();
        let first = CandidateVerdictIdentity {
            manifest_digest: format!("sha256:{}", "1".repeat(64)),
            snapshot_digest: format!("sha256:{}", "a".repeat(64)),
            tree_oid: "a".repeat(40),
            execution_id: 1,
        };
        let second = CandidateVerdictIdentity {
            manifest_digest: format!("sha256:{}", "2".repeat(64)),
            snapshot_digest: format!("sha256:{}", "b".repeat(64)),
            tree_oid: "b".repeat(40),
            execution_id: 2,
        };
        let diagnostic = |message: &str| Diagnostic {
            file_path: PathBuf::from("policy.yaml"),
            line: 1,
            col: 1,
            severity: Severity::Error,
            code: Some("policy.failed".to_string()),
            message: message.to_string(),
            source: Some("candidate-policy".to_string()),
        };
        api.retain_candidate_diagnostics("/shared", &first, vec![diagnostic("first")]);
        api.publish_attributed_with_candidate_checks(
            Path::new("/shared"),
            crate::statusfile::VerdictPayload::red(1),
            Some("same-base".to_string()),
            Some(first.clone()),
            false,
            Vec::new(),
            Vec::new(),
            None,
        );
        api.retain_candidate_diagnostics("/shared", &second, vec![diagnostic("second")]);
        api.publish_attributed_with_candidate_checks(
            Path::new("/shared"),
            crate::statusfile::VerdictPayload::red(1),
            Some("same-base".to_string()),
            Some(second.clone()),
            false,
            Vec::new(),
            Vec::new(),
            None,
        );

        let first_status = api
            .get_status_candidate_attributed("/shared", &first.manifest_digest)
            .unwrap();
        let second_status = api
            .get_status_candidate_attributed("/shared", &second.manifest_digest)
            .unwrap();
        assert_eq!(
            first_status.candidate_snapshot_digest.as_deref(),
            Some(first.snapshot_digest.as_str())
        );
        assert_eq!(
            second_status.candidate_tree_oid.as_deref(),
            Some(second.tree_oid.as_str())
        );
        assert_eq!(
            api.get_diagnostics_candidate_attributed("/shared", &first.manifest_digest)[0].message,
            "first"
        );
        assert_eq!(
            api.get_diagnostics_candidate_attributed("/shared", &second.manifest_digest)[0].message,
            "second"
        );
        let first_event = events.recv_timeout(Duration::from_secs(1)).unwrap();
        let second_event = events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            first_event.candidate_manifest_digest.as_deref(),
            Some(first.manifest_digest.as_str())
        );
        assert_eq!(
            second_event.candidate_manifest_digest.as_deref(),
            Some(second.manifest_digest.as_str())
        );
    }

    #[test]
    fn candidate_snapshot_pairing_rejects_missing_or_mismatched_manifest_before_materialization() {
        let api = ServeVerdictState::new();
        let state = temp_root("candidate-pre-materialize");
        let forbidden_root = state.join("must-not-be-created");
        let comparison_base = "f".repeat(40);

        let missing = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some(forbidden_root.to_string_lossy().into_owned()),
            comparison_base_sha: Some(comparison_base.clone()),
            candidate_snapshot: None,
            ..Default::default()
        };
        let ack = api.push_overlay_with_options(
            "/client/candidate-missing",
            "origin/main",
            &[],
            None,
            Some(&missing),
        );
        assert!(!ack.accepted);
        assert!(
            ack.reject_body
                .as_deref()
                .is_some_and(|body| body.contains("candidate_snapshot.manifest_missing")),
            "comparison_base_sha without a typed manifest must fail with stable taxonomy: {ack:?}"
        );
        assert!(
            !forbidden_root.exists(),
            "pairing validation must run before repository sync or materialization"
        );

        let mismatched = PushOverlayOptions {
            candidate_snapshot: Some(candidate_snapshot_golden()),
            ..missing
        };
        let ack = api.push_overlay_with_options(
            "/client/candidate-mismatch",
            "origin/main",
            &[],
            None,
            Some(&mismatched),
        );
        assert!(!ack.accepted);
        assert!(
            ack.reject_body
                .as_deref()
                .is_some_and(|body| body.contains("candidate_snapshot.comparison_base_mismatch")),
            "transport comparison base must equal the manifest authority: {ack:?}"
        );
        assert!(
            !forbidden_root.exists(),
            "mismatch rejection must remain pre-materialization"
        );
        let _ = std::fs::remove_dir_all(state);
    }

    #[test]
    fn candidate_backed_project_checks_are_never_coalesced() {
        let project = setup_batch_project("candidate-no-coalesce");
        let context = ProjectCheckRunContext {
            root: project.root.clone(),
            changed_files: Some(vec!["src/lib.rs".into()]),
            base_ref: "origin/main".into(),
            base_sha: None,
            source_ref: None,
            source_sha: None,
            candidate_snapshot: Some(candidate_snapshot_golden()),
            overlay_files: Vec::new(),
            materialize_overlay: true,
            gate: true,
            check_ids: Some(vec!["no-fail-token".into()]),
        };

        assert!(
            ServeVerdictState::new()
                .coalesced_project_check(Path::new("/client/candidate"), &context)
                .is_none(),
            "a digest-bound candidate must retain its own materialization and cannot join a union overlay"
        );
    }

    fn candidate_materialization_context(label: &str) -> (PathBuf, ProjectCheckRunContext) {
        let root = temp_root(label);
        git(&root, &["init"]);
        git(
            &root,
            &["config", "user.email", "cargoless@example.invalid"],
        );
        git(&root, &["config", "user.name", "Cargoless Test"]);
        std::fs::write(root.join("remove.txt"), b"remove me\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "candidate base"]);
        let manifest = overlay_manifest_with_delete_empty_executable_and_binary(&root);
        let context = ProjectCheckRunContext {
            root: root.clone(),
            changed_files: Some(vec![
                "binary.bin".into(),
                "empty.bin".into(),
                "remove.txt".into(),
                "script.sh".into(),
            ]),
            base_ref: manifest.comparison_base.commit_sha.clone(),
            base_sha: None,
            source_ref: None,
            source_sha: None,
            candidate_snapshot: Some(manifest),
            overlay_files: Vec::new(),
            materialize_overlay: true,
            gate: true,
            check_ids: None,
        };
        (root, context)
    }

    #[test]
    fn candidate_snapshot_requires_configured_external_daemon_state() {
        let (root, context) = candidate_materialization_context("candidate-state-required");
        let api = ServeVerdictState::new();
        let mut invoked = false;

        let result = api.with_project_check_overlay(
            &context,
            |_scratch, _warm, _candidate_manifest_path| {
                invoked = true;
            },
        );

        let error = result.expect_err("typed candidates must not use repo-internal fallback state");
        assert!(
            error.starts_with("candidate_snapshot.environment_unsafe:"),
            "missing protected daemon state must use the frozen code: {error}"
        );
        assert!(!invoked, "the policy child must not execute");
        assert!(
            !root
                .join(".cargoless/candidate-project-check-runs")
                .exists(),
            "typed candidates must not create transient authority inside repository contents"
        );
        assert!(
            !root.join(".cargoless/project-check-runs").exists(),
            "typed candidates must not fall back to the legacy scratch namespace"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_candidate_rejects_before_creating_protected_state() {
        let (root, context) = candidate_materialization_context("candidate-non-linux-unsupported");
        let state_dir = temp_root("candidate-non-linux-unsupported-state");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let mut invoked = false;

        let error = api
            .with_project_check_overlay(&context, |_scratch, _warm, _sidecar| {
                invoked = true;
            })
            .expect_err("typed candidate authority is unsupported without Linux sealing");

        assert!(
            error.starts_with("candidate_snapshot.environment_unsafe:"),
            "{error}"
        );
        assert!(!invoked, "the candidate callback must not execute");
        assert!(
            !state_dir.join("candidate-project-check-runs").exists(),
            "rejection occurs before scratch authority creation"
        );
        assert!(
            !state_dir.join("project-check-runs").exists(),
            "typed rejection never creates legacy scratch authority"
        );
        assert!(
            !state_dir.join("candidate-snapshots").exists(),
            "rejection occurs before sidecar authority creation"
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn non_linux_protected_cleanup_preserves_post_verify_replacement() {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

        let state_dir = temp_root("non-linux-cleanup-post-verify-replacement");
        let namespace = state_dir.join("candidate-snapshots");
        let run = namespace.join("run-recorded");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&namespace, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(run.join("original.sentinel"), b"original\n").unwrap();
        let protected = ProtectedRunDirectory::capture(
            std::fs::canonicalize(&run).unwrap(),
            &std::fs::canonicalize(&namespace).unwrap(),
        )
        .unwrap();
        let saved = namespace.join("saved-recorded-run");

        let error = remove_bound_protected_run_with_after_verify(&protected, |path| {
            std::fs::rename(path, &saved).unwrap();
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700).create(path).unwrap();
            std::fs::write(path.join("replacement.sentinel"), b"must survive\n").unwrap();
        })
        .expect_err("non-Linux cleanup has no atomic bound deletion primitive");

        assert!(
            error.starts_with("candidate_snapshot.environment_unsafe:"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(run.join("replacement.sentinel")).unwrap(),
            b"must survive\n"
        );
        assert_eq!(
            std::fs::read(saved.join("original.sentinel")).unwrap(),
            b"original\n"
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn candidate_snapshot_rejects_configured_state_inside_candidate_repository() {
        let (root, context) = candidate_materialization_context("candidate-state-in-repo");
        let state_dir = root.join(".daemon-state");
        std::fs::create_dir(&state_dir).unwrap();
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let mut invoked = false;

        let result = api.with_project_check_overlay(
            &context,
            |_scratch, _warm, _candidate_manifest_path| {
                invoked = true;
            },
        );

        let error = result.expect_err("configured candidate state must still be external");
        assert!(
            error.starts_with("candidate_snapshot.environment_unsafe:"),
            "in-repository state rejection must use the frozen code: {error}"
        );
        assert!(!invoked, "the policy child must not execute");
        assert!(
            !state_dir.join("candidate-snapshots").exists(),
            "unsafe state is rejected before sidecar setup"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn candidate_snapshot_rejects_symlinked_sidecar_parent_before_child_execution() {
        use std::os::unix::fs::symlink;

        let (root, context) = candidate_materialization_context("candidate-sidecar-symlink");
        let state_dir = temp_root("candidate-sidecar-symlink-state");
        let outside = temp_root("candidate-sidecar-symlink-outside");
        std::fs::write(outside.join("sentinel"), b"must remain untouched\n").unwrap();
        symlink(&outside, state_dir.join("candidate-snapshots")).unwrap();
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let mut invoked = false;

        let result = api.with_project_check_overlay(
            &context,
            |_scratch, _warm, _candidate_manifest_path| {
                invoked = true;
            },
        );

        let error = result.expect_err("sidecar namespace symlinks must fail closed");
        assert!(
            error.starts_with("candidate_snapshot.environment_unsafe:"),
            "symlink rejection must use the frozen code: {error}"
        );
        assert!(!invoked, "the policy child must not execute");
        assert_eq!(
            std::fs::read(outside.join("sentinel")).unwrap(),
            b"must remain untouched\n"
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(state_dir);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[cfg(unix)]
    #[test]
    fn candidate_snapshot_rejects_non_directory_or_permissive_sidecar_parent() {
        use std::os::unix::fs::PermissionsExt as _;

        for (label, create_parent) in [
            (
                "file",
                Box::new(|parent: &Path| std::fs::write(parent, b"not a directory\n"))
                    as Box<dyn FnOnce(&Path) -> std::io::Result<()>>,
            ),
            (
                "permissive",
                Box::new(|parent: &Path| {
                    std::fs::create_dir(parent)?;
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755))
                }),
            ),
        ] {
            let (root, context) =
                candidate_materialization_context(&format!("candidate-sidecar-{label}"));
            let state_dir = temp_root(&format!("candidate-sidecar-{label}-state"));
            create_parent(&state_dir.join("candidate-snapshots")).unwrap();
            let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
            let mut invoked = false;

            let result = api.with_project_check_overlay(
                &context,
                |_scratch, _warm, _candidate_manifest_path| {
                    invoked = true;
                },
            );

            let error = result.expect_err("unsafe sidecar namespace must fail closed");
            assert!(
                error.starts_with("candidate_snapshot.environment_unsafe:"),
                "{label} parent rejection must use the frozen code: {error}"
            );
            assert!(!invoked, "the policy child must not execute");
            let _ = std::fs::remove_dir_all(root);
            let _ = if label == "file" {
                std::fs::remove_file(state_dir.join("candidate-snapshots"))
            } else {
                Ok(())
            };
            let _ = std::fs::remove_dir_all(state_dir);
        }
    }

    #[cfg(unix)]
    #[test]
    fn candidate_snapshot_materializes_delete_empty_mode_and_binary_then_scopes_cleanup() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = temp_root("candidate-materialize");
        git(&root, &["init"]);
        git(
            &root,
            &["config", "user.email", "cargoless@example.invalid"],
        );
        git(&root, &["config", "user.name", "Cargoless Test"]);
        std::fs::write(root.join("remove.txt"), b"remove me\n").unwrap();
        std::fs::create_dir_all(root.join(".cargoless")).unwrap();
        std::fs::write(
            root.join(".cargoless/candidate-snapshot.json"),
            b"tracked candidate content\n",
        )
        .unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "candidate base"]);
        let manifest = overlay_manifest_with_delete_empty_executable_and_binary(&root);
        let state_dir = temp_root("candidate-materialize-state");
        std::fs::write(state_dir.join("keep.sentinel"), b"unrelated state\n").unwrap();
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let context = ProjectCheckRunContext {
            root: root.clone(),
            changed_files: Some(vec![
                "binary.bin".into(),
                "empty.bin".into(),
                "remove.txt".into(),
                "script.sh".into(),
            ]),
            base_ref: manifest.comparison_base.commit_sha.clone(),
            base_sha: None,
            source_ref: None,
            source_sha: None,
            candidate_snapshot: Some(manifest.clone()),
            overlay_files: Vec::new(),
            materialize_overlay: true,
            gate: true,
            check_ids: None,
        };

        let (scratch, manifest_path, manifest_run_dir) = api
            .with_project_check_overlay(&context, |scratch, _warm, manifest_path| {
                let manifest_path = manifest_path.expect("typed run has an external manifest");
                assert!(
                    !scratch.join("remove.txt").exists(),
                    "typed delete is not an empty-file upsert"
                );
                assert_eq!(std::fs::read(scratch.join("empty.bin")).unwrap(), b"");
                assert_eq!(
                    std::fs::read(scratch.join("binary.bin")).unwrap(),
                    [0_u8, 0xff, 0x80, b'\n'],
                    "candidate payloads are arbitrary bytes, not UTF-8 strings"
                );
                assert_eq!(
                    std::fs::metadata(scratch.join("script.sh"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o111,
                    0o111,
                    "mode 100755 must remain executable"
                );
                assert_eq!(
                    std::fs::read(scratch.join(".cargoless/candidate-snapshot.json")).unwrap(),
                    b"tracked candidate content\n",
                    "the protocol sidecar must never overwrite tracked candidate content"
                );
                assert!(
                    !manifest_path.starts_with(scratch),
                    "the protocol sidecar is outside candidate contents"
                );
                assert!(
                    manifest_path
                        .to_string_lossy()
                        .contains("/candidate-snapshots/run-"),
                    "manifest path must use the state-dir run namespace: {}",
                    manifest_path.display()
                );
                let per_run = parse_and_validate_manifest_json(
                    &std::fs::read_to_string(manifest_path).unwrap(),
                )
                .unwrap();
                assert_eq!(per_run, manifest, "the per-run file is the bound manifest");
                assert_eq!(
                    std::fs::metadata(manifest_path)
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
                let manifest_run_dir = manifest_path.parent().unwrap();
                assert_eq!(
                    std::fs::metadata(manifest_run_dir)
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
                (
                    scratch.to_path_buf(),
                    manifest_path.to_path_buf(),
                    manifest_run_dir.to_path_buf(),
                )
            })
            .unwrap();

        assert!(
            !scratch.exists(),
            "only the completed run scratch is removed"
        );
        assert!(
            !manifest_path.exists(),
            "the per-run manifest is not retained"
        );
        assert!(
            !manifest_run_dir.exists(),
            "the per-run manifest directory is not retained"
        );
        assert_eq!(
            std::fs::read(state_dir.join("keep.sentinel")).unwrap(),
            b"unrelated state\n",
            "cleanup must not sweep sibling daemon state"
        );
        assert_eq!(
            std::fs::read(root.join("remove.txt")).unwrap(),
            b"remove me\n",
            "candidate operations never mutate the shared analysis root"
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[cfg(unix)]
    #[test]
    fn candidate_snapshot_uses_private_unpredictable_state_runs() {
        use std::os::unix::fs::PermissionsExt as _;

        let (root, context) = candidate_materialization_context("candidate-private-runs");
        let state_dir = temp_root("candidate-private-runs-state");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());

        let run_names = api
            .with_project_check_overlay(&context, |scratch, _warm, sidecar| {
                let sidecar = sidecar.expect("typed run has an external sidecar");
                let scratch_name = scratch
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let candidate_name = sidecar
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                assert_eq!(
                    scratch
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .file_name()
                        .and_then(|name| name.to_str()),
                    Some("candidate-project-check-runs")
                );
                for directory in [
                    scratch,
                    scratch.parent().unwrap(),
                    scratch.parent().unwrap().parent().unwrap(),
                    sidecar.parent().unwrap(),
                    sidecar.parent().unwrap().parent().unwrap(),
                ] {
                    assert_eq!(
                        std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                        0o700,
                        "protected namespace/run `{}` must be mode 0700",
                        directory.display()
                    );
                }
                (scratch_name, candidate_name)
            })
            .unwrap();

        for name in [&run_names.0, &run_names.1] {
            let suffix = name
                .strip_prefix("run-")
                .expect("protected run names use the closed run- namespace");
            assert!(
                suffix.len() >= 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "run authority must use an unpredictable hexadecimal suffix, got {name:?}"
            );
            assert!(
                !name.contains(&std::process::id().to_string()),
                "PID/sequence names are predictable and not authorities: {name:?}"
            );
        }
        assert_eq!(run_names.0, run_names.1, "one run uses one authority key");
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[cfg(unix)]
    #[test]
    fn candidate_cleanup_rejects_and_preserves_a_substituted_manifest_run() {
        use std::os::unix::fs::DirBuilderExt as _;

        let (root, context) = candidate_materialization_context("candidate-sidecar-substitute");
        let state_dir = temp_root("candidate-sidecar-substitute-state");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let mut original = None;
        let mut replacement = None;

        let error = api
            .with_project_check_overlay(&context, |_scratch, _warm, sidecar| {
                let run = sidecar.unwrap().parent().unwrap().to_path_buf();
                let saved = run.with_extension("original");
                std::fs::rename(&run, &saved).unwrap();
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700).create(&run).unwrap();
                std::fs::write(run.join("substitute.sentinel"), b"must survive\n").unwrap();
                original = Some(saved);
                replacement = Some(run);
                "verified"
            })
            .expect_err("candidate cleanup must reject a substituted run directory");

        assert!(
            error.starts_with("candidate_snapshot.cleanup_failed:"),
            "{error}"
        );
        let replacement = replacement.unwrap();
        assert_eq!(
            std::fs::read(replacement.join("substitute.sentinel")).unwrap(),
            b"must survive\n",
            "cleanup must never delete a path whose file identity changed"
        );
        std::fs::remove_dir_all(&replacement).unwrap();
        std::fs::rename(original.unwrap(), &replacement).unwrap();
        cleanup_candidate_manifest_run(&replacement).unwrap();
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[cfg(unix)]
    #[test]
    fn candidate_cleanup_preserves_post_quarantine_name_replacement() {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

        let state_dir = temp_root("candidate-quarantine-replacement");
        let namespace = state_dir.join("candidate-snapshots");
        let run = namespace.join("run-recorded");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&namespace, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(run.join("original.sentinel"), b"original\n").unwrap();
        let protected = ProtectedRunDirectory::capture(
            std::fs::canonicalize(&run).unwrap(),
            &std::fs::canonicalize(&namespace).unwrap(),
        )
        .unwrap();
        let saved = namespace.join("saved-recorded-run");
        let mut replacement = None;

        let error = cleanup_protected_candidate_manifest_run_with_after_quarantine(
            &protected,
            |quarantine| {
                std::fs::rename(quarantine, &saved).unwrap();
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700).create(quarantine).unwrap();
                std::fs::write(quarantine.join("replacement.sentinel"), b"must survive\n").unwrap();
                replacement = Some(quarantine.to_path_buf());
            },
        )
        .expect_err("cleanup must reject a post-verification quarantine replacement");

        assert!(
            error.starts_with("candidate_snapshot.environment_unsafe:"),
            "{error}"
        );
        let replacement = replacement.unwrap();
        assert_eq!(
            std::fs::read(replacement.join("replacement.sentinel")).unwrap(),
            b"must survive\n"
        );
        assert_eq!(
            std::fs::read(saved.join("original.sentinel")).unwrap(),
            b"original\n",
            "the recorded run and the replacement are both preserved"
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[cfg(unix)]
    #[test]
    fn candidate_cleanup_rejects_dangling_original_path_substitution() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let state_dir = temp_root("candidate-original-dangling-substitution");
        let namespace = state_dir.join("candidate-snapshots");
        let run = namespace.join("run-recorded");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&namespace, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        let protected = ProtectedRunDirectory::capture(
            std::fs::canonicalize(&run).unwrap(),
            &std::fs::canonicalize(&namespace).unwrap(),
        )
        .unwrap();

        let error = cleanup_protected_candidate_manifest_run_with_after_quarantine(
            &protected,
            |_quarantine| {
                symlink(state_dir.join("missing-target"), &run).unwrap();
            },
        )
        .expect_err("a dangling replacement at the original run name is still a substitution");

        assert!(
            error.starts_with("candidate_snapshot.environment_unsafe:"),
            "{error}"
        );
        assert!(
            std::fs::symlink_metadata(&run)
                .unwrap()
                .file_type()
                .is_symlink(),
            "cleanup must preserve the substituted symlink"
        );
        let _ = std::fs::remove_file(&run);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[cfg(unix)]
    #[test]
    fn candidate_cleanup_rejects_and_preserves_a_substituted_scratch_run() {
        use std::os::unix::fs::DirBuilderExt as _;

        let (root, context) = candidate_materialization_context("candidate-scratch-substitute");
        let state_dir = temp_root("candidate-scratch-substitute-state");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let mut original = None;
        let mut replacement = None;

        let error = api
            .with_project_check_overlay(&context, |scratch, _warm, _sidecar| {
                let run = scratch
                    .parent()
                    .expect("checkout has a protected run parent")
                    .to_path_buf();
                let saved = run.with_extension("original");
                std::fs::rename(&run, &saved).unwrap();
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700).create(&run).unwrap();
                std::fs::write(run.join("substitute.sentinel"), b"must survive\n").unwrap();
                original = Some(saved);
                replacement = Some(run);
                "verified"
            })
            .expect_err("candidate cleanup must reject a substituted scratch directory");

        assert!(
            error.starts_with("candidate_snapshot.cleanup_failed:"),
            "{error}"
        );
        let replacement = replacement.unwrap();
        assert_eq!(
            std::fs::read(replacement.join("substitute.sentinel")).unwrap(),
            b"must survive\n",
            "cleanup must preserve a substituted scratch path"
        );
        std::fs::remove_dir_all(&replacement).unwrap();
        std::fs::rename(original.unwrap(), &replacement).unwrap();
        cleanup_project_check_scratch(&root, &replacement.join("worktree")).unwrap();
        std::fs::remove_dir(&replacement).unwrap();
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_cleanup_never_adopts_an_unrecorded_run_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let project = setup_batch_project("unrecorded-incomplete-scratch");
        let state_dir = temp_root("unrecorded-incomplete-scratch-state");
        let namespace = state_dir.join("project-check-runs");
        let scratch = namespace.join("run-unrecorded");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::set_permissions(&namespace, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(scratch.join("substitute.sentinel"), b"must survive\n").unwrap();

        let error = cleanup_incomplete_project_check_scratch(
            &project.root,
            &scratch,
            &std::fs::canonicalize(&namespace).unwrap(),
        )
        .expect_err("cleanup may only delete a run identity captured before population");

        assert!(
            error.starts_with("candidate_snapshot.environment_unsafe:"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(scratch.join("substitute.sentinel")).unwrap(),
            b"must survive\n",
            "an unrecorded substitute must never be adopted and deleted"
        );
        let _ = std::fs::remove_dir_all(state_dir);
        drop(project);
    }

    #[cfg(unix)]
    #[test]
    fn candidate_checkout_lives_beneath_a_prebound_private_run() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let (root, context) = candidate_materialization_context("candidate-prebound-run");
        let state_dir = temp_root("candidate-prebound-run-state");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());

        api.with_project_check_overlay(&context, |scratch, _warm, _sidecar| {
            assert_eq!(
                scratch.file_name().and_then(|name| name.to_str()),
                Some("worktree"),
                "Git must populate a checkout beneath the pre-bound authority directory"
            );
            let run = scratch
                .parent()
                .expect("checkout has a protected run parent");
            assert!(
                run.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("run-")),
                "the protected wrapper retains the unpredictable run identity"
            );
            let metadata = std::fs::symlink_metadata(run).unwrap();
            assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
            assert_eq!(metadata.uid(), effective_user_id());
        })
        .unwrap();

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn candidate_run_cleans_external_manifest_on_returned_error_and_panic() {
        let root = temp_root("candidate-cleanup-all-exits");
        git(&root, &["init"]);
        git(
            &root,
            &["config", "user.email", "cargoless@example.invalid"],
        );
        git(&root, &["config", "user.name", "Cargoless Test"]);
        std::fs::write(root.join("remove.txt"), b"remove me\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "candidate base"]);
        let manifest = overlay_manifest_with_delete_empty_executable_and_binary(&root);
        let state_dir = temp_root("candidate-cleanup-all-exits-state");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let context = ProjectCheckRunContext {
            root: root.clone(),
            changed_files: Some(vec!["empty.bin".into()]),
            base_ref: manifest.comparison_base.commit_sha.clone(),
            base_sha: None,
            source_ref: None,
            source_sha: None,
            candidate_snapshot: Some(manifest),
            overlay_files: Vec::new(),
            materialize_overlay: true,
            gate: true,
            check_ids: None,
        };

        let callback_error = api
            .with_project_check_overlay(&context, |_scratch, _warm, manifest_path| {
                assert!(manifest_path.is_some());
                Err::<(), String>("policy returned an error".to_string())
            })
            .unwrap()
            .unwrap_err();
        assert_eq!(callback_error, "policy returned an error");
        assert_candidate_run_dirs_empty(&state_dir);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = api.with_project_check_overlay(&context, |_scratch, _warm, manifest_path| {
                assert!(manifest_path.is_some());
                panic!("simulated project-check panic");
            });
        }));
        assert!(panic.is_err(), "the project-check panic remains observable");
        assert_candidate_run_dirs_empty(&state_dir);

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    fn assert_candidate_run_dirs_empty(state_dir: &Path) {
        for parent in [
            "project-check-runs",
            "candidate-project-check-runs",
            "candidate-snapshots",
        ] {
            let path = state_dir.join(parent);
            let count = std::fs::read_dir(&path)
                .map(|entries| entries.count())
                .unwrap_or(0);
            assert_eq!(count, 0, "{parent} must not retain completed run state");
        }
    }

    #[test]
    fn project_check_overlay_materializes_changed_files_then_cleans_root() {
        let root = temp_root("project-overlay");
        git(&root, &["init"]);
        git(
            &root,
            &["config", "user.email", "cargoless@example.invalid"],
        );
        git(&root, &["config", "user.name", "Cargoless Test"]);
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".cargoless/tree.cache")).unwrap();
        std::fs::write(root.join(".cargoless/tree.cache/keep"), "cached\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn old() {}\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        let base = String::from("HEAD");

        let api = ServeVerdictState::new();
        let context = ProjectCheckRunContext {
            root: root.clone(),
            changed_files: Some(vec!["src/lib.rs".into(), "new.yaml".into()]),
            base_ref: base,
            base_sha: None,
            source_ref: None,
            source_sha: None,
            candidate_snapshot: None,
            overlay_files: vec![
                (
                    root.join("src/lib.rs").to_string_lossy().into_owned(),
                    "pub fn changed() {}\n".to_string(),
                ),
                (
                    root.join("new.yaml").to_string_lossy().into_owned(),
                    "value: changed\n".to_string(),
                ),
            ],
            materialize_overlay: true,
            gate: false,
            check_ids: None,
        };

        let seen = api
            .with_project_check_overlay(&context, |root, _warm, _candidate_manifest_path| {
                (
                    std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
                    std::fs::read_to_string(root.join("new.yaml")).unwrap(),
                )
            })
            .unwrap();

        assert_eq!(seen.0, "pub fn changed() {}\n");
        assert_eq!(seen.1, "value: changed\n");
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            "pub fn old() {}\n"
        );
        assert!(!root.join("new.yaml").exists());
        assert_eq!(
            std::fs::read_to_string(root.join(".cargoless/tree.cache/keep")).unwrap(),
            "cached\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_check_overlay_uses_state_dir_scratch_worktree() {
        let project = setup_batch_project("project-overlay-scratch");
        let state_dir = temp_root("project-overlay-scratch-state");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let context = ProjectCheckRunContext {
            root: project.root.clone(),
            changed_files: Some(vec!["src/lib.rs".into(), "new.yaml".into()]),
            base_ref: "origin/main".to_string(),
            base_sha: None,
            source_ref: None,
            source_sha: None,
            candidate_snapshot: None,
            overlay_files: vec![
                (
                    project
                        .root
                        .join("src/lib.rs")
                        .to_string_lossy()
                        .into_owned(),
                    "pub fn changed() {}\n".to_string(),
                ),
                (
                    project.root.join("new.yaml").to_string_lossy().into_owned(),
                    "value: changed\n".to_string(),
                ),
            ],
            materialize_overlay: true,
            gate: false,
            check_ids: None,
        };

        let seen = api
            .with_project_check_overlay(&context, |root, _warm, _candidate_manifest_path| {
                assert_ne!(
                    root,
                    project.root.as_path(),
                    "configured daemons should run project checks in a scratch worktree"
                );
                (
                    root.to_path_buf(),
                    std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
                    std::fs::read_to_string(root.join("new.yaml")).unwrap(),
                )
            })
            .unwrap();

        assert_eq!(seen.1, "pub fn changed() {}\n");
        assert_eq!(seen.2, "value: changed\n");
        assert!(
            !seen.0.exists(),
            "scratch worktree should be removed after the check"
        );
        assert_eq!(
            std::fs::read_to_string(project.root.join("src/lib.rs")).unwrap(),
            "pub fn base() {}\n"
        );
        assert!(!project.root.join("new.yaml").exists());
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn project_check_scratch_recovers_registration_left_by_prior_process() {
        let project = setup_batch_project("project-overlay-stale-registration");
        let state_dir = temp_root("project-overlay-stale-registration-state");
        let scratch = state_dir.join("project-check-runs/run-1-8");

        prepare_project_check_scratch(&project.root, &scratch, "origin/main").unwrap();
        assert!(
            scratch.exists(),
            "the prior process created its scratch tree"
        );
        std::fs::remove_dir_all(&scratch).unwrap();
        assert!(
            !scratch.exists(),
            "simulate pod replacement after the directory disappeared but Git metadata survived"
        );
        prepare_project_check_scratch(&project.root, &scratch, "origin/main")
            .expect("a restarted process may safely reclaim the same scratch path");
        assert!(scratch.exists(), "the replacement scratch tree exists");

        cleanup_project_check_scratch(&project.root, &scratch).unwrap();
        assert!(!scratch.exists(), "cleanup removes the scratch directory");
        let registrations = Command::new("git")
            .arg("-C")
            .arg(&project.root)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("worktree list remains readable");
        assert!(registrations.status.success());
        let registrations = String::from_utf8_lossy(&registrations.stdout);
        assert!(
            !registrations.contains(scratch.to_string_lossy().as_ref()),
            "cleanup removes the Git worktree registration as well"
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[test]
    fn project_check_startup_recovery_removes_abandoned_run_artifacts_only() {
        let project = setup_batch_project("project-overlay-startup-recovery");
        let state_dir = temp_root("project-overlay-startup-recovery-state");
        let scratch = state_dir.join("project-check-runs/run-1-7");
        let candidate_run = state_dir.join("candidate-snapshots/run-1-7");
        let warm_marker = state_dir.join("witness-target-warm/current/keep");

        prepare_project_check_scratch(&project.root, &scratch, "origin/main").unwrap();
        std::fs::create_dir_all(warm_marker.parent().unwrap()).unwrap();
        std::fs::write(&warm_marker, "warm cache is durable\n").unwrap();
        std::fs::create_dir_all(&candidate_run).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                scratch.parent().unwrap(),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(
                candidate_run.parent().unwrap(),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            std::fs::set_permissions(&candidate_run, std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        std::fs::write(candidate_run.join("manifest.json"), "abandoned sidecar\n").unwrap();

        let recovered = recover_project_check_scratch(&project.root, &state_dir)
            .expect("daemon startup must reclaim scratch left by a dead predecessor");

        assert_eq!(recovered, 2, "both abandoned run artifacts were reclaimed");
        assert!(
            !scratch.exists(),
            "abandoned scratch no longer consumes the PVC"
        );
        assert!(
            warm_marker.exists(),
            "startup recovery preserves the durable warm target"
        );
        assert!(
            !candidate_run.exists(),
            "startup recovery removes the abandoned external manifest"
        );
        let registrations = Command::new("git")
            .arg("-C")
            .arg(&project.root)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("worktree list remains readable");
        assert!(registrations.status.success());
        let registrations = String::from_utf8_lossy(&registrations.stdout);
        assert!(
            !registrations.contains(scratch.to_string_lossy().as_ref()),
            "startup recovery removes the stale Git registration too"
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn external_startup_recovers_legacy_0755_direct_worktree_without_weakening_candidates() {
        use std::os::unix::fs::PermissionsExt as _;

        let project = setup_batch_project("external-legacy-direct-worktree-recovery");
        let state_dir = temp_root("external-legacy-direct-worktree-recovery-state");
        let scratch_namespace = state_dir.join("project-check-runs");
        let scratch = scratch_namespace.join("run-legacy-direct");
        prepare_project_check_scratch(&project.root, &scratch, "origin/main").unwrap();
        std::fs::set_permissions(&scratch_namespace, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o755)).unwrap();
        let candidate_namespace = state_dir.join("candidate-snapshots");
        std::fs::create_dir(&candidate_namespace).unwrap();
        std::fs::set_permissions(&candidate_namespace, std::fs::Permissions::from_mode(0o700))
            .unwrap();

        let recovered = recover_project_check_scratch_for_source(
            &project.root,
            &state_dir,
            cargoless_core::config::Source::Cli,
        )
        .expect("safe legacy direct worktrees remain recoverable after upgrade");

        assert_eq!(recovered, 1);
        assert!(
            !scratch.exists(),
            "the abandoned direct worktree is removed"
        );
        assert_eq!(
            std::fs::metadata(&candidate_namespace)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "legacy compatibility must not relax candidate namespace mode"
        );
        let registrations = Command::new("git")
            .arg("-C")
            .arg(&project.root)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(registrations.status.success());
        assert!(
            !String::from_utf8_lossy(&registrations.stdout)
                .contains(scratch.to_string_lossy().as_ref()),
            "recovery prunes the old worktree registration"
        );
        drop(project);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn external_startup_rejects_permissive_candidate_scratch_before_paired_deletion() {
        use std::os::unix::fs::PermissionsExt as _;

        let project = setup_batch_project("external-permissive-candidate-scratch-recovery");
        let state_dir = temp_root("external-permissive-candidate-scratch-recovery-state");
        let candidate_scratch_namespace = state_dir.join("candidate-project-check-runs");
        let candidate_scratch = candidate_scratch_namespace.join("run-paired");
        std::fs::create_dir_all(&candidate_scratch).unwrap();
        std::fs::set_permissions(
            &candidate_scratch_namespace,
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(&candidate_scratch, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(
            candidate_scratch.join("scratch.sentinel"),
            b"must survive\n",
        )
        .unwrap();

        let candidate_namespace = state_dir.join("candidate-snapshots");
        let candidate_run = candidate_namespace.join("run-paired");
        std::fs::create_dir_all(&candidate_run).unwrap();
        std::fs::set_permissions(&candidate_namespace, std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::set_permissions(&candidate_run, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(candidate_run.join("manifest.json"), b"must survive\n").unwrap();

        let error = recover_project_check_scratch_for_source(
            &project.root,
            &state_dir,
            cargoless_core::config::Source::Cli,
        )
        .expect_err("candidate scratch and sidecar namespaces are one strict recovery unit");

        assert!(
            error.starts_with("candidate_snapshot.environment_unsafe:"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(candidate_scratch.join("scratch.sentinel")).unwrap(),
            b"must survive\n",
            "unsafe candidate scratch validation must precede all deletion"
        );
        assert_eq!(
            std::fs::read(candidate_run.join("manifest.json")).unwrap(),
            b"must survive\n",
            "paired sidecar state is preserved when candidate scratch is unsafe"
        );
        drop(project);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[cfg(unix)]
    #[test]
    fn repo_relative_startup_recovery_leaves_candidate_namespace_untouched() {
        use std::os::unix::fs::PermissionsExt as _;

        let project = setup_batch_project("project-overlay-default-state-recovery");
        let state_dir = project.root.join(".cargoless");
        let scratch = state_dir.join("project-check-runs/run-abandoned");
        let candidate_run = state_dir.join("candidate-snapshots/run-abandoned");
        prepare_project_check_scratch(&project.root, &scratch, "origin/main").unwrap();
        std::fs::create_dir_all(&candidate_run).unwrap();
        std::fs::set_permissions(
            candidate_run.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(&candidate_run, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            candidate_run.join("manifest.json"),
            b"do not inspect or delete\n",
        )
        .unwrap();

        let recovered = recover_project_check_scratch(&project.root, &state_dir)
            .expect("legacy exact-Git recovery remains available");

        assert_eq!(recovered, 1, "only the exact-Git scratch is recovered");
        assert!(!scratch.exists());
        assert_eq!(
            std::fs::read(candidate_run.join("manifest.json")).unwrap(),
            b"do not inspect or delete\n",
            "repo-relative default state must leave typed candidate state untouched"
        );
        drop(project);
    }

    #[cfg(unix)]
    #[test]
    fn default_state_symlink_never_reclassifies_or_deletes_external_candidate_state() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let project = setup_batch_project("project-overlay-default-state-symlink");
        let external = temp_root("project-overlay-default-state-symlink-external");
        let candidate_run = external.join("candidate-snapshots/run-abandoned");
        std::fs::create_dir_all(&candidate_run).unwrap();
        std::fs::set_permissions(
            candidate_run.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(&candidate_run, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            candidate_run.join("manifest.json"),
            b"must remain untouched\n",
        )
        .unwrap();
        let default_state = project.root.join(".cargoless");
        symlink(&external, &default_state).unwrap();

        let recovered = recover_project_check_scratch_for_source(
            &project.root,
            &default_state,
            cargoless_core::config::Source::Default,
        )
        .expect("repo-default state stays typed-disabled even when its path is hostile");

        assert_eq!(recovered, 0);
        assert_eq!(
            std::fs::read(candidate_run.join("manifest.json")).unwrap(),
            b"must remain untouched\n",
            "default provenance must be checked before canonicalizing a symlink target"
        );
        let _ = std::fs::remove_file(default_state);
        drop(project);
        let _ = std::fs::remove_dir_all(external);
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_external_startup_state_fails_before_any_recovery_deletion() {
        use std::os::unix::fs::symlink;

        let project = setup_batch_project("project-overlay-unsafe-state-recovery");
        let state_dir = temp_root("project-overlay-unsafe-state-recovery-state");
        let outside = temp_root("project-overlay-unsafe-state-recovery-outside");
        let scratch = state_dir.join("project-check-runs/run-abandoned");
        prepare_project_check_scratch(&project.root, &scratch, "origin/main").unwrap();
        symlink(&outside, state_dir.join("candidate-snapshots")).unwrap();

        let error = recover_project_check_scratch(&project.root, &state_dir)
            .expect_err("an unsafe external namespace must fail closed");

        assert!(
            error.starts_with("candidate_snapshot.environment_unsafe:"),
            "{error}"
        );
        assert!(
            scratch.exists(),
            "all protected namespaces must be validated before recovery deletes anything"
        );
        cleanup_project_check_scratch(&project.root, &scratch).unwrap();
        let _ = std::fs::remove_file(state_dir.join("candidate-snapshots"));
        drop(project);
        let _ = std::fs::remove_dir_all(state_dir);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn push_overlay_with_options_rejects_escaping_repo_relative_paths() {
        let api = ServeVerdictState::new();
        let files = vec![("../outside.rs".to_string(), "bad".to_string())];
        let options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some("/workspace/tf-multiverse".into()),
            base_sha: None,
            source_ref: None,
            source_sha: None,
            comparison_base_sha: None,
            candidate_snapshot: None,
            changed_files: None,
            gate: false,
            check_ids: None,
            semantic: None,
        };

        let ack = api.push_overlay_with_options("/client/wt", "", &files, None, Some(&options));

        assert!(!ack.accepted);
        assert_eq!(ack.applied_files, 0);
        assert!(api.peek_overlay_for("/client/wt").is_none());
    }

    #[test]
    fn take_overlay_for_is_pop_on_consume() {
        let api = ServeVerdictState::new();
        let files = vec![("/wt/x".to_string(), "y".to_string())];
        api.push_overlay("/wt", "main", &files);

        // First take: consumes.
        let first = api.take_overlay_for("/wt");
        assert!(first.is_some(), "first take returns the stored overlay");
        assert_eq!(first.unwrap().files, files);

        // Second take: None (consumed). FS-mode resumes for this WT
        // until a fresh push arrives.
        assert!(
            api.take_overlay_for("/wt").is_none(),
            "second take returns None — pop-on-consume semantic"
        );
        // peek also None.
        assert!(api.peek_overlay_for("/wt").is_none());
    }

    #[test]
    fn ra_spawn_failure_terminalizes_the_exact_queued_attempt() {
        let state_dir = temp_root("ra-spawn-failure-outcome");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let context = attempt_context("attempt-ra-spawn-failure", 1);
        let files = vec![("src/lib.rs".to_string(), "pub fn changed() {}".to_string())];
        let options = PushOverlayOptions {
            base_sha: Some("0123456789abcdef0123456789abcdef01234567".into()),
            changed_files: Some(vec!["src/lib.rs".into()]),
            semantic: Some(context.clone()),
            ..PushOverlayOptions::default()
        };

        assert!(
            api.push_overlay_with_options(
                "/client/ra-spawn-failure",
                "0123456789abcdef0123456789abcdef01234567",
                &files,
                None,
                Some(&options),
            )
            .accepted
        );
        assert!(api.fail_next_pushed_overlay(
            "/client/ra-spawn-failure",
            "ra_spawn_failed: Resource temporarily unavailable (os error 11)",
        ));

        assert!(api.peek_overlay_for("/client/ra-spawn-failure").is_none());
        assert_eq!(api.daemon_activity().active_worktrees, 0);
        let outcome = api
            .get_outcome_v3(&context.attempt_id)
            .expect("accepted attempt remains queryable");
        assert_eq!(
            outcome.conclusion.semantic_code(),
            "indeterminate.process_lost"
        );
        assert!(
            outcome.timeline.last().is_some_and(|phase| {
                phase.phase == Phase::Terminal && phase.finished_at_unix_ms.is_some()
            }),
            "spawn failure must become terminal instead of remaining queued forever"
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    /// **THE load-bearing composing-equivalence assertion (2b spec §5.3).**
    ///
    /// For the SAME `(path, content)` set, the `Vec<OverlayOp>` produced
    /// by `overlay::diff(prev, next)` is byte-identical whether `next`
    /// was built from FS-read pairs OR from pushed pairs. This proves
    /// that `overlay::diff` is source-agnostic — the proven isolation
    /// core (multiplex/clusterdrv/barrier) sees no difference between
    /// pushed-mode and FS-mode, and the §190/#247
    /// precondition-restore story stays intact through the 2b ingest seam.
    ///
    /// This is the structural-correctness guarantee 2b ships. A future
    /// regression that introduces source-asymmetry (e.g. trimming pushed
    /// content) would flip exactly this assertion.
    #[test]
    fn composing_equivalence_pushed_vs_fs_pairs_yield_identical_overlay_ops() {
        use cargoless_core::overlay::{OverlaySet, diff};

        let prev = OverlaySet::from_pairs(vec![(
            "/wt-a/src/old.rs".to_string(),
            "fn old() {}".to_string(),
        )]);

        // Same content, two construction paths:
        //   - FS-mode: the SwitchOverlay arm reads (path, content) from
        //     disk and builds OverlaySet::from_pairs.
        //   - Pushed-mode: the SwitchOverlay arm reads (path, content)
        //     from api.take_overlay_for(wt) and builds OverlaySet::from_pairs.
        // Both produce IDENTICAL OverlaySet → IDENTICAL diff output.
        let pairs = vec![
            (
                "/wt-a/src/lib.rs".to_string(),
                "pub fn new() {}".to_string(),
            ),
            (
                "/wt-a/src/util.rs".to_string(),
                "pub fn util() {}".to_string(),
            ),
        ];

        let fs_next = OverlaySet::from_pairs(pairs.iter().cloned());
        let fs_ops = diff(&prev, &fs_next);

        // Pushed-mode: store + take + reconstruct OverlaySet exactly as
        // the SwitchOverlay arm does.
        let api = ServeVerdictState::new();
        api.push_overlay("/wt-a", "origin/main", &pairs);
        let pushed = api.take_overlay_for("/wt-a").expect("pushed");
        let pushed_next = OverlaySet::from_pairs(pushed.files.iter().cloned());
        let pushed_ops = diff(&prev, &pushed_next);

        assert_eq!(
            fs_ops, pushed_ops,
            "overlay::diff output MUST be byte-identical regardless of \
             source (FS vs pushed) — the load-bearing composing-equivalence \
             assertion (D-PUSHOVERLAY §5.3). A regression here breaks the \
             pushed-mode no-wrong-verdict guarantee."
        );
    }

    #[test]
    fn push_overlay_no_signal_attached_is_safe() {
        // Fail-soft: a push that arrives BEFORE the loop wires its
        // push_signal (or AFTER the receiver was dropped) must store
        // the overlay AND not panic. The loop can still service the
        // push on its next activity tick or next push.
        let api = ServeVerdictState::new();
        // No attach_push_signal called.
        let files = vec![("/wt/f".to_string(), "x".to_string())];
        let ack = api.push_overlay("/wt", "main", &files);
        assert!(
            ack.accepted,
            "no-signal-attached ⇒ push is still accepted + stored"
        );
        assert!(
            api.peek_overlay_for("/wt").is_some(),
            "overlay still in store despite no signal"
        );

        // Dropped-receiver case: attach, drop rx, push again — still safe.
        let (tx, rx) = channel::<String>();
        api.attach_push_signal(tx);
        drop(rx);
        let ack2 = api.push_overlay("/wt-b", "main", &files);
        assert!(
            ack2.accepted,
            "dropped-receiver ⇒ push still accepted + stored (best-effort signal)"
        );
        assert!(api.peek_overlay_for("/wt-b").is_some());
    }

    #[test]
    fn multiple_pushes_same_commit_coalesce_latest_wins() {
        // CGLS-25 — SAME commit (same base_sha, here None for the bare
        // push_overlay path) rapid-pushed N times still coalesces to
        // latest-wins: an FS save-storm or a retried push must NOT queue N
        // witnesses for one commit. This preserves the historical
        // single-slot behavior for the same-commit case.
        let api = ServeVerdictState::new();
        let (tx, rx) = channel::<String>();
        api.attach_push_signal(tx);

        let v1 = vec![("/wt/x".to_string(), "version-1".to_string())];
        let v2 = vec![("/wt/x".to_string(), "version-2".to_string())];
        let v3 = vec![("/wt/x".to_string(), "version-3".to_string())];
        api.push_overlay("/wt", "main", &v1);
        api.push_overlay("/wt", "main", &v2);
        api.push_overlay("/wt", "main", &v3);

        // One consume yields the LATEST content (v3); v1/v2 coalesced away
        // because all three share base_sha == None.
        let consumed = api.take_overlay_for("/wt").expect("stored");
        assert_eq!(
            consumed.files, v3,
            "latest push wins for the same commit (base_sha-keyed coalesce)"
        );
        // Nothing else queued — one commit ⇒ one overlay.
        assert!(api.take_overlay_for("/wt").is_none());

        // All 3 accept-side signals fired (per-push wakeup); the consume
        // side coalesces. The re-signal on drain does NOT fire because the
        // queue emptied on the single take.
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 3, "3 accept signals; 0 re-signal (queue emptied)");
    }

    #[test]
    fn distinct_commits_same_wt_both_survive_no_clobber() {
        // CGLS-25 — the clobber fix. The witness hardcodes ONE worktree key
        // for every PR, so two concurrent PR pushes (DISTINCT base_sha) land
        // on the same key. Historically PR-B's push OVERWROTE PR-A's pending
        // overlay before the serve loop consumed it → PR-A's witness never
        // ran, its poller starved. The FIFO-by-base_sha queue makes both
        // survive: each is consumable in arrival order, each carrying its
        // OWN base_sha for correct downstream attribution.
        let api = ServeVerdictState::new();
        let (tx, rx) = channel::<String>();
        api.attach_push_signal(tx);

        let files_a = vec![("src/lib.rs".to_string(), "// PR-A".to_string())];
        let files_b = vec![("src/lib.rs".to_string(), "// PR-B".to_string())];
        let opts = |sha: &str| PushOverlayOptions {
            repo_relative: false,
            analysis_root: None,
            base_sha: Some(sha.to_string()),
            source_ref: None,
            source_sha: None,
            comparison_base_sha: None,
            candidate_snapshot: None,
            changed_files: None,
            gate: false,
            check_ids: None,
            semantic: None,
        };
        // PR-A pushes, then PR-B pushes on the SAME worktree key before the
        // serve loop has consumed A.
        api.push_overlay_with_options("/wt", "main", &files_a, None, Some(&opts("sha-A")));
        api.push_overlay_with_options("/wt", "main", &files_b, None, Some(&opts("sha-B")));

        // Both survive, FIFO: A consumed first, carrying sha-A.
        let first = api.take_overlay_for("/wt").expect("PR-A survived");
        assert_eq!(first.base_sha.as_deref(), Some("sha-A"), "FIFO: A first");
        assert_eq!(first.files, files_a, "PR-A overlay content intact");
        // B still queued, carrying sha-B — NOT clobbered by A's consume.
        let second = api.take_overlay_for("/wt").expect("PR-B survived");
        assert_eq!(second.base_sha.as_deref(), Some("sha-B"), "then B");
        assert_eq!(second.files, files_b, "PR-B overlay content intact");
        // Queue now empty ⇒ FS-fallback discriminant holds.
        assert!(api.take_overlay_for("/wt").is_none());

        // Signals: 2 accept + 1 re-signal (fired when A's consume left B
        // queued, so the wake-dedup in drain_unique_push_keys cannot starve
        // B). = 3 total.
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(
            count, 3,
            "2 accept signals + 1 re-signal on non-empty drain"
        );
    }

    #[test]
    fn quiesce_refuses_new_pushes_and_drains_on_publish() {
        let api = ServeVerdictState::new();
        let files = vec![("/wt/src/lib.rs".to_string(), "pub fn x() {}".to_string())];

        let ack = api.push_overlay("/wt", "main", &files);
        assert!(ack.accepted);
        assert_eq!(
            api.daemon_activity(),
            DaemonActivity {
                quiescing: false,
                active_worktrees: 1,
                pending_pushes: 1,
                ..DaemonActivity::default()
            }
        );

        let activity = api.request_quiesce();
        assert!(activity.quiescing);
        assert_eq!(activity.active_worktrees, 1);
        assert_eq!(activity.pending_pushes, 1);

        let rejected = api.push_overlay("/wt-2", "main", &files);
        assert!(
            !rejected.accepted,
            "quiescing daemon refuses fresh pushed work"
        );
        assert!(api.peek_overlay_for("/wt-2").is_none());

        let consumed = api.take_overlay_for("/wt").expect("pending overlay");
        assert_eq!(consumed.files, files);
        assert_eq!(api.daemon_activity().pending_pushes, 0);
        assert!(
            !api.drain_complete(),
            "publishing the accepted push's verdict is the drain boundary"
        );

        api.publish(Path::new("/wt"), crate::statusfile::VerdictPayload::green());
        assert_eq!(
            api.daemon_activity(),
            DaemonActivity {
                quiescing: true,
                active_worktrees: 0,
                pending_pushes: 0,
                ..DaemonActivity::default()
            }
        );
        assert!(api.drain_complete());
    }

    #[test]
    fn quiesce_waits_for_independently_counted_witness_workers() {
        // Live finding, 2026-08-04: witness A reported active_worktrees=0
        // while inflight_witness_compiles=1. The worktree tracker is a set;
        // a same-key publish can remove its one entry while another witness
        // worker for that key is still queued or compiling. drain_complete()
        // must therefore consume the witness gate's independent counters,
        // not infer worker liveness from active_worktrees.
        let mut api = ServeVerdictState::new();
        api.witness_gate = test_witness_gate(1, 60_000);
        *poisoned(&api.witness_gate.state) = 1;
        api.witness_gate.waiting.store(1, Ordering::Relaxed);

        let activity = api.request_quiesce();
        assert_eq!(
            activity.active_worktrees, 0,
            "reproduce the lossy set state"
        );
        assert_eq!(activity.inflight_witness_compiles, 1);
        assert_eq!(activity.waiting_witness_compiles, 1);
        assert!(
            !api.drain_complete(),
            "a queued or running witness is accepted work and must survive rollout"
        );

        api.witness_gate.waiting.store(0, Ordering::Relaxed);
        assert!(
            !api.drain_complete(),
            "the running witness alone still keeps the daemon alive"
        );
        *poisoned(&api.witness_gate.state) = 0;
        assert!(
            api.drain_complete(),
            "drain completes only after both counters reach zero"
        );
    }

    // ──────── #A2/#A3/#A7 — verdict attribution + truncation guard ────────

    #[test]
    fn publish_attributed_echoes_base_sha_on_status_and_event() {
        // #A2 — the flip-blocking contract: a poller sharing a status key
        // with other branches must see ITS commit on the verdict (status
        // AND the SSE event — the event is the race-free path).
        let api = ServeVerdictState::new();
        let rx = api.subscribe();
        api.publish_attributed(
            Path::new("/workspace/tf-multiverse"),
            crate::statusfile::VerdictPayload::green(),
            Some("abc123def".into()),
            true,
        );
        let status = api
            .get_status("/workspace/tf-multiverse")
            .expect("status present");
        assert_eq!(status.base_sha.as_deref(), Some("abc123def"));
        assert!(
            status.ra_blind_paths,
            "#A8: blind-path bit must ride the published status"
        );
        let ev = rx.try_recv().expect("transition event");
        assert_eq!(ev.base_sha.as_deref(), Some("abc123def"));
        assert!(
            ev.ra_blind_paths,
            "#A8: blind-path bit must ride the transition event"
        );

        // Unattributed publish (FS-watch path) — echo must CLEAR, never
        // hold a stale SHA from the previous push's verdict.
        api.publish(
            Path::new("/workspace/tf-multiverse"),
            crate::statusfile::VerdictPayload::green(),
        );
        let status = api
            .get_status("/workspace/tf-multiverse")
            .expect("status present");
        assert_eq!(
            status.base_sha, None,
            "FS-watch verdict must not inherit the prior push's base_sha"
        );
        assert!(
            !status.ra_blind_paths,
            "#A8: FS-watch verdict must not inherit the prior push's blind bit"
        );
    }

    #[test]
    fn push_attribution_records_and_pops_per_worktree() {
        // #A2 — the record→take handoff mirrors project_check_context:
        // recorded at SwitchOverlay consume, popped exactly once at
        // publish; a replacing push overwrites.
        let api = ServeVerdictState::new();
        let pushed = |sha: &str| PushedOverlay {
            base_ref: "origin/main".into(),
            files: vec![("src/lib.rs".into(), "pub fn x() {}".into())],
            analysis_root: None,
            base_sha: Some(sha.into()),
            source_ref: None,
            source_sha: None,
            candidate_snapshot: None,
            last_push_unix: crate::statusfile::now_unix(),
            changed_files: None,
            check_profile: None,
            gate: false,
            check_ids: None,
            semantic: None,
        };
        api.record_push_attribution("/wt", &pushed("first"));
        api.record_push_attribution("/wt", &pushed("second"));
        let attribution = api.take_push_attribution("/wt").expect("recorded");
        assert_eq!(
            attribution.base_sha.as_deref(),
            Some("second"),
            "replacing push's attribution wins (matches overlay-replace semantics)"
        );
        assert!(
            api.take_push_attribution("/wt").is_none(),
            "pop-on-consume: one publish consumes the attribution"
        );
    }

    // ────────── CGLS-27 — respawn-stranded push drain ──────────

    /// Minimal pushed overlay for the drain tests.
    fn stranded_pushed(sha: &str) -> PushedOverlay {
        PushedOverlay {
            base_ref: "origin/main".into(),
            files: vec![("src/lib.rs".into(), "pub fn x() {}".into())],
            analysis_root: None,
            base_sha: Some(sha.into()),
            source_ref: None,
            source_sha: None,
            candidate_snapshot: None,
            last_push_unix: crate::statusfile::now_unix(),
            changed_files: None,
            check_profile: None,
            gate: false,
            check_ids: None,
            semantic: None,
        }
    }

    #[test]
    fn drain_stranded_attributions_is_drain_not_peek() {
        // The reap loop this exists for is SUSTAINED, not one-shot: a
        // second respawn must not re-publish a push the first already
        // resolved. Removal (not a read) is what makes publish-once hold
        // across a tight kill/respawn cycle by construction.
        let api = ServeVerdictState::new();
        api.record_push_attribution("/wt", &stranded_pushed("abc123"));

        let keys = BTreeSet::from(["/wt".to_string()]);
        let first = api.drain_push_attributions_for(&keys);
        assert_eq!(first.len(), 1, "the consumed-but-unpublished push");
        assert_eq!(first[0].0, "/wt");
        assert_eq!(first[0].1.base_sha.as_deref(), Some("abc123"));

        assert!(
            api.drain_push_attributions_for(&keys).is_empty(),
            "second respawn must find nothing — no double-publish"
        );
    }

    #[test]
    fn drain_stranded_attributions_is_scoped_to_the_respawned_cluster() {
        // THE load-bearing safety property. `reset_after_respawn` strands
        // only the respawned cluster's in-flight txn; other clusters' RAs
        // are alive and their pushes are still going to publish normally.
        // A global drain would resolve those at exit 75 while they were
        // about to go green — turning a fix into a regression.
        let api = ServeVerdictState::new();
        api.record_push_attribution("/cluster-a/wt", &stranded_pushed("aaa"));
        api.record_push_attribution("/cluster-b/wt", &stranded_pushed("bbb"));

        let only_a = BTreeSet::from(["/cluster-a/wt".to_string()]);
        let drained = api.drain_push_attributions_for(&only_a);

        assert_eq!(drained.len(), 1, "only the respawned cluster's worktree");
        assert_eq!(drained[0].0, "/cluster-a/wt");
        assert_eq!(
            api.take_push_attribution("/cluster-b/wt")
                .expect("healthy cluster's push must be UNTOUCHED")
                .base_sha
                .as_deref(),
            Some("bbb"),
        );
    }

    #[test]
    fn drain_stranded_attributions_empty_when_nothing_consumed() {
        // The first spawn of a cluster also delivers Ctrl::Spawned, so the
        // drain runs there too. Nothing has been consumed yet ⇒ no keys ⇒
        // no spurious `unknown` at boot. This is why no first-spawn guard
        // is needed (and why adding one would misstate the reason).
        let api = ServeVerdictState::new();
        assert!(
            api.drain_push_attributions_for(&BTreeSet::from(["/wt".to_string()]))
                .is_empty(),
            "boot spawn must publish nothing"
        );
    }

    #[test]
    fn drain_stranded_attributions_ignores_unrelated_keys() {
        // A worktree in the driver's deliberately-retained `pending` queue
        // never reached SwitchOverlay, so it has no attribution and cannot
        // be stolen by the drain. Modelled here as a key with no entry.
        let api = ServeVerdictState::new();
        api.record_push_attribution("/wt-consumed", &stranded_pushed("ccc"));

        let keys = BTreeSet::from(["/wt-consumed".to_string(), "/wt-queued-only".to_string()]);
        let drained = api.drain_push_attributions_for(&keys);

        assert_eq!(
            drained.len(),
            1,
            "only the CONSUMED push is stranded; a queued-only WT has no attribution"
        );
        assert_eq!(drained[0].0, "/wt-consumed");
    }

    // ──────────────── #A8 — macro-blind classification ────────────────

    #[test]
    fn macro_blind_hit_matches_changed_files_against_globs() {
        // The tf-mv deployment shape: portal/** etc. are the RA-blind
        // proc-macro surfaces; only changed_files (repo-relative diff
        // list) participates — never the overlay file list.
        let globs = parse_macro_blind_globs(
            "portal/**, chemistry/shell/**,chemistry/generated/portal-*/**,runtime-types/**",
        );
        assert_eq!(globs.len(), 4, "tolerant split incl. space after comma");
        let hit = |files: &[&str]| {
            let files: Vec<String> = files.iter().map(|s| s.to_string()).collect();
            // No macro names ⇒ pure path-glob (pre-CGLS-12 behavior).
            compute_macro_blind_hit(Some(&files), &globs, &[], &[])
        };
        assert!(hit(&["portal/src/app.rs"]));
        assert!(hit(&["chemistry/generated/portal-7/lib.rs"]));
        assert!(
            hit(&["physics/src/lib.rs", "runtime-types/src/ids.rs"]),
            "any single blind file marks the whole push"
        );
        assert!(!hit(&["physics/src/lib.rs", "docs/README.md"]));
    }

    #[test]
    fn macro_blind_hit_never_fires_without_evidence() {
        // Absence-of-evidence posture (same as base_sha: None ⇒
        // unattributed): no globs configured, no changed_files, or an
        // empty list ⇒ false — the annotation must never be a guess.
        let globs = parse_macro_blind_globs("portal/**");
        // No macro names ⇒ pure path-glob (pre-CGLS-12 behavior).
        assert!(!compute_macro_blind_hit(None, &globs, &[], &[]));
        assert!(!compute_macro_blind_hit(Some(&[]), &globs, &[], &[]));
        let files = vec!["portal/src/app.rs".to_string()];
        assert!(
            !compute_macro_blind_hit(Some(&files), &[], &[], &[]),
            "unconfigured daemon (no globs) ⇒ annotation inert"
        );
        assert!(compute_macro_blind_hit(Some(&files), &globs, &[], &[]));
    }

    #[test]
    fn record_push_attribution_classifies_blind_paths_at_consume() {
        // The blind bit rides the SAME record as base_sha (record at
        // consume, pop at publish) so it can never be stamped onto a
        // different push's verdict.
        let api = ServeVerdictState::new();
        let globs = parse_macro_blind_globs("portal/**");
        let pushed = |changed: Option<Vec<String>>| PushedOverlay {
            base_ref: "origin/dev".into(),
            files: vec![("portal/src/app.rs".into(), "fn a() {}".into())],
            analysis_root: None,
            base_sha: Some("cafe1234".into()),
            source_ref: None,
            source_sha: None,
            candidate_snapshot: None,
            last_push_unix: crate::statusfile::now_unix(),
            changed_files: changed,
            check_profile: None,
            gate: false,
            check_ids: None,
            semantic: None,
        };
        // No macro names ⇒ pure path-glob (pre-CGLS-12 behavior).
        api.record_push_attribution_with_globs(
            "/wt",
            &pushed(Some(vec!["portal/src/app.rs".into()])),
            &globs,
            &[],
        );
        let attribution = api.take_push_attribution("/wt").expect("recorded");
        assert!(attribution.macro_blind_hit, "portal/** push classifies");

        api.record_push_attribution_with_globs(
            "/wt",
            &pushed(Some(vec!["physics/src/lib.rs".into()])),
            &globs,
            &[],
        );
        let attribution = api.take_push_attribution("/wt").expect("recorded");
        assert!(!attribution.macro_blind_hit, "non-blind push stays clean");

        // changed_files: None (legacy client) — overlay FILES touch
        // portal/ but provide no diff evidence; must NOT classify.
        api.record_push_attribution_with_globs("/wt", &pushed(None), &globs, &[]);
        let attribution = api.take_push_attribution("/wt").expect("recorded");
        assert!(
            !attribution.macro_blind_hit,
            "overlay file list must not substitute for changed_files"
        );
    }

    #[test]
    fn verdict_latency_composes_queue_wait_and_analysis_time() {
        // #A7 — latency = (consume - receipt) seconds + monotonic
        // analysis ms; saturating against clock skew.
        assert_eq!(latency_ms(100, 103, Duration::from_millis(250)), 3250);
        assert_eq!(latency_ms(100, 100, Duration::from_millis(7)), 7);
        assert_eq!(
            latency_ms(200, 100, Duration::from_millis(5)),
            5,
            "receipt clock ahead of consume clock (NTP step) saturates to analysis-only"
        );
    }

    // ──────────────── #CGLS-12 — content-based macro detection ────────────────

    #[test]
    fn content_scan_macro_present_is_blind() {
        // AC: a glob-matched file that CONTAINS view! ⇒ macro_blind_hit true.
        let globs = parse_macro_blind_globs("portal/**");
        let macro_names = parse_macro_blind_macros("view");
        let changed = vec!["portal/src/app.rs".to_string()];
        let overlay: Vec<(String, String)> = vec![(
            "portal/src/app.rs".into(),
            "pub fn render() { view! { <div/> } }".into(),
        )];
        assert!(
            compute_macro_blind_hit(Some(&changed), &globs, &overlay, &macro_names),
            "glob-matched file with view! must be blind"
        );
    }

    #[test]
    fn content_scan_macro_absent_not_blind() {
        // AC: a glob-matched file with NO view! invocation ⇒ macro_blind_hit false
        // (reduces ~37% over-fire; the file is still in the glob zone but
        // has no proc-macro call).
        let globs = parse_macro_blind_globs("portal/**");
        let macro_names = parse_macro_blind_macros("view");
        let changed = vec!["portal/src/types.rs".to_string()];
        let overlay: Vec<(String, String)> = vec![(
            "portal/src/types.rs".into(),
            "pub struct Foo { pub x: u32 }".into(),
        )];
        assert!(
            !compute_macro_blind_hit(Some(&changed), &globs, &overlay, &macro_names),
            "glob-matched file with no view! must NOT be blind"
        );
    }

    #[test]
    fn content_scan_unreadable_falls_back_to_glob_hit() {
        // AC: content NOT in overlay (e.g. file exists on disk but was not
        // pushed) ⇒ fail-safe: treat as blind (glob hit stands). A real
        // blind file must never be missed.
        let globs = parse_macro_blind_globs("portal/**");
        let macro_names = parse_macro_blind_macros("view");
        let changed = vec!["portal/src/app.rs".to_string()];
        let overlay: Vec<(String, String)> = vec![]; // content absent
        assert!(
            compute_macro_blind_hit(Some(&changed), &globs, &overlay, &macro_names),
            "absent content must fall back to glob hit (blind), never miss a real blind file"
        );
    }

    #[test]
    fn content_scan_empty_macro_list_is_pure_path_glob() {
        // AC: when CARGOLESS_MACRO_BLIND_MACROS is unset (macro_names empty),
        // behavior is byte-identical to pre-CGLS-12 pure path-glob — even if
        // the overlay carries content with no macro invocations.
        let globs = parse_macro_blind_globs("portal/**");
        let macro_names: Vec<String> = vec![]; // env var unset
        let changed = vec!["portal/src/types.rs".to_string()];
        let overlay: Vec<(String, String)> = vec![(
            "portal/src/types.rs".into(),
            "pub struct Foo { pub x: u32 }".into(),
        )];
        // Pure path-glob: file is in portal/** ⇒ blind (no content scan).
        assert!(
            compute_macro_blind_hit(Some(&changed), &globs, &overlay, &macro_names),
            "empty macro list ⇒ pure path-glob, no content scan"
        );
    }

    #[test]
    fn content_scan_detects_various_invocation_forms() {
        // `content_has_macro_call` must handle all three delimiter forms
        // (`{`, `(`, `[`) and tolerate whitespace between `!` and delimiter.
        let names = parse_macro_blind_macros("view,html");
        assert!(content_has_macro_call("view! { <div/> }", &names));
        assert!(content_has_macro_call("view!{ <div/> }", &names));
        assert!(content_has_macro_call("html!( \"<b/>\" )", &names));
        assert!(content_has_macro_call("html![ a, b ]", &names));
        assert!(content_has_macro_call("view!\n{ multiline }", &names));
        // `view! is not here` — `view!` not followed by `{`/`(`/`[`, so no hit.
        assert!(!content_has_macro_call("// view! is not here", &names));
        // Non-matching macro name
        assert!(!content_has_macro_call("format!(\"{}\", x)", &names));
        // Empty content
        assert!(!content_has_macro_call("", &names));
        assert!(!content_has_macro_call("pub fn foo() {}", &names));
    }

    #[test]
    fn parse_macro_blind_macros_tolerant_split() {
        // Mirrors parse_macro_blind_globs: spaces, empty segments, single token.
        let names = parse_macro_blind_macros("view, html,,rsx ");
        assert_eq!(names, vec!["view", "html", "rsx"]);
        assert!(parse_macro_blind_macros("").is_empty());
        assert_eq!(parse_macro_blind_macros("view"), vec!["view"]);
    }

    #[test]
    fn overlay_content_for_matches_absolute_and_relative_paths() {
        // overlay_content_for must find content whether the overlay path is
        // repo-relative (direct push) or absolute (after map_repo_relative_files).
        let overlay: Vec<(String, String)> = vec![
            ("portal/src/app.rs".into(), "relative content".into()),
            (
                "/workspace/root/portal/src/other.rs".into(),
                "absolute content".into(),
            ),
        ];
        assert_eq!(
            overlay_content_for("portal/src/app.rs", &overlay),
            Some("relative content")
        );
        assert_eq!(
            overlay_content_for("portal/src/other.rs", &overlay),
            Some("absolute content")
        );
        assert_eq!(overlay_content_for("portal/src/missing.rs", &overlay), None);
    }

    #[test]
    fn zero_file_push_claiming_changes_is_rejected() {
        // #A3 — the false-green incident class: gate builds a >32MiB
        // payload, the files array arrives empty, the daemon checks the
        // bare base and publishes green "for" the push. The COUNT
        // mismatch (changed_files says N>0, files says 0) is the
        // truncation signature and must refuse the push.
        let api = ServeVerdictState::new();
        let options = PushOverlayOptions {
            repo_relative: false,
            analysis_root: None,
            base_sha: Some("abc123".into()),
            source_ref: None,
            source_sha: None,
            comparison_base_sha: None,
            candidate_snapshot: None,
            changed_files: Some(vec!["src/lib.rs".into(), "src/main.rs".into()]),
            gate: false,
            check_ids: None,
            semantic: None,
        };
        let ack = api.push_overlay_with_options("/wt", "origin/main", &[], None, Some(&options));
        assert!(!ack.accepted, "truncation signature must be rejected");
        assert_eq!(ack.applied_files, 0);
        assert!(
            api.peek_overlay_for("/wt").is_none(),
            "rejected push must not be stored"
        );
    }

    #[test]
    fn zero_file_central_daemon_push_is_rejected() {
        // #A3 — an analysis_root push exists to get a verdict for pushed
        // content; zero files means the daemon would publish a bare-base
        // verdict attributed to the push.
        let api = ServeVerdictState::new();
        let options = PushOverlayOptions {
            repo_relative: false,
            analysis_root: Some("/workspace/tf-multiverse".into()),
            base_sha: None,
            source_ref: None,
            source_sha: None,
            comparison_base_sha: None,
            candidate_snapshot: None,
            changed_files: None,
            gate: false,
            check_ids: None,
            semantic: None,
        };
        let ack = api.push_overlay_with_options("/wt", "", &[], None, Some(&options));
        assert!(
            !ack.accepted,
            "central-daemon zero-file push must be rejected"
        );
    }

    #[test]
    fn delete_only_push_with_empty_content_files_passes_guard() {
        // #A3 — deletions are deliberately carried as empty-CONTENT
        // overlay entries (push.rs); the guard keys on file COUNT, so a
        // delete-only diff (1 file, 0 bytes) must stay accepted.
        let api = ServeVerdictState::new();
        let files = vec![("src/removed.rs".to_string(), String::new())];
        let options = PushOverlayOptions {
            repo_relative: false,
            analysis_root: None,
            base_sha: Some("abc123".into()),
            source_ref: None,
            source_sha: None,
            comparison_base_sha: None,
            candidate_snapshot: None,
            changed_files: Some(vec!["src/removed.rs".into()]),
            gate: false,
            check_ids: None,
            semantic: None,
        };
        let ack = api.push_overlay_with_options("/wt", "origin/main", &files, None, Some(&options));
        assert!(ack.accepted, "delete-only diff (empty content) must pass");
        assert_eq!(ack.applied_files, 1);
    }

    #[test]
    fn plain_optionless_empty_push_stays_accepted() {
        // #A3 boundary — a bare `push_overlay` with no files and no
        // options is the legitimate local "revert RA to the on-disk
        // tree" operation; the guard must not break it.
        let api = ServeVerdictState::new();
        let ack = api.push_overlay("/wt", "origin/main", &[]);
        assert!(ack.accepted, "optionless empty push is a legal revert");
    }

    #[test]
    fn batch_member_truncation_suspect_goes_indeterminate_not_green() {
        // #A3 per-member guard — one truncated member must neither run
        // (bare-base false green) nor poison its batch-mates: the clean
        // member still executes and reports its honest verdict.
        let project = setup_batch_project("member-truncation");
        let request = batch_request(
            "batch-truncated-member",
            &project.root,
            vec![
                batch_member("clean", "src/ok.rs", "pub fn ok() {}\n"),
                BatchMember {
                    worktree: "/client/truncated".into(),
                    files: vec![],
                    changed_files: vec!["src/lost.rs".into()],
                },
            ],
        );
        let report = http_batch_check(&request);

        assert_eq!(report.verdict, BatchVerdict::Indeterminate);
        let clean = member_result(&report, "/client/clean");
        assert_eq!(
            clean.verdict,
            BatchVerdict::Green,
            "clean member's verdict survives a truncated batch-mate"
        );
        let truncated = member_result(&report, "/client/truncated");
        assert_eq!(truncated.verdict, BatchVerdict::Indeterminate);
        assert!(
            truncated
                .diagnostics
                .first()
                .is_some_and(|d| d.message.contains("suspect payload truncation")),
            "diagnostic names the truncation suspicion: {:?}",
            truncated.diagnostics
        );
    }

    #[test]
    fn batch_member_with_no_claims_and_no_files_is_not_suspect() {
        // #A3 boundary — empty changed_files AND empty files is an honest
        // "no diff vs base" member, not a truncation signature.
        let member = BatchMember::new("wt-empty");
        assert_eq!(member_truncation_suspect(&member), None);
    }

    #[test]
    fn readyz_latch_starts_false_and_mark_ready_flips_it() {
        // A6: a fresh daemon state is NOT ready (RA cold ⇒ /readyz 503,
        // k8s keeps the pod out of Service rotation); mark_ready (the
        // servedrv RA-warm flip) latches it true.
        let api = ServeVerdictState::new();
        assert!(!api.ready(), "fresh state must report not-ready");
        api.mark_ready();
        assert!(api.ready(), "after mark_ready the latch reports ready");
        // One-way: a second mark is a no-op, never an un-set.
        api.mark_ready();
        assert!(api.ready());
    }

    #[test]
    fn stale_hard_witness_never_overwrites_fresher() {
        // #A4.3 publish-once / last-writer-wins ordering: two hard
        // witnesses for the same (wt, base_sha) can coexist (a re-push of
        // the SAME commit fires its EmitVerdict while the first witness still
        // runs). Only the LATEST generation may publish; a consumed claim
        // cannot publish twice.
        let api = ServeVerdictState::new();
        let sha = Some("deadbeef");
        let g1 = api.begin_hard_witness("/wt", sha, None);
        let g2 = api.begin_hard_witness("/wt", sha, None);
        assert!(g2 > g1, "generations are monotonic");
        assert!(
            !api.finish_hard_witness("/wt", sha, g1),
            "stale witness (older claim, same commit) must not publish"
        );
        assert!(
            api.finish_hard_witness("/wt", sha, g2),
            "latest witness publishes"
        );
        assert!(
            !api.finish_hard_witness("/wt", sha, g2),
            "a consumed claim cannot publish twice (watchdog-vs-late-worker)"
        );
        // Keys are independent: a witness on another worktree is
        // unaffected by /wt's churn.
        let g3 = api.begin_hard_witness("/other", sha, None);
        assert!(api.finish_hard_witness("/other", sha, g3));
    }

    #[test]
    fn same_tree_witness_replacement_terminalizes_superseded_attempt() {
        let state_dir = temp_root("witness-superseded");
        let api = ServeVerdictState::new().with_project_check_state_dir(state_dir.clone());
        let files = vec![("src/lib.rs".to_string(), "pub fn checked() {}".to_string())];
        let first = attempt_context("attempt-first", 1);
        let successor = attempt_context("attempt-successor", 2);
        let options = |semantic: AttemptContext| PushOverlayOptions {
            base_sha: Some("same-commit".into()),
            changed_files: Some(vec!["src/lib.rs".into()]),
            semantic: Some(semantic),
            ..PushOverlayOptions::default()
        };

        assert!(
            api.push_overlay_with_options(
                "/client/wt",
                "origin/main",
                &files,
                None,
                Some(&options(first.clone())),
            )
            .accepted
        );
        let first_generation =
            api.begin_hard_witness("/client/wt", Some("same-commit"), Some(&first));
        assert_eq!(api.outcome_metrics_v3().unwrap()["pending_attempts"], 1);

        assert!(
            api.push_overlay_with_options(
                "/client/wt",
                "origin/main",
                &files,
                None,
                Some(&options(successor.clone())),
            )
            .accepted
        );
        let successor_generation =
            api.begin_hard_witness("/client/wt", Some("same-commit"), Some(&successor));

        let superseded = api
            .get_outcome_v3(&first.attempt_id)
            .expect("superseded attempt remains queryable");
        assert!(matches!(
            &superseded.conclusion,
            Conclusion::Superseded {
                successor_attempt_id,
                ..
            } if successor_attempt_id == &successor.attempt_id
        ));
        assert!(superseded.relations.iter().any(|relation| {
            relation.kind == RelationKind::SupersededBy
                && relation.attempt_id.as_ref() == Some(&successor.attempt_id)
        }));
        assert_eq!(
            superseded.reaction.state,
            cargoless_core::outcome::CheckState::NoUpdate
        );
        let metrics = api.outcome_metrics_v3().unwrap();
        assert_eq!(
            metrics["pending_attempts"], 1,
            "only the successor stays pending"
        );
        assert_eq!(metrics["terminal_by_code"]["superseded"], 1);
        assert!(
            state_dir
                .join("evidence-v3/attempt-first/outcome.json")
                .is_file()
        );
        assert!(!api.finish_hard_witness("/client/wt", Some("same-commit"), first_generation,));
        assert!(api.finish_hard_witness("/client/wt", Some("same-commit"), successor_generation,));
    }

    #[test]
    fn distinct_base_sha_witnesses_both_publish_under_one_worktree_key() {
        // THE `<absent>` FIX (core half): the witness hardcodes ONE worktree
        // key for every PR, so before this fix a newer commit's push bumped
        // the worktree-only generation and DROPPED an older commit's
        // in-flight witness ("stale-witness-dropped") — the superseded SHA's
        // poller then timed out at ~33min. Keying the latch by
        // (worktree, base_sha) makes two distinct commits independent: each
        // publishes on its own merit even though they share the worktree key.
        let api = ServeVerdictState::new();
        let g_old = api.begin_hard_witness("/workspace/tf-multiverse", Some("aaa111"), None);
        // A newer commit's push arrives for the SAME worktree key while the
        // older witness is still running.
        let g_new = api.begin_hard_witness("/workspace/tf-multiverse", Some("bbb222"), None);
        assert!(g_new > g_old, "generations stay globally monotonic");
        // The newer commit's witness finishes and publishes — fine.
        assert!(
            api.finish_hard_witness("/workspace/tf-multiverse", Some("bbb222"), g_new),
            "newer commit's witness publishes"
        );
        // The OLDER commit's witness finishes LATER and — the fix — STILL
        // publishes, because its (wt, aaa111) latch was never superseded by
        // the bbb222 push. Pre-fix this returned false and the green was lost.
        assert!(
            api.finish_hard_witness("/workspace/tf-multiverse", Some("aaa111"), g_old),
            "older commit's witness still publishes — not dropped by a newer commit's push"
        );
        // An unattributed (FS-watch) witness keeps its own one-per-worktree
        // latch, independent of either attributed commit.
        let g_fs = api.begin_hard_witness("/workspace/tf-multiverse", None, None);
        assert!(api.finish_hard_witness("/workspace/tf-multiverse", None, g_fs));
    }

    #[test]
    fn verdict_retrievable_by_base_sha_after_supersession() {
        // THE `<absent>` FIX (read half): a poller for commit X must retrieve
        // X's verdict from the base_sha-addressable ring even after a newer
        // commit Y overwrote the single live `statuses` slot. A bare lookup
        // (None) sees only the latest; an X-addressed lookup sees X; a lookup
        // for a commit that never published sees None (never cross-attributed).
        let api = ServeVerdictState::new();
        let wt = Path::new("/workspace/tf-multiverse");
        api.publish_attributed(
            wt,
            crate::statusfile::VerdictPayload::green(),
            Some("xxx".into()),
            false,
        );
        api.publish_attributed(
            wt,
            crate::statusfile::VerdictPayload::green(),
            Some("yyy".into()),
            false,
        );

        // The live slot holds the latest commit (yyy).
        assert_eq!(
            api.get_status_attributed("/workspace/tf-multiverse", None)
                .and_then(|s| s.base_sha),
            Some("yyy".into()),
            "bare lookup returns the latest-published commit"
        );
        // The superseded commit (xxx) is still retrievable by its sha.
        assert_eq!(
            api.get_status_attributed("/workspace/tf-multiverse", Some("xxx"))
                .and_then(|s| s.base_sha),
            Some("xxx".into()),
            "superseded commit's verdict survives in the ring"
        );
        // The latest commit is also retrievable by its sha.
        assert_eq!(
            api.get_status_attributed("/workspace/tf-multiverse", Some("yyy"))
                .and_then(|s| s.base_sha),
            Some("yyy".into()),
        );
        // A commit that never published is None — never another commit's verdict.
        assert!(
            api.get_status_attributed("/workspace/tf-multiverse", Some("zzz"))
                .is_none(),
            "a poll for an unknown commit never cross-attributes a different commit's verdict"
        );
    }

    #[test]
    fn verdict_ring_evicts_past_cap_and_dedupes_same_sha() {
        // The ring is bounded (oldest front-evicted past CAP) and a re-push
        // of the same commit replaces its prior entry rather than accumulating.
        let api = ServeVerdictState::new();
        let wt = Path::new("/wt");
        for i in 0..(HARD_WITNESS_HISTORY_CAP_DEFAULT + 5) {
            api.publish_attributed(
                wt,
                crate::statusfile::VerdictPayload::green(),
                Some(format!("sha-{i}")),
                false,
            );
        }
        // The oldest 5 are gone; the most recent CAP remain addressable.
        assert!(
            api.get_status_attributed("/wt", Some("sha-0")).is_none(),
            "oldest commit evicted past the cap"
        );
        let newest = format!("sha-{}", HARD_WITNESS_HISTORY_CAP_DEFAULT + 4);
        assert!(
            api.get_status_attributed("/wt", Some(newest.as_str()))
                .is_some(),
            "newest commit retained"
        );
        // Re-push the same commit: it must not grow the ring beyond CAP nor
        // duplicate — the latest verdict for that sha wins.
        api.publish_attributed(
            wt,
            crate::statusfile::VerdictPayload::green(),
            Some("sha-repeat".into()),
            false,
        );
        api.publish_attributed(
            wt,
            crate::statusfile::VerdictPayload::green(),
            Some("sha-repeat".into()),
            false,
        );
        let ring_len = poisoned(&api.verdict_history)
            .get("/wt")
            .map(|r| r.len())
            .unwrap_or(0);
        assert!(
            ring_len <= HARD_WITNESS_HISTORY_CAP_DEFAULT,
            "ring stays bounded at the cap (was {ring_len})"
        );
    }

    #[test]
    fn witness_history_cap_from_defaults_and_parses_env() {
        // Path D — pure env-parser. Unset ⇒ default. A parseable positive
        // integer ⇒ that value. Zero, negative, and unparseable input all
        // fall back to the default (CGLS-28 convention: a typo must not
        // silently shrink the addressable ring to a value nobody set).
        assert_eq!(
            witness_history_cap_from(None),
            HARD_WITNESS_HISTORY_CAP_DEFAULT,
            "unset ⇒ default (64)"
        );
        assert_eq!(witness_history_cap_from(Some("64")), 64);
        assert_eq!(witness_history_cap_from(Some("128")), 128);
        assert_eq!(
            witness_history_cap_from(Some("0")),
            HARD_WITNESS_HISTORY_CAP_DEFAULT,
            "0 ⇒ default (a zero-cap ring evicts every publish, never operator intent)"
        );
        assert_eq!(
            witness_history_cap_from(Some("bogus")),
            HARD_WITNESS_HISTORY_CAP_DEFAULT,
            "unparseable ⇒ default (CGLS-28 pattern)"
        );
        assert_eq!(
            witness_history_cap_from(Some("")),
            HARD_WITNESS_HISTORY_CAP_DEFAULT,
            "empty string ⇒ default"
        );
    }

    #[test]
    fn pushed_max_per_wt_from_defaults_and_parses_env() {
        // R3 — same env-parser shape as `witness_history_cap_from`.
        assert_eq!(
            pushed_max_per_wt_from(None),
            PUSHED_MAX_PER_WT_DEFAULT,
            "unset ⇒ default (8)"
        );
        assert_eq!(pushed_max_per_wt_from(Some("8")), 8);
        assert_eq!(pushed_max_per_wt_from(Some("2")), 2);
        assert_eq!(
            pushed_max_per_wt_from(Some("0")),
            PUSHED_MAX_PER_WT_DEFAULT,
            "0 ⇒ default (a zero-cap queue rejects EVERY push, never intended)"
        );
        assert_eq!(
            pushed_max_per_wt_from(Some("bogus")),
            PUSHED_MAX_PER_WT_DEFAULT,
            "unparseable ⇒ default"
        );
    }

    #[test]
    fn pushed_queue_rejects_distinct_sha_at_cap_with_429_body() {
        // R3 mitigation — with `pushed_max_per_wt = 2`, the third
        // distinct-base_sha push MUST be rejected with `reject_http_status
        // = 429` and a structured body carrying the cap and the wt id.
        // The daemon accepts the first two so the queue truly has depth 2
        // when the third arrives.
        let api = ServeVerdictState::new().with_caps_for_testing(64, 2);
        let files = vec![("src/lib.rs".to_string(), "pub fn a() {}".to_string())];
        let opts = |sha: &str| PushOverlayOptions {
            base_sha: Some(sha.to_string()),
            ..PushOverlayOptions::default()
        };

        let a =
            api.push_overlay_with_options("/wt", "origin/main", &files, None, Some(&opts("aaa")));
        assert!(a.accepted, "first push accepted");
        assert_eq!(a.reject_http_status, None);
        let b =
            api.push_overlay_with_options("/wt", "origin/main", &files, None, Some(&opts("bbb")));
        assert!(b.accepted, "second push accepted (still under cap=2)");
        assert_eq!(b.reject_http_status, None);

        let c =
            api.push_overlay_with_options("/wt", "origin/main", &files, None, Some(&opts("ccc")));
        assert!(!c.accepted, "third distinct-sha push rejected (queue full)");
        assert_eq!(c.reject_http_status, Some(429), "rejects as HTTP 429");
        let body = c.reject_body.as_deref().unwrap_or_default();
        assert!(
            body.contains("\"error\":\"pushed_queue_full\""),
            "reject body carries structured error: {body}"
        );
        assert!(
            body.contains("\"cap\":2"),
            "reject body carries cap: {body}"
        );
        assert!(
            body.contains("\"wt\":\"/wt\""),
            "reject body carries the wt key: {body}"
        );
    }

    #[test]
    fn pushed_queue_same_sha_replaces_in_place_even_at_cap() {
        // R3 mitigation — a legitimate retry of an ALREADY-QUEUED commit
        // (same base_sha) must NOT be starved by the cap; it replaces
        // the queued entry in place, preserving CGLS-25's latest-wins
        // per-sha semantics.
        let api = ServeVerdictState::new().with_caps_for_testing(64, 2);
        let files_v1 = vec![("src/lib.rs".to_string(), "pub fn v1() {}".to_string())];
        let files_v2 = vec![("src/lib.rs".to_string(), "pub fn v2() {}".to_string())];
        let opts = |sha: &str| PushOverlayOptions {
            base_sha: Some(sha.to_string()),
            ..PushOverlayOptions::default()
        };

        assert!(
            api.push_overlay_with_options(
                "/wt",
                "origin/main",
                &files_v1,
                None,
                Some(&opts("aaa"))
            )
            .accepted
        );
        assert!(
            api.push_overlay_with_options(
                "/wt",
                "origin/main",
                &files_v1,
                None,
                Some(&opts("bbb"))
            )
            .accepted
        );
        // Queue is now at cap (2). Re-push of an already-queued sha ⇒
        // replace-in-place, ack must stay accepted, depth unchanged.
        let retry = api.push_overlay_with_options(
            "/wt",
            "origin/main",
            &files_v2,
            None,
            Some(&opts("aaa")),
        );
        assert!(
            retry.accepted,
            "same-sha retry at cap must replace in place, not reject"
        );
        assert_eq!(retry.reject_http_status, None);
        let depth = poisoned(&api.pushed)
            .get("/wt")
            .map(|q| q.len())
            .unwrap_or(0);
        assert_eq!(depth, 2, "queue depth stays at cap after same-sha replace");
    }

    #[test]
    fn gated_checks_ran_stamps_through_to_attributed_status() {
        // Commit-2: the witness needs a positive "the gated check ran" proof.
        // `publish_attributed_with_checks` must stamp the ran ids onto the
        // status retrievable by base_sha (the ring) AND the live slot, and
        // the JSON wire must carry them only when non-empty (additive).
        let api = ServeVerdictState::new();
        let wt = Path::new("/workspace/tf-multiverse");
        api.publish_attributed_with_checks(
            wt,
            crate::statusfile::VerdictPayload::green(),
            Some("abc123".into()),
            false,
            vec!["wasm-compiler-witness".into(), "fmt".into()],
            None,
        );

        // Retrievable by base_sha with the ran ids intact (the witness polls
        // /status?base_sha=COMMIT and reads gated_checks_ran from the body).
        let by_sha = api
            .get_status_attributed("/workspace/tf-multiverse", Some("abc123"))
            .expect("verdict retrievable by its base_sha");
        assert_eq!(
            by_sha.gated_checks_ran,
            vec!["wasm-compiler-witness".to_string(), "fmt".to_string()],
            "ran-check ids survive into the base_sha-addressable status"
        );
        // And on the wire, in order, only because the list is non-empty.
        let wire = cargoless_core::transport::status_to_json(&by_sha);
        assert!(
            wire.contains(r#""gated_checks_ran":["wasm-compiler-witness","fmt"]"#),
            "ran ids appear on the wire in order: {wire}"
        );

        // The 4-arg `publish_attributed` (the unattributed wrapper / every
        // pre-Commit-2 caller) must leave the list empty ⇒ absent on the wire.
        let api2 = ServeVerdictState::new();
        api2.publish_attributed(
            Path::new("/wt2"),
            crate::statusfile::VerdictPayload::green(),
            Some("def456".into()),
            false,
        );
        let plain = api2
            .get_status_attributed("/wt2", Some("def456"))
            .expect("verdict present");
        assert!(
            plain.gated_checks_ran.is_empty(),
            "the 4-arg form leaves gated_checks_ran empty"
        );
        assert!(
            !cargoless_core::transport::status_to_json(&plain).contains("gated_checks_ran"),
            "empty ran-checks list is absent on the wire (additive contract)"
        );
    }

    #[test]
    fn gated_push_dispatches_directly_without_entering_ra_queue() {
        let api = ServeVerdictState::new();
        let (direct_tx, direct_rx) = channel();
        api.attach_direct_gate_signal(direct_tx);
        let files = vec![("src/lib.rs".to_string(), "pub fn x() {}".to_string())];
        let context = attempt_context("attempt-gated", 1);
        let options = PushOverlayOptions {
            gate: true,
            base_sha: Some("candidate".to_string()),
            check_ids: Some(vec!["ssr-compiler-witness".to_string()]),
            semantic: Some(context.clone()),
            ..Default::default()
        };
        let ack =
            api.push_overlay_with_options("/wt-gate", "origin/main", &files, None, Some(&options));
        assert!(ack.accepted);
        assert!(
            matches!(
                api.get_outcome_v3(&context.attempt_id)
                    .expect("accepted gated attempt must publish an outcome synchronously")
                    .conclusion,
                Conclusion::Pending { .. }
            ),
            "POST /v3/attempts must never acknowledge a gated attempt before its pending outcome exists"
        );
        assert!(
            api.peek_overlay_for("/wt-gate").is_none(),
            "gated work must not enter the shared RA overlay queue"
        );
        let direct = direct_rx.recv_timeout(Duration::from_millis(200)).unwrap();
        assert_eq!(direct.wt, PathBuf::from("/wt-gate"));
        assert!(direct.context.gate);
        assert_eq!(
            direct.context.check_ids,
            Some(vec!["ssr-compiler-witness".to_string()])
        );
        assert!(
            direct.context.source_sha.is_none(),
            "legacy body-overlay gates remain supported during rollout"
        );

        let ack = api.push_overlay_with_options("/wt-plain", "", &files, None, None);
        assert!(ack.accepted);
        assert!(
            !api.peek_overlay_for("/wt-plain").expect("stored").gate,
            "optionless push defaults gate=false (warn-fast posture)"
        );
    }

    #[test]
    fn disconnected_gate_dispatcher_does_not_strand_a_pending_outcome() {
        let api = ServeVerdictState::new();
        let (direct_tx, direct_rx) = channel();
        drop(direct_rx);
        api.attach_direct_gate_signal(direct_tx);
        let context = attempt_context("attempt-disconnected-gate", 1);
        let options = PushOverlayOptions {
            gate: true,
            base_sha: Some("candidate".to_string()),
            semantic: Some(context.clone()),
            ..Default::default()
        };

        let ack = api.push_overlay_with_options(
            "/wt-disconnected-gate",
            "origin/main",
            &[("src/lib.rs".to_string(), "pub fn x() {}".to_string())],
            None,
            Some(&options),
        );

        assert!(!ack.accepted);
        assert!(
            api.get_outcome_v3(&context.attempt_id).is_none(),
            "a rejected dispatch must be retryable with the same attempt id"
        );
    }

    #[test]
    fn record_project_check_context_carries_gate_through_take() {
        let api = ServeVerdictState::new();
        api.record_project_check_context(
            "/wt",
            ProjectCheckRunContext {
                root: PathBuf::from("/root"),
                changed_files: None,
                base_ref: String::new(),
                base_sha: None,
                source_ref: None,
                source_sha: None,
                candidate_snapshot: None,
                overlay_files: Vec::new(),
                materialize_overlay: false,
                gate: true,
                check_ids: None,
            },
        );
        let ctx = api.take_project_check_context("/wt").expect("recorded");
        assert!(ctx.gate, "gate survives the record→take round trip");
        assert!(
            api.take_project_check_context("/wt").is_none(),
            "take consumes"
        );
    }

    // ── CGLS-26: warm shared witness target dir ──────────────────────────

    /// Serializes the two tests that flip `CARGOLESS_WITNESS_WARM_TARGET`.
    /// Env mutation is process-global; without this, one warm test removing
    /// the var mid-flight fails the other — the exact flake class the
    /// appserve `CARGOLESS_APP_PARALLEL_BUILDS` tests already exhibit.
    /// (Other tests are unaffected by a transiently-set flag: their scratch
    /// roots have no Cargo.lock, so key resolution fails ⇒ cold, as before.)
    ///
    /// **Acquire with [`poisoned`], never `.lock().unwrap()`.** These two
    /// tests share this mutex, so when one of them panics it POISONS it and
    /// the sibling then dies on the unwrap — turning a single failure into
    /// two, and burying the real one under a `PoisonError { .. }` that names
    /// nothing. That misreporting has now cost a diagnosis twice (`df0ecab`
    /// on 2026-07-29; again on 2026-07-30), each time sending a reader after
    /// a lock bug that was never there.
    ///
    /// A poisoned env mutex carries no corrupt state to protect: the guard
    /// exists only to serialize `set_var`/`remove_var`, and each test sets
    /// the var it needs on entry. Ignoring poison is therefore correct here,
    /// not a papering-over — it makes the failing test the ONLY test that
    /// fails, which is the whole point of the guard.
    static WARM_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn warm_scratch_with_lockfile(label: &str) -> PathBuf {
        let root = temp_root(label);
        std::fs::write(root.join("Cargo.lock"), "# lock v1\n").unwrap();
        root
    }

    /// Same (toolchain, Cargo.lock) ⇒ same key; different lock bytes ⇒
    /// different key; missing lock ⇒ `None` (fail-closed to cold).
    #[test]
    fn warm_target_key_is_deterministic_per_lockfile() {
        let root = warm_scratch_with_lockfile("warm-key-det");
        let k1 = warm_target_key(&root).expect("toolchain + lock present ⇒ key");
        let k2 = warm_target_key(&root).expect("second resolve");
        assert_eq!(
            k1, k2,
            "key is a pure function of (schema, toolchain, lock)"
        );

        std::fs::write(root.join("Cargo.lock"), "# lock v2 — dep graph moved\n").unwrap();
        let k3 = warm_target_key(&root).expect("changed lock still resolves");
        assert_ne!(k1, k3, "a Cargo.lock change must land in a FRESH warm dir");

        std::fs::remove_file(root.join("Cargo.lock")).unwrap();
        assert!(
            warm_target_key(&root).is_none(),
            "no Cargo.lock ⇒ no key ⇒ cold fallback"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// flock layer: a held `.witness-lock` makes a second non-blocking
    /// acquire report contended (`Ok(None)`), and release makes it
    /// acquirable again. flock(2) locks are per open-file-description, so
    /// two opens in ONE process genuinely conflict — this exercises the
    /// real cross-process semantics.
    #[test]
    fn warm_flock_second_acquire_contended_until_release() {
        let root = temp_root("warm-flock");
        let lock_path = root.join(".witness-lock");
        let held = WarmFlock::acquire_nb(&lock_path)
            .expect("io ok")
            .expect("first acquire wins");
        assert!(
            WarmFlock::acquire_nb(&lock_path)
                .expect("contended is Ok(None), not Err")
                .is_none(),
            "second acquire while held ⇒ contended ⇒ caller goes cold"
        );
        drop(held);
        assert!(
            WarmFlock::acquire_nb(&lock_path).expect("io ok").is_some(),
            "released lock is acquirable again"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// GC: keeps the newest [`WARM_TARGET_KEEP`] dirs by `.last-used` stamp,
    /// never removes the active dir or a dir whose `.witness-lock` is held,
    /// and removes stale unlocked ones.
    #[test]
    fn prune_warm_dirs_keeps_active_and_locked_removes_stale() {
        let state_dir = temp_root("warm-prune-state");
        let warm_root = state_dir.join("witness-target-warm");
        // Oldest→newest: stale, locked, mid, active. Distinct stamp mtimes.
        let mut dirs = Vec::new();
        for name in ["stale", "locked", "mid", "active"] {
            let d = warm_root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(".last-used"), "").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(25));
            dirs.push(d);
        }
        let (stale, locked, mid, active) = (&dirs[0], &dirs[1], &dirs[2], &dirs[3]);
        let _held = WarmFlock::acquire_nb(&locked.join(".witness-lock"))
            .expect("io ok")
            .expect("test holds the busy dir's lock");

        prune_warm_target_dirs(&state_dir, active);

        assert!(active.exists(), "active dir is never pruned");
        assert!(mid.exists(), "2nd-newest is within WARM_TARGET_KEEP=2");
        assert!(
            locked.exists(),
            "flock-held dir is skipped (in use elsewhere)"
        );
        assert!(
            !stale.exists(),
            "stale unlocked dir past keep-count is removed"
        );
        let _ = std::fs::remove_dir_all(state_dir);
    }

    /// The counter must have BOUNDED cardinality (reasons embed errnos and
    /// byte counts) while keeping the two contention interlocks apart — which
    /// one fired is the diagnostic that tells in-process CAS from flock.
    #[test]
    fn warm_obs_bucket_bounds_cardinality_but_keeps_interlocks_distinct() {
        // Variable tails collapse to a stable head.
        assert_eq!(
            warm_obs_bucket("mkdir:Permission denied (os error 13)"),
            "mkdir"
        );
        assert_eq!(
            warm_obs_bucket("flock-open:No such file (os error 2)"),
            "flock-open"
        );
        assert_eq!(
            warm_obs_bucket("disk-pressure:free=1024MiB,warm=16384MiB,need=32768MiB"),
            "disk-pressure"
        );
        // Two disk-pressure events with different numbers share one bucket —
        // otherwise every event is its own series and nothing ever sums.
        assert_eq!(
            warm_obs_bucket("disk-pressure:free=1MiB,warm=2MiB,need=4MiB"),
            warm_obs_bucket("disk-pressure:free=9MiB,warm=8MiB,need=16MiB"),
        );
        // But the interlocks stay separable.
        assert_eq!(warm_obs_bucket("contended:in-proc"), "contended:in-proc");
        assert_eq!(warm_obs_bucket("contended:flock"), "contended:flock");
        assert_ne!(
            warm_obs_bucket("contended:in-proc"),
            warm_obs_bucket("contended:flock"),
            "collapsing these loses which interlock fired"
        );
        assert_eq!(warm_obs_bucket("hit"), "hit");
    }

    /// `GET /daemon`'s `warm_target` field: absent before anything resolves
    /// (never a misleading zero), then counting both outcomes.
    #[test]
    fn warm_target_stats_json_absent_until_first_resolve_then_counts() {
        let api = ServeVerdictState::new();
        assert!(
            api.warm_target_stats_json().is_none(),
            "no resolutions yet ⇒ omit the field rather than publish 0"
        );

        let dir = temp_root("warm-stats");
        api.record_warm_obs(&dir, "warm", "hit");
        api.record_warm_obs(&dir, "warm", "hit");
        api.record_warm_obs(&dir, "cold-fallback", "contended:flock");
        api.record_warm_obs(
            &dir,
            "cold-fallback",
            "disk-pressure:free=1MiB,warm=2MiB,need=4MiB",
        );
        api.record_warm_obs(
            &dir,
            "cold-fallback",
            "disk-pressure:free=7MiB,warm=9MiB,need=18MiB",
        );

        let v = api.warm_target_stats_json().expect("stats after recording");
        assert_eq!(v["warm"], 2);
        assert_eq!(
            v["cold_fallback"]["disk-pressure"], 2,
            "byte-count variants aggregate into one alertable series"
        );
        assert_eq!(v["cold_fallback"]["contended:flock"], 1);
        assert!(
            v["cold_fallback"].get("contended:in-proc").is_none(),
            "reasons that never fired must not appear"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// `dir_size_bytes` totals the tree, recurses, and does not follow
    /// symlinks out of it. An undercount would defeat the disk rung exactly
    /// when the warm dir is biggest, so this pins the arithmetic.
    #[test]
    fn dir_size_bytes_totals_recursively_without_following_symlinks() {
        let root = temp_root("warm-dirsize");
        std::fs::write(root.join("a.bin"), vec![0u8; 1000]).unwrap();
        std::fs::create_dir_all(root.join("nested/deep")).unwrap();
        std::fs::write(root.join("nested/b.bin"), vec![0u8; 2000]).unwrap();
        std::fs::write(root.join("nested/deep/c.bin"), vec![0u8; 3000]).unwrap();

        let plain = dir_size_bytes(&root).expect("readable tree");
        assert_eq!(plain, 6000, "recursive total across all three files");

        // A symlink to a large file outside the tree must not be followed —
        // it would inflate the total and could point anywhere.
        let outside = temp_root("warm-dirsize-outside");
        std::fs::write(outside.join("big.bin"), vec![0u8; 500_000]).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("big.bin"), root.join("link.bin")).unwrap();
            let with_link = dir_size_bytes(&root).expect("readable tree");
            assert!(
                with_link < 6000 + 500_000,
                "symlink target must not be counted; got {with_link}"
            );
        }

        // Missing dir ⇒ None ⇒ caller fails OPEN (keeps warm).
        assert!(
            dir_size_bytes(&root.join("does-not-exist")).is_none(),
            "unreadable ⇒ None so the caller cannot act on a partial total"
        );

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    /// The disk rung's decision logic, pinned at both polarities.
    ///
    /// An empty warm dir must NEVER trip (need == 0): that is the first run
    /// for a key, when there is no cache to protect and the compile has to
    /// write somewhere regardless. A tripping empty dir would mean warm could
    /// never bootstrap.
    #[test]
    fn warm_dir_disk_pressure_never_trips_on_empty_and_fails_open_when_unmeasurable() {
        let root = temp_root("warm-diskpressure");
        assert!(
            warm_dir_disk_pressure(&root).is_none(),
            "empty warm dir must not trip, or warm can never bootstrap"
        );

        // A small dir must stay warm REGARDLESS of how full the volume is —
        // that is the floor, not a ratio that happens to pass here. Without
        // it this very assertion fails on the disk-pressured CI runner while
        // passing on a roomy laptop, which is how the first push of this rung
        // turned the two pre-existing CGLS-26 tests RED.
        std::fs::write(root.join("small.bin"), vec![0u8; 4096]).unwrap();
        assert!(
            warm_dir_disk_pressure(&root).is_none(),
            "4 KiB dir is below WARM_DISK_MIN_INTERESTING_BYTES ⇒ exempt"
        );

        // Nonexistent path: `df` fails ⇒ None ⇒ fail OPEN. Unmeasurable is
        // not evidence of full, and this rung guards cost, not correctness.
        assert!(
            warm_dir_disk_pressure(&root.join("nope")).is_none(),
            "unmeasurable ⇒ fail open (opposite polarity from the lock rungs)"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// The threshold itself, independent of any real filesystem: free space
    /// below `WARM_DISK_HEADROOM_X` × dir size trips, at-or-above does not.
    #[test]
    fn warm_disk_pressure_threshold_arithmetic() {
        // Mirrors `warm_dir_disk_pressure`'s decision, floor first.
        let trips = |free: u64, used: u64| {
            used >= WARM_DISK_MIN_INTERESTING_BYTES
                && free < used.saturating_mul(WARM_DISK_HEADROOM_X)
        };
        assert_eq!(WARM_DISK_HEADROOM_X, 2, "doc + reason string assume 2x");
        // The live witness-b shape that motivated the rung: ~16 GiB warm dir,
        // and a Cargo.lock change would want a whole second key alongside it.
        assert!(
            trips(20 << 30, 16 << 30),
            "20 GiB free vs 16 GiB warm ⇒ cold"
        );
        assert!(
            !trips(35 << 30, 16 << 30),
            "35 GiB free vs 16 GiB warm ⇒ warm"
        );
        // Exactly at the boundary is NOT pressure (`<`, not `<=`).
        assert!(!trips(32 << 30, 16 << 30), "exactly 2x is sufficient");

        // The floor. A tiny dir on a nearly-full volume must NOT trip: the
        // bare ratio said it should, which reddened the two pre-existing
        // CGLS-26 warm-target tests on the disk-pressured CI runner
        // (`ci.yml` header). There is no multi-GiB cache to protect there.
        assert!(!trips(0, 0), "empty dir never trips");
        assert!(
            !trips(0, 4096),
            "4 KiB dir on a full disk ⇒ below floor ⇒ stay warm"
        );
        assert!(
            !trips(1 << 20, (1 << 30) - 1),
            "just under the floor is still exempt"
        );
        assert!(
            trips(1 << 20, 1 << 30),
            "at the floor with 1 MiB free ⇒ genuinely at risk ⇒ cold"
        );
    }

    /// `free_bytes_at` parses `df -Pk` into a plausible byte count for a path
    /// that certainly exists, and returns `None` for one that does not.
    #[test]
    fn free_bytes_at_parses_df_and_fails_open_on_bad_path() {
        let root = temp_root("warm-freebytes");
        let free = free_bytes_at(&root).expect("df works on the temp dir");
        assert!(
            free > (1 << 20),
            "a usable temp filesystem should report more than 1 MiB free, got {free}"
        );
        assert!(
            free_bytes_at(Path::new("/definitely/not/a/real/mount/point")).is_none(),
            "df failure ⇒ None ⇒ caller fails open"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// Default OFF: with the flag unset, `resolve_warm_target` is `None`
    /// (cold, byte-identical to today) even when everything else — key,
    /// locks, dirs — would resolve.
    #[test]
    fn resolve_warm_target_flag_off_is_cold() {
        let _env = poisoned(&WARM_ENV_LOCK);
        // SAFETY: env mutation is process-global; serialized by
        // WARM_ENV_LOCK against the other warm-flag test, and behavior-
        // neutral for non-warm tests (see WARM_ENV_LOCK doc).
        unsafe { std::env::remove_var("CARGOLESS_WITNESS_WARM_TARGET") };
        let api = ServeVerdictState::new();
        let state_dir = temp_root("warm-off-state");
        let scratch = warm_scratch_with_lockfile("warm-off-scratch");
        assert!(
            api.resolve_warm_target(&state_dir, &scratch).is_none(),
            "flag unset ⇒ cold; the warm path must be opt-in"
        );
        let _ = std::fs::remove_dir_all(state_dir);
        let _ = std::fs::remove_dir_all(scratch);
    }

    /// Flag ON: first resolve wins the warm dir; a second resolve for the
    /// SAME key while the first guard is held goes cold (in-proc busy CAS);
    /// dropping the guard makes the key warm-resolvable again. This is the
    /// serialization interlock that keeps CGLS-24 structurally impossible.
    ///
    /// This test caught 3 intermittent failures across the 2026-07-30/31 CI
    /// runs. The final assertion failed while every other test passed.
    ///
    /// LOCALISED TO THE FLOCK RUNG. Task 207005 captured the obs lines, which
    /// map one-to-one onto the three resolves:
    ///
    ///     mode=warm          reason=hit                #1 takes in-proc + flock
    ///     mode=cold-fallback reason=contended:in-proc  #2 correct, key busy
    ///     mode=cold-fallback reason=contended:flock    #3 AFTER drop(first)
    ///
    /// Resolve #3 got PAST the in-proc CAS — so `InProcWarmGuard::drop` did
    /// clear `busy` — and was refused by the flock. That rules out, by
    /// evidence rather than argument:
    ///   - the in-process layer (it released; #3 got past it);
    ///   - `prune_warm_target_dirs` — the refusal is at step 3b, BEFORE prune
    ///     runs at step 4, so prune is not on the failing path at all;
    ///   - the disk-pressure rung (step 5, also after);
    ///   - cross-test interference — `temp_root` is `{label}-{pid}-{nanos}`,
    ///     and the poisoned-`WARM_ENV_LOCK` pattern is excluded because
    ///     `resolve_warm_target_flag_off_is_cold` AND
    ///     `warm_flock_second_acquire_contended_until_release` both passed in
    ///     the same run.
    ///
    /// The lifecycle bug was that `WarmTargetGuard` exposed the in-process key
    /// as idle before releasing its flock: Rust drops struct fields in
    /// declaration order, and `_in_proc` preceded `_flock`. A racing resolver
    /// could therefore pass the CAS while the prior file description still
    /// owned the kernel lock. The guard now explicitly `LOCK_UN`s and closes
    /// the flock before its idempotent in-process release. This assertion pins
    /// the complete handoff and remains unignored so that either lock leaking
    /// becomes a visible regression.
    ///
    /// Until the CI de-dupe landed such a failure was invisible — the duplicate
    /// push/pull_request matrices raced and cancelled each other, so a flaky
    /// run reported as `cancelled` and read as infrastructure noise.
    #[test]
    fn resolve_warm_target_contended_key_goes_cold_until_release() {
        let _env = poisoned(&WARM_ENV_LOCK);
        // SAFETY: see `resolve_warm_target_flag_off_is_cold`.
        unsafe { std::env::set_var("CARGOLESS_WITNESS_WARM_TARGET", "1") };
        let api = ServeVerdictState::new();
        let state_dir = temp_root("warm-cas-state");
        let scratch = warm_scratch_with_lockfile("warm-cas-scratch");

        let first = api
            .resolve_warm_target(&state_dir, &scratch)
            .expect("flag on + key resolvable ⇒ warm");
        assert!(
            first.dir.starts_with(state_dir.join("witness-target-warm")),
            "warm dir lives under <state_dir>/witness-target-warm/"
        );
        assert!(
            api.resolve_warm_target(&state_dir, &scratch).is_none(),
            "same key while held ⇒ contended ⇒ cold (never share a live dir)"
        );
        drop(first);
        assert!(
            api.resolve_warm_target(&state_dir, &scratch).is_some(),
            "guard drop releases both lock layers ⇒ key is warm again"
        );

        unsafe { std::env::remove_var("CARGOLESS_WITNESS_WARM_TARGET") };
        let _ = std::fs::remove_dir_all(state_dir);
        let _ = std::fs::remove_dir_all(scratch);
    }
}
