//! #5 — LSP overlay-set diffing (Model R Stream C, `D-FLEET-SHARED-DAEMON`
//! §6 — the pure, LSP/notify-INDEPENDENT correctness core).
//!
//! ## What this is
//!
//! Model R multiplexes **one** rust-analyzer across N worktrees by
//! applying each worktree's *overlay-set* — the set of files whose content
//! differs from `base.cache` (`git diff base..<wt>`) — to RA via LSP
//! document overlays (`didOpen`/`didChange`/`didClose`), exactly how an
//! IDE client overlays unsaved buffers (§6.1: not novel — RA's existing
//! IDE-mode capability at worktree scale).
//!
//! To check worktree W when worktree V's overlay is currently applied, the
//! daemon must send RA the **minimal** delta from V's overlay-set to W's.
//! This module is that pure delta: [`OverlaySet`] + [`diff`]. It does **no**
//! I/O, holds no `LspClient`, spawns nothing — so the delta correctness
//! (the load-bearing "never attribute V's diagnostics to W" property,
//! together with the flycheck-end barrier the driver enforces) is
//! exhaustively unit-tested with synthetic overlay-sets.
//!
//! The LSP wiring (a `did_close` verb added to `lsp.rs::LspClient`, the
//! open-set tracking that turns an [`OverlayOp::Apply`] into
//! `did_open`-vs-`did_change`, the per-WT flycheck-barrier driver loop +
//! WtId diagnostic tagging) is the thin I/O shell — a follow-up increment,
//! the established pure-core-first house pattern (cf. #174 topology / #4
//! router / #12 activity). Its behaviour is fully covered by the pure
//! tests here: a wrong delta is the only way cross-worktree contamination
//! enters, and that is exactly what `diff` is proven correct against.
//!
//! ## Why `Apply` not `Open`/`Change`
//!
//! Whether a path needs `textDocument/didOpen` (RA hasn't seen this URI)
//! or `didChange` (already open, new content) is **driver state**, not
//! overlay-set state — the same path is "open" or not depending on what
//! the multiplexer has sent RA so far, which is orthogonal to the
//! set-difference. So the pure core emits content-bearing
//! [`OverlayOp::Apply`]; the I/O shell tracks the open-set and lowers it
//! to the right verb. Keeping that split is what keeps THIS layer pure
//! and total.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One worktree's overlay-set: the files whose content differs from
/// `base.cache`, mapped to the worktree's content for that file. A
/// `BTreeMap` (not `HashMap`) so [`diff`]'s output order is deterministic
/// — load-bearing for reproducible LSP message sequences + tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlaySet {
    files: BTreeMap<PathBuf, String>,
}

impl OverlaySet {
    /// Empty overlay — the base state (RA sees on-disk/base for every
    /// file). The applied state before any worktree is selected.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from `(path, content)` pairs — the test seam and the shape
    /// the I/O shell produces from `git diff base..<wt>`.
    pub fn from_pairs<I, P, S>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (P, S)>,
        P: Into<PathBuf>,
        S: Into<String>,
    {
        Self {
            files: pairs
                .into_iter()
                .map(|(p, s)| (p.into(), s.into()))
                .collect(),
        }
    }

    /// Set/replace one file's overlay content.
    pub fn set(&mut self, path: impl Into<PathBuf>, content: impl Into<String>) {
        self.files.insert(path.into(), content.into());
    }

    /// `true` when this is the base state (no overlay) — the I/O shell
    /// uses this to know it can deactivate RA overlays entirely.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Number of overlaid files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// The overlay content for `path`, if it differs from base.
    pub fn get(&self, path: &Path) -> Option<&str> {
        self.files.get(path).map(String::as_str)
    }

    /// Iterate over overlaid `.rs` files in deterministic (sorted) order.
    /// Used by the CGLS-11 forced-reopen guard to pick a stable nudge
    /// target when `overlay::diff` yields zero verbs for a re-push of
    /// identical content. The `BTreeMap` guarantees sorted iteration.
    pub fn iter_rs(&self) -> impl Iterator<Item = (&Path, &str)> {
        self.files
            .iter()
            .filter(|(path, _)| path.extension().is_some_and(|ext| ext == "rs"))
            .map(|(path, content)| (path.as_path(), content.as_str()))
    }
}

/// A minimal LSP overlay operation in a delta. The I/O shell lowers
/// `Apply` to `textDocument/didOpen` (first time RA sees this URI) or
/// `didChange` (already open) by tracking its own open-set; `Close`
/// lowers to `textDocument/didClose` (RA reverts that URI to its
/// base/on-disk content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOp {
    /// RA must see `content` for `path` (it differs from base in the
    /// target worktree).
    Apply { path: PathBuf, content: String },
    /// `path` no longer differs from base in the target worktree — revert
    /// RA to base/on-disk for it (the load-bearing op: a stale `Apply`
    /// from the previously-applied worktree that is NOT closed here is
    /// exactly how worktree V's content would contaminate worktree W's
    /// verdict).
    Close { path: PathBuf },
}

/// The **minimal, deterministic** delta to move RA from the `prev`
/// overlay-set (currently applied) to the `next` overlay-set (the target
/// worktree). Pure — no I/O, total, exhaustively unit-tested.
///
/// Correctness contract (load-bearing for cross-worktree isolation):
/// * every path in `prev` but not `next` ⇒ [`OverlayOp::Close`] (else
///   `prev`'s content leaks into `next`'s analysis — the contamination
///   the one-RA-multiplex must never allow);
/// * every path in `next` whose content differs from `prev` (incl. paths
///   absent from `prev`) ⇒ [`OverlayOp::Apply`];
/// * a path present in both with **identical** content ⇒ **no op** (the
///   minimality that keeps RA's incremental re-analysis cheap);
/// * output order is deterministic: all `Close`s first (sorted), then all
///   `Apply`s (sorted). Closing-before-applying is intentional — it
///   guarantees no transient state where both `prev`'s and `next`'s
///   overlay for a moved/renamed path coexist.
pub fn diff(prev: &OverlaySet, next: &OverlaySet) -> Vec<OverlayOp> {
    let mut ops = Vec::new();

    // Closes first: anything in prev not in next reverts to base. BTreeMap
    // iteration is sorted ⇒ deterministic order, no explicit re-sort.
    for path in prev.files.keys() {
        if !next.files.contains_key(path) {
            ops.push(OverlayOp::Close { path: path.clone() });
        }
    }

    // Then applies: anything in next that is new or content-changed vs
    // prev. Identical (path, content) in both ⇒ skipped (minimality).
    for (path, content) in &next.files {
        match prev.files.get(path) {
            Some(prev_content) if prev_content == content => {} // unchanged
            _ => ops.push(OverlayOp::Apply {
                path: path.clone(),
                content: content.clone(),
            }),
        }
    }

    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(pairs: &[(&str, &str)]) -> OverlaySet {
        OverlaySet::from_pairs(pairs.iter().map(|(p, c)| (*p, *c)))
    }

    #[test]
    fn empty_to_empty_is_no_ops() {
        assert_eq!(diff(&OverlaySet::new(), &OverlaySet::new()), vec![]);
    }

    #[test]
    fn idempotent_same_set_is_no_ops() {
        // The minimality property: re-selecting the already-applied
        // worktree sends RA nothing (no needless re-analysis).
        let s = set(&[("a.rs", "fn a() {}"), ("b.rs", "fn b() {}")]);
        assert_eq!(diff(&s, &s), vec![], "diff(s, s) must be empty");
    }

    #[test]
    fn base_to_worktree_applies_all() {
        let next = set(&[("z.rs", "Z"), ("a.rs", "A")]);
        // Sorted apply order (BTreeMap): a.rs then z.rs.
        assert_eq!(
            diff(&OverlaySet::new(), &next),
            vec![
                OverlayOp::Apply {
                    path: "a.rs".into(),
                    content: "A".into()
                },
                OverlayOp::Apply {
                    path: "z.rs".into(),
                    content: "Z".into()
                },
            ]
        );
    }

    #[test]
    fn worktree_to_base_closes_all() {
        let prev = set(&[("z.rs", "Z"), ("a.rs", "A")]);
        assert_eq!(
            diff(&prev, &OverlaySet::new()),
            vec![
                OverlayOp::Close {
                    path: "a.rs".into()
                },
                OverlayOp::Close {
                    path: "z.rs".into()
                },
            ]
        );
    }

    #[test]
    fn content_change_applies_only_changed() {
        let prev = set(&[("a.rs", "old"), ("b.rs", "same")]);
        let next = set(&[("a.rs", "new"), ("b.rs", "same")]);
        // b.rs identical ⇒ skipped (minimality); only a.rs re-applied.
        assert_eq!(
            diff(&prev, &next),
            vec![OverlayOp::Apply {
                path: "a.rs".into(),
                content: "new".into()
            }]
        );
    }

    #[test]
    fn cross_worktree_switch_closes_stale_and_applies_new() {
        // THE load-bearing isolation case: worktree V applied, switching
        // to worktree W. V-only files MUST close (else V's content
        // contaminates W's verdict); shared-but-different re-apply;
        // identical shared file untouched; W-only files apply.
        let v = set(&[
            ("only_v.rs", "V"),     // must Close
            ("shared.rs", "v-ver"), // must re-Apply (differs)
            ("common.rs", "same"),  // must be untouched
        ]);
        let w = set(&[
            ("shared.rs", "w-ver"), // Apply (differs from v)
            ("common.rs", "same"),  // unchanged ⇒ no op
            ("only_w.rs", "W"),     // Apply (new)
        ]);
        let ops = diff(&v, &w);
        assert_eq!(
            ops,
            vec![
                // Closes first, sorted:
                OverlayOp::Close {
                    path: "only_v.rs".into()
                },
                // Then applies, sorted (only_w.rs < shared.rs):
                OverlayOp::Apply {
                    path: "only_w.rs".into(),
                    content: "W".into()
                },
                OverlayOp::Apply {
                    path: "shared.rs".into(),
                    content: "w-ver".into()
                },
            ],
            "stale V-only file closed; shared re-applied; identical untouched; W-only applied"
        );
        // Explicit isolation assertion: no surviving op references V's
        // unique content.
        assert!(
            !ops.iter().any(|op| matches!(
                op,
                OverlayOp::Apply { content, .. } if content == "V" || content == "v-ver"
            )),
            "no V-specific content may survive the switch to W"
        );
    }

    #[test]
    fn closes_strictly_precede_applies() {
        // Deterministic ordering contract: a path that exists in both but
        // changed yields an Apply; a removed path yields a Close; all
        // Closes must come before all Applies (no transient dual-overlay).
        let prev = set(&[("gone.rs", "x"), ("kept.rs", "old")]);
        let next = set(&[("kept.rs", "new"), ("added.rs", "y")]);
        let ops = diff(&prev, &next);
        let first_apply = ops
            .iter()
            .position(|o| matches!(o, OverlayOp::Apply { .. }));
        let last_close = ops
            .iter()
            .rposition(|o| matches!(o, OverlayOp::Close { .. }));
        if let (Some(fa), Some(lc)) = (first_apply, last_close) {
            assert!(lc < fa, "all Close ops must precede all Apply ops");
        }
        assert_eq!(
            ops,
            vec![
                OverlayOp::Close {
                    path: "gone.rs".into()
                },
                OverlayOp::Apply {
                    path: "added.rs".into(),
                    content: "y".into()
                },
                OverlayOp::Apply {
                    path: "kept.rs".into(),
                    content: "new".into()
                },
            ]
        );
    }

    #[test]
    fn overlayset_accessors() {
        let mut s = OverlaySet::new();
        assert!(s.is_empty());
        s.set("a.rs", "A");
        assert!(!s.is_empty());
        assert_eq!(s.len(), 1);
        assert_eq!(s.get(Path::new("a.rs")), Some("A"));
        assert_eq!(s.get(Path::new("missing.rs")), None);
    }

    // ─── CGLS-11 — iter_rs ──────────────────────────────────────────────

    #[test]
    fn iter_rs_returns_only_rs_files_in_sorted_order() {
        // Mixed overlay: .rs files interleaved with non-.rs files.
        // iter_rs must return only the .rs entries, in BTreeMap order.
        let s = set(&[
            ("zzz/last.rs", "fn last() {}"),
            ("Cargo.toml", "[workspace]"),
            ("aaa/first.rs", "fn first() {}"),
            ("Cargo.lock", "# lock"),
            ("mmm/middle.rs", "fn middle() {}"),
        ]);
        let results: Vec<_> = s.iter_rs().collect();
        assert_eq!(
            results,
            vec![
                (Path::new("aaa/first.rs"), "fn first() {}"),
                (Path::new("mmm/middle.rs"), "fn middle() {}"),
                (Path::new("zzz/last.rs"), "fn last() {}"),
            ],
            "iter_rs must skip non-.rs files and return .rs entries in sorted path order"
        );
    }

    #[test]
    fn iter_rs_empty_when_no_rs_files() {
        let s = set(&[
            ("Cargo.toml", "[workspace]"),
            ("Cargo.lock", "# lock"),
            (".cargo/config.toml", "[build]"),
        ]);
        assert!(
            s.iter_rs().next().is_none(),
            "iter_rs must yield nothing when the overlay has no .rs files"
        );
    }

    #[test]
    fn iter_rs_empty_on_empty_overlay() {
        assert!(
            OverlaySet::new().iter_rs().next().is_none(),
            "iter_rs on an empty overlay must be immediately exhausted"
        );
    }
}
