//! Comparative measurement modes.
//!
//! Two modes, intentionally NEVER blended into one figure:
//!  * `checker`  — save→verdict latency (the AC#2 dimension)
//!  * `artifact` — save→publish latency  (the AC#3 dimension; for the
//!                 cargoless tool this is the moment the
//!                 `.cargoless/latest-green` pointer advances to the new
//!                 input hash)
//!
//! AC#3 is reported in its own line so we never make a sub-second-WASM
//! claim by accident (per docs/EXECUTION.md D-A2 + the v0 scope note).

pub mod artifact;
pub mod checker;

/// Why a per-tool run might not produce a measurement. UNAVAILABLE is the
/// expected case when a comparative tool isn't installed on the runner;
/// NO_READY / NO_SIGNAL surface as honest gaps in the report rather than
/// silent zeros.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// Measurement succeeded; at least one sample was captured.
    Measured,
    /// Tool binary not on PATH (or `--version` failed).
    Unavailable,
    /// Tool started but never reached its `ready` signal in time.
    NoReady,
    /// Tool was warm but never emitted green/red signals after edits — its
    /// output banner words have likely changed.
    NoSignal,
    /// Tool exited unexpectedly during measurement.
    ChildDied,
    /// Filesystem / setup failure (couldn't write the edit, etc.).
    SetupError(String),
}

impl RunOutcome {
    pub fn as_tag(&self) -> String {
        match self {
            RunOutcome::Measured => "MEASURED".to_string(),
            RunOutcome::Unavailable => "UNAVAILABLE".to_string(),
            RunOutcome::NoReady => "NO_READY".to_string(),
            RunOutcome::NoSignal => "NO_SIGNAL".to_string(),
            RunOutcome::ChildDied => "CHILD_DIED".to_string(),
            RunOutcome::SetupError(e) => format!("SETUP_ERR={e}"),
        }
    }
}

/// Shared knobs. Built from env in `bench_main`, plumbed down into both
/// modes so a CI run can override timeouts without recompiling.
#[derive(Debug, Clone)]
pub struct Cfg {
    /// Measurement repetitions (NOT counting the first cold rep, which is
    /// always discarded — matches `ra-latency`'s convention).
    pub reps: usize,
    /// Per-rep timeout: how long we wait for a single verdict/publish edge
    /// after a save before recording a miss.
    pub edit_timeout: std::time::Duration,
    /// How long we wait for the tool to reach its `ready` signal at startup.
    pub warm_timeout: std::time::Duration,
    /// After a save we wait this long for the OS to flush + the tool to
    /// pick the change up; gives a hard lower bound on any measurement and
    /// keeps a busy-loop honest about FS jitter.
    pub settle: std::time::Duration,
}

impl Cfg {
    pub fn default_for_ci() -> Self {
        Self {
            reps: 5,
            // edit_timeout was 60s in #35. The 4th comparative run found
            // it insufficient: cargoless's actual save→verdict on cold-
            // Leptos is ~26s steady-state (post-#49 debouncer is 150ms,
            // but the authoritative cargo-check tier per #55 dominates),
            // and the FIRST edit after warm tends to add cold-fingerprint
            // overhead pushing past 60s. 120s = generous floor that
            // accommodates first-edit cold spike while still surfacing
            // a genuinely-broken signal path as a timeout (not "tool is
            // slow forever").
            edit_timeout: std::time::Duration::from_secs(120),
            // 5min was tight for cold-Leptos bacon (cargo check of ~200
            // crates). 10min gives headroom; run-comparative.sh's
            // explicit `cargo check` pre-warm should make this almost
            // always-fast in practice.
            warm_timeout: std::time::Duration::from_secs(600),
            settle: std::time::Duration::from_millis(250),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_outcome_tags_are_distinct() {
        let tags = [
            RunOutcome::Measured.as_tag(),
            RunOutcome::Unavailable.as_tag(),
            RunOutcome::NoReady.as_tag(),
            RunOutcome::NoSignal.as_tag(),
            RunOutcome::ChildDied.as_tag(),
            RunOutcome::SetupError("x".into()).as_tag(),
        ];
        let set: std::collections::BTreeSet<_> = tags.iter().collect();
        assert_eq!(set.len(), tags.len(), "tags must be distinct: {tags:?}");
    }
}
