//! Content-addressed storage + the build-input hashing that keys it.
//!
//! The [`ContentStore`] trait is the v1 remote-backend seam — it MUST exist in
//! v0 even though only [`LocalDiskStore`] implements it, or v1 becomes a
//! rewrite (decision D10). v0 ships local-disk only; S3/RustFS are v1.
//!
//! This crate also owns the half of the `cargoless-proto` contract that `cargoless-proto`
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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cargoless_proto::InputHash;

/// Per-process monotonic counter for temp-file names. Combined with the
/// PID it makes every in-flight `put` temp path unique across threads
/// *and* across the multiple cargoless processes that share one
/// `--cas-dir` in a Model R fleet — the prerequisite for the
/// write-to-temp-then-atomic-rename store being concurrency-safe.
static PUT_SEQ: AtomicU64 = AtomicU64::new(0);

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

    /// A unique sibling temp path in the *same directory* as `final_path`
    /// (so the subsequent rename is same-filesystem ⇒ atomic). The name
    /// embeds PID + a process-monotonic sequence so two writers — threads
    /// or separate fleet processes — never collide on the temp file.
    fn tmp_path(final_path: &Path) -> PathBuf {
        let seq = PUT_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let name = final_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dir = final_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        dir.join(format!(".tmp.{name}.{pid}.{seq}"))
    }
}

impl ContentStore for LocalDiskStore {
    /// Store `bytes` under `key`, **atomically**.
    ///
    /// ## #2 — Model R concurrent-writer safety
    ///
    /// Before Model R, daemons were single-writer per process and
    /// PID-scoped their CAS dir, so an in-place `fs::write` was adequate.
    /// Model R's whole point is a *shared* `--cas-dir` across a fleet of
    /// daemons: N writers, multiple processes, one directory. An in-place
    /// truncate-then-write is **not** safe there — a concurrent reader
    /// doing `contains()`→`get()` can observe a half-written file.
    /// Content-addressing guarantees the *final* bytes converge (same key
    /// ⇒ identical bytes); it does **not** make an interleaved read whole.
    ///
    /// Fix: write to a unique sibling temp file, `fsync` it, then
    /// `rename` it onto the final path. POSIX `rename(2)` within one
    /// filesystem is atomic — a reader sees either the old state (absent)
    /// or the complete new file, never a torn one. If another writer wins
    /// the race the rename simply replaces an identical-content file
    /// (content-addressed), so the result is still correct and the `put`
    /// stays idempotent (the AC#5 cache-hit invariant). The `exists()`
    /// fast-path is kept so a cache hit costs one `stat`, not a rewrite.
    ///
    /// (Windows non-atomic-replace is out of scope — `CLAUDE.md` parks
    /// Windows in v1; macOS + Linux are the supported fleet hosts.)
    fn put(&self, key: &InputHash, bytes: &[u8]) -> io::Result<()> {
        let path = self.path_for(key);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = Self::tmp_path(&path);
        // Scope the file handle so it is closed before the rename.
        let write_then_sync = || -> io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?; // bytes durable before the name becomes visible
            Ok(())
        };
        if let Err(e) = write_then_sync() {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        match fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                // A racing writer may have created `path` first; since the
                // content is addressed by `key`, an existing file is the
                // correct file — treat as success (idempotent put).
                if path.exists() { Ok(()) } else { Err(e) }
            }
        }
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
    p.push(format!("cargoless-cas-{tag}-{}", std::process::id()));
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

    // ---- #2 Model R: concurrent-writer safety on a shared --cas-dir ----
    //
    // The payload is deliberately large (256 KiB): a non-atomic in-place
    // `fs::write` of this size is NOT a single write(2) syscall, so an
    // interleaved reader would observe a short/torn file. With the
    // temp+rename store the reader sees only absent-or-complete. These
    // tests fail loudly on the pre-#2 implementation and pass on the fix.

    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;

    fn big_payload(seed: u8) -> Vec<u8> {
        (0..256 * 1024)
            .map(|i| (i as u8).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn concurrent_same_key_never_torn_read() {
        let dir = scratch_dir("cc-same-key");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(LocalDiskStore::new(&dir));
        // Content-addressed: same key ⇒ identical bytes, by definition.
        let key = InputHash::new(content_hash(&big_payload(0)).as_str());
        let payload = Arc::new(big_payload(0));

        let writers = 16usize;
        let readers = 8usize;
        let gate = Arc::new(Barrier::new(writers + readers));
        let mut handles = Vec::new();

        for _ in 0..writers {
            let (s, k, p, g) = (store.clone(), key.clone(), payload.clone(), gate.clone());
            handles.push(thread::spawn(move || {
                g.wait();
                for _ in 0..8 {
                    s.put(&k, &p).unwrap();
                }
            }));
        }
        for _ in 0..readers {
            let (s, k, p, g) = (store.clone(), key.clone(), payload.clone(), gate.clone());
            handles.push(thread::spawn(move || {
                g.wait();
                for _ in 0..200 {
                    if let Some(got) = s.get(&k).unwrap() {
                        // The whole point: a present file is ALWAYS the
                        // complete, correct content — never a torn prefix.
                        assert_eq!(
                            got.len(),
                            p.len(),
                            "torn read: short file observed mid-write"
                        );
                        assert_eq!(&got, &*p, "torn read: corrupt content");
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(store.get(&key).unwrap().as_deref(), Some(&payload[..]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_distinct_keys_all_land_intact() {
        let dir = scratch_dir("cc-distinct");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(LocalDiskStore::new(&dir));

        let n = 24u8;
        let gate = Arc::new(Barrier::new(n as usize));
        let mut handles = Vec::new();
        for seed in 0..n {
            let (s, g) = (store.clone(), gate.clone());
            handles.push(thread::spawn(move || {
                let payload = big_payload(seed);
                let key = InputHash::new(content_hash(&payload).as_str());
                g.wait();
                s.put(&key, &payload).unwrap();
                (key, payload)
            }));
        }
        for h in handles {
            let (key, payload) = h.join().unwrap();
            let got = store.get(&key).unwrap().expect("key must be present");
            assert_eq!(got, payload, "cross-key corruption under concurrency");
            // content-addressed atomicity: stored bytes re-hash to the key.
            assert_eq!(content_hash(&got).as_str(), key.as_str());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash_reduction_is_concurrency_stable() {
        // §9a guard within #2's scope: the frozen wire-format constants
        // (SCHEME `b"tf-cas/input-hash/v1"`, field tags) live in
        // identity.rs/tree.rs and are NOT touched by this commit
        // (`git diff` proves it; the #9 determinism suite + #96
        // drift-guard are the standing guards). What #2 must additionally
        // show is that the reduction stays *consistent under fleet
        // concurrency* — if any shared state perturbed the preimage, many
        // threads hashing identical inputs would diverge. They must not.
        use cargoless_proto::{BuildIdentity, ContentHash, Profile, TargetTriple};
        let identity = BuildIdentity {
            source_tree: ContentHash::new("src-tree"),
            cargo_lock: ContentHash::new("lock"),
            rust_toolchain: ContentHash::new("toolchain"),
            tf_config: ContentHash::new("cfg"),
            target: TargetTriple::new("wasm32-unknown-unknown"),
            profile: Profile::Dev,
        };
        let expect = input_hash(&identity).as_str().to_string();
        let identity = Arc::new(identity);
        let gate = Arc::new(Barrier::new(32));
        let mut handles = Vec::new();
        for _ in 0..32 {
            let (id, g, want) = (identity.clone(), gate.clone(), expect.clone());
            handles.push(thread::spawn(move || {
                g.wait();
                for _ in 0..50 {
                    assert_eq!(
                        input_hash(&id).as_str(),
                        want,
                        "InputHash diverged under concurrency — \
                         wire-format preimage is not stable"
                    );
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }
}
