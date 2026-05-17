//! Filesystem helpers shared by the comparative mode drivers.
//!
//! ## Why direct-write, NOT atomic-rename (revised after #36 5th iteration)
//!
//! Earlier revs used a temp+fsync+rename pattern for "atomic" save
//! events. That pattern is correct for editors writing config files, but
//! it produces a `MOVED_FROM .tmp` + `MOVED_TO target` event pair on
//! inotify (and equivalents on FSEvents/kqueue) — which cargoless's
//! notify-rs watcher, in the post-#49 debouncer wiring, does NOT pick
//! up as a "the file content changed" trigger. Empirically (manual
//! probe in cargoless-builder pod, May 2026):
//!
//!   * `sed -i` (temp+rename, but with a randomized temp name)      → works
//!   * harness `atomic_write` (.{stem}.bench-harness.tmp + rename)  → NEVER triggers cargoless
//!   * direct `open + truncate + write_all + sync_all + close`      → works (matches what real editors do)
//!
//! The whole `MOVED_FROM/TO` vs single-MODIFY discrimination is the
//! `notify-rs`-level reality every watcher tool has to navigate. The
//! safest "save event" shape across all the comparative tools we run
//! against (cargoless's notify-rs, trunk's notify-rs, bacon's
//! notify-rs) is the **direct write** — that's how vim, vscode,
//! rust-analyzer-on-save all write source files in practice.
//!
//! The half-written-file race risk for direct writes is theoretical
//! for the < 2KB Rust source files we edit: write_all + sync_all is a
//! single syscall on Linux/macOS, and every watcher we drive
//! debounces ≥ 50ms before reading the file — by then write is done.
//! (build-cas's `latest-green` publisher still uses temp+rename
//! because it writes a binary blob that *can* be partially-read
//! mid-write; the bench fixture is text-only and small.)
//!
//! ## Restore-on-drop
//!
//! The bench drives the fixture's source tree into known-broken
//! states. A panic mid-run, a Ctrl-C, an OOM kill — none of those
//! should leave the fixture dirty for the next CI rerun. A
//! `FileGuard` captures the canonical contents on construction and
//! restores them on drop using the same direct-write path.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Restore-on-drop guard for a fixture file.
pub struct FileGuard {
    path: PathBuf,
    clean: String,
}

impl FileGuard {
    pub fn new(path: PathBuf, clean: String) -> Self {
        Self { path, clean }
    }
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        // Best-effort: an IO error during unwind would abort, which is
        // worse than a stale fixture (CI will redo it on the next push).
        let _ = atomic_write(&self.path, &self.clean);
    }
}

/// Direct overwrite of `target` with `body`. Open(create+truncate) →
/// write_all → sync_all → close. Same-process atomicity, same FS-event
/// shape as a real editor save — see the module-level docstring for
/// why this matters for cargoless's notify-rs watcher (the previous
/// temp+rename pattern produced MOVED_TO events that the watcher did
/// not surface as content changes, making the #36 harness's save→
/// verdict measurements time out at 120s on every rep).
///
/// The function name stays `atomic_write` to keep the call-sites
/// unchanged across the harness; the *semantics* atomic-ness now
/// comes from the single-syscall write_all + sync_all on small files,
/// not from a temp+rename dance. For text fixture files (< 2KB), this
/// is reliable on every modern POSIX kernel.
pub fn atomic_write(target: &Path, body: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(target)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch_dir(line: u32) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cbench-fsutil-{}-{}", std::process::id(), line));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_replaces_and_no_tmp_leak() {
        let dir = scratch_dir(line!());
        let path = dir.join("x.rs");
        fs::write(&path, "before").unwrap();
        atomic_write(&path, "after").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "after");
        assert!(!dir.join(".x.rs.bench-harness.tmp").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_write_creates_missing_file() {
        let dir = scratch_dir(line!());
        let path = dir.join("brand-new.rs");
        atomic_write(&path, "hello").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_guard_restores_on_drop() {
        let dir = scratch_dir(line!());
        let path = dir.join("y.rs");
        fs::write(&path, "clean").unwrap();
        {
            let _g = FileGuard::new(path.clone(), "clean".into());
            atomic_write(&path, "dirty").unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "dirty");
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "clean");
        fs::remove_dir_all(&dir).ok();
    }
}
