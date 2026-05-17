//! Deterministic source-tree hashing → a single [`ContentHash`].
//!
//! CWDL-33 is explicit: the build identity's `source_tree` component is a
//! *file-by-file* content hash of the project, and "if anything that can
//! affect output is not in the hash, that is a determinism bug." It also
//! demands an explicit stance on **`target/` exclusion, gitignore semantics,
//! and symlinks**. This module is that stance, stated once:
//!
//! * **Ordering** — entries are sorted by their forward-slash relative path
//!   before hashing, so the digest is independent of filesystem `readdir`
//!   order (the classic cross-machine determinism trap).
//! * **`target/` and `.git/`** — excluded by directory name. `target/` is
//!   build *output*, not input; including it makes every build its own cache
//!   miss. `.git/` is VCS state, not a compile input, and churns constantly.
//!   This is the whole exclusion policy: a fixed, documented set — **not** a
//!   `.gitignore` engine. Honoring arbitrary `.gitignore` rules is deferred to
//!   v1 (it needs a real ignore parser; the minimal-dep CI model rejects one
//!   now, and a wrong partial parser would be a determinism bug).
//! * **Symlinks** — *not* followed (prevents escaping the tree and cycle
//!   hangs). A symlink is recorded as its own kind plus its textual target, so
//!   repointing a symlink still invalidates the cache, but traversal stays
//!   inside the project.
//! * **Files** — recorded as `(relpath, content-hash)`. Directories carry no
//!   bytes of their own; an empty directory is not a compile input and is not
//!   recorded (documented v0 cut).

use std::fs;
use std::io;
use std::path::Path;

use tf_proto::ContentHash;

use crate::sha256::sha256_hex;

/// Directory names pruned from the walk. Fixed and documented on purpose — see
/// the module docs for why this is not a `.gitignore` engine in v0.
pub const EXCLUDED_DIRS: &[&str] = &["target", ".git"];

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    File,
    Symlink,
}

struct Entry {
    rel: String,
    kind: Kind,
    /// File: content hash hex. Symlink: the link's textual target.
    payload: Vec<u8>,
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> io::Result<()> {
    let mut children: Vec<fs::DirEntry> = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    // Sort within a directory too; the global sort below is authoritative, but
    // this keeps recursion order stable for easier debugging.
    children.sort_by_key(std::fs::DirEntry::file_name);

    for child in children {
        let path = child.path();
        let meta = fs::symlink_metadata(&path)?;
        let name = child.file_name();
        let name = name.to_string_lossy();

        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            out.push(Entry {
                rel: rel_path(root, &path),
                kind: Kind::Symlink,
                payload: target.to_string_lossy().into_owned().into_bytes(),
            });
        } else if meta.is_dir() {
            if EXCLUDED_DIRS.contains(&name.as_ref()) {
                continue;
            }
            walk(root, &path, out)?;
        } else if meta.is_file() {
            let bytes = fs::read(&path)?;
            out.push(Entry {
                rel: rel_path(root, &path),
                kind: Kind::File,
                payload: sha256_hex(&bytes).into_bytes(),
            });
        }
        // Anything else (fifo, socket, device): not a source input — skipped.
    }
    Ok(())
}

fn rel_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    // Normalize to '/' so the digest is identical regardless of host OS.
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<String>>()
        .join("/")
}

/// Hash an entire source tree rooted at `root` into one [`ContentHash`].
///
/// Deterministic by construction: same file contents + layout ⇒ identical
/// hash, regardless of walk order or host filesystem. Any tracked byte change,
/// file add/remove, or symlink retarget changes the result.
///
/// # Errors
/// Returns the underlying [`io::Error`] if `root` cannot be traversed (missing
/// directory, permission denied, unreadable file).
pub fn hash_source_tree(root: impl AsRef<Path>) -> io::Result<ContentHash> {
    let root = root.as_ref();
    let mut entries = Vec::new();
    walk(root, root, &mut entries)?;
    // Global canonical order — independent of readdir / recursion order.
    entries.sort_by(|a, b| a.rel.cmp(&b.rel).then_with(|| a.kind.cmp(&b.kind)));

    let mut buf = Vec::new();
    buf.extend_from_slice(b"tf-cas/source-tree/v1\n");
    for e in &entries {
        let tag: u8 = match e.kind {
            Kind::File => b'F',
            Kind::Symlink => b'L',
        };
        buf.push(tag);
        buf.extend_from_slice(&(e.rel.len() as u64).to_be_bytes());
        buf.extend_from_slice(e.rel.as_bytes());
        buf.extend_from_slice(&(e.payload.len() as u64).to_be_bytes());
        buf.extend_from_slice(&e.payload);
    }
    Ok(ContentHash::new(sha256_hex(&buf)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("tf-cas-tree-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn identical_trees_hash_equal_and_order_independent() {
        let a = scratch("eq-a");
        fs::create_dir_all(a.join("src")).unwrap();
        fs::write(a.join("src/main.rs"), b"fn main() {}").unwrap();
        fs::write(a.join("Cargo.toml"), b"[package]").unwrap();

        let b = scratch("eq-b");
        // Write in a different order — digest must not depend on it.
        fs::write(b.join("Cargo.toml"), b"[package]").unwrap();
        fs::create_dir_all(b.join("src")).unwrap();
        fs::write(b.join("src/main.rs"), b"fn main() {}").unwrap();

        assert_eq!(
            hash_source_tree(&a).unwrap(),
            hash_source_tree(&b).unwrap(),
            "same content+layout ⇒ same hash, regardless of write order"
        );

        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn any_source_byte_change_changes_the_hash() {
        let d = scratch("byte");
        fs::write(d.join("a.rs"), b"fn a() {}").unwrap();
        let before = hash_source_tree(&d).unwrap();

        fs::write(d.join("a.rs"), b"fn a() { }").unwrap();
        let after = hash_source_tree(&d).unwrap();
        assert_ne!(before, after, "one byte must move the source-tree hash");

        // Revert ⇒ original hash returns (this is the AC#5 mutate-and-revert
        // primitive at the tree level).
        fs::write(d.join("a.rs"), b"fn a() {}").unwrap();
        assert_eq!(before, hash_source_tree(&d).unwrap());

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn target_and_git_are_excluded() {
        let d = scratch("excl");
        fs::write(d.join("lib.rs"), b"pub fn x() {}").unwrap();
        let clean = hash_source_tree(&d).unwrap();

        fs::create_dir_all(d.join("target/debug")).unwrap();
        fs::write(d.join("target/debug/app.wasm"), b"BUILD OUTPUT").unwrap();
        fs::create_dir_all(d.join(".git")).unwrap();
        fs::write(d.join(".git/HEAD"), b"ref: refs/heads/main").unwrap();

        assert_eq!(
            clean,
            hash_source_tree(&d).unwrap(),
            "target/ and .git/ must not affect the source-tree hash"
        );

        // But a real source addition must.
        fs::write(d.join("new.rs"), b"// added").unwrap();
        assert_ne!(clean, hash_source_tree(&d).unwrap());

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn file_add_and_remove_change_the_hash() {
        let d = scratch("addrm");
        fs::write(d.join("one.rs"), b"1").unwrap();
        let one = hash_source_tree(&d).unwrap();

        fs::write(d.join("two.rs"), b"2").unwrap();
        let two = hash_source_tree(&d).unwrap();
        assert_ne!(one, two, "adding a file must invalidate");

        fs::remove_file(d.join("two.rs")).unwrap();
        assert_eq!(one, hash_source_tree(&d).unwrap(), "removal restores it");

        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_retarget_invalidates_without_being_followed() {
        use std::os::unix::fs::symlink;
        let d = scratch("link");
        fs::write(d.join("real_a.txt"), b"A").unwrap();
        fs::write(d.join("real_b.txt"), b"B").unwrap();
        symlink("real_a.txt", d.join("link")).unwrap();
        let to_a = hash_source_tree(&d).unwrap();

        fs::remove_file(d.join("link")).unwrap();
        symlink("real_b.txt", d.join("link")).unwrap();
        let to_b = hash_source_tree(&d).unwrap();

        assert_ne!(to_a, to_b, "repointing a symlink must invalidate the hash");

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_root_is_an_io_error_not_a_panic() {
        let mut p = std::env::temp_dir();
        p.push(format!("tf-cas-does-not-exist-{}", std::process::id()));
        assert!(hash_source_tree(&p).is_err());
    }
}
