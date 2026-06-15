//! `appdrv` — the app-serve control loop core: pure state machine ⇄ world.
//!
//! [`crate::appstate::AppState`] decides; this module *acts*. It owns the
//! instance map, feeds observed [`Event`](crate::appstate::Event)s into
//! `AppState::step`, and executes the returned
//! [`Action`](crate::appstate::Action)s against the effectful seams:
//!
//! - [`BuildBackend`] — run a build for `(instance, sha)`, eventually posting
//!   `BuildFinished` (production: a detached thread running
//!   [`crate::appbuild::build`]).
//! - [`ChildLauncher`] — spawn the built bundle's child on an allocated port,
//!   probe its health, stop it (production: the bin crate's process launcher).
//! - [`crate::l4proxy::UpstreamSlot`] — the per-instance proxy upstream; a
//!   promote is one atomic `set`.
//!
//! Both seams are injected so the **whole lifecycle is unit-testable in
//! process** — no real build, no real child, no real socket — exercising the
//! exact action-execution + event-feedback wiring the production daemon runs.
//!
//! ## Generations come only from the state machine
//!
//! [`AppState`] mints every generation and stamps it into each `Action`. The
//! driver never invents one: it threads the action's generation into the
//! spawned work and back through the resulting event, so the state machine's
//! stale-generation discard is the single source of liveness truth.
//!
//! ## The single promote site
//!
//! [`Driver::execute`]'s `Promote` arm is the only place the proxy slot
//! flips, the pointer advances, and the durable state is written — the
//! app-serve analogue of `servedrv::publish_verdict`. Threading the loop
//! (mpsc tick, signal handling) lives in the bin crate; the *logic* is here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::appstate::{Action, AppState, Event, Generation};
use crate::appsvc::{AppServeState, InstanceReport};
use crate::l4proxy::UpstreamSlot;

/// Where one instance builds and stores bundles (mirror of
/// [`crate::appbuild::InstancePaths`], kept local so the driver does not force
/// a particular bundle layout on the seam).
#[derive(Debug, Clone)]
pub struct InstancePaths {
    pub worktree: PathBuf,
    pub bundles: PathBuf,
    /// Extra environment variables injected into every build step for this
    /// instance. Used to set a per-lane `CARGO_TARGET_DIR` when
    /// `max_concurrent > 1`; empty (and ignored) for the default single-slot
    /// mode — that path is byte-identical to the pre-CGLS-15 behaviour.
    pub build_env: Vec<(String, String)>,
}

impl InstancePaths {
    pub fn bundle_dir(&self, sha: &str) -> PathBuf {
        self.bundles.join(sha)
    }
}

/// A spawned child the driver tracks. `token` is launcher-opaque (used to
/// stop the child); `port` is what the proxy points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildHandle {
    pub port: u16,
    pub token: u64,
}

/// Effectful build seam. `start` kicks a build for `(instance, sha,
/// generation)`; the implementation eventually calls back via the
/// [`EventSink`] with `Event::BuildFinished { generation, .. }`. Production
/// spawns a detached thread; the test backend resolves synchronously into a
/// queued reply the harness drains.
pub trait BuildBackend: Send + Sync {
    fn start(&self, instance: &str, sha: &str, generation: Generation, paths: &InstancePaths);
}

/// Effectful child-process seam (spawn / probe / stop). Same shape the bin
/// crate implements over real processes; tests inject in-process fakes.
pub trait ChildLauncher: Send + Sync {
    /// Spawn the app for `instance` from `bundle_dir` on `port`. Err ⇒ the
    /// driver posts `ProbeFailed` (a failed boot is red, never a promote).
    fn spawn(
        &self,
        instance: &str,
        bundle_dir: &Path,
        port: u16,
        env: &[(String, String)],
    ) -> Result<ChildHandle, String>;

    /// Begin a health probe for `(instance, generation)` on `port`; the
    /// implementation calls back with `ProbeSucceeded`/`ProbeFailed`.
    fn start_probe(&self, instance: &str, generation: Generation, port: u16);

    /// Stop a child (drain grace + SIGTERM tree); calls back `DrainComplete`
    /// for a drained promote, nothing for a killed standby. Idempotent.
    fn stop(&self, instance: &str, generation: Generation, token: u64, drain: bool);
}

/// Where the driver posts events it generates synchronously while executing
/// actions (e.g. "no free port" ⇒ an immediate ProbeFailed). Production wires
/// this to the same mpsc the async callbacks use; tests capture into a Vec.
pub trait EventSink: Send + Sync {
    fn post(&self, instance: &str, event: Event);
}

/// A port allocator over `--port-range`, recycling on child exit.
#[derive(Debug)]
pub struct PortAllocator {
    inner: std::sync::Mutex<PortAllocInner>,
}

#[derive(Debug)]
struct PortAllocInner {
    next: u16,
    end: u16,
    free: Vec<u16>,
}

impl PortAllocator {
    pub fn new(start: u16, end: u16) -> Self {
        Self {
            inner: std::sync::Mutex::new(PortAllocInner {
                next: start,
                end,
                free: Vec::new(),
            }),
        }
    }

    /// Next free port, preferring recycled. `None` ⇒ range exhausted.
    pub fn alloc(&self) -> Option<u16> {
        let mut g = self.inner.lock().expect("port alloc");
        if let Some(p) = g.free.pop() {
            return Some(p);
        }
        if g.next <= g.end {
            let p = g.next;
            g.next += 1;
            Some(p)
        } else {
            None
        }
    }

    pub fn release(&self, port: u16) {
        let mut g = self.inner.lock().expect("port alloc");
        if !g.free.contains(&port) {
            g.free.push(port);
        }
    }
}

/// One instance's driver-side resources (the pure phase lives in `AppState`).
struct Runtime {
    slot: Arc<UpstreamSlot>,
    paths: InstancePaths,
    env: Vec<(String, String)>,
    /// Children by generation (promoted, probing, or draining).
    children: BTreeMap<Generation, ChildHandle>,
}

/// The control-loop driver. Generic over both effectful seams.
pub struct Driver<B: BuildBackend, L: ChildLauncher, S: EventSink> {
    state: AppState,
    runtimes: BTreeMap<String, Runtime>,
    build: Arc<B>,
    launcher: Arc<L>,
    sink: Arc<S>,
    svc: Arc<AppServeState>,
    state_dir: PathBuf,
    ports: Arc<PortAllocator>,
    now: fn() -> u64,
}

/// One instance's static configuration handed to [`Driver::new`].
pub struct InstanceConfig {
    pub name: String,
    pub slot: Arc<UpstreamSlot>,
    pub paths: InstancePaths,
    pub env: Vec<(String, String)>,
}

/// The effectful seams + shared services the driver is wired to. Bundled so
/// [`Driver::new`] stays under the argument-count lint and the wiring reads as
/// one unit (the production daemon builds this once; tests build a fake one).
pub struct Backends<B: BuildBackend, L: ChildLauncher, S: EventSink> {
    pub build: Arc<B>,
    pub launcher: Arc<L>,
    pub sink: Arc<S>,
    pub svc: Arc<AppServeState>,
    pub ports: Arc<PortAllocator>,
    /// Clock for state-file heartbeats (injected for test determinism).
    pub now: fn() -> u64,
    /// How many instances may build concurrently (CGLS-15). Default 1 =
    /// original serialised behaviour. Values < 1 are clamped to 1.
    pub max_concurrent_builds: usize,
}

impl<B: BuildBackend, L: ChildLauncher, S: EventSink> Driver<B, L, S> {
    /// Build a driver over `instances` (boot order preserved), wired to
    /// `backends`, with durable state under `state_dir`.
    pub fn new(
        instances: Vec<InstanceConfig>,
        backends: Backends<B, L, S>,
        state_dir: PathBuf,
    ) -> Self {
        let state = AppState::with_max_concurrent(
            instances.iter().map(|i| i.name.clone()),
            backends.max_concurrent_builds,
        );
        let runtimes = instances
            .into_iter()
            .map(|i| {
                (
                    i.name,
                    Runtime {
                        slot: i.slot,
                        paths: i.paths,
                        env: i.env,
                        children: BTreeMap::new(),
                    },
                )
            })
            .collect();
        let d = Self {
            state,
            runtimes,
            build: backends.build,
            launcher: backends.launcher,
            sink: backends.sink,
            svc: backends.svc,
            state_dir,
            ports: backends.ports,
            now: backends.now,
        };
        d.publish();
        d
    }

    /// Feed one observed event into the state machine and execute every
    /// resulting action. THE control-thread entry point.
    pub fn drive(&mut self, instance: &str, event: Event) {
        let actions = self.state.step(instance, event);
        for action in actions {
            self.execute(action);
        }
        // One publish per event keeps the read-plane snapshot and durable
        // state-file current after the whole action batch settled.
        self.persist_and_publish(instance);
    }

    fn execute(&mut self, action: Action) {
        match action {
            Action::StartBuild {
                instance,
                sha,
                generation,
            } => {
                if let Some(rt) = self.runtimes.get(&instance) {
                    self.build.start(&instance, &sha, generation, &rt.paths);
                }
            }
            Action::SpawnAndProbe {
                instance,
                sha,
                generation,
            }
            | Action::Respawn {
                instance,
                sha,
                generation,
            } => self.spawn_and_probe(&instance, &sha, generation),
            Action::Promote {
                instance,
                generation,
                ..
            } => self.promote(&instance, generation),
            Action::StartDrain {
                instance,
                generation,
            } => self.stop_child(&instance, generation, true),
            Action::KillStandby {
                instance,
                generation,
            } => self.stop_child(&instance, generation, false),
            Action::RecordRed { .. } => {
                // last_red is already in the state machine; the per-event
                // persist_and_publish below mirrors it to disk + /app.
            }
        }
    }

    fn spawn_and_probe(&mut self, instance: &str, sha: &str, generation: Generation) {
        // Read what we need from the runtime, then DROP the borrow before any
        // other `self.*` call — `ports`/`sink`/`launcher` are disjoint fields
        // but each access reborrows `self`, so the runtime borrow can't be
        // live across them.
        let (bundle_dir, env) = match self.runtimes.get(instance) {
            Some(rt) => (rt.paths.bundle_dir(sha), rt.env.clone()),
            None => return,
        };
        let Some(port) = self.ports.alloc() else {
            self.sink.post(
                instance,
                Event::ProbeFailed {
                    generation,
                    reason: "no free app port in --port-range".to_string(),
                },
            );
            return;
        };
        match self.launcher.spawn(instance, &bundle_dir, port, &env) {
            Ok(handle) => {
                if let Some(rt) = self.runtimes.get_mut(instance) {
                    rt.children.insert(generation, handle);
                }
                self.launcher.start_probe(instance, generation, port);
            }
            Err(e) => {
                self.ports.release(port);
                self.sink.post(
                    instance,
                    Event::ProbeFailed {
                        generation,
                        reason: format!("spawn failed: {e}"),
                    },
                );
            }
        }
    }

    /// THE single promote site.
    fn promote(&mut self, instance: &str, generation: Generation) {
        let Some(rt) = self.runtimes.get(instance) else {
            return;
        };
        if let Some(child) = rt.children.get(&generation) {
            rt.slot.set(child.port, generation);
        }
        // inc-6: bound disk after a promote landed a new bundle. Protect the
        // live set — the new serving sha + last_green (both set by the state
        // machine before this action runs) — and keep the 1 next-newest so a
        // fast rollback has a warm previous bundle. A still-draining child's
        // bundle is protected because its generation maps to a sha we include.
        self.prune_instance_bundles(instance);
    }

    /// Prune this instance's bundle dir, never deleting a live/recovery
    /// bundle. Protected = currently-serving sha + last_green sha + every sha a
    /// tracked child (promoted or draining) still runs from.
    fn prune_instance_bundles(&self, instance: &str) {
        let Some(rt) = self.runtimes.get(instance) else {
            return;
        };
        let Some(st) = self.state.instance(instance) else {
            return;
        };
        let mut protected: Vec<String> = Vec::new();
        if let Some(s) = &st.serving {
            protected.push(s.sha.clone());
        }
        if let Some(g) = &st.last_green {
            protected.push(g.clone());
        }
        let refs: Vec<&str> = protected.iter().map(String::as_str).collect();
        // keep_extra = 1: a warm previous bundle for instant rollback.
        let _ = crate::appbuild::prune_bundles(&rt.paths.bundles, &refs, 1);
    }

    fn stop_child(&mut self, instance: &str, generation: Generation, drain: bool) {
        // Copy the handle out (and, for a kill, remove it) before touching
        // `self.launcher`/`self.ports` — the runtime borrow can't be live
        // across those reborrows of `self`.
        let child = match self.runtimes.get_mut(instance) {
            Some(rt) if !drain => rt.children.remove(&generation),
            Some(rt) => rt.children.get(&generation).copied(),
            None => return,
        };
        let Some(child) = child else {
            return;
        };
        self.launcher.stop(instance, generation, child.token, drain);
        if !drain {
            // A killed standby never served: reclaim its port now. A
            // *draining* child keeps its port until DrainComplete.
            self.ports.release(child.port);
        }
    }

    /// Register a freshly-configured instance that was not present at boot
    /// (SIGHUP hot-add). The instance starts `Idle`; the ref poller the caller
    /// starts will post the first `HeadAdvanced` to kick off the build.
    pub fn add_instance(&mut self, config: InstanceConfig) {
        self.state.add_instance(config.name.clone());
        self.runtimes.insert(
            config.name,
            Runtime {
                slot: config.slot,
                paths: config.paths,
                env: config.env,
                children: BTreeMap::new(),
            },
        );
        self.publish();
    }

    /// Tear down an instance (SIGHUP hot-remove): stop every tracked child
    /// immediately (no drain grace — the operator explicitly removed it), clear
    /// the upstream slot, then remove from state and runtime. The call is safe
    /// to repeat; an unknown instance is silently ignored.
    pub fn remove_instance(&mut self, instance: &str) {
        // Kill all tracked children (serving + draining).  We copy the
        // handles out first to avoid holding the runtime borrow across the
        // launcher call (the disjoint-fields borrow rule).
        let handles: Vec<(Generation, ChildHandle)> = self
            .runtimes
            .get(instance)
            .map(|rt| rt.children.iter().map(|(&g, &h)| (g, h)).collect())
            .unwrap_or_default();
        for (generation, child) in handles {
            self.launcher.stop(instance, generation, child.token, false);
            self.ports.release(child.port);
        }
        // Clear the proxy slot so new connections see "no upstream" immediately.
        if let Some(rt) = self.runtimes.get(instance) {
            rt.slot.clear();
        }
        self.runtimes.remove(instance);
        self.state.remove_instance(instance);
        self.publish();
    }

    /// Update the per-instance env overlay (SIGHUP env change). Takes effect
    /// on the *next* child spawn; the currently-serving child is not restarted.
    pub fn update_instance_env(&mut self, instance: &str, env: Vec<(String, String)>) {
        if let Some(rt) = self.runtimes.get_mut(instance) {
            rt.env = env;
        }
    }

    /// Called by the loop when a drain finishes: reclaim the child's port.
    pub fn on_drain_reclaimed(&mut self, instance: &str, generation: Generation) {
        let child = match self.runtimes.get_mut(instance) {
            Some(rt) => rt.children.remove(&generation),
            None => return,
        };
        if let Some(child) = child {
            self.ports.release(child.port);
        }
    }

    fn persist_and_publish(&self, instance: &str) {
        if let Some(inst) = self.state.instance(instance) {
            let _ = crate::appstatefile::write(&self.state_dir, instance, inst, (self.now)());
        }
        self.publish();
    }

    fn publish(&self) {
        let reports: Vec<InstanceReport> = self
            .state
            .instances()
            .map(|(name, st)| InstanceReport::from_state(name, st))
            .collect();
        self.svc.publish(reports);
    }

    /// Read-only access for tests / `/app` introspection parity.
    pub fn state(&self) -> &AppState {
        &self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appstate::AppBuildOutcome;
    // `ready()` is a VerdictService method; the trait must be in scope to
    // call it on the AppServeState behind `svc`.
    use crate::transport::VerdictService;
    use std::sync::Mutex;

    #[test]
    fn port_allocator_hands_out_and_recycles_without_duplicates() {
        let pa = PortAllocator::new(8090, 8092);
        assert_eq!(pa.alloc(), Some(8090));
        assert_eq!(pa.alloc(), Some(8091));
        assert_eq!(pa.alloc(), Some(8092));
        assert_eq!(pa.alloc(), None);
        pa.release(8091);
        pa.release(8091); // double release must not duplicate
        assert_eq!(pa.alloc(), Some(8091));
        assert_eq!(pa.alloc(), None);
    }

    /// Records every effectful call so a test can assert the driver's actions.
    #[derive(Default)]
    struct Recorder {
        builds: Mutex<Vec<(String, String, Generation)>>,
        spawns: Mutex<Vec<(String, u16, Generation)>>,
        probes: Mutex<Vec<(String, Generation, u16)>>,
        stops: Mutex<Vec<(String, Generation, bool)>>,
        events: Mutex<Vec<(String, Event)>>,
        next_token: std::sync::atomic::AtomicU64,
        spawn_ok: bool,
    }

    impl BuildBackend for Recorder {
        fn start(&self, instance: &str, sha: &str, generation: Generation, _p: &InstancePaths) {
            self.builds
                .lock()
                .unwrap()
                .push((instance.into(), sha.into(), generation));
        }
    }

    impl ChildLauncher for Recorder {
        fn spawn(
            &self,
            instance: &str,
            _bundle: &Path,
            port: u16,
            _env: &[(String, String)],
        ) -> Result<ChildHandle, String> {
            if !self.spawn_ok {
                return Err("boom".into());
            }
            self.spawns.lock().unwrap().push((instance.into(), port, 0));
            Ok(ChildHandle {
                port,
                token: self
                    .next_token
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            })
        }
        fn start_probe(&self, instance: &str, generation: Generation, port: u16) {
            self.probes
                .lock()
                .unwrap()
                .push((instance.into(), generation, port));
        }
        fn stop(&self, instance: &str, generation: Generation, _token: u64, drain: bool) {
            self.stops
                .lock()
                .unwrap()
                .push((instance.into(), generation, drain));
        }
    }

    impl EventSink for Recorder {
        fn post(&self, instance: &str, event: Event) {
            self.events.lock().unwrap().push((instance.into(), event));
        }
    }

    fn driver(
        names: &[&str],
        rec: Arc<Recorder>,
        ports: Arc<PortAllocator>,
    ) -> Driver<Recorder, Recorder, Recorder> {
        let svc = Arc::new(AppServeState::new());
        let mut dir = std::env::temp_dir();
        dir.push(format!("cargoless-appdrv-{}", std::process::id()));
        let instances = names
            .iter()
            .map(|n| InstanceConfig {
                name: n.to_string(),
                slot: Arc::new(UpstreamSlot::new()),
                paths: InstancePaths {
                    worktree: dir.join(n).join("wt"),
                    bundles: dir.join(n).join("bundles"),
                    build_env: Vec::new(),
                },
                env: Vec::new(),
            })
            .collect();
        Driver::new(
            instances,
            Backends {
                build: rec.clone(),
                launcher: rec.clone(),
                sink: rec,
                svc,
                ports,
                now: || 0,
                max_concurrent_builds: 1,
            },
            dir,
        )
    }

    fn slots(d: &Driver<Recorder, Recorder, Recorder>, instance: &str) -> Arc<UpstreamSlot> {
        d.runtimes.get(instance).unwrap().slot.clone()
    }

    #[test]
    fn happy_path_build_spawn_probe_promote_flips_the_slot() {
        let rec = Arc::new(Recorder {
            spawn_ok: true,
            ..Default::default()
        });
        let ports = Arc::new(PortAllocator::new(9000, 9009));
        let mut d = driver(&["dev"], rec.clone(), ports);
        let slot = slots(&d, "dev");

        // HEAD advances ⇒ the driver asks the build backend to build.
        d.drive("dev", Event::HeadAdvanced { sha: "aaa".into() });
        let builds = rec.builds.lock().unwrap().clone();
        assert_eq!(builds.len(), 1);
        let g = builds[0].2;

        // Build finishes green ⇒ spawn + probe.
        d.drive(
            "dev",
            Event::BuildFinished {
                generation: g,
                outcome: AppBuildOutcome::Green,
            },
        );
        let spawns = rec.spawns.lock().unwrap().clone();
        assert_eq!(spawns.len(), 1, "green ⇒ one spawn");
        let port = spawns[0].1;
        assert_eq!(rec.probes.lock().unwrap().len(), 1, "green ⇒ one probe");
        assert_eq!(slot.get(), None, "not promoted until the probe succeeds");

        // Probe succeeds ⇒ THE promote: the proxy slot flips to the child.
        d.drive("dev", Event::ProbeSucceeded { generation: g });
        let up = slot.get().expect("slot points at the promoted child");
        assert_eq!(up.port, port);
        assert_eq!(up.generation, g);
        assert!(d.svc.ready(), "serving ⇒ /readyz latched");
    }

    #[test]
    fn failed_spawn_posts_probe_failed_and_recycles_the_port() {
        let rec = Arc::new(Recorder {
            spawn_ok: false,
            ..Default::default()
        });
        let ports = Arc::new(PortAllocator::new(9000, 9000)); // exactly one port
        let mut d = driver(&["dev"], rec.clone(), ports.clone());

        d.drive("dev", Event::HeadAdvanced { sha: "aaa".into() });
        let g = rec.builds.lock().unwrap()[0].2;
        d.drive(
            "dev",
            Event::BuildFinished {
                generation: g,
                outcome: AppBuildOutcome::Green,
            },
        );
        // Spawn failed ⇒ a ProbeFailed event was posted to the sink…
        let events = rec.events.lock().unwrap().clone();
        assert!(
            events
                .iter()
                .any(|(i, e)| i == "dev" && matches!(e, Event::ProbeFailed { .. })),
            "failed spawn posts ProbeFailed: {events:?}"
        );
        // …and the port was returned to the pool (re-allocatable).
        assert_eq!(
            ports.alloc(),
            Some(9000),
            "port recycled after failed spawn"
        );
    }

    #[test]
    fn promote_then_new_green_drains_the_old_child() {
        let rec = Arc::new(Recorder {
            spawn_ok: true,
            ..Default::default()
        });
        let ports = Arc::new(PortAllocator::new(9000, 9009));
        let mut d = driver(&["dev"], rec.clone(), ports);

        // First green → serving.
        d.drive("dev", Event::HeadAdvanced { sha: "aaa".into() });
        let g1 = rec.builds.lock().unwrap()[0].2;
        d.drive(
            "dev",
            Event::BuildFinished {
                generation: g1,
                outcome: AppBuildOutcome::Green,
            },
        );
        d.drive("dev", Event::ProbeSucceeded { generation: g1 });

        // Second green → promote + drain the first.
        d.drive("dev", Event::HeadAdvanced { sha: "bbb".into() });
        let g2 = rec
            .builds
            .lock()
            .unwrap()
            .iter()
            .map(|b| b.2)
            .max()
            .unwrap();
        d.drive(
            "dev",
            Event::BuildFinished {
                generation: g2,
                outcome: AppBuildOutcome::Green,
            },
        );
        d.drive("dev", Event::ProbeSucceeded { generation: g2 });

        let stops = rec.stops.lock().unwrap().clone();
        assert!(
            stops
                .iter()
                .any(|(i, g, drain)| i == "dev" && *g == g1 && *drain),
            "old generation g1 is drained (drain=true): {stops:?}"
        );
    }

    #[test]
    fn red_build_keeps_serving_and_never_spawns() {
        let rec = Arc::new(Recorder {
            spawn_ok: true,
            ..Default::default()
        });
        let ports = Arc::new(PortAllocator::new(9000, 9009));
        let mut d = driver(&["dev"], rec.clone(), ports);

        // Establish a green.
        d.drive("dev", Event::HeadAdvanced { sha: "aaa".into() });
        let g1 = rec.builds.lock().unwrap()[0].2;
        d.drive(
            "dev",
            Event::BuildFinished {
                generation: g1,
                outcome: AppBuildOutcome::Green,
            },
        );
        d.drive("dev", Event::ProbeSucceeded { generation: g1 });
        let slot = slots(&d, "dev");
        let serving_before = slot.get();
        let spawns_before = rec.spawns.lock().unwrap().len();

        // A new red build must not spawn anything or touch the slot.
        d.drive("dev", Event::HeadAdvanced { sha: "bad".into() });
        let g2 = rec
            .builds
            .lock()
            .unwrap()
            .iter()
            .map(|b| b.2)
            .max()
            .unwrap();
        d.drive(
            "dev",
            Event::BuildFinished {
                generation: g2,
                outcome: AppBuildOutcome::Red {
                    reason: "boom".into(),
                },
            },
        );
        assert_eq!(
            rec.spawns.lock().unwrap().len(),
            spawns_before,
            "red build spawns nothing"
        );
        assert_eq!(slot.get(), serving_before, "slot untouched by a red build");
    }
}
