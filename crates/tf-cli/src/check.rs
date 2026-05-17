//! `check` — one-shot verdict; exit code reflects green/red; FIELD FINDING
//! #2: prints every diagnostic on red so the user can actually fix it.
//!
//! Bound to daemon-core's adjacent rich-verdict seam (additive alongside the
//! frozen `tf_core::model::check_once`):
//! `tf_core::model::check_once_with_diagnostics(&Path) -> io::Result<CheckResult>`.
//! `CheckResult` pairs the existing boolean `TreeState` (the AC#4 publisher
//! gate) with a `Vec<Diagnostic>` (the human-facing detail the boolean was
//! hiding). The exit-code contract is byte-frozen — only the *body* of the
//! red path gains diagnostic output.
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
            // A green tree can still carry warnings (RA + rustc); show them
            // so the user knows the tool saw them — but the exit code is 0.
            if !diagnostics.is_empty() {
                print_diagnostics(&cfg.root, &diagnostics);
            }
            ui::ok("green — every tracked file compiles");
            ExitCode::SUCCESS
        }
        Ok(tf_core::CheckResult {
            tree: tf_core::TreeState::Red,
            diagnostics,
        }) => {
            // FIELD FINDING #2: a red verdict MUST carry its evidence. If
            // the diagnostic list is somehow empty (RA exited before any
            // publishDiagnostics — unproven-red path), say so explicitly
            // instead of silently swallowing it.
            if diagnostics.is_empty() {
                ui::error(
                    "red — at least one tracked file does not compile, \
                     but no diagnostics were captured before the check \
                     settled (try `tftrunk watch` for live updates, or \
                     re-run with `TF_CHECK_TIMEOUT_SECS=300`).",
                );
            } else {
                print_diagnostics(&cfg.root, &diagnostics);
                let (errs, warns) = severity_tally(&diagnostics);
                ui::error(format!(
                    "red — at least one tracked file does not compile \
                     ({errs} error{}, {warns} warning{} surfaced).",
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
