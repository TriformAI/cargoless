//! Reusability proof — the lane must work for a project that is not this one.
//!
//! The whole justification for building the lane inside cargoless, rather than
//! as glue in the fleet that needed it, is that any Rust/Leptos project should
//! get it by declaring a few lines of `cargoless.checks.yaml`. That claim is
//! easy to make and easy to quietly break: one hard-coded path, one assumption
//! about a forge, one leg the daemon supplies itself, and the lane silently
//! becomes single-tenant.
//!
//! So this test drives the lane end to end against a synthetic project with:
//!
//! * its own root, nowhere near this repo,
//! * its own build legs, declared not built in,
//! * **zero** knowledge of any forge, PR, or branch protection.
//!
//! If it takes more than a config file and a `LaneLander` to ship a change, the
//! goal has failed and the design — not this test — needs revisiting.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};

use cargoless_core::lane::{LaneAction, LaneEvent, LaneMember, LaneState};
use cargoless_core::lanedrv::{
    CandidateTree, LandOutcome, LaneDriver, LaneLander, LegOutcome, LegRunner, MaterializeError,
};
use cargoless_proto::{Diagnostic, Severity, TreeState};

/// A project root that is emphatically not this repo.
const APP_ROOT: &str = "/tmp/some-leptos-app";

/// The candidate tree a real driver would materialise with
/// `git worktree add --detach <scratch> <base>` plus the members' overlays.
/// Here it just reports where it would be — the lane never inspects it.
struct FakeTree {
    materialized: RefCell<Vec<Vec<String>>>,
}

impl CandidateTree for FakeTree {
    fn materialize(&self, members: &[LaneMember]) -> Result<PathBuf, MaterializeError> {
        self.materialized
            .borrow_mut()
            .push(members.iter().map(|m| m.id.clone()).collect());
        Ok(PathBuf::from(APP_ROOT).join(".cargoless/candidate"))
    }
}

/// Stands in for the project's own legs — whatever `cargoless.checks.yaml`
/// declares. A real one shells out to `trunk build` or `cargo build --release`.
struct ScriptedLegs {
    outcomes: RefCell<Vec<LegOutcome>>,
    ran_in: RefCell<Vec<PathBuf>>,
}

impl LegRunner for ScriptedLegs {
    fn run(&self, root: &Path, _changed: &[String]) -> io::Result<LegOutcome> {
        self.ran_in.borrow_mut().push(root.to_path_buf());
        Ok(self.outcomes.borrow_mut().pop().unwrap_or(LegOutcome {
            tree: TreeState::Green,
            diagnostics: Vec::new(),
            ..Default::default()
        }))
    }
}

/// The project's own "ship it". For a single app this is the pointer swap
/// cargoless already does; for a fleet it is a CAS push plus an image promote.
/// The lane does not care which — that is the point.
#[derive(Default)]
struct RecordingLander {
    landed: RefCell<Vec<Vec<String>>>,
    /// What the lander was actually handed to publish.
    ///
    /// Recorded because ignoring it is how the driver got away with dropping
    /// the artifact: every test asserted WHO landed and none asserted WHAT
    /// shipped, so a lane that published nothing looked healthy.
    published: RefCell<Vec<Option<String>>>,
}

impl LaneLander for RecordingLander {
    fn land(&self, members: &[LaneMember], artifact: Option<&str>) -> io::Result<LandOutcome> {
        self.landed
            .borrow_mut()
            .push(members.iter().map(|m| m.id.clone()).collect());
        self.published
            .borrow_mut()
            .push(artifact.map(str::to_string));
        Ok(LandOutcome {
            detail: "pointer advanced".to_string(),
        })
    }
}

fn err_in(path: &str) -> Diagnostic {
    Diagnostic {
        // Absolute, under the candidate root — exactly the shape a real build
        // in a scratch worktree produces.
        file_path: PathBuf::from(APP_ROOT)
            .join(".cargoless/candidate")
            .join(path),
        line: 12,
        col: 5,
        severity: Severity::Error,
        code: Some("E0308".to_string()),
        message: format!("mismatched types in {path}"),
        source: Some("rustc".to_string()),
    }
}

fn driver(outcomes: Vec<LegOutcome>) -> LaneDriver<FakeTree, ScriptedLegs, RecordingLander> {
    LaneDriver::new(
        FakeTree {
            materialized: RefCell::new(Vec::new()),
        },
        ScriptedLegs {
            // popped from the back, so callers list them in reverse.
            outcomes: RefCell::new(outcomes),
            ran_in: RefCell::new(Vec::new()),
        },
        RecordingLander::default(),
    )
}

fn lane() -> LaneState {
    LaneState::with_config(
        APP_ROOT,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 0,
            ..Default::default()
        },
    )
}

#[test]
fn a_third_party_project_can_ship_a_change_through_the_lane() {
    // One green build: enqueue, build, land. No forge, no PR, no branch
    // protection anywhere in the loop.
    let drv = driver(vec![LegOutcome {
        tree: TreeState::Green,
        diagnostics: Vec::new(),
        ..Default::default()
    }]);
    let mut lane = lane();

    let actions = drv.pump(
        &mut lane,
        LaneEvent::Enqueue(
            LaneMember::new("feature-x", "sha-1").with_changed_files(["src/app.rs"]),
        ),
    );

    assert_eq!(
        drv.tree.materialized.borrow().as_slice(),
        &[vec!["feature-x".to_string()]],
        "the candidate is materialised from the members, once"
    );
    assert_eq!(
        drv.legs.ran_in.borrow().as_slice(),
        &[PathBuf::from(APP_ROOT).join(".cargoless/candidate")],
        "the project's own legs run IN the candidate tree, not the source tree"
    );
    assert_eq!(
        drv.lander.landed.borrow().as_slice(),
        &[vec!["feature-x".to_string()]],
        "green ⇒ the project's lander ships it"
    );
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, LaneAction::LandAndPublish { .. })),
        "the land action is reported, not just performed"
    );
}

#[test]
fn a_red_ejects_the_author_and_ships_everyone_else() {
    // Two members; the second's file fails. Outcomes pop from the back, so:
    // first build red, second build green.
    let drv = driver(vec![
        LegOutcome {
            tree: TreeState::Green,
            diagnostics: Vec::new(),
            ..Default::default()
        },
        LegOutcome {
            tree: TreeState::Red,
            diagnostics: vec![err_in("src/broken.rs")],
            ..Default::default()
        },
    ]);
    // A capture window so both members ride ONE build — the production shape,
    // and what makes the "eject one, ship the other" claim meaningful.
    let mut lane = LaneState::with_config(
        APP_ROOT,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 5,
            ..Default::default()
        },
    );
    lane.step(LaneEvent::Enqueue(
        LaneMember::new("good", "g1").with_changed_files(["src/app.rs"]),
    ));
    lane.step(LaneEvent::Enqueue(
        LaneMember::new("bad", "b1").with_changed_files(["src/broken.rs"]),
    ));
    let actions = drv.pump(&mut lane, LaneEvent::Tick { now: 5 });

    let ejected: Vec<&str> = actions
        .iter()
        .filter_map(|a| match a {
            LaneAction::Eject { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ejected, vec!["bad"], "only the author of the failing file");

    assert_eq!(
        drv.lander.landed.borrow().as_slice(),
        &[vec!["good".to_string()]],
        "the innocent member ships in the very next build — no waiting for the \
         ejected one to be fixed"
    );
}

#[test]
fn the_default_lander_publishes_and_never_erases_a_previous_green() {
    use cargoless_core::lanedrv::PointerLander;

    let root = std::env::temp_dir().join(format!(
        "cargoless-lane-pointer-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let lander = PointerLander::new(&root);
    let members = vec![LaneMember::new("m1", "s1")];

    // A green build WITH an artifact advances the pointer.
    lander
        .land(&members, Some("artifact-payload-v1"))
        .expect("publish");
    assert_eq!(
        std::fs::read_to_string(lander.pointer_path()).unwrap(),
        "artifact-payload-v1",
        "the pointer carries exactly what the build published"
    );

    // A green build with NO artifact must leave it byte-untouched. A
    // check-only lane proves the tree compiles without emitting anything, and
    // treating that as "publish nothing" would erase the last real green — a
    // silent rollback dressed up as a success.
    lander.land(&members, None).expect("green, nothing to ship");
    assert_eq!(
        std::fs::read_to_string(lander.pointer_path()).unwrap(),
        "artifact-payload-v1",
        "a green-with-no-artifact must NOT erase the previous green"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The gap this closes: the driver used to hardcode `Green { artifact: None }`,
/// so `PointerLander` always took its "nothing to publish" branch and the
/// pointer NEVER advanced. Every lander test passed — the lander was correct —
/// and the lane still published nothing, which looks exactly like a working
/// lane until someone reads the pointer.
///
/// So this asserts the seam rather than either side of it: an artifact
/// reported by the legs must reach the lander.
#[test]
fn an_artifact_from_the_legs_reaches_the_lander() {
    let drv = driver(vec![LegOutcome {
        tree: TreeState::Green,
        artifact: Some("published-payload".to_string()),
        ..Default::default()
    }]);
    let mut lane = lane();

    drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("m1", "sha1").with_changed_files(["src/lib.rs"])),
    );

    assert_eq!(
        drv.lander.published.borrow().as_slice(),
        &[Some("published-payload".to_string())],
        "a green build's artifact must be handed to the lander, not dropped"
    );
}

/// The other direction: a check-only lane reports no artifact, and the lander
/// must be told `None` rather than an empty string. `Some("")` would advance
/// the pointer to nothing — a silent rollback wearing a success's clothes.
#[test]
fn a_check_only_lane_hands_the_lander_no_artifact() {
    let drv = driver(vec![LegOutcome {
        tree: TreeState::Green,
        ..Default::default()
    }]);
    let mut lane = lane();

    drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("m1", "sha1").with_changed_files(["src/lib.rs"])),
    );

    assert_eq!(
        drv.lander.published.borrow().as_slice(),
        &[None],
        "no artifact must arrive as None, never as an empty payload"
    );
}

#[test]
fn the_lane_needs_no_knowledge_of_a_forge() {
    // The proof-by-construction: a member is (id, head, changed_files) — three
    // strings. Nothing here names a PR, a branch, a remote, or a merge. If a
    // forge concept ever leaks into LaneMember, this stops compiling.
    let m =
        LaneMember::new("anything-you-like", "any-content-id").with_changed_files(["src/lib.rs"]);
    assert_eq!(m.id, "anything-you-like");
    assert_eq!(m.head, "any-content-id");

    // And the only way to ship is a trait with one method.
    fn _accepts_any_lander<L: LaneLander>(_: &L) {}
    _accepts_any_lander(&RecordingLander::default());
}

/// The operator's question: "have you tested it with queueing a few different
/// ones as well?"
///
/// Three members, TWO independent reds in files owned by different members.
/// Both owners must be ejected and the innocent member must ship — the case
/// where a naive implementation ejects the first owner it finds, or gives up
/// and holds everyone.
#[test]
fn three_members_with_two_independent_reds_eject_both_owners_and_ship_the_third() {
    let drv = driver(vec![
        // popped from the back: first build red (two owners), then green.
        LegOutcome {
            tree: TreeState::Green,
            ..Default::default()
        },
        LegOutcome {
            tree: TreeState::Red,
            diagnostics: vec![err_in("src/a_broken.rs"), err_in("src/b_broken.rs")],
            ..Default::default()
        },
    ]);
    let mut lane = LaneState::with_config(
        APP_ROOT,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 5,
            ..Default::default()
        },
    );
    lane.step(LaneEvent::Enqueue(
        LaneMember::new("a", "a1").with_changed_files(["src/a_broken.rs"]),
    ));
    lane.step(LaneEvent::Enqueue(
        LaneMember::new("b", "b1").with_changed_files(["src/b_broken.rs"]),
    ));
    lane.step(LaneEvent::Enqueue(
        LaneMember::new("innocent", "i1").with_changed_files(["src/fine.rs"]),
    ));

    let actions = drv.pump(&mut lane, LaneEvent::Tick { now: 5 });

    let mut ejected: Vec<&str> = actions
        .iter()
        .filter_map(|a| match a {
            LaneAction::Eject { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    ejected.sort_unstable();
    assert_eq!(
        ejected,
        vec!["a", "b"],
        "both owners ejected — not just the first one found"
    );

    assert_eq!(
        drv.lander.landed.borrow().as_slice(),
        &[vec!["innocent".to_string()]],
        "the member who touched neither failing file ships in the very next \
         build, without waiting for either fix"
    );
}

/// A member arriving mid-build must QUEUE, never preempt. On a real lane a
/// build is tens of minutes and a cancelled CI run is permanently not-green,
/// so preempting strands work that was about to pass.
///
/// Driver-level rather than policy-level: this asserts the *driver* does not
/// start a second build, which is the part that would actually burn a runner.
#[test]
fn a_member_arriving_mid_build_queues_and_rides_the_next_build() {
    let drv = driver(vec![
        LegOutcome {
            tree: TreeState::Green,
            ..Default::default()
        },
        LegOutcome {
            tree: TreeState::Green,
            ..Default::default()
        },
    ]);
    let mut lane = lane(); // zero capture window: builds on arrival

    // First member builds and lands immediately.
    drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("first", "f1").with_changed_files(["src/a.rs"])),
    );
    // Second arrives after that build finished; it gets its own build.
    drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("second", "s1").with_changed_files(["src/b.rs"])),
    );

    assert_eq!(
        drv.tree.materialized.borrow().as_slice(),
        &[vec!["first".to_string()], vec!["second".to_string()]],
        "two separate candidates — the second never joined the first's build"
    );
    assert_eq!(
        drv.lander.landed.borrow().as_slice(),
        &[vec!["first".to_string()], vec!["second".to_string()]],
        "and both landed, in order"
    );
}

/// Re-admission after a real fix: an ejected member whose new head touches the
/// file that failed gets back in and ships. Without this the ejection is a
/// dead end rather than a hold.
#[test]
fn an_ejected_member_returns_when_its_fix_touches_the_failing_file() {
    let drv = driver(vec![
        LegOutcome {
            tree: TreeState::Green,
            ..Default::default()
        }, // 3rd: the fix builds green
        LegOutcome {
            tree: TreeState::Green,
            ..Default::default()
        }, // 2nd: survivors rebuild
        LegOutcome {
            tree: TreeState::Red,
            diagnostics: vec![err_in("src/broken.rs")],
            ..Default::default()
        }, // 1st: red
    ]);
    let mut lane = lane();

    drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("author", "v1").with_changed_files(["src/broken.rs"])),
    );
    assert!(
        lane.ejection("author").is_some(),
        "precondition: the author is ejected by the red"
    );

    // A new head that touches the failing file — a plausible fix.
    drv.pump(
        &mut lane,
        LaneEvent::HeadMoved {
            id: "author".to_string(),
            head: "v2".to_string(),
            changed_files: vec!["src/broken.rs".to_string()],
        },
    );

    assert!(lane.ejection("author").is_none(), "the ejection lifted");
    assert!(
        drv.lander
            .landed
            .borrow()
            .iter()
            .any(|ids| ids == &["author".to_string()]),
        "and the fixed change actually shipped: {:?}",
        drv.lander.landed.borrow()
    );
}

/// The other half: a new head that touches nothing implicated must NOT buy a
/// build slot. At tens of minutes per build, a README edit re-entering the
/// queue is how a lane becomes a treadmill.
#[test]
fn an_ejected_member_stays_out_when_its_push_cannot_have_fixed_anything() {
    let drv = driver(vec![LegOutcome {
        tree: TreeState::Red,
        diagnostics: vec![err_in("src/broken.rs")],
        ..Default::default()
    }]);
    let mut lane = lane();

    drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("author", "v1").with_changed_files(["src/broken.rs"])),
    );
    let builds_before = drv.tree.materialized.borrow().len();

    drv.pump(
        &mut lane,
        LaneEvent::HeadMoved {
            id: "author".to_string(),
            head: "v2".to_string(),
            changed_files: vec!["README.md".to_string()],
        },
    );

    assert!(
        lane.ejection("author").is_some(),
        "a push that cannot have fixed the failure must not lift the ejection"
    );
    assert_eq!(
        drv.tree.materialized.borrow().len(),
        builds_before,
        "and must not have started a build"
    );
}
