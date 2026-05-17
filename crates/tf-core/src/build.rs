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
//! [`BuildOutcome::Failed`].
//!
//! ## v0 is a publisher, not a server
//!
//! v0 is a headless continuous checker + **latest-green publisher** (no HTTP,
//! no WebSocket — that is v0.1). On every *servable* green build (`Compiled`
//! or `Deduplicated`) this layer atomically advances a canonical pointer file
//! `<project>/.cargoless/latest-green` (a [`PublishedArtifact`] rendered by
//! the `tf-proto` codec) to the new CAS artifact. **AC#4 — never publish
//! red:** a red tree never reaches here, and a failed build *or* a failed
//! pointer swap leaves the previous pointer byte-untouched (fail closed). The
//! CLI `status` / `build --watch --out` read the pointer via
//! [`read_latest_green`] and fetch bytes with `ContentStore::get`.
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

use std::fs;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use tf_proto::{
    ArtifactMeta, BuildIdentity, BuildOutcome, BuildResult, BuildTrigger, Profile,
    PublishedArtifact, TargetTriple, UnixSeconds,
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
/// packs the resulting `dist/` into a single deterministic blob for the CAS.
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
        pack_dir(&dist).map_err(|e| format!("could not read trunk dist/: {e}"))
    }
}

/// Deterministically serialize a directory tree into one byte blob (sorted,
/// length-prefixed) so an identical `dist/` always produces identical CAS
/// bytes. Not a general archive format — just a stable, unambiguous dump.
fn pack_dir(root: &Path) -> io::Result<Vec<u8>> {
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
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut buf = Vec::new();
    buf.extend_from_slice(b"tf-core/dist/v1\n");
    for (rel, bytes) in &files {
        buf.extend_from_slice(&(rel.len() as u64).to_be_bytes());
        buf.extend_from_slice(rel.as_bytes());
        buf.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        buf.extend_from_slice(bytes);
    }
    Ok(buf)
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

    /// Run one build request, then publish the latest-green pointer.
    ///
    /// * input set already in the CAS ⇒ [`BuildOutcome::Deduplicated`], the
    ///   compiler is **never invoked** (AC#5) — but the pointer is *still*
    ///   advanced: a dedup hit IS the current latest green;
    /// * otherwise compile, store, verify ⇒ [`BuildOutcome::Compiled`];
    /// * compile failure, any CAS I/O error, **or** an inability to advance
    ///   the pointer ⇒ [`BuildOutcome::Failed`] with `artifact: None`. The
    ///   prior pointer is left byte-untouched (**AC#4: never publish red**).
    ///   Failures are `Failed`, never a panic: the daemon must not crash
    ///   because a disk hiccuped.
    pub fn run(&self, trigger: &BuildTrigger) -> BuildResult {
        let key = input_hash(&trigger.identity);
        let meta = ArtifactMeta {
            input_hash: key.clone(),
            identity: trigger.identity.clone(),
        };

        match self.store.contains(&key) {
            // CAS hit: skip the compile (AC#5) but still advance the pointer.
            Ok(true) => return self.publish_and_report(BuildOutcome::Deduplicated, meta),
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
        // Verify the artifact is actually retrievable before we ever point at
        // it — never advance latest-green to a key the CAS cannot serve.
        match self.store.contains(&key) {
            Ok(true) => {}
            Ok(false) => {
                return failed("CAS reported store success but artifact is absent".to_owned());
            }
            Err(e) => return failed(format!("CAS verify failed: {e}")),
        }

        self.publish_and_report(BuildOutcome::Compiled, meta)
    }

    /// Advance the latest-green pointer, then report. **AC#4 (Option A,
    /// ratified):** if the artifact is safe in the CAS but the pointer cannot
    /// be advanced, fail *closed* — return `Failed` so consumers keep the
    /// prior last-green. The artifact stays cached, so the next trigger
    /// dedups + republishes cheaply.
    fn publish_and_report(&self, outcome: BuildOutcome, meta: ArtifactMeta) -> BuildResult {
        match publish_latest_green(&self.project_root, &meta) {
            Ok(()) => BuildResult {
                outcome,
                artifact: Some(meta),
            },
            Err(e) => failed(format!(
                "artifact built but could not advance latest-green pointer: {e}"
            )),
        }
    }
}

/// The canonical latest-green pointer path for a project root:
/// `<project_root>/.cargoless/latest-green`. The CLI (`status`,
/// `build --watch --out`) reads this; nothing else writes it.
pub fn latest_green_path(project_root: &Path) -> PathBuf {
    project_root.join(".cargoless").join("latest-green")
}

/// Read and parse the latest-green pointer, if a green build has been
/// published. `Ok(None)` ⇒ no green yet (pointer absent). A present-but-corrupt
/// pointer is an [`io::ErrorKind::InvalidData`] error, never a silent wrong
/// artifact. Consumers then fetch bytes via `ContentStore::get(&meta.input_hash)`.
pub fn read_latest_green(project_root: &Path) -> io::Result<Option<PublishedArtifact>> {
    match fs::read_to_string(latest_green_path(project_root)) {
        Ok(text) => PublishedArtifact::parse(&text)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Atomically advance the canonical pointer to `meta` (**AC#4**). The new
/// record is written to a temp file in the **same directory** (so the
/// `rename` is same-filesystem and therefore atomic), `fsync`'d, then renamed
/// over the live pointer. The live pointer is never written in place: a crash
/// or a full disk leaves the previous green pointer byte-intact, never torn.
fn publish_latest_green(project_root: &Path, meta: &ArtifactMeta) -> io::Result<()> {
    let published_at = UnixSeconds(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    let record = PublishedArtifact {
        artifact: meta.clone(),
        published_at,
    };

    let dir = project_root.join(".cargoless");
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".latest-green.{}.tmp", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(record.render().as_bytes())?;
        f.sync_all()?;
    }
    match fs::rename(&tmp, dir.join("latest-green")) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Don't leave a stale temp behind if the swap failed.
            let _ = fs::remove_file(&tmp);
            Err(e)
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
    fn compiled_and_deduplicated_both_advance_the_pointer() {
        let dir = scratch("publish");
        let orch = BuildOrchestrator::new(
            LocalDiskStore::new(dir.join("cas")),
            CountingCompiler {
                calls: Cell::new(0),
                bytes: b"artifact".to_vec(),
            },
            &dir,
        );
        let trig = BuildTrigger { identity: ident() };

        assert_eq!(orch.run(&trig).outcome, BuildOutcome::Compiled);
        let p1 = read_latest_green(&dir)
            .unwrap()
            .expect("pointer advanced on first green build");
        assert_eq!(p1.artifact.input_hash, input_hash(&ident()));
        assert!(p1.published_at.0 > 0, "a real timestamp is recorded");

        // Dedup hit still republishes (it IS the current latest green).
        assert_eq!(orch.run(&trig).outcome, BuildOutcome::Deduplicated);
        assert_eq!(orch.compiler.calls.get(), 1, "AC#5: compile still skipped");
        let p2 = read_latest_green(&dir).unwrap().expect("pointer present");
        assert_eq!(p2.artifact.input_hash, p1.artifact.input_hash);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_build_never_moves_the_pointer() {
        struct Boom;
        impl Compiler for Boom {
            fn compile(&self, _r: &Path, _i: &BuildIdentity) -> Result<Vec<u8>, String> {
                Err("trunk exploded".to_owned())
            }
        }
        let dir = scratch("nopublish");
        // First, a good build to establish a green pointer.
        let good = BuildOrchestrator::new(
            LocalDiskStore::new(dir.join("cas")),
            CountingCompiler {
                calls: Cell::new(0),
                bytes: b"green-1".to_vec(),
            },
            &dir,
        );
        assert_eq!(
            good.run(&BuildTrigger { identity: ident() }).outcome,
            BuildOutcome::Compiled
        );
        let before = fs::read(latest_green_path(&dir)).expect("pointer exists");

        // Now a failing build with a *different* identity (so it is not a
        // dedup hit) must NOT touch the pointer (AC#4).
        let mut other = ident();
        other.source_tree = tf_cas::content_hash(b"different-source");
        let bad = BuildOrchestrator::new(LocalDiskStore::new(dir.join("cas")), Boom, &dir);
        let r = bad.run(&BuildTrigger { identity: other });
        assert!(matches!(r.outcome, BuildOutcome::Failed { .. }));
        assert!(r.artifact.is_none());

        let after = fs::read(latest_green_path(&dir)).expect("pointer still exists");
        assert_eq!(
            before, after,
            "AC#4: a failed build leaves the pointer byte-identical"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pointer_write_failure_is_failed_closed_option_a() {
        // Option A (ratified): artifact lands in the CAS, but if the pointer
        // cannot be advanced the result is Failed / artifact None — consumers
        // keep last-green; the cached artifact makes the retry cheap.
        let dir = scratch("ptrfail");
        // Make `.cargoless` a *regular file* so create_dir_all(.cargoless)
        // fails ⇒ the pointer can never be written.
        fs::write(dir.join(".cargoless"), b"not a directory").unwrap();

        let store = LocalDiskStore::new(dir.join("cas"));
        let orch = BuildOrchestrator::new(
            store,
            CountingCompiler {
                calls: Cell::new(0),
                bytes: b"artifact".to_vec(),
            },
            &dir,
        );
        let r = orch.run(&BuildTrigger { identity: ident() });
        match r.outcome {
            BuildOutcome::Failed { reason } => {
                assert!(
                    reason.contains("could not advance latest-green pointer"),
                    "reason names the publish failure: {reason}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(
            r.artifact.is_none(),
            "fail closed: no servable artifact reported"
        );
        // But the artifact IS in the CAS (so the retry dedups + republishes).
        assert!(
            LocalDiskStore::new(dir.join("cas"))
                .contains(&input_hash(&ident()))
                .unwrap(),
            "artifact is safely cached despite the publish failure"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
