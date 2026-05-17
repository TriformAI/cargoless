//! `check` — one-shot verdict; exit code reflects green/red; FIELD FINDING
//! #2: prints every diagnostic on red so the user can actually fix it.
//! FIELD FINDING #8 (regression of #2): filter the print path to the SAME
//! authoritative tier the verdict reads from, so a green verdict cannot
//! appear alongside RA-native "errors" that didn't drive it.
//!
//! Bound to daemon-core's adjacent rich-verdict seam (additive alongside the
//! frozen `tf_core::model::check_once`):
//! `tf_core::model::check_once_with_diagnostics(&Path) -> io::Result<CheckResult>`.
//! `CheckResult` pairs the existing boolean `TreeState` (the AC#4 publisher
//! gate) with a `Vec<Diagnostic>` (the human-facing detail the boolean was
//! hiding). The exit-code contract is byte-frozen — only the *body* of the
//! red path gains diagnostic output.
//!
//! ## FIELD FINDING #8 — verdict/print provenance reconciliation
//!
//! The verdict path reads the AUTHORITATIVE tier (cargo-check, `source =
//! "rustc"`) per the #21 verdict-provenance seam: GREEN means *flycheck has
//! completed with no rustc-source error*. The #42 print path naively
//! displayed EVERY diagnostic — including RA-native syntax errors (`source =
//! "rust-analyzer"`) that the verdict explicitly considers advisory. When
//! the two tiers diverge (RA-native catches a syntax error instantly; cargo
//! check doesn't see it until parse time, or skips that file entirely), the
//! user saw `error[syntax-error; rust-analyzer]: …` followed by `green —
//! every tracked file compiles`. Two distinct sources of truth printed
//! side by side is *worse* than #42's original silence — it is actively
//! contradictory.
//!
//! The fix here is the minimum-surface one: `check` only prints the
//! diagnostics that share the verdict's authority. `watch` continues to
//! show every diagnostic in #45's timestamp format — a live-debug user
//! actively benefits from seeing the RA-native fast-hint stream, and the
//! `[+N.NNNs]` timeline tells them which signal arrived when. The split
//! matches #21's design intent: authoritative tier IS the verdict; the
//! advisory tier exists as a fast hint that NEVER asserts the verdict.
//!
//! Exit-code contract (stable for scripts/CI), per daemon-core's mapping —
//! treat ANY `Err` uniformly (do NOT switch on `ErrorKind`):
//! * `0` — green (every tracked file compiles)
//! * `1` — red (tree does not compile; an *unproven* tree is conservatively
//!   `Ok(TreeState::Red)` per AC#4 — same arm, not special-cased)
//! * `2` — could not run the verdict: rust-analyzer missing / spawn / pipe /
//!   bad root (`Err`). A *setup/env* failure, deliberately distinct from red.

use std::path::Path;
use std::process::ExitCode;

use crate::config::Config;
use crate::ui;

pub fn run(cfg: &Config) -> ExitCode {
    ui::step(format!(
        "checking {} ({})",
        cfg.root.display(),
        cfg.detection.describe()
    ));

    match tf_core::model::check_once_with_diagnostics(&cfg.root) {
        Ok(tf_core::CheckResult {
            tree: tf_core::TreeState::Green,
            diagnostics,
        }) => {
            // FIELD FINDING #8: partition by provenance BEFORE rendering.
            // The verdict reads the authoritative (cargo-check / `source =
            // "rustc"`) tier per #21; the print must match. Advisory
            // diagnostics (RA-native) are silently suppressed in `check`
            // (visible in `watch`) — counting them in the verdict line so
            // the user knows we saw them but did not let them drive the
            // verdict.
            let (authoritative, advisory) = partition_by_provenance(&diagnostics);
            if !authoritative.is_empty() {
                // A green tree can still carry rustc warnings; show those so
                // the user knows the tool saw them. Exit code stays 0.
                print_diagnostics(&cfg.root, &authoritative);
            }
            let advisory_note = advisory_suppression_note(advisory.len());
            ui::ok(format!(
                "green — every tracked file compiles{advisory_note}"
            ));
            ExitCode::SUCCESS
        }
        Ok(tf_core::CheckResult {
            tree: tf_core::TreeState::Red,
            diagnostics,
        }) => {
            // FIELD FINDING #2 stands: a red verdict MUST carry its
            // evidence. FIELD FINDING #8 narrows that to the authoritative
            // tier — the rustc errors that drove the verdict, not RA-native
            // advisory ones that didn't.
            let (authoritative, advisory) = partition_by_provenance(&diagnostics);
            if authoritative.is_empty() {
                // No authoritative diagnostics ⇒ unproven-red path (#21
                // never-claim-unproven-green). Be explicit instead of
                // silently swallowing it; mention the advisory count if any
                // (those didn't drive the verdict but are visible in watch).
                let advisory_hint = if advisory.is_empty() {
                    String::new()
                } else {
                    format!(
                        " ({} advisory hint{} from rust-analyzer; \
                         `tftrunk watch` for the live stream)",
                        advisory.len(),
                        if advisory.len() == 1 { "" } else { "s" },
                    )
                };
                ui::error(format!(
                    "red — at least one tracked file does not compile, \
                     but no diagnostics were captured before the check \
                     settled (try `tftrunk watch` for live updates, or \
                     re-run with `TF_CHECK_TIMEOUT_SECS=300`).{advisory_hint}"
                ));
            } else {
                print_diagnostics(&cfg.root, &authoritative);
                let (errs, warns) = severity_tally(&authoritative);
                let advisory_note = advisory_suppression_note(advisory.len());
                ui::error(format!(
                    "red — at least one tracked file does not compile \
                     ({errs} error{}, {warns} warning{} surfaced{advisory_note}).",
                    if errs == 1 { "" } else { "s" },
                    if warns == 1 { "" } else { "s" },
                ));
            }
            ExitCode::from(1)
        }
        Err(e) => {
            ui::error(format!(
                "could not check (rust-analyzer/setup): {e}\n  \
                 if rust-analyzer is missing: `rustup component add rust-analyzer`."
            ));
            ExitCode::from(2)
        }
    }
}

/// FIELD FINDING #8: the print path is gated on the SAME provenance the
/// verdict reads. `Diagnostic.source == Some("rustc")` is the authoritative
/// tier per #21 (cargo-check / flycheck output); everything else (most
/// notably `rust-analyzer` for native syntax/lint diagnostics) is advisory
/// and must not appear in `check` output — `watch` is where the live
/// advisory stream belongs.
///
/// A `None` source is conservatively treated as ADVISORY: if RA forgot to
/// tag a diagnostic, the safer answer is "don't let it contradict the
/// verdict line" — false-suppress < false-contradict.
fn is_authoritative(d: &tf_core::Diagnostic) -> bool {
    d.source.as_deref() == Some("rustc")
}

/// Partition a diagnostic slice into (authoritative, advisory) in-publish
/// order. Cheap clone — diagnostics are small and this runs once per check.
fn partition_by_provenance(
    diags: &[tf_core::Diagnostic],
) -> (Vec<tf_core::Diagnostic>, Vec<tf_core::Diagnostic>) {
    let mut auth = Vec::new();
    let mut adv = Vec::new();
    for d in diags {
        if is_authoritative(d) {
            auth.push(d.clone());
        } else {
            adv.push(d.clone());
        }
    }
    (auth, adv)
}

/// One-line "we saw N advisory hints but they did not drive the verdict"
/// note appended to the verdict line. Empty when there are no advisory
/// hints — don't pollute the happy-path with parentheticals.
fn advisory_suppression_note(n: usize) -> String {
    match n {
        0 => String::new(),
        1 => " (1 rust-analyzer advisory hint suppressed; \
              `tftrunk watch` shows the live stream)"
            .to_string(),
        _ => format!(
            " ({n} rust-analyzer advisory hints suppressed; \
             `tftrunk watch` shows the live stream)"
        ),
    }
}

/// Render every diagnostic, one per line, in a `rustc`-flavoured form:
///
/// `error[E0277; rustc]: src/lib.rs:42:5: the trait bound …`
///
/// Path is rendered relative to `root` when possible (falling back to the
/// absolute path); severity uses the lowercase tag the user expects; the
/// LSP `source` is shown in `[brackets]` after the code so authoritative
/// (`rustc`) and advisory (`rust-analyzer`) provenance is visible at a
/// glance. Stderr (matching the rest of `ui::*`) so a piped consumer
/// captures the verdict separately from the noise budget.
fn print_diagnostics(root: &Path, diags: &[tf_core::Diagnostic]) {
    use std::io::Write as _;
    let mut err = std::io::stderr();
    let _ = render_diagnostics(&mut err, root, diags);
    let _ = err.flush();
}

/// Pure renderer for [`print_diagnostics`] — splits out for unit testing
/// against an in-memory buffer (the IO half wraps this with stderr +
/// flush). Same format the user sees on the wire: `error[code; source]:
/// path:line:col: message`, one diagnostic per line.
pub(crate) fn render_diagnostics<W: std::io::Write>(
    w: &mut W,
    root: &Path,
    diags: &[tf_core::Diagnostic],
) -> std::io::Result<()> {
    for d in diags {
        let path = d
            .file_path
            .strip_prefix(root)
            .unwrap_or(&d.file_path)
            .display();
        let code = match (&d.code, &d.source) {
            (Some(c), Some(s)) => format!("[{c}; {s}]"),
            (Some(c), None) => format!("[{c}]"),
            (None, Some(s)) => format!("[{s}]"),
            (None, None) => String::new(),
        };
        writeln!(
            w,
            "{}{}: {}:{}:{}: {}",
            d.severity,
            code,
            path,
            d.line,
            d.col,
            // First line of the message — RA sometimes wraps with `\n`.
            d.message.lines().next().unwrap_or(&d.message),
        )?;
        // Continuation lines (multi-line messages) are indented for grep.
        for cont in d.message.lines().skip(1) {
            writeln!(w, "    {cont}")?;
        }
    }
    Ok(())
}

fn severity_tally(diags: &[tf_core::Diagnostic]) -> (usize, usize) {
    let mut errs = 0usize;
    let mut warns = 0usize;
    for d in diags {
        match d.severity {
            tf_core::Severity::Error => errs += 1,
            tf_core::Severity::Warning => warns += 1,
            _ => {}
        }
    }
    (errs, warns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn d(
        path: &str,
        line: u32,
        col: u32,
        sev: tf_core::Severity,
        code: Option<&str>,
        msg: &str,
        source: Option<&str>,
    ) -> tf_core::Diagnostic {
        tf_core::Diagnostic {
            file_path: PathBuf::from(path),
            line,
            col,
            severity: sev,
            code: code.map(str::to_owned),
            message: msg.to_owned(),
            source: source.map(str::to_owned),
        }
    }

    #[test]
    fn render_includes_file_line_col_code_severity_message() {
        // FIELD FINDING #2 — the README promise: a red verdict must tell
        // the user WHERE (file:line:col), WHAT (severity[code]), and WHY
        // (message). Every assertion below is a substring the dogfood
        // reproducer specifically looked for and found ABSENT.
        let root = PathBuf::from("/repo");
        let ds = vec![
            d(
                "/repo/src/lib.rs",
                42,
                5,
                tf_core::Severity::Error,
                Some("E0277"),
                "the trait bound `T: Foo` is not satisfied",
                Some("rustc"),
            ),
            d(
                "/repo/src/main.rs",
                7,
                1,
                tf_core::Severity::Warning,
                Some("unused_imports"),
                "unused import: `std::io`",
                Some("rust-analyzer"),
            ),
        ];
        let mut buf = Vec::new();
        render_diagnostics(&mut buf, &root, &ds).unwrap();
        let out = String::from_utf8(buf).unwrap();
        // First diagnostic: severity, code, source, relative path, line, col, message.
        assert!(out.contains("error"), "severity tag missing: {out}");
        assert!(out.contains("E0277"), "code missing: {out}");
        assert!(out.contains("rustc"), "source provenance missing: {out}");
        assert!(
            out.contains("src/lib.rs:42:5"),
            "file:line:col missing: {out}"
        );
        assert!(out.contains("trait bound"), "message missing: {out}");
        // Second diagnostic, fully rendered too.
        assert!(out.contains("warning"));
        assert!(out.contains("unused_imports"));
        assert!(out.contains("src/main.rs:7:1"));
        assert!(out.contains("rust-analyzer"));
    }

    #[test]
    fn render_falls_back_to_absolute_when_path_outside_root() {
        let root = PathBuf::from("/repo");
        let ds = vec![d(
            "/tmp/elsewhere.rs",
            1,
            2,
            tf_core::Severity::Error,
            None,
            "oops",
            None,
        )];
        let mut buf = Vec::new();
        render_diagnostics(&mut buf, &root, &ds).unwrap();
        let out = String::from_utf8(buf).unwrap();
        // Strip_prefix fails ⇒ the absolute path is rendered as-is.
        assert!(out.contains("/tmp/elsewhere.rs:1:2"));
        // No code, no source ⇒ no `[brackets]` segment after severity.
        assert!(out.contains("error: "));
    }

    #[test]
    fn render_indents_multiline_message_continuation() {
        let root = PathBuf::from("/r");
        let ds = vec![d(
            "/r/a.rs",
            1,
            1,
            tf_core::Severity::Error,
            Some("E0599"),
            "no method named `frob`\nhelp: did you mean `from`?",
            Some("rustc"),
        )];
        let mut buf = Vec::new();
        render_diagnostics(&mut buf, &root, &ds).unwrap();
        let out = String::from_utf8(buf).unwrap();
        // First line carries the header + first message line.
        assert!(out.contains("a.rs:1:1: no method named `frob`"));
        // Continuation line is indented (so a `grep` over file paths still
        // works — the indent says "this belongs to the prior diagnostic").
        assert!(out.contains("    help: did you mean `from`?"));
    }

    #[test]
    fn render_empty_list_is_empty_string() {
        let mut buf = Vec::new();
        render_diagnostics(&mut buf, &PathBuf::from("/r"), &[]).unwrap();
        assert!(buf.is_empty());
    }

    // -----------------------------------------------------------------------
    // FIELD FINDING #8 — verdict/print provenance reconciliation
    // -----------------------------------------------------------------------

    #[test]
    fn partition_splits_rustc_from_everything_else() {
        let ds = vec![
            d(
                "/r/a.rs",
                1,
                1,
                tf_core::Severity::Error,
                Some("E0277"),
                "auth",
                Some("rustc"),
            ),
            d(
                "/r/a.rs",
                2,
                1,
                tf_core::Severity::Error,
                Some("syntax-error"),
                "advisory error",
                Some("rust-analyzer"),
            ),
            d(
                "/r/a.rs",
                3,
                1,
                tf_core::Severity::Warning,
                Some("unused"),
                "advisory warn",
                Some("rust-analyzer"),
            ),
            // No source tagged ⇒ conservatively advisory (false-suppress is
            // safer than false-contradict — the README promise is at stake).
            d("/r/a.rs", 4, 1, tf_core::Severity::Info, None, "unt", None),
        ];
        let (auth, adv) = partition_by_provenance(&ds);
        assert_eq!(auth.len(), 1, "only the source=='rustc' entry");
        assert_eq!(auth[0].code.as_deref(), Some("E0277"));
        assert_eq!(adv.len(), 3, "rust-analyzer + no-source = advisory");
    }

    #[test]
    fn is_authoritative_is_strict_rustc_only() {
        let rustc = d(
            "/r/a.rs",
            1,
            1,
            tf_core::Severity::Error,
            None,
            "x",
            Some("rustc"),
        );
        let ra = d(
            "/r/a.rs",
            1,
            1,
            tf_core::Severity::Error,
            None,
            "x",
            Some("rust-analyzer"),
        );
        let other = d(
            "/r/a.rs",
            1,
            1,
            tf_core::Severity::Error,
            None,
            "x",
            Some("clippy"),
        );
        let untagged = d("/r/a.rs", 1, 1, tf_core::Severity::Error, None, "x", None);
        assert!(is_authoritative(&rustc));
        assert!(!is_authoritative(&ra));
        assert!(!is_authoritative(&other), "clippy/other ≠ verdict source");
        assert!(
            !is_authoritative(&untagged),
            "untagged is conservatively advisory"
        );
    }

    #[test]
    fn f8_regression_green_tree_does_not_render_advisory_errors() {
        // The exact dogfood reproducer for F8: an RA-native syntax-error
        // alongside a green flycheck verdict. The CHECK output must NOT
        // include the RA-native error in the printed diagnostics — it
        // didn't drive the verdict and printing it next to "green" is the
        // user-trust violation #55 fixed.
        let root = PathBuf::from("/repo");
        let ds = vec![d(
            "/repo/src/components/footer.rs",
            25,
            1,
            tf_core::Severity::Error,
            Some("syntax-error"),
            "Syntax Error: expected an item",
            Some("rust-analyzer"),
        )];
        let (auth, adv) = partition_by_provenance(&ds);
        assert!(
            auth.is_empty(),
            "no rustc-source ⇒ nothing to print on green"
        );
        assert_eq!(adv.len(), 1, "advisory hint preserved for the verdict note");

        // Belt-and-braces: render the authoritative slice and confirm the
        // contradictory string never makes it to output.
        let mut buf = Vec::new();
        render_diagnostics(&mut buf, &root, &auth).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.is_empty(), "no auth ⇒ no print: {out:?}");
        assert!(
            !out.contains("Syntax Error"),
            "advisory error must not leak: {out:?}"
        );
    }

    #[test]
    fn f8_red_path_filters_advisory_out_of_tally() {
        // A mixed publish: 1 rustc error (drives RED), 1 RA-native syntax
        // error (advisory). The verdict line's "(N errors, M warnings
        // surfaced)" tally must count only the authoritative rustc error,
        // and the printed diagnostics must include only that one.
        let root = PathBuf::from("/repo");
        let ds = vec![
            d(
                "/repo/src/lib.rs",
                10,
                5,
                tf_core::Severity::Error,
                Some("E0277"),
                "trait bound",
                Some("rustc"),
            ),
            d(
                "/repo/src/lib.rs",
                20,
                1,
                tf_core::Severity::Error,
                Some("syntax-error"),
                "advisory syntax",
                Some("rust-analyzer"),
            ),
        ];
        let (auth, _adv) = partition_by_provenance(&ds);
        let (errs, _warns) = severity_tally(&auth);
        assert_eq!(
            errs, 1,
            "only the rustc error counts toward the verdict tally"
        );
        let mut buf = Vec::new();
        render_diagnostics(&mut buf, &root, &auth).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("E0277"), "rustc error rendered");
        assert!(out.contains("trait bound"), "rustc message rendered");
        assert!(
            !out.contains("advisory syntax"),
            "advisory message suppressed"
        );
        assert!(!out.contains("syntax-error"), "advisory code suppressed");
    }

    #[test]
    fn advisory_suppression_note_grammar() {
        assert!(advisory_suppression_note(0).is_empty());
        assert!(advisory_suppression_note(1).contains("1 rust-analyzer advisory hint suppressed"));
        assert!(advisory_suppression_note(1).contains("`tftrunk watch`"));
        assert!(advisory_suppression_note(5).contains("5 rust-analyzer advisory hints suppressed"));
    }

    #[test]
    fn warnings_from_rustc_are_kept_advisory_warnings_are_not() {
        // rustc warnings (cargo check's lint output) are authoritative;
        // they belong in `check` output. rust-analyzer's
        // `unused_imports`-class warnings are advisory and live in `watch`
        // only — same rule as errors.
        let ds = vec![
            d(
                "/r/a.rs",
                1,
                1,
                tf_core::Severity::Warning,
                Some("unused_variables"),
                "rustc warning",
                Some("rustc"),
            ),
            d(
                "/r/a.rs",
                2,
                1,
                tf_core::Severity::Warning,
                Some("unused_imports"),
                "ra warning",
                Some("rust-analyzer"),
            ),
        ];
        let (auth, adv) = partition_by_provenance(&ds);
        assert_eq!(auth.len(), 1);
        assert_eq!(auth[0].source.as_deref(), Some("rustc"));
        assert_eq!(adv.len(), 1);
        assert_eq!(adv[0].source.as_deref(), Some("rust-analyzer"));
    }

    #[test]
    fn tally_counts_errors_and_warnings_only() {
        let ds = vec![
            d(
                "/r/a.rs",
                1,
                1,
                tf_core::Severity::Error,
                Some("E0277"),
                "x",
                Some("rustc"),
            ),
            d(
                "/r/a.rs",
                2,
                1,
                tf_core::Severity::Error,
                Some("E0308"),
                "y",
                Some("rustc"),
            ),
            d(
                "/r/a.rs",
                3,
                1,
                tf_core::Severity::Warning,
                Some("unused"),
                "z",
                Some("rust-analyzer"),
            ),
            d(
                "/r/a.rs",
                4,
                1,
                tf_core::Severity::Info,
                None,
                "i",
                Some("rustc"),
            ),
            d("/r/a.rs", 5, 1, tf_core::Severity::Hint, None, "h", None),
        ];
        let (e, w) = severity_tally(&ds);
        assert_eq!(e, 2);
        assert_eq!(w, 1);
    }
}
