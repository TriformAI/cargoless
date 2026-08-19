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
//! ## Fail closed — but do not fail *blind*
//!
//! Every failure that is not a compiler verdict — the legs could not be
//! launched, the lander errored, the tree could not be created — reports
//! [`LaneBuildOutcome::Infra`], never `Red`. An infra failure keeps members
//! queued; a `Red` ejects someone. Misclassifying the first as the second
//! blames people for a runner dying, which is how a fleet learns to distrust
//! its own gate.
//!
//! The one failure that is *not* infrastructure, despite producing no compiler
//! verdict, is a member that cannot be merged onto the base. Git names the
//! member before we infer anything, so it reports
//! [`LaneBuildOutcome::Conflict`] and that member alone is ejected. Folding it
//! into `Infra` is the mirror-image mistake — it exonerates a member the lane
//! can prove is at fault, and because infra ejects nobody, the member is
//! re-included in every subsequent candidate and the queue never drains. See
//! [`MaterializeError`].
//!
//! An exact external roster can also become stale while its remote build is
//! running. A trusted dispatcher may name the one obsolete member with the
//! versioned exit-76 protocol; that becomes [`LaneBuildOutcome::RosterStale`],
//! removes no current enrollment, and lets the unjudged peers retry without an
//! infrastructure cooldown.

use std::fs;
use std::io;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use cargoless_proto::TreeState;

use crate::lane::{LaneAction, LaneBuildOutcome, LaneEvent, LaneMember, LaneState};
use crate::project_checks;

const EX_ROSTER_STALE: i32 = 76;
const ROSTER_STALE_MARKER: &str = "cargoless-lane-roster-stale-v1\t";

#[derive(Debug)]
struct RosterStaleError {
    id: String,
}

impl std::fmt::Display for RosterStaleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dispatcher reported stale roster member `{}`", self.id)
    }
}

impl std::error::Error for RosterStaleError {}

fn roster_stale_id(output: &str) -> Option<String> {
    let mut ids = output.lines().filter_map(|line| {
        let id = line.strip_prefix(ROSTER_STALE_MARKER)?;
        (!id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')))
        .then(|| id.to_string())
    });
    let id = ids.next()?;
    ids.next().is_none().then_some(id)
}

/// Why a candidate tree could not be produced.
///
/// The distinction is load-bearing. `Infra` is nobody's fault and everyone
/// stays queued; `Conflict` names a member and gets it ejected. Collapsing the
/// two — which is what `io::Result` did until 2026-08-02 — means an unmergeable
/// member is treated as a transient, never leaves the queue, and is re-included
/// in every subsequent candidate. Observed in production: generations 2 through
/// 5 each died on the same unmergeable member while the rest of the queue
/// waited behind it.
#[derive(Debug)]
pub enum MaterializeError {
    /// A named member could not be merged onto the base. `files` is
    /// best-effort: git's unmerged paths when they can be read, empty
    /// otherwise. An empty list still ejects — it only costs a coarser
    /// readmission rule.
    Conflict {
        id: String,
        files: Vec<PathBuf>,
        /// Earlier-applied members whose declared changes overlap Git's
        /// unmerged paths. Later members have not been applied and therefore
        /// cannot be named as participants in this collision.
        shared_with: Vec<String>,
        reason: String,
    },
    /// A named member is ALREADY contained in the tree being built — it landed
    /// between enqueue and materialize (merged by hand, or carried by an
    /// earlier candidate). Merging it would write an empty commit and leave the
    /// roster naming a closed PR for the lander to act on.
    ///
    /// Attributable but blameless: nobody's code is wrong, the member is simply
    /// done. It leaves the queue and the rest of the roster rebuilds.
    Stale { id: String, head: String },
    /// Anything else: fetch failed, worktree could not be created, disk full.
    /// Not attributable to any member.
    Infra(io::Error),
}

impl MaterializeError {
    /// An infrastructure failure that NAMES the operation and the path.
    ///
    /// `Infra` wraps a bare `io::Error`, and the driver renders it as
    /// `candidate tree could not be materialized: {e}`. For an error straight
    /// off a syscall that whole message is the syscall's — observed in
    /// production as:
    ///
    /// ```text
    /// lane-build generation=13 outcome=infra reason=candidate tree could not
    /// be materialized: No such file or directory (os error 2)
    /// ```
    ///
    /// Which path? Which step? The candidate root, the scratch parent, the repo
    /// and the `git` binary itself can all produce exactly those bytes, and the
    /// operator has no way to tell them apart. A verdict nobody can act on is
    /// the same as no verdict, which is the failure the trail exists to end.
    ///
    /// `e.kind()` is preserved rather than flattened to
    /// [`io::Error::other`]: nothing switches on it today, and a caller that
    /// starts to must not find the kind quietly destroyed by the code that was
    /// meant to make the error *more* informative.
    #[must_use]
    pub fn infra_at(op: &str, path: &Path, e: &io::Error) -> Self {
        Self::Infra(io::Error::new(
            e.kind(),
            format!("{op} {}: {e}", path.display()),
        ))
    }
}

impl std::fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict { id, reason, .. } => {
                write!(
                    f,
                    "member `{id}` could not be merged onto the base: {reason}"
                )
            }
            Self::Stale { id, head } => {
                write!(
                    f,
                    "member `{id}` ({head}) already landed — it is contained in the candidate base"
                )
            }
            Self::Infra(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for MaterializeError {
    fn from(e: io::Error) -> Self {
        Self::Infra(e)
    }
}

/// Materialises `base + members` somewhere the build legs can run.
///
/// Implementations must produce a tree that is **disposable** — the lane may
/// build many candidates and never reuses one — and must not mutate the
/// caller's working tree.
pub trait CandidateTree {
    /// Build the candidate and return its root.
    ///
    /// A [`MaterializeError::Conflict`] names the member that could not be
    /// merged and the lane ejects exactly that member, so the rest of the queue
    /// builds without it. [`MaterializeError::Infra`] is nobody's fault and
    /// everyone stays queued.
    fn materialize(&self, members: &[LaneMember]) -> Result<PathBuf, MaterializeError>;

    /// Release the tree. Best-effort: a leaked scratch dir is a disk problem,
    /// never a reason to fail a build that already produced a verdict.
    fn release(&self, _root: &Path) {}
}

/// Runs the project's build legs against a candidate root.
pub trait LegRunner {
    fn run(&self, root: &Path, changed_files: &[String]) -> io::Result<LegOutcome>;
}

/// WHERE a lane runs its legs, as one value.
///
/// A daemon selects this at boot. It exists so the choice is a single
/// exhaustively-matched value rather than a widening tuple of options — the
/// previous shape passed `Option<(Vec<String>, String, String)>` and adding a
/// third destination would have made the call site unreadable and the illegal
/// combinations invisible.
pub enum LegPlan {
    /// Compile in the daemon process.
    ///
    /// Zero-config, and the right answer for a single developer. NOT the right
    /// answer for a daemon that can reach a credential: `cargo` executes
    /// `build.rs` and proc-macros from the candidate, which is unreviewed code.
    InProcess {
        profile: String,
        artifact_path: Option<PathBuf>,
    },
    /// Publish the candidate and hand it to an external builder.
    Dispatch {
        command: Vec<String>,
        remote: String,
        ref_prefix: String,
    },
    /// Publish the candidate, roll it onto a preview slot, and wait for the
    /// slot to actually SERVE it. The strongest of the three: it proves the
    /// tree boots and answers, not merely that it compiles.
    Preview {
        daemon: String,
        token: String,
        slot: String,
        remote: String,
        ref_prefix: String,
        /// Slot that builds the BASE alone, for the base-health check. Empty
        /// disables it — see [`PreviewLegRunner::base_slot`].
        base_slot: String,
    },
}

impl LegPlan {
    /// One line naming where the legs will run.
    ///
    /// Borrows rather than consuming so a caller can log it BEFORE building the
    /// runner — a boot line that only appears after construction is missing
    /// exactly when construction is what failed.
    ///
    /// This is not decoration: a lane that can move the trunk must announce
    /// where it compiles unreviewed code, because that is the difference
    /// between a sandbox and this pod.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            LegPlan::InProcess { .. } => {
                "in-process (compiles candidate code in THIS pod)".to_string()
            }
            LegPlan::Dispatch {
                command,
                remote,
                ref_prefix,
            } => format!(
                "dispatched:{} remote={remote} ref_prefix={ref_prefix}",
                command.join(" ")
            ),
            LegPlan::Preview {
                daemon,
                slot,
                remote,
                ..
            } => format!("preview:{slot} daemon={daemon} remote={remote}"),
        }
    }

    /// Build the runner. Pair with [`Self::describe`] for the boot line.
    #[must_use]
    pub fn into_runner(self) -> (Box<dyn LegRunner + Send>, String) {
        match self {
            LegPlan::InProcess {
                profile,
                artifact_path,
            } => {
                let mut legs = ProfileLegRunner::new(profile);
                legs.artifact_path = artifact_path;
                (
                    Box::new(legs),
                    "in-process (compiles candidate code in THIS pod)".to_string(),
                )
            }
            LegPlan::Dispatch {
                command,
                remote,
                ref_prefix,
            } => {
                let what = format!(
                    "dispatched:{} remote={remote} ref_prefix={ref_prefix}",
                    command.join(" ")
                );
                (
                    Box::new(DispatchLegRunner::new(command, remote, ref_prefix)),
                    what,
                )
            }
            LegPlan::Preview {
                daemon,
                token,
                slot,
                remote,
                ref_prefix,
                base_slot,
            } => {
                let what = format!("preview:{slot} daemon={daemon} remote={remote}");
                let mut r = PreviewLegRunner::new(daemon, token, slot, remote);
                r.ref_prefix = ref_prefix;
                r.base_slot = base_slot;
                (Box::new(r), what)
            }
        }
    }

    /// Does this plan produce a LOCAL artifact for a lander to publish?
    ///
    /// Only in-process does. Remote plans may report an opaque identity to a
    /// command lander, but they do not create a LOCAL file that PointerLander
    /// can publish.
    #[must_use]
    pub fn publishes_locally(&self) -> bool {
        matches!(
            self,
            LegPlan::InProcess {
                artifact_path: Some(_),
                ..
            }
        )
    }
}

/// So a caller can pick the runner at runtime without monomorphising a branch
/// per combination.
///
/// `LaneHost::spawn` is generic over the runner AND the lander, so choosing
/// between in-process and dispatched legs at boot would otherwise need one
/// `spawn` call per (runner × lander) pair — four for two of each, and eight
/// the next time either grows. The trait is object-safe (`&self`, concrete
/// argument types), so boxing costs one vtable hop per BUILD — measured in
/// tens of minutes — and buys back the combinatorics.
impl LegRunner for Box<dyn LegRunner + Send> {
    fn run(&self, root: &Path, changed_files: &[String]) -> io::Result<LegOutcome> {
        (**self).run(root, changed_files)
    }
}

/// What a leg run reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegOutcome {
    pub tree: TreeState,
    pub diagnostics: Vec<cargoless_proto::Diagnostic>,
    /// Whether error paths are evidence for member attribution.
    ///
    /// Compiler/project-check legs emit structured source paths and use
    /// [`RedAttribution::ChangedFiles`]. A preview daemon currently exposes a
    /// free-text terminal reason only; its synthetic diagnostic anchor is for
    /// display, never ownership, so it uses [`RedAttribution::Unattributed`].
    pub red_attribution: RedAttribution,
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

/// How a red leg's diagnostics may be mapped back to lane members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedAttribution {
    /// Diagnostic paths came from the checker and may be matched to changed
    /// files.
    ChangedFiles,
    /// Diagnostics describe a real red but their paths are synthetic display
    /// anchors. No member may be blamed from them.
    Unattributed,
}

/// Hand-written rather than derived because `TreeState` is a frozen `cargoless-proto`
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
            red_attribution: RedAttribution::ChangedFiles,
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
            red_attribution: RedAttribution::ChangedFiles,
            artifact,
            legs,
        })
    }
}

/// Runs the legs SOMEWHERE ELSE: publishes the candidate on a ref and hands it
/// to an external builder, which compiles it and reports back.
///
/// # Why this exists rather than just using [`ProfileLegRunner`]
///
/// `ProfileLegRunner` compiles in this process. For a lane that is a privilege
/// escalation, and on tf-multiverse it is a demonstrated one: `cargo` executes
/// `build.rs` and proc-macros from the tree it compiles, that tree is the
/// candidate merge of code nobody has reviewed yet, and the daemon's container
/// can read a **push-capable** forge credential from `.git/config` on its
/// shared volume (verified 2026-07-31 with `git push --dry-run`, which reported
/// `* [new branch]`). So "we only compile it, we don't run it" is false, and
/// the blast radius is push access to the trunk.
///
/// It is also a fidelity problem: a daemon image is not a build image. On
/// tf-multiverse the daemon ships wasm-bindgen 0.2.114 against a workspace that
/// resolves 0.2.118, and has no warm cooked target dir, where the deploy
/// builder pins the right CLI and keeps a warm cache.
///
/// # The contract
///
/// The command is spawned with the candidate's ref name and sha in the
/// environment, and must exit 0 for green, non-zero for red. Anything it prints
/// on stdout is parsed as cargo JSON, so a red carries real file paths and the
/// lane can attribute it.
///
/// Cargoless stays forge-agnostic: it publishes a ref and runs a command. What
/// that command does — dispatch a workflow, submit a k8s Job, ssh a builder —
/// is the operator's business, exactly as [`LaneLander`] is for landing.
pub struct DispatchLegRunner {
    /// Argv of the dispatcher. Receives `CARGOLESS_LANE_REF`,
    /// `CARGOLESS_LANE_SHA` and `CARGOLESS_LANE_CHANGED_FILES` in its env.
    pub command: Vec<String>,
    /// Where to publish the candidate so the builder can fetch it. Passed to
    /// `git push` verbatim, so it may be a remote name or a URL.
    pub remote: String,
    /// Ref namespace for published candidates, e.g. `refs/heads/lane-candidate`.
    /// The sha is appended, so a build is always addressable after the fact and
    /// two candidates never collide.
    pub ref_prefix: String,
    /// How long to wait for the dispatcher before calling it infrastructure.
    pub timeout: Duration,
}

impl DispatchLegRunner {
    pub fn new(
        command: Vec<String>,
        remote: impl Into<String>,
        ref_prefix: impl Into<String>,
    ) -> Self {
        Self {
            command,
            remote: remote.into(),
            ref_prefix: ref_prefix.into(),
            // A real release build is 25-80 minutes; 2h leaves headroom for a
            // queued builder without waiting forever on a dead one.
            timeout: Duration::from_secs(7200),
        }
    }

    /// Run `cmd`, killing its whole process tree if `timeout` elapses.
    ///
    /// Returns (success, combined output, elapsed_ms). A timeout is `Err` —
    /// the lane must not read "we gave up waiting" as a code red.
    ///
    /// The pgid + setsid setup mirrors `project_checks::check_command`, for the
    /// same reason recorded there: killing only the immediate child leaves
    /// grandchildren reparented to init and still running. It is inlined rather
    /// than shared because that function's loop is entangled with the check
    /// cancellation flag, and lifting it out to serve two callers is a bigger
    /// change than this one earns.
    #[allow(clippy::type_complexity)]
    fn run_to_completion(
        mut cmd: Command,
        timeout: Duration,
    ) -> io::Result<(bool, Option<i32>, String, u128)> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
            // setsid too: `kill_process_tree`'s escapee sweep enumerates the
            // SESSION (`pgrep -s`), which only finds anything if the child is a
            // session leader. process_group alone would leave that half inert.
            //
            // SAFETY: pre_exec runs post-fork/pre-exec in a single-threaded
            // child; setsid(2) is async-signal-safe. EPERM (already a leader)
            // is swallowed. Mirrors project_checks::check_command.
            unsafe {
                cmd.pre_exec(|| {
                    unsafe extern "C" {
                        fn setsid() -> i32;
                    }
                    let _ = setsid();
                    Ok(())
                });
            }
        }
        let started = Instant::now();
        let mut child = cmd.spawn()?;
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let out_t = thread::spawn(move || {
            let mut s = String::new();
            if let Some(p) = stdout.as_mut() {
                let _ = p.read_to_string(&mut s);
            }
            s
        });
        let err_t = thread::spawn(move || {
            let mut s = String::new();
            if let Some(p) = stderr.as_mut() {
                let _ = p.read_to_string(&mut s);
            }
            s
        });

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Ok(s),
                Ok(None) if Instant::now() >= deadline => {
                    // The crate's existing reaper, not a hand-rolled kill: it
                    // SIGKILLs the process group AND sweeps setpgid escapees
                    // still in the session. That second sweep is what stops a
                    // timed-out build leaking grandchildren that reparent to
                    // init and keep compiling (observed 2026-06-08).
                    crate::project_checks::kill_process_tree(&mut child);
                    let _ = child.wait();
                    break Err(io::Error::other(format!(
                        "dispatcher exceeded {}s",
                        timeout.as_secs()
                    )));
                }
                Ok(None) => thread::sleep(Duration::from_millis(200)),
                Err(e) => break Err(e),
            }
        };
        let combined = format!(
            "{}\n{}",
            out_t.join().unwrap_or_default(),
            err_t.join().unwrap_or_default()
        );
        let elapsed = started.elapsed().as_millis();
        match status {
            // `code()` is None when the child died on a SIGNAL. That is not a
            // verdict either, but it is already not-success, and the caller
            // only special-cases an explicit EX_TEMPFAIL.
            Ok(s) => Ok((s.success(), s.code(), combined, elapsed)),
            Err(e) => Err(e),
        }
    }

    /// Publish the candidate at `root` as `<ref_prefix>/<sha>` on `remote`,
    /// returning `(sha, refname)`.
    ///
    /// Shared with [`PreviewLegRunner`]: both need the candidate reachable by
    /// something that is not this process, and both address it by its own sha.
    ///
    /// `--force` is correct AND safe here precisely because the ref name
    /// contains the sha: the only thing it can overwrite is a byte-identical
    /// rebuild of itself. Publishing under a name that did NOT carry the sha
    /// would make force a real hazard and two concurrent candidates a race.
    ///
    /// The prefix must live under `refs/heads/` for anything that mirrors with
    /// the usual `+refs/heads/*:refs/remotes/origin/*` refspec to see it —
    /// tf-multiverse's preview daemon does exactly that, so a candidate
    /// published outside `refs/heads/` is invisible to it and the instance
    /// silently never binds.
    pub(crate) fn publish_candidate(
        root: &Path,
        remote: &str,
        ref_prefix: &str,
    ) -> io::Result<(String, String)> {
        let sha = Self::head_sha(root)?;
        let refname = format!("{}/{sha}", ref_prefix.trim_end_matches('/'));
        let push = Command::new("git")
            .current_dir(root)
            .args(["push", "--force", remote, &format!("HEAD:{refname}")])
            .output()?;
        if !push.status.success() {
            return Err(io::Error::other(format!(
                "could not publish the candidate as {refname}: {}",
                String::from_utf8_lossy(&push.stderr).trim()
            )));
        }
        Ok((sha, refname))
    }

    fn head_sha(root: &Path) -> io::Result<String> {
        let out = Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "HEAD"])
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "could not resolve the candidate HEAD: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

impl LegRunner for DispatchLegRunner {
    fn run(&self, root: &Path, changed_files: &[String]) -> io::Result<LegOutcome> {
        let Some(program) = self.command.first() else {
            return Err(io::Error::other(
                "dispatch leg runner configured with an empty command",
            ));
        };
        // Publish the candidate so an external builder can fetch it.
        let (sha, refname) = Self::publish_candidate(root, &self.remote, &self.ref_prefix)?;

        // NOTE: no `current_dir(root)`. The dispatcher must not run inside the
        // candidate tree — that tree is the unreviewed code, and a dispatcher
        // that resolved a script relative to it would reintroduce the very
        // execution this type exists to prevent.
        let mut cmd = Command::new(program);
        cmd.args(&self.command[1..])
            .env("CARGOLESS_LANE_REF", &refname)
            .env("CARGOLESS_LANE_SHA", &sha)
            .env("CARGOLESS_LANE_CHANGED_FILES", changed_files.join("\n"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (success, code, text, duration_ms) = Self::run_to_completion(cmd, self.timeout)?;

        // EX_TEMPFAIL. A dispatcher that could not get a verdict — the remote
        // build was cancelled, the runner vanished, the queue timed out — must
        // say so in a way the lane can tell apart from "your code is broken".
        // Without this the only signal is "non-zero", and an infrastructure
        // fault would eject whichever member happened to be aboard: the fastest
        // way to teach a fleet to distrust its own gate.
        //
        // 75 rather than a bespoke number because sysexits.h already means this
        // and shell authors reach for it without being told.
        const EX_TEMPFAIL: i32 = 75;
        if code == Some(EX_TEMPFAIL) {
            return Err(io::Error::other(format!(
                "dispatcher reported a transient failure (exit {EX_TEMPFAIL}); \
                 no verdict was produced"
            )));
        }

        // Versioned, fail-closed protocol for an exact remote roster that
        // changed while its build was running. Exit 76 without exactly one
        // well-formed marker remains infrastructure: an ambiguous string may
        // never remove a lane member.
        if code == Some(EX_ROSTER_STALE) {
            let Some(id) = roster_stale_id(&text) else {
                return Err(io::Error::other(format!(
                    "dispatcher exited {EX_ROSTER_STALE} without exactly one valid roster-stale marker"
                )));
            };
            return Err(io::Error::other(RosterStaleError { id }));
        }

        let diagnostics = crate::cargodiag::parse_cargo_json(root, &text);
        let tree = if success {
            TreeState::Green
        } else {
            TreeState::Red
        };

        Ok(LegOutcome {
            tree,
            diagnostics,
            red_attribution: RedAttribution::ChangedFiles,
            // This is the immutable candidate identity the external builder
            // just verdict-ed. It is deliberately carried through the existing
            // artifact seam to CommandLander, which exports it as
            // CARGOLESS_LANE_ARTIFACT. Nothing local is published; the trusted
            // lander fetches the content-addressed ref and CASes this exact sha.
            artifact: Some(sha),
            legs: vec![LegReport {
                id: "dispatch".to_string(),
                tree,
                required: true,
                duration_ms,
            }],
        })
    }
}

/// Rolls the candidate onto a PREVIEW SLOT and waits for it to be live.
///
/// # Why a preview slot is a better gate than a build
///
/// A build proves the tree compiles. A preview proves it *boots and serves* —
/// the app came up, its health endpoint answered, and the never-serve-red
/// promote actually flipped to this candidate. Those are different claims, and
/// only the second is what "ready to merge" means for a deployed service.
///
/// This adds no second build path. It reuses the app-serve tier wholesale:
/// worktree-per-instance, exact-sha checkout, mid-build HEAD-move detection,
/// warm per-lane target dir, ENOSPC classification and self-heal, bundle
/// pruning, health-gated promote at a single site, TTL reaping. All of that
/// already exists and is in production; re-deriving it is how the lane ended up
/// re-solving four problems this repo had already solved.
///
/// # The gate
///
/// One FIXED slot name — the serial queue has exactly one staging area, so the
/// slot is a position, not a per-candidate resource. `POST /instances` re-points
/// a live preview at a new ref and renews its TTL rather than churning it, which
/// is precisely the behaviour a queue wants.
///
/// Verdict comes from `GET /app`:
/// * `serving_sha == <candidate>` ⇒ **green and live**
/// * `last_red_sha == <candidate>` ⇒ **red**, with `last_red_reason`
///
/// Anything else is "not yet"; the deadline decides when to stop waiting, and a
/// timeout is `Err` (infrastructure) rather than a red, because "we stopped
/// looking" is not a verdict about anyone's code.
/// How many times to try pointing the slot before calling it infrastructure.
///
/// Sized to a POD REPLACEMENT, not a blip. The previous 5 × 6s ≈ 30s was
/// calibrated against "a daemon that is RESTARTING (seconds)", and that
/// estimate was wrong by an order of magnitude: measured 2026-08-03, a preview
/// roll took ~4 minutes end to end (23:34 Terminating -> 23:36 Init:1/2 ->
/// ~23:38 3/3 Running).
///
/// It cannot be made faster either. The preview Deployment is `Recreate` with
/// `replicas: 1` because its 220Gi workspace PVC is ReadWriteOnce — two pods
/// can never mount it at once, so there is no surge/handoff to hide the gap
/// behind. Every manifest edit therefore removes the only backend for minutes.
///
/// 30 attempts × 10s = 5 minutes of tolerance, comfortably past the observed
/// 4-minute replacement with headroom for a slow image pull. A genuinely
/// absent daemon still surfaces as Infra — just 5 minutes later, which costs
/// far less than ejecting the members of a 20-45 minute build whose code was
/// never at fault.
const POINT_ATTEMPTS: u32 = 30;

/// Gap between point attempts. Paired with POINT_ATTEMPTS above: 30 × 10s.
/// Ten seconds rather than six so a 5-minute window does not cost 50 curl
/// invocations against a daemon that is provably not listening yet.
const POINT_RETRY_DELAY: Duration = Duration::from_secs(10);

/// How long an acknowledged point may remain absent from `/app` before the
/// lane reasserts it.
///
/// The preview daemon persists its registry, but a replacement process can
/// restore an older snapshot after the POST was acknowledged. 30 seconds is
/// long enough for ordinary ref discovery and state publication, while still
/// turning that restart race into a short pause instead of the runner's 2h
/// verdict timeout.
const POINT_REASSERT_INTERVAL: Duration = Duration::from_secs(30);

/// How long to wait for a busy slot before pointing anyway.
///
/// Generous, because the thing we are waiting on is a real tf-multiverse
/// compile (20-45 min observed). Bounded all the same: a slot wedged
/// `building` forever must eventually surface as Infra rather than hold the
/// lane open indefinitely.
const SLOT_FREE_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// Poll gap while waiting for the slot. Slow on purpose — this is a
/// tens-of-minutes wait, so a tight loop would only add noise.
const SLOT_FREE_POLL: Duration = Duration::from_secs(20);

/// Does this slot's red reason describe OUR failure rather than the code's?
///
/// Fails toward RED by design. A false "infrastructure" verdict would let a
/// genuinely broken candidate stay queued and eventually land, which is far
/// worse than a false accusation — so this matches only phrases that cannot
/// plausibly come from a compiler or a failing test.
///
/// The patterns are anchored on what the preview slot actually emits when its
/// own setup fails: `git checkout <sha> failed: fatal: cannot change to
/// '<path>': No such file or directory` (observed 2026-08-02, after a PVC fault
/// removed the slot's worktree).
/// The base slot's own `last_red_reason` from an `/app` snapshot, or `""`.
///
/// Empty covers every "cannot tell" case — no such slot, no field, unparseable
/// JSON — and [`same_failure`] treats empty as "not the same", so an unreadable
/// snapshot can never suppress a genuine verdict. Failing that direction is
/// deliberate: a missed base-red costs one false ejection, which the lane
/// already survives; a wrongly-suppressed red would let a broken member land.
/// The per-file evidence a red slot published, from its `/app` instance row.
///
/// Every "cannot tell" shape — field absent (an older daemon), not an array,
/// non-string or blank entries — yields EMPTY, and empty routes the red to the
/// Unattributed path where every member is held. That is the fail-safe
/// direction: missing evidence must widen the hold, never invent a culprit.
fn red_files_from_instance(inst: &serde_json::Value) -> Vec<String> {
    inst.get("last_red_files")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str())
                .filter(|f| !f.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn base_red_reason(snapshot: &serde_json::Value, base_slot: &str) -> String {
    snapshot
        .get("instances")
        .and_then(|i| i.as_array())
        .and_then(|a| {
            a.iter()
                .find(|x| x.get("name").and_then(|n| n.as_str()) == Some(base_slot))
        })
        .and_then(|inst| inst.get("last_red_reason"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Do two build failures name the SAME underlying error?
///
/// Compared on the trailing window, not the whole string. The base and the
/// candidate build different shas in different worktrees, so their reasons
/// diverge in the leading path and step detail while ending identically in the
/// compiler's own verdict — `error: could not compile X (lib) due to 1 previous
/// error; N warnings emitted`. That tail is what identifies the failure.
///
/// Empty on either side is NEVER a match: no evidence must not read as
/// agreement.
fn same_failure(a: &str, b: &str) -> bool {
    /// Long enough to carry `could not compile <crate> (lib) due to ...` — the
    /// part that names the failing crate — and short enough not to reach back
    /// into the per-build path noise.
    const TAIL: usize = 60;
    let tail = |s: &str| {
        let t = s.trim_end();
        t.char_indices()
            .rev()
            .nth(TAIL - 1)
            .map(|(i, _)| t[i..].to_string())
            .unwrap_or_else(|| t.to_string())
    };
    if a.trim().is_empty() || b.trim().is_empty() {
        return false;
    }
    tail(a) == tail(b)
}

fn reason_is_infrastructure(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    // Each phrase names a step BEFORE compilation: preparing the tree, reaching
    // the repo, or having somewhere to put it. A compiler error never says any
    // of these about its own run.
    const SETUP_FAILURES: &[&str] = &[
        "git checkout",
        "cannot change to",
        "no such file or directory",
        "could not create worktree",
        "worktree add failed",
        "no space left on device",
        "could not fetch",
        "repository not found",
    ];
    SETUP_FAILURES.iter().any(|p| r.contains(p))
}

/// Is this slot mid-build in the `/app` snapshot?
///
/// Best-effort by design: unparseable JSON, an absent slot, or a missing
/// `phase` all read as NOT busy, so a bad snapshot can never wedge the lane
/// waiting for a slot it cannot see. The phases that mean "work in flight" are
/// the same set `disk-gc` treats as busy, kept in sync deliberately.
fn slot_is_building(snapshot: &str, slot: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(snapshot) else {
        return false;
    };
    v.get("instances")
        .and_then(|i| i.as_array())
        .and_then(|a| {
            a.iter()
                .find(|x| x.get("name").and_then(|n| n.as_str()) == Some(slot))
        })
        .and_then(|inst| inst.get("phase"))
        .and_then(|p| p.as_str())
        .is_some_and(|p| matches!(p, "building" | "queued" | "probing" | "probing+serving"))
}

/// Did the preview daemon forget an acknowledged point for `sha`?
///
/// This deliberately requires a readable `/app` snapshot. Invalid JSON is no
/// evidence of lost state, so the normal poll error/deadline remains in charge.
/// Likewise, another active build is never interrupted: the reusable slot may
/// only be re-pointed when it is absent or quiescent. Seeing our SHA anywhere
/// in the slot is proof the point survived, including a terminal red which the
/// caller will classify on the same snapshot.
fn slot_needs_repoint(snapshot: &str, slot: &str, sha: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(snapshot) else {
        return false;
    };
    let Some(instances) = v.get("instances").and_then(|i| i.as_array()) else {
        return false;
    };
    let Some(inst) = instances
        .iter()
        .find(|x| x.get("name").and_then(|n| n.as_str()) == Some(slot))
    else {
        return true;
    };

    if ["serving_sha", "pending_sha", "last_red_sha"]
        .iter()
        .any(|field| inst.get(*field).and_then(|x| x.as_str()) == Some(sha))
    {
        return false;
    }

    inst.get("phase")
        .and_then(|p| p.as_str())
        .is_some_and(|phase| matches!(phase, "idle" | "serving"))
}

pub struct PreviewLegRunner {
    /// Base URL of the daemon that owns the preview slot, e.g.
    /// `http://cargoless-preview.triform-staging.svc.cluster.local:8787`.
    pub daemon: String,
    /// Bearer token for `POST /instances`. `GET /app` is auth-exempt, so this
    /// is only needed to create or re-point the slot.
    pub token: String,
    /// The slot name. One per lane — see above.
    pub slot: String,
    /// Where to publish the candidate, and under what prefix. The prefix must
    /// live under `refs/heads/` or the preview daemon's mirror will not see it.
    pub remote: String,
    pub ref_prefix: String,
    /// How long to wait for the slot to go live before calling it infra.
    pub timeout: Duration,
    /// Gap between `GET /app` polls.
    pub poll: Duration,
    /// The slot that builds the BASE alone, with no candidate merged into it —
    /// `dev` on tf-multiverse. Used to tell "this member broke the build" from
    /// "the trunk was already broken", which are indistinguishable from the
    /// candidate's own red. Empty disables the check.
    pub base_slot: String,
}

impl PreviewLegRunner {
    pub fn new(
        daemon: impl Into<String>,
        token: impl Into<String>,
        slot: impl Into<String>,
        remote: impl Into<String>,
    ) -> Self {
        Self {
            daemon: daemon.into(),
            token: token.into(),
            slot: slot.into(),
            remote: remote.into(),
            ref_prefix: "refs/heads/lane-candidate".to_string(),
            // A cold preview build of a real app is minutes; tf-multiverse's
            // own manifest allows 600s just for the health probe after a build
            // that can take 45. 2h leaves room without waiting on a dead slot.
            timeout: Duration::from_secs(7200),
            poll: Duration::from_secs(10),
            // Empty = no base-health check. The host sets this to the slot that
            // builds the base alone (`dev` on tf-multiverse); a project without
            // such a slot keeps today's behaviour.
            base_slot: String::new(),
        }
    }

    /// Name the slot that builds the BASE with no candidate merged in, enabling
    /// the base-health check in [`Self::run`].
    pub fn with_base_slot(mut self, slot: impl Into<String>) -> Self {
        self.base_slot = slot.into();
        self
    }

    /// One `GET /app`, returning the raw body. Auth-exempt by design, so a
    /// poller never needs the bearer token.
    fn app_snapshot(&self) -> io::Result<String> {
        let out = Command::new("curl")
            .args([
                "-sS",
                "--max-time",
                "20",
                &format!("{}/app", self.daemon.trim_end_matches('/')),
            ])
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "GET /app failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Point the reusable slot, tolerating a daemon that is between processes.
    ///
    /// Reused both for the initial registration and for repairing an
    /// acknowledged registration lost when a replacement daemon restores an
    /// older durable snapshot.
    fn point_slot(&self, body: &str, refname: &str) -> io::Result<()> {
        let mut last_err = String::new();
        for attempt in 1..=POINT_ATTEMPTS {
            let post = Command::new("curl")
                .args([
                    "-sS",
                    "-f",
                    "-X",
                    "POST",
                    "--max-time",
                    "30",
                    "-H",
                    &format!("Authorization: Bearer {}", self.token),
                    "-H",
                    "Content-Type: application/json",
                    "-d",
                    body,
                    &format!("{}/instances", self.daemon.trim_end_matches('/')),
                ])
                .output()?;
            if post.status.success() {
                return Ok(());
            }
            last_err = String::from_utf8_lossy(&post.stderr).trim().to_string();
            if attempt < POINT_ATTEMPTS {
                std::thread::sleep(POINT_RETRY_DELAY);
            }
        }
        Err(io::Error::other(format!(
            "could not point preview slot {:?} at {refname} after {} attempts: {}",
            self.slot, POINT_ATTEMPTS, last_err
        )))
    }
}

impl LegRunner for PreviewLegRunner {
    fn run(&self, root: &Path, _changed_files: &[String]) -> io::Result<LegOutcome> {
        let started = Instant::now();
        let (sha, refname) =
            DispatchLegRunner::publish_candidate(root, &self.remote, &self.ref_prefix)?;

        // Point the slot at this candidate. Re-`Add`ing a live preview
        // re-points its ref and renews the TTL — no re-bind, no port churn —
        // which is exactly what a serial queue wants from a reusable slot.
        //
        // Send the REMOTE-TRACKING name, not the name we pushed to. We publish
        // `refs/heads/lane-candidate/<sha>` on the forge, but the preview daemon
        // resolves refs in ITS OWN clone, where a mirror of
        // `+refs/heads/*:refs/remotes/origin/*` lands that ref at
        // `refs/remotes/origin/lane-candidate/<sha>`. Asking it for the
        // `refs/heads/` name gets `fatal: Needed a single revision`, the poller
        // silently skips, and the slot sits `phase=idle` forever with no error
        // anywhere — verified in production 2026-08-02, where the refs WERE
        // mirrored (6 of them) and the slot still never built.
        let local_ref = format!(
            "refs/remotes/origin/{}",
            refname.trim_start_matches("refs/heads/")
        );
        let body = serde_json::json!({
            "name": self.slot,
            "ref": local_ref,
        })
        .to_string();
        // WAIT for the slot to be free before pointing it.
        //
        // Nothing coordinates the lane with the slot it builds on: this runner
        // used to POST /instances without ever reading the slot's phase, and
        // the daemon accepts a re-point while a build is in flight. Meanwhile
        // an infra-ejected member is auto-requeued the moment its TTL lapses,
        // on a timer that knows nothing about the slot.
        //
        // Observed 2026-08-02 20:07Z: an ejection due to expire in ~3 minutes
        // while the slot had 10-20 minutes of `triform_physics` left, so the
        // readmit was guaranteed to land mid-build. That is not dangerous — it
        // fails infra and backs off — but it burns a generation every time, and
        // it is why the generation counter climbed past 11 with no progress.
        //
        // Waiting is honest here: a busy slot is not an infrastructure FAILURE,
        // it is a queue. Bounded, so a slot wedged `building` forever still
        // surfaces as Infra rather than hanging the lane.
        let free_by = Instant::now() + SLOT_FREE_TIMEOUT;
        loop {
            let busy = match self.app_snapshot() {
                Ok(snap) => slot_is_building(&snap, &self.slot),
                // Cannot read it ⇒ do not block on a guess; the point attempt
                // below has its own retry and will report the real error.
                Err(_) => false,
            };
            if !busy || Instant::now() >= free_by {
                break;
            }
            std::thread::sleep(SLOT_FREE_POLL);
        }

        // RETRY, because the daemon we are pointing at restarts.
        //
        // This one POST decides whether a 20-45 minute candidate build happens
        // at all, and `post.status` is CURL's exit code — a connection refused
        // during a rolling update looks identical to a permanent failure. On
        // 2026-08-02 the preview rolled twice in 90 minutes (a Flux apply, then
        // a second kill) and every candidate that raced it died instantly:
        // generations 8-11 all `could not point preview slot`, each one a
        // fresh ejection and backoff for a member whose code was never at
        // fault.
        //
        // A few seconds of retry converts "the daemon was restarting" from a
        // lost build into a pause. Still bounded: a daemon that is genuinely
        // gone must still surface as Infra rather than hanging the lane.
        self.point_slot(&body, &refname)?;
        let mut last_pointed = Instant::now();

        // Poll until the slot SERVES this sha, or reds on it.
        let deadline = Instant::now() + self.timeout;
        loop {
            if Instant::now() >= deadline {
                // Not a red. We stopped looking; that says nothing about the
                // code, and blaming a member for our deadline would be the
                // false accusation the lane exists to avoid.
                return Err(io::Error::other(format!(
                    "preview slot {:?} did not serve {sha} within {}s",
                    self.slot,
                    self.timeout.as_secs()
                )));
            }
            let snap = self.app_snapshot()?;
            let v: serde_json::Value =
                serde_json::from_str(&snap).unwrap_or(serde_json::Value::Null);
            let inst = v
                .get("instances")
                .and_then(|i| i.as_array())
                .and_then(|a| {
                    a.iter()
                        .find(|x| x.get("name").and_then(|n| n.as_str()) == Some(&self.slot))
                })
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            let field = |k: &str| {
                inst.get(k)
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string()
            };

            if field("serving_sha") == sha {
                let host = field("public_host");
                return Ok(LegOutcome {
                    tree: TreeState::Green,
                    diagnostics: Vec::new(),
                    red_attribution: RedAttribution::Unattributed,
                    artifact: None,
                    legs: vec![LegReport {
                        id: format!(
                            "preview:{} live at {}",
                            self.slot,
                            if host.is_empty() { "<no route>" } else { &host }
                        ),
                        tree: TreeState::Green,
                        required: true,
                        duration_ms: started.elapsed().as_millis(),
                    }],
                });
            }
            if field("last_red_sha") == sha {
                // The reason is free text, but a daemon that publishes
                // `last_red_files` gives this red REAL compiler spans (see
                // below) — that is what lets the lane eject only the member
                // whose change carries the failure instead of holding the
                // whole queue. An older daemon (no field) stays honest:
                // synthetic anchor, unattributed, everyone held.
                let reason = field("last_red_reason");

                // NOT EVERY SLOT RED IS A CODE RED.
                //
                // The slot reports setup failures through the same field as
                // compile failures. On 2026-08-02 the lane slot's worktree
                // directory did not survive a PVC fault, and the next candidate
                // produced:
                //
                //   last_red_reason=git checkout <sha> failed: fatal: cannot
                //   change to '.../app/lane/worktree': No such file or directory
                //
                // The lane called that Red and ejected pr-10394 — a member whose
                // code compiles fine — in TWELVE SECONDS. No tf-multiverse build
                // can go red that fast; the elapsed time alone said it was not a
                // verdict.
                //
                // A checkout/setup failure is OUR problem, not the member's, so
                // it must report Infra: everyone stays queued and the lane
                // retries, instead of a false accusation that also lets the real
                // fault go unnoticed. Same rule as
                // `a_missing_artifact_on_green_is_infrastructure_not_a_red`.
                if reason_is_infrastructure(&reason) {
                    return Err(io::Error::other(format!(
                        "preview slot {:?} could not set up the candidate (not a code verdict): {reason}",
                        self.slot
                    )));
                }

                // NOR IS EVERY COMPILE RED THE CANDIDATE'S FAULT.
                //
                // The candidate is base + members. If the BASE ALONE cannot
                // compile, the candidate inherits that failure and the lane
                // blames whichever member happens to be aboard.
                //
                // Observed 2026-08-03: a commit added a `debug!()` call without
                // adding `debug` to the file's `use tracing::{...}`, so
                // triform-physics stopped compiling on dev for ~90 minutes. The
                // lane ejected FOUR members in that window — including pr-10572,
                // which changes ONE YAML FILE and no Rust at all. A YAML change
                // cannot break a Rust compile; the accusation was impossible on
                // its face and the lane made it anyway.
                //
                // The base slot builds the base with NO candidate merged in, and
                // it is in the SAME `/app` snapshot we already fetched — so this
                // costs one extra map lookup, no request.
                //
                // Compared on the TAIL rather than the whole string: the two
                // builds run at different shas and their reasons differ in the
                // leading path/step detail while ending in the same compiler
                // verdict (`error: could not compile X (lib) due to ...`). The
                // tail is the part that identifies the failure.
                if !self.base_slot.is_empty() {
                    let base = base_red_reason(&v, &self.base_slot);
                    if same_failure(&base, &reason) {
                        return Err(io::Error::other(format!(
                            "base slot {:?} is ALSO red with the same failure — the base does not \
                             compile, so this is not a verdict on the candidate: {reason}",
                            self.base_slot
                        )));
                    }
                }
                // Per-file evidence, when the daemon publishes it. Each path
                // came from an ERROR-severity rustc span in the failing
                // build's own output (`appbuild::error_files`), so it is
                // exactly as trustworthy as a compiler diagnostic — the bar
                // `RedAttribution::ChangedFiles` requires. Absent/empty ⇒ the
                // old contract: one synthetic anchor, explicitly Unattributed
                // (the operator's 613f100 seam), every member held.
                let red_files = red_files_from_instance(&inst);

                let (diagnostics, red_attribution) = if red_files.is_empty() {
                    (
                        vec![cargoless_proto::Diagnostic {
                            // Anchored at the manifest, like every other
                            // build-level failure with no source span. The
                            // explicit Unattributed marking (not the path)
                            // is what keeps this from blaming anyone.
                            file_path: root.join("cargoless.checks.yaml"),
                            line: 1,
                            col: 1,
                            severity: cargoless_proto::Severity::Error,
                            code: Some("preview.red".to_string()),
                            message: format!(
                                "preview slot {:?} rejected the candidate: {reason}",
                                self.slot
                            ),
                            source: Some("cargoless-preview".to_string()),
                        }],
                        RedAttribution::Unattributed,
                    )
                } else {
                    (
                        red_files
                            .iter()
                            .map(|f| cargoless_proto::Diagnostic {
                                // Worktree-relative from the daemon; joined
                                // under the candidate root so member
                                // matching (`LaneMember::touches`, which
                                // compares suffix-wise) sees a real path.
                                file_path: root.join(f),
                                // The daemon publishes files, not spans;
                                // 1:1 is the conventional whole-file anchor
                                // and attribution never reads line/col (the
                                // fingerprint deliberately omits them).
                                line: 1,
                                col: 1,
                                severity: cargoless_proto::Severity::Error,
                                code: Some("preview.red".to_string()),
                                message: format!(
                                    "preview slot {:?} rejected the candidate; \
                                     errors in `{f}`: {reason}",
                                    self.slot
                                ),
                                source: Some("cargoless-preview".to_string()),
                            })
                            .collect(),
                        RedAttribution::ChangedFiles,
                    )
                };
                return Ok(LegOutcome {
                    tree: TreeState::Red,
                    diagnostics,
                    red_attribution,
                    artifact: None,
                    legs: vec![LegReport {
                        id: format!("preview:{}", self.slot),
                        tree: TreeState::Red,
                        required: true,
                        duration_ms: started.elapsed().as_millis(),
                    }],
                });
            }
            // An acknowledged POST is not durable proof that the replacement
            // daemon retained it. On 2026-08-03 the point for cace69c0 was
            // acknowledged 11 seconds before a preview restart; the new
            // process restored the prior registry and the lane waited forever
            // for a candidate `/app` could no longer name. Reassert only after
            // a grace interval and only when this readable snapshot proves the
            // slot is quiescent or absent. An active build is never clobbered.
            if last_pointed.elapsed() >= POINT_REASSERT_INTERVAL
                && slot_needs_repoint(&snap, &self.slot, &sha)
            {
                self.point_slot(&body, &refname)?;
                last_pointed = Instant::now();
            }
            thread::sleep(self.poll);
        }
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

/// Same reason as the boxed [`LegRunner`]: `LaneHost::spawn` is generic over
/// BOTH, so choosing each at boot would otherwise mean one `spawn` body per
/// (runner × lander) pair. Three landers and three runners is nine.
///
/// Landing happens once per green build, so a vtable hop is free.
impl LaneLander for Box<dyn LaneLander + Send> {
    fn land(&self, members: &[LaneMember], artifact: Option<&str>) -> io::Result<LandOutcome> {
        (**self).land(members, artifact)
    }
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

/// Hands a green candidate to an external lander command — the auto-merge step.
///
/// # Why a command and not an implementation
///
/// Landing on a forge is not one API call. tf-multiverse's
/// `scripts/merge-train-controller` already does it, and every part was earned:
///
/// * a **k8s Lease** for mutual exclusion (`replicas: 1` is not a lock — a
///   rolling update starts the new pod before the old one exits)
/// * **ONE** `EXIT` trap doing worktree removal, scratch cleanup and lease
///   release, because traps REPLACE rather than append and a second one would
///   silently drop the release, wedging the lane for a full TTL
/// * `git push --force-with-lease=<branch>:<base>` — git's own ref comparison
///   IS the compare-and-swap that makes concurrent landing safe
/// * per-member `POST /pulls/N/merge {Do:"manually-merged"}`, with the queue
///   retraction ordered BEFORE it
///
/// Re-implementing that here would be a second copy of a security- and
/// correctness-critical path to keep in sync, and this lane has already learned
/// what re-deriving solved problems costs. So cargoless stays forge-agnostic:
/// it runs a command, exactly as [`DispatchLegRunner`] does for building.
///
/// # Contract
///
/// The command receives `CARGOLESS_LANE_MEMBERS` (newline-separated
/// `<id>\t<head>`) and `CARGOLESS_LANE_ARTIFACT` when there is one. Exit 0 means
/// landed. **Non-zero is an error, not a silent failure** — the driver
/// re-enqueues every member so a lost race is retried rather than dropped,
/// which matters because these members are GREEN and their work would otherwise
/// vanish looking like nothing happened.
pub struct CommandLander {
    pub command: Vec<String>,
    pub timeout: Duration,
}

/// Default budget for publishing an already-green candidate.
///
/// The original tf-multiverse delegate rebuilt the candidate during landing,
/// so this budget was raised to two hours after a 600s parent repeatedly killed
/// healthy nested builds. The exact-tree contract removed that second build:
/// the dispatcher returns the green candidate SHA and the lander now verifies
/// that same SHA, performs one compare-and-swap push, and reconciles its PRs.
/// Keeping the old two-hour ceiling would therefore hide a wedged forge or
/// broken lander long after there is useful evidence to report.
///
/// Thirty minutes still covers the deliberately pessimistic bound for ten
/// members when every forge call consumes its full 30s timeout, plus fetch,
/// verification, CAS, and reconciliation headroom. Override with
/// `CARGOLESS_LANE_LAND_TIMEOUT_SECS` for another forge shape; no build timeout
/// belongs inside this budget because landing must never rebuild.
const LAND_TIMEOUT_DEFAULT_SECS: u64 = 1800;

impl CommandLander {
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            timeout: Duration::from_secs(land_timeout_secs()),
        }
    }
}

/// Resolve the land budget from the environment, falling back to
/// [`LAND_TIMEOUT_DEFAULT_SECS`].
fn land_timeout_secs() -> u64 {
    parse_land_timeout(
        std::env::var("CARGOLESS_LANE_LAND_TIMEOUT_SECS")
            .ok()
            .as_deref(),
    )
}

/// The parsing half of [`land_timeout_secs`], split out so it can be tested
/// without touching the process environment.
///
/// Not fastidiousness: `set_var` is `unsafe` in Edition 2024, and a test that
/// mutates a global does it for every other test running in parallel. This
/// crate already carries a known env-lock flake from exactly that
/// (`CGLS-26` warm-target, two tests failing together through a poisoned
/// mutex). A pure function takes the whole class off the table.
///
/// Unparseable or zero reads as "unset". Zero especially: it would mean a
/// deadline already in the past, so every land would be killed before its first
/// poll — a typo silently disabling landing, which is precisely the class of
/// failure this constant exists to end.
fn parse_land_timeout(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(LAND_TIMEOUT_DEFAULT_SECS)
}

impl LaneLander for CommandLander {
    fn land(&self, members: &[LaneMember], artifact: Option<&str>) -> io::Result<LandOutcome> {
        let Some(program) = self.command.first() else {
            return Err(io::Error::other(
                "command lander configured with an empty command",
            ));
        };
        let roster = members
            .iter()
            .map(|m| format!("{}\t{}", m.id, m.head))
            .collect::<Vec<_>>()
            .join("\n");

        let mut cmd = Command::new(program);
        cmd.args(&self.command[1..])
            .env("CARGOLESS_LANE_MEMBERS", &roster)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(a) = artifact {
            cmd.env("CARGOLESS_LANE_ARTIFACT", a);
        }

        let (success, _code, text, _ms) = DispatchLegRunner::run_to_completion(cmd, self.timeout)?;
        if !success {
            // Err, never Ok-with-a-sad-message: the driver's LandAndPublish arm
            // re-enqueues on Err, and these members are green. Reporting a
            // failed land as success would drop verified work silently.
            return Err(io::Error::other(format!(
                "lander command failed: {}",
                tail_lines(&text, 12)
            )));
        }
        let ids: Vec<&str> = members.iter().map(|m| m.id.as_str()).collect();
        Ok(LandOutcome {
            detail: format!(
                "landed {} member(s) via `{}`: {}",
                ids.len(),
                program,
                ids.join(", ")
            ),
        })
    }
}

/// Last `n` non-blank lines — enough context to diagnose without pasting a
/// whole build log into a per-member report.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

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

/// What the driver is doing RIGHT NOW, between two state transitions.
///
/// The lane's own [`LanePhase`](crate::lane::LanePhase) answers "is a build in
/// flight". It cannot answer "is the trunk being moved", because by the time
/// [`LaneAction::LandAndPublish`] is executed the lane has already taken its
/// members out of `in_flight` and returned to `Idle` — the green verdict is in,
/// and as far as the state machine is concerned the build is over. The land
/// itself then runs for up to the land budget (7200s by default) with the lane
/// reporting `idle`.
///
/// That is the single most destructive moment to roll the daemon, and a
/// snapshot that says `idle` actively invites it. So the driver announces what
/// it is about to block on, and the host renders it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LaneActivity {
    /// Nothing blocking is in flight; the lane's own phase is the whole truth.
    #[default]
    Settled,
    /// Inside [`CandidateTree::materialize`] + [`LegRunner::run`]. Tens of
    /// minutes for a real lane.
    ///
    /// Carries nothing: the lane's own `phase`, `generation` and `in_flight`
    /// already describe a running build completely. This variant exists only so
    /// the blocking window is *bracketed*, which is what the land case needs.
    Building,
    /// Inside [`LaneLander::land`] — the trunk is being moved. Carries the
    /// roster because the lane's `in_flight` is already empty here, and "who is
    /// landing" is exactly what an author polling `GET /lane` needs.
    Landing { members: Vec<LaneMember> },
}

impl LaneActivity {
    fn of(action: &LaneAction) -> Self {
        match action {
            LaneAction::StartBuild { .. } => Self::Building,
            LaneAction::LandAndPublish { members, .. } => Self::Landing {
                members: members.clone(),
            },
            // Pure notifications — they return immediately, so there is no
            // window during which anyone could read a stale phase.
            LaneAction::Eject { .. } | LaneAction::Readmit { .. } | LaneAction::Report { .. } => {
                Self::Settled
            }
        }
    }

    /// Does executing this take real, observable time?
    fn is_blocking(&self) -> bool {
        !matches!(self, Self::Settled)
    }
}

/// Reads the wall clock **in the same unit the caller ticks the lane with**.
///
/// Boxed rather than a bare `fn` pointer because the only correct
/// implementation captures state: [`LaneHost`](crate::lanehost::LaneHost)
/// derives it from the tick stream it is actually given, so the driver never
/// has to assume the caller counts in Unix seconds. `Send` because the driver
/// is moved onto the lane worker thread.
pub type LaneClock = Box<dyn Fn() -> u64 + Send>;

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
    /// Reads the wall clock so the lane's own clock can be re-synced across a
    /// blocking action. `None` = the lane's clock moves only on the host's
    /// [`LaneEvent::Tick`]s, which is what every pure unit test wants.
    ///
    /// # Why this is not optional in production
    ///
    /// `LaneState::now` advances ONLY on `Tick`, and the host's ticks sit
    /// unread in a channel for the whole of a blocking action — the worker is
    /// inside `execute`. So every deadline an outcome computes is measured from
    /// the moment the action STARTED, not the moment it failed.
    ///
    /// For the infra backoff that is fatal. A failing preview point takes ~35s
    /// (5 attempts, 6s apart) and the backoff is 30 ticks, so
    /// `infra_retry_after = started + 30` is already in the past when the
    /// failure is recorded: the drained tick backlog clears it on the first
    /// tick and the next candidate starts immediately. Observed as a generation
    /// roughly every 30s against an unreachable preview daemon, 51 of them in
    /// one night, each writing an `outcome=infra` trail line and none of them
    /// waiting.
    ///
    /// The land path is starker still: `CommandLander`'s default budget is
    /// 7200s, so a land that times out records a 30-tick backoff measured from
    /// two hours ago. That backoff can never delay anything.
    ///
    /// Re-syncing before the outcome is applied is what makes
    /// `infra_backoff_ticks` mean what it says.
    pub clock: Option<LaneClock>,
    /// Append one line per leg and one per build outcome here. `None` = no
    /// trail (the default, and what every unit test wants).
    ///
    /// This exists because the first real shadow run compiled for 76 minutes
    /// and reported its verdict NOWHERE. `GET /lane` shows only current state,
    /// and `CandidateTree::release` removes the worktree — and its target dir —
    /// the moment the build ends, so afterwards there was no way to tell green
    /// from red from inside the pod. A build whose result is unreadable ten
    /// minutes later cannot be compared against anything, which defeats the
    /// entire purpose of shadowing it.
    ///
    /// Shape deliberately copied from tf-multiverse's
    /// `scripts/ci/_witness_leg_obs.sh`, which solved this for the witness
    /// tier: one `[cargoless:obs]` line per leg, greppable, appended. Same
    /// vocabulary means an operator already knows how to read it.
    pub trail: Option<PathBuf>,
}

impl<T: CandidateTree, R: LegRunner, L: LaneLander> LaneDriver<T, R, L> {
    pub fn new(tree: T, legs: R, lander: L) -> Self {
        Self {
            tree,
            legs,
            lander,
            clock: None,
            trail: None,
        }
    }

    /// Record every leg and every build outcome to `path`.
    #[must_use]
    pub fn with_trail(mut self, path: impl Into<PathBuf>) -> Self {
        self.trail = Some(path.into());
        self
    }

    /// Re-sync the lane's clock from `clock` across every blocking action. See
    /// [`Self::clock`] — without it the infra backoff is measured from before
    /// the failure and cannot pace anything.
    ///
    /// [`LaneHost`](crate::lanehost::LaneHost) installs one automatically from
    /// its own tick stream, so this is for a caller driving the driver directly.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + 'static) -> Self {
        self.clock = Some(Box::new(clock));
        self
    }

    /// Append one line. Best-effort by contract: a trail is evidence ABOUT a
    /// build, never a precondition for one. A full disk or an unwritable state
    /// dir must not turn a green candidate into a failure — that would make the
    /// observability the outage.
    fn trail_line(&self, line: &str) {
        let Some(path) = self.trail.as_deref() else {
            return;
        };
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }

    fn record_legs(&self, generation: u64, legs: &[LegReport]) {
        for leg in legs {
            self.trail_line(&format!(
                "[cargoless:obs] lane-leg generation={generation} id={} tree={:?} \
                 required={} elapsed_ms={}",
                leg.id, leg.tree, leg.required, leg.duration_ms
            ));
        }
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
                // Announce the land BEFORE blocking on it, the same way
                // `run_build` writes `lane-build-start` before compiling.
                //
                // Same defect as the `idle` snapshot, in the durable channel:
                // without this the trail reads `outcome=green` and then nothing
                // for up to two hours, so a daemon killed mid-land leaves a
                // record indistinguishable from one that never tried to land.
                // The live snapshot now says `landing`, but a snapshot dies
                // with the pod and the trail is what is left afterwards.
                self.trail_line(&format!(
                    "[cargoless:obs] lane-land-start members={}",
                    members
                        .iter()
                        .map(|m| format!("{}@{}", m.id, m.head))
                        .collect::<Vec<_>>()
                        .join(",")
                ));
                match self.lander.land(members, artifact.as_deref()) {
                    Ok(o) => {
                        // A land is the only step that moves the trunk, so it
                        // belongs in the durable trail beside the verdict that
                        // authorised it — not only in the process log, which
                        // dies with the pod.
                        self.trail_line(&format!(
                            "[cargoless:obs] lane-land outcome=landed members={} detail={}",
                            members.len(),
                            o.detail
                        ));
                        Vec::new()
                    }
                    // A lander failure is infrastructure by construction: the
                    // build was GREEN, so nobody's code is at fault. The usual
                    // cause is the base moving under us and the forge's
                    // compare-and-swap rejecting the push.
                    //
                    // Re-enqueue every member so the next build re-merges them
                    // against the new base. Silently dropping them here would
                    // lose green work to a push race — the worst outcome
                    // available, because it looks like nothing happened.
                    //
                    // `LandFailed` goes FIRST so the backoff is already pending
                    // when the members arrive. Every `Enqueue` ends in
                    // `maybe_start_build`, and with a zero capture window the
                    // very first one would otherwise start the next build
                    // before any backoff existed — the hot loop this exists to
                    // stop. Setting the timer first means `maybe_start_build`
                    // returns early on every one of them.
                    Err(e) => {
                        // Write the reason to the TRAIL, not just the log.
                        //
                        // Without this line the trail reads `outcome=green`
                        // immediately followed by `lane-build-start` for the
                        // same members, with nothing in between — a green
                        // candidate silently rebuilding forever, which is
                        // indistinguishable from a lane that never tried to
                        // land at all. That gap hid a 600s lander timeout for a
                        // full day on 2026-08-02: five greens, five kills, and
                        // the only evidence was Forgejo status timestamps.
                        //
                        // The trail exists so a verdict outlives the tree; a
                        // failed land is a verdict about the trunk and has the
                        // same claim on it.
                        let reason = e.to_string();
                        self.trail_line(&format!(
                            "[cargoless:obs] lane-land outcome=failed members={} reason={reason}",
                            members.len()
                        ));
                        let mut evs = vec![LaneEvent::LandFailed {
                            reason,
                            members: members.iter().map(|m| m.id.clone()).collect(),
                        }];
                        evs.extend(members.iter().cloned().map(LaneEvent::Enqueue));
                        evs
                    }
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

        self.trail_line(&format!(
            "[cargoless:obs] lane-build-start generation={generation} members={}",
            members
                .iter()
                .map(|m| format!("{}@{}", m.id, m.head))
                .collect::<Vec<_>>()
                .join(",")
        ));

        let root = match self.tree.materialize(members) {
            Ok(r) => r,
            // A member that cannot be merged is ejected by name. Nothing was
            // compiled, but git already told us whose fault it is — no
            // inference, no ambiguity — and the rest of the queue must not wait
            // behind it.
            Err(MaterializeError::Conflict {
                id,
                files,
                shared_with,
                reason,
            }) => {
                self.trail_line(&format!(
                    "[cargoless:obs] lane-build generation={generation} outcome=conflict \
                     member={id} files={} shared_with={} reason={reason}",
                    files.len(),
                    shared_with.join(",")
                ));
                return LaneEvent::BuildFinished {
                    generation,
                    outcome: LaneBuildOutcome::Conflict {
                        id,
                        files,
                        shared_with,
                        reason,
                    },
                };
            }
            // The member landed while it sat in the queue. This is deliberately
            // NOT Conflict-shaped: the public cause is `already_landed`, so an
            // author is never told to resolve a collision that does not exist.
            Err(MaterializeError::Stale { id, head }) => {
                self.trail_line(&format!(
                    "[cargoless:obs] lane-build generation={generation} outcome=stale \
                     member={id} head={head}"
                ));
                return LaneEvent::BuildFinished {
                    generation,
                    outcome: LaneBuildOutcome::Stale { id, head },
                };
            }
            Err(MaterializeError::Infra(e)) => {
                let reason = format!("candidate tree could not be materialized: {e}");
                self.trail_line(&format!(
                    "[cargoless:obs] lane-build generation={generation} outcome=infra \
                     reason={reason}"
                ));
                return LaneEvent::BuildFinished {
                    generation,
                    outcome: LaneBuildOutcome::Infra { reason },
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
                tree,
                diagnostics,
                legs,
                ..
            }) if tree == TreeState::Red && diagnostics.is_empty() => {
                // Record the legs even here. THIS is the case where per-leg
                // evidence matters most: the build says red and cannot say
                // why, so "which leg was red, and for how long" is the only
                // thread an operator has to pull.
                self.record_legs(generation, &legs);
                LaneBuildOutcome::Infra {
                    reason: "build reported red with no diagnostics — cannot attribute; \
                             treating as infrastructure rather than blaming a member"
                        .to_string(),
                }
            }
            Ok(LegOutcome {
                tree,
                diagnostics,
                red_attribution,
                artifact,
                legs,
            }) => {
                self.record_legs(generation, &legs);
                match tree {
                    TreeState::Green => LaneBuildOutcome::Green { artifact },
                    TreeState::Red => match red_attribution {
                        RedAttribution::ChangedFiles => LaneBuildOutcome::Red { diagnostics },
                        RedAttribution::Unattributed => {
                            LaneBuildOutcome::UnattributedRed { diagnostics }
                        }
                    },
                }
            }
            Err(e) => {
                let roster_stale = e
                    .get_ref()
                    .and_then(|source| source.downcast_ref::<RosterStaleError>());
                match roster_stale {
                    Some(stale) if members.iter().any(|m| m.id == stale.id) => {
                        LaneBuildOutcome::RosterStale {
                            id: stale.id.clone(),
                        }
                    }
                    Some(stale) => LaneBuildOutcome::Infra {
                        reason: format!(
                            "build dispatcher named unknown stale roster member `{}`",
                            stale.id
                        ),
                    },
                    None => LaneBuildOutcome::Infra {
                        reason: format!("build legs could not run: {e}"),
                    },
                }
            }
        };

        // One summary line per build, written BEFORE `release` destroys the
        // candidate worktree. The verdict has to outlive the tree it was
        // computed from — that is the whole point.
        match &outcome {
            LaneBuildOutcome::Green { artifact } => self.trail_line(&format!(
                "[cargoless:obs] lane-build generation={generation} outcome=green artifact={}",
                artifact.as_deref().unwrap_or("<none>")
            )),
            LaneBuildOutcome::Red { diagnostics } => self.trail_line(&format!(
                "[cargoless:obs] lane-build generation={generation} outcome=red diagnostics={}",
                diagnostics.len()
            )),
            LaneBuildOutcome::UnattributedRed { diagnostics } => self.trail_line(&format!(
                "[cargoless:obs] lane-build generation={generation} \
                     outcome=red-unattributed diagnostics={}",
                diagnostics.len()
            )),
            LaneBuildOutcome::Infra { reason } => self.trail_line(&format!(
                "[cargoless:obs] lane-build generation={generation} outcome=infra reason={reason}"
            )),
            // Not reachable here: a conflict is detected while materialising,
            // which returns early and writes its own `outcome=conflict` line
            // above. Written as a real arm rather than a wildcard so that if a
            // future path ever produces a conflict *after* materialisation, it
            // still reaches the trail instead of being silently swallowed by a
            // `_ => {}`. The verdict outliving the tree is the point.
            LaneBuildOutcome::Conflict {
                id,
                files,
                shared_with,
                reason,
            } => self.trail_line(&format!(
                "[cargoless:obs] lane-build generation={generation} outcome=conflict \
                 member={id} files={} shared_with={} reason={reason}",
                files.len(),
                shared_with.join(",")
            )),
            LaneBuildOutcome::Stale { id, head } => self.trail_line(&format!(
                "[cargoless:obs] lane-build generation={generation} outcome=stale \
                 member={id} head={head}"
            )),
            LaneBuildOutcome::RosterStale { id } => self.trail_line(&format!(
                "[cargoless:obs] lane-build generation={generation} outcome=roster-stale \
                 member={id}"
            )),
        }

        self.tree.release(&root);
        LaneEvent::BuildFinished {
            generation,
            outcome,
        }
    }

    /// Drive the lane until it is quiet, collecting every action for the caller
    /// to report. Bounded so a policy bug cannot spin forever.
    pub fn pump(&self, lane: &mut LaneState, event: LaneEvent) -> Vec<LaneAction> {
        self.pump_observed(lane, event, |_, _| {})
    }

    /// [`Self::pump`] with a callback fired after every state transition AND
    /// around every blocking action, carrying what the driver is about to do.
    ///
    /// The reason this exists: `execute` runs the BUILD, which for a real lane
    /// is tens of minutes, and the transition that flips the phase to
    /// `Building` happens on the line above it. A host that only observes the
    /// lane after `pump` returns therefore reports `idle` for the entire
    /// duration of every build — exactly the window `GET /lane` exists to
    /// explain. An author whose change stopped moving would look, see "idle",
    /// and reasonably conclude the lane never received their submission.
    ///
    /// # Why a per-STEP callback was not enough
    ///
    /// That reasoning was applied to the build and stopped there, and the LAND
    /// has the identical shape with a worse consequence. By the time
    /// [`LaneAction::LandAndPublish`] is executed the lane has already emptied
    /// `in_flight` and returned to `Idle` — so the snapshot published for that
    /// step says `idle`, and it stays published for however long the lander
    /// takes (up to the 7200s land budget, and a real lander delegates to a
    /// merge-train controller that waits on its own candidate build).
    ///
    /// A snapshot reading `idle` while the trunk is being moved is not merely
    /// unhelpful: it is the single most destructive moment to roll the daemon,
    /// and the snapshot actively invites it. So the activity is reported around
    /// every blocking action, not only at transitions.
    ///
    /// The callback runs on the pump thread and must not block; it is for
    /// publishing a snapshot, not for work.
    pub fn pump_observed(
        &self,
        lane: &mut LaneState,
        event: LaneEvent,
        mut observe: impl FnMut(&LaneState, &LaneActivity),
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
            observe(lane, &LaneActivity::Settled);
            for a in &actions {
                let activity = LaneActivity::of(a);
                if activity.is_blocking() {
                    observe(lane, &activity);
                }
                let followups = self.execute(a);
                if activity.is_blocking() {
                    // RE-SYNC THE CLOCK BEFORE THE OUTCOME IS APPLIED.
                    //
                    // `LaneState::now` only moves on `LaneEvent::Tick`, and the
                    // host's ticks sat unread in a channel for the whole of the
                    // action we just ran — the worker was in here. So without
                    // this, the follow-up event below computes every deadline
                    // from the clock as it stood when the action STARTED.
                    //
                    // For the infra backoff that is not a rounding error, it is
                    // the whole quantity: a failing preview point takes ~24-35s
                    // (5 attempts, 6s apart) against a 30-tick backoff, so
                    // `infra_retry_after = started + 30` is already in the past
                    // when the failure is recorded and the drained tick backlog
                    // clears it on arrival. The lane then retried as fast as the
                    // failure returned — a generation roughly every 30s against
                    // an unreachable preview daemon, each one writing an
                    // `outcome=infra` trail line, and none of them waiting.
                    //
                    // Advanced directly rather than by feeding a `Tick`: a Tick
                    // also runs `maybe_start_build`, which after a failed LAND
                    // (phase is already `Idle` there) could start the next build
                    // BEFORE `LandFailed` installs the backoff — reintroducing
                    // the very hot loop that event exists to prevent.
                    if let Some(clock) = self.clock.as_ref() {
                        lane.advance_clock(clock());
                    }
                    // And publish `Settled` again: for a successful land
                    // `execute` returns NO follow-up event, so without this the
                    // snapshot would stay `landing` until the next unrelated
                    // event arrived.
                    observe(lane, &LaneActivity::Settled);
                }
                pending.extend(followups);
            }
            all.extend(actions);
        }
        all
    }
}

/// Seconds since the Unix epoch — the unit the daemon ticks the lane with.
///
/// Monotonic enough for windows measured in tens of seconds, and `LaneState`
/// clamps its clock forward, so a wall-clock step backwards cannot rewind the
/// lane.
#[must_use]
pub fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod slot_free_tests {
    use super::{reason_is_infrastructure, slot_is_building, slot_needs_repoint};

    /// The 2026-08-02 incident: the slot's worktree vanished after a PVC fault
    /// and the lane blamed the PR. A setup failure is OURS.
    #[test]
    fn a_missing_worktree_is_infrastructure_not_a_code_red() {
        let real = "git checkout 8b6af9d3fa0dfb6acb61fc98358c46ab6e3eb3f7 failed: \
                    fatal: cannot change to '/workspace/cargoless-state/app/lane/worktree': \
                    No such file or directory";
        assert!(
            reason_is_infrastructure(real),
            "the exact reason that ejected an innocent PR must read as infrastructure"
        );
        for r in [
            "could not create worktree",
            "git worktree add failed: already exists",
            "No space left on device",
            "could not fetch origin",
        ] {
            assert!(reason_is_infrastructure(r), "setup failure: {r:?}");
        }
    }

    /// FAILS TOWARD RED. Calling a real compile failure "infrastructure" would
    /// keep a broken candidate queued until it landed — worse than a false
    /// accusation, which at least stops the merge.
    #[test]
    fn a_real_build_failure_stays_red() {
        for r in [
            "build step `server` exited 101",
            "error[E0308]: mismatched types",
            "cannot find function `foo` in this scope",
            "test failed: assertion left == right",
            "health probe returned 500",
            "respawn of previously-green bundle failed health probe: no 200 on /health within 120000ms",
            "",
        ] {
            assert!(
                !reason_is_infrastructure(r),
                "must stay RED so a broken candidate cannot ride through: {r:?}"
            );
        }
    }

    /// The whole point: a slot mid-build must read BUSY so the lane waits
    /// instead of pointing into it and burning a generation.
    #[test]
    fn a_building_slot_reads_busy() {
        let snap = r#"{"instances":[{"name":"lane","phase":"building"}]}"#;
        assert!(
            slot_is_building(snap, "lane"),
            "a slot compiling a candidate must read busy"
        );
        for phase in ["queued", "probing", "probing+serving"] {
            let s = format!(r#"{{"instances":[{{"name":"lane","phase":"{phase}"}}]}}"#);
            assert!(
                slot_is_building(&s, "lane"),
                "`{phase}` means work is in flight and must read busy"
            );
        }
    }

    /// A free slot must NOT read busy, or the lane waits out the full timeout
    /// on every single candidate — worse than the churn this replaces.
    #[test]
    fn an_idle_or_serving_slot_is_free() {
        for phase in ["idle", "serving"] {
            let s = format!(r#"{{"instances":[{{"name":"lane","phase":"{phase}"}}]}}"#);
            assert!(
                !slot_is_building(&s, "lane"),
                "`{phase}` is not work in flight; the lane must proceed"
            );
        }
    }

    /// FAILS OPEN. An unreadable snapshot must never wedge the lane waiting for
    /// a slot it cannot see — the point attempt has its own retry and reports
    /// the real error.
    #[test]
    fn an_unreadable_snapshot_never_blocks() {
        for snap in [
            "",
            "not json",
            "{}",
            r#"{"instances":[]}"#,
            r#"{"instances":[{"name":"other","phase":"building"}]}"#,
            r#"{"instances":[{"name":"lane"}]}"#,
        ] {
            assert!(
                !slot_is_building(snap, "lane"),
                "a snapshot we cannot read must not block: {snap:?}"
            );
        }
    }

    /// Only OUR slot's phase counts. Another slot compiling is none of our
    /// business, and treating it as busy would serialise unrelated lanes.
    #[test]
    fn another_slots_build_does_not_block_us() {
        let snap = r#"{"instances":[
            {"name":"dev","phase":"building"},
            {"name":"lane","phase":"idle"}
        ]}"#;
        assert!(
            !slot_is_building(snap, "lane"),
            "dev building must not make the lane wait"
        );
    }

    /// Exact 2026-08-03 restart race: cace69c0 was acknowledged, then the
    /// replacement daemon restored an older idle/red snapshot. The slot no
    /// longer contained any identity for the candidate, so waiting could never
    /// produce a verdict without reasserting the point.
    #[test]
    fn an_acknowledged_point_lost_across_restart_is_reasserted() {
        let snap = r#"{"instances":[{
            "name":"lane",
            "phase":"idle",
            "serving_sha":null,
            "pending_sha":null,
            "last_red_sha":"9717d8a0"
        }]}"#;
        assert!(slot_needs_repoint(snap, "lane", "cace69c0"));
    }

    #[test]
    fn a_missing_slot_in_a_readable_snapshot_is_reasserted() {
        assert!(slot_needs_repoint(
            r#"{"instances":[{"name":"dev","phase":"building"}]}"#,
            "lane",
            "candidate"
        ));
    }

    #[test]
    fn a_quiescent_slot_serving_an_older_candidate_is_reasserted() {
        let snap = r#"{"instances":[{
            "name":"lane",
            "phase":"serving",
            "serving_sha":"older",
            "pending_sha":null
        }]}"#;
        assert!(slot_needs_repoint(snap, "lane", "candidate"));
    }

    #[test]
    fn candidate_identity_prevents_duplicate_repointing() {
        for (field, phase) in [
            ("pending_sha", "building"),
            ("serving_sha", "serving"),
            ("last_red_sha", "idle"),
        ] {
            let snap = format!(
                r#"{{"instances":[{{"name":"lane","phase":"{phase}","{field}":"candidate"}}]}}"#
            );
            assert!(
                !slot_needs_repoint(&snap, "lane", "candidate"),
                "{field} already names the candidate"
            );
        }
    }

    #[test]
    fn another_active_build_is_never_interrupted() {
        for phase in ["building", "queued", "probing", "probing+serving"] {
            let snap = format!(
                r#"{{"instances":[{{"name":"lane","phase":"{phase}","pending_sha":"other"}}]}}"#
            );
            assert!(
                !slot_needs_repoint(&snap, "lane", "candidate"),
                "active phase {phase} must not be clobbered"
            );
        }
    }

    #[test]
    fn unreadable_or_schema_less_snapshots_are_not_evidence_of_loss() {
        for snap in ["", "not json", "{}", r#"{"instances":"unknown"}"#] {
            assert!(!slot_needs_repoint(snap, "lane", "candidate"));
        }
    }
}

#[cfg(test)]
mod materialize_context_tests {
    use super::MaterializeError;
    use std::io;
    use std::path::Path;

    /// DEFECT 3 — an infra materialize failure must NAME the path and the step.
    ///
    /// The trail line observed on generation 13 was, in full:
    ///
    /// ```text
    /// lane-build generation=13 outcome=infra reason=candidate tree could not
    /// be materialized: No such file or directory (os error 2)
    /// ```
    ///
    /// Which path? Which step? The candidate root, the scratch parent, the repo
    /// and the `git` binary all produce exactly those bytes, and the two
    /// filesystem calls in `materialize` have OPPOSITE fixes — a failed remove
    /// means something is holding the old candidate, a failed create means the
    /// state dir is gone or unwritable. A verdict nobody can act on is the same
    /// as no verdict.
    #[test]
    fn an_infra_materialize_error_names_the_path_and_the_operation() {
        let bare = io::Error::new(
            io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        );
        let e = MaterializeError::infra_at(
            "could not create the candidate scratch directory",
            Path::new("/workspace/cargoless-state/lane-candidates"),
            &bare,
        );
        // Rendered exactly as the driver renders it into the trail.
        let rendered = format!("candidate tree could not be materialized: {e}");

        assert!(
            rendered.contains("/workspace/cargoless-state/lane-candidates"),
            "the PATH must be in the message — without it an operator cannot \
             tell the candidate root from the scratch parent from the repo: \
             {rendered}"
        );
        assert!(
            rendered.contains("could not create the candidate scratch directory"),
            "the OPERATION must be in the message — the two fallible steps have \
             opposite fixes: {rendered}"
        );
        assert!(
            rendered.contains("os error 2"),
            "and the original errno text must survive, or the annotation has \
             traded one missing fact for another: {rendered}"
        );
    }

    /// The bare form is what shipped, and it is what this must never render as
    /// again. Pinned against the real observed string so a future refactor that
    /// drops the context fails here rather than in production.
    #[test]
    fn the_observed_bare_message_is_no_longer_producible_by_infra_at() {
        let bare = io::Error::new(
            io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        );
        let observed = format!(
            "candidate tree could not be materialized: {}",
            MaterializeError::Infra(io::Error::new(bare.kind(), bare.to_string()))
        );
        // The exact production line — proof the test is describing the real bug.
        assert_eq!(
            observed,
            "candidate tree could not be materialized: No such file or directory (os error 2)",
            "this is the string that reached the trail on generation 13"
        );

        let annotated = format!(
            "candidate tree could not be materialized: {}",
            MaterializeError::infra_at(
                "could not remove the stale candidate worktree",
                Path::new("/s/c-1"),
                &bare
            )
        );
        assert_ne!(
            annotated, observed,
            "the annotated form must differ from the bare one"
        );
    }

    /// The `io::ErrorKind` must survive the annotation. Nothing switches on it
    /// today, and a caller that starts to must not find it quietly destroyed by
    /// the code that exists to make the error MORE informative.
    #[test]
    fn annotating_preserves_the_error_kind() {
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::AlreadyExists,
        ] {
            let e = MaterializeError::infra_at("op", Path::new("/p"), &io::Error::new(kind, "x"));
            let MaterializeError::Infra(inner) = e else {
                panic!("infra_at must produce Infra");
            };
            assert_eq!(inner.kind(), kind, "the kind must survive annotation");
        }
    }
}

#[cfg(test)]
mod land_timeout_tests {
    use super::{LAND_TIMEOUT_DEFAULT_SECS, parse_land_timeout};

    #[test]
    fn the_default_bounds_exact_landing_without_build_time() {
        let resolved = parse_land_timeout(None);
        assert_eq!(resolved, 1800);
        assert!(resolved < 5400, "landing must not inherit a build budget");
    }

    #[test]
    fn an_explicit_override_wins() {
        assert_eq!(parse_land_timeout(Some("120")), 120);
        assert_eq!(parse_land_timeout(Some("  9000  ")), 9000);
    }

    /// Every unusable value falls back rather than producing a budget that
    /// cannot work. Zero is the dangerous one: a deadline already in the past
    /// kills the land before its first poll, so a typo would silently disable
    /// landing while looking configured.
    #[test]
    fn unusable_values_fall_back_to_the_default() {
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("0"),
            Some("-1"),
            Some("abc"),
            Some("60s"),
        ] {
            assert_eq!(
                parse_land_timeout(raw),
                LAND_TIMEOUT_DEFAULT_SECS,
                "{raw:?} is not a usable budget and must fall back"
            );
        }
    }
}

#[cfg(test)]
mod base_health_tests {
    use super::{base_red_reason, red_files_from_instance, same_failure};

    /// The exact strings from 2026-08-03. The dev slot (base alone, no PR) and
    /// the lane slot (base + pr-10572, a YAML-ONLY change) ended byte-identical
    /// because the BASE could not compile. The lane ejected the member anyway.
    const REAL_BASE: &str = "build step `server` exited 101: 1587 | ... warning: unused variable: `execution_id` --> physics/src/runtime/executors/app_ops.rs:630:13 error: could not compile `triform-physics` (lib) due to 1 previous error; 63 warnings emitted";
    const REAL_CANDIDATE: &str = "build step `server` exited 101: 2104 | ... warning: value assigned is never read --> physics/src/runtime/executors/lifecycle.rs:533:56 error: could not compile `triform-physics` (lib) due to 1 previous error; 63 warnings emitted";

    #[test]
    fn the_2026_08_03_false_ejection_is_caught() {
        assert!(
            same_failure(REAL_BASE, REAL_CANDIDATE),
            "base and candidate end in the same compiler verdict — this must NOT \
             be attributed to the member"
        );
    }

    /// The leading detail DIFFERS between the two (different line numbers,
    /// different warning bodies) — which is exactly why the comparison is on
    /// the tail and not the whole string.
    #[test]
    fn the_comparison_survives_differing_leading_detail() {
        assert_ne!(REAL_BASE, REAL_CANDIDATE, "fixtures must differ up front");
        assert!(same_failure(REAL_BASE, REAL_CANDIDATE));
    }

    /// A GENUINE member fault must still be attributed. If the base is green
    /// (or fails differently), the candidate's red is its own.
    #[test]
    fn a_real_member_fault_is_still_attributed() {
        let base_green = "";
        assert!(
            !same_failure(base_green, REAL_CANDIDATE),
            "no base red => attribute"
        );

        let base_other = "build step `server` exited 101: error: could not compile `triform-portal` (lib) due to 1 previous error; 2 warnings emitted";
        assert!(
            !same_failure(base_other, REAL_CANDIDATE),
            "a DIFFERENT crate failing on the base does not excuse this candidate"
        );
    }

    /// Empty is never agreement — "I have no evidence" must not read as "they
    /// match", or an unreadable snapshot would suppress every genuine red.
    #[test]
    fn empty_is_never_a_match() {
        assert!(!same_failure("", ""));
        assert!(!same_failure("   ", REAL_CANDIDATE));
        assert!(!same_failure(REAL_CANDIDATE, ""));
    }

    /// Reading the base slot out of a real `/app` shape.
    #[test]
    fn base_red_reason_reads_the_named_slot() {
        let snap: serde_json::Value = serde_json::from_str(
            r#"{"instances":[
                 {"name":"dev","last_red_reason":"boom on the base"},
                 {"name":"lane","last_red_reason":"boom on the candidate"}
               ]}"#,
        )
        .unwrap();
        assert_eq!(base_red_reason(&snap, "dev"), "boom on the base");
        assert_eq!(base_red_reason(&snap, "lane"), "boom on the candidate");
    }

    /// Every "cannot tell" shape yields empty, and empty never suppresses a
    /// verdict. A missing slot, a missing field, or junk must all fail toward
    /// ATTRIBUTING — one false ejection is survivable, letting a broken member
    /// land is not.
    #[test]
    fn an_unreadable_snapshot_never_suppresses_a_red() {
        for raw in [
            r#"{"instances":[]}"#,
            r#"{"instances":[{"name":"lane","last_red_reason":"x"}]}"#,
            r#"{"instances":[{"name":"dev"}]}"#,
            r#"{}"#,
            r#"null"#,
        ] {
            let snap: serde_json::Value = serde_json::from_str(raw).unwrap();
            let base = base_red_reason(&snap, "dev");
            assert!(
                !same_failure(&base, REAL_CANDIDATE),
                "unreadable base ({raw}) must not suppress the candidate's red"
            );
        }
    }

    /// A daemon that publishes `last_red_files` upgrades the preview red to
    /// per-file evidence — the attribution the lane ejects by.
    #[test]
    fn red_files_read_from_the_instance_row() {
        let inst: serde_json::Value = serde_json::from_str(
            r#"{"name":"lane","last_red_reason":"boom",
                "last_red_files":["portal/src/panels/library_panel.rs","physics/src/api/mod.rs"]}"#,
        )
        .unwrap();
        assert_eq!(
            red_files_from_instance(&inst),
            vec![
                "portal/src/panels/library_panel.rs".to_string(),
                "physics/src/api/mod.rs".to_string()
            ]
        );
    }

    /// Every "cannot tell" shape yields EMPTY — which routes the red to
    /// Unattributed (hold everyone), never to a guessed culprit. An OLD daemon
    /// (field absent entirely) is the most important row: rolling the lane
    /// image before the preview image must keep today's behavior, not invent
    /// attribution from nothing.
    #[test]
    fn missing_or_malformed_red_files_yield_empty() {
        for raw in [
            r#"{"name":"lane","last_red_reason":"x"}"#,
            r#"{"name":"lane","last_red_files":null}"#,
            r#"{"name":"lane","last_red_files":"not-an-array"}"#,
            r#"{"name":"lane","last_red_files":[]}"#,
            r#"{"name":"lane","last_red_files":["","   "]}"#,
            r#"{"name":"lane","last_red_files":[42,null]}"#,
        ] {
            let inst: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert!(
                red_files_from_instance(&inst).is_empty(),
                "({raw}) must read as no evidence, not as an accusation"
            );
        }
    }
}

#[cfg(test)]
mod point_retry_budget_tests {
    use super::{POINT_ATTEMPTS, POINT_RETRY_DELAY};
    use std::time::Duration;

    /// The longest preview replacement actually observed, end to end.
    ///
    /// Measured 2026-08-03: 23:34 Terminating -> 23:36 Init:1/2 -> ~23:38
    /// 3/3 Running. Kept as a named constant because the whole point of this
    /// test is that the retry budget must be checked against a MEASUREMENT,
    /// not against an estimate of how long a restart "should" take.
    const OBSERVED_PREVIEW_RESTART: Duration = Duration::from_secs(4 * 60);

    /// The retry budget must outlast a preview pod replacement.
    ///
    /// This exists because the previous budget (5 x 6s = 30s) carried a comment
    /// asserting it "covers the observed gap between a preview pod being killed
    /// and its replacement answering" — and that was wrong by 8x. Nothing
    /// tested it, so the claim survived until a real roll disproved it.
    ///
    /// The gap cannot be engineered away: the preview Deployment is `Recreate`
    /// with `replicas: 1` because its 220Gi workspace PVC is ReadWriteOnce, so
    /// two pods can never overlap and there is no surge to hide behind. Any
    /// manifest edit therefore removes the only backend for minutes, and four
    /// such rolls happened in a single day.
    ///
    /// Falling short does not merely delay a build — it fails the point, which
    /// reds a 20-45 minute candidate and ejects members whose code was never
    /// at fault.
    #[test]
    fn the_point_retry_budget_outlasts_a_preview_pod_replacement() {
        let budget = POINT_RETRY_DELAY * POINT_ATTEMPTS;
        assert!(
            budget >= OBSERVED_PREVIEW_RESTART,
            "point-retry budget {budget:?} is shorter than the observed \
             preview replacement {OBSERVED_PREVIEW_RESTART:?}; a roll would \
             red an in-flight candidate and eject innocent members"
        );
    }

    /// ...but it must still give up. An unbounded retry would hold the lane
    /// open against a daemon that is genuinely gone, which is the failure this
    /// budget's original "small on purpose" instinct was right about.
    #[test]
    fn the_point_retry_budget_still_surfaces_a_dead_daemon() {
        let budget = POINT_RETRY_DELAY * POINT_ATTEMPTS;
        assert!(
            budget <= Duration::from_secs(10 * 60),
            "point-retry budget {budget:?} is so long that an absent daemon \
             stops surfacing as Infra promptly"
        );
    }

    /// The budget must come from PACING, not from a single long sleep or a
    /// tight spin.
    ///
    /// Two degenerate ways to satisfy the "outlasts a replacement" test above
    /// while being useless in practice: one attempt with a 5-minute delay
    /// (never retries — the first curl decides everything, and it runs while
    /// the daemon is provably down), or hundreds of attempts at ~0s (busy-loops
    /// curl against a socket that is not listening, the same hot-retry shape
    /// the infra backoff exists to prevent).
    ///
    /// Asserted as a RELATION between the two constants rather than each
    /// against a literal: `assert!(POINT_ATTEMPTS > 0)` is a comparison of two
    /// constants that the compiler folds away, so it tests nothing and clippy
    /// rejects it under `-D warnings`.
    #[test]
    fn the_point_retry_budget_comes_from_pacing_not_one_long_sleep() {
        let budget = POINT_RETRY_DELAY * POINT_ATTEMPTS;
        assert!(
            POINT_RETRY_DELAY * 3 <= budget,
            "delay {POINT_RETRY_DELAY:?} against budget {budget:?} leaves too \
             few attempts — a retry needs several chances, not one long sleep"
        );
        // Expressed against the budget rather than a bare literal: a
        // const-vs-literal comparison is folded away by the compiler, tests
        // nothing, and trips clippy::assertions_on_constants under -D warnings.
        assert!(
            POINT_RETRY_DELAY * 600 >= budget,
            "delay {POINT_RETRY_DELAY:?} is so small against budget \
             {budget:?} that this is a busy-loop, not a retry"
        );
    }
}

/// Close any generation a previous process left open in the trail.
///
/// `run_build` writes `lane-build-start generation=N` BEFORE it compiles and
/// the matching `lane-build generation=N outcome=…` only after, so a daemon
/// killed mid-build leaves a start with no end. The replacement process starts
/// at generation 0 and never mentions N again, which makes an abandoned build
/// indistinguishable from one still running — the log stops being able to
/// answer "is the lane working?" after the fact.
///
/// Measured 2026-08-03/04 on the shadow lane: 15 orphaned starts across 200,
/// roughly one per pod roll, each a build that burned real minutes and reported
/// nothing.
///
/// Called once at construction, before the lane accepts any work, so every
/// start is terminated by the time the new process writes its own. The
/// invariant this restores: **every `lane-build-start` has a terminal line.**
///
/// Deliberately reads the trail rather than persisting separate state — the
/// trail already records what was in flight, so there is no second file to keep
/// consistent. Same instinct that lets enrollment survive a restart:
/// reconstruct rather than persist.
///
/// Best-effort and infallible by construction. A missing, unreadable or
/// unwritable trail leaves the status quo; observability must never be the
/// thing that stops the lane from starting.
pub fn close_abandoned_generations(trail: &Path) {
    let Ok(existing) = fs::read_to_string(trail) else {
        return; // no trail yet (first boot) or unreadable — nothing to close
    };

    // A generation is open if it started and never reached a terminal line.
    // Insertion order is preserved so the closes are written oldest-first,
    // matching how they would have been written had the process survived.
    let mut open: Vec<u64> = Vec::new();
    for line in existing.lines() {
        if let Some(g) = generation_after(line, "lane-build-start generation=") {
            if !open.contains(&g) {
                open.push(g);
            }
        } else if let Some(g) = generation_after(line, "lane-build generation=") {
            open.retain(|&o| o != g);
        }
    }
    if open.is_empty() {
        return;
    }

    let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(trail) else {
        return;
    };
    for g in open {
        let _ = writeln!(
            f,
            "[cargoless:obs] lane-build generation={g} outcome=abandoned \
             reason=daemon restarted before this build reported"
        );
    }
}

/// Parse `…{prefix}<digits>` out of a trail line.
///
/// Matches on the exact prefix rather than a loose `generation=` search so that
/// `lane-build-start generation=` and `lane-build generation=` stay distinct —
/// the first is a prefix of neither, but a naive contains() would let
/// `lane-leg generation=` and `lane-land …` lines through and close a build
/// that is still running.
fn generation_after(line: &str, prefix: &str) -> Option<u64> {
    let rest = line.split_once(prefix)?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod abandoned_generation_tests {
    use super::{close_abandoned_generations, generation_after};
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("lane-abandoned-{name}-{}.log", std::process::id()));
        let _ = fs::remove_file(&p);
        p
    }

    /// THE INVARIANT: every `lane-build-start` ends up with a terminal line.
    ///
    /// This is the exact shape observed on the shadow lane — a start, then the
    /// NEXT process's start, with nothing in between, because the pod rolled
    /// mid-build.
    #[test]
    fn an_interrupted_build_is_closed_on_the_next_boot() {
        let p = tmp("interrupted");
        fs::write(
            &p,
            "[cargoless:obs] lane-build-start generation=9 members=pr-1@aaa\n",
        )
        .unwrap();

        close_abandoned_generations(&p);

        let out = fs::read_to_string(&p).unwrap();
        assert!(
            out.contains("lane-build generation=9 outcome=abandoned"),
            "generation 9 was left open and must be closed on boot, got:\n{out}"
        );
        let starts = out.matches("lane-build-start generation=").count();
        let terminals = out
            .lines()
            .filter(|l| l.contains("lane-build generation=") && l.contains("outcome="))
            .count();
        assert_eq!(starts, terminals, "every start must have a terminal line");
        let _ = fs::remove_file(&p);
    }

    /// A build that DID report must not be closed twice — that would invent a
    /// second, contradictory verdict for a generation whose real outcome is
    /// already recorded.
    #[test]
    fn a_completed_build_is_left_alone() {
        let p = tmp("completed");
        fs::write(
            &p,
            "[cargoless:obs] lane-build-start generation=4 members=pr-1@aaa\n\
             [cargoless:obs] lane-build generation=4 outcome=green artifact=<none>\n",
        )
        .unwrap();

        close_abandoned_generations(&p);

        let out = fs::read_to_string(&p).unwrap();
        assert!(
            !out.contains("outcome=abandoned"),
            "generation 4 already reported green; it must not be re-closed:\n{out}"
        );
        let _ = fs::remove_file(&p);
    }

    /// The parser must not be fooled by the OTHER lines that carry a
    /// `generation=`. `lane-leg` in particular appears mid-build, and treating
    /// it as terminal would leave a genuinely abandoned build open forever.
    #[test]
    fn only_a_real_terminal_line_closes_a_generation() {
        let p = tmp("legs");
        fs::write(
            &p,
            "[cargoless:obs] lane-build-start generation=7 members=pr-1@aaa\n\
             [cargoless:obs] lane-leg generation=7 id=preview:lane tree=Red required=true elapsed_ms=1\n",
        )
        .unwrap();

        close_abandoned_generations(&p);

        let out = fs::read_to_string(&p).unwrap();
        assert!(
            out.contains("lane-build generation=7 outcome=abandoned"),
            "a lane-leg line is not a verdict; generation 7 must still be closed:\n{out}"
        );
        let _ = fs::remove_file(&p);
    }

    /// Repeated boots must converge. A crash loop would otherwise append one
    /// `abandoned` per restart and bury the real history.
    #[test]
    fn closing_is_idempotent_across_repeated_boots() {
        let p = tmp("idempotent");
        fs::write(
            &p,
            "[cargoless:obs] lane-build-start generation=3 members=pr-1@aaa\n",
        )
        .unwrap();

        close_abandoned_generations(&p);
        close_abandoned_generations(&p);
        close_abandoned_generations(&p);

        let out = fs::read_to_string(&p).unwrap();
        assert_eq!(
            out.matches("outcome=abandoned").count(),
            1,
            "a crash loop must not append one abandoned line per boot:\n{out}"
        );
        let _ = fs::remove_file(&p);
    }

    /// Observability must never be the thing that stops the lane starting.
    #[test]
    fn a_missing_trail_is_not_an_error() {
        let p = tmp("missing");
        let _ = fs::remove_file(&p);
        close_abandoned_generations(&p); // must not panic
        assert!(
            !p.exists(),
            "an absent trail must not be created just to close nothing"
        );
    }

    #[test]
    fn generation_after_matches_the_exact_prefix() {
        assert_eq!(
            generation_after(
                "[x] lane-build-start generation=12 members=y",
                "lane-build-start generation="
            ),
            Some(12)
        );
        // `lane-build generation=` must NOT match a start line.
        assert_eq!(
            generation_after(
                "[x] lane-build-start generation=12 members=y",
                "lane-build generation="
            ),
            None
        );
        assert_eq!(
            generation_after("[x] unrelated line", "lane-build generation="),
            None
        );
    }
}
