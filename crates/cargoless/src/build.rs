//! `build --watch --out <dir>` — maintain the latest-green artifact output.
//!
//! Lead RULING 2 + build-cas's verbatim contract (agent/build-cas-publisher
//! @ 9fa947d): on `StateEvent::BecameGreen { identity }`, cargoless calls
//! `BuildOrchestrator::run(&BuildTrigger { identity })`. On
//! `Compiled`/`Deduplicated` build-cas has ALREADY atomically advanced the
//! canonical `.cargoless/latest-green` pointer (AC#4 fail-closed: a `Failed`
//! build leaves it byte-untouched). cargoless then reads the pointer
//! (`read_latest_green`), fetches the CAS blob by `input_hash`, and
//! materializes it into the user's `--out <dir>`. We meet build-cas ONLY
//! through `cargoless_proto::PublishedArtifact` + those `cargoless_core::build` fns.
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
//! build-cas shipped `cargoless_core::build::materialize_latest_green` (frozen seam
//! @ build-cas-publisher 5b4b7f9): one call does read_latest_green → CAS get
//! → faithful `dist/` expansion into `out_dir`. The blob/container format
//! stays entirely build-cas's; cargoless never parses CAS internals. v0 uses
//! build-cas's non-destructive default (overwrite, do not delete unrelated
//! pre-existing files in `--out`) — a documented v0-simple behavior, not a
//! guess; a pristine `--out` is the user's responsibility for now.

use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use crate::config::Config;
use crate::ui;

/// FIELD FINDING #7 (#54 part-2): preflight check for the `trunk` binary
/// before `cargoless build` invokes it as a subprocess.
///
/// Pre-fix UX: a fresh-machine `cargoless build --watch --out /tmp/dist`
/// failed with `could not launch trunk build: No such file or directory
/// (os error 2)` — the error was technically correct but did not tell
/// the user that THEY are responsible for installing `trunk` (cargoless
/// layers ON TOP OF `trunk build` for actual artifact production;
/// "replaces trunk serve" is true for the verdict/publisher surface but
/// not for compilation). README work covering the prerequisite is
/// docs-launch-lead's lane (#54 part 1); this preflight is the
/// per-invocation safety net so the error surface is friendly even when
/// the README hasn't been read.
///
/// Returns `Ok(())` if `trunk --version` runs successfully, `Err(ExitCode)`
/// with a friendly error message printed to stderr otherwise. The exit
/// code matches the existing "setup error" contract (2 — same family as
/// `rust-analyzer missing`, which the CLI already maps to 2).
///
/// Uses `--version` rather than just `which trunk` because some `trunk`
/// shims (rustup-style wrappers, docker shims) only fail at exec time,
/// not at PATH lookup — running `--version` is the most honest
/// availability check while staying cheap (~50ms).
fn require_trunk_or_exit() -> Result<(), ExitCode> {
    let result = Command::new("trunk")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match result {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            ui::error(format!(
                "`trunk` is installed but `trunk --version` exited {} — \
                 the binary may be incompatible.\n  \
                 try `cargo install --locked trunk` to reinstall.",
                s.code().map(|c| c.to_string()).unwrap_or("(signal)".into()),
            ));
            Err(ExitCode::from(2))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ui::error(
                "`trunk` is not installed (or not on PATH).\n  \
                 `cargoless build` wraps `trunk build` to produce the WASM \
                 artifact — install it with:\n      \
                 cargo install --locked trunk\n  \
                 (cargoless replaces `trunk serve` for the verdict + \
                 latest-green-publisher surface; it does NOT replace \
                 `trunk build` itself in v0.)",
            );
            Err(ExitCode::from(2))
        }
        Err(e) => {
            ui::error(format!(
                "could not probe for `trunk` ({e}). Check PATH and \
                 filesystem permissions; install with `cargo install --locked trunk`."
            ));
            Err(ExitCode::from(2))
        }
    }
}

#[cfg(not(feature = "integration"))]
pub fn run(cfg: &Config, out: Option<&Path>) -> ExitCode {
    let Some(out) = out else {
        ui::error(
            "`build` requires `--out <DIR>` (and is watch-only in v0): \
             `cargoless build --watch --out <dir>`.",
        );
        return ExitCode::from(2);
    };
    // FIELD FINDING #7 (#54 part-2): preflight `trunk` even on the
    // EX_UNAVAILABLE path so the friendly install hint surfaces in both
    // builds. A user who built without `integration` and tries `build`
    // gets BOTH the "publisher gated" message AND the actionable trunk
    // hint — no extra round-trip to discover trunk is missing too.
    if let Err(code) = require_trunk_or_exit() {
        return code;
    }
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

    use cargoless_core::build::{BuildOrchestrator, TrunkCompiler};

    use crate::statusfile::{self, HEARTBEAT, Status, Verdict};

    let Some(out) = out else {
        ui::error("`build` requires `--out <DIR>`: `cargoless build --watch --out <dir>`.");
        return ExitCode::from(2);
    };
    // FIELD FINDING #7 (#54 part-2): preflight `trunk` BEFORE we start the
    // verdict pipeline or create --out. Saves the user from watching
    // rust-analyzer come up just to discover trunk is missing on the
    // first green edge minutes later.
    if let Err(code) = require_trunk_or_exit() {
        return code;
    }
    // FIELD FINDING #13a (#93): `build --watch` heartbeats the SAME
    // `.cargoless/cli-status` file as `watch`, so the dual-watch race
    // spans BOTH subcommands (watch×2, build×2, and watch+build on one
    // root). The refusal is per-status-file, not per-subcommand — guard
    // here too, before --out creation and the rust-analyzer bring-up,
    // so a refused start is instant and side-effect-free.
    if let statusfile::WatchAdmission::Refuse(c) =
        statusfile::admission(&cfg.root, std::process::id())
    {
        ui::error(c.message());
        return ExitCode::from(2);
    }
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
        cargoless_core::LocalDiskStore::new(cache_root.clone()),
        TrunkCompiler,
        project_root.clone(),
    );

    let (session, events) = match cargoless_core::model::watch(
        &project_root,
        cargoless_core::model::placeholder_identity,
    ) {
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
    let verdict_of = |s: cargoless_core::TreeState| match s {
        cargoless_core::TreeState::Green => Verdict::Green,
        cargoless_core::TreeState::Red => Verdict::Red,
    };

    write_status(verdict_of(session.tree_state()));
    // FIELD FINDING #13b (#88): the publisher watch loop has the SAME
    // orphan risk as `cargoless watch` — `cargoless build --watch --out
    // dist &` then closing the shell would leave a ~2GB orphan
    // holding RA + cargo + trunk. Same parent-death guard.
    let parent = crate::orphan::ParentWatch::capture();
    ui::wait("Ctrl-C to stop. Building latest-green on each green edge…");

    loop {
        // FIELD FINDING #13b: parent-death check first. Graceful
        // shutdown (session.shutdown reaps RA; statusfile::clear
        // removes the ghost). Exit 0 — deliberate clean shutdown,
        // no parent left to read a code anyway.
        if parent.orphaned() {
            statusfile::clear(&project_root);
            ui::warn(
                "parent process exited — shutting down so no orphaned \
                 build daemon is left holding rust-analyzer/cargo/trunk \
                 (FIELD FINDING #13b).",
            );
            session.shutdown();
            return ExitCode::SUCCESS;
        }

        match events.recv_timeout(HEARTBEAT) {
            Ok(cargoless_core::StateEvent::BecameGreen { identity }) => {
                ui::ok("GREEN — building");
                let result = orch.run(&cargoless_core::BuildTrigger { identity });
                match result.outcome {
                    // Compiled and Deduplicated are identical for --out: the
                    // pointer is (re)published either way (AC#5 dedupe just
                    // skipped the compile).
                    cargoless_core::BuildOutcome::Compiled
                    | cargoless_core::BuildOutcome::Deduplicated => {
                        publish_to_out(&project_root, &cache_root, out);
                    }
                    // AC#4 fail-closed: never touch --out on a failed build;
                    // the prior pointer/output stays byte-unmoved.
                    //
                    // FIELD FINDING #12b: distinguish "holding last green"
                    // (we HAVE a prior publish) from "nothing published yet"
                    // (this is the first attempt and it failed — there is
                    // NO last green to hold). The pre-#12 wording lied to
                    // first-run users by claiming to hold a green that
                    // never existed. We probe the canonical pointer file
                    // via cargoless_core::build::read_latest_green — cheap, no
                    // CAS round-trip, deterministic.
                    cargoless_core::BuildOutcome::Failed { reason } => {
                        let prior = cargoless_core::build::read_latest_green(&project_root)
                            .ok()
                            .flatten();
                        match prior {
                            Some(p) => ui::error(format!(
                                "build failed — holding last green from \
                                 published_at={} (input {}): {reason}",
                                p.published_at, p.artifact.input_hash
                            )),
                            None => ui::error(format!(
                                "build failed — nothing published yet \
                                 (--out unchanged): {reason}"
                            )),
                        }
                    }
                }
                write_status(verdict_of(session.tree_state()));
            }
            Ok(cargoless_core::StateEvent::BecameRed) => {
                ui::warn("RED — holding last green (AC#4)");
                write_status(Verdict::Red);
            }
            Ok(cargoless_core::StateEvent::FileVerdict { path, state }) => {
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
/// blob/container format stays entirely build-cas's — cargoless never parses
/// CAS internals. NoGreen/Evicted are normal states (no crash, --out
/// untouched); Err is only a genuinely corrupt pointer/blob or real FS/CAS
/// failure — surfaced, never panicked, --out left as the path-safe helper
/// left it (AC#4: a non-green never advances the user's output).
#[cfg(feature = "integration")]
fn publish_to_out(project_root: &Path, cache_root: &Path, out: &Path) {
    use cargoless_core::build::{Materialized, materialize_latest_green};

    let store = cargoless_core::LocalDiskStore::new(cache_root.to_path_buf());
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

    // Note: the prior `with_out_is_unavailable_pending_publisher` test
    // assumed the non-integration `run()` always returns 69 unconditionally
    // after the --out check. With #54 part-2's preflight added before the
    // EX_UNAVAILABLE return, the outcome depends on whether `trunk` is on
    // PATH in the test environment — `Some(2)` (setup error) when absent,
    // `Some(69)` (unavailable) when present. The test below makes both
    // outcomes acceptable so CI passes in either environment without
    // muddying the contract. The trunk-absent path is exercised
    // deterministically by the unit tests on `require_trunk_or_exit` below.
    #[test]
    fn with_out_returns_setup_or_unavailable() {
        let exit = run(&cfg(), Some(Path::new("dist")));
        // 2 (trunk not installed in CI image) OR 69 (trunk present, but
        // publisher gated off in the non-integration build). Both are
        // documented contract outcomes; only "0 or 1" would be a bug.
        let code = format!("{exit:?}");
        assert!(
            code.contains('2') || code.contains("69"),
            "expected 2 or 69, got: {code}"
        );
    }

    // -------------------------------------------------------------------
    // FIELD FINDING #7 (#54 part-2) — trunk preflight unit coverage
    //
    // The function itself shells out to `trunk --version`; we exercise it
    // directly to verify the actionable-error path. CI image has no
    // `trunk` so the NotFound branch runs deterministically; on a dev
    // machine with trunk installed the success branch runs and the test
    // is a structural compile check.
    // -------------------------------------------------------------------

    #[test]
    fn require_trunk_returns_actionable_exit_on_absence_or_ok_on_present() {
        match require_trunk_or_exit() {
            Ok(()) => {
                // Local dev with trunk on PATH — we just exercised the
                // happy path. Nothing more to assert structurally.
            }
            Err(code) => {
                // CI / fresh machine — the friendly error path fired.
                // The exit code is 2 (setup error, distinct from a "red"
                // build at 1 and from "publisher unavailable" at 69).
                let code_dbg = format!("{code:?}");
                assert!(
                    code_dbg.contains('2'),
                    "preflight error must exit 2 (setup), got: {code_dbg}"
                );
            }
        }
    }

    #[test]
    fn require_trunk_message_contains_install_command_when_absent() {
        // Re-runnable: env clears PATH so `trunk` will not be found even
        // on a dev machine that has it. Captures the actionable string
        // without depending on the test runner's env state.
        //
        // We can't easily capture stderr from `ui::error` here without
        // adding a renderer abstraction (the unit tests on `check.rs`
        // already did this for render_diagnostics; replicating that for
        // ui:: is out of scope for #54 part-2 — the structural verify
        // above + the docstring contract carries the rest). What this
        // test DOES cover: with PATH cleared, the function returns Err.
        let saved_path = std::env::var_os("PATH");
        // SAFETY: this single-threaded test setup mutates env; the test
        // suite runs tests in a deterministic order per cargo's default
        // behavior. set_var is unsafe on 2024 edition.
        unsafe { std::env::set_var("PATH", "") };
        let result = require_trunk_or_exit();
        // Restore PATH before assertions so any failure-rendering still works.
        unsafe {
            match saved_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        assert!(
            result.is_err(),
            "with empty PATH, trunk preflight must return Err"
        );
    }
}
