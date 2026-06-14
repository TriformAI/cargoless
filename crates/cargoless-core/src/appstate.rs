//! appstate — the **pure** lifecycle state machine for the app-serve tier.
//!
//! Zero I/O by construction: the daemon (`appdrv` in the bin crate) feeds
//! [`Event`]s in and executes the returned [`Action`]s; everything between —
//! per-instance blue/green lifecycle, the serialized build queue, generation
//! guards, red bookkeeping — is decided here, where it is exhaustively unit
//! tested with no threads, no sockets, no sleeps. This is the same cut that
//! keeps `model` testable in the check tier: the irreducibly racy edges
//! (subprocesses, health polls) live in the driver; the *decisions* do not.
//!
//! ## The two axes of an instance
//!
//! ```text
//!   serving:  Option<ServingChild>        what the L4 proxy points at
//!   pipeline: Idle | Queued{sha} | Building{sha,gen} | Probing{sha,gen}
//! ```
//!
//! "Never serve red" (AC#4, extended) falls out structurally: red paths only
//! ever write `pipeline` and `last_red`; the **only** writer of `serving` is
//! a successful probe (promote) or the serving child's own exit. A failed
//! build or boot cannot touch the running app even by accident.
//!
//! ## Build-queue arbitration (serialized, newest-sha-wins)
//!
//! Builds are serialized daemon-wide (one shared `CARGO_TARGET_DIR`; two
//! concurrent 40 GB-target builds would just contend). The queue is FIFO
//! across instances; *within* an instance the newest sha always supersedes a
//! queued older one (a queued build that nobody wants is pure waste), while
//! a *running* build is never cancelled — its sha was HEAD when it started,
//! its green is real, and it promotes before the newer sha builds
//! (latest-green per ref, applied in order). Probing does **not** hold the
//! build slot: the moment a build finishes, the next instance's build
//! dispatches while the first instance's child boots.
//!
//! ## Generations
//!
//! Every spawned activity (build attempt, probe attempt) carries a
//! daemon-unique generation. Results are only accepted if the generation
//! matches the in-flight pipeline — a late result from a superseded attempt
//! is discarded, never replayed (the hard-witness discipline: detached
//! workers are never joined, so their stale completions must be cheap to
//! ignore).
//!
//! ## Red discipline
//!
//! A red sha is recorded per instance and **never auto-retried**: only a new
//! HEAD on that ref queues again. The one exception is
//! [`AppBuildOutcome::Indeterminate`] (the tree-mutated-mid-build backstop):
//! it requeues the same sha once; a second consecutive indeterminate on the
//! same instance records red — self-limiting, observable.

use std::collections::BTreeMap;

/// Daemon-unique activity generation (build/probe attempts).
pub type Generation = u64;

/// Outcome of one manifest-driven build attempt, as reported by the build
/// worker. Distinct from `cargoless_proto::BuildOutcome` (the frozen v0
/// artifact-publisher seam): this one is app-serve-internal and carries the
/// indeterminate backstop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppBuildOutcome {
    /// Every step exited 0 and the bundle was harvested.
    Green,
    /// A step failed (or harvest failed); `reason` names the step.
    Red { reason: String },
    /// The build cannot be trusted (e.g. the worktree changed underneath
    /// it). Not red — requeued once, then red on a repeat.
    Indeterminate { reason: String },
}

/// What the proxy currently points at (the promoted child).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServingChild {
    pub sha: String,
    pub generation: Generation,
}

/// The per-instance pipeline slot — at most one in-flight activity.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Pipeline {
    #[default]
    Idle,
    /// Waiting in the daemon-wide build queue.
    Queued { sha: String },
    /// The (single) build worker is on this instance.
    Building { sha: String, generation: Generation },
    /// Child spawned, health poll running. `respawn` marks a recovery boot
    /// from an existing bundle (no build preceded it).
    Probing {
        sha: String,
        generation: Generation,
        respawn: bool,
    },
}

/// Everything the daemon knows about one instance. Fields are public for
/// read-only reporting (`/app`); all mutation goes through [`AppState::step`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstanceState {
    pub serving: Option<ServingChild>,
    pub pipeline: Pipeline,
    /// Newest sha seen while the pipeline was busy (newest-sha-wins).
    pub pending: Option<String>,
    /// Last red `(sha, reason)` — that sha is never auto-retried.
    pub last_red: Option<(String, String)>,
    /// Most recent successfully promoted sha (survives a serving-child
    /// death; names the bundle a recovery respawn boots from).
    pub last_green: Option<String>,
    /// Generations of demoted children still draining connections.
    pub draining: Vec<Generation>,
    /// The serving child died while the pipeline was busy; restore serving
    /// from `last_green` as soon as the pipeline frees up.
    pub needs_respawn: bool,
    /// Consecutive `Indeterminate` builds for this instance — internal
    /// backstop bookkeeping (requeue once, then red). `pub` only so sibling
    /// modules (appsvc/appstatefile tests, future read-plane) can build an
    /// `InstanceState` literal with `..Default::default()`; not part of the
    /// lifecycle contract — only [`AppState::step`] ever advances it.
    pub indeterminate_streak: u8,
}

/// Input to [`AppState::step`] — everything the daemon can observe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The ref poller resolved the instance's ref to a new sha.
    HeadAdvanced { sha: String },
    /// The detached build worker finished (or its result arrived late).
    BuildFinished {
        generation: Generation,
        outcome: AppBuildOutcome,
    },
    /// Health poll got a 200 within budget.
    ProbeSucceeded { generation: Generation },
    /// Health poll exhausted its budget (or the child exited pre-promote).
    ProbeFailed {
        generation: Generation,
        reason: String,
    },
    /// The **promoted** child exited. (Draining children exiting is the
    /// expected end of a drain — that is `DrainComplete`.)
    ServingExited { generation: Generation },
    /// The driver finished tearing down a demoted child.
    DrainComplete { generation: Generation },
    /// Boot-time recovery: a latest-green pointer + bundle exist on disk.
    RecoverFromPointer { sha: String },
}

/// Output of [`AppState::step`] — orders for the driver. Each names its
/// instance: arbitration means one instance's event can dispatch another
/// instance's build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Check out `sha` in the instance worktree and run the manifest steps
    /// (on the detached build worker).
    StartBuild {
        instance: String,
        sha: String,
        generation: Generation,
    },
    /// Spawn the freshly built bundle's child and start the health poll.
    SpawnAndProbe {
        instance: String,
        sha: String,
        generation: Generation,
    },
    /// Spawn from an **existing** bundle (crash recovery / boot) and probe.
    Respawn {
        instance: String,
        sha: String,
        generation: Generation,
    },
    /// THE single promote site: flip the instance proxy upstream, advance
    /// the per-instance pointer, write the state file, emit SSE.
    Promote {
        instance: String,
        sha: String,
        generation: Generation,
    },
    /// Demote the previous serving child: stop new connections, grace
    /// period, then SIGTERM the tree.
    StartDrain {
        instance: String,
        generation: Generation,
    },
    /// Kill a standby child that never got promoted (probe failure).
    KillStandby {
        instance: String,
        generation: Generation,
    },
    /// Durably record a red attempt (state file, SSE, telemetry).
    RecordRed {
        instance: String,
        sha: String,
        reason: String,
    },
}

/// What [`AppState::remove_instance`] hands the driver: the children to stop
/// and any actions unblocked by freeing the build slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    /// Generations of this instance's still-tracked children (serving +
    /// draining) the driver must `stop` to free their ports/processes.
    pub child_generations: Vec<Generation>,
    /// Actions newly unblocked by the removal (e.g. a `StartBuild` for the
    /// queued instance that inherits the freed build slot).
    pub actions: Vec<Action>,
}

/// The daemon-wide state: every instance plus the serialized build queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    instances: BTreeMap<String, InstanceState>,
    /// Instances waiting for the build slot, FIFO. An instance appears here
    /// iff its pipeline is `Queued` (the sha lives there).
    queue: Vec<String>,
    /// The instance currently holding the (single) build slot.
    building: Option<String>,
    next_generation: Generation,
}

impl AppState {
    /// Seed one `Idle` instance per configured name (instances-file order).
    pub fn new<I: IntoIterator<Item = String>>(names: I) -> Self {
        Self {
            instances: names
                .into_iter()
                .map(|n| (n, InstanceState::default()))
                .collect(),
            queue: Vec::new(),
            building: None,
            next_generation: 1,
        }
    }

    pub fn instance(&self, name: &str) -> Option<&InstanceState> {
        self.instances.get(name)
    }

    pub fn instances(&self) -> impl Iterator<Item = (&String, &InstanceState)> {
        self.instances.iter()
    }

    /// Which instance currently holds the build slot, if any.
    pub fn building(&self) -> Option<&str> {
        self.building.as_deref()
    }

    /// Add a fresh `Idle` instance at runtime (self-serve previews). Returns
    /// `false` if an instance of that name already exists (the caller should
    /// reject a duplicate add rather than clobber a live instance). A newly
    /// added instance has no serving child and an empty pipeline; the driver
    /// then drives a `HeadAdvanced` to start its first build.
    pub fn add_instance(&mut self, name: String) -> bool {
        if self.instances.contains_key(&name) {
            return false;
        }
        self.instances.insert(name, InstanceState::default());
        true
    }

    /// Remove an instance at runtime. `None` if it does not exist; otherwise a
    /// [`Removal`] carrying the child generations the driver must stop (the
    /// serving child + any draining) and any `actions` newly unblocked —
    /// removing the build-slot holder lets a queued instance dispatch, so the
    /// freed slot is handed out immediately rather than wedging until the next
    /// event.
    pub fn remove_instance(&mut self, name: &str) -> Option<Removal> {
        let inst = self.instances.remove(name)?;
        self.queue.retain(|n| n != name);
        let freed_slot = self.building.as_deref() == Some(name);
        if freed_slot {
            self.building = None;
        }
        // Every child the driver still tracks for this instance must be
        // stopped: the promoted serving child plus any draining generations.
        let mut child_generations: Vec<Generation> = inst.draining.clone();
        if let Some(serving) = &inst.serving {
            child_generations.push(serving.generation);
        }
        // If this instance held the slot, hand it to the next queued instance.
        let mut actions = Vec::new();
        if freed_slot {
            self.dispatch(&mut actions);
        }
        Some(Removal {
            child_generations,
            actions,
        })
    }

    #[cfg(test)]
    fn queue_contains(&self, instance: &str) -> bool {
        self.queue.iter().any(|n| n == instance)
    }

    /// Advance one instance by one observed event; returns the actions the
    /// driver must execute (possibly for *other* instances — queue
    /// arbitration). Unknown instances and stale generations no-op.
    pub fn step(&mut self, instance: &str, event: Event) -> Vec<Action> {
        if !self.instances.contains_key(instance) {
            return Vec::new();
        }
        let mut actions = Vec::new();
        match event {
            Event::HeadAdvanced { sha } => self.on_head(instance, sha, &mut actions),
            Event::BuildFinished {
                generation,
                outcome,
            } => self.on_build_finished(instance, generation, outcome, &mut actions),
            Event::ProbeSucceeded { generation } => {
                self.on_probe_succeeded(instance, generation, &mut actions)
            }
            Event::ProbeFailed { generation, reason } => {
                self.on_probe_failed(instance, generation, reason, &mut actions)
            }
            Event::ServingExited { generation } => {
                self.on_serving_exited(instance, generation, &mut actions)
            }
            Event::DrainComplete { generation } => {
                let inst = self.instances.get_mut(instance).expect("checked above");
                inst.draining.retain(|g| *g != generation);
            }
            Event::RecoverFromPointer { sha } => self.on_recover(instance, sha, &mut actions),
        }
        actions
    }

    fn alloc_generation(&mut self) -> Generation {
        let g = self.next_generation;
        self.next_generation += 1;
        g
    }

    fn on_head(&mut self, instance: &str, sha: String, actions: &mut Vec<Action>) {
        let inst = self.instances.get_mut(instance).expect("checked");
        match &mut inst.pipeline {
            Pipeline::Idle => {
                self.request_build(instance, sha);
                self.dispatch(actions);
            }
            Pipeline::Queued { sha: queued } => {
                // Newest-sha-wins: supersede in place, keep queue position.
                if *queued != sha {
                    *queued = sha;
                }
            }
            Pipeline::Building { sha: busy, .. } | Pipeline::Probing { sha: busy, .. } => {
                if *busy != sha {
                    inst.pending = Some(sha);
                }
            }
        }
    }

    fn on_build_finished(
        &mut self,
        instance: &str,
        generation: Generation,
        outcome: AppBuildOutcome,
        actions: &mut Vec<Action>,
    ) {
        let inst = self.instances.get_mut(instance).expect("checked");
        let sha = match &inst.pipeline {
            Pipeline::Building { sha, generation: g } if *g == generation => sha.clone(),
            _ => return, // stale or out-of-phase result: discard
        };
        // Free the build slot regardless of outcome.
        if self.building.as_deref() == Some(instance) {
            self.building = None;
        }
        let inst = self.instances.get_mut(instance).expect("checked");
        match outcome {
            AppBuildOutcome::Green => {
                inst.indeterminate_streak = 0;
                inst.pipeline = Pipeline::Probing {
                    sha: sha.clone(),
                    generation,
                    respawn: false,
                };
                actions.push(Action::SpawnAndProbe {
                    instance: instance.to_string(),
                    sha,
                    generation,
                });
            }
            AppBuildOutcome::Red { reason } => {
                inst.indeterminate_streak = 0;
                inst.pipeline = Pipeline::Idle;
                self.record_red(instance, sha, reason, actions);
                self.settle_idle(instance, actions);
            }
            AppBuildOutcome::Indeterminate { reason } => {
                let inst = self.instances.get_mut(instance).expect("checked");
                inst.pipeline = Pipeline::Idle;
                if inst.pending.is_some() {
                    // A newer sha is already waiting; abandon the
                    // indeterminate attempt in its favor.
                    inst.indeterminate_streak = 0;
                    self.settle_idle(instance, actions);
                } else if inst.indeterminate_streak >= 1 {
                    inst.indeterminate_streak = 0;
                    self.record_red(
                        instance,
                        sha,
                        format!("indeterminate twice in a row: {reason}"),
                        actions,
                    );
                    self.settle_idle(instance, actions);
                } else {
                    inst.indeterminate_streak += 1;
                    self.request_build(instance, sha);
                }
            }
        }
        self.dispatch(actions);
    }

    fn on_probe_succeeded(
        &mut self,
        instance: &str,
        generation: Generation,
        actions: &mut Vec<Action>,
    ) {
        let inst = self.instances.get_mut(instance).expect("checked");
        let sha = match &inst.pipeline {
            Pipeline::Probing {
                sha, generation: g, ..
            } if *g == generation => sha.clone(),
            _ => return,
        };
        actions.push(Action::Promote {
            instance: instance.to_string(),
            sha: sha.clone(),
            generation,
        });
        if let Some(old) = inst.serving.replace(ServingChild {
            sha: sha.clone(),
            generation,
        }) {
            inst.draining.push(old.generation);
            actions.push(Action::StartDrain {
                instance: instance.to_string(),
                generation: old.generation,
            });
        }
        inst.last_green = Some(sha);
        inst.needs_respawn = false;
        inst.pipeline = Pipeline::Idle;
        self.settle_idle(instance, actions);
        self.dispatch(actions);
    }

    fn on_probe_failed(
        &mut self,
        instance: &str,
        generation: Generation,
        reason: String,
        actions: &mut Vec<Action>,
    ) {
        let inst = self.instances.get_mut(instance).expect("checked");
        let (sha, respawn) = match &inst.pipeline {
            Pipeline::Probing {
                sha,
                generation: g,
                respawn,
            } if *g == generation => (sha.clone(), *respawn),
            _ => return,
        };
        inst.pipeline = Pipeline::Idle;
        actions.push(Action::KillStandby {
            instance: instance.to_string(),
            generation,
        });
        let reason = if respawn {
            format!("respawn of previously-green bundle failed health probe: {reason}")
        } else {
            format!("health probe failed: {reason}")
        };
        self.record_red(instance, sha, reason, actions);
        self.settle_idle(instance, actions);
        self.dispatch(actions);
    }

    fn on_serving_exited(
        &mut self,
        instance: &str,
        generation: Generation,
        actions: &mut Vec<Action>,
    ) {
        // Read what we need and drop the borrow before any `self.` call —
        // the queue/respawn helpers below also borrow `self`. A green bundle
        // on disk means we owe a respawn: record that intent now
        // (`needs_respawn`), and let `try_respawn` consume it as soon as the
        // pipeline is idle. `needs_respawn` is the single "restore is owed"
        // flag — set here on exit, cleared by a respawn or a promote.
        let (matched, pipeline, has_green) = {
            let inst = self.instances.get_mut(instance).expect("checked");
            let matched = inst
                .serving
                .as_ref()
                .is_some_and(|s| s.generation == generation);
            if matched {
                inst.serving = None;
                inst.needs_respawn = inst.last_green.is_some();
            }
            (matched, inst.pipeline.clone(), inst.last_green.is_some())
        };
        if !matched {
            return; // stale: an already-replaced child
        }
        match pipeline {
            Pipeline::Idle => {
                self.try_respawn(instance, actions);
            }
            Pipeline::Queued { sha } if has_green => {
                // Restore serving fast from the existing bundle; the queued
                // build survives as `pending` and re-queues after.
                let inst = self.instances.get_mut(instance).expect("checked");
                inst.pending = Some(sha);
                inst.pipeline = Pipeline::Idle;
                self.queue.retain(|n| n != instance);
                self.try_respawn(instance, actions);
            }
            Pipeline::Queued { .. } => {
                // Nothing green to respawn from; leave it queued to build.
            }
            Pipeline::Building { .. } | Pipeline::Probing { .. } => {
                // Can't preempt the slot; the owed respawn is picked up when
                // the pipeline frees (a successful probe restores serving by
                // itself and clears the flag in `on_probe_succeeded`).
            }
        }
    }

    fn on_recover(&mut self, instance: &str, sha: String, actions: &mut Vec<Action>) {
        let inst = self.instances.get_mut(instance).expect("checked");
        if inst.pipeline != Pipeline::Idle || inst.serving.is_some() {
            return; // defensive: recovery is a boot-time, idle-only event
        }
        inst.last_green = Some(sha);
        inst.needs_respawn = true;
        self.try_respawn(instance, actions);
    }

    /// An idle pipeline just freed up: first restore serving if it is down
    /// (respawn beats a minutes-long build), then promote any pending sha
    /// into the queue.
    fn settle_idle(&mut self, instance: &str, actions: &mut Vec<Action>) {
        self.try_respawn(instance, actions);
        let pending = {
            let inst = self.instances.get_mut(instance).expect("checked");
            if inst.pipeline == Pipeline::Idle {
                inst.pending.take()
            } else {
                None
            }
        };
        if let Some(sha) = pending {
            self.request_build(instance, sha);
        }
    }

    /// Spawn-from-bundle recovery, if needed and possible.
    fn try_respawn(&mut self, instance: &str, actions: &mut Vec<Action>) {
        let needs = {
            let inst = self.instances.get_mut(instance).expect("checked");
            inst.pipeline == Pipeline::Idle
                && inst.serving.is_none()
                && inst.needs_respawn
                && inst.last_green.is_some()
        };
        if !needs {
            return;
        }
        let generation = self.alloc_generation();
        let inst = self.instances.get_mut(instance).expect("checked");
        let sha = inst.last_green.clone().expect("checked above");
        inst.needs_respawn = false;
        inst.pipeline = Pipeline::Probing {
            sha: sha.clone(),
            generation,
            respawn: true,
        };
        actions.push(Action::Respawn {
            instance: instance.to_string(),
            sha,
            generation,
        });
    }

    /// Ask for `sha` to be built: dedupe against what is already serving,
    /// the recorded red sha, and the in-flight pipeline; otherwise join the
    /// build queue. (Pure bookkeeping — `dispatch` hands out the slot.)
    fn request_build(&mut self, instance: &str, sha: String) {
        let inst = self.instances.get_mut(instance).expect("checked");
        if inst.serving.as_ref().is_some_and(|s| s.sha == sha) {
            return; // already serving exactly this sha
        }
        if inst.last_red.as_ref().is_some_and(|(red, _)| *red == sha) {
            return; // red shas are never auto-retried
        }
        match &mut inst.pipeline {
            Pipeline::Idle => {
                inst.pipeline = Pipeline::Queued { sha };
                self.queue.push(instance.to_string());
            }
            Pipeline::Queued { sha: queued } => {
                *queued = sha; // newest-sha-wins in place
            }
            Pipeline::Building { sha: busy, .. } | Pipeline::Probing { sha: busy, .. } => {
                if *busy != sha {
                    inst.pending = Some(sha);
                }
            }
        }
    }

    /// Hand the (single) build slot to the next queued instance, if free.
    fn dispatch(&mut self, actions: &mut Vec<Action>) {
        if self.building.is_some() || self.queue.is_empty() {
            return;
        }
        let instance = self.queue.remove(0);
        let generation = self.alloc_generation();
        let inst = self.instances.get_mut(&instance).expect("queued => exists");
        let sha = match &inst.pipeline {
            Pipeline::Queued { sha } => sha.clone(),
            other => unreachable!("queued instance must be in Queued, was {other:?}"),
        };
        inst.pipeline = Pipeline::Building {
            sha: sha.clone(),
            generation,
        };
        self.building = Some(instance.clone());
        actions.push(Action::StartBuild {
            instance,
            sha,
            generation,
        });
    }

    fn record_red(
        &mut self,
        instance: &str,
        sha: String,
        reason: String,
        actions: &mut Vec<Action>,
    ) {
        let inst = self.instances.get_mut(instance).expect("checked");
        inst.last_red = Some((sha.clone(), reason.clone()));
        actions.push(Action::RecordRed {
            instance: instance.to_string(),
            sha,
            reason,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(names: &[&str]) -> AppState {
        AppState::new(names.iter().map(|s| s.to_string()))
    }

    fn head(s: &mut AppState, inst: &str, sha: &str) -> Vec<Action> {
        s.step(
            inst,
            Event::HeadAdvanced {
                sha: sha.to_string(),
            },
        )
    }

    /// Extract the generation a StartBuild action carries.
    fn build_gen(actions: &[Action]) -> Generation {
        actions
            .iter()
            .find_map(|a| match a {
                Action::StartBuild { generation, .. } => Some(*generation),
                _ => None,
            })
            .expect("a StartBuild action")
    }

    fn green(s: &mut AppState, inst: &str, generation: Generation) -> Vec<Action> {
        s.step(
            inst,
            Event::BuildFinished {
                generation,
                outcome: AppBuildOutcome::Green,
            },
        )
    }

    /// Drive `inst` from HeadAdvanced(sha) all the way to Serving(sha).
    fn drive_to_serving(s: &mut AppState, inst: &str, sha: &str) -> Generation {
        let a = head(s, inst, sha);
        let generation = build_gen(&a);
        let a = green(s, inst, generation);
        assert!(
            a.iter().any(|x| matches!(x, Action::SpawnAndProbe { .. })),
            "green build spawns a standby: {a:?}"
        );
        let a = s.step(inst, Event::ProbeSucceeded { generation });
        assert!(
            a.iter().any(|x| matches!(x, Action::Promote { .. })),
            "successful probe promotes: {a:?}"
        );
        generation
    }

    #[test]
    fn happy_path_builds_probes_promotes() {
        let mut s = state(&["dev"]);
        let actions = head(&mut s, "dev", "aaa");
        assert_eq!(
            actions,
            vec![Action::StartBuild {
                instance: "dev".into(),
                sha: "aaa".into(),
                generation: 1,
            }]
        );
        let actions = green(&mut s, "dev", 1);
        assert_eq!(
            actions,
            vec![Action::SpawnAndProbe {
                instance: "dev".into(),
                sha: "aaa".into(),
                generation: 1,
            }]
        );
        let actions = s.step("dev", Event::ProbeSucceeded { generation: 1 });
        assert_eq!(
            actions,
            vec![Action::Promote {
                instance: "dev".into(),
                sha: "aaa".into(),
                generation: 1,
            }],
            "first promote has no old child to drain"
        );
        let inst = s.instance("dev").unwrap();
        assert_eq!(
            inst.serving,
            Some(ServingChild {
                sha: "aaa".into(),
                generation: 1
            })
        );
        assert_eq!(inst.pipeline, Pipeline::Idle);
        assert_eq!(inst.last_green.as_deref(), Some("aaa"));
    }

    #[test]
    fn promote_drains_the_previous_child() {
        let mut s = state(&["dev"]);
        let g1 = drive_to_serving(&mut s, "dev", "aaa");
        let a = head(&mut s, "dev", "bbb");
        let g2 = build_gen(&a);
        green(&mut s, "dev", g2);
        let actions = s.step("dev", Event::ProbeSucceeded { generation: g2 });
        assert_eq!(
            actions,
            vec![
                Action::Promote {
                    instance: "dev".into(),
                    sha: "bbb".into(),
                    generation: g2,
                },
                Action::StartDrain {
                    instance: "dev".into(),
                    generation: g1,
                },
            ]
        );
        let inst = s.instance("dev").unwrap();
        assert_eq!(inst.serving.as_ref().unwrap().sha, "bbb");
        assert_eq!(inst.draining, vec![g1]);

        // Drain completion is plain bookkeeping.
        s.step("dev", Event::DrainComplete { generation: g1 });
        assert!(s.instance("dev").unwrap().draining.is_empty());
    }

    #[test]
    fn red_build_leaves_serving_untouched_and_sha_is_never_retried() {
        let mut s = state(&["dev"]);
        drive_to_serving(&mut s, "dev", "aaa");

        let a = head(&mut s, "dev", "bad");
        let g = build_gen(&a);
        let actions = s.step(
            "dev",
            Event::BuildFinished {
                generation: g,
                outcome: AppBuildOutcome::Red {
                    reason: "step `server` exited 101".into(),
                },
            },
        );
        assert_eq!(
            actions,
            vec![Action::RecordRed {
                instance: "dev".into(),
                sha: "bad".into(),
                reason: "step `server` exited 101".into(),
            }],
            "no Promote, no StartDrain, no KillStandby — red only records"
        );
        let inst = s.instance("dev").unwrap();
        assert_eq!(
            inst.serving.as_ref().unwrap().sha,
            "aaa",
            "AC#4: serving untouched by a red build"
        );
        assert_eq!(inst.pipeline, Pipeline::Idle);

        // The same sha never auto-retries…
        assert_eq!(head(&mut s, "dev", "bad"), vec![]);
        assert_eq!(s.instance("dev").unwrap().pipeline, Pipeline::Idle);
        // …but a new sha on the ref builds immediately.
        let a = head(&mut s, "dev", "fixed");
        assert_eq!(a.len(), 1);
        assert!(matches!(&a[0], Action::StartBuild { sha, .. } if sha == "fixed"));
    }

    #[test]
    fn probe_failure_kills_standby_and_keeps_serving() {
        let mut s = state(&["dev"]);
        drive_to_serving(&mut s, "dev", "aaa");
        let a = head(&mut s, "dev", "bbb");
        let g = build_gen(&a);
        green(&mut s, "dev", g);
        let actions = s.step(
            "dev",
            Event::ProbeFailed {
                generation: g,
                reason: "no 200 within 120000ms".into(),
            },
        );
        assert_eq!(actions.len(), 2, "{actions:?}");
        assert_eq!(
            actions[0],
            Action::KillStandby {
                instance: "dev".into(),
                generation: g,
            }
        );
        assert!(matches!(
            &actions[1],
            Action::RecordRed { sha, reason, .. }
                if sha == "bbb" && reason.contains("health probe failed")
        ));
        let inst = s.instance("dev").unwrap();
        assert_eq!(inst.serving.as_ref().unwrap().sha, "aaa", "still serving");
        assert_eq!(inst.last_red.as_ref().unwrap().0, "bbb");
    }

    #[test]
    fn stale_generations_are_discarded_everywhere() {
        let mut s = state(&["dev"]);
        let a = head(&mut s, "dev", "aaa");
        let g = build_gen(&a);

        // A result from some other (never-issued / superseded) generation.
        assert_eq!(green(&mut s, "dev", g + 7), vec![]);
        assert!(matches!(
            s.instance("dev").unwrap().pipeline,
            Pipeline::Building { .. }
        ));

        green(&mut s, "dev", g);
        assert_eq!(
            s.step("dev", Event::ProbeSucceeded { generation: g + 7 }),
            vec![]
        );
        assert_eq!(
            s.step(
                "dev",
                Event::ProbeFailed {
                    generation: g + 7,
                    reason: "stale".into()
                }
            ),
            vec![]
        );
        // The real probe still completes normally afterwards.
        let actions = s.step("dev", Event::ProbeSucceeded { generation: g });
        assert!(actions.iter().any(|a| matches!(a, Action::Promote { .. })));

        // Stale ServingExited (an old generation) is ignored too.
        assert_eq!(
            s.step("dev", Event::ServingExited { generation: g + 7 }),
            vec![]
        );
        assert!(s.instance("dev").unwrap().serving.is_some());
    }

    #[test]
    fn builds_serialize_across_instances_and_probing_releases_the_slot() {
        let mut s = state(&["dev", "feature-x"]);
        let a = head(&mut s, "dev", "d1");
        let g_dev = build_gen(&a);

        // feature-x must wait: no StartBuild while dev holds the slot.
        assert_eq!(head(&mut s, "feature-x", "f1"), vec![]);
        assert_eq!(
            s.instance("feature-x").unwrap().pipeline,
            Pipeline::Queued { sha: "f1".into() }
        );

        // dev's build finishing both spawns dev's probe AND dispatches
        // feature-x — probing does not hold the build slot.
        let actions = green(&mut s, "dev", g_dev);
        assert!(matches!(&actions[0], Action::SpawnAndProbe { instance, .. } if instance == "dev"));
        assert!(
            matches!(&actions[1], Action::StartBuild { instance, sha, .. }
                if instance == "feature-x" && sha == "f1"),
            "{actions:?}"
        );
        assert_eq!(s.building(), Some("feature-x"));
    }

    #[test]
    fn newest_sha_wins_supersedes_a_queued_older_sha() {
        let mut s = state(&["dev", "feature-x"]);
        let a = head(&mut s, "dev", "d1");
        let g_dev = build_gen(&a);

        head(&mut s, "feature-x", "f1");
        head(&mut s, "feature-x", "f2"); // pushed again while queued
        assert_eq!(
            s.instance("feature-x").unwrap().pipeline,
            Pipeline::Queued { sha: "f2".into() },
            "queued sha superseded in place"
        );

        let actions = green(&mut s, "dev", g_dev);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::StartBuild { sha, .. } if sha == "f2")),
            "only the newest sha builds — f1 is never built: {actions:?}"
        );
    }

    #[test]
    fn head_advance_during_build_becomes_pending_and_queues_after() {
        let mut s = state(&["dev"]);
        let a = head(&mut s, "dev", "v1");
        let g1 = build_gen(&a);
        head(&mut s, "dev", "v2"); // arrives mid-build: pending
        assert_eq!(s.instance("dev").unwrap().pending.as_deref(), Some("v2"));

        // v1 finishes and promotes; v2 then builds.
        green(&mut s, "dev", g1);
        let actions = s.step("dev", Event::ProbeSucceeded { generation: g1 });
        assert!(matches!(&actions[0], Action::Promote { sha, .. } if sha == "v1"));
        assert!(
            matches!(&actions[1], Action::StartBuild { sha, .. } if sha == "v2"),
            "pending sha queues the moment the pipeline frees: {actions:?}"
        );
        assert_eq!(s.instance("dev").unwrap().pending, None);
    }

    #[test]
    fn duplicate_heads_are_noops() {
        let mut s = state(&["dev"]);
        drive_to_serving(&mut s, "dev", "aaa");
        // Same sha as serving: nothing to do.
        assert_eq!(head(&mut s, "dev", "aaa"), vec![]);

        let a = head(&mut s, "dev", "bbb");
        let _g = build_gen(&a);
        // Same sha as the in-flight build: no pending duplicate.
        assert_eq!(head(&mut s, "dev", "bbb"), vec![]);
        assert_eq!(s.instance("dev").unwrap().pending, None);
    }

    #[test]
    fn indeterminate_requeues_once_then_goes_red() {
        let mut s = state(&["dev"]);
        let a = head(&mut s, "dev", "aaa");
        let g1 = build_gen(&a);
        let actions = s.step(
            "dev",
            Event::BuildFinished {
                generation: g1,
                outcome: AppBuildOutcome::Indeterminate {
                    reason: "tree changed during build".into(),
                },
            },
        );
        // Requeued + dispatched again immediately (slot is free).
        let g2 = build_gen(&actions);
        assert_ne!(g1, g2);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::RecordRed { .. })),
            "first indeterminate is not red: {actions:?}"
        );

        let actions = s.step(
            "dev",
            Event::BuildFinished {
                generation: g2,
                outcome: AppBuildOutcome::Indeterminate {
                    reason: "tree changed during build".into(),
                },
            },
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::RecordRed { reason, .. }
                    if reason.contains("indeterminate twice"))),
            "second consecutive indeterminate records red: {actions:?}"
        );
        assert_eq!(s.instance("dev").unwrap().pipeline, Pipeline::Idle);

        // And a green build resets the streak bookkeeping.
        let a = head(&mut s, "dev", "bbb");
        let g3 = build_gen(&a);
        green(&mut s, "dev", g3);
        assert_eq!(s.instance("dev").unwrap().indeterminate_streak, 0);
    }

    #[test]
    fn indeterminate_with_pending_prefers_the_newer_sha() {
        let mut s = state(&["dev"]);
        let a = head(&mut s, "dev", "v1");
        let g1 = build_gen(&a);
        head(&mut s, "dev", "v2"); // pending while v1 builds
        let actions = s.step(
            "dev",
            Event::BuildFinished {
                generation: g1,
                outcome: AppBuildOutcome::Indeterminate {
                    reason: "tree changed".into(),
                },
            },
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::StartBuild { sha, .. } if sha == "v2")),
            "newer pending sha wins over retrying the indeterminate one: {actions:?}"
        );
    }

    #[test]
    fn serving_exit_while_idle_respawns_from_last_green() {
        let mut s = state(&["dev"]);
        let g = drive_to_serving(&mut s, "dev", "aaa");
        let actions = s.step("dev", Event::ServingExited { generation: g });
        assert_eq!(actions.len(), 1);
        let respawn_gen = match &actions[0] {
            Action::Respawn {
                instance,
                sha,
                generation,
            } => {
                assert_eq!(instance, "dev");
                assert_eq!(sha, "aaa");
                *generation
            }
            other => panic!("expected Respawn, got {other:?}"),
        };
        assert!(s.instance("dev").unwrap().serving.is_none());

        // The respawned child probes and re-promotes.
        let actions = s.step(
            "dev",
            Event::ProbeSucceeded {
                generation: respawn_gen,
            },
        );
        assert!(actions.iter().any(|a| matches!(a, Action::Promote { .. })));
        assert_eq!(
            s.instance("dev").unwrap().serving.as_ref().unwrap().sha,
            "aaa"
        );
    }

    #[test]
    fn serving_exit_during_build_defers_respawn_until_pipeline_frees() {
        let mut s = state(&["dev"]);
        let g_serve = drive_to_serving(&mut s, "dev", "aaa");
        let a = head(&mut s, "dev", "bbb");
        let g_build = build_gen(&a);

        // Child dies mid-build: nothing to do yet (slot is busy).
        let actions = s.step(
            "dev",
            Event::ServingExited {
                generation: g_serve,
            },
        );
        assert_eq!(actions, vec![]);
        assert!(s.instance("dev").unwrap().needs_respawn);

        // The build goes red → recovery respawn fires instead of idling.
        let actions = s.step(
            "dev",
            Event::BuildFinished {
                generation: g_build,
                outcome: AppBuildOutcome::Red {
                    reason: "boom".into(),
                },
            },
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::Respawn { sha, .. } if sha == "aaa")),
            "red build + dead serving ⇒ respawn last green: {actions:?}"
        );
    }

    #[test]
    fn serving_exit_during_probe_is_restored_by_the_probe_itself() {
        let mut s = state(&["dev"]);
        let g_serve = drive_to_serving(&mut s, "dev", "aaa");
        let a = head(&mut s, "dev", "bbb");
        let g2 = build_gen(&a);
        green(&mut s, "dev", g2);

        // Old child dies while the new one is probing.
        s.step(
            "dev",
            Event::ServingExited {
                generation: g_serve,
            },
        );
        assert!(s.instance("dev").unwrap().needs_respawn);

        // New child passes its probe: promote restores serving; no respawn
        // and no drain (the old child is already gone).
        let actions = s.step("dev", Event::ProbeSucceeded { generation: g2 });
        assert_eq!(
            actions,
            vec![Action::Promote {
                instance: "dev".into(),
                sha: "bbb".into(),
                generation: g2,
            }]
        );
        assert!(!s.instance("dev").unwrap().needs_respawn);
    }

    #[test]
    fn serving_exit_while_queued_respawns_and_preserves_the_queued_build() {
        let mut s = state(&["dev", "feature-x"]);
        // dev reaches green FIRST (nothing else contends for the slot).
        let g_dev = drive_to_serving(&mut s, "dev", "d1");
        // feature-x now grabs the (single) build slot…
        head(&mut s, "feature-x", "f1");
        assert_eq!(s.building(), Some("feature-x"));
        // …so dev's new HEAD can only queue behind it.
        head(&mut s, "dev", "d2");
        assert_eq!(
            s.instance("dev").unwrap().pipeline,
            Pipeline::Queued { sha: "d2".into() }
        );

        // dev's serving child dies while its next build is still queued.
        let actions = s.step("dev", Event::ServingExited { generation: g_dev });
        // Respawn the previous green NOW (don't wait for the slot)…
        assert_eq!(actions.len(), 1, "{actions:?}");
        let respawn_gen = match &actions[0] {
            Action::Respawn {
                sha, generation, ..
            } => {
                assert_eq!(sha, "d1", "respawn boots the last green bundle");
                *generation
            }
            other => panic!("expected Respawn, got {other:?}"),
        };
        // …and the queued d2 survives as pending, no longer occupying the
        // queue slot it can't use while respawning.
        let dev = s.instance("dev").unwrap();
        assert_eq!(dev.pending.as_deref(), Some("d2"));
        assert!(matches!(
            dev.pipeline,
            Pipeline::Probing { respawn: true, .. }
        ));
        assert!(!s.queue_contains("dev"), "dev left the build queue");

        // The respawn promotes; d2 then re-queues behind feature-x.
        let actions = s.step(
            "dev",
            Event::ProbeSucceeded {
                generation: respawn_gen,
            },
        );
        assert!(matches!(&actions[0], Action::Promote { sha, .. } if sha == "d1"));
        assert_eq!(
            s.instance("dev").unwrap().pipeline,
            Pipeline::Queued { sha: "d2".into() },
            "the build dev owed before the crash is preserved"
        );
    }

    #[test]
    fn boot_recovery_respawns_before_any_build() {
        let mut s = state(&["dev"]);
        let actions = s.step(
            "dev",
            Event::RecoverFromPointer {
                sha: "prev-green".into(),
            },
        );
        assert_eq!(actions.len(), 1);
        let g = match &actions[0] {
            Action::Respawn {
                sha, generation, ..
            } => {
                assert_eq!(sha, "prev-green");
                *generation
            }
            other => panic!("expected Respawn, got {other:?}"),
        };
        // A head arriving during recovery becomes pending, not a build.
        assert_eq!(head(&mut s, "dev", "newer"), vec![]);
        // Recovery completes; the newer sha then builds.
        let actions = s.step("dev", Event::ProbeSucceeded { generation: g });
        assert!(matches!(&actions[0], Action::Promote { sha, .. } if sha == "prev-green"));
        assert!(
            matches!(&actions[1], Action::StartBuild { sha, .. } if sha == "newer"),
            "{actions:?}"
        );
    }

    #[test]
    fn unknown_instance_is_ignored() {
        let mut s = state(&["dev"]);
        assert_eq!(head(&mut s, "nope", "aaa"), vec![]);
    }

    #[test]
    fn add_instance_makes_it_buildable_and_rejects_duplicates() {
        let mut s = state(&["dev"]);
        // A head for an unknown instance no-ops until it is added.
        assert_eq!(head(&mut s, "feat", "f1"), vec![]);

        assert!(s.add_instance("feat".into()), "first add succeeds");
        assert!(!s.add_instance("feat".into()), "duplicate add is rejected");
        assert!(s.instance("feat").is_some());

        // Now it builds like any instance.
        let a = head(&mut s, "feat", "f1");
        assert!(matches!(&a[0], Action::StartBuild { instance, sha, .. }
            if instance == "feat" && sha == "f1"));
    }

    #[test]
    fn remove_instance_returns_child_generations_and_scrubs_queue() {
        let mut s = state(&["dev", "feat"]);
        // dev takes the single build slot; feat queues behind it.
        let a = head(&mut s, "dev", "d1");
        let _g_dev = build_gen(&a);
        head(&mut s, "feat", "f1");
        assert_eq!(
            s.instance("feat").unwrap().pipeline,
            Pipeline::Queued { sha: "f1".into() }
        );
        assert!(s.queue_contains("feat"));

        // Remove feat while it is queued — no children yet, empty gen list.
        let r = s.remove_instance("feat").expect("feat existed");
        assert!(r.child_generations.is_empty(), "queued-only: no children");
        assert!(
            r.actions.is_empty(),
            "feat didn't hold the slot ⇒ nothing dispatched"
        );
        assert!(s.instance("feat").is_none(), "instance gone");
        assert!(!s.queue_contains("feat"), "scrubbed from the build queue");

        // dev's build still completes normally (the queue scrub didn't wedge).
        let a = green(&mut s, "dev", _g_dev);
        assert!(a.iter().any(|x| matches!(x, Action::SpawnAndProbe { .. })));
    }

    #[test]
    fn remove_serving_instance_reports_its_child_generation() {
        let mut s = state(&["dev"]);
        let g = drive_to_serving(&mut s, "dev", "aaa");
        // Removing a serving instance must hand back its child generation so
        // the driver can stop the process + free the port.
        let r = s.remove_instance("dev").expect("dev existed");
        assert_eq!(
            r.child_generations,
            vec![g],
            "serving child's generation returned"
        );
        assert!(s.instance("dev").is_none());
    }

    #[test]
    fn remove_instance_holding_the_build_slot_dispatches_the_queued_one() {
        let mut s = state(&["dev", "feat"]);
        // dev holds the build slot; feat queues behind it.
        let a = head(&mut s, "dev", "d1");
        let g = build_gen(&a);
        assert_eq!(s.building(), Some("dev"));
        head(&mut s, "feat", "f1");

        // Remove dev *while it builds* — the freed slot is handed to feat in
        // the same call (no separate nudge needed).
        let r = s.remove_instance("dev").expect("dev existed");
        assert!(
            r.child_generations.is_empty(),
            "dev had no serving child yet"
        );
        assert!(
            r.actions
                .iter()
                .any(|x| matches!(x, Action::StartBuild { instance, sha, .. }
                    if instance == "feat" && sha == "f1")),
            "feat dispatches into the freed slot: {:?}",
            r.actions
        );
        assert_eq!(s.building(), Some("feat"), "feat now holds the slot");

        // A stray BuildFinished for the removed dev is a safe no-op.
        assert_eq!(
            s.step(
                "dev",
                Event::BuildFinished {
                    generation: g,
                    outcome: AppBuildOutcome::Green
                }
            ),
            vec![]
        );
    }

    #[test]
    fn remove_unknown_instance_is_none() {
        let mut s = state(&["dev"]);
        assert!(s.remove_instance("ghost").is_none());
    }

    #[test]
    fn removed_instance_name_can_be_re_added_fresh() {
        // A preview teardown + re-create of the same branch name yields a
        // clean Idle instance (no leaked red/serving from the prior life).
        let mut s = state(&["dev"]);
        drive_to_serving(&mut s, "dev", "aaa");
        s.remove_instance("dev").expect("existed");
        assert!(s.add_instance("dev".into()), "name reusable after removal");
        let inst = s.instance("dev").unwrap();
        assert_eq!(inst.serving, None);
        assert_eq!(inst.last_red, None);
        assert_eq!(inst.pipeline, Pipeline::Idle);
    }
}
