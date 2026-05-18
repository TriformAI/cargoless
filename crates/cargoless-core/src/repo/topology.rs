//! Worktree topology: `git worktree list --porcelain` enumeration +
//! nested/sibling/other classification (Model R #4 / `D-FLEET-SHARED-DAEMON`
//! §4, task #174 — the config-INDEPENDENT pure core).
//!
//! ## Why this is the canonical enumeration source
//!
//! `D-FLEET-SHARED-DAEMON` §4: a repo-scoped daemon discovers its worktrees
//! via `git worktree list` "regardless of nesting/sibling placement". The
//! **`--porcelain`** variant is the parse-stable contract: unlike the human
//! form, it emits the worktree path **verbatim to end-of-line** (no
//! quoting, no escaping, no truncation), one attribute per line, records
//! separated by a blank line. So a path containing spaces parses correctly
//! by a simple prefix-strip — which is exactly why porcelain (not the
//! human form) is the source of truth here.
//!
//! ## House purity seam
//!
//! Mirrors `cargoless::config`: a **pure, filesystem-free, exhaustively
//! unit-tested** core ([`parse_worktree_porcelain`], [`classify`]) plus a
//! thin non-pure runner ([`list_worktrees`]) that only spawns `git` and
//! delegates to the pure parser. Every behavioural assertion is on the
//! pure functions; the runner is a ~6-line glue (a `git` spawn cannot be
//! unit-tested deterministically — same split as
//! `config::detect_from_cargo_toml` vs `Config::resolve`).
//!
//! **Scope (task #174):** parser + classifier + the thin git-spawn
//! enumerator only. NO config (Stream A↔B seam), NO daemon lifecycle
//! (#3/#157), NO transport (#10), NO file-watching (#4-watcher). The
//! `WtClass` here is *consumed by* #4's per-WT routing later; it is not
//! that routing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// One worktree as reported by `git worktree list --porcelain`.
///
/// The porcelain grammar per record (the `worktree` line is always first
/// and always present; the rest are optional and order-tolerated here for
/// forward-compatibility, the house pattern):
///
/// ```text
/// worktree <absolute-path>      # always; path verbatim to EOL
/// HEAD <object-id>              # absent for a `bare` worktree
/// branch <ref>                  # absent when detached/bare; e.g. refs/heads/x
/// bare                          # this is the bare repository
/// detached                      # HEAD is detached (no branch)
/// locked [<reason>]             # administratively locked
/// prunable [<reason>]           # eligible for `git worktree prune`
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Absolute path git reports for the worktree (verbatim; may contain
    /// spaces). Not canonicalised here — [`classify`] is pure and works on
    /// the paths as given; the caller canonicalises if it needs symlink
    /// resolution (kept out of the pure core on purpose).
    pub path: PathBuf,
    /// `HEAD` object id, or `None` for a bare worktree.
    pub head: Option<String>,
    /// `branch` ref verbatim (e.g. `refs/heads/feature`), or `None` when
    /// detached/bare. Use [`WorktreeEntry::branch_short`] for the short
    /// name.
    pub branch: Option<String>,
    /// This entry is the bare repository.
    pub bare: bool,
    /// HEAD is detached (no branch).
    pub detached: bool,
    /// Administratively locked (`git worktree lock`).
    pub locked: bool,
    /// Eligible for `git worktree prune` (its checkout dir vanished).
    pub prunable: bool,
}

impl WorktreeEntry {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            head: None,
            branch: None,
            bare: false,
            detached: false,
            locked: false,
            prunable: false,
        }
    }

    /// The branch short name (`refs/heads/x` → `x`), or `None` when
    /// detached/bare. Other ref namespaces are returned verbatim.
    pub fn branch_short(&self) -> Option<&str> {
        self.branch
            .as_deref()
            .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b))
    }
}

/// Parse `git worktree list --porcelain` output. **Pure** (no I/O), so it
/// is exhaustively unit-tested without a git repo.
///
/// Robustness contract (house pattern — tolerant + forward-compatible):
/// * a `worktree ` line starts a new record (and flushes the previous);
/// * a blank line is a soft record separator (redundant with the next
///   `worktree`, accepted either way);
/// * unknown attribute lines are ignored (a future git attribute must not
///   break enumeration);
/// * the path is everything after `worktree ` verbatim (porcelain does not
///   quote — paths with spaces are preserved);
/// * a leading attribute with no preceding `worktree` line is ignored
///   (malformed input degrades to "fewer entries", never a panic).
pub fn parse_worktree_porcelain(output: &str) -> Vec<WorktreeEntry> {
    let mut out: Vec<WorktreeEntry> = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;
    for raw in output.lines() {
        let line = raw.trim_end_matches(['\r', '\n']);
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(done) = cur.take() {
                out.push(done);
            }
            cur = Some(WorktreeEntry::new(PathBuf::from(path)));
            continue;
        }
        if line.is_empty() {
            // Soft separator — the next `worktree` also flushes, so this
            // is belt-and-braces; harmless if records aren't blank-split.
            continue;
        }
        let Some(e) = cur.as_mut() else {
            // Attribute with no open record (malformed) — skip, don't panic.
            continue;
        };
        if let Some(oid) = line.strip_prefix("HEAD ") {
            e.head = Some(oid.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            e.branch = Some(b.to_string());
        } else if line == "bare" {
            e.bare = true;
        } else if line == "detached" {
            e.detached = true;
        } else if line == "locked" || line.starts_with("locked ") {
            e.locked = true;
        } else if line == "prunable" || line.starts_with("prunable ") {
            e.prunable = true;
        }
        // else: unknown attribute — ignored (forward-compatible).
    }
    if let Some(done) = cur.take() {
        out.push(done);
    }
    out
}

/// Where a worktree sits relative to the repo root — the topology axis
/// `D-FLEET-SHARED-DAEMON` §1/§4 cares about (it drives per-WT watcher
/// routing in #4: nested ones are caught by the base subtree watcher;
/// non-nested ones each need a dedicated watcher).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WtClass {
    /// The base worktree itself — `wt_path == repo_root`. Distinguished
    /// from `Other` so downstream (#4 base-RA rooting) never miscounts the
    /// repo root as a peripheral worktree.
    Main,
    /// Nested under `<repo_root>/.claude/worktrees/` — Claude Code's
    /// agent-worktree convention (the dominant class at the operator's
    /// scale; caught for free by a base-subtree watcher).
    Nested,
    /// A sibling directory `<repo_parent>/<repo_name>-*` (manually-managed
    /// worktrees; each needs its own watcher — outside the base subtree).
    Sibling,
    /// Anywhere else (special-case checkouts; each needs its own watcher).
    Other,
}

/// Classify `wt_path` relative to `repo_root`. **Pure** — operates on the
/// paths exactly as given (the caller canonicalises if it needs symlink
/// resolution; keeping fs access out makes this deterministically
/// unit-testable with literal paths, the house pattern).
///
/// Precedence (first match wins): `Main` → `Nested` → `Sibling` → `Other`.
pub fn classify(repo_root: &Path, wt_path: &Path) -> WtClass {
    if wt_path == repo_root {
        return WtClass::Main;
    }
    // Nested: under <repo_root>/.claude/worktrees/
    let nested_anchor = repo_root.join(".claude").join("worktrees");
    if wt_path.starts_with(&nested_anchor) {
        return WtClass::Nested;
    }
    // Sibling: same parent dir as repo_root, and the leaf name is
    // `<repo_name>-<something>` (the `tf-multiverse-flat` convention).
    if let (Some(rp), Some(wp)) = (repo_root.parent(), wt_path.parent()) {
        if rp == wp {
            if let (Some(rn), Some(wn)) = (
                repo_root.file_name().and_then(|s| s.to_str()),
                wt_path.file_name().and_then(|s| s.to_str()),
            ) {
                // `<repo_name>-…` (strict prefix incl. the hyphen) and not
                // the repo itself (the `wt_path == repo_root` Main case is
                // already handled above; the `wn != rn` guard defends a
                // same-parent path that merely *equals* the repo name).
                let pfx = format!("{rn}-");
                if wn != rn && wn.starts_with(&pfx) {
                    return WtClass::Sibling;
                }
            }
        }
    }
    WtClass::Other
}

/// Thin non-pure enumerator: spawn `git -C <repo> worktree list
/// --porcelain` and delegate to [`parse_worktree_porcelain`]. The only
/// I/O in this module; config-INDEPENDENT (takes a repo path arg, reads no
/// config). Not unit-tested (a `git` spawn is not deterministically
/// unit-testable — exactly the `Config::resolve` vs
/// `detect_from_cargo_toml` split); its parsing behaviour is covered by
/// the pure-parser tests. A non-zero `git` exit / spawn failure yields an
/// `Err` the caller (the #3 daemon, later) decides how to surface — this
/// function never panics.
pub fn list_worktrees(repo: &Path) -> std::io::Result<Vec<WorktreeEntry>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "`git -C {} worktree list --porcelain` exited {}",
            repo.display(),
            out.status
        )));
    }
    Ok(parse_worktree_porcelain(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- parse_worktree_porcelain (pure, exhaustive) -----

    #[test]
    fn parses_canonical_multi_record_output() {
        // The shape `git worktree list --porcelain` actually emits: main
        // + a branch worktree + a detached one + a bare one, blank-split.
        let out = "\
worktree /Users/iggy/Documents/GitHub/tf-multiverse
HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
branch refs/heads/dev

worktree /Users/iggy/Documents/GitHub/tf-multiverse/.claude/worktrees/agent-x
HEAD bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
branch refs/heads/agent/x

worktree /Users/iggy/Documents/GitHub/tf-multiverse-flat
HEAD cccccccccccccccccccccccccccccccccccccccc
detached

worktree /Users/iggy/Documents/GitHub/bare-repo.git
bare
";
        let v = parse_worktree_porcelain(out);
        assert_eq!(v.len(), 4);

        assert_eq!(
            v[0].path,
            PathBuf::from("/Users/iggy/Documents/GitHub/tf-multiverse")
        );
        assert_eq!(v[0].branch_short(), Some("dev"));
        assert!(!v[0].detached && !v[0].bare);

        assert_eq!(v[1].branch.as_deref(), Some("refs/heads/agent/x"));
        assert_eq!(v[1].branch_short(), Some("agent/x"));

        assert!(v[2].detached);
        assert_eq!(v[2].branch, None);
        assert_eq!(v[2].branch_short(), None);

        assert!(v[3].bare);
        assert_eq!(v[3].head, None);
        assert_eq!(v[3].branch_short(), None);
    }

    #[test]
    fn path_with_spaces_is_verbatim_to_eol() {
        // Porcelain does NOT quote — a space in the path must survive. The
        // single most important reason to parse `--porcelain` not human.
        let out = "worktree /tmp/My Repo/wt one\nHEAD dddddddddddddddddddddddddddddddddddddddd\nbranch refs/heads/x\n";
        let v = parse_worktree_porcelain(out);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, PathBuf::from("/tmp/My Repo/wt one"));
        assert_eq!(v[0].branch_short(), Some("x"));
    }

    #[test]
    fn last_record_without_trailing_blank_is_flushed() {
        // Real git output ends the final record with just a newline, no
        // trailing blank line — the parser must still flush it.
        let out =
            "worktree /a\nHEAD eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\nbranch refs/heads/main\n";
        let v = parse_worktree_porcelain(out);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, PathBuf::from("/a"));
    }

    #[test]
    fn locked_and_prunable_with_and_without_reason() {
        let out = "\
worktree /a
HEAD ffffffffffffffffffffffffffffffffffffffff
branch refs/heads/a
locked

worktree /b
HEAD 1111111111111111111111111111111111111111
detached
prunable gitdir file points to non-existent location

worktree /c
HEAD 2222222222222222222222222222222222222222
branch refs/heads/c
locked machine relocated
";
        let v = parse_worktree_porcelain(out);
        assert_eq!(v.len(), 3);
        assert!(v[0].locked && !v[0].prunable);
        assert!(v[1].prunable && !v[1].locked);
        assert!(v[2].locked, "locked WITH a reason still sets the flag");
    }

    #[test]
    fn unknown_attribute_lines_are_ignored_forward_compatible() {
        // A hypothetical future git attribute must not break enumeration.
        let out = "worktree /a\nHEAD 3333333333333333333333333333333333333333\nbranch refs/heads/a\nfuture-attr something\n";
        let v = parse_worktree_porcelain(out);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].branch_short(), Some("a"));
    }

    #[test]
    fn degrades_on_malformed_input_never_panics() {
        assert_eq!(parse_worktree_porcelain(""), Vec::new());
        assert_eq!(parse_worktree_porcelain("\n\n\n"), Vec::new());
        // Attribute lines with no opening `worktree ` — skipped, no panic.
        assert_eq!(
            parse_worktree_porcelain("HEAD abc\nbranch refs/heads/x\n"),
            Vec::new()
        );
        // `worktree` with an empty path is still a record (path = "").
        let v = parse_worktree_porcelain("worktree \nbare\n");
        assert_eq!(v.len(), 1);
        assert!(v[0].bare);
        assert_eq!(v[0].path, PathBuf::from(""));
    }

    #[test]
    fn crlf_line_endings_tolerated() {
        let out = "worktree /a\r\nHEAD 4444444444444444444444444444444444444444\r\nbranch refs/heads/a\r\n";
        let v = parse_worktree_porcelain(out);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, PathBuf::from("/a"));
        assert_eq!(v[0].branch_short(), Some("a"));
    }

    // ----- classify (pure, exhaustive) -----

    const REPO: &str = "/Users/iggy/Documents/GitHub/tf-multiverse";

    #[test]
    fn classify_main_is_the_repo_root_itself() {
        assert_eq!(classify(Path::new(REPO), Path::new(REPO)), WtClass::Main);
    }

    #[test]
    fn classify_nested_under_claude_worktrees() {
        assert_eq!(
            classify(
                Path::new(REPO),
                Path::new(&format!("{REPO}/.claude/worktrees/agent-flat")),
            ),
            WtClass::Nested
        );
        // Deeply nested still Nested.
        assert_eq!(
            classify(
                Path::new(REPO),
                Path::new(&format!("{REPO}/.claude/worktrees/a/b/c")),
            ),
            WtClass::Nested
        );
    }

    #[test]
    fn classify_sibling_same_parent_with_repo_name_prefix() {
        assert_eq!(
            classify(
                Path::new(REPO),
                Path::new("/Users/iggy/Documents/GitHub/tf-multiverse-flat"),
            ),
            WtClass::Sibling
        );
        assert_eq!(
            classify(
                Path::new(REPO),
                Path::new("/Users/iggy/Documents/GitHub/tf-multiverse-check-queue"),
            ),
            WtClass::Sibling
        );
    }

    #[test]
    fn classify_sibling_requires_the_hyphen_boundary() {
        // `tf-multiversed` shares the parent and the textual prefix
        // `tf-multiverse` but is NOT `<repo_name>-…` (no hyphen boundary)
        // — must be Other, not Sibling (a false-sibling would route a
        // foreign repo's worktree into this daemon).
        assert_eq!(
            classify(
                Path::new(REPO),
                Path::new("/Users/iggy/Documents/GitHub/tf-multiversed"),
            ),
            WtClass::Other
        );
    }

    #[test]
    fn classify_other_for_unrelated_and_far_locations() {
        assert_eq!(
            classify(Path::new(REPO), Path::new("/tmp/some/checkout")),
            WtClass::Other
        );
        // Same parent but unrelated name.
        assert_eq!(
            classify(
                Path::new(REPO),
                Path::new("/Users/iggy/Documents/GitHub/other-project"),
            ),
            WtClass::Other
        );
    }

    #[test]
    fn classify_precedence_main_beats_nested_anchor_collision() {
        // Defensive: the repo root itself is Main even though it is the
        // prefix of its own `.claude/worktrees` anchor.
        assert_eq!(classify(Path::new(REPO), Path::new(REPO)), WtClass::Main);
    }

    #[test]
    fn end_to_end_parse_then_classify_the_three_classes() {
        // The §1 topology shape in miniature: main + nested + sibling +
        // other, parsed then classified — the exact pipeline #4 consumes.
        let out = format!(
            "worktree {REPO}\nHEAD a\nbranch refs/heads/dev\n\n\
             worktree {REPO}/.claude/worktrees/agent-x\nHEAD b\nbranch refs/heads/agent/x\n\n\
             worktree /Users/iggy/Documents/GitHub/tf-multiverse-flat\nHEAD c\ndetached\n\n\
             worktree /var/tmp/special\nHEAD d\nbranch refs/heads/s\n"
        );
        let v = parse_worktree_porcelain(&out);
        assert_eq!(v.len(), 4);
        let classes: Vec<WtClass> = v
            .iter()
            .map(|e| classify(Path::new(REPO), &e.path))
            .collect();
        assert_eq!(
            classes,
            vec![
                WtClass::Main,
                WtClass::Nested,
                WtClass::Sibling,
                WtClass::Other,
            ]
        );
    }
}
