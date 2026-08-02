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

        // A stale tree at this path would silently contribute its contents to
        // the build. Remove it before git ever looks at it.
        if root.exists() {
            let _ = git(
                &self.repo,
                &["worktree", "remove", "--force", &lossy(&root)],
            );
            std::fs::remove_dir_all(&root)?;
        }
        std::fs::create_dir_all(&self.scratch_parent)?;

        git(
            &self.repo,
            &["worktree", "add", "--detach", &lossy(&root), &self.base_ref],
        )?;

        // Merge each member in submission order. Order is deterministic and
        // reported, which matters when a merge conflicts: "B conflicts when
        // applied after A" is actionable; "the candidate conflicted" is not.
        for member in members {
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
    let out = Command::new("git").current_dir(cwd).args(args).output()?;
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
