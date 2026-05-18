//! Per-worktree change routing + the gitignore-**inversion** (Model R #4 /
//! `D-FLEET-SHARED-DAEMON` §4 — the pure, config/notify-INDEPENDENT core).
//!
//! ## The two opposite gitignore concerns (§4)
//!
//! A repo-scoped daemon treats `.gitignore` in **opposite** ways for two
//! different questions:
//!
//! | Concern | Treatment |
//! |---|---|
//! | *What is in base-RA's workspace?* | **Respect** `.gitignore` — `.claude/worktrees/*` is correctly excluded; base RA never tries to compile a worktree checkout as part of base. (This axis is the unchanged v0 `crate::watcher` behaviour; not this module.) |
//! | *What worktrees do we monitor?* | **Override** `.gitignore` — `git worktree list` is the ground truth; we consciously walk INTO those paths *even though* `.claude/worktrees/` is conventionally gitignored. That is the entire point of monitoring them. |
//!
//! This module owns the **second** axis: routing a filesystem change to the
//! worktree that owns it, using the discovered worktree paths (from #174
//! [`super::topology`]) as ground truth — **not** gitignore. 96.6% of the
//! operator's worktrees are nested under `<repo>/.claude/worktrees/` (a
//! dot-prefixed, conventionally-gitignored subtree); without this inversion
//! the daemon would be blind to them.
//!
//! The **only** filter that still applies to a routed worktree path is the
//! universal build-noise floor (`target/`, `.git/`) — those are never a
//! legitimate watch target in *any* Cargo tree, which is exactly the
//! unconditional invariant [`crate::watcher::IgnoreRules`] already enforces
//! (it ignores `target`/`.git` anywhere, even against `!`-negation). We
//! reuse that invariant rather than re-deriving it.
//!
//! ## House purity seam
//!
//! [`WtRouter`] is pure (no I/O, no notify, no config) and exhaustively
//! unit-tested — the longest-matching-prefix rule is the same one
//! `cargoless::cratemap::CrateMap::crate_of` uses (a nested worktree dir
//! beats its repo-root ancestor). The notify-wiring shell (one base
//! watcher + one per non-nested worktree, composing the existing
//! `crate::watcher` `Debouncer`/`ChangeBatch`, emitting
//! `(WtId, ChangeBatch)`) is the thin I/O layer — a follow-up increment
//! (notify spawn is not deterministically unit-testable, the established
//! `list_worktrees` vs `parse_worktree_porcelain` split); its routing
//! behaviour is fully covered by the pure tests here.

use std::path::{Path, PathBuf};

use super::topology::WorktreeEntry;

/// Stable identifier for a worktree in routed events: its absolute root
/// path (as `git worktree list` reported it). Path-keyed, not an index —
/// no invalidation when the discovered set changes (matches how
/// [`super::RepoScope`] / [`super::topology`] already key on paths).
pub type WtId = PathBuf;

/// Pure longest-prefix router: a filesystem path → the worktree that owns
/// it. Built from the discovered [`WorktreeEntry`] set; **gitignore is
/// deliberately not consulted** (the §4 inversion — `git worktree list`
/// is ground truth). No I/O.
#[derive(Debug, Clone, Default)]
pub struct WtRouter {
    /// Worktree roots, **longest path first**, so [`Self::route`] takes
    /// the first (most-specific) match — a nested
    /// `<repo>/.claude/worktrees/x` beats the repo-root `<repo>` itself.
    roots: Vec<PathBuf>,
}

impl WtRouter {
    /// Build from the discovered worktrees. Order-independent in: sorted
    /// longest-first internally so route() is most-specific-wins
    /// regardless of `git worktree list` output order.
    pub fn new<'a, I>(worktrees: I) -> Self
    where
        I: IntoIterator<Item = &'a WorktreeEntry>,
    {
        let mut roots: Vec<PathBuf> = worktrees.into_iter().map(|w| w.path.clone()).collect();
        // Longest path (most components) first; ties broken by reverse
        // lexical so the order is total + deterministic (tests depend on
        // it; `route` only needs longest-first, the tiebreak is cosmetic).
        roots.sort_by(|a, b| {
            b.components()
                .count()
                .cmp(&a.components().count())
                .then(b.cmp(a))
        });
        Self { roots }
    }

    /// `true` when no worktree was discovered (single-worktree / non-repo
    /// invocation) — caller falls back to the v0 single-root watcher.
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// The worktree that owns `abs_path` (longest matching root prefix),
    /// or `None` when the path is under no known worktree.
    ///
    /// **Gitignore is not consulted** — a path under a known worktree
    /// routes to it even when the repo-root `.gitignore` would exclude
    /// that subtree (the §4 inversion: that is exactly the
    /// `.claude/worktrees/*` case for 96.6% of the fleet).
    pub fn route(&self, abs_path: &Path) -> Option<&Path> {
        self.roots
            .iter()
            .find(|root| abs_path.starts_with(root))
            .map(PathBuf::as_path)
    }

    /// Routing **for monitoring**: the owning worktree for `abs_path`,
    /// applying *only* the universal build-noise floor (`target/`,
    /// `.git/` anywhere under the worktree are never a watch target —
    /// the unconditional [`crate::watcher::IgnoreRules`] invariant). This
    /// is the §4 inversion in one call: gitignore is overridden (a
    /// gitignored worktree subtree IS monitored), but compiler/VCS noise
    /// is still floored out so a `cargo` build inside a worktree does not
    /// storm the daemon.
    ///
    /// `None` ⇒ either not under any known worktree, or it is build noise
    /// within one (both correctly "do not route as a content change").
    pub fn route_for_monitoring(&self, abs_path: &Path) -> Option<&Path> {
        let wt = self.route(abs_path)?;
        // Path components *below the worktree root* — the worktree root
        // itself legitimately lives anywhere (e.g. a `tf-multiverse-x`
        // sibling); only noise *inside* it is floored.
        let rel = abs_path.strip_prefix(wt).unwrap_or(abs_path);
        let is_noise = rel.components().any(|c| {
            matches!(
                c,
                std::path::Component::Normal(s)
                    if s == std::ffi::OsStr::new("target")
                        || s == std::ffi::OsStr::new(".git")
            )
        });
        if is_noise { None } else { Some(wt) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "/Users/iggy/Documents/GitHub/tf-multiverse";

    fn wt(path: &str) -> WorktreeEntry {
        // `super` = the `repo` module; topology is its sibling submodule.
        let mut v = super::topology::parse_worktree_porcelain(&format!(
            "worktree {path}\nHEAD a\nbranch refs/heads/x\n"
        ));
        v.pop().expect("one entry")
    }

    fn router() -> WtRouter {
        // Main + a nested (gitignored-subtree) + a sibling + an "other".
        WtRouter::new(
            [
                wt(REPO),
                wt(&format!("{REPO}/.claude/worktrees/agent-x")),
                wt("/Users/iggy/Documents/GitHub/tf-multiverse-flat"),
                wt("/var/tmp/special-checkout"),
            ]
            .iter(),
        )
    }

    #[test]
    fn nested_path_routes_to_nested_wt_not_repo_root() {
        // THE gitignore-inversion correctness: a change inside the
        // conventionally-gitignored `.claude/worktrees/agent-x` MUST
        // route to agent-x, NOT to Main (the repo-root, which is also a
        // prefix). Without longest-prefix + ignore-override the daemon
        // would either miss it or misattribute it to base.
        let r = router();
        assert_eq!(
            r.route(Path::new(&format!(
                "{REPO}/.claude/worktrees/agent-x/src/orbit.rs"
            ))),
            Some(Path::new(&format!("{REPO}/.claude/worktrees/agent-x")))
        );
    }

    #[test]
    fn repo_root_file_routes_to_main() {
        let r = router();
        assert_eq!(
            r.route(Path::new(&format!("{REPO}/crates/physics/src/a.rs"))),
            Some(Path::new(REPO))
        );
    }

    #[test]
    fn sibling_and_other_route_to_themselves() {
        let r = router();
        assert_eq!(
            r.route(Path::new(
                "/Users/iggy/Documents/GitHub/tf-multiverse-flat/src/x.rs"
            )),
            Some(Path::new("/Users/iggy/Documents/GitHub/tf-multiverse-flat"))
        );
        assert_eq!(
            r.route(Path::new("/var/tmp/special-checkout/lib.rs")),
            Some(Path::new("/var/tmp/special-checkout"))
        );
    }

    #[test]
    fn path_under_no_worktree_is_none() {
        let r = router();
        assert_eq!(r.route(Path::new("/etc/passwd")), None);
        assert_eq!(
            r.route(Path::new("/Users/iggy/Documents/GitHub/other-repo/x.rs")),
            None
        );
    }

    #[test]
    fn longest_prefix_total_order_is_deterministic() {
        // Build order must not change routing — construct reversed and
        // assert the nested-vs-main precedence still holds.
        let r = WtRouter::new([wt(&format!("{REPO}/.claude/worktrees/agent-x")), wt(REPO)].iter());
        assert_eq!(
            r.route(Path::new(&format!(
                "{REPO}/.claude/worktrees/agent-x/deep/a.rs"
            ))),
            Some(Path::new(&format!("{REPO}/.claude/worktrees/agent-x")))
        );
    }

    #[test]
    fn monitoring_floors_target_and_git_noise_within_a_wt() {
        // The inversion overrides gitignore for *content*, but the
        // universal build/VCS-noise floor still applies INSIDE a routed
        // worktree (a `cargo` build in agent-x must not storm the daemon).
        let r = router();
        let base = format!("{REPO}/.claude/worktrees/agent-x");
        // Real source under a gitignored subtree → monitored (inversion).
        assert_eq!(
            r.route_for_monitoring(Path::new(&format!("{base}/src/lib.rs"))),
            Some(Path::new(&base))
        );
        // target/ and .git/ inside the worktree → floored (None).
        assert_eq!(
            r.route_for_monitoring(Path::new(&format!("{base}/target/debug/build/x"))),
            None
        );
        assert_eq!(
            r.route_for_monitoring(Path::new(&format!("{base}/.git/index"))),
            None
        );
        // Not under any worktree → None.
        assert_eq!(
            r.route_for_monitoring(Path::new("/tmp/elsewhere/x.rs")),
            None
        );
    }

    #[test]
    fn empty_router_is_empty_and_routes_nothing() {
        let r = WtRouter::new(std::iter::empty());
        assert!(r.is_empty());
        assert_eq!(r.route(Path::new("/anything")), None);
        assert_eq!(r.route_for_monitoring(Path::new("/anything")), None);
    }
}
