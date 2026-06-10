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
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use cargoless_core::batch::{BatchChecker, BatchMember, BatchReport, BatchVerdict, run_batch};
use cargoless_core::corun::CorunPolicy;
use cargoless_core::project_checks::{ProjectCheckReport, plan_dev_with_changes};
use cargoless_core::transport::{
    BatchCheckRequest, CheckProfile, DaemonActivity, PushOverlayAck, PushOverlayOptions,
    TransitionEvent, VerdictService, WorktreeStatus, WorktreeSummary,
};
use cargoless_core::{Diagnostic, Severity, TreeState};

/// Poison-tolerant lock (same discipline as `model::poisoned` /
/// `inproc::testmock`): a panicked verdict path must not wedge the read
/// plane — recover the guard and carry on (best-effort transport ethos).
fn poisoned<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

static PROJECT_CHECK_SCRATCH_SEQ: AtomicU64 = AtomicU64::new(1);
/// #A4.3 — global hard-witness generation source. Monotonic and never
/// recycled across worktrees, so `finish_hard_witness`'s equality check
/// can never be fooled by a reused value.
static HARD_WITNESS_SEQ: AtomicU64 = AtomicU64::new(0);
const PROJECT_CHECK_MANIFEST_NAME: &str = "cargoless.checks.yaml";

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectCheckRunContext {
    pub root: PathBuf,
    pub changed_files: Option<Vec<String>>,
    pub base_ref: String,
    pub overlay_files: Vec<(String, String)>,
    pub materialize_overlay: bool,
    /// Carried from [`PushedOverlay::gate`]: the EmitVerdict arm promotes
    /// Warn → Hard for this push when set.
    pub gate: bool,
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
    /// `PushedOverlay::last_push_unix` — wall-clock push receipt (seconds).
    pub push_received_unix: u64,
    /// Wall-clock + monotonic pair captured together at overlay-apply, so
    /// publish-time latency = coarse queue wait (receipt→consume, second
    /// granularity) + exact analysis time (consume→publish, monotonic ms).
    pub consumed_unix: u64,
    pub consumed_at: Instant,
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

/// The serve-loop's live verdict state, presented as the shipped logical
/// [`VerdictService`]. `Send + Sync` (the trait demands it so the
/// HTTP/Unix adapters can share one service across connection threads):
/// the four `Mutex`-guarded fields satisfy that by construction.
#[derive(Default)]
pub struct ServeVerdictState {
    /// worktree-key → last published status. Keyed by the SAME string
    /// `servedrv::publish_verdict` uses (`wt.to_string_lossy()`), so a
    /// remote `get_status(<wt>)` resolves the exact tree the loop
    /// attributed.
    statuses: Mutex<BTreeMap<String, WorktreeStatus>>,
    /// Live transition-event subscribers (the SSE / in-proc fan-out).
    /// Retain-on-send like `model`'s buses so a dropped subscriber never
    /// stalls the (single) producer.
    subs: Mutex<Vec<Sender<TransitionEvent>>>,
    /// #240/2b — pushed-overlay store. worktree-key →
    /// [`PushedOverlay`]. Populated by `push_overlay` (the
    /// [`VerdictService`] write-plane ingest), consumed once by
    /// `take_overlay_for` (the serve loop's SwitchOverlay arm). The
    /// `take` is **pop-on-consume semantic** (spike open-question #3
    /// default): once consumed, the WT falls back to the FS path until
    /// a fresh push arrives. Per-WT serialization (a new push for the
    /// same WT REPLACES the prior overlay before consumption) is the
    /// natural BTreeMap semantic.
    pushed: Mutex<BTreeMap<String, PushedOverlay>>,
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
    /// Optional server-side coalescing for explicit `coalesce_key`
    /// batch-check requests. Absent key keeps historical immediate behavior.
    batch_coalescer: BatchCoalescer,
    /// Server-local state directory used for transient project-check
    /// scratch worktrees. `None` keeps the in-root v0 path for unit tests
    /// and embedded callers that do not have a resolved fleet config.
    project_check_state_dir: Option<PathBuf>,
    /// Per-worktree Hard-witness generation counter. The latest generation
    /// for each wt-key is the only witness that may publish; stale witnesses
    /// (from a prior push whose EmitVerdict fired while a newer push's witness
    /// is already running) are detected by `finish_hard_witness` and dropped.
    /// The counter values are sourced from the module-level
    /// `HARD_WITNESS_SEQ` atomic, which is globally monotonic and never
    /// recycled, so a recycled match is structurally impossible.
    hard_witness_generation: Mutex<BTreeMap<String, u64>>,
}

#[derive(Default)]
struct DrainState {
    quiescing: bool,
    active_worktrees: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct BatchCoalesceKey {
    coalesce_key: String,
    base_ref: String,
    analysis_root: Option<String>,
    repo_relative: bool,
    check_profile: String,
    corun: bool,
}

#[derive(Debug, Clone, Copy)]
struct BatchCoalesceConfig {
    debounce: Duration,
    max_wait: Duration,
    max_members: usize,
}

impl Default for BatchCoalesceConfig {
    fn default() -> Self {
        Self {
            debounce: configured_batch_duration("CARGOLESS_BATCH_DEBOUNCE_MS", 250),
            max_wait: configured_batch_duration("CARGOLESS_BATCH_MAX_WAIT_MS", 1000),
            max_members: configured_batch_usize("CARGOLESS_BATCH_MAX_MEMBERS", 40),
        }
    }
}

#[derive(Default)]
struct BatchCoalescer {
    state: Mutex<BatchCoalescerState>,
    cv: Condvar,
    config: BatchCoalesceConfig,
}

#[derive(Default)]
struct BatchCoalescerState {
    queues: BTreeMap<BatchCoalesceKey, BatchQueue>,
    inflight_runs: u32,
    next_run_seq: u64,
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
            if let Some(report) = poisoned(&waiter.result).clone() {
                return report;
            }

            let mut state = poisoned(&self.state);
            let Some(queue) = state.queues.get_mut(&key) else {
                state = self
                    .cv
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                drop(state);
                continue;
            };
            if !queue.leader_active {
                queue.leader_active = true;
                drop(state);

                self.wait_until_flush(&key);
                let group = self.drain_group(&key);
                if group.is_empty() {
                    self.finish_leader(&key);
                    continue;
                }
                let run_start = Instant::now();
                let queue_wait_ms: Vec<u128> = group
                    .iter()
                    .map(|waiter| run_start.duration_since(waiter.enqueued_at).as_millis())
                    .collect();

                let run_seq = {
                    let mut state = poisoned(&self.state);
                    state.inflight_runs = state.inflight_runs.saturating_add(1);
                    state.next_run_seq = state.next_run_seq.saturating_add(1);
                    state.next_run_seq
                };
                let combined = combined_request_for(&key, &group, run_seq);
                // A panic in the physical run (e.g. OOM compiling the union)
                // must NOT leave the already-drained non-leader waiters parked
                // forever. Catch it, fan out an indeterminate report to the whole
                // group, and still release the leader slot so the queue recovers.
                let combined_report =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&combined)))
                        .unwrap_or_else(|_| {
                            batch_indeterminate(
                                &combined,
                                "coalesced batch run panicked; resubmit to retry",
                            )
                        });
                {
                    let mut state = poisoned(&self.state);
                    state.inflight_runs = state.inflight_runs.saturating_sub(1);
                }
                distribute_combined_report(&group, &combined_report, &queue_wait_ms);
                self.finish_leader(&key);
                continue;
            }

            let state = self
                .cv
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(state);
        }
    }

    fn wait_until_flush(&self, key: &BatchCoalesceKey) {
        loop {
            let state = poisoned(&self.state);
            let Some(queue) = state.queues.get(key) else {
                return;
            };
            let now = Instant::now();
            let first_at = queue.first_at.unwrap_or(now);
            let last_at = queue.last_at.unwrap_or(now);
            let members = queue_member_count(queue);
            if members >= self.config.max_members
                || now.duration_since(first_at) >= self.config.max_wait
                || now.duration_since(last_at) >= self.config.debounce
            {
                return;
            }
            let until_quiet = self
                .config
                .debounce
                .saturating_sub(now.duration_since(last_at));
            let until_max = self
                .config
                .max_wait
                .saturating_sub(now.duration_since(first_at));
            let wait_for = until_quiet.min(until_max);
            let (_state, _timeout) = self
                .cv
                .wait_timeout(state, wait_for)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn drain_group(&self, key: &BatchCoalesceKey) -> Vec<Arc<BatchWaiter>> {
        let mut state = poisoned(&self.state);
        let Some(queue) = state.queues.get_mut(key) else {
            return Vec::new();
        };
        let mut group = Vec::new();
        let mut member_count = 0usize;
        while let Some(next) = queue.waiters.front() {
            let next_members = next.request.members.len().max(1);
            if !group.is_empty() && member_count + next_members > self.config.max_members {
                break;
            }
            let next = queue.waiters.pop_front().expect("front existed");
            member_count += next_members;
            group.push(next);
        }
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
    })
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
    pub fn new() -> Self {
        Self::default()
    }

    /// Use the daemon's resolved state directory for transient
    /// project-check scratch worktrees. This keeps slow advisory/project
    /// checks out of the shared mutable analysis root.
    pub fn with_project_check_state_dir(mut self, state_dir: PathBuf) -> Self {
        self.project_check_state_dir = Some(state_dir);
        self
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
        self.publish_attributed(wt, payload, None);
    }

    /// [`Self::publish`] with verdict attribution (#A2): `base_sha` is the
    /// client-resolved commit from the overlay push this verdict answers,
    /// popped by servedrv's `publish_verdict` via
    /// [`Self::take_push_attribution`] at the sole attribution site.
    /// `None` ⇒ FS-watch / legacy verdict — the wire key stays absent.
    pub fn publish_attributed(
        &self,
        wt: &Path,
        payload: crate::statusfile::VerdictPayload,
        base_sha: Option<String>,
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
            // Freshly published ⇒ age computed at read time (get_status)
            // from `published_at` so a remote reader sees an honest age.
            heartbeat_age_secs: 0,
            published_at,
        };
        poisoned(&self.statuses).insert(worktree.clone(), status);
        let ev = TransitionEvent {
            worktree: worktree.clone(),
            verdict: verdict_color,
            red_diagnostics,
            verdict_failure_reason: failure_reason,
            base_sha,
            published_at,
        };
        poisoned(&self.subs).retain(|s| s.send(ev.clone()).is_ok());
        self.mark_worktree_published(&worktree);
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

    /// #240/2b — consume-semantic reader for the SwitchOverlay arm.
    /// Returns the pushed overlay for `wt_key` (matching
    /// `wt.to_string_lossy()` from servedrv) AND removes it from the
    /// store. If no push is pending, returns `None` and the SwitchOverlay
    /// arm falls through to the FS-read path. The pop-on-consume
    /// semantic (spike open-question #3 default) means each push
    /// services exactly one SwitchOverlay cycle; FS path resumes if no
    /// fresh push arrives.
    pub fn take_overlay_for(&self, wt_key: &str) -> Option<PushedOverlay> {
        poisoned(&self.pushed).remove(wt_key)
    }

    /// #240/2b — non-consuming peek. Used by the serve loop's first-push
    /// cluster-hash derivation (`cluster_hash_from_pushed`) which needs
    /// to read the pushed workspace-config files WITHOUT consuming the
    /// overlay (the consume happens later in the SwitchOverlay arm via
    /// `take_overlay_for`). Returns a clone; the store is unchanged.
    pub fn peek_overlay_for(&self, wt_key: &str) -> Option<PushedOverlay> {
        poisoned(&self.pushed).get(wt_key).cloned()
    }

    /// Server-side analysis root for a pending pushed overlay, if the client
    /// supplied one. The serve loop uses this before consuming the overlay so
    /// first-push cluster spawn uses the daemon's mirror path, not the
    /// client's pod-local worktree key.
    pub fn analysis_root_for(&self, wt_key: &str) -> Option<PathBuf> {
        poisoned(&self.pushed)
            .get(wt_key)
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

    /// #A2/#A7 — stamp the attribution for the push just consumed by the
    /// SwitchOverlay arm. Same lifecycle as `record_project_check_context`:
    /// recorded at consume, popped at publish; a replacing push for the
    /// same key overwrites (the verdict that eventually publishes belongs
    /// to the LAST consumed push, so its attribution must win too).
    pub(crate) fn record_push_attribution(&self, worktree: &str, pushed: &PushedOverlay) {
        poisoned(&self.push_attribution).insert(
            worktree.to_string(),
            PushAttribution {
                base_sha: pushed.base_sha.clone(),
                push_received_unix: pushed.last_push_unix,
                consumed_unix: crate::statusfile::now_unix(),
                consumed_at: Instant::now(),
            },
        );
    }

    pub(crate) fn take_push_attribution(&self, worktree: &str) -> Option<PushAttribution> {
        poisoned(&self.push_attribution).remove(worktree)
    }

    /// #A4.3 — claim the hard-witness slot for `wt_key`. Returns the new
    /// generation; a previously claimed (still-running) witness for the
    /// same key is implicitly invalidated (its `finish_hard_witness` will
    /// return `false`). Generations come from a global never-recycled
    /// sequence, so an ABA match is structurally impossible.
    pub(crate) fn begin_hard_witness(&self, wt_key: &str) -> u64 {
        let generation = HARD_WITNESS_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        poisoned(&self.hard_witness_generation).insert(wt_key.to_string(), generation);
        generation
    }

    /// `true` iff `generation` is still the latest claim for `wt_key` —
    /// the caller may publish. Consumes the claim on success so a
    /// duplicate finish (watchdog already published, worker completes
    /// later) reports `false` and stays silent.
    pub(crate) fn finish_hard_witness(&self, wt_key: &str, generation: u64) -> bool {
        let mut map = poisoned(&self.hard_witness_generation);
        if map.get(wt_key) == Some(&generation) {
            map.remove(wt_key);
            true
        } else {
            false
        }
    }

    pub(crate) fn with_project_check_overlay<T>(
        &self,
        context: &ProjectCheckRunContext,
        f: impl FnOnce(&Path) -> T,
    ) -> Result<T, String> {
        if !context.materialize_overlay {
            return Ok(f(&context.root));
        }

        if let Some(state_dir) = self.project_check_state_dir.as_deref() {
            return self.with_project_check_scratch_overlay(context, state_dir, f);
        }

        self.with_project_check_locked_overlay(context, f)
    }

    fn with_project_check_locked_overlay<T>(
        &self,
        context: &ProjectCheckRunContext,
        f: impl FnOnce(&Path) -> T,
    ) -> Result<T, String> {
        let _guard = poisoned(&self.sync_lock);
        reset_analysis_root(&context.root, &context.base_ref)?;
        materialize_overlay_files(&context.root, &context.overlay_files)?;
        let result = f(&context.root);
        if let Err(e) = reset_analysis_root(&context.root, &context.base_ref) {
            eprintln!(
                "[cargoless:obs] project-check-overlay-cleanup root={} error={}",
                context.root.display(),
                e
            );
        }
        Ok(result)
    }

    fn with_project_check_scratch_overlay<T>(
        &self,
        context: &ProjectCheckRunContext,
        state_dir: &Path,
        f: impl FnOnce(&Path) -> T,
    ) -> Result<T, String> {
        let seq = PROJECT_CHECK_SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let scratch_root = state_dir
            .join("project-check-runs")
            .join(format!("run-{}-{seq}", std::process::id()));

        {
            let _guard = poisoned(&self.sync_lock);
            sync_analysis_root(&context.root, &context.base_ref)?;
            prepare_project_check_scratch(&context.root, &scratch_root, &context.base_ref)?;
        }

        let result = match materialize_overlay_files_from_root(
            &context.root,
            &scratch_root,
            &context.overlay_files,
        ) {
            Ok(()) => Ok(f(&scratch_root)),
            Err(e) => Err(e),
        };

        let cleanup = {
            let _guard = poisoned(&self.sync_lock);
            cleanup_project_check_scratch(&context.root, &scratch_root)
        };
        if let Err(e) = cleanup {
            eprintln!(
                "[cargoless:obs] project-check-scratch-cleanup root={} scratch={} error={}",
                context.root.display(),
                scratch_root.display(),
                e
            );
        }

        result
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
    ) -> Option<crate::servedrv::ProjectCheckSummary> {
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
            changed_files: None, // changed_files live on the member, not the options
            gate: false,
        };
        request.members = vec![member];
        request.corun = true;
        request.coalesce_key = Some(project_check_plan_coalesce_token(&context.root, &request)?);

        // coalesce_key was set above, so this is always Some; `?` keeps the
        // defensive None-path (empty-after-trim) without a clippy::question_mark lint.
        let key = batch_coalesce_key(&request)?;

        let report = self
            .batch_coalescer
            .submit(key, &request, |combined| self.run_batch_check_now(combined));

        // Find this WT's slice in the returned report.
        let member_result = report.members.into_iter().find(|m| m.worktree == wt_key);

        Some(match member_result {
            None => {
                // Coalescer returned a report without our member — treat as
                // indeterminate (should not happen in practice).
                crate::servedrv::ProjectCheckSummary::Indeterminate {
                    reason: "project_check_batch_missing_member",
                    detail: format!("coalesced report did not include member {wt_key}"),
                }
            }
            Some(m) => match m.verdict {
                cargoless_core::batch::BatchVerdict::Green => {
                    // CombinedGreen and SoloGreen both map to Green.
                    // Empty is indistinguishable at this layer (documented above).
                    crate::servedrv::ProjectCheckSummary::Green
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
                        crate::servedrv::ProjectCheckSummary::Indeterminate {
                            reason: "project_check_red_without_diagnostics",
                            detail: format!(
                                "batch member {wt_key} red but 0 error-severity diagnostics"
                            ),
                        }
                    } else {
                        crate::servedrv::ProjectCheckSummary::Red { error_count }
                    }
                }
                cargoless_core::batch::BatchVerdict::Indeterminate => {
                    let detail = m
                        .diagnostics
                        .first()
                        .map(|d| d.message.clone())
                        .unwrap_or_else(|| "batch indeterminate (no detail)".to_string());
                    crate::servedrv::ProjectCheckSummary::Indeterminate {
                        reason: "project_check_batch_indeterminate",
                        detail,
                    }
                }
            },
        })
    }

    pub fn quiescing(&self) -> bool {
        poisoned(&self.drain).quiescing
    }

    pub fn drain_complete(&self) -> bool {
        let drain = poisoned(&self.drain);
        let batch_counts = self.batch_coalescer.counts();
        drain.quiescing
            && drain.active_worktrees.is_empty()
            && poisoned(&self.pushed).is_empty()
            && batch_counts.waiters == 0
            && batch_counts.inflight_runs == 0
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
        DaemonActivity {
            quiescing: drain.quiescing,
            active_worktrees: drain.active_worktrees.len() as u32,
            pending_pushes: poisoned(&self.pushed).len() as u32,
            pending_batch_waiters: batch_counts.waiters,
            pending_batch_members: batch_counts.members,
            inflight_batch_runs: batch_counts.inflight_runs,
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
}

impl VerdictService for ServeVerdictState {
    fn get_status(&self, worktree: &str) -> Option<WorktreeStatus> {
        let g = poisoned(&self.statuses);
        let mut s = g.get(worktree).cloned()?;
        // Age is derived at read time from the publish timestamp — the
        // stored `heartbeat_age_secs` is a placeholder; the honest age is
        // "seconds since this verdict was attributed".
        let now = crate::statusfile::now_unix();
        s.heartbeat_age_secs = now.saturating_sub(s.published_at);
        Some(s)
    }

    fn get_verdict(&self, worktree: &str) -> Option<String> {
        poisoned(&self.statuses)
            .get(worktree)
            .map(|s| s.verdict.clone())
    }

    fn get_diagnostics(&self, _worktree: &str) -> Vec<Diagnostic> {
        // Honest Inc-0 boundary: the serve loop does not yet thread
        // `diagnostics_store` retention (a later increment). Empty here is
        // the *correct* answer for the state the loop computes — never a
        // fabricated diagnostic. (`get_diagnostics` empty ⇒ "no detail",
        // the same contract `transport` documents for green/unknown.)
        Vec::new()
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
    /// 1. Record the `(base_ref, files)` pair in the per-WT pushed
    ///    store. A subsequent push for the same WT REPLACES (latest
    ///    wins; per-WT serialization is the natural BTreeMap semantic).
    /// 2. Signal the serve loop via the attached `push_signal` channel
    ///    (best-effort: a wedged send leaves the overlay stored, only
    ///    the wakeup is lost). The loop synthesizes a
    ///    `DriverEvent::RoutedBatch` for this WT, which feeds the
    ///    proven core EXACTLY as if it came from the FS watcher path
    ///    — same event shape, no new emission seam.
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
        if self.quiescing() {
            return rejected_push(worktree, "daemon is quiescing");
        }
        let mut mapped_files = files.to_vec();
        let mut analysis_root = None;
        let mut base_sha = None;
        let mut changed_files = None;
        let mut gate = false;
        if let Some(options) = options {
            changed_files = options.changed_files.clone();
            gate = options.gate;
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

            if options.repo_relative {
                let Some(root) = analysis_root.as_ref() else {
                    return rejected_push(worktree, "repo-relative push missing analysis_root");
                };
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
            if files.is_empty() {
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
                let base = base_ref.trim();
                if !base.is_empty() {
                    let _guard = poisoned(&self.sync_lock);
                    if let Err(e) = ensure_analysis_root(root, base, base_sha.as_deref()) {
                        return rejected_push(worktree, &e);
                    }
                }
            }
        }

        if !self.mark_push_active(worktree) {
            return rejected_push(worktree, "daemon is quiescing");
        }

        let applied_files = files.len() as u32;
        let pushed = PushedOverlay {
            base_ref: base_ref.to_string(),
            files: mapped_files,
            analysis_root,
            base_sha,
            last_push_unix: crate::statusfile::now_unix(),
            changed_files,
            check_profile: check_profile.cloned(),
            gate,
        };
        poisoned(&self.pushed).insert(worktree.to_string(), pushed);
        // Wake the serve loop (best-effort — see attach_push_signal doc).
        if let Some(tx) = poisoned(&self.push_signal).as_ref() {
            let _ = tx.send(worktree.to_string());
        }
        PushOverlayAck {
            worktree: worktree.to_string(),
            accepted: true,
            applied_files,
        }
    }

    fn batch_check(&self, request: &BatchCheckRequest) -> BatchReport {
        if let Some(key) = batch_coalesce_key(request) {
            self.batch_coalescer
                .submit(key, request, |combined| self.run_batch_check_now(combined))
        } else {
            self.run_batch_check_now(request)
        }
    }

    fn daemon_activity(&self) -> DaemonActivity {
        self.activity_snapshot()
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

impl ServeBatchChecker<'_> {
    fn run_overlay(
        &self,
        overlay_files: Vec<(String, String)>,
        changed_files: Vec<String>,
    ) -> Result<ProjectCheckReport, String> {
        let changed_files = (!changed_files.is_empty()).then_some(changed_files);
        let context = ProjectCheckRunContext {
            root: self.root.clone(),
            changed_files: changed_files.clone(),
            base_ref: self.base_ref.clone(),
            overlay_files,
            materialize_overlay: true,
            gate: false,
        };
        self.api
            .with_project_check_overlay(&context, |root| {
                cargoless_core::project_checks::run_dev_with_changes(root, changed_files.as_deref())
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
    run_git(root, &["fetch", "--prune", "origin", fetch_ref])?;
    reset_analysis_root(root, base_ref)?;
    Ok(())
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
    if scratch_root.exists() {
        std::fs::remove_dir_all(scratch_root).map_err(|e| {
            format!(
                "could not remove stale project-check scratch `{}`: {e}",
                scratch_root.display()
            )
        })?;
    }
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

fn cleanup_project_check_scratch(root: &Path, scratch_root: &Path) -> Result<(), String> {
    if !scratch_root.exists() {
        return Ok(());
    }
    let scratch = scratch_root.to_string_lossy().into_owned();
    match run_git(root, &["worktree", "remove", "--force", &scratch]) {
        Ok(()) => Ok(()),
        Err(git_err) => {
            let fallback = std::fs::remove_dir_all(scratch_root).map_err(|e| {
                format!(
                    "{git_err}; fallback remove_dir_all `{}` failed: {e}",
                    scratch_root.display()
                )
            });
            fallback.and(Err(git_err))
        }
    }
}

fn materialize_overlay_files(root: &Path, files: &[(String, String)]) -> Result<(), String> {
    materialize_overlay_files_from_root(root, root, files)
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

fn run_git(root: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| {
            format!(
                "git {:?} in `{}` failed to start: {e}",
                args,
                root.display()
            )
        })?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "git {:?} in `{}` exited {:?}: {}",
        args,
        root.display(),
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

fn run_git_success(root: &Path, args: &[&str]) -> Result<bool, String> {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .map_err(|e| {
            format!(
                "git {:?} in `{}` failed to start: {e}",
                args,
                root.display()
            )
        })?;
    Ok(status.success())
}

fn git_stdout(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| {
            format!(
                "git {:?} in `{}` failed to start: {e}",
                args,
                root.display()
            )
        })?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    Err(format!(
        "git {:?} in `{}` exited {:?}: {}",
        args,
        root.display(),
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use cargoless_core::batch::{BatchMember, BatchProvenance, BatchReport, BatchVerdict};
    use cargoless_core::transport::http::{HttpClient, HttpServer};
    use cargoless_core::transport::{
        AllowAll, BatchCheckRequest, CargoSubcommand, PushOverlayOptions, TransportClient,
        VerdictService,
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
            changed_files: None,
            gate: false,
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
            config: BatchCoalesceConfig {
                debounce: Duration::from_millis(50),
                max_wait: Duration::from_millis(300),
                max_members: 40,
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
        }
    }

    fn coalescer_request(batch_id: &str, member: &str) -> BatchCheckRequest {
        let mut request = BatchCheckRequest::new(batch_id, "origin/main");
        request.options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some("/workspace/repo".into()),
            base_sha: None,
            changed_files: None,
            gate: false,
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

        let mut runs = poisoned(&runs).clone();
        assert_eq!(runs.len(), 1);
        runs[0].sort();
        assert_eq!(runs[0], vec!["member-a", "member-b"]);
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
            config: BatchCoalesceConfig {
                debounce: Duration::from_millis(50),
                max_wait: Duration::from_millis(300),
                max_members: 2,
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
        let mut run_sizes = poisoned(&runs).clone();
        run_sizes.sort();
        assert_eq!(run_sizes, vec![1, 2]);
        assert_eq!(reports.len(), 3);
        let mut executed_ids: Vec<_> = reports
            .iter()
            .filter_map(|report| report.executed_batch_id.clone())
            .collect();
        executed_ids.sort();
        executed_ids.dedup();
        assert_eq!(
            executed_ids.len(),
            2,
            "two physical flushes should have distinct executed_batch_id values"
        );
        assert!(
            reports
                .iter()
                .all(|report| report.verdict == BatchVerdict::Green && report.members.len() == 1)
        );
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

        // Exactly one physical run was attempted (one leader), and it panicked.
        assert_eq!(*poisoned(&panics), 1, "only the leader should invoke run");
        assert_eq!(reports.len(), 3);
        assert!(
            reports
                .iter()
                .all(|report| report.verdict == BatchVerdict::Indeterminate),
            "every coalesced submitter must see indeterminate after a run panic, not hang"
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
                changed_files: None,
                gate: false,
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

        // KEY ASSERTION: only ONE physical run was made despite 3 concurrent submitters.
        let final_run_count = run_count.load(Ordering::SeqCst);
        assert_eq!(
            final_run_count, 1,
            "3 concurrent pushers sharing the same (base_ref, analysis_root) must \
             coalesce into exactly ONE physical run — got {final_run_count}"
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
        };

        let summary = api.coalesced_project_check(wt, &context);

        assert!(
            summary.is_some(),
            "non-empty base_ref + materialize_overlay=true should engage the coalesced path"
        );
        assert_eq!(
            summary.unwrap(),
            ProjectCheckSummary::Green,
            "clean overlay over a clean project should yield ProjectCheckSummary::Green"
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

        assert_eq!(report.verdict, BatchVerdict::Green);
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
    fn get_diagnostics_is_honest_empty_inc0_boundary() {
        // **INFRA-36 update (was: Inc-0 boundary):** the diagnostics
        // *list* (per-diag detail) is still not retained at the
        // serveapi layer — that's a later increment as the original
        // contract said. But the *count* (`red_diagnostics` on
        // `WorktreeStatus`) is now honest: when the publish path
        // supplies a real count, `get_status` returns it.
        //
        // This test now pins two things:
        //   1. The per-diagnostic detail list is still empty here
        //      (the increment-0 boundary the original test guarded).
        //   2. But `red_diagnostics` is the real count, NOT 0 — the
        //      INFRA-36 invariant that closes the "verdict=red, 0
        //      diagnostics" liar state.
        let api = ServeVerdictState::new();
        api.publish(
            Path::new("/r/wt"),
            crate::statusfile::VerdictPayload::red(5),
        );
        assert!(
            api.get_diagnostics("/r/wt").is_empty(),
            "per-diagnostic detail list is not retained at this layer \
             (the original Inc-0 boundary still holds)"
        );
        let status = api.get_status("/r/wt").expect("status present");
        assert_eq!(status.verdict, "red");
        assert_eq!(
            status.red_diagnostics, 5,
            "INFRA-36: red_diagnostics MUST reflect the count supplied \
             at publish time — not the historical hardcoded 0 that \
             produced the verdict=red,0-diagnostics liar state"
        );
        assert!(
            status.verdict_failure_reason.is_none(),
            "a real Red verdict carries no failure reason — the reason \
             is the populated-vs-empty diagnostic count itself"
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
            changed_files: Some(vec!["src/lib.rs".into()]),
            gate: false,
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
            changed_files: Some(vec!["src/lib.rs".into()]),
            gate: false,
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
        };

        let seen = api
            .with_project_check_overlay(&context, |root| {
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
        };

        let seen = api
            .with_project_check_overlay(&context, |root| {
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
    fn push_overlay_with_options_rejects_escaping_repo_relative_paths() {
        let api = ServeVerdictState::new();
        let files = vec![("../outside.rs".to_string(), "bad".to_string())];
        let options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some("/workspace/tf-multiverse".into()),
            base_sha: None,
            changed_files: None,
            gate: false,
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
    fn multiple_pushes_same_wt_latest_wins() {
        // Per-WT serialization: a fresh push for the same WT REPLACES the
        // prior stored overlay (BTreeMap::insert semantic). N rapid
        // pushes coalesce — the SwitchOverlay arm services exactly the
        // latest state. The push_signal still fires per push (each wakeup
        // services whatever the CURRENT stored state is — natural coalesce
        // on the consume side via pop-on-consume).
        let api = ServeVerdictState::new();
        let (tx, rx) = channel::<String>();
        api.attach_push_signal(tx);

        let v1 = vec![("/wt/x".to_string(), "version-1".to_string())];
        let v2 = vec![("/wt/x".to_string(), "version-2".to_string())];
        let v3 = vec![("/wt/x".to_string(), "version-3".to_string())];
        api.push_overlay("/wt", "main", &v1);
        api.push_overlay("/wt", "main", &v2);
        api.push_overlay("/wt", "main", &v3);

        // Store has the LATEST content (v3), not v1/v2.
        let consumed = api.take_overlay_for("/wt").expect("stored");
        assert_eq!(
            consumed.files, v3,
            "latest push wins (BTreeMap::insert replace semantic)"
        );
        // Subsequent take: None (consumed once; v1/v2/v3 collapsed).
        assert!(api.take_overlay_for("/wt").is_none());

        // All 3 signals fired (the wakeup channel is per-push, not
        // coalesced). The serve loop's drain sees 3 wakeups, but each
        // take_overlay_for after the first returns None — natural
        // idempotency.
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(
            count, 3,
            "3 signals (one per push) — consume-side coalesces"
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
        );
        let status = api
            .get_status("/workspace/tf-multiverse")
            .expect("status present");
        assert_eq!(status.base_sha.as_deref(), Some("abc123def"));
        let ev = rx.try_recv().expect("transition event");
        assert_eq!(ev.base_sha.as_deref(), Some("abc123def"));

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
            last_push_unix: crate::statusfile::now_unix(),
            changed_files: None,
            check_profile: None,
            gate: false,
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
            changed_files: Some(vec!["src/lib.rs".into(), "src/main.rs".into()]),
            gate: false,
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
            changed_files: None,
            gate: false,
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
            changed_files: Some(vec!["src/removed.rs".into()]),
            gate: false,
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
}
