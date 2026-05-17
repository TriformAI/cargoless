//! `build --watch --out <dir>` — maintain the latest-green artifact output.
//!
//! Lead RULING 2 + build-cas's verbatim contract (agent/build-cas-publisher
//! @ 9fa947d): on `StateEvent::BecameGreen { identity }`, tf-cli calls
//! `BuildOrchestrator::run(&BuildTrigger { identity })`. On
//! `Compiled`/`Deduplicated` build-cas has ALREADY atomically advanced the
//! canonical `.cargoless/latest-green` pointer (AC#4 fail-closed: a `Failed`
//! build leaves it byte-untouched). tf-cli then reads the pointer
//! (`read_latest_green`), fetches the CAS blob by `input_hash`, and
//! materializes it into the user's `--out <dir>`. We meet build-cas ONLY
//! through `tf_proto::PublishedArtifact` + those `tf_core::build` fns.
//!
//! ## Feature gate
//!
//! `read_latest_green` / `PublishedArtifact` / the publisher pointer live on
//! build-cas's branch, NOT on this branch's `main` base — so the real loop
//! is behind `#[cfg(feature = "integration")]` (off by default, lock-neutral
//! — a local-crate feature does not touch `Cargo.lock`, so the `--locked`
//! gate stays green). The default path is an honest `EX_UNAVAILABLE` stub.
//! #29 convergence enables `integration` on the converged tree.
//!
//! ## Blob → dir materialization (RESOLVED — option (b))
//!
//! build-cas shipped `tf_core::build::materialize_latest_green` (frozen seam
//! @ build-cas-publisher 5b4b7f9): one call does read_latest_green → CAS get
//! → faithful `dist/` expansion into `out_dir`. The blob/container format
//! stays entirely build-cas's; tf-cli never parses CAS internals. v0 uses
//! build-cas's non-destructive default (overwrite, do not delete unrelated
//! pre-existing files in `--out`) — a documented v0-simple behavior, not a
//! guess; a pristine `--out` is the user's responsibility for now.

use std::path::Path;
use std::process::ExitCode;

use crate::config::Config;
use crate::ui;

#[cfg(not(feature = "integration"))]
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
    ui::warn(
        "publisher drive is gated behind the `integration` feature pending \
         #29 convergence (build-cas latest-green publisher). \
         `check`/`watch`/`status`/`clean` are live now.",
    );
    ExitCode::from(69) // EX_UNAVAILABLE — honest, not a fake success
}

#[cfg(feature = "integration")]
pub fn run(cfg: &Config, out: Option<&Path>) -> ExitCode {
    use std::sync::mpsc::RecvTimeoutError;

    use tf_core::build::{BuildOrchestrator, TrunkCompiler};

    use crate::statusfile::{self, HEARTBEAT, Status, Verdict};

    let Some(out) = out else {
        ui::error("`build` requires `--out <DIR>`: `cargoless build --watch --out <dir>`.");
        return ExitCode::from(2);
    };
    if let Err(e) = std::fs::create_dir_all(out) {
        ui::error(format!(
            "could not create --out {} ({e}). Check the path is writable.",
            out.display()
        ));
        return ExitCode::from(1);
    }

    let project_root = cfg.root.clone();
    let cache_root = crate::config::cache_root(&project_root);
    ui::step(format!(
        "build --watch --out {} ({}) — CAS {}",
        out.display(),
        cfg.detection.describe(),
        cache_root.display()
    ));

    let orch = BuildOrchestrator::new(
        tf_core::LocalDiskStore::new(cache_root.clone()),
        TrunkCompiler,
        project_root.clone(),
    );

    let (session, events) =
        match tf_core::model::watch(&project_root, tf_core::model::placeholder_identity) {
            Ok(se) => se,
            Err(e) => {
                ui::error(format!(
                    "could not start the verdict pipeline (rust-analyzer/setup): {e}\n  \
                     install rust-analyzer: `rustup component add rust-analyzer`."
                ));
                return ExitCode::from(2);
            }
        };

    let root_for_status = project_root.clone();
    let started = statusfile::now_unix();
    let write_status = |verdict: Verdict| {
        statusfile::write(
            &root_for_status,
            &Status {
                pid: std::process::id(),
                root: root_for_status.display().to_string(),
                started,
                updated: statusfile::now_unix(),
                verdict_str: verdict.as_str().to_string(),
            },
        );
    };
    let verdict_of = |s: tf_core::TreeState| match s {
        tf_core::TreeState::Green => Verdict::Green,
        tf_core::TreeState::Red => Verdict::Red,
    };

    write_status(verdict_of(session.tree_state()));
    ui::wait("Ctrl-C to stop. Building latest-green on each green edge…");

    loop {
        match events.recv_timeout(HEARTBEAT) {
            Ok(tf_core::StateEvent::BecameGreen { identity }) => {
                ui::ok("GREEN — building");
                let result = orch.run(&tf_core::BuildTrigger { identity });
                match result.outcome {
                    // Compiled and Deduplicated are identical for --out: the
                    // pointer is (re)published either way (AC#5 dedupe just
                    // skipped the compile).
                    tf_core::BuildOutcome::Compiled | tf_core::BuildOutcome::Deduplicated => {
                        publish_to_out(&project_root, &cache_root, out);
                    }
                    // AC#4 fail-closed: never touch --out on a failed build;
                    // the prior pointer/output stays byte-unmoved.
                    tf_core::BuildOutcome::Failed { reason } => {
                        ui::error(format!("build failed — holding last green: {reason}"));
                    }
                }
                write_status(verdict_of(session.tree_state()));
            }
            Ok(tf_core::StateEvent::BecameRed) => {
                ui::warn("RED — holding last green (AC#4)");
                write_status(Verdict::Red);
            }
            Ok(tf_core::StateEvent::FileVerdict { path, state }) => {
                ui::step(format!("{path}: {state:?}"));
                write_status(verdict_of(session.tree_state()));
            }
            Err(RecvTimeoutError::Timeout) => {
                write_status(verdict_of(session.tree_state()));
            }
            Err(RecvTimeoutError::Disconnected) => {
                statusfile::clear(&project_root);
                ui::warn("verdict pipeline stopped — exiting.");
                session.shutdown();
                return ExitCode::from(1);
            }
        }
    }
}

/// Materialize the just-published latest-green tree into `out` via
/// build-cas's frozen `materialize_latest_green` seam (option (b)). The
/// blob/container format stays entirely build-cas's — tf-cli never parses
/// CAS internals. NoGreen/Evicted are normal states (no crash, --out
/// untouched); Err is only a genuinely corrupt pointer/blob or real FS/CAS
/// failure — surfaced, never panicked, --out left as the path-safe helper
/// left it (AC#4: a non-green never advances the user's output).
#[cfg(feature = "integration")]
fn publish_to_out(project_root: &Path, cache_root: &Path, out: &Path) {
    use tf_core::build::{Materialized, materialize_latest_green};

    let store = tf_core::LocalDiskStore::new(cache_root.to_path_buf());
    match materialize_latest_green(&store, project_root, out) {
        Ok(Materialized::Materialized(pa)) => ui::ok(format!(
            "published {} → {} (at {}s)",
            pa.artifact.input_hash,
            out.display(),
            pa.published_at.0
        )),
        Ok(Materialized::NoGreen) => {
            ui::warn("published pointer missing after green build — will retry next edge");
        }
        Ok(Materialized::Evicted(pa)) => ui::warn(format!(
            "CAS blob for {} evicted (cache cleaned) — re-trigger on next green edge",
            pa.artifact.input_hash
        )),
        Err(e) => ui::error(format!(
            "could not materialize latest-green into --out ({e}) — holding last good"
        )),
    }
}

#[cfg(all(test, not(feature = "integration")))]
mod tests {
    use super::*;
    use crate::config::Detection;
    use std::path::PathBuf;

    fn cfg() -> Config {
        Config {
            root: PathBuf::from("/proj"),
            target: "wasm32-unknown-unknown".into(),
            cache_dir: PathBuf::from("/tmp/cargoless/x"),
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
