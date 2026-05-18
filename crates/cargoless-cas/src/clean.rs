//! `tf clean` cache-wipe semantics and safety (CWDL-39).
//!
//! This is the *primitive* the Epic 5 `tf clean` subcommand will call; the CLI
//! owns the command surface, the CAS owns "what is safe to delete." The rule
//! is intentionally narrow:
//!
//! * `tf clean` wipes **only the CAS cache directory's contents**. It never
//!   touches source, `Cargo.lock`, `tf.toml`, or anything outside the cache
//!   root. The cache is pure derived state — re-derivable by one rebuild — so
//!   wiping it is always safe *for correctness*; the only real risk is a
//!   mis-pointed root nuking something that is not a cache.
//! * That risk is what [`guard_cache_root`] exists for. A cache root that is a
//!   filesystem root, a home directory, the current directory, or a
//!   suspiciously shallow path is **refused** — wiping it would be
//!   catastrophic and is never what `tf clean` means.
//!
//! Recreating the (now-empty) cache directory after a wipe keeps the store
//! immediately usable, matching `LocalDiskStore`'s lazy-create behaviour.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path};
use std::time::{Duration, SystemTime};

/// The temp-file name prefix `LocalDiskStore::put` writes before its atomic
/// rename (`.tmp.{key}.{pid}.{seq}` — see `lib.rs::tmp_path`, the #2
/// concurrent-writer-safety contract). The single shared constant the
/// orphan sweep matches, so the prefix cannot silently drift apart
/// between the writer and the sweeper.
pub const PUT_TMP_PREFIX: &str = ".tmp.";

/// Why a cache root was rejected as unsafe to wipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsafeCacheRoot {
    /// A filesystem root or prefix (`/`, `C:\`).
    FilesystemRoot,
    /// Empty or relative-to-nothing path.
    Empty,
    /// Fewer than two path components below the root (e.g. `/tmp`, `/home`) —
    /// too shallow to be a dedicated cache dir; refuse rather than risk it.
    TooShallow,
    /// Resolves to the user's home directory.
    HomeDirectory,
}

impl core::fmt::Display for UnsafeCacheRoot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            UnsafeCacheRoot::FilesystemRoot => "path is a filesystem root",
            UnsafeCacheRoot::Empty => "path is empty",
            UnsafeCacheRoot::TooShallow => "path is too shallow to be a cache dir",
            UnsafeCacheRoot::HomeDirectory => "path is the home directory",
        };
        f.write_str(s)
    }
}

impl std::error::Error for UnsafeCacheRoot {}

/// Reject cache roots that must never be recursively deleted.
///
/// Conservative on purpose: a false reject costs the user an explicit
/// `--cache-dir`; a false accept costs them their home directory.
///
/// # Errors
/// Returns [`UnsafeCacheRoot`] describing why `root` is not a safe wipe target.
pub fn guard_cache_root(root: &Path) -> Result<(), UnsafeCacheRoot> {
    guard_cache_root_with_home(root, std::env::var_os("HOME"))
}

/// The pure core of [`guard_cache_root`] with the home directory injected, so
/// the home-equality rule is unit-testable without mutating process env (which
/// is `unsafe` in edition 2024 and racy under the parallel test runner).
fn guard_cache_root_with_home(root: &Path, home: Option<OsString>) -> Result<(), UnsafeCacheRoot> {
    if root.as_os_str().is_empty() {
        return Err(UnsafeCacheRoot::Empty);
    }

    let mut normal = 0usize;
    let mut only_root_prefix = true;
    for c in root.components() {
        match c {
            Component::Normal(_) => {
                normal += 1;
                only_root_prefix = false;
            }
            Component::CurDir | Component::ParentDir => only_root_prefix = false,
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    if only_root_prefix {
        return Err(UnsafeCacheRoot::FilesystemRoot);
    }
    if let Some(home) = home {
        if !home.is_empty() && Path::new(&home) == root {
            return Err(UnsafeCacheRoot::HomeDirectory);
        }
    }
    if normal < 2 {
        return Err(UnsafeCacheRoot::TooShallow);
    }
    Ok(())
}

/// Wipe the CAS cache directory's contents, then recreate it empty.
///
/// Guarded by [`guard_cache_root`]; only ever deletes inside `root`. A missing
/// `root` is success (nothing cached is the post-condition either way).
///
/// # Errors
/// [`io::ErrorKind::InvalidInput`] wrapping an [`UnsafeCacheRoot`] if the root
/// fails the safety guard, or the underlying [`io::Error`] on a filesystem
/// failure.
pub fn clean_cache(root: impl AsRef<Path>) -> io::Result<()> {
    let root = root.as_ref();
    guard_cache_root(root).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    match fs::remove_dir_all(root) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs::create_dir_all(root)
}

/// Conservative default age below which a `.tmp.*` file is assumed to
/// still belong to an in-flight `put` and is left alone. A real `put`
/// temp lives from `File::create` to `rename` — milliseconds; one hour
/// is many orders of magnitude beyond that, so a `.tmp.*` older than
/// this is *provably* a dead-writer orphan (same conservative
/// "older-than ⇒ not in-flight" reasoning the ci-gate per-ref prune
/// uses). Tunable via the explicit-`older_than` argument.
pub const ORPHAN_MIN_AGE: Duration = Duration::from_secs(3600);

/// Sweep dead-writer `.tmp.*` orphans from a shared CAS directory.
///
/// `LocalDiskStore::put` is crash-safe by atomic temp+rename (#2); but a
/// writer that dies *between* `File::create` and `rename` leaves its
/// `.tmp.{key}.{pid}.{seq}` behind. That is **not** a correctness bug —
/// `get` only ever reads `path_for(key)`, never a `.tmp.*`; the final
/// content is unaffected — but in a long-lived shared `--cas-dir` across
/// a crashing agent fleet the orphans accumulate (the non-gating
/// follow-up dev-fixer flagged on the #2 scoped-review). `clean_cache`
/// already removes them (it wipes everything), but that is the
/// destructive user `tf clean`; this is the targeted, routine-safe
/// reclaim that needs no full wipe.
///
/// Safety / non-interference:
/// * operates **only inside `cache_dir`**, **only** on the cache root's
///   own direct entries, never recursing into the content-addressed
///   subtree;
/// * removes **only** regular files whose name starts with
///   [`PUT_TMP_PREFIX`] — never a content entry, never a directory;
/// * removes one **only** if its mtime is older than `older_than`, so a
///   concurrently-in-flight `put`'s fresh temp (lifetime ≪ 1s) is never
///   touched — no race with a live writer, by construction;
/// * best-effort per entry: a stat/remove failure on one orphan (e.g. a
///   peer fleet process won the unlink) is skipped, not propagated —
///   housekeeping must never fail its caller. Only an unreadable
///   `cache_dir` is an `Err`; a missing one is `Ok(0)`.
///
/// Returns the number of orphans actually removed (observability).
pub fn sweep_tmp_orphans(cache_dir: impl AsRef<Path>, older_than: Duration) -> io::Result<usize> {
    let dir = cache_dir.as_ref();
    let entries = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let now = SystemTime::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(PUT_TMP_PREFIX) {
            continue;
        }
        // file_type()/metadata: skip on any error (a racing peer may have
        // already removed it); never follow into directories.
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        let age = now.duration_since(mtime).unwrap_or_default();
        if age < older_than {
            continue; // possibly an in-flight put — leave it.
        }
        if fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ───────────────── #182 .tmp.* orphan sweep ─────────────────

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cl-182-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sweep_removes_only_old_tmp_files_not_content_or_dirs() {
        let dir = scratch("matrix");
        // Two dead-writer orphans, a real content entry, a subdir whose
        // name even starts with the tmp prefix (must be untouched — not a
        // file), and an unrelated file.
        fs::write(dir.join(".tmp.abc.123.0"), b"partial").unwrap();
        fs::write(dir.join(".tmp.def.456.1"), b"partial2").unwrap();
        fs::write(dir.join("a1b2c3deadbeef"), b"real cached artifact").unwrap();
        fs::write(dir.join("not-a-temp.txt"), b"keep").unwrap();
        fs::create_dir_all(dir.join(".tmp.adir.789.2")).unwrap();

        // older_than = ZERO ⇒ every .tmp.* file is "old enough" ⇒ the
        // age guard is exercised at its permissive bound; only the two
        // regular .tmp.* files go.
        let n = sweep_tmp_orphans(&dir, Duration::ZERO).unwrap();
        assert_eq!(n, 2, "exactly the two .tmp.* regular files removed");
        assert!(!dir.join(".tmp.abc.123.0").exists());
        assert!(!dir.join(".tmp.def.456.1").exists());
        // Content entry, unrelated file, and the .tmp.-prefixed DIR all
        // survive — never a content/dir/non-tmp deletion.
        assert!(dir.join("a1b2c3deadbeef").exists(), "content entry safe");
        assert!(dir.join("not-a-temp.txt").exists(), "non-tmp file safe");
        assert!(
            dir.join(".tmp.adir.789.2").is_dir(),
            ".tmp.-prefixed directory must NOT be removed (not a file)"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_leaves_fresh_tmp_in_flight_writes_untouched() {
        // A just-created .tmp.* (age ≈ 0) under a 1h threshold ⇒ possibly
        // an in-flight put ⇒ MUST be left (no race with a live writer).
        let dir = scratch("inflight");
        fs::write(dir.join(".tmp.live.999.0"), b"being written").unwrap();
        let n = sweep_tmp_orphans(&dir, ORPHAN_MIN_AGE).unwrap();
        assert_eq!(n, 0, "fresh temp under threshold must not be swept");
        assert!(
            dir.join(".tmp.live.999.0").exists(),
            "an in-flight put's fresh temp survives the sweep"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_missing_dir_is_ok_zero() {
        let missing = std::env::temp_dir().join(format!("cl-182-absent-{}", std::process::id()));
        let _ = fs::remove_dir_all(&missing);
        assert_eq!(sweep_tmp_orphans(&missing, Duration::ZERO).unwrap(), 0);
    }

    #[test]
    fn dangerous_roots_are_refused() {
        assert_eq!(
            guard_cache_root(Path::new("/")),
            Err(UnsafeCacheRoot::FilesystemRoot)
        );
        assert_eq!(guard_cache_root(Path::new("")), Err(UnsafeCacheRoot::Empty));
        assert_eq!(
            guard_cache_root(Path::new("/tmp")),
            Err(UnsafeCacheRoot::TooShallow)
        );
    }

    #[test]
    fn home_directory_is_refused() {
        // A HOME deep enough that only the home-equality rule can reject it
        // (3 components ⇒ passes the shallow guard), proving that specific
        // rule. No process-env mutation: the home value is injected.
        let home = "/home/somebody/account";
        assert_eq!(
            guard_cache_root_with_home(Path::new(home), Some(OsString::from(home))),
            Err(UnsafeCacheRoot::HomeDirectory)
        );
        // Same path, different HOME ⇒ accepted (it is not the home dir).
        assert!(
            guard_cache_root_with_home(Path::new(home), Some(OsString::from("/home/someone-else")))
                .is_ok()
        );
        // No HOME set at all ⇒ the home rule simply does not fire.
        assert!(guard_cache_root_with_home(Path::new(home), None).is_ok());
    }

    #[test]
    fn reasonable_cache_dir_is_accepted() {
        assert!(guard_cache_root(Path::new("/home/u/.cache/cargoless-cas")).is_ok());
        assert!(guard_cache_root(Path::new("/var/lib/tf/cache")).is_ok());
    }

    #[test]
    fn clean_wipes_contents_but_leaves_an_empty_dir() {
        let mut root: PathBuf = std::env::temp_dir();
        root.push(format!("cargoless-cas-clean-{}", std::process::id()));
        root.push("cache");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("deadbeef"), b"cached artifact").unwrap();
        assert!(root.join("deadbeef").exists());

        clean_cache(&root).unwrap();

        assert!(root.exists(), "cache dir is recreated, ready for reuse");
        assert!(!root.join("deadbeef").exists(), "cached entries are gone");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clean_on_missing_root_is_ok() {
        let mut root: PathBuf = std::env::temp_dir();
        root.push(format!("cargoless-cas-clean-absent-{}", std::process::id()));
        root.push("nested-cache");
        let _ = fs::remove_dir_all(&root);
        assert!(clean_cache(&root).is_ok());
        assert!(root.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clean_refuses_dangerous_root_with_invalid_input() {
        let err = clean_cache("/").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
