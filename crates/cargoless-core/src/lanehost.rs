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
//! Publishing happens on **every state transition**, not just when the pump
//! returns. The transition that flips the phase to `Building` is immediately
//! followed by the blocking build, so publishing only afterwards would report
//! `idle` for the entire duration of every build — the exact window the
//! endpoint exists to explain.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::lane::{EjectReason, LaneEvent, LaneMember, LanePhase, LaneState};
use crate::lanedrv::{CandidateTree, LaneDriver, LaneLander, LegRunner};

/// What `GET /lane` reports. Cheap to clone; never holds a lock.
#[derive(Debug, Clone, Default)]
pub struct LaneSnapshot {
    pub phase: &'static str,
    pub queue_depth: usize,
    pub generation: u64,
    /// Member ids in the running build, empty when idle.
    pub in_flight: Vec<String>,
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
    fn of(lane: &LaneState) -> Self {
        Self {
            phase: match lane.phase() {
                LanePhase::Idle => "idle",
                LanePhase::Building => "building",
            },
            queue_depth: lane.queue_depth(),
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
        let snapshot = Arc::new(Mutex::new(LaneSnapshot::of(&lane)));
        let running = Arc::new(AtomicBool::new(true));

        let worker_snapshot = snapshot.clone();
        let worker_running = running.clone();
        thread::Builder::new()
            .name("cargoless-lane".to_string())
            .spawn(move || {
                // `recv` ends when every Sender is dropped, i.e. when the host
                // goes away. No shutdown flag to get wrong.
                while let Ok(event) = rx.recv() {
                    // Publish on EVERY transition, not just when the pump
                    // returns.
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
                    // Poisoning is ignored deliberately: a panicked reader must
                    // not silently stop the lane from reporting, and the
                    // snapshot is rebuilt from scratch each time so there is no
                    // corrupt state to inherit.
                    driver.pump_observed(&mut lane, event, |live| match worker_snapshot.lock() {
                        Ok(mut s) => *s = LaneSnapshot::of(live),
                        Err(poisoned) => *poisoned.into_inner() = LaneSnapshot::of(live),
                    });
                    // And once more after, so the terminal state of the last
                    // transition is visible even if the loop exits here.
                    match worker_snapshot.lock() {
                        Ok(mut s) => *s = LaneSnapshot::of(&lane),
                        Err(poisoned) => *poisoned.into_inner() = LaneSnapshot::of(&lane),
                    }
                }
                worker_running.store(false, Ordering::SeqCst);
            })
            .expect("spawn lane worker");

        Self {
            tx: Mutex::new(tx),
            snapshot,
            running,
        }
    }

    /// Submit a member. Returns as soon as the event is queued — the build it
    /// may trigger runs on the worker.
    pub fn enqueue(&self, member: LaneMember) -> Result<String, String> {
        let id = member.id.clone();
        self.send(LaneEvent::Enqueue(member))?;
        Ok(format!("queued `{id}`"))
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
    pub fn tick(&self, now: u64) {
        let _ = self.send(LaneEvent::Tick { now });
    }

    #[must_use]
    pub fn snapshot(&self) -> LaneSnapshot {
        match self.snapshot.lock() {
            Ok(s) => s.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
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
    use crate::lanedrv::{LandOutcome, LegOutcome};
    use cargoless_proto::TreeState;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Sender as StdSender, channel as std_channel};
    use std::time::{Duration, Instant};

    struct NoTree;
    impl CandidateTree for NoTree {
        fn materialize(&self, _members: &[LaneMember]) -> io::Result<PathBuf> {
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
