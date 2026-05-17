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
/// SIGNAL DISCIPLINE (revised after the first run found NO_READY/NO_SIGNAL
/// for all three tools — root cause: the original `ready` lists matched
/// daemon-startup signals like cargoless's "verdict pipeline live"
/// [LSP-handshake-done, NOT compile-done] and trunk's "starting build"
/// [first banner, NOT first success]. The harness then started its
/// measurement loop on a still-cold workspace, where 60s edit_timeout was
/// hopelessly under the cold-Leptos first-edit→verdict time):
///
///   * `ready` matches ONLY the first REAL compile-complete signal. For
///     cargoless that is "GREEN — tree compiles" (watch) / "GREEN —
///     building" + "published " (build). For trunk it is "success" (post-
///     build banner). For bacon it is one of "Success!" / "Failure." /
///     "Errors found" (the post-cargo-check banner).
///   * `green` / `red` lists are the steady-state signals on save edges —
///     they reuse the same banner vocabulary so a green ready hit and a
///     subsequent green save both match the same string.
///
/// Watch-tier signal vocabulary on main as of #45 (timestamps on every
/// verdict line) + #55 (tier-filter F8 fix) + #57 (default=integration):
///
///  * cargoless watch (stderr):  `ok {ts}GREEN — tree compiles`
///                               `xx {ts}RED — tree does not compile`
///  * cargoless build (stderr):  `ok GREEN — building`
///                               `ok published <hash> → <dir> (at <s>s)`
///                               `xx build failed — holding last green:…`
///                               `!! RED — holding last green (AC#4)`
///  * trunk watch (stderr):      `INFO  📦 success`/`success` banner per build
///  * bacon --headless (stdout): `Success!` / `Failure.` / `Errors found.`
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
                // STRICT: only the post-compile banners. "verdict pipeline
                // live" / "Streaming verdicts" / "Building latest-green"
                // are pre-compile setup banners — matching them was the
                // original NO_SIGNAL bug.
                //
                // Both modes' banners are listed: in checker mode the
                // "GREEN — tree compiles" line lands (watch.rs); in
                // artifact mode the "GREEN — building" line lands first
                // (build.rs) and then "published <hash>" once the
                // publisher advances `.cargoless/latest-green`. The
                // artifact-mode driver also watches the pointer-file
                // directly as the authoritative witness, so the banner is
                // belt + suspenders for warm gating.
                ready: &["GREEN — tree compiles", "GREEN — building", "published "],
                green: &["GREEN — tree compiles", "GREEN — building", "published "],
                red: &[
                    "RED — tree does not compile",
                    "RED — holding",
                    "build failed",
                ],
            },
            note: Some(
                "cargoless v0 headless watcher + latest-green publisher \
                 (default debouncer 150ms post-#49)",
            ),
        },
        Tool {
            name: "trunk",
            // `trunk watch` rebuilds on edit but does NOT start a HTTP
            // server — the same compile loop as `trunk serve` without the
            // server overhead that would skew the artifact measurement.
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
                // STRICT: only post-build banners. "starting build" was
                // the original false-ready match.
                ready: &["success", "applying new distribution"],
                green: &["success", "applying new distribution"],
                // Be specific: "error" alone matched warning lines from
                // any sub-tool. `error[E` is rustc's stable error prefix;
                // `build failed` is trunk's banner; `wasm-bindgen error`
                // is the bundler's banner.
                red: &["error[E", "build failed", "wasm-bindgen"],
            },
            note: Some("trunk 0.21.x; needs index.html + Trunk.toml in fixture root"),
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
                // STRICT: only post-cargo-check banners. "warning" was the
                // original false-ready match — it fires on any cargo
                // warning line, well before the first check completes.
                ready: &["Success!", "Failure.", "Errors found"],
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
