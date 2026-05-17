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
    let fresh = st.is_fresh(now);
    let age = now.saturating_sub(st.updated);

    if fresh {
        ui::ok(format!(
            "daemon live — pid {}, verdict {} ({}s ago)",
            st.pid,
            Verdict::parse(&st.verdict_str).as_str(),
            age
        ));
    } else {
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
        ui::wait("latest-green: none yet (no green build published)".to_string());
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
}
