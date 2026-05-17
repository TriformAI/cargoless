//! AC#6 (CWDL-7): the daemon survives `kill -9` of its supervised analyzer
//! subprocess and transparently restarts it.
//!
//! The CI `rust:1.85-bookworm` image ships no `rust-analyzer`, so this
//! exercises the *supervision contract* — the part AC#6 actually asserts —
//! against a portable long-lived stand-in (`sleep`). We `kill -9` it the way
//! an external operator / OOM-killer would (the `kill` binary against the
//! PID, not our own `Child` handle), then assert: the supervisor process
//! itself never died, it respawned a *new* PID, and it counts the restart.
//!
//! Unix-only by design — Windows is explicitly in the v0 parking lot, and
//! CI + the dev machines (Linux, macOS) are both Unix.
#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tf_core::analyzer::Supervisor;

fn sleeper() -> std::io::Result<std::process::Child> {
    Command::new("sleep")
        .arg("3600")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn external_kill9(pid: u32) {
    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("invoke kill(1)");
    assert!(status.success(), "kill -9 {pid} failed");
}

fn wait_until<F: Fn() -> bool>(timeout: Duration, cond: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

#[test]
fn daemon_survives_kill9_of_analyzer_and_restarts() {
    let sup = Supervisor::start(sleeper).expect("initial spawn");

    let pid1 = sup.current_pid().expect("first child has a pid");
    assert!(sup.is_alive(), "child should be alive right after start");
    assert_eq!(sup.restart_count(), 0, "no restarts before the kill");

    // Simulate the kill -9 from outside the daemon.
    external_kill9(pid1);

    // The supervisor must notice, respawn, and bump the counter — without
    // the daemon (this test process + the monitor thread) dying.
    let restarted = wait_until(Duration::from_secs(15), || {
        sup.restart_count() >= 1
            && sup.is_alive()
            && sup.current_pid().is_some()
            && sup.current_pid() != Some(pid1)
    });

    assert!(
        restarted,
        "expected transparent restart: restarts={}, pid1={:?}, pid_now={:?}, alive={}",
        sup.restart_count(),
        pid1,
        sup.current_pid(),
        sup.is_alive(),
    );

    // A second kill is also survived (restart is not a one-shot).
    let pid2 = sup.current_pid().unwrap();
    external_kill9(pid2);
    let restarted_again = wait_until(Duration::from_secs(15), || {
        sup.restart_count() >= 2 && sup.is_alive() && sup.current_pid() != Some(pid2)
    });
    assert!(restarted_again, "expected a second transparent restart");

    sup.shutdown();
}
