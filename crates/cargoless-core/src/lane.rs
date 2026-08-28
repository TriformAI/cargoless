//! The build lane — build the candidate merge *before* it lands.
//!
//! ## The problem
//!
//! The usual arrangement builds after the merge: land the PR, then let CI build
//! the trunk. That has two failure modes. The trunk can go red, blocking
//! everyone until someone reverts; and the tree that actually ships was never
//! compiled *as a tree* — only each PR's head was, separately, against a base
//! that has since moved.
//!
//! This lane inverts it. A member is enqueued, the lane merges it into a
//! candidate on top of the current base, builds *that*, and only a green build
//! is allowed to land and publish. What ships is exactly what compiled.
//!
//! ## Shape
//!
//! Pure `Event → (State, Vec<Action>)`, modelled on [`crate::appstate`]. No
//! I/O, no threads, no clock beyond what the caller passes in. The driver
//! ([`crate::lanedrv`]) owns every side effect, so the whole policy — queueing,
//! attribution, ejection, re-admission — is exhaustively unit-testable without
//! launching a compiler.
//!
//! Deliberately *not* built on [`crate::serveapi`]'s `BatchCoalescer`: that is
//! a Condvar/thread design suited to sub-second RA checks. A lane build is
//! minutes-to-an-hour, and its policy is where all the risk lives, so it wants
//! to be a value you can drive with a test vector.
//!
//! ## Never cancel, only collapse
//!
//! A running build always finishes and publishes its verdict. Arrivals queue;
//! they never preempt. This is the repo axiom, and it is not merely stylistic —
//! a cancelled build is permanently not-green, so cancelling to start a "better"
//! build strands whatever was already in flight. [`LaneState`] carries a
//! generation counter so a completion that arrives after the lane moved on is
//! cheap to ignore rather than something the driver must prevent.
//!
//! ## Attribution: never guess, and never stay silent
//!
//! On a red build the lane maps each erroring file to the member(s) that touched
//! it:
//!
//! | errors owned by | outcome |
//! |---|---|
//! | exactly one member | eject that member; everyone else rebuilds at once |
//! | several members | eject **all** of them, each told the failure is shared |
//! | nobody | eject **all** members, each told it could not be attributed |
//!
//! The third case is the honest one. An error in a file nobody in the train
//! touched is either an interaction between members or a pre-existing base red;
//! picking a culprit would eject someone innocent and teach people to distrust
//! the gate. Everyone is told, with the diagnostics, that they must check.
//!
//! There is no separate confirmation build. The *next* build — survivors plus
//! whatever arrived meanwhile — is the verification, and it was going to run
//! anyway.
//!
//! ## Ejection identity is line-INSENSITIVE, and that is load-bearing
//!
//! An ejected member is re-admitted when the *error* changes, not when the file
//! merely moves. The key is the fingerprint multiset from
//! [`crate::attribution`] (`source|code|path|normalized_message`, count-matched),
//! which omits line and column on purpose. [`crate::attribution`]'s own docs
//! give the reason: inserting three lines shifts the line number of every error
//! below the edit, so a line-keyed identity reports all of them as new and
//! blames the wrong person. Do not "fix" this by adding line back.
//!
//! ## Re-admission, and why the two cases differ
//!
//! * **Attributed** — we know which files carried the errors, so a new head is
//!   re-admitted only if it touches at least one of them. A README edit does not
//!   buy an hour of build time.
//! * **Unattributed** — we do *not* know where the fault lies, so **any** new
//!   head is re-admitted. Gating on files we could not identify would strand
//!   someone whose fix legitimately lives elsewhere. When in doubt, let them
//!   retry and say plainly that the diagnosis was inconclusive.
//!
//! Both carry a TTL so nothing is ejected forever.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use cargoless_proto::{Diagnostic, Severity};
use serde_json::{Value, json};

use crate::attribution::{FingerprintCounts, fingerprint_counts};

/// A candidate waiting to be built into the trunk.
///
/// `id` is caller-chosen and opaque — a PR number, a branch name, a ticket.
/// It is the ONLY identity the lane uses, which is what lets this work for a
/// forge the lane knows nothing about.
///
/// Note this is deliberately *not* [`crate::batch::BatchMember`], whose identity
/// is a worktree path. Every production submission there uses the same path, so
/// it cannot tell two members apart — the exact reason its cross-run ejection
/// map could not be reused here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneMember {
    pub id: String,
    /// Immutable content identity of the submission (a commit sha). A member
    /// whose head moves is a *different* candidate, which is what makes
    /// staleness detection automatic rather than something to track.
    pub head: String,
    /// Repo-relative paths this member changes. The attribution input.
    pub changed_files: Vec<String>,
}

impl LaneMember {
    pub fn new(id: impl Into<String>, head: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            head: head.into(),
            changed_files: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_changed_files<I, S>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.changed_files = files.into_iter().map(Into::into).collect();
        self
    }

    pub(crate) fn recovery_value(&self) -> Value {
        json!({
            "id": self.id,
            "head": self.head,
            "changed_files": self.changed_files,
        })
    }

    fn from_recovery_value(value: &Value) -> Result<Self, String> {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "lane recovery member is missing a non-empty id".to_string())?;
        let head = value
            .get("head")
            .and_then(Value::as_str)
            .filter(|head| !head.is_empty())
            .ok_or_else(|| format!("lane recovery member `{id}` is missing a non-empty head"))?;
        let changed_files = value
            .get("changed_files")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("lane recovery member `{id}` is missing changed_files"))?
            .iter()
            .map(|file| {
                file.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("lane recovery member `{id}` has a non-string path"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            id: id.to_string(),
            head: head.to_string(),
            changed_files,
        })
    }

    /// Does this member touch `path`?
    ///
    /// Compares suffix-wise so an absolute diagnostic path
    /// (`/scratch/abc/portal/src/x.rs`) matches a repo-relative changed file
    /// (`portal/src/x.rs`). The candidate is built in a scratch worktree whose
    /// prefix the member never sees, so exact equality would attribute nothing
    /// and every red would read as unattributable.
    #[must_use]
    pub fn touches(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy().replace('\\', "/");
        self.changed_files.iter().any(|f| {
            let f = f.trim_start_matches("./");
            !f.is_empty() && (path_str == f || path_str.ends_with(&format!("/{f}")))
        })
    }
}

/// Why a member is not currently eligible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EjectReason {
    /// We know which files carried the errors.
    Attributed {
        files: Vec<PathBuf>,
        fingerprints: FingerprintCounts,
        /// Other members implicated by the same build. Empty ⇒ sole owner.
        shared_with: Vec<String>,
    },
    /// The errors landed in files no member touched — an interaction between
    /// members, or a red that was already in the base. Nobody is blamed.
    Unattributed {
        fingerprints: FingerprintCounts,
        /// Everyone ejected alongside this member (all of them, by definition).
        shared_with: Vec<String>,
    },
    /// The build never produced a verdict: it could not be set up at all, and
    /// it kept failing the same way. NOT a statement about anyone's code.
    ///
    /// Distinct from `Unattributed` on purpose. `Unattributed` means "your tree
    /// is red and we could not tell whose change did it" — the code is
    /// implicated even though the owner is unknown. This means "we never
    /// compiled anything", which is the lane admitting a fault in itself. An
    /// author who reads the wrong one of those two goes hunting a bug that does
    /// not exist.
    Infrastructure {
        /// The failure as the build reported it, verbatim.
        reason: String,
        /// How many consecutive attempts failed before the lane gave up.
        attempts: u32,
        /// Everyone ejected alongside this member (all of them, by definition).
        shared_with: Vec<String>,
    },
}

impl EjectReason {
    /// Operator-facing sentence. This *is* the product surface — it is what an
    /// author sees when their PR stops moving, so it says what happened, who
    /// else is affected, and what to do.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            EjectReason::Attributed {
                files, shared_with, ..
            } => {
                let where_ = files
                    .iter()
                    .map(|f| f.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", ");
                if shared_with.is_empty() {
                    format!(
                        "build failed in files this change touches ({where_}). \
                         Re-enqueued automatically once you push a change to one of them."
                    )
                } else {
                    format!(
                        "build failed in files this change touches ({where_}). \
                         This failure is SHARED with {}: every one of these changes \
                         touches the failing code and each must be checked — do not \
                         assume another change is the cause. Re-enqueued once you push \
                         a change to one of those files.",
                        shared_with.join(", ")
                    )
                }
            }
            EjectReason::Unattributed { shared_with, .. } => format!(
                "build failed, but the errors are in files NO queued change touches, \
                 so the cause could not be attributed. All {} queued changes have been \
                 held and every one must be checked properly — this may be an \
                 interaction between them or a pre-existing failure in the base. \
                 Any new push re-enqueues you.",
                shared_with.len().max(1)
            ),
            // Says plainly that the code was never judged. An author reading
            // this should NOT go looking at their change — there is no verdict
            // about it, and the thing to fix is the lane's own environment.
            EjectReason::Infrastructure {
                reason, attempts, ..
            } => format!(
                "the lane could not build a candidate at all, and the same failure \
                 repeated across {attempts} attempts: {reason}. This is NOT a verdict \
                 about your change — nothing was compiled, so nothing was judged. \
                 Held so the queue can move; an operator has to clear the underlying \
                 fault. Any new push re-enqueues you, as does the ejection lapsing."
            ),
        }
    }

    /// The error identities that caused this ejection.
    ///
    /// Public because `GET /lane` reports them: an author whose change is held
    /// needs to see *which* errors are holding it, and a fingerprint set that
    /// changed between two builds is the evidence that a fix took effect.
    #[must_use]
    pub fn fingerprints(&self) -> &FingerprintCounts {
        // An infrastructure ejection has no fingerprints BY CONSTRUCTION: no
        // build ran, so no diagnostic exists to fingerprint. Returning an empty
        // set is the honest answer, and it keeps the "fingerprints changed ⇒ a
        // fix took effect" reading intact — an empty set never looks like
        // progress on an error, because it never claimed one.
        static NONE: std::sync::OnceLock<FingerprintCounts> = std::sync::OnceLock::new();
        match self {
            EjectReason::Attributed { fingerprints, .. }
            | EjectReason::Unattributed { fingerprints, .. } => fingerprints,
            EjectReason::Infrastructure { .. } => NONE.get_or_init(FingerprintCounts::new),
        }
    }
}

/// A live ejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ejection {
    pub reason: EjectReason,
    /// The head that was ejected. A member re-enqueued at this same head is
    /// still ejected — nothing about the candidate changed.
    pub head: String,
    /// The member's changed set at the moment it was ejected.
    ///
    /// Kept so a forced re-admission can restore the member intact. Without it
    /// the member would come back with an empty changed set and be silently
    /// **unattributable** for every build it subsequently rides — a red it
    /// caused would land as "could not attribute" and hold the whole queue.
    pub changed_files: Vec<String>,
    /// Tick at which the ejection lapses regardless. The backstop against a
    /// permanently-stuck member if attribution is ever wrong.
    pub expires_at_tick: u64,
}

/// What a finished build reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneBuildOutcome {
    /// Compiled and produced a servable artifact.
    Green { artifact: Option<String> },
    /// Compiled red. Diagnostics drive attribution.
    Red { diagnostics: Vec<Diagnostic> },
    /// Compiled red, but the reporting leg could not provide source paths that
    /// are evidence of ownership.
    ///
    /// The diagnostics remain attached for fingerprinting and operator
    /// context, but every member is held as unattributed. This is distinct from
    /// [`Self::Infra`]: the code really was built and rejected; only the owner
    /// is unknown.
    UnattributedRed { diagnostics: Vec<Diagnostic> },
    /// A named member could not be merged onto the candidate. Nothing was
    /// compiled, but unlike [`Self::Infra`] we know exactly whose fault it is —
    /// git told us, by name, before any inference.
    ///
    /// This is its own outcome rather than a `Red` because attribution for a
    /// red runs *backwards*, from erroring files to the members that touched
    /// them, and needs `changed_files` to do it. A caller that submits members
    /// without a diff would get `Unattributed`, which readmits on any new head
    /// — and a member that cannot merge would then be re-included in every
    /// subsequent candidate, forever. That livelock was observed in production
    /// on 2026-08-02: generations 2 through 5 each died on the same
    /// unmergeable member while the rest of the queue waited behind it.
    ///
    /// `files` is best-effort (git's unmerged paths); empty means the conflict
    /// was real but the paths could not be read, which must not stop the
    /// ejection.
    Conflict {
        /// The member that failed to merge.
        id: String,
        /// Conflicting paths, when git could report them.
        files: Vec<PathBuf>,
        /// git's own explanation, for the trail.
        reason: String,
    },
    /// Neither — the build could not be trusted to have run (runner died,
    /// timeout, cancelled). NOT a code red: members stay queued and ride the
    /// next build. Treating a transient as a red is how a fleet learns to
    /// bypass its own gate.
    Infra { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneEvent {
    /// Submit or re-submit a member.
    Enqueue(LaneMember),
    /// The member's head moved. Carries the new changed set so an ejection can
    /// be re-evaluated against what actually changed.
    HeadMoved {
        id: String,
        head: String,
        changed_files: Vec<String>,
    },
    /// Withdraw a member entirely (PR closed, superseded).
    Withdraw { id: String },
    /// Lift an ejection by hand, without consulting the attribution.
    ///
    /// The escape hatch behind `POST /lane/readmit`, for a fix the attribution
    /// cannot see — a dependency bump, a toolchain change, a red that was
    /// never the member's fault. It is deliberately NOT modelled as a
    /// [`LaneEvent::HeadMoved`] to the same head: that path asks
    /// "does this change touch a failing file?" and answers no, which is the
    /// correct answer to a different question.
    ///
    /// Using it does not make the previous failure untrue, and the next build
    /// is still the verification — it just declines to require evidence the
    /// operator already has.
    ForceReadmit { id: String },
    /// A build finished. `generation` is checked against the lane's current
    /// build; a stale completion is discarded.
    BuildFinished {
        generation: u64,
        outcome: LaneBuildOutcome,
    },
    /// The lander refused a GREEN build's members.
    ///
    /// Distinct from re-enqueueing them directly, which is what this used to
    /// do. A land failure is infrastructure by construction — the build was
    /// green, so nobody's code is at fault — and it therefore needs the same
    /// pacing every other infra failure gets: a backoff before the retry, and
    /// a cap so a permanently-refusing lander eventually stops.
    ///
    /// Without that, a failed land re-enqueues, `maybe_start_build` starts the
    /// next candidate at once, the lander refuses again, and the lane rebuilds
    /// the same tree as fast as it can loop — each turn a real multi-minute
    /// build occupying the slot. The realistic trigger is the base moving and
    /// the forge's compare-and-swap rejecting the push, which on a busy trunk
    /// persists for many minutes.
    ///
    /// Carries the member ids rather than reading the queue, because this is
    /// sent BEFORE the re-enqueues — the backoff has to exist before the first
    /// `Enqueue` reaches `maybe_start_build`, or with a zero capture window
    /// that enqueue starts the next build and the timer arrives too late.
    LandFailed {
        reason: String,
        members: Vec<String>,
    },
    /// Time passes. Drives TTL expiry and lets an idle lane with a ready queue
    /// start a build.
    Tick { now: u64 },
}

/// Side effects for the driver. The lane itself performs none of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneAction {
    /// Build `members` merged onto `base`. Tag the result with `generation`.
    StartBuild {
        generation: u64,
        members: Vec<LaneMember>,
    },
    /// Hold this member out of the lane and tell them why.
    Eject { id: String, reason: EjectReason },
    /// The member is eligible again.
    Readmit { id: String, why: String },
    /// The build was green: merge these members and publish the artifact,
    /// together. The lane emits this exactly once per green build.
    LandAndPublish {
        members: Vec<LaneMember>,
        artifact: Option<String>,
    },
    /// Progress for a member, for the forge/UI.
    Report { id: String, state: String },
}

/// Where the lane is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanePhase {
    Idle,
    Building,
}

// `PartialEq, Eq` so `config::LaneSettings` (which wraps this) can derive
// them for its own tests. Not `Copy`: a future non-integer field would
// silently make that a trap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneConfig {
    /// Cap on members per build. A bigger train amortises the build cost but
    /// widens the blast radius of one red and raises the chance some head goes
    /// stale mid-build.
    pub max_members: usize,
    /// Ticks an ejection survives before lapsing regardless of pushes.
    pub eject_ttl_ticks: u64,
    /// How long an idle lane waits for company before building.
    ///
    /// Without this the lane builds whoever arrives first and everyone else
    /// waits a full cycle — and a cycle here is a real release build, 25–80
    /// minutes. Two changes landing seconds apart would take two hours instead
    /// of one, and the whole point of a lane is that a build carries as many
    /// changes as it safely can.
    ///
    /// The window only ever delays the FIRST member of an idle lane. Once a
    /// build is running, later arrivals queue against it and are picked up the
    /// moment it finishes, so the window costs nothing on a busy lane.
    ///
    /// It is filled early when the queue reaches [`Self::max_members`] — there
    /// is nothing to wait for once the build is full.
    ///
    /// `0` disables it (build immediately), which is what unit tests and a
    /// single-developer project want.
    pub capture_window_ticks: u64,
    /// Ticks to wait after an infrastructure failure before rebuilding the
    /// same members.
    ///
    /// An infra failure is not the members' fault, so they are requeued — but
    /// requeueing with no delay is a hot loop: the retry hits the same broken
    /// condition immediately and the lane spins as fast as the failure returns.
    /// Observed on the first real deployment at roughly one full candidate
    /// attempt every 2.5 seconds, indefinitely, because the PR head commits
    /// were missing from the daemon's object store and every materialize
    /// failed the same way.
    pub infra_backoff_ticks: u64,
    /// Consecutive infra failures on the same members before the lane stops
    /// retrying and ejects them.
    ///
    /// Retrying forever assumes every infra failure is transient. Some are
    /// permanent from the lane's side — an unreachable commit, a scratch
    /// directory it cannot write — and for those, an infinite retry is strictly
    /// worse than an ejection: it burns the machine, it never reports, and it
    /// blocks every later submission behind members that cannot build. Ejecting
    /// carries the reason to the author and lets the queue move; the ejection
    /// TTL brings them back once the condition clears.
    pub infra_max_attempts: u32,
}

impl Default for LaneConfig {
    fn default() -> Self {
        Self {
            max_members: 10,
            eject_ttl_ticks: 3_600,
            // 60s at one tick per second: long enough to gather a burst of
            // agent pushes, negligible against a build measured in tens of
            // minutes.
            capture_window_ticks: 60,
            // 120s between infra retries.
            //
            // This was 30s, calibrated against "a brief forge outage". The
            // failure that actually dominates in production is slower by an
            // order of magnitude: the PREVIEW DAEMON RESTARTING. A preview pod
            // that rolls takes minutes to come back — it answers /readyz with
            // `warming` while it seeds, and every lane leg that targets it
            // fails with connection-refused until it finishes.
            //
            // Measured 2026-08-03: a preview roll at 18:29 burned lane
            // generations 46,47,48,49,50 in roughly six minutes — a candidate
            // attempt every ~30s against a daemon that could not possibly
            // answer yet — and then ejected pr-10388, pr-10394 and pr-6956 as
            // `infrastructure`. Three innocent PRs lost their place in the
            // queue because an unrelated pod restarted.
            infra_backoff_ticks: 120,
            // Ten attempts x 120s = 20 minutes of patience before the lane
            // concludes the failure is not transient.
            //
            // This bound must exceed the slowest infra failure that DOES clear
            // on its own, and that is the preview restart above (minutes, not
            // seconds). The previous 5 x 30s = 2.5 minutes was shorter than the
            // most common recoverable outage, so the cap fired on exactly the
            // case it was meant to ride out. The permanent failures this cap
            // exists for — an unreachable commit, an unwritable scratch dir —
            // are still caught, just 20 minutes later, and that lateness costs
            // far less than ejecting members who did nothing wrong.
            infra_max_attempts: 10,
        }
    }
}

/// The lane.
#[derive(Debug, Clone)]
pub struct LaneState {
    cfg: LaneConfig,
    phase: LanePhase,
    /// FIFO of eligible members. Order is arrival order — first in, first built.
    queue: Vec<LaneMember>,
    /// Members in the build currently running.
    in_flight: Vec<LaneMember>,
    /// Bumped on every StartBuild. A completion for an older generation is
    /// discarded, which is what makes "never cancel" cheap: we let the old
    /// build run and simply ignore what it says.
    generation: u64,
    ejected: BTreeMap<String, Ejection>,
    now: u64,
    /// When the currently-idle lane first had something to build. `None` while
    /// the queue is empty or a build is running. Drives the capture window.
    queued_since: Option<u64>,
    /// Consecutive infrastructure failures, and the tick before which the lane
    /// must not retry. Reset by any outcome that is not `Infra`, so a lane that
    /// gets one bad build and then succeeds carries nothing forward.
    infra_failures: u32,
    infra_retry_after: Option<u64>,
    /// Root the diagnostics' paths are fingerprinted against.
    root: PathBuf,
}

impl LaneState {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_config(root, LaneConfig::default())
    }

    #[must_use]
    pub fn with_config(root: impl Into<PathBuf>, cfg: LaneConfig) -> Self {
        Self {
            cfg,
            phase: LanePhase::Idle,
            queue: Vec::new(),
            in_flight: Vec::new(),
            generation: 0,
            ejected: BTreeMap::new(),
            now: 0,
            queued_since: None,
            infra_failures: 0,
            infra_retry_after: None,
            root: root.into(),
        }
    }

    /// Restore the one piece of lane state whose loss spends real build time:
    /// the exact generation that was blocking when the process stopped.
    ///
    /// Queue membership is replayed by the forge adapter, but a running remote
    /// build has no caller left to deliver its result after a pod replacement.
    /// Recovering the immutable roster and generation lets the driver issue the
    /// same `StartBuild` again; an idempotent dispatcher can then reattach to
    /// the exact external run instead of selecting a different batch and
    /// orphaning the first compile.
    pub fn load_active_build(
        root: impl Into<PathBuf>,
        path: &Path,
    ) -> Result<Option<Self>, String> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let value: Value =
            serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
        if value.get("schema").and_then(Value::as_u64) != Some(1) {
            return Err(format!(
                "{} has an unsupported lane recovery schema",
                path.display()
            ));
        }
        let generation = value
            .get("generation")
            .and_then(Value::as_u64)
            .filter(|generation| *generation > 0)
            .ok_or_else(|| format!("{} has no active generation", path.display()))?;
        let members = value
            .get("members")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{} has no active member roster", path.display()))?
            .iter()
            .map(LaneMember::from_recovery_value)
            .collect::<Result<Vec<_>, _>>()?;
        if members.is_empty() {
            return Err(format!(
                "{} has an empty active member roster",
                path.display()
            ));
        }
        let mut lane = Self::new(root);
        lane.phase = LanePhase::Building;
        lane.in_flight = members;
        lane.generation = generation;
        lane.now = value.get("now").and_then(Value::as_u64).unwrap_or(0);
        Ok(Some(lane))
    }

    /// Durable representation of the currently blocking generation.
    /// `None` means there is no remote build to reattach after a restart.
    #[must_use]
    pub(crate) fn active_build_recovery_value(&self) -> Option<Value> {
        if self.phase != LanePhase::Building || self.in_flight.is_empty() {
            return None;
        }
        Some(json!({
            "schema": 1,
            "generation": self.generation,
            "now": self.now,
            "members": self
                .in_flight
                .iter()
                .map(LaneMember::recovery_value)
                .collect::<Vec<_>>(),
        }))
    }

    /// Re-emit the exact action that was blocking when a durable lane snapshot
    /// was written. This does not bump the generation or alter the roster.
    #[must_use]
    pub(crate) fn resume_active_build_action(&self) -> Option<LaneAction> {
        self.active_build_recovery_value()
            .map(|_| LaneAction::StartBuild {
                generation: self.generation,
                members: self.in_flight.clone(),
            })
    }

    #[must_use]
    pub fn phase(&self) -> LanePhase {
        self.phase
    }

    #[must_use]
    pub fn queue_depth(&self) -> usize {
        self.queue.len()
    }

    #[must_use]
    pub fn in_flight(&self) -> &[LaneMember] {
        &self.in_flight
    }

    /// The queued members, in build order.
    ///
    /// `queue_depth` answers "how many are waiting"; this answers "which ones",
    /// which is what an author polling `GET /lane` actually asked. It is also
    /// what lets a host de-duplicate its own accepted-but-not-yet-stepped
    /// members against the lane's queue instead of double-counting them.
    #[must_use]
    pub fn queued(&self) -> &[LaneMember] {
        &self.queue
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The lane's clock, in whatever unit the caller ticks it with.
    #[must_use]
    pub fn now(&self) -> u64 {
        self.now
    }

    /// Move the clock forward WITHOUT stepping the machine.
    ///
    /// A `Tick` does two things: it advances `now`, and it runs the expiry +
    /// `maybe_start_build` pipeline. The driver needs only the first, in one
    /// specific place — immediately after a blocking action, before the outcome
    /// event is applied.
    ///
    /// The driver is inside `execute` for the whole of a build or a land, so
    /// the host's ticks sit unread in a channel and `now` is frozen at whatever
    /// it was when the action started. Every deadline the outcome computes is
    /// then measured from the past: an infra failure taking longer than
    /// `infra_backoff_ticks` installs a backoff that has already expired, and
    /// the lane retries immediately. That is the observed 30-second generation
    /// loop against an unreachable preview daemon.
    ///
    /// It must NOT be a `Tick`, and that distinction is the whole reason this
    /// exists. `Tick` ends in `maybe_start_build`, and after a failed LAND the
    /// phase is already `Idle` with the members re-enqueued — so a Tick there
    /// would start the next build BEFORE `LandFailed` installs the backoff,
    /// reintroducing the exact hot loop that event was added to prevent.
    ///
    /// Clamped forward like `Tick`, so a clock that steps backwards cannot
    /// resurrect a lapsed ejection.
    pub fn advance_clock(&mut self, now: u64) {
        self.now = self.now.max(now);
    }

    #[must_use]
    pub fn ejection(&self, id: &str) -> Option<&Ejection> {
        self.ejected.get(id)
    }

    // No `#[must_use]`: `Iterator` already carries it, and doubling it is a
    // clippy error under `-D warnings`.
    pub fn ejections(&self) -> impl Iterator<Item = (&String, &Ejection)> {
        self.ejected.iter()
    }

    /// Advance the lane.
    pub fn step(&mut self, event: LaneEvent) -> Vec<LaneAction> {
        let mut actions = Vec::new();
        match event {
            LaneEvent::Enqueue(member) => self.on_enqueue(member, &mut actions),
            LaneEvent::HeadMoved {
                id,
                head,
                changed_files,
            } => self.on_head_moved(&id, head, changed_files, &mut actions),
            LaneEvent::Withdraw { id } => {
                self.queue.retain(|m| m.id != id);
                self.ejected.remove(&id);
                // ALSO drop it from the running build's roster.
                //
                // Withdrawing only from `queue` looks sufficient and is not: on
                // completion `on_build_finished` takes `in_flight` and, on any
                // non-green outcome, requeues the members it finds there. A
                // member withdrawn mid-build would therefore come BACK when the
                // build it was withdrawn from ends — the one moment the operator
                // is least likely to be watching, and the exact situation the
                // verb exists for (a ~45-minute build you have decided to stop
                // feeding).
                //
                // This does NOT cancel the running build. The candidate tree is
                // already materialised and the compile is already paying for
                // itself; killing it would waste the work and, worse, the
                // remaining members would lose a verdict they were about to get.
                // The build finishes and is simply attributed to whoever is
                // left. Removing the last member is fine: the roster empties,
                // the outcome applies to nobody, and the lane goes idle.
                self.in_flight.retain(|m| m.id != id);
            }
            LaneEvent::ForceReadmit { id } => self.on_force_readmit(&id, &mut actions),
            // Members are already back in the queue — the driver re-enqueues
            // them before sending this, because losing green work to a push
            // race is the worst outcome available. What is missing is the
            // pacing, and that is all this arm adds.
            LaneEvent::LandFailed { reason, members } => {
                self.infra_failures = self.infra_failures.saturating_add(1);
                self.infra_retry_after =
                    Some(self.now.saturating_add(self.cfg.infra_backoff_ticks));
                let attempt = self.infra_failures;
                for id in members {
                    actions.push(LaneAction::Report {
                        id,
                        state: format!(
                            "green, but the land failed ({reason}) — requeued, retry {attempt} \
                             of {} in {} ticks",
                            self.cfg.infra_max_attempts, self.cfg.infra_backoff_ticks
                        ),
                    });
                }
            }
            LaneEvent::BuildFinished {
                generation,
                outcome,
            } => self.on_build_finished(generation, outcome, &mut actions),
            LaneEvent::Tick { now } => {
                // `now` is ABSOLUTE, and the clock only moves forward. A caller
                // passing a smaller value (a restarted counter, a test reusing
                // a literal) must not rewind time and resurrect an ejection
                // that has already lapsed — the lane would then hold a member
                // for longer than its TTL with no way to tell.
                self.now = self.now.max(now);
                self.expire_ejections(&mut actions);
            }
        }
        // One place decides whether a build starts, so every path that could
        // make the lane runnable gets the same treatment.
        self.maybe_start_build(&mut actions);
        actions
    }

    fn on_enqueue(&mut self, member: LaneMember, actions: &mut Vec<LaneAction>) {
        if let Some(ej) = self.ejected.get(&member.id) {
            // Re-enqueueing at the SAME head changes nothing about the
            // candidate, so it must not buy a build slot.
            if ej.head == member.head {
                actions.push(LaneAction::Report {
                    id: member.id,
                    state: format!("ejected: {}", ej.reason.describe()),
                });
                return;
            }
            // A different head is a different candidate; fall through to the
            // same re-admission test a HeadMoved would get.
            let readmit = self.readmission_decision(&member.id, &member.changed_files);
            match readmit {
                Some(why) => {
                    self.ejected.remove(&member.id);
                    actions.push(LaneAction::Readmit {
                        id: member.id.clone(),
                        why,
                    });
                }
                None => {
                    let ej = &self.ejected[&member.id];
                    actions.push(LaneAction::Report {
                        id: member.id,
                        state: format!("still ejected: {}", ej.reason.describe()),
                    });
                    return;
                }
            }
        }
        self.admit_new(member, actions);
    }

    /// Queue a member that arrived from outside, opening the capture window if
    /// the lane was idle and empty.
    fn admit_new(&mut self, member: LaneMember, actions: &mut Vec<LaneAction>) {
        if self.phase == LanePhase::Idle && self.queue.is_empty() && self.queued_since.is_none() {
            self.queued_since = Some(self.now);
        }
        self.admit(member, actions);
    }

    fn admit(&mut self, member: LaneMember, actions: &mut Vec<LaneAction>) {
        // Re-submitting an already-queued id replaces it in place, preserving
        // arrival order: a member who pushes twice while waiting should not
        // jump ahead of, or fall behind, someone who arrived after them.
        if let Some(slot) = self.queue.iter_mut().find(|m| m.id == member.id) {
            *slot = member.clone();
        } else {
            self.queue.push(member.clone());
        }
        actions.push(LaneAction::Report {
            id: member.id,
            state: "queued".to_string(),
        });
    }

    /// Lift an ejection because an operator said so.
    ///
    /// Re-queues at the SAME head the ejection recorded — a forced re-admission
    /// asserts the tree is now buildable, not that the submission changed. The
    /// member's original changed-file set is preserved so a subsequent red can
    /// still be attributed to it; dropping it would quietly make this member
    /// unblameable for every future build it rides.
    fn on_force_readmit(&mut self, id: &str, actions: &mut Vec<LaneAction>) {
        let Some(ejection) = self.ejected.remove(id) else {
            actions.push(LaneAction::Report {
                id: id.to_string(),
                state: "not ejected; nothing to re-admit".to_string(),
            });
            return;
        };
        actions.push(LaneAction::Readmit {
            id: id.to_string(),
            why: "re-admitted by hand — the previous failure still stands, but \
                  the operator has evidence the attribution cannot see"
                .to_string(),
        });
        let member = LaneMember {
            id: id.to_string(),
            head: ejection.head,
            changed_files: ejection.changed_files,
        };
        self.admit(member, actions);
    }

    fn on_head_moved(
        &mut self,
        id: &str,
        head: String,
        changed_files: Vec<String>,
        actions: &mut Vec<LaneAction>,
    ) {
        if self.ejected.contains_key(id) {
            if let Some(why) = self.readmission_decision(id, &changed_files) {
                self.ejected.remove(id);
                actions.push(LaneAction::Readmit {
                    id: id.to_string(),
                    why,
                });
                let member = LaneMember {
                    id: id.to_string(),
                    head,
                    changed_files,
                };
                self.admit(member, actions);
            } else {
                let ej = &self.ejected[id];
                actions.push(LaneAction::Report {
                    id: id.to_string(),
                    state: format!("still ejected: {}", ej.reason.describe()),
                });
            }
            return;
        }
        // Not ejected: a queued member's head moving just updates the candidate.
        if let Some(slot) = self.queue.iter_mut().find(|m| m.id == id) {
            slot.head = head;
            slot.changed_files = changed_files;
            actions.push(LaneAction::Report {
                id: id.to_string(),
                state: "queued (head updated)".to_string(),
            });
        }
        // A head move for a member in the CURRENT build is deliberately not
        // acted on: the build keeps running (never cancel) and its verdict is
        // published against the head it actually compiled. The stale head is
        // caught at landing time, where the forge's own compare-and-swap
        // rejects it — a check this pure layer cannot make.
    }

    /// `Some(why)` if the ejection should lift.
    fn readmission_decision(&self, id: &str, changed_files: &[String]) -> Option<String> {
        let ej = self.ejected.get(id)?;
        match &ej.reason {
            EjectReason::Attributed { files, .. } => {
                let touches_failing = files.iter().any(|f| {
                    let m = LaneMember {
                        id: String::new(),
                        head: String::new(),
                        changed_files: changed_files.to_vec(),
                    };
                    m.touches(f)
                });
                touches_failing
                    .then(|| "new head touches a file that carried the failure".to_string())
            }
            // We could not identify the fault, so we must not gate on files we
            // are not sure about — that would strand someone whose fix lives
            // elsewhere. Any new head earns another attempt.
            EjectReason::Unattributed { .. } => Some(
                "new head, and the previous failure could not be attributed to specific files"
                    .to_string(),
            ),
            // Nothing was ever compiled, so there are no failing files to gate
            // on — and the member's code was never implicated in the first
            // place. Any new head earns another attempt, for the same reason
            // `Unattributed` does but more strongly: here we are certain the
            // fault was ours.
            //
            // Note this readmits even for an UNCHANGED tree once the operator
            // clears the underlying fault, because the TTL lapse in
            // `expire_ejections` also applies. That is deliberate: a member
            // held by a daemon-side problem must not need its author to push
            // something to escape.
            EjectReason::Infrastructure { .. } => Some(
                "new head, and the previous hold was an infrastructure failure — \
                 the change was never judged"
                    .to_string(),
            ),
        }
    }

    fn expire_ejections(&mut self, actions: &mut Vec<LaneAction>) {
        let lapsed: Vec<String> = self
            .ejected
            .iter()
            .filter(|(_, ej)| self.now >= ej.expires_at_tick)
            .map(|(id, _)| id.clone())
            .collect();
        for id in lapsed {
            // Put the member BACK IN THE QUEUE, not merely out of `ejected`.
            //
            // This used to remove the ejection and announce `Readmit` without
            // ever calling `admit`, so a member whose TTL lapsed was silently
            // dropped: gone from `ejected`, absent from `queue`, and reported
            // as re-admitted. Observed in production 2026-08-02 — three members
            // ejected `infrastructure` by a preview outage hit their TTL and
            // vanished, leaving `queue_depth: 0` with nothing building and no
            // way to tell from `GET /lane` that anything had been lost.
            //
            // That is the exact failure the TTL exists to PREVENT. It is the
            // backstop for a member the attribution stranded; a backstop that
            // discards the thing it was protecting is worse than none, because
            // the log says "re-admitted" either way.
            //
            // `Ejection` retains `head` and `changed_files` precisely so the
            // member can be rebuilt intact — the same reconstruction
            // `on_force_readmit` does.
            let Some(ejection) = self.ejected.remove(&id) else {
                continue;
            };
            actions.push(LaneAction::Readmit {
                id: id.clone(),
                why: "ejection expired (TTL backstop)".to_string(),
            });
            self.admit(
                LaneMember {
                    id,
                    head: ejection.head,
                    changed_files: ejection.changed_files,
                },
                actions,
            );
        }
    }

    fn on_build_finished(
        &mut self,
        generation: u64,
        outcome: LaneBuildOutcome,
        actions: &mut Vec<LaneAction>,
    ) {
        // A completion from a build we already moved past. Ignoring it is what
        // makes "let the old build finish" free.
        if generation != self.generation || self.phase != LanePhase::Building {
            return;
        }
        let members = std::mem::take(&mut self.in_flight);
        self.phase = LanePhase::Idle;

        // Any outcome that is not `Infra` means the infrastructure worked, so
        // the failure streak is over. Resetting here rather than in each branch
        // keeps a future outcome variant from silently inheriting a stale
        // count and ejecting on its first failure.
        if !matches!(outcome, LaneBuildOutcome::Infra { .. }) {
            self.infra_failures = 0;
            self.infra_retry_after = None;
        }

        match outcome {
            LaneBuildOutcome::Green { artifact } => {
                for m in &members {
                    actions.push(LaneAction::Report {
                        id: m.id.clone(),
                        state: "green — landing".to_string(),
                    });
                }
                actions.push(LaneAction::LandAndPublish { members, artifact });
            }
            LaneBuildOutcome::Infra { reason } => {
                self.infra_failures = self.infra_failures.saturating_add(1);

                // GIVE UP eventually. Requeueing unconditionally assumes every
                // infra failure is transient, and some are permanent from the
                // lane's side: a member whose head commit the daemon cannot
                // reach never becomes mergeable by waiting. Retrying such a
                // member forever is worse than ejecting it — it burns the
                // machine, reports nothing an author can act on, and blocks
                // every later submission behind a build that cannot succeed.
                //
                // Observed on the first real deployment: every candidate failed
                // to materialize because the PR heads were absent from the
                // object store, and the lane retried about once every 2.5
                // seconds, indefinitely, while `GET /lane` showed a steady
                // `phase=building` — indistinguishable from a long compile.
                if self.infra_failures >= self.cfg.infra_max_attempts {
                    let attempts = self.infra_failures;
                    self.infra_failures = 0;
                    self.infra_retry_after = None;
                    let all: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
                    for m in members {
                        let ejection = Ejection {
                            reason: EjectReason::Infrastructure {
                                reason: reason.clone(),
                                attempts,
                                shared_with: all.iter().filter(|o| *o != &m.id).cloned().collect(),
                            },
                            head: m.head.clone(),
                            changed_files: m.changed_files.clone(),
                            expires_at_tick: self.now.saturating_add(self.cfg.eject_ttl_ticks),
                        };
                        actions.push(LaneAction::Report {
                            id: m.id.clone(),
                            state: ejection.reason.describe(),
                        });
                        actions.push(LaneAction::Eject {
                            id: m.id.clone(),
                            reason: ejection.reason.clone(),
                        });
                        self.ejected.insert(m.id.clone(), ejection);
                    }
                    return;
                }

                // Requeue at the FRONT, preserving order: these members were
                // already waiting and an infra failure is not their fault.
                //
                // The backoff is what stops this being a hot loop. Without it
                // the retry runs the instant `maybe_start_build` is reached,
                // hits the same broken condition, and spins as fast as the
                // failure returns.
                self.infra_retry_after =
                    Some(self.now.saturating_add(self.cfg.infra_backoff_ticks));
                for (i, m) in members.into_iter().enumerate() {
                    actions.push(LaneAction::Report {
                        id: m.id.clone(),
                        state: format!(
                            "infrastructure failure ({reason}) — still queued, retry {} of {} \
                             in {} ticks",
                            self.infra_failures,
                            self.cfg.infra_max_attempts,
                            self.cfg.infra_backoff_ticks
                        ),
                    });
                    self.queue.insert(i, m);
                }
            }
            LaneBuildOutcome::Red { diagnostics } => {
                self.attribute_red(&members, &diagnostics, actions);
            }
            LaneBuildOutcome::UnattributedRed { diagnostics } => {
                self.eject_unattributed(&members, &diagnostics, actions);
            }
            LaneBuildOutcome::Conflict { id, files, reason } => {
                // The infrastructure worked — git ran and answered. Reset the
                // consecutive-failure count so a conflict cannot creep the lane
                // toward its infra give-up threshold.
                self.infra_failures = 0;

                let expires = self.now.saturating_add(self.cfg.eject_ttl_ticks);
                for m in members {
                    if m.id == id {
                        // Attributed, with the conflicting paths as the files
                        // that carried the failure. `readmission_decision`'s
                        // existing gate then does the right thing without
                        // knowing a conflict happened: a new head touching one
                        // of those paths is a plausible fix and readmits;
                        // anything else stays out. That is what stops the
                        // member re-entering every candidate forever.
                        //
                        // When git could not report the paths, fall back to
                        // `Unattributed`: it still ejects, and it readmits on
                        // any new head — which is the honest answer when we
                        // cannot say which files to watch. It does NOT reopen
                        // the livelock, because the member is out until its
                        // head actually moves.
                        let eject_reason = if files.is_empty() {
                            EjectReason::Unattributed {
                                fingerprints: FingerprintCounts::default(),
                                shared_with: Vec::new(),
                            }
                        } else {
                            EjectReason::Attributed {
                                files: files.clone(),
                                fingerprints: FingerprintCounts::default(),
                                // Nobody else is implicated: a conflict is
                                // between this member and the base, and blaming
                                // a co-rider for it would be the false
                                // accusation the lane exists to avoid.
                                shared_with: Vec::new(),
                            }
                        };
                        actions.push(LaneAction::Eject {
                            id: m.id.clone(),
                            reason: eject_reason.clone(),
                        });
                        self.ejected.insert(
                            m.id.clone(),
                            Ejection {
                                reason: eject_reason,
                                head: m.head.clone(),
                                changed_files: m.changed_files.clone(),
                                expires_at_tick: expires,
                            },
                        );
                    } else {
                        // Everyone else was never judged — they rode a
                        // candidate that was never built. Requeue at the front
                        // so they rebuild immediately, WITHOUT the infra
                        // backoff: the next attempt has the conflicting member
                        // removed, so it is a genuinely different candidate
                        // rather than a retry of the same broken one.
                        actions.push(LaneAction::Report {
                            id: m.id.clone(),
                            state: format!(
                                "requeued — `{id}` could not be merged onto the base \
                                 ({reason}) and was ejected; the next candidate is built \
                                 without it"
                            ),
                        });
                        self.queue.push(m);
                    }
                }
            }
        }
    }

    /// The attribution ladder. See the module docs.
    fn attribute_red(
        &mut self,
        members: &[LaneMember],
        diagnostics: &[Diagnostic],
        actions: &mut Vec<LaneAction>,
    ) {
        let errors: Vec<&Diagnostic> = diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();

        // Which members own an erroring file, and which files those are.
        let mut owners: BTreeSet<String> = BTreeSet::new();
        let mut owned_files: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
        for d in &errors {
            for m in members {
                if m.touches(&d.file_path) {
                    owners.insert(m.id.clone());
                    owned_files
                        .entry(m.id.clone())
                        .or_default()
                        .insert(d.file_path.clone());
                }
            }
        }

        if owners.is_empty() {
            // Nobody touched a failing file. Could be an interaction between
            // members, could be a base red. Either way we do not know, so we
            // hold everyone and say so — never invent a culprit.
            self.eject_unattributed(members, diagnostics, actions);
            return;
        }

        let owned: Vec<Diagnostic> = errors.iter().map(|d| (*d).clone()).collect();
        let fingerprints = fingerprint_counts(&self.root, &owned);
        let expires = self.now.saturating_add(self.cfg.eject_ttl_ticks);

        // One or more owners: eject exactly those, and requeue everyone else so
        // the next build starts immediately with the survivors plus anything
        // that arrived meanwhile.
        for m in members {
            if owners.contains(&m.id) {
                let mut shared_with: Vec<String> =
                    owners.iter().filter(|o| **o != m.id).cloned().collect();
                shared_with.sort();
                let files: Vec<PathBuf> = owned_files
                    .get(&m.id)
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default();
                let reason = EjectReason::Attributed {
                    files,
                    fingerprints: fingerprints.clone(),
                    shared_with,
                };
                self.ejected.insert(
                    m.id.clone(),
                    Ejection {
                        reason: reason.clone(),
                        head: m.head.clone(),
                        changed_files: m.changed_files.clone(),
                        expires_at_tick: expires,
                    },
                );
                actions.push(LaneAction::Eject {
                    id: m.id.clone(),
                    reason,
                });
            } else {
                self.queue.push(m.clone());
                actions.push(LaneAction::Report {
                    id: m.id.clone(),
                    state: "not implicated — requeued for the next build".to_string(),
                });
            }
        }
    }

    /// Hold every member for a real red whose owner cannot be proven.
    ///
    /// This is shared by ordinary changed-file attribution when no diagnostic
    /// path has an owner and by legs that explicitly declare their paths to be
    /// synthetic display anchors. Keeping one implementation prevents those
    /// two honest-uncertainty paths from drifting in TTL, fingerprints, or
    /// readmission behavior.
    fn eject_unattributed(
        &mut self,
        members: &[LaneMember],
        diagnostics: &[Diagnostic],
        actions: &mut Vec<LaneAction>,
    ) {
        let errors: Vec<Diagnostic> = diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .cloned()
            .collect();
        let fingerprints = fingerprint_counts(&self.root, &errors);
        let expires = self.now.saturating_add(self.cfg.eject_ttl_ticks);
        let all: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
        for m in members {
            let reason = EjectReason::Unattributed {
                fingerprints: fingerprints.clone(),
                shared_with: all.clone(),
            };
            self.ejected.insert(
                m.id.clone(),
                Ejection {
                    reason: reason.clone(),
                    head: m.head.clone(),
                    changed_files: m.changed_files.clone(),
                    expires_at_tick: expires,
                },
            );
            actions.push(LaneAction::Eject {
                id: m.id.clone(),
                reason,
            });
        }
    }

    fn maybe_start_build(&mut self, actions: &mut Vec<LaneAction>) {
        if self.phase == LanePhase::Building || self.queue.is_empty() {
            // An empty idle queue has nothing to capture; reset so the next
            // arrival opens a fresh window instead of inheriting an old one.
            if self.queue.is_empty() {
                self.queued_since = None;
            }
            return;
        }
        // Serve the infra backoff. This is checked BEFORE the capture window
        // because the two answer different questions — the window asks "has this
        // gathered enough company yet", the backoff asks "is it worth trying at
        // all yet" — and only the backoff can be pending on members that have
        // already been through a build.
        if let Some(retry_at) = self.infra_retry_after {
            if self.now < retry_at {
                return;
            }
            self.infra_retry_after = None;
        }
        // The window gathers FRESH arrivals. It must never delay work that was
        // already waiting: after a red, the survivors requeue and have to
        // rebuild at once — making them sit through another window would add a
        // full cycle to a queue that is already behind. Same for a member whose
        // ejection just lifted, and for anything left over when a build ends.
        //
        // `defer_until` is therefore set ONLY by a genuinely new enqueue, and
        // cleared by everything else. `None` here means "these were waiting
        // already; build now".
        let full = self.queue.len() >= self.cfg.max_members;
        if !full {
            if let Some(opened) = self.queued_since {
                let elapsed = self.now.saturating_sub(opened);
                if elapsed < self.cfg.capture_window_ticks {
                    return;
                }
            }
        }
        self.queued_since = None;
        let take = self.queue.len().min(self.cfg.max_members);
        let members: Vec<LaneMember> = self.queue.drain(..take).collect();
        self.generation += 1;
        self.phase = LanePhase::Building;
        self.in_flight = members.clone();
        for m in &members {
            actions.push(LaneAction::Report {
                id: m.id.clone(),
                state: format!("building (generation {})", self.generation),
            });
        }
        actions.push(LaneAction::StartBuild {
            generation: self.generation,
            members,
        });
    }
}
