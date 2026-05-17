//! Std-only subprocess driver: spawn a watcher tool, stream its stdout +
//! stderr line-by-line into a single timestamped channel, and graceful
//! shutdown on drop.
//!
//! Why line-by-line: every comparative tool (cargoless, trunk, bacon) uses
//! line-oriented stdout/stderr to communicate verdicts ("GREEN", "Success!",
//! "compilation finished"). Substring matching on those lines is honest,
//! cheap, and avoids adding a `regex` dep — which would inflate `Cargo.lock`
//! across the whole tree for one harness.
//!
//! Why one merged channel: tools split verdict signals across stdout and
//! stderr inconsistently (cargoless writes to stderr via `ui::ok`; trunk
//! writes to stdout; bacon to stdout). A single merged stream with a `Source`
//! tag matches whatever the tool actually does without per-tool plumbing.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// Which stream a line came from. Useful for diagnostics in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Stdout,
    Stderr,
}

/// A single line of output, time-tagged at the moment the reader saw it.
#[derive(Debug, Clone)]
pub struct Line {
    pub source: Source,
    pub at: Instant,
    pub text: String,
}

/// A running subprocess, with merged stdout+stderr line stream.
///
/// `Drop` kills the child + waits — so a panicking caller never leaks a
/// long-running `tftrunk`/`trunk`/`bacon` process. The drop is best-effort
/// (kill may have already happened in `shutdown`); it is never a panic.
pub struct Spawned {
    child: Option<Child>,
    pub lines: Receiver<Line>,
    pub label: String,
}

impl Spawned {
    /// Wait up to `timeout` for the next line whose `text` contains any of
    /// `needles`. Returns the matched line, OR `None` if the timeout elapsed
    /// or the child exited without producing a match.
    ///
    /// Lines that do NOT match are *discarded* — this is a forward-only edge
    /// detector. Callers wanting verbatim transcripts should drain `lines`
    /// directly.
    pub fn wait_for_any(&self, needles: &[&str], timeout: Duration) -> Option<Line> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    if needles.iter().any(|n| line.text.contains(n)) {
                        return Some(line);
                    }
                }
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// Drain the channel into a Vec until quiet for `quiet`. Caps at `max`
    /// lines so a chatty tool can't blow memory.
    pub fn drain_until_quiet(&self, quiet: Duration, max: usize) -> Vec<Line> {
        let mut out = Vec::new();
        loop {
            if out.len() >= max {
                return out;
            }
            match self.lines.recv_timeout(quiet) {
                Ok(line) => out.push(line),
                Err(_) => return out,
            }
        }
    }

    /// Kill the child + reap. Idempotent — safe to call before drop.
    pub fn shutdown(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Spawn `program` with `args` in `cwd`. Pipes stdout + stderr through
/// reader threads into a single merged `Line` channel.
///
/// `label` is a human name for the report ("cargoless", "trunk", "bacon").
/// On failure (binary not on PATH, missing perm, etc.) returns the io error
/// so the caller can mark the tool UNAVAILABLE rather than panic.
pub fn spawn(label: &str, program: &str, args: &[&str], cwd: &Path) -> std::io::Result<Spawned> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        // No-buffering env hint for tools that respect it. Trunk/bacon
        // inherit a TTY-less stdout/stderr so they'll already line-buffer;
        // this is belt + suspenders.
        .env("PYTHONUNBUFFERED", "1")
        .env("RUST_LOG_STYLE", "never")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("CLICOLOR_FORCE", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let (tx, rx) = channel::<Line>();

    if let Some(out) = child.stdout.take() {
        spawn_reader(out, Source::Stdout, tx.clone());
    }
    if let Some(err) = child.stderr.take() {
        spawn_reader(err, Source::Stderr, tx.clone());
    }
    drop(tx); // last refs are inside the reader threads.

    Ok(Spawned {
        child: Some(child),
        lines: rx,
        label: label.to_string(),
    })
}

fn spawn_reader<R: Read + Send + 'static>(r: R, source: Source, tx: Sender<Line>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(r);
        let mut buf = String::new();
        loop {
            buf.clear();
            // read_line preserves the trailing newline; trim before send so
            // substring matches work.
            match reader.read_line(&mut buf) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let text = buf.trim_end_matches(['\n', '\r']).to_string();
                    if text.is_empty() {
                        continue;
                    }
                    let line = Line {
                        source,
                        at: Instant::now(),
                        text,
                    };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// Is `program` available on PATH (and does `--version` succeed)?
/// Best-effort: a tool that prints help-to-stderr and exits 1 on
/// `--version` is treated as available (status code is ignored — what we
/// care about is `Command::new(...).spawn()` succeeding).
pub fn is_on_path(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure-unit tests only — std-only, no external tools required, no
    // network or filesystem races. The integration drivers (checker.rs,
    // artifact.rs) exercise spawn() for real against the fixture.

    #[test]
    fn is_on_path_handles_missing_binary() {
        // A binary that definitively does not exist.
        assert!(!is_on_path("cargoless-bench-nonexistent-tool-xyzzy"));
    }
}
