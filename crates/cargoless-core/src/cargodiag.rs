//! `cargo --message-format=json` → [`Diagnostic`].
//!
//! ## Why this exists
//!
//! Until this module, the only structured diagnostics a `kind: command` check
//! could return were cargoless's own `cargoless.check-diagnostic/v1` line
//! protocol (see [`crate::project_checks`]). Cargo does not speak that
//! protocol — it speaks its own JSON — so every project wanting real compiler
//! diagnostics out of a command check had to write and maintain a shell
//! wrapper translating one into the other.
//!
//! That wrapper is the single biggest barrier to "point cargoless at any Cargo
//! workspace and it works". Worse, it fails *silently* in the direction that
//! matters: a check that pipes raw cargo JSON matches no `v1` line, so it
//! yields **zero** diagnostics, and the non-zero exit then produces one
//! synthetic diagnostic pinned at `cargoless.checks.yaml:1:1`. The build is
//! correctly red, but every error's file, line and code have been thrown away —
//! and those are exactly the fields attribution needs to decide *whose* change
//! broke the build.
//!
//! ## Scope
//!
//! Pure parsing. No I/O, no process spawning, no policy. Feed it the stdout of
//! a `cargo … --message-format=json` run and it returns the diagnostics.
//!
//! ## What is deliberately dropped
//!
//! * Non-`compiler-message` records (`compiler-artifact`, `build-script-executed`,
//!   `build-finished`) carry no diagnostic and are skipped.
//! * `level` values other than `error`/`warning` (`note`, `help`, `failure-note`)
//!   are skipped as *top-level* records: rustc emits them as children of a
//!   parent error, and surfacing them standalone would triple the red count
//!   while adding nothing a human can act on. They still reach the reader
//!   through `rendered`, which we keep verbatim as the message when present.
//! * `message.children` are not walked for the same reason.
//!
//! ## Position
//!
//! rustc reports 1-based `line_start`/`column_start`, which is already the
//! convention [`Diagnostic`] documents, so there is no off-by-one conversion
//! here (unlike the LSP path in [`crate::lsp`], which converts from 0-based).
//!
//! A message may carry several spans. We anchor on the one rustc marked
//! `is_primary` — that is the span rustc's own renderer points its `-->` at.
//! Falling back to "first span" would anchor a `E0308` on the *expected* type's
//! definition site rather than the offending expression, which sends attribution
//! at a file the author may never have touched.

use std::path::{Path, PathBuf};

use cargoless_proto::{Diagnostic, Severity};

/// `source` tag stamped on every diagnostic this module produces.
///
/// `"rustc"` exactly — not `"cargo"` — because [`crate::attribution`] builds
/// its fingerprint from `source|code|path|message`, and the daemon's existing
/// authoritative-vs-advisory split keys on this same string (`"rustc"` =
/// authoritative, `"rust-analyzer"` = advisory). Emitting anything else here
/// would silently drop these into the advisory tier.
pub const CARGO_DIAGNOSTIC_SOURCE: &str = "rustc";

/// Parse the stdout of `cargo … --message-format=json`.
///
/// Cargo emits one JSON object per line, but real command output is rarely
/// pure: `cargo` writes progress to stderr, wrapper scripts echo banners, and a
/// caller may have merged the two streams. Every line that is not a JSON object
/// we recognise is skipped rather than treated as an error — a parse failure
/// must never be able to turn a red build green by aborting the scan early.
///
/// `root` anchors relative paths (cargo reports paths relative to the workspace
/// root). Absolute paths are kept as-is.
#[must_use]
pub fn parse_cargo_json(root: &Path, text: &str) -> Vec<Diagnostic> {
    text.lines()
        .filter_map(|line| parse_cargo_json_line(root, line))
        .collect()
}

/// Parse one line. `None` for anything that is not an actionable
/// `compiler-message`.
#[must_use]
pub fn parse_cargo_json_line(root: &Path, line: &str) -> Option<Diagnostic> {
    let trimmed = line.trim();
    // Cheap pre-filter: the overwhelming majority of lines in a real build are
    // not JSON at all, and `serde_json::from_str` on each is measurably slower
    // than one byte check.
    if !trimmed.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;

    // `reason` is absent on some wrapper-produced streams that emit bare
    // `message` objects; accept those too rather than forcing a wrapper.
    if let Some(reason) = value.get("reason").and_then(serde_json::Value::as_str) {
        if reason != "compiler-message" {
            return None;
        }
    }
    let message = value.get("message").unwrap_or(&value);

    let severity = match message.get("level").and_then(serde_json::Value::as_str) {
        Some("error") => Severity::Error,
        Some("warning") => Severity::Warning,
        // note/help/failure-note are children of a parent error — see module docs.
        _ => return None,
    };

    // `rendered` is rustc's own multi-line display, complete with the source
    // excerpt and caret. It is strictly more useful to a human than the bare
    // `message`, and the CLI renderer already handles multi-line messages.
    let text = message
        .get("rendered")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| message.get("message").and_then(serde_json::Value::as_str))?
        .to_string();

    // `code` is an object (`{"code":"E0308","explanation":"…"}`) for rustc
    // errors and for lints (`{"code":"unused_imports",…}`); it is null for
    // things like "aborting due to previous error".
    let code = message
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    let (file_path, line_no, col) = match primary_span(message) {
        Some(span) => span,
        // A message with no spans is still real — "aborting due to N previous
        // errors", a linker failure, an unresolved crate. Anchoring it at the
        // manifest keeps it visible instead of dropping it, and attribution
        // treats a file nobody touched as unattributable, which is the honest
        // outcome for a build-level failure.
        None => (root.join("Cargo.toml"), 1, 1),
    };
    let file_path = if file_path.is_absolute() {
        file_path
    } else {
        root.join(file_path)
    };

    Some(Diagnostic {
        file_path,
        line: line_no,
        col,
        severity,
        code,
        message: text,
        source: Some(CARGO_DIAGNOSTIC_SOURCE.to_string()),
    })
}

/// The span rustc's renderer points at: the one flagged `is_primary`, else the
/// first span present. See the module docs for why "first" alone is wrong.
fn primary_span(message: &serde_json::Value) -> Option<(PathBuf, u32, u32)> {
    let spans = message.get("spans")?.as_array()?;
    let chosen = spans
        .iter()
        .find(|s| {
            s.get("is_primary")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .or_else(|| spans.first())?;
    let file = chosen
        .get("file_name")
        .and_then(serde_json::Value::as_str)?;
    Some((
        PathBuf::from(file),
        // rustc is already 1-based here; `Diagnostic` documents 1-based. No
        // conversion — the LSP path converts, this one must not.
        chosen
            .get("line_start")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1) as u32,
        chosen
            .get("column_start")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1) as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/w")
    }

    #[test]
    fn parses_a_primary_span_error() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":{"code":"E0308"},"rendered":"error[E0308]: mismatched types\n --> src/lib.rs:7:9","spans":[{"file_name":"src/lib.rs","line_start":7,"column_start":9,"is_primary":true}]}}"#;
        let d = parse_cargo_json_line(&root(), line).expect("an error is a diagnostic");
        assert_eq!(d.file_path, PathBuf::from("/w/src/lib.rs"));
        assert_eq!((d.line, d.col), (7, 9));
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.code.as_deref(), Some("E0308"));
        assert_eq!(d.source.as_deref(), Some("rustc"));
        assert!(
            d.message.contains("mismatched types"),
            "rendered text is preserved: {}",
            d.message
        );
    }

    #[test]
    fn anchors_on_the_primary_span_not_the_first() {
        // rustc lists the "expected" definition site first and the offending
        // expression second. Anchoring on the first span would blame a file the
        // author may never have touched — the exact failure this ordering guards.
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":{"code":"E0308"},"spans":[{"file_name":"src/other.rs","line_start":3,"column_start":1,"is_primary":false},{"file_name":"src/culprit.rs","line_start":42,"column_start":5,"is_primary":true}]}}"#;
        let d = parse_cargo_json_line(&root(), line).expect("diagnostic");
        assert_eq!(
            d.file_path,
            PathBuf::from("/w/src/culprit.rs"),
            "must anchor on is_primary, not on spans[0]"
        );
        assert_eq!(d.line, 42);
    }

    #[test]
    fn leptos_view_macro_error_keeps_its_file() {
        // The macro-expansion case the witness exists for: the span points at
        // the invocation site inside view!, which IS a file the author edited.
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"expected `String`, found `i32`","code":{"code":"E0308"},"rendered":"error[E0308]","spans":[{"file_name":"portal/src/page.rs","line_start":118,"column_start":21,"is_primary":true}]}}"#;
        let d = parse_cargo_json_line(&root(), line).expect("diagnostic");
        assert_eq!(d.file_path, PathBuf::from("/w/portal/src/page.rs"));
        assert_eq!(d.line, 118);
    }

    #[test]
    fn message_without_spans_anchors_at_the_manifest() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"aborting due to 2 previous errors","spans":[]}}"#;
        let d = parse_cargo_json_line(&root(), line).expect("still a diagnostic");
        assert_eq!(d.file_path, PathBuf::from("/w/Cargo.toml"));
        assert_eq!((d.line, d.col), (1, 1));
        assert!(d.code.is_none());
    }

    #[test]
    fn absolute_span_paths_are_not_rejoined() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"boom","spans":[{"file_name":"/abs/src/x.rs","line_start":2,"column_start":1,"is_primary":true}]}}"#;
        let d = parse_cargo_json_line(&root(), line).expect("diagnostic");
        assert_eq!(d.file_path, PathBuf::from("/abs/src/x.rs"));
    }

    #[test]
    fn warnings_are_kept_notes_and_help_are_not() {
        let warn = r#"{"reason":"compiler-message","message":{"level":"warning","message":"unused import","code":{"code":"unused_imports"},"spans":[{"file_name":"src/a.rs","line_start":1,"column_start":5,"is_primary":true}]}}"#;
        let d = parse_cargo_json_line(&root(), warn).expect("warning is actionable");
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(d.code.as_deref(), Some("unused_imports"));

        for level in ["note", "help", "failure-note"] {
            let l = format!(
                r#"{{"reason":"compiler-message","message":{{"level":"{level}","message":"x","spans":[]}}}}"#
            );
            assert!(
                parse_cargo_json_line(&root(), &l).is_none(),
                "{level} is a child of a parent error; surfacing it standalone \
                 inflates the red count without adding anything actionable"
            );
        }
    }

    #[test]
    fn non_diagnostic_records_are_skipped() {
        for line in [
            r#"{"reason":"compiler-artifact","target":{"name":"x"}}"#,
            r#"{"reason":"build-script-executed","package_id":"x"}"#,
            r#"{"reason":"build-finished","success":false}"#,
        ] {
            assert!(parse_cargo_json_line(&root(), line).is_none());
        }
    }

    #[test]
    fn interleaved_noise_never_aborts_the_scan() {
        // THE fail-safe property: a parse failure must not be able to swallow
        // the diagnostics that follow it, or a red build could read as green.
        let text = concat!(
            "   Compiling triform-portal v0.1.0\n",
            "warning: some plain-text banner\n",
            "{ this is not valid json at all\n",
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"x"}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"error","message":"boom","code":{"code":"E0001"},"spans":[{"file_name":"src/late.rs","line_start":9,"column_start":1,"is_primary":true}]}}"#,
            "\n",
            "error: could not compile `triform-portal`\n",
        );
        let ds = parse_cargo_json(&root(), text);
        assert_eq!(ds.len(), 1, "exactly the one real diagnostic survives");
        assert_eq!(ds[0].file_path, PathBuf::from("/w/src/late.rs"));
        assert_eq!(ds[0].line, 9);
    }

    #[test]
    fn bare_message_object_without_reason_is_accepted() {
        // Some wrappers pipe `message` objects directly. Accepting them means
        // one less reason for a project to need a translation script.
        let line = r#"{"level":"error","message":"bare","code":{"code":"E0433"},"spans":[{"file_name":"src/b.rs","line_start":4,"column_start":2,"is_primary":true}]}"#;
        let d = parse_cargo_json_line(&root(), line).expect("diagnostic");
        assert_eq!(d.code.as_deref(), Some("E0433"));
        assert_eq!(d.line, 4);
    }

    #[test]
    fn falls_back_to_message_when_rendered_is_absent_or_empty() {
        for body in [
            r#""message":"plain text","spans":[]"#,
            r#""message":"plain text","rendered":"","spans":[]"#,
        ] {
            let line =
                format!(r#"{{"reason":"compiler-message","message":{{"level":"error",{body}}}}}"#);
            let d = parse_cargo_json_line(&root(), &line).expect("diagnostic");
            assert_eq!(d.message, "plain text");
        }
    }
}
