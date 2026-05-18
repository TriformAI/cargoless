//! Repo-scoped daemon (Model R Stream B, `D-FLEET-SHARED-DAEMON` §3-§5).
//!
//! **Status (task #175):** the *config-consuming scaffold* — a
//! [`RepoScope`] that binds the resolved repo-level [`FleetConfig`] to the
//! discovered+classified worktree [`topology`], plus the load-bearing
//! **per-worktree config re-resolution** path
//! ([`RepoScope::worktree_config`]). The daemon lifecycle itself
//! (`serve --repo` process model, the serve/select loop, Unix-socket
//! binding), the #4 file-watcher, and #12 activity-activation remain
//! **#157**, which formally unblocks when Stream A (#156: config + CAS)
//! integrates to `origin/main`. No RA, no watcher, no activity, no
//! transport, no daemonize code lives here yet — by #175 scope.
//!
//! ## Why core re-resolves config (builder-infra's #1 refinement)
//!
//! `cargoless-core` owns BOTH the resolved [`FleetConfig`] type AND the
//! clap-free precedence resolver, because the Model R daemon must
//! **re-resolve config at runtime, per discovered worktree, with no CLI in
//! the loop**: a worktree may carry its own `tf.toml` whose
//! `[project] state_dir` differs (the operator's convention is
//! `.triform/cargoless`). [`FleetConfig::resolve_layered`] with a default
//! [`FleetOverrides`] + the env seam is exactly that no-CLI re-resolution
//! path; this module drives it.
//!
//! ## The per-worktree correctness boundary (Stream-B design decision)
//!
//! Only `state_dir` is *legitimately* per-worktree — each worktree writes
//! its own `cli-status` / `tree.cache` / diagnostics under its own state
//! dir. **Every other field is fleet-global** and is carried from the
//! daemon's resolved repo-level config UNCHANGED. In particular `cas_dir`
//! (the SHARED content-addressed CAS) must NEVER be re-derived per-worktree
//! — doing so would silently defeat fleet CAS dedup, which is the
//! existence-lever of the whole Model R architecture (`D-FLEET` §2). And a
//! worktree overrides `state_dir` **only via its own `tf.toml`**: a WT
//! without one *inherits the daemon's resolved `state_dir`* (which already
//! encodes the full CLI > env > repo-`tf.toml` > default precedence) —
//! resetting it to the bare `.cargoless` default would break the
//! operator's fleet-wide `.triform/cargoless` convention for every
//! tf.toml-less worktree. This is the load-bearing semantic of #175.

pub mod topology;

use std::fmt;
use std::path::{Path, PathBuf};

use crate::config::{FleetConfig, FleetConfigError, FleetOverrides, Source};
use topology::{WorktreeEntry, WtClass, classify, list_worktrees};

/// Failure bringing up a [`RepoScope`]. A daemon's startup error is its
/// onboarding UX (same principle as `FleetConfigError` / the CLI
/// `ConfigError`) — each variant renders one actionable line.
#[derive(Debug)]
pub enum RepoScopeError {
    /// `fleet.repo` is `None` ⇒ not daemon mode. `serve --repo <path>`
    /// (or `TF_REPO` / `[fleet] repo`) is required to run repo-scoped.
    NotDaemonMode,
    /// `git worktree list --porcelain` could not be run / failed.
    Enumerate(std::io::Error),
}

impl fmt::Display for RepoScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepoScopeError::NotDaemonMode => write!(
                f,
                "repo-scoped daemon needs a repo root.\n  \
                 pass `serve --repo <path>` (or set `TF_REPO` / \
                 `[fleet] repo` in tf.toml)."
            ),
            RepoScopeError::Enumerate(e) => write!(
                f,
                "could not enumerate worktrees via \
                 `git worktree list --porcelain`: {e}.\n  \
                 run cargoless from inside the repo, or check `git` is on PATH."
            ),
        }
    }
}

impl std::error::Error for RepoScopeError {}

/// The resolved repo-level [`FleetConfig`] bound to its
/// discovered+classified worktree topology — the structural seam the
/// #157 serve-loop builds on. Holds **no live daemon state** (no RA, no
/// watcher, no activity) by #175 scope: it is the *config × topology*
/// view, not the running daemon.
#[derive(Debug, Clone)]
pub struct RepoScope {
    /// Repo-level resolved fleet config (`repo` is `Some`, i.e.
    /// `daemon_mode()` is true). Fleet-global fields here are
    /// authoritative for every worktree.
    pub fleet: FleetConfig,
    /// The repo root (`fleet.repo` unwrapped). Worktree classification +
    /// per-WT state paths are computed relative to this.
    pub repo_root: PathBuf,
    /// Worktrees as enumerated by `git worktree list --porcelain`.
    pub worktrees: Vec<WorktreeEntry>,
}

impl RepoScope {
    /// Pure constructor — the test seam, and the exact shape the #157
    /// serve-loop assembles after it has run discovery itself. No I/O.
    pub fn from_parts(
        fleet: FleetConfig,
        repo_root: impl Into<PathBuf>,
        worktrees: Vec<WorktreeEntry>,
    ) -> Self {
        Self {
            fleet,
            repo_root: repo_root.into(),
            worktrees,
        }
    }

    /// I/O constructor: require daemon mode, then enumerate worktrees via
    /// [`topology::list_worktrees`]. The only I/O in `RepoScope`; the
    /// pure `from_parts` is what the tests exercise (a `git` spawn is not
    /// deterministically unit-testable — the established house split).
    pub fn discover(fleet: FleetConfig) -> Result<Self, RepoScopeError> {
        let repo_root = fleet.repo.clone().ok_or(RepoScopeError::NotDaemonMode)?;
        let worktrees = list_worktrees(&repo_root).map_err(RepoScopeError::Enumerate)?;
        Ok(Self::from_parts(fleet, repo_root, worktrees))
    }

    /// `(WtClass, &WorktreeEntry)` for every discovered worktree — ties
    /// #174's pure [`classify`] to the repo scope. This is the topology
    /// the #4 watcher will *route on* (nested ones caught by the base
    /// subtree watcher, non-nested ones each needing their own); it is
    /// the classification, NOT the watcher (that is #157/#4).
    pub fn classified(&self) -> impl Iterator<Item = (WtClass, &WorktreeEntry)> {
        self.worktrees
            .iter()
            .map(|wt| (classify(&self.repo_root, &wt.path), wt))
    }

    /// **The #175 load-bearing path.** The effective [`FleetConfig`] for
    /// the worktree rooted at `wt_root`.
    ///
    /// Re-resolves config layered against `wt_root` (so that worktree's
    /// own `tf.toml` is consulted) with a **default** [`FleetOverrides`]
    /// (no CLI in the loop — CLI flags are repo-global, applied once at
    /// daemon start) and the injected `env` seam. Then:
    ///
    /// * **Only** `state_dir` may differ per-worktree, and only when the
    ///   worktree set it in **its own `tf.toml`**
    ///   ([`Source::TfToml`] at the per-WT layer). Otherwise the daemon's
    ///   resolved `state_dir` is inherited verbatim (it already encodes
    ///   CLI > env > repo-`tf.toml` > default — re-resolving to the bare
    ///   default would wrongly strip a repo-level `.triform/cargoless`).
    /// * **All fleet-global fields** (`cas_dir`, `repo`, `bind`, `corun`,
    ///   `auth_token`) are carried from `self.fleet` UNCHANGED. Re-deriving
    ///   `cas_dir` per-worktree would defeat shared-CAS dedup (`D-FLEET`
    ///   §2) — the architecture's existence lever.
    ///
    /// Errors only if the worktree's `tf.toml` is itself malformed in a
    /// fleet-owned key (propagated verbatim from
    /// [`FleetConfig::resolve_layered`]).
    pub fn worktree_config(
        &self,
        wt_root: &Path,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<FleetConfig, FleetConfigError> {
        let per_wt = FleetConfig::resolve_layered(wt_root, FleetOverrides::default(), env)?;
        let mut cfg = self.fleet.clone();
        // A worktree overrides state_dir ONLY via its own tf.toml; env /
        // default at the per-WT layer must not clobber the daemon's
        // resolved (possibly CLI/env/repo-tf.toml) state_dir.
        if per_wt.provenance.state_dir == Source::TfToml {
            cfg.state_dir = per_wt.state_dir;
            cfg.provenance.state_dir = Source::TfToml;
        }
        Ok(cfg)
    }

    /// Convenience: the **absolute** state directory for the worktree at
    /// `wt_root` (where that worktree's `cli-status` / `tree.cache` /
    /// diagnostics live), honoring its own `tf.toml` per
    /// [`Self::worktree_config`] and resolved against `wt_root` via the
    /// frozen [`FleetConfig::state_dir_abs`].
    pub fn worktree_state_dir(
        &self,
        wt_root: &Path,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<PathBuf, FleetConfigError> {
        Ok(self.worktree_config(wt_root, env)?.state_dir_abs(wt_root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn wt(path: &str) -> WorktreeEntry {
        // Minimal entry — only `path` matters for classify / per-WT cfg.
        let mut e = topology::parse_worktree_porcelain(&format!(
            "worktree {path}\nHEAD a\nbranch refs/heads/x\n"
        ));
        e.pop().expect("one entry")
    }

    const REPO: &str = "/Users/iggy/Documents/GitHub/tf-multiverse";

    fn fleet_with(state_dir: &str, cas: Option<&str>) -> FleetConfig {
        let mut f = FleetConfig::defaults();
        f.repo = Some(PathBuf::from(REPO));
        f.state_dir = PathBuf::from(state_dir);
        f.provenance.state_dir = Source::Cli; // pretend the operator set it
        f.cas_dir = cas.map(PathBuf::from);
        f
    }

    #[test]
    fn discover_requires_daemon_mode() {
        // fleet.repo == None ⇒ NotDaemonMode (no git spawn attempted).
        let f = FleetConfig::defaults();
        let err = RepoScope::discover(f).unwrap_err();
        assert!(matches!(err, RepoScopeError::NotDaemonMode));
        assert!(err.to_string().contains("--repo"));
    }

    #[test]
    fn classified_ties_topology_to_repo_scope() {
        let scope = RepoScope::from_parts(
            fleet_with(".triform/cargoless", None),
            REPO,
            vec![
                wt(REPO),
                wt(&format!("{REPO}/.claude/worktrees/agent-x")),
                wt("/Users/iggy/Documents/GitHub/tf-multiverse-flat"),
                wt("/var/tmp/special"),
            ],
        );
        let classes: Vec<WtClass> = scope.classified().map(|(c, _)| c).collect();
        assert_eq!(
            classes,
            vec![
                WtClass::Main,
                WtClass::Nested,
                WtClass::Sibling,
                WtClass::Other,
            ]
        );
    }

    #[test]
    fn worktree_without_own_tftoml_inherits_daemon_state_dir() {
        // THE load-bearing semantic: a tf.toml-less worktree must inherit
        // the daemon's resolved `.triform/cargoless`, NOT reset to the
        // bare `.cargoless` default (which would break the operator's
        // fleet-wide convention for every such worktree).
        let scope = RepoScope::from_parts(fleet_with(".triform/cargoless", None), REPO, vec![]);
        let tmp = std::env::temp_dir().join(format!(
            "cargoless-rs-noTOML-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let cfg = scope.worktree_config(&tmp, &no_env).unwrap();
        assert_eq!(
            cfg.state_dir,
            PathBuf::from(".triform/cargoless"),
            "tf.toml-less WT inherits the daemon's resolved state_dir"
        );
        assert_eq!(
            cfg.provenance.state_dir,
            Source::Cli,
            "provenance preserved"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn worktree_with_own_tftoml_overrides_only_state_dir() {
        // A worktree that sets [project] state_dir in ITS OWN tf.toml
        // overrides state_dir — but cas_dir (fleet-global, the shared
        // CAS) is carried UNCHANGED (re-deriving it per-WT would defeat
        // fleet dedup — the architecture's existence lever).
        let scope = RepoScope::from_parts(
            fleet_with(".triform/cargoless", Some("/shared/cas")),
            REPO,
            vec![],
        );
        let tmp = std::env::temp_dir().join(format!(
            "cargoless-rs-ownTOML-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("tf.toml"),
            "[project]\nstate_dir = \".wt-local/state\"\n",
        )
        .unwrap();

        let cfg = scope.worktree_config(&tmp, &no_env).unwrap();
        assert_eq!(
            cfg.state_dir,
            PathBuf::from(".wt-local/state"),
            "WT's own tf.toml overrides state_dir"
        );
        assert_eq!(cfg.provenance.state_dir, Source::TfToml);
        // Fleet-global cas_dir is UNCHANGED — the load-bearing invariant.
        assert_eq!(
            cfg.cas_dir,
            Some(PathBuf::from("/shared/cas")),
            "shared CAS must NOT be re-derived per-worktree"
        );
        assert_eq!(cfg.repo, Some(PathBuf::from(REPO)));

        // And the absolute per-WT state dir is rooted at the worktree.
        let abs = scope.worktree_state_dir(&tmp, &no_env).unwrap();
        assert_eq!(abs, tmp.join(".wt-local/state"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn malformed_worktree_tftoml_propagates_error() {
        // A fleet-owned key malformed in the worktree's own tf.toml is a
        // hard error, surfaced verbatim from the frozen resolver (not
        // swallowed — a daemon's config error is its onboarding UX).
        let scope = RepoScope::from_parts(fleet_with(".cargoless", None), REPO, vec![]);
        let tmp = std::env::temp_dir().join(format!(
            "cargoless-rs-badTOML-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("tf.toml"), "[fleet]\nbind = \"not-a-socket\"\n").unwrap();
        let err = scope.worktree_config(&tmp, &no_env).unwrap_err();
        // A bad bind *in tf.toml* is mapped by the frozen resolver to
        // BadTfToml (line+context), NOT the bare BadBind — propagated
        // verbatim, not swallowed (a daemon's config error is its UX).
        assert!(
            matches!(err, FleetConfigError::BadTfToml { .. }),
            "malformed per-WT tf.toml fleet key propagates verbatim: {err:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
