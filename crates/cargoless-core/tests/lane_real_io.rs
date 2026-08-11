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
    CommandLander, DispatchLegRunner, LaneDriver, LaneLander, LegRunner, PointerLander,
    ProfileLegRunner, ReportOnlyLander,
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
    // `file` may be nested (`src/broken.rs`); `fs::write` does not create
    // parents, and the resulting ENOENT surfaces as a bare unwrap panic in this
    // helper rather than anything resembling the caller's intent.
    if let Some(parent) = root.join(file).parent() {
        fs::create_dir_all(parent).unwrap();
    }
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

    // DEFECT 2, in the DURABLE channel. `GET /lane` now reports `activity:
    // landing` while the lander runs, but a snapshot dies with the pod and the
    // trail is what is left afterwards. Without a start line the trail reads
    // `outcome=green` and then nothing for up to two hours, so a daemon killed
    // mid-land leaves a record indistinguishable from one that never tried to
    // land at all — the same gap that hid a 600s lander timeout for a full day.
    assert!(
        log.contains("lane-land-start"),
        "the trail must record that a land STARTED, not only that one finished: \
         a land is the only step that moves the trunk and it can run for hours, \
         so `outcome=green` followed by silence must be distinguishable from a \
         lane that never tried to land: {log}"
    );
    assert!(
        log.contains(&format!("lane-land-start members=a@{a}")),
        "and it must name who is being landed, by id@head — `in_flight` is \
         already empty by this point, so nothing else can answer that: {log}"
    );
    // Ordering is the whole point: a start line written after the land would
    // prove nothing about the window it exists to cover.
    let start = log.find("lane-land-start").expect("start line present");
    let outcome = log
        .find("lane-land outcome=")
        .expect("outcome line present");
    assert!(
        start < outcome,
        "the start line must precede the outcome, or it does not cover the \
         window a killed daemon falls into: {log}"
    );

    let _ = fs::remove_file(&trail);
}

/// THE SECURITY PROPERTY, asserted rather than asserted-about.
///
/// `ProfileLegRunner` runs `cargo build` in-process, and `cargo` executes
/// `build.rs` and proc-macros from the tree it compiles. On a lane that tree is
/// unreviewed code, and the daemon's container can read a push-capable forge
/// credential (verified 2026-07-31 with `git push --dry-run` from inside
/// `cargoless-serve`). `DispatchLegRunner` exists to move that compile
/// somewhere unprivileged.
///
/// So the test is: does a `build.rs`-shaped payload in the candidate get
/// executed by the lane? With dispatch it must not — the dispatcher runs
/// instead, and it never enters the candidate tree.
#[test]
fn dispatching_never_executes_code_from_the_candidate_tree() {
    let root = repo_with_legs("dispatch-safety", &leg("never-runs", "true"));

    // A bare remote to push candidates at — stands in for the forge.
    let remote = scratch("dispatch-safety-remote");
    sh(&remote, &["init", "-q", "--bare", "-b", "main"]);
    sh(
        &root,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    sh(&root, &["push", "-q", "origin", "main"]);

    // The canary: a member whose branch carries a script that, if the lane ever
    // executed anything from the candidate, would leave a mark outside the tree.
    let marker = scratch("dispatch-safety-marker").join("pwned");
    let payload = format!("#!/bin/sh\ntouch {}\n", marker.display());
    sh(&root, &["checkout", "-q", "-B", "m", "main"]);
    fs::write(root.join("build.rs"), &payload).unwrap();
    sh(&root, &["add", "build.rs"]);
    sh(&root, &["commit", "-q", "-m", "m"]);
    let m = String::from_utf8_lossy(
        &Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    sh(&root, &["checkout", "-q", "main"]);

    // The dispatcher: reports green, and records what it was handed. Stands in
    // for "dispatch a credential-free workflow".
    let seen = scratch("dispatch-safety-seen").join("dispatched");
    let script = scratch("dispatch-safety-bin").join("dispatch.sh");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s %s\\n' \"$CARGOLESS_LANE_REF\" \"$CARGOLESS_LANE_SHA\" > {}\nexit 0\n",
            seen.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let legs = DispatchLegRunner::new(
        vec![script.to_string_lossy().into_owned()],
        remote.to_string_lossy().into_owned(),
        "refs/heads/lane-candidate",
    );
    let drv = LaneDriver::new(tree, legs, ReportOnlyLander);

    let mut lane = LaneState::with_config(
        &root,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 0,
            ..Default::default()
        },
    );
    let actions = drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("m", &m).with_changed_files(["build.rs"])),
    );

    assert!(
        !marker.exists(),
        "the lane executed code from the candidate tree — that is the escalation \
         DispatchLegRunner exists to prevent"
    );
    let dispatched = fs::read_to_string(&seen).expect("the dispatcher must have been invoked");
    // The sha handed over is the CANDIDATE's, not the member's, and that is the
    // contract: the candidate is a `--no-ff` merge of every member onto the
    // base, so it is a new commit that exists nowhere else. Asserting the
    // member's sha here would be asserting that the builder compiles the
    // unmerged branch — the exact thing the lane exists not to do.
    let (dispatched_ref, dispatched_sha) = dispatched
        .trim()
        .split_once(' ')
        .expect("the dispatcher records `<ref> <sha>`");
    assert_ne!(
        dispatched_sha, m,
        "the builder must be given the merged candidate, not the member's own head"
    );
    assert!(
        dispatched_ref.ends_with(dispatched_sha),
        "the ref must be addressed by the sha it carries, so two candidates \
         cannot collide and a build stays findable after the fact: {dispatched}"
    );
    assert!(
        dispatched_ref.starts_with("refs/heads/lane-candidate/"),
        "and it must live under the configured prefix: {dispatched}"
    );
    assert!(
        actions
            .iter()
            .any(|x| matches!(x, cargoless_core::lane::LaneAction::LandAndPublish { .. })),
        "a green dispatch must still reach a landing: {actions:?}"
    );
    let landed_artifact = actions.iter().find_map(|x| match x {
        cargoless_core::lane::LaneAction::LandAndPublish { artifact, .. } => artifact.as_deref(),
        _ => None,
    });
    assert_eq!(
        landed_artifact,
        Some(dispatched_sha),
        "the trusted lander must receive the exact candidate identity that the \
         unprivileged builder greened; rediscovering it from a separate preview \
         would create a second source of truth: {actions:?}"
    );

    // The candidate must be FETCHABLE by the builder — a ref the sandbox cannot
    // reach is a build that cannot happen. Resolve the ref the dispatcher was
    // actually handed, and check it names the same commit on the remote.
    let ls = Command::new("git")
        .current_dir(&remote)
        .args(["rev-parse", dispatched_ref])
        .output()
        .unwrap();
    assert!(
        ls.status.success(),
        "the candidate must be published on the remote as {dispatched_ref}: {}",
        String::from_utf8_lossy(&ls.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&ls.stdout).trim(),
        dispatched_sha,
        "and the published ref must point at the very commit the dispatcher was \
         told to build — otherwise the builder compiles a different tree than \
         the lane judged"
    );

    for d in [root, remote] {
        let _ = fs::remove_dir_all(d);
    }
}

/// THE AUTO-MERGE STEP: a green candidate is handed to the lander command,
/// with every member's id and head.
#[test]
fn a_green_candidate_is_handed_to_the_lander_with_its_roster() {
    let root = repo_with_legs("land-ok", &leg("build", "true"));
    let a = branch(&root, "a", "a.txt", "a\n");
    let b = branch(&root, "b", "b.txt", "b\n");

    let seen = scratch("land-ok-seen").join("roster");
    let script = scratch("land-ok-bin").join("land.sh");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s' \"$CARGOLESS_LANE_MEMBERS\" > {}\nexit 0\n",
            seen.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let drv = LaneDriver::new(
        tree,
        ProfileLegRunner::new("lane"),
        CommandLander::new(vec![script.to_string_lossy().into_owned()]),
    );
    // A NON-ZERO capture window, closed by an explicit tick.
    //
    // This test is about a roster carrying BOTH members, so both have to ride
    // the same candidate. With `capture_window_ticks: 0` the window is already
    // expired when the first enqueue is pumped, so `a` builds alone, `b`
    // arrives while the lane is Building and rides a second candidate, and the
    // lander — invoked once per build, writing the same file each time — ends
    // up holding only `b`. The assertion then fails on `a` for a reason that
    // has nothing to do with rosters.
    //
    // Ticking to exactly the window edge is what makes the coalescing
    // deterministic rather than a race.
    let mut lane = LaneState::with_config(
        &root,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 5,
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
    // Both are queued; closing the window is what starts the single candidate.
    drv.pump(&mut lane, LaneEvent::Tick { now: 5 });

    let roster = fs::read_to_string(&seen).expect("the lander must have been invoked");
    // id AND head for every member: a lander that only knows the ids cannot
    // reconcile PR state, and one that only knows the heads cannot name who.
    for (id, head) in [("a", &a), ("b", &b)] {
        assert!(
            roster.lines().any(|l| l == format!("{id}\t{head}")),
            "roster must carry `<id>\\t<head>` for {id}: {roster:?}"
        );
    }
    let _ = fs::remove_dir_all(root);
}

/// A FAILED land must re-enqueue, never silently drop green work.
///
/// These members compiled. If a lost CAS race or a forge hiccup made them
/// vanish, the lane would look like it did nothing at all — the worst available
/// outcome, because nobody is told and the work is gone. The driver re-enqueues
/// on `Err`, so the lander must report failure as `Err` rather than an
/// `Ok(LandOutcome)` carrying a sad sentence.
#[test]
fn a_failed_land_requeues_the_members_instead_of_losing_them() {
    let root = repo_with_legs("land-fail", &leg("build", "true"));
    let a = branch(&root, "a", "a.txt", "a\n");

    let script = scratch("land-fail-bin").join("land.sh");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    // What a lost compare-and-swap looks like: the base moved under us.
    fs::write(
        &script,
        "#!/bin/sh\necho '! [rejected] dev -> dev (stale info)' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let drv = LaneDriver::new(
        tree,
        ProfileLegRunner::new("lane"),
        CommandLander::new(vec![script.to_string_lossy().into_owned()]),
    );
    // A NON-ZERO capture window, so the re-enqueue lands in the QUEUE and stops
    // there instead of immediately starting another build.
    //
    // With `capture_window_ticks: 0` the driver spins: Enqueue → StartBuild →
    // BuildFinished → LandAndPublish → lander Err → Enqueue → … Each turn is
    // several lane events, so `pump`'s MAX_STEPS backstop (64) cuts the loop at
    // an arbitrary point. `on_build_finished` does `mem::take(&mut in_flight)`,
    // so a cut between that take and the re-enqueue being applied leaves BOTH
    // `queue_depth()` and `in_flight()` at zero — and the member looks lost
    // when it is not. That is what this assertion was reading.
    //
    // The window is the thing that makes "requeued" observable: the member
    // comes to rest in the queue, and only a tick would start the next attempt.
    let mut lane = LaneState::with_config(
        &root,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 5,
            ..Default::default()
        },
    );
    let mut actions = drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("a", &a).with_changed_files(["a.txt"])),
    );
    // Close the window: this is the pump that builds, lands, fails, requeues.
    actions.extend(drv.pump(&mut lane, LaneEvent::Tick { now: 5 }));

    assert!(
        !actions
            .iter()
            .any(|x| matches!(x, cargoless_core::lane::LaneAction::Eject { .. })),
        "a failed LAND is not the member's fault — nobody may be ejected: {actions:?}"
    );
    // The member is back in the lane rather than gone. `queue_depth` counts
    // what will be built next; anything else means green work evaporated.
    assert_eq!(
        lane.queue_depth() + lane.in_flight().len(),
        1,
        "the member must survive a failed land, not vanish"
    );
    let _ = fs::remove_dir_all(root);
}

/// A dispatcher that could not GET a verdict must never eject anyone.
///
/// Remote builds get cancelled, runners vanish, queues time out. If any of
/// those read as "your code is broken", the lane ejects whichever member
/// happened to be aboard for an infrastructure fault — the fastest way to teach
/// a fleet to distrust its own gate. EX_TEMPFAIL (75) is the dispatcher's way
/// to say "no verdict", and it must reach the lane as infra, not red.
#[test]
fn a_dispatcher_that_cannot_get_a_verdict_ejects_nobody() {
    let root = repo_with_legs("dispatch-tempfail", &leg("unused", "true"));
    let remote = scratch("dispatch-tempfail-remote");
    sh(&remote, &["init", "-q", "--bare", "-b", "main"]);
    sh(
        &root,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    sh(&root, &["push", "-q", "origin", "main"]);
    let a = branch(&root, "a", "src/x.rs", "fn x() {}\n");

    let script = scratch("dispatch-tempfail-bin").join("d.sh");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    // Exactly what scripts/ci/lane-dispatch.sh does on cancelled/timeout.
    fs::write(
        &script,
        "#!/bin/sh\necho 'remote build cancelled' >&2\nexit 75\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let legs = DispatchLegRunner::new(
        vec![script.to_string_lossy().into_owned()],
        remote.to_string_lossy().into_owned(),
        "refs/heads/lane-candidate",
    );
    let drv = LaneDriver::new(tree, legs, ReportOnlyLander);
    let mut lane = LaneState::with_config(
        &root,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 0,
            ..Default::default()
        },
    );
    let actions = drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("a", &a).with_changed_files(["src/x.rs"])),
    );

    assert!(
        !actions
            .iter()
            .any(|x| matches!(x, cargoless_core::lane::LaneAction::Eject { .. })),
        "a transient dispatcher failure must not eject a member: {actions:?}"
    );
    assert!(
        !actions
            .iter()
            .any(|x| matches!(x, cargoless_core::lane::LaneAction::LandAndPublish { .. })),
        "and it must certainly not LAND — no verdict was produced: {actions:?}"
    );

    for d in [root, remote] {
        let _ = fs::remove_dir_all(d);
    }
}

/// The runner must be selectable at RUNTIME, or the daemon cannot offer the
/// choice without monomorphising a branch per (runner × lander) pair.
///
/// This is a compile-shaped property — it passes trivially once the boxed impl
/// exists and fails to build without it — so the assertion is that a boxed
/// runner still produces a real verdict through the real driver, not merely
/// that the types line up.
#[test]
fn a_boxed_runner_drives_a_real_build() {
    let root = repo_with_legs("boxed", &leg("build", "true"));
    let a = branch(&root, "a", "a.txt", "a\n");

    let legs: Box<dyn LegRunner + Send> = Box::new(ProfileLegRunner::new("lane"));
    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let drv = LaneDriver::new(tree, legs, ReportOnlyLander);

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
        "a boxed runner must reach a landing exactly as a concrete one does: {actions:?}"
    );
    let _ = fs::remove_dir_all(root);
}

/// A dispatcher that fails is a RED with real diagnostics, not an infra error —
/// the whole point of moving the build out is that its verdict still attributes.
#[test]
fn a_dispatcher_red_carries_the_builders_cargo_json() {
    let root = repo_with_legs("dispatch-red", &leg("unused", "true"));
    let remote = scratch("dispatch-red-remote");
    sh(&remote, &["init", "-q", "--bare", "-b", "main"]);
    sh(
        &root,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    sh(&root, &["push", "-q", "origin", "main"]);
    let a = branch(&root, "a", "src/broken.rs", "fn x() {}\n");

    let script = scratch("dispatch-red-bin").join("d.sh");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    // Emits exactly what a remote `cargo build --message-format=json` would.
    fs::write(
        &script,
        "#!/bin/sh\necho '{\"reason\":\"compiler-message\",\"message\":{\"level\":\"error\",\
         \"message\":\"remote boom\",\"spans\":[{\"file_name\":\"src/broken.rs\",\
         \"line_start\":1,\"column_start\":1,\"is_primary\":true}]}}'\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let legs = DispatchLegRunner::new(
        vec![script.to_string_lossy().into_owned()],
        remote.to_string_lossy().into_owned(),
        "refs/heads/lane-candidate",
    );
    let drv = LaneDriver::new(tree, legs, ReportOnlyLander);
    let mut lane = LaneState::with_config(
        &root,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 0,
            ..Default::default()
        },
    );
    let actions = drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("a", &a).with_changed_files(["src/broken.rs"])),
    );

    // Ejected, not held: a remote red with file paths is attributable, which is
    // what `output: cargo-json` buys and what an infra classification loses.
    let ejected: Vec<_> = actions
        .iter()
        .filter_map(|x| match x {
            cargoless_core::lane::LaneAction::Eject { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ejected,
        vec!["a".to_string()],
        "a dispatcher red must attribute to the member who touched the failing \
         file, not degrade to infrastructure: {actions:?}"
    );

    for d in [root, remote] {
        let _ = fs::remove_dir_all(d);
    }
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

/// A member that cannot be merged onto the base is EJECTED — and the queue
/// keeps moving without it.
///
/// This is the livelock regression guard. Until 2026-08-02 every
/// `materialize()` failure was reported as `Infra`, and `Infra` ejects nobody
/// by design ("nothing was compiled, so nothing was judged"). A conflicting
/// member therefore never left the queue and was re-included in every
/// subsequent candidate. Observed in production against tf-multiverse:
/// generations 2, 3, 4 and 5 each died on the same unmergeable member while
/// three real PRs waited behind it.
///
/// The third assertion is the one that would have caught it: after the
/// ejection, a fresh tick must NOT put the conflicting member back in flight.
#[test]
fn an_unmergeable_member_is_ejected_and_the_queue_keeps_moving() {
    let root = repo_with_legs("conflict", &leg("build", "true"));

    // `bad` and main both change the same line of the same file, so merging
    // `bad` onto main after main moved is a genuine conflict — not a fetch
    // failure, not a disk error.
    let bad = branch(&root, "bad", "contested.txt", "from the branch\n");
    fs::write(root.join("contested.txt"), "from main\n").unwrap();
    sh(&root, &["add", "contested.txt"]);
    sh(&root, &["commit", "-q", "-m", "main moves contested.txt"]);

    // `good` touches a different file and merges cleanly.
    let good = branch(&root, "good", "fine.txt", "no conflict here\n");

    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let drv = LaneDriver::new(tree, ProfileLegRunner::new("lane"), ReportOnlyLander);
    let mut lane = LaneState::with_config(
        &root,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 0,
            ..Default::default()
        },
    );

    // Both members ride the same candidate. `bad` is submitted first so it is
    // merged first and is unambiguously the one that fails.
    let _ = drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("bad", &bad).with_changed_files(["contested.txt"])),
    );
    let actions = drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("good", &good).with_changed_files(["fine.txt"])),
    );

    // 1. The conflicting member is ejected BY NAME.
    let ejected: Vec<&str> = lane.ejections().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ejected,
        vec!["bad"],
        "the member that could not be merged must be ejected, and only that \
         member — a co-rider is not responsible for someone else's conflict. \
         actions: {actions:?}"
    );

    // 2. It is ejected as a VERDICT about that member, never as infrastructure.
    //    Infra ejects nobody, which is exactly how the livelock happened.
    let (_, ejection) = lane.ejections().next().expect("`bad` is ejected");
    assert_eq!(
        ejection.cause,
        cargoless_core::lane::EjectionCause::MergeConflict
    );
    assert!(
        !matches!(
            ejection.reason,
            cargoless_core::lane::EjectReason::Infrastructure { .. }
        ),
        "a conflict is attributable to the member git named; classifying it as \
         infrastructure is what let it re-enter every candidate forever: {:?}",
        ejection.reason
    );

    // 3. THE LIVELOCK GUARD. A fresh tick must not resurrect the ejected
    //    member. If this fails, every future candidate re-includes it and the
    //    queue never drains.
    let after = drv.pump(&mut lane, LaneEvent::Tick { now: 1 });
    let in_flight: Vec<&str> = lane.in_flight().iter().map(|m| m.id.as_str()).collect();
    assert!(
        !in_flight.contains(&"bad"),
        "the ejected member must stay out until its head moves — putting it \
         back is the livelock this test exists to prevent: in_flight={in_flight:?} \
         actions={after:?}"
    );

    let _ = fs::remove_dir_all(root);
}

/// A member that LANDS while it waits in the queue must leave the queue.
///
/// The window is real and now dangerous: a candidate build takes minutes, and
/// in that time someone can merge the PR by hand, or a previous candidate can
/// carry it. Its head is then already an ancestor of the base, and
/// `git merge --no-ff` does not fail — it writes an EMPTY commit. So the
/// candidate builds, goes green, and the lander is handed a roster naming a PR
/// that is already closed.
///
/// While landing was report-only that was untidy. With auto-merge armed it is a
/// real merge API call against a merged PR, which is exactly the situation that
/// makes an auto-merger untrustworthy.
#[test]
fn a_member_that_already_landed_is_ejected_instead_of_merged_empty() {
    let root = repo_with_legs("stale", &leg("build", "true"));

    // `done` is a real branch with a real commit...
    let done = branch(&root, "done", "shipped.txt", "already in main\n");
    // ...which then lands on main. This is the mid-flight merge.
    sh(&root, &["checkout", "-q", "main"]);
    sh(
        &root,
        &["merge", "--no-ff", "-q", "-m", "operator merged it", &done],
    );

    // `fresh` is genuinely unmerged and must survive.
    let fresh = branch(&root, "fresh", "new.txt", "not yet landed\n");

    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let drv = LaneDriver::new(tree, ProfileLegRunner::new("lane"), ReportOnlyLander);
    let mut lane = LaneState::with_config(
        &root,
        cargoless_core::lane::LaneConfig {
            capture_window_ticks: 0,
            ..Default::default()
        },
    );

    let _ = drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("done", &done).with_changed_files(["shipped.txt"])),
    );
    let actions = drv.pump(
        &mut lane,
        LaneEvent::Enqueue(LaneMember::new("fresh", &fresh).with_changed_files(["new.txt"])),
    );

    // 1. The landed member is ejected BY NAME — not silently skipped. A green
    //    candidate that never contained the member must not look like one
    //    that did.
    let ejected: Vec<&str> = lane.ejections().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ejected,
        vec!["done"],
        "a member already contained in the base must leave the queue, and only \
         that member. actions: {actions:?}"
    );

    // 2. Never infrastructure. Infra ejects nobody and backs off, so a landed
    //    member would ride every future candidate forever.
    let (_, ejection) = lane.ejections().next().expect("`done` is ejected");
    assert_eq!(
        ejection.cause,
        cargoless_core::lane::EjectionCause::AlreadyLanded
    );
    assert!(
        !matches!(
            ejection.reason,
            cargoless_core::lane::EjectReason::Infrastructure { .. }
        ),
        "an already-landed member is attributable, not an infra failure: {:?}",
        ejection.reason
    );

    // 3. The co-rider is untouched — someone else landing is not its problem.
    assert!(
        !ejected.contains(&"fresh"),
        "the unmerged member must not be ejected because a co-rider landed: \
         ejected={ejected:?}"
    );

    let _ = fs::remove_dir_all(root);
}

/// A failing lander must not retry as fast as the driver can loop.
///
/// The realistic cause of a failed land is the base moving and the forge's
/// compare-and-swap rejecting the push — which on a busy trunk persists for
/// many minutes. Without pacing, every rejection re-enqueues and
/// `maybe_start_build` starts the next candidate at once, so the lane rebuilds
/// the same tree continuously, each turn a real multi-minute build holding the
/// slot. The infra path has carried `infra_backoff_ticks` for exactly this
/// reason since the first deployment; the land path had nothing.
#[test]
fn a_failing_lander_backs_off_instead_of_spinning() {
    let root = repo_with_legs("landbackoff", &leg("build", "true"));
    let a = branch(&root, "a", "a.txt", "a\n");

    // A lander that always fails.
    let script = root.join("fail-land.sh");
    fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let tree = GitCandidateTree::new(&root, root.join(".cargoless/lane-candidates"), "main");
    let drv = LaneDriver::new(
        tree,
        ProfileLegRunner::new("lane"),
        CommandLander::new(vec![script.to_string_lossy().into_owned()]),
    );
    // Zero capture window on purpose: it is the WORST case for this property.
    // Nothing else would stop the requeued member building again immediately,
    // so if a backoff holds here it holds anywhere.
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

    // The member survived — a failed land must never lose green work.
    assert_eq!(
        lane.queue_depth() + lane.in_flight().len(),
        1,
        "the member must survive a failed land: {actions:?}"
    );
    // And it is WAITING, not building. Before the backoff existed this was
    // `Building`, over and over, until `pump`'s MAX_STEPS cut the loop.
    assert!(
        lane.in_flight().is_empty(),
        "a failed land must not immediately restart the build — that is the hot \
         loop the backoff exists to prevent: in_flight={:?}",
        lane.in_flight()
    );
    // The operator is told why, with the retry budget. A member that stops
    // moving and says nothing is the failure mode `GET /lane` exists to avoid.
    assert!(
        actions.iter().any(|x| matches!(
            x,
            cargoless_core::lane::LaneAction::Report { state, .. } if state.contains("land failed")
        )),
        "the failed land must be reported, not silent: {actions:?}"
    );

    let _ = fs::remove_dir_all(root);
}
