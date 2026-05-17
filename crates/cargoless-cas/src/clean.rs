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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
