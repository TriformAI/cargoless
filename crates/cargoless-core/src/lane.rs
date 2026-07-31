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
use std::path::{Path, PathBuf};

use cargoless_proto::{Diagnostic, Severity};

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

#[derive(Debug, Clone)]
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
            // 30s between infra retries. Long enough that a persistent failure
            // costs ~2 attempts a minute rather than ~24, short enough that a
            // genuinely transient one (a brief forge outage) recovers without
            // anyone noticing.
            infra_backoff_ticks: 30,
            // Five attempts ≈ 2.5 minutes of retrying before the lane concludes
            // the failure is not going to clear on its own and says so.
            infra_max_attempts: 5,
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

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
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
            }
            LaneEvent::ForceReadmit { id } => self.on_force_readmit(&id, &mut actions),
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
            self.ejected.remove(&id);
            actions.push(LaneAction::Readmit {
                id,
                why: "ejection expired (TTL backstop)".to_string(),
            });
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

        let owned: Vec<Diagnostic> = errors.iter().map(|d| (*d).clone()).collect();
        let fingerprints = fingerprint_counts(&self.root, &owned);
        let expires = self.now.saturating_add(self.cfg.eject_ttl_ticks);

        if owners.is_empty() {
            // Nobody touched a failing file. Could be an interaction between
            // members, could be a base red. Either way we do not know, so we
            // hold everyone and say so — never invent a culprit.
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
            return;
        }

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
