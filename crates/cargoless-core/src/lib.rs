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

pub mod activity;
pub mod analyzer;
pub mod build;
pub mod config;
pub mod diagnostics_store;
pub mod idle;
pub mod lsp;
pub mod model;
pub mod procmacro;
pub mod repo;
pub mod structural;
pub mod transport;
pub mod watcher;

pub use cargoless_cas::{ContentStore, LocalDiskStore};
pub use cargoless_proto::{
    ArtifactMeta, BuildIdentity, BuildOutcome, BuildResult, BuildTrigger, CheckResult, ContentHash,
    Diagnostic, FileState, InputHash, Profile, Severity, StateEvent, TargetTriple, TreeState,
};
pub use config::{FleetConfig, FleetConfigError, FleetOverrides, Provenance, Source};
pub use model::LifecycleEvent;

/// The single canonical identity string — `<product> <version>` — used
/// by `--version`, `help`, AND every command banner.
///
/// ## §gap-3 / #89: this is the ONLY product-name site in the binary
///
/// Before #89 the binary rendered TWO different product names depending
/// on the command: `--version` / `help` showed `tf-trunk <ver>` (this
/// constant), while `watch` built its own `cargoless <ver>` banner
/// straight off `CARGO_PKG_VERSION` in `cargoless`. Same binary, two
/// names — dogfood-lead's §gap-3 finding. Every banner now reads THIS
/// constant (`cargoless_core::BUILD_ID`), so the binary speaks one name.
///
/// **Decision D1 (CWDL-12) rename = change the `"cargoless"` literal on
/// the next line, and nothing else.** That single-site property is the
/// entire point of #89: it turns docs-launch-lead's D1 rename (#87)
/// from "hunt every banner across N files, hope you got them all" into
/// "change one literal, the type system + the
/// `build_id_is_name_neutral` test enforce the rest".
///
/// The placeholder is `"cargoless"` (the working repo/binary name per
/// CLAUDE.md) — explicitly NOT `"tf"` / `"tf-trunk"` (Terraform
/// collision, rejected per CLAUDE.md; the old `tf-trunk` value leaked
/// that rejected token into `--version` output).
pub const BUILD_ID: &str = concat!("cargoless ", env!("CARGO_PKG_VERSION"));

/// Returns [`BUILD_ID`]. Kept as a fn (not just the const) because
/// callers historically bound to `cargoless_core::build_id()`; both paths now
/// resolve to the single canonical string.
pub fn build_id() -> &'static str {
    BUILD_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_id_is_single_consolidated_identity() {
        // §gap-3 / #89: BUILD_ID is THE single product-name site. This
        // test is the regression guard that enforces the consolidation
        // invariants a future edit (incl. the D1 rename) must keep:
        let id = build_id();

        // 1. Non-empty `<name> <version>` shape.
        let (name, version) = id
            .split_once(' ')
            .expect("BUILD_ID is `<product> <version>` (one space)");
        assert!(!name.is_empty(), "product name must not be empty");
        assert!(!version.is_empty(), "version must not be empty");

        // 2. Carries the crate version (D1 rename must not drop it).
        assert_eq!(
            version,
            env!("CARGO_PKG_VERSION"),
            "BUILD_ID must end with the workspace package version"
        );

        // 3. Terraform-collision guard (the project-long invariant):
        //    the bare `tf` name must never be the identifier, AND the
        //    old `tf-trunk` placeholder (which leaked the rejected `tf`
        //    token) must be gone post-#89.
        assert_ne!(name, "tf", "bare Terraform-colliding name rejected");
        assert!(
            !id.contains("tf-trunk"),
            "the stale `tf-trunk` placeholder must not survive #89: got {id:?}"
        );

        // 4. `build_id()` and the `BUILD_ID` const are the SAME string
        //    (callers may use either; they must never diverge — that
        //    divergence IS the §gap-3 bug, just internalised).
        assert_eq!(build_id(), BUILD_ID);
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
