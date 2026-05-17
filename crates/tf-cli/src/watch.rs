//! `watch` (and `check --watch`) — continuous **headless** verdict stream.
//!
//! Bound exact to daemon-core's frozen contract (on `main`):
//! `tf_core::model::watch(&Path, IdentityProvider) -> io::Result<(
//! ModelSession, std::sync::mpsc::Receiver<StateEvent>)>`. We pass
//! `tf_core::model::placeholder_identity` directly — a bare
//! `fn() -> BuildIdentity` satisfying the blanket `IdentityProvider` impl;
//! tf-cli never computes a `BuildIdentity` (build-cas owns the real seam).
//!
//! Headless: prints verdict transitions to stderr, NO browser/HTTP. The
//! event stream is drained with [`HEARTBEAT`](crate::statusfile::HEARTBEAT)
//! `recv_timeout` so the tf-cli status file's `updated` is refreshed even
//! when a quiet green tree emits no events — that heartbeat is the liveness
//! signal `status` reads (RULING 1). `ModelSession` is held for the whole
//! run (drop = graceful shutdown: stops watcher, kills rust-analyzer).
//!
//! AC#1 (D-A1, headless): daemon up + detection + verdict pipeline live
//! within 30s — asserted against the time for `watch()` to return.

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::statusfile::{self, HEARTBEAT, Status, Verdict};
use crate::ui;

const BRINGUP_BUDGET: Duration = Duration::from_secs(30);

fn verdict_of(s: tf_core::TreeState) -> Verdict {
    match s {
        tf_core::TreeState::Green => Verdict::Green,
        tf_core::TreeState::Red => Verdict::Red,
    }
}

/// FIELD FINDING #2: render every diagnostic the model currently knows
/// about, same format as `check`'s renderer (file:line:col + severity +
/// code + message + source) so a `watch` user sees a consistent shape
/// whether they re-ran `check` or kept the stream open. Stderr-only.
/// Delegates to the shared `crate::check::render_diagnostics` so a fix to
/// the format never has to be applied in two places.
fn print_diagnostics(root: &Path, diags: &[tf_core::Diagnostic]) {
    if diags.is_empty() {
        return;
    }
    let mut err = std::io::stderr();
    let _ = crate::check::render_diagnostics(&mut err, root, diags);
    let _ = err.flush();
}

pub fn run(cfg: &Config) -> ExitCode {
    let t0 = Instant::now();
    ui::step(format!(
        "cargoless {} — watching {} ({})",
        env!("CARGO_PKG_VERSION"),
        cfg.root.display(),
        cfg.detection.describe()
    ));

    // `watch()` blocks on rust-analyzer's initialize handshake (seconds);
    // the first green waits on a cold cargo check (minutes) — that is fine,
    // AC#1 is "pipeline live", not "first green".
    let (session, events) =
        match tf_core::model::watch(&cfg.root, tf_core::model::placeholder_identity) {
            Ok(se) => se,
            Err(e) => {
                ui::error(format!(
                    "could not start the verdict pipeline (rust-analyzer/setup): {e}\n  \
                     install rust-analyzer: `rustup component add rust-analyzer`."
                ));
                return ExitCode::from(2);
            }
        };

    let bringup = t0.elapsed();
    if bringup <= BRINGUP_BUDGET {
        ui::ok(format!(
            "verdict pipeline live in {:.2}s (AC#1 budget {}s) — headless, no browser",
            bringup.as_secs_f64(),
            BRINGUP_BUDGET.as_secs()
        ));
    } else {
        ui::warn(format!(
            "bring-up took {:.2}s, over the {}s AC#1 budget — investigate.",
            bringup.as_secs_f64(),
            BRINGUP_BUDGET.as_secs()
        ));
    }

    let root = cfg.root.clone();
    let started = statusfile::now_unix();
    let write_status = |verdict: Verdict| {
        statusfile::write(
            &root,
            &Status {
                pid: std::process::id(),
                root: root.display().to_string(),
                started,
                updated: statusfile::now_unix(),
                verdict_str: verdict.as_str().to_string(),
            },
        );
    };
    write_status(verdict_of(session.tree_state()));
    ui::wait("Ctrl-C to stop. Streaming verdicts…");

    // Single-thread drain (Receiver is not Sync). recv_timeout = heartbeat:
    // refresh the status file on every event AND every quiet HEARTBEAT so
    // `status` sees a live daemon even with no file changes.
    loop {
        match events.recv_timeout(HEARTBEAT) {
            Ok(ev) => {
                match &ev {
                    tf_core::StateEvent::BecameGreen { .. } => ui::ok("GREEN — tree compiles"),
                    tf_core::StateEvent::BecameRed => {
                        ui::error("RED — tree does not compile");
                        // FIELD FINDING #2: on the red edge, print every
                        // diagnostic the model knows about so the user can
                        // act on it without re-running `check`.
                        print_diagnostics(&cfg.root, &session.current_diagnostics());
                    }
                    tf_core::StateEvent::FileVerdict { path, state } => {
                        ui::step(format!("{path}: {state:?}"));
                        // On a per-file flip, surface just that file's
                        // diagnostics (the change-set the user is debugging).
                        // Compare via `to_str()` so we never accidentally
                        // alias an OsString that isn't valid UTF-8 with a
                        // String path — model paths come from `path_from_uri`
                        // which is already UTF-8.
                        let path_str = path.as_str();
                        let all = session.current_diagnostics();
                        let just_this_file: Vec<_> = all
                            .into_iter()
                            .filter(|d| d.file_path.to_str() == Some(path_str))
                            .collect();
                        print_diagnostics(&cfg.root, &just_this_file);
                    }
                }
                write_status(verdict_of(session.tree_state()));
            }
            Err(RecvTimeoutError::Timeout) => {
                // Quiet tree: heartbeat the liveness signal.
                write_status(verdict_of(session.tree_state()));
            }
            Err(RecvTimeoutError::Disconnected) => {
                // Pipeline shut down (RA unrecoverable / model dropped).
                statusfile::clear(&root);
                ui::warn("verdict pipeline stopped — exiting.");
                session.shutdown();
                return ExitCode::from(1);
            }
        }
    }
}
