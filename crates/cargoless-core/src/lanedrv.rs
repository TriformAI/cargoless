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
    /// Only in-process does. Both remote plans build elsewhere and report
    /// `artifact: None`, so pairing either with a publishing lander would leave
    /// it taking its "green with nothing to publish" branch forever: no error,
    /// no pointer movement, and an operator watching a publishing lane publish
    /// nothing.
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

        let diagnostics = crate::cargodiag::parse_cargo_json(root, &text);
        let tree = if success {
            TreeState::Green
        } else {
            TreeState::Red
        };

        Ok(LegOutcome {
            tree,
            diagnostics,
            // The artifact lives wherever the external builder put it (a
            // registry tag, a CAS handle); the dispatcher reports it, and the
            // lander promotes it. Nothing local to publish.
            artifact: None,
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
/// Small on purpose: this covers a daemon that is RESTARTING (seconds), not one
/// that is gone. A genuinely absent daemon must still surface as Infra quickly
/// rather than hold the lane.
const POINT_ATTEMPTS: u32 = 5;

/// Gap between point attempts. 5 × 6s ≈ 30s of tolerance, which covers the
/// observed gap between a preview pod being killed and its replacement
/// answering, without meaningfully delaying a real failure.
const POINT_RETRY_DELAY: Duration = Duration::from_secs(6);

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
        .any(|field| inst.get(field).and_then(|x| x.as_str()) == Some(sha))
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
                // The preview reports a free-text reason, not cargo JSON, so
                // this red carries no file paths and the lane will correctly
                // decline to attribute it. That is honest: a boot failure is
                // frequently an interaction, not one member's line.
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
                return Ok(LegOutcome {
                    tree: TreeState::Red,
                    diagnostics: vec![cargoless_proto::Diagnostic {
                        // Anchored at the manifest, like every other
                        // build-level failure with no source span. Attribution
                        // treats a file nobody touched as unattributable, which
                        // is the correct outcome here.
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

/// Default land budget. Deliberately large, and that needs justifying because
/// the number it replaces was chosen for a good reason that turned out to
/// describe a different lander than the one we run.
///
/// "Landing is a push plus N PR reconciles — seconds, not minutes" is true of a
/// lander that lands *itself*. The lander actually configured on tf-multiverse
/// is `scripts/ci/lane-land.sh`, which delegates to
/// `scripts/merge-train-controller --land` — and the controller does not just
/// push. It re-derives the candidate, publishes a merge-train ref, DISPATCHES A
/// CANDIDATE BUILD and waits for the verdict, under its own
/// `TRAIN_BUILD_MAX_WAIT_SECS` (default 5400).
///
/// A parent budget below the delegate's own ceiling cannot ever observe an
/// outcome: it SIGKILLs a healthy land mid-wait and reports infrastructure
/// failure. Measured 2026-08-02 — five green candidates in a row, each killed
/// at exactly 600s and re-enqueued, so the trunk never moved and the trail read
/// `outcome=green` followed by a fresh build with no reason in between.
///
/// So: 7200s, above the delegate's 5400 with room for the forge round-trips
/// that bracket it. This still bounds a hung forge — it does not remove the
/// timeout, it makes it mean what it says. Override with
/// `CARGOLESS_LANE_LAND_TIMEOUT_SECS` when the delegate's budget differs; the
/// rule to preserve is **parent > delegate**, never the reverse.
const LAND_TIMEOUT_DEFAULT_SECS: u64 = 7200;

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
            trail: None,
        }
    }

    /// Record every leg and every build outcome to `path`.
    #[must_use]
    pub fn with_trail(mut self, path: impl Into<PathBuf>) -> Self {
        self.trail = Some(path.into());
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
            Err(MaterializeError::Conflict { id, files, reason }) => {
                self.trail_line(&format!(
                    "[cargoless:obs] lane-build generation={generation} outcome=conflict \
                     member={id} files={} reason={reason}",
                    files.len()
                ));
                return LaneEvent::BuildFinished {
                    generation,
                    outcome: LaneBuildOutcome::Conflict { id, files, reason },
                };
            }
            // The member landed while it sat in the queue. Eject it by name with
            // NO conflicting files, which the state machine already treats as
            // `Unattributed` — the member leaves, everyone else rebuilds without
            // it. Reusing the Conflict outcome rather than adding a parallel one
            // keeps a single ejection path; the trail line says `stale` so the
            // reason is never mistaken for a genuine merge conflict.
            Err(MaterializeError::Stale { id, head }) => {
                let reason = format!("member `{id}` ({head}) already landed before this build");
                self.trail_line(&format!(
                    "[cargoless:obs] lane-build generation={generation} outcome=stale \
                     member={id} head={head}"
                ));
                return LaneEvent::BuildFinished {
                    generation,
                    outcome: LaneBuildOutcome::Conflict {
                        id,
                        files: Vec::new(),
                        reason,
                    },
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
                artifact,
                legs,
            }) => {
                self.record_legs(generation, &legs);
                match tree {
                    TreeState::Green => LaneBuildOutcome::Green { artifact },
                    TreeState::Red => LaneBuildOutcome::Red { diagnostics },
                }
            }
            Err(e) => LaneBuildOutcome::Infra {
                reason: format!("build legs could not run: {e}"),
            },
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
            LaneBuildOutcome::Infra { reason } => self.trail_line(&format!(
                "[cargoless:obs] lane-build generation={generation} outcome=infra reason={reason}"
            )),
            // Not reachable here: a conflict is detected while materialising,
            // which returns early and writes its own `outcome=conflict` line
            // above. Written as a real arm rather than a wildcard so that if a
            // future path ever produces a conflict *after* materialisation, it
            // still reaches the trail instead of being silently swallowed by a
            // `_ => {}`. The verdict outliving the tree is the point.
            LaneBuildOutcome::Conflict { id, files, reason } => self.trail_line(&format!(
                "[cargoless:obs] lane-build generation={generation} outcome=conflict \
                 member={id} files={} reason={reason}",
                files.len()
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
mod land_timeout_tests {
    use super::{parse_land_timeout, LAND_TIMEOUT_DEFAULT_SECS};

    /// The regression that cost a day. The configured lander delegates to
    /// `merge-train-controller --land`, whose own `TRAIN_BUILD_MAX_WAIT_SECS`
    /// defaults to 5400. A parent budget at or below that can never observe an
    /// outcome — it SIGKILLs a healthy land mid-wait and calls it infra.
    ///
    /// Asserted against the delegate's real number, not a copy of our own, so
    /// this fails if someone lowers the default back toward it.
    ///
    /// Asserted on `parse_land_timeout(None)` — the budget the code actually
    /// resolves when unset — rather than on the constant. Two constants compare
    /// at compile time, so `assertions_on_constants` folds the check away and
    /// the test proves nothing at runtime; going through the resolver also
    /// covers the (real) possibility of the default being reachable but the
    /// fallback path not returning it.
    #[test]
    fn the_default_outlives_the_delegates_own_budget() {
        const CONTROLLER_BUILD_MAX_WAIT_SECS: u64 = 5400;
        let resolved = parse_land_timeout(None);
        assert!(
            resolved > CONTROLLER_BUILD_MAX_WAIT_SECS,
            "the land budget ({resolved}s) must exceed the delegate's \
             ({CONTROLLER_BUILD_MAX_WAIT_SECS}s) or every land is killed before it can answer"
        );
    }

    /// Specifically 600s, the value that was there. Named because a comment
    /// explaining why it was wrong is not a test.
    #[test]
    fn the_old_600s_budget_would_still_be_wrong() {
        let resolved = parse_land_timeout(None);
        assert!(
            resolved > 600,
            "600s killed five consecutive green candidates on 2026-08-02"
        );
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
    use super::{base_red_reason, same_failure};

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
}
