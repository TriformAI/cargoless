//! `check` — one-shot verdict; exit code reflects green/red.
//!
//! Bound exact to daemon-core's frozen model contract (on `main`):
//! `tf_core::model::check_once(&Path) -> io::Result<TreeState>`.
//!
//! Exit-code contract (stable for scripts/CI), per daemon-core's mapping —
//! treat ANY `Err` uniformly (do NOT switch on `ErrorKind`):
//! * `0` — green (every tracked file compiles)
//! * `1` — red (tree does not compile; an *unproven* tree is conservatively
//!   `Ok(TreeState::Red)` per AC#4 — same arm, not special-cased)
//! * `2` — could not run the verdict: rust-analyzer missing / spawn / pipe /
//!   bad root (`Err`). A *setup/env* failure, deliberately distinct from red.

use std::process::ExitCode;

use crate::config::Config;
use crate::ui;

pub fn run(cfg: &Config) -> ExitCode {
    ui::step(format!(
        "checking {} ({})",
        cfg.root.display(),
        cfg.detection.describe()
    ));

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
            ui::error(format!(
                "could not check (rust-analyzer/setup): {e}\n  \
                 if rust-analyzer is missing: `rustup component add rust-analyzer`."
            ));
            ExitCode::from(2)
        }
    }
}
