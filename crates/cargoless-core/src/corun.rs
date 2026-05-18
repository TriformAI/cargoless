//! #8 corun batching — the optimistic-combined + solo-fallback verdict
//! attribution state machine (pure core).
//!
//! ## What this module is (and is deliberately not)
//!
//! Model R serves N worktrees off ONE rust-analyzer. The common agent-fleet
//! case is "N agents each editing an independent feature in a different part
//! of the workspace" — disjoint overlay file-sets. For that case, design
//! `D-FLEET-SHARED-DAEMON.md §7` specifies **corun batching**:
//!
//! 1. **Optimistic combined check** — apply the *union* of N worktrees'
//!    overlays as one overlay-set, run the checker once, cache the result at
//!    [`CacheLayout::combined_entry`] (keyed by the sorted+deduped overlay-hash
//!    *set* — `combined_key`, frozen in [`crate::cache_layout`]).
//! 2. **Combined GREEN** → emit green for *all* N worktrees in the batch
//!    (one check, N verdicts — the N×-throughput win).
//! 3. **Combined RED** → fall back to per-worktree *solo* checks so the red
//!    can be **attributed** to the responsible worktree(s).
//!
//! This module owns *only* that batching/attribution algorithm + its cache
//! routing + the `--no-corun` policy. It does **not** implement the checker
//! (that is #5, LSP overlay multiplexing through RA) nor the on-disk verdict
//! store (that rides `cargoless-cas`'s content-addressed write-once layer +
//! builder-infra's #2 atomicity fix — same compose-don't-reinvent discipline
//! as #7). Both are expressed as **traits** ([`OverlaySetChecker`],
//! [`VerdictCache`]) so the algorithm is fully unit-testable now, ahead of #5,
//! with **zero #5-contract speculation** — the only thing assumed of #5 is the
//! inherent, inevitable "given an overlay-set, produce green/red".
//!
//! ## The honest caveat, encoded in the type system (design §7.3)
//!
//! **Combined-green does not strictly guarantee solo-green under
//! cross-dependencies.** Worked example: worktree A adds `pub fn new_thing()`
//! to `foo.rs`; worktree B calls `foo::new_thing()` in `bar.rs`. A-solo ✅,
//! B-solo ❌ (references a fn absent from base), A+B-combined ✅ (the union has
//! both). For agent-fleet workloads cross-deps are rare, so optimistic-combined
//! is a real N×-win with solo-fallback-on-red as the safety net and post-merge
//! CI catching the residual cross-dep case. The trade-off is named, accepted,
//! and operator-escapable via [`CorunPolicy::NoCorun`] (`--no-corun`).
//!
//! Crucially, the optimism is **not** hidden in a comment: a combined-batch
//! green is returned as [`Provenance::CombinedGreen`], a *distinct variant*
//! from [`Provenance::SoloGreen`]. A consumer that wants a solo-verified
//! guarantee must pattern-match for it — the type system makes silently
//! treating an optimistic batch-green as solo-verified impossible. And a
//! combined-green **never forges solo cache entries** (it isn't a solo-green
//! proof, so it must not poison the solo cache with an unverified green).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::cache_layout::{CacheLayout, OverlayHash};

/// Identifies one worktree within a corun batch. Opaque; the repo-scoped
/// daemon (#3) supplies it. Distinct from `OverlayHash`: a worktree *has* an
/// overlay-hash, but two worktrees with byte-identical overlays still have
/// distinct identities (and distinct diagnostic destinations).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorktreeId(String);

impl WorktreeId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The verdict for an overlay-set. Minimal by design: per-diagnostic detail
/// (file:line:crate, schema=2) is #9/#11's seam, not #8's. #8 only needs the
/// green/red bit to drive batching + attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorunVerdict {
    Green,
    Red,
}

/// *How* a verdict was reached — the launch-critical honesty encoding.
///
/// `CombinedGreen` is **optimistic**: it means "the union of this batch's
/// overlays checked green", which under cross-deps is *not* a proof that each
/// worktree is green in isolation (design §7.3). It is a deliberately separate
/// variant from `SoloGreen` so no consumer can conflate the two without
/// explicitly pattern-matching — the type system carries the caveat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Optimistic: the combined batch checked green. NOT a per-worktree
    /// solo-green guarantee under cross-deps. Post-merge CI is the safety net.
    CombinedGreen,
    /// This worktree's overlay checked green *in isolation*. A real guarantee.
    SoloGreen,
    /// This worktree's overlay checked red in isolation — the attributed
    /// failure.
    SoloRed,
}

/// Per-worktree result of a corun batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    pub worktree: WorktreeId,
    pub verdict: CorunVerdict,
    pub provenance: Provenance,
}

/// Operator policy. Default is `Corun` (the N×-win); `NoCorun` (`--no-corun`)
/// forces always-solo always-correct verdicts at N× the checker cost — the
/// escape hatch for workspaces with high cross-dep prevalence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CorunPolicy {
    #[default]
    Corun,
    NoCorun,
}

/// The #5 seam: "given an overlay-set, produce a verdict". #8 mocks this in
/// tests and makes **no** assumption about #5 beyond this inherent shape.
pub trait OverlaySetChecker {
    /// Check the union of `overlays` as one overlay-set applied to the pinned
    /// base. An empty slice means "base with no overlay".
    fn check(&self, overlays: &[OverlayHash]) -> CorunVerdict;
}

/// The #7-path-keyed verdict store. Keys are [`CacheLayout::solo_entry`] /
/// [`CacheLayout::combined_entry`] paths (content-addressed, write-once). The
/// on-disk impl is deferred to ride `cargoless-cas` + builder-infra's #2
/// atomicity fix; the algorithm depends only on this trait so it is gateable
/// now. [`MemVerdictCache`] is the in-memory test impl.
pub trait VerdictCache {
    fn get(&self, key: &Path) -> Option<CorunVerdict>;
    fn put(&self, key: &Path, verdict: CorunVerdict);
}

/// In-memory [`VerdictCache`] for unit tests and (later) a warm in-process
/// tier. Thread-safe so it composes with a multi-worktree daemon.
#[derive(Debug, Default)]
pub struct MemVerdictCache {
    map: Mutex<BTreeMap<PathBuf, CorunVerdict>>,
}

impl MemVerdictCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct cache entries — lets tests assert that a
    /// combined-green did NOT forge solo entries (the honest-caveat guard).
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.lock().expect("cache mutex").len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether a specific key is present (test introspection).
    #[must_use]
    pub fn contains(&self, key: &Path) -> bool {
        self.map.lock().expect("cache mutex").contains_key(key)
    }
}

impl VerdictCache for MemVerdictCache {
    fn get(&self, key: &Path) -> Option<CorunVerdict> {
        self.map.lock().expect("cache mutex").get(key).copied()
    }

    fn put(&self, key: &Path, verdict: CorunVerdict) {
        self.map
            .lock()
            .expect("cache mutex")
            .insert(key.to_path_buf(), verdict);
    }
}

/// Check one worktree's overlay *in isolation*, caching at its solo entry.
/// Content-addressed: a cache hit skips the checker entirely.
fn solo_attribution(
    checker: &dyn OverlaySetChecker,
    cache: &dyn VerdictCache,
    layout: &CacheLayout,
    wt: &WorktreeId,
    hw: &OverlayHash,
) -> Attribution {
    let key = layout.solo_entry(hw);
    let verdict = match cache.get(&key) {
        Some(v) => v,
        None => {
            let v = checker.check(std::slice::from_ref(hw));
            cache.put(&key, v);
            v
        }
    };
    Attribution {
        worktree: wt.clone(),
        verdict,
        provenance: match verdict {
            CorunVerdict::Green => Provenance::SoloGreen,
            CorunVerdict::Red => Provenance::SoloRed,
        },
    }
}

/// Run a corun batch and return one [`Attribution`] per input worktree, in
/// input order.
///
/// Algorithm (design §7.1):
/// * `NoCorun`, or a batch of ≤1 worktree → straight solo path (always-correct
///   verdicts; nothing to batch).
/// * `Corun` with ≥2 worktrees → optimistic combined check keyed by the
///   sorted+deduped overlay-set ([`CacheLayout::combined_entry`]):
///   * **combined green** → every worktree gets `Green` /
///     [`Provenance::CombinedGreen`]. Solo cache entries are **not** written
///     (a combined-green is not a solo-green proof — it must not poison the
///     solo cache).
///   * **combined red** → solo-fallback: each worktree is checked alone so the
///     red is attributed to the worktree(s) actually responsible.
///
/// Duplicate worktree ids in `batch` are the caller's contract to avoid; the
/// combined *key* is set-keyed regardless (so `{A,B}` ≡ `{B,A}`), but
/// attribution is emitted positionally per input entry.
pub fn corun(
    checker: &dyn OverlaySetChecker,
    cache: &dyn VerdictCache,
    layout: &CacheLayout,
    batch: &[(WorktreeId, OverlayHash)],
    policy: CorunPolicy,
) -> Vec<Attribution> {
    if batch.is_empty() {
        return Vec::new();
    }

    if policy == CorunPolicy::NoCorun || batch.len() == 1 {
        return batch
            .iter()
            .map(|(wt, hw)| solo_attribution(checker, cache, layout, wt, hw))
            .collect();
    }

    // Optimistic combined check over the union of overlays.
    let overlays: Vec<OverlayHash> = batch.iter().map(|(_, hw)| hw.clone()).collect();
    let ckey = layout.combined_entry(&overlays);
    let combined = match cache.get(&ckey) {
        Some(v) => v,
        None => {
            let v = checker.check(&overlays);
            cache.put(&ckey, v);
            v
        }
    };

    match combined {
        // One check, N green verdicts — but flagged optimistic so no consumer
        // mistakes it for a solo guarantee. Deliberately does NOT write solo
        // cache entries.
        CorunVerdict::Green => batch
            .iter()
            .map(|(wt, _)| Attribution {
                worktree: wt.clone(),
                verdict: CorunVerdict::Green,
                provenance: Provenance::CombinedGreen,
            })
            .collect(),
        // Something in the batch is red — fall back to solo so we can say
        // *which* worktree(s).
        CorunVerdict::Red => batch
            .iter()
            .map(|(wt, hw)| solo_attribution(checker, cache, layout, wt, hw))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A scriptable checker: returns Red iff the overlay-set contains any
    /// `OverlayHash` whose string is in `red_overlays`; records every call's
    /// overlay-set so tests can assert how many checks ran (the N×-win is a
    /// *call-count* property, not just a verdict property).
    struct MockChecker {
        red_overlays: Vec<String>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl MockChecker {
        fn new(red: &[&str]) -> Self {
            Self {
                red_overlays: red.iter().map(|s| s.to_string()).collect(),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    impl OverlaySetChecker for MockChecker {
        fn check(&self, overlays: &[OverlayHash]) -> CorunVerdict {
            self.calls
                .borrow_mut()
                .push(overlays.iter().map(|h| h.as_str().to_string()).collect());
            if overlays
                .iter()
                .any(|h| self.red_overlays.iter().any(|r| r == h.as_str()))
            {
                CorunVerdict::Red
            } else {
                CorunVerdict::Green
            }
        }
    }

    fn layout() -> CacheLayout {
        let mut p = std::env::temp_dir();
        p.push(format!("cargoless-corun-{}", std::process::id()));
        CacheLayout::for_repo(p, crate::cache_layout::TF_STATE_DIR_REL)
    }

    fn wt(id: &str, hw: &str) -> (WorktreeId, OverlayHash) {
        (WorktreeId::new(id), OverlayHash::new(hw))
    }

    #[test]
    fn corun_empty_batch_is_empty() {
        let c = MockChecker::new(&[]);
        let cache = MemVerdictCache::new();
        assert!(corun(&c, &cache, &layout(), &[], CorunPolicy::Corun).is_empty());
        assert_eq!(c.call_count(), 0);
    }

    #[test]
    fn corun_single_wt_is_solo_not_combined() {
        let c = MockChecker::new(&[]);
        let cache = MemVerdictCache::new();
        let l = layout();
        let out = corun(&c, &cache, &l, &[wt("A", "hwA")], CorunPolicy::Corun);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].provenance, Provenance::SoloGreen);
        // Cached under the SOLO key, not a combined key.
        assert!(cache.contains(&l.solo_entry(&OverlayHash::new("hwA"))));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn corun_combined_green_attributes_green_to_all_with_combined_provenance() {
        let c = MockChecker::new(&[]); // nothing red ⇒ combined green
        let cache = MemVerdictCache::new();
        let l = layout();
        let batch = [wt("A", "hwA"), wt("B", "hwB"), wt("C", "hwC")];
        let out = corun(&c, &cache, &l, &batch, CorunPolicy::Corun);

        assert_eq!(out.len(), 3);
        for a in &out {
            assert_eq!(a.verdict, CorunVerdict::Green);
            assert_eq!(
                a.provenance,
                Provenance::CombinedGreen,
                "batch-green must be flagged optimistic, never SoloGreen"
            );
        }
        // The N×-win: ONE checker call for the whole batch.
        assert_eq!(
            c.call_count(),
            1,
            "combined green = exactly one check for N WTs"
        );
    }

    #[test]
    fn corun_combined_green_does_not_forge_solo_cache_entries() {
        // The honest-caveat guard: a combined-green is NOT a solo-green proof,
        // so it must not write per-WT solo cache entries (that would poison
        // the solo cache with an unverified green).
        let c = MockChecker::new(&[]);
        let cache = MemVerdictCache::new();
        let l = layout();
        let batch = [wt("A", "hwA"), wt("B", "hwB")];
        corun(&c, &cache, &l, &batch, CorunPolicy::Corun);

        assert!(!cache.contains(&l.solo_entry(&OverlayHash::new("hwA"))));
        assert!(!cache.contains(&l.solo_entry(&OverlayHash::new("hwB"))));
        // Exactly one entry: the combined key.
        assert_eq!(cache.len(), 1);
        assert!(
            cache.contains(&l.combined_entry(&[OverlayHash::new("hwA"), OverlayHash::new("hwB")]))
        );
    }

    #[test]
    fn corun_combined_red_falls_back_to_solo_attribution() {
        // hwB is red ⇒ combined red ⇒ solo-fallback attributes precisely.
        let c = MockChecker::new(&["hwB"]);
        let cache = MemVerdictCache::new();
        let l = layout();
        let batch = [wt("A", "hwA"), wt("B", "hwB"), wt("C", "hwC")];
        let out = corun(&c, &cache, &l, &batch, CorunPolicy::Corun);

        let by = |id: &str| out.iter().find(|a| a.worktree.as_str() == id).unwrap();
        assert_eq!(by("A").verdict, CorunVerdict::Green);
        assert_eq!(by("A").provenance, Provenance::SoloGreen);
        assert_eq!(by("B").verdict, CorunVerdict::Red);
        assert_eq!(by("B").provenance, Provenance::SoloRed);
        assert_eq!(by("C").verdict, CorunVerdict::Green);
        assert_eq!(by("C").provenance, Provenance::SoloGreen);
        // 1 combined (red) + 3 solo checks.
        assert_eq!(c.call_count(), 4);
    }

    #[test]
    fn corun_no_corun_policy_forces_solo_path() {
        let c = MockChecker::new(&[]);
        let cache = MemVerdictCache::new();
        let l = layout();
        let batch = [wt("A", "hwA"), wt("B", "hwB")];
        let out = corun(&c, &cache, &l, &batch, CorunPolicy::NoCorun);

        assert!(out.iter().all(|a| a.provenance == Provenance::SoloGreen));
        // No combined key — strictly per-WT solo (always-correct, N× cost).
        assert!(
            !cache.contains(&l.combined_entry(&[OverlayHash::new("hwA"), OverlayHash::new("hwB")]))
        );
        assert_eq!(c.call_count(), 2, "--no-corun = one check per worktree");
    }

    #[test]
    fn corun_combined_cache_hit_skips_recheck() {
        let c = MockChecker::new(&[]);
        let cache = MemVerdictCache::new();
        let l = layout();
        let batch = [wt("A", "hwA"), wt("B", "hwB")];
        corun(&c, &cache, &l, &batch, CorunPolicy::Corun);
        assert_eq!(c.call_count(), 1);
        // Identical batch again ⇒ combined cache hit ⇒ no new check.
        corun(&c, &cache, &l, &batch, CorunPolicy::Corun);
        assert_eq!(
            c.call_count(),
            1,
            "content-addressed combined hit, no recheck"
        );
    }

    #[test]
    fn corun_solo_cache_hit_skips_recheck() {
        let c = MockChecker::new(&[]);
        let cache = MemVerdictCache::new();
        let l = layout();
        corun(&c, &cache, &l, &[wt("A", "hwA")], CorunPolicy::Corun);
        corun(&c, &cache, &l, &[wt("A", "hwA")], CorunPolicy::Corun);
        assert_eq!(c.call_count(), 1, "solo content-addressed hit, no recheck");
    }

    #[test]
    fn corun_provenance_distinguishes_combined_from_solo_green() {
        // Type-level honesty: the SAME worktree green via combined vs solo
        // carries DIFFERENT provenance — a consumer can always tell an
        // optimistic batch-green from a verified solo-green.
        let c = MockChecker::new(&[]);
        let cache1 = MemVerdictCache::new();
        let l = layout();
        let combined = corun(
            &c,
            &cache1,
            &l,
            &[wt("A", "hwA"), wt("B", "hwB")],
            CorunPolicy::Corun,
        );
        let cache2 = MemVerdictCache::new();
        let solo = corun(&c, &cache2, &l, &[wt("A", "hwA")], CorunPolicy::NoCorun);

        assert_eq!(combined[0].verdict, solo[0].verdict); // both Green
        assert_ne!(
            combined[0].provenance, solo[0].provenance,
            "combined-green and solo-green must be type-distinguishable"
        );
    }

    #[test]
    fn corun_cross_dep_combined_green_can_mask_solo_red_is_visible_in_provenance() {
        // The documented §7.3 hazard, as an executable contract: model a
        // cross-dep where the COMBINED set is green but B alone is red. The
        // algorithm (correctly, by design) returns combined-green here — but
        // it is tagged CombinedGreen, NOT SoloGreen, so the optimism is
        // *visible* to any consumer and post-merge CI remains the safety net.
        // checker: red iff EXACTLY {hwB} alone (B references A's symbol);
        // green for the union {hwA,hwB} (A provides it).
        struct CrossDep;
        impl OverlaySetChecker for CrossDep {
            fn check(&self, ov: &[OverlayHash]) -> CorunVerdict {
                let set: Vec<&str> = ov.iter().map(|h| h.as_str()).collect();
                if set == ["hwB"] {
                    CorunVerdict::Red
                } else {
                    CorunVerdict::Green
                }
            }
        }
        let cache = MemVerdictCache::new();
        let l = layout();
        let out = corun(
            &CrossDep,
            &cache,
            &l,
            &[wt("A", "hwA"), wt("B", "hwB")],
            CorunPolicy::Corun,
        );
        // Combined {hwA,hwB} is green ⇒ optimistic green for both...
        assert!(out.iter().all(|a| a.verdict == CorunVerdict::Green));
        // ...but typed CombinedGreen so the masked solo-red of B is never
        // misread as a solo guarantee (this IS the honest caveat, enforced).
        assert!(
            out.iter()
                .all(|a| a.provenance == Provenance::CombinedGreen)
        );
    }
}
