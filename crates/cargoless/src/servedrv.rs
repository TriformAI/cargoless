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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitCode};
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use cargoless_core::activity::ActivityConfig;
use cargoless_core::activitymgr::ActivityTracker;
use cargoless_core::analyzer::{Supervisor, rust_analyzer_command};
use cargoless_core::cluster::WorkspaceConfigHash;
use cargoless_core::clusterdrv::{ClusterAction, ClusterDriver, DriverEvent};
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
    /// The currently-live RA's client; `None` until the first
    /// `Spawned` message lands, swapped on every (re)spawn.
    lsp: Option<Arc<LspClient>>,
    /// Spatial-isolation multiplexer; `reset()` on every (re)spawn.
    mux: OverlayMultiplexer,
    /// The per-cluster transaction sequencer (Judgments A+B structural).
    driver: ClusterDriver,
    /// Monotonic LSP document version for `did_change`.
    next_ver: i64,
}

/// Control messages from the per-cluster Supervisor `on_spawn` hook to
/// the serve loop.
enum Ctrl {
    /// A cluster's RA (re)spawned and its LSP handshake completed.
    Spawned(WorkspaceConfigHash, Arc<LspClient>),
}

/// Run the live repo-scoped Model R daemon loop. Replaces serve.rs's
/// park. Returns when the parent is orphaned (FIELD-FINDING-#13b parity)
/// — OS-default signal handling otherwise (Supervisors drop ⇒ RAs
/// reaped).
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

    crate::ui::wait("repo-scoped Model R daemon up. Ctrl-C / SIGTERM to stop.");

    // ---- the serve loop (single owner ⇒ Judgment A holds composed) ---
    loop {
        if parent.orphaned() {
            crate::ui::warn("parent process exited — shutting down (FIELD FINDING #13b parity).");
            return ExitCode::SUCCESS;
        }

        // (v) respawn-staleness closure: the SOLE site a cluster's
        // LspClient is (re)set — reset() the multiplexer here, before any
        // subsequent switch_to for that cluster.
        while let Ok(Ctrl::Spawned(h, client)) = ctrl_rx.try_recv() {
            if let Some(cs) = clusters.get_mut(&h) {
                cs.mux.reset();
                cs.lsp = Some(client);
            }
        }

        // Drain forwarded RA events → the owning cluster's ClusterDriver.
        while let Ok((h, ev)) = lsp_rx.try_recv() {
            if clusters.contains_key(&h) {
                step(&mut clusters, &h, DriverEvent::Lsp(ev), &pending_batch);
            }
        }

        // Routed file-changes: raw_repo_watch yields changed absolute
        // paths (Receiver<PathBuf>); feed them straight into the proven
        // RepoWatchRouter (it owns §4 routing + target/.git floor +
        // per-WT debounce). Drain any burst non-blocking after the first
        // so a save-storm coalesces into one debounced batch.
        let now = Instant::now();
        match raw_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(path) => {
                repo_watch.record(&path, now);
                while let Ok(p) = raw_rx.try_recv() {
                    repo_watch.record(&p, now);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return ExitCode::SUCCESS,
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
            }
            step(
                &mut clusters,
                &h,
                DriverEvent::RoutedBatch { wt: wt.clone() },
                &pending_batch,
            );
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
                    );
                }
                if let LifecycleAction::TeardownRa(_) = lifecycle.deactivate(path_key(&wt)) {
                    clusters.remove(&h); // Supervisor drop ⇒ RA reaped
                }
            }
        }
    }
}

/// WtId (PathBuf) → the `String` key `ClusterLifecycle` uses.
fn path_key(wt: &WtId) -> String {
    wt.to_string_lossy().into_owned()
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
            lsp: None,
            mux: OverlayMultiplexer::new(),
            driver: ClusterDriver::new(),
            next_ver: 2,
        },
    );
}

/// Feed one `DriverEvent` to a cluster's `ClusterDriver` and faithfully
/// execute the resulting `ClusterAction` (+ any post-settle follow-up).
fn step(
    clusters: &mut BTreeMap<WorkspaceConfigHash, ClusterState>,
    h: &WorkspaceConfigHash,
    ev: DriverEvent,
    pending_batch: &BTreeMap<WtId, Vec<PathBuf>>,
) {
    let Some(cs) = clusters.get_mut(h) else {
        return;
    };
    let action = cs.driver.on_event(ev);
    exec(cs, action, pending_batch);
    while let Some(followup) = cs.driver.take_followup() {
        exec(cs, followup, pending_batch);
    }
}

/// Execute one `ClusterAction` (faithful composition — surface (ii)).
fn exec(
    cs: &mut ClusterState,
    action: ClusterAction,
    pending_batch: &BTreeMap<WtId, Vec<PathBuf>>,
) {
    match action {
        ClusterAction::Idle => {}
        ClusterAction::SwitchOverlay { wt } => {
            let Some(lsp) = cs.lsp.clone() else {
                return; // RA not handshaked yet; a later batch retries
            };
            // Build the WT's overlay from its changed files' on-disk
            // content (v0: empty ⇒ base/on-disk, still correct).
            let mut pairs: Vec<(String, String)> = Vec::new();
            if let Some(files) = pending_batch.get(&wt) {
                for f in files {
                    if let Ok(text) = std::fs::read_to_string(f) {
                        pairs.push((f.to_string_lossy().into_owned(), text));
                    }
                }
            }
            let target = OverlaySet::from_pairs(pairs.iter().map(|(p, c)| (p.clone(), c.clone())));
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
            // Trigger the flycheck the barrier waits on. Save a real
            // file of the WT (first changed file, else its Cargo.toml).
            let save = pending_batch
                .get(&wt)
                .and_then(|f| f.first().cloned())
                .unwrap_or_else(|| wt.join("Cargo.toml"));
            let _ = lsp.did_save(&save.to_string_lossy());
        }
        ClusterAction::EmitVerdict {
            wt,
            authoritative_error,
        } => {
            // THE sole verdict-attribution site (Judgment B as composed).
            publish_verdict(&wt, authoritative_error);
        }
    }
}

/// Write `wt`'s per-worktree statusfile verdict — the only place a
/// verdict is attributed/published in the whole wire.
fn publish_verdict(wt: &Path, authoritative_error: bool) {
    let verdict = if authoritative_error {
        Verdict::Red
    } else {
        Verdict::Green
    };
    let now = statusfile::now_unix();
    let st = Status {
        pid: std::process::id(),
        root: wt.to_string_lossy().into_owned(),
        started: now,
        updated: now,
        verdict_str: verdict.as_str().to_string(),
        crates: Vec::new(),
        red_diagnostics: 0,
    };
    statusfile::write(wt, &st);
}
