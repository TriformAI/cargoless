//! `tf-proto` — the cross-crate contract for cargoless.
//!
//! This crate is the seam the daemon (`watcher`/`analyzer`/`model`), the build
//! pipeline + CAS (`build`/`tf-cas`), the dev server (`server`), the CLI, and
//! future remote backends communicate through. Cross-boundary data flows as
//! these types; nobody reaches across a module boundary with a direct call.
//! Authoring this jointly and freezing it early is the whole point of Plane
//! **CWDL-19 (D8)** — the two-engineer split silently diverges otherwise.
//!
//! ## Why dependency-free and serde-free in v0 (decision of record)
//!
//! v0 is single-machine, single-process: every consumer of these types links
//! `tf-proto` directly and passes them in-memory (channels / function args).
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
/// (`tf-cas`). `tf-proto` only carries the resulting identity so producers and
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
/// owner; `tf-proto` only guarantees that equal `BuildIdentity` ⇒ equal
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
}
