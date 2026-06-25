//! KNOWN-BLIND corpus file (#DONUT) — DO NOT add to the module tree.
//!
//! Class: **cross-crate type resolution in a GENERATED TWIN**. A build
//! step emits a "frozen" snapshot of a component into
//! `generated/ui-frozen/*.rs`, referencing a domain type that lives in a
//! SIBLING crate — but the generator forgot to emit the matching `use`
//! import. `cargo build` fails (E0412 cannot find type / E0425 cannot find
//! value, the tf-mv `cannot find type DonutSlice` incident) while the
//! daemon's RA-native verdict stays GREEN.
//!
//! Why RA-native is blind here, and why this rides `CARGOLESS_BLIND_PATHS`
//! (content-exempt) NOT `CARGOLESS_MACRO_BLIND_PATHS`:
//!   * There is NO `view!` (or any proc-macro) for a content scan to key
//!     on — the macro-narrowed net (#A8) would clear this file to green
//!     because `macro_names=["view"]` finds no `view!` call. The blindness
//!     is in cross-crate name resolution, unrelated to macro expansion, so
//!     the path must be classified blind REGARDLESS of content. That is
//!     exactly what the content-exempt glob set is for.
//!   * `DonutSlice` is a sibling-crate type the generated twin references
//!     without `use chemistry_domain::DonutSlice;`. RA-native, analyzing
//!     this file in isolation, does not surface the unresolved name as an
//!     authoritative error in its settle window; a full rustc/SSR compile
//!     does. The fix is the missing `use` the generator should have emitted.
//!
//! A self-contained reproduction would need a real sibling crate to import
//! FROM — deliberately omitted so the fixture stays a single-crate, single
//! external-dependency (leptos) workspace with a pinned, hand-unmaintainable
//! Cargo.lock. This file is documentation-as-code of the #DONUT blind class
//! and the content source the in-tree detector test (`serveapi.rs`) reads to
//! assert the content-exempt path classifies it blind. It is NOT compiled
//! (no `mod` reaches it); the cross-crate `use` is intentionally absent so
//! the class it documents is faithful.

// NOTE: `use chemistry_domain::DonutSlice;` is INTENTIONALLY MISSING — that
// omission IS the bug this corpus file reproduces.

/// Frozen twin of a donut-chart component, as a code generator would emit
/// it. `DonutSlice` resolves in the real (non-frozen) module via a glob
/// import that the generator did not reproduce here → E0412 at `cargo build`.
pub struct DonutChartFrozen {
    pub slices: Vec<DonutSlice>,
    pub total: f64,
}

impl DonutChartFrozen {
    pub fn new(slices: Vec<DonutSlice>) -> Self {
        let total = slices.iter().map(|s| s.value).sum();
        Self { slices, total }
    }

    /// References an associated constant on the unimported type as well —
    /// the E0425 (cannot find value) sibling of the E0412 above.
    pub fn is_empty(&self) -> bool {
        self.total <= DonutSlice::EPSILON
    }
}
