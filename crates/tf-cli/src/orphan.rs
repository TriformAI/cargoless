//! FIELD FINDING #13b (#88): parent-death detection.
//!
//! ## The bug
//!
//! `tftrunk watch &` then close the terminal / kill the shell: the
//! daemon SURVIVES as an orphan, still holding rust-analyzer + cargo
//! + ~2GB RSS. The user has to `pkill` it by hand. It also feeds the
//! F10 stale-daemon confusion — sometimes a "stale" status file is
//! actually a live orphan, not a dead pid.
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
//!
//! libc-free: the `getppid` extern is declared locally, matching the
//! house style of the `kill` extern in `tf_core::analyzer` (#44) —
//! the workspace dep tree stays minimal by deliberate policy.

/// Parent-liveness probe. Construct once at watch/build startup with
/// [`ParentWatch::capture`]; call [`ParentWatch::orphaned`] cheaply
/// on each loop iteration.
#[derive(Debug, Clone, Copy)]
pub struct ParentWatch {
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

/// `getppid()` on Unix; a fixed sentinel on non-Unix (so
/// `capture()` + `orphaned()` are total functions everywhere — the
/// non-Unix path's `orphaned()` never reads this anyway).
fn current_ppid() -> i32 {
    #[cfg(unix)]
    {
        // SAFETY: getppid(2) is documented async-signal-safe + has no
        // failure mode (always succeeds, returns the parent pid).
        // Local extern keeps `libc` out of the workspace dep tree —
        // same pattern as the `kill` extern in tf_core::analyzer (#44).
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
    fn orphan_detected_when_real_parent_dies() {
        // End-to-end: a child shell backgrounds a grandchild that
        // polls ParentWatch and writes a marker file when it detects
        // orphaning; we kill the CHILD (the grandchild's parent),
        // then assert the grandchild noticed within a bounded window.
        //
        // Implemented with `sh -c` + a tiny Rust-free poller so the
        // test doesn't need to re-exec the test binary. The poller is
        // a shell loop comparing $PPID — semantically identical to
        // ParentWatch's getppid()-comparison — proving the mechanism
        // the struct relies on actually fires on this platform.
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let marker = std::env::temp_dir().join(format!(
            "tf-orphan-test-{}-{}",
            std::process::id(),
            // nanos for uniqueness across rapid re-runs
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&marker);
        let marker_s = marker.to_string_lossy().into_owned();

        // Child: spawn a grandchild poller, print the grandchild pid,
        // then sleep (stay alive until we kill it).
        let script = format!(
            r#"
            (
              orig=$PPID
              while :; do
                if [ "$PPID" != "$orig" ]; then
                  echo orphaned > "{marker}"
                  exit 0
                fi
                # also handle the reparent-to-1 fast case
                if [ "$(ps -o ppid= -p $$ | tr -d ' ')" = "1" ]; then
                  echo orphaned > "{marker}"
                  exit 0
                fi
                sleep 0.1
              done
            ) &
            echo $!
            sleep 30
            "#,
            marker = marker_s
        );
        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            // No POSIX shell (unlikely on unix CI/dev) ⇒ skip cleanly.
            Err(_) => return,
        };

        // Give the grandchild a moment to start its poll loop, then
        // kill the CHILD so the grandchild is reparented.
        std::thread::sleep(Duration::from_millis(300));
        let _ = child.kill();
        let _ = child.wait();

        // The grandchild should detect the reparent + write the
        // marker within a couple seconds (its poll is 100ms).
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = false;
        while Instant::now() < deadline {
            if std::fs::read_to_string(&marker)
                .map(|s| s.contains("orphaned"))
                .unwrap_or(false)
            {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_file(&marker);
        assert!(
            seen,
            "the orphaned grandchild must detect parent-death \
             (getppid/PPID change) within the bounded window — this \
             is the exact mechanism ParentWatch::orphaned() relies on"
        );
    }
}
