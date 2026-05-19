//! The cargoless-owned daemon-status file (lead RULING 1).
//!
//! `ModelSession` is in-process only — a separate `status` invocation cannot
//! call `tree_state()`. So the running `watch` / `build --watch` process
//! writes this small file (liveness heartbeat + current verdict) and
//! `status` reads it. This is DISTINCT from build-cas's
//! `.cargoless/latest-green` (latest *green* artifact pointer, build-cas
//! owned/format-owned). cargoless owns and documents *this* file's format.
//!
//! ## Format (`<root>/.cargoless/cli-status`) — documented contract
//!
//! ```text
//! schema=2
//! pid=<u32>
//! root=<canonical project root>
//! started=<unix seconds>
//! updated=<unix seconds>          # heartbeat; freshness = liveness signal
//! verdict=green|red|unknown       # authoritative tree verdict at last update
//! crates=<name>:<v>,<name>:<v>    # schema=2, OPTIONAL — see below
//! red_diagnostics=<u32>           # schema=2 — count of error-severity diags
//! ```
//!
//! **schema=2 (Model R #9, `D-FLEET-SHARED-DAEMON` §9):** adds the
//! OPTIONAL `crates=` per-crate verdict roll-up + the `red_diagnostics=`
//! scalar. Backward-compatible **both ways**: a schema=1 reader ignores
//! the new keys (the parser has no `schema=` arm and skips unknown keys —
//! proven by `roundtrips_and_ignores_unknown_keys`); a schema=2 reader of
//! an old schema=1 file simply sees an absent `crates=`/`red_diagnostics=`
//! (⇒ empty map, zero count). The `verdict=` line is **always** the
//! authoritative tree verdict and stands alone; `crates=` is written
//! **only** when every error diagnostic was attributable to a known
//! workspace crate (else omitted — never a false per-crate all-green; see
//! [`crate::cratemap`]).
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
    /// schema=2 (#9): per-crate verdict roll-up, `(crate_name, verdict)`
    /// in stable sorted order. Empty for a schema=1 file, a single-crate
    /// project, or when the per-crate map could not be fully trusted
    /// (an unattributable error — see [`crate::cratemap`]); the `crates=`
    /// line is then omitted on serialize and the authoritative
    /// `verdict_str` stands alone.
    pub crates: Vec<(String, Verdict)>,
    /// schema=2 (#9): count of error-severity diagnostics — the
    /// asymmetric-stream "how bad is red" scalar (`D-FLEET-SHARED-DAEMON`
    /// §9.2). Zero on green and on a schema=1 file.
    pub red_diagnostics: u32,
    /// #247 in-scope obs fold: unix-seconds timestamp of the
    /// AUTHORITATIVE analysis that produced this verdict (= the
    /// barrier-settle instant observed at `publish_verdict`). Distinct
    /// from `updated` (the statusfile-write instant): if a future
    /// heartbeat-refresh path were to update `updated` without re-checking,
    /// `analysed_at` would stay put at the original settle time — so a
    /// diagnostician can answer "is this verdict from a real recent check
    /// or just a heartbeat refresh?" by comparing `analysed_at` to
    /// `updated`. Closes the AC4-class diagnosis gap that
    /// `serve.out`-bring-up-banner-only could not resolve (partial #243
    /// close; full OTEL+SigNoz spans land in #246). Zero on schema=1 or
    /// pre-#247 files (forward-compatible default).
    pub analysed_at: u64,
}

pub fn path(root: &Path) -> PathBuf {
    root.join(".cargoless").join("cli-status")
}

/// Parse the schema=2 `crates=` value (`name:verdict,name:verdict`).
/// Empty ⇒ empty vec. Tolerant: a token without a `:` is skipped; an
/// unrecognised verdict maps to [`Verdict::Unknown`] (via
/// [`Verdict::parse`]) rather than dropping the crate — a reader should
/// still see that the crate exists.
fn parse_crates(v: &str) -> Vec<(String, Verdict)> {
    if v.is_empty() {
        return Vec::new();
    }
    v.split(',')
        .filter_map(|tok| {
            let (name, verdict) = tok.trim().split_once(':')?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some((name.to_string(), Verdict::parse(verdict.trim())))
        })
        .collect()
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Status {
    pub fn serialize(&self) -> String {
        // The first 6 lines are byte-identical to schema=1 except the
        // `schema` value — a schema=1 reader never matches `schema=` (no
        // arm) and ignores the trailing schema=2 keys, so old consumers
        // are unaffected. `crates=` is emitted ONLY when non-empty: an
        // empty map means "no trustworthy per-crate breakdown", and an
        // absent line (not `crates=`) is the unambiguous signal for that.
        let mut out = format!(
            "schema=2\npid={}\nroot={}\nstarted={}\nupdated={}\nverdict={}\n",
            self.pid, self.root, self.started, self.updated, self.verdict_str
        );
        if !self.crates.is_empty() {
            let joined = self
                .crates
                .iter()
                .map(|(n, v)| format!("{n}:{}", v.as_str()))
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&format!("crates={joined}\n"));
        }
        out.push_str(&format!("red_diagnostics={}\n", self.red_diagnostics));
        // #247: analysed_at emitted unconditionally (zero on green/never-
        // checked is meaningful — distinguishes "no authoritative check
        // yet" from a stale "recently re-heartbeated" state). Forward-
        // compatible: a schema=2 reader pre-#247 will ignore the unknown
        // `analysed_at` key (the parse-arm-driven discipline).
        out.push_str(&format!("analysed_at={}\n", self.analysed_at));
        out
    }

    /// Parse the documented format. Unknown keys ignored
    /// (forward-compatible); absent schema=2 keys ⇒ empty/zero
    /// (schema=1-file-compatible). No `schema=` arm by design — the schema
    /// number is advisory; field presence is authoritative.
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
                "crates" => s.crates = parse_crates(v.trim()),
                "red_diagnostics" => s.red_diagnostics = v.trim().parse().unwrap_or(0),
                "analysed_at" => s.analysed_at = v.trim().parse().unwrap_or(0),
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

/// `status --remote <url>` — query a remote `serve --bind` fleet daemon
/// over the **shipped** HTTP transport instead of the on-disk
/// `cli-status`. Increment 0 (Model R #10): the client half of the
/// read-plane wire (the server half is [`crate::serveapi`]).
///
/// Routes through the shipped `transport::discovery` precedence
/// (`--remote` is §10.3 step 1 — explicit operator intent beats
/// socket/file/spawn) + the shipped `HttpClient` / `TransportClient`; no
/// bespoke HTTP. Exit codes mirror the local [`run_status`] contract so a
/// script gates identically regardless of transport:
/// * `0` — daemon reachable (the remote fleet is up);
/// * `3` — no reachable daemon at `url` (the local "no/stale daemon"
///   analogue — fall-through-safe, never a panic);
/// * `2` — `url` is not a usable remote URL (setup error).
pub fn run_status_remote(url: &str) -> ExitCode {
    use cargoless_core::transport::TransportClient;
    use cargoless_core::transport::discovery::{Resolution, resolve};
    use cargoless_core::transport::http::HttpClient;

    // Shipped discovery precedence: an explicit `--remote` resolves to the
    // HTTP transport (beats socket/file/spawn — §10.3 step 1). Going
    // through `resolve` (not bypassing it) is the point: this IS the
    // shipped read path.
    let Resolution::Remote(target) = resolve(Some(url), None, None) else {
        ui::error(format!(
            "--remote {url}: not a resolvable remote URL (expected http://host:port)"
        ));
        return ExitCode::from(2);
    };
    let client = match HttpClient::new(&target) {
        Ok(c) => c,
        Err(e) => {
            ui::error(format!("--remote {target}: {e}"));
            return ExitCode::from(2);
        }
    };
    match client.list_worktrees() {
        Ok(list) if !list.is_empty() => {
            ui::ok(format!(
                "remote daemon {target} live — {} worktree(s):",
                list.len()
            ));
            for w in &list {
                ui::step(format!(
                    "{}  verdict {}  ({} red diagnostic{})",
                    w.worktree,
                    w.verdict,
                    w.red_diagnostics,
                    if w.red_diagnostics == 1 { "" } else { "s" }
                ));
            }
            ExitCode::SUCCESS
        }
        Ok(_) => {
            // Reachable but no verdict attributed yet (a cold daemon — no
            // worktree has settled a flycheck pass). Still "live".
            ui::ok(format!(
                "remote daemon {target} reachable — no worktree verdicts yet"
            ));
            ExitCode::SUCCESS
        }
        Err(e) => {
            ui::warn(format!(
                "no reachable cargoless daemon at {target} ({e}) — is \
                 `serve --repo <dir> --bind <host:port>` running there?"
            ));
            ExitCode::from(3)
        }
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
    /// No prior `.cargoless/cli-status` at all — safe clean start.
    Proceed,
    /// #128: a prior `cli-status` EXISTED but its owner is **not a
    /// demonstrably-live cargoless watcher** (SIGKILL'd-then-stale,
    /// zombie, dead pid, or a pid reused by an unrelated process). The
    /// orphaned file is taken over and the caller logs an actionable
    /// recovery note — **never a bare refuse** (the agent-fleet bug:
    /// fleets hard-kill daemons routinely). `stale_pid` is the dead /
    /// not-our-live-daemon pid the old file named.
    Recover { stale_pid: u32 },
    /// A genuinely-live sibling cargoless watcher (heartbeat-fresh AND
    /// pid-alive AND same-binary) already owns this root.
    Refuse(Conflict),
}

/// Pure decision (no I/O) so every branch is unit-tested
/// deterministically.
///
/// #128: this now applies the **same F10 composite `run_status` uses** —
/// the prior watcher counts as a genuinely-live sibling (⇒ `Refuse`)
/// ONLY if its `cli-status` heartbeat is still fresh **AND** its pid is
/// alive **AND** it is the same binary. A live cargoless watcher
/// rewrites `updated` every [`HEARTBEAT`]; a SIGKILL'd one's heartbeat
/// freezes and ages past [`STALE_AFTER`]. So any of {stale heartbeat,
/// dead pid, pid reused by an unrelated process} ⇒ the prior daemon is
/// gone ⇒ `Recover` (take over the orphaned file + log it), **never a
/// bare refuse**. `pid_alive == None` (non-Unix, unprobable) keeps the
/// #56 legacy `Proceed` posture unchanged.
fn admission_decision(
    existing: Option<&Status>,
    my_pid: u32,
    now: u64,
    file_fresh: bool,
    pid_alive: Option<bool>,
    same_binary: bool,
) -> WatchAdmission {
    let Some(st) = existing else {
        return WatchAdmission::Proceed; // no prior watcher at all
    };
    if st.pid == 0 || st.pid == my_pid {
        // Malformed (pid 0) or our own re-read — not a conflict, and
        // not an orphan to "recover" from either.
        return WatchAdmission::Proceed;
    }
    if pid_alive.is_none() {
        // Non-Unix: we cannot probe liveness, so we can neither refuse
        // nor claim the file is stale. Preserve the #56 legacy posture
        // exactly (Proceed, no recovery note).
        return WatchAdmission::Proceed;
    }
    // Unix from here. The prior watcher is a genuinely-live sibling iff
    // ALL THREE hold (the run_status F10 composite + same-binary):
    // heartbeat fresh, pid alive, same binary. Anything else means the
    // recorded daemon is NOT actually watching this root anymore.
    let demonstrably_live = file_fresh && pid_alive == Some(true) && same_binary;
    if demonstrably_live {
        WatchAdmission::Refuse(Conflict {
            pid: st.pid,
            root: st.root.clone(),
            verdict: Verdict::parse(&st.verdict_str),
            age_secs: now.saturating_sub(st.updated),
        })
    } else {
        // SIGKILL'd-then-stale, zombie, dead pid, or pid reused by an
        // unrelated process: the file is orphaned. Take it over with an
        // actionable recovery note — the agent-fleet bug fix (#128).
        WatchAdmission::Recover { stale_pid: st.pid }
    }
}

/// Resolve the I/O probes and apply [`admission_decision`]. Call ONCE at
/// `watch` / `build --watch` startup, BEFORE the costly rust-analyzer
/// bring-up, so a refused start (or stale-file recovery) is instant.
///
/// #128: `file_fresh` is computed from the heartbeat exactly as
/// [`run_status`] does, so `watch` and `status` now AGREE on "is the
/// prior daemon really there". The binary-identity probe runs only when
/// the heartbeat is fresh AND the pid is alive — a stale or dead entry
/// is recovered without spawning `ps` at all.
pub fn admission(root: &Path, my_pid: u32) -> WatchAdmission {
    let Ok(text) = std::fs::read_to_string(path(root)) else {
        return WatchAdmission::Proceed; // no status file ⇒ clean start
    };
    let st = Status::parse(&text);
    let now = now_unix();
    let file_fresh = st.is_fresh(now);
    let alive = pid_is_alive(st.pid);
    // Probe binary identity only for a fresh + live entry — a stale or
    // dead pid is recovered without the (process-spawning) `ps` probe.
    let same = file_fresh && alive == Some(true) && pid_is_this_binary(st.pid);
    admission_decision(Some(&st), my_pid, now, file_fresh, alive, same)
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
/// `cargo test` runner binary `cargoless-<hash>` on the Linux CI builder,
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
            crates: vec![],
            red_diagnostics: 0,
            analysed_at: 0,
        };
        assert_eq!(Status::parse(&st.serialize()), st);
        let forward = format!("{}future_key=42\n", st.serialize());
        assert_eq!(Status::parse(&forward), st);
    }

    // -----------------------------------------------------------------------
    // Model R #9 — schema=2 per-crate verdicts + red_diagnostics
    // -----------------------------------------------------------------------

    #[test]
    fn schema2_roundtrips_with_crates_and_red_count() {
        let st = Status {
            pid: 9,
            root: "/ws".into(),
            started: 1,
            updated: 2,
            verdict_str: "red".into(),
            crates: vec![
                ("isolation".into(), Verdict::Green),
                ("physics".into(), Verdict::Red),
            ],
            red_diagnostics: 2,
            analysed_at: 0,
        };
        let wire = st.serialize();
        assert!(wire.starts_with("schema=2\n"), "schema bumped: {wire}");
        assert!(
            wire.contains("crates=isolation:green,physics:red\n"),
            "per-crate line: {wire}"
        );
        assert!(wire.contains("red_diagnostics=2\n"), "scalar: {wire}");
        assert_eq!(Status::parse(&wire), st, "exact schema=2 roundtrip");
    }

    #[test]
    fn schema1_file_reads_as_empty_per_crate() {
        // A literal pre-#9 schema=1 file: a schema=2 reader must see an
        // absent crates=/red_diagnostics= as empty/zero, NOT fail.
        let legacy = "schema=1\npid=42\nroot=/p\nstarted=10\nupdated=20\nverdict=green\n";
        let s = Status::parse(legacy);
        assert_eq!(s.pid, 42);
        assert_eq!(s.verdict_str, "green");
        assert!(s.crates.is_empty(), "no crates= ⇒ empty, not error");
        assert_eq!(s.red_diagnostics, 0, "no red_diagnostics= ⇒ 0");
    }

    #[test]
    fn empty_per_crate_omits_the_crates_line() {
        // The honesty invariant at the serialization boundary: an empty
        // map (untrustworthy / single-crate / schema=1) must NOT emit a
        // bare `crates=` that a reader could mistake for "zero crates";
        // the line is absent entirely and `verdict=` stands alone.
        let st = Status {
            pid: 1,
            root: "/p".into(),
            started: 0,
            updated: 0,
            verdict_str: "red".into(),
            crates: vec![],
            red_diagnostics: 3,
            analysed_at: 0,
        };
        let wire = st.serialize();
        assert!(!wire.contains("crates="), "no crates line: {wire}");
        assert!(wire.contains("verdict=red\n"), "verdict still stands");
        assert!(wire.contains("red_diagnostics=3\n"));
        assert_eq!(Status::parse(&wire), st);
    }

    #[test]
    fn crates_parser_is_tolerant() {
        // Unknown verdict ⇒ Unknown (crate still visible); a colon-less
        // token is skipped; empty value ⇒ empty vec.
        assert_eq!(parse_crates(""), vec![]);
        assert_eq!(
            parse_crates("a:green,bogus,b:weird"),
            vec![
                ("a".to_string(), Verdict::Green),
                ("b".to_string(), Verdict::Unknown),
            ]
        );
    }

    #[test]
    fn schema1_reader_simulation_ignores_schema2_keys() {
        // Prove the both-ways claim: emulate a schema=1 consumer (only the
        // 5 v0 keys) reading a schema=2 blob — it must recover the v0
        // fields untouched and never trip on crates=/red_diagnostics=.
        let st = Status {
            pid: 7,
            root: "/r".into(),
            started: 3,
            updated: 4,
            verdict_str: "green".into(),
            crates: vec![("x".into(), Verdict::Green)],
            red_diagnostics: 0,
            analysed_at: 0,
        };
        let wire = st.serialize();
        // The schema=1-era parser was exactly today's parser minus the
        // two new arms; its output for the v0 fields is unchanged.
        let v0 = Status::parse(&wire);
        assert_eq!(v0.pid, 7);
        assert_eq!(v0.root, "/r");
        assert_eq!(v0.started, 3);
        assert_eq!(v0.updated, 4);
        assert_eq!(v0.verdict_str, "green");
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
    // #247 — analysed_at obs field (distinct-from-updated semantics)
    // -----------------------------------------------------------------------

    #[test]
    fn analysed_at_roundtrips_with_nonzero_value() {
        // The #247 obs fold: analysed_at is the barrier-settle instant
        // (publish_verdict's `now` at the EmitVerdict arm) — distinct
        // from `updated` (statusfile-write instant). Test a non-zero
        // value to prove ser/parse symmetry on the new field (the
        // existing roundtrip tests all use 0 from the Default).
        let st = Status {
            pid: 42,
            root: "/p".into(),
            started: 100,
            updated: 250,
            verdict_str: "green".into(),
            crates: vec![],
            red_diagnostics: 0,
            analysed_at: 200, // settled 50s before write (meaningful gap)
        };
        let wire = st.serialize();
        assert!(wire.contains("analysed_at=200\n"), "emitted: {wire}");
        assert_eq!(
            Status::parse(&wire),
            st,
            "exact roundtrip incl. analysed_at"
        );
    }

    #[test]
    fn pre_247_file_without_analysed_at_defaults_to_zero() {
        // Forward-compatibility: a pre-#247 statusfile (no `analysed_at=`
        // line) parses with analysed_at=0 — meaning "no #247-aware
        // authoritative-check-instant recorded." Mirrors the schema=1
        // backward-compat discipline.
        let pre247 = "schema=2\npid=1\nroot=/p\nstarted=0\nupdated=10\n\
                      verdict=green\nred_diagnostics=0\n";
        let s = Status::parse(pre247);
        assert_eq!(s.analysed_at, 0, "absent ⇒ 0, not error");
        assert_eq!(s.verdict_str, "green");
        assert_eq!(s.updated, 10);
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
        root.push(format!("cargoless-sf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let st = Status {
            pid: 7,
            root: root.display().to_string(),
            started: 1,
            updated: 2,
            verdict_str: "red".into(),
            crates: vec![],
            red_diagnostics: 0,
            analysed_at: 0,
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
            crates: vec![],
            red_diagnostics: 0,
            analysed_at: 0,
        }
    }

    // admission_decision(existing, my_pid, now, file_fresh, pid_alive,
    // same_binary). #128: Refuse ONLY a demonstrably-live sibling
    // (file_fresh ∧ pid_alive==Some(true) ∧ same_binary); every other
    // prior-status case Recovers (take over + actionable note), never
    // bare-Proceeds-silently and never bare-Refuses.

    #[test]
    fn admission_proceeds_when_no_status_file() {
        // No prior file at all ⇒ clean start (not a "recovery").
        assert_eq!(
            admission_decision(None, 100, 0, true, Some(true), true),
            WatchAdmission::Proceed
        );
    }

    #[test]
    fn admission_proceeds_on_pid_zero_or_self() {
        // pid 0 malformed / pid == us: neither a sibling nor an orphan.
        let z = st_with(0, 10, "green");
        assert_eq!(
            admission_decision(Some(&z), 4242, 100, true, Some(true), true),
            WatchAdmission::Proceed
        );
        let me = st_with(4242, 10, "green");
        assert_eq!(
            admission_decision(Some(&me), 4242, 100, true, Some(true), true),
            WatchAdmission::Proceed
        );
    }

    #[test]
    fn admission_non_unix_keeps_legacy_proceed() {
        // pid_alive == None (non-Unix): unprobable — preserve the #56
        // legacy posture EXACTLY (Proceed, no recovery claim).
        let s = st_with(777, 10, "green");
        assert_eq!(
            admission_decision(Some(&s), 4242, 100, true, None, false),
            WatchAdmission::Proceed
        );
    }

    #[test]
    fn admission_recovers_on_dead_pid() {
        // #128: dead pid + a prior cli-status present ⇒ orphaned file;
        // take it over with an actionable recovery note (was a bare
        // Proceed pre-#128).
        let s = st_with(777, 10, "green");
        assert_eq!(
            admission_decision(Some(&s), 4242, 100, true, Some(false), false),
            WatchAdmission::Recover { stale_pid: 777 }
        );
    }

    #[test]
    fn admission_recovers_when_pid_reused_by_other_program() {
        // pid alive but NOT a cargoless (reused by some unrelated
        // process): the prior daemon is gone — recover, never refuse.
        let s = st_with(777, 10, "green");
        assert_eq!(
            admission_decision(Some(&s), 4242, 100, true, Some(true), false),
            WatchAdmission::Recover { stale_pid: 777 }
        );
    }

    #[test]
    fn admission_recovers_on_stale_heartbeat_even_if_pid_alive_same_binary() {
        // THE #128 CORE FIX. A SIGKILL'd-then-zombie (or pid reused by
        // a *sibling* cargoless on a fleet box) looks pid-alive +
        // same-binary, but our cli-status heartbeat is STALE — a
        // genuinely-live watcher would have refreshed it every
        // HEARTBEAT. Pre-#128 this bare-refused; now it recovers,
        // matching what `run_status` (F10) already reports.
        let s = st_with(777, 40, "red");
        assert_eq!(
            admission_decision(
                Some(&s),
                4242,
                100,
                /*file_fresh=*/ false,
                Some(true),
                /*same_binary=*/ true,
            ),
            WatchAdmission::Recover { stale_pid: 777 }
        );
    }

    #[test]
    fn admission_refuses_only_fresh_live_same_binary_sibling() {
        // The ONLY Refuse: heartbeat-fresh ∧ pid-alive ∧ same-binary —
        // a genuinely-live concurrent watcher (the real #93 dual-watch).
        let s = st_with(777, 40, "red");
        assert_eq!(
            admission_decision(Some(&s), 4242, 100, true, Some(true), true),
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
    fn admission_e2e_refuses_fresh_live_self_then_recovers_dead_pid() {
        use std::process::{Command, Stdio};

        let mut root = std::env::temp_dir();
        root.push(format!(
            "cargoless-128-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // (a) status names a LIVE same-binary process (ourselves) with a
        // JUST-written (fresh) heartbeat ⇒ a genuinely-live sibling ⇒
        // Refuse. Call with a DIFFERENT my_pid so WE are the "sibling".
        // Skip if `ps` is unavailable (the probe can't run).
        if process_comm(std::process::id()).is_some() {
            write(&root, &st_with(std::process::id(), now_unix(), "green"));
            match admission(&root, std::process::id().wrapping_add(1)) {
                WatchAdmission::Refuse(c) => assert_eq!(c.pid, std::process::id()),
                other => {
                    panic!("a fresh + live + same-binary sibling must be REFUSED, got {other:?}")
                }
            }
        }

        // (b) #128: status names a guaranteed-DEAD pid ⇒ the orphaned
        // file is RECOVERED (not a bare Proceed, never a Refuse) —
        // through the real pid_is_alive + freshness path. This is the
        // agent-fleet SIGKILL'd-daemon case.
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
            WatchAdmission::Recover {
                stale_pid: dead_pid
            },
            "a SIGKILL'd/dead prior daemon must auto-recover, never refuse"
        );

        // (c) #128 LITERAL BUG: status names a pid that is alive AND
        // the same binary (ourselves) BUT the heartbeat is STALE (the
        // SIGKILL'd-then-zombie / pid-reused-by-sibling-cargoless
        // fleet case). Pre-#128 this bare-refused; it MUST recover —
        // and agree with what `run_status`/F10 reports for the same
        // file. Skip if `ps` unavailable (probe can't run).
        if process_comm(std::process::id()).is_some() {
            let stale_updated = now_unix().saturating_sub(STALE_AFTER.as_secs() + 5);
            write(&root, &st_with(std::process::id(), stale_updated, "green"));
            assert_eq!(
                admission(&root, std::process::id().wrapping_add(1)),
                WatchAdmission::Recover {
                    stale_pid: std::process::id()
                },
                "alive+same-binary but STALE heartbeat ⇒ recover, never \
                 bare-refuse (the #128 agent-fleet bug)"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
