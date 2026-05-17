//! Build orchestration (Epic 3, CWDL-35) — the layer between a green verdict
//! and a servable artifact.
//!
//! Data flow (from the `tf-proto` contract): the model emits
//! `StateEvent::BecameGreen { identity }`; the daemon turns that into a
//! [`BuildTrigger`]; this orchestrator either finds the input set already in
//! the CAS (→ [`BuildOutcome::Deduplicated`], **no compile** — the observable
//! proof of AC#5) or shells out to `trunk build`, stores the artifact, and
//! returns [`BuildOutcome::Compiled`]. A build that fails despite a green
//! verdict (a link/toolchain error the analyzer cannot see) becomes
//! [`BuildOutcome::Failed`] and the server keeps serving last-green (AC#4).
//!
//! ## Why `trunk build` is wrapped, not reimplemented
//!
//! CWDL-35 is explicit and the v0 parking lot agrees: replacing Trunk's
//! machinery is premature optimization. v0 *wraps* `trunk build` (debug
//! profile, no `wasm-opt` — the AC#3 latency constraint, enforced by the
//! workspace `[profile.dev]`), and earns its speed from the **CAS skip**, not
//! from out-building Trunk.
//!
//! ## Why the compiler is a trait
//!
//! The entire test suite runs in Forgejo CI on `rust:1.85-bookworm` with **no
//! `trunk`, no `cargo-leptos`, no `wasm32` target**. A build orchestrator that
//! could only ever shell out to a real `trunk` would be untestable here — and
//! AC#5 ("same state twice ⇒ build skipped") is a *hard* acceptance criterion
//! that must be proven in CI. So compilation is abstracted behind
//! [`Compiler`]: [`TrunkCompiler`] is the real shell-out; tests inject a
//! counting fake and assert the second identical trigger never calls it.
//!
//! ## Artifact framing — `tf_proto::Bundle` (build↔server seam)
//!
//! A WASM dev build is several files (HTML shell, JS loader, `_bg.wasm`,
//! assets) but the CAS contract stores **one opaque blob per
//! [`InputHash`](tf_proto::InputHash)**. The blob this layer stores is
//! therefore exactly `tf_proto::Bundle::pack(<dist tree>)`, and the dev server
//! recovers the files with `tf_proto::Bundle::unpack` — one shared framing,
//! owned by the contract crate, never a parallel format. The byte layout
//! (4-byte BE entry count; per entry 2-byte BE path-len, UTF-8 path, 8-byte BE
//! content-len, content; entries ordered by the bundle's `BTreeMap` key) is
//! the contract; this crate only *produces* it from the `dist/` tree.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use tf_proto::{
    ArtifactMeta, BuildIdentity, BuildOutcome, BuildResult, BuildTrigger, Bundle, Profile,
    TargetTriple,
};

use tf_cas::{ContentStore, absent_marker, content_hash, hash_source_tree, input_hash};

/// Produces artifact bytes for a green [`BuildIdentity`], or a one-line failure
/// reason. Implemented by [`TrunkCompiler`] in production and by a counting
/// fake in the AC#5 test (CI has no real `trunk`).
pub trait Compiler {
    /// Compile the project at `project_root`. `identity` is the assembled
    /// input set (provenance / logging). Return the bytes to cache on success,
    /// or a human-readable one-liner on failure (no structured diagnostic —
    /// the same v0-simple cut as `FileState`).
    fn compile(&self, project_root: &Path, identity: &BuildIdentity) -> Result<Vec<u8>, String>;
}

/// Real compiler: shells out to `trunk build` in debug / no-`wasm-opt` mode and
/// packs the resulting `dist/` into a [`tf_proto::Bundle`] blob for the CAS —
/// the exact bytes the dev server `Bundle::unpack`s back.
#[derive(Debug, Default, Clone, Copy)]
pub struct TrunkCompiler;

impl Compiler for TrunkCompiler {
    fn compile(&self, project_root: &Path, _identity: &BuildIdentity) -> Result<Vec<u8>, String> {
        // No `--release` ⇒ debug profile and Trunk skips `wasm-opt` (AC#3).
        let output = Command::new("trunk")
            .arg("build")
            .current_dir(project_root)
            .output()
            .map_err(|e| format!("could not launch `trunk build`: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let reason = stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(str::trim)
                .map(str::to_owned)
                .unwrap_or_else(|| match output.status.code() {
                    Some(c) => format!("`trunk build` exited with status {c}"),
                    None => "`trunk build` terminated by signal".to_owned(),
                });
            return Err(reason);
        }

        let dist = project_root.join("dist");
        if !dist.is_dir() {
            return Err("`trunk build` succeeded but produced no dist/ directory".to_owned());
        }
        let entries =
            collect_dist_entries(&dist).map_err(|e| format!("could not read trunk dist/: {e}"))?;
        // The CAS blob *is* the server's Bundle: store `Bundle::pack` bytes so
        // the dev server can `Bundle::unpack` straight out of the CAS. The
        // bundle's `BTreeMap` gives a deterministic order, so an identical
        // `dist/` always yields identical CAS bytes (and an identical
        // `InputHash`-keyed entry).
        Ok(Bundle::from_entries(entries).pack())
    }
}

/// Walk `root` (the `dist/` tree) into `(forward-slash relpath, bytes)` pairs
/// for [`Bundle::from_entries`]. Order does not matter — the bundle re-keys
/// into a `BTreeMap` — but the walk is still deterministic. Symlinks are not
/// followed (a dev `dist/` is plain files; this avoids escaping the tree).
fn collect_dist_entries(root: &Path) -> io::Result<Vec<(String, Vec<u8>)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> io::Result<()> {
        let mut kids: Vec<fs::DirEntry> = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
        kids.sort_by_key(std::fs::DirEntry::file_name);
        for k in kids {
            let path = k.path();
            let meta = fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                walk(root, &path, out)?;
            } else if meta.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<String>>()
                    .join("/");
                out.push((rel, fs::read(&path)?));
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

fn hash_optional_file(path: &Path, kind: &str) -> io::Result<tf_proto::ContentHash> {
    match fs::read(path) {
        Ok(bytes) => Ok(content_hash(&bytes)),
        // Absent is a *distinct, deterministic* state — see `absent_marker`.
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(absent_marker(kind)),
        Err(e) => Err(e),
    }
}

/// Assemble the full [`BuildIdentity`] for the project rooted at
/// `project_root`. The daemon does this on a green transition; it is also the
/// exact thing the AC#5 mutate-and-revert test drives.
///
/// Components (CWDL-33): whole source-tree content hash + `Cargo.lock` +
/// `rust-toolchain.toml` + `tf.toml` (D6 location; absent is a stable distinct
/// state, not an error) + target triple + profile.
///
/// # Errors
/// Propagates any [`io::Error`] from walking the source tree or reading a
/// present-but-unreadable lock/toolchain/config file.
pub fn assemble_identity(
    project_root: impl AsRef<Path>,
    target: TargetTriple,
    profile: Profile,
) -> io::Result<BuildIdentity> {
    let root = project_root.as_ref();
    Ok(BuildIdentity {
        source_tree: hash_source_tree(root)?,
        cargo_lock: hash_optional_file(&root.join("Cargo.lock"), "cargo_lock")?,
        rust_toolchain: hash_optional_file(&root.join("rust-toolchain.toml"), "rust_toolchain")?,
        tf_config: hash_optional_file(&root.join("tf.toml"), "tf_config")?,
        target,
        profile,
    })
}

/// Turns a [`BuildTrigger`] into a [`BuildResult`], skipping the compile
/// entirely on a CAS hit. Holds the project root (the daemon constructs one
/// per watched project) and a pluggable [`Compiler`].
pub struct BuildOrchestrator<S: ContentStore, C: Compiler> {
    store: S,
    compiler: C,
    project_root: PathBuf,
}

impl<S: ContentStore, C: Compiler> BuildOrchestrator<S, C> {
    pub fn new(store: S, compiler: C, project_root: impl Into<PathBuf>) -> Self {
        Self {
            store,
            compiler,
            project_root: project_root.into(),
        }
    }

    /// Run one build request.
    ///
    /// * input set already in the CAS ⇒ [`BuildOutcome::Deduplicated`], the
    ///   compiler is **never invoked** (AC#5);
    /// * otherwise compile, store, ⇒ [`BuildOutcome::Compiled`];
    /// * compile failure *or* any CAS I/O error ⇒ [`BuildOutcome::Failed`]
    ///   with `artifact: None` — the server then holds last-green (AC#4). A
    ///   storage error is deliberately a `Failed`, never a panic: the daemon
    ///   must not crash because a disk hiccuped.
    pub fn run(&self, trigger: &BuildTrigger) -> BuildResult {
        let key = input_hash(&trigger.identity);

        match self.store.contains(&key) {
            Ok(true) => {
                return BuildResult {
                    outcome: BuildOutcome::Deduplicated,
                    artifact: Some(ArtifactMeta {
                        input_hash: key,
                        identity: trigger.identity.clone(),
                    }),
                };
            }
            Ok(false) => {}
            Err(e) => return failed(format!("CAS lookup failed: {e}")),
        }

        let bytes = match self.compiler.compile(&self.project_root, &trigger.identity) {
            Ok(b) => b,
            Err(reason) => return failed(reason),
        };

        if let Err(e) = self.store.put(&key, &bytes) {
            return failed(format!("CAS store failed: {e}"));
        }

        BuildResult {
            outcome: BuildOutcome::Compiled,
            artifact: Some(ArtifactMeta {
                input_hash: key,
                identity: trigger.identity.clone(),
            }),
        }
    }
}

fn failed(reason: String) -> BuildResult {
    BuildResult {
        outcome: BuildOutcome::Failed { reason },
        artifact: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;
    use tf_cas::LocalDiskStore;

    fn scratch(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("tf-core-build-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    struct CountingCompiler {
        calls: Cell<usize>,
        bytes: Vec<u8>,
    }
    impl Compiler for CountingCompiler {
        fn compile(&self, _root: &Path, _id: &BuildIdentity) -> Result<Vec<u8>, String> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.bytes.clone())
        }
    }

    fn ident() -> BuildIdentity {
        BuildIdentity {
            source_tree: tf_cas::content_hash(b"s"),
            cargo_lock: tf_cas::content_hash(b"l"),
            rust_toolchain: tf_cas::content_hash(b"t"),
            tf_config: tf_cas::content_hash(b"c"),
            target: TargetTriple::new("wasm32-unknown-unknown"),
            profile: Profile::Dev,
        }
    }

    #[test]
    fn first_build_compiles_second_is_deduplicated() {
        let dir = scratch("dedupe");
        let store = LocalDiskStore::new(dir.join("cas"));
        let compiler = CountingCompiler {
            calls: Cell::new(0),
            bytes: b"artifact".to_vec(),
        };
        let orch = BuildOrchestrator::new(store, compiler, &dir);
        let trig = BuildTrigger { identity: ident() };

        let r1 = orch.run(&trig);
        assert_eq!(r1.outcome, BuildOutcome::Compiled);
        assert!(r1.artifact.is_some());

        let r2 = orch.run(&trig);
        assert_eq!(
            r2.outcome,
            BuildOutcome::Deduplicated,
            "identical identity ⇒ cache hit"
        );
        assert_eq!(
            orch.compiler.calls.get(),
            1,
            "the compile MUST be skipped on the dedupe path (AC#5)"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compile_failure_holds_last_green() {
        struct Boom;
        impl Compiler for Boom {
            fn compile(&self, _r: &Path, _i: &BuildIdentity) -> Result<Vec<u8>, String> {
                Err("linker exploded".to_owned())
            }
        }
        let dir = scratch("fail");
        let orch = BuildOrchestrator::new(LocalDiskStore::new(dir.join("cas")), Boom, &dir);
        let r = orch.run(&BuildTrigger { identity: ident() });
        assert_eq!(
            r.outcome,
            BuildOutcome::Failed {
                reason: "linker exploded".to_owned()
            }
        );
        assert!(
            r.artifact.is_none(),
            "no artifact ⇒ server holds last-green"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn assemble_identity_is_stable_and_revert_sensitive() {
        let proj = scratch("assemble");
        fs::create_dir_all(proj.join("src")).unwrap();
        fs::write(proj.join("src/main.rs"), b"fn main() {}").unwrap();
        fs::write(proj.join("Cargo.lock"), b"# lock").unwrap();

        let id1 = assemble_identity(
            &proj,
            TargetTriple::new("wasm32-unknown-unknown"),
            Profile::Dev,
        )
        .unwrap();
        let id_again = assemble_identity(
            &proj,
            TargetTriple::new("wasm32-unknown-unknown"),
            Profile::Dev,
        )
        .unwrap();
        assert_eq!(id1, id_again, "unchanged tree ⇒ identical identity");

        fs::write(proj.join("src/main.rs"), b"fn main() { /* x */ }").unwrap();
        let id2 = assemble_identity(
            &proj,
            TargetTriple::new("wasm32-unknown-unknown"),
            Profile::Dev,
        )
        .unwrap();
        assert_ne!(id1, id2, "a source edit ⇒ different identity");

        fs::write(proj.join("src/main.rs"), b"fn main() {}").unwrap();
        let id3 = assemble_identity(
            &proj,
            TargetTriple::new("wasm32-unknown-unknown"),
            Profile::Dev,
        )
        .unwrap();
        assert_eq!(id1, id3, "revert ⇒ original identity returns");

        // Absent tf.toml (D6 open) must not break assembly.
        assert!(!proj.join("tf.toml").exists());
        let _ = fs::remove_dir_all(&proj);
    }

    #[test]
    fn dist_packs_into_a_server_unpackable_bundle() {
        // De-risks the build↔server seam: the bytes TrunkCompiler stores in
        // the CAS must be exactly what the dev server `Bundle::unpack`s. We
        // can't run real `trunk` in CI, so drive `collect_dist_entries` over a
        // hand-built `dist/` and round-trip through the shared contract type.
        let dist = scratch("dist").join("dist");
        fs::create_dir_all(dist.join("assets")).unwrap();
        fs::write(dist.join("index.html"), b"<body>hi</body>").unwrap();
        fs::write(dist.join("app_bg.wasm"), b"\0asm\x01\0\0\0").unwrap();
        fs::write(dist.join("assets/logo.svg"), b"<svg/>").unwrap();

        let entries = collect_dist_entries(&dist).unwrap();
        let packed = Bundle::from_entries(entries).pack();

        // Identical tree ⇒ identical CAS bytes (so the InputHash-keyed entry
        // is stable and AC#5 dedupe holds on the real compile path too).
        let packed_again = Bundle::from_entries(collect_dist_entries(&dist).unwrap()).pack();
        assert_eq!(packed, packed_again, "dist pack must be deterministic");

        // The server side recovers every file, addressed by request path.
        let b = Bundle::unpack(&packed).expect("server can unpack build output");
        assert_eq!(b.get("index.html"), Some(&b"<body>hi</body>"[..]));
        assert_eq!(b.get("app_bg.wasm"), Some(&b"\0asm\x01\0\0\0"[..]));
        assert_eq!(b.get("assets/logo.svg"), Some(&b"<svg/>"[..]));
        assert_eq!(b.get("/index.html"), Some(&b"<body>hi</body>"[..]));
        assert_eq!(b.len(), 3);

        let _ = fs::remove_dir_all(dist.parent().unwrap());
    }
}
