//! #13 leg-C — SIGTERM **graceful shutdown** drain state machine (pure core).
//!
//! ## Why this module exists (design `D-13-CRASH-RESTART.md` §4)
//!
//! AC#6's [`Supervisor::shutdown`](crate::analyzer::Supervisor) /
//! `Drop` already terminate the rust-analyzer *child* cleanly (flag → join
//! monitor → kill+reap, idempotent). The gap leg C fills is the **daemon-level
//! orchestration** around it: on SIGTERM the Model R daemon must, *in order*,
//!
//!   1. **stop intake** — accept no new verdict/overlay work, so nothing new
//!      is started that the drain would then have to wait for or lose, then
//!   2. **flush pending per-WT state** to `<wt>/tree.cache` (so a restarted
//!      daemon re-attaches to accurate state — #7's decoupled-lifecycle cache
//!      is the durability substrate), then
//!   3. **stop** (hand off to `Supervisor::shutdown`).
//!
//! This module is the **#5-independent, cache-independent pure core** of that
//! orchestration: the ordered, idempotent, bounded `Running → Draining →
//! Stopped` state machine. It does not own the cache, RA, or signal handler —
//! the flush target is the mockable [`DrainTarget`] seam; the signal mechanism
//! is open decision **D-13-Q1** (see §"Crash-safe by construction").
//!
//! ## The launch-critical invariant: stop-intake *strictly before* flush
//!
//! If flush ran while intake were still open, freshly-accepted work could land
//! after its worktree was already flushed → that work is silently lost on
//! restart (a wrong/missing verdict — the failure class cargoless exists to
//! prevent). The state machine therefore makes `stop_intake()` happen-before
//! `flush_pending()` *structurally* (one ordered method body, not a
//! convention), regression-guarded by
//! [`tests::shutdown_stops_intake_strictly_before_any_flush`].
//!
//! ## Bounded: a wedged flush must never hang termination
//!
//! Cleanup that can hang is worse than no cleanup (it blocks the SIGTERM, then
//! the inevitable SIGKILL loses *everything* un-flushed *and* gives no clean
//! signal). [`GracefulShutdown::shutdown`] takes a `max_flush_passes` budget;
//! exceeding it force-transitions to `Stopped` with a *distinct, honest*
//! [`ShutdownOutcome::ForcedAfterBudget`] — never silently reported as a clean
//! drain. Same "never let cleanup hang the process" discipline as the Tier-4
//! reap.
//!
//! ## Crash-safe by construction (so leg-C correctness is zero-dep)
//!
//! Per D-13 §4.3 (open question **D-13-Q1**: signal dep — `libc`/`signal-hook`
//! /none): leg C is designed so that even a *force-stopped or un-drained*
//! shutdown is recoverable on restart via leg A (AC#6 respawn) + leg B
//! (generation-fenced overlay replay) — the per-WT `tree.cache` writes that
//! *did* flush are atomic (#7 / `cargoless-cas` temp+rename), and anything
//! un-flushed is simply re-derived by the next replay. So the *correctness* of
//! graceful shutdown does **not** depend on resolving D-13-Q1; only the
//! clean-stop *latency* does (a clean drain restarts faster than crash-replay).
//! [`ShutdownOutcome`] reports exactly how much durable state the drain
//! achieved so recovery knows what is on disk.

use std::io;

/// Lifecycle state. `Stopped` is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Draining,
    Stopped,
}

/// The result of a [`GracefulShutdown::shutdown`] call — deliberately
/// fine-grained and honestly-typed so a caller can never mistake a forced or
/// error-laden stop for a clean one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// Intake stopped, every pending item flushed durably, then stopped.
    DrainedClean { flushed: usize },
    /// Drained to `Stopped`, but `flush_pending` reported `errors` failed
    /// flushes. The successfully-flushed items ARE durable; the rest are
    /// recoverable by leg-B replay on restart. NOT a clean drain.
    DrainedWithErrors { flushed: usize, errors: usize },
    /// `max_flush_passes` exhausted with work still pending — force-stopped to
    /// guarantee termination. `flushed` is what reached disk before the
    /// budget bound; the remainder is recovered by leg-B replay. NOT clean.
    ForcedAfterBudget { flushed: usize },
    /// A drain was already in progress (a second SIGTERM). The first call owns
    /// the drain; this is a safe no-op. Idempotent.
    AlreadyDraining,
    /// Already `Stopped`. Idempotent no-op.
    AlreadyStopped,
}

impl ShutdownOutcome {
    /// True only for a fully-clean drain. A forced/errored stop is honestly
    /// *not* clean — callers (and the launch narrative) must not conflate them.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        matches!(self, ShutdownOutcome::DrainedClean { .. })
    }
}

/// The seam leg C drains through. `#13` owns the *ordering + bound +
/// idempotency*; the daemon implements this over its per-WT `tree.cache`
/// writers + `Supervisor::shutdown`. Mocked in tests — **zero #5-contract and
/// zero cache_layout-API speculation** (only the inherent
/// stop-intake / flush-one-pass shapes are assumed).
pub trait DrainTarget {
    /// Stop accepting new verdict/overlay work. Called exactly once, **before
    /// any flush**. Must be cheap and not block.
    fn stop_intake(&mut self);

    /// Flush one pass of currently-pending per-WT state to durable
    /// `tree.cache`. Returns `(flushed_this_pass, still_pending)`. Called
    /// repeatedly until `still_pending == 0` or the pass budget is hit.
    ///
    /// # Errors
    /// A pass-level failure; the drain records it, counts it, and continues to
    /// `Stopped` (a flush error must never hang termination).
    fn flush_pending(&mut self) -> io::Result<FlushProgress>;
}

/// One `flush_pending` pass result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushProgress {
    /// Items made durable in this pass.
    pub flushed: usize,
    /// Items still pending after this pass.
    pub still_pending: usize,
}

/// The `Running → Draining → Stopped` graceful-shutdown state machine.
#[derive(Debug)]
pub struct GracefulShutdown {
    state: State,
}

impl Default for GracefulShutdown {
    fn default() -> Self {
        Self {
            state: State::Running,
        }
    }
}

impl GracefulShutdown {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.state == State::Stopped
    }

    /// Drive a graceful shutdown (call from the SIGTERM handler — D-13-Q1).
    ///
    /// Ordered + idempotent + bounded:
    /// * already `Draining` ⇒ [`ShutdownOutcome::AlreadyDraining`] (a second
    ///   SIGTERM is a safe no-op — the first call owns the drain);
    /// * already `Stopped` ⇒ [`ShutdownOutcome::AlreadyStopped`];
    /// * else: `Running → Draining`, **`stop_intake()` first**, then up to
    ///   `max_flush_passes` `flush_pending()` passes until nothing is pending,
    ///   then `→ Stopped`. Budget exhaustion or flush errors still reach
    ///   `Stopped` (termination is guaranteed) with a non-clean outcome.
    ///
    /// `max_flush_passes` must be ≥ 1; 0 is treated as 1 (a drain always makes
    /// at least one flush attempt — never a silent no-flush stop).
    pub fn shutdown(
        &mut self,
        target: &mut dyn DrainTarget,
        max_flush_passes: usize,
    ) -> ShutdownOutcome {
        match self.state {
            State::Draining => return ShutdownOutcome::AlreadyDraining,
            State::Stopped => return ShutdownOutcome::AlreadyStopped,
            State::Running => {}
        }

        // Running → Draining. Intake is stopped STRICTLY before any flush so
        // no newly-accepted work can land behind an already-flushed worktree.
        self.state = State::Draining;
        target.stop_intake();

        let budget = max_flush_passes.max(1);
        let mut flushed_total = 0usize;
        let mut error_total = 0usize;

        for _ in 0..budget {
            match target.flush_pending() {
                Ok(p) => {
                    flushed_total += p.flushed;
                    if p.still_pending == 0 {
                        self.state = State::Stopped;
                        return if error_total == 0 {
                            ShutdownOutcome::DrainedClean {
                                flushed: flushed_total,
                            }
                        } else {
                            ShutdownOutcome::DrainedWithErrors {
                                flushed: flushed_total,
                                errors: error_total,
                            }
                        };
                    }
                }
                Err(_) => {
                    // A pass failed: count it, keep going (bounded). Never
                    // hang termination on a flush error.
                    error_total += 1;
                }
            }
        }

        // Budget exhausted with work still pending (or every pass errored):
        // force-stop. Termination is guaranteed; the outcome is honestly
        // non-clean; un-flushed state is recovered by leg-B replay on restart.
        self.state = State::Stopped;
        if error_total > 0 && flushed_total == 0 {
            ShutdownOutcome::DrainedWithErrors {
                flushed: 0,
                errors: error_total,
            }
        } else {
            ShutdownOutcome::ForcedAfterBudget {
                flushed: flushed_total,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records the exact call sequence + drives a scriptable flush schedule.
    /// `pending` is decremented by `per_pass` each flush; `err_passes` lists
    /// (0-based) pass indices that return Err.
    struct MockTarget {
        log: RefCell<Vec<&'static str>>,
        pending: RefCell<usize>,
        per_pass: usize,
        pass: RefCell<usize>,
        err_passes: Vec<usize>,
    }

    impl MockTarget {
        fn new(pending: usize, per_pass: usize) -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                pending: RefCell::new(pending),
                per_pass,
                pass: RefCell::new(0),
                err_passes: Vec::new(),
            }
        }
        fn erroring(mut self, passes: &[usize]) -> Self {
            self.err_passes = passes.to_vec();
            self
        }
    }

    impl DrainTarget for MockTarget {
        fn stop_intake(&mut self) {
            self.log.borrow_mut().push("stop_intake");
        }
        fn flush_pending(&mut self) -> io::Result<FlushProgress> {
            self.log.borrow_mut().push("flush");
            let idx = *self.pass.borrow();
            *self.pass.borrow_mut() += 1;
            if self.err_passes.contains(&idx) {
                return Err(io::Error::other("flush boom"));
            }
            let mut p = self.pending.borrow_mut();
            let f = self.per_pass.min(*p);
            *p -= f;
            Ok(FlushProgress {
                flushed: f,
                still_pending: *p,
            })
        }
    }

    #[test]
    fn shutdown_stops_intake_strictly_before_any_flush() {
        // The launch-critical ordering invariant.
        let mut t = MockTarget::new(5, 10);
        let mut sm = GracefulShutdown::new();
        sm.shutdown(&mut t, 4);
        let log = t.log.borrow();
        assert_eq!(log[0], "stop_intake", "intake MUST stop before any flush");
        assert!(
            log.iter().filter(|s| **s == "stop_intake").count() == 1,
            "stop_intake exactly once"
        );
        let first_flush = log.iter().position(|s| *s == "flush").unwrap();
        let stop_idx = log.iter().position(|s| *s == "stop_intake").unwrap();
        assert!(
            stop_idx < first_flush,
            "stop_intake strictly precedes flush"
        );
    }

    #[test]
    fn shutdown_drains_until_empty_then_clean_stop() {
        let mut t = MockTarget::new(10, 4); // 4+4+2 → 3 passes
        let mut sm = GracefulShutdown::new();
        let out = sm.shutdown(&mut t, 8);
        assert_eq!(out, ShutdownOutcome::DrainedClean { flushed: 10 });
        assert!(out.is_clean());
        assert_eq!(sm.state(), State::Stopped);
    }

    #[test]
    fn shutdown_is_idempotent_double_sigterm() {
        let mut t = MockTarget::new(2, 2);
        let mut sm = GracefulShutdown::new();
        let first = sm.shutdown(&mut t, 4);
        assert_eq!(first, ShutdownOutcome::DrainedClean { flushed: 2 });
        // A second SIGTERM after completion is a safe no-op.
        let second = sm.shutdown(&mut t, 4);
        assert_eq!(second, ShutdownOutcome::AlreadyStopped);
        assert!(
            !second.is_clean(),
            "AlreadyStopped is not a fresh clean drain"
        );
        // stop_intake was NOT called a second time.
        assert_eq!(
            t.log
                .borrow()
                .iter()
                .filter(|s| **s == "stop_intake")
                .count(),
            1
        );
    }

    #[test]
    fn shutdown_bounded_forces_stop_after_budget_never_hangs() {
        // 100 pending, 1/pass, only 3 passes allowed ⇒ force-stop, terminates.
        let mut t = MockTarget::new(100, 1);
        let mut sm = GracefulShutdown::new();
        let out = sm.shutdown(&mut t, 3);
        assert_eq!(out, ShutdownOutcome::ForcedAfterBudget { flushed: 3 });
        assert!(
            !out.is_clean(),
            "a budget-forced stop is honestly NOT clean"
        );
        assert_eq!(sm.state(), State::Stopped, "termination guaranteed");
        assert_eq!(t.log.borrow().iter().filter(|s| **s == "flush").count(), 3);
    }

    #[test]
    fn shutdown_flush_error_still_reaches_stopped() {
        // Every pass errors; must still terminate, honestly non-clean.
        let mut t = MockTarget::new(5, 5).erroring(&[0, 1, 2]);
        let mut sm = GracefulShutdown::new();
        let out = sm.shutdown(&mut t, 3);
        assert_eq!(
            out,
            ShutdownOutcome::DrainedWithErrors {
                flushed: 0,
                errors: 3
            }
        );
        assert!(!out.is_clean());
        assert_eq!(sm.state(), State::Stopped);
    }

    #[test]
    fn shutdown_partial_flush_then_error_reports_durable_count() {
        // pass0 flushes 3 (pending 6→3), pass1 errors, pass2 flushes 3 → 0.
        let mut t = MockTarget::new(6, 3).erroring(&[1]);
        let mut sm = GracefulShutdown::new();
        let out = sm.shutdown(&mut t, 5);
        // Recovery must know exactly what reached disk: 6 flushed, 1 errored
        // pass survived (clean completion after the transient error).
        assert_eq!(
            out,
            ShutdownOutcome::DrainedWithErrors {
                flushed: 6,
                errors: 1
            }
        );
        assert!(!out.is_clean());
    }

    #[test]
    fn shutdown_zero_budget_is_treated_as_one_attempt() {
        // 0 must never mean "stop without ever attempting a flush".
        let mut t = MockTarget::new(1, 1);
        let mut sm = GracefulShutdown::new();
        let out = sm.shutdown(&mut t, 0);
        assert_eq!(out, ShutdownOutcome::DrainedClean { flushed: 1 });
        assert_eq!(t.log.borrow().iter().filter(|s| **s == "flush").count(), 1);
    }

    #[test]
    fn shutdown_stopped_is_terminal() {
        let mut t = MockTarget::new(0, 1);
        let mut sm = GracefulShutdown::new();
        sm.shutdown(&mut t, 1);
        assert_eq!(sm.state(), State::Stopped);
        // Any further shutdown call is AlreadyStopped, state unchanged.
        assert_eq!(sm.shutdown(&mut t, 1), ShutdownOutcome::AlreadyStopped);
        assert_eq!(sm.state(), State::Stopped);
    }

    #[test]
    fn shutdown_empty_pending_is_immediate_clean_stop() {
        let mut t = MockTarget::new(0, 4);
        let mut sm = GracefulShutdown::new();
        let out = sm.shutdown(&mut t, 4);
        assert_eq!(out, ShutdownOutcome::DrainedClean { flushed: 0 });
        // Still stops intake first, then one (empty) flush pass.
        assert_eq!(t.log.borrow()[0], "stop_intake");
        assert!(out.is_clean());
    }

    #[test]
    fn shutdown_outcome_is_clean_only_for_drained_clean() {
        assert!(ShutdownOutcome::DrainedClean { flushed: 9 }.is_clean());
        assert!(
            !ShutdownOutcome::DrainedWithErrors {
                flushed: 1,
                errors: 1
            }
            .is_clean()
        );
        assert!(!ShutdownOutcome::ForcedAfterBudget { flushed: 2 }.is_clean());
        assert!(!ShutdownOutcome::AlreadyDraining.is_clean());
        assert!(!ShutdownOutcome::AlreadyStopped.is_clean());
    }
}
