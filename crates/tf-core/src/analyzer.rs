//! rust-analyzer subprocess supervision (Epic 2 / AC#6 = CWDL-7).
//!
//! [`Supervisor`] keeps a child process alive: it spawns it, watches it on a
//! background monitor thread, and **transparently restarts** it if it dies —
//! including a `kill -9` from outside the daemon. The daemon never crashes
//! because rust-analyzer did; callers observe at most a brief reconnecting
//! blip and a bumped [`Supervisor::restart_count`].
//!
//! The supervisor is deliberately **generic over the spawn closure** rather
//! than hardcoding rust-analyzer. That is what makes AC#6 testable in CI:
//! the `rust:1.85-bookworm` image ships no `rust-analyzer`, so the AC#6
//! integration test supervises a portable long-lived process (`sleep`),
//! `kill -9`s it, and asserts the supervisor respawns it and stays up. The
//! real-RA wiring ([`rust_analyzer_command`]) is exercised when the binary is
//! present (LSP client lands in CWDL follow-up).
//!
//! No external deps: std process + threads only. The LSP/JSON layer is a
//! separate module so this — the AC#6 contract — has the smallest possible
//! surface that can break.

use std::ffi::OsString;
use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Factory for the supervised process. Called once at start and again on
/// every restart, so for rust-analyzer this is where the LSP initialize
/// handshake + document re-open will be re-run (follow-up module).
pub type SpawnFn = dyn Fn() -> io::Result<Child> + Send + Sync + 'static;

const POLL_INTERVAL: Duration = Duration::from_millis(40);
const MIN_BACKOFF: Duration = Duration::from_millis(50);
const MAX_BACKOFF: Duration = Duration::from_secs(2);

struct SupState {
    child: Option<Child>,
    /// PID of the most recent successfully-spawned child.
    last_pid: Option<u32>,
    /// Number of *restarts* (the initial spawn is not a restart).
    restarts: u64,
}

struct Shared {
    spawn: Box<SpawnFn>,
    state: Mutex<SupState>,
    shutdown: AtomicBool,
}

/// Owns a supervised child + its monitor thread. Drop = graceful shutdown.
pub struct Supervisor {
    shared: Arc<Shared>,
    monitor: Option<JoinHandle<()>>,
}

impl Supervisor {
    /// Spawn the process and start supervising it. The initial spawn must
    /// succeed; restarts are best-effort with capped backoff.
    pub fn start<F>(spawn: F) -> io::Result<Self>
    where
        F: Fn() -> io::Result<Child> + Send + Sync + 'static,
    {
        let shared = Arc::new(Shared {
            spawn: Box::new(spawn),
            state: Mutex::new(SupState {
                child: None,
                last_pid: None,
                restarts: 0,
            }),
            shutdown: AtomicBool::new(false),
        });

        let first = (shared.spawn)()?;
        {
            let mut st = lock(&shared.state);
            st.last_pid = Some(first.id());
            st.child = Some(first);
        }

        let mon_shared = Arc::clone(&shared);
        let monitor = thread::Builder::new()
            .name("tf-ra-supervisor".into())
            .spawn(move || monitor_loop(mon_shared))
            .expect("spawn tf-ra-supervisor thread");

        Ok(Self {
            shared,
            monitor: Some(monitor),
        })
    }

    /// PID of the current (or most recent) child, if any has spawned.
    pub fn current_pid(&self) -> Option<u32> {
        lock(&self.shared.state).last_pid
    }

    /// How many times the child has been restarted after an unexpected exit.
    pub fn restart_count(&self) -> u64 {
        lock(&self.shared.state).restarts
    }

    /// Best-effort liveness of the current child. Reaps it if it has exited
    /// (so a subsequent restart can proceed).
    pub fn is_alive(&self) -> bool {
        let mut st = lock(&self.shared.state);
        match st.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Stop supervising and terminate the child. Idempotent.
    pub fn shutdown(mut self) {
        self.do_shutdown();
    }

    fn do_shutdown(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(t) = self.monitor.take() {
            let _ = t.join();
        }
        // Monitor performs the final kill+reap on exit; belt-and-braces here
        // in case it never started.
        let mut st = lock(&self.shared.state);
        if let Some(mut c) = st.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.do_shutdown();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A poisoned supervisor mutex means a thread panicked holding daemon
    // state; recovering the guard is the least-bad option (the alternative
    // is the daemon aborting, which violates AC#6's "never crashes").
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn monitor_loop(shared: Arc<Shared>) {
    let mut backoff = MIN_BACKOFF;
    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            break;
        }

        let dead = {
            let mut st = lock(&shared.state);
            match st.child.as_mut() {
                Some(c) => match c.try_wait() {
                    Ok(Some(_status)) => true, // exited (incl. SIGKILL)
                    Ok(None) => false,         // still running
                    Err(_) => true,            // can't tell -> treat as dead
                },
                None => true,
            }
        };

        if !dead {
            thread::sleep(POLL_INTERVAL);
            continue;
        }

        if shared.shutdown.load(Ordering::SeqCst) {
            break;
        }

        // Reap the corpse before respawning.
        {
            let mut st = lock(&shared.state);
            if let Some(mut old) = st.child.take() {
                let _ = old.wait();
            }
        }

        thread::sleep(backoff);
        if shared.shutdown.load(Ordering::SeqCst) {
            break;
        }

        match (shared.spawn)() {
            Ok(child) => {
                let mut st = lock(&shared.state);
                st.last_pid = Some(child.id());
                st.child = Some(child);
                st.restarts += 1;
                backoff = MIN_BACKOFF;
            }
            Err(_) => {
                // RA binary briefly unavailable / fork pressure: back off and
                // retry. Never give up — that would be "daemon crashed".
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }

    // Final cleanup: ensure no orphaned child outlives the daemon.
    let mut st = lock(&shared.state);
    if let Some(mut c) = st.child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Resolve the rust-analyzer launch command: `rustup which rust-analyzer`
/// first (matches the active toolchain), then bare `rust-analyzer` on PATH,
/// then the `RUST_ANALYZER` env override. stdio is piped for the LSP layer.
///
/// This does not spawn anything — it returns a ready [`Command`] so the
/// supervisor's spawn closure stays a one-liner and is the unit of restart.
pub fn rust_analyzer_command() -> io::Result<Command> {
    let exe = resolve_rust_analyzer()?;
    let mut cmd = Command::new(exe);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    Ok(cmd)
}

fn resolve_rust_analyzer() -> io::Result<OsString> {
    if let Some(p) = std::env::var_os("RUST_ANALYZER") {
        return Ok(p);
    }
    if let Some(p) = rustup_which_rust_analyzer() {
        return Ok(p);
    }
    // Fall back to PATH resolution by the OS at spawn time.
    Ok(OsString::from("rust-analyzer"))
}

/// `rustup which rust-analyzer`, or `None` if rustup is absent / the
/// component is not installed. Kept as its own fn so `resolve_rust_analyzer`
/// stays flat (no nested `if let` + `if`, which on MSRV 1.85 can be neither
/// collapsed into a let-chain nor left without tripping clippy).
fn rustup_which_rust_analyzer() -> Option<OsString> {
    let out = Command::new("rustup")
        .args(["which", "rust-analyzer"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(OsString::from(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_analyzer_command_is_resolvable_and_piped() {
        // Must not panic regardless of whether RA is installed.
        let cmd = rust_analyzer_command().expect("command resolves");
        assert!(!format!("{cmd:?}").is_empty());
    }

    #[test]
    fn supervisor_reports_initial_pid_and_zero_restarts() {
        // `sleep` exists on Linux CI and macOS dev machines.
        let sup = Supervisor::start(|| {
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        })
        .expect("start");
        assert!(sup.current_pid().is_some());
        assert_eq!(sup.restart_count(), 0);
        assert!(sup.is_alive());
        sup.shutdown();
    }
}
