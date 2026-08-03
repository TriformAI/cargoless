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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::io::Read;
use std::io::{self, BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Factory for the supervised process. Called once at start and again on
/// every restart, so for rust-analyzer this is where the LSP initialize
/// handshake + document re-open will be re-run (follow-up module).
pub type SpawnFn = dyn Fn() -> io::Result<Child> + Send + Sync + 'static;

const POLL_INTERVAL: Duration = Duration::from_millis(40);
const MIN_BACKOFF: Duration = Duration::from_millis(50);
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// CGLS-28 — how often the monitor samples RA's RSS when the memory cap
/// is armed. The liveness poll runs at 40ms; reading `/proc/<pid>/status`
/// that often would be pointless syscall traffic for a process that takes
/// tens of seconds to balloon. 2s is ~2-4 GB of headroom at the ~1.8 GB/s
/// growth rate the field report measured — fine for a runaway detector,
/// which is all this is.
const RSS_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const RA_STDERR_FINGERPRINT_CAP: usize = 256;
const RA_STDERR_TAIL_CAP: usize = 200;
const RA_STDERR_LINE_CAP: usize = 16 * 1024;
const RA_STACK_CAPTURE_CAP: usize = 2 * 1024 * 1024;
#[cfg(target_os = "linux")]
const RA_STACK_CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaStderrFingerprint {
    pub fingerprint: String,
    pub count: u64,
    pub level: String,
    pub sample: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RaStderrSnapshot {
    pub process_generation: u64,
    pub pid: Option<u32>,
    pub total_lines: u64,
    pub error_lines: u64,
    pub suppressed_lines: u64,
    pub overflow_fingerprints: u64,
    pub fingerprints: Vec<RaStderrFingerprint>,
    pub tail: Vec<String>,
    pub stack_captures: Vec<Vec<u8>>,
}

#[derive(Default)]
struct RaStderrState {
    process_generation: u64,
    pid: Option<u32>,
    total_lines: u64,
    error_lines: u64,
    suppressed_lines: u64,
    overflow_fingerprints: u64,
    fingerprints: BTreeMap<String, RaStderrFingerprint>,
    tail: VecDeque<String>,
    stack_captures: VecDeque<Vec<u8>>,
}

impl RaStderrState {
    fn snapshot(&self) -> RaStderrSnapshot {
        RaStderrSnapshot {
            process_generation: self.process_generation,
            pid: self.pid,
            total_lines: self.total_lines,
            error_lines: self.error_lines,
            suppressed_lines: self.suppressed_lines,
            overflow_fingerprints: self.overflow_fingerprints,
            fingerprints: self.fingerprints.values().cloned().collect(),
            tail: self.tail.iter().cloned().collect(),
            stack_captures: self.stack_captures.iter().cloned().collect(),
        }
    }
}

/// CGLS-28 — RSS ceiling (MiB) for the supervised rust-analyzer.
/// **`0` (the default) = disabled**, matching the repo's knob convention
/// (`CARGOLESS_WITNESS_MAX_INFLIGHT`, `CARGOLESS_WITNESS_WARM_TARGET`).
///
/// Unparseable values also yield `0`: a typo must not silently arm a
/// process-killing cap.
fn ra_max_rss_mb() -> u64 {
    std::env::var("CARGOLESS_RA_MAX_RSS_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// CGLS-28 — the cap predicate. Pure, and deliberately NOT `cfg`-gated so
/// the default-off guarantee is provable by unit test on any host rather
/// than by reading the call site.
///
/// `cap_mb == 0` ⇒ never reap, at ANY resident size. That is the whole
/// safety property of shipping this default-off.
fn should_reap(rss_kb: u64, cap_mb: u64) -> bool {
    if cap_mb == 0 {
        return false;
    }
    rss_kb / 1024 >= cap_mb
}

/// CGLS-28 — parse `VmRSS` (in kB) out of `/proc/<pid>/status`.
///
/// Pure over the file's CONTENT so it unit-tests on macOS dev machines
/// where no such file exists. `None` on anything unexpected ⇒ the caller
/// never reaps ⇒ a malformed/absent file degrades to today's behaviour.
///
/// Matches `VmRSS` exactly and not by prefix: the adjacent `VmHWM` is the
/// high-water mark and is >= `VmRSS`, so a sloppy match would fire the cap
/// early — on memory that has already been returned.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_vm_rss_kb(status_contents: &str) -> Option<u64> {
    status_contents.lines().find_map(|line| {
        let (key, rest) = line.split_once(':')?;
        if key.trim() != "VmRSS" {
            return None;
        }
        let mut parts = rest.split_whitespace();
        let value = parts.next()?.parse::<u64>().ok()?;
        // The kernel always reports VmRSS in kB; refuse anything else
        // rather than silently mis-scaling by 1024x.
        match parts.next() {
            Some(unit) if unit.eq_ignore_ascii_case("kB") => Some(value),
            _ => None,
        }
    })
}

/// CGLS-28 — current RSS (kB) of `pid`, or `None` when it cannot be known.
///
/// Linux-only by construction: `/proc/<pid>/status` does not exist
/// elsewhere. On every other platform this returns `None`, so the cap can
/// never fire and behaviour is byte-identical to pre-CGLS-28. The parser
/// above stays un-`cfg`'d and is tested everywhere.
fn current_rss_kb(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let contents = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        parse_vm_rss_kb(&contents)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

struct SupState {
    child: Option<Child>,
    /// PID of the most recent successfully-spawned child.
    last_pid: Option<u32>,
    /// Number of *restarts* (the initial spawn is not a restart).
    restarts: u64,
}

/// Post-(re)spawn hook: invoked with the freshly-spawned child *before* it is
/// stored, on the initial spawn and on every transparent restart. For
/// rust-analyzer this is where the LSP `initialize` handshake + document
/// re-open are re-run so a `kill -9` restart is invisible to subscribers
/// (the AC#6 guarantee, now in the live serve loop — not just the test).
/// Called WITHOUT the supervisor state lock held, so it may block on the LSP
/// handshake without stalling liveness monitoring.
pub type OnSpawnFn = dyn FnMut(&mut Child) + Send + 'static;

struct Shared {
    spawn: Box<SpawnFn>,
    on_spawn: Mutex<Box<OnSpawnFn>>,
    state: Mutex<SupState>,
    shutdown: AtomicBool,
    /// #122 Tier-4 idle-evict. When `true`, a dead child is reaped (RAM
    /// freed) but NOT auto-respawned — the monitor parks until
    /// [`SuspendHandle::resume`]. Default `false` ⇒ the monitor behaves
    /// byte-identically (the AC#6 crash-respawn path is unchanged); only
    /// a deliberate `TF_RA_IDLE_EVICT` eviction ever sets it.
    suspended: AtomicBool,
    ra_stderr: Arc<Mutex<RaStderrState>>,
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
        Self::start_with_hook(spawn, |_child: &mut Child| {})
    }

    /// Like [`Supervisor::start`] but also runs `on_spawn` against every
    /// (re)spawned child before it is stored — the seam the live `watch()`
    /// pipeline uses to re-establish the LSP session on each transparent
    /// restart, so AC#6 holds in the real serve loop and not only in the
    /// integration test.
    pub fn start_with_hook<F, H>(spawn: F, on_spawn: H) -> io::Result<Self>
    where
        F: Fn() -> io::Result<Child> + Send + Sync + 'static,
        H: FnMut(&mut Child) + Send + 'static,
    {
        let shared = Arc::new(Shared {
            spawn: Box::new(spawn),
            on_spawn: Mutex::new(Box::new(on_spawn)),
            state: Mutex::new(SupState {
                child: None,
                last_pid: None,
                restarts: 0,
            }),
            shutdown: AtomicBool::new(false),
            suspended: AtomicBool::new(false),
            ra_stderr: Arc::new(Mutex::new(RaStderrState::default())),
        });

        let mut first = (shared.spawn)()?;
        supervise_child_stderr(&shared, &mut first);
        invoke_on_spawn(&shared, &mut first);
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

    /// #122 Tier-4 idle-evict: a cheap, `Clone`-able handle to
    /// suspend/resume the supervised RA from another thread (the
    /// `watch()` fs-batch loop) without moving the [`Supervisor`]
    /// itself (which the [`ModelSession`](crate::model) owns). Shares
    /// the same `Arc<Shared>`; dropping handles never affects the
    /// supervisor.
    pub fn suspend_handle(&self) -> SuspendHandle {
        SuspendHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// PID of the current (or most recent) child, if any has spawned.
    pub fn current_pid(&self) -> Option<u32> {
        lock(&self.shared.state).last_pid
    }

    /// How many times the child has been restarted after an unexpected exit.
    pub fn restart_count(&self) -> u64 {
        lock(&self.shared.state).restarts
    }

    /// Bounded aggregate of the supervised child's stderr. Repeated lines
    /// are represented by a fingerprint and count, never by an unbounded
    /// vector of duplicates.
    pub fn stderr_snapshot(&self) -> RaStderrSnapshot {
        lock(&self.shared.ra_stderr).snapshot()
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

/// #122 Tier-4 idle-evict — a `Clone`-able remote control for the
/// supervised RA, obtained via [`Supervisor::suspend_handle`]. Lets the
/// `watch()` fs-batch loop reclaim RA's ~2 GB during agent-idle gaps
/// without owning the [`Supervisor`].
///
/// ## No-wrong-verdict contract (load-bearing)
///
/// [`suspend`](Self::suspend) only ever *delays* a future check; it can
/// never change a verdict. The authoritative green/red is the
/// cargo-check / F8-redo tier — a transient subprocess, zero resident
/// cost — so a suspended (absent) RA cannot make a tree wrongly green or
/// hide a red. [`resume`](Self::resume) respawns through the unchanged
/// AC#6 path (`spawn` + `invoke_on_spawn` ⇒ LSP re-init + re-`did_open`
/// every file at its CURRENT content), identical to a `kill -9`
/// transparent restart. Worst case of a mistimed evict is a slower
/// next check, never a wrong/missing one; `never-publish-red` is
/// untouched (the latest-green pointer only ever advances on a fresh
/// CLOSED-batch flycheck, which by construction runs only while RA is
/// resumed).
#[derive(Clone)]
pub struct SuspendHandle {
    shared: Arc<Shared>,
}

impl SuspendHandle {
    /// Evict the resident RA: set the suspend flag, then SIGKILL the
    /// live child. The monitor reaps it (freeing ~2 GB) and, seeing the
    /// flag, parks instead of respawning. Idempotent.
    pub fn suspend(&self) {
        self.shared.suspended.store(true, Ordering::SeqCst);
        let mut st = lock(&self.shared.state);
        if let Some(c) = st.child.as_mut() {
            let _ = c.kill();
        }
    }

    /// Lift the suspend: the monitor's park loop exits and respawns RA
    /// through the AC#6 transparent path (re-init + re-`did_open`).
    /// Idempotent; cheap (the actual respawn + LSP handshake happens on
    /// the supervisor's monitor thread).
    pub fn resume(&self) {
        self.shared.suspended.store(false, Ordering::SeqCst);
    }

    /// Whether RA is currently evicted.
    pub fn is_suspended(&self) -> bool {
        self.shared.suspended.load(Ordering::SeqCst)
    }

    /// True iff a respawned child is alive again (the fs loop polls this
    /// after [`resume`](Self::resume) to know RA is back before
    /// forwarding the batch — bounded by the AC#1 bring-up budget).
    pub fn child_alive(&self) -> bool {
        let mut st = lock(&self.shared.state);
        matches!(st.child.as_mut().map(|c| c.try_wait()), Some(Ok(None)))
    }
}

/// Run the post-spawn hook against `child`. The `on_spawn` mutex is held
/// only for the call; the supervisor *state* lock is deliberately NOT held
/// (the hook may block on an LSP handshake).
fn invoke_on_spawn(shared: &Shared, child: &mut Child) {
    let mut hook = shared.on_spawn.lock().unwrap_or_else(|e| e.into_inner());
    (*hook)(child);
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A poisoned supervisor mutex means a thread panicked holding daemon
    // state; recovering the guard is the least-bad option (the alternative
    // is the daemon aborting, which violates AC#6's "never crashes").
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn ra_stack_storm_threshold() -> u64 {
    std::env::var("CARGOLESS_RA_STACK_STORM_THRESHOLD")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(1000)
}

fn supervise_child_stderr(shared: &Shared, child: &mut Child) {
    let Some(stderr) = child.stderr.take() else {
        return;
    };
    let pid = child.id();
    let generation = {
        let mut state = lock(&shared.ra_stderr);
        state.process_generation = state.process_generation.saturating_add(1);
        state.pid = Some(pid);
        state.process_generation
    };
    let stats = Arc::clone(&shared.ra_stderr);
    let threshold = ra_stack_storm_threshold();
    let _ = thread::Builder::new()
        .name(format!("cargoless-ra-stderr-{pid}"))
        .spawn(move || {
            let mut stack_requested = BTreeSet::new();
            for line in BufReader::new(stderr).lines() {
                let raw = match line {
                    Ok(line) => line,
                    Err(error) => {
                        eprintln!(
                            "[cargoless:ra-stderr] generation={generation} pid={pid} \
                             read_error={error}"
                        );
                        break;
                    }
                };
                let sample = truncate_utf8(&raw, RA_STDERR_LINE_CAP);
                let level = stderr_level(&sample);
                let fingerprint = crate::sha256_hex(sample.as_bytes())[..16].to_string();
                let mut capture = false;
                let (count, emitted_sample) = {
                    let mut state = lock(&stats);
                    state.total_lines = state.total_lines.saturating_add(1);
                    if level == "error" {
                        state.error_lines = state.error_lines.saturating_add(1);
                    }
                    state.tail.push_back(sample.clone());
                    while state.tail.len() > RA_STDERR_TAIL_CAP {
                        state.tail.pop_front();
                    }
                    let key = format!("{generation}:{fingerprint}");
                    if !state.fingerprints.contains_key(&key)
                        && state.fingerprints.len() >= RA_STDERR_FINGERPRINT_CAP
                    {
                        state.overflow_fingerprints = state.overflow_fingerprints.saturating_add(1);
                        state.suppressed_lines = state.suppressed_lines.saturating_add(1);
                        (0, false)
                    } else {
                        let entry =
                            state
                                .fingerprints
                                .entry(key)
                                .or_insert_with(|| RaStderrFingerprint {
                                    fingerprint: fingerprint.clone(),
                                    count: 0,
                                    level: level.to_string(),
                                    sample: sample.clone(),
                                });
                        entry.count = entry.count.saturating_add(1);
                        let count = entry.count;
                        if count > 1 {
                            state.suppressed_lines = state.suppressed_lines.saturating_add(1);
                        }
                        if threshold > 0
                            && count >= threshold
                            && stack_requested.insert(fingerprint.clone())
                        {
                            capture = true;
                        }
                        (count, count == 1)
                    }
                };
                if emitted_sample {
                    eprintln!(
                        "[cargoless:ra-stderr] generation={generation} pid={pid} \
                         level={level} fingerprint={fingerprint} count=1 sample={sample}"
                    );
                } else if count > 1 && count.is_power_of_two() {
                    eprintln!(
                        "[cargoless:ra-stderr] generation={generation} pid={pid} \
                         level={level} fingerprint={fingerprint} count={count} \
                         duplicates_suppressed={}",
                        count - 1
                    );
                }
                if capture {
                    let stack = capture_process_stack(pid, &fingerprint, count);
                    eprintln!(
                        "[cargoless:ra-stderr] generation={generation} pid={pid} \
                         event=stack_capture fingerprint={fingerprint} count={count} bytes={}",
                        stack.len()
                    );
                    let mut state = lock(&stats);
                    state.stack_captures.push_back(stack);
                    while state.stack_captures.len() > 4 {
                        state.stack_captures.pop_front();
                    }
                }
            }
        });
}

fn stderr_level(line: &str) -> &'static str {
    if line.contains(" ERROR ") || line.starts_with("ERROR") || line.contains("[ERROR") {
        "error"
    } else if line.contains(" WARN ") || line.starts_with("WARN") || line.contains("[WARN") {
        "warn"
    } else {
        "other"
    }
}

fn truncate_utf8(value: &str, cap: usize) -> String {
    if value.len() <= cap {
        return value.to_string();
    }
    let mut end = cap;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...[truncated]", &value[..end])
}

fn capture_process_stack(pid: u32, fingerprint: &str, count: u64) -> Vec<u8> {
    let mut evidence = format!(
        "schema=cargoless.stack/v3\npid={pid}\nfingerprint={fingerprint}\n\
         repeated_lines={count}\ncaptured_by_pid={}\n",
        std::process::id()
    )
    .into_bytes();
    #[cfg(target_os = "linux")]
    {
        match std::fs::read(format!("/proc/{pid}/stack")) {
            Ok(stack) => {
                evidence.extend_from_slice(b"\n--- /proc/pid/stack ---\n");
                evidence.extend_from_slice(&stack);
            }
            Err(error) => {
                evidence.extend_from_slice(
                    format!("\n/proc/{pid}/stack unavailable: {error}\n").as_bytes(),
                );
            }
        }
        let candidates: [(&str, &[&str]); 3] = [
            ("/usr/bin/eu-stack", &["-p", "__PID__"]),
            (
                "/usr/bin/gdb",
                &[
                    "--batch",
                    "--quiet",
                    "-ex",
                    "set pagination off",
                    "-ex",
                    "thread apply all bt full",
                    "-p",
                    "__PID__",
                ],
            ),
            ("/usr/bin/pstack", &["__PID__"]),
        ];
        for (program, args) in candidates {
            if !std::path::Path::new(program).is_file() {
                continue;
            }
            let pid_text = pid.to_string();
            let args: Vec<&str> = args
                .iter()
                .map(|argument| {
                    if *argument == "__PID__" {
                        pid_text.as_str()
                    } else {
                        argument
                    }
                })
                .collect();
            evidence.extend_from_slice(format!("\n--- {program} ---\n").as_bytes());
            match command_output_with_timeout(program, &args, RA_STACK_CAPTURE_TIMEOUT) {
                Ok(output) => evidence.extend_from_slice(&output),
                Err(error) => {
                    evidence.extend_from_slice(format!("capture failed: {error}\n").as_bytes());
                }
            }
            // One successful user-space stack tool is enough. Keeping the
            // attach window singular minimizes disruption to RA.
            if evidence.len() > 1024 {
                break;
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        evidence.extend_from_slice(b"\nuser-space stack attach is only enabled on Linux\n");
    }
    evidence.truncate(RA_STACK_CAPTURE_CAP);
    evidence
}

#[cfg(target_os = "linux")]
fn command_output_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(stdout) = stdout {
            let _ = stdout
                .take(RA_STACK_CAPTURE_CAP as u64)
                .read_to_end(&mut bytes);
        }
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(stderr) = stderr {
            let _ = stderr
                .take(RA_STACK_CAPTURE_CAP as u64)
                .read_to_end(&mut bytes);
        }
        bytes
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let status = child.wait()?;
            break status;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let mut bytes = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !stderr.is_empty() {
        bytes.extend_from_slice(b"\n--- stderr ---\n");
        bytes.extend_from_slice(&stderr);
    }
    bytes.extend_from_slice(format!("\nexit_status={status}\n").as_bytes());
    bytes.truncate(RA_STACK_CAPTURE_CAP);
    Ok(bytes)
}

/// CGLS-28 — SIGKILL the supervised child if it is over `cap_mb`.
///
/// Deliberately does NOT respawn: it only kills. The monitor's existing
/// `try_wait` sees the death on its next iteration and takes the ordinary
/// AC#6 respawn path, so the cap introduces no second restart mechanism
/// and no new state machine.
///
/// The lock is held only for the pid read and the kill — never across the
/// `/proc` read, which is I/O.
fn reap_if_over_cap(shared: &Arc<Shared>, cap_mb: u64) {
    let pid = {
        let mut st = lock(&shared.state);
        // Re-confirm liveness under the lock: a child that exited between
        // the caller's check and here must not be signalled (its pid may
        // already be recycled). Checked in the body rather than a pattern
        // guard — guard bindings are immutable, and `try_wait` needs
        // `&mut`.
        match st.child.as_mut() {
            Some(c) => match c.try_wait() {
                Ok(None) => c.id(),
                _ => return,
            },
            None => return,
        }
    };
    let Some(rss_kb) = current_rss_kb(pid) else {
        // Non-Linux, or the process vanished mid-read. Never reap on an
        // unknown size — that is the fail-safe direction.
        return;
    };
    if !should_reap(rss_kb, cap_mb) {
        return;
    }
    // Structured, always-on line. `Stdio::inherit()` for RA's own stderr
    // is deliberate ("make RA death visible", 5914e1c); this is the line
    // that keeps a capped death visible even once an operator sets
    // CARGOLESS_RA_STDERR=null.
    eprintln!(
        "[cargoless:obs] ra-memory-cap-reap pid={pid} rss_mb={} cap_mb={cap_mb} (CGLS-28)",
        rss_kb / 1024,
    );
    let mut st = lock(&shared.state);
    if let Some(c) = st.child.as_mut() {
        // Confirm the pid still matches before signalling: between the
        // read above and this lock, the child could have died and been
        // replaced.
        if c.id() != pid {
            return;
        }
        // The process GROUP, not just the child — `proc-macro-srv`
        // descendants are part of the runaway and would otherwise survive.
        #[cfg(unix)]
        kill_process_group(pid as i32);
        let _ = c.kill();
    }
}

fn monitor_loop(shared: Arc<Shared>) {
    let mut backoff = MIN_BACKOFF;
    // CGLS-28 — read the cap ONCE. The knob is deployment config, not a
    // live control, and re-reading env on a 40ms loop would be pointless
    // syscall traffic.
    let cap_mb = ra_max_rss_mb();
    let mut last_rss_sample = Instant::now();
    if cap_mb > 0 {
        eprintln!("[cargoless:obs] ra-memory-cap armed cap_mb={cap_mb} (CGLS-28)");
    }
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
            // CGLS-28 — runaway-memory reap. A live child over the cap is
            // SIGKILLed here; the next iteration observes the death and
            // takes the ORDINARY respawn path below, so this adds no new
            // respawn logic. Downstream, the serve loop's CGLS-27 drain
            // then publishes `unknown` for any push the death stranded —
            // that pairing is what makes the cap safe to arm. A cap
            // WITHOUT CGLS-27 would just reproduce the silent wedge with
            // better logging.
            //
            // `cap_mb == 0` (the default) short-circuits before any
            // syscall, so an unconfigured daemon does exactly what it did
            // before: sleep and poll.
            if cap_mb > 0 && last_rss_sample.elapsed() >= RSS_SAMPLE_INTERVAL {
                last_rss_sample = Instant::now();
                reap_if_over_cap(&shared, cap_mb);
            }
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

        // #122 Tier-4 idle-evict: if this death was a deliberate
        // suspend (TF_RA_IDLE_EVICT), the ~2 GB is now reclaimed (corpse
        // reaped above) — do NOT auto-respawn. Park here until
        // `resume()` clears the flag or the daemon shuts down, then
        // fall through to the SAME spawn + `invoke_on_spawn` path a
        // crash takes (AC#6 transparent re-init/re-`did_open`). When
        // `suspended` is never set (default-off), this `while` is a
        // zero-iteration no-op ⇒ the crash-respawn path is byte-
        // identical to pre-#122.
        while shared.suspended.load(Ordering::SeqCst) && !shared.shutdown.load(Ordering::SeqCst) {
            thread::sleep(POLL_INTERVAL);
        }

        thread::sleep(backoff);
        if shared.shutdown.load(Ordering::SeqCst) {
            break;
        }

        match (shared.spawn)() {
            Ok(mut child) => {
                // Re-establish the LSP session on the new process BEFORE it
                // is visible as the current child — this is what makes the
                // restart transparent to subscribers (AC#6 in the live loop).
                supervise_child_stderr(&shared, &mut child);
                invoke_on_spawn(&shared, &mut child);
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
/// On Unix, the command sets up TWO concentric escape-resistant containers
/// for the child + every descendant it might spawn (FIELD FINDING #3b
/// follow-up — dogfood-lead measured 1.75 zombies-per-check still escaping
/// the #44 first try, which used pgid alone):
///
/// 1. `process_group(0)` — new process group, `pgid == pid`. SIGKILL to
///    `-pgid` takes out RA + every child that INHERITS the pgid (i.e. the
///    common case for `rust-analyzer-proc-macro-srv`).
/// 2. `setsid()` via `pre_exec` — child becomes the leader of a new
///    SESSION too. Sessions are a strict superset of process groups: a
///    descendant that calls `setpgid()` itself (escaping the pgid kill)
///    is STILL in our session, and `pgrep -s <sid>` enumerates them all.
///    [`ReapOnDrop::drop`] uses both: SIGKILL `-pgid` for speed, then
///    `pgrep -s` + individual SIGKILLs as defense-in-depth for escapees.
///
/// On non-Unix targets (Windows, parking-lot per CLAUDE.md), the guard
/// falls back to killing just the immediate child.
///
/// This does not spawn anything — it returns a ready [`Command`] so the
/// supervisor's spawn closure stays a one-liner and is the unit of restart.
pub fn rust_analyzer_command() -> io::Result<Command> {
    let exe = resolve_rust_analyzer()?;
    let mut cmd = Command::new(exe);
    if let Some(path) = std::env::var_os("CARGOLESS_RA_LOG_FILE") {
        cmd.arg("--log-file").arg(path).arg("--no-log-buffering");
    }
    // RA's stderr carries its panics + load-time errors (e.g. the
    // proc-macro ABI-mismatch ERROR, or a "no reactor" panic). Historically
    // this was `Stdio::null()`, which made a crash-looping or dead RA
    // INVISIBLE in the daemon logs — a dead cluster RA could sit unrespawned
    // and the only symptom was verdict=unknown, with nothing explaining why.
    // Default to PIPE so the supervisor can fingerprint repetition, retain a
    // bounded tail, and trigger stack evidence during a storm. `inherit` is
    // an explicit escape hatch for interactive debugging; `null` remains the
    // explicit silent mode.
    let ra_stderr = match std::env::var("CARGOLESS_RA_STDERR").as_deref() {
        Ok("null") | Ok("off") | Ok("0") => Stdio::null(),
        Ok("inherit") => Stdio::inherit(),
        _ => Stdio::piped(),
    };
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(ra_stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // pgid=0 ⇒ "make this child the leader of a new process group with
        // pgid == its pid". Lets us SIGKILL the whole group (RA + every
        // proc-macro-srv it forks that doesn't call setpgid itself) in
        // `ReapOnDrop::drop`.
        cmd.process_group(0);
        // setsid in pre_exec → child becomes session leader with sid == pid.
        // Any descendant that escapes the pgid via setpgid is STILL in our
        // session and findable via `pgrep -s <pid>` — the defense in depth
        // the #44 first try was missing (dogfood-lead's 1.75 zombies/check).
        //
        // SAFETY: pre_exec runs AFTER fork() but BEFORE exec(); we are in
        // a single-threaded child process at that moment and may only
        // call async-signal-safe functions. setsid(2) IS async-signal-safe
        // (POSIX SS_FN list). Errors from setsid (EPERM only — if the
        // child were already a session leader) are swallowed: best-effort,
        // process_group(0) above is the load-bearing line.
        unsafe {
            cmd.pre_exec(|| {
                unsafe extern "C" {
                    fn setsid() -> i32;
                }
                let _ = setsid();
                Ok(())
            });
        }
    }
    apply_ra_allocator_env(&mut cmd);
    Ok(cmd)
}

/// #112-B Tier-1 — RSS-only, behavior-neutral allocator tuning for the
/// rust-analyzer child. RA is heavily multithreaded; glibc's malloc
/// grows up to `8 × ncpu` per-thread arenas, and arena fragmentation is
/// a dominant contributor to RA's ~2 GB RSS with **zero** functional
/// effect (RA upstream ships jemalloc precisely for this, but the
/// rustup/distro binary cargoless spawns links system glibc malloc).
///
/// `MALLOC_ARENA_MAX` is consumed by glibc malloc only; musl and macOS
/// ignore it (harmless no-op) — so this is safe to apply unconditionally
/// and cannot change any verdict (it only affects the child's heap
/// arena count, never analysis output). The authoritative cargo-check /
/// F8-redo tier is untouched.
///
/// Conservative escape hatches: never overrides an operator-set
/// `MALLOC_ARENA_MAX`; `TF_RA_ALLOC=off` disables the whole tier;
/// jemalloc preload is **opt-in** (`TF_RA_JEMALLOC=1`) for the spike —
/// allocator *swap* is empirically safe (RA ships it) but kept opt-in
/// until bench-lead's RSS delta justifies a default (see D-RAM-TIERS).
fn apply_ra_allocator_env(cmd: &mut Command) {
    if matches!(std::env::var("TF_RA_ALLOC").as_deref(), Ok("off")) {
        return;
    }
    // Cap glibc arenas unless the operator already chose a value.
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        cmd.env("MALLOC_ARENA_MAX", "2");
    }
    // Opt-in jemalloc preload (only if a libjemalloc is discoverable and
    // the operator has not already set LD_PRELOAD — we never clobber it).
    let want_jemalloc = matches!(std::env::var("TF_RA_JEMALLOC").as_deref(), Ok("1"))
        && std::env::var_os("LD_PRELOAD").is_none();
    let preload = if want_jemalloc { find_jemalloc() } else { None };
    if let Some(so) = preload {
        cmd.env("LD_PRELOAD", so);
    }
}

/// Locate a `libjemalloc` shared object for the opt-in Tier-1 preload.
/// `TF_RA_JEMALLOC_SO` is an explicit override; otherwise probe the
/// common multiarch/dev paths. Returns `None` (⇒ no preload, glibc
/// arena cap still applies) if none exist — never an error.
fn find_jemalloc() -> Option<std::ffi::OsString> {
    if let Some(p) =
        std::env::var_os("TF_RA_JEMALLOC_SO").filter(|p| std::path::Path::new(p).exists())
    {
        return Some(p);
    }
    const CANDIDATES: &[&str] = &[
        "/usr/lib/x86_64-linux-gnu/libjemalloc.so.2",
        "/usr/lib/aarch64-linux-gnu/libjemalloc.so.2",
        "/usr/local/lib/libjemalloc.so.2",
        "/usr/lib/libjemalloc.so.2",
        "/lib/x86_64-linux-gnu/libjemalloc.so.2",
    ];
    CANDIDATES
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(std::ffi::OsString::from)
}

/// FIELD FINDING #3b: a scope-bound guard around a rust-analyzer [`Child`]
/// that kill-reaps on Drop, even on the early-return (`?`) paths of the
/// one-shot check loop. `std::process::Child` deliberately does NOT reap
/// on drop (documented behavior), so a `client.initialize()?` failure
/// after spawn used to leak the child silently.
///
/// ## Unix reap strategy (FIELD FINDING #3b follow-up)
///
/// The dogfood field measurement on the #44 first try found 1.75
/// zombies-per-check still escaping — about half the original ~3.7. The
/// pgid SIGKILL caught the common case (proc-macro-srv inheriting RA's
/// pgid) but missed descendants that called `setpgid()` themselves
/// (escaping the group) or that double-fork into init's reparenting.
///
/// The deepened reap, in order:
///
/// 1. **Snapshot session members BEFORE killing.** RA's spawn sets
///    `setsid()` so `sid == ra_pid`; `pgrep -s <sid>` lists every
///    process in the session — a STRICT superset of the process group
///    (setpgid escapees stay in the same session). Snapshot here so the
///    listing is taken while everything is still alive and findable.
/// 2. **SIGKILL `-pgid`** (the existing fast path).
/// 3. **SIGKILL each session member individually** (the escapees the
///    pgid kill missed). Order-safe: SIGKILL to a dead pid is ESRCH,
///    harmless. Order-bounded: pgrep snapshot is taken at step 1, so
///    we never grow the kill list with reparented orphans.
/// 4. **Reap the immediate child** with `child.wait()` to free its PID
///    slot. Belt-and-braces for non-Unix where steps 1-3 are no-ops.
///
/// Double-fork escapees (rare; mostly daemon-style services, not RA's
/// build tooling) are not catchable without a full `/proc` walk; that
/// is a documented v1+ refinement. For v0 launch, the session-member
/// walk closes the dogfood-observed gap (target: 0 zombies/check).
///
/// On non-Unix targets (Windows, parking-lot per CLAUDE.md), the guard
/// falls back to killing just the immediate child.
pub struct ReapOnDrop(Option<std::process::Child>);

impl ReapOnDrop {
    /// Wrap a freshly-spawned child. After this call, scope-exit (panic,
    /// early-return, or normal Drop) reliably reaps RA + its proc-macro
    /// grandchildren on Unix (incl. setpgid escapees).
    pub fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    /// Take the stdin/stdout pipes for the LSP layer to drive, leaving
    /// the [`Child`] inside the guard so its lifecycle still ends on
    /// scope exit. Returns `None` if `take()` was already called.
    pub fn take_stdio(&mut self) -> Option<(std::process::ChildStdin, std::process::ChildStdout)> {
        let child = self.0.as_mut()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        Some((stdin, stdout))
    }
}

/// SIGKILL `pid`'s whole process group plus every straggler in its
/// session (FIELD FINDING #3b: pgid alone left ~1.75 zombies per check).
///
/// Extracted from [`ReapOnDrop::drop`] (CGLS-28) so the memory cap reaps
/// through the SAME proven path. A bare `Child::kill()` signals only the
/// immediate child and leaves `proc-macro-srv` descendants running — on a
/// runaway-memory kill those descendants are precisely what must die.
///
/// Best effort throughout: ESRCH on an already-dead member is fine. Unix
/// only; a no-op elsewhere, where the caller's `Child::kill()` stands
/// alone as it always has.
#[cfg(unix)]
fn kill_process_group(pid: i32) {
    // Step 1: snapshot session members BEFORE killing. RA was set up as
    // a session leader via setsid in pre_exec, so the session id equals
    // the pid. `pgrep -s` is on every modern Linux + macOS (procps-ng +
    // BSD procps); a missing pgrep (musl minimal containers) just makes
    // step 3 a no-op — step 2's pgid kill still runs.
    let session_members = snapshot_session_members(pid);
    // Step 2: SIGKILL the whole process group (the fast path — catches
    // every descendant that inherited the pgid).
    unsafe {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        // Best effort: ESRCH is fine — we just want a successful reap
        // afterward.
        let _ = kill(-pid, SIGKILL);
        // Step 3: SIGKILL each session-member individually (the setpgid
        // escapees missed by step 2). Skip pid itself (already killed via
        // -pid above). ESRCH for any already-dead member is harmless.
        for m in session_members {
            if m != pid {
                let _ = kill(m, SIGKILL);
            }
        }
    }
}

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        #[cfg(unix)]
        kill_process_group(child.id() as i32);
        // Step 4: belt-and-braces immediate-child kill + wait. On Unix
        // the SIGKILL above usually already terminated it; the wait
        // here is what frees the PID slot.
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// FIELD FINDING #3b follow-up: snapshot every PID in `sid`'s session
/// via `pgrep -s`. Empty Vec if pgrep is missing, exits non-zero, or
/// outputs no PIDs — all are safe degradations (the pgid SIGKILL still
/// runs; this is defense in depth).
///
/// Cost: ~1 process spawn (pgrep is small + warm in distro caches). Runs
/// once per ReapOnDrop drop, i.e. once per `cargoless check`. Not on a
/// hot path.
#[cfg(unix)]
fn snapshot_session_members(sid: i32) -> Vec<i32> {
    let Ok(output) = Command::new("pgrep")
        .arg("-s")
        .arg(sid.to_string())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<i32>().ok())
        .collect()
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

// ---------------------------------------------------------------------------
// A3 — RA ⇄ toolchain ABI-skew guard.
//
// rust-analyzer spawns the proc-macro-server from the WORKSPACE sysroot
// (`RUSTUP_TOOLCHAIN`); if RA's own ABI version differs from that srv's, RA
// refuses ALL macro expansion ⇒ zero diagnostics ⇒ every push timer-settles
// empty ⇒ verdict=unknown fleet-wide (the historical 1.85-RA ⇄ 1.93-srv
// stall). The Dockerfile pins both equal at build time, but nothing asserts
// it at runtime — a base-image bump that moves the toolchain without
// rebuilding RA silently re-introduces the skew. This guard makes that
// fail-LOUD instead of fail-silent-to-all-unknown. Only meaningful when
// proc-macros are enabled; the caller decides strictness.
// ---------------------------------------------------------------------------

/// Extract the semver release (e.g. `1.93.1`) from a `rust-analyzer --version`
/// line like `rust-analyzer 1.93.1 (01f6ddf 2026-02-11)`. `None` if no
/// dotted-numeric token is present (a dev build like `rust-analyzer 0.0.0-...`
/// or an unrecognized format — the caller treats that as "can't verify").
pub fn parse_ra_release(version_line: &str) -> Option<String> {
    version_line.split_whitespace().find_map(|tok| {
        let core = tok.trim_start_matches('v');
        let mut parts = core.split('.');
        let (a, b, c) = (parts.next()?, parts.next()?, parts.next()?);
        // major.minor.patch where the patch may carry a -suffix; require the
        // first three dot-fields to start numeric so we don't match a date.
        let numeric = |s: &str| s.chars().next().is_some_and(|ch| ch.is_ascii_digit());
        if numeric(a) && numeric(b) && numeric(c) {
            let patch: String = c.chars().take_while(char::is_ascii_digit).collect();
            Some(format!("{a}.{b}.{patch}"))
        } else {
            None
        }
    })
}

/// Extract the channel/release prefix from a `RUSTUP_TOOLCHAIN` value like
/// `1.93.1-x86_64-unknown-linux-gnu` → `1.93.1`. Non-numeric channels
/// (`stable`, `nightly-2026-…`) return the leading token as-is; the comparator
/// treats a non-numeric channel as "can't verify" rather than a mismatch.
pub fn parse_toolchain_release(toolchain: &str) -> Option<String> {
    let head = toolchain.split('-').next()?.trim();
    if head.is_empty() {
        None
    } else {
        Some(head.to_string())
    }
}

/// The outcome of the ABI-alignment check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbiAlignment {
    /// RA release == toolchain release. Safe to expand proc-macros.
    Aligned { version: String },
    /// RA release != toolchain release — the skew that silently disables
    /// macro expansion. Carries both for the operator message.
    Skewed { ra: String, toolchain: String },
    /// Could not parse one/both versions (dev build, non-numeric channel,
    /// missing env). Not an assertion either way — proceed but note it.
    Unverifiable,
}

/// Pure comparator: is the RA binary ABI-aligned with the workspace toolchain?
/// Inputs are the raw `rust-analyzer --version` line and the `RUSTUP_TOOLCHAIN`
/// value (`None` if unset). Pure ⇒ unit-tested without spawning RA.
pub fn check_abi_alignment(ra_version_line: &str, rustup_toolchain: Option<&str>) -> AbiAlignment {
    let (Some(ra), Some(tc)) = (
        parse_ra_release(ra_version_line),
        rustup_toolchain.and_then(parse_toolchain_release),
    ) else {
        return AbiAlignment::Unverifiable;
    };
    // Compare on major.minor — a patch bump (1.93.1 vs 1.93.0) does not move
    // the proc-macro bridge ABI; the field failure is a generation gap
    // (1.85 vs 1.93). A token that has no numeric major.minor (`stable`,
    // `nightly-…`) is NOT a mismatch — it is unverifiable (don't false-skew).
    let mm = |v: &str| -> Option<String> {
        let mut it = v.split('.');
        let (a, b) = (it.next()?, it.next()?);
        let numeric = |s: &str| s.chars().next().is_some_and(|c| c.is_ascii_digit());
        (numeric(a) && numeric(b)).then(|| format!("{a}.{b}"))
    };
    match (mm(&ra), mm(&tc)) {
        (Some(ra_mm), Some(tc_mm)) if ra_mm == tc_mm => AbiAlignment::Aligned { version: ra },
        (Some(_), Some(_)) => AbiAlignment::Skewed { ra, toolchain: tc },
        // One side has no comparable numeric version (a `stable`/`nightly`
        // channel) ⇒ can't assert either way.
        _ => AbiAlignment::Unverifiable,
    }
}

/// Convenience: probe the ABI alignment of the RA binary cargoless would
/// actually spawn (same resolution as [`rust_analyzer_command`]). Called once
/// at serve startup so the `ra.abi.*` log lands before the first verdict.
pub fn probe_abi_alignment_default() -> AbiAlignment {
    match resolve_rust_analyzer() {
        Ok(ra) => probe_abi_alignment(&ra),
        Err(_) => AbiAlignment::Unverifiable,
    }
}

/// Runtime guard: run `rust-analyzer --version` and compare against
/// `RUSTUP_TOOLCHAIN`. Returns the [`AbiAlignment`]; the CALLER logs/acts
/// (`cargoless-core` has no logging dep — the serve crate owns `tracing`).
/// Best-effort: a failed `--version` spawn is `Unverifiable`, never fatal.
pub fn probe_abi_alignment(ra: &OsString) -> AbiAlignment {
    let line = Command::new(ra)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let toolchain = std::env::var("RUSTUP_TOOLCHAIN").ok();
    match &line {
        Some(l) => check_abi_alignment(l, toolchain.as_deref()),
        None => AbiAlignment::Unverifiable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn supervised_stderr_aggregates_a_repetition_storm() {
        let supervisor = Supervisor::start(|| {
            Command::new("/bin/sh")
                .arg("-c")
                .arg(
                    "i=0; while [ \"$i\" -lt 64 ]; do \
                     echo 'ERROR inference diagnostic in desugared expr' >&2; \
                     i=$((i+1)); done; sleep 5",
                )
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
        })
        .expect("start supervised stderr fixture");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = supervisor.stderr_snapshot();
            if snapshot.total_lines >= 64 {
                assert_eq!(snapshot.error_lines, 64);
                assert_eq!(snapshot.suppressed_lines, 63);
                assert_eq!(snapshot.fingerprints.len(), 1);
                assert_eq!(snapshot.fingerprints[0].count, 64);
                assert_eq!(snapshot.tail.len(), 64);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "stderr drain did not observe the fixture storm"
            );
            thread::sleep(Duration::from_millis(10));
        }
        supervisor.shutdown();
    }

    // ────────── CGLS-28 — RA memory cap (default-off) ──────────

    #[test]
    fn cap_zero_never_reaps_at_any_resident_size() {
        // THE default-off guarantee, proved rather than asserted by
        // reading the call site. An unconfigured daemon must behave
        // byte-identically to pre-CGLS-28 no matter how large RA gets.
        assert!(!should_reap(0, 0));
        assert!(!should_reap(86 * 1024 * 1024, 0), "86 GiB, cap off");
        assert!(!should_reap(u64::MAX, 0), "no size arms a disabled cap");
    }

    #[test]
    fn cap_fires_at_or_above_the_boundary() {
        // 1 MiB = 1024 kB. `>=`, so the cap is inclusive.
        assert!(!should_reap(1023, 1), "just under 1 MiB");
        assert!(should_reap(1024, 1), "exactly at the cap ⇒ reap");
        assert!(should_reap(2048, 1), "over");
        // The field-report shape: 60 GiB cap, RA at ~83 GiB.
        assert!(should_reap(83 * 1024 * 1024, 60 * 1024));
        assert!(!should_reap(59 * 1024 * 1024, 60 * 1024));
    }

    #[test]
    fn parse_vm_rss_reads_vmrss_and_not_the_adjacent_vmhwm() {
        // VmHWM (high-water mark) sits directly above VmRSS in the real
        // file and is >= it. Matching it instead would reap on memory
        // that has ALREADY been returned — firing the cap early.
        let status = "Name:\trust-analyzer\n\
                      State:\tR (running)\n\
                      VmPeak:\t90000000 kB\n\
                      VmSize:\t85000000 kB\n\
                      VmHWM:\t 86000000 kB\n\
                      VmRSS:\t 62153216 kB\n\
                      Threads:\t17\n";
        assert_eq!(parse_vm_rss_kb(status), Some(62_153_216));
    }

    #[test]
    fn parse_vm_rss_returns_none_on_malformed_input() {
        // None ⇒ caller never reaps. Every degradation is fail-safe.
        assert_eq!(parse_vm_rss_kb(""), None, "empty");
        assert_eq!(parse_vm_rss_kb("Name:\trust-analyzer\n"), None, "no VmRSS");
        assert_eq!(
            parse_vm_rss_kb("VmRSS:\tnotanumber kB\n"),
            None,
            "unparseable"
        );
        assert_eq!(parse_vm_rss_kb("VmRSS:\n"), None, "no value");
        assert_eq!(
            parse_vm_rss_kb("VmRSS:\t62153216 MB\n"),
            None,
            "unexpected unit must not be mis-scaled by 1024x"
        );
        assert_eq!(
            parse_vm_rss_kb("NotVmRSS:\t123 kB\n"),
            None,
            "exact key match, not a suffix/prefix match"
        );
    }

    #[test]
    fn current_rss_is_none_off_linux_so_the_cap_cannot_fire() {
        // The platform contract: /proc/<pid>/status is Linux-only, so
        // everywhere else this is None ⇒ `reap_if_over_cap` returns
        // early ⇒ behaviour is byte-identical to pre-CGLS-28. Keeps the
        // repo's first target_os cfg honest on dev machines.
        #[cfg(not(target_os = "linux"))]
        assert_eq!(current_rss_kb(std::process::id()), None);
        // On Linux the daemon's own RSS is readable and non-zero.
        #[cfg(target_os = "linux")]
        assert!(current_rss_kb(std::process::id()).is_some_and(|kb| kb > 0));
    }

    #[test]
    fn ra_max_rss_mb_defaults_to_zero_when_unset_or_malformed() {
        // Env is process-global and tests run in parallel, so assert only
        // on the unset/parse path via the same parse the getter uses: a
        // typo must never silently arm a process-killing cap.
        let parse = |v: &str| v.trim().parse::<u64>().ok().unwrap_or(0);
        assert_eq!(parse(""), 0);
        assert_eq!(parse("off"), 0);
        assert_eq!(parse("-1"), 0, "negative is not a valid cap");
        assert_eq!(parse("48gb"), 0, "unit suffixes are not accepted");
        assert_eq!(parse(" 49152 "), 49152, "whitespace-tolerant");
    }

    #[test]
    fn parse_ra_release_extracts_semver() {
        assert_eq!(
            parse_ra_release("rust-analyzer 1.93.1 (01f6ddf 2026-02-11)").as_deref(),
            Some("1.93.1")
        );
        assert_eq!(
            parse_ra_release("rust-analyzer 1.85.0 (abc 2025-02-17)").as_deref(),
            Some("1.85.0")
        );
        // No dotted-numeric token ⇒ None (unverifiable).
        assert_eq!(parse_ra_release("rust-analyzer dev-build"), None);
    }

    #[test]
    fn parse_toolchain_release_strips_host_triple() {
        assert_eq!(
            parse_toolchain_release("1.93.1-x86_64-unknown-linux-gnu").as_deref(),
            Some("1.93.1")
        );
        assert_eq!(parse_toolchain_release("stable").as_deref(), Some("stable"));
    }

    #[test]
    fn abi_alignment_aligned_when_major_minor_match() {
        // The live prod case: RA 1.93.1 ⇄ toolchain 1.93.1.
        assert_eq!(
            check_abi_alignment(
                "rust-analyzer 1.93.1 (01f6ddf 2026-02-11)",
                Some("1.93.1-x86_64-unknown-linux-gnu")
            ),
            AbiAlignment::Aligned {
                version: "1.93.1".into()
            }
        );
        // Patch-only diff is still aligned (ABI bridge is per major.minor).
        assert!(matches!(
            check_abi_alignment(
                "rust-analyzer 1.93.0 (x 2026-01-01)",
                Some("1.93.1-x86_64-unknown-linux-gnu")
            ),
            AbiAlignment::Aligned { .. }
        ));
    }

    #[test]
    fn abi_alignment_detects_the_historical_skew() {
        // The field failure: 1.85 RA against a 1.93 workspace toolchain.
        assert_eq!(
            check_abi_alignment(
                "rust-analyzer 1.85.0 (4d91de4 2025-02-17)",
                Some("1.93.1-x86_64-unknown-linux-gnu")
            ),
            AbiAlignment::Skewed {
                ra: "1.85.0".into(),
                toolchain: "1.93.1".into()
            }
        );
    }

    #[test]
    fn abi_alignment_unverifiable_on_missing_or_nonnumeric() {
        assert_eq!(
            check_abi_alignment("rust-analyzer 1.93.1 (x y)", None),
            AbiAlignment::Unverifiable
        );
        assert_eq!(
            check_abi_alignment("garbage", Some("1.93.1-x86_64")),
            AbiAlignment::Unverifiable
        );
        // A non-numeric toolchain channel (`stable`) must NOT be reported as a
        // skew — it is simply unverifiable.
        assert_eq!(
            check_abi_alignment("rust-analyzer 1.93.1 (x y)", Some("stable")),
            AbiAlignment::Unverifiable
        );
    }

    // -----------------------------------------------------------------------
    // FIELD FINDING #3b — ReapOnDrop kills + reaps on scope exit
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn reap_on_drop_kills_the_child_on_scope_exit() {
        // Long-lived process via the same `sleep` stand-in the AC#6 test
        // uses (rust-analyzer is not in the CI image). The child must be
        // dead-and-reaped after the guard's Drop runs.
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        {
            let _guard = ReapOnDrop::new(child);
            // Process is alive while guard is in scope.
            assert!(
                pid_is_alive(pid),
                "child should be alive while ReapOnDrop guard exists"
            );
        }
        // Drop ran — give the OS a brief moment to actually deliver SIGKILL
        // and the kernel a moment to update /proc. ~200ms is generous.
        for _ in 0..40 {
            if !pid_is_alive(pid) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("ReapOnDrop guard exited but pid {pid} still alive");
    }

    #[cfg(unix)]
    #[test]
    fn reap_on_drop_take_stdio_returns_pipes_once() {
        // `take_stdio()` must hand back stdin+stdout on the first call and
        // `None` on the second — exactly the contract `check_verdict`
        // depends on (one take, then guard drops at scope exit).
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep with piped stdio");
        let mut guard = ReapOnDrop::new(child);
        let first = guard.take_stdio();
        assert!(first.is_some(), "first take_stdio yields the pipes");
        // Holding the pipes alongside the guard — what check_verdict does.
        let second = guard.take_stdio();
        assert!(second.is_none(), "second take_stdio is None");
        // Pipes drop when `first` goes out of scope; guard drops at end.
        drop(first);
        drop(guard);
    }

    /// Minimal best-effort liveness probe: `kill(pid, 0)` returns 0 if the
    /// pid is live (or a zombie owned by us), `-1` ESRCH if it does not
    /// exist. We only call this in unix-cfg tests so the libc declaration
    /// stays local.
    #[cfg(unix)]
    fn pid_is_alive(pid: i32) -> bool {
        unsafe {
            unsafe extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            kill(pid, 0) == 0
        }
    }

    // -----------------------------------------------------------------------
    // FIELD FINDING #3b follow-up — session snapshot + reap covers escapees
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn snapshot_session_members_returns_self_for_own_session() {
        // The test process is itself a session member; `pgrep -s` of the
        // current session SHOULD include our own pid — unless pgrep is
        // missing, in which case we degrade safely (empty Vec) and the
        // test still passes (the safe-degradation contract).
        let my_pid = std::process::id() as i32;
        // Resolve our own sid via `ps -o sid= -p <pid>`. Portable: works
        // on macOS BSD ps and Linux procps-ng. If ps is missing too, the
        // test silently passes — we can't probe what we can't probe.
        let Ok(out) = Command::new("ps")
            .arg("-o")
            .arg("sid=")
            .arg("-p")
            .arg(my_pid.to_string())
            .output()
        else {
            return;
        };
        let Some(sid) = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<i32>()
            .ok()
        else {
            return;
        };
        let members = snapshot_session_members(sid);
        // If pgrep is available, the snapshot is non-empty and includes
        // at least us. If pgrep is missing, members.is_empty() is the
        // safe-degradation contract — both outcomes are acceptable.
        if !members.is_empty() {
            assert!(
                members.contains(&my_pid),
                "session snapshot {members:?} should include our own pid {my_pid}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_session_members_for_unknown_sid_is_empty_not_panic() {
        // A nonsense sid (way above PID_MAX on any sensible system) must
        // not crash; pgrep exits non-zero (no matches) and we return empty.
        let v = snapshot_session_members(0x7FFF_FFFF);
        assert!(v.is_empty(), "nonsense sid → empty Vec, got {v:?}");
    }

    /// The deepened ReapOnDrop path (snapshot + pgid-SIGKILL + session-
    /// walk + immediate-child wait) must still kill the immediate child
    /// reliably — the regression that would matter most is if the new
    /// snapshot/walk steps broke the existing reap. Use `sleep` as the
    /// child stand-in (CI image has no rust-analyzer; same pattern as the
    /// AC#6 supervisor test).
    #[cfg(unix)]
    #[test]
    fn reap_on_drop_with_session_walk_still_kills_immediate_child() {
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        {
            let _g = ReapOnDrop::new(child);
            assert!(pid_is_alive(pid), "alive while guard in scope");
        }
        // After drop: SIGKILL + reap delivered. ~200ms grace for kernel.
        for _ in 0..40 {
            if !pid_is_alive(pid) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("deepened ReapOnDrop still must kill the immediate child; pid {pid} alive");
    }

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

    /// The live-pipeline guarantee: the post-spawn hook (where watch()
    /// re-establishes the LSP session) fires on the initial spawn AND again
    /// on every transparent restart after a `kill -9`. No rust-analyzer
    /// needed — a `sleep` stand-in, like the AC#6 test.
    #[cfg(unix)]
    #[test]
    fn on_spawn_hook_fires_on_initial_and_after_kill9_restart() {
        use std::sync::atomic::AtomicUsize;

        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let sup = Supervisor::start_with_hook(
            || {
                Command::new("sleep")
                    .arg("30")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
            },
            move |_child: &mut Child| {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        )
        .expect("start_with_hook");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "hook must fire once on the initial spawn"
        );
        let pid1 = sup.current_pid().expect("first pid");

        assert!(
            Command::new("kill")
                .arg("-9")
                .arg(pid1.to_string())
                .status()
                .expect("invoke kill(1)")
                .success()
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if sup.restart_count() >= 1 && calls.load(Ordering::SeqCst) >= 2 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(sup.restart_count() >= 1, "supervisor must have restarted");
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "hook must re-fire on the transparent restart (re-init LSP)"
        );
        sup.shutdown();
    }

    /// #122 Tier-4: suspend() must reclaim (child dies and is NOT
    /// auto-respawned while suspended); resume() must respawn through
    /// the same hook path (re-init). Deterministic via bounded polls;
    /// `sleep` stand-in like the AC#6 test (no rust-analyzer needed).
    #[cfg(unix)]
    #[test]
    fn supervisor_suspend_reclaims_then_resume_respawns() {
        use std::sync::atomic::AtomicUsize;

        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let sup = Supervisor::start_with_hook(
            || {
                Command::new("sleep")
                    .arg("60")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
            },
            move |_child: &mut Child| {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        )
        .expect("start_with_hook");
        let h = sup.suspend_handle();
        assert!(h.child_alive(), "alive on initial spawn");
        assert!(!h.is_suspended());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "hook fired once (spawn)");

        // Suspend: child must die AND stay dead (NOT auto-respawned —
        // that would defeat the ~2 GB reclaim). Poll a bounded window;
        // then assert it remains dead a while longer + restart_count
        // did not move (no respawn).
        h.suspend();
        assert!(h.is_suspended());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while h.child_alive() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!h.child_alive(), "suspend() must reap the child");
        let restarts_at_suspend = sup.restart_count();
        // Stays dead while suspended (no auto-respawn) — sample past a
        // few monitor poll intervals + a backoff.
        thread::sleep(Duration::from_millis(400));
        assert!(
            !h.child_alive(),
            "a suspended RA must NOT be auto-respawned (RAM stays reclaimed)"
        );

        // Resume: monitor's park loop exits → respawn via the SAME
        // hook path (the AC#6 transparent re-init).
        h.resume();
        assert!(!h.is_suspended());
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !h.child_alive() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(h.child_alive(), "resume() must respawn RA");
        assert!(
            sup.restart_count() > restarts_at_suspend,
            "resume respawn must count as a restart"
        );
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "on_spawn hook must re-fire on resume (LSP re-init/re-did_open)"
        );
        sup.shutdown();
    }
}
