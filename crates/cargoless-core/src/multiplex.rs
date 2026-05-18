//! #5 driver-loop — open-set → LSP-verb lowering + per-WT switch
//! composition (Model R Stream C #158, I/O-shell increment-2: the pure
//! correctness core).
//!
//! ## What this is
//!
//! The multiplex driver moves the **one shared rust-analyzer** between
//! worktrees by applying each WT's `overlay::OverlaySet` (the files that
//! differ from base) as LSP document overlays. #5's pure core
//! ([`overlay::diff`], now on main) computes the *minimal isolation-safe
//! delta* between two overlay-sets. This module is the next pure layer:
//! turning that delta into the **minimal correct LSP verb sequence**,
//! tracking the open-set RA actually holds — the driver-state #5's
//! design doc deliberately kept *out* of `overlay` (whether a path needs
//! `didOpen` vs `didChange` depends on what the multiplexer has sent RA
//! so far, which is orthogonal to the set-difference).
//!
//! Pure: no `LspClient`, no I/O, no flycheck wait. The flycheck-end
//! barrier + threaded driver loop + live `LspClient` calls (the F8-redo
//! isolation guarantee — never snapshot W's verdict until W's overlay +
//! flycheck have settled) are I/O-shell increment-3. This split is the
//! same pure-core-first structure that made `overlay::diff` /
//! `cluster::hash` structurally backstoppable: the load-bearing
//! correctness (verb minimality + no-stale-open across a WT switch) is
//! exhaustively unit-tested here; the shell just executes the verbs +
//! enforces the barrier.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::Diagnostic;
use crate::overlay::{OverlayOp, OverlaySet, diff};

/// The LSP verb the I/O shell (increment-3) sends `LspClient` for one
/// lowered overlay op. `DidOpen`/`DidChange`/`DidClose` map 1:1 to
/// `lsp::LspClient::{did_open,did_change,did_close}` (the `did_close`
/// primitive landed in increment-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspVerb {
    /// RA has not seen this URI yet — open it with `content`.
    DidOpen { path: PathBuf, content: String },
    /// RA already has this URI open — replace its content (full-doc
    /// sync, matching `lsp::LspClient::did_change`'s v0 model).
    DidChange { path: PathBuf, content: String },
    /// Revert RA to base/on-disk for this URI (the isolation op — a
    /// stale prior-WT overlay that is NOT closed here is exactly how
    /// worktree V's content would contaminate worktree W's verdict).
    DidClose { path: PathBuf },
}

/// Tracks which URIs RA currently holds an overlay open for, plus which
/// overlay-set is applied. Pure: feed it `OverlaySet` selections via
/// [`switch_to`](Self::switch_to); it emits the minimal correct
/// [`LspVerb`] sequence and maintains the open-set. No I/O.
#[derive(Debug, Clone, Default)]
pub struct OverlayMultiplexer {
    /// Paths RA currently has an overlay open for. `BTreeSet` ⇒
    /// deterministic iteration (reproducible inspection/tests).
    open: BTreeSet<PathBuf>,
    /// The overlay-set currently applied to RA (last switched-to WT's
    /// set; empty = base state, RA sees on-disk for everything).
    applied: OverlaySet,
}

impl OverlayMultiplexer {
    /// Fresh multiplexer at base state (nothing open, base applied).
    pub fn new() -> Self {
        Self::default()
    }

    /// Lower one op-list to LSP verbs, updating the open-set.
    ///
    /// * `Apply` first time (path not open) ⇒ `DidOpen` + track;
    /// * `Apply` while already open ⇒ `DidChange`;
    /// * `Close` ⇒ `DidClose` + untrack.
    ///
    /// The untrack is defensively idempotent: by `overlay::diff`'s proven
    /// Close-completeness every `Close` path was a previous `Apply` (hence
    /// open), so `remove` always finds it — but a redundant `Close`
    /// cannot corrupt state (set-remove of an absent key is a no-op, and
    /// a `didClose` of an un-open URI is an RA no-op).
    fn lower(&mut self, ops: &[OverlayOp]) -> Vec<LspVerb> {
        let mut out = Vec::with_capacity(ops.len());
        for op in ops {
            match op {
                OverlayOp::Close { path } => {
                    self.open.remove(path);
                    out.push(LspVerb::DidClose { path: path.clone() });
                }
                OverlayOp::Apply { path, content } => {
                    // `insert` returns true iff the key was NOT present —
                    // i.e. RA has not seen this URI ⇒ first-time open.
                    if self.open.insert(path.clone()) {
                        out.push(LspVerb::DidOpen {
                            path: path.clone(),
                            content: content.clone(),
                        });
                    } else {
                        out.push(LspVerb::DidChange {
                            path: path.clone(),
                            content: content.clone(),
                        });
                    }
                }
            }
        }
        out
    }

    /// **The load-bearing pure path.** Switch the one shared RA from the
    /// currently-applied overlay to `target` (the next worktree's
    /// overlay-set). Composes `overlay::diff` (the proven minimal +
    /// isolation-correct delta — Closes strictly before Applies, every
    /// prev-only path Closed) with open-set lowering.
    ///
    /// Post-conditions (the cross-worktree isolation guarantee, in pure
    /// form — the I/O shell adds only the flycheck barrier on top):
    /// * `self.applied == *target`;
    /// * the open-set equals `target`'s key-set — **no path that was
    ///   overlaid for the previous worktree but not `target` remains
    ///   open** (a stale-V overlay surviving into W's analysis is exactly
    ///   the contamination the one-RA-multiplex must never allow;
    ///   `overlay::diff` proves the Close set, `lower` executes it as
    ///   `DidClose` + untrack);
    /// * re-selecting the already-applied set emits **zero** verbs
    ///   (minimality — no needless RA re-analysis).
    pub fn switch_to(&mut self, target: &OverlaySet) -> Vec<LspVerb> {
        let ops = diff(&self.applied, target);
        let verbs = self.lower(&ops);
        self.applied = target.clone();
        verbs
    }

    /// Paths RA currently has an overlay open for (deterministic order).
    /// Inspection/tests; the I/O shell does not need it (it just sends
    /// the verbs `switch_to` returns).
    pub fn open_paths(&self) -> impl Iterator<Item = &Path> {
        self.open.iter().map(PathBuf::as_path)
    }
}

/// Attribute diagnostics to the worktree whose overlay is applied. Since
/// per-WT checks serialize through the one shared RA, every diagnostic
/// produced *while WT `wt`'s overlay is applied* belongs to `wt`. This is
/// the mechanical tag; the *temporal* correctness (only reading
/// diagnostics that belong to the just-switched WT) is the flycheck-end
/// barrier the I/O shell (increment-3) enforces — `overlay::diff` +
/// [`OverlayMultiplexer::switch_to`] guarantee the *overlay* is exactly
/// `wt`'s, the barrier guarantees the *diagnostics read* are post-settle.
pub fn tag_for_worktree<W: Clone>(wt: &W, diags: &[Diagnostic]) -> Vec<(W, Diagnostic)> {
    diags.iter().map(|d| (wt.clone(), d.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oset(pairs: &[(&str, &str)]) -> OverlaySet {
        OverlaySet::from_pairs(pairs.iter().map(|(p, c)| (*p, *c)))
    }

    fn opened(m: &OverlayMultiplexer) -> Vec<String> {
        m.open_paths()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn base_to_worktree_opens_all_first_time() {
        let mut m = OverlayMultiplexer::new();
        let verbs = m.switch_to(&oset(&[("a.rs", "A"), ("z.rs", "Z")]));
        // overlay::diff yields sorted Applies (a then z); first time ⇒
        // DidOpen each.
        assert_eq!(
            verbs,
            vec![
                LspVerb::DidOpen {
                    path: "a.rs".into(),
                    content: "A".into()
                },
                LspVerb::DidOpen {
                    path: "z.rs".into(),
                    content: "Z".into()
                },
            ]
        );
        assert_eq!(opened(&m), vec!["a.rs", "z.rs"]);
    }

    #[test]
    fn reselect_same_set_emits_nothing() {
        let mut m = OverlayMultiplexer::new();
        let s = oset(&[("a.rs", "A")]);
        m.switch_to(&s);
        assert_eq!(
            m.switch_to(&s),
            vec![],
            "re-select ⇒ zero verbs (minimality)"
        );
    }

    #[test]
    fn content_change_of_open_file_is_didchange_not_didopen() {
        let mut m = OverlayMultiplexer::new();
        m.switch_to(&oset(&[("a.rs", "old")]));
        let verbs = m.switch_to(&oset(&[("a.rs", "new")]));
        assert_eq!(
            verbs,
            vec![LspVerb::DidChange {
                path: "a.rs".into(),
                content: "new".into()
            }],
            "already-open path ⇒ DidChange"
        );
    }

    #[test]
    fn cross_worktree_switch_no_stale_open_isolation() {
        // THE load-bearing isolation case. WT-V applied, switch to WT-W:
        // V-only file MUST DidClose + leave the open-set; shared-changed
        // DidChange; identical untouched (no verb); W-only DidOpen. After
        // the switch NO V-only path may remain open (stale-V overlay in
        // W's analysis = the contamination one-RA-multiplex must never
        // allow).
        let mut m = OverlayMultiplexer::new();
        m.switch_to(&oset(&[
            ("only_v.rs", "V"),
            ("shared.rs", "v-ver"),
            ("common.rs", "same"),
        ]));
        let verbs = m.switch_to(&oset(&[
            ("shared.rs", "w-ver"),
            ("common.rs", "same"),
            ("only_w.rs", "W"),
        ]));
        assert_eq!(
            verbs,
            vec![
                // overlay::diff: Closes first (sorted), then Applies
                // (sorted). only_v Closed; only_w first-time DidOpen;
                // shared content-changed but already-open ⇒ DidChange;
                // common identical ⇒ no verb.
                LspVerb::DidClose {
                    path: "only_v.rs".into()
                },
                LspVerb::DidOpen {
                    path: "only_w.rs".into(),
                    content: "W".into()
                },
                LspVerb::DidChange {
                    path: "shared.rs".into(),
                    content: "w-ver".into()
                },
            ]
        );
        // The isolation post-condition: open-set is exactly W's keys; no
        // V-only path survives.
        assert_eq!(opened(&m), vec!["common.rs", "only_w.rs", "shared.rs"]);
        assert!(
            !opened(&m).iter().any(|p| p == "only_v.rs"),
            "no stale V-only overlay may remain open after switching to W"
        );
    }

    #[test]
    fn close_then_reapply_reopens() {
        // A path closed (WT no longer overlays it) then later re-applied
        // must DidOpen again — RA reverted it to base on the Close, so
        // re-overlaying is a fresh open, not a change.
        let mut m = OverlayMultiplexer::new();
        m.switch_to(&oset(&[("a.rs", "A1")]));
        m.switch_to(&OverlaySet::new()); // back to base ⇒ DidClose a.rs
        assert!(opened(&m).is_empty(), "base state ⇒ nothing open");
        let verbs = m.switch_to(&oset(&[("a.rs", "A2")]));
        assert_eq!(
            verbs,
            vec![LspVerb::DidOpen {
                path: "a.rs".into(),
                content: "A2".into()
            }],
            "re-apply after close ⇒ DidOpen (fresh), not DidChange"
        );
    }

    #[test]
    fn switch_to_base_closes_everything() {
        let mut m = OverlayMultiplexer::new();
        m.switch_to(&oset(&[("a.rs", "A"), ("b.rs", "B")]));
        let verbs = m.switch_to(&OverlaySet::new());
        assert_eq!(
            verbs,
            vec![
                LspVerb::DidClose {
                    path: "a.rs".into()
                },
                LspVerb::DidClose {
                    path: "b.rs".into()
                },
            ]
        );
        assert!(opened(&m).is_empty());
    }

    #[test]
    fn tag_for_worktree_attributes_each_diagnostic() {
        use crate::Severity;
        let d = Diagnostic {
            file_path: "physics/src/orbit.rs".into(),
            line: 1,
            col: 1,
            severity: Severity::Error,
            code: None,
            message: "x".into(),
            source: None,
        };
        let tagged = tag_for_worktree(&"wt-A".to_string(), std::slice::from_ref(&d));
        assert_eq!(tagged, vec![("wt-A".to_string(), d.clone())]);
        // Empty in ⇒ empty out (no fabricated attribution).
        assert!(tag_for_worktree(&"wt-A".to_string(), &[]).is_empty());
    }
}
