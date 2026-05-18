//! Repo-scoped daemon (Model R Stream B, `D-FLEET-SHARED-DAEMON` §3-§4).
//!
//! **Status (task #174):** only the [`topology`] submodule is implemented —
//! the *config-independent* worktree enumeration + classification core
//! (pure `git worktree list --porcelain` parser + nested/sibling/other
//! classifier). The repo-scoped daemon lifecycle itself (`serve --repo`,
//! activity-activation, the cluster/overlay wiring) is **#3 / #157**, which
//! is gated on Stream A (#156: config layer + CAS). When #3 lands,
//! `repo.rs` consumes a **resolved config struct passed down from the CLI
//! crate** (repo_root / state_dir / cas_dir / …) via constructor injection
//! — `cargoless-core` does NOT parse config (the dep direction core←cli is
//! preserved; the Stream A↔B seam is owned by builder-infra's #1). No
//! config/daemon/transport/watcher code lives here yet, by design.

pub mod topology;
