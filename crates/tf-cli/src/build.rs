//! `build --watch --out <dir>` — maintain the latest-green artifact output.
//!
//! Lead RULING 2: on `StateEvent::BecameGreen`, tf-cli calls a **build-cas
//! publisher entrypoint** (trigger + await build + publish to the canonical
//! `.cargoless/latest-green`); tf-cli's `--out <dir>` responsibility is to
//! materialize/sync the artifact bytes from the CAS per that pointer into
//! the user's directory.
//!
//! That publisher-drive API is build-cas's seam (#23) and is **not yet on
//! `main`** (no `PublishedArtifact`/`UnixSeconds`, no publisher fn). Per the
//! no-guessing discipline this command is a deliberate, honest stub: it
//! validates the v0 invocation (`--out` required; v0 build is watch-only)
//! and exits `EX_UNAVAILABLE` with the exact reason, rather than wiring an
//! unconfirmed signature. The watch/CAS-sync loop drops in here verbatim the
//! moment build-cas DMs the entrypoint contract — localized to this file.

use std::path::Path;
use std::process::ExitCode;

use crate::config::Config;
use crate::ui;

pub fn run(cfg: &Config, out: Option<&Path>) -> ExitCode {
    let Some(out) = out else {
        ui::error(
            "`build` requires `--out <DIR>` (and is watch-only in v0): \
             `cargoless build --watch --out <dir>`.",
        );
        return ExitCode::from(2);
    };

    ui::step(format!(
        "build --watch --out {} ({}, target {})",
        out.display(),
        cfg.detection.describe(),
        cfg.target
    ));

    // No guessing: the publisher-drive entrypoint + the PublishedArtifact /
    // UnixSeconds proto type are build-cas's #23 seam, not yet on main.
    ui::warn(
        "publisher drive pending build-cas #23 (latest-green publisher API). \
         `check`/`watch`/`status`/`clean` are live; `build --watch` wires the \
         moment build-cas pins the publisher entrypoint contract.",
    );
    ExitCode::from(69) // EX_UNAVAILABLE — honest, not a fake success
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Detection;
    use std::path::PathBuf;

    fn cfg() -> Config {
        Config {
            root: PathBuf::from("/proj"),
            target: "wasm32-unknown-unknown".into(),
            cache_dir: PathBuf::from("/proj/.cargoless/cache"),
            detection: Detection::AutoLeptosCdylib,
        }
    }

    #[test]
    fn missing_out_is_usage_error() {
        assert_eq!(run(&cfg(), None), ExitCode::from(2));
    }

    #[test]
    fn with_out_is_unavailable_pending_publisher() {
        assert_eq!(run(&cfg(), Some(Path::new("dist"))), ExitCode::from(69));
    }
}
