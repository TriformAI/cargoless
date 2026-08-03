//! Lane policy tests.
//!
//! The lane decides who gets blamed for a red build and who has to wait. Both
//! are expensive to get wrong — an unjust ejection costs someone an hour and
//! teaches them to distrust the gate, and a missed ejection re-runs a build that
//! cannot pass. The policy is a pure state machine precisely so those decisions
//! can be pinned here without launching a compiler.
//!
//! Every test below corresponds to a rule the design commits to. If a rule
//! changes, a test here must change with it — that is the point.

use std::path::PathBuf;

use cargoless_core::lane::{
    EjectReason, LaneAction, LaneBuildOutcome, LaneEvent, LaneMember, LanePhase, LaneState,
};
use cargoless_proto::{Diagnostic, Severity};

const ROOT: &str = "/w";

fn member(id: &str, head: &str, files: &[&str]) -> LaneMember {
    LaneMember::new(id, head).with_changed_files(files.iter().copied())
}

/// An error diagnostic at `path`. Absolute, as a real build in a scratch
/// worktree would report it.
fn err(path: &str, line: u32, code: &str) -> Diagnostic {
    Diagnostic {
        file_path: PathBuf::from(format!("/scratch/xyz/{path}")),
        line,
        col: 1,
        severity: Severity::Error,
        code: Some(code.to_string()),
        message: format!("something is wrong in {path}"),
        source: Some("rustc".to_string()),
    }
}

const WINDOW: u64 = 60;
/// Mirror of `LaneConfig::default()`'s infra retry policy. Kept as constants so
/// the tests below read as "one tick before the backoff" rather than as magic
/// arithmetic, and asserted against the real defaults in
/// `the_infra_retry_constants_match_the_shipped_defaults` — a test that would
/// otherwise silently start proving nothing if a default changed.
const INFRA_BACKOFF: u64 = 120;
const INFRA_MAX_ATTEMPTS: u32 = 10;

/// A lane with the default capture window.
fn lane() -> LaneState {
    LaneState::with_config(
        ROOT,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: WINDOW,
            ..Default::default()
        },
    )
}

/// Enqueue everyone, let the capture window close, and return the generation of
/// the build they all landed in.
///
/// This is the production shape: arrivals gather during the window, then one
/// build takes the batch. The assert makes a setup bug (someone left out of the
/// build) fail here with a clear message rather than surfacing as a confusing
/// attribution failure further down.
fn start_build(st: &mut LaneState, members: Vec<LaneMember>) -> u64 {
    let want = members.len();
    for m in members {
        st.step(LaneEvent::Enqueue(m));
    }
    st.step(LaneEvent::Tick { now: WINDOW });
    assert_eq!(
        st.in_flight().len(),
        want,
        "setup expected all {want} members in ONE build; got {:?}",
        st.in_flight().iter().map(|m| &m.id).collect::<Vec<_>>()
    );
    st.generation()
}

/// Same as [`start_build`] but for a lane whose capture window is 0, so no Tick
/// is needed and the clock stays at its initial value.
fn start_build_now(st: &mut LaneState, members: Vec<LaneMember>) -> u64 {
    let want = members.len();
    for m in members {
        st.step(LaneEvent::Enqueue(m));
    }
    assert_eq!(
        st.in_flight().len(),
        want,
        "setup expected all {want} members in ONE build; got {:?}",
        st.in_flight().iter().map(|m| &m.id).collect::<Vec<_>>()
    );
    st.generation()
}

fn ejected_ids(actions: &[LaneAction]) -> Vec<String> {
    actions
        .iter()
        .filter_map(|a| match a {
            LaneAction::Eject { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn eject_reason(actions: &[LaneAction], id: &str) -> EjectReason {
    actions
        .iter()
        .find_map(|a| match a {
            LaneAction::Eject { id: i, reason } if i == id => Some(reason.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected {id} to be ejected; got {actions:?}"))
}

fn started(actions: &[LaneAction]) -> Option<Vec<String>> {
    actions.iter().find_map(|a| match a {
        LaneAction::StartBuild { members, .. } => {
            Some(members.iter().map(|m| m.id.clone()).collect())
        }
        _ => None,
    })
}

// ── queueing ──────────────────────────────────────────────────────────────

#[test]
fn the_capture_window_gathers_a_burst_into_one_build() {
    // Without this the lane builds whoever arrives first and everyone else
    // waits a FULL cycle — and a cycle is a real release build, 25-80 minutes.
    // Two changes landing seconds apart would cost two hours instead of one.
    let mut st = lane();
    let a = st.step(LaneEvent::Enqueue(member("A", "a1", &["src/a.rs"])));
    assert!(
        started(&a).is_none(),
        "the first arrival opens the window, it does not start a build"
    );

    st.step(LaneEvent::Enqueue(member("B", "b1", &["src/b.rs"])));
    st.step(LaneEvent::Enqueue(member("C", "c1", &["src/c.rs"])));
    assert_eq!(st.phase(), LanePhase::Idle, "still gathering");

    let actions = st.step(LaneEvent::Tick { now: WINDOW });
    assert_eq!(
        started(&actions).as_deref(),
        Some(&["A".to_string(), "B".to_string(), "C".to_string()][..]),
        "the whole burst rides one build"
    );
}

#[test]
fn a_full_queue_does_not_wait_out_the_window() {
    // There is nothing left to capture once the build is full, so waiting would
    // be pure latency.
    let mut st = LaneState::with_config(
        ROOT,
        cargoless_core::lane::LaneConfig {
            max_members: 2,
            capture_window_ticks: 600,
            ..Default::default()
        },
    );
    st.step(LaneEvent::Enqueue(member("A", "a", &["src/a.rs"])));
    let actions = st.step(LaneEvent::Enqueue(member("B", "b", &["src/b.rs"])));
    assert_eq!(
        started(&actions).as_deref(),
        Some(&["A".to_string(), "B".to_string()][..]),
        "a full build starts immediately, window or not"
    );
}

#[test]
fn work_that_was_already_waiting_never_sits_through_a_second_window() {
    // The window gathers FRESH arrivals. It must never delay work that was
    // already queued: after a red the survivors have to rebuild AT ONCE, and
    // making them wait another window would add a full cycle to a queue that is
    // already behind. (CI caught this: the first implementation opened a window
    // on every path into the queue, so survivors stalled until the caller
    // happened to tick.)
    let mut st = lane();
    let build_gen = start_build(
        &mut st,
        vec![
            member("A", "a1", &["src/a.rs"]),
            member("B", "b1", &["src/b.rs"]),
        ],
    );
    // No Tick between the red and the assertion: the rebuild must not depend on
    // one.
    let actions = st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/b.rs", 4, "E0308")],
        },
    });
    assert_eq!(
        started(&actions).as_deref(),
        Some(&["A".to_string()][..]),
        "the survivor rebuilds immediately, with no second capture window"
    );
}

#[test]
fn a_zero_window_builds_immediately() {
    let mut st = LaneState::with_config(
        ROOT,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 0,
            ..Default::default()
        },
    );
    let actions = st.step(LaneEvent::Enqueue(member("A", "a1", &["src/a.rs"])));
    assert_eq!(started(&actions).as_deref(), Some(&["A".to_string()][..]));
    assert_eq!(st.phase(), LanePhase::Building);
}

#[test]
fn an_arrival_during_a_build_queues_and_never_preempts() {
    // THE axiom: a running build is never cancelled. A second arrival must not
    // start a build, must not stop the current one, and must be waiting when
    // the current one finishes.
    let mut st = lane();
    let build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);

    let actions = st.step(LaneEvent::Enqueue(member("B", "b1", &["src/b.rs"])));
    assert!(
        started(&actions).is_none(),
        "a second arrival must not start a build while one is running"
    );
    assert_eq!(st.generation(), build_gen, "generation must not advance");
    assert_eq!(st.queue_depth(), 1);
    assert_eq!(st.in_flight().len(), 1, "the running build is untouched");

    // The in-flight build still gets to publish its verdict.
    let actions = st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Green { artifact: None },
    });
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, LaneAction::LandAndPublish { .. })),
        "the build that was already running still lands"
    );
    assert_eq!(
        started(&actions).as_deref(),
        Some(&["B".to_string()][..]),
        "and B starts immediately after"
    );
}

#[test]
fn a_stale_completion_is_ignored() {
    let mut st = lane();
    let build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);
    let actions = st.step(LaneEvent::BuildFinished {
        generation: build_gen.saturating_sub(1),
        outcome: LaneBuildOutcome::Green { artifact: None },
    });
    assert!(
        actions.is_empty(),
        "a completion for a generation we moved past must do nothing: {actions:?}"
    );
    assert_eq!(
        st.phase(),
        LanePhase::Building,
        "the real build is still live"
    );
}

#[test]
fn max_members_bounds_a_build() {
    let mut st = LaneState::with_config(
        ROOT,
        cargoless_core::lane::LaneConfig {
            max_members: 2,
            ..Default::default()
        },
    );
    st.step(LaneEvent::Enqueue(member("A", "a", &["src/a.rs"])));
    st.step(LaneEvent::Enqueue(member("B", "b", &["src/b.rs"])));
    st.step(LaneEvent::Enqueue(member("C", "c", &["src/c.rs"])));
    assert_eq!(st.in_flight().len(), 2, "cap respected");
    assert_eq!(st.queue_depth(), 1, "the rest ride the next build");
}

// ── attribution ───────────────────────────────────────────────────────────

#[test]
fn single_owner_red_ejects_only_that_member_and_rebuilds_the_rest_at_once() {
    let mut st = lane();
    let build_gen = start_build(
        &mut st,
        vec![
            member("A", "a1", &["src/a.rs"]),
            member("B", "b1", &["src/b.rs"]),
            member("C", "c1", &["src/c.rs"]),
        ],
    );
    let actions = st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/b.rs", 12, "E0308")],
        },
    });

    assert_eq!(ejected_ids(&actions), vec!["B".to_string()]);
    match eject_reason(&actions, "B") {
        EjectReason::Attributed {
            files, shared_with, ..
        } => {
            assert!(files.iter().any(|f| f.ends_with("src/b.rs")));
            assert!(
                shared_with.is_empty(),
                "sole owner is not told it is shared"
            );
        }
        other => panic!("expected Attributed, got {other:?}"),
    }
    // The survivors do not wait for a confirmation build — the next build IS
    // the verification.
    assert_eq!(
        started(&actions).as_deref(),
        Some(&["A".to_string(), "C".to_string()][..]),
        "survivors rebuild immediately"
    );
}

#[test]
fn multi_owner_red_ejects_all_implicated_and_tells_each_it_is_shared() {
    // The operator's rule: when several PRs touch the failing code we must not
    // pick one. Everyone implicated is held AND told the failure is shared, so
    // nobody assumes it is someone else's problem.
    let mut st = lane();
    let build_gen = start_build(
        &mut st,
        vec![
            member("A", "a1", &["src/shared.rs"]),
            member("B", "b1", &["src/shared.rs"]),
            member("C", "c1", &["src/c.rs"]),
        ],
    );
    let actions = st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/shared.rs", 3, "E0425")],
        },
    });

    let mut ej = ejected_ids(&actions);
    ej.sort();
    assert_eq!(ej, vec!["A".to_string(), "B".to_string()]);

    for (who, other) in [("A", "B"), ("B", "A")] {
        match eject_reason(&actions, who) {
            EjectReason::Attributed { shared_with, .. } => {
                assert_eq!(shared_with, vec![other.to_string()]);
            }
            o => panic!("expected Attributed for {who}, got {o:?}"),
        }
        let text = eject_reason(&actions, who).describe();
        assert!(
            text.contains("SHARED") && text.contains(other),
            "{who} must be told the failure is shared with {other}: {text}"
        );
    }
    assert_eq!(
        started(&actions).as_deref(),
        Some(&["C".to_string()][..]),
        "the uninvolved member rebuilds immediately"
    );
}

#[test]
fn unattributable_red_holds_everyone_and_says_so() {
    // Errors in a file nobody touched: an interaction, or a base red. Picking a
    // culprit here would eject someone innocent.
    let mut st = lane();
    let build_gen = start_build(
        &mut st,
        vec![
            member("A", "a1", &["src/a.rs"]),
            member("B", "b1", &["src/b.rs"]),
        ],
    );
    let actions = st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/untouched.rs", 99, "E0433")],
        },
    });

    let mut ej = ejected_ids(&actions);
    ej.sort();
    assert_eq!(
        ej,
        vec!["A".to_string(), "B".to_string()],
        "everyone is held"
    );
    for who in ["A", "B"] {
        let reason = eject_reason(&actions, who);
        assert!(
            matches!(reason, EjectReason::Unattributed { .. }),
            "{who} must not be blamed: {reason:?}"
        );
        let text = reason.describe();
        assert!(
            text.contains("could not be attributed") && text.contains("checked properly"),
            "the message must be explicit that nobody was blamed and all must \
             check: {text}"
        );
    }
    assert!(
        started(&actions).is_none(),
        "nothing is eligible, so no build starts"
    );
}

#[test]
fn warnings_never_eject_anyone() {
    let mut st = lane();
    let build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);
    let mut warn = err("src/a.rs", 4, "unused_imports");
    warn.severity = Severity::Warning;
    let actions = st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![warn],
        },
    });
    // A red with only warnings attributes to nobody — it must fall to the
    // honest "cannot attribute" path, not silently blame the only member.
    assert!(matches!(
        eject_reason(&actions, "A"),
        EjectReason::Unattributed { .. }
    ));
}

#[test]
fn infra_failure_is_not_a_code_red() {
    // A transient must never read as a code red, or people learn to bypass the
    // gate. Members stay queued and ride the next build.
    let mut st = lane();
    let build_gen = start_build(
        &mut st,
        vec![
            member("A", "a1", &["src/a.rs"]),
            member("B", "b1", &["src/b.rs"]),
        ],
    );
    let actions = st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Infra {
            reason: "runner vanished".to_string(),
        },
    });
    assert!(ejected_ids(&actions).is_empty(), "nobody is ejected");
    // The retry is DEFERRED, not immediate. This assertion used to require the
    // rebuild to start in the same step; that is precisely the hot loop that
    // shipped — see `an_infra_failure_does_not_retry_before_its_backoff`.
    assert!(
        started(&actions).is_none(),
        "the retry waits for the backoff rather than restarting instantly"
    );

    // Once the backoff elapses the SAME members rebuild, in their original
    // order. That is the half of the old assertion that was always right: an
    // infra failure must not cost anyone their place.
    let actions = st.step(LaneEvent::Tick {
        now: WINDOW + INFRA_BACKOFF + 1,
    });
    assert_eq!(
        started(&actions).as_deref(),
        Some(&["A".to_string(), "B".to_string()][..]),
        "the same members retry, in order, once the backoff lapses"
    );
}

#[test]
fn the_infra_retry_constants_match_the_shipped_defaults() {
    // The tests above hardcode the backoff and attempt cap so they can assert
    // exact boundaries. That is only sound while the constants agree with what
    // actually ships: if a default were raised and these were not, every
    // "one tick before the backoff" assertion would land somewhere arbitrary
    // and the boundary checks would stop meaning anything — passing, silently,
    // on a policy they no longer describe.
    let cfg = cargoless_core::lane::LaneConfig::default();
    assert_eq!(
        cfg.infra_backoff_ticks, INFRA_BACKOFF,
        "INFRA_BACKOFF must track LaneConfig::default()"
    );
    assert_eq!(
        cfg.infra_max_attempts, INFRA_MAX_ATTEMPTS,
        "INFRA_MAX_ATTEMPTS must track LaneConfig::default()"
    );
    // A zero backoff IS the hot loop, so the default must never be zero — that
    // is the whole point of the field existing.
    assert!(
        cfg.infra_backoff_ticks > 0,
        "a zero infra backoff reintroduces the retry storm"
    );
    assert!(
        cfg.infra_max_attempts > 0,
        "a zero attempt cap would eject on the first transient failure"
    );
}

#[test]
fn an_infra_failure_does_not_retry_before_its_backoff() {
    // THE HOT-LOOP REGRESSION. Without a backoff the requeued members are
    // eligible again the instant the failure is reported, so the lane rebuilds
    // as fast as the failure returns. Observed in the first real deployment at
    // roughly one candidate attempt every 2.5 seconds, indefinitely, while
    // `GET /lane` showed a steady `phase=building` — indistinguishable from a
    // slow compile, which is why it ran unnoticed.
    let mut st = lane();
    let build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);
    let actions = st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Infra {
            reason: "candidate tree could not be materialized".to_string(),
        },
    });
    assert!(
        started(&actions).is_none(),
        "a rebuild must NOT start in the same step as the failure"
    );

    // Still nothing one tick before the backoff expires. Asserting the
    // boundary, not just "eventually", is what makes this catch a backoff that
    // is present but effectively zero.
    let actions = st.step(LaneEvent::Tick {
        now: WINDOW + INFRA_BACKOFF - 1,
    });
    assert!(
        started(&actions).is_none(),
        "no rebuild before the backoff elapses"
    );

    let actions = st.step(LaneEvent::Tick {
        now: WINDOW + INFRA_BACKOFF + 1,
    });
    assert_eq!(
        started(&actions).as_deref(),
        Some(&["A".to_string()][..]),
        "and it does rebuild once the backoff has elapsed"
    );
}

#[test]
fn a_persistent_infra_failure_stops_retrying_and_ejects() {
    // Retrying forever assumes every infra failure is transient. Some are
    // permanent from the lane's side — the deployment that surfaced this could
    // not reach the members' head commits at all, so no amount of waiting would
    // ever have produced a build. The lane must give up, say why, and let the
    // queue move.
    let mut st = lane();
    let mut now = WINDOW;
    let mut build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);

    // One short of the cap: still retrying, nobody ejected.
    for attempt in 1..INFRA_MAX_ATTEMPTS {
        let actions = st.step(LaneEvent::BuildFinished {
            generation: build_gen,
            outcome: LaneBuildOutcome::Infra {
                reason: "member `A` (a1) could not be merged onto the candidate".to_string(),
            },
        });
        assert!(
            ejected_ids(&actions).is_empty(),
            "attempt {attempt} of {INFRA_MAX_ATTEMPTS} must not eject yet"
        );
        now += INFRA_BACKOFF + 1;
        let actions = st.step(LaneEvent::Tick { now });
        build_gen = st.generation();
        assert!(
            started(&actions).is_some(),
            "attempt {attempt} must retry after its backoff"
        );
    }

    // The cap. Now it ejects rather than starting an endless attempt N+1.
    let actions = st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Infra {
            reason: "member `A` (a1) could not be merged onto the candidate".to_string(),
        },
    });
    assert_eq!(
        ejected_ids(&actions),
        vec!["A".to_string()],
        "a persistently-failing candidate is ejected once the attempt cap is reached"
    );
    assert!(
        started(&actions).is_none(),
        "and no further build is started"
    );

    // Ejected as INFRASTRUCTURE, not as Unattributed. The distinction is the
    // product surface: unattributed says "your tree is red and we cannot tell
    // whose change did it", which would send an author hunting a bug that was
    // never diagnosed. Nothing compiled here, so nothing was judged.
    let reason = eject_reason(&actions, "A");
    assert!(
        matches!(reason, EjectReason::Infrastructure { .. }),
        "an infra ejection must not masquerade as a code verdict: {reason:?}"
    );
    // Borrow rather than destructure by value: `reason` is used again below for
    // `describe()` and `fingerprints()`, and moving the `String` out here would
    // leave it partially moved.
    let EjectReason::Infrastructure {
        reason: why,
        attempts,
        ..
    } = &reason
    else {
        unreachable!("just asserted the variant")
    };
    assert_eq!(
        *attempts, INFRA_MAX_ATTEMPTS,
        "it reports how many it tried"
    );
    assert!(
        why.contains("could not be merged"),
        "and carries the build's own words so an operator can fix the cause: {why}"
    );
    assert!(
        reason
            .describe()
            .contains("NOT a verdict about your change"),
        "the author-facing sentence must say plainly that their code was not judged"
    );
    assert!(
        reason.fingerprints().is_empty(),
        "no build ran, so there are no error fingerprints to report"
    );
}

#[test]
fn a_green_build_clears_the_infra_failure_streak() {
    // The counter must be about CONSECUTIVE failures. If it accumulated across
    // a working build, a lane that hit one transient every so often would
    // eventually eject a perfectly good member for reasons spread over hours.
    let mut st = lane();
    let mut now = WINDOW;
    let mut build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);

    for _ in 0..INFRA_MAX_ATTEMPTS - 1 {
        st.step(LaneEvent::BuildFinished {
            generation: build_gen,
            outcome: LaneBuildOutcome::Infra {
                reason: "transient".to_string(),
            },
        });
        now += INFRA_BACKOFF + 1;
        st.step(LaneEvent::Tick { now });
        build_gen = st.generation();
    }

    // A green build in the middle of the streak.
    st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Green { artifact: None },
    });

    // Now the streak must start over: another near-cap run of failures still
    // does not eject. If the counter had survived the green, this would.
    now += 1;
    st.step(LaneEvent::Enqueue(member("B", "b1", &["src/b.rs"])));
    now += WINDOW + 1;
    st.step(LaneEvent::Tick { now });
    let mut build_gen = st.generation();
    for attempt in 0..INFRA_MAX_ATTEMPTS - 1 {
        let actions = st.step(LaneEvent::BuildFinished {
            generation: build_gen,
            outcome: LaneBuildOutcome::Infra {
                reason: "transient".to_string(),
            },
        });
        assert!(
            ejected_ids(&actions).is_empty(),
            "the streak restarted after the green, so attempt {attempt} must not eject"
        );
        now += INFRA_BACKOFF + 1;
        st.step(LaneEvent::Tick { now });
        build_gen = st.generation();
    }
}

#[test]
fn green_lands_once_with_every_member() {
    let mut st = lane();
    let build_gen = start_build(
        &mut st,
        vec![
            member("A", "a1", &["src/a.rs"]),
            member("B", "b1", &["src/b.rs"]),
        ],
    );
    let actions = st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Green {
            artifact: Some("sha256:abc".to_string()),
        },
    });
    let lands: Vec<_> = actions
        .iter()
        .filter_map(|a| match a {
            LaneAction::LandAndPublish { members, artifact } => Some((members, artifact)),
            _ => None,
        })
        .collect();
    assert_eq!(lands.len(), 1, "exactly one land per green build");
    let (members, artifact) = lands[0];
    assert_eq!(
        members.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["A", "B"]
    );
    assert_eq!(artifact.as_deref(), Some("sha256:abc"));
    assert_eq!(st.phase(), LanePhase::Idle);
}

// ── re-admission ──────────────────────────────────────────────────────────

#[test]
fn attributed_ejection_lifts_only_when_a_failing_file_changes() {
    let mut st = lane();
    let build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);
    st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/a.rs", 7, "E0308")],
        },
    });
    assert!(st.ejection("A").is_some());

    // A push that cannot possibly fix the error must not buy a build slot.
    let actions = st.step(LaneEvent::HeadMoved {
        id: "A".to_string(),
        head: "a2".to_string(),
        changed_files: vec!["README.md".to_string()],
    });
    assert!(
        st.ejection("A").is_some(),
        "a README edit does not clear an ejection"
    );
    assert!(started(&actions).is_none());

    // A push touching the failing file does.
    let actions = st.step(LaneEvent::HeadMoved {
        id: "A".to_string(),
        head: "a3".to_string(),
        changed_files: vec!["src/a.rs".to_string()],
    });
    assert!(st.ejection("A").is_none(), "readmitted");
    assert_eq!(started(&actions).as_deref(), Some(&["A".to_string()][..]));
}

#[test]
fn unattributed_ejection_lifts_on_any_push() {
    // We could not identify the fault, so gating on files we are unsure about
    // would strand someone whose fix lives elsewhere.
    let mut st = lane();
    let build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);
    st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/nobody-touched.rs", 1, "E0433")],
        },
    });
    assert!(matches!(
        st.ejection("A").map(|e| e.reason.clone()),
        Some(EjectReason::Unattributed { .. })
    ));

    let actions = st.step(LaneEvent::HeadMoved {
        id: "A".to_string(),
        head: "a2".to_string(),
        changed_files: vec!["docs/unrelated.md".to_string()],
    });
    assert!(
        st.ejection("A").is_none(),
        "an unattributed ejection must never strand anyone"
    );
    assert!(started(&actions).is_some());
}

#[test]
fn re_enqueue_at_the_same_head_does_not_clear_an_ejection() {
    // Nothing about the candidate changed, so it must not buy a build slot.
    let mut st = lane();
    let build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);
    st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/a.rs", 7, "E0308")],
        },
    });
    let actions = st.step(LaneEvent::Enqueue(member("A", "a1", &["src/a.rs"])));
    assert!(st.ejection("A").is_some());
    assert!(started(&actions).is_none());
}

/// The escape hatch. `POST /lane/readmit` exists for a fix the attribution
/// cannot see — a dependency bump, a toolchain change, a red that was never the
/// member's fault. It lifts the ejection without asking for evidence.
#[test]
fn a_forced_readmission_lifts_an_ejection_the_attribution_would_keep() {
    let mut st = lane();
    let build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);
    st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/a.rs", 7, "E0308")],
        },
    });
    assert!(st.ejection("A").is_some(), "precondition: A is ejected");

    let actions = st.step(LaneEvent::ForceReadmit {
        id: "A".to_string(),
    });
    assert!(st.ejection("A").is_none(), "the ejection must be lifted");
    assert_eq!(
        started(&actions),
        Some(vec!["A".to_string()]),
        "and A must be back in a build"
    );
}

/// The subtle half. A forced re-admission must restore the member's changed
/// files, or it comes back **unattributable**: a red it causes would land as
/// "could not attribute" and hold the whole queue instead of ejecting it again.
/// Silently laundering a member out of accountability is worse than refusing
/// the re-admission.
#[test]
fn a_forced_readmission_keeps_the_member_attributable() {
    let mut st = lane();
    let g1 = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);
    st.step(LaneEvent::BuildFinished {
        generation: g1,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/a.rs", 7, "E0308")],
        },
    });
    st.step(LaneEvent::ForceReadmit {
        id: "A".to_string(),
    });

    // Same failure again. If the changed set survived, A owns it and is
    // ejected as Attributed rather than sliding into Unattributed.
    let g2 = st.generation();
    let actions = st.step(LaneEvent::BuildFinished {
        generation: g2,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/a.rs", 7, "E0308")],
        },
    });
    assert_eq!(ejected_ids(&actions), vec!["A".to_string()]);
    assert!(
        matches!(eject_reason(&actions, "A"), EjectReason::Attributed { .. }),
        "a re-admitted member must still be blameable for the file it changed"
    );
}

/// Force-readmitting something that is not ejected reports rather than
/// silently succeeding — an operator who typos an id must not believe they
/// unblocked something.
#[test]
fn a_forced_readmission_of_an_unejected_member_says_so() {
    let mut st = lane();
    let actions = st.step(LaneEvent::ForceReadmit {
        id: "nobody".to_string(),
    });
    assert!(started(&actions).is_none());
    assert!(
        actions.iter().any(|a| matches!(
            a,
            LaneAction::Report { id, state }
                if id == "nobody" && state.contains("not ejected")
        )),
        "expected a Report explaining nothing was re-admitted; got {actions:?}"
    );
}

#[test]
fn ttl_expiry_readmits_as_a_backstop() {
    let mut st = LaneState::with_config(
        ROOT,
        cargoless_core::lane::LaneConfig {
            eject_ttl_ticks: 10,
            ..Default::default()
        },
    );
    let build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);
    st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/a.rs", 7, "E0308")],
        },
    });
    assert!(st.ejection("A").is_some());

    // `start_build` ticks the clock to WINDOW to close the capture window, so
    // the ejection was created at now=WINDOW and expires at WINDOW + ttl.
    // Ticking to a smaller absolute time would move the clock BACKWARDS.
    st.step(LaneEvent::Tick { now: WINDOW + 5 });
    assert!(st.ejection("A").is_some(), "not yet");

    let actions = st.step(LaneEvent::Tick { now: WINDOW + 11 });
    assert!(st.ejection("A").is_none(), "TTL lapsed");
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, LaneAction::Readmit { .. })),
        "expiry is reported, never silent"
    );

    // AND THE MEMBER IS ACTUALLY BACK. Leaving `ejected` is not the same as
    // rejoining the queue, and this test used to assert only the former —
    // so a TTL expiry that dropped the member entirely passed it.
    //
    // That shipped. On 2026-08-02 three members ejected `infrastructure` by a
    // preview outage hit their TTL and vanished: `queue_depth: 0`, nothing
    // building, and `GET /lane` showing no trace that anything had been lost.
    // A backstop that discards the member it was protecting is worse than no
    // backstop, because the log says "re-admitted" either way.
    assert_eq!(
        st.queue_depth() + st.in_flight().len(),
        1,
        "a lapsed ejection must put the member BACK IN THE LANE, not merely \
         out of `ejected`: {actions:?}"
    );
    // Intact, not a husk. Ticking past the capture window starts the build,
    // which is what puts the member in `in_flight` where its fields are
    // readable — `LaneState` deliberately exposes no `queue()`, and widening
    // that surface for a test would be the wrong trade.
    //
    // This matters: without `head` the candidate cannot be built, and without
    // `changed_files` every future red it rides is unattributable and holds
    // the whole queue instead of ejecting one member.
    st.step(LaneEvent::Tick {
        now: WINDOW + 11 + WINDOW,
    });
    let back = st
        .in_flight()
        .iter()
        .find(|m| m.id == "A")
        .expect("the readmitted member rebuilds");
    assert_eq!(back.head, "a1", "the readmitted member keeps its head");
    assert_eq!(
        back.changed_files,
        vec!["src/a.rs".to_string()],
        "the readmitted member keeps its changed set — losing it makes every \
         later red it rides unattributable"
    );
}

#[test]
fn the_clock_never_rewinds() {
    // `Tick.now` is absolute. A caller passing a smaller value — a restarted
    // counter, a test reusing a literal — must not rewind time and resurrect an
    // ejection that already lapsed, which would hold a member past its TTL with
    // nothing to show why.
    let mut st = LaneState::with_config(
        ROOT,
        cargoless_core::lane::LaneConfig {
            eject_ttl_ticks: 10,
            capture_window_ticks: 0,
            ..Default::default()
        },
    );
    let build_gen = start_build_now(&mut st, vec![member("A", "a1", &["src/a.rs"])]);
    st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/a.rs", 7, "E0308")],
        },
    });
    st.step(LaneEvent::Tick { now: 100 });
    assert!(st.ejection("A").is_none(), "lapsed at 100");

    // Going backwards must not undo it.
    st.step(LaneEvent::Tick { now: 1 });
    assert!(
        st.ejection("A").is_none(),
        "a backwards tick must not resurrect a lapsed ejection"
    );
}

#[test]
fn withdraw_removes_a_member_and_its_ejection() {
    let mut st = lane();
    let build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);
    st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/a.rs", 7, "E0308")],
        },
    });
    st.step(LaneEvent::Withdraw {
        id: "A".to_string(),
    });
    assert!(st.ejection("A").is_none());
    assert_eq!(st.queue_depth(), 0);
}

/// A member withdrawn WHILE ITS BUILD RUNS must not come back when that build
/// ends.
///
/// `Withdraw` originally cleared only `queue` and `ejected`, which reads as
/// complete and is not: `on_build_finished` takes `in_flight` and, on any
/// non-green outcome, requeues whatever it finds there. So the member returned
/// at the end of the very build it was withdrawn from — minutes later, when
/// nobody is looking, and in exactly the case the verb exists for.
///
/// Observed 2026-08-03: pr-10394 is red on a REQUIRED forge check, so it can
/// never merge; the lane rebuilt it three times because there was no way to
/// take it out.
#[test]
fn withdraw_during_a_build_survives_the_builds_end() {
    let mut st = lane();
    let build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);

    st.step(LaneEvent::Withdraw {
        id: "A".to_string(),
    });

    // The build is NOT cancelled — the compile is already paid for, and killing
    // it would deny a verdict to any other member aboard.
    assert_eq!(st.phase(), LanePhase::Building, "the build keeps running");

    st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/a.rs", 7, "E0308")],
        },
    });

    assert_eq!(
        st.queue_depth(),
        0,
        "a withdrawn member must NOT be requeued by the build it was withdrawn from"
    );
    assert!(
        st.ejection("A").is_none(),
        "nor should it acquire an ejection — it is gone, not blamed"
    );
}

/// Withdrawing one of several keeps the rest — the build still belongs to them.
#[test]
fn withdraw_leaves_the_other_members_of_the_running_build() {
    let mut st = lane();
    let build_gen = start_build(
        &mut st,
        vec![
            member("A", "a1", &["src/a.rs"]),
            member("B", "b1", &["src/b.rs"]),
        ],
    );

    st.step(LaneEvent::Withdraw {
        id: "A".to_string(),
    });
    st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/b.rs", 3, "E0425")],
        },
    });

    // B caused the red and is ejected for it; A is simply absent.
    assert!(st.ejection("B").is_some(), "B still owns the red it caused");
    assert!(st.ejection("A").is_none(), "A was withdrawn, not blamed");
}

// ── attribution identity ──────────────────────────────────────────────────

#[test]
fn ejection_identity_survives_a_line_shift() {
    // attribution.rs omits line/col from the fingerprint on purpose: inserting
    // lines above an error shifts every line below it, and a line-keyed identity
    // would report all of them as new. This pins that the lane inherits that
    // property rather than reintroducing line sensitivity.
    let mut st = lane();
    let build_gen = start_build(&mut st, vec![member("A", "a1", &["src/a.rs"])]);
    st.step(LaneEvent::BuildFinished {
        generation: build_gen,
        outcome: LaneBuildOutcome::Red {
            diagnostics: vec![err("src/a.rs", 7, "E0308")],
        },
    });
    let before = match st.ejection("A").map(|e| e.reason.clone()) {
        Some(EjectReason::Attributed { fingerprints, .. }) => fingerprints,
        other => panic!("expected Attributed, got {other:?}"),
    };

    let mut st2 = lane();
    let gen2 = start_build(&mut st2, vec![member("A", "a1", &["src/a.rs"])]);
    st2.step(LaneEvent::BuildFinished {
        generation: gen2,
        outcome: LaneBuildOutcome::Red {
            // SAME error, moved down 200 lines by an unrelated insertion.
            diagnostics: vec![err("src/a.rs", 207, "E0308")],
        },
    });
    let after = match st2.ejection("A").map(|e| e.reason.clone()) {
        Some(EjectReason::Attributed { fingerprints, .. }) => fingerprints,
        other => panic!("expected Attributed, got {other:?}"),
    };

    assert_eq!(
        before, after,
        "the same error at a different line is the SAME ejection identity; \
         adding line back to the fingerprint would break this"
    );
}

#[test]
fn absolute_scratch_paths_attribute_to_repo_relative_changed_files() {
    // The build runs in a scratch worktree, so diagnostics carry a prefix the
    // member never sees. If this ever regressed, EVERY red would read as
    // unattributable and the lane would hold the whole queue on every failure.
    let m = member("A", "a1", &["portal/src/page.rs"]);
    assert!(m.touches(&PathBuf::from("/scratch/deadbeef/portal/src/page.rs")));
    assert!(!m.touches(&PathBuf::from("/scratch/deadbeef/portal/src/other.rs")));
    // A suffix that is not a path-component boundary must NOT match.
    assert!(!m.touches(&PathBuf::from("/scratch/x/notportal/src/page.rs")));
}
