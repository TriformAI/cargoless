//! `appstatefile` — the durable, human-inspectable per-instance state mirror.
//!
//! The daemon writes one file per instance at
//! `<state_dir>/app/<instance>/state` on every lifecycle transition (and a
//! periodic heartbeat). It is a *mirror*, not the source of truth — the live
//! truth is [`crate::appstate::AppState`] in memory — but it is what survives
//! a crash and what `/app` / an operator `cat` reads to answer "what is this
//! instance doing right now, and what is the last thing that broke?".
//!
//! Two independent consumers, one format:
//! - **boot recovery** reads `last_green` to respawn the previous bundle
//!   before any build (the `RecoverFromPointer` path);
//! - **the `/app` report** ([`crate::appsvc`]) reads the live in-memory state,
//!   but the on-disk file is the durable record telemetry/operators trust.
//!
//! Flat `key=value` with a scheme header — the same byte-discipline as the
//! latest-green pointer ([`crate::build::write_pointer_atomic`], which this
//! reuses for the atomic temp+fsync+rename write) and the bundle `meta`. No
//! serde: a five-field record does not earn a derive, and a flat file an
//! operator can read without tooling is the point.

use std::path::{Path, PathBuf};

use crate::appstate::{InstanceState, Pipeline};
use crate::build::write_pointer_atomic;

/// Scheme header — bump only on an incompatible format change (a reader that
/// sees an unknown scheme treats the file as absent rather than misparsing).
const SCHEME: &str = "cargoless-app-state/1";

/// A parsed state-file snapshot. Only the fields a *reader* needs are
/// reconstructed — the live machine is the source of truth, so this is
/// deliberately lossy (it does not round-trip `pending`/`draining`, which are
/// meaningless across a restart). `last_green` is the load-bearing field:
/// boot recovery keys the respawn on it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StateSnapshot {
    pub phase: String,
    pub serving_sha: Option<String>,
    pub last_green: Option<String>,
    pub last_red: Option<(String, String)>,
    /// Daemon-written wall-clock heartbeat (unix seconds), 0 if unknown.
    pub heartbeat_unix: u64,
}

/// `<state_dir>/app/<instance>/state`.
pub fn state_path(state_dir: &Path, instance: &str) -> PathBuf {
    state_dir.join("app").join(instance).join("state")
}

/// Render a one-word phase label for the pipeline/serving combination — the
/// at-a-glance "what is it doing" field.
fn phase_label(inst: &InstanceState) -> &'static str {
    match (&inst.pipeline, inst.serving.is_some()) {
        (Pipeline::Building { .. }, _) => "building",
        (Pipeline::Queued { .. }, _) => "queued",
        (Pipeline::Probing { .. }, true) => "probing+serving",
        (Pipeline::Probing { .. }, false) => "probing",
        (Pipeline::Idle, true) => "serving",
        (Pipeline::Idle, false) => "idle",
    }
}

/// Render `inst` to the durable flat format. `heartbeat_unix` is passed in
/// (the caller owns the clock — this module does no I/O beyond the write and
/// no time lookup, keeping it unit-testable with a fixed stamp).
pub fn render(inst: &InstanceState, heartbeat_unix: u64) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    s.push_str(SCHEME);
    s.push('\n');
    let _ = writeln!(s, "phase={}", phase_label(inst));
    let _ = writeln!(s, "heartbeat_unix={heartbeat_unix}");
    if let Some(serving) = &inst.serving {
        let _ = writeln!(s, "serving_sha={}", serving.sha);
        let _ = writeln!(s, "serving_generation={}", serving.generation);
    }
    if let Some(green) = &inst.last_green {
        let _ = writeln!(s, "last_green={green}");
    }
    if let Some((sha, reason)) = &inst.last_red {
        let _ = writeln!(s, "last_red_sha={sha}");
        // The reason can be multi-line (a build tail); collapse newlines so
        // the flat framing stays one-value-per-line and unambiguous.
        let _ = writeln!(s, "last_red_reason={}", one_line(reason));
    }
    if let Some(pending) = &inst.pending {
        let _ = writeln!(s, "pending_sha={pending}");
    }
    let _ = writeln!(s, "draining={}", inst.draining.len());
    s
}

/// Atomically write `inst`'s snapshot to `<state_dir>/app/<instance>/state`.
/// Reuses the latest-green pointer's temp+fsync+rename primitive, so a crash
/// mid-write leaves the previous snapshot byte-intact (never torn).
pub fn write(
    state_dir: &Path,
    instance: &str,
    inst: &InstanceState,
    heartbeat_unix: u64,
) -> std::io::Result<()> {
    write_pointer_atomic(
        &state_path(state_dir, instance),
        &render(inst, heartbeat_unix),
    )
}

/// Parse a snapshot back. `Ok(None)` ⇒ the file is absent or carries an
/// unknown scheme (treat as "no prior state" — a fresh instance). A present
/// file with a known scheme but a malformed body is a soft error too: boot
/// recovery must never wedge on a corrupt mirror, it just starts cold.
pub fn read(state_dir: &Path, instance: &str) -> Option<StateSnapshot> {
    let text = std::fs::read_to_string(state_path(state_dir, instance)).ok()?;
    parse(&text)
}

/// Pure parse of the flat format. Separated from [`read`] for unit testing.
pub fn parse(text: &str) -> Option<StateSnapshot> {
    let mut lines = text.lines();
    if lines.next() != Some(SCHEME) {
        return None; // absent scheme / wrong version ⇒ "no prior state"
    }
    let mut snap = StateSnapshot::default();
    let mut red_sha: Option<String> = None;
    let mut red_reason: Option<String> = None;
    for line in lines {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "phase" => snap.phase = v.to_string(),
            "serving_sha" => snap.serving_sha = Some(v.to_string()),
            "last_green" => snap.last_green = Some(v.to_string()),
            "last_red_sha" => red_sha = Some(v.to_string()),
            "last_red_reason" => red_reason = Some(v.to_string()),
            "heartbeat_unix" => snap.heartbeat_unix = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    if let Some(sha) = red_sha {
        snap.last_red = Some((sha, red_reason.unwrap_or_default()));
    }
    Some(snap)
}

/// Collapse any newlines/CRs to spaces so a value stays on one line.
fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appstate::{Pipeline, ServingChild};

    fn serving_instance() -> InstanceState {
        InstanceState {
            serving: Some(ServingChild {
                sha: "greensha".into(),
                generation: 4,
            }),
            pipeline: Pipeline::Idle,
            last_green: Some("greensha".into()),
            last_red: Some(("badsha".into(), "step `portal` exited 101\nE0432".into())),
            ..Default::default()
        }
    }

    #[test]
    fn round_trips_the_load_bearing_fields() {
        let inst = serving_instance();
        let text = render(&inst, 1_700_000_000);
        let snap = parse(&text).expect("known scheme parses");
        assert_eq!(snap.phase, "serving");
        assert_eq!(snap.serving_sha.as_deref(), Some("greensha"));
        assert_eq!(snap.last_green.as_deref(), Some("greensha"));
        assert_eq!(snap.heartbeat_unix, 1_700_000_000);
        let (rsha, rreason) = snap.last_red.unwrap();
        assert_eq!(rsha, "badsha");
        // The multi-line reason was flattened to one line (framing intact).
        assert!(rreason.contains("E0432"));
        assert!(!rreason.contains('\n'), "reason flattened: {rreason:?}");
    }

    #[test]
    fn phase_label_reflects_every_pipeline_state() {
        let mut inst = InstanceState::default();
        assert_eq!(phase_label(&inst), "idle");
        inst.serving = Some(ServingChild {
            sha: "s".into(),
            generation: 1,
        });
        assert_eq!(phase_label(&inst), "serving");
        inst.pipeline = Pipeline::Queued { sha: "q".into() };
        assert_eq!(phase_label(&inst), "queued");
        inst.pipeline = Pipeline::Building {
            sha: "b".into(),
            generation: 2,
        };
        assert_eq!(phase_label(&inst), "building");
        inst.pipeline = Pipeline::Probing {
            sha: "p".into(),
            generation: 3,
            respawn: false,
        };
        assert_eq!(phase_label(&inst), "probing+serving");
        inst.serving = None;
        assert_eq!(phase_label(&inst), "probing");
    }

    #[test]
    fn unknown_scheme_is_treated_as_no_prior_state() {
        assert_eq!(parse("some-other-format/9\nphase=idle\n"), None);
        assert_eq!(parse(""), None);
        // A known scheme with only a heartbeat is a valid (minimal) snapshot.
        let snap = parse("cargoless-app-state/1\nphase=idle\nheartbeat_unix=7\n").unwrap();
        assert_eq!(snap.phase, "idle");
        assert_eq!(snap.heartbeat_unix, 7);
        assert_eq!(snap.last_green, None);
    }

    #[test]
    fn write_then_read_through_the_filesystem() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("cargoless-appstatefile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // Absent file ⇒ no prior state.
        assert_eq!(read(&dir, "dev"), None);

        let inst = serving_instance();
        write(&dir, "dev", &inst, 42).unwrap();
        let snap = read(&dir, "dev").expect("written state reads back");
        assert_eq!(snap.last_green.as_deref(), Some("greensha"));
        assert_eq!(snap.heartbeat_unix, 42);

        // A second write replaces in full (atomic rename, never appends).
        let idle = InstanceState::default();
        write(&dir, "dev", &idle, 43).unwrap();
        let snap = read(&dir, "dev").unwrap();
        assert_eq!(snap.phase, "idle");
        assert_eq!(snap.last_green, None, "replaced, not merged");
        assert_eq!(snap.heartbeat_unix, 43);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn building_and_pending_are_rendered() {
        let inst = InstanceState {
            pipeline: Pipeline::Building {
                sha: "newsha".into(),
                generation: 9,
            },
            pending: Some("evennewer".into()),
            last_green: Some("oldgreen".into()),
            ..Default::default()
        };
        let text = render(&inst, 100);
        assert!(text.contains("phase=building"));
        assert!(text.contains("pending_sha=evennewer"));
        assert!(text.contains("last_green=oldgreen"));
        // Building with no serving child ⇒ no serving_sha line.
        assert!(!text.contains("serving_sha="));
    }
}
