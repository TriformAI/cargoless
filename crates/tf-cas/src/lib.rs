//! Content-addressed storage + the build-input hashing that keys it.
//!
//! The [`ContentStore`] trait is the v1 remote-backend seam — it MUST exist in
//! v0 even though only [`LocalDiskStore`] implements it, or v1 becomes a
//! rewrite (decision D10). v0 ships local-disk only; S3/RustFS are v1.
//!
//! This crate also owns the half of the `tf-proto` contract that `tf-proto`
//! deliberately does *not* specify: the hash algorithm ([`sha256`]) and the
//! `BuildIdentity → InputHash` reduction ([`input_hash`]) that the whole AC#5
//! dedupe / AC#4 provenance guarantee rests on. The daemon assembles a
//! `BuildIdentity`; this crate turns it into the CAS key and stores the bytes.
//!
//! | Concern | Where |
//! |---|---|
//! | stable hash primitive | [`sha256`] |
//! | `BuildIdentity → InputHash`, per-file content hashing | [`identity`] |
//! | deterministic whole-source-tree hash | [`tree`] |
//! | `tf clean` wipe semantics + safety guard | [`clean`] |
//! | artifact byte storage (trait + local disk) | [`ContentStore`] |

pub mod clean;
pub mod identity;
pub mod sha256;
pub mod tree;

pub use clean::{UnsafeCacheRoot, clean_cache, guard_cache_root};
pub use identity::{absent_marker, content_hash, input_hash};
pub use sha256::sha256_hex;
pub use tree::hash_source_tree;

use std::fs;
use std::io;
use std::path::PathBuf;

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
