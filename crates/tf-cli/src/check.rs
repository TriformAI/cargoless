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
//! ## FIELD FINDING #8 → #8-redo — verdict/print agreement on severity
//!
//! The original #8 fix said "filter print to source=='rustc'" to match
//! the #21 rustc-source verdict rule. dogfood-lead caught the bigger
//! problem: with `let bad =` at file scope (RA-native parse error, never
//! reaches cargo-check's per-file JSON output), the verdict reported
//! GREEN while cargo check exited non-zero. The first fix had silenced
//! the symptom (contradictory output) but not the actual gap — the
//! verdict itself was wrong.
//!
//! The #8-redo invariant: **the verdict and the diagnostic stream MUST
//! agree on green/red**. The fix is in two coordinated halves:
//!
//! 1. tf-core::model: any `severity:Error` from any source (rustc OR
//!    rust-analyzer-native) flips the file Red. GREEN gating is
//!    unchanged — still requires a completed flycheck with zero errors
//!    of any source. The asymmetry is honest: RA's "saw an error" is
//!    strictly stronger evidence than "didn't see one" because RA's
//!    analysis is partial.
//! 2. tf-cli::check (this file): `is_authoritative` is now severity-
//!    based — `severity:Error` from any source is verdict-driving and
//!    rendered; warnings/info/hints stay source-filtered (rustc-source
//!    kept as authoritative; rust-analyzer-native suppressed in `check`
//!    output and visible in `watch`).
//!
//! `watch` continues to show every diagnostic in #45's timestamp format
//! — a live-debug user actively benefits from seeing the RA-native
//! fast-hint stream, and the `[+N.NNNs]` timeline tells them which
//! signal arrived when. The split is honest: severity:Error from any
//! source is honest evidence of brokenness; severity:Warning from
//! rust-analyzer-native is noise that doesn't drive a verdict either way.
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
            // FIELD FINDING #8-redo: partition by severity-then-provenance.
            // Authoritative = severity:Error from any source + rustc-tier
            // warnings (matches the model's verdict rule). Advisory = RA-
            // native warnings/info/hints (noise that doesn't drive a
            // verdict). On Green, only authoritative is printed; the
            // advisory count is mentioned at the end so the user knows
            // we saw them but knows where to find them (`tftrunk watch`).
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
            // evidence. #8-redo says that evidence is *every severity:Error
            // from any source* (the verdict-driving set) — RA-native parse
            // errors get rendered alongside rustc errors because both are
            // honest evidence the code does not compile.
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

/// FIELD FINDING #8-redo: the print path matches the model's verdict rule
/// (model/`apply_event` and `check_*_with_diagnostics` agree on this).
///
/// **Severity:Error from ANY source is "verdict-driving"** — it appears in
/// `check` output and drives the green/red bit. This matches the dogfood-
/// lead invariant: the verdict (green/red bit) and the diagnostic stream
/// MUST agree. A diagnostic the user reads on stderr is one of the things
/// that made the verdict what it is.
///
/// **Warnings / Info / Hints from non-rustc sources** stay "advisory" —
/// they don't drive the verdict either way (cargo-check is the authority
/// for green), and they tend to be noisy lints in `check` mode. Users can
/// see them all in `watch` mode (#45's timestamp format makes provenance
/// visible by ordering anyway).
///
/// **Warnings / Info / Hints from rustc** are kept (rustc warnings are
/// authoritative — they came from cargo-check just like the errors did).
///
/// The original #8 fix (filter by source only) collapsed everything in the
/// non-rustc bucket to "advisory" — including severity:Error from
/// RA-native parse failures. That hid real errors and produced silent
/// green on a broken tree. Severity-based gating is the honest fix.
fn is_authoritative(d: &tf_core::Diagnostic) -> bool {
    // Severity:Error from any source — always counted, always rendered,
    // always drives the verdict (matches model::apply_event's new rule).
    if d.severity == tf_core::Severity::Error {
        return true;
    }
    // Warnings/Info/Hints: only rustc-source is kept as authoritative
    // (rustc's lint pipeline is the authority on these); rust-analyzer
    // and unsourced are "advisory" — visible in watch, suppressed here.
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
/// hints — don't pollute the happy-path with parentheticals. Per #8-redo
/// the advisory set is severity:Warning/Info/Hint from non-rustc sources
/// only (RA-native lints, etc.) — severity:Error from any source is in
/// the authoritative set and shown on its own line.
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
    // FIELD FINDING #8-redo — verdict/print agreement on severity:Error
    //
    // The original #8 fix partitioned by source only — that silenced the
    // RA-native error path entirely, hiding real parse errors and
    // producing silent-green on a broken tree (the dogfood `let bad =`
    // reproducer). #8-redo: severity:Error from any source is verdict-
    // driving AND always rendered; warnings/info/hints stay source-
    // filtered.
    // -----------------------------------------------------------------------

    #[test]
    fn partition_keeps_every_severity_error_in_authoritative() {
        let ds = vec![
            d(
                "/r/a.rs",
                1,
                1,
                tf_core::Severity::Error,
                Some("E0277"),
                "rustc err",
                Some("rustc"),
            ),
            d(
                "/r/a.rs",
                2,
                1,
                tf_core::Severity::Error,
                Some("syntax-error"),
                "ra-native err",
                Some("rust-analyzer"),
            ),
            d(
                "/r/a.rs",
                3,
                1,
                tf_core::Severity::Warning,
                Some("unused"),
                "ra-native warn",
                Some("rust-analyzer"),
            ),
            // Untagged severity:Error: still authoritative — severity-
            // based gating is unambiguous regardless of source tagging.
            d(
                "/r/a.rs",
                4,
                1,
                tf_core::Severity::Error,
                None,
                "untagged-err",
                None,
            ),
            d(
                "/r/a.rs",
                5,
                1,
                tf_core::Severity::Info,
                None,
                "ra-note",
                None,
            ),
        ];
        let (auth, adv) = partition_by_provenance(&ds);
        assert_eq!(
            auth.len(),
            3,
            "three severity:Error from any source: {auth:?}"
        );
        // All Errors are in `auth` regardless of source.
        let auth_codes: Vec<&str> = auth.iter().filter_map(|d| d.code.as_deref()).collect();
        assert!(auth_codes.contains(&"E0277"));
        assert!(auth_codes.contains(&"syntax-error"));
        // The untagged Error has no code but IS in auth.
        assert!(
            auth.iter()
                .any(|d| d.code.is_none() && d.message == "untagged-err"),
            "untagged severity:Error must be authoritative-rendered"
        );
        // RA-native warnings + RA-native info stay advisory.
        assert_eq!(adv.len(), 2, "ra-native warning + ra-native info: {adv:?}");
    }

    #[test]
    fn is_authoritative_is_severity_based_for_errors_source_based_for_warnings() {
        // Severity:Error → authoritative regardless of source.
        let rustc_err = d(
            "/r/a.rs",
            1,
            1,
            tf_core::Severity::Error,
            None,
            "x",
            Some("rustc"),
        );
        let ra_err = d(
            "/r/a.rs",
            1,
            1,
            tf_core::Severity::Error,
            None,
            "x",
            Some("rust-analyzer"),
        );
        let clippy_err = d(
            "/r/a.rs",
            1,
            1,
            tf_core::Severity::Error,
            None,
            "x",
            Some("clippy"),
        );
        let untagged_err = d("/r/a.rs", 1, 1, tf_core::Severity::Error, None, "x", None);
        assert!(is_authoritative(&rustc_err));
        assert!(
            is_authoritative(&ra_err),
            "RA-native error is honest evidence"
        );
        assert!(
            is_authoritative(&clippy_err),
            "clippy error is honest evidence"
        );
        assert!(
            is_authoritative(&untagged_err),
            "severity beats source tagging"
        );

        // Severity:Warning → source-based (rustc kept, others advisory).
        let rustc_warn = d(
            "/r/a.rs",
            1,
            1,
            tf_core::Severity::Warning,
            None,
            "x",
            Some("rustc"),
        );
        let ra_warn = d(
            "/r/a.rs",
            1,
            1,
            tf_core::Severity::Warning,
            None,
            "x",
            Some("rust-analyzer"),
        );
        let untagged_warn = d("/r/a.rs", 1, 1, tf_core::Severity::Warning, None, "x", None);
        assert!(
            is_authoritative(&rustc_warn),
            "rustc warnings stay authoritative"
        );
        assert!(
            !is_authoritative(&ra_warn),
            "RA-native warnings stay advisory"
        );
        assert!(
            !is_authoritative(&untagged_warn),
            "untagged warning ⇒ advisory"
        );
    }

    #[test]
    fn f8_redo_smoking_gun_ra_native_error_is_rendered_not_suppressed() {
        // The dogfood reproducer that broke the first F8 fix: `let bad =`
        // at file scope produces an RA-native severity:Error, and cargo-
        // check may or may not emit a matching source:rustc diagnostic.
        // Under the first F8 fix this was suppressed as "advisory hint"
        // → user saw "green" exit 0 on a broken tree. Post-#8-redo: the
        // RA-native error IS rendered (severity:Error is verdict-driving
        // regardless of source).
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
        assert_eq!(
            auth.len(),
            1,
            "RA-native severity:Error must be authoritative-rendered"
        );
        assert!(
            adv.is_empty(),
            "no advisory hints — the only diag is an error"
        );

        // Render and verify the user-visible string actually appears.
        let mut buf = Vec::new();
        render_diagnostics(&mut buf, &root, &auth).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("Syntax Error: expected an item"),
            "the syntax error must reach the user: {out:?}"
        );
        assert!(out.contains("src/components/footer.rs:25:1"));
        // The severity tag is "error" not "warning"; rendering preserves it.
        assert!(out.starts_with("error"), "rendered as error: {out:?}");
    }

    #[test]
    fn f8_redo_mixed_publish_renders_both_tiers_when_both_severity_error() {
        // Mixed publish: 1 rustc Error + 1 RA-native Error. Both are
        // severity:Error → both authoritative → both rendered → both
        // counted in the verdict tally. (The prior #8 fix counted only
        // the rustc one, hiding the RA-native one as "advisory".)
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
                "ra syntax",
                Some("rust-analyzer"),
            ),
        ];
        let (auth, adv) = partition_by_provenance(&ds);
        assert_eq!(auth.len(), 2, "both errors are authoritative");
        assert!(adv.is_empty());
        let (errs, _warns) = severity_tally(&auth);
        assert_eq!(errs, 2, "both severity:Error count toward the tally");

        let mut buf = Vec::new();
        render_diagnostics(&mut buf, &root, &auth).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("E0277"));
        assert!(out.contains("trait bound"));
        assert!(out.contains("syntax-error"));
        assert!(
            out.contains("ra syntax"),
            "RA-native error reaches user: {out:?}"
        );
    }

    #[test]
    fn f8_redo_advisory_is_now_warnings_only_from_non_rustc() {
        // The "advisory" channel is now severity:Warning|Info|Hint from
        // non-rustc sources only — those are the genuinely-noisy ones
        // the user might want to hide in `check` and see in `watch`.
        let ds = vec![
            d(
                "/r/a.rs",
                1,
                1,
                tf_core::Severity::Warning,
                Some("unused_imports"),
                "ra lint",
                Some("rust-analyzer"),
            ),
            d(
                "/r/a.rs",
                2,
                1,
                tf_core::Severity::Info,
                None,
                "ra hint",
                Some("rust-analyzer"),
            ),
            d(
                "/r/a.rs",
                3,
                1,
                tf_core::Severity::Hint,
                None,
                "ra hint2",
                Some("rust-analyzer"),
            ),
        ];
        let (auth, adv) = partition_by_provenance(&ds);
        assert!(
            auth.is_empty(),
            "no errors, no rustc-warnings ⇒ nothing authoritative"
        );
        assert_eq!(adv.len(), 3);
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
