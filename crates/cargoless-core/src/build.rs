//! Build orchestration (Epic 3, CWDL-35) — the layer between a green verdict
//! and a servable artifact.
//!
//! Data flow (from the `cargoless-proto` contract): the model emits
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
//! the `cargoless-proto` codec) to the new CAS artifact. **AC#4 — never publish
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

use cargoless_proto::{
    ArtifactMeta, BuildIdentity, BuildOutcome, BuildResult, BuildTrigger, Profile,
    PublishedArtifact, TargetTriple, UnixSeconds,
};

use cargoless_cas::{ContentStore, absent_marker, content_hash, hash_source_tree, input_hash};

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
            // FIELD FINDING #12: exit code is the gate (correct here);
            // the failure-reason extraction needs to actually find error
            // lines rather than blindly taking the last stderr line —
            // the dogfood reproducer caught us surfacing cargo's
            // "Finished `dev` profile … in 0.16s" success message as if
            // it were a failure cause, because that line happened to be
            // the very last non-empty stderr line after trunk's actual
            // wasm-bindgen / dist-assembly error a few lines earlier.
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(extract_trunk_failure_reason(
                &stdout,
                &stderr,
                output.status.code(),
            ));
        }

        let dist = project_root.join("dist");
        if !dist.is_dir() {
            return Err("`trunk build` succeeded but produced no dist/ directory".to_owned());
        }
        pack_dir(&dist).map_err(|e| format!("could not read trunk dist/: {e}"))
    }
}

/// FIELD FINDING #12: extract an actionable failure-reason from
/// trunk-build's combined output streams, given that the subprocess
/// already exited non-zero.
///
/// The naive "last non-empty stderr line" picker (pre-#12 behavior)
/// surfaced cargo's `Finished \`dev\` profile [unoptimized + debuginfo]
/// target(s) in 0.16s` — a SUCCESS message — as the failure reason,
/// because that's the order trunk happens to emit when wasm-bindgen
/// fails after cargo-check succeeds. Result: user sees "build failed —
/// holding last green: Finished dev profile" and the publisher half of
/// AC#4 looks non-functional even when it's actually trying.
///
/// Strategy (deterministic, pure):
///   1. Scan BOTH stdout and stderr for lines whose first
///      whitespace-delimited word is "error", "error[E…]", or "ERROR"
///      (rustc, cargo, and trunk's canonical error prefixes). Keep up
///      to the last 5 — recency-biased + bounded so multi-error builds
///      stay readable.
///   2. If none found, fall back to the last 3 non-empty stderr lines
///      joined by ` | ` — better than the prior 1-line picker because
///      cargo's "Finished" success message no longer appears in
///      isolation; the user gets a small window of context.
///   3. Empty output ⇒ name the exit code explicitly.
///
/// All paths include the exit code so a script/tester can correlate
/// against trunk's own diagnostics.
pub(crate) fn extract_trunk_failure_reason(
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
) -> String {
    let exit_label = match exit_code {
        Some(c) => format!("exit {c}"),
        None => "signal".to_owned(),
    };

    // Step 1: scan both streams for error-prefix lines.
    let error_lines: Vec<String> = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| is_error_prefix_line(l))
        .map(str::to_owned)
        .collect();

    if !error_lines.is_empty() {
        let take_n = error_lines.len().min(5);
        let summary = error_lines[error_lines.len() - take_n..].join(" | ");
        return format!("`trunk build` {exit_label} — {summary}");
    }

    // Step 2: no error-prefix lines — fall back to a small stderr tail.
    let stderr_tail: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if stderr_tail.is_empty() {
        return format!(
            "`trunk build` {exit_label} with no diagnostic output \
             (run `trunk build` directly to investigate)"
        );
    }
    let take_n = stderr_tail.len().min(3);
    let context = stderr_tail[stderr_tail.len() - take_n..].join(" | ");
    format!(
        "`trunk build` {exit_label} — no canonical `error:` line found; \
         last {take_n} stderr line(s): {context}"
    )
}

/// True iff the first whitespace-delimited token of `line` is a known
/// error-prefix used by rustc / cargo / trunk:
///   * `error:`           — cargo / rustc plain
///   * `error[E0277]:`    — rustc with code
///   * `ERROR` (any case) — trunk's `ERROR ❌ …` prefix
///
/// Crucially, cargo's `Finished` / `Compiling` / `Building` SUCCESS
/// markers do NOT match this — that's the #12 invariant.
fn is_error_prefix_line(line: &str) -> bool {
    let Some(first) = line.split_whitespace().next() else {
        return false;
    };
    let first_lc = first.to_ascii_lowercase();
    first_lc == "error:"
        || first_lc.starts_with("error[")
        || first_lc == "error"
        // Trunk variants: `ERROR ❌ ...`, sometimes just `ERROR`.
        || first == "ERROR"
}

/// Magic+version prefix of the v0 CAS artifact blob. Bumping it is a
/// deliberate, repo-visible format change — [`unpack_artifact`] rejects any
/// other header rather than mis-expanding an old blob into `--out`.
const DIST_BLOB_HEADER: &[u8] = b"tf-core/dist/v1\n";

/// Deterministically serialize a directory tree into one byte blob (sorted,
/// length-prefixed) so an identical `dist/` always produces identical CAS
/// bytes. Not a general archive format — just a stable, unambiguous dump whose
/// only reader is [`unpack_artifact`] (the blob layout is owned here; the CLI
/// never parses CAS internals).
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
    buf.extend_from_slice(DIST_BLOB_HEADER);
    for (rel, bytes) in &files {
        buf.extend_from_slice(&(rel.len() as u64).to_be_bytes());
        buf.extend_from_slice(rel.as_bytes());
        buf.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        buf.extend_from_slice(bytes);
    }
    Ok(buf)
}

/// Expand a v0 CAS artifact blob (the [`pack_dir`] framing produced by
/// [`TrunkCompiler`]) into `out_dir`, faithfully recreating the original
/// `dist/` tree. This is the **inverse of the packer and the only sanctioned
/// reader of the blob layout** — the CLI calls this so it never has to know
/// the container format (the open flag cli-ux raised for `build --watch
/// --out`).
///
/// Strict: a wrong/absent header, a truncated record, or a length that
/// overruns the buffer ⇒ `Err` (a corrupt artifact is never half-expanded into
/// a servable dir). Path-safe: each entry path is rebuilt from its
/// forward-slash components with `.`/`..`/absolute/empty segments rejected, so
/// a malformed blob can never escape `out_dir`. Existing files at the same
/// relative paths are overwritten; unrelated pre-existing files are left as-is
/// (v0-simple — the caller owns whether to clear `out_dir` first).
///
/// # Errors
/// [`io::ErrorKind::InvalidData`] for a malformed blob; the underlying
/// [`io::Error`] for a filesystem failure under `out_dir`.
pub fn unpack_artifact(blob: &[u8], out_dir: &Path) -> io::Result<()> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());

    let mut cur = blob
        .strip_prefix(DIST_BLOB_HEADER)
        .ok_or_else(|| bad("not a cargoless dist blob (bad/absent header)"))?;

    let take = |cur: &mut &[u8], n: usize| -> io::Result<Vec<u8>> {
        if cur.len() < n {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "artifact blob truncated",
            ));
        }
        let (head, tail) = cur.split_at(n);
        *cur = tail;
        Ok(head.to_vec())
    };
    let take_u64 = |cur: &mut &[u8]| -> io::Result<u64> {
        Ok(u64::from_be_bytes(
            take(cur, 8)?
                .try_into()
                .map_err(|_| bad("short length field"))?,
        ))
    };

    while !cur.is_empty() {
        let rel_len =
            usize::try_from(take_u64(&mut cur)?).map_err(|_| bad("path length exceeds usize"))?;
        let rel = String::from_utf8(take(&mut cur, rel_len)?)
            .map_err(|_| bad("entry path is not UTF-8"))?;
        let content_len = usize::try_from(take_u64(&mut cur)?)
            .map_err(|_| bad("content length exceeds usize"))?;
        let content = take(&mut cur, content_len)?;

        // Rebuild the destination from sanitized components — never trust the
        // blob to stay inside out_dir.
        let mut dest = out_dir.to_path_buf();
        for seg in rel.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                return Err(bad("unsafe component in artifact path"));
            }
            dest.push(seg);
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, &content)?;
    }
    Ok(())
}

/// Outcome of [`materialize_latest_green`] — distinct so the CLI can render
/// honest `status` / `build --watch --out` states without guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Materialized {
    /// No green build has been published yet (pointer absent).
    NoGreen,
    /// The pointer is present but the CAS no longer holds the bytes (cache
    /// evicted / `clean`'d). Nothing was written to `out_dir`; the caller
    /// should treat this as "no green — re-trigger a build".
    Evicted(PublishedArtifact),
    /// `out_dir` now contains the published `dist/` tree for this artifact.
    Materialized(PublishedArtifact),
}

/// One-call read path for the CLI (`build --watch --out`, `status`): read the
/// canonical pointer, fetch the blob from the caller-supplied `store`, and
/// expand it into `out_dir`. Keeps the blob layout entirely inside this crate
/// — `cargoless` never reaches into CAS internals (the option-(b) seam cli-ux
/// asked for).
///
/// Both `store` (the cli-ux-configured out-of-tree cache) and `project_root`
/// (where `.cargoless/latest-green` lives) are caller-supplied — nothing is
/// derived here.
///
/// # Errors
/// A corrupt pointer or a malformed blob is [`io::ErrorKind::InvalidData`]; a
/// CAS or filesystem failure is the underlying [`io::Error`]. A *missing*
/// pointer or an *evicted* blob is **not** an error — it is
/// [`Materialized::NoGreen`] / [`Materialized::Evicted`].
pub fn materialize_latest_green<S: ContentStore>(
    store: &S,
    project_root: &Path,
    out_dir: &Path,
) -> io::Result<Materialized> {
    let Some(pa) = read_latest_green(project_root)? else {
        return Ok(Materialized::NoGreen);
    };
    match store.get(&pa.artifact.input_hash)? {
        None => Ok(Materialized::Evicted(pa)),
        Some(blob) => {
            unpack_artifact(&blob, out_dir)?;
            Ok(Materialized::Materialized(pa))
        }
    }
}

fn hash_optional_file(path: &Path, kind: &str) -> io::Result<cargoless_proto::ContentHash> {
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

/// Atomically advance the canonical pointer to `meta` (**AC#4**): render the
/// [`PublishedArtifact`] record and swap it in via [`write_pointer_atomic`].
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
    write_pointer_atomic(&latest_green_path(project_root), &record.render())
}

/// Atomically replace `path` with `contents` — the pointer-file write
/// discipline behind every "never publish red" surface (the latest-green
/// artifact pointer here; the app-serve per-instance pointers reuse it).
///
/// The new contents go to a temp file in the **same directory** (so the
/// `rename` is same-filesystem and therefore atomic), are `fsync`'d, then
/// renamed over the live pointer. The live pointer is never written in
/// place: a crash or a full disk leaves the previous contents byte-intact,
/// never torn. The parent directory is created if absent; a failed rename
/// removes the temp so no stale `.tmp` litters the directory.
pub fn write_pointer_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pointer path has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pointer path has no name"))?
        .to_string_lossy();
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    match fs::rename(&tmp, path) {
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
    use cargoless_cas::LocalDiskStore;
    use std::cell::Cell;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cargoless-core-build-{tag}-{}", std::process::id()));
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
            source_tree: cargoless_cas::content_hash(b"s"),
            cargo_lock: cargoless_cas::content_hash(b"l"),
            rust_toolchain: cargoless_cas::content_hash(b"t"),
            tf_config: cargoless_cas::content_hash(b"c"),
            target: TargetTriple::new("wasm32-unknown-unknown"),
            profile: Profile::Dev,
        }
    }

    // -----------------------------------------------------------------------
    // FIELD FINDING #12 — extract_trunk_failure_reason behaviour
    // -----------------------------------------------------------------------

    #[test]
    fn extract_reason_finds_cargo_error_lines_in_stderr() {
        // Canonical cargo error output: error[code] + final "could not
        // compile" + "Finished" line ABSENT (because cargo failed). The
        // extractor must surface the rustc errors, not the (absent) tail.
        let stderr = "\
            Compiling foo v0.1.0\n\
            error[E0277]: the trait bound `T: Foo` is not satisfied\n\
             --> src/lib.rs:10:5\n\
            error: could not compile `foo` (lib) due to 1 previous error\n";
        let got = extract_trunk_failure_reason("", stderr, Some(101));
        assert!(got.contains("E0277"), "rustc code surfaced: {got}");
        assert!(got.contains("could not compile"));
        assert!(got.contains("exit 101"));
        // The non-error "Compiling foo" line must not leak in.
        assert!(
            !got.contains("Compiling foo"),
            "non-error line leaked: {got}"
        );
    }

    #[test]
    fn extract_reason_finds_trunk_error_lines() {
        // Trunk-style: "ERROR ❌ <thing>" on stderr.
        let stderr = "\
            INFO  starting build\n\
            ERROR ❌ wasm-bindgen-cli not found\n\
            ERROR ❌ aborting due to previous error\n";
        let got = extract_trunk_failure_reason("", stderr, Some(1));
        assert!(got.contains("wasm-bindgen-cli not found"));
        assert!(got.contains("exit 1"));
        // INFO line excluded.
        assert!(!got.contains("INFO"), "INFO line leaked: {got}");
    }

    #[test]
    fn extract_reason_f12_dogfood_smoking_gun_does_not_surface_finished_line() {
        // THE EXACT dogfood reproducer: trunk's wasm-bindgen step fails
        // after cargo-check successfully completes. Cargo emits its
        // "Finished `dev` profile" success line to stderr; trunk emits
        // its actual ERROR slightly earlier, then exits non-zero. The
        // bug was reason=`Finished ...` (cargo's success line treated as
        // failure cause). Post-fix: the ERROR line is surfaced.
        let stderr = "\
            Compiling dogfood-realapp v0.1.0\n\
            ERROR ❌ wasm-bindgen-cli missing — install via `cargo install wasm-bindgen-cli`\n\
            Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.16s\n";
        let got = extract_trunk_failure_reason("", stderr, Some(1));
        // The CRITICAL assertion: cargo's success line must NOT be the
        // surfaced reason (the bug that broke F12).
        let surfaced_as_reason = got.ends_with("in 0.16s")
            || (got.contains("Finished") && !got.contains("wasm-bindgen-cli"));
        assert!(
            !surfaced_as_reason,
            "cargo success line was surfaced as failure reason: {got}"
        );
        // The ACTUAL error must reach the user.
        assert!(
            got.contains("wasm-bindgen-cli missing"),
            "real error must be surfaced: {got}"
        );
    }

    #[test]
    fn extract_reason_scans_both_stdout_and_stderr() {
        // Trunk sometimes prints errors to stdout (older versions) or
        // cargo's own errors split between streams. The extractor must
        // look at both — the F12 bug was stderr-tail-only.
        let stdout = "error: linking failed via lld\n";
        let stderr = "Compiling foo\nFinished `dev` profile in 1.2s\n";
        let got = extract_trunk_failure_reason(stdout, stderr, Some(1));
        assert!(
            got.contains("linking failed"),
            "stdout error surfaced: {got}"
        );
        assert!(!got.ends_with("in 1.2s"));
    }

    #[test]
    fn extract_reason_fallback_when_no_error_lines() {
        // No `error:` / `ERROR` lines anywhere — fall back to the last
        // few stderr lines for context. Must NOT pick "Finished" in
        // isolation: a 3-line tail keeps the user oriented.
        let stderr = "step 1: prepare\nstep 2: compile\nFinished `dev` profile in 0.5s\n";
        let got = extract_trunk_failure_reason("", stderr, Some(2));
        assert!(got.contains("exit 2"));
        assert!(got.contains("no canonical `error:` line found"));
        // Last 3 lines all present in the context window.
        assert!(got.contains("step 1: prepare"));
        assert!(got.contains("step 2: compile"));
        assert!(got.contains("Finished"));
        // The pre-#12 bug was reason BEING `Finished ...` alone; here it's
        // ONE of several lines in a "context" window — clearly labelled
        // as such by the message prefix. Acceptable.
    }

    #[test]
    fn extract_reason_handles_empty_output() {
        // Trunk killed by signal with no diagnostic output. Don't pretend
        // there's a reason — name the situation honestly.
        let got = extract_trunk_failure_reason("", "", None);
        assert!(got.contains("signal"), "signal flagged: {got}");
        assert!(got.contains("no diagnostic output"));
    }

    #[test]
    fn extract_reason_caps_at_five_error_lines_for_readability() {
        // A build with 20 errors must not splat all 20 onto one line —
        // the cap is the last 5 (recency-biased + bounded for readability).
        //
        // Note: the per-line marker uses a UNIQUE delimited token
        // (`#{i}#`) rather than a bare integer, so substring matching
        // doesn't false-positive (e.g. "failure 1" would otherwise be a
        // substring of "failure 16" — exactly the test bug the first
        // self-gate revealed).
        // Use fold rather than map+collect+format to satisfy clippy's
        // `format_collect` lint (`-D warnings`); semantically identical
        // but allocates one String once instead of one per element.
        let stderr: String = (1..=20).fold(String::new(), |mut s, i| {
            use std::fmt::Write as _;
            let _ = writeln!(s, "error: failure #{i}# marker");
            s
        });
        let got = extract_trunk_failure_reason("", &stderr, Some(101));
        // The LAST 5 (numbers 16..=20) MUST appear in the surfaced message.
        for i in 16..=20 {
            let marker = format!("#{i}#");
            assert!(got.contains(&marker), "expected marker {marker} in: {got}");
        }
        // The earlier 15 (1..=15) MUST NOT appear — they got capped out.
        for i in 1..=15 {
            let marker = format!("#{i}#");
            assert!(
                !got.contains(&marker),
                "earlier marker {marker} leaked into: {got}"
            );
        }
    }

    #[test]
    fn is_error_prefix_line_negative_cases() {
        // The success markers cargo prints — none must match the
        // error-prefix predicate.
        assert!(!is_error_prefix_line(
            "Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.16s"
        ));
        assert!(!is_error_prefix_line("Compiling dogfood-realapp v0.1.0"));
        assert!(!is_error_prefix_line("Building target dist/..."));
        assert!(!is_error_prefix_line("warning: unused import"));
        // "errored" / "errors" must NOT match — only the canonical
        // `error` / `error[X]` / `ERROR` prefixes.
        assert!(!is_error_prefix_line(
            "summary: 3 errors during compilation"
        ));
        // Bonus: an empty line / whitespace-only.
        assert!(!is_error_prefix_line(""));
        assert!(!is_error_prefix_line("   "));
    }

    #[test]
    fn is_error_prefix_line_positive_cases() {
        // The patterns we WANT to surface.
        assert!(is_error_prefix_line("error: linking failed"));
        assert!(is_error_prefix_line("error[E0277]: trait bound"));
        assert!(is_error_prefix_line("error[unused_imports]: …"));
        assert!(is_error_prefix_line("ERROR ❌ wasm-bindgen-cli not found"));
        assert!(is_error_prefix_line("ERROR aborting"));
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
        other.source_tree = cargoless_cas::content_hash(b"different-source");
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

    #[test]
    fn write_pointer_atomic_creates_replaces_and_cleans_up() {
        let dir = scratch("ptr-atomic");

        // Creates the parent directory and the pointer in one call.
        let ptr = dir.join("deep").join("nested").join("pointer");
        write_pointer_atomic(&ptr, "one\n").unwrap();
        assert_eq!(fs::read_to_string(&ptr).unwrap(), "one\n");

        // Replaces in full — never appends, never truncates partially.
        write_pointer_atomic(&ptr, "two\n").unwrap();
        assert_eq!(fs::read_to_string(&ptr).unwrap(), "two\n");

        // A failed swap (target is a non-empty directory ⇒ rename fails)
        // leaves no `.tmp` litter behind and the obstacle untouched.
        let blocked = dir.join("blocked");
        fs::create_dir_all(blocked.join("occupied")).unwrap();
        write_pointer_atomic(&blocked, "x").unwrap_err();
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp litter: {leftovers:?}");
        assert!(blocked.join("occupied").is_dir(), "obstacle untouched");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A fake `trunk` that emits a real packed-dist blob (CI has no trunk), so
    /// the full publish → store → materialize path is exercised end-to-end.
    struct DistCompiler {
        blob: Vec<u8>,
    }
    impl Compiler for DistCompiler {
        fn compile(&self, _r: &Path, _i: &BuildIdentity) -> Result<Vec<u8>, String> {
            Ok(self.blob.clone())
        }
    }

    fn make_dist(tag: &str) -> (PathBuf, Vec<u8>) {
        let d = scratch(tag).join("dist");
        fs::create_dir_all(d.join("assets")).unwrap();
        fs::write(d.join("index.html"), b"<body>hi</body>").unwrap();
        fs::write(d.join("app_bg.wasm"), b"\0asm\x01\0\0\0").unwrap();
        fs::write(d.join("assets/app.css"), b".x{}").unwrap();
        let blob = pack_dir(&d).unwrap();
        (d, blob)
    }

    #[test]
    fn unpack_artifact_round_trips_pack_dir() {
        let (src, blob) = make_dist("rt-src");
        let out = scratch("rt-out");
        unpack_artifact(&blob, &out).unwrap();
        assert_eq!(
            fs::read(out.join("index.html")).unwrap(),
            b"<body>hi</body>"
        );
        assert_eq!(
            fs::read(out.join("app_bg.wasm")).unwrap(),
            b"\0asm\x01\0\0\0"
        );
        assert_eq!(fs::read(out.join("assets/app.css")).unwrap(), b".x{}");
        let _ = fs::remove_dir_all(src.parent().unwrap());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn unpack_artifact_rejects_corruption_and_traversal() {
        let out = scratch("bad-out");
        assert_eq!(
            unpack_artifact(b"not-a-blob", &out).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        // Header OK but truncated length field.
        let mut t = DIST_BLOB_HEADER.to_vec();
        t.extend_from_slice(&[0, 0, 0]);
        assert!(unpack_artifact(&t, &out).is_err());
        // Header OK, a path that tries to escape out_dir.
        let mut e = DIST_BLOB_HEADER.to_vec();
        let rel = b"../escape.txt";
        e.extend_from_slice(&(rel.len() as u64).to_be_bytes());
        e.extend_from_slice(rel);
        e.extend_from_slice(&(3u64).to_be_bytes());
        e.extend_from_slice(b"pwn");
        assert_eq!(
            unpack_artifact(&e, &out).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(!out.parent().unwrap().join("escape.txt").exists());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn materialize_latest_green_no_green_evicted_and_done() {
        let project = scratch("mat-proj");
        fs::create_dir_all(&project).unwrap();
        let cache = scratch("mat-cache");
        let out = scratch("mat-out");

        // No pointer yet ⇒ NoGreen.
        assert_eq!(
            materialize_latest_green(&LocalDiskStore::new(&cache), &project, &out).unwrap(),
            Materialized::NoGreen
        );

        // Publish a real dist blob through the orchestrator.
        let (distdir, blob) = make_dist("mat-dist");
        let orch =
            BuildOrchestrator::new(LocalDiskStore::new(&cache), DistCompiler { blob }, &project);
        assert_eq!(
            orch.run(&BuildTrigger { identity: ident() }).outcome,
            BuildOutcome::Compiled
        );

        // Pointer + blob present ⇒ Materialized, files faithfully expanded.
        match materialize_latest_green(&LocalDiskStore::new(&cache), &project, &out).unwrap() {
            Materialized::Materialized(pa) => {
                assert_eq!(pa.artifact.input_hash, input_hash(&ident()));
            }
            other => panic!("expected Materialized, got {other:?}"),
        }
        assert_eq!(
            fs::read(out.join("index.html")).unwrap(),
            b"<body>hi</body>"
        );
        assert_eq!(fs::read(out.join("assets/app.css")).unwrap(), b".x{}");

        // Wipe the cache (simulate `clean`) — pointer dangles ⇒ Evicted.
        fs::remove_dir_all(&cache).unwrap();
        match materialize_latest_green(&LocalDiskStore::new(&cache), &project, &out).unwrap() {
            Materialized::Evicted(pa) => {
                assert_eq!(pa.artifact.input_hash, input_hash(&ident()))
            }
            other => panic!("expected Evicted, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&project);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(distdir.parent().unwrap());
    }
}
