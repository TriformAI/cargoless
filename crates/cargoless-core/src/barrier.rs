//! #5 flycheck-end barrier — the **temporal** half of the one-RA
//! multiplex isolation guarantee (Model R Stream C #158, I/O-shell
//! increment-3: the pure correctness core; the live `LspClient` /
//! `Receiver<LspEvent>` driving is the follow-on thin adapter).
//!
//! ## Why this exists (the load-bearing temporal judgment)
//!
//! [`overlay::diff`](crate::overlay) + [`multiplex::OverlayMultiplexer`]
//! (both proven, on main) give the **spatial** isolation guarantee: when
//! the single shared rust-analyzer is switched from worktree V to
//! worktree W, the overlay RA holds is *exactly* W's set — no stale-V
//! overlay survives. That is necessary but **not sufficient** for a
//! correct per-worktree verdict, because RA's analysis is asynchronous:
//! after we apply W's overlay and trigger W's flycheck, RA keeps
//! streaming `publishDiagnostics` / `$/progress` events, and some of
//! those reflect *V's* just-finished (or still-running) flycheck, not
//! W's. Snapshotting W's verdict the instant the overlay is applied — or
//! at the first flycheck-end we happen to see — would attribute V's
//! diagnostics (or a half-analysed pre-flycheck state) to W. That is a
//! direct **wrong-verdict** class, the temporal twin of the spatial
//! contamination `multiplex` already prevents.
//!
//! This module is the pure state machine that closes the temporal hole.
//!
//! ### The invariant (named for the layer-3 structural backstop)
//!
//! > A worktree W's verdict is **never** snapshotted until W's overlay
//! > **and** W's own flycheck-end have both settled.
//!
//! Made precise and falsifiable:
//!
//! * **Settle-condition.** [`FlycheckBarrier`] reaches [`Settled`] EXACTLY
//!   at the `(1 + stale_ends)`-th [`LspEvent::FlycheckEnded`] observed
//!   after [`arm`](FlycheckBarrier::arm) — where `stale_ends` is the
//!   number of in-flight flycheck-ends carried over from the worktree we
//!   switched away from (the driver supplies this; see below). It is
//!   reached at *no other event*: [`LspEvent::Diagnostics`] only mutates
//!   the window, and [`LspEvent::IndexingEnded`] is explicitly inert (the
//!   FIELD FINDING #3a distinction — RA's project-indexing end rides the
//!   same `$/progress`/`end` shape as a flycheck end but is **not** a
//!   verdict boundary; mirrored here so a switch can never settle on it).
//!   [`LspEvent::FlycheckFailed`] is the one terminal exception: cargo did
//!   not run successfully, so the barrier settles RED immediately.
//! * **No pre-settle escape.** [`snapshot`](FlycheckBarrier::snapshot)
//!   and [`has_authoritative_error`](FlycheckBarrier::has_authoritative_error)
//!   are honest only once [`is_settled`](FlycheckBarrier::is_settled).
//!   Before W's flycheck-end the state is [`Waiting`] *by construction* —
//!   there is no code path that returns [`Settled`] without consuming W's
//!   flycheck-end. After settle, every further event is a no-op (the
//!   latch is idempotent), so a late stray publish cannot mutate a
//!   verdict the driver may already have read.
//! * **No V→W bleed.** When a stale (V's) flycheck-end is consumed the
//!   per-URI window is **cleared** at that boundary, so V's window can
//!   never contribute to W's snapshot. W's snapshot is exactly the
//!   publishes from W's flycheck window.
//!
//! ### The one bit the barrier cannot divine — and why the driver owns it
//!
//! Only `$/progress` **`end`** is on the wire ([`LspEvent::FlycheckEnded`]);
//! there is no flycheck-*begin* event. So "is a stale V flycheck still
//! running at switch time?" is **not** derivable from the event stream.
//! The driver, which *issued* the saves, is the sole authority on that —
//! it knows whether it triggered a flycheck for the worktree it is
//! switching away from that may still be in flight. Pushing exactly that
//! one bit to [`arm`](FlycheckBarrier::arm) keeps the barrier a pure,
//! total state machine whose temporal-isolation invariant is exhaustively
//! unit-testable (the same author/caller split that made `overlay::diff`
//! and `cluster::hash` structurally backstoppable rather than only
//! integration-tested).
//!
//! ### Residual assumption (stated, not over-claimed)
//!
//! "W's snapshot = W's flycheck-window publishes" rests on RA
//! re-publishing the crate's diagnostics within a flycheck pass. That is
//! **exactly** the assumption [`model`](crate::model) already makes when
//! it gates GREEN on the authoritative (flycheck) tier; this module does
//! not introduce a stronger one.

use std::collections::BTreeMap;

use crate::lsp::{LspEvent, PublishDiagnostics};

/// What the barrier tells the driver after each observed [`LspEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarrierState {
    /// Not yet — keep feeding events. W's verdict is **not** ready and
    /// MUST NOT be snapshotted (the load-bearing "never settle early").
    Waiting,
    /// W's own flycheck has ended; the accumulated per-URI window IS W's
    /// authoritative snapshot — and only W's (every pre-window / stale-V
    /// publish was dropped at the window boundary).
    Settled,
}

/// Pure flycheck-end barrier for **one** worktree switch.
///
/// Construct with [`arm`](Self::arm) (or [`arm_skipping`](Self::arm_skipping)),
/// feed every [`LspEvent`] the reader thread emits via
/// [`observe`](Self::observe), and read the verdict only once
/// [`is_settled`](Self::is_settled). One barrier == one switch: after
/// settle the driver reads the snapshot and arms a fresh barrier for the
/// next switch (post-settle idempotence is the safety net for a late
/// stray publish, not a reuse mechanism).
///
/// See the module docs for the precise, falsifiable temporal-isolation
/// invariant this type enforces.
#[derive(Debug, Clone)]
pub struct FlycheckBarrier {
    /// Flycheck-ends still to be **discarded** before W's authoritative
    /// one. The one-RA driver only ever needs `0` (prior worktree was
    /// flycheck-quiescent) or `1` (it triggered a flycheck for the
    /// worktree it switched away from that may still be running — the
    /// only stale-end source). `>1` is supported and tested for
    /// robustness, but is not produced by the v0 driver.
    stale_ends_remaining: u32,
    /// Per-URI latest `publishDiagnostics` for the **current** window.
    /// LSP replace semantics: a new publish for a URI supersedes the
    /// prior one for that URI (RA's `publishDiagnostics` is the full set
    /// for a document, including the empty "cleared" publish). Cleared
    /// when a stale end is consumed (window boundary) so V's window can
    /// never bleed into W's snapshot.
    window: BTreeMap<String, PublishDiagnostics>,
    /// Latched once W's flycheck-end is consumed. Further events are
    /// no-ops — a verdict the driver may already have read can never be
    /// mutated by a late stray event.
    settled: bool,
}

impl FlycheckBarrier {
    /// Arm for a switch.
    ///
    /// `prior_flycheck_in_flight` is the driver's local knowledge that it
    /// triggered a flycheck for the worktree it is switching **away
    /// from** which may still be running (⇒ exactly one stale
    /// flycheck-end to skip before W's). This is caller knowledge *by
    /// construction*: no flycheck-begin is on the wire, so the barrier
    /// cannot divine it; the driver, which issued the saves, is the sole
    /// authority. Maps to `arm_skipping(if … {1} else {0})` — the only
    /// two values the one-RA driver ever needs.
    pub fn arm(prior_flycheck_in_flight: bool) -> Self {
        Self::arm_skipping(u32::from(prior_flycheck_in_flight))
    }

    /// Arm, discarding exactly `stale_ends` flycheck-ends before W's
    /// authoritative one. The general form of [`arm`](Self::arm); exposed
    /// (rather than poking fields) so the `>1` generality is exercised
    /// through the public API. The driver uses `arm`; this is for
    /// completeness and robustness testing.
    pub fn arm_skipping(stale_ends: u32) -> Self {
        Self {
            stale_ends_remaining: stale_ends,
            window: BTreeMap::new(),
            settled: false,
        }
    }

    /// Feed one event; returns the post-event [`BarrierState`].
    ///
    /// * [`LspEvent::Diagnostics`] ⇒ per-URI **replace** into the current
    ///   window (LSP full-document-set semantics). Stays [`Waiting`].
    /// * [`LspEvent::FlycheckEnded`] ⇒ if a stale end is still owed:
    ///   consume it, **clear the window** (V→W boundary), stay
    ///   [`Waiting`]; else: latch and return [`Settled`].
    /// * [`LspEvent::FlycheckFailed`] ⇒ latch [`Settled`] with a synthetic
    ///   authoritative diagnostic. A failed cargo/flycheck process cannot
    ///   be a green check.
    /// * [`LspEvent::IndexingEnded`] ⇒ inert — never settles, never
    ///   touches the window (the FIELD FINDING #3a non-boundary).
    /// * Any event after settle ⇒ no-op ([`Settled`]).
    pub fn observe(&mut self, ev: &LspEvent) -> BarrierState {
        if self.settled {
            // Idempotent latch: a verdict already exposed to the driver
            // can never be mutated by a late stray event.
            return BarrierState::Settled;
        }
        match ev {
            LspEvent::Diagnostics(pd) => {
                // Per-URI replace: RA's publishDiagnostics is the
                // authoritative full set for that document.
                self.window.insert(pd.uri.clone(), pd.clone());
                BarrierState::Waiting
            }
            LspEvent::FlycheckEnded => {
                if self.stale_ends_remaining > 0 {
                    // A stale (prior-worktree) flycheck-end. Consume it
                    // and drop everything published so far — those
                    // publishes belong to the worktree we switched away
                    // from; they must never reach W's snapshot.
                    self.stale_ends_remaining -= 1;
                    self.window.clear();
                    BarrierState::Waiting
                } else {
                    // W's own flycheck-end. The window now holds exactly
                    // W's flycheck-window publishes — the authoritative
                    // snapshot. Latch.
                    self.settled = true;
                    BarrierState::Settled
                }
            }
            LspEvent::FlycheckFailed { message } => {
                let pd = crate::lsp::flycheck_failure_diagnostics(message.clone());
                self.window.insert(pd.uri.clone(), pd);
                self.settled = true;
                BarrierState::Settled
            }
            // FIELD FINDING #3a, mirrored: RA's project-indexing end
            // rides the same `$/progress`/`end` shape as a flycheck end
            // but is NOT a verdict boundary. A switch must never settle
            // on it — and it must not perturb the window either.
            LspEvent::IndexingEnded => BarrierState::Waiting,
        }
    }

    /// `true` once W's flycheck-end has been consumed. The driver gates
    /// every read of the verdict on this — never on window emptiness.
    pub fn is_settled(&self) -> bool {
        self.settled
    }

    /// W's authoritative per-URI snapshot — the publishes from W's
    /// flycheck window. Meaningful **only** once
    /// [`is_settled`](Self::is_settled); returned in either state (so the
    /// driver gates on `is_settled`, not on emptiness — an empty window
    /// at settle is a legitimately green W, distinct from a not-yet
    /// state).
    pub fn snapshot(&self) -> &BTreeMap<String, PublishDiagnostics> {
        &self.window
    }

    /// `true` iff W's window has any **authoritative** (cargo-check /
    /// `source:"rustc"`) error — the exact verdict reduction
    /// [`model`](crate::model)'s GREEN gate binds to, applied to exactly
    /// W's window. Honest only post-[`Settled`](BarrierState::Settled);
    /// the driver must check [`is_settled`](Self::is_settled) first.
    pub fn has_authoritative_error(&self) -> bool {
        self.window
            .values()
            .any(PublishDiagnostics::has_authoritative_error)
    }

    /// `true` iff W's window has any error-severity diagnostic from any
    /// source. This is intentionally weaker than [`has_authoritative_error`]:
    /// it supports the tf-multiverse development loop where Cargoless replaces
    /// `cargo check`/`cargo clippy` as an always-on RA-native signal, while
    /// full Cargo authority is deferred to explicit compile/build gates.
    pub fn has_any_error(&self) -> bool {
        self.window
            .values()
            .any(PublishDiagnostics::has_any_severity_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Diagnostic, Severity};

    /// Build a `publishDiagnostics` event for `uri` with `auth` rustc
    /// errors (each a real `Diagnostic` in the rich list so the count and
    /// the detail stay consistent, exactly as the extractor produces).
    fn diags(uri: &str, auth: usize) -> LspEvent {
        let rich = (0..auth)
            .map(|i| Diagnostic {
                file_path: uri.into(),
                line: (i as u32) + 1,
                col: 1,
                severity: Severity::Error,
                code: Some("E0599".into()),
                message: "no method".into(),
                source: Some("rustc".into()),
            })
            .collect::<Vec<_>>();
        LspEvent::Diagnostics(PublishDiagnostics {
            uri: uri.into(),
            authoritative_errors: auth,
            advisory_errors: 0,
            total: auth,
            diagnostics: rich,
        })
    }

    /// An advisory-only (native rust-analyzer) error publish — must NOT
    /// count toward the authoritative verdict.
    fn advisory(uri: &str) -> LspEvent {
        LspEvent::Diagnostics(PublishDiagnostics {
            uri: uri.into(),
            authoritative_errors: 0,
            advisory_errors: 1,
            total: 1,
            diagnostics: vec![Diagnostic {
                file_path: uri.into(),
                line: 1,
                col: 1,
                severity: Severity::Error,
                code: None,
                message: "native".into(),
                source: Some("rust-analyzer".into()),
            }],
        })
    }

    fn failed() -> LspEvent {
        LspEvent::FlycheckFailed {
            message: "Flycheck failed to run the following command: cargo check".into(),
        }
    }

    #[test]
    fn quiescent_prior_first_flycheck_end_settles() {
        // arm(false): no stale end. Publishes accumulate; the FIRST
        // flycheck-end is W's ⇒ Settled with W's window.
        let mut b = FlycheckBarrier::arm(false);
        assert_eq!(
            b.observe(&diags("file:///w/a.rs", 0)),
            BarrierState::Waiting
        );
        assert_eq!(
            b.observe(&diags("file:///w/b.rs", 1)),
            BarrierState::Waiting
        );
        assert!(!b.is_settled());
        assert_eq!(b.observe(&LspEvent::FlycheckEnded), BarrierState::Settled);
        assert!(b.is_settled());
        assert!(b.has_authoritative_error(), "b.rs has a rustc error");
        assert_eq!(b.snapshot().len(), 2);
    }

    #[test]
    fn never_settles_before_any_flycheck_end() {
        // The "never settle early" half. Many diagnostics + an
        // indexing-end must NOT settle — only a flycheck-end may.
        let mut b = FlycheckBarrier::arm(false);
        for _ in 0..5 {
            assert_eq!(
                b.observe(&diags("file:///w/x.rs", 1)),
                BarrierState::Waiting
            );
        }
        assert_eq!(
            b.observe(&LspEvent::IndexingEnded),
            BarrierState::Waiting,
            "indexing-end is NOT a verdict boundary (FIELD FINDING #3a)"
        );
        assert!(!b.is_settled(), "no flycheck-end seen ⇒ never settled");
    }

    #[test]
    fn stale_in_flight_end_is_skipped_and_v_window_dropped() {
        // THE F8-redo temporal-isolation case. V flycheck still running
        // at switch (arm(true)). RA streams: V publishes → V's stale
        // flycheck-end → W publishes → W's flycheck-end. The stale end
        // must be skipped, V's window dropped, and only W's window
        // survive into the settled snapshot.
        let mut b = FlycheckBarrier::arm(true);
        // V's leftover analysis (a V-only error file).
        assert_eq!(
            b.observe(&diags("file:///v/only_v.rs", 3)),
            BarrierState::Waiting
        );
        assert!(b.has_authoritative_error(), "V window currently has errors");
        // V's stale flycheck-end: consumed, NOT a settle; V window dropped.
        assert_eq!(b.observe(&LspEvent::FlycheckEnded), BarrierState::Waiting);
        assert!(!b.is_settled(), "stale end must not settle the barrier");
        assert!(
            b.snapshot().is_empty(),
            "V's window must be dropped at the stale boundary"
        );
        assert!(
            !b.has_authoritative_error(),
            "no V error may survive into W's window"
        );
        // W's window: a clean file then W's flycheck-end.
        assert_eq!(
            b.observe(&diags("file:///w/only_w.rs", 0)),
            BarrierState::Waiting
        );
        assert_eq!(b.observe(&LspEvent::FlycheckEnded), BarrierState::Settled);
        assert!(b.is_settled());
        // The isolation post-condition: snapshot is exactly W's; the
        // V-only error file is NOT present, W is green.
        assert!(b.snapshot().contains_key("file:///w/only_w.rs"));
        assert!(
            !b.snapshot().contains_key("file:///v/only_v.rs"),
            "no stale-V URI may appear in W's settled snapshot"
        );
        assert!(
            !b.has_authoritative_error(),
            "W is authoritatively green; V's red must not be attributed to W"
        );
    }

    #[test]
    fn per_uri_replace_semantics() {
        // LSP publishDiagnostics is the full set for a document — a later
        // publish for a URI supersedes the earlier (incl. the empty
        // "cleared" publish that flips a file green).
        let mut b = FlycheckBarrier::arm(false);
        b.observe(&diags("file:///w/a.rs", 2)); // a.rs: 2 errors
        b.observe(&diags("file:///w/a.rs", 0)); // a.rs cleared → green
        b.observe(&LspEvent::FlycheckEnded);
        assert!(b.is_settled());
        assert_eq!(b.snapshot().len(), 1, "same URI replaced, not appended");
        assert!(
            !b.has_authoritative_error(),
            "the cleared (latest) publish wins → green"
        );
    }

    #[test]
    fn post_settle_events_are_no_ops() {
        // Idempotent latch: once the driver can read the verdict, a late
        // stray publish/flycheck-end cannot mutate it.
        let mut b = FlycheckBarrier::arm(false);
        b.observe(&diags("file:///w/a.rs", 0));
        assert_eq!(b.observe(&LspEvent::FlycheckEnded), BarrierState::Settled);
        // A stray late rustc-error publish for the same file + another
        // flycheck-end — must NOT flip the already-read green verdict.
        assert_eq!(
            b.observe(&diags("file:///w/a.rs", 5)),
            BarrierState::Settled
        );
        assert_eq!(b.observe(&LspEvent::FlycheckEnded), BarrierState::Settled);
        assert_eq!(b.observe(&LspEvent::IndexingEnded), BarrierState::Settled);
        assert!(
            !b.has_authoritative_error(),
            "post-settle events must not mutate the latched verdict"
        );
        assert_eq!(b.snapshot().len(), 1);
    }

    #[test]
    fn advisory_only_window_is_authoritatively_green() {
        // A native rust-analyzer error is advisory — it never asserts an
        // authoritative red (the #21 provenance split, applied to W's
        // window).
        let mut b = FlycheckBarrier::arm(false);
        b.observe(&advisory("file:///w/a.rs"));
        b.observe(&LspEvent::FlycheckEnded);
        assert!(b.is_settled());
        assert!(
            !b.has_authoritative_error(),
            "advisory-only ⇒ authoritatively green (no rustc error)"
        );
        assert_eq!(
            b.snapshot().len(),
            1,
            "the advisory publish is still tracked"
        );
    }

    #[test]
    fn flycheck_failure_settles_red() {
        let mut b = FlycheckBarrier::arm(false);
        assert_eq!(b.observe(&failed()), BarrierState::Settled);
        assert!(b.is_settled());
        assert!(
            b.has_authoritative_error(),
            "cargo/flycheck execution failure is authoritative red"
        );
    }

    #[test]
    fn multi_stale_end_robustness() {
        // Defensive: the general `arm_skipping(N)` form discards exactly
        // N ends before W's. Not produced by the v0 one-RA driver, but
        // the state machine must be total and correct for N>1.
        let mut b = FlycheckBarrier::arm_skipping(2);
        b.observe(&diags("file:///stale1.rs", 1));
        assert_eq!(b.observe(&LspEvent::FlycheckEnded), BarrierState::Waiting); // skip 1
        assert!(b.snapshot().is_empty());
        b.observe(&diags("file:///stale2.rs", 1));
        assert_eq!(b.observe(&LspEvent::FlycheckEnded), BarrierState::Waiting); // skip 2
        assert!(b.snapshot().is_empty());
        assert!(!b.is_settled(), "still owed W's own end");
        b.observe(&diags("file:///w/clean.rs", 0));
        assert_eq!(b.observe(&LspEvent::FlycheckEnded), BarrierState::Settled);
        assert!(b.is_settled());
        assert_eq!(b.snapshot().len(), 1);
        assert!(!b.has_authoritative_error());
    }

    #[test]
    fn indexing_end_does_not_drop_or_settle_window() {
        // IndexingEnded is fully inert: it neither settles nor perturbs
        // the accumulated window (distinct from a stale flycheck-end,
        // which DOES clear the window). This pins the FIELD FINDING #3a
        // distinction at the barrier boundary.
        let mut b = FlycheckBarrier::arm(false);
        b.observe(&diags("file:///w/a.rs", 1));
        assert_eq!(b.observe(&LspEvent::IndexingEnded), BarrierState::Waiting);
        assert_eq!(
            b.snapshot().len(),
            1,
            "indexing-end must not clear the window (only a stale flycheck-end does)"
        );
        assert_eq!(b.observe(&LspEvent::FlycheckEnded), BarrierState::Settled);
        assert!(
            b.has_authoritative_error(),
            "the pre-indexing-end error survived to W's settled snapshot"
        );
    }
}
