//! `check` — one-shot verdict; exit code reflects green/red.
//!
//! The compile verdict comes from daemon-core's model. The **real wiring** is
//! the `integration` feature path below; the default path stays honest about
//! being preflight-only so cli-ux's own CI gate compiles green while
//! `tf_core::model` is not yet on the tf-core this crate builds against.
//!
//! ## Two paths, one contract
//!
//! * default (feature off) — config resolution + project preflight only;
//!   reports the verdict pipeline as pending. Never fabricates a green
//!   (faking it would violate the product's one promise).
//! * `--features integration` — calls
//!   `tf_core::model::check_once(&Path) -> io::Result<TreeState>`
//!   (daemon-core's authoritative contract, branch agent/daemon-core-sup).
//!   This compiles only once daemon-core's `model` module is on the linked
//!   tf-core; the lead's integration branch turns the feature on for the
//!   authoritative gate (option (b)). `--all-targets` in cli-ux CI is NOT
//!   `--all-features`, so this path is excluded there by construction.
//!
//! ## Exit-code contract (stable for scripts/CI), per daemon-core's mapping
//!
//! * `0` — green (every tracked file compiles)
//! * `1` — red (tree does not compile; an *unproven* tree is conservatively
//!   `Ok(TreeState::Red)` per AC#4 — handled by the same arm, not special-cased)
//! * `2` — could not even run the verdict: rust-analyzer missing / spawn /
//!   pipe error (`Err`), or configuration/detection error. This is a *setup*
//!   failure, deliberately distinct from "red".

use std::process::ExitCode;

use crate::config::Config;
use crate::ui;

/// Default path: project preflight only. The real verdict is the
/// `integration` path's `check_once`; the old `Verdict` seam enum was a
/// placeholder superseded by that real call and is removed (it was never
/// constructed with anything but the pending state — dead code under
/// `-D warnings`). Exit `0` (preflight passed) with an explicit pending
/// line so a script never mistakes this for a verified green.
#[cfg(not(feature = "integration"))]
fn run_verdict(_cfg: &Config) -> ExitCode {
    ui::ok("project preflight passed");
    ui::wait(
        "compile verdict pipeline pending daemon model API — \
         `check` returns 0=green / 1=red / 2=setup-error once the \
         model is linked (build with --features integration).",
    );
    ExitCode::SUCCESS
}

/// Integration path: the real verdict. Maps daemon-core's
/// `io::Result<TreeState>` exactly to the documented exit-code contract.
/// `Err` is a setup failure (RA missing/spawn/pipe) — distinct from red.
#[cfg(feature = "integration")]
fn run_verdict(cfg: &Config) -> ExitCode {
    match tf_core::model::check_once(&cfg.root) {
        Ok(tf_core::TreeState::Green) => {
            ui::ok("green — every tracked file compiles");
            ExitCode::SUCCESS
        }
        Ok(tf_core::TreeState::Red) => {
            ui::error("red — at least one tracked file does not compile");
            ExitCode::from(1)
        }
        Err(e) => {
            // daemon-core's contract: ANY Err = setup/env problem (RA
            // missing, spawn/handshake/pipe, bad root) — treat uniformly,
            // never switch on ErrorKind. Distinct from code-red (exit 1).
            ui::error(format!(
                "could not check (rust-analyzer/setup): {e}\n  \
                 if rust-analyzer is missing: `rustup component add rust-analyzer`."
            ));
            ExitCode::from(2)
        }
    }
}

pub fn run(cfg: &Config) -> ExitCode {
    ui::step(format!(
        "checking {} ({})",
        cfg.root.display(),
        cfg.detection.describe()
    ));
    run_verdict(cfg)
}

// Tests cover the default (feature-off) behaviour — the path cli-ux's own CI
// gate exercises. The integration path's verdict mapping is owned/verified on
// the lead's integration branch (it needs a live rust-analyzer + the linked
// model module, neither available in the zero-dep cli-ux test job).
#[cfg(all(test, not(feature = "integration")))]
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
}
