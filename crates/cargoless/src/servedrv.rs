//! capstone-wire — the live repo-scoped serve-loop driver (Model R
//! Stream B+C tie-point, the cycle's final unit). Replaces serve.rs's
//! honest park-skeleton with the real multiplexed verdict pipeline by
//! **faithfully composing the proven cores**:
//!
//! * cluster assignment — [`clustermgr::read_workspace_config`] +
//!   [`cluster`] (pure, #158-backstop-CLEAR'd) ;
//! * per-cluster one shared RA — [`analyzer::Supervisor`] +
//!   [`lsp::LspClient`], lifecycle driven by
//!   [`clustermgr::ClusterLifecycle`] (proven 0→1 / 1→0 edges) ;
//! * routed file-changes — [`repo::watch::RepoWatchRouter`] (proven §4
//!   gitignore-inversion + per-WT debounce) fed by a RAW unfiltered
//!   `notify` watcher ;
//! * the per-cluster transaction sequencer — [`clusterdrv::ClusterDriver`]
//!   (Judgments A+B structural @capstone-core) ;
//! * overlay multiplexing — [`multiplex::OverlayMultiplexer`] (spatial
//!   isolation; respawn-seam closed by `reset()`) ;
//! * activity lifecycle — [`activitymgr::ActivityTracker`] (proven
//!   per-WT, no-wrong-verdict).
//!
//! ## Scoped-faithfulness surface (this is composition, NOT new pure
//! ## correctness — A/B stay structural in `clusterdrv`)
//!
//! * **(i)** feeds `ClusterDriver` its `DriverEvent`s per contract
//!   (RoutedBatch on a settled routed batch; Lsp on every forwarded RA
//!   event; Deactivated on an activity-tracker deactivation edge).
//! * **(ii)** executes each `ClusterAction` faithfully — `SwitchOverlay`
//!   → `OverlayMultiplexer::switch_to` → `LspClient` verbs + `did_save`
//!   (flycheck trigger); `EmitVerdict` → per-WT statusfile verdict.
//! * **(iii)** composition is non-vacuous (real RA, real notify).
//! * **(iv) A-as-composed:** `clusters` is a `BTreeMap<hash,
//!   ClusterState>`; a `ClusterState` (hence its single `ClusterDriver`)
//!   is constructed at EXACTLY ONE site — the `LifecycleAction::SpawnRa`
//!   arm — and `ClusterLifecycle` proves `SpawnRa` fires only on the
//!   0→1 edge ⇒ ≤1 `ClusterDriver` per cluster BY CONSTRUCTION; every
//!   `ClusterDriver` is mutated only from the single serve loop ⇒ no 2nd
//!   concurrent per-cluster transaction is representable.
//! * **(iv) B-as-composed:** a verdict is written at EXACTLY ONE site —
//!   the `ClusterAction::EmitVerdict` match arm. No wire path reads a
//!   barrier or attributes a verdict elsewhere (the barrier is private
//!   to `clusterdrv`; the wire only ever sees `ClusterAction`) ⇒
//!   pre-settle attribution stays unrepresentable through the
//!   composition.
//! * **(v) respawn-staleness closure:** the cluster's
//!   `OverlayMultiplexer::reset()` is called at EXACTLY ONE site — the
//!   `Spawned` control-message handler, which is the sole place a
//!   cluster's `LspClient` is (re)set — BEFORE any subsequent
//!   `switch_to` for that cluster. (Placement note / flag-at-land: this
//!   is the loop-side spawned-handler rather than literally inside the
//!   Supervisor `on_spawn` closure — same structural guarantee "reset
//!   before any post-(re)spawn switch_to", chosen so the multiplexer
//!   stays single-owner in the serve loop and is never shared across
//!   the supervisor thread; the load-bearing property is identical.)
//!
//! ## Honest verification boundary (stated, not papered over)
//!
//! Verdict-correctness is **structurally proven in the cores**
//! (`clusterdrv` A+B, `barrier` temporal, `multiplex` spatial incl.
//! `reset()`, `cluster`/`clustermgr`/`activitymgr`/`repo::watch`). This
//! module is faithful composition; its **live multiplexed runtime**
//! (real rust-analyzer processes, per-cluster forwarder-thread
//! scheduling, deadlock-freedom, LSP-handshake timing) is **integration-
//! validated-downstream** via #15-bench (measured numbers on the real
//! wired daemon) + Track-1 dogfood (operator tf-mv) — a CLOSED
//! validation chain: cores-structurally-proven + integration-CLOSED via
//! #15/Track-1. It is **never** "fully pure-unit-proven end-to-end". The
//! authoritative v2-gate covers build/clippy/fmt/integ here (it does
//! catch compile/borrow/clippy/contract-shape); runtime is the
//! downstream half of the closed chain.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use cargoless_core::activity::ActivityConfig;
use cargoless_core::activitymgr::ActivityTracker;
use cargoless_core::analyzer::{Supervisor, rust_analyzer_command};
use cargoless_core::cluster::{WorkspaceConfig, WorkspaceConfigHash};
use cargoless_core::clusterdrv::{ClusterAction, ClusterDriver, DriverEvent, VerdictPolicy};
use cargoless_core::clustermgr::{ClusterLifecycle, LifecycleAction, read_workspace_config};
use cargoless_core::lsp::{InitOpts, LspClient, LspEvent};
use cargoless_core::multiplex::LspVerb;
use cargoless_core::multiplex::OverlayMultiplexer;
use cargoless_core::overlay::OverlaySet;
use cargoless_core::repo::RepoScope;
use cargoless_core::repo::watch::{RepoWatchRouter, WtId, WtRouter};

use crate::orphan::ParentWatch;
use crate::statusfile::{self, Status, Verdict};

/// v0 debounce quiet-window for the per-WT routed batches (the same
/// order as the v0 single-watch default; runtime-tunable later).
const QUIET: Duration = Duration::from_millis(200);

/// One cluster's live state. Constructed at exactly one site (the
/// `SpawnRa` arm); the `ClusterDriver`/`OverlayMultiplexer` are mutated
/// only from the single serve loop (Judgment A as composed).
struct ClusterState {
    /// RAII: dropping the supervisor kills + reaps the RA (TeardownRa).
    _supervisor: Supervisor,
    /// This cluster's hash and event sink, retained so background cargo
    /// checks can feed their completion back through the same driver.
    cluster: WorkspaceConfigHash,
    lsp_tx: Sender<(WorkspaceConfigHash, LspEvent)>,
    /// The currently-live RA's client; `None` until the first
    /// `Spawned` message lands, swapped on every (re)spawn.
    lsp: Option<Arc<LspClient>>,
    /// Spatial-isolation multiplexer; `reset()` on every (re)spawn.
    mux: OverlayMultiplexer,
    /// The per-cluster transaction sequencer (Judgments A+B structural).
    driver: ClusterDriver,
    /// Monotonic LSP document version for `did_change`.
    next_ver: i64,
    /// True once the current RA instance has finished its initial roots scan.
    /// First batches are deferred until this flips so save/flycheck is not
    /// lost during RA workspace bootstrap.
    ready: bool,
    /// Worktrees with a routed batch that arrived before the current RA
    /// instance reached project-ready.
    deferred: VecDeque<WtId>,
}

fn truthy_env(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| {
            let v = v.trim();
            v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("yes")
                || v.eq_ignore_ascii_case("on")
        })
        .unwrap_or(false)
}

fn push_only_mode() -> bool {
    truthy_env("CARGOLESS_PUSH_ONLY") || truthy_env("TF_FS_WATCH_DISABLED")
}

fn ra_native_verdict_mode() -> bool {
    // Product invariant: Cargoless replaces iterative cargo check/clippy; it
    // does not offer a hidden mode that executes them from daemon verdict
    // requests. Keep the helper so existing mode-plumbing stays simple, but
    // make the answer unconditional.
    true
}

fn ready_after_respawn_for_modes(push_only: bool, ra_native: bool) -> bool {
    push_only && ra_native
}

fn ra_native_settle_delay() -> Duration {
    std::env::var("CARGOLESS_RA_SETTLE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(2_000))
}

/// Control messages from the per-cluster Supervisor `on_spawn` hook to
/// the serve loop.
enum Ctrl {
    /// A cluster's RA (re)spawned and its LSP handshake completed.
    Spawned(WorkspaceConfigHash, Arc<LspClient>),
}

/// Process-global SIGTERM/SIGINT stop flag — the serve loop polls it
/// each iteration. Always present (the loop reads it on every target);
/// set ONLY by [`on_term`], whose entire body is one atomic store
/// (async-signal-safe — no allocation / locking / reentrancy).
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// SIGTERM/SIGINT handler: flip the stop flag, nothing else (the
/// async-signal-safety contract — an atomic store is on the SS-safe
/// list; a handler must not allocate, lock, or do I/O).
#[cfg(unix)]
extern "C" fn on_term(_sig: core::ffi::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Install SIGTERM + SIGINT → [`SHUTDOWN`]. std-only, NO `signal`/`libc`
/// crate (house dependency-minimal) — the SAME `unsafe extern "C"`
/// libc-symbol idiom `analyzer.rs` already uses for `setsid()` in
/// `pre_exec`. This is FIELD FINDING A / #198's structural restore: the
/// proven `analyzer::Supervisor` reap (`do_shutdown` = monitor-join +
/// `kill()`+`wait()`, plus the `process_group(0)`+`setsid` pgid
/// discipline #3b/#44/#61/#128) only runs on normal scope-unwind; a
/// default-disposition SIGTERM bypasses unwind entirely, so we route the
/// signal to a polled flag ⇒ the loop returns normally ⇒ the proven
/// reap actually executes AT THE SEAM. Non-unix: no-op (the fleet
/// restart-churn seam is POSIX `kill -TERM`; supported targets are all
/// unix per D-RELEASE §3).
#[cfg(unix)]
fn install_signal_stops() {
    // POSIX-stable on the only supported OS families (linux-gnu +
    // apple-darwin): SIGINT = 2, SIGTERM = 15.
    const SIGINT: core::ffi::c_int = 2;
    const SIGTERM: core::ffi::c_int = 15;
    unsafe extern "C" {
        // signal(2): we need only "flip a flag on delivery", not the
        // full sigaction surface. Return (previous handler, pointer-
        // width) is intentionally discarded — never called through.
        fn signal(signum: core::ffi::c_int, handler: extern "C" fn(core::ffi::c_int)) -> usize;
    }
    // SAFETY: the registered handler's whole body is a single atomic
    // store (async-signal-safe). Same unsafe-extern-libc-symbol house
    // pattern as analyzer.rs's pre_exec `setsid()`.
    unsafe {
        let _ = signal(SIGTERM, on_term);
        let _ = signal(SIGINT, on_term);
    }
}

#[cfg(not(unix))]
fn install_signal_stops() {}

/// Run the live repo-scoped Model R daemon loop. Replaces serve.rs's
/// park. Exits on SIGTERM/SIGINT (the operator / fleet restart-churn
/// path), parent-orphan (#13b parity), or watcher-disconnect — and on
/// EVERY exit explicitly reaps every per-cluster rust-analyzer child via
/// the proven `analyzer::Supervisor` teardown.
///
/// FIELD FINDING A / #198: the prior "OS-default signal handling …
/// Supervisors drop ⇒ RAs reaped" claim was FALSE — a
/// default-disposition SIGTERM terminates the process WITHOUT
/// unwinding, so the implicit `clusters` drop (and thus the proven
/// reap) never ran ⇒ zombie/orphan rust-analyzer under fleet
/// restart-churn. Fixed by routing the signal to a polled flag + an
/// explicit single-funnel reap at the seam (see the loop's exit block).
pub fn run(scope: RepoScope, parent: &ParentWatch) -> ExitCode {
    // ---- cluster assignment (pure, gated cores) ----------------------
    // Each discovered worktree → its WorkspaceConfig → cluster hash.
    // `cluster_root` keeps a representative root per cluster (the RA
    // workspace). An unreadable config (Err) ⇒ split-safe: that WT is
    // skipped from clustering for v0 (its own cluster is a v0.1 refinement
    // — never under-cluster, the bias-to-split contract).
    let mut wt_hash: BTreeMap<WtId, WorkspaceConfigHash> = BTreeMap::new();
    let mut cluster_root: BTreeMap<WorkspaceConfigHash, PathBuf> = BTreeMap::new();
    for wt in &scope.worktrees {
        let cfg = match read_workspace_config(&wt.path) {
            Ok(c) => c,
            Err(_) => continue, // split-safe skip (v0)
        };
        let h = cfg.hash();
        cluster_root
            .entry(h.clone())
            .or_insert_with(|| wt.path.clone());
        wt_hash.insert(wt.path.clone(), h);
    }

    // ---- routed watcher (proven RepoWatchRouter + RAW notify) --------
    let router = WtRouter::new(scope.worktrees.iter());
    let mut repo_watch = RepoWatchRouter::new(router, QUIET);
    let mut activity = ActivityTracker::new(ActivityConfig::defaults());

    // RAW repo-scoped watcher via the cargoless-core surface (incr-1):
    // NO ignore-filter — the §4 inversion (a filtered watcher would
    // blind us to gitignored worktree subtrees; RepoWatchRouter owns
    // routing + the universal target/.git floor + per-WT debounce, all
    // proven). `notify` is cargoless-core's owned dep; the binary stays
    // cargoless-core-only. `_watch_handle` is held for the whole loop —
    // its RAII drop (fn-scope end) stops the OS watch.
    let repo_root = scope.repo_root.clone();
    let (_watch_handle, raw_rx) = match cargoless_core::repo::watch::raw_repo_watch(&repo_root) {
        Ok(pair) => pair,
        Err(e) => {
            crate::ui::error(format!("watch {}: {e}", repo_root.display()));
            return ExitCode::from(2);
        }
    };

    // ---- per-cluster RA event + control channels ---------------------
    let (lsp_tx, lsp_rx) = channel::<(WorkspaceConfigHash, LspEvent)>();
    let (ctrl_tx, ctrl_rx) = channel::<Ctrl>();

    let mut clusters: BTreeMap<WorkspaceConfigHash, ClusterState> = BTreeMap::new();
    let mut lifecycle = ClusterLifecycle::new();
    // The settled routed batch's file list, awaiting its SwitchOverlay.
    let mut pending_batch: BTreeMap<WtId, Vec<PathBuf>> = BTreeMap::new();

    // ---- Increment 0: read-plane VerdictService + HTTP transport -----
    // The serve-loop's live per-WT verdict state, presented as the
    // shipped logical `VerdictService` (transport #10). Fed from the SOLE
    // verdict site (`publish_verdict`) — a faithful MIRROR of the
    // authoritative write-plane, never a second verdict-attribution path
    // (Judgment B as composed; the #189/#198 story is intact). `serve.rs`
    // already resolved `--bind`/`--auth-token` into the FleetConfig and
    // ran `security_check`; THIS is #10's actual binding (the serve.rs
    // module-doc "Stream E #10 binds it; #3 only resolves+carries" seam).
    let api = Arc::new(crate::serveapi::ServeVerdictState::new());

    // #240/2b — overlay-push ingest signal channel. Wired BEFORE
    // `HttpServer::bind` so no `POST /overlay` from a client can race
    // the channel-not-yet-attached window (api.push_overlay would store
    // the overlay but the wakeup would be silently dropped, leaving the
    // push unservicable until activity tick). Pre-binding eliminates
    // that race by construction.
    let (push_tx, push_rx) = channel::<String>();
    api.attach_push_signal(push_tx);

    // /healthz readiness latch (#225 0d). `false` until the serve loop is
    // live ⇒ unauthenticated `GET /healthz` answers `503
    // {"status":"starting"}`; flipped `true` at loop-entry below ⇒ `200
    // {"status":"ready"}`. Honest boundary: `RepoScope::discover` already
    // completed in serve.rs *before* servedrv::run, so the meaningful
    // daemon-ready boundary servedrv owns is "serve loop entered" (a bound
    // listener alone only proves liveness — the k8s probe needs "actually
    // serving"). One-way monotonic latch ⇒ `Relaxed` is sufficient and
    // matches the adapter's `ready.load(Relaxed)`.
    let ready = Arc::new(AtomicBool::new(false));
    let http_server = match scope.fleet.bind {
        Some(addr) => {
            // #14 policy seam, fail-closed. Re-runs `security_check`:
            // serve.rs already refused a non-loopback-no-token bind before
            // discover, so this is defense-in-depth, not a new gate — and
            // the contract is "surface the typed config error, never a
            // silent AllowAll on a public socket".
            let auth = match cargoless_core::transport::authorizer_for(&scope.fleet) {
                Ok(a) => a,
                Err(e) => {
                    crate::ui::error(format!("refusing to bind transport: {e}"));
                    return ExitCode::from(2);
                }
            };
            match cargoless_core::transport::http::HttpServer::bind_with_health(
                &addr.to_string(),
                Arc::clone(&api) as Arc<dyn cargoless_core::transport::VerdictService>,
                auth,
                Arc::clone(&ready),
            ) {
                Ok(s) => {
                    crate::ui::ok(format!("HTTP transport bound on http://{}", s.addr()));
                    Some(s)
                }
                Err(e) => {
                    crate::ui::error(format!("HTTP transport bind {addr}: {e}"));
                    return ExitCode::from(2);
                }
            }
        }
        // No `--bind` ⇒ the #10 default (loopback / in-proc / Unix): the
        // HTTP adapter is simply inactive. `api` is still fed so an
        // in-proc / Unix reader could consume the same live state.
        None => None,
    };

    // FIELD FINDING A / #198: arm SIGTERM/SIGINT → graceful loop-exit
    // BEFORE announcing "up", so a fleet `kill -TERM` during/just-after
    // bring-up still routes through the proven per-cluster RA reap.
    install_signal_stops();
    crate::ui::wait("repo-scoped Model R daemon up. Ctrl-C / SIGTERM to stop.");
    let push_only = push_only_mode();
    if push_only {
        crate::ui::wait(
            "push-only mode enabled — filesystem watch batches are suppressed; \
             remote push requests drive verdicts.",
        );
    }
    // #225 0d: the daemon's serve loop is now live → flip the /healthz
    // readiness latch (503 {"status":"starting"} → 200 {"status":"ready"}).
    // This is the ONE meaningful readiness transition the k8s probe needs;
    // it is harmless (a no-op observer) when `--bind` is absent.
    ready.store(true, Ordering::Relaxed);
    let mut last_status_heartbeat = Instant::now()
        .checked_sub(statusfile::HEARTBEAT)
        .unwrap_or_else(Instant::now);
    heartbeat_repo_status(&repo_root);
    let mut quiesce_announced = false;

    // ---- the serve loop (single owner ⇒ Judgment A holds composed) ---
    loop {
        if last_status_heartbeat.elapsed() >= statusfile::HEARTBEAT {
            heartbeat_repo_status(&repo_root);
            last_status_heartbeat = Instant::now();
        }
        if SHUTDOWN.load(Ordering::SeqCst) {
            crate::ui::warn(
                "SIGTERM/SIGINT received — draining: reaping per-cluster \
                 rust-analyzer children (FIELD FINDING A / #198).",
            );
            break;
        }
        if parent.orphaned() {
            crate::ui::warn("parent process exited — shutting down (FIELD FINDING #13b parity).");
            break;
        }
        if api.quiescing() {
            if !quiesce_announced {
                crate::ui::warn(
                    "quiesce requested — refusing new pushes and draining accepted worktrees.",
                );
                quiesce_announced = true;
            }
            if api.drain_complete() {
                crate::ui::warn("quiesce drain complete — exiting cleanly for restart.");
                break;
            }
        }

        // (v) respawn-staleness closure: the SOLE site a cluster's
        // LspClient is (re)set — restore BOTH proven cores' preconditions
        // here, before any subsequent switch_to / barrier observation for
        // that cluster.
        //
        // #247 STOP-class AC4 fix: kill-mid-flycheck leaves
        // `cs.driver: ClusterDriver` carrying an `ActiveTxn` whose
        // flycheck barrier is `Waiting` for a `FlycheckEnded` from a
        // rust-analyzer process that's no longer alive. Without
        // `driver.reset_after_respawn()`, the new RA's initial cargo
        // check (which never received `SwitchOverlay`-pushed overlays
        // for the in-flight WT — those only re-fire from a *new*
        // RoutedBatch) emits FlycheckEnded → settles the stale barrier →
        // `EmitVerdict{wt, authoritative_error=false}` from a window
        // that contains zero diagnostics about that WT's overlay ⇒
        // **FALSE GREEN attributed to a WT whose source is broken.**
        // dev-fixer source-traced (045d6dc) + clusterdrv test
        // `reset_after_respawn_drops_in_flight_txn_no_emit_without_fresh_routed_batch`
        // proves the structural restore.
        //
        // The [[proven-core-precondition-violated-at-integration-seam]]
        // pattern recurring on a 2nd axis (mirrors #190's mux.reset and
        // #198's RA reap — restore the precondition AT the wire seam,
        // never weaken the proven core). ORDER: driver.reset_after_respawn
        // BEFORE swapping in the new LspClient, so any LspEvents drained
        // next iteration from the new RA cannot interleave with the dead
        // state.
        drain_spawned(&mut clusters, &ctrl_rx);

        // #240/2b — overlay-push ingest drain. The PushOverlay write-plane
        // wakeup signal: every `api.push_overlay(...)` call sends the
        // worktree key here. We synthesize a `DriverEvent::RoutedBatch`
        // for the WT — IDENTICAL event shape to the watcher path — so
        // `clusterdrv` / `multiplex` see no difference (pushed-vs-FS is
        // a SOURCE mode, not a wire mode; the proven cores stay
        // byte-untouched). On first push for a never-seen WT, we
        // register it: derive the cluster hash from the server-side base
        // checkout, then apply any pushed workspace-config overrides
        // (Cargo.toml / Cargo.lock / rust-toolchain / .cargo/config).
        // Best-effort: an unreadable base config falls back to the pushed
        // overrides only, preserving split-safe routing without forcing the
        // client to resend unchanged config bodies on every push.
        while let Ok(wt_key) = push_rx.try_recv() {
            let wt: WtId = PathBuf::from(&wt_key);
            // Register on first push (tap 1 substitute) + derive cluster
            // hash from pushed content (tap 2 substitute). On subsequent
            // pushes for the same WT, this is a no-op (entry::or_insert).
            if !wt_hash.contains_key(&wt) {
                let h = cluster_hash_from_pushed(&api, &wt_key);
                let root = api.analysis_root_for(&wt_key).unwrap_or_else(|| wt.clone());
                cluster_root.entry(h.clone()).or_insert(root);
                wt_hash.insert(wt.clone(), h);
            }
            // The cluster hash for this WT (always present after the
            // registration above).
            let Some(h) = wt_hash.get(&wt).cloned() else {
                continue; // unreachable; defensive
            };
            activity.touch(wt.clone(), Instant::now());
            // Ensure the cluster's RA exists (proven 0→1 SpawnRa) — same
            // as the FS path's gate.
            if let LifecycleAction::SpawnRa(_) = lifecycle.activate(path_key(&wt), h.clone()) {
                spawn_cluster(
                    &mut clusters,
                    &h,
                    cluster_root.get(&h).cloned().unwrap_or_else(|| wt.clone()),
                    lsp_tx.clone(),
                    ctrl_tx.clone(),
                );
                // `spawn_cluster` runs the initial LSP handshake inside
                // the Supervisor hook and queues `Ctrl::Spawned` before
                // returning. Drain it now so the first pushed batch does
                // not switch while `cs.lsp` is still None, then get reset
                // by the next loop's spawn drain.
                drain_spawned(&mut clusters, &ctrl_rx);
            }
            // Feed the SAME DriverEvent::RoutedBatch the watcher path
            // feeds — clusterdrv sees no difference. The SwitchOverlay
            // arm's source pick (FS-read vs api.take_overlay_for) is
            // where the pushed/FS divergence actually lives (one line).
            route_or_defer(&mut clusters, &h, wt.clone(), &pending_batch, &api);
        }

        // Drain forwarded RA events → the owning cluster's ClusterDriver.
        while let Ok((h, ev)) = lsp_rx.try_recv() {
            if clusters.contains_key(&h) {
                let ev = early_red_event(ev);
                let indexing_ended = matches!(ev, LspEvent::IndexingEnded);
                step(
                    &mut clusters,
                    &h,
                    DriverEvent::Lsp(ev),
                    &pending_batch,
                    &api,
                );
                if indexing_ended {
                    let deferred = mark_ready_and_take_deferred(&mut clusters, &h);
                    for wt in deferred {
                        step(
                            &mut clusters,
                            &h,
                            DriverEvent::RoutedBatch { wt },
                            &pending_batch,
                            &api,
                        );
                    }
                }
            }
        }

        if push_only {
            // tf-multiverse check-remote replacement mode. The live repo
            // can have hundreds of independently edited worktrees; those
            // filesystem edits must not start background watch transactions
            // in the same daemon that is serving pushed RA-native
            // check/clippy replacement verdicts.
            // Drain the OS watcher so its channel cannot grow unbounded,
            // but never turn those events into DriverEvent::RoutedBatch.
            match raw_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(_) => while raw_rx.try_recv().is_ok() {},
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            // Routed file-changes: raw_repo_watch yields changed absolute
            // paths (Receiver<PathBuf>); feed them straight into the
            // proven RepoWatchRouter (it owns §4 routing + target/.git
            // floor + per-WT debounce). Drain any burst non-blocking
            // after the first so a save-storm coalesces into one
            // debounced batch.
            let now = Instant::now();
            match raw_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(path) => {
                    repo_watch.record(&path, now);
                    while let Ok(p) = raw_rx.try_recv() {
                        repo_watch.record(&p, now);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            for (wt, batch) in repo_watch.poll(Instant::now()) {
                let Some(h) = wt_hash.get(&wt).cloned() else {
                    continue;
                };
                activity.touch(wt.clone(), Instant::now());
                pending_batch.insert(wt.clone(), batch);
                // Ensure the cluster's RA exists (proven 0→1 SpawnRa).
                if let LifecycleAction::SpawnRa(_) = lifecycle.activate(path_key(&wt), h.clone()) {
                    spawn_cluster(
                        &mut clusters,
                        &h,
                        cluster_root.get(&h).cloned().unwrap_or_else(|| wt.clone()),
                        lsp_tx.clone(),
                        ctrl_tx.clone(),
                    );
                    drain_spawned(&mut clusters, &ctrl_rx);
                }
                route_or_defer(&mut clusters, &h, wt.clone(), &pending_batch, &api);
            }
        }

        // Activity tick → deactivation edges (proven WtLifecycle).
        for wt in activity.tick(Instant::now()) {
            if let Some(h) = wt_hash.get(&wt).cloned() {
                if clusters.contains_key(&h) {
                    step(
                        &mut clusters,
                        &h,
                        DriverEvent::Deactivated { wt: wt.clone() },
                        &pending_batch,
                        &api,
                    );
                }
                if let LifecycleAction::TeardownRa(_) = lifecycle.deactivate(path_key(&wt)) {
                    clusters.remove(&h); // Supervisor drop ⇒ RA reaped
                }
            }
        }
    }

    // ── FIELD FINDING A / #198 — the structural reap AT THE SEAM ──
    // The [[proven-core-precondition-violated-at-integration-seam]]
    // pattern, made VISIBLE here (exactly like the #193 take_followup
    // WHY-comment): EVERY serve-loop exit — SIGTERM/SIGINT, parent-
    // orphan, watcher-disconnect — funnels to this ONE site. Clearing
    // `clusters` drops each `ClusterState`, whose `_supervisor:
    // analyzer::Supervisor` `Drop` runs `do_shutdown`: join the monitor
    // thread + `kill()`+`wait()` the rust-analyzer child (and the
    // `process_group(0)`+`setsid` pgid discipline #3b/#44/#61/#128 takes
    // its proc-macro-srv descendants). Done EXPLICITLY — not via the
    // invisible "a normal return drops the BTreeMap which drops
    // ClusterState which drops Supervisor which reaps" chain. That very
    // invisibility is *why* #198's clean-SIGTERM gap went unnoticed (the
    // prior doc even asserted "Supervisors drop ⇒ RAs reaped" as if it
    // were obvious/automatic — but a default-disposition SIGTERM never
    // unwinds, so it wasn't). One funnel ⇒ no future exit path can
    // silently skip the reap. Proven cores (analyzer/clusterdrv/…)
    // UNTOUCHED — this restores their precondition at the wire seam.
    clusters.clear();
    // Increment 0: stop the HTTP accept loop at the SAME single exit
    // funnel. `HttpServer::Drop` flips the listener stop-flag; in-flight
    // one-shot/SSE connections drain when their peer disconnects. Done
    // EXPLICITLY (not via invisible scope-end drop) — the same
    // visible-teardown discipline as the per-cluster RA reap above, so no
    // future exit path can silently leave the listener thread spinning.
    drop(http_server);
    ExitCode::SUCCESS
}

fn heartbeat_repo_status(repo_root: &Path) {
    let now = statusfile::now_unix();
    let mut status = std::fs::read_to_string(statusfile::path(repo_root))
        .ok()
        .map(|text| Status::parse(&text))
        .filter(|st| st.root == repo_root.to_string_lossy())
        .unwrap_or_else(|| Status {
            pid: std::process::id(),
            root: repo_root.to_string_lossy().into_owned(),
            started: now,
            updated: now,
            verdict_str: Verdict::Unknown.as_str().to_string(),
            crates: Vec::new(),
            red_diagnostics: 0,
            analysed_at: 0,
            build_id: cargoless_core::build_id().to_string(),
        });
    status.pid = std::process::id();
    status.updated = now;
    status.build_id = cargoless_core::build_id().to_string();
    if status.started == 0 {
        status.started = now;
    }
    statusfile::write(repo_root, &status);
}

/// WtId (PathBuf) → the `String` key `ClusterLifecycle` uses.
fn path_key(wt: &WtId) -> String {
    wt.to_string_lossy().into_owned()
}

/// #240/2b — derive a WorkspaceConfigHash for a pushed overlay. PEEKS at the
/// api's pushed store (does NOT consume — `take_overlay_for` does that later
/// in the SwitchOverlay arm). The server owns the base checkout, so unchanged
/// workspace config is read from disk; pushed config bodies are only overrides
/// for changed config files. This keeps the push body to the actual local diff
/// while preserving the same cluster-routing shape as the FS path.
///
/// Path-matching is suffix-based: `path.ends_with("Cargo.toml")` so
/// both absolute (`/abs/wt/Cargo.toml`) and relative (`Cargo.toml`)
/// push paths resolve. The workspace-defining files mirror
/// `clustermgr::read_workspace_config`'s set.
fn cluster_hash_from_pushed(
    api: &Arc<crate::serveapi::ServeVerdictState>,
    wt_key: &str,
) -> WorkspaceConfigHash {
    // PEEK (non-consuming) — the consume happens later in the
    // SwitchOverlay arm via `take_overlay_for`.
    let Some(pushed) = api.peek_overlay_for(wt_key) else {
        return cargoless_core::cluster::WorkspaceConfig::default().hash();
    };
    fn find(files: &[(String, String)], suffix: &str) -> Option<String> {
        files
            .iter()
            .find(|(p, _)| p.ends_with(suffix))
            .map(|(_, c)| c.clone())
    }
    let root = pushed
        .analysis_root
        .clone()
        .unwrap_or_else(|| PathBuf::from(wt_key));
    let mut cfg = read_workspace_config(&root).unwrap_or_else(|_| WorkspaceConfig::default());
    if let Some(content) = find(&pushed.files, "Cargo.toml") {
        cfg.cargo_toml = Some(content);
    }
    if let Some(content) = find(&pushed.files, "Cargo.lock") {
        cfg.cargo_lock = Some(content);
    }
    if let Some(content) =
        find(&pushed.files, "rust-toolchain.toml").or_else(|| find(&pushed.files, "rust-toolchain"))
    {
        cfg.rust_toolchain = Some(content);
    }
    if let Some(content) =
        find(&pushed.files, ".cargo/config.toml").or_else(|| find(&pushed.files, ".cargo/config"))
    {
        cfg.cargo_config = Some(content);
    }
    cfg.hash()
}

/// Construct a cluster's RA Supervisor (sole `ClusterState` creation
/// site — Judgment A as composed). The `on_spawn` hook does the LSP
/// handshake, ships the client via `Ctrl::Spawned`, and detaches a
/// forwarder thread tagging every `LspEvent` with the cluster hash.
fn spawn_cluster(
    clusters: &mut BTreeMap<WorkspaceConfigHash, ClusterState>,
    h: &WorkspaceConfigHash,
    root: PathBuf,
    lsp_tx: Sender<(WorkspaceConfigHash, LspEvent)>,
    ctrl_tx: Sender<Ctrl>,
) {
    if clusters.contains_key(h) {
        return; // ClusterLifecycle proves SpawnRa is 0→1 only; defensive.
    }
    let spawn_root = root.clone();
    let spawn = move || -> std::io::Result<Child> {
        let mut cmd = rust_analyzer_command()?;
        cmd.current_dir(&spawn_root);
        cmd.spawn()
    };
    let hook_root = root.clone();
    let hook_h = h.clone();
    let cluster_lsp_tx = lsp_tx.clone();
    let on_spawn = move |child: &mut Child| {
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return;
        };
        let root_str = hook_root.to_string_lossy().into_owned();
        let opts = InitOpts::from_env_and_project(&hook_root);
        let Ok((client, events)) = LspClient::initialize(stdin, stdout, &root_str, &opts) else {
            return; // RA broke mid-handshake; Supervisor retries
        };
        let client = Arc::new(client);
        // #246 5c KEYSTONE: `ra.spawn` event — load-bearing AC4 oracle
        // input. The plan's Wave-1 spec calls for both `ra.spawn` (initial)
        // and `ra.respawn` (post-restart) spans; Wave-1 simplifies to a
        // single `ra.spawn` event at every supervisor handshake (initial
        // OR restart — Supervisor's caller doesn't distinguish at this
        // seam). The `overlay.reset` event that fires from the Ctrl::Spawned
        // handler on every spawn IS the distinguishing signal — its
        // presence after the FIRST `ra.spawn` proves the multiplex+driver
        // reset ran, and its absence is the AC4 false-GREEN smoking gun.
        tracing::info!(
            cluster_id = %hook_h.as_str(),
            ra_pid = ?child.id(),
            "ra.spawn",
        );
        let _ = ctrl_tx.send(Ctrl::Spawned(hook_h.clone(), Arc::clone(&client)));
        let fwd_h = hook_h.clone();
        let fwd_tx = lsp_tx.clone();
        let _ = std::thread::Builder::new()
            .name("tf-cluster-fwd".into())
            .spawn(move || {
                // Ends when this RA instance's stdout EOFs (Receiver-
                // lifecycle: a dropped/dead channel just stops the
                // forwarder; the next on_spawn starts a fresh one).
                while let Ok(ev) = events.recv() {
                    if fwd_tx.send((fwd_h.clone(), ev)).is_err() {
                        break;
                    }
                }
            });
    };
    let Ok(supervisor) = Supervisor::start_with_hook(spawn, on_spawn) else {
        crate::ui::warn("rust-analyzer spawn failed for a cluster — skipping");
        return;
    };
    clusters.insert(
        h.clone(),
        ClusterState {
            _supervisor: supervisor,
            cluster: h.clone(),
            lsp_tx: cluster_lsp_tx,
            lsp: None,
            mux: OverlayMultiplexer::new(),
            driver: if ra_native_verdict_mode() {
                ClusterDriver::with_verdict_policy(VerdictPolicy::RaNative)
            } else {
                ClusterDriver::new()
            },
            next_ver: 2,
            ready: false,
            deferred: VecDeque::new(),
        },
    );
}

fn drain_spawned(
    clusters: &mut BTreeMap<WorkspaceConfigHash, ClusterState>,
    ctrl_rx: &Receiver<Ctrl>,
) {
    while let Ok(Ctrl::Spawned(h, client)) = ctrl_rx.try_recv() {
        if let Some(cs) = clusters.get_mut(&h) {
            cs.driver.reset_after_respawn();
            cs.mux.reset();
            cs.lsp = Some(client);
            // In pushed RA-native service mode, a request already carries the
            // concrete overlay to check and `spawn_ra_native_settle` provides
            // the settle delay. Do not wait for an IndexingEnded notification
            // that rust-analyzer may never emit on a cold, push-only daemon.
            cs.ready = ready_after_respawn_for_modes(push_only_mode(), ra_native_verdict_mode());
            // #246 5c KEYSTONE: the `overlay.reset` event — load-bearing
            // for AC4 diagnostics. Its PRESENCE between an `ra.respawn`
            // span (emitted inside on_spawn) and the next
            // `verdict.publish` proves the #190 + #247 structural-
            // precondition-restore ran; ABSENCE is the smoking gun for
            // the proven-core-precondition-violated-at-integration-seam
            // false-GREEN path. Pairs with the always-on `[cargoless:obs]`
            // eprintln (kept as ops-without-collector fallback).
            tracing::info!(
                cluster_id = %h.as_str(),
                reset_actually_called = true,
                "overlay.reset",
            );
            eprintln!(
                "[cargoless:obs] respawn cluster={} driver+mux reset (#247)",
                h.as_str()
            );
        }
    }
}

fn route_or_defer(
    clusters: &mut BTreeMap<WorkspaceConfigHash, ClusterState>,
    h: &WorkspaceConfigHash,
    wt: WtId,
    pending_batch: &BTreeMap<WtId, Vec<PathBuf>>,
    api: &Arc<crate::serveapi::ServeVerdictState>,
) {
    let ready = {
        let Some(cs) = clusters.get_mut(h) else {
            return;
        };
        if !cs.ready {
            if !cs.deferred.contains(&wt) {
                cs.deferred.push_back(wt.clone());
            }
            false
        } else {
            true
        }
    };
    if ready {
        step(
            clusters,
            h,
            DriverEvent::RoutedBatch { wt },
            pending_batch,
            api,
        );
    }
}

fn mark_ready_and_take_deferred(
    clusters: &mut BTreeMap<WorkspaceConfigHash, ClusterState>,
    h: &WorkspaceConfigHash,
) -> Vec<WtId> {
    let Some(cs) = clusters.get_mut(h) else {
        return Vec::new();
    };
    cs.ready = true;
    cs.deferred.drain(..).collect()
}

fn early_red_event(ev: LspEvent) -> LspEvent {
    match ev {
        LspEvent::Diagnostics(pd) if pd.has_any_severity_error() => LspEvent::FlycheckFailed {
            message: format!("rust-analyzer reported error diagnostics for {}", pd.uri),
        },
        other => other,
    }
}

/// Feed one `DriverEvent` to a cluster's `ClusterDriver` and faithfully
/// execute the resulting `ClusterAction` (+ any post-settle follow-up).
fn step(
    clusters: &mut BTreeMap<WorkspaceConfigHash, ClusterState>,
    h: &WorkspaceConfigHash,
    ev: DriverEvent,
    pending_batch: &BTreeMap<WtId, Vec<PathBuf>>,
    api: &Arc<crate::serveapi::ServeVerdictState>,
) {
    let Some(cs) = clusters.get_mut(h) else {
        return;
    };
    let action = cs.driver.on_event(ev);
    exec_driver_action(cs, action, pending_batch, api);
}

fn exec_driver_action(
    cs: &mut ClusterState,
    action: ClusterAction,
    pending_batch: &BTreeMap<WtId, Vec<PathBuf>>,
    api: &Arc<crate::serveapi::ServeVerdictState>,
) {
    let take_followup = matches!(action, ClusterAction::EmitVerdict { .. });
    // #247 obs: log the wire-side detection of barrier-settle (= the
    // moment ClusterDriver emits an EmitVerdict — Judgment B's sole
    // attribution boundary, observed BEFORE we dispatch the action to
    // `exec`). The eprintln is dep-free; full OTEL `verdict.publish`
    // span lands in #246.
    if let ClusterAction::EmitVerdict { wt, .. } = &action {
        // #246 5c: `flycheck.end` event (event form, not span — the spanning
        // check.cycle is Wave-2 5d scope). Captures the WT + settle instant
        // at the same site the eprintln does, so the structured trace has
        // the barrier-settlement boundary explicitly marked. Paired with
        // the always-on `[cargoless:obs]` eprintln as ops-without-collector
        // fallback.
        tracing::info!(
            worktree = %wt.display(),
            settled_at_unix = statusfile::now_unix(),
            "flycheck.end",
        );
        eprintln!(
            "[cargoless:obs] flycheck-end wt={} settled at={} (#247)",
            wt.display(),
            statusfile::now_unix()
        );
    }
    exec(cs, action, pending_batch, api);
    // EXACTLY ONCE — `clusterdrv::take_followup` is structurally
    // non-mutating (`self.current.as_ref().map(...)`, never clears
    // `current`) and its doc contract is "the adapter calls this exactly
    // once right after an `EmitVerdict`" (one event ⇒ (verdict,
    // optional-next-switch) pair). A `while` here violates that
    // precondition: after a settle with a queued/recheck next,
    // `start_next_after_settle` sets `current = Some(next)` and nothing
    // in the loop body clears it, so `take_followup` would re-yield
    // `SwitchOverlay{next}` forever (non-terminating spin on the
    // ≥2-WT-per-cluster serialization path). `if let` drives exactly one
    // follow-up switch per settle; the next txn's barrier then advances
    // on subsequent `DriverEvent::Lsp` in later serve-loop iterations —
    // restoring the proven core's exactly-once precondition AT THE WIRE
    // SEAM (the core is never weakened to accommodate a seam misuse).
    if take_followup {
        if let Some(followup) = cs.driver.take_followup() {
            exec(cs, followup, pending_batch, api);
        }
    }
}

/// Execute one `ClusterAction` (faithful composition — surface (ii)).
fn exec(
    cs: &mut ClusterState,
    action: ClusterAction,
    pending_batch: &BTreeMap<WtId, Vec<PathBuf>>,
    api: &Arc<crate::serveapi::ServeVerdictState>,
) {
    match action {
        ClusterAction::Idle => {}
        ClusterAction::SwitchOverlay { wt } => {
            // #246 5c KEYSTONE: `overlay.switch` span wraps the body —
            // captures wt, file_count, overlay_size_bytes. Bound via
            // `.entered()` to the arm scope so the span closes when the
            // arm exits. Field values are computed eagerly from
            // `pending_batch` and recorded **on every exit path** below
            // (both the early-return AND the normal end-of-arm) so the
            // span never emits with Empty fields — the
            // `tracing::field::record_at_close` discipline made explicit.
            let _span = tracing::info_span!(
                "overlay.switch",
                worktree = %wt.display(),
                file_count = tracing::field::Empty,
                overlay_size_bytes = tracing::field::Empty,
            )
            .entered();
            // #247 obs: log the wire-side check-start (the SwitchOverlay
            // arm dispatching mux.switch_to + did_save = the flycheck
            // trigger). Pairs with `flycheck-end` (step's EmitVerdict
            // detection) and `verdict-emit` (publish_verdict) to give a
            // grep-able sequence per WT per generation. Dep-free.
            eprintln!(
                "[cargoless:obs] check-started wt={} at={} (#247)",
                wt.display(),
                statusfile::now_unix()
            );
            // #240/2b — overlay source pick (D-PUSHOVERLAY §4.1 pivot).
            // PUSHED-mode: if a fresh push is pending for this WT, source
            // the overlay from the in-memory store (consumed — the
            // pop-on-consume semantic). The FS-mode path below is the
            // unchanged v0.2.0 wire. The proven core (`overlay::diff`,
            // `multiplex::switch_to`, LSP verbs, barrier, EmitVerdict,
            // publish_verdict) is BYTE-UNTOUCHED — only the SOURCE of the
            // pairs changes. THIS is the composing-equivalence assertion
            // that 2b's load-bearing test pins: for the same `(prev,
            // pairs)`, `overlay::diff` produces a byte-identical
            // `Vec<OverlayOp>` whether `pairs` came from the FS or from
            // the pushed store. Per-WT mode arbitration (one WT can be
            // pushed while another is FS-watched).
            // Build BEFORE the lsp-present guard so the span's
            // load-bearing fields (file_count, overlay_size_bytes) can be
            // recorded on BOTH exit paths — the early-return case STILL
            // carries valid attrs, distinguishing "0-file early return"
            // from "0-file no-overlay-found" (CATCH-1 from #246-L3).
            let mut pushed_check_profile = None;
            let wt_key = wt.to_string_lossy().into_owned();
            let pairs: Vec<(String, String)> = if let Some(pushed) = api.take_overlay_for(&wt_key) {
                pushed_check_profile = pushed.check_profile;
                let project_root = pushed.analysis_root.clone().unwrap_or_else(|| wt.clone());
                let materialize_overlay = pushed.analysis_root.is_some();
                api.record_project_check_context(
                    &wt_key,
                    project_root,
                    pushed.changed_files.clone(),
                    pushed.base_ref.clone(),
                    pushed.files.clone(),
                    materialize_overlay,
                    pushed.gate,
                );
                pushed.files
            } else {
                let mut pairs = Vec::new();
                if let Some(files) = pending_batch.get(&wt) {
                    for f in files {
                        if let Ok(text) = std::fs::read_to_string(f) {
                            pairs.push((f.to_string_lossy().into_owned(), text));
                        }
                    }
                }
                pairs
            };
            let file_count = pairs.len() as u64;
            let overlay_size_bytes: u64 = pairs.iter().map(|(_, c)| c.len() as u64).sum();
            _span.record("file_count", file_count);
            _span.record("overlay_size_bytes", overlay_size_bytes);

            let Some(lsp) = cs.lsp.clone() else {
                // Early-return: RA not handshaked yet; a later batch
                // retries. Fields are recorded above so the span still
                // carries valid attrs at drop.
                return;
            };
            let lsp_pairs = lsp_source_pairs(&pairs);
            let target =
                OverlaySet::from_pairs(lsp_pairs.iter().map(|(p, c)| (p.clone(), c.clone())));
            for verb in cs.mux.switch_to(&target) {
                match verb {
                    LspVerb::DidOpen { path, content } => {
                        let _ = lsp.did_open(&path.to_string_lossy(), &content, 1);
                    }
                    LspVerb::DidChange { path, content } => {
                        let v = cs.next_ver;
                        cs.next_ver += 1;
                        let _ = lsp.did_change(&path.to_string_lossy(), &content, v);
                    }
                    LspVerb::DidClose { path } => {
                        let _ = lsp.did_close(&path.to_string_lossy());
                    }
                }
            }
            // Cargoless replaces iterative cargo check/clippy; pushed Cargo
            // selectors are compatibility metadata, not an execution request.
            // They must not create a minute-scale direct Cargo lane inside
            // the daemon.
            if pushed_check_profile.is_some() {
                eprintln!(
                    "[cargoless:obs] pushed-check-profile-ignored wt={} replacement=ra-native (#tfmv)",
                    wt.display()
                );
            }
            // The replacement verdict path has no didSave/runFlycheck and no
            // direct Cargo subprocess. A delayed synthetic settle lets RA
            // publish diagnostics for the just-applied overlay before the
            // existing barrier publishes the worktree bit.
            spawn_ra_native_settle(&wt, cs.cluster.clone(), cs.lsp_tx.clone());
        }
        ClusterAction::EmitVerdict {
            wt,
            authoritative_error,
        } => {
            // THE sole verdict-attribution site (Judgment B as composed).
            let project_check_context = api.take_project_check_context(&wt.to_string_lossy());
            // warn-fast / witness-gated hybrid: a push that opted in with
            // `gate: true` (the merge-gate workflow) is promoted to hard-mode
            // behavior FOR THIS WORKTREE — it runs the witness and publishes
            // the gated verdict on `/status` even when the daemon default is
            // `warn`. The fleet's live FS-watch loop and ordinary pushes carry
            // `gate=false`, so they keep the instant RA-native warn verdict and
            // pay nothing for the witness. Off mode ignores the bit (no checks
            // configured to gate on). `gate` rides on the per-worktree
            // `ProjectCheckRunContext` (set from `PushedOverlay.gate` in the
            // SwitchOverlay arm); absent context ⇒ not a gate push.
            let gate_requested = project_check_context.as_ref().is_some_and(|ctx| ctx.gate);
            let effective_mode =
                effective_project_checks_mode(project_checks_mode(), gate_requested);
            match effective_mode {
                ProjectChecksMode::Off => publish_verdict(&wt, authoritative_error, api),
                ProjectChecksMode::Warn => {
                    publish_verdict(&wt, authoritative_error, api);
                    spawn_project_checks_warn(wt, project_check_context, Arc::clone(api));
                }
                ProjectChecksMode::Hard => {
                    // FIELD FINDING (latency): the prior implementation ran
                    // `run_project_checks_and_log` SYNCHRONOUSLY here, then
                    // published. The project-check witness is a real
                    // `cargo check` of the changed crate's reverse-dependency
                    // cone (~minutes on a foundational-crate edit). Because the
                    // serve loop's single owner thread is the SOLE drainer of
                    // every cluster's RA event stream (`lsp_rx`), the pushed
                    // overlay channel (`push_rx`), the watcher, and activity
                    // ticks, a synchronous witness froze the ENTIRE pod — every
                    // other worktree, across every cluster, stopped getting
                    // verdicts for the witness's whole duration. (That pod-wide
                    // stall — "all agents seeing slow checks" — is what forced
                    // the operator hard→warn revert; the per-pusher latency
                    // floor is irreducible cargo-check work and is NOT the
                    // thing being fixed here.)
                    //
                    // Fix: run the witness OFF the loop thread (the proven warn
                    // threading shape) but — unlike warn — DEFER the verdict
                    // publish to INSIDE the closure, OR'd with the RA-native
                    // bit. So the gate stays real ("green means the witness
                    // compiled it"), there is NO early/provisional green (warn
                    // publishes-then-checks; hard checks-then-publishes), and
                    // the loop returns immediately so no other worktree is
                    // blocked. Only the pushing worktree waits for its own gate.
                    spawn_project_checks_hard(
                        wt,
                        authoritative_error,
                        project_check_context,
                        Arc::clone(api),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
fn save_trigger_path(
    wt: &WtId,
    pending_batch: &BTreeMap<WtId, Vec<PathBuf>>,
    overlay_pairs: &[(String, String)],
) -> PathBuf {
    if let Some(path) = pending_batch.get(wt).and_then(|files| files.first()) {
        return path.clone();
    }

    overlay_pairs
        .iter()
        .map(|(path, _)| overlay_path_for_wt(wt, path))
        .find(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .or_else(|| {
            overlay_pairs
                .first()
                .map(|(path, _)| overlay_path_for_wt(wt, path))
        })
        .unwrap_or_else(|| wt.join("Cargo.toml"))
}

fn lsp_source_pairs(pairs: &[(String, String)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .filter(|(path, _)| is_rust_source_path(path))
        .cloned()
        .collect()
}

fn is_rust_source_path(path: &str) -> bool {
    Path::new(path).extension().is_some_and(|ext| ext == "rs")
}

#[cfg(test)]
fn overlay_path_for_wt(wt: &WtId, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        wt.join(path)
    }
}

fn spawn_ra_native_settle(
    wt: &WtId,
    h: WorkspaceConfigHash,
    tx: Sender<(WorkspaceConfigHash, LspEvent)>,
) {
    let wt = wt.clone();
    let delay = ra_native_settle_delay();
    let _ = std::thread::Builder::new()
        .name("tf-ra-settle".into())
        .spawn(move || {
            eprintln!(
                "[cargoless:obs] ra-native-settle-started wt={} delay_ms={} (#tfmv)",
                wt.display(),
                delay.as_millis()
            );
            std::thread::sleep(delay);
            eprintln!(
                "[cargoless:obs] ra-native-settle-ended wt={} status=settled (#tfmv)",
                wt.display()
            );
            let _ = tx.send((h, LspEvent::FlycheckEnded));
        });
}

/// Write `wt`'s per-worktree verdict — the only place a verdict is
/// attributed/published in the whole wire (Judgment B as composed).
///
/// Increment 0: this one site now feeds BOTH sinks — the durable
/// `statusfile` (the v0 on-disk read path, unchanged) AND the in-memory
/// [`crate::serveapi::ServeVerdictState`] that backs the shipped HTTP+SSE
/// transport (`api.publish`, which also fans out the subscribe-emit
/// transition, plan 0b). One real verdict ⇒ one statusfile write ⇒ one
/// service update ⇒ one transition event. NO second verdict-attribution
/// path is introduced: the read-plane is a faithful mirror of this single
/// authoritative write-plane.
fn publish_verdict(
    wt: &Path,
    authoritative_error: bool,
    api: &Arc<crate::serveapi::ServeVerdictState>,
) {
    let verdict = if authoritative_error {
        Verdict::Red
    } else {
        Verdict::Green
    };
    let now = statusfile::now_unix();
    // #246 5c KEYSTONE — **Judgment-B sole-attribution at the OTEL surface.**
    // This span MUST be the only emission of `verdict.publish`, mirroring
    // the structural invariant that publish_verdict is called from exactly
    // one site (the ClusterAction::EmitVerdict arm in `exec()`). A future
    // emission seam introducing a non-EmitVerdict path would by-pass this
    // span site → loud telemetry signal at the type-system level (Layer-2
    // keystone criterion at the OTEL surface).
    let _span = tracing::info_span!(
        "verdict.publish",
        worktree = %wt.display(),
        verdict_color = verdict.as_str(),
        pid = std::process::id(),
        trigger_source = "EmitVerdict",
        analysed_at = now,
        otel.status_code = if authoritative_error { "ERROR" } else { "OK" },
    )
    .entered();
    let st = Status {
        pid: std::process::id(),
        root: wt.to_string_lossy().into_owned(),
        started: now,
        updated: now,
        verdict_str: verdict.as_str().to_string(),
        crates: Vec::new(),
        red_diagnostics: 0,
        // #247 obs: analysed_at = settle-observed instant (Judgment B sole
        // attribution site = the moment the wire reached this arm). For
        // the current single-write path, analysed_at == updated; the
        // distinction is preserved-design (a future heartbeat-refresh
        // path would tick `updated` without re-checking, leaving
        // `analysed_at` at the original settle time).
        analysed_at: now,
        build_id: cargoless_core::build_id().to_string(),
    };
    statusfile::write(wt, &st);
    eprintln!(
        "[cargoless:obs] verdict-emit wt={} verdict={} analysed_at={} (#247)",
        wt.display(),
        verdict.as_str(),
        now
    );
    // Same site, mirror sink: feed the read-plane VerdictService + emit
    // the transition (subscribe-emit, 0b). Best-effort by construction —
    // a poisoned lock recovers; a transport hiccup never wedges the loop.
    api.publish(wt, authoritative_error);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectChecksMode {
    Off,
    Warn,
    Hard,
}

fn project_checks_mode() -> ProjectChecksMode {
    match std::env::var("CARGOLESS_PROJECT_CHECKS_MODE")
        .unwrap_or_else(|_| "hard".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "off" | "0" | "false" | "disabled" => ProjectChecksMode::Off,
        "warn" => ProjectChecksMode::Warn,
        _ => ProjectChecksMode::Hard,
    }
}

/// warn-fast / witness-gated hybrid: a `gate: true` push promotes the daemon's
/// default mode for THIS worktree's verdict. Warn→Hard so the merge-gate push
/// gets the authoritative witness verdict while the fleet's fast loop (gate
/// false) stays warn. Off is never promoted — no checks are configured to
/// gate on — and Hard is already authoritative. Pure + total ⇒ unit-testable.
fn effective_project_checks_mode(
    default: ProjectChecksMode,
    gate_requested: bool,
) -> ProjectChecksMode {
    match (default, gate_requested) {
        (ProjectChecksMode::Warn, true) => ProjectChecksMode::Hard,
        (mode, _) => mode,
    }
}

fn spawn_project_checks_warn(
    wt: PathBuf,
    context: Option<crate::serveapi::ProjectCheckRunContext>,
    api: Arc<crate::serveapi::ServeVerdictState>,
) {
    let display = wt.display().to_string();
    if let Err(e) = std::thread::Builder::new()
        .name("cargoless-project-checks-warn".to_string())
        .spawn(move || {
            let red = run_project_checks_and_log(&wt, context, &api);
            eprintln!(
                "[cargoless:obs] project-checks-warn wt={} gate=false observed_red={}",
                wt.display(),
                red
            );
        })
    {
        eprintln!(
            "[cargoless:obs] project-checks-warn wt={} spawn_error={}",
            display, e
        );
    }
}

/// Hard-mode gated witness, run OFF the serve-loop thread (see the
/// `ProjectChecksMode::Hard` arm for the why). Mirrors
/// [`spawn_project_checks_warn`]'s threading, but the verdict is published
/// from INSIDE the closure — only after the witness settles — OR'd with the
/// RA-native `authoritative_error`. This preserves the gate semantics
/// (`authoritative_error || project_check_error`) byte-for-byte with the prior
/// synchronous code; the only change is *where* it runs (a detached thread
/// instead of the loop owner). No early/provisional green is published: the
/// publish happens once, after the check, so a slow witness leaves the
/// worktree's prior verdict in place rather than briefly flipping green.
///
/// Spawn-failure fallback: if the thread cannot be created, publish the
/// RA-native verdict synchronously so the worktree is never left without a
/// verdict (degrades to "RA-native only" for that one push, never to silence).
fn spawn_project_checks_hard(
    wt: PathBuf,
    authoritative_error: bool,
    context: Option<crate::serveapi::ProjectCheckRunContext>,
    api: Arc<crate::serveapi::ServeVerdictState>,
) {
    // `wt` + `api` are moved into the `move` closure, which `spawn` consumes
    // even when it returns `Err` — so the spawn-failure fallback below cannot
    // reference them. Keep cheap clones for that path (`api` is an `Arc`).
    let display = wt.display().to_string();
    let fallback_wt = wt.clone();
    let fallback_api = Arc::clone(&api);
    if let Err(e) = std::thread::Builder::new()
        .name("cargoless-project-checks-hard".to_string())
        .spawn(move || {
            let project_check_error = run_project_checks_and_log(&wt, context, &api);
            let gated = authoritative_error || project_check_error;
            eprintln!(
                "[cargoless:obs] project-checks-hard wt={} gate=true ra_native_error={} project_check_error={} gated_verdict={}",
                wt.display(),
                authoritative_error,
                project_check_error,
                if gated { "red" } else { "green" },
            );
            publish_verdict(&wt, gated, &api);
        })
    {
        eprintln!(
            "[cargoless:obs] project-checks-hard wt={} spawn_error={} — publishing RA-native verdict synchronously",
            display, e
        );
        publish_verdict(&fallback_wt, authoritative_error, &fallback_api);
    }
}

fn run_project_checks_and_log(
    wt: &Path,
    context: Option<crate::serveapi::ProjectCheckRunContext>,
    api: &Arc<crate::serveapi::ServeVerdictState>,
) -> bool {
    let root = context.as_ref().map(|ctx| ctx.root.as_path()).unwrap_or(wt);
    let changed_files = context
        .as_ref()
        .and_then(|ctx| ctx.changed_files.as_deref());
    let report = match context.as_ref() {
        Some(ctx) => api.with_project_check_overlay(ctx, |root| {
            cargoless_core::project_checks::run_dev_with_changes(root, changed_files)
        }),
        None => Ok(cargoless_core::project_checks::run_dev_with_changes(
            root,
            changed_files,
        )),
    };
    match report {
        Ok(Ok(report)) if report.results.is_empty() && report.skipped.is_empty() => false,
        Ok(Ok(report)) => {
            let cache_hits = report.results.iter().filter(|r| r.cache_hit).count();
            let slowest = slowest_project_checks(&report.results);
            eprintln!(
                "[cargoless:obs] project-checks wt={} root={} verdict={} checks={} skipped={} cache_hits={} duration_ms={} slowest={}",
                wt.display(),
                root.display(),
                if report.tree == cargoless_core::TreeState::Red {
                    "red"
                } else {
                    "green"
                },
                report.results.len(),
                report.skipped.len(),
                cache_hits,
                report.duration_ms,
                slowest
            );
            for diagnostic in report
                .diagnostics
                .iter()
                .filter(|d| d.severity == cargoless_core::Severity::Error)
                .take(8)
            {
                eprintln!(
                    "[cargoless:obs] project-check-red wt={} path={} line={} code={} message={}",
                    wt.display(),
                    diagnostic.file_path.display(),
                    diagnostic.line,
                    diagnostic.code.as_deref().unwrap_or("project-check"),
                    diagnostic.message.lines().next().unwrap_or("")
                );
            }
            report.tree == cargoless_core::TreeState::Red
        }
        Ok(Err(e)) => {
            eprintln!(
                "[cargoless:obs] project-checks wt={} verdict=red setup_error={}",
                wt.display(),
                e
            );
            true
        }
        Err(e) => {
            eprintln!(
                "[cargoless:obs] project-checks wt={} root={} verdict=red overlay_error={}",
                wt.display(),
                root.display(),
                e
            );
            true
        }
    }
}

fn slowest_project_checks(
    results: &[cargoless_core::project_checks::ProjectCheckResult],
) -> String {
    let mut items: Vec<_> = results
        .iter()
        .filter(|r| r.duration_ms > 0)
        .map(|r| (r.duration_ms, r.id.as_str()))
        .collect();
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let rendered: Vec<String> = items
        .into_iter()
        .take(3)
        .map(|(duration, id)| format!("{id}:{duration}ms"))
        .collect();
    if rendered.is_empty() {
        "-".to_string()
    } else {
        rendered.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargoless_core::transport::{PushOverlayOptions, VerdictService};

    fn temp_root(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cargoless-servedrv-{label}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Hard mode runs the witness OFF the loop and still gates the verdict.
    /// With no manifest the witness is a no-op (Green), so the published
    /// verdict equals the RA-native bit: clean RA ⇒ green, RA error ⇒ red
    /// (the `authoritative_error || project_check_error` gate). Crucially the
    /// publish happens from the spawned thread, so we join by polling the
    /// API the verdict lands in — proving the gate is preserved across the
    /// sync→async move without an early/provisional green.
    fn await_verdict(api: &Arc<crate::serveapi::ServeVerdictState>, wt: &str) -> String {
        // The hard witness publishes from a detached thread; poll briefly.
        for _ in 0..200 {
            if let Some(st) = api.get_status(wt) {
                return st.verdict;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("hard witness never published a verdict for {wt}");
    }

    #[test]
    fn hard_witness_publishes_gated_green_when_ra_clean_and_no_checks() {
        let root = temp_root("hard-green");
        let api = Arc::new(crate::serveapi::ServeVerdictState::new());
        // authoritative_error=false (RA clean) + no manifest (witness green)
        // ⇒ gated verdict green, published from the spawned thread.
        spawn_project_checks_hard(root.clone(), false, None, Arc::clone(&api));
        assert_eq!(await_verdict(&api, &root.to_string_lossy()), "green");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn gate_push_promotes_warn_to_hard_only_for_that_push() {
        use ProjectChecksMode::*;
        // warn-fast / witness-gated hybrid contract:
        // a gate push promotes warn→hard (the merge-gate gets the witness);
        // a non-gate push in warn stays warn (the fleet's fast loop).
        assert_eq!(effective_project_checks_mode(Warn, true), Hard);
        assert_eq!(effective_project_checks_mode(Warn, false), Warn);
        // Off is never promoted (nothing configured to gate on); Hard is
        // already authoritative regardless of the gate bit.
        assert_eq!(effective_project_checks_mode(Off, true), Off);
        assert_eq!(effective_project_checks_mode(Off, false), Off);
        assert_eq!(effective_project_checks_mode(Hard, true), Hard);
        assert_eq!(effective_project_checks_mode(Hard, false), Hard);
    }

    #[test]
    fn hard_witness_publishes_red_when_ra_native_error_even_if_checks_pass() {
        let root = temp_root("hard-red");
        let api = Arc::new(crate::serveapi::ServeVerdictState::new());
        // authoritative_error=true (RA flagged a real error) OR'd with a
        // green/no-op witness ⇒ gated verdict RED. This is the no-false-green
        // invariant: the gate never downgrades an RA-native red to green.
        spawn_project_checks_hard(root.clone(), true, None, Arc::clone(&api));
        assert_eq!(await_verdict(&api, &root.to_string_lossy()), "red");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cluster_hash_from_pushed_reads_base_config_when_overlay_has_no_config() {
        let root = temp_root("base-config");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        std::fs::write(root.join("Cargo.lock"), "# base lock\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let api = Arc::new(crate::serveapi::ServeVerdictState::new());
        let files = vec![("src/lib.rs".to_string(), "pub fn x() {}".to_string())];
        let options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some(root.to_string_lossy().into_owned()),
            base_sha: None,
            changed_files: Some(vec!["src/lib.rs".into()]),
            gate: false,
        };

        let ack = api.push_overlay_with_options("/client/wt", "", &files, None, Some(&options));

        assert!(ack.accepted);
        assert_eq!(
            cluster_hash_from_pushed(&api, "/client/wt"),
            read_workspace_config(&root).unwrap().hash()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cluster_hash_from_pushed_overrides_changed_config_only() {
        let root = temp_root("changed-config");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[\"base\"]\n").unwrap();
        std::fs::write(root.join("Cargo.lock"), "# base lock\n").unwrap();
        let api = Arc::new(crate::serveapi::ServeVerdictState::new());
        let changed_cargo_toml = "[workspace]\nmembers=[\"changed\"]\n";
        let files = vec![("Cargo.toml".to_string(), changed_cargo_toml.to_string())];
        let options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some(root.to_string_lossy().into_owned()),
            base_sha: None,
            changed_files: Some(vec!["Cargo.toml".into()]),
            gate: false,
        };

        let ack = api.push_overlay_with_options("/client/wt", "", &files, None, Some(&options));

        assert!(ack.accepted);
        assert_eq!(
            cluster_hash_from_pushed(&api, "/client/wt"),
            WorkspaceConfig::new(
                Some(changed_cargo_toml.to_string()),
                Some("# base lock\n".to_string()),
                None,
                None,
            )
            .hash()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_trigger_prefers_pending_fs_batch() {
        let wt = PathBuf::from("/repo/wt-01");
        let mut pending = BTreeMap::new();
        pending.insert(wt.clone(), vec![wt.join("alchemy/src/lib.rs")]);

        let save = save_trigger_path(
            &wt,
            &pending,
            &[(
                "/repo/wt-01/other/src/main.rs".into(),
                "fn main() {}".into(),
            )],
        );

        assert_eq!(save, wt.join("alchemy/src/lib.rs"));
    }

    #[test]
    fn save_trigger_prefers_rust_file_from_pushed_overlay() {
        let wt = PathBuf::from("/repo/wt-01");
        let pending = BTreeMap::new();

        let save = save_trigger_path(
            &wt,
            &pending,
            &[
                ("/repo/wt-01/Cargo.toml".into(), "[workspace]".into()),
                (
                    "/repo/wt-01/alchemy/src/protocols/transfer.rs".into(),
                    "pub struct TransferProtocol;".into(),
                ),
            ],
        );

        assert_eq!(save, wt.join("alchemy/src/protocols/transfer.rs"));
    }

    #[test]
    fn save_trigger_normalizes_relative_pushed_paths() {
        let wt = PathBuf::from("/repo/wt-01");
        let pending = BTreeMap::new();

        let save = save_trigger_path(
            &wt,
            &pending,
            &[("alchemy/src/lib.rs".into(), "pub fn f() {}".into())],
        );

        assert_eq!(save, wt.join("alchemy/src/lib.rs"));
    }

    #[test]
    fn lsp_overlay_pairs_keep_only_rust_sources() {
        let pairs = vec![
            ("/repo/wt-01/Cargo.toml".into(), "[workspace]".into()),
            ("/repo/wt-01/Cargo.lock".into(), "# lock".into()),
            (
                "/repo/wt-01/alchemy/src/protocols/transfer.rs".into(),
                "pub struct TransferProtocol;".into(),
            ),
            ("/repo/wt-01/.cargo/config.toml".into(), "[build]".into()),
        ];

        let lsp_pairs = lsp_source_pairs(&pairs);

        assert_eq!(
            lsp_pairs,
            vec![(
                "/repo/wt-01/alchemy/src/protocols/transfer.rs".into(),
                "pub struct TransferProtocol;".into(),
            )]
        );
    }

    #[test]
    fn push_only_ra_native_is_ready_after_respawn_without_indexing_end() {
        assert!(ready_after_respawn_for_modes(true, true));
        assert!(!ready_after_respawn_for_modes(true, false));
        assert!(!ready_after_respawn_for_modes(false, true));
        assert!(!ready_after_respawn_for_modes(false, false));
    }

    #[test]
    fn early_red_event_maps_ra_error_diagnostics_to_terminal_red() {
        let ev = LspEvent::Diagnostics(cargoless_core::lsp::PublishDiagnostics {
            uri: "file:///repo/wt/src/lib.rs".into(),
            authoritative_errors: 0,
            advisory_errors: 1,
            total: 1,
            diagnostics: Vec::new(),
        });

        assert!(matches!(
            early_red_event(ev),
            LspEvent::FlycheckFailed { message } if message.contains("file:///repo/wt/src/lib.rs")
        ));
    }
}
