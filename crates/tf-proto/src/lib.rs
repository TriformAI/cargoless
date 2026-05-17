//! Shared contracts between the daemon, the build pipeline, and (later)
//! remote backends. This is the seam the two-engineer split is built around:
//! cross-crate communication goes through these types, never direct calls.
//!
//! v0 keeps these dependency-free and serde-free; the owning agents add
//! serialization when they wire the boundary. Decision of record: Plane D8.

/// Content hash of a build's full input set. Opaque newtype so callers cannot
/// accidentally pass a raw string where a verified hash is expected.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InputHash(String);

impl InputHash {
    pub fn new(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Per-file compile state. v0 granularity is file-level (decision D4); the
/// symbol-level upgrade is explicitly v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Green,
    Red,
}

/// Emitted by the daemon's green/red model. The build pipeline subscribes to
/// these instead of calling the model directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateEvent {
    /// A file's verdict changed.
    FileVerdict { path: String, state: FileState },
    /// The whole tree just became green — a build should be triggered.
    BecameGreen,
    /// The tree went red — the dev server must keep serving last-green.
    BecameRed,
}

/// Metadata stored alongside every cached artifact in the CAS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactMeta {
    pub input_hash: InputHash,
    pub toolchain_id: String,
    pub target_triple: String,
    pub profile: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_hash_roundtrips() {
        let h = InputHash::new("deadbeef");
        assert_eq!(h.as_str(), "deadbeef");
        assert_eq!(h, InputHash::new("deadbeef".to_string()));
    }

    #[test]
    fn state_events_are_comparable() {
        assert_ne!(StateEvent::BecameGreen, StateEvent::BecameRed);
        let v = StateEvent::FileVerdict {
            path: "src/lib.rs".into(),
            state: FileState::Red,
        };
        assert_ne!(v, StateEvent::BecameGreen);
    }
}
