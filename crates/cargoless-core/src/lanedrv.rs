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
            } => {
                let what = format!("preview:{slot} daemon={daemon} remote={remote}");
                let mut r = PreviewLegRunner::new(daemon, token, slot, remote);
                r.ref_prefix = ref_prefix;
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
        }
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
}

impl LegRunner for PreviewLegRunner {
    fn run(&self, root: &Path, _changed_files: &[String]) -> io::Result<LegOutcome> {
        let started = Instant::now();
        let (sha, refname) =
            DispatchLegRunner::publish_candidate(root, &self.remote, &self.ref_prefix)?;

        // Point the slot at this candidate. Re-`Add`ing a live preview
        // re-points its ref and renews the TTL — no re-bind, no port churn —
        // which is exactly what a serial queue wants from a reusable slot.
        let body = serde_json::json!({
            "name": self.slot,
            "ref": refname,
        })
        .to_string();
        let post = Command::new("curl")
            .args([
                "-sS",
                "-X",
                "POST",
                "--max-time",
                "30",
                "-H",
                &format!("Authorization: Bearer {}", self.token),
                "-H",
                "Content-Type: application/json",
                "-d",
                &body,
                &format!("{}/instances", self.daemon.trim_end_matches('/')),
            ])
            .output()?;
        if !post.status.success() {
            return Err(io::Error::other(format!(
                "could not point preview slot {:?} at {refname}: {}",
                self.slot,
                String::from_utf8_lossy(&post.stderr).trim()
            )));
        }

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

impl CommandLander {
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            // Landing is a push plus N PR reconciles — seconds, not minutes.
            // A long budget here would hide a hung forge behind a lane that
            // looks busy.
            timeout: Duration::from_secs(600),
        }
    }
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
            Err(e) => {
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
