//! FIELD FINDING #13b (#88): parent-death detection.
//!
//! ## The bug
//!
//! `cargoless watch &` then close the terminal / kill the shell: the
//! daemon SURVIVES as an orphan, still holding rust-analyzer, cargo,
//! and ~2GB RSS. The user has to `pkill` it by hand. It also feeds
//! the F10 stale-daemon confusion — sometimes a "stale" status file
//! is actually a live orphan, not a dead pid.
//!
//! ## The fix
//!
//! Capture the parent pid at watch/build startup. On every watch-loop
//! iteration (cheap — `getppid()` is a vDSO call on Linux, a trivial
//! syscall elsewhere) check whether our parent changed. When it does,
//! the shell that launched us is gone and the kernel reparented us
//! (to `init`, or — on modern Linux — to the nearest subreaper such
//! as `systemd --user`, which is why we compare against the *original*
//! ppid rather than testing `== 1`). The loop then runs the SAME
//! graceful shutdown the `Disconnected` path already uses:
//! `session.shutdown()` reaps rust-analyzer through the AC#6
//! Supervisor + the #44/#61 `ReapOnDrop` process-group/session kill,
//! and `statusfile::clear()` removes the ghost so a later `status`
//! does not show F10-style "live" for a process we just terminated.
//!
//! Worst-case detection latency is one `HEARTBEAT` (the watch loop's
//! `recv_timeout`, ~5s) — bounded orphan-time instead of indefinite.
//!
//! Unix-only mechanism. On non-Unix (Windows is v0 parking-lot per
//! CLAUDE.md) `orphaned()` is a permanent `false` — the daemon keeps
//! the prior behavior there, documented rather than silently wrong.
//! A service manager that intentionally launches `serve` as a detached
//! pidfile daemon may set `CARGOLESS_MANAGED_SERVICE=1`; this disables
//! the parent-shell guard for that process only.
//!
//! libc-free: the `getppid` extern is declared locally, matching the
//! house style of the `kill` extern in `cargoless_core::analyzer` (#44) —
//! the workspace dep tree stays minimal by deliberate policy.

/// Parent-liveness probe. Construct once at watch/build startup with
/// [`ParentWatch::capture`]; call [`ParentWatch::orphaned`] cheaply
/// on each loop iteration.
#[derive(Debug, Clone, Copy)]
pub struct ParentWatch {
    /// Disabled only for an explicitly managed service process. The
    /// default remains enabled so `watch &` / `build --watch &` still
    /// self-terminate when their launcher shell disappears.
    enabled: bool,
    /// The parent pid observed at construction. On non-Unix this is
    /// unused (the field still exists so the struct shape is
    /// platform-stable; `orphaned()` short-circuits).
    #[cfg_attr(not(unix), allow(dead_code))]
    orig_ppid: i32,
}

impl ParentWatch {
    /// Snapshot the current parent pid. Call ONCE, before the watch
    /// loop, so `orphaned()` has a stable baseline to compare against.
    pub fn capture() -> Self {
        Self {
            enabled: !managed_service_env(),
            orig_ppid: current_ppid(),
        }
    }

    /// True iff our parent process is gone — i.e. `getppid()` differs
    /// from the value captured at construction.
    ///
    /// "Changed from original", NOT "== 1": modern Linux reparents an
    /// orphan to the nearest *subreaper* (e.g. `systemd --user`,
    /// containerd-shim), which is frequently not pid 1. Comparing
    /// against the captured baseline is the reliable signal across
    /// init systems.
    ///
    /// On non-Unix: always `false` (no reparenting model we probe in
    /// v0; documented in the module header).
    pub fn orphaned(&self) -> bool {
        if !self.enabled {
            return false;
        }
        #[cfg(unix)]
        {
            current_ppid() != self.orig_ppid
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

fn managed_service_env() -> bool {
    std::env::var("CARGOLESS_MANAGED_SERVICE")
        .ok()
        .map(|v| {
            let v = v.trim();
            v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("yes")
                || v.eq_ignore_ascii_case("on")
        })
        .unwrap_or(false)
}

/// `getppid()` on Unix; a fixed sentinel on non-Unix (so
/// `capture()` + `orphaned()` are total functions everywhere — the
/// non-Unix path's `orphaned()` never reads this anyway).
fn current_ppid() -> i32 {
    #[cfg(unix)]
    {
        // SAFETY: getppid(2) is documented async-signal-safe + has no
        // failure mode (always succeeds, returns the parent pid).
        // Local extern keeps `libc` out of the workspace dep tree —
        // same pattern as the `kill` extern in cargoless_core::analyzer (#44).
        unsafe {
            unsafe extern "C" {
                fn getppid() -> i32;
            }
            getppid()
        }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshly_captured_is_not_orphaned() {
        // Immediately after capture our parent has NOT changed, so
        // orphaned() must be false. (If this flaked true, the watch
        // loop would shut down instantly on startup — the exact
        // opposite of the bug, equally bad.)
        let pw = ParentWatch::capture();
        assert!(
            !pw.orphaned(),
            "a just-captured ParentWatch must not report orphaned"
        );
    }

    #[test]
    fn orphaned_is_stable_across_repeated_calls_when_parent_alive() {
        // The test process's parent (cargo/the test harness) stays
        // alive for the test's duration, so repeated polls stay
        // false — no spurious orphan-detection.
        let pw = ParentWatch::capture();
        for _ in 0..5 {
            assert!(!pw.orphaned());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[cfg(unix)]
    #[test]
    fn capture_records_a_plausible_ppid() {
        // On Unix our ppid is a real pid (> 0). Sanity that the
        // extern is wired and returning something sane, not 0/garbage.
        let pw = ParentWatch::capture();
        assert!(
            pw.orig_ppid > 0,
            "captured ppid should be a real pid, got {}",
            pw.orig_ppid
        );
    }

    #[cfg(unix)]
    #[test]
    fn orphaned_true_when_baseline_no_longer_matches_live_parent() {
        // The positive edge: `orphaned()` must read `true` the instant
        // the recorded baseline stops matching the live `getppid()` —
        // that divergence IS the reparent signal in production.
        //
        // We assert it deterministically by constructing a ParentWatch
        // whose baseline can never equal a live parent pid (0 is not a
        // valid parent for any running process on Unix — real pids are
        // ≥ 1). `orphaned()` then exercises the real code path
        // (`current_ppid() != self.orig_ppid`, i.e. a live `getppid()`
        // syscall compared against the recorded value) and must flip
        // true. Same-module test ⇒ we can seed the private field.
        //
        // Why not a fork/kill end-to-end test: a cargo test process
        // cannot cleanly get itself reparented, and the earlier shell
        // proxy used POSIX `$PPID`, which is frozen at subshell birth
        // and does NOT track reparenting the way `getppid()` does — so
        // it tested a different mechanism than the code and flaked.
        // The honest, faithful coverage is: `capture()` records a real
        // ppid (`capture_records_a_plausible_ppid`), a fresh capture is
        // not orphaned (`freshly_captured_is_not_orphaned`), it stays
        // stable while the parent lives
        // (`orphaned_is_stable_across_repeated_calls_when_parent_alive`),
        // and a baseline that cannot match the live parent reads
        // orphaned (this test). That fully pins the one-line
        // comparison without a flaky subprocess proxy.
        let pw = ParentWatch {
            enabled: true,
            orig_ppid: 0,
        };
        assert!(
            pw.orphaned(),
            "a baseline that can never equal the live getppid() must \
             read orphaned — this is the exact reparent signal the \
             watch loop relies on to self-terminate"
        );
    }

    #[cfg(unix)]
    #[test]
    fn disabled_parent_watch_never_reports_orphaned() {
        let pw = ParentWatch {
            enabled: false,
            orig_ppid: 0,
        };
        assert!(
            !pw.orphaned(),
            "managed service mode must not self-terminate just because \
             the short-lived launcher script exits"
        );
    }
}
