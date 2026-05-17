//! `status` — what does the daemon know right now?
//!
//! v0 is single-process with no IPC bus yet (the green/red model + event bus
//! are Plane CWDL daemon-core work, tasks #4/#5). So `serve` drops a tiny
//! status file and `status` reads it back, then probes the bound port for
//! liveness. This is the honest v0 of "the codebase tells you what works":
//! the recorded latest-green hash + whether the server is actually up.
//!
//! The format is a hand-rolled `key=value` file (no serde, matching house
//! style). It is forward-compatible: unknown keys are ignored on read, so
//! daemon-core can add `tree_state=`/`red_files=` later without a flag day.

use std::fmt::Write as _;
use std::io::Write as _;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use crate::config::Config;
use crate::ui;

/// Where the status file lives for a given project root.
pub fn status_path(root: &Path) -> PathBuf {
    root.join(".cargoless").join("daemon.status")
}

/// The daemon's externally-observable state. `latest_green` is `None` until
/// the first successful build (wired when the build/CAS trigger lands).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DaemonStatus {
    pub pid: u32,
    pub host: String,
    pub port: u16,
    pub detection: String,
    pub latest_green: Option<String>,
}

impl DaemonStatus {
    /// Serialise to the `key=value` line format.
    pub fn serialize(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "pid={}", self.pid);
        let _ = writeln!(s, "host={}", self.host);
        let _ = writeln!(s, "port={}", self.port);
        let _ = writeln!(s, "detection={}", self.detection);
        if let Some(h) = &self.latest_green {
            let _ = writeln!(s, "latest_green={h}");
        }
        s
    }

    /// Parse the `key=value` line format. Unknown keys are ignored
    /// (forward-compatible with future daemon-core fields).
    pub fn parse(text: &str) -> Self {
        let mut st = DaemonStatus::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim() {
                "pid" => st.pid = v.trim().parse().unwrap_or(0),
                "host" => st.host = v.trim().to_string(),
                "port" => st.port = v.trim().parse().unwrap_or(0),
                "detection" => st.detection = v.trim().to_string(),
                "latest_green" => st.latest_green = Some(v.trim().to_string()),
                _ => {}
            }
        }
        st
    }
}

/// Write the status file (best-effort; serve must not die because the status
/// file could not be written — it is observability, not the daemon).
pub fn write_status(root: &Path, st: &DaemonStatus) {
    let path = status_path(root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::File::create(&path) {
        let _ = f.write_all(st.serialize().as_bytes());
    }
}

/// Remove the status file (called by `serve` on clean shutdown so `status`
/// does not report a ghost daemon).
pub fn clear_status(root: &Path) {
    let _ = std::fs::remove_file(status_path(root));
}

/// Is something actually listening on the recorded address?
fn port_is_live(host: &str, port: u16) -> bool {
    let addr = format!("{host}:{port}");
    addr.parse()
        .ok()
        .and_then(|a| TcpStream::connect_timeout(&a, Duration::from_millis(300)).ok())
        .is_some()
}

/// `status` command. Exit code: `0` daemon up, `3` no/stale daemon — so
/// scripts can gate on "is cargoless serving this project?".
pub fn run(cfg: &Config) -> ExitCode {
    let path = status_path(&cfg.root);
    let Ok(text) = std::fs::read_to_string(&path) else {
        ui::warn(format!(
            "no daemon for {} — run `cargoless serve` first.",
            cfg.root.display()
        ));
        return ExitCode::from(3);
    };

    let st = DaemonStatus::parse(&text);
    let live = port_is_live(&st.host, st.port);

    if live {
        ui::ok(format!(
            "daemon up — pid {}, serving http://{}:{}",
            st.pid, st.host, st.port
        ));
    } else {
        ui::warn(format!(
            "status file present but nothing is listening on {}:{} — \
             daemon likely stopped uncleanly (run `cargoless serve` again).",
            st.host, st.port
        ));
    }
    ui::step(format!("project: {}", st.detection));
    match &st.latest_green {
        Some(h) => ui::ok(format!("latest green build: {h}")),
        None => ui::wait("no green build yet (waiting for first green tree)".to_string()),
    }

    if live {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrips_and_ignores_unknown_keys() {
        let st = DaemonStatus {
            pid: 4242,
            host: "127.0.0.1".to_string(),
            port: 8080,
            detection: "auto-detected: cdylib + leptos".to_string(),
            latest_green: Some("blake3:abcd".to_string()),
        };
        let round = DaemonStatus::parse(&st.serialize());
        assert_eq!(round, st);

        // Forward-compat: a future daemon-core field must not break parsing.
        let with_future = format!("{}tree_state=green\nred_files=0\n", st.serialize());
        assert_eq!(DaemonStatus::parse(&with_future), st);
    }

    #[test]
    fn missing_latest_green_is_none() {
        let st = DaemonStatus {
            pid: 1,
            host: "h".into(),
            port: 1,
            detection: "d".into(),
            latest_green: None,
        };
        assert!(!st.serialize().contains("latest_green"));
        assert_eq!(DaemonStatus::parse(&st.serialize()).latest_green, None);
    }

    #[test]
    fn write_then_clear_roundtrips_on_disk() {
        let mut root = std::env::temp_dir();
        root.push(format!("tf-cli-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let st = DaemonStatus {
            pid: 7,
            host: "127.0.0.1".into(),
            port: 9999,
            detection: "x".into(),
            latest_green: None,
        };
        write_status(&root, &st);
        let back = DaemonStatus::parse(
            &std::fs::read_to_string(status_path(&root)).expect("status file written"),
        );
        assert_eq!(back, st);

        clear_status(&root);
        assert!(std::fs::read_to_string(status_path(&root)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
