//! #13 leg-B — per-worktree overlay **replay** on base-RA respawn (pure core).
//!
//! ## Why this module exists (design `D-13-CRASH-RESTART.md` §3)
//!
//! Model R multiplexes N active worktrees through ONE rust-analyzer (design
//! §6). AC#6 ([`crate::analyzer::Supervisor`]) already makes a base-RA crash /
//! `kill -9` transparently respawn — that is leg A, **reused verbatim, zero
//! new code**. But after a respawn the fresh RA's overlay state is *empty*:
//! every active worktree's overlay-set must be re-applied, or the first
//! post-respawn check for worktree *k* would silently run against
//! base-without-*k*'s-edits — a **wrong verdict**, the exact failure class
//! cargoless exists to prevent.
//!
//! Leg B is that replay. This module is the **#5-independent pure core** of
//! it: the live active-overlay set + the deterministic, generation-fenced
//! replay driver. It does **not** apply overlays to RA (that is #5's LSP
//! multiplexer, expressed here as the mockable [`RecoverySink`] seam) and it
//! makes **no #5-contract speculation** — the only thing assumed of #5 is the
//! inherent "re-apply this worktree's overlay" shape.
//!
//! ## Two refinements over the D-13 §3.2 sketch (strictly better, same intent)
//!
//! 1. **Generic over the worktree/overlay key types.** The design sketched
//!    `WorktreeId → OverlayHash` (the #7/#8 newtypes). Those modules are not
//!    integrated yet; hard-depending on them would couple this to an
//!    un-landed branch. [`ReplayQueue<W, O>`] is generic, so leg B builds and
//!    gates **now** with zero cross-stream dependency; the concrete
//!    `ReplayQueue<WorktreeId, OverlayHash>` is a one-line alias added at the
//!    #5/#7/#8 wire. Identical semantics, looser coupling.
//! 2. **Read-only snapshot replay, not destructive drain.** The sketch said
//!    "drain on replay". But if a *second* respawn supersedes a replay
//!    mid-flight, a drained queue would have lost entries the newer respawn
//!    still needs. Instead the queue is the **live active-overlay set**
//!    maintained by the activity layer (#12); [`replay`] takes a *read-only
//!    sorted snapshot* and is generation-fenced — a superseded replay simply
//!    aborts and the newer respawn re-runs against the still-intact queue. No
//!    half-applied state can ever be lost. This is the launch-critical
//!    correctness property and it is regression-guarded by
//!    [`tests::recovery_replay_superseded_by_newer_generation_aborts_cleanly`].
//!
//! ## The generation fence (the respawn-during-replay race)
//!
//! Each base-RA respawn bumps a [`Generation`] counter (it lives at the
//! respawn site — the supervisor — *not* in the queue, because it is a
//! property of "which RA instance are we replaying into", not of the overlay
//! set). [`replay`] captures the generation it started for; before applying
//! each overlay it re-checks the live generation. If a newer respawn bumped it
//! mid-replay, the in-flight replay is **aborted** ([`ReplayOutcome::Superseded`])
//! — the newer respawn will drive its own fresh, complete replay. A single
//! [`RecoverySink::reapply`] is atomic from `replay`'s view, so the precise,
//! honestly-stated guarantee is: once supersession is observable `replay`
//! issues **no further** reapplies and returns `Superseded` (**never**
//! `Completed`); at most the one overlay in flight when the newer respawn
//! landed is applied to the about-to-be-discarded RA — harmless, since that RA
//! is replaced anyway and the newer generation re-replays the full set. A
//! superseded partial pass is never silently treated as complete.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic "which base-RA instance are we on" counter. Cheap to clone
/// (`Arc`); lives at the respawn site (the AC#6 supervisor bumps it on every
/// transparent restart). The queue does **not** own it — supersession is a
/// property of the RA lifecycle, not of the overlay set.
#[derive(Debug, Clone, Default)]
pub struct Generation(Arc<AtomicU64>);

impl Generation {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bump on each base-RA (re)spawn; returns the new generation. The value a
    /// [`replay`] should be started for is the one current *after* the respawn
    /// that triggered it.
    pub fn bump(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The current generation (the live "which RA instance" value).
    #[must_use]
    pub fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// The live set of active per-worktree overlays to (re)apply on the next
/// base-RA (re)spawn. Maintained by the activity layer (#12): a worktree's
/// activation/overlay-change calls [`set`](Self::set); deactivation calls
/// [`remove`](Self::remove). `replay` only ever *reads* it.
///
/// Invariants (all regression-guarded):
/// * **Idempotent** — `set(w, o)` twice for the same `(w, o)` is one entry.
/// * **Latest-wins per worktree** — `set(w, o2)` after `set(w, o1)` keeps
///   only `o2`; a superseded overlay is never replayed (it was never
///   authoritative — replaying it would be a wrong verdict).
/// * **Deterministic order** — [`snapshot_sorted`](Self::snapshot_sorted) is
///   ordered by worktree key, so a respawn replay is reproducible (the same
///   determinism discipline `cargoless-cas::tree` holds for hashing).
#[derive(Debug, Clone)]
pub struct ReplayQueue<W: Ord + Clone, O: Clone> {
    /// `BTreeMap` gives latest-wins-by-key + sorted iteration for free.
    pending: BTreeMap<W, O>,
}

impl<W: Ord + Clone, O: Clone> Default for ReplayQueue<W, O> {
    fn default() -> Self {
        Self {
            pending: BTreeMap::new(),
        }
    }
}

impl<W: Ord + Clone, O: Clone> ReplayQueue<W, O> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record/refresh a worktree's current overlay (activity layer calls this
    /// on activation and on every overlay change). Latest-wins, idempotent.
    pub fn set(&mut self, worktree: W, overlay: O) {
        self.pending.insert(worktree, overlay);
    }

    /// Drop a worktree (deactivated / disconnected) so it is not replayed.
    pub fn remove(&mut self, worktree: &W) {
        self.pending.remove(worktree);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// A deterministic, worktree-sorted read-only snapshot. `replay` works off
    /// this; the queue itself is never consumed (so a superseded replay loses
    /// nothing).
    #[must_use]
    pub fn snapshot_sorted(&self) -> Vec<(W, O)> {
        self.pending
            .iter()
            .map(|(w, o)| (w.clone(), o.clone()))
            .collect()
    }
}

/// The #5 seam: re-apply one worktree's overlay to the freshly respawned RA.
/// #13 owns the queue + ordering + fence; #5's LSP-overlay multiplexer
/// implements this later. Mocked in tests — **zero #5-contract speculation**
/// beyond this inherent shape.
pub trait RecoverySink<W, O> {
    /// Re-establish `worktree`'s overlay (`overlay`) on the current base-RA.
    ///
    /// # Errors
    /// Any failure to re-apply (LSP transport, RA not ready, …). [`replay`]
    /// stops on the first error and reports it — a respawn whose replay
    /// errored must not be reported as a clean recovery.
    fn reapply(&mut self, worktree: &W, overlay: &O) -> io::Result<()>;
}

/// Outcome of a [`replay`] pass.
#[derive(Debug)]
pub enum ReplayOutcome {
    /// Every active worktree's overlay was re-applied for the captured
    /// generation. The respawn is transparent across the whole fleet.
    Completed { applied: usize },
    /// A newer base-RA respawn bumped the generation mid-replay. This replay
    /// is abandoned (its target RA is already being replaced); the newer
    /// respawn drives a fresh, complete replay. **Not** an error and **not**
    /// a completion — a distinct, honestly-typed state so a caller can never
    /// mistake an aborted partial replay for a finished one.
    Superseded { applied_before_abort: usize },
    /// [`RecoverySink::reapply`] failed for a worktree. The respawn must not
    /// be treated as a clean recovery.
    SinkError {
        applied_before_error: usize,
        error: io::Error,
    },
}

/// Re-apply every active worktree's overlay to a freshly (re)spawned base-RA,
/// in deterministic worktree order, fenced to `expected_generation`.
///
/// Drive this from the AC#6 `on_spawn` hook
/// ([`Supervisor::start_with_hook`](crate::analyzer::Supervisor::start_with_hook)):
/// on each (re)spawn, `let g = generation.bump(); replay(&queue, sink,
/// &generation, g)`. The fence makes a respawn-during-replay race safe — a
/// second `kill -9` mid-replay yields [`ReplayOutcome::Superseded`], never a
/// half-set silently reported as complete.
///
/// Read-only over `queue` (the live active set the activity layer owns), so a
/// superseded replay loses nothing.
pub fn replay<W, O>(
    queue: &ReplayQueue<W, O>,
    sink: &mut dyn RecoverySink<W, O>,
    generation: &Generation,
    expected_generation: u64,
) -> ReplayOutcome
where
    W: Ord + Clone,
    O: Clone,
{
    let snapshot = queue.snapshot_sorted();
    let mut applied = 0usize;

    for (w, o) in &snapshot {
        // Fence: re-check *before every apply* (cheap atomic load). A newer
        // respawn that bumped the generation supersedes us immediately — we
        // do not keep pushing overlays into an RA that is being replaced.
        if generation.current() != expected_generation {
            return ReplayOutcome::Superseded {
                applied_before_abort: applied,
            };
        }
        if let Err(error) = sink.reapply(w, o) {
            return ReplayOutcome::SinkError {
                applied_before_error: applied,
                error,
            };
        }
        applied += 1;
    }

    // Final fence check: if a respawn landed exactly as the last overlay went
    // in, the newer respawn must still drive its own replay — don't report a
    // stale-generation pass as the authoritative completion.
    if generation.current() != expected_generation {
        return ReplayOutcome::Superseded {
            applied_before_abort: applied,
        };
    }
    ReplayOutcome::Completed { applied }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records every `reapply` in order; can be scripted to fail at a given
    /// call or to bump the generation at a given call (to model a respawn
    /// landing mid-replay).
    struct MockSink<'g> {
        applied: RefCell<Vec<(String, String)>>,
        fail_at: Option<usize>,
        bump_at: Option<(usize, &'g Generation)>,
    }

    impl<'g> MockSink<'g> {
        fn new() -> Self {
            Self {
                applied: RefCell::new(Vec::new()),
                fail_at: None,
                bump_at: None,
            }
        }
        fn failing_at(mut self, n: usize) -> Self {
            self.fail_at = Some(n);
            self
        }
        fn bumping_at(mut self, n: usize, g: &'g Generation) -> Self {
            self.bump_at = Some((n, g));
            self
        }
    }

    impl RecoverySink<String, String> for MockSink<'_> {
        fn reapply(&mut self, w: &String, o: &String) -> io::Result<()> {
            let n = self.applied.borrow().len();
            if let Some((at, g)) = self.bump_at {
                if n == at {
                    g.bump(); // a newer base-RA respawn lands mid-replay
                }
            }
            if self.fail_at == Some(n) {
                return Err(io::Error::other("sink boom"));
            }
            self.applied.borrow_mut().push((w.clone(), o.clone()));
            Ok(())
        }
    }

    fn q(pairs: &[(&str, &str)]) -> ReplayQueue<String, String> {
        let mut q = ReplayQueue::new();
        for (w, o) in pairs {
            q.set((*w).to_string(), (*o).to_string());
        }
        q
    }

    #[test]
    fn recovery_enqueue_is_idempotent_same_pair() {
        let mut queue = ReplayQueue::new();
        queue.set("A".to_string(), "ov1".to_string());
        queue.set("A".to_string(), "ov1".to_string());
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn recovery_latest_wins_per_worktree_stale_overlay_never_replayed() {
        let mut queue = ReplayQueue::new();
        queue.set("A".to_string(), "old".to_string());
        queue.set("A".to_string(), "new".to_string()); // supersedes
        let snap = queue.snapshot_sorted();
        assert_eq!(snap, vec![("A".to_string(), "new".to_string())]);
        // The stale "old" overlay is structurally unreachable — replaying it
        // would be a wrong verdict.
        assert!(!snap.iter().any(|(_, o)| o == "old"));
    }

    #[test]
    fn recovery_snapshot_is_deterministic_sorted_by_worktree() {
        // Insertion order deliberately scrambled; snapshot must be WT-sorted.
        let queue = q(&[("C", "c"), ("A", "a"), ("B", "b")]);
        let snap = queue.snapshot_sorted();
        let order: Vec<&str> = snap.iter().map(|(w, _)| w.as_str()).collect();
        assert_eq!(order, ["A", "B", "C"], "replay order must be reproducible");
    }

    #[test]
    fn recovery_remove_drops_worktree_from_replay() {
        let mut queue = q(&[("A", "a"), ("B", "b")]);
        queue.remove(&"A".to_string());
        assert_eq!(
            queue.snapshot_sorted(),
            vec![("B".to_string(), "b".to_string())]
        );
    }

    #[test]
    fn recovery_replay_applies_all_in_sorted_order() {
        let queue = q(&[("C", "c"), ("A", "a"), ("B", "b")]);
        let genr = Generation::new();
        let g = genr.bump(); // generation 1 = the (re)spawn we replay into
        let mut sink = MockSink::new();
        let out = replay(&queue, &mut sink, &genr, g);

        match out {
            ReplayOutcome::Completed { applied } => assert_eq!(applied, 3),
            other => panic!("expected Completed, got {other:?}"),
        }
        let got: Vec<String> = sink
            .applied
            .borrow()
            .iter()
            .map(|(w, _)| w.clone())
            .collect();
        assert_eq!(got, ["A", "B", "C"], "applied in deterministic WT order");
    }

    #[test]
    fn recovery_empty_queue_replay_is_completed_noop() {
        let queue: ReplayQueue<String, String> = ReplayQueue::new();
        let genr = Generation::new();
        let g = genr.bump();
        let mut sink = MockSink::new();
        match replay(&queue, &mut sink, &genr, g) {
            ReplayOutcome::Completed { applied } => assert_eq!(applied, 0),
            other => panic!("expected empty Completed, got {other:?}"),
        }
        assert!(sink.applied.borrow().is_empty());
    }

    #[test]
    fn recovery_replay_propagates_sink_error_without_false_completion() {
        let queue = q(&[("A", "a"), ("B", "b"), ("C", "c")]);
        let genr = Generation::new();
        let g = genr.bump();
        let mut sink = MockSink::new().failing_at(1); // B fails
        match replay(&queue, &mut sink, &genr, g) {
            ReplayOutcome::SinkError {
                applied_before_error,
                ..
            } => assert_eq!(
                applied_before_error, 1,
                "A applied, B errored, C not reached"
            ),
            other => panic!("expected SinkError, got {other:?}"),
        }
        // A respawn whose replay errored is NOT a clean recovery.
        assert_eq!(sink.applied.borrow().len(), 1);
    }

    #[test]
    fn recovery_replay_superseded_by_newer_generation_aborts_cleanly() {
        // The launch-critical respawn-during-replay race. A single
        // `sink.reapply` is ATOMIC from `replay`'s view — `replay` cannot
        // un-ring a sink call already in flight. So the honest, guaranteed
        // property is: once a newer respawn is observable, `replay` issues
        // NO FURTHER reapplies and returns `Superseded` (never `Completed`);
        // at most the single overlay in flight when the respawn landed is
        // applied to the about-to-be-discarded RA (harmless — the newer
        // respawn drives its own full fresh replay).
        //
        // Here the respawn lands DURING C's reapply (`bumping_at(2)`): A, B
        // apply normally; C is the in-flight one (unavoidably applied); D is
        // NOT issued — the fence trips before it. ⇒ Superseded{3}, [A,B,C],
        // D never pushed into the doomed RA, and crucially NOT Completed.
        let queue = q(&[("A", "a"), ("B", "b"), ("C", "c"), ("D", "d")]);
        let genr = Generation::new();
        let g = genr.bump(); // gen 1
        let mut sink = MockSink::new().bumping_at(2, &genr);
        match replay(&queue, &mut sink, &genr, g) {
            ReplayOutcome::Superseded {
                applied_before_abort,
            } => assert_eq!(
                applied_before_abort, 3,
                "A,B normal; respawn during C (in-flight, atomic) ⇒ C applied, \
                 D never issued — Superseded, never Completed"
            ),
            other => panic!("expected Superseded, got {other:?}"),
        }
        // D was NOT pushed into the doomed RA; C was (in-flight, unavoidable).
        let names: Vec<String> = sink
            .applied
            .borrow()
            .iter()
            .map(|(w, _)| w.clone())
            .collect();
        assert_eq!(names, ["A", "B", "C"], "D must never reach the doomed RA");
    }

    #[test]
    fn recovery_final_fence_catches_respawn_on_last_overlay() {
        // Edge: the respawn lands exactly as the final overlay goes in. The
        // post-loop fence must still report Superseded — the newer RA needs
        // its own full replay; this pass is not authoritative.
        let queue = q(&[("A", "a")]);
        let genr = Generation::new();
        let g = genr.bump();
        // Bump at call index 0 = during the only apply ⇒ post-loop fence trips.
        let mut sink = MockSink::new().bumping_at(0, &genr);
        match replay(&queue, &mut sink, &genr, g) {
            ReplayOutcome::Superseded {
                applied_before_abort,
            } => assert_eq!(applied_before_abort, 1),
            other => panic!("expected Superseded at final fence, got {other:?}"),
        }
    }

    #[test]
    fn recovery_generation_bumps_monotonically() {
        let genr = Generation::new();
        assert_eq!(genr.current(), 0);
        assert_eq!(genr.bump(), 1);
        assert_eq!(genr.bump(), 2);
        assert_eq!(genr.current(), 2);
    }

    #[test]
    fn recovery_replay_into_correct_generation_completes() {
        // Sanity: matching generation, no supersede ⇒ Completed (the fence
        // does not false-positive on a stable generation).
        let queue = q(&[("A", "a"), ("B", "b")]);
        let genr = Generation::new();
        genr.bump();
        genr.bump(); // gen now 2
        let mut sink = MockSink::new();
        match replay(&queue, &mut sink, &genr, 2) {
            ReplayOutcome::Completed { applied } => assert_eq!(applied, 2),
            other => panic!("expected Completed at stable gen 2, got {other:?}"),
        }
    }
}
