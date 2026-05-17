//! Artifact-mode driver: measure save→publish latency for one tool.
//!
//! "Publish" means: the tool's user-visible artifact has been updated. For
//! cargoless that is the `.cargoless/latest-green` pointer changing
//! contents (it stores the new `input_hash`); for trunk that is the dist
//! `index.html` mtime advancing. Bacon has no publish, so it's `N/A`.
//!
//! NEVER blended with checker-mode numbers — AC#3 is its own line. We do
//! NOT claim sub-second latency for the artifact dimension; the report is
//! the number, and the comparative judgement is "did cargoless beat trunk".
//!
//! Edit shape: we use a SEMANTICALLY-MEANINGLESS, GREEN-PRESERVING edit
//! (toggle a single comment-suffix on a stable line). This is the
//! realistic ergonomic case (a save during normal editing that keeps the
//! tree green); a save that goes red doesn't trigger a re-publish (AC#4
//! fail-closed), so timing "publish" off it would be measuring the
//! holding behavior, not the publish behavior. The edit is byte-different
//! AND AST-identical — every comparative tool sees the same save event.
//!
//! Witness reading: we snapshot the witness BEFORE the save and poll
//! AFTER the save (cheap stat or short file read). A change vs. baseline
//! = publish happened. Polling interval is shorter than the smallest
//! meaningful latency unit (50ms) so it doesn't bias the measurement.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crate::fsutil::{atomic_write, FileGuard};
use crate::modes::{Cfg, RunOutcome};
use crate::proc;
use crate::tools::{PublishWitness, Tool};

/// Same anchor file as the checker driver; we just change the edit shape.
pub const ARTIFACT_FILE: &str = "src/domain/model.rs";
pub const ARTIFACT_FIND: &str = "self.entries.len() /* BENCH_TRAIT_ANCHOR */";
/// AST-identical alternations: only the trailing comment differs.
pub const ARTIFACT_FLIP_A: &str = "self.entries.len() /* BENCH_TRAIT_ANCHOR */ /* bench-flip:a */";
pub const ARTIFACT_FLIP_B: &str = "self.entries.len() /* BENCH_TRAIT_ANCHOR */ /* bench-flip:b */";

const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct ArtifactRun {
    pub tool: String,
    pub samples_ms: Vec<u64>,
    pub detected: usize,
    pub attempted: usize,
    pub outcome: RunOutcome,
    pub warm_secs: f64,
    pub timeout_ms: u64,
}

impl ArtifactRun {
    pub fn median_ms(&self) -> Option<u64> {
        crate::stats::median(&self.samples_ms)
    }
}

pub fn run(tool: &Tool, fixture: &Path, cfg: &Cfg) -> ArtifactRun {
    let timeout_ms = cfg.edit_timeout.as_millis() as u64;
    let blank = ArtifactRun {
        tool: tool.name.to_string(),
        samples_ms: Vec::new(),
        detected: 0,
        attempted: 0,
        outcome: RunOutcome::Measured,
        warm_secs: 0.0,
        timeout_ms,
    };

    let Some(argv) = tool.artifact_argv.as_ref() else {
        return ArtifactRun {
            outcome: RunOutcome::Unavailable,
            ..blank
        };
    };
    if matches!(tool.artifact_witness, PublishWitness::None) {
        return ArtifactRun {
            outcome: RunOutcome::Unavailable,
            ..blank
        };
    }
    if !proc::is_on_path(&argv[0]) {
        return ArtifactRun {
            outcome: RunOutcome::Unavailable,
            ..blank
        };
    }

    let target = fixture.join(ARTIFACT_FILE);
    let clean = match std::fs::read_to_string(&target) {
        Ok(s) => s,
        Err(e) => {
            return ArtifactRun {
                outcome: RunOutcome::SetupError(format!("read fixture: {e}")),
                ..blank
            };
        }
    };
    if !clean.contains(ARTIFACT_FIND) {
        return ArtifactRun {
            outcome: RunOutcome::SetupError(format!(
                "anchor {ARTIFACT_FIND:?} missing — fixture drifted"
            )),
            ..blank
        };
    }
    let guard = FileGuard::new(target.clone(), clean.clone());

    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let mut child = match proc::spawn(tool.name, &argv[0], &args, fixture) {
        Ok(c) => c,
        Err(e) => {
            return ArtifactRun {
                outcome: RunOutcome::SetupError(format!("spawn {} failed: {e}", tool.name)),
                ..blank
            };
        }
    };

    // Warm: wait for the first ready signal. For cargoless that's "GREEN
    // — building"; for trunk it's the first "success" line.
    let warm_start = Instant::now();
    let warm_hit = child.wait_for_any(tool.signals.ready, cfg.warm_timeout);
    if warm_hit.is_none() {
        let warm_secs = warm_start.elapsed().as_secs_f64();
        return ArtifactRun {
            outcome: RunOutcome::NoReady,
            warm_secs,
            ..blank
        };
    }
    // Wait a beat: for cargoless, the "GREEN — building" line precedes the
    // pointer write — give the publisher 1 settle window to actually
    // produce the artifact before we start sampling.
    let _ = child.drain_until_quiet(cfg.settle, 256);
    // Snapshot witness baseline.
    let mut baseline = match read_witness(fixture, &tool.artifact_witness) {
        Ok(w) => w,
        Err(e) => {
            return ArtifactRun {
                outcome: RunOutcome::SetupError(format!("witness baseline: {e}")),
                ..blank
            };
        }
    };
    let warm_secs = warm_start.elapsed().as_secs_f64();

    let mut samples: Vec<u64> = Vec::new();
    let mut detected: usize = 0;
    let mut attempted: usize = 0;
    let mut consecutive_misses: usize = 0;
    let mut outcome = RunOutcome::Measured;

    for rep in 0..(cfg.reps + 1) {
        // `% 2` (not `is_multiple_of`) — the latter is post-MSRV-1.85.
        let flip = if rep % 2 == 0 {
            ARTIFACT_FLIP_A
        } else {
            ARTIFACT_FLIP_B
        };
        let body = clean.replacen(ARTIFACT_FIND, flip, 1);
        if let Err(e) = atomic_write(&target, &body) {
            outcome = RunOutcome::SetupError(format!("write flip: {e}"));
            break;
        }
        let t0 = Instant::now();
        let hit = poll_until_change(fixture, &tool.artifact_witness, &baseline, cfg.edit_timeout);
        let elapsed_ms = t0.elapsed().as_millis() as u64;

        if let Some(new_witness) = hit.as_ref() {
            baseline = new_witness.clone();
        }

        if rep == 0 {
            continue;
        }
        attempted += 1;
        if hit.is_some() {
            samples.push(elapsed_ms);
            detected += 1;
            consecutive_misses = 0;
        } else {
            samples.push(timeout_ms);
            consecutive_misses += 1;
            if consecutive_misses >= 3 {
                outcome = RunOutcome::NoSignal;
                break;
            }
        }
    }

    child.shutdown();
    drop(guard);

    ArtifactRun {
        tool: tool.name.to_string(),
        samples_ms: samples,
        detected,
        attempted,
        outcome,
        warm_secs,
        timeout_ms,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WitnessSnap {
    Mtime(Option<SystemTime>),
    Contents(Vec<u8>),
}

fn read_witness(root: &Path, w: &PublishWitness) -> std::io::Result<WitnessSnap> {
    match w {
        PublishWitness::FileMtime(rel) => {
            let p = root.join(rel);
            match std::fs::metadata(&p) {
                Ok(m) => Ok(WitnessSnap::Mtime(m.modified().ok())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(WitnessSnap::Mtime(None)),
                Err(e) => Err(e),
            }
        }
        PublishWitness::FileContents(rel) => {
            let p = root.join(rel);
            match std::fs::read(&p) {
                Ok(b) => Ok(WitnessSnap::Contents(b)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Ok(WitnessSnap::Contents(Vec::new()))
                }
                Err(e) => Err(e),
            }
        }
        PublishWitness::None => Ok(WitnessSnap::Contents(Vec::new())),
    }
}

fn poll_until_change(
    root: &Path,
    w: &PublishWitness,
    baseline: &WitnessSnap,
    timeout: Duration,
) -> Option<WitnessSnap> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(now) = read_witness(root, w) {
            if &now != baseline {
                return Some(now);
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL.min(left));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch_dir(line: u32) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cbench-art-{}-{}", std::process::id(), line));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_witness_missing_file_is_valid_baseline_mtime() {
        let dir = scratch_dir(line!());
        let w = PublishWitness::FileMtime("never-existed.txt".into());
        let snap = read_witness(&dir, &w).unwrap();
        assert_eq!(snap, WitnessSnap::Mtime(None));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_witness_missing_file_is_valid_baseline_contents() {
        let dir = scratch_dir(line!());
        let w = PublishWitness::FileContents("never-existed.txt".into());
        let snap = read_witness(&dir, &w).unwrap();
        assert_eq!(snap, WitnessSnap::Contents(Vec::new()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn poll_returns_none_on_timeout_with_no_change() {
        let dir = scratch_dir(line!());
        let rel = std::path::PathBuf::from("stable.txt");
        fs::write(dir.join(&rel), b"x").unwrap();
        let w = PublishWitness::FileContents(rel);
        let baseline = read_witness(&dir, &w).unwrap();
        let out = poll_until_change(&dir, &w, &baseline, Duration::from_millis(120));
        assert!(out.is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn poll_detects_contents_change() {
        let dir = scratch_dir(line!());
        let rel = std::path::PathBuf::from("changing.txt");
        fs::write(dir.join(&rel), b"v1").unwrap();
        let w = PublishWitness::FileContents(rel.clone());
        let baseline = read_witness(&dir, &w).unwrap();
        let dir2 = dir.clone();
        let rel2 = rel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            fs::write(dir2.join(rel2), b"v2").unwrap();
        });
        let out = poll_until_change(&dir, &w, &baseline, Duration::from_secs(2));
        assert!(matches!(out, Some(WitnessSnap::Contents(ref b)) if b == b"v2"));
        fs::remove_dir_all(&dir).ok();
    }
}
