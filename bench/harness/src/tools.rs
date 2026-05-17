//! Comparative tool registry.
//!
//! Each tool has two roles: a *checker* (continuous compile-check loop with
//! green/red verdicts) and optionally an *artifact* (continuous re-build of
//! a wasm dist). The registry is the single source of truth for:
//!
//!  * the executable + args used in each mode,
//!  * the substring patterns that mean "ready", "green", and "red",
//!  * the artifact-publish witness (file to stat, or pointer to read).
//!
//! Substring-based signal matching (not regex) keeps us std-only and is
//! resilient to tool versioning: each tool's *banner words* are stable
//! across versions (cargoless prints "GREEN"/"RED"; trunk prints "success"/
//! "error"; bacon prints "Success"/"Failure" / "Errors found"). If a tool
//! later changes its banner words, the fix is one line here.
//!
//! Adding a 4th tool (e.g. `cargo-watch -x check`) is a one-struct PR.

use std::path::{Path, PathBuf};

/// Patterns the harness substring-matches on each line of stdout/stderr.
/// "Any of" semantics: the line counts as a hit if it contains ANY of the
/// patterns in the relevant list.
#[derive(Debug, Clone)]
pub struct Signals {
    /// Line(s) that mean "warmed up — initial compile complete, idle for
    /// edits". Used to gate the start of measurement.
    pub ready: &'static [&'static str],
    /// Line(s) that mean "the tree is GREEN — the most recent verdict was
    /// successful compilation". Used for save→verdict edge timing.
    pub green: &'static [&'static str],
    /// Line(s) that mean "the tree is RED — the most recent verdict was a
    /// compile failure". Used for save→verdict edge timing.
    pub red: &'static [&'static str],
}

/// What the artifact-mode driver watches for "publish happened".
#[derive(Debug, Clone)]
pub enum PublishWitness {
    /// Stat a single file inside the project root; "published" = mtime
    /// strictly newer than the baseline captured before the edit.
    ///
    /// Path is relative to the fixture project root.
    FileMtime(PathBuf),
    /// Read a small text file in the project root; "published" = its
    /// contents differ from the baseline captured before the edit.
    /// Used for cargoless `.cargoless/latest-green` pointer (which is an
    /// `input_hash` string that changes on every advance).
    FileContents(PathBuf),
    /// This tool does not produce a publishable artifact (bacon).
    None,
}

/// One comparative tool. `None` cmd/witness marks "not applicable".
#[derive(Debug, Clone)]
pub struct Tool {
    pub name: &'static str,
    /// Argv for checker mode (program + args). Run with cwd = fixture.
    pub checker_argv: Vec<String>,
    /// Argv for artifact mode (program + args). `None` if this tool does
    /// not publish artifacts.
    pub artifact_argv: Option<Vec<String>>,
    /// Witness for artifact-publish completion.
    pub artifact_witness: PublishWitness,
    /// Banner-words for substring matching.
    pub signals: Signals,
    /// Optional human-readable note for the report (e.g. "bacon = checker-
    /// only by design").
    pub note: Option<&'static str>,
}

impl Tool {
    pub fn supports_artifact(&self) -> bool {
        self.artifact_argv.is_some() && !matches!(self.artifact_witness, PublishWitness::None)
    }

    pub fn program(&self) -> &str {
        self.checker_argv
            .first()
            .map(String::as_str)
            .unwrap_or(self.name)
    }
}

/// Build the comparative registry. `cargoless_bin` is the path (or PATH
/// name) of the cargoless binary to test, `cargoless_out` is the artifact
/// `--out` directory to materialize into.
///
/// Static-string signals work for all three v0 tools — promoted from raw
/// observation of their stdout, NOT from clever regex. We pick noun-banner
/// words rather than punctuation or emoji because terminals strip those
/// inconsistently:
///
///  * cargoless: `ui::ok("GREEN — tree compiles")` /
///    `ui::error("RED — tree does not compile")` (stderr) and
///    `ui::ok("published <hash> → <dir>")` / `ui::warn("RED — holding
///    last green")` (in build mode).
///  * trunk:    `INFO compilation finished`/`SUCCESS` on green;
///              `ERROR`/`build failed`/`error[E` on red.
///  * bacon:    `Success!` on green; `Failure.`/`Errors found`/`error[E`
///              on red.
pub fn registry(cargoless_bin: &str, cargoless_out: &Path) -> Vec<Tool> {
    let cargoless_out_s = cargoless_out.to_string_lossy().into_owned();
    vec![
        Tool {
            name: "cargoless",
            // Continuous verdict stream. `watch` is the right mode for the
            // AC#2 dimension — no artifact build is on the critical path.
            checker_argv: vec![cargoless_bin.to_string(), "watch".to_string()],
            artifact_argv: Some(vec![
                cargoless_bin.to_string(),
                "build".to_string(),
                "--watch".to_string(),
                "--out".to_string(),
                cargoless_out_s,
            ]),
            // The publisher writes `.cargoless/latest-green` atomically on
            // each green edge; its contents are the new `input_hash`. A
            // content-diff witness is honest in a way mtime-only isn't —
            // it cannot be spoofed by `touch`.
            artifact_witness: PublishWitness::FileContents(PathBuf::from(
                ".cargoless/latest-green",
            )),
            signals: Signals {
                ready: &[
                    "verdict pipeline live",
                    "Streaming verdicts",
                    "Building latest-green",
                    "GREEN", // first cold green also counts as ready
                ],
                green: &["GREEN", "published "],
                red: &["RED", "build failed"],
            },
            note: Some("cargoless v0 headless watcher + latest-green publisher"),
        },
        Tool {
            name: "trunk",
            // `trunk watch` rebuilds on edit but does NOT start a HTTP
            // server — the same compile loop as `trunk serve` without the
            // server overhead that would skew the artifact measurement.
            // `--no-autoreload` keeps it from injecting reload JS, which
            // doesn't affect compile latency but keeps output minimal.
            checker_argv: vec!["trunk".to_string(), "watch".to_string()],
            artifact_argv: Some(vec![
                "trunk".to_string(),
                "watch".to_string(),
                "--dist".to_string(),
                // trunk's --dist is project-relative; the artifact driver
                // resolves the witness against the same project root.
                "trunk-dist".to_string(),
            ]),
            artifact_witness: PublishWitness::FileMtime(PathBuf::from("trunk-dist/index.html")),
            signals: Signals {
                ready: &["success", "applying new distribution", "starting build"],
                green: &["success", "applying new distribution"],
                red: &["error", "failed", "panicked"],
            },
            note: None,
        },
        Tool {
            name: "bacon",
            // `bacon` is a TUI by default; `--headless` writes plain text
            // banner lines we can match. Default job in modern bacon is
            // `check` (cargo check). We pin it explicitly so a contributor
            // with a custom default doesn't get a misleading number.
            checker_argv: vec![
                "bacon".to_string(),
                "--headless".to_string(),
                "--job".to_string(),
                "check".to_string(),
            ],
            artifact_argv: None,
            artifact_witness: PublishWitness::None,
            signals: Signals {
                // bacon prints `Success!` / `Failure.` / `Errors found.`
                // banners; `Job ` is the per-run divider that fires on
                // every edit.
                ready: &["Success!", "Failure.", "Errors found", "warning"],
                green: &["Success!"],
                red: &["Failure.", "Errors found", "error[E"],
            },
            note: Some("bacon is checker-only by design (no wasm artifact publish)"),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn registry_has_three_tools() {
        let r = registry("tftrunk", Path::new("/tmp/out"));
        assert_eq!(r.len(), 3);
        let names: Vec<_> = r.iter().map(|t| t.name).collect();
        assert!(names.contains(&"cargoless"));
        assert!(names.contains(&"trunk"));
        assert!(names.contains(&"bacon"));
    }

    #[test]
    fn bacon_has_no_artifact_mode() {
        let r = registry("tftrunk", Path::new("/tmp/out"));
        let bacon = r.iter().find(|t| t.name == "bacon").unwrap();
        assert!(!bacon.supports_artifact());
    }

    #[test]
    fn cargoless_and_trunk_support_artifact_mode() {
        let r = registry("tftrunk", Path::new("/tmp/out"));
        assert!(r
            .iter()
            .find(|t| t.name == "cargoless")
            .unwrap()
            .supports_artifact());
        assert!(r
            .iter()
            .find(|t| t.name == "trunk")
            .unwrap()
            .supports_artifact());
    }

    #[test]
    fn cargoless_argv_threads_the_bin_path() {
        let r = registry("/opt/tftrunk", Path::new("/tmp/out"));
        let c = r.iter().find(|t| t.name == "cargoless").unwrap();
        assert_eq!(c.checker_argv[0], "/opt/tftrunk");
        assert_eq!(c.artifact_argv.as_ref().unwrap()[0], "/opt/tftrunk");
    }
}
