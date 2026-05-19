//! `serve --repo <path>` — the Model R repo-scoped daemon (Stream B #3 /
//! `D-FLEET-SHARED-DAEMON` §3-§4).
//!
//! ## What #3 is (and is not)
//!
//! #3 is the **daemon lifecycle harness**: resolve the fleet config from
//! flags+env+tf.toml, require daemon mode, discover+classify the repo's
//! worktrees ([`cargoless_core::repo::RepoScope`], built in #175), print
//! the §3.3 bring-up banner, install graceful shutdown (Ctrl-C / parent-
//! orphan, reusing the FIELD-FINDING-#13b [`crate::orphan`] guard the v0
//! `watch` loop already trusts), and run the serve-loop *skeleton*.
//!
//! #3 deliberately does **not** check anything yet. The serve-loop has two
//! explicit seams the rest of Stream B/C plug into, each marked in the
//! loop body:
//!
//! * **#4 (Stream B, next):** the per-worktree file-watcher + gitignore-
//!   inversion routing. Until #4 lands, nothing *drives* a check, so the
//!   loop legitimately parks on the shutdown signal — this is an honest
//!   harness, not faked work. #4 replaces the park with the routed
//!   `(WtId, ChangeBatch)` stream.
//! * **#5/#6 (Stream C, load-bearing):** the one-RA overlay multiplexer +
//!   workspace-cluster manager that turns a routed change into a per-WT
//!   verdict. `RepoScope::worktree_config` (the #175 seam) is what the
//!   serve-loop hands Stream C per discovered worktree.
//!
//! Keeping #3 a true harness (discovers, classifies, reports, holds the
//! process, shuts down cleanly — and nothing it cannot yet substantiate)
//! is the same asymmetric-honesty discipline the verdict surface uses:
//! never claim coverage that isn't there.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use cargoless_core::repo::RepoScope;
use cargoless_core::repo::topology::WtClass;
use cargoless_core::{FleetConfig, FleetOverrides, TelemetryConfig, TelemetryOverrides};

use crate::ui;

/// AC#1-style bring-up budget: discovery + classification + harness live
/// within 30s (the same headless budget the v0 `watch` path asserts).
const BRINGUP_BUDGET: Duration = Duration::from_secs(30);

/// CLI surface for `serve`, kept plain (mirrors `main::Opts`): the parser
/// fills this, [`run`] maps it to the frozen [`FleetOverrides`] injection
/// struct. No clap types cross into `cargoless-core` (the frozen-contract
/// boundary — core never gains an arg dep).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ServeOpts {
    /// `--repo <path>` — repo root for daemon mode (required: without a
    /// repo there is no fleet to serve).
    pub repo: Option<PathBuf>,
    /// `--bind HOST:PORT` — network transport (Stream E #10 binds it;
    /// #3 only resolves+carries it). `None` ⇒ no network exposure.
    pub bind: Option<String>,
    /// `--no-corun` — disable corun batching (design §7). Maps to
    /// `FleetOverrides.corun = Some(false)`.
    pub no_corun: bool,
    /// `--cas-dir <path>` — shared CAS directory (fleet dedup).
    pub cas_dir: Option<PathBuf>,
    /// `--state-dir <path>` — state/cache root override.
    pub state_dir: Option<PathBuf>,
    /// `--auth-token <secret>` — bearer token for authed HTTP (#14
    /// enforces; #3 only carries). Prefer the env over a flag for
    /// secrets; the flag exists for completeness/parity with the
    /// frozen contract.
    pub auth_token: Option<String>,
}

impl ServeOpts {
    /// Map the CLI surface to the frozen [`FleetOverrides`]. Pure — the
    /// flag→injection-struct boundary in one tested place (the same
    /// shape `cargoless_core::config::overrides_from_map` documents for
    /// the string path).
    pub fn to_overrides(&self) -> FleetOverrides {
        FleetOverrides {
            cas_dir: self.cas_dir.clone(),
            state_dir: self.state_dir.clone(),
            repo: self.repo.clone(),
            bind: self.bind.clone(),
            // `--no-corun` ⇒ Some(false); absent ⇒ None (fall through to
            // env/tf.toml/default). `Some(true)` is reserved for a future
            // explicit `--corun` (per the frozen FleetOverrides doc).
            corun: if self.no_corun { Some(false) } else { None },
            auth_token: self.auth_token.clone(),
        }
    }
}

/// `serve` entry. Resolve fleet config, require daemon mode, discover the
/// fleet, run the lifecycle harness. Exit codes mirror the rest of the
/// CLI: 0 = clean shutdown, 2 = setup/config error.
pub fn run(opts: &ServeOpts) -> ExitCode {
    let t0 = Instant::now();

    // The repo root is required for daemon mode. We resolve config rooted
    // at it so the repo's own tf.toml `[fleet]`/`[project]` keys layer in
    // (default < tf.toml < env < CLI — the frozen precedence).
    let Some(repo_root) = opts.repo.clone() else {
        ui::error(
            "serve needs a repo root.\n  \
             usage: cargoless serve --repo <path>\n  \
             (or set `TF_REPO` / `[fleet] repo` in tf.toml).",
        );
        return ExitCode::from(2);
    };

    let fleet = match FleetConfig::resolve(&repo_root, opts.to_overrides()) {
        Ok(f) => f,
        Err(e) => {
            ui::error(format!("fleet config: {e}"));
            return ExitCode::from(2);
        }
    };

    // #14 pre-flight: a non-loopback bind without a token is an unsafe
    // network exposure. The frozen contract froze the check + message;
    // #3 wires it into the daemon startup path (the enforcement seam #14
    // owns is "post-Stream-E-#10 transport"; refusing to *start* an
    // unauthenticated network daemon is correct here and cheap).
    if let Err(e) = fleet.security_check() {
        ui::error(format!("refusing to start: {e}"));
        return ExitCode::from(2);
    }

    let scope = match RepoScope::discover(fleet) {
        Ok(s) => s,
        Err(e) => {
            ui::error(e.to_string());
            return ExitCode::from(2);
        }
    };

    banner(&scope, t0);

    // ---- the serve-loop skeleton -------------------------------------
    //
    // #3 harness: hold the process, shut down cleanly. The two seams:
    //   • #4  → replace the park with the routed (WtId, ChangeBatch)
    //           file-watcher stream (gitignore-inversion).
    //   • #5/#6 (Stream C) → per routed change, drive
    //           scope.worktree_config(wt, env) into the one-RA overlay
    //           multiplexer / cluster manager for a per-WT verdict.
    //
    // Until #4, nothing drives a check; parking is the honest behaviour
    // (discovered + classified + reported, holding the daemon — claims
    // nothing it cannot substantiate).
    //
    // Shutdown scope (honest #3 boundary): the #3 harness holds NO
    // rust-analyzer and NO per-worktree state (those arrive with Stream C
    // / #12). There is therefore nothing to gracefully tear down on
    // Ctrl-C — OS-default SIGINT/SIGTERM termination is correct here. A
    // custom signal handler + graceful teardown is a #12 (activity) / #13
    // (crash+restart) concern and lands when there IS resident state to
    // reclaim. Parent-orphan IS guarded now (FIELD-FINDING-#13b parity):
    // one daemon for the whole fleet makes an orphaned `serve &` strictly
    // worse than the v0 single-watch case, so the proven `orphan` guard
    // the v0 loop trusts is wired in from #3.
    let parent = crate::orphan::ParentWatch::capture();

    // #246 Wave-1 5a — telemetry init + ordered shutdown around the serve
    // loop. Resolved from env (OTEL_EXPORTER_OTLP_ENDPOINT etc.) + repo's
    // tf.toml `[telemetry]` overlay. The tokio runtime substrate is owned
    // here (NOT created inside init_telemetry) so the runtime's lifetime
    // brackets the serve loop — exporter batch tasks live as long as the
    // daemon does. When `enabled() == false` (no endpoint), the runtime
    // is still created but the telemetry init returns inert; cost is one
    // idle tokio thread for the daemon's lifetime, negligible.
    let tcfg = match TelemetryConfig::resolve(&repo_root, TelemetryOverrides::default()) {
        Ok(c) => c,
        Err(e) => {
            // Telemetry config errors are non-fatal — log + continue with
            // disabled defaults. The fail-soft contract: telemetry MUST
            // NOT wedge the daemon, not even at config-resolution time.
            eprintln!(
                "[cargoless:telemetry] config error ({e}); continuing \
                 with telemetry disabled."
            );
            TelemetryConfig::defaults()
        }
    };
    run_with_telemetry(scope, &parent, &tcfg)
}

/// Wrap `servedrv::run` in a tokio runtime + telemetry init/shutdown.
/// Extracted so the runtime lifetime is explicit + telemetry shutdown
/// fires BEFORE any orphan-reap path (5f ordered-flush requirement).
#[cfg(feature = "telemetry")]
fn run_with_telemetry(
    scope: RepoScope,
    parent: &crate::orphan::ParentWatch,
    tcfg: &TelemetryConfig,
) -> ExitCode {
    // Multi-thread runtime so the OTel SDK's batch exporter tasks can
    // run independently of the daemon's main thread (which blocks on
    // std::sync::mpsc::recv_timeout — a tokio current-thread runtime
    // would be starved by that). Default worker count = std::thread
    // count, which is fine for a daemon binary.
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!(
                "[cargoless:telemetry] tokio runtime init failed ({e}); \
                 continuing without OTEL export."
            );
            return crate::servedrv::run(scope, parent);
        }
    };
    let _guard = runtime.enter();
    // capstone-wire: live Model R driver. See servedrv.rs header for the
    // honest verification-boundary statement (cores-structurally-proven
    // + integration-validated-downstream via #15/Track-1).
    let telemetry = crate::telemetry::init_telemetry(tcfg);
    let code = crate::servedrv::run(scope, parent);
    // Ordered flush BEFORE any orphan-reap path (5f). Telemetry shutdown
    // is best-effort — it must never block return on a slow collector.
    crate::telemetry::shutdown_telemetry(telemetry);
    drop(_guard);
    drop(runtime); // tokio runtime drops + batch tasks finalize.
    code
}

#[cfg(not(feature = "telemetry"))]
fn run_with_telemetry(
    scope: RepoScope,
    parent: &crate::orphan::ParentWatch,
    _tcfg: &TelemetryConfig,
) -> ExitCode {
    // Feature compiled out — no runtime, no telemetry, just the serve
    // loop. Path exists so the binary still ships under
    // `--no-default-features` builds.
    crate::servedrv::run(scope, parent)
}

/// §3.3 bring-up banner: one process, the discovered+classified topology,
/// the AC#1-style budget line. Honest — it reports what was discovered,
/// not a verdict (there is none yet at #3).
fn banner(scope: &RepoScope, t0: Instant) {
    let (mut main, mut nested, mut sibling, mut other) = (0u32, 0u32, 0u32, 0u32);
    for (class, _) in scope.classified() {
        match class {
            WtClass::Main => main += 1,
            WtClass::Nested => nested += 1,
            WtClass::Sibling => sibling += 1,
            WtClass::Other => other += 1,
        }
    }
    let total = scope.worktrees.len();
    ui::ok(format!(
        "repo-scoped daemon — {} ({} worktrees: {} main, {} nested, \
         {} sibling, {} other)",
        scope.repo_root.display(),
        total,
        main,
        nested,
        sibling,
        other
    ));
    let st = scope.fleet.state_dir.display().to_string();
    let cas = scope
        .fleet
        .cas_dir
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(per-process, no fleet dedup)".to_string());
    ui::ok(format!(
        "state_dir={st}  cas_dir={cas}  corun={}",
        scope.fleet.corun
    ));
    let bringup = t0.elapsed();
    if bringup <= BRINGUP_BUDGET {
        ui::ok(format!(
            "discovery+classification live in {:.2}s (AC#1 budget {}s) — \
             headless harness; checks land with #4 watcher + Stream C",
            bringup.as_secs_f64(),
            BRINGUP_BUDGET.as_secs()
        ));
    } else {
        ui::warn(format!(
            "bring-up took {:.2}s, over the {}s AC#1 budget — investigate.",
            bringup.as_secs_f64(),
            BRINGUP_BUDGET.as_secs()
        ));
    }
}

/// Ctrl-C → set a stop flag the serve-loop polls. std-only (no `signal`
/// crate — house dependency-minimal policy); the handler just flips an
/// `AtomicBool`, which is async-signal-safe.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opts_map_to_frozen_overrides() {
        let o = ServeOpts {
            repo: Some(PathBuf::from("/repo")),
            bind: Some("127.0.0.1:8080".into()),
            no_corun: true,
            cas_dir: Some(PathBuf::from("/shared/cas")),
            state_dir: Some(PathBuf::from(".triform/cargoless")),
            auth_token: Some("sekret".into()),
        };
        let ov = o.to_overrides();
        assert_eq!(ov.repo, Some(PathBuf::from("/repo")));
        assert_eq!(ov.bind.as_deref(), Some("127.0.0.1:8080"));
        assert_eq!(ov.corun, Some(false), "--no-corun ⇒ Some(false)");
        assert_eq!(ov.cas_dir, Some(PathBuf::from("/shared/cas")));
        assert_eq!(ov.state_dir, Some(PathBuf::from(".triform/cargoless")));
        assert_eq!(ov.auth_token.as_deref(), Some("sekret"));
    }

    #[test]
    fn corun_absent_is_none_not_some_true() {
        // The frozen-contract subtlety: no `--no-corun` ⇒ None (fall
        // through to env/tf.toml/default true), NOT Some(true) — only a
        // future explicit `--corun` sets Some(true).
        let o = ServeOpts {
            repo: Some(PathBuf::from("/r")),
            ..Default::default()
        };
        assert_eq!(o.to_overrides().corun, None);
    }

    #[test]
    fn no_repo_is_a_setup_error() {
        // serve without --repo must exit 2 (setup error), never panic or
        // silently no-op — a daemon's config error is its onboarding UX.
        let code = run(&ServeOpts::default());
        assert_eq!(code, ExitCode::from(2));
    }
}
