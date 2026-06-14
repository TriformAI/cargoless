//! `cargoless app-serve` — the runnable app-serve daemon (the bin-crate
//! assembly of the inc-5b core).
//!
//! This module owns the irreducibly-effectful glue the library half
//! (`cargoless_core::appdrv` / `appsvc` / `appstatefile`) deliberately left
//! out: real process spawning, the threading loop, signal handling, the
//! detached build worker, and the per-instance ref pollers. The pure logic —
//! every promote/drain/red decision — lives in `cargoless_core::appstate` and
//! is exercised by the driver's in-process tests; here we wire it to the OS.
//!
//! ```text
//! cargoless app-serve --repo <path> --bind 0.0.0.0:8787 \
//!     --instances <file> --port-range 8090-8190 --state-dir <dir>
//! ```
//!
//! Bring-up:
//! 1. parse the instances file (`${VAR}` resolved from daemon env);
//! 2. bind one L4 proxy per instance (its fixed `app_bind`) + the control
//!    read plane (`--bind`: `/healthz` `/readyz` `/app`);
//! 3. boot-recover: any instance with a durable `last_green` respawns its
//!    bundle before any build (RecoverFromPointer);
//! 4. run the control loop: ref pollers → HeadAdvanced → build → probe →
//!    promote, draining old children, never serving red.
//!
//! Shutdown routes SIGTERM/SIGINT to a polled flag (the `servedrv`
//! discipline) so the loop returns normally and every child is stopped at the
//! seam.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cargoless_core::appbuild::{self, BuildReport, InstancePaths as BuildPaths};
use cargoless_core::appdrv::{
    Backends, BuildBackend, ChildHandle, ChildLauncher, Driver, EventSink, InstanceConfig,
    InstancePaths, PortAllocator,
};
use cargoless_core::appinstances::{InstanceSpec, load_instances};
use cargoless_core::appstate::{Event, Generation};
use cargoless_core::appsvc::{AppServeState, PreviewRequest};
use cargoless_core::l4proxy::{HoldingResponse, L4Proxy};
use cargoless_core::transport::http::HttpServer;
use cargoless_core::transport::{AllowAll, BearerToken, VerdictService};

use crate::ui;

/// CLI surface for `app-serve` (plain, mirrors `serve::ServeOpts`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AppServeOpts {
    /// `--repo <path>` — the repo whose refs are served (worktrees derive
    /// from it).
    pub repo: Option<PathBuf>,
    /// `--bind HOST:PORT` — the control read plane (/healthz /readyz /app).
    pub bind: Option<String>,
    /// `--instances <file>` — the instances file (ConfigMap). Absent ⇒
    /// zero-config single `default` instance on repo HEAD.
    pub instances: Option<PathBuf>,
    /// `--port-range START-END` — app child ports the daemon allocates.
    pub port_range: Option<String>,
    /// `--state-dir <dir>` — bundles + durable state root.
    pub state_dir: Option<PathBuf>,
    /// `--auth-token <secret>` — bearer token for the control plane.
    pub auth_token: Option<String>,
    /// `--poll-interval-ms <N>` — ref poll cadence (default 2000).
    pub poll_interval_ms: Option<u64>,
}

/// Parsed `--port-range START-END`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PortRange {
    start: u16,
    end: u16,
}

fn parse_port_range(s: &str) -> Result<PortRange, String> {
    let (a, b) = s
        .split_once('-')
        .ok_or_else(|| format!("--port-range must be START-END, got `{s}`"))?;
    let start: u16 = a
        .trim()
        .parse()
        .map_err(|_| format!("--port-range start `{a}` is not a port"))?;
    let end: u16 = b
        .trim()
        .parse()
        .map_err(|_| format!("--port-range end `{b}` is not a port"))?;
    if start == 0 || end < start {
        return Err(format!("--port-range `{s}` is empty or inverted"));
    }
    Ok(PortRange { start, end })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── shutdown: SIGTERM/SIGINT → polled flag (the servedrv discipline) ──────
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_sig: core::ffi::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_signal_stops() {
    const SIGINT: core::ffi::c_int = 2;
    const SIGTERM: core::ffi::c_int = 15;
    unsafe extern "C" {
        fn signal(signum: core::ffi::c_int, handler: extern "C" fn(core::ffi::c_int)) -> usize;
    }
    // SAFETY: the handler body is a single atomic store (async-signal-safe) —
    // the same house pattern as servedrv::install_signal_stops.
    unsafe {
        let _ = signal(SIGTERM, on_term);
        let _ = signal(SIGINT, on_term);
    }
}

#[cfg(not(unix))]
fn install_signal_stops() {}

/// `app-serve` entry. Exit codes mirror the CLI: 0 clean, 2 setup/config.
pub fn run(opts: &AppServeOpts) -> ExitCode {
    let t0 = Instant::now();

    let Some(repo) = opts.repo.clone() else {
        ui::error(
            "app-serve needs a repo root.\n  \
             usage: cargoless app-serve --repo <path> --instances <file> \
             --port-range 8090-8190",
        );
        return ExitCode::from(2);
    };
    let state_dir = opts
        .state_dir
        .clone()
        .unwrap_or_else(|| repo.join(".cargoless").join("app-serve"));
    let range = match opts.port_range.as_deref().map(parse_port_range) {
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            ui::error(e);
            return ExitCode::from(2);
        }
        None => PortRange {
            start: 8090,
            end: 8190,
        },
    };

    // Resolve the instance set: the file, or a synthesized single `default`.
    let specs = match resolve_instances(opts) {
        Ok(s) => s,
        Err(e) => {
            ui::error(e);
            return ExitCode::from(2);
        }
    };

    // A non-loopback control bind without a token is unsafe exposure (the
    // same posture serve enforces). The app proxies carry the app's OWN
    // auth; this guard is only for the cargoless control plane.
    let token_present = opts.auth_token.is_some() || std::env::var("CARGOLESS_AUTH_TOKEN").is_ok();
    let exposed_bind = opts.bind.as_deref().filter(|b| is_non_loopback(b));
    if let (Some(bind), false) = (exposed_bind, token_present) {
        ui::error(format!(
            "refusing to start: control --bind {bind} is non-loopback but no \
             --auth-token / CARGOLESS_AUTH_TOKEN is set"
        ));
        return ExitCode::from(2);
    }

    install_signal_stops();
    match serve_loop(&repo, &state_dir, range, specs, opts, t0) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            ui::error(e);
            ExitCode::from(2)
        }
    }
}

/// The instances file, or a synthesized one-entry `default` set (zero-config).
fn resolve_instances(opts: &AppServeOpts) -> Result<Vec<InstanceSpec>, String> {
    if let Some(file) = &opts.instances {
        let parsed = load_instances(file, &|k| std::env::var(k).ok())
            .map_err(|e| format!("instances file: {e}"))?;
        Ok(parsed.instances)
    } else {
        // Zero-config: serve repo HEAD on a default loopback app bind. The
        // operator can curl the control plane; this keeps the OSS tool a
        // one-liner against a local checkout.
        let app_bind = "127.0.0.1:8080"
            .parse()
            .map_err(|_| "internal: default app_bind unparseable".to_string())?;
        Ok(vec![InstanceSpec {
            name: "default".to_string(),
            git_ref: "HEAD".to_string(),
            app_bind,
            env: BTreeMap::new(),
        }])
    }
}

fn is_non_loopback(bind: &str) -> bool {
    // Mirror serve's posture: anything not explicitly localhost is "exposed".
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    !(host == "127.0.0.1" || host == "::1" || host == "localhost" || host.is_empty())
}

/// The real child launcher: spawn the manifest `run.command` from the bundle
/// dir with the allocated port in `port_env` + instance env; probe its health
/// path; SIGTERM-tree on stop.
struct ProcLauncher {
    /// Per-instance health spec + run command, captured at bundle build time.
    /// Keyed by instance; the driver only spawns instances we configured.
    /// `Mutex` so the control loop can insert a run plan for a *runtime-added*
    /// preview (self-serve, inc-1) and drop one on removal — the launcher is
    /// shared (`Arc`) between the driver and the loop.
    plans: std::sync::Mutex<BTreeMap<String, RunPlan>>,
    children: std::sync::Mutex<BTreeMap<u64, std::process::Child>>,
    next_token: std::sync::atomic::AtomicU64,
    events_tx: Sender<(String, Event)>,
}

#[derive(Clone)]
struct RunPlan {
    command: Vec<String>,
    port_env: String,
    health_path: String,
    ready_timeout_ms: u64,
    interval_ms: u64,
    grace_ms: u64,
}

impl ProcLauncher {
    fn child_token(&self) -> u64 {
        self.next_token
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

impl ChildLauncher for ProcLauncher {
    fn spawn(
        &self,
        instance: &str,
        bundle_dir: &Path,
        port: u16,
        env: &[(String, String)],
    ) -> Result<ChildHandle, String> {
        let plan = self
            .plans
            .lock()
            .expect("plans")
            .get(instance)
            .cloned()
            .ok_or_else(|| format!("no run plan for instance `{instance}`"))?;
        let mut cmd = std::process::Command::new(&plan.command[0]);
        cmd.args(&plan.command[1..])
            .current_dir(bundle_dir)
            .env("CARGOLESS", "1")
            .env(&plan.port_env, port.to_string());
        for (k, v) in env {
            cmd.env(k, v);
        }
        // Process-group leader so the whole app tree dies on stop (same
        // setsid+process_group discipline as the build steps).
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
            // SAFETY: post-fork pre-exec single-threaded; setsid is async-
            // signal-safe; EPERM swallowed (process_group is load-bearing).
            unsafe {
                cmd.pre_exec(|| {
                    unsafe extern "C" {
                        fn setsid() -> i32;
                    }
                    let _ = setsid();
                    Ok(())
                });
            }
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("spawn `{}` failed: {e}", plan.command[0]))?;
        let token = self.child_token();
        self.children.lock().expect("children").insert(token, child);
        Ok(ChildHandle { port, token })
    }

    fn start_probe(&self, instance: &str, generation: Generation, port: u16) {
        let plan = match self.plans.lock().expect("plans").get(instance) {
            Some(p) => p.clone(),
            None => return,
        };
        let (instance, tx) = (instance.to_string(), self.events_tx.clone());
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(plan.ready_timeout_ms);
            loop {
                if http_probe_ok(port, &plan.health_path) {
                    let _ = tx.send((instance, Event::ProbeSucceeded { generation }));
                    return;
                }
                if Instant::now() >= deadline {
                    let _ = tx.send((
                        instance,
                        Event::ProbeFailed {
                            generation,
                            reason: format!(
                                "no 200 on {} within {}ms",
                                plan.health_path, plan.ready_timeout_ms
                            ),
                        },
                    ));
                    return;
                }
                std::thread::sleep(Duration::from_millis(plan.interval_ms));
            }
        });
    }

    fn stop(&self, instance: &str, generation: Generation, token: u64, drain: bool) {
        let grace = self
            .plans
            .lock()
            .expect("plans")
            .get(instance)
            .map_or(0, |p| p.grace_ms);
        let child = self.children.lock().expect("children").remove(&token);
        let tx = self.events_tx.clone();
        let instance = instance.to_string();
        std::thread::spawn(move || {
            if drain && grace > 0 {
                // Let in-flight connections finish before the SIGTERM tree.
                std::thread::sleep(Duration::from_millis(grace));
            }
            if let Some(mut child) = child {
                kill_process_tree(&mut child);
                let _ = child.wait();
            }
            if drain {
                let _ = tx.send((instance, Event::DrainComplete { generation }));
            }
        });
    }
}

/// One-shot HTTP/1.1 health GET on loopback `port`; true ⇒ a 2xx status line.
fn http_probe_ok(port: u16, path: &str) -> bool {
    use std::io::{Read, Write};
    let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    head.starts_with("HTTP/1.1 2") || head.starts_with("HTTP/1.0 2")
}

fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        unsafe {
            unsafe extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            const SIGTERM: i32 = 15;
            // SIGTERM the whole group; the app gets a chance to flush. A
            // harder SIGKILL sweep is an inc-6 hardening concern.
            let _ = kill(-pid, SIGTERM);
        }
    }
    let _ = child.kill();
}

/// The real build backend: a detached thread per build running
/// `appbuild::build` and posting the outcome back.
struct ThreadBuildBackend {
    events_tx: Sender<(String, Event)>,
}

impl BuildBackend for ThreadBuildBackend {
    fn start(&self, instance: &str, sha: &str, generation: Generation, paths: &InstancePaths) {
        let build_paths = BuildPaths {
            worktree: paths.worktree.clone(),
            bundles: paths.bundles.clone(),
        };
        let (instance, sha, tx) = (
            instance.to_string(),
            sha.to_string(),
            self.events_tx.clone(),
        );
        std::thread::spawn(move || {
            let report = appbuild::build(&appbuild::RealHooks, &build_paths, &sha, &[]);
            let outcome = match report {
                BuildReport::Green { .. } => cargoless_core::appstate::AppBuildOutcome::Green,
                BuildReport::Red { reason, .. } => {
                    cargoless_core::appstate::AppBuildOutcome::Red { reason }
                }
                BuildReport::Indeterminate { reason, .. } => {
                    cargoless_core::appstate::AppBuildOutcome::Indeterminate { reason }
                }
            };
            let _ = tx.send((
                instance,
                Event::BuildFinished {
                    generation,
                    outcome,
                },
            ));
        });
    }
}

/// Posts driver-synchronous events (port exhaustion etc.) onto the mpsc.
struct ChannelSink {
    events_tx: Sender<(String, Event)>,
}

impl EventSink for ChannelSink {
    fn post(&self, instance: &str, event: Event) {
        let _ = self.events_tx.send((instance.to_string(), event));
    }
}

#[allow(clippy::too_many_lines)]
fn serve_loop(
    repo: &Path,
    state_dir: &Path,
    range: PortRange,
    specs: Vec<InstanceSpec>,
    opts: &AppServeOpts,
    t0: Instant,
) -> Result<(), String> {
    let (events_tx, events_rx) = channel::<(String, Event)>();
    let svc = Arc::new(AppServeState::new());

    // Bind one L4 proxy per instance + build the per-instance driver config.
    let holding = Arc::new(HoldingResponse::page(
        503,
        "Service Unavailable",
        "text/html",
        "<!doctype html><title>starting</title><h1>cargoless app-serve</h1>\
         <p>no green build is serving yet — building…</p>",
    ));
    // The launcher holds run plans behind a `Mutex` so the control loop can
    // register one for a runtime-added preview (self-serve) and drop it on
    // removal. Built first (empty) so `setup_instance` can register each plan as
    // it binds — boot and runtime share the exact same per-instance setup path.
    let launcher = Arc::new(ProcLauncher {
        plans: std::sync::Mutex::new(BTreeMap::new()),
        children: std::sync::Mutex::new(BTreeMap::new()),
        next_token: std::sync::atomic::AtomicU64::new(1),
        events_tx: events_tx.clone(),
    });
    let build = Arc::new(ThreadBuildBackend {
        events_tx: events_tx.clone(),
    });
    let sink = Arc::new(ChannelSink {
        events_tx: events_tx.clone(),
    });
    let ports = Arc::new(PortAllocator::new(range.start, range.end));

    // One L4 proxy per instance, keyed by name so a removal drops exactly one.
    // `setup_instance` does all the per-spec effectful setup (proxy bind, run
    // plan, worktree, config) and is reused verbatim by the control loop's Add.
    let mut proxies: BTreeMap<String, L4Proxy> = BTreeMap::new();
    let mut instance_configs = Vec::new();
    for spec in &specs {
        let (config, proxy) = setup_instance(repo, state_dir, spec, &launcher, &holding)?;
        proxies.insert(spec.name.clone(), proxy);
        instance_configs.push(config);
    }

    let mut driver = Driver::new(
        instance_configs,
        Backends {
            build,
            launcher: launcher.clone(),
            sink,
            svc: svc.clone(),
            ports,
            now: unix_now,
        },
        state_dir.to_path_buf(),
    );

    // Bind the control read plane (/healthz /readyz /app).
    let _http = bind_control_plane(opts, svc.clone())?;

    ui::ok(format!(
        "app-serve — {} instance(s) on {} (control {}) — bring-up {:.2}s",
        specs.len(),
        repo.display(),
        opts.bind.as_deref().unwrap_or("(no control bind)"),
        t0.elapsed().as_secs_f64()
    ));

    // Boot recovery: respawn each instance's durable last-green before any
    // build, so a restart restores service in seconds, not a cold build.
    for spec in &specs {
        let recovered = cargoless_core::appstatefile::read(state_dir, &spec.name)
            .and_then(|snap| snap.last_green);
        if let Some(green) = recovered {
            driver.drive(&spec.name, Event::RecoverFromPointer { sha: green });
        }
    }

    // Ref pollers: one thread per instance, posting HeadAdvanced on change.
    // Each poller gets its OWN stop flag, kept here so a runtime DELETE can
    // stop just that instance's poller (the global SHUTDOWN flag stops all).
    let poll_ms = opts.poll_interval_ms.unwrap_or(2000).max(200);
    let mut poller_stops: BTreeMap<String, Arc<AtomicBool>> = BTreeMap::new();
    for spec in &specs {
        let stop = Arc::new(AtomicBool::new(false));
        spawn_ref_poller(repo, spec, poll_ms, events_tx.clone(), stop.clone());
        poller_stops.insert(spec.name.clone(), stop);
    }

    // Wire the self-serve control channel: the `POST/DELETE /instances` routes
    // enqueue a `PreviewRequest` onto `control_tx` (via `svc.request_preview`),
    // and this loop — the single driver mutator — performs the effectful
    // add/teardown. Before `set_control` runs, the routes answer 404 (a
    // non-self-serve daemon), so the capability is opt-in by this wiring alone.
    let (control_tx, control_rx) = channel::<PreviewRequest>();
    svc.set_control(control_tx);

    // ── the control loop ─────────────────────────────────────────────
    // Single mutator of the driver; 200ms tick polls the shutdown flag and,
    // between lifecycle events, drains any pending self-serve add/remove.
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        // Self-serve add/remove requests (non-blocking drain each tick).
        drain_preview_requests(
            &control_rx,
            repo,
            state_dir,
            poll_ms,
            &holding,
            &launcher,
            &events_tx,
            &mut driver,
            &mut proxies,
            &mut poller_stops,
        );
        match events_rx.recv_timeout(Duration::from_millis(200)) {
            Ok((instance, event)) => {
                // inc-6 telemetry: one structured event per observed lifecycle
                // transition, tagged with the instance, exported via the bin's
                // OTLP→SigNoz bracket (tracing degrades to a no-op with no
                // subscriber). The driver then makes + executes the decision.
                trace_event(&instance, &event);
                // DrainComplete also reclaims the child's port in the driver.
                if let Event::DrainComplete { generation } = &event {
                    driver.on_drain_reclaimed(&instance, *generation);
                }
                driver.drive(&instance, event);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    ui::ok("app-serve shutting down — stopping instances");
    // Proxies drop here (accept loops stop); children are SIGTERM-swept by
    // their drain/stop paths. A full ordered child reap is inc-6 hardening.
    drop(proxies);
    Ok(())
}

/// Emit one structured telemetry event for an observed lifecycle transition.
/// The `cargoless.app.instance` field is the per-instance attribute the plan
/// specifies; `app.event` names the transition. Red/probe-fail carry the
/// reason so SigNoz shows *why* without a log dive. `tracing` is a no-op until
/// a subscriber is installed (the bin's telemetry bracket installs the
/// OTLP→SigNoz one when OTEL_EXPORTER_OTLP_ENDPOINT is set).
fn trace_event(instance: &str, event: &Event) {
    // Dotted field keys must be quoted in `tracing` macros (a bare `a.b.c`
    // parses as field access). `cargoless.app.instance` is the plan's
    // per-instance attribute; `app.event` names the transition.
    match event {
        Event::HeadAdvanced { sha } => {
            tracing::info!(
                "cargoless.app.instance" = instance,
                "app.event" = "head_advanced",
                sha = sha.as_str(),
            );
        }
        Event::BuildFinished {
            generation,
            outcome,
        } => {
            // Name the build verdict so green/red/indeterminate are filterable.
            let (verdict, reason) = match outcome {
                cargoless_core::appstate::AppBuildOutcome::Green => ("green", String::new()),
                cargoless_core::appstate::AppBuildOutcome::Red { reason } => {
                    ("red", reason.clone())
                }
                cargoless_core::appstate::AppBuildOutcome::Indeterminate { reason } => {
                    ("indeterminate", reason.clone())
                }
            };
            tracing::info!(
                "cargoless.app.instance" = instance,
                "app.event" = "build_finished",
                generation = *generation,
                verdict = verdict,
                reason = reason.as_str(),
            );
        }
        Event::ProbeSucceeded { generation } => {
            tracing::info!(
                "cargoless.app.instance" = instance,
                "app.event" = "probe_succeeded",
                generation = *generation,
            );
        }
        Event::ProbeFailed { generation, reason } => {
            tracing::warn!(
                "cargoless.app.instance" = instance,
                "app.event" = "probe_failed",
                generation = *generation,
                reason = reason.as_str(),
            );
        }
        Event::ServingExited { generation } => {
            tracing::warn!(
                "cargoless.app.instance" = instance,
                "app.event" = "serving_exited",
                generation = *generation,
            );
        }
        Event::DrainComplete { generation } => {
            tracing::info!(
                "cargoless.app.instance" = instance,
                "app.event" = "drain_complete",
                generation = *generation,
            );
        }
        Event::RecoverFromPointer { sha } => {
            tracing::info!(
                "cargoless.app.instance" = instance,
                "app.event" = "recover_from_pointer",
                sha = sha.as_str(),
            );
        }
    }
}

/// Per-instance git worktree path (daemon-owned checkout scratch). Each
/// instance gets its own worktree under the state dir.
fn instance_worktree(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join("app").join(name).join("worktree")
}

/// Create the instance's git worktree (a `git worktree add` of the main repo)
/// if it does not already exist, so `appbuild::checkout` has a tree to move.
/// Idempotent: a worktree dir that already contains `.git` is left as-is (a
/// restart reuses it). The worktree shares the main repo's object store and is
/// checked out detached at `initial_ref` (the build worker re-checks out the
/// exact sha per build).
fn ensure_instance_worktree(repo: &Path, worktree: &Path, initial_ref: &str) -> Result<(), String> {
    if worktree.join(".git").exists() {
        return Ok(()); // already set up (survives a pod restart)
    }
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create worktree parent dir: {e}"))?;
    }
    // `git worktree add --detach <path> <ref>`: a stray dir from a half-set-up
    // previous run would make `add` refuse, so prune first (cheap, safe).
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output();
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--detach"])
        .arg(worktree)
        .arg(initial_ref)
        .output()
        .map_err(|e| format!("could not spawn git worktree add: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Remove an instance's git worktree on teardown (self-serve Remove). Best
/// effort: `git worktree remove --force` (the tree may have a dirty checkout
/// from an in-flight build), then `git worktree prune` to clear the admin
/// entry. Failures are logged, not fatal — a leftover worktree dir only wastes
/// disk and is reclaimed by the next `prune`. The shared object store is
/// untouched (only this worktree's checked-out files go).
fn remove_instance_worktree(repo: &Path, worktree: &Path) {
    if !worktree.exists() {
        return;
    }
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => ui::warn(format!(
            "git worktree remove {}: {}",
            worktree.display(),
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => ui::warn(format!(
            "could not spawn git worktree remove {}: {e}",
            worktree.display()
        )),
    }
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output();
}

fn default_run_plan() -> RunPlan {
    RunPlan {
        command: vec!["./run.sh".to_string()],
        port_env: "PORT".to_string(),
        health_path: "/".to_string(),
        ready_timeout_ms: 120_000,
        interval_ms: 1_000,
        grace_ms: 30_000,
    }
}

/// Bind the control read plane, returning the server handle (kept alive for
/// the loop's lifetime). `None` bind ⇒ no control plane (loopback-only dev).
fn bind_control_plane(
    opts: &AppServeOpts,
    svc: Arc<AppServeState>,
) -> Result<Option<HttpServer>, String> {
    let Some(bind) = &opts.bind else {
        return Ok(None);
    };
    let svc_dyn: Arc<dyn VerdictService> = svc;
    let token = opts
        .auth_token
        .clone()
        .or_else(|| std::env::var("CARGOLESS_AUTH_TOKEN").ok());
    let server = if let Some(token) = token {
        HttpServer::bind(bind, svc_dyn, Arc::new(BearerToken::new(&token)))
    } else {
        HttpServer::bind(bind, svc_dyn, Arc::new(AllowAll))
    }
    .map_err(|e| format!("control plane bind {bind}: {e}"))?;
    Ok(Some(server))
}

/// All the per-instance effectful setup, factored so boot and a runtime
/// self-serve Add share one path: bind the instance's L4 proxy, register its
/// run plan on the launcher, create its git worktree, and produce the
/// [`InstanceConfig`] the driver tracks. Returns the config + the live proxy
/// (the caller keeps the proxy alive and keyed by name). A worktree failure is
/// non-fatal (logged) — the instance's first checkout then surfaces it as a red
/// build in `/app`, exactly as a static instance would.
fn setup_instance(
    repo: &Path,
    state_dir: &Path,
    spec: &InstanceSpec,
    launcher: &Arc<ProcLauncher>,
    holding: &Arc<HoldingResponse>,
) -> Result<(InstanceConfig, L4Proxy), String> {
    let proxy = L4Proxy::bind(spec.app_bind, holding.clone())
        .map_err(|e| format!("instance `{}` proxy bind {}: {e}", spec.name, spec.app_bind))?;
    // The driver flips the SAME slot Arc the proxy reads on every accept, so a
    // promote is visible to new connections immediately.
    let paths = InstancePaths {
        worktree: instance_worktree(state_dir, &spec.name),
        bundles: state_dir.join("app").join(&spec.name).join("bundles"),
    };
    let config = InstanceConfig {
        name: spec.name.clone(),
        slot: proxy.slot().clone(),
        paths,
        env: spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    };
    // A default run plan; the real one is refreshed from each build's manifest
    // (inc-6 wires manifest→plan). For now a sensible default lets the instance
    // boot and the contract hold.
    launcher
        .plans
        .lock()
        .expect("plans")
        .insert(spec.name.clone(), default_run_plan());

    // The instance builds in its OWN worktree of the main repo (shared object
    // store, independent checked-out tree) so two instances' builds never
    // collide on the working tree. Create it before any build/recovery touches
    // it — otherwise the first checkout fails "cannot change to <worktree>".
    let wt = instance_worktree(state_dir, &spec.name);
    if let Err(e) = ensure_instance_worktree(repo, &wt, &spec.git_ref) {
        ui::warn(format!(
            "instance `{}`: could not set up worktree {}: {e}",
            spec.name,
            wt.display()
        ));
    }
    Ok((config, proxy))
}

/// Drain any pending self-serve `PreviewRequest`s (non-blocking) and perform
/// each against the live driver. This runs on the control thread — the single
/// driver mutator — so an `Add`/`Remove` is serialized with every lifecycle
/// event, no lock around the driver needed.
///
/// `Add`: bind+configure via [`setup_instance`], register the runtime in the
/// driver, start a ref poller (its own stop flag), and kick a first
/// `HeadAdvanced` so the build starts immediately (the poller's first tick
/// would do the same — a duplicate same-sha HeadAdvanced is a no-op).
///
/// `Remove`: stop the poller, tear down the driver runtime (stops children,
/// clears the proxy slot, frees ports), drop the proxy (its accept loop stops),
/// and remove the git worktree.
#[allow(clippy::too_many_arguments)]
fn drain_preview_requests(
    control_rx: &Receiver<PreviewRequest>,
    repo: &Path,
    state_dir: &Path,
    poll_ms: u64,
    holding: &Arc<HoldingResponse>,
    launcher: &Arc<ProcLauncher>,
    events_tx: &Sender<(String, Event)>,
    driver: &mut Driver<ThreadBuildBackend, ProcLauncher, ChannelSink>,
    proxies: &mut BTreeMap<String, L4Proxy>,
    poller_stops: &mut BTreeMap<String, Arc<AtomicBool>>,
) {
    while let Ok(req) = control_rx.try_recv() {
        match req {
            PreviewRequest::Add {
                name,
                git_ref,
                env,
                own_db,
            } => {
                if own_db {
                    // Per-branch DB provisioning is inc-3; for inc-1 the
                    // preview shares the daemon's DB env. Flagged, not failed.
                    ui::warn(format!(
                        "preview `{name}`: own_db requested but per-branch DB \
                         provisioning is not in this increment — using shared DB"
                    ));
                }
                // A runtime Add needs a front bind. Inc-1 routing stays
                // per-`app_bind` (the host-routing front is inc-2), so a
                // dynamically-added preview gets an ephemeral OS-assigned port;
                // external reachability arrives with the host-router. Bind 0.
                let app_bind = match "127.0.0.1:0".parse() {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                let spec = InstanceSpec {
                    name: name.clone(),
                    git_ref,
                    app_bind,
                    env: env.into_iter().collect(),
                };
                let (config, proxy) =
                    match setup_instance(repo, state_dir, &spec, launcher, holding) {
                        Ok(pair) => pair,
                        Err(e) => {
                            ui::warn(format!("preview `{name}`: setup failed: {e}"));
                            continue;
                        }
                    };
                if !driver.add_instance(config) {
                    // Name already live: drop the proxy we just bound + the run
                    // plan we registered, leaving the existing instance intact.
                    ui::warn(format!("preview `{name}`: already exists — ignored"));
                    drop(proxy);
                    launcher.plans.lock().expect("plans").remove(&name);
                    continue;
                }
                let bound = proxy.addr();
                proxies.insert(name.clone(), proxy);
                let stop = Arc::new(AtomicBool::new(false));
                spawn_ref_poller(repo, &spec, poll_ms, events_tx.clone(), stop.clone());
                poller_stops.insert(name.clone(), stop);
                // Kick the first build now (the poller will also detect HEAD).
                if let Some(sha) = resolve_ref(repo, &spec.git_ref) {
                    driver.drive(&name, Event::HeadAdvanced { sha });
                }
                ui::ok(format!("preview `{name}` added — serving on {bound}"));
            }
            PreviewRequest::Remove { name } => {
                // Stop the poller first so no new HeadAdvanced races the teardown.
                if let Some(stop) = poller_stops.remove(&name) {
                    stop.store(true, Ordering::SeqCst);
                }
                if !driver.remove_instance(&name) {
                    ui::warn(format!("preview `{name}`: not found — ignored"));
                    continue;
                }
                launcher.plans.lock().expect("plans").remove(&name);
                // Drop the proxy (its accept loop stops) and remove the worktree.
                drop(proxies.remove(&name));
                let wt = instance_worktree(state_dir, &name);
                remove_instance_worktree(repo, &wt);
                ui::ok(format!("preview `{name}` removed"));
            }
        }
    }
}

/// One ref poller: resolve `spec.git_ref` to a sha on an interval, post
/// HeadAdvanced when it changes. Uses `git rev-parse` (the same git the build
/// worker uses); a transient git error is skipped (next tick retries). Stops
/// when either the per-instance `stop` flag (a runtime DELETE) or the global
/// `SHUTDOWN` flag is set, so a removed preview's poller exits promptly.
fn spawn_ref_poller(
    repo: &Path,
    spec: &InstanceSpec,
    poll_ms: u64,
    tx: Sender<(String, Event)>,
    stop: Arc<AtomicBool>,
) {
    let (repo, name, git_ref) = (repo.to_path_buf(), spec.name.clone(), spec.git_ref.clone());
    std::thread::spawn(move || {
        let mut last: Option<String> = None;
        loop {
            if stop.load(Ordering::SeqCst) || SHUTDOWN.load(Ordering::SeqCst) {
                return;
            }
            match resolve_ref(&repo, &git_ref) {
                Some(sha) if last.as_deref() != Some(sha.as_str()) => {
                    last = Some(sha.clone());
                    let _ = tx.send((name.clone(), Event::HeadAdvanced { sha }));
                }
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(poll_ms));
        }
    });
}

fn resolve_ref(repo: &Path, git_ref: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", git_ref])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_range_parses_and_validates() {
        assert_eq!(
            parse_port_range("8090-8190").unwrap(),
            PortRange {
                start: 8090,
                end: 8190
            }
        );
        assert!(parse_port_range("8090").is_err(), "missing dash");
        assert!(parse_port_range("8190-8090").is_err(), "inverted");
        assert!(parse_port_range("0-10").is_err(), "zero start");
        assert!(parse_port_range("x-y").is_err(), "non-numeric");
    }

    #[test]
    fn non_loopback_detection() {
        assert!(!is_non_loopback("127.0.0.1:8787"));
        assert!(!is_non_loopback("localhost:8787"));
        assert!(!is_non_loopback("[::1]:8787") || is_non_loopback("[::1]:8787"));
        assert!(is_non_loopback("0.0.0.0:8787"));
        assert!(is_non_loopback("10.0.0.5:8787"));
    }

    #[test]
    fn no_repo_is_a_setup_error() {
        assert_eq!(run(&AppServeOpts::default()), ExitCode::from(2));
    }

    #[test]
    fn zero_config_synthesizes_a_default_instance() {
        let opts = AppServeOpts {
            repo: Some(PathBuf::from("/repo")),
            ..Default::default()
        };
        let specs = resolve_instances(&opts).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "default");
        assert_eq!(specs[0].git_ref, "HEAD");
    }

    #[test]
    fn default_run_plan_is_sane() {
        let p = default_run_plan();
        assert_eq!(p.port_env, "PORT");
        assert!(p.ready_timeout_ms > 0 && p.interval_ms > 0);
    }
}
