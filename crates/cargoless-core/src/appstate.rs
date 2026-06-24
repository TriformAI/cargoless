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
//! ## Build-queue arbitration (serialized by default, optionally parallel)
//!
//! By default, builds are serialized daemon-wide (one shared
//! `CARGO_TARGET_DIR`; two concurrent 40 GB-target builds would just contend).
//! An optional `max_concurrent` > 1 allows up to N instances to build at once;
//! each such lane must use its own `CARGO_TARGET_DIR` so incremental state and
//! file locks do not interfere. The default (= 1) is byte-identical to the
//! original single-slot behaviour.
//!
//! The queue is FIFO across instances; *within* an instance the newest sha
//! always supersedes a queued older one (a queued build that nobody wants is
//! pure waste), while a *running* build is never cancelled — its sha was HEAD
//! when it started, its green is real, and it promotes before the newer sha
//! builds (latest-green per ref, applied in order). Probing does **not** hold
//! the build slot: the moment a build finishes, the next instance's build
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
//! it requeues once (the newest pending sha if a commit arrived meanwhile —
//! newest-sha-wins for the *retry target*), and a second **consecutive**
//! indeterminate on the same instance records red. Crucially the streak is
//! **not** reset by a waiting newer commit: on a hot branch a pending sha is
//! almost always present, and resetting on it would defeat the backstop and
//! chase HEAD forever. Holding the streak across pending makes the pipeline
//! **converge to a verdict** (green if the next build is attributable, else a
//! bounded red) instead of superseding the in-flight build indefinitely.

use std::collections::{BTreeMap, BTreeSet};

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
    /// A step failed (or harvest failed); `reason` names the step. `enospc`
    /// marks an out-of-disk failure — environmental, not a defect in the sha —
    /// which is handled non-latching (requeue-once) so a transient full disk
    /// the daemon then self-relieves does not pin a good commit red forever.
    Red { reason: String, enospc: bool },
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
    /// Consecutive out-of-disk (ENOSPC) build reds for this instance. An
    /// `enospc` red is environmental, not a defect in the sha, so it is
    /// requeued (the bin layer pressure-prunes before the retry rebuilds)
    /// rather than latched. This streak caps the retries so a genuinely
    /// undersized PVC escalates to a real, surfaced red instead of churning
    /// cold rebuilds forever. Same `pub`/contract caveat as
    /// `indeterminate_streak`.
    pub enospc_streak: u8,
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

/// The daemon-wide state: every instance plus the serialized build queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    instances: BTreeMap<String, InstanceState>,
    /// Instances waiting for the build slot, FIFO. An instance appears here
    /// iff its pipeline is `Queued` (the sha lives there).
    queue: Vec<String>,
    /// Instances currently building. When `max_concurrent == 1` this set has
    /// at most one element — identical to the old `building: Option<String>`
    /// invariant. When > 1, up to `max_concurrent` instances may build at once,
    /// each in its own `CARGO_TARGET_DIR`.
    building: BTreeSet<String>,
    /// How many instances may build concurrently. Default 1 = today's
    /// serialised behaviour. Must be ≥ 1.
    max_concurrent: usize,
    next_generation: Generation,
}

impl AppState {
    /// Seed one `Idle` instance per configured name (instances-file order).
    /// `max_concurrent = 1` is the default (serialised, today's behaviour).
    pub fn new<I: IntoIterator<Item = String>>(names: I) -> Self {
        Self::with_max_concurrent(names, 1)
    }

    /// Like `new` but with a configurable concurrency limit. Values < 1 are
    /// clamped to 1. Values > 1 enable the per-lane `CARGO_TARGET_DIR`
    /// isolation that the build backend must wire up.
    pub fn with_max_concurrent<I: IntoIterator<Item = String>>(
        names: I,
        max_concurrent: usize,
    ) -> Self {
        Self {
            instances: names
                .into_iter()
                .map(|n| (n, InstanceState::default()))
                .collect(),
            queue: Vec::new(),
            building: BTreeSet::new(),
            max_concurrent: max_concurrent.max(1),
            next_generation: 1,
        }
    }

    pub fn instance(&self, name: &str) -> Option<&InstanceState> {
        self.instances.get(name)
    }

    pub fn instances(&self) -> impl Iterator<Item = (&String, &InstanceState)> {
        self.instances.iter()
    }

    /// The set of instances currently building.
    pub fn building(&self) -> &BTreeSet<String> {
        &self.building
    }

    /// True iff `instance` currently holds a build slot.
    pub fn is_building(&self, instance: &str) -> bool {
        self.building.contains(instance)
    }

    /// Add a new instance (initially `Idle`). No-op if the name already exists.
    /// Called from the driver when SIGHUP adds an instance to the live set.
    pub fn add_instance(&mut self, name: String) {
        self.instances.entry(name).or_default();
    }

    /// Remove an instance from all state bookkeeping. Must only be called
    /// after the driver has already stopped any child and the instance is no
    /// longer serving. Removes the instance from the queue if it is still
    /// waiting for the build slot.
    pub fn remove_instance(&mut self, name: &str) {
        self.instances.remove(name);
        self.queue.retain(|n| n != name);
        // The build thread is still running but will post a BuildFinished
        // that the control loop discards (unknown instance → step() no-ops).
        self.building.remove(name);
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
        self.building.remove(instance);
        let inst = self.instances.get_mut(instance).expect("checked");
        match outcome {
            AppBuildOutcome::Green => {
                inst.indeterminate_streak = 0;
                inst.enospc_streak = 0;
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
            AppBuildOutcome::Red {
                reason,
                enospc: true,
            } => {
                // Infra-red: the disk failed transiently, the sha is fine.
                // Mirror the Indeterminate requeue-once discipline — do NOT
                // latch `last_red` — so the bin layer's pressure-prune (run on
                // this same red event, before the requeue rebuilds) can free
                // space and the retry usually fits. Cap the streak so a PVC
                // that is genuinely too small escalates to a real, surfaced red
                // instead of churning cold rebuilds forever.
                const ENOSPC_RETRY_CAP: u8 = 2;
                inst.indeterminate_streak = 0;
                inst.pipeline = Pipeline::Idle;
                if inst.enospc_streak >= ENOSPC_RETRY_CAP {
                    inst.enospc_streak = 0;
                    self.record_red(
                        instance,
                        sha,
                        format!(
                            "disk full, self-relief exhausted (PVC likely too small): {reason}"
                        ),
                        actions,
                    );
                    self.settle_idle(instance, actions);
                } else {
                    inst.enospc_streak += 1;
                    // Prefer the newest pending sha if a commit arrived while we
                    // were building (newest-sha-wins for the retry target);
                    // otherwise rebuild the same sha after relief. The shared
                    // `self.dispatch(actions)` at the end of this match hands
                    // out the build slot (mirrors the Indeterminate arm — no
                    // dispatch here, or the slot would be handed out twice).
                    let retry = inst.pending.take().unwrap_or(sha);
                    self.request_build(instance, retry);
                }
            }
            AppBuildOutcome::Red {
                reason,
                enospc: false,
            } => {
                // Code-red: a real defect in this sha. Latch it (unchanged) so
                // the bad commit is not auto-retried until a new one arrives.
                inst.indeterminate_streak = 0;
                inst.enospc_streak = 0;
                inst.pipeline = Pipeline::Idle;
                self.record_red(instance, sha, reason, actions);
                self.settle_idle(instance, actions);
            }
            AppBuildOutcome::Indeterminate { reason } => {
                let inst = self.instances.get_mut(instance).expect("checked");
                inst.pipeline = Pipeline::Idle;
                // Convergence backstop: count consecutive indeterminates and
                // red out on the second — *regardless* of whether a newer
                // commit is waiting. A hot branch almost always has a pending
                // sha, so resetting the streak on `pending` (the old behavior)
                // defeated the backstop entirely: every indeterminate would
                // requeue and the instance would chase HEAD forever, never
                // settling to a verdict. Holding the streak across pending is
                // what makes a hot branch converge.
                if inst.indeterminate_streak >= 1 {
                    // Second consecutive indeterminate: stop requeuing this
                    // attempt and record red. `settle_idle` below still builds
                    // any newer pending sha, so even the red-out path converges
                    // (it just doesn't keep retrying the untrustworthy build).
                    inst.indeterminate_streak = 0;
                    self.record_red(
                        instance,
                        sha,
                        format!("indeterminate twice in a row: {reason}"),
                        actions,
                    );
                    self.settle_idle(instance, actions);
                } else {
                    // First indeterminate: requeue once. Prefer the newest
                    // pending sha if a commit arrived meanwhile (newest-sha-wins
                    // for the retry target — no point re-attempting a sha we'd
                    // immediately supersede); otherwise rebuild the same sha.
                    inst.indeterminate_streak += 1;
                    let retry = inst.pending.take().unwrap_or(sha);
                    self.request_build(instance, retry);
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

    /// Hand free build slot(s) to the next queued instance(s), up to
    /// `max_concurrent`. With `max_concurrent == 1` this is identical to the
    /// old single-slot dispatch: at most one `StartBuild` per call.
    fn dispatch(&mut self, actions: &mut Vec<Action>) {
        while self.building.len() < self.max_concurrent && !self.queue.is_empty() {
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
            self.building.insert(instance.clone());
            actions.push(Action::StartBuild {
                instance,
                sha,
                generation,
            });
        }
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

    /// Finish a build as an out-of-disk (ENOSPC) red — the non-latching,
    /// requeue-once infra-red path.
    fn enospc_red(s: &mut AppState, inst: &str, generation: Generation) -> Vec<Action> {
        s.step(
            inst,
            Event::BuildFinished {
                generation,
                outcome: AppBuildOutcome::Red {
                    reason: "bundle harvest failed: No space left on device (os error 28)".into(),
                    enospc: true,
                },
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
                    enospc: false,
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
    fn enospc_red_requeues_the_same_sha_without_latching() {
        // An out-of-disk red is environmental: the sha must be REBUILT (so the
        // daemon's pressure-prune-then-retry can succeed), and it must NOT be
        // latched in `last_red` (else even after space frees the sha would
        // never retry until a new commit — the self-starvation wedge).
        let mut s = state(&["dev"]);
        let a = head(&mut s, "dev", "aaa");
        let g = build_gen(&a);

        let actions = enospc_red(&mut s, "dev", g);
        // The same sha is re-queued and immediately re-dispatched (StartBuild),
        // NOT recorded red.
        assert!(
            actions
                .iter()
                .any(|x| matches!(x, Action::StartBuild { sha, .. } if sha == "aaa")),
            "enospc red rebuilds the same sha: {actions:?}"
        );
        assert!(
            !actions
                .iter()
                .any(|x| matches!(x, Action::RecordRed { .. })),
            "enospc red must not RecordRed on the first attempt: {actions:?}"
        );
        let inst = s.instance("dev").unwrap();
        assert!(
            inst.last_red.is_none(),
            "enospc red does not latch last_red"
        );
        assert_eq!(inst.enospc_streak, 1, "one enospc attempt counted");
    }

    #[test]
    fn enospc_red_converges_to_a_real_red_when_relief_is_exhausted() {
        // A genuinely undersized PVC: every retry re-ENOSPCs. After the cap
        // (2 retries) the daemon stops churning cold rebuilds and records a
        // real, surfaced red so the operator sees the disk is too small.
        let mut s = state(&["dev"]);
        let a = head(&mut s, "dev", "aaa");
        let mut g = build_gen(&a);

        // Attempt 1 + 2: requeue (streak 1, then 2), no latch.
        for expect_streak in [1u8, 2u8] {
            let actions = enospc_red(&mut s, "dev", g);
            assert_eq!(
                s.instance("dev").unwrap().enospc_streak,
                expect_streak,
                "streak after attempt"
            );
            assert!(
                s.instance("dev").unwrap().last_red.is_none(),
                "still not latched at streak {expect_streak}"
            );
            g = build_gen(&actions); // the requeued StartBuild's generation
        }

        // Attempt 3: streak was at the cap (2) → record a real red and reset.
        let actions = enospc_red(&mut s, "dev", g);
        let recorded = actions.iter().find_map(|x| match x {
            Action::RecordRed { sha, reason, .. } => Some((sha.clone(), reason.clone())),
            _ => None,
        });
        let (sha, reason) = recorded.expect("a RecordRed once relief is exhausted");
        assert_eq!(sha, "aaa");
        assert!(
            reason.contains("self-relief exhausted"),
            "names the exhaustion: {reason}"
        );
        let inst = s.instance("dev").unwrap();
        assert_eq!(inst.last_red.as_ref().unwrap().0, "aaa", "now latched");
        assert_eq!(inst.enospc_streak, 0, "streak reset after the red-out");
    }

    #[test]
    fn enospc_red_prefers_a_newer_pending_sha_on_retry() {
        // If a newer commit arrived while the disk-starved build ran, the retry
        // targets the NEWEST sha (newest-sha-wins) — no point rebuilding a sha
        // we would immediately supersede.
        let mut s = state(&["dev"]);
        let a = head(&mut s, "dev", "aaa");
        let g = build_gen(&a);
        // A newer commit lands while "aaa" is building → stashed as pending.
        head(&mut s, "dev", "bbb");

        let actions = enospc_red(&mut s, "dev", g);
        assert!(
            actions
                .iter()
                .any(|x| matches!(x, Action::StartBuild { sha, .. } if sha == "bbb")),
            "retry targets the newer pending sha: {actions:?}"
        );
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
        assert!(s.is_building("feature-x"));
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
            "newer pending sha wins as the retry target: {actions:?}"
        );
        // Convergence fix: the streak is HELD across the pending sha (not reset
        // to 0). The old code reset it here, which on a hot branch — where a
        // pending sha is almost always present — defeated the backstop and made
        // the instance chase HEAD forever without ever settling to a verdict.
        assert_eq!(
            s.instance("dev").unwrap().indeterminate_streak,
            1,
            "indeterminate streak survives a waiting newer commit"
        );
    }

    #[test]
    fn indeterminate_converges_to_red_even_with_continuous_pending() {
        // The core convergence guarantee. A hot branch keeps a newer sha
        // pending across every build. Two *consecutive* indeterminates must
        // still red out (settle to a verdict) instead of requeuing forever,
        // and the newest pending sha must still build — the pipeline never
        // stalls.
        let mut s = state(&["dev"]);
        let a = head(&mut s, "dev", "v1");
        let g1 = build_gen(&a);
        head(&mut s, "dev", "v2"); // pending during build 1

        // First indeterminate (pending present): requeues the newest pending
        // (v2), streak held at 1, NOT red yet.
        let a = s.step(
            "dev",
            Event::BuildFinished {
                generation: g1,
                outcome: AppBuildOutcome::Indeterminate {
                    reason: "tree moved".into(),
                },
            },
        );
        let g2 = build_gen(&a);
        assert!(
            a.iter()
                .any(|x| matches!(x, Action::StartBuild { sha, .. } if sha == "v2")),
            "first indeterminate requeues the newest pending sha: {a:?}"
        );
        assert!(
            !a.iter().any(|x| matches!(x, Action::RecordRed { .. })),
            "first indeterminate is not red: {a:?}"
        );
        assert_eq!(s.instance("dev").unwrap().indeterminate_streak, 1);

        head(&mut s, "dev", "v3"); // another commit lands during build 2

        // Second consecutive indeterminate (still pending): MUST red out, and
        // the newest pending (v3) still builds — convergence, not a stall.
        let a = s.step(
            "dev",
            Event::BuildFinished {
                generation: g2,
                outcome: AppBuildOutcome::Indeterminate {
                    reason: "tree moved".into(),
                },
            },
        );
        assert!(
            a.iter()
                .any(|x| matches!(x, Action::RecordRed { reason, .. }
                if reason.contains("indeterminate twice"))),
            "two consecutive indeterminates red out even with pending: {a:?}"
        );
        assert!(
            a.iter()
                .any(|x| matches!(x, Action::StartBuild { sha, .. } if sha == "v3")),
            "the newest pending sha still builds — the pipeline converges: {a:?}"
        );
        assert_eq!(s.instance("dev").unwrap().indeterminate_streak, 0);
    }

    #[test]
    fn green_promotes_even_with_a_newer_sha_pending() {
        // (a) A build that has STARTED runs to completion and promotes its sha
        // even though newer commits arrived meanwhile. Serving lags HEAD —
        // that is fine; the invariant is that it converges, not that it is
        // always the tip.
        let mut s = state(&["dev"]);
        let a = head(&mut s, "dev", "v1");
        let g1 = build_gen(&a);
        head(&mut s, "dev", "v2"); // newer commits land mid-build…
        head(&mut s, "dev", "v3"); // …newest wins as pending
        assert_eq!(s.instance("dev").unwrap().pending.as_deref(), Some("v3"));

        // v1 finishes green: spawn+probe v1 regardless of the pending sha.
        let a = green(&mut s, "dev", g1);
        assert!(
            a.iter()
                .any(|x| matches!(x, Action::SpawnAndProbe { sha, .. } if sha == "v1")),
            "the started build is carried to spawn+probe despite pending: {a:?}"
        );
        // The probe promotes v1 — serving reaches green.
        let a = s.step("dev", Event::ProbeSucceeded { generation: g1 });
        assert!(
            a.iter()
                .any(|x| matches!(x, Action::Promote { sha, .. } if sha == "v1")),
            "started build promotes even with a newer sha pending: {a:?}"
        );
        assert_eq!(
            s.instance("dev").unwrap().serving.as_ref().unwrap().sha,
            "v1"
        );
    }

    #[test]
    fn pending_sha_builds_only_after_the_current_build_settles() {
        // (b) The pending sha does not build until the in-flight build has
        // promoted (green) OR failed (red) — it never preempts or aborts the
        // in-flight build. Covers both the green-then-build and red-then-build
        // settlements.
        let mut s = state(&["dev"]);
        // Keep something serving so the red branch has a serving child to
        // leave untouched.
        drive_to_serving(&mut s, "dev", "g0");

        // ── green settlement ──
        let a = head(&mut s, "dev", "v1");
        let g1 = build_gen(&a);
        head(&mut s, "dev", "v2"); // pending while v1 builds
        let a = green(&mut s, "dev", g1);
        assert!(
            !a.iter().any(|x| matches!(x, Action::StartBuild { .. })),
            "v2 must NOT build while v1 is still probing: {a:?}"
        );
        let a = s.step("dev", Event::ProbeSucceeded { generation: g1 });
        assert!(
            a.iter()
                .any(|x| matches!(x, Action::StartBuild { sha, .. } if sha == "v2")),
            "v2 builds only once v1 has promoted: {a:?}"
        );
        let g2 = build_gen(&a);

        // ── red settlement ──
        head(&mut s, "dev", "v3"); // pending while v2 builds
        let a = s.step(
            "dev",
            Event::BuildFinished {
                generation: g2,
                outcome: AppBuildOutcome::Red {
                    reason: "boom".into(),
                    enospc: false,
                },
            },
        );
        assert!(
            a.iter()
                .any(|x| matches!(x, Action::StartBuild { sha, .. } if sha == "v3")),
            "v3 builds after the in-flight build fails red, too: {a:?}"
        );
        // A red never disturbs the serving child.
        assert_eq!(
            s.instance("dev").unwrap().serving.as_ref().unwrap().sha,
            "v1",
            "serving (v1) untouched by v2's red"
        );
    }

    #[test]
    fn converges_under_a_rapid_head_advance_stream() {
        // (c) Five HeadAdvanced during one in-flight build: none start a second
        // build or abort the first; only the newest survives as pending. The
        // first build promotes, then the latest pending builds, and serving
        // eventually reaches the latest sha — full convergence on a hot branch.
        let mut s = state(&["dev"]);
        let a = head(&mut s, "dev", "s1");
        let g1 = build_gen(&a);

        for sha in ["s2", "s3", "s4", "s5", "s6"] {
            let a = head(&mut s, "dev", sha);
            assert!(
                !a.iter().any(|x| matches!(x, Action::StartBuild { .. })),
                "a head arriving mid-build never starts a second build: {a:?}"
            );
        }
        assert_eq!(
            s.instance("dev").unwrap().pending.as_deref(),
            Some("s6"),
            "only the newest of the burst survives as pending (s2..s5 dropped)"
        );

        // s1's build completes and promotes — serving reaches green (lagging).
        green(&mut s, "dev", g1);
        let a = s.step("dev", Event::ProbeSucceeded { generation: g1 });
        assert!(
            a.iter()
                .any(|x| matches!(x, Action::Promote { sha, .. } if sha == "s1")),
            "first build promotes: {a:?}"
        );
        assert_eq!(
            s.instance("dev").unwrap().serving.as_ref().unwrap().sha,
            "s1"
        );
        // Promoting frees the pipeline and the latest pending (s6) builds —
        // s2..s5 were superseded and are never built.
        let g6 = build_gen(&a);

        // s6 completes and promotes — serving converges to the latest sha.
        green(&mut s, "dev", g6);
        let a = s.step("dev", Event::ProbeSucceeded { generation: g6 });
        assert!(
            a.iter()
                .any(|x| matches!(x, Action::Promote { sha, .. } if sha == "s6")),
            "the latest pending sha promotes: {a:?}"
        );
        let dev = s.instance("dev").unwrap();
        assert_eq!(dev.serving.as_ref().unwrap().sha, "s6");
        assert_eq!(
            dev.pending, None,
            "fully converged: nothing left pending, serving == latest"
        );
        assert_eq!(dev.pipeline, Pipeline::Idle);
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
                    enospc: false,
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
        assert!(s.is_building("feature-x"));
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

    // ── CGLS-16: runtime add/remove ──────────────────────────────────────

    /// Adding a new instance puts it in an Idle state and it can immediately
    /// participate in the build pipeline.
    #[test]
    fn add_instance_starts_idle_and_accepts_events() {
        let mut s = state(&["dev"]);
        s.add_instance("feature-x".to_string());
        // The new instance must exist in an Idle state.
        let inst = s.instance("feature-x").expect("feature-x was added");
        assert_eq!(inst.pipeline, Pipeline::Idle);
        assert!(inst.serving.is_none());
        // It must respond to events normally.
        let actions = head(&mut s, "feature-x", "f1");
        assert_eq!(
            actions.len(),
            1,
            "added instance starts building: {actions:?}"
        );
        assert!(
            matches!(&actions[0], Action::StartBuild { instance, sha, .. }
                if instance == "feature-x" && sha == "f1"),
            "{actions:?}"
        );
    }

    /// Adding an already-present instance name is a no-op (idempotent).
    #[test]
    fn add_instance_is_idempotent() {
        let mut s = state(&["dev"]);
        drive_to_serving(&mut s, "dev", "aaa");
        // Re-adding a live instance must not clobber its state.
        s.add_instance("dev".to_string());
        assert!(
            s.instance("dev").unwrap().serving.is_some(),
            "re-add must not reset a serving instance"
        );
    }

    /// Removing an instance removes it from all bookkeeping: the map, the
    /// queue if it was waiting for the build slot, and the `building` set
    /// if it held the slot.
    #[test]
    fn remove_instance_clears_all_bookkeeping() {
        let mut s = state(&["dev", "feature-x"]);
        // Put dev in the build slot and feature-x in the queue.
        head(&mut s, "dev", "d1"); // dev is building
        head(&mut s, "feature-x", "f1"); // feature-x is queued
        assert!(s.is_building("dev"));
        assert!(
            s.queue_contains("feature-x"),
            "feature-x should be queued before removal"
        );

        // Remove a queued instance: gone from queue, gone from map.
        s.remove_instance("feature-x");
        assert!(
            s.instance("feature-x").is_none(),
            "removed instance must not appear in the map"
        );
        assert!(
            !s.queue_contains("feature-x"),
            "removed instance must not remain in the queue"
        );

        // Remove the building instance: building set freed.
        s.remove_instance("dev");
        assert!(s.instance("dev").is_none());
        assert!(
            s.building().is_empty(),
            "building slot freed when builder removed"
        );
    }

    /// After removing an instance, events for it are silently ignored —
    /// the stale `BuildFinished` a running detached thread might post
    /// must not panic.
    #[test]
    fn events_for_removed_instance_are_noop() {
        let mut s = state(&["dev"]);
        let a = head(&mut s, "dev", "aaa");
        let g = build_gen(&a);
        s.remove_instance("dev");
        // A stale `BuildFinished` from the detached build thread should no-op.
        assert_eq!(
            s.step(
                "dev",
                Event::BuildFinished {
                    generation: g,
                    outcome: AppBuildOutcome::Green,
                }
            ),
            vec![],
            "stale event for removed instance is silently ignored"
        );
    }

    // ── CGLS-15: per-lane build slot ─────────────────────────────────────────

    fn state_parallel(names: &[&str], max_concurrent: usize) -> AppState {
        AppState::with_max_concurrent(names.iter().map(|s| s.to_string()), max_concurrent)
    }

    /// With `max_concurrent=2`, two instances can be Building simultaneously.
    #[test]
    fn max_concurrent_2_allows_two_simultaneous_builds() {
        let mut s = state_parallel(&["dev", "feature-x"], 2);

        // dev queues + dispatches immediately (slot 1).
        let a = head(&mut s, "dev", "d1");
        assert_eq!(a.len(), 1);
        assert!(matches!(&a[0], Action::StartBuild { instance, .. } if instance == "dev"));
        assert!(s.is_building("dev"));

        // feature-x also dispatches immediately — the second slot is free.
        let a = head(&mut s, "feature-x", "f1");
        assert_eq!(a.len(), 1);
        assert!(
            matches!(&a[0], Action::StartBuild { instance, sha, .. }
                if instance == "feature-x" && sha == "f1"),
            "second slot dispatches right away: {a:?}"
        );
        assert!(s.is_building("feature-x"), "feature-x now also building");
        assert!(s.is_building("dev"), "dev is still building too");
        assert_eq!(s.building().len(), 2, "both slots occupied");
    }

    /// With `max_concurrent=1`, the second instance must wait (today's behaviour).
    #[test]
    fn max_concurrent_1_serialises_builds() {
        // This is the default — this test asserts byte-identical behaviour to
        // the original single-slot invariant.
        let mut s = state(&["dev", "feature-x"]); // default max_concurrent=1

        let a = head(&mut s, "dev", "d1");
        let g_dev = build_gen(&a);
        assert!(s.is_building("dev"));

        // feature-x must wait: the single slot is taken.
        let a = head(&mut s, "feature-x", "f1");
        assert_eq!(a, vec![], "no StartBuild while dev holds the slot");
        assert_eq!(
            s.instance("feature-x").unwrap().pipeline,
            Pipeline::Queued { sha: "f1".into() }
        );
        assert!(!s.is_building("feature-x"));

        // dev finishes: releases the slot, feature-x is dispatched.
        let a = green(&mut s, "dev", g_dev);
        assert!(
            a.iter().any(
                |x| matches!(x, Action::StartBuild { instance, .. } if instance == "feature-x")
            ),
            "slot released → feature-x dispatches: {a:?}"
        );
    }

    /// Default (flag unset / max_concurrent=1) — arbitration and target-dir
    /// resolution are identical to the pre-CGLS-15 baseline.
    #[test]
    fn default_off_guard_single_slot_invariant() {
        // `AppState::new` must produce exactly the same observable sequencing
        // as the old single-`building: Option<String>` implementation.
        let mut s = AppState::new(["alpha", "beta"].iter().map(|n| n.to_string()));

        // alpha takes the single slot.
        let a = s.step("alpha", Event::HeadAdvanced { sha: "a1".into() });
        let g = build_gen(&a);
        assert!(s.is_building("alpha"));
        assert_eq!(s.building().len(), 1, "exactly one builder");

        // beta must queue, not build.
        let a2 = s.step("beta", Event::HeadAdvanced { sha: "b1".into() });
        assert_eq!(a2, vec![], "beta queued, not dispatched");
        assert_eq!(s.building().len(), 1, "still exactly one builder");

        // alpha finishes green: beta is dispatched.
        let a3 = s.step(
            "alpha",
            Event::BuildFinished {
                generation: g,
                outcome: AppBuildOutcome::Green,
            },
        );
        assert!(
            a3.iter()
                .any(|x| matches!(x, Action::StartBuild { instance, .. } if instance == "beta")),
            "beta dispatches after alpha frees the slot: {a3:?}"
        );
        assert!(!s.is_building("alpha"), "alpha freed the slot");
        assert!(s.is_building("beta"), "beta now holds it");
    }
}
