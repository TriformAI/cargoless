//! `[lane]` policy, from a project's tf.toml all the way into a build.
//!
//! Separate from `lane_policy.rs`, which stakes itself on being a pure state
//! machine "pinned here without launching a compiler" — these tests touch the
//! filesystem, and mixing them in would falsify that claim.
//!
//! What they prove is the seam, not the arithmetic: that a number written in a
//! project's own tf.toml reaches the machine that bounds a real build. That
//! seam was broken for the whole life of the lane — `with_lane` built its
//! `LaneState` with `LaneState::new`, so `max_members` was pinned at the
//! built-in 10 no matter what anyone configured, and it was documented as
//! tunable the entire time.

use cargoless_core::LaneSettings;
use cargoless_core::lane::{LaneEvent, LaneMember, LanePhase, LaneState};

const ROOT: &str = "/w";

fn member(id: &str, head: &str, files: &[&str]) -> LaneMember {
    LaneMember::new(id, head).with_changed_files(files.iter().copied())
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// A unique temp dir holding one tf.toml.
fn project(tag: &str, tf_toml: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cl-lane-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create project dir");
    std::fs::write(dir.join("tf.toml"), tf_toml).expect("write tf.toml");
    dir
}

#[test]
fn a_tf_toml_batch_size_actually_bounds_a_build() {
    // The end-to-end proof. If any seam between the file and the machine
    // regresses to `LaneConfig::default()`, this goes red: the default cap is
    // 10, so all five members would ride one build instead of three.
    let dir = project(
        "bounds",
        "[lane]\nmax_members = 3\ncapture_window_ticks = 0\n",
    );
    let settings = LaneSettings::resolve_layered(&dir, &no_env).expect("resolve [lane]");
    assert_eq!(settings.lane.max_members, 3);

    let mut st = LaneState::with_config(ROOT, settings.lane);
    for (id, head, file) in [
        ("A", "a", "src/a.rs"),
        ("B", "b", "src/b.rs"),
        ("C", "c", "src/c.rs"),
        ("D", "d", "src/d.rs"),
        ("E", "e", "src/e.rs"),
    ] {
        st.step(LaneEvent::Enqueue(member(id, head, &[file])));
    }

    assert_eq!(
        st.in_flight().len(),
        3,
        "the project's cap bounds the build, not the built-in 10"
    );
    assert_eq!(st.queue_depth(), 2, "the rest ride the next build");
    assert_eq!(st.cfg().max_members, 3, "the lane reports what it ran with");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_zero_capture_window_from_tf_toml_builds_immediately() {
    // The one legal zero. It must survive `validate` AND reach the machine —
    // a validator that rejected it would break the documented
    // single-developer configuration, and one that silently replaced it with
    // the 60-tick default would make a lone developer wait a minute per build.
    let dir = project("window", "[lane]\ncapture_window_ticks = 0\n");
    let settings = LaneSettings::resolve_layered(&dir, &no_env)
        .expect("a zero capture window is the documented build-immediately mode");
    assert_eq!(settings.lane.capture_window_ticks, 0);

    let mut st = LaneState::with_config(ROOT, settings.lane);
    st.step(LaneEvent::Enqueue(member("A", "a", &["src/a.rs"])));
    assert_eq!(
        st.phase(),
        LanePhase::Building,
        "a zero window builds the first arrival without waiting"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_out_of_range_policy_is_refused_before_it_can_reach_a_lane() {
    // `max_members = 0` is not a deadlock, it is an unbounded loop of real
    // release builds with an empty roster. It must never reach `LaneState`.
    let dir = project("zero", "[lane]\nmax_members = 0\n");
    let err = LaneSettings::resolve_layered(&dir, &no_env)
        .expect_err("a lane that may carry nobody must be refused");
    assert!(
        err.to_string().contains("max_members"),
        "the refusal must name the setting: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
