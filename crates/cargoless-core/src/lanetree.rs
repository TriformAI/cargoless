//! A git-backed [`CandidateTree`](crate::lanedrv::CandidateTree).
//!
//! The lane's whole claim is that **what ships is exactly what compiled**. That
//! only holds if the tree the legs run against is genuinely `base + every
//! member merged` — not base with some files copied over it, and not one
//! member's branch checked out. So this merges, with git, and reports honestly
//! when it cannot.
//!
//! ## Why a scratch worktree, not the analysis root
//!
//! A lane build runs for tens of minutes. Doing it in the shared analysis root
//! would pin that root for the whole build and leave it holding a merge nobody
//! asked for if the process died. `git worktree add --detach` gives a throwaway
//! tree that shares the object store — cheap to make, cheap to abandon.
//!
//! ## A conflict is the member's, an I/O failure is ours
//!
//! A member that cannot merge has not *failed a build*; it has failed to
//! produce a candidate. But it is still **its** failure, and git says so by
//! name before we infer anything — so it is reported as
//! [`MaterializeError::Conflict`] and the lane ejects that member alone.
//! Everything else (fetch failed, worktree could not be created, disk full) is
//! [`MaterializeError::Infra`]: nobody's fault, everyone stays queued.
//!
//! This distinction was originally collapsed — both were an `io::Error`, and
//! the driver called the lot infrastructure. Because infra ejects nobody, an
//! unmergeable member never left the queue and was re-included in every
//! subsequent candidate. Observed in production 2026-08-02: generations 2
//! through 5 each died on the same unmergeable member while the rest of the
//! queue waited behind it. The old reasoning was that the other side of a
//! conflict is equally "responsible" — true of the *content*, but not of the
//! decision: the base is what everyone else already agreed on, so the member
//! that cannot apply to it is the one that has to move.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::lane::LaneMember;
use crate::lanedrv::{CandidateTree, MaterializeError};

/// Monotonic suffix so two candidates never collide on a path, even within one
/// process and even if a previous cleanup failed.
static CANDIDATE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Materialises candidates as detached git worktrees under `scratch_parent`.
pub struct GitCandidateTree {
    /// The repository whose object store the worktrees share.
    pub repo: PathBuf,
    /// Where candidate worktrees are created.
    pub scratch_parent: PathBuf,
    /// The ref every candidate is built on top of — the trunk the lane lands
    /// to. Members are merged onto *this*, resolved fresh for each candidate,
    /// so a candidate is always tested against the current trunk.
    pub base_ref: String,
    /// Identity for the merge commits. Git refuses to commit without one, and
    /// inheriting the daemon's ambient config would make the author depend on
    /// which host ran the build.
    pub author_name: String,
    pub author_email: String,
}

impl GitCandidateTree {
    pub fn new(
        repo: impl Into<PathBuf>,
        scratch_parent: impl Into<PathBuf>,
        base_ref: impl Into<String>,
    ) -> Self {
        Self {
            repo: repo.into(),
            scratch_parent: scratch_parent.into(),
            base_ref: base_ref.into(),
            author_name: "cargoless build lane".to_string(),
            author_email: "lane@cargoless.invalid".to_string(),
        }
    }
}

impl CandidateTree for GitCandidateTree {
    fn materialize(&self, members: &[LaneMember]) -> Result<PathBuf, MaterializeError> {
        let seq = CANDIDATE_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = self
            .scratch_parent
            .join(format!("candidate-{}-{seq}", std::process::id()));

        // Both fallible filesystem steps name the OPERATION and the PATH.
        //
        // `?` on a bare `io::Error` renders through `MaterializeError::Infra`
        // as `candidate tree could not be materialized: No such file or
        // directory (os error 2)` — the trail line observed on generation 13,
        // which says nothing an operator can act on. The candidate root, the
        // scratch parent and the repo all produce identical bytes, and the two
        // calls below have opposite fixes: a failed remove means something is
        // holding the old candidate, a failed create means the state dir is
        // gone or unwritable.
        //
        // A stale tree at this path would silently contribute its contents to
        // the build. Remove it before git ever looks at it.
        remove_stale_candidate(&self.repo, &root)?;
        std::fs::create_dir_all(&self.scratch_parent).map_err(|e| {
            MaterializeError::infra_at(
                "could not create the candidate scratch directory",
                &self.scratch_parent,
                &e,
            )
        })?;

        git(
            &self.repo,
            &["worktree", "add", "--detach", &lossy(&root), &self.base_ref],
        )?;

        // Merge each member in submission order. Order is deterministic and
        // reported, which matters when a merge conflicts: "B conflicts when
        // applied after A" is actionable; "the candidate conflicted" is not.
        for member in members {
            // A member can LAND between enqueue and here — someone merges the
            // PR by hand, or a previous candidate already carried it. Its head
            // is then already an ancestor of the base, and `merge --no-ff`
            // writes an EMPTY commit rather than failing: the candidate builds,
            // goes green, and the lander is handed a roster naming a PR that is
            // already closed. Before landing was armed that was merely untidy;
            // now it is a real merge API call against a merged PR.
            //
            // Reported as a Conflict-shaped ejection with NO files, which the
            // state machine already handles as `Unattributed` — the member
            // leaves the queue, everyone else keeps building. Silently skipping
            // it would make a green candidate that never contained the member
            // look like one that did.
            if is_already_in_base(&root, &member.head) {
                self.release(&root);
                return Err(MaterializeError::Stale {
                    id: member.id.clone(),
                    head: member.head.clone(),
                });
            }
            // Read the conflicting paths BEFORE aborting — `merge --abort`
            // clears the index, and with it the only record of which files
            // collided. Those paths are what makes the ejection attributable
            // instead of a bare "it conflicted".
            if let Err(reason) = merge_one_raw(&root, member, &self.author_name, &self.author_email)
            {
                let files = unmerged_paths(&root);
                let _ = git(&root, &["merge", "--abort"]);
                // Leave nothing half-merged behind: a leaked conflicted
                // worktree would poison the next candidate that reused the path.
                self.release(&root);
                return Err(MaterializeError::Conflict {
                    id: member.id.clone(),
                    files,
                    reason,
                });
            }
        }

        Ok(root)
    }

    fn release(&self, root: &Path) {
        // Best-effort by contract: a leaked scratch dir is a disk problem,
        // never a reason to discard a verdict we already have.
        let _ = git(&self.repo, &["worktree", "remove", "--force", &lossy(root)]);
        if root.exists() {
            let _ = std::fs::remove_dir_all(root);
        }
        // Prune the admin entry too — `git worktree list` growing without
        // bound is how these become hard to reason about at 2am.
        let _ = git(&self.repo, &["worktree", "prune"]);
    }
}

/// Remove a candidate path left by an interrupted generation.
///
/// `git worktree remove --force` normally removes both the administrative
/// entry and the directory. The filesystem cleanup is only a fallback for a
/// path git did not own, so `NotFound` after the git command is success rather
/// than an infrastructure failure. Treating that expected two-step race as
/// fatal caused every retry to fail after cleanup had already succeeded.
fn remove_stale_candidate(repo: &Path, root: &Path) -> Result<(), MaterializeError> {
    if !root.exists() {
        return Ok(());
    }

    let _ = git(repo, &["worktree", "remove", "--force", &lossy(root)]);
    match std::fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(MaterializeError::infra_at(
            "could not remove the stale candidate worktree",
            root,
            &e,
        )),
    }
}

fn merge_one_raw(root: &Path, member: &LaneMember, name: &str, email: &str) -> Result<(), String> {
    // `--no-ff` so every member is a distinguishable commit in the candidate,
    // even when it happens to fast-forward. The lane's premise is that the
    // combination was built; a fast-forward would erase the evidence that this
    // member was part of it.
    let message = format!("lane candidate: {}", member.id);
    let args = [
        "-c",
        &format!("user.name={name}"),
        "-c",
        &format!("user.email={email}"),
        "merge",
        "--no-ff",
        "--no-edit",
        "-m",
        &message,
        &member.head,
    ];
    match git(root, &args) {
        Ok(()) => Ok(()),
        // The caller aborts, so that it can read the unmerged paths first.
        Err(e) => Err(format!(
            "member `{}` ({}) could not be merged onto the candidate: {e}",
            member.id, member.head
        )),
    }
}

/// Is this member's head already contained in the tree we are building on?
///
/// True means the member landed between enqueue and now — merging it would
/// write an empty commit and the roster would name a PR that is already closed.
///
/// Fails CLOSED (returns `false`) when git cannot answer: an unreadable
/// ancestry check must not eject a member that is perfectly fine. The cost of a
/// false `false` is one empty commit in a candidate; the cost of a false `true`
/// is dropping live work.
fn is_already_in_base(root: &Path, head: &str) -> bool {
    // `HEAD` here is the candidate tree as merged SO FAR, not just the base —
    // which also catches the case where an earlier member in this same roster
    // already carried this one's commits.
    Command::new("git")
        .current_dir(root)
        .args(["merge-base", "--is-ancestor", head, "HEAD"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Paths git left unmerged, i.e. the files that actually collided.
///
/// Best-effort by design: this runs on a failure path, and a member that
/// conflicts must be ejected whether or not we can name the files. An empty
/// result only costs a coarser readmission rule (any new head, rather than one
/// touching a conflicting file).
///
/// Must be called BEFORE `merge --abort` — the abort clears the index.
fn unmerged_paths(root: &Path) -> Vec<PathBuf> {
    let Ok(out) = Command::new("git")
        .current_dir(root)
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn git(cwd: &Path, args: &[&str]) -> io::Result<()> {
    // SPAWNING git can fail too, and that failure is the one that reads worst.
    //
    // `Command::output()` returns a bare `No such file or directory (os error
    // 2)` when the CWD does not exist — not only when the binary is missing —
    // and `cwd` here is the repo or a candidate worktree, both of which have
    // been observed to vanish under a PVC fault. Propagating it unannotated is
    // how `outcome=infra reason=candidate tree could not be materialized: No
    // such file or directory (os error 2)` reached the trail naming neither the
    // path nor the step.
    //
    // The kind is preserved so a caller that wants to match `NotFound` still
    // can; only the message grows.
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("could not run `git {:?}` in {}: {e}", args, cwd.display()),
            )
        })?;
    if out.status.success() {
        return Ok(());
    }
    // stderr carries git's own explanation (conflict paths, unknown ref).
    // Passing it through is the difference between an operator diagnosing a
    // conflict in seconds and reading our source to guess what we ran.
    Err(io::Error::other(format!(
        "git {:?} exited {:?}: {}",
        args,
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sh(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with one commit on `main` and a helper to branch off it.
    fn repo(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "cargoless-lanetree-{tag}-{}-{}",
            std::process::id(),
            CANDIDATE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        sh(&root, &["init", "-q", "-b", "main"]);
        sh(&root, &["config", "user.name", "t"]);
        sh(&root, &["config", "user.email", "t@t.invalid"]);
        fs::write(root.join("base.txt"), "base\n").unwrap();
        sh(&root, &["add", "base.txt"]);
        sh(&root, &["commit", "-q", "-m", "base"]);
        root
    }

    /// Commit `file` with `body` on a new branch off main, return its sha.
    fn branch(root: &Path, name: &str, file: &str, body: &str) -> String {
        sh(root, &["checkout", "-q", "-B", name, "main"]);
        fs::write(root.join(file), body).unwrap();
        sh(root, &["add", file]);
        sh(root, &["commit", "-q", "-m", name]);
        let out = Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        sh(root, &["checkout", "-q", "main"]);
        sha
    }

    /// `git worktree remove` deletes the directory itself. The fallback
    /// filesystem removal must therefore accept `NotFound`; otherwise a
    /// successful cleanup is misreported as infrastructure failure and every
    /// lane retry repeats the same false failure.
    #[test]
    fn cleanup_accepts_a_path_already_removed_by_git() {
        let root = repo("cleanup-git-removes-path");
        let stale = root.join(".scratch/stale-candidate");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        let stale_arg = lossy(&stale);
        sh(&root, &["worktree", "add", "--detach", &stale_arg, "main"]);
        assert!(stale.exists(), "the fixture must create a real worktree");

        remove_stale_candidate(&root, &stale)
            .expect("an already-removed fallback path is successful cleanup");

        assert!(!stale.exists(), "the stale candidate must be gone");
        let _ = fs::remove_dir_all(root);
    }

    /// The core claim: the candidate contains EVERY member's changes at once.
    /// This is what "what ships is exactly what compiled" means — if the tree
    /// only carried one member, the lane would be proving nothing about the
    /// combination.
    #[test]
    fn a_candidate_contains_every_member_merged_onto_base() {
        let root = repo("merge-all");
        let a = branch(&root, "a", "a.txt", "a\n");
        let b = branch(&root, "b", "b.txt", "b\n");

        let tree = GitCandidateTree::new(&root, root.join(".scratch"), "main");
        let candidate = tree
            .materialize(&[LaneMember::new("A", &a), LaneMember::new("B", &b)])
            .expect("independent members merge");

        assert_eq!(
            fs::read_to_string(candidate.join("base.txt")).unwrap(),
            "base\n"
        );
        assert_eq!(fs::read_to_string(candidate.join("a.txt")).unwrap(), "a\n");
        assert_eq!(fs::read_to_string(candidate.join("b.txt")).unwrap(), "b\n");

        tree.release(&candidate);
        assert!(!candidate.exists(), "release must remove the worktree");
        let _ = fs::remove_dir_all(root);
    }

    /// A conflict is `Err` — infrastructure — never a silent partial tree.
    /// Returning a tree with only some members merged would let the lane
    /// report a verdict about a candidate that was never assembled.
    #[test]
    fn a_conflicting_member_fails_the_candidate_rather_than_half_merging() {
        let root = repo("conflict");
        let a = branch(&root, "a", "same.txt", "from-a\n");
        let b = branch(&root, "b", "same.txt", "from-b\n");

        let tree = GitCandidateTree::new(&root, root.join(".scratch"), "main");
        let err = tree
            .materialize(&[LaneMember::new("A", &a), LaneMember::new("B", &b)])
            .expect_err("conflicting members cannot produce a candidate");

        let msg = err.to_string();
        assert!(msg.contains('B'), "the failing member must be named: {msg}");

        let _ = fs::remove_dir_all(root);
    }

    /// The tree is disposable and never reused, so two candidates in flight
    /// cannot see each other's files.
    #[test]
    fn two_candidates_get_distinct_roots() {
        let root = repo("distinct");
        let a = branch(&root, "a", "a.txt", "a\n");

        let tree = GitCandidateTree::new(&root, root.join(".scratch"), "main");
        let first = tree.materialize(&[LaneMember::new("A", &a)]).unwrap();
        let second = tree.materialize(&[LaneMember::new("A", &a)]).unwrap();
        assert_ne!(first, second);

        tree.release(&first);
        tree.release(&second);
        let _ = fs::remove_dir_all(root);
    }

    /// The exact bare line that reached the trail on generation 13. Kept as a
    /// constant so both tests below assert against the real thing rather than a
    /// paraphrase of it.
    const OBSERVED_BARE: &str =
        "candidate tree could not be materialized: No such file or directory (os error 2)";

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cargoless-lanetree-{tag}-{}-{}",
            std::process::id(),
            CANDIDATE_SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// DEFECT 3 — a candidate scratch dir that cannot be created must say WHERE.
    ///
    /// Driven through a real syscall failure, not a constructed error: the
    /// scratch parent is placed *inside a regular file*, so `create_dir_all`
    /// fails the way a vanished or unwritable state dir does.
    ///
    /// Without the annotation the whole trail line is the syscall's own words —
    /// `outcome=infra reason=candidate tree could not be materialized: No such
    /// file or directory (os error 2)` — and the candidate root, the scratch
    /// parent and the repo all produce exactly those bytes with different
    /// fixes. A verdict nobody can act on is the same as no verdict.
    #[test]
    fn a_scratch_dir_that_cannot_be_created_names_the_path_and_the_step() {
        let blocker = scratch("blocked");
        fs::write(&blocker, b"not a directory\n").expect("write blocker file");
        // `<file>/sub` cannot be created: the parent is not a directory.
        let scratch_parent = blocker.join("sub");

        let tree = GitCandidateTree::new(scratch("repo"), &scratch_parent, "main");
        let err = tree
            .materialize(&[LaneMember::new("A", "deadbeef")])
            .expect_err("a scratch dir under a regular file cannot be created");

        let rendered = format!("candidate tree could not be materialized: {err}");
        assert!(
            rendered.contains(&scratch_parent.to_string_lossy().into_owned()),
            "the failing PATH must appear, or an operator cannot tell which of \
             the three candidate paths failed: {rendered}"
        );
        assert!(
            rendered.contains("scratch directory"),
            "the STEP must be named — a failed create and a failed remove have \
             opposite fixes: {rendered}"
        );
        assert_ne!(
            rendered, OBSERVED_BARE,
            "this is the generation-13 line that named nothing"
        );

        let _ = fs::remove_file(blocker);
    }

    /// DEFECT 3 — and when `git` cannot even be SPAWNED, say where we tried.
    ///
    /// `Command::output()` returns a bare `os error 2` when the working
    /// directory does not exist, not only when the binary is missing — and the
    /// cwd here is the repo or a candidate worktree, both of which have been
    /// observed to vanish under a PVC fault. That is the same `os error 2` with
    /// a third meaning.
    #[test]
    fn a_git_that_cannot_be_spawned_names_the_directory() {
        let missing_repo = scratch("gone-repo");
        // Deliberately never created. The scratch parent IS creatable, so this
        // gets past `create_dir_all` and fails at the `git worktree add` spawn.
        let scratch_parent = scratch("gone-scratch");
        let tree = GitCandidateTree::new(&missing_repo, &scratch_parent, "main");
        let err = tree
            .materialize(&[LaneMember::new("A", "deadbeef")])
            .expect_err("git cannot run in a directory that does not exist");

        let rendered = format!("candidate tree could not be materialized: {err}");
        assert!(
            rendered.contains(&missing_repo.to_string_lossy().into_owned()),
            "the directory git could not run in must be named: {rendered}"
        );
        assert_ne!(
            rendered, OBSERVED_BARE,
            "a failed spawn must not render as the bare generation-13 line"
        );

        let _ = fs::remove_dir_all(scratch_parent);
    }

    /// An empty candidate is still a legitimate tree: it is the base. The lane
    /// never asks for one today, but returning an error would turn a harmless
    /// edge into an infra failure that holds a queue.
    #[test]
    fn an_empty_member_list_yields_the_base_tree() {
        let root = repo("empty");
        let tree = GitCandidateTree::new(&root, root.join(".scratch"), "main");
        let candidate = tree.materialize(&[]).expect("base alone is a valid tree");
        assert_eq!(
            fs::read_to_string(candidate.join("base.txt")).unwrap(),
            "base\n"
        );
        tree.release(&candidate);
        let _ = fs::remove_dir_all(root);
    }
}
