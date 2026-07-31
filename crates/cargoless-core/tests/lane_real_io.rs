//! The seam where the lane meets a real build.
//!
//! Everything else about the lane is well covered — the policy has 24 unit
//! tests plus a mutation proof, the staging engine has 22 real-subprocess
//! sites, `GitCandidateTree` has real-git tests. But an audit found the join
//! between them untested: `ProfileLegRunner` — the only production `LegRunner`,
//! the one type that turns a candidate tree into a verdict — had **zero** test
//! coverage of any kind, and no test at any level composed two real components.
//!
//! The tested combinations were {real git tree, no legs}, {fake tree, fake
//! legs, real host} and {real subprocesses, no lane}. The shipped combination
//! is {real tree, real legs, real lander, real host} — which nothing exercised.
//!
//! So these tests use no fakes. Real git repo, real `git worktree add`, real
//! merges, real `bash` subprocesses, real diagnostics parsed back out. If the
//! lane is going to fail on its first production candidate, it should fail
//! here first.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use cargoless_core::lane::{LaneEvent, LaneMember, LaneState};
use cargoless_core::lanedrv::{
    LaneDriver, LaneLander, LegRunner, PointerLander, ProfileLegRunner, ReportOnlyLander,
};
use cargoless_core::lanetree::GitCandidateTree;
use cargoless_proto::{Severity, TreeState};

fn sh(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn scratch(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "cargoless-lane-realio-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

/// A repo whose "build" is a shell script, so the whole lane runs end to end
/// without a compiler. `legs` is spliced into cargoless.checks.yaml verbatim.
fn repo_with_legs(tag: &str, legs: &str) -> PathBuf {
    let root = scratch(tag);
    sh(&root, &["init", "-q", "-b", "main"]);
    sh(&root, &["config", "user.name", "t"]);
    sh(&root, &["config", "user.email", "t@t.invalid"]);
    fs::write(
        root.join("cargoless.checks.yaml"),
        format!("version: 1\nprofiles:\n  lane:\n    include: [\"lane\"]\n    timeout_ms: 60000\nchecks:\n{legs}"),
    )
    .unwrap();
    fs::write(root.join("ok.txt"), "fine\n").unwrap();
    sh(&root, &["add", "."]);
    sh(&root, &["commit", "-q", "-m", "base"]);
    root
}

/// Commit `file` on a branch off main; return its sha.
fn branch(root: &Path, name: &str, file: &str, body: &str) -> String {
    sh(root, &["checkout", "-q", "-B", name, "main"]);
    fs::write(root.join(file), body).unwrap();
    sh(root, &["add", file]);
    sh(root, &["commit", "-q", "-m", name]);
    let out = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    sh(root, &["checkout", "-q", "main"]);
    sha
}

fn leg(id: &str, cmd: &str) -> String {
    format!(
        "  - id: \"{id}\"\n    kind: command\n    tier: lane\n    read_only: true\n    \
         timeout_ms: 30000\n    cache: none\n    command: [\"bash\", \"-lc\", {cmd:?}]\n"
    )
}

/// The baseline claim: a real profile, run by the real leg runner, in a real
/// candidate tree, reports green.
#[test]
fn the_real_leg_runner_reports_green_on_a_passing_profile() {
    let root = repo_with_legs("green", &leg("build", "true"));
    let runner = ProfileLegRunner::new("lane");

    let outcome = runner.run(&root, &[]).expect("legs run");

    assert_eq!(outcome.tree, TreeState::Green);
    assert!(
        outcome.diagnostics.is_empty(),
        "a green run reports no diagnostics: {:?}",
        outcome.diagnostics
    );
    assert_eq!(
        outcome.legs.len(),
        1,
        "per-leg reports must survive the LegOutcome boundary — they are what \
         a staged lane shows an operator: {:?}",
        outcome.legs
    );
    assert_eq!(outcome.legs[0].id, "build");
    let _ = fs::remove_dir_all(root);
}

/// A red must arrive with diagnostics carrying REAL file paths. This is the
/// property attribution depends on: a red whose diagnostics point at the
/// manifest instead of a source file is unattributable, and the lane then holds
/// the whole queue instead of ejecting one member.
#[test]
fn a_red_carries_diagnostics_with_real_file_paths() {
    // One JSON object per line — the `cargoless.check-diagnostic/v1` protocol
    // the daemon parses by default (`parse_command_diagnostic_line`). The
    // relative `path` is resolved against the run root, which is what makes the
    // diagnostic attributable to a member's changed file.
    let emit = concat!(
        r#"echo '{"schema":"cargoless.check-diagnostic/v1","path":"src/broken.rs","#,
        r#""line":7,"col":1,"severity":"error","code":"E0308","#,
        r#""message":"mismatched types"}'; exit 1"#
    );
    let root = repo_with_legs("red", &leg("build", emit));
    let runner = ProfileLegRunner::new("lane");

    let outcome = runner.run(&root, &[]).expect("legs run");

    assert_eq!(outcome.tree, TreeState::Red);
    let errs: Vec<_> = outcome
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert!(
        !errs.is_empty(),
        "a red must carry errors: {:?}",
        outcome.diagnostics
    );
    assert!(
        errs.iter().any(|d| d.file_path.ends_with("src/broken.rs")),
        "the failing FILE must survive — without it the lane cannot attribute \
         the red to anyone: {:?}",
        errs.iter().map(|d| &d.file_path).collect::<Vec<_>>()
    );
    let _ = fs::remove_dir_all(root);
}

/// A green build whose legs did not write the declared artifact is INFRA, not
/// a code red. Nobody's change is at fault, so ejecting a member would be a
/// false accusation — the lane holds instead. Untested before this, and the
/// failure mode is a permanently stuck queue.
#[test]
fn a_missing_artifact_on_green_is_infrastructure_not_a_red() {
    let root = repo_with_legs("noartifact", &leg("build", "true"));
    let mut runner = ProfileLegRunner::new("lane");
    runner.artifact_path = Some(PathBuf::from("dist/never-written"));

    let err = runner
        .run(&root, &[])
        .expect_err("a promised-but-absent artifact must not read as success");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    let _ = fs::remove_dir_all(root);
}

/// The composition nothing tested: real tree + real legs + real lander + real
/// policy, driven end to end. Two members ride one candidate, the build is a
/// real subprocess in a real merged worktree, and the pointer really moves.
#[test]
fn the_whole_lane_ships_a_real_candidate_and_advances_the_pointer() {
    // The leg writes the artifact the lander will publish, proving the
    // legs → artifact → lander chain rather than just its ends.
    let root = repo_with_legs(
        "e2e",
        &leg(
            "build",
            "mkdir -p dist && printf 'built-ok' > dist/artifact",
        ),
    );
    let a = branch(&root, "a", "a.txt", "a\n");
    let b = branch(&root, "b", "b.txt", "b\n");

    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let mut legs = ProfileLegRunner::new("lane");
    legs.artifact_path = Some(PathBuf::from("dist/artifact"));
    let lander = PointerLander::new(&root);
    let pointer = lander.pointer_path();
    let drv = LaneDriver::new(tree, legs, lander);

    // Zero capture window so the first arrival builds immediately; the window
    // itself is covered by the policy tests.
    let mut lane = LaneState::with_config(
        &root,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 0,
            ..Default::default()
        },
    );

    drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("a", &a).with_changed_files(["a.txt"])),
    );
    drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("b", &b).with_changed_files(["b.txt"])),
    );

    assert_eq!(
        fs::read_to_string(&pointer).unwrap_or_default(),
        "built-ok",
        "the artifact the legs actually wrote must reach the pointer"
    );
    let _ = fs::remove_dir_all(root);
}

/// The shadow-run shape: same real pipeline, report-only lander, pointer must
/// stay untouched. This is the configuration an operator is asked to start
/// with, so "it really does not publish" is worth asserting rather than
/// assuming.
#[test]
fn a_report_only_lane_builds_for_real_and_publishes_nothing() {
    let root = repo_with_legs(
        "shadow",
        &leg(
            "build",
            "mkdir -p dist && printf 'built-ok' > dist/artifact",
        ),
    );
    let a = branch(&root, "a", "a.txt", "a\n");

    let pointer_probe = PointerLander::new(&root).pointer_path();
    assert!(!pointer_probe.exists(), "precondition: no pointer yet");

    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let mut legs = ProfileLegRunner::new("lane");
    legs.artifact_path = Some(PathBuf::from("dist/artifact"));
    let drv = LaneDriver::new(tree, legs, ReportOnlyLander);

    let mut lane = LaneState::with_config(
        &root,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 0,
            ..Default::default()
        },
    );
    drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("a", &a).with_changed_files(["a.txt"])),
    );

    assert!(
        !pointer_probe.exists(),
        "a report-only lane must not publish — that is the entire point of \
         shadow-running it"
    );
    let _ = fs::remove_dir_all(root);
}

/// The report-only lander still says an artifact EXISTED. Shadow-running is
/// only useful if it tells you the legs really produced something; "green" on
/// its own does not distinguish a real build from a no-op profile.
#[test]
fn report_only_names_the_artifact_it_declined_to_publish() {
    let members = vec![LaneMember::new("m1", "sha1")];
    let out = ReportOnlyLander
        .land(&members, Some("payload"))
        .expect("report-only never fails");
    assert!(
        out.detail.contains("NOT published"),
        "must be explicit that nothing shipped: {}",
        out.detail
    );
    assert!(
        out.detail.contains("7 byte"),
        "must report the artifact size so a shadow run proves the legs produced \
         something: {}",
        out.detail
    );
}

/// THE SHADOW-RUN REGRESSION. The first real shadow build compiled for 76
/// minutes and left no readable verdict anywhere: `GET /lane` reports only
/// current state, and `CandidateTree::release` removes the candidate worktree —
/// and with it the target dir and every artifact — the instant the build ends.
/// Afterwards there was no way to tell green from red from inside the pod, so
/// the comparison the shadow run existed to produce could not be made.
///
/// The assertion that matters is therefore not "a line was written" but "the
/// verdict outlives the tree it was computed from". The test deletes the whole
/// repo before reading, which is strictly harsher than what `release` does.
#[test]
fn the_verdict_and_per_leg_timings_outlive_the_candidate_worktree() {
    let root = repo_with_legs(
        "trail",
        &format!(
            "{}\n{}",
            leg("alpha", "true"),
            leg("beta", "mkdir -p dist && printf 'ok' > dist/artifact")
        ),
    );
    let a = branch(&root, "a", "a.txt", "a\n");

    // Deliberately OUTSIDE the repo: a trail written inside the tree would be
    // destroyed by the very cleanup this test exists to survive.
    let trail = root.with_extension("trail.log");

    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let mut legs = ProfileLegRunner::new("lane");
    legs.artifact_path = Some(PathBuf::from("dist/artifact"));
    // `trail.clone()`, not `&trail`: `with_trail` takes `impl Into<PathBuf>`
    // and `&PathBuf` does not implement it (only `&Path` and `&str` do, via
    // `AsRef`-flavoured impls that `Into` does not cover).
    let drv = LaneDriver::new(tree, legs, ReportOnlyLander).with_trail(trail.clone());

    let mut lane = LaneState::with_config(
        &root,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 0,
            ..Default::default()
        },
    );
    drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("a", &a).with_changed_files(["a.txt"])),
    );

    // Destroy everything the build ran in — harsher than `release`, which only
    // removes the candidate worktree.
    let _ = fs::remove_dir_all(&root);

    let log = fs::read_to_string(&trail).expect("the trail must survive the tree");

    assert!(
        log.contains("lane-build-start generation=1"),
        "the trail must record that a build started, and which generation: {log}"
    );
    assert!(
        log.contains(&format!("a@{a}")),
        "and WHICH members were in it, by id@head — without that a verdict \
         cannot be matched to a candidate: {log}"
    );
    assert!(
        log.contains("outcome=green"),
        "the verdict itself must be readable after the fact: {log}"
    );

    // Per-leg lines are what make the trail comparable against
    // dev-staging-build's phase timings. A single verdict line would say the
    // build was green without saying where the 76 minutes went.
    for id in ["alpha", "beta"] {
        assert!(
            log.contains(&format!("lane-leg generation=1 id={id}")),
            "every leg must appear by id, not just the rolled-up verdict: {log}"
        );
    }
    assert!(
        log.contains("elapsed_ms="),
        "each leg must carry its own duration: {log}"
    );

    let _ = fs::remove_file(&trail);
}

/// A trail is evidence ABOUT a build, never a precondition FOR one. If an
/// unwritable path could fail a build, the observability would itself be the
/// outage — and it would fail closed on exactly the disk-pressure days when the
/// evidence is most wanted.
#[test]
fn an_unwritable_trail_never_fails_a_build() {
    let root = repo_with_legs("trail-unwritable", &leg("only", "true"));
    let a = branch(&root, "a", "a.txt", "a\n");

    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let drv = LaneDriver::new(tree, ProfileLegRunner::new("lane"), ReportOnlyLander)
        // Descend THROUGH a regular file (`ok.txt`, written by repo_with_legs).
        // `open(.../ok.txt/nope/trail.log)` is ENOTDIR no matter the
        // permissions — portable in a way a 0o000 chmod is not, since CI may
        // run as root, where permission bits are ignored and the open would
        // unexpectedly succeed.
        .with_trail(root.join("ok.txt").join("nope").join("trail.log"));

    let mut lane = LaneState::with_config(
        &root,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 0,
            ..Default::default()
        },
    );
    let actions = drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("a", &a).with_changed_files(["a.txt"])),
    );

    assert!(
        actions
            .iter()
            .any(|x| matches!(x, cargoless_core::lane::LaneAction::LandAndPublish { .. })),
        "the build must still reach a green landing with the trail unwritable: {actions:?}"
    );
    let _ = fs::remove_dir_all(root);
}
