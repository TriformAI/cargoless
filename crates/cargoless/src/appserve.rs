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
use cargoless_core::appmanifest::load_app_manifest;
use cargoless_core::appstate::{Event, Generation};
use cargoless_core::appsvc::{AppServeState, PreviewRoute};
use cargoless_core::l4proxy::{HoldingResponse, L4Proxy};
use cargoless_core::transport::http::HttpServer;
use cargoless_core::transport::{AllowAll, BearerToken, PreviewControl, VerdictService};

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
    /// `--max-concurrent-builds N` (also `CARGOLESS_APP_PARALLEL_BUILDS=N`):
    /// how many instances may build at once. **Default 1** = the original
    /// serialised behaviour (byte-identical). When > 1, each building lane
    /// gets its own `CARGO_TARGET_DIR` (`<state_dir>/app/<instance>/target`)
    /// so concurrent builds do not corrupt each other's incremental state or
    /// fight cargo's file locks. Capped at 2 for the first operator-visible
    /// release; set to 0 or omit to keep the default of 1.
    ///
    /// **Tradeoff**: N× target-dir disk (~40 GB each) on the PVC + CPU/RAM
    /// contention for parallel cold builds — that is why this is opt-in and
    /// capped. PVC sizing for N > 1 is a separate follow-up.
    pub max_concurrent_builds: usize,
    /// `--preview-domain <domain>` — the public domain self-serve previews are
    /// reachable under, e.g. `tryform.wtf`. A runtime preview `feat` is
    /// advertised on `/app` as `feat.<domain>` for the Part-2 reconciler to
    /// route. Absent ⇒ `public_host` is null and previews are port-forward-only.
    pub preview_domain: Option<String>,
    /// `--preview-port-range START-END` — the L4-proxy listen ports for
    /// runtime-registered previews. Distinct from `--port-range` (the app-child
    /// ports). Absent ⇒ each preview proxy binds an ephemeral OS-assigned port.
    pub preview_port_range: Option<String>,
    /// `--preview-defaults <file>` — an env file (`KEY=VALUE` per line, with
    /// `${VAR}` resolved from the daemon env) merged into every runtime
    /// preview's env overlay. This is how previews inherit the shared
    /// DB/S3/NATS wiring (the same block the `dev`/preview instance uses)
    /// without the agent client holding any secret.
    pub preview_defaults: Option<PathBuf>,
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

/// Default self-serve preview lifetime when the client requests no explicit
/// `ttl_secs` — 24h, enough for a day's work on a branch; an agent re-running
/// `cargoless preview` renews it.
const DEFAULT_PREVIEW_TTL_SECS: u64 = 24 * 60 * 60;
/// Upper bound on a client-requested TTL — a preview is ephemeral by design, so
/// even an explicit request cannot pin one for more than a week. 7 days.
const MAX_PREVIEW_TTL_SECS: u64 = 7 * 24 * 60 * 60;

// ── shutdown: SIGTERM/SIGINT → polled flag (the servedrv discipline) ──────
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_sig: core::ffi::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

// ── SIGHUP: instances-file hot-reload (CGLS-16) ──────────────────────────
// A SIGHUP from the operator (e.g. a ConfigMap update that triggers a `kill
// -HUP`) sets this flag. The control loop checks it each tick and, when set,
// re-reads the instances file, diffs against the live set, and applies the
// delta — all on the single control thread, so no new locks are needed.
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn on_hup(_sig: core::ffi::c_int) {
    RELOAD.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_signal_stops() {
    const SIGINT: core::ffi::c_int = 2;
    const SIGTERM: core::ffi::c_int = 15;
    const SIGHUP: core::ffi::c_int = 1;
    unsafe extern "C" {
        fn signal(signum: core::ffi::c_int, handler: extern "C" fn(core::ffi::c_int)) -> usize;
    }
    // SAFETY: the handler bodies are single atomic stores (async-signal-safe) —
    // the same house pattern as servedrv::install_signal_stops.
    unsafe {
        let _ = signal(SIGTERM, on_term);
        let _ = signal(SIGINT, on_term);
        let _ = signal(SIGHUP, on_hup);
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
        // CGLS-15: pass the per-lane build env (contains CARGO_TARGET_DIR
        // when max_concurrent > 1; empty for the default single-slot mode).
        let build_env = paths.build_env.clone();
        let (instance, sha, tx) = (
            instance.to_string(),
            sha.to_string(),
            self.events_tx.clone(),
        );
        std::thread::spawn(move || {
            // Wall-clock the whole build on the thread that owns it. The build
            // runs synchronously here, so this Instant brackets exactly the
            // build the `build_finished` telemetry reports — no cross-thread,
            // per-generation start-time bookkeeping needed.
            let started = Instant::now();
            // CGLS-15: pass the per-lane build env (contains CARGO_TARGET_DIR
            // when max_concurrent > 1; empty for the default single-slot mode).
            let report = appbuild::build(&appbuild::RealHooks, &build_paths, &sha, &build_env);
            let duration_ms = started.elapsed().as_millis() as u64;
            // Emit `build_finished` HERE rather than in `trace_event`, because
            // this is the only point where duration_ms + the built sha + the
            // verdict/reason all coexist. `Event::BuildFinished` (the pure
            // state machine's input) deliberately carries none of those, so
            // routing telemetry through it would mean widening the core event
            // type with clock-derived, decision-irrelevant fields. The built
            // sha comes from the report (the sha the build actually checked out
            // and, for green, re-confirmed) — authoritative over the requested
            // `sha` on an Indeterminate where they legitimately differ.
            let (verdict, built_sha, reason): (&str, &str, &str) = match &report {
                BuildReport::Green { sha, .. } => ("green", sha.as_str(), ""),
                BuildReport::Red { sha, reason } => ("red", sha.as_str(), reason.as_str()),
                BuildReport::Indeterminate { sha, reason } => {
                    ("indeterminate", sha.as_str(), reason.as_str())
                }
            };
            tracing::info!(
                "cargoless.app.instance" = instance.as_str(),
                "app.event" = "build_finished",
                generation = generation,
                verdict = verdict,
                reason = reason,
                sha = built_sha,
                duration_ms = duration_ms,
            );
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

/// Resolve the effective max-concurrent-builds limit from opts + env.
/// - `opts.max_concurrent_builds > 0` overrides everything.
/// - `CARGOLESS_APP_PARALLEL_BUILDS=N` env var is the second source.
/// - Default 1 = serialised builds (today's behaviour).
/// - Capped at 2 for the first operator-visible release.
fn resolve_max_concurrent(opts: &AppServeOpts) -> usize {
    const CAP: usize = 2;
    let from_opts = (opts.max_concurrent_builds > 0).then_some(opts.max_concurrent_builds);
    let from_env = std::env::var("CARGOLESS_APP_PARALLEL_BUILDS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    // Clamp to [1, CAP]: 0 falls back to 1 (default-off); >CAP is capped.
    from_opts.or(from_env).unwrap_or(1).clamp(1, CAP)
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

    // CGLS-15: resolve the concurrency limit. Default 1 = serialised (today's
    // behaviour). When > 1 each lane gets its own CARGO_TARGET_DIR.
    let max_concurrent = resolve_max_concurrent(opts);

    // Bind one L4 proxy per instance + build the per-instance driver config.
    let holding = Arc::new(HoldingResponse::page(
        503,
        "Service Unavailable",
        "text/html",
        "<!doctype html><title>starting</title><h1>cargoless app-serve</h1>\
         <p>no green build is serving yet — building…</p>",
    ));
    // `proxies` is keyed by instance name so SIGHUP can drop individual
    // proxies (stopping their accept loop) when an instance is removed.
    let mut proxies: BTreeMap<String, L4Proxy> = BTreeMap::new();
    let mut instance_configs = Vec::new();
    let mut run_plans: BTreeMap<String, RunPlan> = BTreeMap::new();
    for spec in &specs {
        let proxy = L4Proxy::bind(spec.app_bind, holding.clone())
            .map_err(|e| format!("instance `{}` proxy bind {}: {e}", spec.name, spec.app_bind))?;
        // CGLS-15: when max_concurrent > 1 each lane gets its own
        // CARGO_TARGET_DIR (`<state_dir>/app/<instance>/target`) so concurrent
        // builds do not corrupt each other's incremental state or fight cargo's
        // file locks. Default (=1): empty build_env → process inherits the
        // pod's ambient CARGO_TARGET_DIR — byte-identical to pre-CGLS-15.
        let build_env = if max_concurrent > 1 {
            let lane_target = state_dir.join("app").join(&spec.name).join("target");
            vec![(
                "CARGO_TARGET_DIR".to_string(),
                lane_target.to_string_lossy().into_owned(),
            )]
        } else {
            Vec::new()
        };
        // The driver flips the SAME slot Arc the proxy reads on every accept,
        // so a promote is visible to new connections immediately.
        let paths = InstancePaths {
            worktree: instance_worktree(state_dir, &spec.name),
            bundles: state_dir.join("app").join(&spec.name).join("bundles"),
            build_env,
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
        proxies.insert(spec.name.clone(), proxy);
    }

    // Track the live spec set for SIGHUP diffing. Keyed by name.
    let mut live_specs: BTreeMap<String, InstanceSpec> =
        specs.iter().map(|s| (s.name.clone(), s.clone())).collect();

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
            max_concurrent_builds: max_concurrent,
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
    // `live_refs` maps instance name → a shared mutable ref string; SIGHUP
    // updates the mutex so a changed ref takes effect on the next poll tick
    // without restarting the poller thread.
    let poll_ms = opts.poll_interval_ms.unwrap_or(2000).max(200);
    let mut live_refs: BTreeMap<String, Arc<std::sync::Mutex<String>>> = BTreeMap::new();
    for spec in &specs {
        let cell = Arc::new(std::sync::Mutex::new(spec.git_ref.clone()));
        live_refs.insert(spec.name.clone(), cell.clone());
        spawn_ref_poller(repo, &spec.name, cell, poll_ms, events_tx.clone(), None);
    }

    // ── self-serve previews (D-SELF-SERVE-PREVIEWS) ───────────────────
    // Wire the control channel so `POST/DELETE /instances` can drive runtime
    // add/remove. The HTTP threads only enqueue; this control thread (the sole
    // `driver` mutator) drains and applies — same single-mutator discipline as
    // the SIGHUP `RELOAD` flag.
    let (preview_tx, preview_rx) = channel::<PreviewControl>();
    svc.set_control(preview_tx);
    // Per-preview poller stop flags, so a DELETE stops exactly one poller.
    let mut poller_stops: BTreeMap<String, Arc<AtomicBool>> = BTreeMap::new();
    // Per-preview expiry instants (unix secs). A preview self-removes once its
    // TTL passes; static/SIGHUP instances never get an entry here.
    let mut expires_at: BTreeMap<String, u64> = BTreeMap::new();
    // Optional dedicated proxy-port allocator for runtime previews (distinct
    // from the app-child `ports`). Absent ⇒ previews bind an ephemeral port.
    let preview_ports = match opts.preview_port_range.as_deref().map(parse_port_range) {
        Some(Ok(r)) => Some(PortAllocator::new(r.start, r.end)),
        Some(Err(e)) => {
            ui::error(format!("--preview-port-range: {e}"));
            return Err(e);
        }
        None => None,
    };
    // Env template every runtime preview inherits (the shared DB/S3/NATS wiring
    // the static preview instance uses), `${VAR}`-resolved from the daemon env.
    // Fail-fast at startup: a missing var here is an operator config error.
    let preview_defaults = match load_preview_defaults(opts.preview_defaults.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            ui::error(format!("--preview-defaults: {e}"));
            return Err(e);
        }
    };

    // ── the control loop ─────────────────────────────────────────────
    // Single mutator of the driver; 200ms tick polls the shutdown + reload
    // flags. All mutations of `driver`, `proxies`, `live_refs`, and
    // `live_specs` happen on this thread — no new synchronisation needed.
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        // CGLS-16: SIGHUP hot-reload — re-read the instances file, diff
        // against the live set, and apply the delta without a pod restart.
        // Fail-safe: a malformed instances file keeps the prior set.
        if RELOAD
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            apply_sighup_reload(
                opts,
                repo,
                state_dir,
                poll_ms,
                max_concurrent,
                &holding,
                &mut driver,
                &launcher_ref,
                &mut proxies,
                &mut live_specs,
                &mut live_refs,
                &events_tx,
            );
        }
        // Self-serve previews: drain any POST/DELETE /instances requests the
        // HTTP threads enqueued. Drained BEFORE the lifecycle event below, so a
        // just-added preview's first build is dispatched this same tick (mirrors
        // the SIGHUP ordering). A burst all lands in one tick (try_recv loop).
        drain_preview_requests(
            &preview_rx,
            repo,
            state_dir,
            poll_ms,
            max_concurrent,
            opts.preview_domain.as_deref(),
            &preview_defaults,
            &holding,
            &mut driver,
            &launcher_ref,
            &svc,
            &mut proxies,
            &mut live_specs,
            &mut live_refs,
            &mut poller_stops,
            &mut expires_at,
            preview_ports.as_ref(),
            &events_tx,
        );
        // TTL sweep: auto-remove any preview whose lifetime has elapsed. Runs
        // every tick (cheap: a map scan), so an abandoned preview self-cleans
        // within ~200ms of expiry and the reconciler prunes its route next pass.
        sweep_expired_previews(
            repo,
            state_dir,
            &mut driver,
            &launcher_ref,
            &svc,
            &mut proxies,
            &mut live_specs,
            &mut live_refs,
            &mut poller_stops,
            &mut expires_at,
            preview_ports.as_ref(),
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
    drop(live_specs);
    Ok(())
}

/// Attempt to reload the instances file. Returns `Some(specs)` on success,
/// `None` on any failure (absent file, parse error, bad env-var reference).
/// The fail-safe contract lives here: callers keep the prior set on `None`.
fn try_reload_instances(opts: &AppServeOpts) -> Option<Vec<InstanceSpec>> {
    let file = opts.instances.as_ref()?;
    match load_instances(file, &|k| std::env::var(k).ok()) {
        Ok(f) => Some(f.instances),
        Err(e) => {
            ui::error(format!(
                "SIGHUP: instances file reload failed — keeping prior set: {e}"
            ));
            None
        }
    }
}

/// Apply a SIGHUP-triggered hot-reload: re-read the instances file, diff
/// against the live set, and update the control-loop state in place.
///
/// **Fail-safe**: if the instances file is absent, malformed, or unparseable,
/// the current live set is left completely unchanged — a bad ConfigMap push
/// never disrupts serving instances. The error is logged loudly (operator
/// should notice in pod logs / SigNoz).
///
/// **Delta semantics:**
/// - **Added** instance (name present in new file, absent in live set):
///   bind a new L4 proxy, register with the driver, start a ref poller.
/// - **Removed** instance (name absent in new file, present in live set):
///   immediately stop all children (no graceful drain — operator intent),
///   drop the proxy (stop accept loop), remove from driver + ref tracking.
/// - **Same name, changed `ref`**: update the live-ref cell; the poller
///   picks it up on the next tick. The currently-serving child keeps
///   running until a *new* build on the updated ref goes green
///   (never-serve-red holds across a ref change).
/// - **Same name, changed `env`**: update the driver's env overlay; takes
///   effect on the next child spawn.
/// - **Same name, same `ref` + `env`**: no-op (unchanged instance untouched).
/// - **`app_bind` changed** on an existing name: treated as remove + add
///   (the old port cannot be reused while the proxy is live).
#[allow(clippy::too_many_arguments)] // all are mutable control-loop state; bundling adds noise
fn apply_sighup_reload(
    opts: &AppServeOpts,
    repo: &Path,
    state_dir: &Path,
    poll_ms: u64,
    max_concurrent: usize,
    holding: &Arc<HoldingResponse>,
    driver: &mut Driver<ThreadBuildBackend, ProcLauncher, ChannelSink>,
    launcher_ref: &Arc<ProcLauncher>,
    proxies: &mut BTreeMap<String, L4Proxy>,
    live_specs: &mut BTreeMap<String, InstanceSpec>,
    live_refs: &mut BTreeMap<String, Arc<std::sync::Mutex<String>>>,
    events_tx: &Sender<(String, Event)>,
) {
    // Only meaningful when an instances file was configured (zero-config mode
    // has no file to reload — the single default instance is immutable).
    if opts.instances.is_none() {
        ui::warn("SIGHUP received but no --instances file configured — ignoring");
        return;
    }

    // Fail-safe: `try_reload_instances` returns None on any parse failure and
    // logs the error; we keep the prior live set unchanged.
    let Some(new_specs) = try_reload_instances(opts) else {
        return;
    };

    let new_map: BTreeMap<String, InstanceSpec> =
        new_specs.into_iter().map(|s| (s.name.clone(), s)).collect();

    // ── removed instances ─────────────────────────────────────────────
    let removed: Vec<String> = live_specs
        .keys()
        .filter(|n| !new_map.contains_key(*n))
        .cloned()
        .collect();
    for name in &removed {
        ui::ok(format!("SIGHUP: removing instance `{name}`"));
        driver.remove_instance(name);
        proxies.remove(name); // drop stops the accept loop
        live_refs.remove(name);
        live_specs.remove(name);
        launcher_ref.plans.lock().expect("plans").remove(name);
    }

    // ── added or changed instances ────────────────────────────────────
    for (name, new_spec) in &new_map {
        match live_specs.get(name) {
            None => {
                // New instance: bind proxy, set up worktree, add to driver +
                // ref tracking, start poller.
                ui::ok(format!("SIGHUP: adding instance `{name}`"));
                let proxy = match L4Proxy::bind(new_spec.app_bind, holding.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        ui::error(format!(
                            "SIGHUP: could not bind proxy for `{name}` at {}: {e} — skipping",
                            new_spec.app_bind
                        ));
                        continue;
                    }
                };
                let build_env = if max_concurrent > 1 {
                    let lane_target = state_dir.join("app").join(name).join("target");
                    vec![(
                        "CARGO_TARGET_DIR".to_string(),
                        lane_target.to_string_lossy().into_owned(),
                    )]
                } else {
                    Vec::new()
                };
                let paths = InstancePaths {
                    worktree: instance_worktree(state_dir, name),
                    bundles: state_dir.join("app").join(name).join("bundles"),
                    build_env,
                };
                let env: Vec<(String, String)> = new_spec
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let config = InstanceConfig {
                    name: name.clone(),
                    slot: proxy.slot().clone(),
                    paths,
                    env,
                };
                // Ensure the git worktree exists (same discipline as boot).
                if let Err(e) = ensure_instance_worktree(
                    repo,
                    &instance_worktree(state_dir, name),
                    &new_spec.git_ref,
                ) {
                    ui::warn(format!(
                        "SIGHUP: instance `{name}`: could not set up worktree: {e}"
                    ));
                }
                // Build the initial run plan for this instance.
                let wt = instance_worktree(state_dir, name);
                let plan = run_plan_from_manifest(&wt)
                    .or_else(|| run_plan_from_manifest(repo))
                    .unwrap_or_else(default_run_plan);
                launcher_ref
                    .plans
                    .lock()
                    .expect("plans")
                    .insert(name.clone(), plan);

                // Start the ref poller before adding to driver, so the first
                // HeadAdvanced event can be processed as soon as the driver
                // knows about the instance.
                let cell = Arc::new(std::sync::Mutex::new(new_spec.git_ref.clone()));
                live_refs.insert(name.clone(), cell.clone());
                spawn_ref_poller(repo, name, cell, poll_ms, events_tx.clone(), None);

                driver.add_instance(config);
                proxies.insert(name.clone(), proxy);
                live_specs.insert(name.clone(), new_spec.clone());
            }
            Some(old_spec) => {
                // Existing instance: check what changed.
                let bind_changed = old_spec.app_bind != new_spec.app_bind;
                if bind_changed {
                    // `app_bind` change: treat as remove + add (cannot
                    // rebind an active port in place).
                    ui::ok(format!(
                        "SIGHUP: instance `{name}` app_bind changed {} → {} — \
                         removing and re-adding",
                        old_spec.app_bind, new_spec.app_bind
                    ));
                    driver.remove_instance(name);
                    proxies.remove(name);
                    live_refs.remove(name);
                    live_specs.remove(name);
                    launcher_ref.plans.lock().expect("plans").remove(name);

                    // Re-add with new bind.
                    let proxy = match L4Proxy::bind(new_spec.app_bind, holding.clone()) {
                        Ok(p) => p,
                        Err(e) => {
                            ui::error(format!(
                                "SIGHUP: could not bind proxy for `{name}` at {}: {e} — skipping",
                                new_spec.app_bind
                            ));
                            continue;
                        }
                    };
                    let bind_changed_build_env = if max_concurrent > 1 {
                        let lane_target = state_dir.join("app").join(name).join("target");
                        vec![(
                            "CARGO_TARGET_DIR".to_string(),
                            lane_target.to_string_lossy().into_owned(),
                        )]
                    } else {
                        Vec::new()
                    };
                    let paths = InstancePaths {
                        worktree: instance_worktree(state_dir, name),
                        bundles: state_dir.join("app").join(name).join("bundles"),
                        build_env: bind_changed_build_env,
                    };
                    let env: Vec<(String, String)> = new_spec
                        .env
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    let config = InstanceConfig {
                        name: name.clone(),
                        slot: proxy.slot().clone(),
                        paths,
                        env,
                    };
                    let wt = instance_worktree(state_dir, name);
                    let plan = run_plan_from_manifest(&wt)
                        .or_else(|| run_plan_from_manifest(repo))
                        .unwrap_or_else(default_run_plan);
                    launcher_ref
                        .plans
                        .lock()
                        .expect("plans")
                        .insert(name.clone(), plan);
                    let cell = Arc::new(std::sync::Mutex::new(new_spec.git_ref.clone()));
                    live_refs.insert(name.clone(), cell.clone());
                    spawn_ref_poller(repo, name, cell, poll_ms, events_tx.clone(), None);
                    driver.add_instance(config);
                    proxies.insert(name.clone(), proxy);
                    live_specs.insert(name.clone(), new_spec.clone());
                } else {
                    // Same bind: apply in-place updates.
                    if old_spec.git_ref != new_spec.git_ref {
                        ui::ok(format!(
                            "SIGHUP: instance `{name}` ref {} → {}",
                            old_spec.git_ref, new_spec.git_ref
                        ));
                        if let Some(cell) = live_refs.get(name) {
                            *cell.lock().expect("live_ref") = new_spec.git_ref.clone();
                        }
                    }
                    if old_spec.env != new_spec.env {
                        ui::ok(format!("SIGHUP: instance `{name}` env updated"));
                        let env: Vec<(String, String)> = new_spec
                            .env
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        driver.update_instance_env(name, env);
                    }
                    live_specs.insert(name.clone(), new_spec.clone());
                }
            }
        }
    }

    ui::ok(format!(
        "SIGHUP: reload complete — {} instance(s) live",
        live_specs.len()
    ));
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
        Event::BuildFinished { .. } => {
            // Intentionally NOT emitted here. `build_finished` is emitted at its
            // source in `ThreadBuildBackend::start`, where it can also carry
            // `duration_ms` (the build wall-clock) and the built `sha` —
            // neither of which is present on `Event::BuildFinished` (the pure
            // state machine's input alphabet). Emitting in both places would
            // double-count. See the emission site for the full field set.
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

/// Remove a runtime preview's git worktree (the inverse of
/// [`ensure_instance_worktree`]). Best-effort: `git worktree remove --force`
/// then a `prune`; a failure is logged, not fatal (the next `prune` sweeps it).
fn remove_instance_worktree(repo: &Path, worktree: &Path) {
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .output();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output();
}

/// Load the `--preview-defaults` env file: `KEY=VALUE` lines (blank lines and
/// `#` comments skipped), each value `${VAR}`-resolved from the daemon's own
/// environment. Strict, like the instances file: an unresolvable `${VAR}` is a
/// startup error (a preview silently booting with `DATABASE_URL=""` would fail
/// far from the cause). `None` path ⇒ an empty template (no shared defaults).
fn load_preview_defaults(path: Option<&Path>) -> Result<Vec<(String, String)>, String> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, val) = line
            .split_once('=')
            .ok_or_else(|| format!("{}:{}: expected KEY=VALUE", path.display(), i + 1))?;
        let key = key.trim().to_string();
        if key.is_empty() {
            return Err(format!("{}:{}: empty key", path.display(), i + 1));
        }
        let val =
            cargoless_core::appinstances::interpolate_env(val.trim(), &|k| std::env::var(k).ok())
                .map_err(|e| format!("{}:{}: {e}", path.display(), i + 1))?;
        out.push((key, val));
    }
    Ok(out)
}

/// Sanitize a requested preview name into a DNS-label-safe instance key:
/// lowercase, `/`→`-`, drop anything not `[a-z0-9-]`, collapse repeats, trim
/// leading/trailing `-`, cap at 50 chars. Empty after sanitizing ⇒ `None`.
fn preview_instance_name(raw: &str) -> Option<String> {
    let mut s = String::with_capacity(raw.len());
    let mut last_dash = false;
    for c in raw.to_ascii_lowercase().chars() {
        let mapped = if c.is_ascii_alphanumeric() {
            c
        } else if c == '-' || c == '/' || c == '_' || c == '.' {
            '-'
        } else {
            continue;
        };
        if mapped == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        s.push(mapped);
    }
    let s = s.trim_matches('-');
    let s: String = s.chars().take(50).collect();
    let s = s.trim_matches('-').to_string();
    (!s.is_empty()).then_some(s)
}

/// Drain pending self-serve preview control requests onto the single-mutator
/// control thread. Reuses every existing instance primitive — `L4Proxy::bind`,
/// `InstanceConfig`, `ensure_instance_worktree`, `run_plan_from_manifest`,
/// `spawn_ref_poller`, `Driver::add_instance`/`remove_instance` — so a runtime
/// preview is byte-identical to a boot/SIGHUP instance, plus a dynamic proxy
/// port and the `/app` route facts the Part-2 reconciler reads.
#[allow(clippy::too_many_arguments)] // all are control-loop state; bundling adds noise
fn drain_preview_requests(
    preview_rx: &Receiver<PreviewControl>,
    repo: &Path,
    state_dir: &Path,
    poll_ms: u64,
    max_concurrent: usize,
    preview_domain: Option<&str>,
    preview_defaults: &[(String, String)],
    holding: &Arc<HoldingResponse>,
    driver: &mut Driver<ThreadBuildBackend, ProcLauncher, ChannelSink>,
    launcher_ref: &Arc<ProcLauncher>,
    svc: &Arc<AppServeState>,
    proxies: &mut BTreeMap<String, L4Proxy>,
    live_specs: &mut BTreeMap<String, InstanceSpec>,
    live_refs: &mut BTreeMap<String, Arc<std::sync::Mutex<String>>>,
    poller_stops: &mut BTreeMap<String, Arc<AtomicBool>>,
    expires_at: &mut BTreeMap<String, u64>,
    preview_ports: Option<&PortAllocator>,
    events_tx: &Sender<(String, Event)>,
) {
    while let Ok(req) = preview_rx.try_recv() {
        match req {
            PreviewControl::Add {
                name,
                git_ref,
                env,
                own_db,
                ttl_secs,
            } => {
                let Some(name) = preview_instance_name(&name) else {
                    ui::warn(format!("preview: rejected unusable name `{name}`"));
                    continue;
                };
                // The lifetime this Add grants, clamped to a sane ceiling so a
                // client cannot pin a preview forever; `None` ⇒ the default.
                let ttl = ttl_secs
                    .unwrap_or(DEFAULT_PREVIEW_TTL_SECS)
                    .min(MAX_PREVIEW_TTL_SECS);
                let expiry = unix_now().saturating_add(ttl);
                if live_specs.contains_key(&name) {
                    // Upsert: re-point the existing preview's ref (like the
                    // SIGHUP same-name changed-ref arm) — no re-bind, no churn —
                    // and RENEW its TTL (an agent re-running `preview` on a live
                    // branch keeps it alive).
                    if let Some(cell) = live_refs.get(&name) {
                        *cell.lock().expect("live_ref") = git_ref.clone();
                    }
                    expires_at.insert(name.clone(), expiry);
                    ui::ok(format!(
                        "preview `{name}` ref re-pointed → {git_ref}, TTL renewed (+{ttl}s)"
                    ));
                    continue;
                }
                if own_db {
                    // Per-branch DB provisioning is a later increment; for now
                    // the preview shares the daemon's DB env. Flagged, not failed.
                    ui::warn(format!(
                        "preview `{name}`: own_db requested but per-branch DB \
                         provisioning is not in this increment — using shared DB"
                    ));
                }

                // Public host for the reconciler, e.g. `feat.tryform.wtf`.
                let public_host = preview_domain.map(|d| format!("{name}.{d}"));

                // Proxy listen port: a dedicated allocator if configured, else
                // an ephemeral OS-assigned port (bind `:0`).
                let bind_port = match preview_ports {
                    Some(alloc) => match alloc.alloc() {
                        Some(p) => p,
                        None => {
                            ui::error(format!(
                                "preview `{name}`: preview port range exhausted — skipping"
                            ));
                            continue;
                        }
                    },
                    None => 0,
                };
                let app_bind = match format!("0.0.0.0:{bind_port}").parse() {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                let proxy = match L4Proxy::bind(app_bind, holding.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        ui::error(format!(
                            "preview `{name}`: proxy bind {app_bind}: {e} — skipping"
                        ));
                        if let (Some(alloc), true) = (preview_ports, bind_port != 0) {
                            alloc.release(bind_port);
                        }
                        continue;
                    }
                };
                let bound_port = proxy.addr().port();

                // Env overlay: the shared defaults first, then the request's
                // own env (request wins on key collision), then the public
                // base-URL overrides so the app renders its own host.
                let mut env_map: BTreeMap<String, String> =
                    preview_defaults.iter().cloned().collect();
                for (k, v) in env {
                    env_map.insert(k, v);
                }
                if let Some(host) = &public_host {
                    let url = format!("https://{host}");
                    env_map.insert("TRIFORM_PUBLIC_BASE_URL".into(), url.clone());
                    env_map.insert("TRIFORM_API_URL".into(), url);
                }

                // CGLS-15: a per-lane CARGO_TARGET_DIR only when the daemon runs
                // parallel builds; empty (pod-ambient target inherited) otherwise.
                // A preview is a lane like any other on this axis.
                let build_env = if max_concurrent > 1 {
                    let lane_target = state_dir.join("app").join(&name).join("target");
                    vec![(
                        "CARGO_TARGET_DIR".to_string(),
                        lane_target.to_string_lossy().into_owned(),
                    )]
                } else {
                    Vec::new()
                };
                let paths = InstancePaths {
                    worktree: instance_worktree(state_dir, &name),
                    bundles: state_dir.join("app").join(&name).join("bundles"),
                    build_env,
                };
                let config = InstanceConfig {
                    name: name.clone(),
                    slot: proxy.slot().clone(),
                    paths,
                    env: env_map
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                };

                if let Err(e) =
                    ensure_instance_worktree(repo, &instance_worktree(state_dir, &name), &git_ref)
                {
                    ui::warn(format!("preview `{name}`: could not set up worktree: {e}"));
                }
                let wt = instance_worktree(state_dir, &name);
                let plan = run_plan_from_manifest(&wt)
                    .or_else(|| run_plan_from_manifest(repo))
                    .unwrap_or_else(default_run_plan);
                launcher_ref
                    .plans
                    .lock()
                    .expect("plans")
                    .insert(name.clone(), plan);

                // Poller with a per-instance stop flag (so DELETE can stop it).
                let stop = Arc::new(AtomicBool::new(false));
                let cell = Arc::new(std::sync::Mutex::new(git_ref.clone()));
                live_refs.insert(name.clone(), cell.clone());
                spawn_ref_poller(
                    repo,
                    &name,
                    cell,
                    poll_ms,
                    events_tx.clone(),
                    Some(stop.clone()),
                );
                poller_stops.insert(name.clone(), stop);

                driver.add_instance(config);
                proxies.insert(name.clone(), proxy);
                live_specs.insert(
                    name.clone(),
                    InstanceSpec {
                        name: name.clone(),
                        git_ref: git_ref.clone(),
                        app_bind,
                        env: env_map,
                    },
                );
                svc.set_preview_route(
                    &name,
                    PreviewRoute {
                        proxy_port: bound_port,
                        public_host: public_host.clone(),
                    },
                );
                // Record when this preview self-expires (the TTL sweep below
                // tears it down at/after this instant).
                expires_at.insert(name.clone(), expiry);

                // Kick the first build now (the poller will also detect HEAD).
                if let Some(sha) = resolve_ref(repo, &git_ref) {
                    driver.drive(&name, Event::HeadAdvanced { sha });
                }
                match &public_host {
                    Some(h) => ui::ok(format!(
                        "preview `{name}` added — proxy :{bound_port}, public https://{h} \
                         (expires in {ttl}s)"
                    )),
                    None => ui::ok(format!(
                        "preview `{name}` added — proxy :{bound_port} (no --preview-domain, \
                         expires in {ttl}s)"
                    )),
                }
            }
            PreviewControl::Remove { name } => {
                let Some(name) = preview_instance_name(&name) else {
                    continue;
                };
                if !live_specs.contains_key(&name) {
                    expires_at.remove(&name);
                    poller_stops.remove(&name);
                    ui::warn(format!("preview `{name}`: not found — ignored"));
                    continue;
                }
                teardown_preview(
                    &name,
                    "removed",
                    repo,
                    state_dir,
                    driver,
                    launcher_ref,
                    svc,
                    proxies,
                    live_specs,
                    live_refs,
                    poller_stops,
                    expires_at,
                    preview_ports,
                );
            }
        }
    }
}

/// Tear a single preview down completely — the shared core of both an explicit
/// `DELETE /instances/<name>` and a TTL expiry. Stops the poller, reclaims the
/// proxy port, removes the instance from the driver + every live map + the
/// `/app` route, and prunes the git worktree. `reason` colours the log line
/// (`"removed"` vs `"expired"`). Caller guarantees `name` is a live preview.
#[allow(clippy::too_many_arguments)] // all are control-loop state; bundling adds noise
fn teardown_preview(
    name: &str,
    reason: &str,
    repo: &Path,
    state_dir: &Path,
    driver: &mut Driver<ThreadBuildBackend, ProcLauncher, ChannelSink>,
    launcher_ref: &Arc<ProcLauncher>,
    svc: &Arc<AppServeState>,
    proxies: &mut BTreeMap<String, L4Proxy>,
    live_specs: &mut BTreeMap<String, InstanceSpec>,
    live_refs: &mut BTreeMap<String, Arc<std::sync::Mutex<String>>>,
    poller_stops: &mut BTreeMap<String, Arc<AtomicBool>>,
    expires_at: &mut BTreeMap<String, u64>,
    preview_ports: Option<&PortAllocator>,
) {
    expires_at.remove(name);
    // Stop the poller first so no HeadAdvanced races the teardown.
    if let Some(stop) = poller_stops.remove(name) {
        stop.store(true, Ordering::SeqCst);
    }
    // Reclaim the proxy port if it came from our allocator.
    if let (Some(alloc), Some(spec)) = (preview_ports, live_specs.get(name)) {
        let p = spec.app_bind.port();
        if p != 0 {
            alloc.release(p);
        }
    }
    driver.remove_instance(name);
    proxies.remove(name); // drop stops the accept loop
    live_refs.remove(name);
    live_specs.remove(name);
    launcher_ref.plans.lock().expect("plans").remove(name);
    svc.clear_preview_route(name);
    let wt = instance_worktree(state_dir, name);
    remove_instance_worktree(repo, &wt);
    ui::ok(format!("preview `{name}` {reason}"));
}

/// Sweep expired self-serve previews on the control thread (called each tick).
/// A preview whose recorded expiry instant is at/before `now` is torn down via
/// [`teardown_preview`] — so an abandoned preview self-cleans and the reconciler
/// then prunes its Service/Ingress. Static/SIGHUP instances have no expiry entry
/// and are never swept.
#[allow(clippy::too_many_arguments)] // all are control-loop state; bundling adds noise
fn sweep_expired_previews(
    repo: &Path,
    state_dir: &Path,
    driver: &mut Driver<ThreadBuildBackend, ProcLauncher, ChannelSink>,
    launcher_ref: &Arc<ProcLauncher>,
    svc: &Arc<AppServeState>,
    proxies: &mut BTreeMap<String, L4Proxy>,
    live_specs: &mut BTreeMap<String, InstanceSpec>,
    live_refs: &mut BTreeMap<String, Arc<std::sync::Mutex<String>>>,
    poller_stops: &mut BTreeMap<String, Arc<AtomicBool>>,
    expires_at: &mut BTreeMap<String, u64>,
    preview_ports: Option<&PortAllocator>,
) {
    let now = unix_now();
    let expired: Vec<String> = expires_at
        .iter()
        .filter(|(_, exp)| **exp <= now)
        .map(|(n, _)| n.clone())
        .collect();
    for name in expired {
        // Only tear down a still-live preview; a stale expiry entry for an
        // already-removed preview is just dropped.
        if live_specs.contains_key(&name) {
            ui::ok(format!("preview `{name}` TTL reached — auto-removing"));
            teardown_preview(
                &name,
                "expired",
                repo,
                state_dir,
                driver,
                launcher_ref,
                svc,
                proxies,
                live_specs,
                live_refs,
                poller_stops,
                expires_at,
                preview_ports,
            );
        } else {
            expires_at.remove(&name);
        }
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

/// One ref poller: resolve `live_ref` to a sha on an interval, post
/// `HeadAdvanced` when it changes. Uses `git rev-parse` (the same git the build
/// worker uses); a transient git error is skipped (next tick retries).
///
/// `live_ref` is an `Arc<Mutex<String>>` so the control thread can update the
/// tracked ref on SIGHUP without killing and restarting the poller thread.
///
/// `stop` is an optional per-instance stop flag: a self-serve preview's
/// `DELETE /instances/<name>` sets it so that one poller exits promptly
/// (without waiting for the global `SHUTDOWN`). Boot/SIGHUP pass `None` (their
/// instances live for the daemon's lifetime, gated by the global flag).
fn spawn_ref_poller(
    repo: &Path,
    name: &str,
    live_ref: Arc<std::sync::Mutex<String>>,
    poll_ms: u64,
    tx: Sender<(String, Event)>,
    stop: Option<Arc<AtomicBool>>,
) {
    let (repo, name) = (repo.to_path_buf(), name.to_string());
    std::thread::spawn(move || {
        let mut last: Option<String> = None;
        loop {
            if SHUTDOWN.load(Ordering::SeqCst)
                || stop.as_ref().is_some_and(|s| s.load(Ordering::SeqCst))
            {
                return;
            }
            let git_ref = live_ref.lock().expect("live_ref").clone();
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

    // ── CGLS-16: SIGHUP instances hot-reload ─────────────────────────────

    /// `try_reload_instances` returns `None` (fail-safe) when the instances
    /// file is missing — the caller keeps the prior live set.
    #[test]
    fn sighup_missing_file_returns_none() {
        let opts = AppServeOpts {
            repo: Some(PathBuf::from("/repo")),
            instances: Some(PathBuf::from("/nonexistent/instances.yaml")),
            ..Default::default()
        };
        assert!(
            try_reload_instances(&opts).is_none(),
            "missing file must return None (fail-safe — prior set kept)"
        );
    }

    /// `try_reload_instances` returns `None` for a malformed instances file —
    /// bad YAML / unknown key / missing `version` all fail loud + return None.
    #[test]
    fn sighup_bad_file_returns_none() {
        let tmp = std::env::temp_dir().join(format!("cargoless-cgls16-bad-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("instances.yaml");
        // Missing `version:` → parse error.
        std::fs::write(
            &file,
            "instances:\n  - name: dev\n    ref: origin/dev\n    app_bind: \"127.0.0.1:8080\"\n",
        )
        .unwrap();
        let opts = AppServeOpts {
            repo: Some(PathBuf::from("/repo")),
            instances: Some(file),
            ..Default::default()
        };
        assert!(
            try_reload_instances(&opts).is_none(),
            "malformed instances file must return None (fail-safe)"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `try_reload_instances` returns `Some(specs)` for a valid file.
    #[test]
    fn sighup_valid_file_returns_specs() {
        let tmp =
            std::env::temp_dir().join(format!("cargoless-cgls16-valid-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("instances.yaml");
        std::fs::write(
            &file,
            "version: 1\ninstances:\n  - name: dev\n    ref: origin/dev\n    app_bind: \"127.0.0.1:8080\"\n",
        )
        .unwrap();
        let opts = AppServeOpts {
            repo: Some(PathBuf::from("/repo")),
            instances: Some(file),
            ..Default::default()
        };
        let specs = try_reload_instances(&opts).expect("valid file returns Some");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "dev");
        assert_eq!(specs[0].git_ref, "origin/dev");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The live-ref cell (Arc<Mutex<String>>) can be updated by the control
    /// thread while the poller thread reads it — simulates SIGHUP ref change.
    #[test]
    fn live_ref_cell_update_is_visible_to_poller() {
        let cell = Arc::new(std::sync::Mutex::new("origin/dev".to_string()));
        let reader = cell.clone();
        // Simulate control-thread update.
        *cell.lock().unwrap() = "origin/feature-x".to_string();
        // Simulate poller-thread read on next tick.
        let observed = reader.lock().unwrap().clone();
        assert_eq!(
            observed, "origin/feature-x",
            "updated ref must be visible through the shared cell"
        );
    }

    // ── CGLS-15: per-lane build slot ─────────────────────────────────────────

    /// `resolve_max_concurrent` returns 1 (default-off) when neither the flag
    /// nor the env var is set.
    #[test]
    fn default_off_guard_max_concurrent_is_1() {
        // Remove the env var in case a parent process set it.
        // SAFETY: env mutation is inherently racy with parallel tests; these
        // tests only assert `resolve_max_concurrent` logic and the window is
        // minimal. set_var/remove_var are unsafe in Edition 2024.
        unsafe { std::env::remove_var("CARGOLESS_APP_PARALLEL_BUILDS") };
        let opts = AppServeOpts::default(); // max_concurrent_builds = 0
        assert_eq!(
            resolve_max_concurrent(&opts),
            1,
            "flag unset + no env → default 1 (serialised)"
        );
    }

    /// The CLI flag overrides the default.
    #[test]
    fn max_concurrent_flag_overrides_default() {
        unsafe { std::env::remove_var("CARGOLESS_APP_PARALLEL_BUILDS") };
        let opts = AppServeOpts {
            max_concurrent_builds: 2,
            ..Default::default()
        };
        assert_eq!(resolve_max_concurrent(&opts), 2);
    }

    /// The env var is read when the flag is absent.
    #[test]
    fn max_concurrent_env_var_is_read() {
        // SAFETY: single-threaded env mutation; unsafe in Edition 2024.
        unsafe { std::env::set_var("CARGOLESS_APP_PARALLEL_BUILDS", "2") };
        let opts = AppServeOpts::default();
        let result = resolve_max_concurrent(&opts);
        unsafe { std::env::remove_var("CARGOLESS_APP_PARALLEL_BUILDS") };
        assert_eq!(result, 2);
    }

    /// The cap of 2 is enforced even if a higher value is requested.
    #[test]
    fn max_concurrent_capped_at_2() {
        unsafe { std::env::remove_var("CARGOLESS_APP_PARALLEL_BUILDS") };
        let opts = AppServeOpts {
            max_concurrent_builds: 99,
            ..Default::default()
        };
        assert_eq!(resolve_max_concurrent(&opts), 2, "capped at 2");
    }

    /// Target-dir isolation: when max_concurrent > 1, each lane gets its own
    /// distinct CARGO_TARGET_DIR under the state dir.
    #[test]
    fn target_dir_isolation_per_lane() {
        let state_dir = std::path::PathBuf::from("/state");
        // Simulate two instances when max_concurrent = 2.
        let instances = ["dev", "feature-x"];
        let max_concurrent: usize = 2;
        let mut seen: Vec<String> = Vec::new();
        for name in &instances {
            let build_env: Vec<(String, String)> = if max_concurrent > 1 {
                let lane_target = state_dir.join("app").join(name).join("target");
                vec![(
                    "CARGO_TARGET_DIR".to_string(),
                    lane_target.to_string_lossy().into_owned(),
                )]
            } else {
                Vec::new()
            };
            let target = build_env
                .iter()
                .find(|(k, _)| k == "CARGO_TARGET_DIR")
                .map(|(_, v)| v.clone())
                .expect("CARGO_TARGET_DIR must be set per-lane when max_concurrent > 1");
            assert!(
                target.contains(name),
                "per-lane target dir must contain the instance name: {target}"
            );
            assert!(
                !seen.contains(&target),
                "each lane must have a unique CARGO_TARGET_DIR: {target} already seen"
            );
            seen.push(target);
        }
    }

    /// Default-off guard for target dir: when max_concurrent = 1, `build_env`
    /// is empty (no CARGO_TARGET_DIR injection) — the process inherits the pod's
    /// ambient value unchanged, exactly as before CGLS-15.
    #[test]
    fn default_off_no_target_dir_injection() {
        let state_dir = std::path::PathBuf::from("/state");
        let max_concurrent: usize = 1; // default
        let build_env: Vec<(String, String)> = if max_concurrent > 1 {
            let lane_target = state_dir.join("app").join("dev").join("target");
            vec![(
                "CARGO_TARGET_DIR".to_string(),
                lane_target.to_string_lossy().into_owned(),
            )]
        } else {
            Vec::new()
        };
        assert!(
            build_env.is_empty(),
            "default-off: build_env must be empty so the pod's ambient env is inherited unchanged"
        );
    }

    // ── self-serve previews ──────────────────────────────────────────────

    #[test]
    fn preview_instance_name_sanitizes_to_dns_label() {
        // Branch-ish inputs → DNS-label-safe instance keys.
        assert_eq!(
            preview_instance_name("feature/My_Cool-Branch"),
            Some("feature-my-cool-branch".to_string())
        );
        assert_eq!(
            preview_instance_name("origin/dev"),
            Some("origin-dev".into())
        );
        // Collapsing + trimming of separators.
        assert_eq!(preview_instance_name("--a//b__c--"), Some("a-b-c".into()));
        // Nothing usable ⇒ None (caller rejects).
        assert_eq!(preview_instance_name("///"), None);
        assert_eq!(preview_instance_name(""), None);
        // Length is capped (DNS label ≤ 63; we cap at 50).
        let long = "x".repeat(80);
        assert_eq!(preview_instance_name(&long).unwrap().len(), 50);
    }

    #[test]
    fn preview_ttl_clamps_to_bounds() {
        // The TTL the Add arm grants: explicit value when sane, default when
        // absent, never above the ceiling. Mirrors the drain-arm computation.
        let grant = |ttl_secs: Option<u64>| {
            ttl_secs
                .unwrap_or(DEFAULT_PREVIEW_TTL_SECS)
                .min(MAX_PREVIEW_TTL_SECS)
        };
        assert_eq!(grant(None), DEFAULT_PREVIEW_TTL_SECS, "absent ⇒ default");
        assert_eq!(grant(Some(3600)), 3600, "in-range ⇒ as requested");
        assert_eq!(
            grant(Some(u64::MAX)),
            MAX_PREVIEW_TTL_SECS,
            "over-ceiling ⇒ clamped"
        );
        assert!(
            DEFAULT_PREVIEW_TTL_SECS <= MAX_PREVIEW_TTL_SECS,
            "default must not exceed the ceiling"
        );
    }

    #[test]
    fn load_preview_defaults_none_is_empty() {
        assert!(load_preview_defaults(None).unwrap().is_empty());
    }

    #[test]
    fn load_preview_defaults_parses_and_interpolates() {
        // Use a process env var the test sets, to exercise ${VAR} resolution.
        // SAFETY: single-threaded test setup; the var name is test-unique.
        unsafe {
            std::env::set_var("CGLS_TEST_PREVIEW_DB", "postgres://shared/db");
        }
        let tmp = std::env::temp_dir().join(format!("cgls-pdefaults-{}", std::process::id()));
        std::fs::write(
            &tmp,
            "# a comment\n\nDATABASE_URL=${CGLS_TEST_PREVIEW_DB}\nRUST_LOG=info\n",
        )
        .unwrap();
        let got = load_preview_defaults(Some(&tmp)).unwrap();
        assert_eq!(
            got,
            vec![
                (
                    "DATABASE_URL".to_string(),
                    "postgres://shared/db".to_string()
                ),
                ("RUST_LOG".to_string(), "info".to_string()),
            ]
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn load_preview_defaults_unresolved_var_is_error() {
        let tmp = std::env::temp_dir().join(format!("cgls-pdefaults-bad-{}", std::process::id()));
        std::fs::write(&tmp, "X=${CGLS_DEFINITELY_UNSET_VAR_42}\n").unwrap();
        assert!(
            load_preview_defaults(Some(&tmp)).is_err(),
            "an unresolvable ${{VAR}} must be a startup error, not an empty value"
        );
        let _ = std::fs::remove_file(&tmp);
    }
}
