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
    assert_eq!(
        started(&actions).as_deref(),
        Some(&["A".to_string(), "B".to_string()][..]),
        "the same members retry, in order"
    );
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
