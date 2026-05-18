//! Per-worktree red-state diagnostic retention (Model R #11,
//! `D-FLEET-SHARED-DAEMON` §8.3 / §11).
//!
//! The operator-specified product principle (`D-FLEET-SHARED-DAEMON` §8):
//! *"All Green doesn't need one per, but if something is red we need to
//! know all about it so we can relay the information or fix correctly."*
//! GREEN is the boring case; RED is the entire reason the tool exists —
//! an agent (or the agent orchestrating other agents) needs the full
//! `file:line:col:severity:code:message` set to fix a red worktree or
//! route the fix elsewhere.
//!
//! So for any worktree **in red state** this module retains the full
//! diagnostic list at `<wt>/.cargoless/tree.cache/diagnostics`, queryable
//! on demand via [`get_diagnostics`]. The transport layer (#10) will bind
//! this to the logical `get_diagnostics(wt)` API; the on-disk file is also
//! directly readable (the v0 file-reading fallback in the §10.3
//! auto-discovery chain).
//!
//! ## Asymmetric honesty invariant (load-bearing)
//!
//! GREEN does not merely *skip* writing — it **atomically overwrites the
//! file with `[]`** (the empty sentinel) using the *same* temp-file +
//! rename primitive the RED path uses. A red diagnostics file left
//! lingering after the tree went green would tell a querying agent "still
//! broken" when it is not: a false-red, exactly the failure class
//! cargoless exists to eliminate (cf. FIELD FINDING #8 — verdict/
//! diagnostics disagreement is a launch-blocker-class defect; #176
//! hardening: the GREEN transition must not be able to leave a stale red
//! file, so it does NOT use `remove_file` whose failure modes — busy,
//! perm, transient — leave the prior *red* bytes in place; an atomic
//! rename-over deterministically replaces them, and `[]` is already this
//! module's defined "nothing actionable" value, so `get_diagnostics` ⇒
//! empty exactly as an absent file would). One write primitive, one
//! failure surface, shared with the well-exercised RED path. The retained
//! file therefore tracks the verdict edge precisely:
//!
//! | tree verdict | diagnostics present | action |
//! |---|---|---|
//! | GREEN | (any) | atomically overwrite with `[]` (terse: green retains nothing actionable; rename-over so a clear-failure cannot leave a stale red file — #176) |
//! | RED | non-empty | write the full list (verbose) |
//! | RED | empty (RA silent so far) | write `[]` — itself honest info: "red, specifics not yet available"; the authoritative `verdict=red` in `cli-status` still stands |
//!
//! GREEN and RED-empty therefore produce a byte-identical `[]` file; that
//! is intentional — both mean "no actionable diagnostics", and the
//! authoritative red/green distinction lives in `cli-status` `verdict=`
//! (the sidecar never carries the verdict — criterion-4 sidecar
//! discipline).
//!
//! ## Path / state-dir note
//!
//! `D-FLEET-SHARED-DAEMON` §8.3 writes `<wt>/.triform/cargoless/
//! tree.cache/diagnostics`. The `.triform/cargoless/` vs `.cargoless/`
//! *root* is the configurable state-dir (Model R #1, Stream A — not yet
//! landed). Until #1 unifies it, this uses `.cargoless/` to stay
//! consistent with the **currently shipped** `cli-status` root
//! (`<root>/.cargoless/cli-status`, see `cargoless::statusfile`) — a
//! split-brain (status under `.cargoless/`, diagnostics under
//! `.triform/cargoless/`) would be worse than either choice. The
//! `tree.cache/` leaf is honored per §5/§8.3. When #1 lands, the single
//! state-dir resolver replaces [`diagnostics_path`]'s `.cargoless/`
//! segment — the format + API here are unaffected.
//!
//! ## Format
//!
//! A JSON array (one object per diagnostic). cargoless-core already
//! depends on `serde_json` (Value + `json!`, no derive — the sanctioned
//! house tool; hand-rolled JSON for diagnostic text "is a latent-bug
//! factory" per the crate's own dep rationale). Best-effort throughout: a
//! retention or parse failure must never take the daemon down and must
//! never panic a query — the authoritative verdict lives in `cli-status`,
//! this is the parallel detail channel.

use std::io::Write;
use std::path::{Path, PathBuf};

use cargoless_proto::{Diagnostic, Severity, TreeState};

/// The retained-diagnostics file for a worktree:
/// `<wt_root>/.cargoless/tree.cache/diagnostics`. See the module-level
/// state-dir note for the `.cargoless/` vs `.triform/cargoless/` choice.
pub fn diagnostics_path(wt_root: &Path) -> PathBuf {
    wt_root
        .join(".cargoless")
        .join("tree.cache")
        .join("diagnostics")
}

fn severity_str(s: Severity) -> &'static str {
    s.as_str()
}

fn severity_from_str(s: &str) -> Severity {
    match s {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "info" => Severity::Info,
        "hint" => Severity::Hint,
        // Conservative: an unrecognised retained severity is surfaced as
        // an error rather than dropped — on red we want to know
        // everything; silently losing a diagnostic is the worse failure.
        _ => Severity::Error,
    }
}

/// Serialise diagnostics to the on-disk JSON array string. Pure (no I/O)
/// so it is unit-tested directly.
pub fn serialize(diags: &[Diagnostic]) -> String {
    let arr: Vec<serde_json::Value> = diags
        .iter()
        .map(|d| {
            serde_json::json!({
                "file": d.file_path.to_string_lossy(),
                "line": d.line,
                "col": d.col,
                "severity": severity_str(d.severity),
                "code": d.code,
                "message": d.message,
                "source": d.source,
            })
        })
        .collect();
    // `Value::Array(..).to_string()` is infallible; pretty is unnecessary
    // (machine-read channel) and would bloat the file.
    serde_json::Value::Array(arr).to_string()
}

/// Parse the on-disk JSON array back to diagnostics. Best-effort: a
/// missing/garbled file or a malformed element yields what could be
/// recovered (never panics, never errors) — a query for detail must
/// degrade to "less detail", never to a crash.
pub fn deserialize(text: &str) -> Vec<Diagnostic> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(text)
    else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|v| {
            let file = v.get("file")?.as_str()?;
            Some(Diagnostic {
                file_path: PathBuf::from(file),
                line: v
                    .get("line")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as u32,
                col: v
                    .get("col")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as u32,
                severity: severity_from_str(
                    v.get("severity")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("error"),
                ),
                code: v
                    .get("code")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                message: v
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                source: v
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Atomic write: temp file + rename in the same dir (atomic on the fs),
/// mirroring `cargoless::statusfile::write`. Best-effort — a retention
/// failure must never take the daemon down (the authoritative verdict is
/// in `cli-status`; this is the parallel detail channel).
fn atomic_write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    if let Ok(mut f) = std::fs::File::create(&tmp) {
        if f.write_all(body.as_bytes()).is_ok() {
            let _ = f.flush();
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// The GREEN action: retain "nothing actionable" by atomically writing
/// the empty sentinel `[]` — **not** `remove_file` (#176 hardening). A
/// `remove_file` failure (busy / perm / transient fs) would leave the
/// prior *red* bytes in place = a stale false-red, the FIELD-FINDING-#8
/// scar class. An atomic temp+rename-over of `[]` (the same primitive the
/// RED path uses, and `[]` is already this module's "nothing actionable"
/// value so `get_diagnostics` ⇒ empty exactly as an absent file would)
/// deterministically replaces any prior red content with one shared,
/// well-exercised write path. Best-effort and infallible to the caller —
/// the authoritative verdict is `cli-status` `verdict=`; this sidecar
/// never carries it.
pub fn clear(wt_root: &Path) {
    atomic_write(&diagnostics_path(wt_root), &serialize(&[]));
}

/// Persist (or clear) the retained diagnostics for `wt_root` to match the
/// `tree` verdict edge. See the module-level asymmetric honesty table.
///
/// * `TreeState::Green` ⇒ [`clear`] — atomically overwrite with `[]`
///   (#176: rename-over, NOT `remove_file`, so a clear-failure cannot
///   leave a stale red file). Byte-identical to the RED-empty result.
/// * `TreeState::Red`   ⇒ atomically write the full diagnostic list
///   (an empty `[]` when RA has not reported specifics yet — itself
///   honest: "red, details pending"; `cli-status` `verdict=red` stands).
///
/// Best-effort and infallible to the caller: a status/retention I/O
/// failure must never take the verdict pipeline down.
pub fn persist(wt_root: &Path, tree: TreeState, diags: &[Diagnostic]) {
    match tree {
        TreeState::Green => clear(wt_root),
        TreeState::Red => atomic_write(&diagnostics_path(wt_root), &serialize(diags)),
    }
}

/// Query the retained diagnostics for a worktree (the logical
/// `get_diagnostics(wt)` the transport layer (#10) will bind). Empty when
/// the worktree is green / never went red / the file is absent or
/// unreadable — callers treat "no retained detail" and "green" the same,
/// which is correct: a green tree has nothing to retain.
pub fn get_diagnostics(wt_root: &Path) -> Vec<Diagnostic> {
    match std::fs::read_to_string(diagnostics_path(wt_root)) {
        Ok(text) => deserialize(&text),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(file: &str, sev: Severity, msg: &str) -> Diagnostic {
        Diagnostic {
            file_path: PathBuf::from(file),
            line: 142,
            col: 18,
            severity: sev,
            code: Some("E0308".into()),
            message: msg.into(),
            source: Some("rustc".into()),
        }
    }

    #[test]
    fn serialize_roundtrips_including_tricky_message() {
        // A message with quotes + newline + backslash — exactly the class
        // the crate's dep rationale warns hand-rolled JSON mangles.
        let diags = vec![
            diag(
                "physics/src/orbit.rs",
                Severity::Error,
                "expected `f64`,\n found \"f32\" \\ x",
            ),
            diag("isolation/src/lib.rs", Severity::Warning, "unused import"),
        ];
        let back = deserialize(&serialize(&diags));
        assert_eq!(
            back, diags,
            "JSON roundtrip preserves every field + tricky text"
        );
    }

    #[test]
    fn empty_red_serialises_as_empty_array() {
        assert_eq!(serialize(&[]), "[]");
        assert_eq!(deserialize("[]"), Vec::<Diagnostic>::new());
    }

    #[test]
    fn deserialize_is_best_effort_never_panics() {
        assert_eq!(deserialize(""), Vec::<Diagnostic>::new());
        assert_eq!(deserialize("not json"), Vec::<Diagnostic>::new());
        assert_eq!(deserialize("{}"), Vec::<Diagnostic>::new());
        // An element missing the required `file` key is skipped, not fatal.
        assert_eq!(deserialize(r#"[{"line":1}]"#), Vec::<Diagnostic>::new());
        // Unknown severity surfaces as Error (never silently dropped).
        let one = deserialize(r#"[{"file":"a.rs","severity":"weird","message":"m"}]"#);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].severity, Severity::Error);
    }

    #[test]
    fn green_overwrites_red_with_empty_sentinel_no_stale_red() {
        let mut root = std::env::temp_dir();
        root.push(format!(
            "cargoless-ds-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // RED with diagnostics → retained + queryable.
        let diags = vec![diag(
            "physics/src/orbit.rs",
            Severity::Error,
            "mismatched types",
        )];
        persist(&root, TreeState::Red, &diags);
        assert_eq!(get_diagnostics(&root), diags, "red retains the full list");
        let path = diagnostics_path(&root);
        assert!(path.exists());
        let red_bytes = std::fs::read_to_string(&path).unwrap();
        assert!(
            red_bytes.contains("mismatched types"),
            "red file holds the real diagnostic bytes"
        );

        // GREEN → #176 hardening: the file is atomically OVERWRITTEN with
        // the empty sentinel `[]` (NOT removed). The load-bearing
        // anti-stale-red invariant: the prior red bytes are
        // deterministically gone — a `remove_file` that silently failed
        // would have left them, the FIELD-FINDING-#8 false-red class.
        persist(&root, TreeState::Green, &[]);
        assert!(
            path.exists(),
            "#176: green overwrites with [] (atomic rename-over), it does not remove"
        );
        let green_bytes = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            green_bytes, "[]",
            "green file is exactly the empty sentinel"
        );
        assert!(
            !green_bytes.contains("mismatched types"),
            "no stale red bytes survive the green transition (the #176 invariant)"
        );
        assert_eq!(
            get_diagnostics(&root),
            Vec::<Diagnostic>::new(),
            "query after green ⇒ empty, exactly as an absent file would read"
        );

        // Green-after-green idempotency: still exactly `[]`, still empty.
        persist(&root, TreeState::Green, &[]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
        assert_eq!(get_diagnostics(&root), Vec::<Diagnostic>::new());

        // RED again but RA silent so far → empty array retained (honest
        // "red, details pending"); byte-identical to the green file —
        // intentional: the red/green distinction lives in cli-status
        // verdict=, never in this sidecar (criterion-4 sidecar discipline).
        persist(&root, TreeState::Red, &[]);
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
        assert_eq!(get_diagnostics(&root), Vec::<Diagnostic>::new());

        let _ = std::fs::remove_dir_all(&root);
    }
}
