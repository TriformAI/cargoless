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
    /// The rendered artifact this build produced, if any.
    ///
    /// `None` is legitimate — a check-only lane proves a tree compiles without
    /// emitting anything — and the lander deliberately leaves the pointer alone
    /// rather than erasing the last real green. But a lane that DOES build an
    /// artifact and reports `None` here silently never publishes, which looks
    /// exactly like a working lane until someone checks the pointer.
    pub artifact: Option<String>,
    /// Per-leg outcomes, in the order the runner reported them.
    ///
    /// The rolled-up `tree` answers "did it pass"; this answers "how far did it
    /// get, and what did each stage cost". For a staged lane that is the
    /// difference between "the build failed" and "stage 1 rejected it in 4
    /// minutes, stages 2-3 never ran" — the second is what an operator needs
    /// and what `GET /lane` should be able to show.
    pub legs: Vec<LegReport>,
}

/// One leg's outcome within a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegReport {
    pub id: String,
    pub tree: TreeState,
    pub required: bool,
    pub duration_ms: u128,
}

/// Hand-written rather than derived because `TreeState` is a frozen `tf-proto`
/// seam with no `Default` — and giving it one there would be a contract change
/// to answer a convenience question here.
///
/// Green is the right default for the same reason `TreeState::Green` is the
/// empty-tree verdict: a run that reported nothing has found nothing wrong.
/// Callers that care always set `tree` explicitly; this exists so a test can
/// say `..Default::default()` about the fields it is not testing.
impl Default for LegOutcome {
    fn default() -> Self {
        Self {
            tree: TreeState::Green,
            diagnostics: Vec::new(),
            artifact: None,
            legs: Vec::new(),
        }
    }
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
    /// Path, relative to the candidate root, where the legs leave the rendered
    /// artifact for the lander to publish.
    ///
    /// `None` means this is a check-only lane and nothing is published. Set it
    /// and the legs must write that file; a missing file on a green build is
    /// reported as infra rather than published as an empty pointer, because
    /// "green but produced nothing it promised" is a build-system fault, not a
    /// verdict about anyone's code.
    pub artifact_path: Option<PathBuf>,
}

impl ProfileLegRunner {
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            check_ids: Vec::new(),
            warm_target_dir: None,
            artifact_path: None,
        }
    }

    /// Publish the file the legs write at `path` (relative to the candidate
    /// root) as this build's artifact.
    #[must_use]
    pub fn publishing(mut self, path: impl Into<PathBuf>) -> Self {
        self.artifact_path = Some(path.into());
        self
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
        let legs = report
            .results
            .iter()
            .map(|r| LegReport {
                id: r.id.clone(),
                tree: r.tree,
                required: r.required,
                duration_ms: r.duration_ms,
            })
            .collect();

        // Only read the artifact on green. A red build may well have left a
        // stale file from a previous run in the (warm) target dir, and
        // publishing that would be the precise failure "never publish red"
        // exists to prevent.
        let artifact = match (&self.artifact_path, report.tree) {
            (Some(rel), TreeState::Green) => Some(std::fs::read_to_string(root.join(rel))?),
            _ => None,
        };

        Ok(LegOutcome {
            tree: report.tree,
            diagnostics: report.diagnostics,
            artifact,
            legs,
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

impl PointerLander {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
        }
    }

    /// Where the pointer lives, for callers that want to read it back.
    #[must_use]
    pub fn pointer_path(&self) -> PathBuf {
        crate::build::latest_green_path(&self.project_root)
    }
}

impl LaneLander for PointerLander {
    fn land(&self, members: &[LaneMember], artifact: Option<&str>) -> io::Result<LandOutcome> {
        let ids: Vec<&str> = members.iter().map(|m| m.id.as_str()).collect();
        let Some(published) = artifact else {
            // A green build that produced no artifact is legitimate — a
            // check-only lane proves the tree compiles without emitting one.
            // Say so rather than implying something shipped, and do NOT touch
            // the pointer: advancing it to nothing would erase the last real
            // green.
            return Ok(LandOutcome {
                detail: format!(
                    "green, no artifact to publish; {} member(s) cleared: {}",
                    ids.len(),
                    ids.join(", ")
                ),
            });
        };

        // `artifact` is the rendered `PublishedArtifact` the build produced.
        // Written atomically (temp + fsync + rename) by `write_pointer_atomic`,
        // so a crash mid-write leaves the previous pointer byte-untouched —
        // AC#4, never publish red, extended to never publish HALF.
        let path = self.pointer_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::build::write_pointer_atomic(&path, published)?;
        Ok(LandOutcome {
            detail: format!(
                "advanced {} for {} member(s): {}",
                path.display(),
                ids.len(),
                ids.join(", ")
            ),
        })
    }
}

/// Reports a green candidate and ships nothing.
///
/// For shadow-running a lane against a real project before anything depends on
/// it: the members are cleared and `GET /lane` shows the verdict, but no
/// pointer moves, no tag is pushed, no PR is touched. Whatever normally lands
/// changes stays authoritative.
///
/// This is the honest default for a fleet whose real "publish" means an epoch
/// tag plus an image pin plus a PR reconcile — none of which should happen on
/// the first run of a leg runner that has never executed. Cargoless does not
/// know how to do those things anyway (that is what [`LaneLander`] is for), so
/// the choice is between doing nothing and *saying so*, or doing nothing and
/// implying something shipped. This says so.
pub struct ReportOnlyLander;

impl LaneLander for ReportOnlyLander {
    fn land(&self, members: &[LaneMember], artifact: Option<&str>) -> io::Result<LandOutcome> {
        let ids: Vec<&str> = members.iter().map(|m| m.id.as_str()).collect();
        // Name the artifact's existence without publishing it. "green, would
        // have published N bytes" is the shadow signal worth having: it proves
        // the legs really produced something, which is exactly what a later
        // arming decision needs to know.
        let produced = match artifact {
            Some(a) => format!("{} byte artifact produced (NOT published)", a.len()),
            None => "no artifact (check-only)".to_string(),
        };
        Ok(LandOutcome {
            detail: format!(
                "green, reported only — nothing landed or published; {produced}; \
                 {} member(s) cleared: {}",
                ids.len(),
                ids.join(", ")
            ),
        })
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
            Ok(LegOutcome {
                tree, diagnostics, ..
            }) if tree == TreeState::Red && diagnostics.is_empty() => LaneBuildOutcome::Infra {
                reason: "build reported red with no diagnostics — cannot attribute; \
                         treating as infrastructure rather than blaming a member"
                    .to_string(),
            },
            Ok(LegOutcome {
                tree,
                diagnostics,
                artifact,
                ..
            }) => match tree {
                TreeState::Green => LaneBuildOutcome::Green { artifact },
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
        self.pump_observed(lane, event, |_| {})
    }

    /// [`Self::pump`] with a callback fired after every state transition, before
    /// the resulting actions are executed.
    ///
    /// The reason this exists: `execute` runs the BUILD, which for a real lane
    /// is tens of minutes, and the transition that flips the phase to
    /// `Building` happens on the line above it. A host that only observes the
    /// lane after `pump` returns therefore reports `idle` for the entire
    /// duration of every build — exactly the window `GET /lane` exists to
    /// explain. An author whose change stopped moving would look, see "idle",
    /// and reasonably conclude the lane never received their submission.
    ///
    /// The callback runs on the pump thread and must not block; it is for
    /// publishing a snapshot, not for work.
    pub fn pump_observed(
        &self,
        lane: &mut LaneState,
        event: LaneEvent,
        mut observe: impl FnMut(&LaneState),
    ) -> Vec<LaneAction> {
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
            // Observe the NEW state before running the actions it produced.
            // `lane.step` has already set phase/in_flight; `execute` is what
            // blocks.
            observe(lane);
            for a in &actions {
                pending.extend(self.execute(a));
            }
            all.extend(actions);
        }
        all
    }
}
