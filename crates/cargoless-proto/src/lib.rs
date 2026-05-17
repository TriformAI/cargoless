//! `cargoless-proto` — the cross-crate contract for cargoless.
//!
//! This crate is the seam the daemon (`watcher`/`analyzer`/`model`), the build
//! pipeline + CAS (`build`/`cargoless-cas`), the dev server (`server`), the CLI, and
//! future remote backends communicate through. Cross-boundary data flows as
//! these types; nobody reaches across a module boundary with a direct call.
//! Authoring this jointly and freezing it early is the whole point of Plane
//! **CWDL-19 (D8)** — the two-engineer split silently diverges otherwise.
//!
//! ## Why dependency-free and serde-free in v0 (decision of record)
//!
//! v0 is single-machine, single-process: every consumer of these types links
//! `cargoless-proto` directly and passes them in-memory (channels / function args).
//! Nothing crosses a process or network boundary, so nothing needs to be
//! serialized. Adding `serde` now would (a) put a non-trivial dependency in the
//! crate every other crate depends on, slowing the cold build that AC#1/#2 are
//! measured against, and (b) freeze a wire format we have no v0 consumer for.
//!
//! When a boundary genuinely needs serialization (the dev-server↔browser reload
//! channel speaks WebSocket — decision **D3** — and remote CAS is a v1 want),
//! the owning crate adds `serde` here behind an off-by-default `serde` feature
//! and derives it on exactly the types that cross that boundary. The contract
//! shapes below are designed so that bolt-on is additive, never a reshape.
//!
//! ## The data-flow at a glance
//!
//! ```text
//!   watcher → analyzer → model ──StateEvent──▶ everyone (verdict stream)
//!                          │
//!                          └─on BecameGreen──▶ BuildTrigger ─▶ build/CAS
//!                                                                  │
//!                          server ◀──BuildResult── build/CAS ◀─────┘
//! ```
//!
//! The model is the single source of truth for "what works"; the build/CAS
//! layer is the single source of truth for "is this exact input already
//! built". Everything else subscribes.

#![forbid(unsafe_code)]

use core::fmt;

// ---------------------------------------------------------------------------
// Content identity
// ---------------------------------------------------------------------------

/// An opaque content hash, rendered as a hex string.
///
/// The *algorithm* (blake3, sha256, …) and the *hashing implementation* are
/// deliberately **not** part of this contract — they belong to the CAS owner
/// (`cargoless-cas`). `cargoless-proto` only carries the resulting identity so producers and
/// consumers agree on what equality means without agreeing on how it is
/// computed. Comparison is byte-exact on the hex string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash(String);

impl ContentHash {
    pub fn new(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The target triple a build is produced for (e.g. `wasm32-unknown-unknown`).
///
/// A newtype rather than a bare `String` so it cannot be transposed with the
/// profile or a path at a call site.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TargetTriple(String);

impl TargetTriple {
    pub fn new(triple: impl Into<String>) -> Self {
        Self(triple.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TargetTriple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Cargo build profile. v0 inner-loop builds are always [`Profile::Dev`]
/// (workspace `[profile.dev]` pins `opt-level = 0`, no `wasm-opt`, per the
/// AC#3 latency constraint); [`Profile::Release`] exists in the contract so
/// the identity is honest and a release build can never alias a dev artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Profile {
    Dev,
    Release,
}

impl Profile {
    /// Cargo's name for this profile (`dev` / `release`).
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Dev => "dev",
            Profile::Release => "release",
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The complete set of inputs whose identity determines a build artifact.
///
/// This is the dedupe key behind **AC#5** (identical source state ⇒ cache hit,
/// build skipped) and the provenance record behind **AC#4** (never serve red:
/// the server only ever swaps to an artifact whose `BuildIdentity` it can name).
/// Each component is carried as its own type so the contract is explicit about
/// *what* makes a build distinct; folding these into the single [`InputHash`]
/// CAS key is the CAS owner's job and is intentionally not specified here.
///
/// Two builds with an `Eq` `BuildIdentity` MUST be substitutable. If a real
/// input is not represented here, identical-key collisions become wrong-artifact
/// bugs — so additions to this struct are a deliberate contract change, not an
/// implementation detail.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BuildIdentity {
    /// Hash over every tracked source file in the crate/workspace tree.
    pub source_tree: ContentHash,
    /// Hash of `Cargo.lock` — pins the exact resolved dependency graph.
    pub cargo_lock: ContentHash,
    /// Hash of the resolved Rust toolchain (`rust-toolchain.toml` content /
    /// pinned channel + version). A toolchain bump must invalidate the cache.
    pub rust_toolchain: ContentHash,
    /// Hash of the cargoless config file (`tf.toml`, decision **D6**). Config
    /// changes the build, so it is part of the identity.
    pub tf_config: ContentHash,
    /// The target triple (typically `wasm32-unknown-unknown`).
    pub target: TargetTriple,
    /// The cargo profile (always [`Profile::Dev`] for the v0 inner loop).
    pub profile: Profile,
}

/// The CAS key: the single digest derived from a [`BuildIdentity`].
///
/// Opaque newtype so a caller cannot pass a raw string where a verified key is
/// expected. The reduction `BuildIdentity → InputHash` is performed by the CAS
/// owner; `cargoless-proto` only guarantees that equal `BuildIdentity` ⇒ equal
/// `InputHash` is the invariant every consumer may rely on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InputHash(String);

impl InputHash {
    pub fn new(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InputHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Green/red state model
// ---------------------------------------------------------------------------

/// Per-file compile verdict.
///
/// v0 granularity is **file-level** (decision **D4**). Symbol-level tracking is
/// what rust-analyzer does internally and is an explicit v1 want — out of v0 by
/// construction. The verdict itself *is* the signal here; a `Red` deliberately
/// carries no diagnostic payload in v0 so this type stays `Copy` and
/// dependency-free. Human-readable detail is the daemon/CLI's job to surface
/// from its own analyzer state, not something every contract consumer must
/// thread through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileState {
    Green,
    Red,
}

/// Aggregate verdict for the whole watched tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TreeState {
    /// Every tracked file is green — safe to build and serve.
    Green,
    /// At least one tracked file is red — keep serving last-green (AC#4).
    Red,
}

/// The event stream emitted by the daemon's green/red model. Every other
/// subsystem *subscribes* to this; nothing calls the model directly.
///
/// Two flavours, deliberately distinct:
/// * [`FileVerdict`](StateEvent::FileVerdict) — level: "this file is now X".
///   Idempotent; fine to re-emit the same state.
/// * [`BecameGreen`](StateEvent::BecameGreen) /
///   [`BecameRed`](StateEvent::BecameRed) — *edges*: the tree just crossed the
///   green⇄red boundary. These are the latency-to-signal events the product is
///   built around ("tells you the moment it doesn't"): `BecameRed` is the
///   instant the server must freeze on last-green; `BecameGreen` is the only
///   thing that may trigger a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateEvent {
    /// A single file's verdict (re)settled. Level-triggered.
    FileVerdict { path: String, state: FileState },
    /// The tree transitioned red → green. Carries the identity of the now-green
    /// input set so the build can be triggered without a second round-trip to
    /// the model. Edge-triggered: emitted once per crossing.
    BecameGreen { identity: BuildIdentity },
    /// The tree transitioned green → red. The dev server must immediately stop
    /// advancing and keep serving the last green artifact. Edge-triggered.
    BecameRed,
}

// ---------------------------------------------------------------------------
// Build trigger / result
// ---------------------------------------------------------------------------

/// Sent by the daemon to the build/CAS layer to request that a green input set
/// be made servable. The only legitimate cause of a `BuildTrigger` is a
/// [`StateEvent::BecameGreen`] — red inputs are never built (AC#4).
///
/// It carries the full [`BuildIdentity`] (not just the derived [`InputHash`])
/// so the CAS can both compute its key *and* persist honest provenance for the
/// resulting [`ArtifactMeta`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildTrigger {
    pub identity: BuildIdentity,
}

/// What the build/CAS layer did with a [`BuildTrigger`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildOutcome {
    /// The input set was already in the CAS — no compile ran. This variant
    /// existing and being observable is what proves **AC#5**.
    Deduplicated,
    /// A fresh compile produced the artifact.
    Compiled,
    /// The build failed despite a green verdict (e.g. a toolchain/link error
    /// the analyzer cannot see). The server keeps serving last-green; the
    /// `reason` is a human-readable one-liner for the CLI/log, not a structured
    /// diagnostic (kept dependency-free and v0-simple, like [`FileState`]).
    Failed { reason: String },
}

impl BuildOutcome {
    /// Did this outcome yield a servable artifact?
    pub fn is_servable(&self) -> bool {
        matches!(self, BuildOutcome::Deduplicated | BuildOutcome::Compiled)
    }
}

/// Metadata persisted alongside every cached artifact in the CAS, and the
/// payload the dev server consumes to decide whether to hot-reload the browser.
///
/// Holds the full [`BuildIdentity`] (provenance: answers "what exactly is this")
/// plus the derived [`InputHash`] (the CAS key it is stored under).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactMeta {
    /// The CAS key this artifact is stored under.
    pub input_hash: InputHash,
    /// The full input identity that produced it (provenance).
    pub identity: BuildIdentity,
}

/// Returned by the build/CAS layer for each [`BuildTrigger`]; consumed by the
/// daemon (logging/state) and the dev server (reload decision — decisions
/// **D3** WebSocket signaling and **D5** full-reload-not-hot-swap govern *how*
/// the browser is told, not this contract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildResult {
    pub outcome: BuildOutcome,
    /// Present iff [`BuildOutcome::is_servable`] — the artifact the server may
    /// now advance to. `None` on `Failed`, where the server holds last-green.
    pub artifact: Option<ArtifactMeta>,
}

// ---------------------------------------------------------------------------
// Diagnostics — additive CLI-facing surface (FIELD FINDING #2 fix)
//
// The boolean `TreeState` answers "should the publisher advance?" (AC#4 —
// load-bearing for v0 and STAYS BYTE-FROZEN); but a user staring at a red
// tree needs to know *which* file, *which* line, *what* the rustc said. The
// existing frozen seams (`StateEvent` / `TreeState` / `BuildTrigger` /
// `BuildResult` / `ArtifactMeta`) deliberately carry no diagnostic payload —
// adding one in place would break every wired consumer. So this is an
// ADJACENT, additive surface: a parallel rich verdict the CLI may opt into
// without touching the existing API anyone else binds to.
//
// Same discipline as the latest-green publisher: serde-free, no new deps,
// the existing types are unchanged. Pairing the boolean tree with the
// diagnostic list is what restores the README promise that "the codebase
// always knows what works, *and tells you the moment it doesn't*".
// ---------------------------------------------------------------------------

/// Diagnostic severity, derived from the LSP `DiagnosticSeverity` integers
/// (1=Error, 2=Warning, 3=Information, 4=Hint). A typed enum so the CLI
/// renders `error`/`warning`/`info`/`hint` headers without re-deriving from
/// raw numbers, and so a future consumer can pattern-match exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    /// Lowercase tag used by `rustc`-style display (`error[E0277]: …`).
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
            Severity::Hint => "hint",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One actionable diagnostic, surfaced by the LSP layer and aggregated by
/// the daemon model. Carries the absolute file path, 1-based (file:line:col)
/// position, severity, optional compiler/lint code, the human message, and
/// the diagnostic `source` string verbatim (`"rustc"` for cargo-check
/// authoritative, `"rust-analyzer"` for native advisory, anything else for
/// future tiers) so the CLI can both show provenance and the model can keep
/// classifying authoritative-vs-advisory off the same value.
///
/// Additive alongside the frozen `StateEvent`/`TreeState`/`check_once`
/// surfaces — those keep their byte-frozen shapes; this is the parallel
/// detail channel the CLI subscribes to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Diagnostic {
    /// Absolute file path as reported by rust-analyzer (`file://` URI
    /// stripped to its path). `PathBuf` (not `String`) so callers can render
    /// it relative to the project root with `Path::strip_prefix` without
    /// re-parsing.
    pub file_path: std::path::PathBuf,
    /// 1-based line number. The LSP wire is 0-based; the extraction
    /// converts at the boundary so every consumer sees the same convention
    /// (matches `cargo`/`rustc` display).
    pub line: u32,
    /// 1-based column number, same convention as `line`.
    pub col: u32,
    /// LSP severity, mapped to [`Severity`].
    pub severity: Severity,
    /// Diagnostic code, e.g. `"E0277"` (rustc) or `"unused_imports"` (lint).
    /// `None` when the LSP omitted it (some advisory native diagnostics).
    pub code: Option<String>,
    /// Human-readable message text exactly as reported by the LSP. May be
    /// multi-line — the CLI renderer is responsible for any indentation.
    pub message: String,
    /// `source` field verbatim — `"rustc"` for cargo-check authoritative
    /// diagnostics, `"rust-analyzer"` for native advisory. `None` if the
    /// LSP did not tag the diagnostic with a source.
    pub source: Option<String>,
}

/// The rich one-shot check verdict: the existing boolean [`TreeState`] paired
/// with the full diagnostic list a user needs to fix a red tree. Returned by
/// the adjacent `cargoless_core::model::check_once_with_diagnostics`; existing
/// callers of [`TreeState`]-returning APIs (`check_once`, frozen for cli-ux
/// and the bench harness) are byte-unaffected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// The same authoritative tree verdict `check_once` would return.
    pub tree: TreeState,
    /// Every diagnostic the model knew about at the moment the verdict was
    /// finalised — every severity, both rustc-authoritative and RA-advisory.
    /// The CLI renderer is free to filter (e.g. errors-only) by `severity`
    /// and/or `source`.
    pub diagnostics: Vec<Diagnostic>,
}

// ---------------------------------------------------------------------------
// Latest-green publisher seam (the ONLY additive v0 surface — D-A1 / AC#4)
// ---------------------------------------------------------------------------

/// Wall-clock seconds since the Unix epoch (UTC). A newtype so a timestamp
/// cannot be transposed with any other `u64` at a call site. `cargoless-proto` is
/// deliberately dependency-free, so there is no `chrono`/`time` here: the
/// producer (`cargoless-core::build`) fills this from `std::time::SystemTime`; this
/// crate only carries the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnixSeconds(pub u64);

impl fmt::Display for UnixSeconds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The latest-green publisher record (decision D-A1; AC#4 "never publish
/// red"). The build/CAS layer writes this beside the canonical pointer file
/// `.cargoless/latest-green` on every servable green build; the CLI `status`
/// reads it back. This is the **only additive v0 contract surface** — it does
/// not touch the four frozen seams (`StateEvent` / `BuildTrigger` /
/// `BuildResult` / `ArtifactMeta`) and adds no dependency: the on-disk form is
/// a hand-rolled, versioned text codec ([`render`](Self::render) /
/// [`parse`](Self::parse)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifact {
    /// What was published: the CAS key + full input provenance. `profile` and
    /// `target` live inside `artifact.identity` — not duplicated here.
    pub artifact: ArtifactMeta,
    /// When the pointer was advanced to this artifact.
    pub published_at: UnixSeconds,
}

/// Returned by [`PublishedArtifact::parse`] when the pointer file is not the
/// expected `cargoless-latest-green/v1` shape. Dependency-free (no
/// `thiserror`); a corrupt pointer is treated as "no green yet", never
/// half-decoded into a wrong artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerFormatError(pub String);

impl fmt::Display for PointerFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid latest-green pointer: {}", self.0)
    }
}

impl std::error::Error for PointerFormatError {}

/// The frozen on-disk schema version. Bumping it is a deliberate, repo-visible
/// contract change — old pointer files then fail [`parse`](PublishedArtifact::parse)
/// loudly rather than being silently misread.
const POINTER_SCHEME: &str = "cargoless-latest-green/v1";

impl PublishedArtifact {
    /// Serialize to the canonical pointer-file text: a scheme-version header
    /// line, then `key=value` lines. Deliberately flat and human-inspectable
    /// (the nested type is the in-memory contract; the file is its faithful,
    /// stable projection). Every value (hex hash, target triple,
    /// `dev`/`release`, decimal `u64`) is free of `=`/newline, so the framing
    /// is unambiguous.
    pub fn render(&self) -> String {
        use core::fmt::Write as _;
        let id = &self.artifact.identity;
        let mut s = String::new();
        s.push_str(POINTER_SCHEME);
        s.push('\n');
        // Infallible: writing to a String never errors.
        let _ = writeln!(s, "input_hash={}", self.artifact.input_hash.as_str());
        let _ = writeln!(s, "source_tree={}", id.source_tree.as_str());
        let _ = writeln!(s, "cargo_lock={}", id.cargo_lock.as_str());
        let _ = writeln!(s, "rust_toolchain={}", id.rust_toolchain.as_str());
        let _ = writeln!(s, "tf_config={}", id.tf_config.as_str());
        let _ = writeln!(s, "target={}", id.target.as_str());
        let _ = writeln!(s, "profile={}", id.profile.as_str());
        let _ = writeln!(s, "published_at={}", self.published_at.0);
        s
    }

    /// Inverse of [`render`](Self::render). Strict: wrong header, a missing
    /// key, a non-numeric timestamp, or an unknown profile all ⇒ `Err`.
    pub fn parse(text: &str) -> Result<Self, PointerFormatError> {
        let err = |m: &str| PointerFormatError(m.to_string());
        let mut lines = text.lines();
        match lines.next() {
            Some(h) if h == POINTER_SCHEME => {}
            _ => return Err(err("missing or unknown scheme header")),
        }
        let mut map: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| err("line is not key=value"))?;
            map.insert(k.to_string(), v.to_string());
        }
        let get = |k: &str| -> Result<String, PointerFormatError> {
            map.get(k)
                .cloned()
                .ok_or_else(|| err(&format!("missing key `{k}`")))
        };
        let profile = match get("profile")?.as_str() {
            "dev" => Profile::Dev,
            "release" => Profile::Release,
            other => return Err(err(&format!("unknown profile `{other}`"))),
        };
        let published_at = get("published_at")?
            .parse::<u64>()
            .map_err(|_| err("published_at is not a u64"))?;
        Ok(Self {
            artifact: ArtifactMeta {
                input_hash: InputHash::new(get("input_hash")?),
                identity: BuildIdentity {
                    source_tree: ContentHash::new(get("source_tree")?),
                    cargo_lock: ContentHash::new(get("cargo_lock")?),
                    rust_toolchain: ContentHash::new(get("rust_toolchain")?),
                    tf_config: ContentHash::new(get("tf_config")?),
                    target: TargetTriple::new(get("target")?),
                    profile,
                },
            },
            published_at: UnixSeconds(published_at),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_identity() -> BuildIdentity {
        BuildIdentity {
            source_tree: ContentHash::new("aaaa"),
            cargo_lock: ContentHash::new("bbbb"),
            rust_toolchain: ContentHash::new("cccc"),
            tf_config: ContentHash::new("dddd"),
            target: TargetTriple::new("wasm32-unknown-unknown"),
            profile: Profile::Dev,
        }
    }

    #[test]
    fn input_hash_roundtrips_and_displays() {
        let h = InputHash::new("deadbeef");
        assert_eq!(h.as_str(), "deadbeef");
        assert_eq!(h, InputHash::new("deadbeef".to_string()));
        assert_eq!(h.to_string(), "deadbeef");
    }

    #[test]
    fn identity_equality_is_componentwise() {
        let a = sample_identity();
        let b = sample_identity();
        assert_eq!(
            a, b,
            "equal components ⇒ equal identity (the AC#5 invariant)"
        );

        let mut c = sample_identity();
        c.profile = Profile::Release;
        assert_ne!(a, c, "a release build must never alias a dev artifact");

        let mut d = sample_identity();
        d.source_tree = ContentHash::new("ffff");
        assert_ne!(a, d, "a source change must invalidate the cache key");
    }

    #[test]
    fn became_green_carries_identity_for_one_shot_build_trigger() {
        let ev = StateEvent::BecameGreen {
            identity: sample_identity(),
        };
        match ev {
            StateEvent::BecameGreen { identity } => {
                let trigger = BuildTrigger { identity };
                assert_eq!(trigger.identity, sample_identity());
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn state_events_are_distinct() {
        assert_ne!(
            StateEvent::BecameRed,
            StateEvent::BecameGreen {
                identity: sample_identity()
            }
        );
        let v = StateEvent::FileVerdict {
            path: "src/lib.rs".into(),
            state: FileState::Red,
        };
        assert_ne!(v, StateEvent::BecameRed);
    }

    #[test]
    fn outcome_servability_drives_artifact_presence() {
        assert!(BuildOutcome::Deduplicated.is_servable());
        assert!(BuildOutcome::Compiled.is_servable());
        assert!(
            !BuildOutcome::Failed {
                reason: "linker exploded".into()
            }
            .is_servable()
        );

        let ok = BuildResult {
            outcome: BuildOutcome::Compiled,
            artifact: Some(ArtifactMeta {
                input_hash: InputHash::new("0123"),
                identity: sample_identity(),
            }),
        };
        assert!(ok.outcome.is_servable() && ok.artifact.is_some());

        let bad = BuildResult {
            outcome: BuildOutcome::Failed {
                reason: "rustc ICE".into(),
            },
            artifact: None,
        };
        assert!(!bad.outcome.is_servable() && bad.artifact.is_none());
    }

    #[test]
    fn profile_and_tree_state_render() {
        assert_eq!(Profile::Dev.as_str(), "dev");
        assert_eq!(Profile::Release.to_string(), "release");
        assert_ne!(TreeState::Green, TreeState::Red);
    }

    fn sample_published() -> PublishedArtifact {
        PublishedArtifact {
            artifact: ArtifactMeta {
                input_hash: InputHash::new("0123abcd"),
                identity: sample_identity(),
            },
            published_at: UnixSeconds(1_747_000_000),
        }
    }

    #[test]
    fn published_artifact_round_trips_through_the_pointer_codec() {
        let p = sample_published();
        let text = p.render();
        // Human-inspectable, versioned, flat.
        assert!(text.starts_with("cargoless-latest-green/v1\n"));
        assert!(text.contains("input_hash=0123abcd\n"));
        assert!(text.contains("profile=dev\n"));
        assert!(text.contains("published_at=1747000000\n"));
        // Exact inverse: parse(render(x)) == x (the producer/reader contract).
        assert_eq!(PublishedArtifact::parse(&text).unwrap(), p);
    }

    #[test]
    fn pointer_parse_is_strict() {
        // Wrong/absent header ⇒ Err (never a half-decoded artifact).
        assert!(PublishedArtifact::parse("").is_err());
        assert!(PublishedArtifact::parse("not-a-pointer\ninput_hash=x\n").is_err());
        // Missing a required key ⇒ Err.
        assert!(PublishedArtifact::parse("cargoless-latest-green/v1\ninput_hash=x\n").is_err());
        // Unknown profile / non-numeric timestamp ⇒ Err.
        let mut bad = sample_published()
            .render()
            .replace("profile=dev", "profile=fast");
        assert!(PublishedArtifact::parse(&bad).is_err());
        bad = sample_published()
            .render()
            .replace("published_at=1747000000", "published_at=soon");
        assert!(PublishedArtifact::parse(&bad).is_err());
    }

    #[test]
    fn unix_seconds_is_a_distinct_newtype() {
        assert_eq!(UnixSeconds(42).to_string(), "42");
        assert!(UnixSeconds(1) < UnixSeconds(2));
        assert_eq!(UnixSeconds(7), UnixSeconds(7));
    }

    // -----------------------------------------------------------------------
    // Diagnostics — additive CLI-facing surface (FIELD FINDING #2 fix)
    // -----------------------------------------------------------------------

    #[test]
    fn severity_renders_lowercase_and_is_exhaustive() {
        assert_eq!(Severity::Error.as_str(), "error");
        assert_eq!(Severity::Warning.as_str(), "warning");
        assert_eq!(Severity::Info.as_str(), "info");
        assert_eq!(Severity::Hint.as_str(), "hint");
        assert_eq!(Severity::Error.to_string(), "error");
        // Distinct values — the four variants do not collide.
        let s: std::collections::BTreeSet<_> = [
            Severity::Error,
            Severity::Warning,
            Severity::Info,
            Severity::Hint,
        ]
        .into_iter()
        .collect();
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn diagnostic_carries_position_code_and_source() {
        let d = Diagnostic {
            file_path: std::path::PathBuf::from("/repo/src/lib.rs"),
            line: 42,
            col: 5,
            severity: Severity::Error,
            code: Some("E0277".to_string()),
            message: "the trait bound `T: Foo` is not satisfied".to_string(),
            source: Some("rustc".to_string()),
        };
        // Field accesses (the CLI's binding surface) compile and round-trip.
        assert_eq!(d.line, 42);
        assert_eq!(d.col, 5);
        assert_eq!(d.code.as_deref(), Some("E0277"));
        assert_eq!(d.source.as_deref(), Some("rustc"));
        assert_eq!(d.severity, Severity::Error);
        assert!(d.message.contains("trait bound"));
        // Eq is value-equality (the AC: two identical diagnostics dedupe).
        let d2 = d.clone();
        assert_eq!(d, d2);
    }

    #[test]
    fn check_result_pairs_tree_with_diagnostics() {
        // Empty diagnostics + green = the canonical "happy path".
        let green = CheckResult {
            tree: TreeState::Green,
            diagnostics: Vec::new(),
        };
        assert_eq!(green.tree, TreeState::Green);
        assert!(green.diagnostics.is_empty());

        // Red verdict ⇒ at least one diagnostic the CLI will print. The
        // FIELD FINDING #2 invariant: a red tree carries its evidence.
        let red = CheckResult {
            tree: TreeState::Red,
            diagnostics: vec![Diagnostic {
                file_path: std::path::PathBuf::from("/r/src/lib.rs"),
                line: 1,
                col: 1,
                severity: Severity::Error,
                code: Some("E0599".to_string()),
                message: "no method named `frob` found".to_string(),
                source: Some("rustc".to_string()),
            }],
        };
        assert_eq!(red.tree, TreeState::Red);
        assert_eq!(red.diagnostics.len(), 1);
        assert_eq!(red.diagnostics[0].code.as_deref(), Some("E0599"));
    }

    #[test]
    fn diagnostic_path_is_relativisable_against_a_root() {
        // The CLI takes the absolute file_path and renders relative to the
        // project root with std::path::Path::strip_prefix — verify the shape
        // supports that (PathBuf, not String).
        let d = Diagnostic {
            file_path: std::path::PathBuf::from("/repo/src/lib.rs"),
            line: 1,
            col: 1,
            severity: Severity::Warning,
            code: None,
            message: "x".to_string(),
            source: None,
        };
        let rel = d
            .file_path
            .strip_prefix("/repo")
            .expect("strips the root cleanly");
        assert_eq!(rel, std::path::Path::new("src/lib.rs"));
    }
}
