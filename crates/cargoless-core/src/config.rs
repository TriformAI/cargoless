//! Model R fleet/daemon configuration — the **Stream A↔B seam**.
//!
//! ## Why this lives in `cargoless-core` (the ratified A↔B decision)
//!
//! cargoless v0 config is CLI-crate-local (`crates/cargoless/src/config.rs`:
//! single-root project `Config` + `detect_from_cargo_toml`, the house
//! tf.toml pattern). Model R adds a *fleet/daemon* dimension
//! (`--cas-dir/--state-dir/--repo/--bind/--no-corun/--auth-token`) that
//! `cargoless-core` itself consumes — `repo.rs` (Stream B), `cluster.rs`,
//! `corun.rs`, `transport/` all need the resolved values, with **no CLI in
//! the loop** for daemon-runtime re-resolution (per-worktree `tf.toml`
//! `state_dir` overrides resolved while the daemon runs).
//!
//! The lead's recommendation (CLI parses, core consumes a resolved struct
//! via injection) is **ratified on dependency direction** and **refined on
//! resolver placement**:
//!
//! - **`cargoless-core` owns the resolved type [`FleetConfig`] AND the
//!   clap-free precedence resolver** ([`FleetConfig::resolve`]). It takes a
//!   plain [`FleetOverrides`] struct — **never a clap type**. Core gains no
//!   arg-parsing dep and no dependency on the CLI crate; the
//!   `core ← cli` direction is intact and core stays unit-testable without
//!   clap. The bug-prone parts (precedence, the tolerant-overlay tf.toml
//!   reader) live in ONE place, exhaustively unit-tested here, and are
//!   reusable by the daemon-runtime re-resolution path that has no CLI.
//! - **The CLI crate owns only the clap flag surface.** It maps flags into
//!   [`FleetOverrides`] and calls [`FleetConfig::resolve`]. It does not
//!   re-implement env/`tf.toml` parsing or the precedence rule.
//!
//! Net: the lead's three constraints (no circular dep, core clap-free, core
//! unit-testable) are all satisfied, and precedence logic is singular +
//! testable + reusable by the daemon. This struct shape is the **frozen
//! contract** Stream B codes `repo.rs` against.
//!
//! ## Precedence
//!
//! `CLI flag  >  environment  >  tf.toml  >  built-in default`
//!
//! ## Backward-compat (every default is v0 behaviour, unchanged)
//!
//! | Field        | Default            | v0 meaning preserved                       |
//! |--------------|--------------------|--------------------------------------------|
//! | `cas_dir`    | `None`             | per-process PID-scoped CAS (no fleet share) |
//! | `state_dir`  | `.cargoless`       | the existing v0 state directory             |
//! | `repo`       | `None`             | single-worktree mode; no daemon             |
//! | `bind`       | `None`             | no network transport bound                  |
//! | `corun`      | `true`             | (only meaningful once `repo` is set)        |
//! | `auth_token` | `None`             | (only meaningful once `bind` is non-loopback) |
//!
//! A v0 invocation with no flags / no `[fleet]` `tf.toml` resolves to
//! exactly today's behaviour — verified by [`tests::defaults_are_v0`].
//!
//! ## Hand-rolled, serde-free
//!
//! No `toml`/`serde`/`clap` dep — matches the CLI `config.rs` precedent and
//! `CLAUDE.md` (no new deps; keep the cold-build path AC#1/#2 measure
//! lean). The `tf.toml` reader here is a **tolerant partial overlay**: it
//! reads only the keys it owns and *ignores everything else*. This is the
//! deliberate opposite of the CLI `Config` reader, which hard-errors on any
//! unknown section/key. Both are correct for their ownership scope — the
//! CLI reader owns `[project] root/target` + `[cache] dir` and must catch
//! typos there; this reader is a partial view of the *same shared file* and
//! must never reject keys it does not own (doing so would break every
//! existing v0 `tf.toml`).

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// The v0 default state directory (relative to the project/repo root).
pub const DEFAULT_STATE_DIR: &str = ".cargoless";

/// Which precedence layer set a given field — surfaced for diagnostics,
/// `--version`-style introspection, and three-layer-validation evidence.
/// (Mirrors the intent of the CLI `Config`'s `Detection`: the codebase
/// should always be able to say *why* it is configured the way it is.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Built-in v0-compatible default (no override anywhere).
    Default,
    /// Set by a `[fleet]`/`[project]`/`[cache]` key in `tf.toml`.
    TfToml,
    /// Set by a `TF_*` / `CARGOLESS_*` environment variable.
    Env,
    /// Set by an explicit CLI flag.
    Cli,
}

impl Source {
    pub fn describe(self) -> &'static str {
        match self {
            Source::Default => "default (v0-compatible)",
            Source::TfToml => "tf.toml",
            Source::Env => "environment",
            Source::Cli => "CLI flag",
        }
    }
}

/// Per-field provenance. Cheap, `Copy`, and load-bearing for the
/// "codebase always knows what it is" vision cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provenance {
    pub cas_dir: Source,
    pub state_dir: Source,
    pub repo: Source,
    pub bind: Source,
    pub corun: Source,
    pub auth_token: Source,
}

impl Default for Provenance {
    fn default() -> Self {
        Self {
            cas_dir: Source::Default,
            state_dir: Source::Default,
            repo: Source::Default,
            bind: Source::Default,
            corun: Source::Default,
            auth_token: Source::Default,
        }
    }
}

/// CLI-supplied overrides — the **injection struct** the CLI crate fills
/// from clap and hands to [`FleetConfig::resolve`]. Deliberately plain
/// (`Option`-of-value, no clap types) so `cargoless-core` never gains an
/// arg-parsing dependency.
///
/// `corun` is `Option<bool>`: `None` = flag absent (fall through to
/// env/toml/default); `Some(false)` = `--no-corun` was passed;
/// `Some(true)` is reserved for a future explicit `--corun`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetOverrides {
    pub cas_dir: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub repo: Option<PathBuf>,
    pub bind: Option<String>,
    pub corun: Option<bool>,
    pub auth_token: Option<String>,
}

/// Fully-resolved Model R fleet configuration. Every field is populated
/// after [`FleetConfig::resolve`]; consumers in `cargoless-core`
/// (`repo.rs`, `cluster.rs`, `corun.rs`, `transport/`) read it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetConfig {
    /// Shared content-addressed CAS directory. `None` ⇒ v0 per-process
    /// PID-scoped CAS (no fleet dedup) — the unchanged v0 behaviour.
    pub cas_dir: Option<PathBuf>,
    /// State/cache directory (cli-status, tree.cache, diagnostics). v0
    /// default `.cargoless`; tf-multiverse sets `.triform/cargoless`.
    pub state_dir: PathBuf,
    /// Repo root for daemon mode (`serve --repo <path>`). `None` ⇒
    /// single-worktree v0 mode; no daemon, no worktree discovery.
    pub repo: Option<PathBuf>,
    /// Network bind address for the HTTP+SSE transport. `None` ⇒ no
    /// network transport (in-proc / Unix-socket only) — the safe default.
    pub bind: Option<SocketAddr>,
    /// Corun batching enabled (design §7). Default `true`; only takes
    /// effect once `repo` is set (multi-worktree).
    pub corun: bool,
    /// Bearer token for authenticated HTTP mode (#14). `None` ⇒ no auth.
    /// Prefer the `CARGOLESS_AUTH_TOKEN` env over `tf.toml` for secrets.
    pub auth_token: Option<String>,
    /// Per-field provenance (which layer won).
    pub provenance: Provenance,
}

/// Configuration failure. Like the CLI `ConfigError`, every variant renders
/// one actionable message — a daemon's config error is its onboarding UX.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetConfigError {
    BadTfToml {
        line_no: usize,
        line: String,
        why: String,
    },
    BadBind {
        value: String,
        why: String,
    },
    BadBool {
        origin: &'static str,
        key: String,
        value: String,
    },
}

impl fmt::Display for FleetConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FleetConfigError::BadTfToml { line_no, line, why } => write!(
                f,
                "tf.toml: {why} (line {line_no}: `{line}`).\n  \
                 [fleet] keys: repo, bind, corun, auth_token. \
                 [project] state_dir. [cache] cas_dir."
            ),
            FleetConfigError::BadBind { value, why } => write!(
                f,
                "invalid bind address `{value}`: {why}.\n  \
                 expected `HOST:PORT`, e.g. `127.0.0.1:8080` (loopback, \
                 safe) or `0.0.0.0:8080` (network — requires --auth-token)."
            ),
            FleetConfigError::BadBool { origin, key, value } => write!(
                f,
                "{origin}: `{key}` expects a boolean (`true`/`false`), \
                 got `{value}`."
            ),
        }
    }
}

impl std::error::Error for FleetConfigError {}

impl FleetConfig {
    /// The all-defaults config = exact v0 behaviour.
    pub fn defaults() -> Self {
        Self {
            cas_dir: None,
            state_dir: PathBuf::from(DEFAULT_STATE_DIR),
            repo: None,
            bind: None,
            corun: true,
            auth_token: None,
            provenance: Provenance::default(),
        }
    }

    /// Resolve fleet config for `repo_root`, layering
    /// `default < tf.toml < env < CLI`. Reads the process environment via
    /// `std::env::var`; see [`FleetConfig::resolve_layered`] for the
    /// env-injected (unit-testable) variant.
    pub fn resolve(
        repo_root: impl AsRef<Path>,
        overrides: FleetOverrides,
    ) -> Result<Self, FleetConfigError> {
        let env = |k: &str| std::env::var(k).ok();
        Self::resolve_layered(repo_root.as_ref(), overrides, &env)
    }

    /// Env-injected resolver core. `env` is the only IO seam (so the
    /// precedence + tf.toml-overlay logic is pure and exhaustively
    /// unit-testable without touching the process environment).
    pub fn resolve_layered(
        repo_root: &Path,
        ov: FleetOverrides,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, FleetConfigError> {
        let mut cfg = Self::defaults();

        // ---- layer 1: tf.toml (tolerant partial overlay) -------------
        if let Ok(text) = std::fs::read_to_string(repo_root.join("tf.toml")) {
            apply_tf_toml_overlay(&mut cfg, &text)?;
        }

        // ---- layer 2: environment ------------------------------------
        if let Some(v) = env("TF_CAS_DIR").filter(|s| !s.is_empty()) {
            cfg.cas_dir = Some(PathBuf::from(v));
            cfg.provenance.cas_dir = Source::Env;
        }
        if let Some(v) = env("TF_STATE_DIR").filter(|s| !s.is_empty()) {
            cfg.state_dir = PathBuf::from(v);
            cfg.provenance.state_dir = Source::Env;
        }
        if let Some(v) = env("TF_REPO").filter(|s| !s.is_empty()) {
            cfg.repo = Some(PathBuf::from(v));
            cfg.provenance.repo = Source::Env;
        }
        if let Some(v) = env("TF_BIND").filter(|s| !s.is_empty()) {
            cfg.bind = Some(parse_bind(&v)?);
            cfg.provenance.bind = Source::Env;
        }
        if let Some(v) = env("TF_NO_CORUN").filter(|s| !s.is_empty()) {
            // presence/truthy ⇒ disable corun.
            if parse_bool("env TF_NO_CORUN", "TF_NO_CORUN", &v)? {
                cfg.corun = false;
                cfg.provenance.corun = Source::Env;
            }
        }
        if let Some(v) = env("CARGOLESS_AUTH_TOKEN").filter(|s| !s.trim().is_empty()) {
            cfg.auth_token = Some(v);
            cfg.provenance.auth_token = Source::Env;
        }

        // ---- layer 3: explicit CLI flags (highest) -------------------
        if let Some(v) = ov.cas_dir {
            cfg.cas_dir = Some(v);
            cfg.provenance.cas_dir = Source::Cli;
        }
        if let Some(v) = ov.state_dir {
            cfg.state_dir = v;
            cfg.provenance.state_dir = Source::Cli;
        }
        if let Some(v) = ov.repo {
            cfg.repo = Some(v);
            cfg.provenance.repo = Source::Cli;
        }
        if let Some(v) = ov.bind {
            cfg.bind = Some(parse_bind(&v)?);
            cfg.provenance.bind = Source::Cli;
        }
        if let Some(b) = ov.corun {
            cfg.corun = b;
            cfg.provenance.corun = Source::Cli;
        }
        if let Some(v) = ov.auth_token.filter(|s| !s.trim().is_empty()) {
            cfg.auth_token = Some(v);
            cfg.provenance.auth_token = Source::Cli;
        }

        Ok(cfg)
    }

    /// `true` once a repo root is set ⇒ run the repo-scoped daemon
    /// (`serve --repo`). Stream B's `repo.rs` gates on this.
    pub fn daemon_mode(&self) -> bool {
        self.repo.is_some()
    }

    /// `true` if the bind address is non-loopback — i.e. reachable off-host
    /// and therefore MUST carry auth. This is the **#14 enforcement hook**:
    /// `parse` + this predicate land in #1; the daemon-side
    /// reject-non-loopback-without-token enforcement lands in #14 (after
    /// Stream E #10 transport). Defined here so the contract is frozen and
    /// Stream E can depend on the predicate now.
    pub fn requires_auth(&self) -> bool {
        match self.bind {
            Some(addr) => !addr.ip().is_loopback(),
            None => false,
        }
    }

    /// The auth token iff one is **effectively** present — `Some(secret)`
    /// only when configured AND non-blank (not empty, not whitespace-only).
    /// A blank token is treated as **absent**: the real invariant the
    /// security policy models is "an effective shared secret exists", not
    /// the `Option::is_some` proxy. CWDL #197 — `--auth-token ""` /
    /// `[fleet] auth_token = ""` / `CARGOLESS_AUTH_TOKEN=" "` must NOT
    /// yield an unauthenticated non-loopback socket. All three config
    /// sources also reject a blank token at parse time; this is the single
    /// consulted predicate (used by [`security_check`](Self::security_check)
    /// and `transport::authorizer_for`) so no current/future path can
    /// reintroduce a blank effective secret.
    pub fn effective_auth_token(&self) -> Option<&str> {
        self.auth_token.as_deref().filter(|t| !t.trim().is_empty())
    }

    /// #14 pre-flight: non-loopback bind without an auth token is an unsafe
    /// network exposure. Inert until #14 wires it into the daemon startup
    /// path; provided now so the contract + message are frozen.
    pub fn security_check(&self) -> Result<(), FleetConfigError> {
        if self.requires_auth() && self.effective_auth_token().is_none() {
            let value = self.bind.map(|a| a.to_string()).unwrap_or_default();
            return Err(FleetConfigError::BadBind {
                value,
                why: "non-loopback bind requires --auth-token / \
                      CARGOLESS_AUTH_TOKEN (refusing unauthenticated \
                      network exposure)"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// State directory resolved against `repo_root` (absolute if the
    /// configured `state_dir` is relative — the v0 default `.cargoless`
    /// is relative by design).
    pub fn state_dir_abs(&self, repo_root: &Path) -> PathBuf {
        if self.state_dir.is_absolute() {
            self.state_dir.clone()
        } else {
            repo_root.join(&self.state_dir)
        }
    }
}

/// Parse a `HOST:PORT` bind string into a `SocketAddr`.
fn parse_bind(s: &str) -> Result<SocketAddr, FleetConfigError> {
    SocketAddr::from_str(s.trim()).map_err(|e| FleetConfigError::BadBind {
        value: s.to_string(),
        why: e.to_string(),
    })
}

/// Parse a permissive boolean (`true/false/1/0/yes/no/on/off`,
/// case-insensitive). Used for `[fleet] corun` and `TF_NO_CORUN`.
fn parse_bool(origin: &'static str, key: &str, v: &str) -> Result<bool, FleetConfigError> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(FleetConfigError::BadBool {
            origin,
            key: key.to_string(),
            value: v.to_string(),
        }),
    }
}

/// Apply the **fleet-owned** keys from a (shared) `tf.toml` over `cfg`.
///
/// Tolerant by contract: unknown sections/keys are *ignored*, not rejected
/// — the CLI `Config` reader owns strict validation of `[project]
/// root/target` + `[cache] dir`; this is a partial view of the same file
/// and must not reject keys outside its ownership. Only the *values it
/// owns* are validated (bad bind / bad bool ⇒ hard error).
///
/// Owned keys:
/// - `[project] state_dir = "<path>"`
/// - `[cache]   cas_dir   = "<path>"`
/// - `[fleet]   repo      = "<path>"`
/// - `[fleet]   bind      = "HOST:PORT"`
/// - `[fleet]   corun     = true|false`
/// - `[fleet]   auth_token = "<secret>"` (discouraged; prefer env)
fn apply_tf_toml_overlay(cfg: &mut FleetConfig, text: &str) -> Result<(), FleetConfigError> {
    let mut section = String::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_string();
            continue; // tolerant: never reject an unknown section.
        }
        let Some((key, val)) = line.split_once('=') else {
            continue; // tolerant: malformed non-owned line ⇒ ignore.
        };
        let key = key.trim();
        let val = unquote(val.trim());
        match (section.as_str(), key) {
            ("project", "state_dir") => {
                cfg.state_dir = PathBuf::from(&val);
                cfg.provenance.state_dir = Source::TfToml;
            }
            ("cache", "cas_dir") => {
                cfg.cas_dir = Some(PathBuf::from(&val));
                cfg.provenance.cas_dir = Source::TfToml;
            }
            ("fleet", "repo") => {
                cfg.repo = Some(PathBuf::from(&val));
                cfg.provenance.repo = Source::TfToml;
            }
            ("fleet", "bind") => {
                cfg.bind = Some(parse_bind(&val).map_err(|_| FleetConfigError::BadTfToml {
                    line_no,
                    line: raw.trim().to_string(),
                    why: format!("invalid bind address `{val}`"),
                })?);
                cfg.provenance.bind = Source::TfToml;
            }
            ("fleet", "corun") => {
                let b = parse_bool("tf.toml", "corun", &val).map_err(|_| {
                    FleetConfigError::BadTfToml {
                        line_no,
                        line: raw.trim().to_string(),
                        why: format!("`corun` expects true/false, got `{val}`"),
                    }
                })?;
                cfg.corun = b;
                cfg.provenance.corun = Source::TfToml;
            }
            // Blank (empty / whitespace-only) ⇒ NOT a token: falls to the
            // tolerant `_` arm, uniform with the env + CLI paths (CWDL
            // #197 — `[fleet] auth_token = ""` must not yield an
            // unauthenticated non-loopback socket).
            ("fleet", "auth_token") if !val.trim().is_empty() => {
                cfg.auth_token = Some(val);
                cfg.provenance.auth_token = Source::TfToml;
            }
            // Tolerant: any other (section,key) — incl. a blank
            // `auth_token` (handled above) — belongs to the CLI `Config`
            // reader or a future consumer; ignore silently.
            _ => {}
        }
    }
    Ok(())
}

/// Strip a `#` comment, respecting `#` inside a double-quoted string.
/// (Same rule as the CLI `config.rs` — kept identical so the two readers
/// over the same file never disagree on what a comment is.)
fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

fn unquote(s: &str) -> String {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}

/// Build a [`FleetOverrides`] from an already-collected string map — a
/// convenience for the CLI crate / tests that have flag values as strings.
/// Not used by core itself; keeps the string→typed boundary in one place.
pub fn overrides_from_map(m: &BTreeMap<String, String>) -> FleetOverrides {
    FleetOverrides {
        cas_dir: m.get("cas-dir").map(PathBuf::from),
        state_dir: m.get("state-dir").map(PathBuf::from),
        repo: m.get("repo").map(PathBuf::from),
        bind: m.get("bind").cloned(),
        corun: if m.contains_key("no-corun") {
            Some(false)
        } else {
            None
        },
        auth_token: m.get("auth-token").cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_are_v0() {
        // The whole backward-compat guarantee in one assertion: no flags,
        // no env, no tf.toml ⇒ exactly today's v0 behaviour.
        let c = FleetConfig::defaults();
        assert_eq!(c.cas_dir, None, "v0: per-process PID-scoped CAS");
        assert_eq!(c.state_dir, PathBuf::from(".cargoless"));
        assert_eq!(c.repo, None, "v0: no daemon mode");
        assert_eq!(c.bind, None, "v0: no network transport");
        assert!(c.corun, "corun default-on (inert until repo set)");
        assert_eq!(c.auth_token, None);
        assert!(!c.daemon_mode());
        assert!(!c.requires_auth());
        assert!(c.security_check().is_ok());
    }

    #[test]
    fn resolve_no_inputs_equals_defaults() {
        let tmp = std::env::temp_dir().join("cl-cfg-empty-xyz");
        let _ = std::fs::create_dir_all(&tmp);
        let c = FleetConfig::resolve_layered(&tmp, FleetOverrides::default(), &no_env).unwrap();
        assert_eq!(c, FleetConfig::defaults());
    }

    #[test]
    fn precedence_cli_beats_env_beats_toml_beats_default() {
        let dir = std::env::temp_dir().join(format!("cl-cfg-prec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tf.toml"),
            "[project]\nstate_dir = \".triform/cargoless\"\n\
             [fleet]\nrepo = \"/from/toml\"\ncorun = false\n",
        )
        .unwrap();

        // toml-only: state_dir + repo + corun come from tf.toml.
        let c = FleetConfig::resolve_layered(&dir, FleetOverrides::default(), &no_env).unwrap();
        assert_eq!(c.state_dir, PathBuf::from(".triform/cargoless"));
        assert_eq!(c.provenance.state_dir, Source::TfToml);
        assert_eq!(c.repo, Some(PathBuf::from("/from/toml")));
        assert!(!c.corun);
        assert_eq!(c.provenance.corun, Source::TfToml);

        // env overrides toml for repo.
        let env = |k: &str| match k {
            "TF_REPO" => Some("/from/env".to_string()),
            _ => None,
        };
        let c = FleetConfig::resolve_layered(&dir, FleetOverrides::default(), &env).unwrap();
        assert_eq!(c.repo, Some(PathBuf::from("/from/env")));
        assert_eq!(c.provenance.repo, Source::Env);
        // state_dir still from toml (env didn't touch it).
        assert_eq!(c.state_dir, PathBuf::from(".triform/cargoless"));

        // CLI overrides everything for repo.
        let ov = FleetOverrides {
            repo: Some(PathBuf::from("/from/cli")),
            ..Default::default()
        };
        let c = FleetConfig::resolve_layered(&dir, ov, &env).unwrap();
        assert_eq!(c.repo, Some(PathBuf::from("/from/cli")));
        assert_eq!(c.provenance.repo, Source::Cli);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tf_toml_overlay_is_tolerant_of_cli_owned_keys() {
        // A realistic v0 tf.toml: [project] root/target + [cache] dir are
        // owned by the CLI Config reader. The fleet overlay MUST ignore
        // them (not hard-error) while still reading its own state_dir.
        let dir = std::env::temp_dir().join(format!("cl-cfg-tol-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tf.toml"),
            "[project]\nroot = \"/proj\"\ntarget = \"wasm32-unknown-unknown\"\n\
             state_dir = \".triform/cargoless\"\n\
             [cache]\ndir = \"/tmp/cache\"\ncas_dir = \"/shared/cas\"\n\
             [serve]\nport = 8080\n",
        )
        .unwrap();
        let c = FleetConfig::resolve_layered(&dir, FleetOverrides::default(), &no_env).unwrap();
        // owned keys read:
        assert_eq!(c.state_dir, PathBuf::from(".triform/cargoless"));
        assert_eq!(c.cas_dir, Some(PathBuf::from("/shared/cas")));
        // non-owned keys ([project] root/target, [cache] dir, [serve])
        // ignored — no error, defaults untouched:
        assert_eq!(c.repo, None);
        assert_eq!(c.bind, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bind_parsing_and_auth_predicate() {
        // loopback bind needs no auth.
        let ov = FleetOverrides {
            bind: Some("127.0.0.1:8080".to_string()),
            ..Default::default()
        };
        let c = FleetConfig::resolve_layered(std::path::Path::new("/nonexistent"), ov, &no_env)
            .unwrap();
        assert_eq!(c.bind.unwrap().to_string(), "127.0.0.1:8080");
        assert!(!c.requires_auth());
        assert!(c.security_check().is_ok());

        // non-loopback bind requires auth — security_check rejects.
        let ov = FleetOverrides {
            bind: Some("0.0.0.0:8080".to_string()),
            ..Default::default()
        };
        let c = FleetConfig::resolve_layered(std::path::Path::new("/nonexistent"), ov, &no_env)
            .unwrap();
        assert!(c.requires_auth());
        assert!(c.security_check().is_err());

        // …with a token it passes.
        let ov = FleetOverrides {
            bind: Some("0.0.0.0:8080".to_string()),
            auth_token: Some("s3cr3t".to_string()),
            ..Default::default()
        };
        let c = FleetConfig::resolve_layered(std::path::Path::new("/nonexistent"), ov, &no_env)
            .unwrap();
        assert!(c.security_check().is_ok());
    }

    #[test]
    fn bad_bind_is_actionable() {
        let ov = FleetOverrides {
            bind: Some("not-an-addr".to_string()),
            ..Default::default()
        };
        let e = FleetConfig::resolve_layered(std::path::Path::new("/nonexistent"), ov, &no_env)
            .unwrap_err();
        assert!(matches!(e, FleetConfigError::BadBind { .. }));
        assert!(e.to_string().contains("--auth-token"));
    }

    #[test]
    fn no_corun_via_env_and_cli() {
        // env TF_NO_CORUN=1 disables corun.
        let env = |k: &str| (k == "TF_NO_CORUN").then(|| "1".to_string());
        let c = FleetConfig::resolve_layered(
            std::path::Path::new("/nonexistent"),
            FleetOverrides::default(),
            &env,
        )
        .unwrap();
        assert!(!c.corun);
        assert_eq!(c.provenance.corun, Source::Env);

        // --no-corun (Some(false)) wins over env-unset default.
        let ov = FleetOverrides {
            corun: Some(false),
            ..Default::default()
        };
        let c = FleetConfig::resolve_layered(std::path::Path::new("/nonexistent"), ov, &no_env)
            .unwrap();
        assert!(!c.corun);
        assert_eq!(c.provenance.corun, Source::Cli);
    }

    #[test]
    fn auth_token_prefers_env_over_toml() {
        let dir = std::env::temp_dir().join(format!("cl-cfg-tok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tf.toml"), "[fleet]\nauth_token = \"from-toml\"\n").unwrap();
        let env = |k: &str| (k == "CARGOLESS_AUTH_TOKEN").then(|| "from-env".to_string());
        let c = FleetConfig::resolve_layered(&dir, FleetOverrides::default(), &env).unwrap();
        assert_eq!(c.auth_token.as_deref(), Some("from-env"));
        assert_eq!(c.provenance.auth_token, Source::Env);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ───────── CWDL #197: blank auth_token is NOT a token ─────────

    #[test]
    fn blank_auth_token_is_no_token_via_cli_env_toml() {
        // Empty AND whitespace-only, every source ⇒ parsed as no token
        // (uniform with the env path's long-standing empty-filter).
        for blank in ["", "   ", "\t", " \n "] {
            // CLI
            let ov = FleetOverrides {
                auth_token: Some(blank.to_string()),
                ..FleetOverrides::default()
            };
            let c = FleetConfig::resolve_layered(std::path::Path::new("/nonexistent"), ov, &no_env)
                .unwrap();
            assert_eq!(c.auth_token, None, "CLI blank {blank:?} ⇒ no token");
            assert_eq!(c.effective_auth_token(), None);

            // env
            let env = |k: &str| (k == "CARGOLESS_AUTH_TOKEN").then(|| blank.to_string());
            let c = FleetConfig::resolve_layered(
                std::path::Path::new("/nonexistent"),
                FleetOverrides::default(),
                &env,
            )
            .unwrap();
            assert_eq!(c.auth_token, None, "env blank {blank:?} ⇒ no token");
            assert_eq!(c.effective_auth_token(), None);

            // tf.toml
            let dir = std::env::temp_dir().join(format!(
                "cl-cfg-blank-{}-{}",
                std::process::id(),
                blank.len()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("tf.toml"),
                format!("[fleet]\nauth_token = \"{blank}\"\n"),
            )
            .unwrap();
            let c = FleetConfig::resolve_layered(&dir, FleetOverrides::default(), &no_env).unwrap();
            assert_eq!(c.auth_token, None, "toml blank {blank:?} ⇒ no token");
            assert_eq!(c.effective_auth_token(), None);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn blank_auth_token_nonloopback_refuses_security_check() {
        // THE security property: a blank token on a non-loopback bind
        // is REFUSED exactly like None — no unauthenticated public
        // socket. (parse path AND a directly-blank FleetConfig — the
        // effective_auth_token defense-in-depth seam.)
        let ov = FleetOverrides {
            bind: Some("0.0.0.0:8080".to_string()),
            auth_token: Some("   ".to_string()),
            ..FleetOverrides::default()
        };
        let c = FleetConfig::resolve_layered(std::path::Path::new("/nonexistent"), ov, &no_env)
            .unwrap();
        assert!(
            matches!(c.security_check(), Err(FleetConfigError::BadBind { .. })),
            "non-loopback + blank CLI token MUST refuse (no unauth socket)"
        );

        // Defense-in-depth: even a FleetConfig that already holds a
        // blank auth_token (bypassing the parse-reject) is refused —
        // security_check models "effective secret present", not is_none.
        let mut c2 = FleetConfig::defaults();
        c2.bind = Some("0.0.0.0:9090".parse().unwrap());
        c2.auth_token = Some(" \t ".to_string());
        assert_eq!(c2.effective_auth_token(), None);
        assert!(
            matches!(c2.security_check(), Err(FleetConfigError::BadBind { .. })),
            "blank token in FleetConfig + non-loopback MUST still refuse"
        );

        // A real token on the same bind is accepted (no over-rejection).
        let mut ok = FleetConfig::defaults();
        ok.bind = Some("0.0.0.0:9090".parse().unwrap());
        ok.auth_token = Some("s3cr3t".to_string());
        assert!(ok.security_check().is_ok());
        assert_eq!(ok.effective_auth_token(), Some("s3cr3t"));
    }

    #[test]
    fn state_dir_abs_resolution() {
        let c = FleetConfig::defaults();
        assert_eq!(
            c.state_dir_abs(std::path::Path::new("/repo")),
            PathBuf::from("/repo/.cargoless")
        );
        let mut c = FleetConfig::defaults();
        c.state_dir = PathBuf::from("/abs/state");
        assert_eq!(
            c.state_dir_abs(std::path::Path::new("/repo")),
            PathBuf::from("/abs/state")
        );
    }

    #[test]
    fn overrides_from_map_helper() {
        let mut m = BTreeMap::new();
        m.insert("repo".to_string(), "/r".to_string());
        m.insert("no-corun".to_string(), String::new());
        let ov = overrides_from_map(&m);
        assert_eq!(ov.repo, Some(PathBuf::from("/r")));
        assert_eq!(ov.corun, Some(false));
        assert_eq!(ov.cas_dir, None);
    }
}
