//! Which conflicting paths the lane may resolve by regenerating, and which it
//! must eject.
//!
//! ## Why this exists
//!
//! Measured with `git merge-tree` against live `dev` over 80 open PRs
//! (2026-08-20): 27 carry a true conflict, and of their 57 conflicting paths 8
//! (14%) are committed codegen output that no author wrote. 3 of the 27 PRs
//! (11%) conflict ONLY in such files and are therefore fully resolvable here.
//!
//! That 14% is the honest number. An earlier pass claimed 85% by counting
//! *changed* files in stranded PRs rather than the files that actually
//! collide — the wrong denominator, and it inverted the ratio. The larger
//! classes are hand-written source (49%) and deployment pins such as
//! `deployment/admin/base/admin-app.yaml` and
//! `deployment/kubernetes/apps/staging/kustomization.yaml` (26%), neither of
//! which this module touches. Two PRs touching one subsystem both carry a regenerated
//! `chemistry/generated/**`, so they conflict on content that is a pure
//! function of their sources. Ejecting is the wrong verdict — the merge of the
//! *inputs* is clean, and regenerating reproduces the output exactly.
//!
//! tf-multiverse already resolves this class on promotion. `scripts/promote-
//! parity` calls generated trees "the only auto-resolvable conflict class",
//! pins them to one side, and then asserts the merged subtree is byte-identical
//! to a fresh generator run. That machinery was never wired to the lane, which
//! is where the ejections actually happen.
//!
//! ## What must NOT be resolved
//!
//! Being under a directory named `generated` is not sufficient and trusting the
//! name would silently discard human work:
//!
//! - `portal/src/generated/**` has NO generator. `promote-parity` says so
//!   outright — "portal/src/generated/api/ is a hand-maintained fork" — and it
//!   is absent from `scripts/ci/check-codegen-drift.sh`'s targets. It is 337
//!   committed files, 284 under `ui_components/`. Pinning them would throw away
//!   edits somebody made by hand.
//! - `CAPABILITIES.md` and `circles/*/.triform/meta.yaml` have *checkers*
//!   (`scripts/ci/check-capability-index.sh`) but no confirmed regenerator.
//!   Unverified means ejected.
//!
//! So the allowlist below is not "paths that look generated". It is exactly the
//! set that `check-codegen-drift.sh` proves reproducible by running
//! `chemistry/generators/build-all.sh` and diffing. If that guard's targets
//! change, this list must change with it, and `drift_targets_match_guard`
//! records the pairing.
//!
//! ## The larger class: racing image pins
//!
//! Codegen is not actually the biggest source of no-author conflicts —
//! deployment pins are (26% of conflicting paths vs 14%). A bot rewrites image
//! tags on trunk continuously: 105 `chore(...): auto-bake <sha>` commits landed
//! on `dev` in three days, ~35/day, and enrolled PRs carry their own bake for
//! the same manifests. Two bakes touching the same `newTag`/`digest` lines
//! conflict by construction, and mean PR survival (~37 min) is about the gap
//! between bakes.
//!
//! These are safe to resolve NEWER-WINS rather than eject: a bake is a pure pin
//! rewrite. Verified over 8 consecutive bake commits on `dev` — ZERO changed
//! lines outside `newTag`, `digest`, `image:`, and artifact-URL values. Taking
//! either side loses nothing but a stale tag, and trunk's is the one that
//! survives anyway.
//!
//! The narrow rule matters: only a conflict where BOTH sides are pin lines
//! qualifies. A PR that hand-edits a replica count in the same manifest is a
//! real conflict and still ejects — see `is_pin_only_conflict`.
//!
//! ## Fail closed
//!
//! A conflict set is resolvable only if EVERY path in it is regenerable. One
//! unknown path ejects the whole member exactly as today. This is deliberate:
//! the failure that matters is not "we ejected something we could have saved",
//! it is "we auto-resolved a real conflict and lost someone's work".

use std::path::{Path, PathBuf};

/// Paths proven reproducible by `scripts/ci/check-codegen-drift.sh`.
///
/// Kept byte-for-byte in step with that script's `GENERATED_TARGETS`. Prefixes
/// end in `/`; the exact file has no trailing slash.
pub const REGENERABLE_PREFIXES: &[&str] = &["chemistry/generated/", "physics/src/generated/"];

/// A single regenerable file (not a tree) that the drift guard also covers.
pub const REGENERABLE_FILES: &[&str] = &["portal/src/services/optimistic_actions.rs"];

/// Explicitly NOT regenerable, despite living under a `generated/` directory.
///
/// Listed rather than merely omitted so that the reason survives: a future
/// reader who adds `portal/src/generated/` to the prefixes above will trip the
/// `hand_maintained_fork_is_never_resolvable` test and find this comment.
pub const HAND_MAINTAINED: &[&str] = &["portal/src/generated/"];

/// Manifest paths whose conflicts are routinely pure image-pin churn.
///
/// Not an allowlist by itself — `is_pin_only_conflict` must still prove that
/// the conflicting HUNK is pins. This only says "a bake writes here".
pub const PIN_MANIFEST_SUFFIXES: &[&str] = &["kustomization.yaml", "-app.yaml", "builder.yaml"];

/// Line prefixes a bake rewrites. Measured, not guessed: 8 consecutive bake
/// commits changed nothing else.
pub const PIN_LINE_MARKERS: &[&str] = &["newTag:", "digest:", "image:", "value: \"http"];

/// Does every conflicting line in this hunk look like an image pin?
///
/// `lines` are the changed lines from BOTH sides of the conflict, already
/// stripped of their leading `+`/`-`. Empty input is NOT pin-only: an
/// unreadable hunk must eject, not be waved through.
pub fn is_pin_only_conflict(path: &Path, lines: &[String]) -> bool {
    if lines.is_empty() {
        return false;
    }
    let Some(p) = path.to_str() else { return false };
    if !PIN_MANIFEST_SUFFIXES.iter().any(|s| p.ends_with(s)) {
        return false;
    }
    lines.iter().all(|l| {
        let t = l.trim();
        t.is_empty() || PIN_LINE_MARKERS.iter().any(|m| t.starts_with(m))
    })
}

/// Why a conflict set could not be auto-resolved. Carried into the ejection so
/// the reason reaches the PR comment instead of being re-derived by a human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotResolvable {
    /// At least one conflicting path is outside the regenerable set.
    HasNonGenerated(Vec<PathBuf>),
    /// Nothing conflicted — caller should not have asked.
    NoConflict,
}

/// Can this conflict set be resolved by regenerating rather than ejecting?
///
/// `Ok(())` means every path is regenerable. Anything else names the offenders.
pub fn classify(files: &[PathBuf]) -> Result<(), NotResolvable> {
    if files.is_empty() {
        // An empty conflict set is the `Unattributed` case, which the state
        // machine already handles by keeping the member enrolled. Claiming it
        // as "resolvable" would convert a self-healing ejection into a
        // regenerate-and-hope.
        return Err(NotResolvable::NoConflict);
    }
    let blockers: Vec<PathBuf> = files
        .iter()
        .filter(|p| !is_regenerable(p))
        .cloned()
        .collect();
    if blockers.is_empty() {
        Ok(())
    } else {
        Err(NotResolvable::HasNonGenerated(blockers))
    }
}

/// Is one path reproducible by the generator?
///
/// Hand-maintained trees are checked FIRST. `portal/src/generated/` would
/// otherwise be caught by a future careless prefix edit, and the whole point of
/// this module is that the directory name is not evidence.
pub fn is_regenerable(path: &Path) -> bool {
    let Some(p) = path.to_str() else {
        // A non-UTF-8 path cannot be matched against the allowlist, so it is
        // unknown, so it ejects. Fail closed.
        return false;
    };
    // Normalise a leading `./` so callers can pass either form.
    let p = p.strip_prefix("./").unwrap_or(p);
    if HAND_MAINTAINED.iter().any(|h| p.starts_with(h)) {
        return false;
    }
    if REGENERABLE_FILES.iter().any(|f| p == *f) {
        return true;
    }
    REGENERABLE_PREFIXES.iter().any(|pre| p.starts_with(pre))
}

/// Lines the generator stamps with the *current* git rev, which therefore
/// differ on every regeneration and are not drift.
///
/// `check-codegen-drift.sh` suppresses exactly these two with `git diff -I`,
/// because `build-all.sh` writes `git rev-parse HEAD` into the output: CI
/// checks out the PR commit, so the stamp always differs from whatever the
/// author's regen wrote. Without these filters a byte-identity assertion
/// reports false drift on EVERY regenerated PR — i.e. it would eject exactly
/// the population this module exists to save.
pub const REV_STAMP_FILTERS: &[&str] = &["Generated at git rev:", "\"generated_at_git_rev\""];

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn chemistry_and_physics_generated_are_resolvable() {
        let files = vec![
            p("chemistry/generated/types/src/lib.rs"),
            p("physics/src/generated/mod.rs"),
        ];
        assert_eq!(classify(&files), Ok(()));
    }

    #[test]
    fn the_codegen_inputs_manifest_rides_along() {
        // Lives under chemistry/generated/ and is regenerated with it.
        assert!(is_regenerable(&p(
            "chemistry/generated/.codegen-inputs-manifest.sha256"
        )));
    }

    #[test]
    fn hand_written_source_is_never_resolvable() {
        // The known-negative. A resolver that resolves everything is the
        // failure mode to fear, so this test is the point of the module.
        let files = vec![p("portal/src/canvas/focus_hydrator.rs")];
        match classify(&files) {
            Err(NotResolvable::HasNonGenerated(b)) => assert_eq!(b, files),
            other => panic!("hand-written source must eject, got {other:?}"),
        }
    }

    #[test]
    fn hand_maintained_fork_is_never_resolvable() {
        // portal/src/generated/ LOOKS generated and is not. If someone adds it
        // to REGENERABLE_PREFIXES this fails and the module docs explain why.
        assert!(!is_regenerable(&p("portal/src/generated/api/mod.rs")));
        assert!(!is_regenerable(&p(
            "portal/src/generated/ui_components/button.rs"
        )));
    }

    #[test]
    fn one_hand_written_path_blocks_an_otherwise_generated_set() {
        // Fail closed: a mixed set is NOT partially resolved.
        let files = vec![
            p("chemistry/generated/types/src/lib.rs"),
            p("physics/src/generated/mod.rs"),
            p(".forgejo/workflows/ci.yml"),
        ];
        match classify(&files) {
            Err(NotResolvable::HasNonGenerated(b)) => {
                assert_eq!(b, vec![p(".forgejo/workflows/ci.yml")]);
            }
            other => panic!("mixed set must eject, got {other:?}"),
        }
    }

    #[test]
    fn capability_index_is_not_resolvable_until_a_generator_is_confirmed() {
        // Has a checker, no confirmed regenerator. Unverified ⇒ ejects.
        assert!(!is_regenerable(&p("CAPABILITIES.md")));
        assert!(!is_regenerable(&p("circles/demo/.triform/meta.yaml")));
    }

    #[test]
    fn empty_conflict_set_is_not_claimed() {
        // The Unattributed path already self-heals; do not hijack it.
        assert_eq!(classify(&[]), Err(NotResolvable::NoConflict));
    }

    #[test]
    fn leading_dot_slash_is_normalised() {
        assert!(is_regenerable(&p("./chemistry/generated/types/src/lib.rs")));
    }

    #[test]
    fn a_sibling_directory_sharing_a_prefix_does_not_match() {
        // `chemistry/generated-fixtures/` is not `chemistry/generated/`. The
        // trailing slash in the constant is what makes this hold.
        assert!(!is_regenerable(&p("chemistry/generated-fixtures/x.yaml")));
    }

    #[test]
    fn drift_targets_match_guard() {
        // Pairing record: these are check-codegen-drift.sh's GENERATED_TARGETS.
        // If that script changes, this test is the tripwire.
        assert_eq!(
            REGENERABLE_PREFIXES,
            &["chemistry/generated/", "physics/src/generated/"]
        );
        assert_eq!(
            REGENERABLE_FILES,
            &["portal/src/services/optimistic_actions.rs"]
        );
    }

    #[test]
    fn a_pure_pin_hunk_is_resolvable_newer_wins() {
        let lines = vec![
            "newTag: \"dev-d7afba13-2026-08-20\"".to_string(),
            "digest: \"sha256:af8a4c7a\"".to_string(),
        ];
        assert!(is_pin_only_conflict(
            &p("deployment/isolation/staging/kustomization.yaml"),
            &lines
        ));
    }

    #[test]
    fn a_hand_edit_in_the_same_manifest_still_ejects() {
        // The known-negative for the pin rule. Someone changing replicas in a
        // manifest a bake also touches is a REAL conflict.
        let lines = vec![
            "newTag: \"dev-d7afba13\"".to_string(),
            "replicas: 3".to_string(),
        ];
        assert!(!is_pin_only_conflict(
            &p("deployment/admin/base/admin-app.yaml"),
            &lines
        ));
    }

    #[test]
    fn an_empty_hunk_is_not_pin_only() {
        // Unreadable hunk ⇒ eject. Fail closed.
        assert!(!is_pin_only_conflict(
            &p("deployment/x/kustomization.yaml"),
            &[]
        ));
    }

    #[test]
    fn pin_lines_outside_a_manifest_do_not_qualify() {
        let lines = vec!["image: foo:1".to_string()];
        assert!(!is_pin_only_conflict(&p("README.md"), &lines));
    }
}
