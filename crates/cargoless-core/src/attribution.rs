//! Differential diagnostic attribution: "which of these errors are NEW
//! relative to a base set?"
//!
//! A project-check / compiler witness verdict over an overlay-on-tip compile
//! is *absolute*: it reds whenever the compiled tree has any error, including
//! errors that were already on the base tip before the pusher's overlay was
//! applied. That conflates "the trunk is broken" with "this change broke
//! something" — fine for an advisory verdict, fatal for a merge gate, because
//! a pre-existing break would block every innocent pusher.
//!
//! This module is the shared identity + set-difference primitive that lets a
//! caller subtract a base error set from an overlay error set and keep only
//! the diagnostics the overlay *introduced*. It is the extraction of the
//! fingerprinting that previously lived (and was CLI-only) inside the
//! `cargoless` binary's `checks.rs` `--allow-existing-red` path, moved down to
//! `cargoless-core` so the CLI **and** the serve/witness/coalescer path call
//! ONE implementation — never two divergent fingerprint algorithms.
//!
//! ## Identity is line-INSENSITIVE by design
//!
//! The fingerprint is `source|code|relative_path|normalized_message` — it
//! deliberately omits line/column. A pusher who inserts three lines shifts the
//! line number of every error below their edit; a line-sensitive identity
//! would then see all those shifted base errors as "new" and wrongly blame the
//! pusher — reintroducing the exact false-attribution this module exists to
//! prevent. Count-matching (a fingerprint is inherited only if the base has at
//! least as many occurrences) bounds the one weakness of dropping line: two
//! identical-message errors in one file can't masquerade for three.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use cargoless_proto::{Diagnostic, Severity};

/// Multiset of error fingerprints → occurrence count. Built from the `Error`
/// severity diagnostics only (warnings never gate, so they never attribute).
pub type FingerprintCounts = BTreeMap<String, usize>;

/// Build the fingerprint multiset for a diagnostic slice, relative to `root`.
/// Only `Severity::Error` diagnostics contribute — warnings are not gating and
/// must not affect attribution.
#[must_use]
pub fn fingerprint_counts(root: &Path, diagnostics: &[Diagnostic]) -> FingerprintCounts {
    let mut out = BTreeMap::new();
    for diagnostic in diagnostics.iter().filter(|d| d.severity == Severity::Error) {
        *out.entry(fingerprint(root, diagnostic)).or_insert(0) += 1;
    }
    out
}

/// `true` when every current fingerprint also appears in `base` with at least
/// the same count — i.e. the current red set introduces nothing the base did
/// not already have. Empty `current` returns `false` (there is nothing to
/// classify as inherited; callers treat an empty red set as green upstream).
#[must_use]
pub fn are_inherited(current: &FingerprintCounts, base: &FingerprintCounts) -> bool {
    !current.is_empty()
        && current
            .iter()
            .all(|(fp, count)| base.get(fp).copied().unwrap_or(0) >= *count)
}

/// The differential: return the subset of `overlay` error diagnostics whose
/// fingerprint is NOT covered by `base` (i.e. the errors the overlay
/// introduced). `base_counts` is the base error multiset (compute once per
/// base tip with [`fingerprint_counts`] and reuse across overlays).
///
/// Count-aware: if the overlay has two errors with fingerprint F and the base
/// had one, exactly one of the overlay's two is returned as new. Non-error
/// severities are never returned (they are not gating).
#[must_use]
pub fn new_diagnostics(
    overlay_root: &Path,
    overlay: &[Diagnostic],
    base_counts: &FingerprintCounts,
) -> Vec<Diagnostic> {
    let mut remaining = base_counts.clone();
    let mut new = Vec::new();
    for diagnostic in overlay.iter().filter(|d| d.severity == Severity::Error) {
        let fp = fingerprint(overlay_root, diagnostic);
        match remaining.get_mut(&fp) {
            // The base covered this occurrence — consume one credit, inherited.
            Some(credit) if *credit > 0 => *credit -= 1,
            // No remaining base credit for this fingerprint — newly introduced.
            _ => new.push(diagnostic.clone()),
        }
    }
    new
}

/// `source|code|relative_path|normalized_message`. The single identity used
/// everywhere a diagnostic must be matched across two compiles of the same
/// project at (possibly) different line offsets.
#[must_use]
pub fn fingerprint(root: &Path, diagnostic: &Diagnostic) -> String {
    let source = diagnostic.source.as_deref().unwrap_or("unknown");
    let code = diagnostic.code.as_deref().unwrap_or("unknown");
    let rel = rel_path(root, diagnostic);
    let message = normalize_message(root, &diagnostic.message);
    format!("{source}|{code}|{rel}|{message}")
}

fn rel_path(root: &Path, diagnostic: &Diagnostic) -> String {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let file_path =
        fs::canonicalize(&diagnostic.file_path).unwrap_or_else(|_| diagnostic.file_path.clone());
    file_path
        .strip_prefix(&root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| file_path.to_string_lossy().replace('\\', "/"))
}

fn normalize_message(root: &Path, message: &str) -> String {
    let mut out = message.replace(&root.to_string_lossy().to_string(), "$ROOT");
    if let Ok(canon) = fs::canonicalize(root) {
        out = out.replace(&canon.to_string_lossy().to_string(), "$ROOT");
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn err(path: &str, line: u32, code: &str, message: &str) -> Diagnostic {
        Diagnostic {
            file_path: PathBuf::from(path),
            line,
            col: 1,
            severity: Severity::Error,
            code: Some(code.to_string()),
            message: message.to_string(),
            source: Some("rustc".to_string()),
        }
    }

    fn warn(path: &str, line: u32, code: &str, message: &str) -> Diagnostic {
        Diagnostic {
            severity: Severity::Warning,
            ..err(path, line, code, message)
        }
    }

    // root is a path that won't canonicalize to anything containing the test
    // file paths, so rel_path falls back to the raw (relative) path — stable
    // across the test regardless of CWD.
    fn root() -> PathBuf {
        PathBuf::from("/nonexistent-attribution-test-root")
    }

    #[test]
    fn identical_tree_is_fully_inherited_zero_new() {
        let r = root();
        let base = [err("src/a.rs", 10, "E0277", "trait bound not satisfied")];
        let overlay = [err("src/a.rs", 10, "E0277", "trait bound not satisfied")];
        let base_counts = fingerprint_counts(&r, &base);
        assert!(are_inherited(
            &fingerprint_counts(&r, &overlay),
            &base_counts
        ));
        assert!(new_diagnostics(&r, &overlay, &base_counts).is_empty());
    }

    #[test]
    fn added_error_is_new() {
        let r = root();
        let base = [err("src/a.rs", 10, "E0277", "trait bound not satisfied")];
        let overlay = [
            err("src/a.rs", 10, "E0277", "trait bound not satisfied"),
            err("src/b.rs", 5, "E0308", "mismatched types"),
        ];
        let base_counts = fingerprint_counts(&r, &base);
        let new = new_diagnostics(&r, &overlay, &base_counts);
        assert_eq!(new.len(), 1, "exactly the b.rs error is new");
        assert_eq!(new[0].code.as_deref(), Some("E0308"));
        assert!(!are_inherited(
            &fingerprint_counts(&r, &overlay),
            &base_counts
        ));
    }

    #[test]
    fn line_shift_only_is_inherited_zero_new() {
        // Same error, different line (pusher inserted lines above it). The
        // line-insensitive identity must treat it as inherited, NOT new.
        let r = root();
        let base = [err("src/a.rs", 10, "E0277", "trait bound not satisfied")];
        let overlay = [err("src/a.rs", 42, "E0277", "trait bound not satisfied")];
        let base_counts = fingerprint_counts(&r, &base);
        assert!(
            are_inherited(&fingerprint_counts(&r, &overlay), &base_counts),
            "a pure line shift must remain inherited"
        );
        assert!(
            new_diagnostics(&r, &overlay, &base_counts).is_empty(),
            "a pure line shift introduces no new error"
        );
    }

    #[test]
    fn fix_one_add_one_nets_one_new() {
        // Base had error X; overlay fixed X but introduced Y. Count-matching
        // must surface Y as new even though the total count is unchanged.
        let r = root();
        let base = [err("src/a.rs", 10, "E0277", "trait bound not satisfied")];
        let overlay = [err("src/b.rs", 5, "E0308", "mismatched types")];
        let base_counts = fingerprint_counts(&r, &base);
        let new = new_diagnostics(&r, &overlay, &base_counts);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].code.as_deref(), Some("E0308"));
    }

    #[test]
    fn duplicate_message_count_matching() {
        // Base had ONE occurrence of fingerprint F; overlay has TWO. Exactly
        // one is new (count-aware), not zero and not two.
        let r = root();
        let base = [err("src/a.rs", 1, "E0277", "dup")];
        let overlay = [
            err("src/a.rs", 1, "E0277", "dup"),
            err("src/a.rs", 9, "E0277", "dup"),
        ];
        let base_counts = fingerprint_counts(&r, &base);
        assert_eq!(new_diagnostics(&r, &overlay, &base_counts).len(), 1);
        assert!(
            !are_inherited(&fingerprint_counts(&r, &overlay), &base_counts),
            "2 vs 1 occurrence is not fully inherited"
        );
    }

    #[test]
    fn warnings_never_attribute() {
        let r = root();
        let base: [Diagnostic; 0] = [];
        let overlay = [warn("src/a.rs", 3, "unused_imports", "unused import")];
        let base_counts = fingerprint_counts(&r, &base);
        assert!(
            new_diagnostics(&r, &overlay, &base_counts).is_empty(),
            "a warning is never a new gating error"
        );
    }
}
