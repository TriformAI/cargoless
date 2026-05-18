//! #3 serve-loop-capstone — the **pure per-cluster transaction
//! sequencer** (Model R Stream B+C tie-point, capstone-core; the live
//! serve-loop adapter that executes its actions against the real
//! `LspClient`/`Supervisor`/`OverlayMultiplexer` and replaces serve.rs's
//! park is the follow-on thin wire, capstone-wire).
//!
//! ## Why a pure core here too
//!
//! The capstone is the B+C tie-point: routed file-changes
//! ([`repo::watch::RepoWatchRouter`]) → overlay-switch the one shared
//! per-cluster RA ([`multiplex::OverlayMultiplexer`]) → wait the
//! flycheck barrier ([`barrier::FlycheckBarrier`]) → attribute the
//! verdict to the worktree ([`multiplex::tag_for_worktree`]). The
//! load-bearing correctness is **not** the I/O — it is the *sequencing*:
//! the two named judgments the #3 backstop locks. Extracting the
//! sequencing as a pure, total, exhaustively-tested state machine makes
//! both judgments **structural** (a property of the program's shape, not
//! a runtime discipline), exactly the pure-core-first move that turned
//! `overlay::diff` / `cluster::hash` / the flycheck barrier into
//! impossibility-proofs rather than sample-pass tests.
//!
//! One [`ClusterDriver`] models **one cluster's** RA (the live shell
//! owns one per cluster, each driven by that cluster's own single
//! consumer of its RA event stream — so cross-cluster work is naturally
//! parallel and independent; *within* a cluster it is strictly
//! serialized here).
//!
//! ## Judgment A — per-cluster-RA single-transaction serialization
//!
//! > Per-WT checks are serialized through the cluster's single shared
//! > RA; concurrent routed changes for worktrees sharing one RA can
//! > never interleave into a mixed / cross-WT verdict.
//!
//! **Structural:** [`ClusterDriver::current`] is an
//! `Option<ActiveTxn>` — there is *at most one* in-flight transaction
//! per driver. A [`DriverEvent::RoutedBatch`] that arrives while
//! `current.is_some()` can only **enqueue** the worktree
//! ([`ClusterDriver::pending`], deduped); it cannot start a second
//! overlay-switch / flycheck. A second concurrent transaction is
//! therefore *unrepresentable*, not "avoided by care". The next
//! transaction starts only when the current one reaches
//! [`BarrierState::Settled`]. (Corollary, and why the barrier is always
//! armed `stale=0`: a new WT's flycheck is never triggered until the
//! prior WT's flycheck has *ended* — `Settled` ⇒ no stale in-flight
//! flycheck can exist when the next `arm` happens.)
//!
//! ## Judgment B — structural `is_settled` gate
//!
//! > A worktree's verdict is never read / attributed before its
//! > flycheck barrier is `Settled`.
//!
//! **Structural:** the driver *owns* the in-flight
//! [`FlycheckBarrier`]. The **only** code path that produces
//! [`ClusterAction::EmitVerdict`] is the single arm where
//! `barrier.observe(ev)` returns [`BarrierState::Settled`]; the verdict
//! bit it carries is computed *in that arm* from the just-settled
//! barrier. There is **no** `ClusterDriver` API that exposes the barrier
//! or yields a verdict from a non-`Settled` state. A pre-settle
//! attribution is thus unrepresentable in the driver's type/control
//! path — exactly the carry-forward the incr-3 barrier proof's
//! documented boundary made the capstone's obligation. (A pre-settle
//! read would be the F8-redo "GREEN while errors still printing" failure
//! in temporal form; here it cannot be written.)

use std::collections::VecDeque;

use crate::barrier::{BarrierState, FlycheckBarrier};
use crate::lsp::LspEvent;
use crate::repo::watch::WtId;

/// One in-flight per-cluster transaction: the worktree being checked and
/// its (owned) flycheck barrier. Private — the barrier is never handed
/// out (Judgment B: no non-`Settled` verdict extraction path).
#[derive(Debug)]
struct ActiveTxn {
    wt: WtId,
    barrier: FlycheckBarrier,
}

/// What the live serve-loop adapter must do next for this cluster. The
/// adapter maps these onto `OverlayMultiplexer::switch_to` + the
/// `LspClient` verbs / `tag_for_worktree` — nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterAction {
    /// Begin worktree `wt`'s transaction: switch the shared RA's overlay
    /// to `wt` (multiplexer → LspVerbs) and trigger its flycheck
    /// (`did_save`). The barrier for `wt` is now armed and waiting.
    SwitchOverlay { wt: WtId },
    /// `wt`'s flycheck barrier has **Settled** — and only now — so its
    /// verdict is authoritative. `authoritative_error` is the verdict
    /// bit, computed from the settled barrier at the settle instant.
    /// The adapter attributes this to `wt` (`tag_for_worktree`) and
    /// publishes; it is the *sole* verdict-bearing output.
    EmitVerdict { wt: WtId, authoritative_error: bool },
    /// Nothing to do for this event (serialized-wait, stray pre-arm RA
    /// chatter, deduped re-request, or pruned deactivation).
    Idle,
}

/// Events into one cluster's sequencer.
#[derive(Debug, Clone)]
pub enum DriverEvent {
    /// `RepoWatchRouter` produced a settled batch for `wt` (∈ this
    /// cluster) — a check is due. (The batch's file list is the
    /// adapter's concern when it builds the overlay; the sequencer needs
    /// only the worktree identity + "a check is due".)
    RoutedBatch { wt: WtId },
    /// One event from this cluster's RA event stream.
    Lsp(LspEvent),
    /// `wt` deactivated (idle long enough). Prunes a queued check so a
    /// no-longer-active worktree does not later get a needless check; an
    /// in-flight transaction is left to settle harmlessly (the adapter
    /// simply does not publish a deactivated WT, and `ClusterLifecycle`
    /// owns RA teardown at 1→0 — orthogonal to this sequencer).
    Deactivated { wt: WtId },
}

/// Pure per-cluster transaction sequencer. Feed it [`DriverEvent`]s; it
/// emits [`ClusterAction`]s that the live adapter executes. Pure: no
/// `LspClient`, no threads, no I/O — the barrier is driven purely off
/// the [`LspEvent`]s the adapter forwards. See the module docs for the
/// two structural judgments (A serialization, B `is_settled` gate).
#[derive(Debug, Default)]
pub struct ClusterDriver {
    /// The single in-flight transaction, or `None` when the cluster RA
    /// is idle (no WT mid-check). Judgment A: at most one, ever.
    current: Option<ActiveTxn>,
    /// Worktrees with a check due, waiting for the RA. Deduped (a WT
    /// already queued / in-flight does not stack); FIFO so a starved WT
    /// is eventually serviced.
    pending: VecDeque<WtId>,
    /// Set when a routed batch arrives for the *currently in-flight* WT:
    /// its overlay may now be stale, so re-check it once the current
    /// transaction settles (a self-recheck, distinct from `pending`).
    recheck_current: bool,
}

impl ClusterDriver {
    /// Fresh idle cluster (no transaction, empty queue).
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` while a transaction is in flight (the cluster RA is busy).
    /// Inspection / tests.
    pub fn is_busy(&self) -> bool {
        self.current.is_some()
    }

    /// Worktrees currently queued for a check (deterministic FIFO order).
    /// Inspection / tests.
    pub fn pending(&self) -> Vec<WtId> {
        self.pending.iter().cloned().collect()
    }

    /// Feed one event; returns the action the adapter must perform.
    ///
    /// At most one [`ClusterAction::SwitchOverlay`] is ever "open" at a
    /// time (Judgment A). [`ClusterAction::EmitVerdict`] is produced
    /// **only** from the `Settled` arm (Judgment B).
    pub fn on_event(&mut self, ev: DriverEvent) -> ClusterAction {
        match ev {
            DriverEvent::RoutedBatch { wt } => self.on_routed_batch(wt),
            DriverEvent::Lsp(ev) => self.on_lsp(ev),
            DriverEvent::Deactivated { wt } => self.on_deactivated(wt),
        }
    }

    fn on_routed_batch(&mut self, wt: WtId) -> ClusterAction {
        if self.current.is_none() {
            // RA idle ⇒ start this WT's transaction now. Barrier armed
            // `stale=0`: serialization (Judgment A) guarantees no prior
            // flycheck is in flight (the previous txn, if any, reached
            // Settled ⇒ its flycheck ended).
            self.current = Some(ActiveTxn {
                wt: wt.clone(),
                barrier: FlycheckBarrier::arm(false),
            });
            return ClusterAction::SwitchOverlay { wt };
        }
        // A transaction is in flight ⇒ serialize. Judgment A: never
        // start a second overlay-switch/flycheck. (Owned clone so the
        // `self.current` borrow is released before mutating the queue.)
        let in_flight = self.current.as_ref().map(|a| a.wt.clone());
        if in_flight.as_ref() == Some(&wt) {
            // Change to the WT being checked right now — its result
            // will be stale; re-check after it settles.
            self.recheck_current = true;
        } else if !self.pending.contains(&wt) {
            self.pending.push_back(wt);
        }
        ClusterAction::Idle
    }

    fn on_lsp(&mut self, ev: LspEvent) -> ClusterAction {
        // Observe under the borrow, then DROP it (`BarrierState` is
        // `Copy`) before re-borrowing `self.current` to take the txn —
        // no second verdict path, just borrow hygiene.
        let state = match self.current.as_mut() {
            // No transaction ⇒ RA stream chatter (e.g. initial indexing)
            // with nothing to attribute it to. Never a verdict.
            None => return ClusterAction::Idle,
            Some(active) => active.barrier.observe(&ev),
        };
        match state {
            BarrierState::Waiting => ClusterAction::Idle,
            BarrierState::Settled => {
                // THE single verdict-bearing path. The barrier reached
                // `Settled` here and nowhere else; compute the verdict
                // bit from it *now*, in this arm, and hand it out as the
                // action payload. No other code path yields a verdict;
                // the barrier is never exposed (Judgment B: a pre-settle
                // attribution is unrepresentable).
                let txn = self
                    .current
                    .take()
                    .expect("current is Some in the Settled arm");
                let action = ClusterAction::EmitVerdict {
                    wt: txn.wt.clone(),
                    authoritative_error: txn.barrier.has_authoritative_error(),
                };
                // Transaction complete ⇒ start the next serialized one
                // (self-recheck of the just-finished WT takes priority
                // over the queue: its overlay changed under it).
                self.start_next_after_settle(txn.wt);
                action
            }
        }
    }

    fn on_deactivated(&mut self, wt: WtId) -> ClusterAction {
        // Prune a queued check for a now-inactive WT. An in-flight
        // transaction is left to settle harmlessly (the adapter does not
        // publish a deactivated WT; RA teardown is ClusterLifecycle's,
        // at 1→0 — orthogonal to this sequencer's A/B invariants).
        self.pending.retain(|w| w != &wt);
        if self.current.as_ref().is_some_and(|a| a.wt == wt) {
            // It deactivated mid-check: cancel the self-recheck so we do
            // not needlessly re-check a WT the daemon just decided is
            // idle. The in-flight barrier still settles (harmless).
            self.recheck_current = false;
        }
        ClusterAction::Idle
    }

    /// After a transaction settles, begin the next one (if any), keeping
    /// the single-in-flight invariant. NOT a public/`SwitchOverlay`-
    /// returning path on its own — `on_lsp`'s Settled arm returns the
    /// `EmitVerdict`; the *next* `SwitchOverlay` is observed by the
    /// adapter via [`take_followup`](Self::take_followup) immediately
    /// after, so exactly one verdict and at most one new switch result
    /// from one settle (keeps `on_event`'s "one action" shape while
    /// preserving serialization).
    fn start_next_after_settle(&mut self, just_finished: WtId) {
        let next = if self.recheck_current {
            self.recheck_current = false;
            Some(just_finished)
        } else {
            self.pending.pop_front()
        };
        if let Some(wt) = next {
            self.current = Some(ActiveTxn {
                wt,
                barrier: FlycheckBarrier::arm(false),
            });
        }
    }

    /// The `SwitchOverlay` for the transaction that
    /// [`on_event`](Self::on_event) just started as the *successor* of a
    /// settle, or `None`. The adapter calls this exactly once right
    /// after an [`ClusterAction::EmitVerdict`] to drive the next
    /// serialized overlay-switch. Modelled as a follow-up (not a second
    /// `on_event` return) so one event yields a deterministic
    /// (verdict, optional-next-switch) pair while the single-transaction
    /// invariant stays structural.
    pub fn take_followup(&mut self) -> Option<ClusterAction> {
        self.current
            .as_ref()
            .map(|a| ClusterAction::SwitchOverlay { wt: a.wt.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::PublishDiagnostics;
    use crate::{Diagnostic, Severity};
    use std::path::PathBuf;

    fn wt(s: &str) -> PathBuf {
        PathBuf::from(s)
    }
    fn rustc_err(uri: &str) -> LspEvent {
        LspEvent::Diagnostics(PublishDiagnostics {
            uri: uri.into(),
            authoritative_errors: 1,
            advisory_errors: 0,
            total: 1,
            diagnostics: vec![Diagnostic {
                file_path: uri.into(),
                line: 1,
                col: 1,
                severity: Severity::Error,
                code: Some("E0599".into()),
                message: "x".into(),
                source: Some("rustc".into()),
            }],
        })
    }

    #[test]
    fn idle_cluster_first_batch_starts_transaction() {
        let mut d = ClusterDriver::new();
        assert_eq!(
            d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/a") }),
            ClusterAction::SwitchOverlay { wt: wt("/r/a") }
        );
        assert!(d.is_busy());
    }

    #[test]
    fn judgment_a_second_wt_is_queued_not_started() {
        let mut d = ClusterDriver::new();
        d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/a") }); // starts A
        // B arrives while A in flight ⇒ queued, NOT a second switch.
        assert_eq!(
            d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/b") }),
            ClusterAction::Idle
        );
        assert_eq!(d.pending(), vec![wt("/r/b")]);
        // Duplicate B while queued ⇒ deduped (no stacking).
        assert_eq!(
            d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/b") }),
            ClusterAction::Idle
        );
        assert_eq!(d.pending(), vec![wt("/r/b")]);
        assert!(d.is_busy(), "still exactly one in-flight txn");
    }

    #[test]
    fn judgment_b_no_verdict_before_settle() {
        let mut d = ClusterDriver::new();
        d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/a") });
        // Pre-settle RA chatter (diagnostics, indexing-end) must NEVER
        // produce an EmitVerdict — only Idle (barrier still Waiting).
        for _ in 0..3 {
            assert_eq!(
                d.on_event(DriverEvent::Lsp(rustc_err("file:///r/a/x.rs"))),
                ClusterAction::Idle
            );
        }
        assert_eq!(
            d.on_event(DriverEvent::Lsp(LspEvent::IndexingEnded)),
            ClusterAction::Idle,
            "indexing-end is NOT a verdict boundary"
        );
        assert!(d.is_busy(), "no settle yet ⇒ txn still in flight");
    }

    #[test]
    fn verdict_emitted_exactly_at_settle_with_correct_bit() {
        let mut d = ClusterDriver::new();
        d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/a") });
        d.on_event(DriverEvent::Lsp(rustc_err("file:///r/a/x.rs")));
        // The flycheck-end settles the barrier ⇒ the ONE verdict, with
        // authoritative_error=true (the rustc error was in the window).
        assert_eq!(
            d.on_event(DriverEvent::Lsp(LspEvent::FlycheckEnded)),
            ClusterAction::EmitVerdict {
                wt: wt("/r/a"),
                authoritative_error: true
            }
        );
        // Transaction consumed ⇒ idle again, no follow-up (queue empty).
        assert!(!d.is_busy());
        assert_eq!(d.take_followup(), None);
    }

    #[test]
    fn green_verdict_when_no_authoritative_error_in_window() {
        let mut d = ClusterDriver::new();
        d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/a") });
        // No rustc error published; flycheck ends ⇒ authoritative-green.
        assert_eq!(
            d.on_event(DriverEvent::Lsp(LspEvent::FlycheckEnded)),
            ClusterAction::EmitVerdict {
                wt: wt("/r/a"),
                authoritative_error: false
            }
        );
    }

    #[test]
    fn settle_starts_next_queued_txn_serialized() {
        let mut d = ClusterDriver::new();
        d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/a") }); // A in flight
        d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/b") }); // B queued
        d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/c") }); // C queued
        // A settles ⇒ A's verdict, then B (FIFO) becomes the new txn.
        let v = d.on_event(DriverEvent::Lsp(LspEvent::FlycheckEnded));
        assert_eq!(
            v,
            ClusterAction::EmitVerdict {
                wt: wt("/r/a"),
                authoritative_error: false
            }
        );
        assert!(d.is_busy(), "B's txn started — still exactly one in flight");
        assert_eq!(
            d.take_followup(),
            Some(ClusterAction::SwitchOverlay { wt: wt("/r/b") })
        );
        assert_eq!(d.pending(), vec![wt("/r/c")], "C still queued behind B");
        // B settles ⇒ B verdict, C next.
        d.on_event(DriverEvent::Lsp(LspEvent::FlycheckEnded));
        assert_eq!(
            d.take_followup(),
            Some(ClusterAction::SwitchOverlay { wt: wt("/r/c") })
        );
        // C settles ⇒ C verdict, queue empty ⇒ idle, no follow-up.
        d.on_event(DriverEvent::Lsp(LspEvent::FlycheckEnded));
        assert!(!d.is_busy());
        assert_eq!(d.take_followup(), None);
    }

    #[test]
    fn change_to_in_flight_wt_triggers_self_recheck_after_settle() {
        let mut d = ClusterDriver::new();
        d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/a") }); // A in flight
        // A changes again mid-check ⇒ queued as a self-recheck, NOT a
        // second concurrent txn.
        assert_eq!(
            d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/a") }),
            ClusterAction::Idle
        );
        assert!(d.pending().is_empty(), "self-recheck is not a queue entry");
        // A settles ⇒ verdict, then A is re-checked (overlay changed).
        d.on_event(DriverEvent::Lsp(LspEvent::FlycheckEnded));
        assert_eq!(
            d.take_followup(),
            Some(ClusterAction::SwitchOverlay { wt: wt("/r/a") }),
            "self-recheck re-runs the just-finished WT"
        );
    }

    #[test]
    fn deactivated_prunes_queue_and_cancels_self_recheck() {
        let mut d = ClusterDriver::new();
        d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/a") }); // A in flight
        d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/b") }); // B queued
        d.on_event(DriverEvent::RoutedBatch { wt: wt("/r/a") }); // A self-recheck armed
        // B deactivates ⇒ pruned from queue.
        assert_eq!(
            d.on_event(DriverEvent::Deactivated { wt: wt("/r/b") }),
            ClusterAction::Idle
        );
        assert!(d.pending().is_empty());
        // A deactivates mid-check ⇒ self-recheck cancelled.
        d.on_event(DriverEvent::Deactivated { wt: wt("/r/a") });
        // A's in-flight barrier still settles harmlessly (verdict emitted
        // — the adapter simply won't publish a deactivated WT), and NO
        // follow-up re-check is started.
        d.on_event(DriverEvent::Lsp(LspEvent::FlycheckEnded));
        assert!(!d.is_busy());
        assert_eq!(
            d.take_followup(),
            None,
            "no needless re-check of an idle WT"
        );
    }

    #[test]
    fn lsp_event_with_no_transaction_is_idle_never_verdict() {
        let mut d = ClusterDriver::new();
        // Stray RA events with nothing in flight (idle cluster) ⇒ Idle,
        // never a verdict (nothing to attribute to).
        assert_eq!(
            d.on_event(DriverEvent::Lsp(LspEvent::FlycheckEnded)),
            ClusterAction::Idle
        );
        assert_eq!(
            d.on_event(DriverEvent::Lsp(rustc_err("file:///x.rs"))),
            ClusterAction::Idle
        );
        assert!(!d.is_busy());
    }
}
