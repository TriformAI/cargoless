//! `watch` (and `check --watch`) — continuous **headless** verdict stream.
//!
//! Bound exact to daemon-core's frozen contract (on `main`):
//! `cargoless_core::model::watch(&Path, IdentityProvider) -> io::Result<(
//! ModelSession, std::sync::mpsc::Receiver<StateEvent>)>`. We pass
//! `cargoless_core::model::placeholder_identity` directly — a bare
//! `fn() -> BuildIdentity` satisfying the blanket `IdentityProvider` impl;
//! cargoless never computes a `BuildIdentity` (build-cas owns the real seam).
//!
//! Headless: prints verdict transitions to stderr, NO browser/HTTP. The
//! event stream is drained with [`HEARTBEAT`](crate::statusfile::HEARTBEAT)
//! `recv_timeout` so the cargoless status file's `updated` is refreshed even
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
use crate::cratemap::{self, CrateMap};
use crate::statusfile::{self, HEARTBEAT, Status, Verdict};
use crate::ui;

const BRINGUP_BUDGET: Duration = Duration::from_secs(30);

fn verdict_of(s: cargoless_core::TreeState) -> Verdict {
    match s {
        cargoless_core::TreeState::Green => Verdict::Green,
        cargoless_core::TreeState::Red => Verdict::Red,
    }
}

/// FIELD FINDING #4-NEG-C: a relative-seconds timestamp prefix for verdict
/// stream lines. Uses the wall-clock interval since `t0` (the watch start),
/// not an absolute ISO-8601 stamp, because:
///   - the existing AC#1 line already uses this convention
///     (`verdict pipeline live in 0.08s`);
///   - users can `awk` an absolute time from the start-line + delta if they
///     want one — but the reverse (delta from an absolute stamp) requires
///     parsing dates;
///   - no timezone/locale/date-crate questions;
///   - it makes save→verdict latency directly readable from any pair of
///     adjacent lines, which is the whole point of `watch` for a
///     latency-pitched tool.
///
/// Format: `[+   0.000s]` — fixed 7-char numeric field (5 digits +
/// `.` + 3 decimals) so columns align even after 99s+ uptime,
/// reading like a stopwatch.
fn stamp(t0: Instant) -> String {
    let secs = t0.elapsed().as_secs_f64();
    // `{:7.3}` ⇒ minimum width 7, 3 decimals: ` 0.123`, `12.345`, `123.456`.
    format!("[+{secs:8.3}s] ")
}

/// FIELD FINDING #2: render every diagnostic the model currently knows
/// about, same format as `check`'s renderer (file:line:col + severity +
/// code + message + source) so a `watch` user sees a consistent shape
/// whether they re-ran `check` or kept the stream open. Stderr-only.
/// Delegates to the shared `crate::check::render_diagnostics` so a fix to
/// the format never has to be applied in two places.
fn print_diagnostics(root: &Path, diags: &[cargoless_core::Diagnostic]) {
    if diags.is_empty() {
        return;
    }
    let mut err = std::io::stderr();
    let _ = crate::check::render_diagnostics(&mut err, root, diags);
    let _ = err.flush();
}

pub fn run(cfg: &Config) -> ExitCode {
    let t0 = Instant::now();
    // §gap-3 / #89: read the single canonical identity from
    // `cargoless_core::BUILD_ID` instead of building a second "cargoless
    // {ver}" banner off CARGO_PKG_VERSION here. Before #89 this line
    // was the divergent site — `--version` said "tf-trunk {ver}" while
    // this said "cargoless {ver}", same binary two names. One source
    // now; the D1 rename is one literal in cargoless-core.
    ui::step(format!(
        "{} — watching {} ({})",
        cargoless_core::BUILD_ID,
        cfg.root.display(),
        cfg.detection.describe()
    ));

    // FIELD FINDING #13a (#93) + #128: dual-watch admission, checked
    // HERE (before the costly RA bring-up) so it is instant. Refuse
    // ONLY a genuinely-live sibling (heartbeat-fresh AND pid-alive AND
    // same-binary — the F10 composite `status` uses). A SIGKILL'd /
    // zombie / dead / pid-reused prior daemon left an orphaned
    // `cli-status`: auto-recover (take it over) with an actionable note
    // — never a bare refuse (agent fleets hard-kill daemons routinely).
    match statusfile::admission(&cfg.root, std::process::id()) {
        statusfile::WatchAdmission::Refuse(c) => {
            ui::error(c.message());
            return ExitCode::from(2);
        }
        statusfile::WatchAdmission::Recover { stale_pid } => {
            ui::warn(format!(
                "recovered: prior cli-status (pid {stale_pid}) is \
                 stale/dead — its daemon was SIGKILL'd or aged out, \
                 not running. Taking over this root."
            ));
        }
        statusfile::WatchAdmission::Proceed => {}
    }

    // `watch()` blocks on rust-analyzer's initialize handshake (seconds);
    // the first green waits on a cold cargo check (minutes) — that is fine,
    // AC#1 is "pipeline live", not "first green".
    let (session, events) = match cargoless_core::model::watch(
        &cfg.root,
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
    // Model R #9: resolve the workspace crate map ONCE at startup (a
    // dependency-free Cargo.toml scan — see `cratemap`). Empty for a
    // single-crate project or a detection miss, in which case the
    // `crates=` line is simply never written and the authoritative
    // `verdict=` line stands alone (unchanged v0 behaviour).
    let crate_map: CrateMap = CrateMap::from_workspace(&cfg.root);
    // Takes `&session` (not a bare verdict) so it can roll diagnostics up
    // per-crate. `current_diagnostics()` is the same call the red /
    // file-verdict edges already make; the list is small (errors+warnings
    // for changed files) so doing it every heartbeat is cheap.
    let write_status = |session: &cargoless_core::model::ModelSession| {
        let tree = session.tree_state();
        let verdict = verdict_of(tree);
        // Fetch the diagnostic list ONCE and reuse it for both the #9
        // per-crate roll-up and the #11 red-state retention — it is the
        // same `current_diagnostics()` call the red / file-verdict edges
        // already make; the list is small (changed-file errors+warnings)
        // so doing it every heartbeat is cheap.
        let diags = session.current_diagnostics();
        let (crates, red_diagnostics) = if crate_map.is_empty() {
            (Vec::new(), 0)
        } else {
            let pc = cratemap::aggregate(&diags, &crate_map);
            // Honesty invariant: if an error could not be attributed to a
            // known crate, a partial map would falsely read all-green —
            // omit `crates=` entirely (empty ⇒ no line); `verdict=` still
            // carries the authoritative red.
            let crates = if pc.all_errors_attributed {
                pc.verdicts
            } else {
                Vec::new()
            };
            (crates, pc.error_count)
        };
        statusfile::write(
            &root,
            &Status {
                pid: std::process::id(),
                root: root.display().to_string(),
                started,
                updated: statusfile::now_unix(),
                verdict_str: verdict.as_str().to_string(),
                crates,
                red_diagnostics,
                // #247: v0 watch path — settle ≈ write instant (the
                // model fires BecameGreen/Red and we write immediately).
                // analysed_at == updated honestly for v0; the distinction
                // is preserved for the Model R serve --repo path.
                analysed_at: statusfile::now_unix(),
            },
        );
        // Model R #11: retain the full diagnostic list for a red tree
        // (queryable via `cargoless_core::diagnostics_store::get_diagnostics`),
        // and CLEAR it on green so a stale red file never reads as a
        // false-red (the asymmetric honesty invariant — see the module).
        cargoless_core::diagnostics_store::persist(&root, tree, &diags);
    };
    write_status(&session);
    // FIELD FINDING #6-NEG-A (#51): subscribe to the lifecycle channel
    // BEFORE entering the loop so the very first transparent RA restart is
    // already observable. Drained non-blockingly inside the loop body
    // alongside the existing verdict events; mpsc Receivers aren't `Sync`,
    // so we keep them both in this single thread.
    let lifecycle = session.subscribe_lifecycle();
    // FIELD FINDING #13b (#88): snapshot the parent pid BEFORE the loop.
    // `cargoless watch &` then closing the shell would otherwise leave
    // this daemon orphaned, holding rust-analyzer + cargo + ~2GB RSS,
    // until the user `pkill`s it by hand. Polled cheaply each loop
    // iteration; on parent-death we run the same graceful shutdown the
    // Disconnected path uses.
    let parent = crate::orphan::ParentWatch::capture();
    ui::wait("Ctrl-C to stop. Streaming verdicts…");

    // Single-thread drain (Receiver is not Sync). recv_timeout = heartbeat:
    // refresh the status file on every event AND every quiet HEARTBEAT so
    // `status` sees a live daemon even with no file changes.
    //
    // FIELD FINDING #4-NEG-C: every verdict line is timestamped relative to
    // watch-start (`t0`) so a user can read save→verdict latency directly
    // from any pair of adjacent lines, and so scripted post-hoc analysis no
    // longer needs `awk` line-stamping.
    //
    // FIELD FINDING #6-NEG-A: drain the lifecycle channel on every iteration
    // (event branch AND timeout branch) so a transparent RA restart is
    // surfaced to the user inside one HEARTBEAT, even if the verdict
    // stream stays silent during the post-restart reindex window.
    loop {
        // FIELD FINDING #13b (#88): parent-death check FIRST — if the
        // shell that launched us is gone, do NOT keep running as a
        // ~2GB orphan. Same graceful shutdown as the Disconnected
        // path (session.shutdown() reaps RA via the AC#6 Supervisor +
        // #44/#61 ReapOnDrop; statusfile::clear removes the ghost so a
        // later `status` doesn't show F10-style live-for-a-dead-tree).
        // Exit 0: this is a deliberate clean shutdown, not an error
        // (and there's no parent left to read an exit code anyway).
        if parent.orphaned() {
            statusfile::clear(&root);
            ui::warn(format!(
                "{}parent process exited — shutting down so no orphaned \
                 daemon is left holding rust-analyzer (FIELD FINDING #13b).",
                stamp(t0)
            ));
            session.shutdown();
            return ExitCode::SUCCESS;
        }

        // Lifecycle drain — cheap, non-blocking, no allocations on the
        // empty path. Belongs OUTSIDE the verdict-event match because a
        // restart can fire while the verdict stream is mid-reindex-silence.
        drain_lifecycle(t0, &lifecycle);

        match events.recv_timeout(HEARTBEAT) {
            Ok(ev) => {
                let ts = stamp(t0);
                match &ev {
                    cargoless_core::StateEvent::BecameGreen { .. } => {
                        ui::ok(format!("{ts}GREEN — tree compiles"));
                    }
                    cargoless_core::StateEvent::BecameRed => {
                        ui::error(format!("{ts}RED — tree does not compile"));
                        // FIELD FINDING #2: on the red edge, print every
                        // diagnostic the model knows about so the user can
                        // act on it without re-running `check`.
                        print_diagnostics(&cfg.root, &session.current_diagnostics());
                    }
                    cargoless_core::StateEvent::FileVerdict { path, state } => {
                        ui::step(format!("{ts}{path}: {state:?}"));
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
                write_status(&session);
            }
            Err(RecvTimeoutError::Timeout) => {
                // Quiet tree: heartbeat the liveness signal.
                write_status(&session);
            }
            Err(RecvTimeoutError::Disconnected) => {
                // Pipeline shut down (RA unrecoverable / model dropped).
                statusfile::clear(&root);
                ui::warn(format!("{}verdict pipeline stopped — exiting.", stamp(t0)));
                session.shutdown();
                return ExitCode::from(1);
            }
        }
    }
}

/// FIELD FINDING #6-NEG-A (#51): drain every pending lifecycle event into
/// timestamped, user-facing lines. Runs on EVERY watch-loop iteration so
/// the worst-case latency from "RA restarted" to "user sees the message"
/// is bounded by `HEARTBEAT` (250ms) — not 30-60s of silent reindex.
fn drain_lifecycle(
    t0: Instant,
    lifecycle: &std::sync::mpsc::Receiver<cargoless_core::LifecycleEvent>,
) {
    while let Ok(ev) = lifecycle.try_recv() {
        match ev {
            cargoless_core::LifecycleEvent::AnalyzerRestarting => {
                // `ui::warn` (yellow) over `ui::step` (cyan) because this is
                // a degraded-mode signal: AC#6 transparent restart, but the
                // user is staring at a silent stream until reindex
                // completes. Color-cues the unusualness.
                ui::warn(format!(
                    "{}rust-analyzer restarted — re-indexing; next verdict when ready",
                    stamp(t0)
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_format_is_brackets_plus_seconds_fixed_width() {
        // Just-after-construction: ~0.000 seconds elapsed.
        let t0 = Instant::now();
        let s = stamp(t0);
        // `[+   0.000s] ` (8-char numeric field, 3 decimals, trailing space)
        assert!(s.starts_with("[+"), "starts with '[+': {s:?}");
        assert!(s.ends_with("s] "), "ends with 's] ': {s:?}");
        // 8-char numeric field + `+` + `s] ` framing = stable column width
        // (the whole point of fixed-width: a column-aligned stopwatch view
        //  in `grep`/`tail` output).
        assert!(s.contains("0.000"), "near-zero: {s:?}");
        assert_eq!(s.len(), "[+   0.000s] ".len());
    }

    #[test]
    fn stamp_monotonic_increases_with_elapsed() {
        let t0 = Instant::now();
        let a = stamp(t0);
        std::thread::sleep(Duration::from_millis(15));
        let b = stamp(t0);
        // Lexicographic compare is enough — both are zero-padded fixed-width.
        assert!(b > a, "later stamp must sort after earlier: {a:?} vs {b:?}");
    }
}
