//! `serve` — the headline command and the **AC#1** proof.
//!
//! Decision **D-A1**: zero config, `serve` brings the daemon up + auto-detects
//! the project + serves a holding page within 30s (NOT a finished app — a
//! cold Leptos build is minutes).
//!
//! ## Two paths, one contract (the `integration` feature)
//!
//! * default (feature off) — the standalone bring-up cli-ux fully owns and
//!   ships green today: own std holding server ([`crate::holding`]) +
//!   RA supervisor + filesystem watcher. AC#1 satisfied by cli-ux directly.
//! * `--features integration` — the **single-server** design the lead
//!   decided: there is *no* holding→server handoff. `tf_core::server::
//!   DevServer` owns the port and serves its *own* cold-start holding page
//!   (with the reload shim) until first-green, then swaps to the artifact.
//!   cli-ux runs NO second server. We construct the DevServer, `spawn` it
//!   (binds synchronously — that is the AC#1 moment), and drive it from
//!   daemon-core's model `StateEvent` stream.
//!
//! Wired against devserver's frozen surface (`agent/devserver` @ 6d4b5f8 /
//! `agent/devserver-bundle` @ a8d063b): `DevServer::new(Arc<S: ContentStore>)`,
//! `spawn(self, SocketAddr) -> io::Result<ServerHandle>`, `ServerHandle::
//! {local_addr,notify_state,notify_build,has_green,tree_is_red,shutdown}`.
//!
//! The model drive (`tf_core::model::watch` → `handle.notify_state`) is the
//! one piece still pending daemon-core's `IdentityProvider` signature
//! clarification; until that lands the integration path stands the DevServer
//! up (AC#1 holds — it serves its own holding page) and parks, rather than
//! guessing a signature. That fill is localized to one block.

use std::process::ExitCode;
use std::time::{Duration, Instant};

#[cfg(not(feature = "integration"))]
use tf_core::watcher;

use crate::config::Config;
#[cfg(not(feature = "integration"))]
use crate::holding::{HoldingServer, Phase};
use crate::status::{self, DaemonStatus};
use crate::ui;

/// AC#1 budget. We never approach this (bind + watcher/spawn are
/// milliseconds), but asserting it in the bring-up makes a future regression
/// that *does* (e.g. a blocking RA probe) loud instead of silent.
const BRINGUP_BUDGET: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Default path (feature off) — standalone bring-up, ships green.
// ---------------------------------------------------------------------------

#[cfg(not(feature = "integration"))]
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
                 rust-analyzer`.",
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

    report_bringup(t0, &url);
    server.set_phase(Phase::Building);
    ui::wait("Ctrl-C to stop. Watching for changes…");

    // The change loop. SEAM: the integration path replaces this whole
    // standalone bring-up with the DevServer + model stream.
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
                status::clear_status(&cfg.root);
                ui::warn("watcher stopped — exiting.");
                return ExitCode::from(1);
            }
        }
    }
}

#[cfg(not(feature = "integration"))]
fn report_bringup(t0: Instant, url: &str) {
    let bringup = t0.elapsed();
    if bringup <= BRINGUP_BUDGET {
        ui::ok(format!(
            "daemon up in {:.2}s (AC#1 budget {}s) — serving {url}",
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
}

// ---------------------------------------------------------------------------
// Integration path (feature on) — single-server: DevServer owns the port.
// ---------------------------------------------------------------------------

#[cfg(feature = "integration")]
pub fn run(cfg: &Config) -> ExitCode {
    use std::net::ToSocketAddrs;
    use std::sync::Arc;

    let t0 = Instant::now();
    ui::step(format!(
        "cargoless {} — {}",
        env!("CARGO_PKG_VERSION"),
        cfg.detection.describe()
    ));

    // Single-server (lead's decision): DevServer IS the holding page. The CAS
    // store is the production `ArtifactProvider` path; `tf_core::LocalDiskStore`
    // satisfies `DevServer::new`'s `S: ContentStore + Send + Sync + 'static`.
    let store = Arc::new(tf_core::LocalDiskStore::new(cfg.cache_dir.clone()));
    let dev = tf_core::server::DevServer::new(store);

    let Some(addr) = (cfg.host.as_str(), cfg.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
    else {
        ui::error(format!(
            "could not resolve bind address {}:{} — set `[serve] host`/`port` \
             in tf.toml to a literal address.",
            cfg.host, cfg.port
        ));
        return ExitCode::from(2);
    };

    // `spawn` binds the TcpListener synchronously: on Ok it is already
    // listening (this is the AC#1 "up + holding page" moment — DevServer
    // serves its own cold-start page with the reload shim).
    let handle = match dev.spawn(addr) {
        Ok(h) => h,
        Err(e) => {
            ui::error(format!(
                "could not bind http://{}:{} ({e}). Port in use (another \
                 cargoless / dev server)? Set `[serve] port` in tf.toml.",
                cfg.host, cfg.port
            ));
            return ExitCode::from(1);
        }
    };
    let bound = handle.local_addr();
    ui::ok(format!("holding page live at http://{bound}"));

    status::write_status(
        &cfg.root,
        &DaemonStatus {
            pid: std::process::id(),
            host: cfg.host.clone(),
            port: bound.port(),
            detection: cfg.detection.describe().to_string(),
            latest_green: None,
        },
    );

    let bringup = t0.elapsed();
    if bringup <= BRINGUP_BUDGET {
        ui::ok(format!(
            "daemon up in {:.2}s (AC#1 budget {}s) — serving http://{bound}",
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

    // MODEL DRIVE — the one block pending daemon-core's `IdentityProvider`
    // signature (asked; not guessing). When confirmed, this becomes:
    //
    //   let (session, events) =
    //       tf_core::model::watch(&cfg.root, <IdentityProvider per daemon-core>)?;
    //   for ev in events {                       // std::sync::mpsc::Receiver
    //       handle.notify_state(&ev);            // AC#4: Red only flips status
    //       // first BecameGreen ⇒ DevServer auto-reloads to the artifact;
    //       // status line + status-file latest_green updated from `ev`
    //       //   / session.tree_state()
    //   }
    //
    // DevServer already satisfies AC#1 (its own holding page is live above),
    // so the honest interim is to keep it serving and park — not to fabricate
    // an unconfirmed `watch` call. `handle` stays in scope (Drop = clean stop).
    ui::step(format!(
        "DevServer state: has_green={}, tree_is_red={} (cold start)",
        handle.has_green(),
        handle.tree_is_red()
    ));
    ui::wait(
        "DevServer up and serving its holding page. Model-drive wiring is \
         pending daemon-core's IdentityProvider signature — Ctrl-C to stop.",
    );
    loop {
        std::thread::sleep(Duration::from_secs(3600));
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
