//! Filesystem helpers shared by the comparative mode drivers.
//!
//! Two concerns, both motivated by *fairness*:
//!
//!  * **Atomic writes** — naive overwrite-in-place races every modern
//!    filesystem watcher (inotify/FSEvents/kqueue), which can deliver a
//!    `MODIFY` event for the half-written file and make a tool report a
//!    junk red against text that the editor never actually saved. We
//!    write to a sibling temp file, fsync, and `rename(2)` — the same
//!    pattern editors use, the same pattern build-cas's publisher uses
//!    for `latest-green`. This is what gives every comparative tool the
//!    same observable "save" event.
//!
//!  * **Restore-on-drop** — the bench drives the fixture's source tree
//!    into known-broken or non-canonical states. A panic mid-run, a
//!    Ctrl-C, an OOM kill — none of those should leave the fixture
//!    dirty for the next CI rerun. A `FileGuard` captures the canonical
//!    contents on construction and atomic-writes them back on drop.

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

/// Atomic write: temp file (same dir, leading dot + `.bench-harness.tmp`
/// suffix to make leaks visible) → fsync → rename. Same-fs guarantee:
/// the temp lives next to the target, so rename(2) is atomic.
pub fn atomic_write(target: &Path, body: &str) -> std::io::Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| std::io::Error::other("target has no parent"))?;
    let stem = target
        .file_name()
        .ok_or_else(|| std::io::Error::other("target has no filename"))?
        .to_string_lossy()
        .into_owned();
    let tmp = dir.join(format!(".{stem}.bench-harness.tmp"));
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, target)?;
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
