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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::topology::WorktreeEntry;
use crate::watcher::{ChangeBatch, Debouncer};

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

/// Repo-scoped watch coalescer (#4 I/O-shell increment, Model R Stream
/// B): the pure composition of the backstop-CLEAR'd [`WtRouter`]
/// (`route_for_monitoring`) with one **per-worktree**
/// [`crate::watcher::Debouncer`]. Raw fs path-changes across the whole
/// repo go in; per-worktree debounced `(WtId, ChangeBatch)`es come out.
///
/// Pure: the caller supplies the clock (`Instant`), so the routing +
/// per-WT coalescing rule is unit-tested deterministically without
/// sleeping or a live `notify` watcher. The thin `notify`-thread adapter
/// that pumps real OS events through this is the serve-loop capstone's
/// concern (it has no standalone consumer until then — the same
/// pure-core-first split that kept the flycheck barrier / cluster
/// lifecycle pure).
///
/// ## Contained correctness properties (falsifiable; lean on proven
/// cores — NOT a new backstop target)
///
/// * **No fabricated WtId.** The *only* `WtId`s that can ever appear in
///   [`poll`](Self::poll) output are exact [`WtRouter::route_for_monitoring`]
///   results. A path that routes to `None` (under no worktree, or
///   `target/`/`.git/` build-noise inside one) is dropped —
///   [`record`](Self::record) returns `false` and creates no debouncer
///   entry. (Leans entirely on the backstop-CLEAR'd router.)
/// * **Per-WT debounce isolation.** Each `WtId` owns its *own*
///   `Debouncer` instance (distinct pending-set + distinct quiet timer),
///   so worktree V's edit churn can never delay, merge into, or suppress
///   worktree W's batch. This is structural — a property of *distinct
///   instances*, not of timing — and is exactly why the multiplexed
///   verdict per WT stays attributable.
/// * **Deterministic emission order.** A `BTreeMap` keys the debouncers,
///   so `poll` yields ready worktrees in sorted `WtId` order regardless
///   of `record` order — no spurious churn from fs-event arrival order
///   (same determinism rationale as `cluster_worktrees`).
#[derive(Debug)]
pub struct RepoWatchRouter {
    router: WtRouter,
    quiet: Duration,
    /// `WtId → that worktree's own debouncer`. Lazily created on the
    /// first routed change for a worktree; a worktree with nothing
    /// pending simply never `poll`s `Some` (cheap to keep tracked).
    debouncers: BTreeMap<WtId, Debouncer>,
}

impl RepoWatchRouter {
    /// Build from a (longest-prefix) [`WtRouter`] and the per-worktree
    /// debounce quiet-window.
    pub fn new(router: WtRouter, quiet: Duration) -> Self {
        Self {
            router,
            quiet,
            debouncers: BTreeMap::new(),
        }
    }

    /// Record one raw fs path-change observed at `now`.
    ///
    /// Routed (under a worktree, not build-noise) ⇒ recorded into *that
    /// worktree's own* debouncer; returns `true`. Unrouted (`None` from
    /// [`WtRouter::route_for_monitoring`]) ⇒ dropped, no debouncer entry
    /// created; returns `false`. No `WtId` is ever fabricated.
    pub fn record(&mut self, abs_path: &Path, now: Instant) -> bool {
        let wt: WtId = match self.router.route_for_monitoring(abs_path) {
            Some(w) => w.to_path_buf(),
            None => return false,
        };
        // Local copy so the `or_insert_with` closure does not borrow
        // `self` while `self.debouncers` is mutably borrowed.
        let quiet = self.quiet;
        self.debouncers
            .entry(wt)
            .or_insert_with(|| Debouncer::new(quiet))
            .record(abs_path.to_path_buf(), now);
        true
    }

    /// Every worktree whose batch has settled as of `now`, drained, in
    /// deterministic (`BTreeMap`-sorted) `WtId` order. A worktree with
    /// nothing pending contributes nothing (never an empty batch).
    pub fn poll(&mut self, now: Instant) -> Vec<(WtId, ChangeBatch)> {
        let mut out = Vec::new();
        for (wt, deb) in self.debouncers.iter_mut() {
            if let Some(batch) = deb.poll(now) {
                out.push((wt.clone(), batch));
            }
        }
        out
    }

    /// Soonest any worktree's batch could next yield, given `now` — the
    /// `min` across worktrees (used to size the capstone watcher
    /// thread's blocking recv timeout). `None` ⇒ nothing pending
    /// anywhere.
    pub fn time_until_ready(&self, now: Instant) -> Option<Duration> {
        self.debouncers
            .values()
            .filter_map(|d| d.time_until_ready(now))
            .min()
    }

    /// The underlying router (inspection / capstone reuse).
    pub fn router(&self) -> &WtRouter {
        &self.router
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "/Users/iggy/Documents/GitHub/tf-multiverse";

    fn wt(path: &str) -> WorktreeEntry {
        // Absolute path: inside `mod tests`, `super` is the `watch`
        // module, NOT `repo` — `crate::repo::topology` is unambiguous
        // (rustc's own E0433 suggestion; matches the file-level
        // `use super::topology::WorktreeEntry` which IS at `repo` scope).
        let mut v = crate::repo::topology::parse_worktree_porcelain(&format!(
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

    // --- RepoWatchRouter (#4 I/O-shell: route + per-WT debounce) --------

    const QUIET: Duration = Duration::from_millis(100);

    fn two_wt_router() -> WtRouter {
        // Main (repo root) + the nested gitignored-subtree worktree.
        WtRouter::new([wt(REPO), wt(&format!("{REPO}/.claude/worktrees/agent-x"))].iter())
    }
    fn main_a() -> String {
        format!("{REPO}/crates/physics/src/a.rs")
    }
    fn main_b() -> String {
        format!("{REPO}/crates/physics/src/b.rs")
    }
    fn nested() -> String {
        format!("{REPO}/.claude/worktrees/agent-x/src/lib.rs")
    }

    #[test]
    fn repo_watch_routes_and_debounces_per_wt() {
        let mut rw = RepoWatchRouter::new(two_wt_router(), QUIET);
        let t0 = Instant::now();
        assert!(rw.record(Path::new(&main_a()), t0));
        // Not quiet yet ⇒ nothing.
        assert!(rw.poll(t0 + Duration::from_millis(99)).is_empty());
        // Quiet elapsed ⇒ (main, [a.rs]).
        let out = rw.poll(t0 + QUIET);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, PathBuf::from(REPO));
        assert_eq!(out[0].1, vec![PathBuf::from(main_a())]);
        // Drained — a second poll yields nothing.
        assert!(rw.poll(t0 + QUIET + QUIET).is_empty());
    }

    #[test]
    fn repo_watch_per_wt_isolation() {
        // V's continued churn must not delay/suppress W's settled batch,
        // and W settles on ITS OWN timer (distinct Debouncer instances).
        let mut rw = RepoWatchRouter::new(two_wt_router(), QUIET);
        let t0 = Instant::now();
        rw.record(Path::new(&main_a()), t0); // main @ t0
        rw.record(Path::new(&nested()), t0); // nested @ t0
        // Nested keeps churning at t0+50 — resets ONLY nested's timer.
        rw.record(Path::new(&nested()), t0 + Duration::from_millis(50));
        // At t0+100: main's quiet elapsed (settled); nested's timer was
        // reset at t0+50 so only 50ms quiet ⇒ NOT ready.
        let out = rw.poll(t0 + QUIET);
        assert_eq!(out.len(), 1, "only main settled; nested still churning");
        assert_eq!(out[0].0, PathBuf::from(REPO));
        // Nested settles independently once ITS quiet elapses.
        let out2 = rw.poll(t0 + Duration::from_millis(150));
        assert_eq!(out2.len(), 1);
        assert_eq!(
            out2[0].0,
            PathBuf::from(format!("{REPO}/.claude/worktrees/agent-x"))
        );
    }

    #[test]
    fn repo_watch_unrouted_dropped_no_fabricated_wtid() {
        let mut rw = RepoWatchRouter::new(two_wt_router(), QUIET);
        let t0 = Instant::now();
        // Under no worktree.
        assert!(!rw.record(Path::new("/etc/passwd"), t0));
        // Build-noise inside a routed worktree (target/ and .git/).
        assert!(!rw.record(Path::new(&format!("{REPO}/target/debug/x")), t0));
        assert!(!rw.record(Path::new(&format!("{REPO}/.git/index")), t0));
        // No debouncer entry was fabricated ⇒ nothing ever settles.
        assert!(rw.poll(t0 + QUIET + QUIET).is_empty());
        assert_eq!(rw.time_until_ready(t0), None);
    }

    #[test]
    fn repo_watch_deterministic_sorted_wtid_order() {
        let mut rw = RepoWatchRouter::new(two_wt_router(), QUIET);
        let t0 = Instant::now();
        // Record nested BEFORE main (reverse of sorted order).
        rw.record(Path::new(&nested()), t0);
        rw.record(Path::new(&main_a()), t0);
        let out = rw.poll(t0 + QUIET);
        assert_eq!(out.len(), 2);
        // BTreeMap ⇒ sorted WtId order regardless of record order:
        // REPO (shorter) sorts before its nested child.
        assert_eq!(out[0].0, PathBuf::from(REPO));
        assert_eq!(
            out[1].0,
            PathBuf::from(format!("{REPO}/.claude/worktrees/agent-x"))
        );
    }

    #[test]
    fn repo_watch_batch_has_only_that_wts_paths() {
        let mut rw = RepoWatchRouter::new(two_wt_router(), QUIET);
        let t0 = Instant::now();
        rw.record(Path::new(&main_a()), t0);
        rw.record(Path::new(&main_b()), t0);
        rw.record(Path::new(&nested()), t0);
        let out = rw.poll(t0 + QUIET);
        assert_eq!(out.len(), 2);
        let agent = format!("{REPO}/.claude/worktrees/agent-x");
        let main = out
            .iter()
            .find(|(w, _)| w.as_path() == Path::new(REPO))
            .unwrap();
        // Debouncer's BTreeSet ⇒ sorted, deduped batch.
        assert_eq!(
            main.1,
            vec![PathBuf::from(main_a()), PathBuf::from(main_b())]
        );
        let nest = out
            .iter()
            .find(|(w, _)| w.as_path() == Path::new(&agent))
            .unwrap();
        assert_eq!(nest.1, vec![PathBuf::from(nested())], "no cross-WT bleed");
    }

    #[test]
    fn repo_watch_time_until_ready_is_min_across_wts() {
        let mut rw = RepoWatchRouter::new(two_wt_router(), QUIET);
        let t0 = Instant::now();
        rw.record(Path::new(&main_a()), t0); // main last_change = t0
        rw.record(Path::new(&nested()), t0 + Duration::from_millis(50)); // nested last_change = t0+50
        // Query @ t0+60: main elapsed 60 ⇒ 40 left; nested elapsed 10 ⇒
        // 90 left; min = 40ms (the sooner-ready worktree).
        assert_eq!(
            rw.time_until_ready(t0 + Duration::from_millis(60)),
            Some(Duration::from_millis(40))
        );
    }

    #[test]
    fn repo_watch_empty_router_drops_all() {
        let mut rw = RepoWatchRouter::new(WtRouter::new(std::iter::empty()), QUIET);
        let t0 = Instant::now();
        assert!(!rw.record(Path::new(&main_a()), t0));
        assert!(rw.poll(t0 + QUIET + QUIET).is_empty());
        assert_eq!(rw.time_until_ready(t0), None);
    }
}
