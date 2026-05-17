//! #122 Tier-4 — idle-evict RA (bounded prototype, DEFAULT-OFF).
//!
//! Operator #1 priority (RAM) + the fleet-scale *existence* lever:
//! model-A is ~40 GB at 20 agents on a 16 GB box (#116). Under the
//! agent-input model the gaps between agent-edit-batches are long
//! (model think-time, tool calls) and provably check-free (the #112-A
//! CLOSED∧quiescent boundaries) — yet RA sits resident at ~2 GB doing
//! nothing. Evicting it during those gaps and transparently respawning
//! it on the next batch is what makes a real multi-agent deployment fit
//! in memory at all.
//!
//! ## Default-off (operator doctrine: ship behind a flag, measure,
//! data decides v0-default-vs-v0.1)
//!
//! Enabled iff `TF_RA_IDLE_EVICT=1`. Unset ⇒ [`enabled`] is `false`,
//! the `watch()` fs loop never calls `suspend()`, the supervisor's
//! `suspended` flag is never set, and every code path is
//! **byte-identical** to pre-#122. This is the exact safety property
//! the structural-trigger spike (#112-A) and Tier-1/2 were approved on.
//!
//! ## No-wrong-verdict invariant (load-bearing — proved in
//! `docs/design/D-IDLE-EVICT.md`)
//!
//! Closedness/eviction gate only *when RA is resident*, never the
//! verdict colour. The authoritative green/red is the cargo-check /
//! F8-redo tier (a transient subprocess, no resident cost); a suspended
//! RA cannot make a tree wrongly green or hide a red. Eviction is gated
//! on `flycheck_done` so the cold first authoritative pass is never
//! interrupted, and a batch arriving while suspended resumes RA (AC#6
//! transparent re-init/re-`did_open`) *before* any `didChange`/`didSave`
//! is forwarded. Worst case of a mistimed evict is a slower next check,
//! never a wrong/missing verdict; `never-publish-red` is untouched.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// True iff `TF_RA_IDLE_EVICT=1` (strict; idiom matches
/// `TF_STRUCTURAL_TRIGGER` / `TF_DEBOUNCE_MS`). Any other value ⇒
/// default-off ⇒ pre-#122 behavior, byte-identical.
pub fn enabled() -> bool {
    matches!(std::env::var("TF_RA_IDLE_EVICT").as_deref(), Ok("1"))
}

/// Idle quiet-window before eviction. `TF_RA_IDLE_SECS` overrides the
/// 30 s default; clamped to a 5 s floor so a fat-finger value cannot
/// thrash spawn/evict, and a non-numeric value falls back to the
/// default rather than erroring.
///
/// Operator guidance (see D-IDLE-EVICT §risk): set this comfortably
/// above your flycheck p99 — a batch whose authoritative check is still
/// running when the window elapses has that check cancelled and
/// recomputed on the next batch (stale-but-correct, never wrong).
pub fn idle_window() -> Duration {
    const DEFAULT_SECS: u64 = 30;
    const FLOOR_SECS: u64 = 5;
    let secs = match std::env::var("TF_RA_IDLE_SECS") {
        Ok(v) => v
            .parse::<u64>()
            .map(|n| n.max(FLOOR_SECS))
            .unwrap_or(DEFAULT_SECS),
        Err(_) => DEFAULT_SECS,
    };
    Duration::from_secs(secs)
}

/// bench-lead measurement hook (composes with #116 stage-3 fleet-scale).
/// `evictions` = number of idle-evict cycles; `suspended_ms` = total
/// wall-time RA was evicted (≈ the time-averaged ~2 GB reclaimed).
/// Dormant `(0, 0)` while default-off so an instrumented run is opt-in.
#[derive(Debug, Default)]
pub struct IdleEvictCounters {
    evictions: AtomicU64,
    suspended_ms: AtomicU64,
}

impl IdleEvictCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one eviction (RA suspended).
    pub fn record_eviction(&self) {
        self.evictions.fetch_add(1, Ordering::Relaxed);
    }

    /// Add a completed suspended interval (recorded on resume).
    pub fn add_suspended(&self, d: Duration) {
        self.suspended_ms
            .fetch_add(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// `(evictions, suspended_ms)` snapshot for bench-lead.
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.evictions.load(Ordering::Relaxed),
            self.suspended_ms.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_is_strict_one_default_off() {
        // `enabled()` reads process env (unsafe to mutate across
        // threads on edition 2024); pin the parse RULE via a mirror —
        // same discipline as structural::enabled's test.
        fn rule(v: Option<&str>) -> bool {
            v == Some("1")
        }
        assert!(rule(Some("1")));
        assert!(!rule(None));
        assert!(!rule(Some("")));
        assert!(!rule(Some("0")));
        assert!(!rule(Some("true")));
    }

    #[test]
    fn idle_window_default_and_clamp_rule() {
        fn rule(v: Option<&str>) -> u64 {
            const DEFAULT_SECS: u64 = 30;
            const FLOOR_SECS: u64 = 5;
            match v {
                Some(s) => s
                    .parse::<u64>()
                    .map(|n| n.max(FLOOR_SECS))
                    .unwrap_or(DEFAULT_SECS),
                None => DEFAULT_SECS,
            }
        }
        assert_eq!(rule(None), 30, "unset ⇒ default");
        assert_eq!(rule(Some("garbage")), 30, "non-numeric ⇒ default");
        assert_eq!(rule(Some("120")), 120, "sane value honored");
        assert_eq!(rule(Some("1")), 5, "below floor ⇒ clamped to 5");
        assert_eq!(rule(Some("5")), 5, "floor exact");
    }

    #[test]
    fn counters_track_evictions_and_suspended_time() {
        let c = IdleEvictCounters::new();
        assert_eq!(c.snapshot(), (0, 0));
        c.record_eviction();
        c.add_suspended(Duration::from_millis(1500));
        c.record_eviction();
        c.add_suspended(Duration::from_millis(500));
        assert_eq!(c.snapshot(), (2, 2000));
    }
}
