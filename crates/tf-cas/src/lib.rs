//! Content-addressed storage.
//!
//! The [`ContentStore`] trait is the v1 remote-backend seam — it MUST exist in
//! v0 even though only [`LocalDiskStore`] implements it, or v1 becomes a
//! rewrite (decision D10). v0 ships local-disk only; S3/RustFS are v1.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tf_proto::InputHash;

/// A store that maps an [`InputHash`] to opaque artifact bytes.
pub trait ContentStore {
    /// Store `bytes` under `key`. Idempotent: storing the same key twice is a
    /// no-op (this is what makes AC#5 cache-dedupe possible).
    fn put(&self, key: &InputHash, bytes: &[u8]) -> io::Result<()>;

    /// Fetch the bytes for `key`, or `None` if absent.
    fn get(&self, key: &InputHash) -> io::Result<Option<Vec<u8>>>;

    /// Whether `key` is already present — the build pipeline checks this to
    /// skip a build entirely on a cache hit.
    fn contains(&self, key: &InputHash) -> io::Result<bool> {
        Ok(self.get(key)?.is_some())
    }
}

/// Local filesystem CAS: one file per content hash under `root`.
pub struct LocalDiskStore {
    root: PathBuf,
}

impl LocalDiskStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, key: &InputHash) -> PathBuf {
        self.root.join(key.as_str())
    }
}

impl ContentStore for LocalDiskStore {
    fn put(&self, key: &InputHash, bytes: &[u8]) -> io::Result<()> {
        let path = self.path_for(key);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)
    }

    fn get(&self, key: &InputHash) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.path_for(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Test-only helper: a unique temp dir under the OS temp root.
fn scratch_dir(tag: &str) -> PathBuf {
    let mut p: PathBuf = std::env::temp_dir();
    p.push(format!("tf-trunk-cas-{tag}-{}", std::process::id()));
    p
}

/// Public so `scratch_dir` is exercised outside `#[cfg(test)]` and the
/// skeleton stays clippy-clean under `-D warnings`. Returns the default
/// cache root for the current process when no config overrides it.
pub fn default_scratch_root() -> PathBuf {
    scratch_dir("default")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_dedupe() {
        let dir = scratch_dir("roundtrip");
        let _ = fs::remove_dir_all(&dir);
        let store = LocalDiskStore::new(&dir);
        let key = InputHash::new("abc123");

        assert!(!store.contains(&key).unwrap());
        store.put(&key, b"hello").unwrap();
        assert!(store.contains(&key).unwrap());
        assert_eq!(store.get(&key).unwrap().as_deref(), Some(&b"hello"[..]));

        // Idempotent put — the AC#5 cache-hit invariant.
        store.put(&key, b"ignored-second-write").unwrap();
        assert_eq!(store.get(&key).unwrap().as_deref(), Some(&b"hello"[..]));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_key_is_none() {
        let store = LocalDiskStore::new(default_scratch_root());
        assert!(
            store
                .get(&InputHash::new("nope-not-here"))
                .unwrap()
                .is_none()
        );
    }
}
