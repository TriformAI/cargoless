//! Checker-mode driver: measure save→verdict latency for one tool.
//!
//! Design:
//!   1. Verify the tool is on PATH (else `Unavailable`).
//!   2. Spawn the tool in checker mode in the fixture cwd. Wait up to
//!      `warm_timeout` for one of the tool's `ready` signals — almost
//!      always its first green compilation report. (A cold Leptos build
//!      is minutes; that wait is unavoidable and matches the AC#2
//!      precondition "warm daemon".)
//!   3. For each rep (with the 1st discarded as a warm-cold spike):
//!        a. Drop in a known-bad edit at `BENCH_TRAIT_ANCHOR`.
//!        b. Time from save to first matching `red` signal. (This is the
//!           authoritative save→verdict edge — going green requires a full
//!           recompile and is the artifact-mode dimension.)
//!        c. Revert the file. Drain quietly until the tool announces a
//!           fresh `green` signal so the NEXT rep starts from a true
//!           steady state. The revert-edge is NOT counted in the latency
//!           samples — only the broken→red edge is.
//!   4. Report MEDIAN + p90 of the broken→red latencies. A miss (no red
//!      within `edit_timeout`) is recorded at the timeout ceiling AND
//!      marked as a fidelity gap so the median is never silently bumped.
//!
//! The "broken→red" edge is the right thing to measure because every tool
//! reports it (it is the failure signal) and it's the one where AC#2's
//! sub-second budget is contested. The "fixed→green" edge depends on the
//! tool's caching/incremental behavior and bleeds into AC#3.
//!
//! The known-bad edit uses the same `BENCH_TRAIT_ANCHOR` the existing
//! `ra-latency` harness uses (`bench/fixture/src/domain/model.rs`), so the
//! two harnesses agree on what counts as a "save". This is plain trait/
//! type code — not a `view!` macro site — to keep the comparative honest
//! across tools (bacon and trunk do not run the same proc-macro path RA
//! does; comparing on a macro-only error would unfairly handicap them).

use std::path::Path;
use std::time::Instant;

use crate::fsutil::{atomic_write, FileGuard};
use crate::modes::{Cfg, RunOutcome};
use crate::proc;
use crate::tools::Tool;

/// The anchor lives in the trait-error scenario file. Keep this exact
/// string in sync with the `ra-latency` harness's `trait_file` constant.
pub const TRAIT_FILE: &str = "src/domain/model.rs";
pub const TRAIT_FIND: &str = "self.entries.len() /* BENCH_TRAIT_ANCHOR */";
pub const TRAIT_BREAK: &str = "self.entries.len_oops() /* BENCH_TRAIT_ANCHOR */";

#[derive(Debug, Clone)]
pub struct CheckerRun {
    pub tool: String,
    pub samples_red_ms: Vec<u64>,
    pub detected_red: usize,
    pub attempted: usize,
    pub outcome: RunOutcome,
    pub warm_secs: f64,
    pub timeout_ms: u64,
}

impl CheckerRun {
    pub fn median_ms(&self) -> Option<u64> {
        crate::stats::median(&self.samples_red_ms)
    }
}

pub fn run(tool: &Tool, fixture: &Path, cfg: &Cfg) -> CheckerRun {
    let timeout_ms = cfg.edit_timeout.as_millis() as u64;
    let blank = CheckerRun {
        tool: tool.name.to_string(),
        samples_red_ms: Vec::new(),
        detected_red: 0,
        attempted: 0,
        outcome: RunOutcome::Measured,
        warm_secs: 0.0,
        timeout_ms,
    };

    if !proc::is_on_path(tool.program()) {
        return CheckerRun {
            outcome: RunOutcome::Unavailable,
            ..blank
        };
    }

    let target = fixture.join(TRAIT_FILE);
    let clean = match std::fs::read_to_string(&target) {
        Ok(s) => s,
        Err(e) => {
            return CheckerRun {
                outcome: RunOutcome::SetupError(format!("read fixture file {:?}: {e}", target)),
                ..blank
            };
        }
    };
    if !clean.contains(TRAIT_FIND) {
        return CheckerRun {
            outcome: RunOutcome::SetupError(format!(
                "anchor {:?} missing from {:?} (fixture drifted)",
                TRAIT_FIND, target
            )),
            ..blank
        };
    }
    let broken = clean.replacen(TRAIT_FIND, TRAIT_BREAK, 1);

    // Restore-on-panic guard.
    let guard = FileGuard::new(target.clone(), clean.clone());

    let args: Vec<&str> = tool.checker_argv[1..].iter().map(String::as_str).collect();
    let mut child = match proc::spawn(tool.name, tool.program(), &args, fixture) {
        Ok(c) => c,
        Err(e) => {
            return CheckerRun {
                outcome: RunOutcome::SetupError(format!("spawn {} failed: {e}", tool.name)),
                ..blank
            };
        }
    };

    // Warm: wait for the first `ready` signal (e.g. cargoless's "verdict
    // pipeline live" or trunk's first "success"). On a cold Leptos build
    // this is the slow part — `warm_timeout` is generous (5min default).
    let warm_start = Instant::now();
    let warm_hit = child.wait_for_any(tool.signals.ready, cfg.warm_timeout);
    if warm_hit.is_none() {
        let warm_secs = warm_start.elapsed().as_secs_f64();
        return CheckerRun {
            outcome: RunOutcome::NoReady,
            warm_secs,
            ..blank
        };
    }
    let warm_secs = warm_start.elapsed().as_secs_f64();

    // Settle: let the watcher's debounce / first-rep noise die down.
    let _ = child.drain_until_quiet(cfg.settle, 256);

    let mut samples: Vec<u64> = Vec::new();
    let mut detected: usize = 0;
    let mut attempted: usize = 0;
    let mut consecutive_misses: usize = 0;
    let mut outcome = RunOutcome::Measured;

    // reps + 1: rep 0 is a warm-cold spike, discarded.
    for rep in 0..(cfg.reps + 1) {
        if let Err(e) = atomic_write(&target, &broken) {
            outcome = RunOutcome::SetupError(format!("write broken: {e}"));
            break;
        }
        let t0 = Instant::now();
        let hit = child.wait_for_any(tool.signals.red, cfg.edit_timeout);
        let elapsed_ms = t0.elapsed().as_millis() as u64;

        // Revert + drain to green so the next rep starts steady.
        if let Err(e) = atomic_write(&target, &clean) {
            outcome = RunOutcome::SetupError(format!("write clean: {e}"));
            break;
        }
        let _ = child.wait_for_any(tool.signals.green, cfg.edit_timeout);
        let _ = child.drain_until_quiet(cfg.settle, 256);

        if rep == 0 {
            continue; // discard warm-cold rep
        }
        attempted += 1;
        if hit.is_some() {
            samples.push(elapsed_ms);
            detected += 1;
            consecutive_misses = 0;
        } else {
            samples.push(timeout_ms); // honest ceiling, not a silent skip
            consecutive_misses += 1;
            if consecutive_misses >= 3 {
                outcome = RunOutcome::NoSignal;
                break;
            }
        }
    }

    child.shutdown();
    drop(guard);

    CheckerRun {
        tool: tool.name.to_string(),
        samples_red_ms: samples,
        detected_red: detected,
        attempted,
        outcome,
        warm_secs,
        timeout_ms,
    }
}
