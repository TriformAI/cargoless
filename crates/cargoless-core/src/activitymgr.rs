//! #12 daemon-tick I/O shell (Model R Stream B) — the pure per-worktree
//! manager over the already-proven [`activity::WtLifecycle`]
//! (`touch`-on-routed-change / `tick`-on-timer / detect the actionable
//! state edges). The live actions those edges drive (drop a worktree's
//! in-RAM overlay; signal [`clustermgr::ClusterLifecycle::deactivate`];
//! re-establish overlay on re-activation) are the serve-loop capstone's
//! concern — the same pure-core-first split that kept the flycheck
//! barrier / cluster-lifecycle cores pure.
//!
//! ## What this adds (mechanical, over a proven core)
//!
//! [`WtLifecycle`] already proves the load-bearing safety per worktree:
//! monotonic `Active → Idle → Deactivated`, only observed activity
//! (`touch`) re-activates, a non-monotonic clock can only *fail to
//! advance* (never regress → never a wrong verdict), and `Deactivated`
//! means "free the overlay" — never "assert a verdict". [`ActivityTracker`]
//! is the mechanical fan-out: one **distinct** `WtLifecycle` per `WtId`,
//! plus the two edge detections the daemon acts on.
//!
//! ### Contained correctness properties (falsifiable; lean on the proven
//! core — NOT a new backstop target, per the #4/#12 classification)
//!
//! * **Per-WT isolation is structural.** Each `WtId` owns its *own*
//!   `WtLifecycle` instance (its own `last_activity` / `idle_since` /
//!   `state`). Worktree V's activity timeline cannot advance, delay, or
//!   reset worktree W's lifecycle — a property of *distinct instances*,
//!   not of timing. (Same argument as `repo::watch::RepoWatchRouter`'s
//!   per-WT debouncers and `clustermgr`'s per-cluster refcount.)
//! * **Deactivated edge is emitted exactly once.** [`tick`](ActivityTracker::tick)
//!   returns a `WtId` only on the *transition into* `Deactivated`
//!   (`prev != Deactivated && post == Deactivated`); a worktree that was
//!   already `Deactivated` is never re-emitted. So the capstone drops the
//!   overlay + signals cluster teardown exactly once per deactivation
//!   (and even if it didn't, `ClusterLifecycle`'s idempotence absorbs a
//!   double — but emit-once is the honest contract, not a reliance on
//!   that safety net).
//! * **Re-activation edge is reported precisely.** [`touch`](ActivityTracker::touch)
//!   returns `true` *iff* it was a `Deactivated → Active` transition —
//!   the one case where the overlay was actually freed and the capstone
//!   must re-establish it. A first-seen WT (created `Active`, nothing
//!   ever dropped) or an already-`Active`/`Idle` touch returns `false`
//!   (no needless overlay rebuild → no spurious RA churn).
//! * **Deterministic emission.** A `BTreeMap` keys the lifecycles, so
//!   `tick` yields deactivated `WtId`s in sorted order regardless of
//!   `touch`/insertion order (same determinism rationale as
//!   `cluster_worktrees`).

use std::collections::BTreeMap;
use std::time::Instant;

use crate::activity::{ActivityConfig, WtActivity, WtLifecycle};
use crate::repo::watch::WtId;

/// Pure per-worktree activity manager. Feed it routed activity
/// ([`touch`](Self::touch)) and timer ticks ([`tick`](Self::tick)); it
/// owns one [`WtLifecycle`] per [`WtId`] and surfaces exactly the two
/// edges the daemon acts on. Pure: the caller supplies the clock
/// (`Instant`), so the fan-out + edge detection is unit-tested
/// deterministically without sleeping or a timer thread.
#[derive(Debug, Clone)]
pub struct ActivityTracker {
    cfg: ActivityConfig,
    /// `WtId → that worktree's own lifecycle`. Created `Active` on the
    /// first `touch` for a worktree; kept tracked thereafter (a
    /// `Deactivated` entry stays so a later `touch` is correctly seen as
    /// the re-activation edge — dropping the entry would lose that).
    wts: BTreeMap<WtId, WtLifecycle>,
}

impl ActivityTracker {
    /// New tracker with the given thresholds (`ActivityConfig::defaults`
    /// / `from_env` upstream).
    pub fn new(cfg: ActivityConfig) -> Self {
        Self {
            cfg,
            wts: BTreeMap::new(),
        }
    }

    /// Record routed activity for `wt` at `now` (one settled
    /// `RepoWatchRouter` batch ⇒ one `touch`).
    ///
    /// Returns `true` **iff** this was a `Deactivated → Active`
    /// re-activation edge — the capstone re-establishes that worktree's
    /// overlay/cluster before its next check only on `true`. First-seen
    /// (created `Active`) or already-`Active`/`Idle` ⇒ `false` (nothing
    /// was dropped; no rebuild).
    pub fn touch(&mut self, wt: impl Into<WtId>, now: Instant) -> bool {
        let wt = wt.into();
        if let Some(lc) = self.wts.get_mut(&wt) {
            let reactivated = lc.state() == WtActivity::Deactivated;
            lc.touch(now);
            return reactivated;
        }
        // First sighting: Active by definition, nothing ever freed.
        self.wts.insert(wt, WtLifecycle::new(now, self.cfg));
        false
    }

    /// Recompute every tracked worktree for `now`; return the `WtId`s
    /// that transitioned **into** `Deactivated` on *this* tick, in
    /// deterministic (`BTreeMap`-sorted) order. Edge-once: an
    /// already-`Deactivated` worktree is never re-emitted.
    pub fn tick(&mut self, now: Instant) -> Vec<WtId> {
        let mut newly_deactivated = Vec::new();
        for (wt, lc) in self.wts.iter_mut() {
            let was_deactivated = lc.state() == WtActivity::Deactivated;
            let post = lc.tick(now);
            if post == WtActivity::Deactivated && !was_deactivated {
                newly_deactivated.push(wt.clone());
            }
        }
        newly_deactivated
    }

    /// Last-computed state for `wt` (no recompute — call [`tick`](Self::tick)
    /// first for a fresh value), or `None` if never touched. Inspection
    /// / cli-status / tests.
    pub fn state(&self, wt: &WtId) -> Option<WtActivity> {
        self.wts.get(wt).map(WtLifecycle::state)
    }

    /// Number of tracked worktrees. Inspection / tests.
    pub fn tracked(&self) -> usize {
        self.wts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn cfg() -> ActivityConfig {
        // Small, exact windows for deterministic edge tests.
        ActivityConfig {
            idle_after: Duration::from_secs(10),
            deactivate_after: Duration::from_secs(20),
        }
    }
    fn wt(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn touch_creates_active_first_seen_returns_false() {
        let mut t = ActivityTracker::new(cfg());
        let t0 = Instant::now();
        // First sighting ⇒ NOT a reactivation (nothing was freed).
        assert!(!t.touch(wt("/r/a"), t0));
        assert_eq!(t.state(&wt("/r/a")), Some(WtActivity::Active));
        assert_eq!(t.tracked(), 1);
        // Re-touch while Active ⇒ still not a reactivation edge.
        assert!(!t.touch(wt("/r/a"), t0 + Duration::from_secs(1)));
    }

    #[test]
    fn tick_advances_active_idle_deactivated_and_emits_edge_once() {
        let mut t = ActivityTracker::new(cfg());
        let t0 = Instant::now();
        t.touch(wt("/r/a"), t0);
        // Before idle_after ⇒ still Active, no edge.
        assert!(t.tick(t0 + Duration::from_secs(5)).is_empty());
        assert_eq!(t.state(&wt("/r/a")), Some(WtActivity::Active));
        // Past idle_after, before deactivate ⇒ Idle, still no edge.
        assert!(t.tick(t0 + Duration::from_secs(12)).is_empty());
        assert_eq!(t.state(&wt("/r/a")), Some(WtActivity::Idle));
        // Past idle_after + deactivate_after ⇒ Deactivated edge emitted.
        let dz = t.tick(t0 + Duration::from_secs(31));
        assert_eq!(dz, vec![wt("/r/a")]);
        // Already Deactivated ⇒ edge NOT re-emitted on the next tick.
        assert!(
            t.tick(t0 + Duration::from_secs(40)).is_empty(),
            "Deactivated edge must fire exactly once"
        );
        assert_eq!(t.state(&wt("/r/a")), Some(WtActivity::Deactivated));
    }

    #[test]
    fn touch_after_deactivated_is_the_reactivation_edge() {
        let mut t = ActivityTracker::new(cfg());
        let t0 = Instant::now();
        t.touch(wt("/r/a"), t0);
        t.tick(t0 + Duration::from_secs(31)); // → Deactivated
        assert_eq!(t.state(&wt("/r/a")), Some(WtActivity::Deactivated));
        // The Deactivated → Active edge: capstone must re-establish
        // overlay ⇒ touch returns true.
        assert!(
            t.touch(wt("/r/a"), t0 + Duration::from_secs(35)),
            "Deactivated→Active is the re-activation edge"
        );
        assert_eq!(t.state(&wt("/r/a")), Some(WtActivity::Active));
        // And the very next touch (still Active) is NOT a reactivation.
        assert!(!t.touch(wt("/r/a"), t0 + Duration::from_secs(36)));
    }

    #[test]
    fn per_wt_isolation_independent_timelines() {
        // V's activity must not advance/delay/reset W's lifecycle.
        let mut t = ActivityTracker::new(cfg());
        let t0 = Instant::now();
        t.touch(wt("/r/v"), t0);
        t.touch(wt("/r/w"), t0);
        // W keeps being active; V goes quiet.
        t.touch(wt("/r/w"), t0 + Duration::from_secs(25));
        let dz = t.tick(t0 + Duration::from_secs(31));
        // Only V deactivates (quiet since t0); W was touched at t0+25 so
        // at t0+31 it is only 6s idle ⇒ still Active.
        assert_eq!(dz, vec![wt("/r/v")]);
        assert_eq!(t.state(&wt("/r/v")), Some(WtActivity::Deactivated));
        assert_eq!(t.state(&wt("/r/w")), Some(WtActivity::Active));
    }

    #[test]
    fn tick_emits_deactivations_in_deterministic_sorted_order() {
        let mut t = ActivityTracker::new(cfg());
        let t0 = Instant::now();
        // Insert in non-sorted order.
        t.touch(wt("/r/z"), t0);
        t.touch(wt("/r/a"), t0);
        t.touch(wt("/r/m"), t0);
        let dz = t.tick(t0 + Duration::from_secs(31));
        assert_eq!(
            dz,
            vec![wt("/r/a"), wt("/r/m"), wt("/r/z")],
            "BTreeMap ⇒ sorted WtId order regardless of touch order"
        );
    }

    #[test]
    fn untracked_state_is_none_and_no_phantom_edges() {
        let mut t = ActivityTracker::new(cfg());
        let t0 = Instant::now();
        assert_eq!(t.state(&wt("/never")), None);
        // tick over an empty tracker ⇒ no edges, no panic.
        assert!(t.tick(t0 + Duration::from_secs(99)).is_empty());
        assert_eq!(t.tracked(), 0);
    }
}
