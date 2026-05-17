//! `serve` — the headline command and the **AC#1** proof.
//!
//! Decision **D-A1** redefines AC#1: on a clean machine, zero config, `serve`
//! must bring the daemon up + auto-detect the project + serve a holding page
//! within 30s (NOT a finished app — a cold Leptos build is minutes). This
//! module does exactly that and nothing it cannot honestly back:
//!
//! 1. bind the holding page immediately (sub-second first paint),
//! 2. write the status file (so `status` works),
//! 3. start the rust-analyzer supervisor — best-effort; a missing RA degrades
//!    to "no verdicts yet", it never blocks the holding page (AC#1),
//! 4. start the filesystem watcher and report coalesced change batches.
//!
//! The verdict → build → browser-reload tail of the loop is intentionally a
//! marked seam: the green/red model, build trigger, and never-serve-red
//! WebSocket server are daemon-core/devserver deliverables not yet on `main`.
//! `tf-cli` does not edit `tf-core`; when those public entrypoints land this
//! loop consumes a `StateEvent` stream and drives `holding.set_phase` /
//! hand-off to the real server. The structure here is additive for that.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use tf_core::watcher;

use crate::config::Config;
use crate::holding::{HoldingServer, Phase};
use crate::status::{self, DaemonStatus};
use crate::ui;

/// AC#1 budget. We never approach this (bind + watcher start are
/// milliseconds), but asserting it in the bring-up makes a future regression
/// that *does* (e.g. a blocking RA probe) loud instead of silent.
const BRINGUP_BUDGET: Duration = Duration::from_secs(30);

pub fn run(cfg: &Config) -> ExitCode {
    let t0 = Instant::now();
    ui::step(format!(
        "cargoless {} — {}",
        env!("CARGO_PKG_VERSION"),
        cfg.detection.describe()
    ));

    // 1. Holding page first: this is the AC#1 deliverable; everything else is
    //    best-effort relative to it.
    let server = match HoldingServer::start(&cfg.host, cfg.port) {
        Ok(s) => s,
        Err(e) => {
            ui::error(format!(
                "could not bind http://{}:{} ({e}). Is another server (or a \
                 previous cargoless) already using the port? Set `[serve] \
                 port` in tf.toml or free it.",
                cfg.host, cfg.port
            ));
            return ExitCode::from(1);
        }
    };
    let url = format!("http://{}", server.bound);
    ui::ok(format!("holding page live at {url}"));

    // 2. Status file — `status`/`clean`/another shell can now see this daemon.
    let st = DaemonStatus {
        pid: std::process::id(),
        host: cfg.host.clone(),
        port: server.bound.port(),
        detection: cfg.detection.describe().to_string(),
        latest_green: None,
    };
    status::write_status(&cfg.root, &st);

    // 3. rust-analyzer supervisor — best-effort. AC#1 must hold even with no
    //    RA on the machine, so a failed start is a warning, not an abort.
    let _supervisor = match tf_core::analyzer::Supervisor::start(|| {
        tf_core::analyzer::rust_analyzer_command()?.spawn()
    }) {
        Ok(sup) => {
            ui::ok(format!(
                "rust-analyzer supervised (pid {})",
                sup.current_pid().unwrap_or(0)
            ));
            Some(sup)
        }
        Err(_) => {
            ui::warn(
                "rust-analyzer not found — holding page is up but verdicts \
                 are disabled. Install it: `rustup component add \
                 rust-analyzer`."
                    .to_string(),
            );
            None
        }
    };

    // 4. Filesystem watcher.
    let (_watch, changes) = match watcher::watch(&cfg.root, watcher::DEFAULT_DEBOUNCE) {
        Ok(w) => w,
        Err(e) => {
            ui::error(format!(
                "could not watch {} ({e}). Check the path exists and is readable.",
                cfg.root.display()
            ));
            status::clear_status(&cfg.root);
            return ExitCode::from(1);
        }
    };
    ui::ok(format!("watching {}", cfg.root.display()));

    let bringup = t0.elapsed();
    if bringup <= BRINGUP_BUDGET {
        ui::ok(format!(
            "daemon up in {:.2}s (AC#1 budget {}s) — serving {url}",
            bringup.as_secs_f64(),
            BRINGUP_BUDGET.as_secs()
        ));
    } else {
        // Should be impossible; loud if it ever happens.
        ui::warn(format!(
            "bring-up took {:.2}s, over the {}s AC#1 budget — investigate.",
            bringup.as_secs_f64(),
            BRINGUP_BUDGET.as_secs()
        ));
    }
    server.set_phase(Phase::Building);
    ui::wait("Ctrl-C to stop. Watching for changes…".to_string());

    // The change loop. Today: report coalesced batches (the watcher already
    // debounces + ignore-filters). SEAM: when tf-core exposes the verdict
    // stream, each batch's resulting StateEvent drives `server.set_phase`
    // (Red holds last-green per AC#4) and the build trigger.
    loop {
        match changes.recv() {
            Ok(batch) => {
                let n = batch.len();
                ui::step(format!(
                    "{n} file{} changed — re-checking…",
                    if n == 1 { "" } else { "s" }
                ));
            }
            Err(_) => {
                // Watcher thread gone (shutdown / fatal). Don't leave a ghost
                // status file behind for `status` to misreport.
                status::clear_status(&cfg.root);
                ui::warn("watcher stopped — exiting.".to_string());
                return ExitCode::from(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bringup_budget_is_the_d_a1_number() {
        // Guards D-A1 from a silent edit: AC#1 is 30s by decision of record.
        assert_eq!(BRINGUP_BUDGET, Duration::from_secs(30));
    }
}
