//! #6 — workspace-cluster manager (Model R Stream C, `D-FLEET-SHARED-DAEMON`
//! §6.3 — the pure config/RA-INDEPENDENT correctness core).
//!
//! ## The problem (§6.3)
//!
//! LSP overlay multiplexing (#5) cleanly serves `.rs` *content* across N
//! worktrees through one RA. It does **not** cleanly handle changes to the
//! files that define the *workspace itself* rather than file content:
//! `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.cargo/config.toml`.
//! Two worktrees that disagree on any of those are not the same RA
//! workspace and **must not** be served by the same RA instance.
//!
//! Solution: group worktrees by [`WorkspaceConfigHash`] (a hash of those
//! four files); the daemon spawns one RA per distinct cluster. For
//! tf-multiverse the dominant case is a single cluster (every worktree
//! shares the base `Cargo.toml`/`Cargo.lock`); worktrees experimenting
//! with dep changes form their own cluster — an extra RA only when
//! genuinely needed.
//!
//! ## Load-bearing design judgment (flagged for senior review — §6.3
//! is "the hardest piece")
//!
//! The asymmetry that drives every choice here:
//!
//! * **Under-clustering is a correctness bug.** Co-serving two worktrees
//!   with *different* workspace config on one RA gives at least one of
//!   them an RA whose crate-graph / cfg / toolchain model does not match
//!   its real workspace ⇒ a wrong verdict. This must never happen.
//! * **Over-clustering is only a performance cost.** Splitting two
//!   worktrees that *could* have shared an RA just spends one extra RA —
//!   the §6.3-sanctioned "spawn additional RA only when needed", erring
//!   safe.
//!
//! So the hash **biases to split**: any byte difference in any of the
//! four files ⇒ a distinct cluster, and **absent ≠ present-but-empty**
//! (a deliberately-committed empty `.cargo/config.toml` is a different
//! artifact from having none; co-clustering them would be an *assumption*
//! about cargo semantics this layer must not make — splitting is the safe
//! default, the cost is at most one RA). Correctness is purchased with a
//! bounded, sanctioned resource cost; the reverse trade would buy a
//! resource saving with a wrong verdict.
//!
//! ## Length-unambiguous canonical encoding
//!
//! The hash is **not** a naive concatenation of the four files (which
//! would let `Cargo.toml="x"`+`lock=""` collide with `Cargo.toml=""`+
//! `lock="x"`). Each file is reduced to a fixed-shape token first —
//! `P<64-hex-subhash>` when present, the single char `A` when absent —
//! and the four tokens are newline-joined in a **fixed order** under a
//! versioned domain prefix. A 64-hex sub-hash and `A` are mutually
//! unambiguous and fixed-width-or-shorter with a hard delimiter, so no
//! cross-file boundary ambiguity is possible by construction.
//!
//! `cargoless/wsconfig/v1` is a *new additive* domain tag (the §9a
//! wire-constant *discipline* — versioned, identifiable, future-evolvable
//! — applied to a brand-new surface; it is NOT one of the frozen §9a
//! constants and touches none of them).
//!
//! ## Pure (house pattern)
//!
//! [`WorkspaceConfig::hash`] + [`cluster_worktrees`] are pure (no I/O, no
//! RA, no spawn) and exhaustively unit-tested. Reading the four files
//! off each worktree's disk and the RA-per-cluster lifecycle (spawn,
//! last-WT-disconnect teardown, the cardinality/teardown-race handling
//! §6.3 calls the hard part) is the thin I/O shell — a follow-up
//! increment, the established pure-core-first split (cf.
//! #174/#175/#4/#12/#5). Its correctness reduces to *this* hash being
//! collision-free across distinct workspace configs, which is what the
//! tests prove.

use std::collections::BTreeMap;

use cargoless_cas::sha256_hex;

/// The four workspace-defining files for a worktree. `None` = the file
/// is absent in that worktree (distinct from `Some(String::new())` =
/// present-but-empty — see the bias-to-split judgment in the module
/// docs). The I/O shell fills these by reading the worktree; this core
/// is pure over the contents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub cargo_toml: Option<String>,
    pub cargo_lock: Option<String>,
    pub rust_toolchain: Option<String>,
    pub cargo_config: Option<String>,
}

/// A worktree's workspace-cluster identity. Equal hashes ⇒ the daemon
/// MAY serve those worktrees with one shared RA; any difference ⇒ they
/// MUST get separate RAs (the load-bearing under-vs-over-clustering
/// asymmetry — see module docs).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceConfigHash(String);

impl WorkspaceConfigHash {
    /// The 64-hex digest string (for the cli-status / bench / logs).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Versioned domain prefix — the §9a *discipline* (identifiable,
/// future-evolvable) on a NEW additive surface; not a frozen §9a
/// constant.
const WSCONFIG_DOMAIN: &str = "cargoless/wsconfig/v1";

impl WorkspaceConfig {
    /// Build from the four optional file contents (the test seam + the
    /// shape the I/O shell produces from a worktree's disk).
    pub fn new(
        cargo_toml: Option<String>,
        cargo_lock: Option<String>,
        rust_toolchain: Option<String>,
        cargo_config: Option<String>,
    ) -> Self {
        Self {
            cargo_toml,
            cargo_lock,
            rust_toolchain,
            cargo_config,
        }
    }

    /// The cluster hash. Pure, deterministic, length-unambiguous,
    /// biased-to-split (see module docs). Equal iff all four files are
    /// byte-identical-or-both-absent.
    pub fn hash(&self) -> WorkspaceConfigHash {
        // Reduce each file to a fixed-shape, boundary-unambiguous token
        // BEFORE joining, so no concatenation collision is possible.
        fn token(f: &Option<String>) -> String {
            match f {
                Some(content) => format!("P{}", sha256_hex(content.as_bytes())),
                None => "A".to_string(),
            }
        }
        // Fixed file order; newline-delimited; domain-prefixed.
        let canonical = format!(
            "{WSCONFIG_DOMAIN}\n{}\n{}\n{}\n{}",
            token(&self.cargo_toml),
            token(&self.cargo_lock),
            token(&self.rust_toolchain),
            token(&self.cargo_config),
        );
        WorkspaceConfigHash(sha256_hex(canonical.as_bytes()))
    }
}

/// Group worktrees by workspace-cluster. Input: `(WtId, WorkspaceConfig)`
/// pairs (`WtId` is the worktree-root path string — same path-keyed
/// identity #4/#175 use). Output: `cluster_hash → sorted WtIds`, a
/// `BTreeMap` so the grouping is **deterministic regardless of input
/// order** (load-bearing: the daemon must not spin up/tear down RAs just
/// because discovery enumerated worktrees in a different order). Pure.
pub fn cluster_worktrees<I, S>(entries: I) -> BTreeMap<WorkspaceConfigHash, Vec<String>>
where
    I: IntoIterator<Item = (S, WorkspaceConfig)>,
    S: Into<String>,
{
    let mut clusters: BTreeMap<WorkspaceConfigHash, Vec<String>> = BTreeMap::new();
    for (wt, cfg) in entries {
        clusters.entry(cfg.hash()).or_default().push(wt.into());
    }
    // Sort WtIds within each cluster ⇒ stable membership lists.
    for wts in clusters.values_mut() {
        wts.sort();
        wts.dedup();
    }
    clusters
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(
        ct: Option<&str>,
        cl: Option<&str>,
        rt: Option<&str>,
        cc: Option<&str>,
    ) -> WorkspaceConfig {
        WorkspaceConfig::new(
            ct.map(str::to_string),
            cl.map(str::to_string),
            rt.map(str::to_string),
            cc.map(str::to_string),
        )
    }

    #[test]
    fn identical_config_same_hash() {
        let a = cfg(Some("[workspace]"), Some("lock-v1"), Some("1.85"), None);
        let b = cfg(Some("[workspace]"), Some("lock-v1"), Some("1.85"), None);
        assert_eq!(a.hash(), b.hash(), "byte-identical config ⇒ same cluster");
        assert_eq!(a.hash().as_str().len(), 64, "64-hex digest");
    }

    #[test]
    fn any_single_file_difference_splits_cluster() {
        let base = cfg(
            Some("[workspace]"),
            Some("lockA"),
            Some("1.85"),
            Some("cfgA"),
        );
        // Each variant differs in exactly ONE of the four files.
        let diff_toml = cfg(
            Some("[workspace]\nx=1"),
            Some("lockA"),
            Some("1.85"),
            Some("cfgA"),
        );
        let diff_lock = cfg(
            Some("[workspace]"),
            Some("lockB"),
            Some("1.85"),
            Some("cfgA"),
        );
        let diff_tc = cfg(
            Some("[workspace]"),
            Some("lockA"),
            Some("1.86"),
            Some("cfgA"),
        );
        let diff_cc = cfg(
            Some("[workspace]"),
            Some("lockA"),
            Some("1.85"),
            Some("cfgB"),
        );
        for v in [&diff_toml, &diff_lock, &diff_tc, &diff_cc] {
            assert_ne!(
                base.hash(),
                v.hash(),
                "a difference in ANY of the 4 files must split the cluster (under-cluster = wrong verdict)"
            );
        }
    }

    #[test]
    fn absent_differs_from_present_empty_bias_to_split() {
        // The load-bearing safety judgment: a missing .cargo/config.toml
        // and a present-but-empty one are NOT assumed equivalent — split
        // (cost: ≤1 extra RA; the reverse would risk a wrong verdict).
        let absent = cfg(Some("[workspace]"), Some("L"), None, None);
        let present_empty = cfg(Some("[workspace]"), Some("L"), None, Some(""));
        assert_ne!(
            absent.hash(),
            present_empty.hash(),
            "absent ≠ present-but-empty (bias-to-split: never assume cargo semantics here)"
        );
    }

    #[test]
    fn no_cross_file_boundary_collision() {
        // The naive-concat collision the tokenized encoding defeats:
        // (cargo_toml="x", cargo_lock="") vs (cargo_toml="", cargo_lock="x")
        // must NOT hash equal.
        let a = cfg(Some("x"), Some(""), None, None);
        let b = cfg(Some(""), Some("x"), None, None);
        assert_ne!(
            a.hash(),
            b.hash(),
            "tokenized encoding must be length/boundary unambiguous"
        );
    }

    #[test]
    fn tf_mv_common_case_one_cluster() {
        // The dominant tf-multiverse shape: every worktree shares the
        // base workspace config ⇒ exactly ONE cluster (one shared RA).
        let shared = || {
            cfg(
                Some("[workspace]\nmembers=[]"),
                Some("lock"),
                Some("1.85"),
                None,
            )
        };
        let clusters = cluster_worktrees([
            ("/repo/.claude/worktrees/a", shared()),
            ("/repo/.claude/worktrees/b", shared()),
            ("/repo", shared()),
            ("/repo-sibling", shared()),
        ]);
        assert_eq!(clusters.len(), 1, "all-shared ⇒ single cluster");
        let (_, members) = clusters.iter().next().unwrap();
        assert_eq!(
            members,
            &vec![
                "/repo".to_string(),
                "/repo-sibling".to_string(),
                "/repo/.claude/worktrees/a".to_string(),
                "/repo/.claude/worktrees/b".to_string(),
            ],
            "sorted, deduped membership"
        );
    }

    #[test]
    fn divergent_dep_experiment_forms_own_cluster() {
        let base = || cfg(Some("[workspace]"), Some("lock-v1"), None, None);
        let experiment = cfg(
            Some("[workspace]"),
            Some("lock-v2-bumped-serde"),
            None,
            None,
        );
        let clusters = cluster_worktrees([
            ("/repo/.claude/worktrees/a", base()),
            ("/repo/.claude/worktrees/b", base()),
            ("/repo/.claude/worktrees/dep-exp", experiment),
        ]);
        assert_eq!(clusters.len(), 2, "the dep-experiment WT splits off");
        // The base cluster has the two base WTs; the experiment cluster
        // has exactly the one.
        let sizes: Vec<usize> = clusters.values().map(Vec::len).collect();
        let mut sorted = sizes.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![1, 2],
            "2-member base cluster + 1-member experiment"
        );
    }

    #[test]
    fn grouping_is_input_order_independent() {
        let a = || cfg(Some("A"), None, None, None);
        let b = || cfg(Some("B"), None, None, None);
        let fwd = cluster_worktrees([("w1", a()), ("w2", b()), ("w3", a())]);
        let rev = cluster_worktrees([("w3", a()), ("w2", b()), ("w1", a())]);
        assert_eq!(
            fwd, rev,
            "discovery order must not change clustering (no RA churn from enumeration order)"
        );
        // a-cluster = {w1,w3}, b-cluster = {w2}.
        assert_eq!(fwd.len(), 2);
        assert_eq!(fwd[&a().hash()], vec!["w1".to_string(), "w3".to_string()]);
        assert_eq!(fwd[&b().hash()], vec!["w2".to_string()]);
    }

    #[test]
    fn all_absent_is_a_valid_stable_cluster() {
        // A worktree with none of the four files (degenerate, but must
        // not panic and must be stable/groupable).
        let none = WorkspaceConfig::default();
        assert_eq!(none.hash(), WorkspaceConfig::default().hash());
        let clusters = cluster_worktrees([("w1", none.clone()), ("w2", none)]);
        assert_eq!(clusters.len(), 1);
    }
}
