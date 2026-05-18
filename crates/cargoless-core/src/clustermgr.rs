//! #6 cluster-manager I/O shell (Model R Stream C #158, I/O-shell
//! increment-4) — two pure pieces that the eventual live adapter
//! composes with `analyzer::Supervisor`:
//!
//! 1. [`read_workspace_config`] — the worktree disk-read that feeds the
//!    already-backstop-CLEAR'd [`cluster::WorkspaceConfig::hash`]
//!    (on main @318b78f). Pure over the bytes it reads; the only
//!    subtlety is the absent-vs-present distinction the pure core's
//!    bias-to-split judgment rests on, plus the legacy-filename fallback.
//! 2. [`ClusterLifecycle`] — the pure refcount state machine that decides
//!    **when** the daemon spawns / tears down the one shared RA per
//!    cluster. This is the load-bearing **teardown-race** core: the live
//!    adapter is a thin map from [`LifecycleAction`] to
//!    `Supervisor::{start,shutdown}`, held single-owner behind one mutex.
//!
//! ## The named load-bearing judgment (layer-3 backstop target)
//!
//! > The last worktree disconnecting from a cluster's RA cannot race a
//! > concurrent re-activation of that same cluster into a
//! > use-after-free / double-spawn.
//!
//! Made precise and falsifiable, in pure form:
//!
//! * **Spawn iff 0→1, Teardown iff 1→0, atomically with the membership
//!   mutation.** [`activate`](ClusterLifecycle::activate) and
//!   [`deactivate`](ClusterLifecycle::deactivate) each take `&mut self`
//!   and, within that single call, *both* mutate the membership set *and*
//!   compute the [`LifecycleAction`] from the exact pre/post cardinality.
//!   There is no point at which membership is mutated but the action does
//!   not reflect the 0↔1 boundary, and no path returns
//!   [`SpawnRa`](LifecycleAction::SpawnRa) /
//!   [`TeardownRa`](LifecycleAction::TeardownRa) without the matching
//!   transition having just been applied to `self`.
//! * **Why that kills the race.** The live adapter owns the
//!   `ClusterLifecycle` behind one mutex, so `activate`/`deactivate`
//!   calls are *serialized*. Because the transition and its action are
//!   the *same atomic `&mut self` step*, a `TeardownRa(H)` and a
//!   `SpawnRa(H)` can never be derived from an interleaved/torn view of
//!   H's membership — the only way to get `SpawnRa(H)` is the 0→1 edge
//!   and the only way to get `TeardownRa(H)` is the 1→0 edge, and those
//!   edges are totally ordered by the serialized mutations. The pure
//!   core makes "no torn cardinality" a structural property; the mutex
//!   makes "serialized" a one-line adapter obligation, not a
//!   correctness-spread-across-the-codebase concern.
//! * **Idempotent / total.** Re-activating an existing member or
//!   deactivating a non-member is [`NoOp`](LifecycleAction::NoOp) (never
//!   a spurious spawn/teardown — a double-activate cannot double-spawn,
//!   a double-deactivate cannot double-teardown). Every (event, state)
//!   pair has a defined transition.
//!
//! `read_workspace_config` returns [`io::Result`]: a genuinely-absent
//! file is `None` (the pure core's documented "absent" case), but a hard
//! read error on a *present* file is propagated — the live adapter's
//! contract is to treat such a worktree as **its own cluster** (split is
//! always verdict-safe; silently mapping unreadable→absent could
//! under-cluster two divergent configs into one wrong-verdict RA). That
//! one bit of policy is the caller's, by the same author/caller split
//! that kept the flycheck barrier pure.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use crate::cluster::{WorkspaceConfig, WorkspaceConfigHash};

/// Read one file: `Ok(Some(content))` if present & readable, `Ok(None)`
/// if genuinely absent (`NotFound`), `Err` for any other I/O failure on
/// a path that exists (propagated — see module docs' split-safe policy).
fn read_one(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read the first present file among `candidates` (canonical name first,
/// legacy name second). `Ok(None)` iff *every* candidate is absent; a
/// hard error on a present candidate is propagated.
fn read_first(root: &Path, candidates: &[&str]) -> io::Result<Option<String>> {
    for rel in candidates {
        if let Some(content) = read_one(&root.join(rel))? {
            return Ok(Some(content));
        }
    }
    Ok(None)
}

/// Read a worktree's four workspace-defining files into the proven
/// [`WorkspaceConfig`] (whose [`hash`](WorkspaceConfig::hash) decides
/// cluster identity). Handles the two real dual-name cases so a worktree
/// pinning its toolchain / cargo-config via the *legacy* filename is not
/// silently treated as absent (which would under-cluster it with a
/// differently-pinned sibling — a wrong-verdict shared RA):
///
/// * toolchain: `rust-toolchain.toml` (canonical) ‖ `rust-toolchain` (legacy)
/// * cargo cfg: `.cargo/config.toml` (canonical) ‖ `.cargo/config` (legacy)
///
/// `Cargo.toml` / `Cargo.lock` have no legacy alias.
pub fn read_workspace_config(root: &Path) -> io::Result<WorkspaceConfig> {
    Ok(WorkspaceConfig::new(
        read_first(root, &["Cargo.toml"])?,
        read_first(root, &["Cargo.lock"])?,
        read_first(root, &["rust-toolchain.toml", "rust-toolchain"])?,
        read_first(root, &[".cargo/config.toml", ".cargo/config"])?,
    ))
}

/// What the lifecycle tells the live adapter after one membership event.
/// The adapter maps this 1:1 onto `analyzer::Supervisor::{start,shutdown}`
/// — nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleAction {
    /// First worktree joined this cluster (0→1) — spawn the shared RA.
    SpawnRa(WorkspaceConfigHash),
    /// Last worktree left this cluster (1→0) — tear the shared RA down.
    TeardownRa(WorkspaceConfigHash),
    /// Membership changed within an already-live (or already-dead)
    /// cluster, or a no-op event — the RA set is unchanged.
    NoOp,
}

/// Pure refcount state machine over `cluster_hash → {worktree members}`.
///
/// Single-owner by contract: the live adapter holds exactly one of these
/// behind one mutex, so [`activate`](Self::activate) /
/// [`deactivate`](Self::deactivate) are serialized. See the module docs
/// for the precise, falsifiable teardown-race invariant this enforces.
#[derive(Debug, Clone, Default)]
pub struct ClusterLifecycle {
    /// `cluster hash → live members`. A cluster is present here iff it
    /// has ≥1 member (i.e. iff its RA is live); a 1→0 transition removes
    /// the key entirely, so the map's keyset == the set of live RAs.
    members: BTreeMap<WorkspaceConfigHash, BTreeSet<String>>,
    /// `worktree → its current cluster hash`, so a re-route (config edit
    /// moving a WT between clusters) is expressible as
    /// deactivate-old-then-activate-new without the caller having to
    /// remember the old hash.
    wt_cluster: BTreeMap<String, WorkspaceConfigHash>,
}

impl ClusterLifecycle {
    /// Fresh — no clusters, no RAs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Worktree `wt` is now active in cluster `hash`.
    ///
    /// * `wt` already a member of `hash` ⇒ [`NoOp`](LifecycleAction::NoOp)
    ///   (idempotent — a double-activate can never double-spawn).
    /// * `wt` currently in a *different* cluster ⇒ it is first removed
    ///   from the old one (which may itself 1→0 and need a teardown — but
    ///   only one action is returned per call; callers that re-route a WT
    ///   across clusters must go through [`reroute`](Self::reroute), which
    ///   surfaces both actions).
    /// * cluster `hash` had 0 members ⇒ 0→1 ⇒
    ///   [`SpawnRa`](LifecycleAction::SpawnRa).
    /// * cluster `hash` already had ≥1 member ⇒ [`NoOp`](LifecycleAction::NoOp).
    ///
    /// The membership mutation and the returned action are computed in
    /// this one `&mut self` step from the exact pre/post cardinality —
    /// the structural property the teardown-race proof rests on.
    pub fn activate(
        &mut self,
        wt: impl Into<String>,
        hash: WorkspaceConfigHash,
    ) -> LifecycleAction {
        let wt = wt.into();
        if self.wt_cluster.get(&wt) == Some(&hash) {
            // Already a member of exactly this cluster — idempotent.
            return LifecycleAction::NoOp;
        }
        // Distinct from this fn's contract: a WT that was in another
        // cluster must be detached there first. Single-action-per-call:
        // we apply the detach to state but only surface THIS activate's
        // action; `reroute` is the API that needs both.
        self.detach(&wt);

        let set = self.members.entry(hash.clone()).or_default();
        let was_empty = set.is_empty();
        set.insert(wt.clone());
        self.wt_cluster.insert(wt, hash.clone());
        if was_empty {
            LifecycleAction::SpawnRa(hash)
        } else {
            LifecycleAction::NoOp
        }
    }

    /// Worktree `wt` is no longer active.
    ///
    /// * `wt` not a tracked member ⇒ [`NoOp`](LifecycleAction::NoOp)
    ///   (idempotent — a double-deactivate can never double-teardown).
    /// * its cluster drops to 0 members ⇒ 1→0 ⇒
    ///   [`TeardownRa`](LifecycleAction::TeardownRa) (and the cluster key
    ///   is removed — keyset stays == live-RA set).
    /// * its cluster still has ≥1 member ⇒ [`NoOp`](LifecycleAction::NoOp).
    pub fn deactivate(&mut self, wt: impl Into<String>) -> LifecycleAction {
        let wt = wt.into();
        match self.detach(&wt) {
            Some(hash) if !self.members.contains_key(&hash) => {
                // detach removed the cluster key ⇒ it hit 0 ⇒ 1→0.
                LifecycleAction::TeardownRa(hash)
            }
            _ => LifecycleAction::NoOp,
        }
    }

    /// Re-route `wt` to cluster `new_hash` (e.g. its `Cargo.lock`
    /// changed). Returns `(old_cluster_action, new_cluster_action)`:
    /// tearing the old cluster down iff `wt` was its last member, and
    /// spawning the new one iff `wt` is its first — the only API that can
    /// surface two RA actions, because a cross-cluster move is the one
    /// event that can both kill and birth an RA. Order is intentional:
    /// the adapter must act the teardown before the spawn is irrelevant
    /// to correctness (distinct RAs) but the tuple order documents
    /// "leaving" precedes "joining".
    pub fn reroute(
        &mut self,
        wt: impl Into<String>,
        new_hash: WorkspaceConfigHash,
    ) -> (LifecycleAction, LifecycleAction) {
        let wt = wt.into();
        if self.wt_cluster.get(&wt) == Some(&new_hash) {
            return (LifecycleAction::NoOp, LifecycleAction::NoOp);
        }
        let leaving = self.deactivate(wt.clone());
        let joining = self.activate(wt, new_hash);
        (leaving, joining)
    }

    /// Number of live clusters (== number of shared RAs the adapter
    /// should have running). Inspection / cli-status / tests.
    pub fn live_cluster_count(&self) -> usize {
        self.members.len()
    }

    /// Members of a cluster (deterministic order), or empty if it has no
    /// live RA. Inspection / tests.
    pub fn members_of(&self, hash: &WorkspaceConfigHash) -> Vec<String> {
        self.members
            .get(hash)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Remove `wt` from whatever cluster it is in, pruning the cluster
    /// key if it becomes empty. Returns the hash it *was* in (so the
    /// caller can tell whether a 1→0 just happened by checking
    /// `members.contains_key`). The single mutation primitive both
    /// `activate` (detach-before-join) and `deactivate` build on, so the
    /// "membership mutated == cardinality used for the action" property
    /// holds in exactly one place.
    fn detach(&mut self, wt: &str) -> Option<WorkspaceConfigHash> {
        let hash = self.wt_cluster.remove(wt)?;
        if let Some(set) = self.members.get_mut(&hash) {
            set.remove(wt);
            if set.is_empty() {
                self.members.remove(&hash);
            }
        }
        Some(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn h(cfg: &WorkspaceConfig) -> WorkspaceConfigHash {
        cfg.hash()
    }

    // --- read_workspace_config (disk-read shell) ------------------------

    #[test]
    fn reads_present_files_and_distinguishes_absent() {
        let dir = std::env::temp_dir().join(format!("cl-cm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".cargo")).unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        fs::write(dir.join("Cargo.lock"), "# lock\n").unwrap();
        // toolchain present via the LEGACY bare name only.
        fs::write(dir.join("rust-toolchain"), "1.85.0\n").unwrap();
        // cargo config present via the CANONICAL name.
        fs::write(dir.join(".cargo/config.toml"), "[build]\n").unwrap();

        let cfg = read_workspace_config(&dir).unwrap();
        assert_eq!(cfg.cargo_toml.as_deref(), Some("[package]\nname='x'\n"));
        assert_eq!(cfg.cargo_lock.as_deref(), Some("# lock\n"));
        assert_eq!(
            cfg.rust_toolchain.as_deref(),
            Some("1.85.0\n"),
            "legacy bare `rust-toolchain` must be detected (else under-cluster)"
        );
        assert_eq!(cfg.cargo_config.as_deref(), Some("[build]\n"));

        // A worktree with NO config files at all ⇒ all None (the pure
        // core's documented "absent" — distinct from present-but-empty).
        let empty = dir.join("empty");
        fs::create_dir_all(&empty).unwrap();
        let none = read_workspace_config(&empty).unwrap();
        assert_eq!(none, WorkspaceConfig::default());
        assert_eq!(
            none.hash(),
            WorkspaceConfig::default().hash(),
            "all-absent hashes to the canonical empty cluster"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn canonical_name_wins_over_legacy_when_both_present() {
        let dir = std::env::temp_dir().join(format!("cl-cm2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".cargo")).unwrap();
        fs::write(dir.join("rust-toolchain.toml"), "CANON\n").unwrap();
        fs::write(dir.join("rust-toolchain"), "LEGACY\n").unwrap();
        fs::write(dir.join(".cargo/config.toml"), "CANON\n").unwrap();
        fs::write(dir.join(".cargo/config"), "LEGACY\n").unwrap();
        let cfg = read_workspace_config(&dir).unwrap();
        assert_eq!(cfg.rust_toolchain.as_deref(), Some("CANON\n"));
        assert_eq!(cfg.cargo_config.as_deref(), Some("CANON\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn present_empty_file_is_some_not_none() {
        // The bias-to-split distinction the pure core depends on:
        // present-but-empty ≠ absent. read_one must yield Some("") for a
        // zero-byte file, NOT None.
        let dir = std::env::temp_dir().join(format!("cl-cm3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Cargo.toml"), "").unwrap();
        let cfg = read_workspace_config(&dir).unwrap();
        assert_eq!(cfg.cargo_toml.as_deref(), Some(""));
        assert_ne!(
            cfg.hash(),
            WorkspaceConfig::default().hash(),
            "present-but-empty Cargo.toml must NOT cluster with all-absent"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // --- ClusterLifecycle (teardown-race decision core) -----------------

    fn cfg_a() -> WorkspaceConfig {
        WorkspaceConfig::new(Some("A".into()), None, None, None)
    }
    fn cfg_b() -> WorkspaceConfig {
        WorkspaceConfig::new(Some("B".into()), None, None, None)
    }

    #[test]
    fn spawn_only_on_0_to_1_teardown_only_on_1_to_0() {
        let mut lc = ClusterLifecycle::new();
        let a = h(&cfg_a());
        // First member ⇒ 0→1 ⇒ Spawn.
        assert_eq!(
            lc.activate("wt1", a.clone()),
            LifecycleAction::SpawnRa(a.clone())
        );
        // Second member of same cluster ⇒ stays ≥1 ⇒ NoOp (NOT a 2nd spawn).
        assert_eq!(lc.activate("wt2", a.clone()), LifecycleAction::NoOp);
        assert_eq!(lc.live_cluster_count(), 1);
        // One leaves, one remains ⇒ still ≥1 ⇒ NoOp (NOT a teardown).
        assert_eq!(lc.deactivate("wt1"), LifecycleAction::NoOp);
        assert_eq!(lc.members_of(&a), vec!["wt2".to_string()]);
        // Last leaves ⇒ 1→0 ⇒ Teardown, key pruned.
        assert_eq!(lc.deactivate("wt2"), LifecycleAction::TeardownRa(a.clone()));
        assert_eq!(lc.live_cluster_count(), 0);
        assert!(lc.members_of(&a).is_empty());
    }

    #[test]
    fn idempotent_double_activate_and_double_deactivate() {
        let mut lc = ClusterLifecycle::new();
        let a = h(&cfg_a());
        assert_eq!(
            lc.activate("wt1", a.clone()),
            LifecycleAction::SpawnRa(a.clone())
        );
        // Re-activate the SAME wt in the SAME cluster ⇒ NoOp (no double-spawn).
        assert_eq!(lc.activate("wt1", a.clone()), LifecycleAction::NoOp);
        assert_eq!(lc.members_of(&a), vec!["wt1".to_string()]);
        assert_eq!(lc.deactivate("wt1"), LifecycleAction::TeardownRa(a.clone()));
        // Deactivate again — not a member ⇒ NoOp (no double-teardown / UAF).
        assert_eq!(lc.deactivate("wt1"), LifecycleAction::NoOp);
        // Deactivate a never-seen wt ⇒ NoOp (total).
        assert_eq!(lc.deactivate("ghost"), LifecycleAction::NoOp);
    }

    #[test]
    fn distinct_configs_get_distinct_ras() {
        let mut lc = ClusterLifecycle::new();
        let a = h(&cfg_a());
        let b = h(&cfg_b());
        assert_ne!(
            a, b,
            "different Cargo.toml ⇒ different cluster (bias-to-split)"
        );
        assert_eq!(
            lc.activate("wA", a.clone()),
            LifecycleAction::SpawnRa(a.clone())
        );
        assert_eq!(
            lc.activate("wB", b.clone()),
            LifecycleAction::SpawnRa(b.clone())
        );
        assert_eq!(
            lc.live_cluster_count(),
            2,
            "two divergent configs ⇒ two RAs"
        );
        assert_eq!(lc.deactivate("wA"), LifecycleAction::TeardownRa(a));
        assert_eq!(lc.live_cluster_count(), 1);
        assert_eq!(lc.deactivate("wB"), LifecycleAction::TeardownRa(b));
        assert_eq!(lc.live_cluster_count(), 0);
    }

    #[test]
    fn reroute_tears_old_and_spawns_new_only_at_boundaries() {
        let mut lc = ClusterLifecycle::new();
        let a = h(&cfg_a());
        let b = h(&cfg_b());
        // Two WTs in A; reroute wt1 to B. A still has wt2 ⇒ no teardown;
        // B is fresh ⇒ spawn.
        lc.activate("wt1", a.clone());
        lc.activate("wt2", a.clone());
        let (leaving, joining) = lc.reroute("wt1", b.clone());
        assert_eq!(leaving, LifecycleAction::NoOp, "A still has wt2");
        assert_eq!(joining, LifecycleAction::SpawnRa(b.clone()), "B is new");
        assert_eq!(lc.members_of(&a), vec!["wt2".to_string()]);
        assert_eq!(lc.members_of(&b), vec!["wt1".to_string()]);
        // Now reroute wt2 to B as well: A hits 1→0 ⇒ teardown; B already
        // live ⇒ NoOp.
        let (leaving, joining) = lc.reroute("wt2", b.clone());
        assert_eq!(leaving, LifecycleAction::TeardownRa(a.clone()));
        assert_eq!(joining, LifecycleAction::NoOp, "B already live");
        assert_eq!(lc.live_cluster_count(), 1);
        // Rerouting a WT to the cluster it is already in ⇒ (NoOp, NoOp).
        assert_eq!(
            lc.reroute("wt1", b.clone()),
            (LifecycleAction::NoOp, LifecycleAction::NoOp)
        );
    }

    #[test]
    fn activate_moving_across_clusters_detaches_old_membership() {
        // `activate` on a WT currently in another cluster must detach it
        // there (so membership stays consistent) even though it only
        // surfaces THIS call's action. The old cluster hitting 0 is then
        // observable via live_cluster_count (the adapter uses `reroute`
        // when it needs the teardown action; this guards the state
        // integrity of the bare `activate` path).
        let mut lc = ClusterLifecycle::new();
        let a = h(&cfg_a());
        let b = h(&cfg_b());
        lc.activate("wt1", a.clone());
        assert_eq!(lc.live_cluster_count(), 1);
        // Bare activate to B: detaches from A (A→0, key pruned), joins B.
        assert_eq!(
            lc.activate("wt1", b.clone()),
            LifecycleAction::SpawnRa(b.clone())
        );
        assert!(
            lc.members_of(&a).is_empty(),
            "old cluster A must have no stale membership for wt1"
        );
        assert_eq!(lc.members_of(&b), vec!["wt1".to_string()]);
        assert_eq!(
            lc.live_cluster_count(),
            1,
            "A pruned at 1→0, B live ⇒ exactly one RA"
        );
    }
}
