//! The tf-cli-owned daemon-status file (lead RULING 1).
//!
//! `ModelSession` is in-process only — a separate `status` invocation cannot
//! call `tree_state()`. So the running `watch` / `build --watch` process
//! writes this small file (liveness heartbeat + current verdict) and
//! `status` reads it. This is DISTINCT from build-cas's
//! `.cargoless/latest-green` (latest *green* artifact pointer, build-cas
//! owned/format-owned). tf-cli owns and documents *this* file's format.
//!
//! ## Format (`<root>/.cargoless/cli-status`) — documented contract
//!
//! ```text
//! schema=1
//! pid=<u32>
//! root=<canonical project root>
//! started=<unix seconds>
//! updated=<unix seconds>      # heartbeat; freshness = liveness signal
//! verdict=green|red|unknown   # current tree verdict at last update
//! ```
//!
//! Forward-compatible: unknown keys are ignored on read. Liveness is
//! freshness-based (no libc/pid-kill, no port — v0 is headless): the writer
//! refreshes `updated` at least every [`HEARTBEAT`]; a reader treats the
//! daemon as live iff `now - updated <= STALE_AFTER`. Written atomically
//! (temp file + rename) so `status` never reads a torn line.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::ui;

/// The writer refreshes `updated` at least this often (also its event
/// recv-timeout, so a quiet green tree still heartbeats).
pub const HEARTBEAT: Duration = Duration::from_secs(5);

/// A reader treats the daemon as stopped if the heartbeat is older than
/// this (3× HEARTBEAT — tolerates one missed beat + scheduling jitter).
pub const STALE_AFTER: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Green,
    Red,
    Unknown,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Green => "green",
            Verdict::Red => "red",
            Verdict::Unknown => "unknown",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "green" => Verdict::Green,
            "red" => Verdict::Red,
            _ => Verdict::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    pub pid: u32,
    pub root: String,
    pub started: u64,
    pub updated: u64,
    pub verdict_str: String,
}

pub fn path(root: &Path) -> PathBuf {
    root.join(".cargoless").join("cli-status")
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Status {
    pub fn serialize(&self) -> String {
        format!(
            "schema=1\npid={}\nroot={}\nstarted={}\nupdated={}\nverdict={}\n",
            self.pid, self.root, self.started, self.updated, self.verdict_str
        )
    }

    /// Parse the documented format. Unknown keys ignored (forward-compatible).
    pub fn parse(text: &str) -> Self {
        let mut s = Status::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim() {
                "pid" => s.pid = v.trim().parse().unwrap_or(0),
                "root" => s.root = v.trim().to_string(),
                "started" => s.started = v.trim().parse().unwrap_or(0),
                "updated" => s.updated = v.trim().parse().unwrap_or(0),
                "verdict" => s.verdict_str = v.trim().to_string(),
                _ => {}
            }
        }
        s
    }

    pub fn is_fresh(&self, now: u64) -> bool {
        now.saturating_sub(self.updated) <= STALE_AFTER.as_secs()
    }
}

/// Atomic write: temp file + rename (same dir ⇒ atomic on the fs). Best
/// effort — a status-file failure must never take the daemon down.
pub fn write(root: &Path, st: &Status) {
    let p = path(root);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = p.with_extension("tmp");
    if let Ok(mut f) = std::fs::File::create(&tmp) {
        if f.write_all(st.serialize().as_bytes()).is_ok() {
            let _ = f.flush();
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

pub fn clear(root: &Path) {
    let _ = std::fs::remove_file(path(root));
}

/// `status` command. Exit `0` = daemon live, `3` = no/stale daemon — so a
/// script can gate on "is cargoless watching this project?".
///
/// FIELD FINDING #10 (#56): freshness alone is not sufficient. The status
/// file's `updated` field is heartbeated every [`HEARTBEAT`] (5s) by the
/// running watch process; if a user kills the watch and runs `status`
/// within the [`STALE_AFTER`] (15s) window, the file looks fresh even
/// though the daemon is dead. We additionally ask the kernel
/// (`kill(pid, 0)`) whether `st.pid` is still a live process; if it is
/// not, treat the entry as stale regardless of file age.
pub fn run_status(cfg: &Config) -> ExitCode {
    let Ok(text) = std::fs::read_to_string(path(&cfg.root)) else {
        ui::warn(format!(
            "no cargoless daemon for {} — start one: `cargoless watch` or \
             `cargoless build --watch --out <dir>`.",
            cfg.root.display()
        ));
        report_latest_green(&cfg.root);
        return ExitCode::from(3);
    };

    let st = Status::parse(&text);
    let now = now_unix();
    let file_fresh = st.is_fresh(now);
    let age = now.saturating_sub(st.updated);
    // FIELD FINDING #10: cross-check freshness against pid liveness.
    // `pid_is_alive` returns Some(bool) on Unix; None on non-Unix
    // (where we trust the file-freshness rule unchanged).
    let pid_alive = pid_is_alive(st.pid);
    // A daemon is "live" iff the heartbeat is fresh AND we believe the
    // process exists. On Unix: a dead pid invalidates a fresh file —
    // the dogfood reproducer's exact case.
    let fresh = match (file_fresh, pid_alive) {
        (true, Some(true)) => true,
        (true, Some(false)) => false, // stale-via-kernel (#10)
        (true, None) => true,         // non-Unix: trust the file (legacy)
        (false, _) => false,          // heartbeat aged out — stale
    };

    if fresh {
        ui::ok(format!(
            "daemon live — pid {}, verdict {} ({}s ago)",
            st.pid,
            Verdict::parse(&st.verdict_str).as_str(),
            age
        ));
    } else if file_fresh && pid_alive == Some(false) {
        // The dogfood reproducer's exact case: file says fresh (e.g.
        // "(6s ago)"), but kill(pid, 0) says the pid doesn't exist.
        // Be EXPLICIT about why we don't trust the file — a vague
        // "stale" message would suggest the daemon hadn't heartbeated,
        // when in fact the process died. Clarifying the discrepancy
        // is what makes status trustworthy again.
        ui::warn(format!(
            "stale status: pid {} is no longer running (file claims \
             {age}s ago, but the process exited / was killed). \
             `cargoless watch` to restart.",
            st.pid
        ));
    } else {
        // Heartbeat actually aged past STALE_AFTER. The original message.
        ui::warn(format!(
            "stale status (last heartbeat {age}s ago > {}s) — daemon likely \
             stopped; `cargoless watch` to restart.",
            STALE_AFTER.as_secs()
        ));
    }
    ui::step(format!("project: {}", cfg.detection.describe()));
    report_latest_green(&cfg.root);

    if fresh {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    }
}

/// FIELD FINDING #10 (#56): `kill(pid, 0)` liveness probe. Returns:
/// * `Some(true)`  — pid exists and we may signal it (Unix);
/// * `Some(false)` — pid does not exist (Unix);
/// * `None`        — non-Unix target; caller falls back to file-freshness
///   (the legacy v0 behavior — unchanged for any non-Unix port).
///
/// Signal 0 is the POSIX `kill(2)` "probe without signalling" pattern.
/// EPERM (pid exists but we lack permission) is theoretically possible
/// but implausible for cargoless's own daemon under the user's own uid;
/// we treat any non-zero return as "dead" because the conservative
/// outcome (false-stale) is the safer trust answer than the alternative
/// (false-claiming live) — same false-suppress-vs-false-contradict
/// asymmetry that drove #55's classification design.
fn pid_is_alive(pid: u32) -> Option<bool> {
    #[cfg(unix)]
    {
        if pid == 0 {
            // pid 0 is the "every process in our group" target — not a
            // real daemon pid. Defensive against a malformed status file.
            return Some(false);
        }
        unsafe {
            unsafe extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            // r == 0 → exists; r == -1 → ESRCH or EPERM (we treat both
            // as "not our live daemon"; see fn-level comment on the
            // EPERM-implausibility decision).
            Some(kill(pid as i32, 0) == 0)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

// ---------------------------------------------------------------------------
// FIELD FINDING #13a (#93) — dual-watch refusal
//
// Two `cargoless watch` (or a `watch` + `build --watch`) on the SAME
// project root both heartbeat THIS `.cargoless/cli-status` file; `pid`
// then flaps between writers and `status` ambiguously reports whichever
// wrote last. (AC#4's `latest-green` pointer is unaffected — its content
// is input-hash-derived, so concurrent publishers stay consistent; this
// is a `cli-status` ambiguity, not data corruption.)
//
// The fix is a startup admission check: if a status file for this root
// already names a LIVE process that is another instance of THIS binary,
// refuse to start (exit 2) with an actionable message instead of racing.
//
// Refuse-and-exit, not flock: v0 is headless + dependency-minimal. The
// check reuses the #56 `kill(pid,0)` liveness lane plus a nix-free `ps`
// name probe and degrades SAFE — any uncertainty proceeds, never
// false-refusing a legitimate lone watcher (the same
// false-suppress-over-false-contradict asymmetry that drove #55/#10).
// ---------------------------------------------------------------------------

/// A detected live sibling watcher (the "refuse" payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub pid: u32,
    pub root: String,
    pub verdict: Verdict,
    pub age_secs: u64,
}

impl Conflict {
    /// The actionable refusal text (rendered via `ui::error`, so it is
    /// prefixed `xx` and may span lines). Remedies are rename-proof:
    /// `kill <pid>` is exact regardless of the D1 binary name, and the
    /// `--root` alternative names the legitimate concurrent path —
    /// watching two *different* trees at once is fine; only same-root
    /// is the race.
    pub fn message(&self) -> String {
        format!(
            "another cargoless watcher is already running for {} \
             (pid {}, verdict {}, last heartbeat {}s ago).\n  \
             stop it first: `kill {}` — or watch a different tree: \
             `cargoless watch --root <other-dir>`.",
            self.root,
            self.pid,
            self.verdict.as_str(),
            self.age_secs,
            self.pid,
        )
    }
}

/// Startup admission verdict for `watch` / `build --watch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchAdmission {
    /// No live sibling — safe to become the watcher for this root.
    Proceed,
    /// A live sibling watcher already owns this root.
    Refuse(Conflict),
}

/// Pure decision (no I/O) so every branch is unit-tested deterministically.
/// `pid_alive` mirrors [`pid_is_alive`]'s tri-state: `None` (non-Unix)
/// cannot safely refuse and therefore proceeds — the #56 legacy posture.
fn admission_decision(
    existing: Option<&Status>,
    my_pid: u32,
    now: u64,
    pid_alive: Option<bool>,
    same_binary: bool,
) -> WatchAdmission {
    let Some(st) = existing else {
        return WatchAdmission::Proceed; // no prior watcher
    };
    if st.pid == 0 || st.pid == my_pid {
        // Malformed (pid 0) or our own re-read — never a conflict.
        return WatchAdmission::Proceed;
    }
    if pid_alive != Some(true) {
        // Dead pid (the #56 F10 stale-file case) OR non-Unix (None):
        // the prior daemon is gone / unprobable — take over the root.
        return WatchAdmission::Proceed;
    }
    if !same_binary {
        // pid alive but recycled to some OTHER program — not a
        // cargoless watcher; do NOT false-refuse.
        return WatchAdmission::Proceed;
    }
    WatchAdmission::Refuse(Conflict {
        pid: st.pid,
        root: st.root.clone(),
        verdict: Verdict::parse(&st.verdict_str),
        age_secs: now.saturating_sub(st.updated),
    })
}

/// Resolve the I/O probes and apply [`admission_decision`]. Call ONCE at
/// `watch` / `build --watch` startup, BEFORE the costly rust-analyzer
/// bring-up, so a refused start is instant.
pub fn admission(root: &Path, my_pid: u32) -> WatchAdmission {
    let Ok(text) = std::fs::read_to_string(path(root)) else {
        return WatchAdmission::Proceed; // no status file ⇒ no sibling
    };
    let st = Status::parse(&text);
    let alive = pid_is_alive(st.pid);
    // Only run the (process-spawning) binary-identity probe when the pid
    // is actually alive — short-circuits the common stale-file path.
    let same = alive == Some(true) && pid_is_this_binary(st.pid);
    admission_decision(Some(&st), my_pid, now_unix(), alive, same)
}

/// True iff `pid` is running the *same executable as us*. Identity is by
/// binary basename (via [`std::env::current_exe`]) — rename-proof: it
/// asks "is that pid another instance of THIS program?", so the pending
/// D1 binary rename needs no change here.
///
/// nix-free, matching house policy (local-extern for single syscalls —
/// `kill`/`getppid`; `ps` for richer per-pid queries — `pgrep -s` in
/// cargoless_core::analyzer). `ps -p <pid> -o comm=` is portable across the v0
/// targets (Linux + macOS). Any failure ⇒ `false` ⇒ caller proceeds: a
/// missed refusal (rare dual-watch) is strictly safer than false-refusing
/// a legitimate lone watcher.
fn pid_is_this_binary(pid: u32) -> bool {
    #[cfg(unix)]
    {
        match (self_exe_basename(), process_comm(pid)) {
            (Some(mine), Some(reported)) => names_match(&mine, &reported),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(unix)]
fn self_exe_basename() -> Option<String> {
    std::env::current_exe()
        .ok()?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// `ps -p <pid> -o comm=` → the process's command name. macOS may print
/// a full path; we reduce to the file-name. Spawn/exit-status/empty
/// failures all collapse to `None` (caller then proceeds — safe).
#[cfg(unix)]
fn process_comm(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(
        Path::new(raw)
            .file_name()
            .map_or_else(|| raw.to_string(), |n| n.to_string_lossy().into_owned()),
    )
}

/// Compare our binary name to a `ps`-reported `comm`, tolerating the
/// Linux `comm` 15-char truncation (`TASK_COMM_LEN - 1`) WITHOUT a naive
/// generic-prefix match. A bare `mine.starts_with(reported)` would
/// false-match `cargo` against `cargoless` — fatal in a tool that
/// literally runs next to `cargo`. So a prefix only counts when it is
/// *exactly* a 15-char truncation of a longer real name (e.g. the
/// `cargo test` runner binary `tf_cli-<hash>` on the Linux CI builder,
/// which is how this very module's own tests exercise the self-pid).
#[cfg(unix)]
fn names_match(mine: &str, reported: &str) -> bool {
    const LINUX_COMM_MAX: usize = 15; // TASK_COMM_LEN (16) - 1 (NUL)
    if mine == reported {
        return true;
    }
    reported.len() == LINUX_COMM_MAX && mine.len() > LINUX_COMM_MAX && mine.starts_with(reported)
}

/// Report build-cas's latest-green pointer. Its on-disk format is
/// build-cas-owned and the publisher type (#23) is not yet on main, so we do
/// NOT guess a parse: presence is reported honestly; structured fields land
/// when #23's format is pinned.
fn report_latest_green(root: &Path) {
    let p = root.join(".cargoless").join("latest-green");
    if p.exists() {
        ui::ok(format!(
            "latest-green pointer present: {} (fields shown once build-cas \
             #23 publisher format is pinned)",
            p.display()
        ));
    } else {
        ui::wait("latest-green: none yet (no green build published)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_ignores_unknown_keys() {
        let st = Status {
            pid: 4242,
            root: "/p".into(),
            started: 100,
            updated: 200,
            verdict_str: "green".into(),
        };
        assert_eq!(Status::parse(&st.serialize()), st);
        let forward = format!("{}future_key=42\n", st.serialize());
        assert_eq!(Status::parse(&forward), st);
    }

    #[test]
    fn freshness_window() {
        let st = Status {
            updated: 1000,
            ..Default::default()
        };
        assert!(st.is_fresh(1000 + STALE_AFTER.as_secs()));
        assert!(!st.is_fresh(1000 + STALE_AFTER.as_secs() + 1));
    }

    #[test]
    fn verdict_roundtrip() {
        for v in [Verdict::Green, Verdict::Red, Verdict::Unknown] {
            assert_eq!(Verdict::parse(v.as_str()), v);
        }
        assert_eq!(Verdict::parse("garbage"), Verdict::Unknown);
    }

    // -----------------------------------------------------------------------
    // FIELD FINDING #10 (#56) — pid liveness probe via kill(pid, 0)
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn pid_is_alive_returns_true_for_our_own_pid() {
        // The test process itself is the canonical example of "live pid".
        let me = std::process::id();
        assert_eq!(pid_is_alive(me), Some(true));
    }

    #[cfg(unix)]
    #[test]
    fn pid_is_alive_returns_false_for_pid_zero() {
        // Defensive case — pid 0 isn't a real daemon pid (it's the
        // process-group target on kill). A malformed status file with
        // pid=0 must NOT be reported live.
        assert_eq!(pid_is_alive(0), Some(false));
    }

    #[cfg(unix)]
    #[test]
    fn pid_is_alive_returns_false_for_known_dead_pid() {
        // Spawn `true` (exits immediately), wait for it, then probe its
        // pid — guaranteed ESRCH (we reaped it). Deterministic dead-pid
        // case without needing to invent a pid number.
        let mut child = std::process::Command::new("true")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        let _ = child.wait();
        assert_eq!(pid_is_alive(pid), Some(false));
    }

    #[cfg(not(unix))]
    #[test]
    fn pid_is_alive_is_none_on_non_unix() {
        // Non-Unix legacy contract: probe returns None so the caller
        // falls back to file-freshness — same behavior as pre-#56.
        assert_eq!(pid_is_alive(std::process::id()), None);
    }

    #[test]
    fn atomic_write_then_clear() {
        let mut root = std::env::temp_dir();
        root.push(format!("tf-cli-sf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let st = Status {
            pid: 7,
            root: root.display().to_string(),
            started: 1,
            updated: 2,
            verdict_str: "red".into(),
        };
        write(&root, &st);
        let back = Status::parse(&std::fs::read_to_string(path(&root)).unwrap());
        assert_eq!(back, st);
        clear(&root);
        assert!(std::fs::read_to_string(path(&root)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // FIELD FINDING #13a (#93) — dual-watch admission
    // -----------------------------------------------------------------------

    fn st_with(pid: u32, updated: u64, verdict: &str) -> Status {
        Status {
            pid,
            root: "/proj".into(),
            started: 0,
            updated,
            verdict_str: verdict.into(),
        }
    }

    #[test]
    fn admission_proceeds_when_no_status_file() {
        assert_eq!(
            admission_decision(None, 100, 0, Some(true), true),
            WatchAdmission::Proceed
        );
    }

    #[test]
    fn admission_proceeds_on_pid_zero_or_self() {
        // pid 0 is malformed; pid == us is our own re-read — neither is a
        // sibling, regardless of the (irrelevant) liveness/binary probes.
        let z = st_with(0, 10, "green");
        assert_eq!(
            admission_decision(Some(&z), 4242, 100, Some(true), true),
            WatchAdmission::Proceed
        );
        let me = st_with(4242, 10, "green");
        assert_eq!(
            admission_decision(Some(&me), 4242, 100, Some(true), true),
            WatchAdmission::Proceed
        );
    }

    #[test]
    fn admission_proceeds_on_dead_or_unprobable_pid() {
        let s = st_with(777, 10, "green");
        // Dead pid (the #56 F10 stale-file case): take over the root.
        assert_eq!(
            admission_decision(Some(&s), 4242, 100, Some(false), false),
            WatchAdmission::Proceed
        );
        // Non-Unix (None): cannot safely refuse — legacy proceed posture.
        assert_eq!(
            admission_decision(Some(&s), 4242, 100, None, false),
            WatchAdmission::Proceed
        );
    }

    #[test]
    fn admission_proceeds_when_pid_recycled_to_other_program() {
        // pid alive but it's NOT another cargoless — must not false-refuse.
        let s = st_with(777, 10, "green");
        assert_eq!(
            admission_decision(Some(&s), 4242, 100, Some(true), false),
            WatchAdmission::Proceed
        );
    }

    #[test]
    fn admission_refuses_only_live_same_binary_sibling() {
        let s = st_with(777, 40, "red");
        let d = admission_decision(Some(&s), 4242, 100, Some(true), true);
        assert_eq!(
            d,
            WatchAdmission::Refuse(Conflict {
                pid: 777,
                root: "/proj".into(),
                verdict: Verdict::Red,
                age_secs: 60, // now(100) - updated(40)
            })
        );
    }

    #[test]
    fn conflict_message_is_actionable() {
        let c = Conflict {
            pid: 777,
            root: "/proj".into(),
            verdict: Verdict::Green,
            age_secs: 3,
        };
        let m = c.message();
        // Identifies the offender, the tree, the freshness, and BOTH
        // remedies (exact `kill <pid>` + the legitimate --root path).
        assert!(m.contains("/proj"), "names the root: {m}");
        assert!(m.contains("777"), "names the pid: {m}");
        assert!(m.contains("green"), "names the verdict: {m}");
        assert!(m.contains("kill 777"), "exact rename-proof remedy: {m}");
        assert!(m.contains("--root"), "names the concurrent-use path: {m}");
    }

    #[cfg(unix)]
    #[test]
    fn names_match_exact_and_bounded_truncation_only() {
        // Exact.
        assert!(names_match("cargoless", "cargoless"));
        // The critical false-positive guard: `cargo` must NOT match
        // `cargoless` (this tool runs literally next to cargo).
        assert!(!names_match("cargoless", "cargo"));
        assert!(!names_match("cargo", "cargoless"));
        // Genuine Linux 15-char comm truncation of a longer real name
        // (e.g. the `cargo test` runner binary on the CI builder).
        let long = "cargoless-0123456789abcdef"; // 26 chars
        let trunc = &long[..15]; // exactly TASK_COMM_LEN-1
        assert_eq!(trunc.len(), 15);
        assert!(names_match(long, trunc));
        // A short prefix that is NOT a 15-char truncation never matches.
        assert!(!names_match("cargolessXX", "cargoless"));
    }

    #[cfg(unix)]
    #[test]
    fn pid_is_this_binary_true_for_our_own_pid() {
        // Our own process is, by definition, an instance of our own
        // binary. Proves the `ps -o comm=` + current_exe() wiring works
        // on THIS platform — including the Linux comm-truncation path on
        // the CI builder (analog of #56's pid_is_alive self-pid test).
        // If `ps` is unavailable the probe cannot function at all; skip
        // cleanly rather than assert a mechanism the platform lacks
        // (mirrors orphan.rs's no-POSIX-shell skip precedent).
        if process_comm(std::process::id()).is_none() {
            return;
        }
        assert!(pid_is_this_binary(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn admission_end_to_end_refuses_live_self_then_proceeds_on_dead_pid() {
        use std::process::{Command, Stdio};

        let mut root = std::env::temp_dir();
        root.push(format!("tf-cli-f13a-{}-{}", std::process::id(), now_unix()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // (a) status file names a LIVE same-binary process (ourselves);
        // call admission with a DIFFERENT my_pid so WE are the sibling.
        // Skip if `ps` is unavailable (the probe can't run — see above).
        if process_comm(std::process::id()).is_some() {
            write(&root, &st_with(std::process::id(), now_unix(), "green"));
            match admission(&root, std::process::id().wrapping_add(1)) {
                WatchAdmission::Refuse(c) => assert_eq!(c.pid, std::process::id()),
                WatchAdmission::Proceed => {
                    panic!("a live same-binary sibling must be refused")
                }
            }
        }

        // (b) status file names a guaranteed-DEAD pid → proceed (the #56
        // F10 stale-file case, through the real pid_is_alive path).
        let mut dead = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn true");
        let dead_pid = dead.id();
        let _ = dead.wait();
        write(&root, &st_with(dead_pid, now_unix(), "green"));
        assert_eq!(
            admission(&root, std::process::id()),
            WatchAdmission::Proceed
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
