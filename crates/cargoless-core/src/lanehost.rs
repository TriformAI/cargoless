//! Hosts a running [`LaneState`](crate::lane::LaneState) behind a thread.
//!
//! [`crate::lane`] is a pure state machine and [`crate::lanedrv`] is a
//! synchronous driver: `pump` runs the whole build *inside itself*, which for
//! a real lane is tens of minutes. That is the right shape for correctness —
//! one action at a time, no concurrency to reason about — and exactly the
//! wrong shape to call from an HTTP handler.
//!
//! So this owns a worker thread. Requests enqueue an event and return
//! immediately; the worker pumps them one at a time in arrival order. The
//! serialization the driver relies on is preserved, because there is only ever
//! one pump in flight.
//!
//! ## Why a snapshot rather than a shared lock on the lane
//!
//! `GET /lane` must answer while a build is running. If readers took the same
//! lock the worker holds for the duration of a build, the endpoint would block
//! for the whole build — and the endpoint exists precisely so someone whose
//! change stopped moving can find out why. The worker therefore publishes an
//! immutable snapshot, and readers only ever touch that.
//!
//! Publishing happens on **every state transition** and around **every blocking
//! action**, not just when the pump returns. The transition that flips the
//! phase to `Building` is immediately followed by the blocking build, so
//! publishing only afterwards would report `idle` for the entire duration of
//! every build — the exact window the endpoint exists to explain. The same is
//! true of the LAND, where the lane is genuinely `Idle` (the verdict is in, the
//! roster is empty) while the trunk is being moved.
//!
//! ## The channel is part of the state
//!
//! [`LaneHost::enqueue`] returns as soon as the event is in the mpsc channel,
//! and nothing reads that channel until the worker returns from `pump` — which
//! for a build is tens of minutes. A snapshot built only from `LaneState`
//! therefore reports `queue_depth: 0` with members waiting, and an author who
//! sees "queued" then polls `GET /lane` and sees an empty queue reasonably
//! concludes the lane never got their submission and re-submits.
//!
//! So the host keeps its own count of what it has ACCEPTED and not yet seen the
//! lane step, and reports the union. See [`LaneHost::accepted`].

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::lane::{EjectReason, LaneAction, LaneEvent, LaneMember, LanePhase, LaneState};
use crate::lanedrv::{CandidateTree, LaneActivity, LaneDriver, LaneLander, LegRunner};

/// What `GET /lane` reports. Cheap to clone; never holds a lock.
#[derive(Debug, Clone, Default)]
pub struct LaneSnapshot {
    pub phase: &'static str,
    /// Members waiting: the lane's own queue PLUS anything accepted over HTTP
    /// that the worker has not stepped yet. The second half is not a detail —
    /// during a build the worker is blocked and the lane's queue cannot grow,
    /// so without it every member submitted during a build is invisible for the
    /// whole build.
    pub queue_depth: usize,
    /// Who those members are, lane-queue first then newly accepted, each in
    /// arrival order.
    pub queued: Vec<String>,
    pub generation: u64,
    /// The lane's clock (unix seconds), the same scale as every
    /// `expires_at_tick`.
    ///
    /// Published because a deadline is not readable without the clock it is
    /// measured against: `expires_at_tick` alone cannot answer "how long until
    /// this ejection lapses", which is the question an author whose change
    /// stopped moving actually has.
    pub now: u64,
    /// Member ids in the running build, empty when idle.
    pub in_flight: Vec<String>,
    /// What the driver is blocked on, when that is not visible from `phase`.
    ///
    /// `landing` is the case this exists for: the lane is legitimately `Idle`
    /// there — the build finished, the verdict was green, the roster is empty —
    /// while the lander moves the trunk for up to two hours. A reader told
    /// `idle` at that moment is being invited to roll the daemon during the one
    /// operation that must not be interrupted.
    pub activity: &'static str,
    /// The members being landed, when `activity == "landing"`. Empty otherwise.
    pub landing: Vec<String>,
    /// Complete reconciliation identities for every member the host owns.
    /// The legacy id-only fields above remain for operator readability; this
    /// list is the machine contract that lets a forge adapter prove that an
    /// id still names the same immutable head before it withdraws or lands it.
    pub members: Vec<MemberView>,
    /// One entry per live ejection: (id, kind, human-readable reason, files).
    pub ejections: Vec<EjectionView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    pub id: String,
    pub head: String,
    /// `queued`, `building`, `landing`, or `ejected`.
    pub state: &'static str,
}

#[derive(Debug, Clone)]
pub struct EjectionView {
    pub id: String,
    pub head: String,
    /// `build_failure`, `merge_conflict`, `already_landed`, or
    /// `infrastructure` — what happened, independent of attribution kind.
    pub cause: &'static str,
    /// `attributed`, `unattributed`, or `infrastructure` — each is cleared by
    /// a different thing, so an author needs to know which they have.
    pub kind: &'static str,
    /// The files that carried the failure. Empty for an unattributed ejection,
    /// which is the point: we could not identify them.
    pub files: Vec<String>,
    /// Other members implicated in the same failure.
    ///
    /// The membership rule DIFFERS by `kind`, which is why the sentence in
    /// `why` is the thing to read: for `attributed` these are the other
    /// co-owners of the failing files, and for `unattributed` /
    /// `infrastructure` they are the other held members. The current member
    /// is never repeated in its own `shared_with` list.
    pub shared_with: Vec<String>,
    /// Unix seconds at which the ejection lapses regardless. Compare against
    /// [`LaneSnapshot::now`] — a bare deadline with no clock beside it cannot
    /// be turned into "how long until this clears".
    pub expires_at_tick: u64,
    /// The author-facing sentence, from [`Ejection::describe`].
    ///
    /// The fields above are the machine contract; this is the one a human — or
    /// an agent that has never seen this system — can act on without a lookup
    /// table. It exists because none of the above can be read alone: `files:
    /// []` means "could not identify" for `unattributed` and "nothing was
    /// compiled" for `infrastructure`, and the re-admission rule differs per
    /// kind. Every consumer that had to re-derive this sentence drifted from
    /// the daemon that owns it.
    pub why: String,
}

impl LaneSnapshot {
    /// Render the LANE's own view. The host folds in what it has accepted but
    /// the lane has not stepped yet — see [`LaneSnapshot::with_accepted`].
    fn of(lane: &LaneState, activity: &LaneActivity) -> Self {
        let queued: Vec<String> = lane.queued().iter().map(|m| m.id.clone()).collect();
        let in_flight: Vec<String> = lane.in_flight().iter().map(|m| m.id.clone()).collect();
        let landing: Vec<String> = match activity {
            LaneActivity::Landing { members } => members.iter().map(|m| m.id.clone()).collect(),
            _ => Vec::new(),
        };
        let ejections: Vec<EjectionView> = lane
            .ejections()
            .map(|(id, e)| {
                let (kind, files, shared_with) = match &e.reason {
                    EjectReason::Attributed {
                        files, shared_with, ..
                    } => (
                        "attributed",
                        files
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect(),
                        shared_with.clone(),
                    ),
                    EjectReason::Unattributed { shared_with, .. } => {
                        ("unattributed", Vec::new(), shared_with.clone())
                    }
                    EjectReason::Infrastructure { shared_with, .. } => {
                        ("infrastructure", Vec::new(), shared_with.clone())
                    }
                };
                EjectionView {
                    id: id.clone(),
                    head: e.head.clone(),
                    cause: e.cause.as_str(),
                    kind,
                    files,
                    shared_with,
                    expires_at_tick: e.expires_at_tick,
                    // `describe()` already folds `cause` and `reason` together,
                    // so this is deliberately NOT a second match on the reason:
                    // a parallel match here is exactly how the wire text would
                    // drift from the text the lane reports internally.
                    why: e.describe(),
                }
            })
            .collect();
        let mut members = Vec::new();
        members.extend(lane.queued().iter().map(|m| MemberView {
            id: m.id.clone(),
            head: m.head.clone(),
            state: "queued",
        }));
        members.extend(lane.in_flight().iter().map(|m| MemberView {
            id: m.id.clone(),
            head: m.head.clone(),
            state: "building",
        }));
        if let LaneActivity::Landing {
            members: landing_members,
        } = activity
        {
            members.extend(landing_members.iter().map(|m| MemberView {
                id: m.id.clone(),
                head: m.head.clone(),
                state: "landing",
            }));
        }
        members.extend(ejections.iter().map(|e| MemberView {
            id: e.id.clone(),
            head: e.head.clone(),
            state: "ejected",
        }));
        Self {
            phase: match lane.phase() {
                LanePhase::Idle => "idle",
                LanePhase::Building => "building",
            },
            activity: match activity {
                LaneActivity::Settled => "settled",
                LaneActivity::Building => "building",
                LaneActivity::Landing { .. } => "landing",
            },
            landing,
            members,
            queue_depth: queued.len(),
            queued,
            generation: lane.generation(),
            now: lane.now(),
            in_flight,
            ejections,
        }
    }

    /// Fold in `accepted` — ids the host has taken over HTTP that the lane has
    /// not stepped yet.
    ///
    /// Applied at READ time, not when the snapshot is published, and that is
    /// load-bearing. A published snapshot is a frozen copy; merging into it
    /// would leave a member the operator has since withdrawn stranded in it
    /// until the worker happened to republish — which during a build is the
    /// whole build. Merging on every read means the accepted set is the single
    /// source of truth and every removal takes effect immediately.
    ///
    /// Ids the lane already knows about are SKIPPED rather than added: a member
    /// that has been stepped is already in `queued`/`in_flight`/`ejections`,
    /// and counting it twice would over-report the depth — the same dishonesty
    /// in the other direction.
    fn with_accepted(mut self, accepted: &[LaneMember]) -> Self {
        for member in accepted {
            if !self.members.iter().any(|m| m.id == member.id) {
                self.queued.push(member.id.clone());
                let insert_at = self
                    .members
                    .iter()
                    .position(|m| m.state != "queued")
                    .unwrap_or(self.members.len());
                self.members.insert(
                    insert_at,
                    MemberView {
                        id: member.id.clone(),
                        head: member.head.clone(),
                        state: "queued",
                    },
                );
            }
        }
        self.queue_depth = self.queued.len();
        self
    }

    /// Hide members whose withdrawal the host has accepted but the lane worker
    /// has not stepped yet.
    ///
    /// A build blocks that worker for tens of minutes. During that interval a
    /// queued member lives in the last published `LaneState` snapshot, so
    /// removing it only from `accepted` is insufficient: `POST
    /// /lane/withdraw` answers 202 while the next `GET /lane` still reports the
    /// member. Reconciliation then cannot replace a superseded head. The
    /// tombstone is folded into reads until the worker applies the real event;
    /// a newer enqueue of the same id is added afterwards by `with_accepted`,
    /// so its exact head remains visible.
    fn without_pending_withdrawals(mut self, ids: &HashSet<String>) -> Self {
        self.queued.retain(|id| !ids.contains(id));
        self.in_flight.retain(|id| !ids.contains(id));
        self.landing.retain(|id| !ids.contains(id));
        self.members.retain(|member| !ids.contains(&member.id));
        self.ejections
            .retain(|ejection| !ids.contains(&ejection.id));
        self.queue_depth = self.queued.len();
        self
    }
}

/// A lane running on its own thread.
///
/// The `Sender` is behind a `Mutex` because `mpsc::Sender` is `Send` but not
/// `Sync`, and this is shared across connection threads via
/// `VerdictService: Send + Sync`. The lock is held only for the `send` — a
/// pointer hand-off, never a build — so it cannot become the contention point
/// the snapshot design exists to avoid.
pub struct LaneHost {
    tx: Mutex<Sender<LaneEvent>>,
    snapshot: Arc<Mutex<LaneSnapshot>>,
    running: Arc<AtomicBool>,
    /// Ids accepted over HTTP that the worker has not stepped yet.
    ///
    /// **This is real lane state, not a cache.** `enqueue` returns the instant
    /// the event is in the channel, and nothing reads that channel until the
    /// worker returns from `pump` — tens of minutes for a real build. So for
    /// the whole of every build, a member submitted through the front door
    /// exists ONLY here: the lane has never seen it, and a snapshot built from
    /// `LaneState` alone reports `queue_depth: 0` with members waiting.
    ///
    /// Observed in production: `POST /lane` returned
    /// `{"detail":"queued `pr-6956`","ok":true}` and `queue_depth` stayed 0
    /// across 36s of polling. An author reads that as "the lane never got it"
    /// and re-submits — which is the correct inference from what we told them,
    /// and the reason this cannot be left as a "the snapshot is one pump
    /// behind" caveat.
    ///
    /// Shared with the worker, which removes an id once the lane has stepped
    /// its `Enqueue` and can account for it itself.
    accepted: Arc<Mutex<Vec<LaneMember>>>,
    /// Ids whose `Withdraw` event is queued behind a blocking lane action.
    ///
    /// This is a read-side tombstone, not a second lane state machine. It keeps
    /// the accepted 202 response and the immediately following `GET /lane`
    /// consistent until the worker applies the authoritative event.
    pending_withdrawals: Arc<Mutex<HashSet<String>>>,
    /// The most recent tick the host was given, so the driver can re-sync the
    /// lane's clock across a blocking action. See [`LaneDriver::clock`].
    ///
    /// Fed from the host's OWN tick stream rather than a wall clock, so the
    /// driver never assumes what unit the caller counts in — a test that ticks
    /// 1, 2, 3 gets a clock that reads 1, 2, 3.
    clock: Arc<Mutex<u64>>,
}

impl LaneHost {
    /// Start the worker. The lane runs until the host is dropped.
    pub fn spawn<T, R, L>(lane: LaneState, driver: LaneDriver<T, R, L>) -> Self
    where
        T: CandidateTree + Send + 'static,
        R: LegRunner + Send + 'static,
        L: LaneLander + Send + 'static,
    {
        Self::spawn_with_intergeneration_yield(lane, driver, Duration::ZERO)
    }

    /// Start the worker and expose a bounded settled window after each build.
    ///
    /// The lane normally consumes the next queued event immediately after a
    /// blocking build/land returns. With a non-empty queue that leaves only a
    /// few milliseconds of observable `idle` state, so another trunk writer
    /// that correctly refuses to invalidate an in-flight candidate can starve
    /// forever. During this delay the snapshot is deliberately `idle` /
    /// `settled` while retaining the queued roster; no candidate or lander is
    /// running, so an external compare-and-swap writer may safely advance the
    /// base. The next generation then materializes against that new base.
    ///
    /// Zero preserves the historical behavior for embedded/test callers.
    pub fn spawn_with_intergeneration_yield<T, R, L>(
        mut lane: LaneState,
        driver: LaneDriver<T, R, L>,
        intergeneration_yield: Duration,
    ) -> Self
    where
        T: CandidateTree + Send + 'static,
        R: LegRunner + Send + 'static,
        L: LaneLander + Send + 'static,
    {
        let (tx, rx): (Sender<LaneEvent>, Receiver<LaneEvent>) = channel();
        let accepted: Arc<Mutex<Vec<LaneMember>>> = Arc::new(Mutex::new(Vec::new()));
        let pending_withdrawals: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let snapshot = Arc::new(Mutex::new(LaneSnapshot::of(&lane, &LaneActivity::Settled)));
        let running = Arc::new(AtomicBool::new(true));
        // Seeded from the lane so a host over an already-advanced lane cannot
        // rewind it on the first blocking action.
        let clock = Arc::new(Mutex::new(lane.now()));

        // Give the driver the host's tick stream as its clock. Without this the
        // lane's `now` is frozen for the whole of every blocking action, so an
        // infra failure that takes longer than `infra_backoff_ticks` installs a
        // backoff that has ALREADY expired — the observed ~30s generation loop
        // against an unreachable preview daemon (51 wasted generations in one
        // night, each writing an `outcome=infra` trail line).
        let driver_clock = clock.clone();
        let driver = driver.with_clock(move || match driver_clock.lock() {
            Ok(c) => *c,
            Err(poisoned) => *poisoned.into_inner(),
        });

        let worker_snapshot = snapshot.clone();
        let worker_running = running.clone();
        let worker_accepted = accepted.clone();
        let worker_pending_withdrawals = pending_withdrawals.clone();
        thread::Builder::new()
            .name("cargoless-lane".to_string())
            .spawn(move || {
                // `recv` ends when every Sender is dropped, i.e. when the host
                // goes away. No shutdown flag to get wrong.
                while let Ok(event) = rx.recv() {
                    let withdrawn_id = match &event {
                        LaneEvent::Withdraw { id } => Some(id.clone()),
                        _ => None,
                    };
                    // The lane is about to account for this member itself, so
                    // stop counting it here. Done BEFORE the pump: the lane's
                    // `Enqueue` step is the first thing that runs, and the
                    // snapshot published from inside the pump must already see
                    // it in `queue` rather than in both places.
                    if let LaneEvent::Enqueue(m) = &event {
                        let id = m.id.clone();
                        match worker_accepted.lock() {
                            Ok(mut a) => a.retain(|x| x.id != id),
                            Err(poisoned) => poisoned.into_inner().retain(|x| x.id != id),
                        }
                    }
                    // Publish on EVERY transition and around every blocking
                    // action, not just when the pump returns.
                    //
                    // `pump` runs the whole build inside itself — tens of
                    // minutes for a real lane — and the transition that flips
                    // the phase to Building happens just before that blocking
                    // call. Publishing only afterwards meant `GET /lane`
                    // reported `idle` for the entire duration of every build:
                    // precisely the window the endpoint exists to explain. An
                    // author whose change stopped moving would look, see
                    // "idle", and reasonably conclude the lane never received
                    // their submission.
                    //
                    // The LAND needed the same treatment and never got it. There
                    // the lane is genuinely `Idle` — green verdict in, roster
                    // emptied — while the lander moves the trunk for up to two
                    // hours, so no amount of transition-watching would have
                    // helped: the phase is honest and still misleading. The
                    // driver reports the activity instead.
                    //
                    // Poisoning is ignored deliberately: a panicked reader must
                    // not silently stop the lane from reporting, and the
                    // snapshot is rebuilt from scratch each time so there is no
                    // corrupt state to inherit.
                    // The accepted set is folded in at READ time, not here — see
                    // `LaneSnapshot::with_accepted`.
                    let actions = driver.pump_observed(&mut lane, event, |live, activity| {
                        let next = LaneSnapshot::of(live, activity);
                        match worker_snapshot.lock() {
                            Ok(mut s) => *s = next,
                            Err(poisoned) => *poisoned.into_inner() = next,
                        }
                    });
                    // And once more after, so the terminal state of the last
                    // transition is visible even if the loop exits here.
                    let next = LaneSnapshot::of(&lane, &LaneActivity::Settled);
                    match worker_snapshot.lock() {
                        Ok(mut s) => *s = next,
                        Err(poisoned) => *poisoned.into_inner() = next,
                    }
                    // The authoritative lane state now reflects this event, so
                    // the read-side tombstone is no longer needed. Clear it
                    // only after publishing the terminal snapshot; otherwise a
                    // reader could briefly see the stale member reappear.
                    if let Some(id) = withdrawn_id {
                        match worker_pending_withdrawals.lock() {
                            Ok(mut pending) => {
                                pending.remove(&id);
                            }
                            Err(poisoned) => {
                                poisoned.into_inner().remove(&id);
                            }
                        }
                    }
                    if !intergeneration_yield.is_zero()
                        && actions.iter().any(|action| {
                            matches!(
                                action,
                                LaneAction::StartBuild { .. } | LaneAction::LandAndPublish { .. }
                            )
                        })
                    {
                        eprintln!(
                            "[cargoless:obs] lane-writer-yield generation={} duration_ms={}",
                            lane.generation(),
                            intergeneration_yield.as_millis()
                        );
                        thread::sleep(intergeneration_yield);
                    }
                }
                worker_running.store(false, Ordering::SeqCst);
            })
            .expect("spawn lane worker");

        Self {
            tx: Mutex::new(tx),
            snapshot,
            running,
            accepted,
            pending_withdrawals,
            clock,
        }
    }

    /// Submit a member. Returns as soon as the event is queued — the build it
    /// may trigger runs on the worker.
    ///
    /// The member is counted as waiting IMMEDIATELY, before the worker has seen
    /// it. Telling a caller "queued" and then reporting `queue_depth: 0` for
    /// the next 45 minutes is the same lie told twice, and the second telling
    /// is the one the author acts on.
    pub fn enqueue(&self, member: LaneMember) -> Result<String, String> {
        let id = member.id.clone();
        // Recorded BEFORE the send, and removed again if the send fails, so
        // there is no window in which the worker has stepped the event while
        // the host still has not counted it. The reverse order could drop a
        // member from the snapshot for exactly as long as the worker is fast.
        self.remember_accepted(&member);
        if let Err(e) = self.send(LaneEvent::Enqueue(member)) {
            self.forget_accepted(&id);
            return Err(e);
        }
        Ok(format!("queued `{id}`"))
    }

    fn remember_accepted(&self, member: &LaneMember) {
        let mut a = match self.accepted.lock() {
            Ok(a) => a,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(existing) = a.iter_mut().find(|x| x.id == member.id) {
            *existing = member.clone();
        } else {
            a.push(member.clone());
        }
    }

    fn forget_accepted(&self, id: &str) {
        match self.accepted.lock() {
            Ok(mut a) => a.retain(|x| x.id != id),
            Err(poisoned) => poisoned.into_inner().retain(|x| x.id != id),
        }
    }

    /// Take a member out of the lane permanently.
    ///
    /// Unlike [`Self::readmit`] this does NOT pre-check the snapshot. The
    /// snapshot is one pump behind, and worse, a member enqueued during a build
    /// sits in the channel where the snapshot cannot see it at all — refusing
    /// on "not found" would make the verb useless in exactly the situation that
    /// motivates it (a long build you want to stop feeding). The lane itself
    /// answers authoritatively; an unknown id is a harmless no-op there.
    pub fn withdraw(&self, id: &str) -> Result<String, String> {
        let mut pending = match self.pending_withdrawals.lock() {
            Ok(pending) => pending,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !pending.insert(id.to_string()) {
            // A PR can advance more than once while a long build keeps the
            // first Withdraw event in the worker channel. In that interval a
            // replacement Enqueue is recorded in `accepted`. A second
            // withdrawal is still an idempotent event, but it is not an
            // idempotent read-side operation: the newly superseded accepted
            // head must be forgotten as well. Otherwise `snapshot()` folds it
            // back in after applying the pending-withdrawal tombstone and the
            // lane reports the stale head until the build ends.
            drop(pending);
            self.forget_accepted(id);
            return Ok(format!("withdrawal already pending for `{id}`"));
        }
        if let Err(error) = self.send(LaneEvent::Withdraw { id: id.to_string() }) {
            pending.remove(id);
            return Err(error);
        }
        drop(pending);
        // Also drop it from the accepted set. A member withdrawn while it is
        // still only in the channel would otherwise keep being reported as
        // waiting until the worker drained a build's worth of backlog — and
        // "stop feeding a long build" is precisely the situation this verb
        // exists for.
        self.forget_accepted(id);
        Ok(format!("withdrew `{id}`"))
    }

    /// Force a member back in, bypassing its ejection.
    ///
    /// Refuses when the member is not ejected, rather than reporting a
    /// re-admission that did nothing. The snapshot is one pump behind, so this
    /// is a courtesy check for the common typo — the lane itself re-checks and
    /// reports the same refusal authoritatively.
    pub fn readmit(&self, id: &str) -> Result<String, String> {
        if !self.snapshot().ejections.iter().any(|e| e.id == id) {
            return Err(format!("`{id}` is not ejected"));
        }
        self.send(LaneEvent::ForceReadmit { id: id.to_string() })?;
        Ok(format!("re-admitted `{id}`"))
    }

    /// Advance the lane's clock. The capture window and ejection TTLs are both
    /// measured in ticks, so something must drive this — without it an idle
    /// lane waits forever for a window that never closes.
    ///
    /// Recorded here as well as sent. The `Tick` event can only be applied when
    /// the worker next reads the channel, which is after the current build or
    /// land finishes; the recorded value is what lets the driver re-sync the
    /// lane's clock the moment a blocking action ends, so a backoff computed
    /// from that moment is measured from NOW rather than from an hour ago.
    pub fn tick(&self, now: u64) {
        // Clamped forward, matching `LaneState`: a caller whose clock steps
        // backwards must not be able to rewind the lane's deadlines.
        match self.clock.lock() {
            Ok(mut c) => *c = (*c).max(now),
            Err(poisoned) => {
                let mut c = poisoned.into_inner();
                *c = (*c).max(now);
            }
        }
        let _ = self.send(LaneEvent::Tick { now });
    }

    /// What `GET /lane` reports: the worker's last published view of the lane,
    /// PLUS anything accepted since that the worker has not stepped yet.
    ///
    /// The merge happens here rather than at publish time because the worker
    /// may not publish again for the length of a build, and in that window
    /// `enqueue` and `withdraw` both need to take effect on the next read. A
    /// snapshot that only changed when the worker republished would report a
    /// withdrawn member as waiting for the rest of the build.
    #[must_use]
    pub fn snapshot(&self) -> LaneSnapshot {
        let published = match self.snapshot.lock() {
            Ok(s) => s.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let accepted = match self.accepted.lock() {
            Ok(a) => a.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let pending_withdrawals = match self.pending_withdrawals.lock() {
            Ok(pending) => pending.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        published
            .without_pending_withdrawals(&pending_withdrawals)
            .with_accepted(&accepted)
    }

    fn send(&self, event: LaneEvent) -> Result<(), String> {
        if !self.running.load(Ordering::SeqCst) {
            return Err("build lane worker is not running".to_string());
        }
        let tx = match self.tx.lock() {
            Ok(tx) => tx,
            // A poisoned send-lock means a previous caller panicked mid-send.
            // The channel itself is unaffected — recovering keeps the lane
            // reachable rather than bricking it over an unrelated panic.
            Err(poisoned) => poisoned.into_inner(),
        };
        tx.send(event)
            // A dead worker must be reported, never swallowed: a caller told
            // "queued" for a lane that will never build would wait forever.
            .map_err(|_| "build lane worker has stopped".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lane::{LaneBuildOutcome, LaneConfig};
    use crate::lanedrv::{LandOutcome, LegOutcome, MaterializeError};
    use cargoless_proto::TreeState;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::mpsc::{Sender as StdSender, channel as std_channel};
    use std::time::{Duration, Instant};

    struct NoTree;
    impl CandidateTree for NoTree {
        fn materialize(&self, _members: &[LaneMember]) -> Result<PathBuf, MaterializeError> {
            Ok(PathBuf::from("/tmp/lanehost-test-candidate"))
        }
    }

    /// A candidate materializer that deterministically names the first member
    /// as conflicting. A deep queue therefore produces one terminal conflict
    /// after another without involving a compiler or a forge.
    struct FirstMemberConflicts {
        attempts: Arc<AtomicUsize>,
    }
    impl CandidateTree for FirstMemberConflicts {
        fn materialize(&self, members: &[LaneMember]) -> Result<PathBuf, MaterializeError> {
            self.attempts.fetch_add(1, AtomicOrdering::SeqCst);
            Err(MaterializeError::Conflict {
                id: members[0].id.clone(),
                files: vec![PathBuf::from(format!("src/{}.rs", members[0].id))],
                shared_with: Vec::new(),
                reason: "fixture merge conflict".to_string(),
            })
        }
    }

    /// Legs that announce they started, then block until released. Lets a test
    /// observe the lane WHILE a build is in flight, which is the whole point of
    /// the snapshot.
    struct BlockingLegs {
        started: StdSender<()>,
        release: Mutex<Receiver<()>>,
    }
    impl LegRunner for BlockingLegs {
        fn run(&self, _root: &Path, _changed: &[String]) -> io::Result<LegOutcome> {
            let _ = self.started.send(());
            let _ = self.release.lock().expect("release lock").recv();
            Ok(LegOutcome {
                tree: TreeState::Green,
                ..Default::default()
            })
        }
    }

    struct NoLander;
    impl LaneLander for NoLander {
        fn land(&self, _m: &[LaneMember], _a: Option<&str>) -> io::Result<LandOutcome> {
            Ok(LandOutcome {
                detail: "ok".to_string(),
            })
        }
    }

    struct FailingLander;
    impl LaneLander for FailingLander {
        fn land(&self, _m: &[LaneMember], _a: Option<&str>) -> io::Result<LandOutcome> {
            Err(io::Error::other("the candidate base moved"))
        }
    }

    /// Legs that go green immediately, so a test reaches the LAND without
    /// waiting on anything.
    struct GreenLegs;
    impl LegRunner for GreenLegs {
        fn run(&self, _root: &Path, _changed: &[String]) -> io::Result<LegOutcome> {
            Ok(LegOutcome {
                tree: TreeState::Green,
                ..Default::default()
            })
        }
    }

    /// A lander that announces it started, then blocks. The real one delegates
    /// to a merge-train controller that waits on its own candidate build — up
    /// to two hours — so this is the shape, not an exaggeration.
    struct BlockingLander {
        started: StdSender<()>,
        release: Mutex<Receiver<()>>,
    }
    impl LaneLander for BlockingLander {
        fn land(&self, _m: &[LaneMember], _a: Option<&str>) -> io::Result<LandOutcome> {
            let _ = self.started.send(());
            let _ = self.release.lock().expect("release lock").recv();
            Ok(LandOutcome {
                detail: "landed".to_string(),
            })
        }
    }

    /// Poll `f` until it holds or the deadline passes. Everything here races a
    /// worker thread, and a fixed sleep is either flaky or slow.
    fn until(mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// DEFECT 1 — a member submitted DURING a build must be visible for the
    /// whole build, not after it.
    ///
    /// `enqueue` returns as soon as the event is in the mpsc channel, and
    /// nothing reads that channel until the worker returns from `pump` — which
    /// for a real lane is ~45 minutes. A snapshot built only from `LaneState`
    /// therefore reports `queue_depth: 0` with members waiting.
    ///
    /// Observed in production: `POST /lane` returned
    /// `{"detail":"queued `pr-6956`","ok":true}` and `queue_depth` stayed 0
    /// across 36s of polling. An author reads that as "the lane never got it"
    /// and re-submits — which is the correct inference from what we told them.
    #[test]
    fn a_member_enqueued_during_a_build_is_visible_immediately() {
        let (started_tx, started_rx) = std_channel();
        let (release_tx, release_rx) = std_channel();
        let driver = LaneDriver::new(
            NoTree,
            BlockingLegs {
                started: started_tx,
                release: Mutex::new(release_rx),
            },
            NoLander,
        );
        let host = LaneHost::spawn(
            LaneState::with_config(
                "/tmp/lanehost-test-inflight-queue",
                LaneConfig {
                    capture_window_ticks: 0,
                    ..Default::default()
                },
            ),
            driver,
        );

        host.enqueue(LaneMember::new("A", "sha-a")).expect("queued");
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the build should have started");
        assert!(
            until(|| host.snapshot().in_flight == vec!["A".to_string()]),
            "setup: A must be the one building"
        );

        // B arrives mid-build. The worker is parked inside the leg runner and
        // cannot read the channel, so the lane will not see this for as long as
        // the build lasts.
        host.enqueue(LaneMember::new("B", "sha-b")).expect("queued");

        // NO polling loop here, deliberately: the point is that the member is
        // visible on the very next read, not eventually. A `until(...)` would
        // pass against a fix that merely made the window shorter.
        let snap = host.snapshot();
        assert_eq!(
            snap.queue_depth, 1,
            "a member accepted during a build must be counted at once. \
             Reporting `queue_depth: 0` right after answering `queued` is the \
             same lie told twice, and the second telling is the one the author \
             acts on — they re-submit. Got {snap:?}"
        );
        assert_eq!(
            snap.queued,
            vec!["B".to_string()],
            "and it must be NAMED, so an author can reconcile it against their \
             own submission: {snap:?}"
        );
        assert_eq!(
            snap.in_flight,
            vec!["A".to_string()],
            "without disturbing who is actually building"
        );
        assert_eq!(
            snap.members
                .iter()
                .map(|m| (m.id.as_str(), m.head.as_str(), m.state))
                .collect::<Vec<_>>(),
            vec![("B", "sha-b", "queued"), ("A", "sha-a", "building")],
            "the reconciliation surface must preserve the immutable head for \
             both host-accepted and lane-owned members: {snap:?}"
        );

        let _ = release_tx.send(());

        // Once the worker drains the channel, B is in the lane's own queue and
        // must be counted ONCE, not twice.
        assert!(
            until(|| {
                let s = host.snapshot();
                s.queued == vec!["B".to_string()] || s.in_flight == vec!["B".to_string()]
            }),
            "B must be accounted for exactly once after the worker catches up: \
             {:?}",
            host.snapshot()
        );
    }

    /// A saturated ordinary lane must expose a real writer hand-off between
    /// generations. Without this window the worker consumes B immediately
    /// after A settles, so a cooperative external writer that polls once per
    /// minute sees only `building` forever and can never advance the base.
    #[test]
    fn an_intergeneration_yield_is_idle_with_the_next_member_still_queued() {
        let (started_tx, started_rx) = std_channel();
        let (release_tx, release_rx) = std_channel();
        let driver = LaneDriver::new(
            NoTree,
            BlockingLegs {
                started: started_tx,
                release: Mutex::new(release_rx),
            },
            NoLander,
        );
        let host = LaneHost::spawn_with_intergeneration_yield(
            LaneState::with_config(
                "/tmp/lanehost-test-writer-yield",
                LaneConfig {
                    capture_window_ticks: 0,
                    ..Default::default()
                },
            ),
            driver,
            Duration::from_millis(500),
        );

        host.enqueue(LaneMember::new("A", "sha-a")).expect("queued");
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("A should start");
        host.enqueue(LaneMember::new("B", "sha-b")).expect("queued");
        release_tx.send(()).expect("release A");

        assert!(
            until(|| {
                let snapshot = host.snapshot();
                snapshot.generation == 1
                    && snapshot.phase == "idle"
                    && snapshot.activity == "settled"
                    && snapshot.queued == vec!["B".to_string()]
                    && snapshot.in_flight.is_empty()
            }),
            "the hand-off must be observably safe while preserving B: {:?}",
            host.snapshot()
        );
        assert!(
            started_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "B started inside the configured writer hand-off window"
        );
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("B should start after the bounded yield");
        assert!(
            until(|| host.snapshot().in_flight == vec!["B".to_string()]),
            "B must remain queued work, not be dropped by the yield: {:?}",
            host.snapshot()
        );
        release_tx.send(()).expect("release B");
    }

    /// Production regression, 2026-08-28: generation 111 wrote a terminal
    /// `outcome=conflict`, but GET /lane remained `phase=building` /
    /// `activity=settled` for hours with seven members in flight and no build
    /// process. The driver had reached its 64-step recursion guard and silently
    /// dropped the pending BuildFinished event, leaving the pure state machine
    /// latched in Building after the subprocess was already gone.
    ///
    /// A large conflict-heavy queue is legitimate finite work, not a policy
    /// loop. Reaching the pump budget must yield in an honest idle state with
    /// every unjudged survivor retained. A later host tick may start the next
    /// generation; this pump may not claim a build that does not exist.
    #[test]
    fn the_step_budget_yields_idle_instead_of_latching_a_completed_conflict_build() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let driver = LaneDriver::new(
            FirstMemberConflicts {
                attempts: attempts.clone(),
            },
            GreenLegs,
            NoLander,
        );
        let host = LaneHost::spawn(
            LaneState::with_config(
                "/tmp/lanehost-test-conflict-step-budget",
                LaneConfig {
                    max_members: 10,
                    capture_window_ticks: 100,
                    ..Default::default()
                },
            ),
            driver,
        );

        for i in 0..70 {
            host.enqueue(LaneMember::new(format!("P{i:02}"), format!("sha-{i:02}")))
                .expect("queued");
        }
        // This sentinel is ordered after every Enqueue on the host channel.
        // Seeing its clock value proves the worker owns all 70 members before
        // the build-triggering tick arrives; the accepted read-side overlay
        // alone is not sufficient evidence of that.
        host.tick(1);
        assert!(
            until(|| {
                let snapshot = host.snapshot();
                snapshot.now == 1 && snapshot.phase == "idle" && snapshot.queue_depth == 70
            }),
            "setup must drain all admissions without opening the capture window: {:?}",
            host.snapshot()
        );

        host.tick(100);
        assert!(
            until(|| attempts.load(AtomicOrdering::SeqCst) >= 63),
            "the fixture must reach the old 64-step boundary"
        );

        let snapshot = host.snapshot();
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 63);
        assert_eq!(snapshot.generation, 63);
        assert_eq!(snapshot.phase, "idle", "no build exists: {snapshot:?}");
        assert_eq!(
            snapshot.activity, "settled",
            "the pump has returned and no blocking action exists: {snapshot:?}"
        );
        assert!(
            snapshot.in_flight.is_empty(),
            "no phantom build: {snapshot:?}"
        );
        assert_eq!(
            snapshot.queue_depth, 7,
            "all seven unjudged survivors must remain queued for the next tick: {snapshot:?}"
        );
    }

    /// A member accepted behind a long build must join the retry after a
    /// compare-and-swap miss instead of being starved behind the old roster.
    ///
    /// The event order is load-bearing and mirrors the production incident:
    /// while A builds, a timer tick is queued and THEN B is accepted. A green
    /// build loses its landing CAS race, so A is requeued with an infrastructure
    /// backoff. If that deadline was measured from build START, the already
    /// queued tick clears it and starts A again before the worker reaches B.
    /// B stays trapped in the host channel for another whole generation.
    ///
    /// Re-syncing the lane clock after the blocking land measures the backoff
    /// from failure instead. The stale tick cannot expire it, the worker drains
    /// B into the lane, and the next generation contains both members.
    #[test]
    fn a_stale_tick_cannot_starve_a_member_accepted_during_a_failed_land() {
        let (started_tx, started_rx) = std_channel();
        let (release_tx, release_rx) = std_channel();
        let driver = LaneDriver::new(
            NoTree,
            BlockingLegs {
                started: started_tx,
                release: Mutex::new(release_rx),
            },
            FailingLander,
        );
        let host = LaneHost::spawn(
            LaneState::with_config(
                "/tmp/lanehost-test-land-retry-admission",
                LaneConfig {
                    capture_window_ticks: 0,
                    infra_backoff_ticks: 120,
                    ..Default::default()
                },
            ),
            driver,
        );

        host.enqueue(LaneMember::new("A", "sha-a")).expect("queued");
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the first build should start");

        // This order reproduces the incident. Both events sit behind the
        // blocking build, but the stale tick is ahead of B in the worker's
        // channel.
        host.tick(1_000);
        host.enqueue(LaneMember::new("B", "sha-b")).expect("queued");
        assert_eq!(
            host.snapshot().queued,
            vec!["B".to_string()],
            "B must be visible while it waits in the host channel"
        );

        release_tx.send(()).expect("release the first build");

        assert!(
            until(|| {
                let s = host.snapshot();
                s.phase == "idle"
                    && s.generation == 1
                    && s.queued == vec!["A".to_string(), "B".to_string()]
            }),
            "the failed land must retain A AND drain B before retrying; an old \
             tick must not start another A-only generation: {:?}",
            host.snapshot()
        );

        // The backoff was installed at tick 1,000, so only its real deadline
        // may start generation 2. That generation must include the member that
        // arrived during generation 1.
        host.tick(1_120);
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the retry should start at its deadline");
        assert!(
            until(|| {
                let s = host.snapshot();
                s.generation == 2 && s.in_flight == vec!["A".to_string(), "B".to_string()]
            }),
            "the retry must build the complete admitted roster: {:?}",
            host.snapshot()
        );

        release_tx.send(()).expect("release the retry build");
    }

    /// DEFECT 1, the withdraw half: a member withdrawn while it is still only
    /// in the channel must stop being reported as waiting.
    ///
    /// "Stop feeding a long build" is the situation `withdraw` exists for, so
    /// the accepted-set must not keep the member alive in the snapshot until
    /// the worker drains a build's worth of backlog.
    #[test]
    fn withdrawing_a_member_still_in_the_channel_removes_it_from_the_snapshot() {
        let (started_tx, started_rx) = std_channel();
        let (release_tx, release_rx) = std_channel();
        let driver = LaneDriver::new(
            NoTree,
            BlockingLegs {
                started: started_tx,
                release: Mutex::new(release_rx),
            },
            NoLander,
        );
        let host = LaneHost::spawn(
            LaneState::with_config(
                "/tmp/lanehost-test-withdraw-channel",
                LaneConfig {
                    capture_window_ticks: 0,
                    ..Default::default()
                },
            ),
            driver,
        );

        host.enqueue(LaneMember::new("A", "sha-a")).expect("queued");
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("build started");

        host.enqueue(LaneMember::new("B", "sha-b")).expect("queued");
        assert_eq!(host.snapshot().queue_depth, 1, "setup: B is waiting");

        host.withdraw("B").expect("withdrawn");
        let snap = host.snapshot();
        assert_eq!(
            snap.queue_depth, 0,
            "a withdrawn member must leave the snapshot at once — it is exactly \
             the long build you have decided to stop feeding: {snap:?}"
        );

        let _ = release_tx.send(());
    }

    /// A single PR may be updated repeatedly while the worker is blocked. The
    /// first withdrawal owns the queued lane event; each later withdrawal must
    /// still remove the accepted replacement that became stale meanwhile.
    #[test]
    fn repeated_pending_withdrawal_removes_the_latest_accepted_head() {
        let (started_tx, started_rx) = std_channel();
        let (release_tx, release_rx) = std_channel();
        let driver = LaneDriver::new(
            NoTree,
            BlockingLegs {
                started: started_tx,
                release: Mutex::new(release_rx),
            },
            NoLander,
        );
        let host = LaneHost::spawn(
            LaneState::with_config(
                "/tmp/lanehost-test-repeated-pending-withdrawal",
                LaneConfig {
                    capture_window_ticks: 0,
                    ..Default::default()
                },
            ),
            driver,
        );

        host.enqueue(LaneMember::new("A", "sha-a")).expect("queued");
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("build started");

        host.enqueue(LaneMember::new("B", "sha-old"))
            .expect("old head queued");
        host.withdraw("B").expect("first withdrawal queued");
        host.enqueue(LaneMember::new("B", "sha-replacement"))
            .expect("replacement queued");
        assert!(
            host.snapshot()
                .members
                .iter()
                .any(|member| { member.id == "B" && member.head == "sha-replacement" })
        );

        let detail = host.withdraw("B").expect("repeat withdrawal accepted");
        assert!(detail.contains("already pending"));
        assert!(
            !host
                .snapshot()
                .members
                .iter()
                .any(|member| member.id == "B"),
            "the repeated withdrawal must hide the superseded replacement"
        );

        host.enqueue(LaneMember::new("B", "sha-newest"))
            .expect("newest head queued");
        assert!(
            host.snapshot()
                .members
                .iter()
                .any(|member| { member.id == "B" && member.head == "sha-newest" })
        );

        let _ = release_tx.send(());
    }

    /// A member can already be in the lane's published queue when an unrelated
    /// build blocks the worker. The accepted-set fix above does not cover that
    /// case: the old member must be hidden by the withdrawal tombstone, and a
    /// newer exact-head enqueue of the same PR must still be shown.
    #[test]
    fn pending_withdrawal_hides_published_head_but_not_its_replacement() {
        let published = LaneSnapshot {
            phase: "building",
            queue_depth: 1,
            queued: vec!["B".to_string()],
            generation: 7,
            now: 0,
            in_flight: vec!["A".to_string()],
            activity: "building",
            landing: Vec::new(),
            members: vec![
                MemberView {
                    id: "B".to_string(),
                    head: "sha-old".to_string(),
                    state: "queued",
                },
                MemberView {
                    id: "A".to_string(),
                    head: "sha-a".to_string(),
                    state: "building",
                },
            ],
            ejections: Vec::new(),
        };
        let pending = HashSet::from(["B".to_string()]);

        let withdrawn = published.clone().without_pending_withdrawals(&pending);
        assert_eq!(withdrawn.queue_depth, 0);
        assert!(!withdrawn.members.iter().any(|member| member.id == "B"));

        let replacement = LaneMember::new("B", "sha-new");
        let visible = published
            .without_pending_withdrawals(&pending)
            .with_accepted(&[replacement]);
        assert_eq!(visible.queue_depth, 1);
        assert_eq!(visible.queued, vec!["B".to_string()]);
        assert!(visible.members.iter().any(|member| {
            member.id == "B" && member.head == "sha-new" && member.state == "queued"
        }));
        assert!(
            !visible
                .members
                .iter()
                .any(|member| member.head == "sha-old")
        );
    }

    /// DEFECT 2 — `GET /lane` must not report a quiet lane while the lander is
    /// moving the trunk.
    ///
    /// This is NOT the build case. Here the lane is legitimately `Idle`: the
    /// build finished, the verdict was green, `in_flight` was emptied. The
    /// phase is honest and still misleading, because the lander then runs for
    /// up to two hours — and that is the single most destructive moment to roll
    /// the daemon. A snapshot reading `idle` actively invites it.
    #[test]
    fn the_snapshot_says_landing_while_the_lander_runs() {
        let (land_started_tx, land_started_rx) = std_channel();
        let (release_tx, release_rx) = std_channel();
        let driver = LaneDriver::new(
            NoTree,
            GreenLegs,
            BlockingLander {
                started: land_started_tx,
                release: Mutex::new(release_rx),
            },
        );
        let host = LaneHost::spawn(
            LaneState::with_config(
                "/tmp/lanehost-test-landing",
                LaneConfig {
                    capture_window_ticks: 0,
                    ..Default::default()
                },
            ),
            driver,
        );

        host.enqueue(LaneMember::new("A", "sha-a")).expect("queued");
        land_started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the lander should have been called");

        // The worker is parked inside `land`. Poll briefly: the observe call
        // that publishes `landing` happens a few instructions before the lander
        // signals, so asserting immediately would race it.
        assert!(
            until(|| host.snapshot().activity == "landing"),
            "the trunk is being moved RIGHT NOW and the snapshot must say so. \
             Reporting a quiet lane here is what invites an operator to roll the \
             daemon mid-merge — the one operation that must not be interrupted. \
             Got {:?}",
            host.snapshot()
        );

        let snap = host.snapshot();
        assert_eq!(
            snap.landing,
            vec!["A".to_string()],
            "and it must name WHO is landing — `in_flight` is already empty by \
             this point, so nothing else can answer that: {snap:?}"
        );
        assert_eq!(
            snap.members
                .iter()
                .map(|m| (m.id.as_str(), m.head.as_str(), m.state))
                .collect::<Vec<_>>(),
            vec![("A", "sha-a", "landing")],
            "a landing lease must still expose the exact head after in_flight \
             has been drained: {snap:?}"
        );
        // The lane's own phase really is idle here. That is the whole reason
        // `activity` had to be added rather than fixing `phase`: no amount of
        // transition-watching can report a state the state machine is not in.
        assert_eq!(
            snap.phase, "idle",
            "the lane IS idle — the fix reports the driver's activity beside \
             the phase, it does not falsify the phase"
        );

        let _ = release_tx.send(());

        // And it must not get STUCK on `landing` — a successful land returns no
        // follow-up event, so the snapshot has to be republished explicitly.
        assert!(
            until(|| host.snapshot().activity == "settled"),
            "after the land completes the activity must return to settled, or \
             the fix is just the opposite lie: {:?}",
            host.snapshot()
        );
    }

    /// DEFECT 2, the build half: the activity must not regress the case that
    /// already worked.
    #[test]
    fn the_snapshot_says_building_while_the_legs_run() {
        let (started_tx, started_rx) = std_channel();
        let (release_tx, release_rx) = std_channel();
        let driver = LaneDriver::new(
            NoTree,
            BlockingLegs {
                started: started_tx,
                release: Mutex::new(release_rx),
            },
            NoLander,
        );
        let host = LaneHost::spawn(
            LaneState::with_config(
                "/tmp/lanehost-test-activity-building",
                LaneConfig {
                    capture_window_ticks: 0,
                    ..Default::default()
                },
            ),
            driver,
        );

        host.enqueue(LaneMember::new("A", "sha-a")).expect("queued");
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("build started");
        assert!(
            until(|| {
                let s = host.snapshot();
                s.activity == "building" && s.phase == "building"
            }),
            "a running build must report BOTH phase and activity as building: \
             {:?}",
            host.snapshot()
        );

        let _ = release_tx.send(());
    }

    /// The property the host exists for: `GET /lane` must answer while a build
    /// is running. A reader sharing the worker's lock would block for the whole
    /// build — and the endpoint exists precisely for someone whose change
    /// stopped moving.
    #[test]
    fn the_snapshot_answers_while_a_build_is_in_flight() {
        let (started_tx, started_rx) = std_channel();
        let (release_tx, release_rx) = std_channel();
        let driver = LaneDriver::new(
            NoTree,
            BlockingLegs {
                started: started_tx,
                release: Mutex::new(release_rx),
            },
            NoLander,
        );
        let lane = LaneState::with_config(
            "/tmp/lanehost-test",
            LaneConfig {
                capture_window_ticks: 0,
                ..Default::default()
            },
        );
        let host = LaneHost::spawn(lane, driver);

        host.enqueue(LaneMember::new("A", "sha-a")).expect("queued");
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the build should have started");

        // The worker is now parked inside the build. A snapshot must still
        // return promptly rather than waiting it out.
        // Two independent properties, asserted separately.
        //
        // 1. The read does not BLOCK. This is the whole reason the host
        //    publishes a snapshot instead of letting readers take the lane's
        //    lock: the worker is parked inside a build right now, and a reader
        //    sharing that lock would wait out the entire build.
        let t0 = Instant::now();
        let snap = host.snapshot();
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "snapshot blocked for {:?} — readers are sharing the build's lock",
            t0.elapsed()
        );

        // 2. The snapshot REFLECTS the running build. `started_rx` fires from
        //    inside the leg runner, which the worker reaches a few instructions
        //    after publishing — so poll briefly rather than racing it. The
        //    earlier version asserted immediately and flaked, reading the
        //    pre-build `idle`.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut snap = snap;
        while snap.phase != "building" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            snap = host.snapshot();
        }
        assert_eq!(
            snap.phase, "building",
            "a build is in flight, so `GET /lane` must say so — reporting idle \
             for the duration of a build is exactly what makes an author think \
             the lane never received their submission"
        );
        assert_eq!(snap.in_flight, vec!["A".to_string()]);

        let _ = release_tx.send(());
    }

    /// Re-admitting something that is not ejected must be refused, not
    /// silently accepted — an operator told "re-admitted" would believe they
    /// unblocked something.
    #[test]
    fn readmitting_an_unejected_member_is_refused() {
        let (started_tx, _started_rx) = std_channel();
        let (_release_tx, release_rx) = std_channel();
        let driver = LaneDriver::new(
            NoTree,
            BlockingLegs {
                started: started_tx,
                release: Mutex::new(release_rx),
            },
            NoLander,
        );
        let host = LaneHost::spawn(LaneState::new("/tmp/lanehost-test-2"), driver);

        let err = host.readmit("never-heard-of-it").expect_err("must refuse");
        assert!(err.contains("not ejected"), "unhelpful message: {err}");
    }

    /// A green build with no diagnostics is not interesting on its own; what
    /// matters is that the outcome type still round-trips through the host so
    /// a future change cannot quietly drop `BuildFinished` handling.
    #[test]
    fn the_lane_reaches_idle_after_a_build_completes() {
        let (started_tx, started_rx) = std_channel();
        let (release_tx, release_rx) = std_channel();
        let driver = LaneDriver::new(
            NoTree,
            BlockingLegs {
                started: started_tx,
                release: Mutex::new(release_rx),
            },
            NoLander,
        );
        let lane = LaneState::with_config(
            "/tmp/lanehost-test-3",
            LaneConfig {
                capture_window_ticks: 0,
                ..Default::default()
            },
        );
        let host = LaneHost::spawn(lane, driver);

        host.enqueue(LaneMember::new("A", "sha-a")).expect("queued");
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("build started");
        let _ = release_tx.send(());

        // Poll rather than sleep a fixed amount: the worker publishes after the
        // pump, and a fixed sleep is either flaky or slow.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if host.snapshot().phase == "idle" {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("lane never returned to idle: {:?}", host.snapshot());
    }

    // Silences an unused-import warning under -D warnings when the outcome
    // type is only referenced in doc prose above.
    #[allow(dead_code)]
    fn _outcome_type_is_used(_: LaneBuildOutcome) {}

    // ── the projection must carry the SENTENCE, not just the tags ──────────
    //
    // `EjectReason::describe_for` is the author-facing product surface and is
    // contract-tested in tests/lane_policy.rs. It reached `LaneAction::Report`
    // — which `LaneDriver::execute` discards — and nothing else, so the one
    // field that explains an ejection never left the process. Everything a
    // reader could actually see (`kind`, `files`, `shared_with`) is ambiguous
    // on its own: `files: []` means "could not identify them" for
    // `unattributed` and "nothing was compiled" for `infrastructure`.
    //
    // These tests fail if the sentence is ever dropped from the projection
    // again.

    fn ejecting_lane(outcome: LaneBuildOutcome, changed: &[&str]) -> LaneState {
        let mut lane = LaneState::with_config(
            "/w",
            LaneConfig {
                capture_window_ticks: 0,
                ..Default::default()
            },
        );
        lane.step(LaneEvent::Enqueue(LaneMember {
            id: "pr-1".to_string(),
            head: "head-1".to_string(),
            changed_files: changed.iter().map(|s| (*s).to_string()).collect(),
        }));
        let generation = lane.generation();
        lane.step(LaneEvent::BuildFinished {
            generation,
            outcome,
        });
        lane
    }

    fn only_ejection(lane: &LaneState) -> EjectionView {
        let snap = LaneSnapshot::of(lane, &LaneActivity::Settled);
        assert_eq!(snap.ejections.len(), 1, "fixture ejects exactly one member");
        snap.ejections
            .into_iter()
            .next()
            .expect("just asserted one")
    }

    #[test]
    fn an_attributed_ejection_publishes_the_sentence_that_names_the_files() {
        let lane = ejecting_lane(
            LaneBuildOutcome::Red {
                diagnostics: vec![cargoless_proto::Diagnostic {
                    file_path: PathBuf::from("/w/src/a.rs"),
                    line: 1,
                    col: 1,
                    severity: cargoless_proto::Severity::Error,
                    code: Some("E0308".to_string()),
                    message: "boom".to_string(),
                    source: Some("rustc".to_string()),
                }],
            },
            &["src/a.rs"],
        );
        let ej = only_ejection(&lane);
        assert_eq!(ej.kind, "attributed");
        assert!(
            ej.why.contains("src/a.rs"),
            "the published sentence must name the failing file: {}",
            ej.why
        );
        assert!(
            !ej.why.is_empty(),
            "an ejection must never be published without its reason"
        );
    }

    #[test]
    fn an_infrastructure_ejection_publishes_that_nothing_was_judged() {
        // The distinction that costs the most when it is lost: `unattributed`
        // means "your tree is red and we cannot say whose change did it";
        // `infrastructure` means nothing compiled at all. A reader who takes
        // the second for the first hunts a bug that does not exist — and
        // `kind` alone, with `files: []` in both, cannot tell them apart.
        let mut lane = LaneState::with_config(
            "/w",
            LaneConfig {
                capture_window_ticks: 0,
                infra_backoff_ticks: 0,
                ..Default::default()
            },
        );
        lane.step(LaneEvent::Enqueue(LaneMember {
            id: "pr-1".to_string(),
            head: "head-1".to_string(),
            changed_files: vec!["src/a.rs".to_string()],
        }));
        // Fail the same way until the lane gives up and ejects.
        for _ in 0..(LaneConfig::default().infra_max_attempts + 1) {
            let generation = lane.generation();
            lane.step(LaneEvent::BuildFinished {
                generation,
                outcome: LaneBuildOutcome::Infra {
                    reason: "runner vanished".to_string(),
                },
            });
            lane.step(LaneEvent::Tick {
                now: lane.now() + 1,
            });
        }
        let ej = only_ejection(&lane);
        assert_eq!(ej.kind, "infrastructure");
        assert!(
            ej.why.contains("NOT a verdict about your change"),
            "an infra ejection must say plainly that nothing was judged: {}",
            ej.why
        );
        assert!(
            ej.why.contains("runner vanished"),
            "and must carry the build's own words: {}",
            ej.why
        );
    }

    #[test]
    fn the_snapshot_publishes_the_clock_its_deadlines_are_measured_against() {
        // `expires_at_tick` is a bare deadline. Without the lane's own clock
        // beside it a reader cannot answer "how long until this clears", which
        // is the question an author whose change stopped moving actually has.
        let lane = ejecting_lane(
            LaneBuildOutcome::UnattributedRed {
                diagnostics: Vec::new(),
            },
            &["src/a.rs"],
        );
        let snap = LaneSnapshot::of(&lane, &LaneActivity::Settled);
        assert_eq!(snap.now, lane.now(), "the published clock is the lane's");
        let ej = &snap.ejections[0];
        assert!(
            ej.expires_at_tick > snap.now,
            "a live ejection lapses in the future: {} vs {}",
            ej.expires_at_tick,
            snap.now
        );
    }
}
