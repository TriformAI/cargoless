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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::lane::{EjectReason, LaneEvent, LaneMember, LanePhase, LaneState};
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
    /// One entry per live ejection: (id, kind, human-readable reason, files).
    pub ejections: Vec<EjectionView>,
}

#[derive(Debug, Clone)]
pub struct EjectionView {
    pub id: String,
    pub head: String,
    /// `attributed` or `unattributed` — the two are cleared by different
    /// things, so an author needs to know which they have.
    pub kind: &'static str,
    /// The files that carried the failure. Empty for an unattributed ejection,
    /// which is the point: we could not identify them.
    pub files: Vec<String>,
    /// Other members implicated in the same failure.
    pub shared_with: Vec<String>,
    pub expires_at_tick: u64,
}

impl LaneSnapshot {
    /// Render the LANE's own view. The host folds in what it has accepted but
    /// the lane has not stepped yet — see [`LaneSnapshot::with_accepted`].
    fn of(lane: &LaneState, activity: &LaneActivity) -> Self {
        let queued: Vec<String> = lane.queued().iter().map(|m| m.id.clone()).collect();
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
            landing: match activity {
                LaneActivity::Landing { members } => members.clone(),
                _ => Vec::new(),
            },
            queue_depth: queued.len(),
            queued,
            generation: lane.generation(),
            in_flight: lane.in_flight().iter().map(|m| m.id.clone()).collect(),
            ejections: lane
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
                        // A THIRD kind, not folded into "unattributed". They
                        // clear differently and they mean different things:
                        // unattributed says the tree is red and the owner is
                        // unknown, infrastructure says nothing was ever built.
                        // An operator triaging `GET /lane` needs to tell "go
                        // find the interaction bug" from "go fix the daemon".
                        EjectReason::Infrastructure { shared_with, .. } => {
                            ("infrastructure", Vec::new(), shared_with.clone())
                        }
                    };
                    EjectionView {
                        id: id.clone(),
                        head: e.head.clone(),
                        kind,
                        files,
                        shared_with,
                        expires_at_tick: e.expires_at_tick,
                    }
                })
                .collect(),
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
    fn with_accepted(mut self, accepted: &[String]) -> Self {
        for id in accepted {
            if !self.queued.contains(id)
                && !self.in_flight.contains(id)
                && !self.ejections.iter().any(|e| e.id == *id)
            {
                self.queued.push(id.clone());
            }
        }
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
    accepted: Arc<Mutex<Vec<String>>>,
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
    pub fn spawn<T, R, L>(mut lane: LaneState, driver: LaneDriver<T, R, L>) -> Self
    where
        T: CandidateTree + Send + 'static,
        R: LegRunner + Send + 'static,
        L: LaneLander + Send + 'static,
    {
        let (tx, rx): (Sender<LaneEvent>, Receiver<LaneEvent>) = channel();
        let accepted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
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
        thread::Builder::new()
            .name("cargoless-lane".to_string())
            .spawn(move || {
                // `recv` ends when every Sender is dropped, i.e. when the host
                // goes away. No shutdown flag to get wrong.
                while let Ok(event) = rx.recv() {
                    // The lane is about to account for this member itself, so
                    // stop counting it here. Done BEFORE the pump: the lane's
                    // `Enqueue` step is the first thing that runs, and the
                    // snapshot published from inside the pump must already see
                    // it in `queue` rather than in both places.
                    if let LaneEvent::Enqueue(m) = &event {
                        let id = m.id.clone();
                        match worker_accepted.lock() {
                            Ok(mut a) => a.retain(|x| *x != id),
                            Err(poisoned) => poisoned.into_inner().retain(|x| *x != id),
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
                    driver.pump_observed(&mut lane, event, |live, activity| {
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
                }
                worker_running.store(false, Ordering::SeqCst);
            })
            .expect("spawn lane worker");

        Self {
            tx: Mutex::new(tx),
            snapshot,
            running,
            accepted,
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
        self.remember_accepted(&id);
        if let Err(e) = self.send(LaneEvent::Enqueue(member)) {
            self.forget_accepted(&id);
            return Err(e);
        }
        Ok(format!("queued `{id}`"))
    }

    fn remember_accepted(&self, id: &str) {
        let mut a = match self.accepted.lock() {
            Ok(a) => a,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !a.iter().any(|x| x == id) {
            a.push(id.to_string());
        }
    }

    fn forget_accepted(&self, id: &str) {
        match self.accepted.lock() {
            Ok(mut a) => a.retain(|x| x != id),
            Err(poisoned) => poisoned.into_inner().retain(|x| x != id),
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
        self.send(LaneEvent::Withdraw { id: id.to_string() })?;
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
        published.with_accepted(&accepted)
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
    use std::sync::mpsc::{Sender as StdSender, channel as std_channel};
    use std::time::{Duration, Instant};

    struct NoTree;
    impl CandidateTree for NoTree {
        fn materialize(&self, _members: &[LaneMember]) -> Result<PathBuf, MaterializeError> {
            Ok(PathBuf::from("/tmp/lanehost-test-candidate"))
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
}
