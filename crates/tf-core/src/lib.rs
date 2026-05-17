//! The daemon. Owners fill these modules in against the Plane CWDL epics:
//!
//! - watcher  — `notify` filesystem watcher (Epic 2)
//! - analyzer — rust-analyzer subprocess + LSP client (Epic 2)
//! - model    — file-level green/red state (Epic 2, decision D4)
//! - build    — trunk-build orchestration + input hashing (Epic 3)
//! - server   — HTTP + WebSocket dev server, never-serve-red (Epic 4)
//!
//! Skeleton only exposes a build identifier so the workspace links and CI is
//! green-on-empty (decision D10).

pub mod analyzer;
pub mod build;
pub mod lsp;
pub mod model;
pub mod watcher;

pub use model::LifecycleEvent;
pub use tf_cas::{ContentStore, LocalDiskStore};
pub use tf_proto::{
    ArtifactMeta, BuildIdentity, BuildOutcome, BuildResult, BuildTrigger, CheckResult, ContentHash,
    Diagnostic, FileState, InputHash, Profile, Severity, StateEvent, TargetTriple, TreeState,
};

/// Name-neutral build identifier. The shipping product name is decision D1;
/// nothing in the codebase hardcodes a public name until then.
pub const BUILD_ID: &str = concat!("tf-trunk ", env!("CARGO_PKG_VERSION"));

/// Returns the build identifier string.
pub fn build_id() -> &'static str {
    BUILD_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_id_is_name_neutral() {
        let id = build_id();
        assert!(id.starts_with("tf-trunk "));
        // Guard: the Terraform-colliding bare name must never be the identifier.
        assert_ne!(id.trim(), "tf");
    }

    #[test]
    fn proto_and_cas_are_reexported() {
        let _h = InputHash::new("x");
        let _s = LocalDiskStore::new(std::env::temp_dir());
        let _e = StateEvent::BecameRed;
        let _f = FileState::Green;
        let identity = BuildIdentity {
            source_tree: ContentHash::new("a"),
            cargo_lock: ContentHash::new("b"),
            rust_toolchain: ContentHash::new("c"),
            tf_config: ContentHash::new("d"),
            target: TargetTriple::new("wasm32-unknown-unknown"),
            profile: Profile::Dev,
        };
        let _m = ArtifactMeta {
            input_hash: InputHash::new("x"),
            identity,
        };
    }
}
