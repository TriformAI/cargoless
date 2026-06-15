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
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cargoless_core::appbuild::{self, BuildReport, InstancePaths as BuildPaths};
use cargoless_core::appdrv::{
    Backends, BuildBackend, ChildHandle, ChildLauncher, Driver, EventSink, InstanceConfig,
    InstancePaths, PortAllocator,
};
use cargoless_core::appinstances::{InstanceSpec, load_instances};
use cargoless_core::appmanifest::load_app_manifest;
use cargoless_core::appstate::{Event, Generation};
use cargoless_core::appsvc::AppServeState;
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
    /// Per-instance health spec + run command. Refreshed from `cargoless.app.yaml`
    /// on every green build so a manifest change (e.g. bumping
    /// `health.ready_timeout_ms`) takes effect on the *next* build, not at daemon
    /// restart. Protected by a `Mutex` because `start_probe` threads read it while
    /// the control thread may be writing a new plan.
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

    /// Refresh the run plan for `instance` from `cargoless.app.yaml` in
    /// `worktree` (falling back to `repo`, then the built-in default). Called
    /// from the control thread on every green build so a manifest change
    /// (e.g. bumping `health.ready_timeout_ms`) takes effect on the next
    /// spawn+probe cycle without a daemon restart.
    ///
    /// **Fail-safe**: if the new manifest is missing, unreadable, or invalid
    /// the *previous* plan is kept unchanged — a bad manifest push never
    /// disrupts a currently-serving instance.
    fn refresh_plan(&self, instance: &str, worktree: &Path, repo: &Path) {
        refresh_plan_into(
            &mut self.plans.lock().expect("plans"),
            instance,
            worktree,
            repo,
        );
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
        let plans = self.plans.lock().expect("plans");
        let plan = plans
            .get(instance)
            .ok_or_else(|| format!("no run plan for instance `{instance}`"))?
            .clone();
        drop(plans);
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
    let mut proxies: Vec<L4Proxy> = Vec::new();
    let mut instance_configs = Vec::new();
    let mut run_plans: BTreeMap<String, RunPlan> = BTreeMap::new();
    for spec in &specs {
        let proxy = L4Proxy::bind(spec.app_bind, holding.clone())
            .map_err(|e| format!("instance `{}` proxy bind {}: {e}", spec.name, spec.app_bind))?;
        // The driver flips the SAME slot Arc the proxy reads on every accept,
        // so a promote is visible to new connections immediately.
        let paths = InstancePaths {
            worktree: instance_worktree(state_dir, &spec.name),
            bundles: state_dir.join("app").join(&spec.name).join("bundles"),
        };
        instance_configs.push(InstanceConfig {
            name: spec.name.clone(),
            slot: proxy.slot().clone(),
            paths,
            env: spec
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        });
        // Build the run plan from the instance's `cargoless.app.yaml`: how to
        // run the harvested app (command, port_env), how to health-probe it,
        // and the drain grace. The manifest rides each branch's sha, so read it
        // from the instance worktree first; before the first checkout that dir
        // is empty, so fall back to the daemon's repo checkout, then to the
        // built-in default. (inc-6: previously every instance used the hardcoded
        // `./run.sh` default, which never matched a real manifest — the app
        // failed to spawn with "no such file" even on a green build.)
        let wt = instance_worktree(state_dir, &spec.name);
        let plan = run_plan_from_manifest(&wt)
            .or_else(|| run_plan_from_manifest(repo))
            .unwrap_or_else(default_run_plan);
        run_plans.insert(spec.name.clone(), plan);
        proxies.push(proxy);
    }

    let launcher = Arc::new(ProcLauncher {
        plans: std::sync::Mutex::new(run_plans),
        children: std::sync::Mutex::new(BTreeMap::new()),
        next_token: std::sync::atomic::AtomicU64::new(1),
        events_tx: events_tx.clone(),
    });
    // Keep a second Arc handle so the control loop can call refresh_plan
    // without holding any borrow on `driver` (which owns the first handle).
    let launcher_ref = launcher.clone();
    let build = Arc::new(ThreadBuildBackend {
        events_tx: events_tx.clone(),
    });
    let sink = Arc::new(ChannelSink {
        events_tx: events_tx.clone(),
    });
    let ports = Arc::new(PortAllocator::new(range.start, range.end));

    let mut driver = Driver::new(
        instance_configs,
        Backends {
            build,
            launcher,
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

    // Per-instance git worktrees: each instance builds in its OWN worktree of
    // the main repo (shared object store, independent checked-out tree) so two
    // instances' builds never collide on the working tree. The daemon owns the
    // worktree; `appbuild::checkout` moves it to each build sha. Create it once
    // here, before any build or recovery touches it — otherwise the first
    // checkout fails with "cannot change to <worktree>: No such file or
    // directory" (the build worker assumes the tree exists).
    for spec in &specs {
        let wt = instance_worktree(state_dir, &spec.name);
        if let Err(e) = ensure_instance_worktree(repo, &wt, &spec.git_ref) {
            // Non-fatal: log and continue. That instance's first checkout will
            // surface the failure as a red build (visible in /app), and the
            // other instances still come up.
            ui::warn(format!(
                "instance `{}`: could not set up worktree {}: {e}",
                spec.name,
                wt.display()
            ));
        }
    }

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
    let poll_ms = opts.poll_interval_ms.unwrap_or(2000).max(200);
    for spec in &specs {
        spawn_ref_poller(repo, spec, poll_ms, events_tx.clone());
    }

    // ── the control loop ─────────────────────────────────────────────
    // Single mutator of the driver; 200ms tick polls the shutdown flag.
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
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
                // CGLS-14: on every green build, refresh the run plan from the
                // freshly-checked-out manifest BEFORE the driver dispatches
                // SpawnAndProbe. This ensures a manifest change (e.g. bumping
                // `health.ready_timeout_ms`) takes effect on the next boot cycle
                // without a daemon restart. Fail-safe: a bad/missing manifest
                // leaves the previous plan unchanged (see `refresh_plan_into`).
                if matches!(
                    &event,
                    Event::BuildFinished {
                        outcome: cargoless_core::appstate::AppBuildOutcome::Green,
                        ..
                    }
                ) {
                    let wt = instance_worktree(state_dir, &instance);
                    launcher_ref.refresh_plan(&instance, &wt, repo);
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

/// Pure core of plan refresh: update the map entry for `instance` from the
/// manifest in `worktree` (falling back to `repo`, then the existing entry,
/// then the built-in default). Called from [`ProcLauncher::refresh_plan`] on
/// every green build so a manifest change takes effect on the next spawn cycle.
///
/// **Fail-safe rule**: if the new manifest is missing, unreadable, or invalid,
/// the existing entry is left *unchanged* — a bad manifest push never disrupts
/// a currently-serving instance. The fallback chain is:
/// 1. `worktree/cargoless.app.yaml` (the freshly-checked-out sha)
/// 2. `repo/cargoless.app.yaml` (the daemon's base checkout)
/// 3. Existing plan (new: fail-safe — bad manifest leaves the old plan in place)
/// 4. Built-in default (only if there was no existing plan at all)
fn refresh_plan_into(
    plans: &mut BTreeMap<String, RunPlan>,
    instance: &str,
    worktree: &Path,
    repo: &Path,
) {
    if let Some(new_plan) =
        run_plan_from_manifest(worktree).or_else(|| run_plan_from_manifest(repo))
    {
        plans.insert(instance.to_string(), new_plan);
    }
    // else: keep the existing plan (fail-safe — bad or absent manifest after a
    // green build leaves the proven-working plan in place).
    // If there was no prior plan at all, insert the default so spawn never fails.
    plans
        .entry(instance.to_string())
        .or_insert_with(default_run_plan);
}

/// Build a [`RunPlan`] from a `cargoless.app.yaml` under `root`, if present.
/// `None` ⇒ the repo has no manifest (or it is unreadable/invalid) and the
/// caller should fall back to another root or the default. The manifest's
/// run command + port_env, health path/timeouts, and drain grace map directly
/// onto the launcher's run plan (the manifest's `run.env` rides the app via the
/// instance env overlay, not the plan, so it is not duplicated here).
fn run_plan_from_manifest(root: &Path) -> Option<RunPlan> {
    let manifest = match load_app_manifest(root) {
        Ok(Some(m)) => m,
        Ok(None) => return None,
        Err(e) => {
            ui::warn(format!(
                "manifest {} at {}: {} — falling back",
                cargoless_core::appmanifest::APP_MANIFEST_NAME,
                root.display(),
                e.message
            ));
            return None;
        }
    };
    Some(RunPlan {
        command: manifest.run.command,
        port_env: manifest.run.port_env,
        health_path: manifest.health.path,
        ready_timeout_ms: manifest.health.ready_timeout_ms,
        interval_ms: manifest.health.interval_ms,
        grace_ms: manifest.drain.grace_ms,
    })
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

/// One ref poller: resolve `spec.git_ref` to a sha on an interval, post
/// HeadAdvanced when it changes. Uses `git rev-parse` (the same git the build
/// worker uses); a transient git error is skipped (next tick retries).
fn spawn_ref_poller(repo: &Path, spec: &InstanceSpec, poll_ms: u64, tx: Sender<(String, Event)>) {
    let (repo, name, git_ref) = (repo.to_path_buf(), spec.name.clone(), spec.git_ref.clone());
    std::thread::spawn(move || {
        let mut last: Option<String> = None;
        loop {
            if SHUTDOWN.load(Ordering::SeqCst) {
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

    // ── CGLS-14: per-build RunPlan refresh ───────────────────────────────

    /// Helper: write a minimal manifest with the given `ready_timeout_ms` into
    /// `dir/cargoless.app.yaml` and return `dir`.
    fn write_manifest(dir: &std::path::Path, ready_timeout_ms: u64) {
        std::fs::write(
            dir.join("cargoless.app.yaml"),
            format!(
                "version: 1\nrun:\n  command: [\"./app\"]\nhealth:\n  ready_timeout_ms: {ready_timeout_ms}\n"
            ),
        )
        .unwrap();
    }

    /// `refresh_plan_into` picks up a new `ready_timeout_ms` from a worktree
    /// manifest on a second call — simulates a build that checked out a new sha
    /// whose manifest has a different health timeout.
    #[test]
    fn refresh_plan_into_adopts_new_manifest_values() {
        let tmp =
            std::env::temp_dir().join(format!("cargoless-cgls14-refresh-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let wt = tmp.join("worktree");
        std::fs::create_dir_all(&wt).unwrap();
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        // First build: manifest sets timeout = 120_000 (the default).
        write_manifest(&wt, 120_000);
        let mut plans: BTreeMap<String, RunPlan> = BTreeMap::new();
        refresh_plan_into(&mut plans, "dev", &wt, &repo);
        assert_eq!(
            plans["dev"].ready_timeout_ms, 120_000,
            "first build: timeout from manifest"
        );

        // Second build: branch bumps timeout to 600_000 in the manifest.
        write_manifest(&wt, 600_000);
        refresh_plan_into(&mut plans, "dev", &wt, &repo);
        assert_eq!(
            plans["dev"].ready_timeout_ms, 600_000,
            "second build: updated timeout is adopted from the new manifest"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `refresh_plan_into` keeps the *prior* plan when the manifest is absent
    /// or invalid — a bad push never disrupts a currently-serving instance.
    #[test]
    fn refresh_plan_into_keeps_prior_on_bad_manifest() {
        let tmp =
            std::env::temp_dir().join(format!("cargoless-cgls14-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let wt = tmp.join("worktree");
        std::fs::create_dir_all(&wt).unwrap();
        let repo = tmp.join("repo"); // no manifest here either
        std::fs::create_dir_all(&repo).unwrap();

        // Establish an initial plan with a distinctive timeout.
        let initial = RunPlan {
            ready_timeout_ms: 999_000,
            ..default_run_plan()
        };
        let mut plans: BTreeMap<String, RunPlan> = BTreeMap::new();
        plans.insert("dev".to_string(), initial);

        // Worktree has an invalid manifest (bad YAML / unknown key).
        std::fs::write(
            wt.join("cargoless.app.yaml"),
            "version: 1\nunknown_key: bad\nrun:\n  command: [\"./app\"]\n",
        )
        .unwrap();

        // Refresh: manifest is invalid, repo also has no manifest → prior plan
        // must be preserved exactly.
        refresh_plan_into(&mut plans, "dev", &wt, &repo);
        assert_eq!(
            plans["dev"].ready_timeout_ms, 999_000,
            "bad manifest must not clobber the prior plan"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// When there is no prior plan AND the manifest is absent, `refresh_plan_into`
    /// inserts the built-in default so spawn never fails with "no run plan".
    #[test]
    fn refresh_plan_into_inserts_default_when_no_prior_and_no_manifest() {
        let tmp =
            std::env::temp_dir().join(format!("cargoless-cgls14-noprior-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let wt = tmp.join("worktree");
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&repo).unwrap();

        let mut plans: BTreeMap<String, RunPlan> = BTreeMap::new();
        refresh_plan_into(&mut plans, "dev", &wt, &repo);
        // The built-in default must have been inserted.
        assert!(plans.contains_key("dev"), "default plan inserted");
        assert_eq!(
            plans["dev"].ready_timeout_ms,
            default_run_plan().ready_timeout_ms
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
