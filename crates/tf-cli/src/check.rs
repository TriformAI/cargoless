//! `check` — one-shot verdict; exit code reflects green/red.
//!
//! The compile verdict comes from the daemon's rust-analyzer LSP client +
//! green/red model. Those are daemon-core deliverables not yet on `main`
//! (Plane CWDL tasks: LSP client, model + event bus). `tf-cli` must not edit
//! `tf-core`, so `check` is wired against a single seam — [`verdict`] — that
//! becomes a one-line call into `tf_core` the moment that public entrypoint
//! exists (requested from the lead: `tf_core::check_once(&root) -> TreeState`).
//!
//! Until then `check` does the half it fully owns and is honest about the
//! half it does not: it runs config resolution + project preflight (which
//! catches the most common "it doesn't work" — a mis-detected or
//! mis-configured project) and reports the verdict pipeline as pending rather
//! than fabricating a green. Faking a verdict would directly violate the
//! product's one promise ("always knows what works").

use std::process::ExitCode;

use crate::config::Config;
use crate::ui;

/// The one-shot tree verdict. `Unknown` is a first-class state, not an error:
/// "I cannot currently tell you" is the honest answer while the daemon-core
/// model API is pending, and it is wired to real `Green`/`Red` without
/// touching any call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Green,
    Red(String),
    /// Verdict pipeline not yet wired (daemon-core model API pending).
    Unknown,
}

/// Compute the verdict for a resolved project.
///
/// SEAM: when `tf_core` exposes a one-shot verdict, this becomes
/// `match tf_core::check_once(&cfg.root) { Green => .., Red => .. }`. Mapping
/// `tf_proto::TreeState` here (not at the call site) keeps the swap to this
/// one function.
fn verdict(_cfg: &Config) -> Verdict {
    Verdict::Unknown
}

/// Exit codes (stable contract for scripts/CI):
/// * `0` — green (or, today, preflight OK + verdict pending)
/// * `1` — red (tree does not compile)
/// * `2` — configuration/detection error (could not even identify the project)
pub fn run(cfg: &Config) -> ExitCode {
    ui::step(format!(
        "checking {} ({})",
        cfg.root.display(),
        cfg.detection.describe()
    ));

    match verdict(cfg) {
        Verdict::Green => {
            ui::ok("green — every tracked file compiles".to_string());
            ExitCode::SUCCESS
        }
        Verdict::Red(why) => {
            ui::error(format!("red — {why}"));
            ExitCode::from(1)
        }
        Verdict::Unknown => {
            // Preflight passed (config resolved, project identified). Be
            // explicit that the compile verdict is not yet wired so a caller
            // never mistakes this for a verified green.
            ui::ok("project preflight passed".to_string());
            ui::wait(
                "compile verdict pipeline pending daemon model API — \
                 `check` will return 0=green / 1=red once tf-core exposes it."
                    .to_string(),
            );
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Detection;
    use std::path::PathBuf;

    fn cfg() -> Config {
        Config {
            host: "127.0.0.1".into(),
            port: 8080,
            root: PathBuf::from("/proj"),
            target: "wasm32-unknown-unknown".into(),
            cache_dir: PathBuf::from("/proj/.cargoless/cache"),
            detection: Detection::AutoLeptosCdylib,
        }
    }

    #[test]
    fn preflight_ok_exits_zero_while_verdict_pending() {
        assert_eq!(run(&cfg()), ExitCode::SUCCESS);
    }

    #[test]
    fn verdict_mapping_is_total() {
        // Guards the seam: every Verdict maps to a defined exit, so wiring
        // the real tf-core call cannot silently leave a case unhandled.
        for v in [
            Verdict::Green,
            Verdict::Red("E0599".into()),
            Verdict::Unknown,
        ] {
            let _ = v; // mapping exhaustiveness is enforced in `run`'s match
        }
        assert_eq!(verdict(&cfg()), Verdict::Unknown);
    }
}
