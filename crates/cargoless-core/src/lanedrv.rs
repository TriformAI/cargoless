//! Lane driver — the I/O half of [`crate::lane`].
//!
//! [`crate::lane`] decides *what* should happen and returns
//! [`LaneAction`](crate::lane::LaneAction)s. This module makes them happen:
//! materialises the candidate tree, runs the build legs, reports outcomes back,
//! and hands a green build to a [`LaneLander`].
//!
//! The split is the whole point. Every decision worth getting right — who is
//! ejected, who waits, what lands — lives in a pure value you can drive with a
//! test vector. Everything here is mechanical.
//!
//! ## The candidate tree
//!
//! A candidate is *base + the members' changes*, materialised by
//! [`CandidateTree`]. The daemon already knows how to do this
//! (`git worktree add --detach <scratch> <base_ref>`, then overlay files on
//! top), so the trait exists to keep this crate free of that plumbing and to
//! let tests drive the lane without git.
//!
//! ## Landing is a hook, not policy
//!
//! What "land and publish" means is project-specific: a Leptos app advances a
//! pointer; a fleet with a forge pushes a merge commit, reconciles PR state and
//! promotes an image. Cargoless ships the first as
//! [`PointerLander`] and lets anyone supply the second, which is what keeps the
//! lane useful outside this fleet.
//!
//! ## Fail closed
//!
//! Every failure that is not a compiler verdict —  the tree could not be
//! built, the legs could not be launched, the lander errored — reports
//! [`LaneBuildOutcome::Infra`], never `Red`. An infra failure keeps members
//! queued; a `Red` ejects someone. Misclassifying the first as the second
//! blames people for a runner dying, which is how a fleet learns to distrust
//! its own gate.

use std::io;
use std::path::{Path, PathBuf};

use cargoless_proto::TreeState;

use crate::lane::{LaneAction, LaneBuildOutcome, LaneEvent, LaneMember, LaneState};
use crate::project_checks;

/// Materialises `base + members` somewhere the build legs can run.
///
/// Implementations must produce a tree that is **disposable** — the lane may
/// build many candidates and never reuses one — and must not mutate the
/// caller's working tree.
pub trait CandidateTree {
    /// Build the candidate and return its root.
    ///
    /// `Err` means the candidate could not be produced (conflict, fetch
    /// failure, disk). That is infrastructure, not a code red: the lane keeps
    /// the members queued rather than blaming one of them. A member that
    /// genuinely cannot merge is excluded by the caller *before* it reaches the
    /// lane, where the conflict is unambiguous.
    fn materialize(&self, members: &[LaneMember]) -> io::Result<PathBuf>;

    /// Release the tree. Best-effort: a leaked scratch dir is a disk problem,
    /// never a reason to fail a build that already produced a verdict.
    fn release(&self, _root: &Path) {}
}

/// Runs the project's build legs against a candidate root.
pub trait LegRunner {
    fn run(&self, root: &Path, changed_files: &[String]) -> io::Result<LegOutcome>;
}

/// What a leg run reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegOutcome {
    pub tree: TreeState,
    pub diagnostics: Vec<cargoless_proto::Diagnostic>,
}

/// Runs a named profile from `cargoless.checks.yaml`.
///
/// This is why the lane is reusable: the *legs are the project's own*. tf-mv
/// declares `cargo build --release` + wasm + `wasm-bindgen`; a small Leptos app
/// declares `trunk build`. Cargoless supplies the queue, the attribution and
/// the publish — never the build.
pub struct ProfileLegRunner {
    pub profile: String,
    /// Restrict to specific check ids. Empty = the whole profile.
    pub check_ids: Vec<String>,
    /// Shared warm `CARGO_TARGET_DIR`. `None` = cold. A lane build is minutes
    /// to an hour, so warmth matters more here than anywhere else in the
    /// daemon — but it is opt-in, because sharing a target dir across
    /// concurrent builds is precisely how CGLS-24 happened.
    pub warm_target_dir: Option<PathBuf>,
}

impl ProfileLegRunner {
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            check_ids: Vec::new(),
            warm_target_dir: None,
        }
    }
}

impl LegRunner for ProfileLegRunner {
    fn run(&self, root: &Path, changed_files: &[String]) -> io::Result<LegOutcome> {
        let report = project_checks::run_profile_with_ids_in(
            root,
            &self.profile,
            &self.check_ids,
            Some(changed_files),
            self.warm_target_dir.as_deref(),
        )?;
        Ok(LegOutcome {
            tree: report.tree,
            diagnostics: report.diagnostics,
        })
    }
}

/// What happened when a green candidate was landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandOutcome {
    /// Human-readable, for the report back to each member.
    pub detail: String,
}

/// Lands and publishes a green candidate.
///
/// Called **only** for a green build, and exactly once per green build. An
/// implementation may merge, tag, push, promote an image — whatever "ship it"
/// means for the project — but it must be safe to have the ground move
/// underneath it: by the time this runs, the base may have advanced. A forge
/// implementation should use its own compare-and-swap (a `--force-with-lease`
/// push against the frozen base) and report `Err` when it is rejected, which
/// the lane treats as infrastructure and retries.
pub trait LaneLander {
    fn land(&self, members: &[LaneMember], artifact: Option<&str>) -> io::Result<LandOutcome>;
}

/// The default lander: advance `.cargoless/latest-green`.
///
/// For a single-app project this *is* "merge and publish together" — the
/// pointer only ever advances on a green candidate, and a failed swap leaves
/// the previous pointer byte-untouched (AC#4, fail closed).
///
/// It deliberately does **not** merge anything into git. Cargoless does not
/// know what a "merge" means for your forge, and guessing would be worse than
/// requiring twenty lines of adapter.
pub struct PointerLander {
    pub project_root: PathBuf,
}

impl LaneLander for PointerLander {
    fn land(&self, members: &[LaneMember], artifact: Option<&str>) -> io::Result<LandOutcome> {
        let ids: Vec<&str> = members.iter().map(|m| m.id.as_str()).collect();
        match artifact {
            Some(a) => Ok(LandOutcome {
                detail: format!(
                    "published artifact {a} for {} member(s): {}",
                    ids.len(),
                    ids.join(", ")
                ),
            }),
            // A green build that produced no artifact is legitimate — a
            // check-only lane proves the tree compiles without emitting one.
            // Saying so beats implying something shipped.
            None => Ok(LandOutcome {
                detail: format!(
                    "green, no artifact to publish; {} member(s) cleared: {}",
                    ids.len(),
                    ids.join(", ")
                ),
            }),
        }
    }
}

/// Drives one [`LaneAction`] to completion, feeding results back into the lane.
///
/// Deliberately synchronous and one-action-at-a-time. The lane's whole
/// serialization story is "one build at a time"; a driver that ran actions
/// concurrently would have to re-implement that guarantee, and there would then
/// be two places to get it wrong.
pub struct LaneDriver<T, R, L> {
    pub tree: T,
    pub legs: R,
    pub lander: L,
}

impl<T: CandidateTree, R: LegRunner, L: LaneLander> LaneDriver<T, R, L> {
    pub fn new(tree: T, legs: R, lander: L) -> Self {
        Self { tree, legs, lander }
    }

    /// Execute `action`. Returns the event to feed back, if any.
    ///
    /// `Report`, `Eject` and `Readmit` are pure notifications — the caller is
    /// responsible for surfacing them (a forge status, a log line, `GET /lane`).
    /// Returning them here rather than acting keeps this crate free of any
    /// notion of a forge.
    pub fn execute(&self, action: &LaneAction) -> Vec<LaneEvent> {
        match action {
            LaneAction::StartBuild {
                generation,
                members,
            } => vec![self.run_build(*generation, members)],
            LaneAction::LandAndPublish { members, artifact } => {
                match self.lander.land(members, artifact.as_deref()) {
                    Ok(_) => Vec::new(),
                    // A lander failure is infrastructure by construction: the
                    // build was GREEN, so nobody's code is at fault. The usual
                    // cause is the base moving under us and the forge's
                    // compare-and-swap rejecting the push.
                    //
                    // Re-enqueue every member so the next build re-merges them
                    // against the new base. Silently dropping them here would
                    // lose green work to a push race — the worst outcome
                    // available, because it looks like nothing happened.
                    Err(_) => members.iter().cloned().map(LaneEvent::Enqueue).collect(),
                }
            }
            LaneAction::Eject { .. } | LaneAction::Readmit { .. } | LaneAction::Report { .. } => {
                Vec::new()
            }
        }
    }

    fn run_build(&self, generation: u64, members: &[LaneMember]) -> LaneEvent {
        let changed: Vec<String> = {
            let mut v: Vec<String> = members
                .iter()
                .flat_map(|m| m.changed_files.iter().cloned())
                .collect();
            v.sort();
            v.dedup();
            v
        };

        let root = match self.tree.materialize(members) {
            Ok(r) => r,
            Err(e) => {
                return LaneEvent::BuildFinished {
                    generation,
                    outcome: LaneBuildOutcome::Infra {
                        reason: format!("candidate tree could not be materialized: {e}"),
                    },
                };
            }
        };

        let outcome = match self.legs.run(&root, &changed) {
            // A red tree with no diagnostics is not a usable red: the lane
            // cannot attribute it, and ejecting the whole queue on an empty
            // report punishes everyone for a reporting gap. Treat it as infra
            // and say why — the same no-vacuous-red discipline `statusfile`
            // applies when a red arrives without evidence.
            Ok(LegOutcome { tree, diagnostics })
                if tree == TreeState::Red && diagnostics.is_empty() =>
            {
                LaneBuildOutcome::Infra {
                    reason: "build reported red with no diagnostics — cannot attribute; \
                             treating as infrastructure rather than blaming a member"
                        .to_string(),
                }
            }
            Ok(LegOutcome { tree, diagnostics }) => match tree {
                TreeState::Green => LaneBuildOutcome::Green { artifact: None },
                TreeState::Red => LaneBuildOutcome::Red { diagnostics },
            },
            Err(e) => LaneBuildOutcome::Infra {
                reason: format!("build legs could not run: {e}"),
            },
        };

        self.tree.release(&root);
        LaneEvent::BuildFinished {
            generation,
            outcome,
        }
    }

    /// Drive the lane until it is quiet, collecting every action for the caller
    /// to report. Bounded so a policy bug cannot spin forever.
    pub fn pump(&self, lane: &mut LaneState, event: LaneEvent) -> Vec<LaneAction> {
        const MAX_STEPS: usize = 64;
        let mut all = Vec::new();
        let mut pending = std::collections::VecDeque::from(vec![event]);
        let mut steps = 0;
        // FIFO, not LIFO: follow-up events must be applied in the order they
        // were produced, or a lander re-enqueueing several members would
        // reverse their arrival order and quietly reshuffle the queue.
        while let Some(ev) = pending.pop_front() {
            steps += 1;
            if steps > MAX_STEPS {
                break;
            }
            let actions = lane.step(ev);
            for a in &actions {
                pending.extend(self.execute(a));
            }
            all.extend(actions);
        }
        all
    }
}
