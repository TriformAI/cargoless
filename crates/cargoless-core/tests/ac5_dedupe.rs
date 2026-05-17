//! **AC#5 (CWDL-6 / CWDL-38)** — *the* hard acceptance criterion for Epic 3:
//! building the same source-tree state twice is a cache hit on the second
//! attempt, with the build **skipped entirely**.
//!
//! This is an integration test (public API only) and runs in Forgejo CI on
//! `rust:1.85-bookworm`, which has no `trunk`/`cargo-leptos`/`wasm32`. It
//! therefore injects a counting [`Compiler`] in place of the real
//! `TrunkCompiler` — the production code path (CAS lookup → skip-or-compile →
//! store) is exercised end-to-end; only the leaf shell-out is faked. "Build
//! skipped" is asserted *directly*: the fake compiler's invocation count must
//! not advance across a dedupe.
//!
//! The scenario follows CWDL-38 verbatim: build a source state, mutate, then
//! **revert**, and assert the reverted state is a cache hit — proving the key
//! is a pure function of input content, not of build history.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fs, io};

use cargoless_core::LocalDiskStore;
use cargoless_core::build::{BuildOrchestrator, Compiler, assemble_identity};
use cargoless_core::{BuildOutcome, BuildTrigger, Profile, TargetTriple};

/// A stand-in for `trunk build` that records how many times it actually ran.
/// The whole point of AC#5 is that this counter does **not** advance on a
/// dedupe — a real compile being skipped is the observable guarantee.
struct CountingCompiler {
    runs: Arc<AtomicUsize>,
}

impl Compiler for CountingCompiler {
    fn compile(
        &self,
        _project_root: &Path,
        _identity: &cargoless_core::BuildIdentity,
    ) -> Result<Vec<u8>, String> {
        let n = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        // Deterministic "artifact" so a cache hit and a fresh compile are
        // distinguishable only by the outcome, never by accident.
        Ok(format!("wasm-artifact-build-{n}").into_bytes())
    }
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("tf-ac5-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    p
}

fn write_project(root: &Path) -> io::Result<()> {
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join("Cargo.toml"), b"[package]\nname=\"app\"\n")?;
    fs::write(root.join("Cargo.lock"), b"# resolved deps\n")?;
    fs::write(
        root.join("rust-toolchain.toml"),
        b"[toolchain]\nchannel=\"1.85.0\"\n",
    )?;
    fs::write(root.join("src/main.rs"), b"fn main() { println!(\"v1\"); }")?;
    fs::write(
        root.join("src/lib.rs"),
        b"pub fn add(a:i32,b:i32)->i32{a+b}",
    )?;
    Ok(())
}

fn trigger_for(project: &Path) -> BuildTrigger {
    let identity = assemble_identity(
        project,
        TargetTriple::new("wasm32-unknown-unknown"),
        Profile::Dev,
    )
    .expect("assemble identity");
    BuildTrigger { identity }
}

#[test]
fn same_source_state_twice_is_a_cache_hit_and_build_is_skipped() {
    let project = scratch("proj");
    write_project(&project).unwrap();

    let runs = Arc::new(AtomicUsize::new(0));
    // The CAS cache MUST live outside the watched source tree (D6/D10 global
    // cache root). If it were under `project/`, the first build's stored
    // artifact would become a new source-tree input and the second
    // `assemble_identity` would see a different state — defeating dedupe.
    let store = LocalDiskStore::new(scratch("dedupe-cache"));
    let orch = BuildOrchestrator::new(store, CountingCompiler { runs: runs.clone() }, &project);

    // 1. First build of this exact source state → a real compile.
    let r1 = orch.run(&trigger_for(&project));
    assert_eq!(r1.outcome, BuildOutcome::Compiled, "first build compiles");
    assert!(r1.outcome.is_servable());
    let meta1 = r1.artifact.expect("compiled artifact has metadata");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "exactly one compile so far");

    // 2. Identical source state again → dedupe, compiler NOT called again.
    let r2 = orch.run(&trigger_for(&project));
    assert_eq!(
        r2.outcome,
        BuildOutcome::Deduplicated,
        "identical state ⇒ cache hit"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "AC#5: the compile must be skipped ENTIRELY on the second attempt"
    );
    assert_eq!(
        r2.artifact.expect("dedup carries provenance").input_hash,
        meta1.input_hash,
        "the dedup resolves to the very same CAS key"
    );

    // 3. Mutate a source file → a genuinely different input set ⇒ recompile.
    fs::write(
        project.join("src/main.rs"),
        b"fn main() { println!(\"v2\"); }",
    )
    .unwrap();
    let r3 = orch.run(&trigger_for(&project));
    assert_eq!(
        r3.outcome,
        BuildOutcome::Compiled,
        "a real source change must NOT dedupe"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the edit forced a recompile"
    );
    assert_ne!(
        r3.artifact.expect("meta").input_hash,
        meta1.input_hash,
        "different inputs ⇒ different CAS key"
    );

    // 4. Revert byte-for-byte → the key returns ⇒ cache hit, build skipped.
    fs::write(
        project.join("src/main.rs"),
        b"fn main() { println!(\"v1\"); }",
    )
    .unwrap();
    let r4 = orch.run(&trigger_for(&project));
    assert_eq!(
        r4.outcome,
        BuildOutcome::Deduplicated,
        "mutate-and-revert returns to the cached state (CWDL-38)"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "AC#5: the reverted state is served from cache, no third compile"
    );
    assert_eq!(
        r4.artifact.expect("meta").input_hash,
        meta1.input_hash,
        "reverted source ⇒ original CAS key, proving the key is content-pure"
    );

    let _ = fs::remove_dir_all(&project);
}

/// A profile flip (Dev → Release) must never alias a dev artifact even though
/// every other input is byte-identical — the AC#4 provenance face of AC#5.
#[test]
fn release_profile_does_not_alias_a_dev_artifact() {
    let project = scratch("profile");
    write_project(&project).unwrap();

    let runs = Arc::new(AtomicUsize::new(0));
    let orch = BuildOrchestrator::new(
        // Out-of-tree cache root (see the dedupe test for why).
        LocalDiskStore::new(scratch("profile-cache")),
        CountingCompiler { runs: runs.clone() },
        &project,
    );

    let dev = BuildTrigger {
        identity: assemble_identity(
            &project,
            TargetTriple::new("wasm32-unknown-unknown"),
            Profile::Dev,
        )
        .unwrap(),
    };
    let release = BuildTrigger {
        identity: assemble_identity(
            &project,
            TargetTriple::new("wasm32-unknown-unknown"),
            Profile::Release,
        )
        .unwrap(),
    };

    assert_eq!(orch.run(&dev).outcome, BuildOutcome::Compiled);
    assert_eq!(
        orch.run(&release).outcome,
        BuildOutcome::Compiled,
        "release must compile fresh, not serve the dev artifact"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    assert_eq!(
        orch.run(&dev).outcome,
        BuildOutcome::Deduplicated,
        "dev is still cached and still dedupes"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 2);

    let _ = fs::remove_dir_all(&project);
}
