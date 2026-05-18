//! #12 — per-worktree activity lifecycle (Model R Stream B,
//! `D-FLEET-SHARED-DAEMON` §3.2 / §12 / §14).
//!
//! ## What this is, and how it differs from Tier-4 idle-evict
//!
//! Model R's promise (§3.2): *dormant worktrees cost zero resources.* A
//! repo-scoped daemon discovers hundreds of worktrees but only a small
//! active subset is ever being edited at once. This module is the pure
//! **per-worktree state lifecycle** that makes "dormant ⇒ free" true:
//!
//! ```text
//! Active ──(no activity ≥ idle_after)──▶ Idle
//!   ▲                                     │
//!   │                          (still idle ≥ deactivate_after)
//!   │                                     ▼
//!   └──────────(any activity)──────── Deactivated
//! ```
//!
//! `Deactivated` ⇒ the daemon may drop that worktree's **in-RAM overlay
//! state** (the per-WT bookkeeping Stream C maintains). It explicitly
//! does **not** drop the worktree's `solo/<HW>` cache — that is
//! content-addressed and lives on disk (`D-FLEET` §5), so it costs
//! nothing to keep and makes re-activation instant when the overlay-set
//! hash is unchanged.
//!
//! This is **distinct from `crate::idle` (Tier-4)**: Tier-4 evicts the
//! single *shared base rust-analyzer* process during fleet-wide quiet
//! gaps; this governs *each worktree's lightweight overlay state*
//! independently. They compose (a fully-idle fleet ⇒ every WT
//! Deactivated *and* Tier-4 evicts base RA) but are orthogonal axes.
//!
//! ## No-wrong-verdict invariant (load-bearing — mirrors `crate::idle`)
//!
//! Deactivation only ever *delays* a future check; it can never change a
//! verdict. The authoritative green/red is the cargo-check / F8-redo tier
//! (a transient subprocess, zero resident cost). Dropping a worktree's
//! overlay state cannot make a tree wrongly green or hide a red: the next
//! activity re-activates the worktree and the overlay-set is recomputed
//! from `git diff base..<wt>` at its CURRENT content (the `solo` cache
//! short-circuits to the same verdict iff the hash is unchanged — a
//! content-addressed identity, never a stale guess). Worst case of an
//! over-eager deactivation is a slightly slower next check, never a
//! wrong/missing one; `never-publish-red` is untouched.
//!
//! ## Pure + clock-injected (house pattern)
//!
//! [`WtLifecycle::tick`]/[`touch`](WtLifecycle::touch) take `now:
//! Instant` (the `watcher::Debouncer` / `model` pattern) so the state
//! machine is exhaustively unit-tested with synthetic time — no sleeps,
//! deterministic. The daemon's wiring (calling `touch` on a routed
//! change, `tick` on a timer, acting on `Deactivated` to drop overlay
//! RAM) is the thin I/O shell — a follow-up increment / Stream C's
//! consumer; this is the correctness core.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Per-worktree lifecycle state. `Copy` + cheap — the daemon keys a map
/// `WtId → WtLifecycle` and reads this every tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WtActivity {
    /// Recently active — overlay state is live, checks run normally.
    Active,
    /// No activity for ≥ `idle_after`. Still resident (cheap), but a
    /// candidate for deactivation if the quiet continues. Surfacing
    /// `Idle` distinctly lets the daemon do graduated backoff later.
    Idle,
    /// Quiet long enough that the daemon may free this worktree's in-RAM
    /// overlay state (solo cache retained on disk — see module docs).
    Deactivated,
}

/// Thresholds for the lifecycle. Conservative by default (`D-FLEET` §14:
/// "conservative default 5-15 min") so a worktree the operator steps away
/// from briefly is not thrashed in/out of deactivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityConfig {
    /// `Active → Idle` after this much no-activity.
    pub idle_after: Duration,
    /// `Idle → Deactivated` after this much *additional* continuous
    /// idleness (measured from the Active→Idle transition).
    pub deactivate_after: Duration,
}

impl ActivityConfig {
    /// Conservative defaults: Idle after 5 min, Deactivated 10 min later
    /// (≈15 min total continuous quiet → overlay RAM freed). Within the
    /// §14 5-15 min band.
    pub const DEFAULT_IDLE_SECS: u64 = 300;
    pub const DEFAULT_DEACTIVATE_SECS: u64 = 600;
    /// Floor (both knobs): below this, in/out thrash dominates the win —
    /// a fat-finger small value is clamped, not honored (idle.rs idiom).
    pub const FLOOR_SECS: u64 = 30;

    /// All-defaults config.
    pub fn defaults() -> Self {
        Self {
            idle_after: Duration::from_secs(Self::DEFAULT_IDLE_SECS),
            deactivate_after: Duration::from_secs(Self::DEFAULT_DEACTIVATE_SECS),
        }
    }

    /// Env-injected resolver (the only IO seam; the precedence rule is
    /// pure + unit-tested via [`resolve_secs`]). `TF_WT_IDLE_SECS` /
    /// `TF_WT_DEACTIVATE_SECS` override the defaults; non-numeric ⇒
    /// default, below-floor ⇒ clamped to [`FLOOR_SECS`] (exactly the
    /// `crate::idle::idle_window` discipline).
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Self {
        Self {
            idle_after: Duration::from_secs(resolve_secs(
                env("TF_WT_IDLE_SECS").as_deref(),
                Self::DEFAULT_IDLE_SECS,
            )),
            deactivate_after: Duration::from_secs(resolve_secs(
                env("TF_WT_DEACTIVATE_SECS").as_deref(),
                Self::DEFAULT_DEACTIVATE_SECS,
            )),
        }
    }
}

impl Default for ActivityConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Pure threshold-seconds resolver: `None`/non-numeric ⇒ `default`;
/// numeric ⇒ `max(value, FLOOR_SECS)`. Pure so it is exhaustively
/// unit-tested without touching the process env (mirrors the
/// `idle::idle_window` rule + its mirror test).
pub fn resolve_secs(v: Option<&str>, default: u64) -> u64 {
    match v {
        Some(s) => s
            .parse::<u64>()
            .map(|n| n.max(ActivityConfig::FLOOR_SECS))
            .unwrap_or(default),
        None => default,
    }
}

/// One worktree's lifecycle. Pure state machine: [`touch`](Self::touch)
/// on activity, [`tick`](Self::tick) to recompute state for a given
/// `now`. Construction starts `Active` (a just-discovered/just-activated
/// worktree is active by definition).
#[derive(Debug, Clone)]
pub struct WtLifecycle {
    cfg: ActivityConfig,
    state: WtActivity,
    /// Last observed activity instant.
    last_activity: Instant,
    /// When the worktree entered `Idle` (set on the Active→Idle edge,
    /// cleared on re-activation) — the clock the `deactivate_after`
    /// window measures from, so it is "continuous idle since going
    /// idle", not "wall time since the last edit".
    idle_since: Option<Instant>,
}

impl WtLifecycle {
    /// A freshly active worktree at `now`.
    pub fn new(now: Instant, cfg: ActivityConfig) -> Self {
        Self {
            cfg,
            state: WtActivity::Active,
            last_activity: now,
            idle_since: None,
        }
    }

    /// Current state (no recompute — call [`tick`](Self::tick) first if
    /// you need it fresh for `now`).
    pub fn state(&self) -> WtActivity {
        self.state
    }

    /// Record activity at `now`: unconditionally returns the worktree to
    /// `Active` and resets the idle clock. This is the re-activation
    /// edge — a `Deactivated` worktree coming back is exactly this call
    /// (the daemon then re-establishes overlay state; the `solo` cache
    /// makes the verdict instant if the overlay-set hash is unchanged).
    /// Idempotent for an already-active worktree.
    pub fn touch(&mut self, now: Instant) {
        self.state = WtActivity::Active;
        self.last_activity = now;
        self.idle_since = None;
    }

    /// Recompute + return the lifecycle state for `now`. Pure given
    /// `now`. Monotonic by construction: `touch` is the only way back to
    /// `Active`; `tick` only ever advances Active→Idle→Deactivated.
    ///
    /// A `now` earlier than `last_activity` (non-monotonic clock / test
    /// edge) is treated as "no elapsed time" via saturating arithmetic —
    /// it can never *regress* state, only fail to advance it (safe: a
    /// missed advance is a slower future check, never a wrong verdict).
    pub fn tick(&mut self, now: Instant) -> WtActivity {
        let idle_for = now.saturating_duration_since(self.last_activity);
        if idle_for < self.cfg.idle_after {
            // Still active. (A `touch` is what clears Idle/Deactivated;
            // tick never reactivates — only observed activity does.)
            if self.state == WtActivity::Active {
                self.idle_since = None;
            }
            return self.state;
        }
        // Past the idle threshold.
        if self.state == WtActivity::Active {
            self.state = WtActivity::Idle;
            // Idle clock starts at last_activity + idle_after (the moment
            // it *became* idle), not `now` — so a coarse tick interval
            // cannot postpone deactivation indefinitely.
            self.idle_since = Some(self.last_activity + self.cfg.idle_after);
        }
        if self.state == WtActivity::Idle {
            let since = self
                .idle_since
                .unwrap_or(self.last_activity + self.cfg.idle_after);
            if now.saturating_duration_since(since) >= self.cfg.deactivate_after {
                self.state = WtActivity::Deactivated;
            }
        }
        self.state
    }
}

/// bench-lead measurement hook (composes with #116 stage-3 fleet-scale +
/// the §15 Model-R characterization). `deactivations` = WT→Deactivated
/// edges; `reactivations` = Deactivated→Active edges (the re-activation
/// cost signal); `deactivated_ms` = summed wall-time worktrees spent
/// Deactivated (≈ the time-averaged per-WT overlay RAM reclaimed).
/// Dormant `(0,0,0)` until the daemon wires it — opt-in instrumentation,
/// exactly the `idle::IdleEvictCounters` pattern.
#[derive(Debug, Default)]
pub struct ActivityCounters {
    deactivations: AtomicU64,
    reactivations: AtomicU64,
    deactivated_ms: AtomicU64,
}

impl ActivityCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one Active/Idle → Deactivated edge.
    pub fn record_deactivation(&self) {
        self.deactivations.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one Deactivated → Active edge (re-activation paid the
    /// overlay-recompute; solo cache may still make the verdict instant).
    pub fn record_reactivation(&self) {
        self.reactivations.fetch_add(1, Ordering::Relaxed);
    }

    /// Add a completed Deactivated interval (recorded on re-activation).
    pub fn add_deactivated(&self, d: Duration) {
        self.deactivated_ms
            .fetch_add(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// `(deactivations, reactivations, deactivated_ms)` for bench-lead.
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.deactivations.load(Ordering::Relaxed),
            self.reactivations.load(Ordering::Relaxed),
            self.deactivated_ms.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(idle: u64, deact: u64) -> ActivityConfig {
        ActivityConfig {
            idle_after: Duration::from_secs(idle),
            deactivate_after: Duration::from_secs(deact),
        }
    }

    #[test]
    fn starts_active_and_stays_active_under_threshold() {
        let t0 = Instant::now();
        let mut wt = WtLifecycle::new(t0, cfg(300, 600));
        assert_eq!(wt.state(), WtActivity::Active);
        // 299s < 300s idle_after ⇒ still Active.
        assert_eq!(wt.tick(t0 + Duration::from_secs(299)), WtActivity::Active);
    }

    #[test]
    fn active_to_idle_to_deactivated_progression() {
        let t0 = Instant::now();
        let mut wt = WtLifecycle::new(t0, cfg(300, 600));
        // At +300s ⇒ Idle (idle clock starts here).
        assert_eq!(wt.tick(t0 + Duration::from_secs(300)), WtActivity::Idle);
        // +300s..+899s still Idle (deactivate_after=600 from idle start).
        assert_eq!(wt.tick(t0 + Duration::from_secs(899)), WtActivity::Idle);
        // +900s = idle-start(300) + 600 ⇒ Deactivated.
        assert_eq!(
            wt.tick(t0 + Duration::from_secs(900)),
            WtActivity::Deactivated
        );
    }

    #[test]
    fn coarse_tick_cannot_postpone_deactivation() {
        // The load-bearing subtlety: the idle clock anchors at the moment
        // it BECAME idle (last_activity + idle_after), not at `now`. So a
        // single coarse tick that lands far past both thresholds still
        // deactivates — a sparse timer cannot keep a dead worktree
        // resident forever.
        let t0 = Instant::now();
        let mut wt = WtLifecycle::new(t0, cfg(300, 600));
        // First and only tick is at +5000s (way past 300+600).
        assert_eq!(
            wt.tick(t0 + Duration::from_secs(5000)),
            WtActivity::Deactivated
        );
    }

    #[test]
    fn touch_reactivates_from_any_state() {
        let t0 = Instant::now();
        let mut wt = WtLifecycle::new(t0, cfg(300, 600));
        wt.tick(t0 + Duration::from_secs(2000)); // → Deactivated
        assert_eq!(wt.state(), WtActivity::Deactivated);
        // Activity at +2001s ⇒ back to Active, idle clock reset.
        wt.touch(t0 + Duration::from_secs(2001));
        assert_eq!(wt.state(), WtActivity::Active);
        // And it stays active for a fresh full idle window from +2001.
        assert_eq!(
            wt.tick(t0 + Duration::from_secs(2001 + 299)),
            WtActivity::Active
        );
        assert_eq!(
            wt.tick(t0 + Duration::from_secs(2001 + 300)),
            WtActivity::Idle,
            "fresh idle window measured from the re-activation, not t0"
        );
    }

    #[test]
    fn non_monotonic_now_never_regresses_state() {
        // A `now` earlier than last_activity (clock skew / test) must not
        // regress state — saturating arithmetic ⇒ "no elapsed time", so
        // it can only fail to advance, never move backward. Never a
        // wrong verdict, only a possibly-slower next check.
        let t0 = Instant::now();
        let mut wt = WtLifecycle::new(t0 + Duration::from_secs(1000), cfg(300, 600));
        // tick with a `now` 500s BEFORE construction's last_activity.
        assert_eq!(
            wt.tick(t0 + Duration::from_secs(500)),
            WtActivity::Active,
            "earlier now ⇒ saturating 0 elapsed ⇒ stays Active, no panic/regress"
        );
    }

    #[test]
    fn resolve_secs_rule_default_clamp_floor() {
        // Mirror-test the env precedence (env reads are unsafe to mutate
        // across threads on edition 2024 — same discipline as
        // idle::tests::idle_window_default_and_clamp_rule).
        assert_eq!(resolve_secs(None, 300), 300, "unset ⇒ default");
        assert_eq!(
            resolve_secs(Some("nope"), 300),
            300,
            "non-numeric ⇒ default"
        );
        assert_eq!(resolve_secs(Some("900"), 300), 900, "sane value honored");
        assert_eq!(
            resolve_secs(Some("5"), 300),
            ActivityConfig::FLOOR_SECS,
            "below floor ⇒ clamped"
        );
        assert_eq!(resolve_secs(Some("30"), 300), 30, "floor exact honored");
    }

    #[test]
    fn from_env_threads_both_knobs() {
        let env = |k: &str| match k {
            "TF_WT_IDLE_SECS" => Some("120".to_string()),
            "TF_WT_DEACTIVATE_SECS" => Some("7".to_string()), // below floor
            _ => None,
        };
        let c = ActivityConfig::from_env(&env);
        assert_eq!(c.idle_after, Duration::from_secs(120));
        assert_eq!(
            c.deactivate_after,
            Duration::from_secs(ActivityConfig::FLOOR_SECS),
            "below-floor deactivate clamped, not honored"
        );
        // Unset ⇒ defaults.
        let c2 = ActivityConfig::from_env(&|_| None);
        assert_eq!(c2, ActivityConfig::defaults());
    }

    #[test]
    fn counters_track_edges_and_time() {
        let c = ActivityCounters::new();
        assert_eq!(c.snapshot(), (0, 0, 0));
        c.record_deactivation();
        c.add_deactivated(Duration::from_millis(2000));
        c.record_reactivation();
        c.record_deactivation();
        c.add_deactivated(Duration::from_millis(500));
        assert_eq!(c.snapshot(), (2, 1, 2500));
    }
}
