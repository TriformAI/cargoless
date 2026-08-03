//! `appbuild` — the app-serve build worker: turn a sha into a servable bundle.
//!
//! Runs on the daemon's **detached build worker thread** (inc-5 wires it to
//! the [`crate::appstate`] queue). One call = one build attempt for one
//! instance at one sha:
//!
//! 1. **Checkout** `sha` into the instance worktree (the daemon owns *when*
//!    the tree moves — that is what shrinks the mid-build mutation race).
//! 2. Load `cargoless.app.yaml` from that worktree ([`crate::appmanifest`]) —
//!    the manifest rides the commit, so each branch builds with its own
//!    pipeline at the exact sha.
//! 3. Run the ordered build **steps**, each as the leader of its own process
//!    group + session so a timeout SIGKILLs the whole `cargo`→`rustc` tree,
//!    not just the immediate child (the warn-soak leak fix, mirrored from
//!    `project_checks::check_command`). First non-zero step ⇒ **Red**.
//! 4. **Re-resolve HEAD**. If the worktree sha moved underneath the build,
//!    the green can't be trusted as "green for `sha`": return
//!    **Indeterminate** (inc-2 requeues once, then reds on a repeat).
//! 5. **Harvest** the declared artifacts into a fresh bundle dir via
//!    tmp-dir + atomic rename, write a flat `meta` provenance file
//!    (key=value, the [`crate::build`] pointer idiom), and return the path.
//!
//! Zero new deps: `std::process` + `git` (already a runtime dependency of the
//! check tier). The pure scheduling lives in [`crate::appstate`]; this module
//! is the irreducibly-effectful half (subprocess, fs) kept deliberately thin
//! and behind a `BuildHooks` seam so inc-5's tests can inject fake steps.

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::appmanifest::{AppManifest, BuildStep, load_app_manifest};

/// Outcome of one build attempt. Mirrors [`crate::appstate::AppBuildOutcome`]
/// but carries the harvested bundle path on success — the driver converts
/// between the two at the seam (this crate stays appstate-independent so the
/// build worker can be tested in isolation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildReport {
    /// Every step exited 0, the sha held, and the bundle was harvested.
    /// `manifest` is boxed: it is by far the largest field and only the rare
    /// green path carries it, so the common Red/Indeterminate variants stay
    /// small (clippy::large_enum_variant).
    Green {
        sha: String,
        manifest: Box<AppManifest>,
        bundle_dir: PathBuf,
    },
    /// A step failed (or checkout / harvest failed). `reason` is operator-
    /// facing and names the failing step + a short output tail. `enospc` is
    /// `true` when the failure was an out-of-disk (ENOSPC) condition — an
    /// *environmental* fault, not a defect in `sha`. The state machine treats
    /// an `enospc` red as non-latching (requeue-once, like Indeterminate) so a
    /// transient full disk the daemon then self-relieves does not pin a good
    /// commit red forever (the disk self-starvation latch).
    Red {
        sha: String,
        reason: String,
        enospc: bool,
    },
    /// The build cannot be trusted: the worktree sha moved mid-build, or HEAD
    /// could not be re-resolved. Not a defect in `sha` — requeued once.
    Indeterminate { sha: String, reason: String },
}

/// Where build state for one instance lives on disk:
/// `<state_dir>/app/<instance>/`. The worktree is the checked-out repo; the
/// bundles dir holds `<sha>/` harvest dirs.
#[derive(Debug, Clone)]
pub struct InstancePaths {
    /// The git worktree this instance builds in (daemon-owned checkouts).
    pub worktree: PathBuf,
    /// `<state_dir>/app/<instance>/bundles` — one `<sha>/` subdir per green.
    pub bundles: PathBuf,
}

impl InstancePaths {
    pub fn bundle_dir(&self, sha: &str) -> PathBuf {
        self.bundles.join(sha)
    }
}

/// True if this io error is ENOSPC (disk full). `harvest`'s `fs::copy` /
/// `fs::rename` / `create_dir_all` surface a full disk as a real `io::Error`
/// whose `raw_os_error()` is 28 on every unix — the robust signal (a string
/// match is the fallback only where we have lost the typed error, see
/// [`reason_looks_like_enospc`]).
fn io_is_enospc(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(28)
}

/// True if a build/checkout error *string* looks like a disk-full failure.
/// Used where the typed `io::Error` is gone — `checkout` returns git's stderr
/// as a `String`, and git surfaces ENOSPC as "No space left on device" /
/// "index.lock: ... Out of disk space" / "os error 28". Lowercased to be
/// phrasing-robust. Conservative: a non-match just means "treat as a normal
/// (latching) red", which is the safe default.
fn reason_looks_like_enospc(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.contains("no space left")
        || s.contains("os error 28")
        || s.contains("enospc")
        || s.contains("out of disk")
        || s.contains("disk quota exceeded")
}

/// Injection seam for the effectful operations, so inc-5's daemon tests can
/// drive the full lifecycle without a real cargo build. Production uses
/// [`RealHooks`]; tests supply fakes. Only the two genuinely slow/external
/// operations are behind it — checkout and step-exec; harvest is plain fs.
pub trait BuildHooks: Send + Sync {
    /// Check out `sha` in `worktree` (detached). Err ⇒ the build is Red with
    /// this message (a bad sha / dirty tree is a real, reportable failure).
    fn checkout(&self, worktree: &Path, sha: &str) -> Result<(), String>;

    /// Resolve the worktree's current HEAD to a full sha. Used twice: the
    /// post-build recheck, and (by the daemon) ref→sha polling. Err here ⇒
    /// Indeterminate (we cannot prove what we built).
    fn resolve_head(&self, worktree: &Path) -> Result<String, String>;

    /// Run one build step in `worktree` with `env` overlaid. Returns the
    /// exit outcome; the worker decides Red/continue. Default impl is the
    /// real process-group runner — tests override.
    fn run_step(&self, worktree: &Path, step: &BuildStep, env: &[(String, String)]) -> StepOutcome;
}

/// Result of running one build step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    Ok,
    /// Non-zero exit; `code` is the status (None if signal-killed) and `tail`
    /// is the last few lines of combined output for the red reason.
    Failed {
        code: Option<i32>,
        tail: String,
    },
    /// The step exceeded its `timeout_ms`; the process tree was SIGKILLed.
    TimedOut,
    /// The step could not be spawned at all (missing interpreter, etc.).
    SpawnError {
        message: String,
    },
}

/// Production [`BuildHooks`]: real `git` checkout + real process-group step
/// execution with timeout and whole-tree SIGKILL.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealHooks;

/// Re-create an instance worktree that vanished while the daemon was running.
///
/// The repo root is *derived*, not passed: `RealHooks::checkout` is handed only
/// the worktree, and threading a repo path down to it would change the
/// `BuildHooks` signature for every fake in the test suite to serve one
/// recovery path. `git worktree list` from inside a sibling worktree would be
/// the obvious source, but the sibling may be mid-build and the tree we need is
/// precisely the one that is gone — so we read the layout instead:
/// `<state_dir>/app/<name>/worktree`, and the daemon's repo is the `--repo` it
/// was started with. That is not derivable from the path, so we ask git for it
/// via any surviving worktree's admin file, and fall back to the common
/// ancestor convention only if that fails.
///
/// Errors are returned, never swallowed: a worktree we could not rebuild must
/// still surface as a build failure with the git stderr attached, exactly as
/// before this function existed. The one thing it must not do is silently
/// succeed and leave the caller checking out into nothing.
fn recreate_worktree(worktree: &Path) -> Result<(), String> {
    // `<state>/app/<name>/worktree` -> the `.git` file of a sibling instance
    // names the real repo. Any sibling will do; we only need the common
    // repository path, which every worktree of the same repo shares.
    let app_dir = worktree
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| format!("worktree path {} has no app dir", worktree.display()))?;
    let repo = sibling_repo_root(app_dir).ok_or_else(|| {
        format!(
            "worktree {} is missing and the repo it belongs to could not be \
             determined from any sibling instance",
            worktree.display()
        )
    })?;

    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create worktree parent dir: {e}"))?;
    }
    // Prune first: git still holds an administrative record for the deleted
    // tree, and `worktree add` refuses a path it believes is already
    // registered. This is the step whose absence makes a hand-recovery fail
    // with the baffling "already exists" on a directory that plainly does not.
    let _ = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "prune"])
        .output();
    // `origin/HEAD` would be nicer but is often unset in a bare-ish CI clone.
    // The ref here is throwaway — the caller checks out the real sha next, and
    // that checkout is what the build actually uses.
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "add", "--detach"])
        .arg(worktree)
        .output()
        .map_err(|e| format!("could not spawn git worktree add: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "worktree {} was missing and could not be recreated: {}",
            worktree.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The repository that `<app_dir>`'s instances are worktrees of, learned from
/// whichever sibling still has its `.git` file. Returns `None` when no sibling
/// survives — the caller then reports a plain failure rather than guessing.
fn sibling_repo_root(app_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(app_dir).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("worktree");
        if !candidate.join(".git").exists() {
            continue;
        }
        let out = Command::new("git")
            .arg("-C")
            .arg(&candidate)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .output()
            .ok()?;
        if !out.status.success() {
            continue;
        }
        let common = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if common.is_empty() {
            continue;
        }
        // `<repo>/.git` -> `<repo>`. A bare repo has no `.git` component, in
        // which case the common dir IS the repo and is usable as-is.
        let common = PathBuf::from(common);
        return Some(if common.file_name().is_some_and(|n| n == ".git") {
            common.parent().map(Path::to_path_buf).unwrap_or(common)
        } else {
            common
        });
    }
    None
}

impl BuildHooks for RealHooks {
    fn checkout(&self, worktree: &Path, sha: &str) -> Result<(), String> {
        // A worktree that vanished mid-life is REBUILT, not reported.
        //
        // `ensure_instance_worktree` runs at boot (manifest instances) and at
        // creation (runtime previews) — and nowhere else. So a worktree
        // destroyed while the daemon keeps running is never recreated, and
        // every subsequent build of that instance fails identically:
        //
        //     git checkout <sha> failed: fatal: cannot change to
        //     '<state>/app/lane/worktree': No such file or directory
        //
        // Observed 2026-08-02: the preview pod self-deleted mid-build and took
        // the `lane` slot's worktree with it (gone from disk AND from
        // `git worktree list`, while the manifest instances survived because
        // their boot path re-ran). The build lane then burned a generation
        // every few minutes against a fault no retry could ever clear — the
        // slot cannot heal itself and nothing else was going to heal it.
        //
        // Recreating here rather than at the call sites is deliberate: this is
        // the ONE place that requires the tree to exist, so it is the one place
        // that cannot forget. Boot-time setup stays — it keeps the first build
        // fast and surfaces a broken repo early — but it is no longer the only
        // line of defence.
        if !worktree.join(".git").exists() {
            recreate_worktree(worktree)?;
        }
        // `--detach` + the explicit sha: we build a specific commit, never a
        // branch tip that could move. `-f` discards any stray worktree state
        // from a previous interrupted build (the tree is daemon-owned scratch,
        // never a human's working copy).
        let out = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["checkout", "-f", "--detach", sha])
            .output()
            .map_err(|e| format!("could not spawn git checkout: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "git checkout {sha} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn resolve_head(&self, worktree: &Path) -> Result<String, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(|e| format!("could not spawn git rev-parse: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git rev-parse HEAD failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if sha.is_empty() {
            return Err("git rev-parse HEAD returned empty".to_string());
        }
        Ok(sha)
    }

    fn run_step(&self, worktree: &Path, step: &BuildStep, env: &[(String, String)]) -> StepOutcome {
        run_step_real(worktree, step, env)
    }
}

/// Run the full build for `sha` in `paths`, with per-build `env` overlaid on
/// every step. The single entry point the daemon calls on its build worker.
pub fn build(
    hooks: &dyn BuildHooks,
    paths: &InstancePaths,
    sha: &str,
    env: &[(String, String)],
) -> BuildReport {
    // 1. Checkout — a failure here is a real, reportable Red (bad sha)…
    //    unless it is the *disk* that failed (git can't write its index/lock
    //    on a full PVC), which is environmental — classify it `enospc` so the
    //    state machine retries the sha once after the daemon self-relieves.
    if let Err(e) = hooks.checkout(&paths.worktree, sha) {
        let enospc = reason_looks_like_enospc(&e);
        return BuildReport::Red {
            sha: sha.to_string(),
            reason: e,
            enospc,
        };
    }

    // 2. Manifest rides the commit.
    let manifest = match load_app_manifest(&paths.worktree) {
        Ok(Some(m)) => m,
        Ok(None) => {
            return BuildReport::Red {
                sha: sha.to_string(),
                reason: "no cargoless.app.yaml at this sha (app-serve not configured for this ref)"
                    .to_string(),
                enospc: false,
            };
        }
        Err(e) => {
            return BuildReport::Red {
                sha: sha.to_string(),
                reason: format!("invalid cargoless.app.yaml: {e}"),
                enospc: false,
            };
        }
    };

    // 3. Ordered steps; first non-zero ⇒ Red.
    for step in &manifest.build.steps {
        match hooks.run_step(&paths.worktree, step, env) {
            StepOutcome::Ok => {}
            StepOutcome::Failed { code, tail } => {
                let code = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                return BuildReport::Red {
                    sha: sha.to_string(),
                    // A build step that ran out of disk reports it in its tail
                    // (linker / cargo "No space left"): treat that as ENOSPC so
                    // the sha is retried after relief rather than latched.
                    enospc: reason_looks_like_enospc(&tail),
                    reason: format!("build step `{}` exited {code}:\n{tail}", step.id),
                };
            }
            StepOutcome::TimedOut => {
                return BuildReport::Red {
                    sha: sha.to_string(),
                    reason: format!(
                        "build step `{}` timed out after {}ms",
                        step.id, step.timeout_ms
                    ),
                    enospc: false,
                };
            }
            StepOutcome::SpawnError { message } => {
                return BuildReport::Red {
                    sha: sha.to_string(),
                    reason: format!("build step `{}` could not start: {message}", step.id),
                    enospc: false,
                };
            }
        }
    }

    // 4. Re-resolve HEAD: did the tree move under us? If so the green is not
    //    attributable to `sha` — Indeterminate, not Red.
    match hooks.resolve_head(&paths.worktree) {
        Ok(head) if head == sha => {}
        Ok(head) => {
            return BuildReport::Indeterminate {
                sha: sha.to_string(),
                reason: format!("worktree moved to {head} during the build"),
            };
        }
        Err(e) => {
            return BuildReport::Indeterminate {
                sha: sha.to_string(),
                reason: format!("could not re-resolve HEAD after build: {e}"),
            };
        }
    }

    // 5. Harvest the declared artifacts into a fresh bundle dir.
    match harvest(paths, sha, &manifest) {
        Ok(bundle_dir) => BuildReport::Green {
            sha: sha.to_string(),
            manifest: Box::new(manifest),
            bundle_dir,
        },
        // Harvest is the primary ENOSPC site: a full PVC fails the artifact
        // copy / atomic rename with a typed `os error 28`. Classify off the
        // typed error (robust) so the state machine retries the sha once after
        // the daemon self-relieves, rather than latching a good commit red.
        Err(e) => {
            let enospc = io_is_enospc(&e);
            BuildReport::Red {
                sha: sha.to_string(),
                reason: format!("bundle harvest failed: {e}"),
                enospc,
            }
        }
    }
}

/// Copy the manifest's declared artifacts out of the worktree into
/// `<bundles>/<sha>/`, built first under a sibling tmp dir and atomically
/// renamed into place — a reader never sees a half-harvested bundle, and a
/// crashed harvest leaves no partial `<sha>/` (only `<sha>.tmp.<pid>` litter,
/// removed on the next attempt). A `meta` provenance file is written last.
fn harvest(paths: &InstancePaths, sha: &str, manifest: &AppManifest) -> std::io::Result<PathBuf> {
    fs::create_dir_all(&paths.bundles)?;
    let final_dir = paths.bundle_dir(sha);
    if final_dir.exists() {
        // A previous green for this exact sha: idempotent, reuse it.
        return Ok(final_dir);
    }
    let tmp = paths
        .bundles
        .join(format!("{sha}.tmp.{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;

    let harvest_one = || -> std::io::Result<()> {
        for rel in &manifest.build.artifacts {
            let src = paths.worktree.join(rel);
            let dst = tmp.join(rel);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            let meta = fs::symlink_metadata(&src).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!("artifact `{rel}` not found in worktree: {e}"),
                )
            })?;
            if meta.is_dir() {
                copy_dir_recursive(&src, &dst)?;
            } else {
                fs::copy(&src, &dst)?;
            }
        }
        // Provenance: flat key=value, the build-pointer idiom. Written into
        // the tmp dir so it lands atomically with the artifacts.
        fs::write(tmp.join("meta"), render_meta(sha, manifest))?;
        Ok(())
    };

    if let Err(e) = harvest_one() {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }

    match fs::rename(&tmp, &final_dir) {
        Ok(()) => Ok(final_dir),
        Err(e) => {
            // Lost a race to a concurrent harvest of the same sha? Accept the
            // winner. Otherwise clean up and surface the error.
            if final_dir.exists() {
                let _ = fs::remove_dir_all(&tmp);
                Ok(final_dir)
            } else {
                let _ = fs::remove_dir_all(&tmp);
                Err(e)
            }
        }
    }
}

/// Prune an instance's bundle dir to bound disk: keep every `protected` sha
/// (the currently-serving + last-green bundles — never delete what a running
/// or recoverable child needs), plus the `keep_extra` most-recently-modified
/// of the remaining bundles; delete the rest. Returns the shas removed.
///
/// inc-6 hardening: without this the PVC fills with every sha ever built (the
/// 250Gi preview PVC assumes a bounded set). Called by the driver after a
/// promote, with `protected = {serving_sha, last_green}` so it is impossible
/// to delete a live or recovery bundle even if it is old. A pathological
/// `<sha>.tmp.<pid>` left by a crashed harvest is also swept (never protected,
/// never a valid `<sha>`).
pub fn prune_bundles(
    bundles: &Path,
    protected: &[&str],
    keep_extra: usize,
) -> std::io::Result<Vec<String>> {
    let read = match fs::read_dir(bundles) {
        Ok(r) => r,
        // No bundles dir yet ⇒ nothing to prune (not an error).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    // Collect candidate (name, mtime) for every bundle dir that is NOT
    // protected. Sort newest-first; everything past `keep_extra` is removed.
    let mut candidates: Vec<(String, std::time::SystemTime)> = Vec::new();
    let mut to_remove: Vec<String> = Vec::new();
    for entry in read {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // Always sweep tmp-harvest litter regardless of keep counts.
        if name.contains(".tmp.") {
            to_remove.push(name);
            continue;
        }
        if protected.contains(&name.as_str()) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        candidates.push((name, mtime));
    }
    // Newest first; keep the first `keep_extra`, remove the tail.
    candidates.sort_by(|a, b| b.1.cmp(&a.1));
    for (name, _) in candidates.into_iter().skip(keep_extra) {
        to_remove.push(name);
    }
    for name in &to_remove {
        let _ = fs::remove_dir_all(bundles.join(name));
    }
    Ok(to_remove)
}

/// Flat, human-inspectable bundle provenance (the [`crate::build`] pointer
/// style). Records the sha, the manifest hash (which pipeline produced it),
/// the app name, and the harvested artifact list.
fn render_meta(sha: &str, manifest: &AppManifest) -> String {
    use fmt::Write as _;
    let mut s = String::new();
    s.push_str("cargoless-app-bundle/1\n");
    let _ = writeln!(s, "sha={sha}");
    let _ = writeln!(s, "app_name={}", manifest.app_name);
    let _ = writeln!(s, "manifest_hash={}", manifest.manifest_hash);
    let _ = writeln!(s, "artifacts={}", manifest.build.artifacts.join(","));
    s
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_symlink() {
            // Preserve symlinks as-is (the site/ dir may contain them); a
            // broken link is harvested faithfully rather than dereferenced.
            copy_symlink(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(from: &Path, to: &Path) -> std::io::Result<()> {
    let target = fs::read_link(from)?;
    let _ = fs::remove_file(to);
    std::os::unix::fs::symlink(target, to)
}

#[cfg(not(unix))]
fn copy_symlink(from: &Path, to: &Path) -> std::io::Result<()> {
    // No portable symlink create off-unix; fall back to a content copy.
    fs::copy(from, to).map(|_| ())
}

/// The real process-group step runner — its own copy of the
/// `project_checks::check_command` discipline (that one is private and
/// check-specific): spawn as a session/group leader, stream both pipes on
/// helper threads, SIGKILL the whole tree on timeout.
fn run_step_real(worktree: &Path, step: &BuildStep, env: &[(String, String)]) -> StepOutcome {
    let mut cmd = Command::new(&step.command[0]);
    cmd.args(&step.command[1..])
        .current_dir(worktree)
        .env("CARGOLESS", "1")
        .env("CARGOLESS_APP_STEP", &step.id)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    // Leader of its own process group + session so a timeout SIGKILLs the
    // whole `cargo`→`rustc` tree, not just the immediate child (else
    // grandchildren reparent to init and compile on past the deadline).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
        // SAFETY: pre_exec runs post-fork/pre-exec, single-threaded; setsid(2)
        // is async-signal-safe. EPERM (already a leader) is swallowed —
        // process_group(0) is the load-bearing line.
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

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return StepOutcome::SpawnError {
                message: e.to_string(),
            };
        }
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_thread = thread::spawn(move || read_pipe(&mut stdout));
    let err_thread = thread::spawn(move || read_pipe(&mut stderr));
    let deadline = Instant::now() + Duration::from_millis(step.timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                kill_process_tree(&mut child);
                let _ = child.wait();
                break Err(());
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                kill_process_tree(&mut child);
                let _ = child.wait();
                break Err(());
            }
        }
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    match status {
        Ok(s) if s.success() => StepOutcome::Ok,
        Ok(s) => StepOutcome::Failed {
            code: s.code(),
            // The FIRST error and what follows it — not the last 20 lines, which
            // on a warning-heavy crate are guaranteed to contain no diagnostic.
            tail: failure_excerpt(&format!("{stdout}\n{stderr}"), 20),
        },
        Err(()) => StepOutcome::TimedOut,
    }
}

fn read_pipe(pipe: &mut Option<impl Read>) -> String {
    let mut out = String::new();
    if let Some(pipe) = pipe {
        let _ = pipe.read_to_string(&mut out);
    }
    out
}

/// The last `n` non-blank lines — the fallback when nothing looks like an error.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// True if this line opens a rustc/cargo ERROR (not a warning, not a note).
///
/// `error[E0308]:` and `error:` both count; `error: could not compile ...` does
/// NOT — that is the trailing summary rustc prints after every failure, and
/// keeping it while dropping the real diagnostic is exactly the bug this
/// function exists to prevent.
fn opens_an_error(line: &str) -> bool {
    let t = line.trim_start();
    if !t.starts_with("error") {
        return false;
    }
    // `error:` or `error[E1234]:`
    let after = t.strip_prefix("error").unwrap_or("");
    let is_error_head = after.starts_with(':')
        || (after.starts_with('[') && after.split(']').nth(1).is_some_and(|r| r.starts_with(':')));
    is_error_head && !t.contains("could not compile") && !t.contains("aborting due to")
}

/// The failure excerpt for a red build: the FIRST real error and the lines that
/// follow it, falling back to the tail when nothing matches.
///
/// # Why not just the tail
///
/// `tail_lines(.., 20)` was the whole capture, and it made real reds
/// undiagnosable. rustc prints warnings AFTER the error that stopped the build,
/// then a one-line summary. On 2026-08-03 a broken `triform-physics` on dev
/// produced 63 warnings, so the last 20 lines were *guaranteed* to be warnings
/// plus `error: could not compile ... due to 1 previous error`. The 1039-char
/// reason stored on the slot began mid-warning at line 1587 and contained no
/// diagnostic at all — I could see WHICH FILES were implicated but never WHAT
/// WAS WRONG, on the one build that mattered. There is no build log on disk, the
/// daemon log does not carry it, and no CI workflow compiles this crate, so the
/// truncated string was the only evidence in existence — and it had thrown the
/// error away.
///
/// So: anchor on the first line that OPENS an error and keep the window after
/// it. rustc emits the diagnostic body (`-->`, source excerpt, `= help:`)
/// immediately after that line, which is precisely what a human needs.
///
/// Falls back to the tail when no error line is found — a step can fail without
/// rustc (a shell error, a missing binary), and the tail is right for those.
fn failure_excerpt(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    match lines.iter().position(|l| opens_an_error(l)) {
        Some(i) => lines[i..lines.len().min(i + n)].join("\n"),
        None => tail_lines(text, n),
    }
}

fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        unsafe {
            unsafe extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            const SIGKILL: i32 = 9;
            // SIGKILL the whole process group (negative pid = the group the
            // setsid leader created).
            let _ = kill(-pid, SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appmanifest::{BuildSpec, DrainSpec, HealthSpec, RunSpec};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    fn scratch(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cargoless-appbuild-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn manifest(steps: Vec<BuildStep>, artifacts: Vec<&str>) -> AppManifest {
        AppManifest {
            app_name: "toy".into(),
            build: BuildSpec {
                steps,
                artifacts: artifacts.into_iter().map(String::from).collect(),
            },
            run: RunSpec {
                command: vec!["./run".into()],
                env: BTreeMap::new(),
                port_env: "PORT".into(),
            },
            health: HealthSpec {
                path: "/".into(),
                ready_timeout_ms: 1000,
                interval_ms: 100,
            },
            drain: DrainSpec { grace_ms: 1000 },
            manifest_hash: "deadbeef".into(),
        }
    }

    fn write_manifest(worktree: &Path, m: &AppManifest) {
        // Render a real cargoless.app.yaml the build worker will load back.
        let mut y = String::from("version: 1\napp:\n  name: ");
        y.push_str(&m.app_name);
        y.push_str("\nbuild:\n  steps:\n");
        for s in &m.build.steps {
            y.push_str(&format!("    - id: {}\n", s.id));
            let cmd = s
                .command
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ");
            y.push_str(&format!("      command: [{cmd}]\n"));
            y.push_str(&format!("      timeout_ms: {}\n", s.timeout_ms));
        }
        let arts = m
            .build
            .artifacts
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(", ");
        y.push_str(&format!("  artifacts: [{arts}]\n"));
        y.push_str("run:\n  command: [\"./run\"]\n  port_env: PORT\n");
        fs::write(worktree.join("cargoless.app.yaml"), y).unwrap();
    }

    /// A scriptable hooks fake: records checkout calls, returns a queued HEAD
    /// per resolve_head call, and runs steps via a closure-driven table.
    struct FakeHooks {
        head_after_build: Mutex<String>,
        checked_out: Mutex<Vec<String>>,
        step_results: Mutex<BTreeMap<String, StepOutcome>>,
        run_log: Mutex<Vec<String>>,
    }

    impl FakeHooks {
        fn new(head_after_build: &str) -> Self {
            Self {
                head_after_build: Mutex::new(head_after_build.into()),
                checked_out: Mutex::new(Vec::new()),
                step_results: Mutex::new(BTreeMap::new()),
                run_log: Mutex::new(Vec::new()),
            }
        }
        fn with_step(self, id: &str, outcome: StepOutcome) -> Self {
            self.step_results.lock().unwrap().insert(id.into(), outcome);
            self
        }
    }

    impl BuildHooks for FakeHooks {
        fn checkout(&self, _worktree: &Path, sha: &str) -> Result<(), String> {
            self.checked_out.lock().unwrap().push(sha.into());
            Ok(())
        }
        fn resolve_head(&self, _worktree: &Path) -> Result<String, String> {
            Ok(self.head_after_build.lock().unwrap().clone())
        }
        fn run_step(
            &self,
            _worktree: &Path,
            step: &BuildStep,
            _env: &[(String, String)],
        ) -> StepOutcome {
            self.run_log.lock().unwrap().push(step.id.clone());
            self.step_results
                .lock()
                .unwrap()
                .get(&step.id)
                .cloned()
                .unwrap_or(StepOutcome::Ok)
        }
    }

    fn paths(dir: &Path) -> InstancePaths {
        let worktree = dir.join("wt");
        fs::create_dir_all(&worktree).unwrap();
        InstancePaths {
            worktree,
            bundles: dir.join("bundles"),
        }
    }

    #[test]
    fn green_build_harvests_a_bundle_with_provenance() {
        let dir = scratch("green");
        let p = paths(&dir);
        // Real artifacts in the worktree: a binary file and a site/ dir.
        fs::write(p.worktree.join("server-bin"), b"ELF...").unwrap();
        fs::create_dir_all(p.worktree.join("site/pkg")).unwrap();
        fs::write(p.worktree.join("site/index.html"), b"<html>").unwrap();

        let m = manifest(
            vec![BuildStep {
                id: "compile".into(),
                command: vec!["true".into()],
                timeout_ms: 1000,
            }],
            vec!["server-bin", "site"],
        );
        write_manifest(&p.worktree, &m);
        let hooks = FakeHooks::new("sha1");

        let report = build(&hooks, &p, "sha1", &[]);
        let bundle = match report {
            BuildReport::Green {
                bundle_dir, sha, ..
            } => {
                assert_eq!(sha, "sha1");
                bundle_dir
            }
            other => panic!("expected Green, got {other:?}"),
        };
        // Artifacts harvested faithfully.
        assert_eq!(fs::read(bundle.join("server-bin")).unwrap(), b"ELF...");
        assert_eq!(fs::read(bundle.join("site/index.html")).unwrap(), b"<html>");
        // Provenance names the sha + manifest hash. The hash is the sha256 of
        // the on-disk `cargoless.app.yaml` that `build()` re-loads at the sha
        // (the in-memory `manifest_hash` on the test struct is discarded — the
        // worker rehashes the committed manifest text), so assert against THAT
        // real hash, not the struct's placeholder. Re-load the same way build()
        // does to compute the expected value.
        let meta = fs::read_to_string(bundle.join("meta")).unwrap();
        assert!(meta.contains("sha=sha1"), "{meta}");
        let expected_hash = load_app_manifest(&p.worktree)
            .expect("manifest re-loads")
            .expect("manifest present")
            .manifest_hash;
        assert!(
            !expected_hash.is_empty() && expected_hash != "deadbeef",
            "re-loaded manifest must carry a real computed hash, got {expected_hash:?}"
        );
        assert!(
            meta.contains(&format!("manifest_hash={expected_hash}")),
            "meta must record the re-loaded manifest's real hash ({expected_hash}): {meta}"
        );
        assert!(meta.contains("artifacts=server-bin,site"), "{meta}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_failing_step_is_red_and_stops_the_pipeline() {
        let dir = scratch("red");
        let p = paths(&dir);
        let m = manifest(
            vec![
                BuildStep {
                    id: "server".into(),
                    command: vec!["true".into()],
                    timeout_ms: 1000,
                },
                BuildStep {
                    id: "portal".into(),
                    command: vec!["false".into()],
                    timeout_ms: 1000,
                },
                BuildStep {
                    id: "never".into(),
                    command: vec!["true".into()],
                    timeout_ms: 1000,
                },
            ],
            vec![],
        );
        write_manifest(&p.worktree, &m);
        let hooks = FakeHooks::new("sha1").with_step(
            "portal",
            StepOutcome::Failed {
                code: Some(101),
                tail: "error[E0432]: unresolved import".into(),
            },
        );

        let report = build(&hooks, &p, "sha1", &[]);
        match report {
            BuildReport::Red {
                reason,
                sha,
                enospc,
            } => {
                assert_eq!(sha, "sha1");
                assert!(reason.contains("`portal` exited 101"), "{reason}");
                assert!(reason.contains("E0432"), "tail surfaced: {reason}");
                // A genuine compile error is a code-red, NOT environmental:
                // it must latch (enospc=false) so the bad sha is not retried.
                assert!(!enospc, "compile-error red must not be classified ENOSPC");
            }
            other => panic!("expected Red, got {other:?}"),
        }
        // The step AFTER the failure never ran.
        let log = hooks.run_log.lock().unwrap().clone();
        assert_eq!(log, vec!["server", "portal"], "pipeline stops at first red");
        // No bundle was harvested.
        assert!(!p.bundle_dir("sha1").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sha_moving_under_the_build_is_indeterminate_not_green() {
        let dir = scratch("indet");
        let p = paths(&dir);
        fs::write(p.worktree.join("server-bin"), b"x").unwrap();
        let m = manifest(
            vec![BuildStep {
                id: "compile".into(),
                command: vec!["true".into()],
                timeout_ms: 1000,
            }],
            vec!["server-bin"],
        );
        write_manifest(&p.worktree, &m);
        // HEAD resolves to a DIFFERENT sha after the build ⇒ tree moved.
        let hooks = FakeHooks::new("sha2");

        let report = build(&hooks, &p, "sha1", &[]);
        match report {
            BuildReport::Indeterminate { reason, sha } => {
                assert_eq!(sha, "sha1");
                assert!(reason.contains("moved to sha2"), "{reason}");
            }
            other => panic!("expected Indeterminate, got {other:?}"),
        }
        // Critically: a moved tree must NOT publish a bundle for sha1.
        assert!(!p.bundle_dir("sha1").exists(), "no bundle on indeterminate");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_manifest_at_sha_is_red() {
        let dir = scratch("nomanifest");
        let p = paths(&dir);
        // No cargoless.app.yaml written.
        let hooks = FakeHooks::new("sha1");
        match build(&hooks, &p, "sha1", &[]) {
            BuildReport::Red { reason, .. } => {
                assert!(reason.contains("no cargoless.app.yaml"), "{reason}");
            }
            other => panic!("expected Red, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn timed_out_step_is_red_with_the_budget() {
        let dir = scratch("timeout");
        let p = paths(&dir);
        let m = manifest(
            vec![BuildStep {
                id: "slow".into(),
                command: vec!["sleep".into()],
                timeout_ms: 5000,
            }],
            vec![],
        );
        write_manifest(&p.worktree, &m);
        let hooks = FakeHooks::new("sha1").with_step("slow", StepOutcome::TimedOut);
        match build(&hooks, &p, "sha1", &[]) {
            BuildReport::Red { reason, .. } => {
                assert!(reason.contains("`slow` timed out"), "{reason}");
                assert!(reason.contains("5000ms"), "names the budget: {reason}");
            }
            other => panic!("expected Red, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_artifact_makes_harvest_red() {
        let dir = scratch("noart");
        let p = paths(&dir);
        // Manifest declares an artifact that the build did not produce.
        let m = manifest(
            vec![BuildStep {
                id: "compile".into(),
                command: vec!["true".into()],
                timeout_ms: 1000,
            }],
            vec!["target/release/missing-bin"],
        );
        write_manifest(&p.worktree, &m);
        let hooks = FakeHooks::new("sha1");
        match build(&hooks, &p, "sha1", &[]) {
            BuildReport::Red { reason, enospc, .. } => {
                assert!(reason.contains("harvest failed"), "{reason}");
                assert!(
                    reason.contains("missing-bin"),
                    "names the artifact: {reason}"
                );
                // A missing artifact is ENOENT, not ENOSPC — must NOT be
                // classified as a disk-full red (else a real harvest bug would
                // be retried forever instead of latching).
                assert!(!enospc, "missing-artifact harvest red is not ENOSPC");
            }
            other => panic!("expected Red, got {other:?}"),
        }
        // The failed harvest left no partial bundle and no tmp litter.
        assert!(!p.bundle_dir("sha1").exists());
        let litter: Vec<_> = fs::read_dir(&p.bundles)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.contains(".tmp."))
                    .collect()
            })
            .unwrap_or_default();
        assert!(litter.is_empty(), "no tmp litter: {litter:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn io_is_enospc_matches_only_os_error_28() {
        // The typed signal harvest relies on: raw os error 28 == ENOSPC.
        let enospc = std::io::Error::from_raw_os_error(28);
        assert!(io_is_enospc(&enospc));
        // ENOENT (2) and a kind-only error are NOT disk-full.
        assert!(!io_is_enospc(&std::io::Error::from_raw_os_error(2)));
        assert!(!io_is_enospc(&std::io::Error::other("no os errno")));
    }

    #[test]
    fn reason_looks_like_enospc_matches_disk_full_phrasings() {
        // git / cargo / linker phrasings the string-fallback must catch.
        assert!(reason_looks_like_enospc(
            "fatal: ... index.lock: No space left on device"
        ));
        assert!(reason_looks_like_enospc(
            "error: failed to write (os error 28)"
        ));
        assert!(reason_looks_like_enospc("ENOSPC: disk full"));
        assert!(reason_looks_like_enospc("linker: Disk quota exceeded"));
        // Case-insensitive.
        assert!(reason_looks_like_enospc("NO SPACE LEFT"));
        // A real compile / checkout error is NOT disk-full.
        assert!(!reason_looks_like_enospc("error[E0308]: mismatched types"));
        assert!(!reason_looks_like_enospc(
            "fatal: reference is not a tree: abc123"
        ));
        assert!(!reason_looks_like_enospc(""));
    }

    #[test]
    fn re_harvesting_the_same_sha_is_idempotent() {
        let dir = scratch("idem");
        let p = paths(&dir);
        fs::write(p.worktree.join("bin"), b"v1").unwrap();
        let m = manifest(
            vec![BuildStep {
                id: "c".into(),
                command: vec!["true".into()],
                timeout_ms: 1000,
            }],
            vec!["bin"],
        );
        write_manifest(&p.worktree, &m);
        let hooks = FakeHooks::new("sha1");
        let first = match build(&hooks, &p, "sha1", &[]) {
            BuildReport::Green { bundle_dir, .. } => bundle_dir,
            other => panic!("expected Green, got {other:?}"),
        };
        // Second build of the same sha reuses the existing bundle dir.
        let second = match build(&hooks, &p, "sha1", &[]) {
            BuildReport::Green { bundle_dir, .. } => bundle_dir,
            other => panic!("expected Green, got {other:?}"),
        };
        assert_eq!(first, second);
        let _ = fs::remove_dir_all(&dir);
    }

    // The real process-group runner exercised end-to-end with /bin/sh, so the
    // spawn + pipe-stream + exit-code path is covered (not just the fake).
    #[test]
    fn real_runner_reports_exit_codes_and_tails() {
        let dir = scratch("realrun");
        let wt = dir.join("wt");
        fs::create_dir_all(&wt).unwrap();
        let ok = run_step_real(
            &wt,
            &BuildStep {
                id: "ok".into(),
                command: vec!["sh".into(), "-c".into(), "echo hi; exit 0".into()],
                timeout_ms: 5000,
            },
            &[],
        );
        assert_eq!(ok, StepOutcome::Ok);

        let bad = run_step_real(
            &wt,
            &BuildStep {
                id: "bad".into(),
                command: vec!["sh".into(), "-c".into(), "echo boom >&2; exit 7".into()],
                timeout_ms: 5000,
            },
            &[],
        );
        match bad {
            StepOutcome::Failed { code, tail } => {
                assert_eq!(code, Some(7));
                assert!(tail.contains("boom"), "stderr tail surfaced: {tail}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_runner_kills_a_timed_out_tree() {
        let dir = scratch("realkill");
        let wt = dir.join("wt");
        fs::create_dir_all(&wt).unwrap();
        let start = Instant::now();
        let out = run_step_real(
            &wt,
            &BuildStep {
                id: "hang".into(),
                // A child that would sleep far past the budget.
                command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
                timeout_ms: 300,
            },
            &[],
        );
        assert_eq!(out, StepOutcome::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout fired promptly, not after the 30s sleep"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Make `<bundles>/<name>/` with a marker file and a controlled mtime
    /// (older `age_secs` = older bundle), so prune ordering is deterministic.
    fn bundle_at(bundles: &Path, name: &str, age_secs: u64) {
        let d = bundles.join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("meta"), format!("sha={name}\n")).unwrap();
        let when = std::time::SystemTime::now() - Duration::from_secs(age_secs);
        // Best-effort: set mtime so newest-first sorting is stable in test.
        let _ = filetime_set(&d, when);
    }

    // std has no stable set-mtime; shell out to `touch -d` (portable enough for
    // the test container) and fall back to leaving the natural mtime.
    fn filetime_set(path: &Path, when: std::time::SystemTime) -> std::io::Result<()> {
        let secs = when
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // `touch -t` wants [[CC]YY]MMDDhhmm[.ss]; use -d @epoch (GNU coreutils
        // on the Debian CI image). Non-fatal if it fails.
        let status = std::process::Command::new("touch")
            .arg("-d")
            .arg(format!("@{secs}"))
            .arg(path)
            .status();
        match status {
            Ok(s) if s.success() => Ok(()),
            _ => Ok(()),
        }
    }

    #[test]
    fn prune_keeps_protected_and_newest_extra_removes_rest() {
        let dir = scratch("prune");
        let bundles = dir.join("bundles");
        // serving (old), last_green (old), + 4 others of varying age.
        bundle_at(&bundles, "serving", 1000);
        bundle_at(&bundles, "lastgreen", 900);
        bundle_at(&bundles, "new1", 10);
        bundle_at(&bundles, "new2", 20);
        bundle_at(&bundles, "old1", 500);
        bundle_at(&bundles, "old2", 600);

        // Keep the 2 protected + the 1 newest non-protected.
        let mut removed = prune_bundles(&bundles, &["serving", "lastgreen"], 1).unwrap();
        removed.sort();

        // Protected always survive even though they are the OLDEST.
        assert!(bundles.join("serving").is_dir(), "serving protected");
        assert!(bundles.join("lastgreen").is_dir(), "last_green protected");
        // The single newest non-protected (new1, age 10) survives.
        assert!(bundles.join("new1").is_dir(), "newest extra kept");
        // The rest are gone.
        assert!(!bundles.join("new2").exists());
        assert!(!bundles.join("old1").exists());
        assert!(!bundles.join("old2").exists());
        assert_eq!(removed, vec!["new2", "old1", "old2"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_sweeps_tmp_litter_and_tolerates_absent_dir() {
        let dir = scratch("prune-litter");
        let bundles = dir.join("bundles");
        bundle_at(&bundles, "keep", 10);
        // A crashed harvest's tmp dir — must be swept regardless of keep count.
        fs::create_dir_all(bundles.join("abc.tmp.99999")).unwrap();

        let removed = prune_bundles(&bundles, &["keep"], 5).unwrap();
        assert!(bundles.join("keep").is_dir());
        assert!(!bundles.join("abc.tmp.99999").exists(), "tmp litter swept");
        assert!(removed.iter().any(|n| n.contains(".tmp.")));

        // Absent bundles dir is a clean no-op (not an error).
        let absent = dir.join("nope");
        assert_eq!(
            prune_bundles(&absent, &[], 3).unwrap(),
            Vec::<String>::new()
        );

        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod worktree_recovery_tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// A real repo with two instance worktrees under `<root>/app/`, mirroring
    /// the daemon's `<state_dir>/app/<name>/worktree` layout. Returns None when
    /// git is unavailable so the suite degrades rather than failing spuriously.
    fn fixture(tag: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
        let mut root = std::env::temp_dir();
        root.push(format!("cargoless-wtrec-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).ok()?;

        let repo = root.join("repo");
        fs::create_dir_all(&repo).ok()?;
        if !git(&repo, &["init", "--quiet"]) {
            return None;
        }
        // Identity must be local: a CI box may have no global git config, and
        // commit would fail for a reason that has nothing to do with the test.
        git(&repo, &["config", "user.email", "t@example.invalid"]);
        git(&repo, &["config", "user.name", "t"]);
        fs::write(repo.join("f.txt"), "x").ok()?;
        git(&repo, &["add", "f.txt"]);
        if !git(&repo, &["commit", "--quiet", "-m", "c1"]) {
            return None;
        }

        let app = root.join("app");
        let alive = app.join("alive").join("worktree");
        let gone = app.join("gone").join("worktree");
        fs::create_dir_all(app.join("alive")).ok()?;
        fs::create_dir_all(app.join("gone")).ok()?;
        let a = alive.to_string_lossy().to_string();
        let g = gone.to_string_lossy().to_string();
        git(&repo, &["worktree", "add", "--detach", &a]);
        git(&repo, &["worktree", "add", "--detach", &g]);
        Some((root, alive, gone))
    }

    /// THE REGRESSION. A worktree destroyed while the daemon runs was never
    /// recreated: `ensure_instance_worktree` only runs at boot and at instance
    /// creation, so every later build failed with "cannot change to <path>: No
    /// such file or directory" — forever, since no retry can clear it. The
    /// build lane burned five generations in four minutes against exactly this
    /// on 2026-08-02 before it was recovered by hand.
    #[test]
    fn a_worktree_deleted_mid_life_is_recreated_by_checkout() {
        let Some((root, _alive, gone)) = fixture("deleted") else {
            return; // no git in this environment
        };
        assert!(gone.join(".git").exists(), "fixture must start healthy");

        // Exactly what the pod self-delete did: the tree is gone from disk,
        // while git still holds its administrative record.
        fs::remove_dir_all(&gone).unwrap();
        assert!(!gone.join(".git").exists());

        recreate_worktree(&gone).expect("a vanished worktree must be rebuilt");
        assert!(
            gone.join(".git").exists(),
            "the worktree must exist again so `git checkout` has a tree to move"
        );
        assert!(
            gone.join("f.txt").exists(),
            "and it must be a real checkout, not an empty dir"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// The prune is load-bearing, not defensive. git still has the deleted
    /// tree registered, and `worktree add` refuses a path it believes is taken
    /// — with "already exists", about a directory that plainly does not. This
    /// asserts recovery works on the SECOND go too, i.e. that the stale record
    /// is actually being cleared rather than tolerated once by luck.
    #[test]
    fn recovery_is_repeatable_so_the_stale_admin_record_is_really_pruned() {
        let Some((root, _alive, gone)) = fixture("repeat") else {
            return;
        };
        for round in 1..=2 {
            fs::remove_dir_all(&gone).unwrap();
            recreate_worktree(&gone).unwrap_or_else(|e| panic!("round {round}: {e}"));
            assert!(gone.join(".git").exists(), "round {round}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// The repo root is derived from a SURVIVING sibling. With none left there
    /// is nothing to derive it from, and the honest answer is a reported
    /// failure — never a guess at a path, which would either do nothing or
    /// create a worktree of the wrong repository.
    #[test]
    fn with_no_surviving_sibling_it_reports_rather_than_guesses() {
        let Some((root, alive, gone)) = fixture("nosibling") else {
            return;
        };
        fs::remove_dir_all(&alive).unwrap();
        fs::remove_dir_all(&gone).unwrap();

        let err = recreate_worktree(&gone).expect_err("must not claim success");
        assert!(
            err.contains("could not be determined"),
            "the error must say WHY recovery was impossible, got: {err}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A healthy worktree is left alone. `checkout` only calls this when
    /// `.git` is absent, but the guard is asserted here too: re-creating a
    /// live tree would discard an in-progress build's state.
    #[test]
    fn a_healthy_worktree_is_not_touched_by_checkout() {
        let Some((root, alive, _gone)) = fixture("healthy") else {
            return;
        };
        fs::write(alive.join("marker"), "keep me").unwrap();

        let head = Command::new("git")
            .arg("-C")
            .arg(&alive)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();

        RealHooks
            .checkout(&alive, &sha)
            .expect("checkout of a healthy worktree must succeed");
        assert!(
            alive.join("marker").exists(),
            "an existing worktree must not be rebuilt out from under a build"
        );
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod failure_excerpt_tests {
    use super::{failure_excerpt, opens_an_error};

    /// THE REGRESSION, with the real 2026-08-03 shape: rustc prints the error
    /// FIRST, then 63 warnings, then a one-line summary. `tail_lines(_, 20)`
    /// therefore captured only warnings + summary and threw the diagnostic away
    /// — on a build whose log exists nowhere else.
    #[test]
    fn the_first_error_survives_63_trailing_warnings() {
        let mut log = String::from("   Compiling triform-physics v0.1.0\n");
        log.push_str("error[E0308]: mismatched types\n");
        log.push_str("   --> physics/src/runtime/executors/app_ops.rs:630:13\n");
        log.push_str("630 |         let execution_id = flow_result.execution_id.clone();\n");
        for i in 0..63 {
            log.push_str(&format!("warning: unused variable: `v{i}`\n"));
        }
        log.push_str("error: could not compile `triform-physics` (lib) due to 1 previous error; 63 warnings emitted\n");

        let got = failure_excerpt(&log, 20);
        assert!(got.contains("E0308"), "the real error must survive: {got}");
        assert!(
            got.contains("app_ops.rs:630"),
            "and so must the file:line that locates it: {got}"
        );
        assert!(
            got.starts_with("error[E0308]"),
            "the excerpt must LEAD with the error, not bury it: {got}"
        );
    }

    /// The trailing summary is NOT a diagnostic. Anchoring on it would keep the
    /// exact useless line the old code kept.
    #[test]
    fn the_could_not_compile_summary_is_not_an_error_anchor() {
        assert!(!opens_an_error(
            "error: could not compile `triform-physics` (lib) due to 1 previous error"
        ));
        assert!(!opens_an_error("error: aborting due to 2 previous errors"));
        assert!(opens_an_error("error[E0432]: unresolved import"));
        assert!(opens_an_error(
            "error: linking with `cc` failed: exit status: 1"
        ));
        assert!(!opens_an_error("warning: unused variable: `x`"));
        assert!(!opens_an_error("note: `#[warn(unused)]` on by default"));
        assert!(!opens_an_error("   = help: maybe it is overwritten"));
    }

    /// A step can fail without rustc — a missing binary, a shell error. There is
    /// no error line to anchor on and the tail IS the right answer there, so the
    /// fallback must still work.
    #[test]
    fn a_non_rustc_failure_falls_back_to_the_tail() {
        let log = "bash: line 3: cargo: command not found\nexit status 127";
        let got = failure_excerpt(log, 20);
        assert!(got.contains("command not found"), "got: {got}");
    }

    /// Indented diagnostics (cargo indents sub-crate output) still anchor.
    #[test]
    fn an_indented_error_still_anchors() {
        let log = "   Compiling x\n  error[E0599]: no method named `foo`\nwarning: later noise";
        let got = failure_excerpt(log, 20);
        assert!(got.contains("E0599"), "got: {got}");
    }

    /// Empty input must not panic (the slice arithmetic is the risk).
    #[test]
    fn empty_input_is_safe() {
        assert_eq!(failure_excerpt("", 20), "");
        assert_eq!(failure_excerpt("\n\n  \n", 20), "");
    }
}
