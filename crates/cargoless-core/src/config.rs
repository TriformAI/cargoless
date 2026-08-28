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
//! deliberate opposite of the CLI `Config` reader, which hard-errors on an
//! unknown key inside a section *it* owns (`[project] root/target`,
//! `[cache] dir`) — a silently-ignored typo in a zero-config tool is a
//! support nightmare. Both are correct for their ownership scope, and they
//! read the *same shared file*: this reader must never reject keys it does
//! not own (doing so would break every existing v0 `tf.toml`), and the CLI
//! reader skips the sections owned here (`[fleet]`, `[telemetry]`, `[lane]`)
//! rather than rejecting them.

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

    /// Short, grep-safe tag for `key=value` observability lines.
    ///
    /// [`Self::describe`] is prose for humans ("default (v0-compatible)") and
    /// its spaces and parens break a boot-line field split. An operator reading
    /// `max_members_src=tf.toml` needs one token, not a sentence.
    pub fn tag(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::TfToml => "tf.toml",
            Source::Env => "env",
            Source::Cli => "cli",
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
    /// A `[lane]` / `CARGOLESS_LANE_*` setting is unparseable or out of range.
    ///
    /// Carries `key` rather than hardcoding it (unlike `parse_sampler_arg`
    /// below, which names `OTEL_TRACES_SAMPLER_ARG` in every message it
    /// produces): the lane has five numeric settings, and an error that names
    /// the wrong one is worse than no error.
    BadLaneSetting {
        origin: &'static str,
        key: &'static str,
        value: String,
        why: &'static str,
    },
}

impl fmt::Display for FleetConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FleetConfigError::BadTfToml { line_no, line, why } => write!(
                f,
                "tf.toml: {why} (line {line_no}: `{line}`).\n  \
                 [fleet] repo, bind, corun, auth_token. \
                 [telemetry] otel_endpoint, otel_headers, otel_service_name, \
                 otel_log_level, otel_sampler_arg. \
                 [lane] max_members, capture_window_ticks, eject_ttl_ticks, \
                 infra_backoff_ticks, infra_max_attempts. \
                 [project] state_dir. [cache] cas_dir."
            ),
            FleetConfigError::BadBind { value, why } => write!(
                f,
                "invalid bind address `{value}`: {why}.\n  \
                 expected `HOST:PORT`, e.g. `127.0.0.1:8080` (loopback, \
                 safe) or `0.0.0.0:8080` (network — requires --auth-token)."
            ),
            FleetConfigError::BadLaneSetting {
                origin,
                key,
                value,
                why,
            } => write!(
                f,
                "{origin}: `{key}` = `{value}` — {why}.\n  \
                 [lane] keys: max_members, capture_window_ticks, \
                 eject_ttl_ticks, infra_backoff_ticks, infra_max_attempts \
                 (whole numbers; only capture_window_ticks may be 0)."
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

// ═══════════════════════════════════════════════════════════════════════
// #246 Wave-1 (5b) — Telemetry configuration (OTEL + SigNoz export seam).
// ═══════════════════════════════════════════════════════════════════════
//
// Pure-data struct living in `cargoless-core` so the daemon-runtime
// re-resolution path has no CLI dependency. The `cargoless` binary owns
// the actual `tracing` / `opentelemetry-*` SDK init (`telemetry.rs`) —
// cores stay log-free per the Explore-confirmed crate-boundary discipline
// (Wave-1 5a foundation lands in the binary, not here).
//
// All fields default to "no-op" — if `otel_endpoint` is `None`, the
// binary-side `init_telemetry` is a no-op and zero OTEL overhead is paid.
// Matches fleet convention (physics telemetry.rs:201-218) and the plan's
// fail-soft requirement (telemetry MUST NOT wedge the daemon).
//
// Precedence: `CLI flag  >  environment  >  tf.toml  >  built-in default`
// (identical to FleetConfig's layering rule).

/// Per-field provenance for [`TelemetryConfig`] — mirror of [`Provenance`]
/// for the FleetConfig surface. Surfaced so the codebase can always say
/// *why* telemetry is configured the way it is (the vision-cut property).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetryProvenance {
    pub otel_endpoint: Source,
    pub otel_headers: Source,
    pub otel_service_name: Source,
    pub otel_log_level: Source,
    pub otel_sampler_arg: Source,
}

impl Default for TelemetryProvenance {
    fn default() -> Self {
        Self {
            otel_endpoint: Source::Default,
            otel_headers: Source::Default,
            otel_service_name: Source::Default,
            otel_log_level: Source::Default,
            otel_sampler_arg: Source::Default,
        }
    }
}

/// CLI-supplied telemetry overrides — the **injection struct** the CLI
/// crate fills from clap (`--otel-endpoint`, `--otel-service-name`) and
/// hands to [`TelemetryConfig::resolve`]. Plain `Option`-of-value, no clap
/// types ⇒ core never gains an arg-parsing dependency.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetryOverrides {
    pub otel_endpoint: Option<String>,
    pub otel_service_name: Option<String>,
}

/// Fully-resolved telemetry configuration consumed by the binary-side
/// `init_telemetry` (5a). `f64` precludes `Eq` but PartialEq is enough for
/// every consumer (tests + diagnostics).
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryConfig {
    /// OTLP exporter endpoint (gRPC default). `None` ⇒ telemetry init is
    /// a no-op (zero OTEL overhead for local `cargoless check` / ad-hoc
    /// `serve` against no operator instance). Env: `OTEL_EXPORTER_OTLP_ENDPOINT`.
    pub otel_endpoint: Option<String>,
    /// Optional OTLP headers (e.g. `Authorization=Bearer …`). Comma-
    /// separated `key=value` pairs per OTEL spec. Env: `OTEL_EXPORTER_OTLP_HEADERS`.
    pub otel_headers: Option<BTreeMap<String, String>>,
    /// Service name (resource attr `service.name`). Default `"cargoless"`.
    /// Env: `OTEL_SERVICE_NAME`.
    pub otel_service_name: String,
    /// OTLP log filter level — default `"warn"` matches fleet WARN+;
    /// `INFO` captured by stdout filelog if operator runs one.
    /// Env: `OTEL_LOG_LEVEL`.
    pub otel_log_level: String,
    /// Trace sampler ratio (AlwaysOn = 1.0 for v0/v0.1 low volume).
    /// Env: `OTEL_TRACES_SAMPLER_ARG`.
    pub otel_sampler_arg: f64,
    /// Per-field provenance.
    pub provenance: TelemetryProvenance,
}

impl TelemetryConfig {
    /// All-defaults = telemetry init is a no-op (the safe v0-compatible
    /// state — no endpoint ⇒ no exporters ⇒ no overhead).
    pub fn defaults() -> Self {
        Self {
            otel_endpoint: None,
            otel_headers: None,
            otel_service_name: "cargoless".to_string(),
            otel_log_level: "warn".to_string(),
            otel_sampler_arg: 1.0,
            provenance: TelemetryProvenance::default(),
        }
    }

    /// Resolve telemetry config from process env + `tf.toml` + CLI overrides.
    /// `repo_root` is the project/repo root (the `tf.toml` search path).
    pub fn resolve(
        repo_root: impl AsRef<Path>,
        overrides: TelemetryOverrides,
    ) -> Result<Self, FleetConfigError> {
        let env = |k: &str| std::env::var(k).ok();
        Self::resolve_layered(repo_root.as_ref(), overrides, &env)
    }

    /// Env-injected resolver (the unit-testable variant — `env` is the
    /// only IO seam so precedence logic is pure).
    pub fn resolve_layered(
        repo_root: &Path,
        ov: TelemetryOverrides,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, FleetConfigError> {
        let mut cfg = Self::defaults();

        // ---- layer 1: tf.toml (tolerant partial overlay) -------------
        if let Ok(text) = std::fs::read_to_string(repo_root.join("tf.toml")) {
            apply_telemetry_tf_toml_overlay(&mut cfg, &text)?;
        }

        // ---- layer 2: environment ------------------------------------
        if let Some(v) = env("OTEL_EXPORTER_OTLP_ENDPOINT").filter(|s| !s.trim().is_empty()) {
            cfg.otel_endpoint = Some(v);
            cfg.provenance.otel_endpoint = Source::Env;
        }
        if let Some(v) = env("OTEL_EXPORTER_OTLP_HEADERS").filter(|s| !s.trim().is_empty()) {
            cfg.otel_headers = Some(parse_otel_headers(&v));
            cfg.provenance.otel_headers = Source::Env;
        }
        if let Some(v) = env("OTEL_SERVICE_NAME").filter(|s| !s.trim().is_empty()) {
            cfg.otel_service_name = v;
            cfg.provenance.otel_service_name = Source::Env;
        }
        if let Some(v) = env("OTEL_LOG_LEVEL").filter(|s| !s.trim().is_empty()) {
            cfg.otel_log_level = v;
            cfg.provenance.otel_log_level = Source::Env;
        }
        if let Some(v) = env("OTEL_TRACES_SAMPLER_ARG").filter(|s| !s.trim().is_empty()) {
            cfg.otel_sampler_arg = parse_sampler_arg("env OTEL_TRACES_SAMPLER_ARG", &v)?;
            cfg.provenance.otel_sampler_arg = Source::Env;
        }

        // ---- layer 3: explicit CLI flags (highest) -------------------
        if let Some(v) = ov.otel_endpoint.filter(|s| !s.trim().is_empty()) {
            cfg.otel_endpoint = Some(v);
            cfg.provenance.otel_endpoint = Source::Cli;
        }
        if let Some(v) = ov.otel_service_name.filter(|s| !s.trim().is_empty()) {
            cfg.otel_service_name = v;
            cfg.provenance.otel_service_name = Source::Cli;
        }

        Ok(cfg)
    }

    /// `true` once an endpoint is resolved ⇒ binary-side `init_telemetry`
    /// should actually spin up exporters. The single load-bearing predicate
    /// for fail-soft init — no endpoint = no-op, never an OTLP connect
    /// attempt + log noise.
    pub fn enabled(&self) -> bool {
        self.otel_endpoint
            .as_ref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }
}

/// Parse a comma-separated `key=value,key=value` OTEL headers string
/// (per the OTEL spec for `OTEL_EXPORTER_OTLP_HEADERS`). Tolerant on
/// whitespace; silently skips malformed pairs (per fleet convention —
/// telemetry MUST NOT wedge the daemon on a typo).
fn parse_otel_headers(s: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for pair in s.split(',') {
        if let Some((k, v)) = pair.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            if !k.is_empty() && !v.is_empty() {
                m.insert(k.to_string(), v.to_string());
            }
        }
    }
    m
}

/// Parse the OTEL_TRACES_SAMPLER_ARG f64. Reuses [`FleetConfigError`] for
/// consistency with the rest of the resolver surface — a malformed sampler
/// arg is an actionable user error, not a "fail soft" surface (the
/// fail-soft contract is for exporter unreachability at runtime, not for
/// startup config typos).
fn parse_sampler_arg(origin: &'static str, v: &str) -> Result<f64, FleetConfigError> {
    f64::from_str(v.trim()).map_err(|_| FleetConfigError::BadBool {
        origin,
        key: "OTEL_TRACES_SAMPLER_ARG".to_string(),
        value: v.to_string(),
    })
}

/// Apply the **telemetry-owned** keys from a (shared) `tf.toml` over `cfg`.
/// Same tolerant-overlay contract as [`apply_tf_toml_overlay`] — unknown
/// sections/keys are ignored, owned keys' values are validated.
///
/// Owned keys (all under `[telemetry]`):
/// - `otel_endpoint = "<url>"`
/// - `otel_service_name = "<name>"`
/// - `otel_log_level = "<level>"`
/// - `otel_sampler_arg = <number>`
/// - `otel_headers = "k1=v1,k2=v2"`
fn apply_telemetry_tf_toml_overlay(
    cfg: &mut TelemetryConfig,
    text: &str,
) -> Result<(), FleetConfigError> {
    let mut section = String::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = unquote(val.trim());
        match (section.as_str(), key) {
            ("telemetry", "otel_endpoint") if !val.trim().is_empty() => {
                cfg.otel_endpoint = Some(val);
                cfg.provenance.otel_endpoint = Source::TfToml;
            }
            ("telemetry", "otel_service_name") if !val.trim().is_empty() => {
                cfg.otel_service_name = val;
                cfg.provenance.otel_service_name = Source::TfToml;
            }
            ("telemetry", "otel_log_level") if !val.trim().is_empty() => {
                cfg.otel_log_level = val;
                cfg.provenance.otel_log_level = Source::TfToml;
            }
            ("telemetry", "otel_sampler_arg") if !val.trim().is_empty() => {
                cfg.otel_sampler_arg =
                    parse_sampler_arg("tf.toml [telemetry] otel_sampler_arg", &val).map_err(
                        |_| FleetConfigError::BadTfToml {
                            line_no,
                            line: raw.trim().to_string(),
                            why: format!("`otel_sampler_arg` expects a number, got `{val}`"),
                        },
                    )?;
                cfg.provenance.otel_sampler_arg = Source::TfToml;
            }
            ("telemetry", "otel_headers") if !val.trim().is_empty() => {
                cfg.otel_headers = Some(parse_otel_headers(&val));
                cfg.provenance.otel_headers = Source::TfToml;
            }
            _ => {}
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════
// Lane policy — the `[lane]` section of `tf.toml`.
// ═══════════════════════════════════════════════════════════════════════
//
// The build lane's batch size and retry policy, declared BY THE PROJECT, in
// the project's own repo, versioned alongside the code whose build cost it
// describes. How many changes may safely ride one release build is a
// property of the codebase — not of whoever happened to start the daemon.
//
// Before this existed, `lane::LaneConfig::default()` was the only value the
// production path could ever see: `with_lane` called `LaneState::new`, which
// is `with_config(root, LaneConfig::default())`. `max_members` was documented
// as a tunable knob in two places and reachable from none, which cost one
// investigation already — a shell-script constant was raised to 20, found to
// be inert, and reverted, without locating the real bound.
//
// Do not confuse this `max_members` with the CHECK COALESCER's identically
// named field (`serveapi.rs`, default 40, `CARGOLESS_BATCH_MAX_MEMBERS`).
// That one bounds a fast per-save check batch; this one bounds how many
// members ride a 25-80 minute release build. The name collision is exactly
// what derailed the earlier investigation.
//
// Precedence: `environment  >  tf.toml  >  built-in default`.
//
// There is deliberately NO `LaneOverrides` / CLI layer: no `--lane-*` flag
// exists, and an injection struct that is always `Default::default()` is the
// dead machinery `servedrv.rs` forbids ("a knob whose effect is invisible").
// Add one when the first flag lands, not before.

/// Per-field provenance for [`LaneSettings`] — mirror of [`Provenance`] and
/// [`TelemetryProvenance`]. Surfaced so an operator can tell "the tf.toml was
/// read" from "the default merely coincided" — without that, a knob that
/// silently failed to apply looks identical to one that applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneProvenance {
    pub max_members: Source,
    pub capture_window_ticks: Source,
    pub eject_ttl_ticks: Source,
    pub infra_backoff_ticks: Source,
    pub infra_max_attempts: Source,
}

impl Default for LaneProvenance {
    fn default() -> Self {
        Self {
            max_members: Source::Default,
            capture_window_ticks: Source::Default,
            eject_ttl_ticks: Source::Default,
            infra_backoff_ticks: Source::Default,
            infra_max_attempts: Source::Default,
        }
    }
}

/// Fully-resolved lane policy for this project: `[lane]` in `tf.toml` plus
/// `CARGOLESS_LANE_*` env, layered over the built-in defaults.
///
/// Holds a [`crate::lane::LaneConfig`] rather than mirroring its five fields.
/// That type is the machine's own contract — it is what
/// `LaneState::with_config` takes, and it carries the production forensics
/// behind each default in its doc comments. Duplicating five integers here
/// would mean two places to change and a conversion that can drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneSettings {
    /// Ready to hand to `LaneState::with_config`.
    pub lane: crate::lane::LaneConfig,
    /// Which layer won, per field.
    pub provenance: LaneProvenance,
}

impl LaneSettings {
    /// The built-in policy — byte-identical to `LaneConfig::default()`, so a
    /// project with no `[lane]` section and no env behaves exactly as before.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            lane: crate::lane::LaneConfig::default(),
            provenance: LaneProvenance::default(),
        }
    }

    /// Resolve for `repo_root`, layering `default < tf.toml < env`. Reads the
    /// process environment; see [`Self::resolve_layered`] for the env-injected
    /// (unit-testable) variant.
    pub fn resolve(repo_root: impl AsRef<Path>) -> Result<Self, FleetConfigError> {
        let env = |k: &str| std::env::var(k).ok();
        Self::resolve_layered(repo_root.as_ref(), &env)
    }

    /// Env-injected resolver core. `env` is the only IO seam beyond reading
    /// `tf.toml`, so the precedence logic is pure and unit-testable without
    /// touching the process environment.
    ///
    /// Calls [`Self::validate`] before returning, so no caller can forget it.
    pub fn resolve_layered(
        repo_root: &Path,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, FleetConfigError> {
        let mut cfg = Self::defaults();

        // ---- layer 1: tf.toml (tolerant partial overlay) -------------
        if let Ok(text) = std::fs::read_to_string(repo_root.join("tf.toml")) {
            apply_lane_tf_toml_overlay(&mut cfg, &text)?;
        }

        // ---- layer 2: environment ------------------------------------
        // `CARGOLESS_LANE_*`, matching the lane's existing env family
        // (CARGOLESS_LANE_PROFILE / _BASE / _ARTIFACT / ...), NOT the older
        // `TF_*` fleet family. Env matters concretely here: the lane is turned
        // ON by env in a deployment, where the tf.toml is the repo's own and
        // shared with every developer's checkout — a shared daemon needs a
        // bigger batch than a laptop does.
        if let Some(v) = env("CARGOLESS_LANE_MAX_MEMBERS").filter(|s| !s.trim().is_empty()) {
            cfg.lane.max_members =
                parse_lane_num("env CARGOLESS_LANE_MAX_MEMBERS", "max_members", &v)?;
            cfg.provenance.max_members = Source::Env;
        }
        if let Some(v) = env("CARGOLESS_LANE_CAPTURE_WINDOW_TICKS").filter(|s| !s.trim().is_empty())
        {
            cfg.lane.capture_window_ticks = parse_lane_num(
                "env CARGOLESS_LANE_CAPTURE_WINDOW_TICKS",
                "capture_window_ticks",
                &v,
            )?;
            cfg.provenance.capture_window_ticks = Source::Env;
        }
        if let Some(v) = env("CARGOLESS_LANE_EJECT_TTL_TICKS").filter(|s| !s.trim().is_empty()) {
            cfg.lane.eject_ttl_ticks =
                parse_lane_num("env CARGOLESS_LANE_EJECT_TTL_TICKS", "eject_ttl_ticks", &v)?;
            cfg.provenance.eject_ttl_ticks = Source::Env;
        }
        if let Some(v) = env("CARGOLESS_LANE_INFRA_BACKOFF_TICKS").filter(|s| !s.trim().is_empty())
        {
            cfg.lane.infra_backoff_ticks = parse_lane_num(
                "env CARGOLESS_LANE_INFRA_BACKOFF_TICKS",
                "infra_backoff_ticks",
                &v,
            )?;
            cfg.provenance.infra_backoff_ticks = Source::Env;
        }
        if let Some(v) = env("CARGOLESS_LANE_INFRA_MAX_ATTEMPTS").filter(|s| !s.trim().is_empty()) {
            cfg.lane.infra_max_attempts = parse_lane_num(
                "env CARGOLESS_LANE_INFRA_MAX_ATTEMPTS",
                "infra_max_attempts",
                &v,
            )?;
            cfg.provenance.infra_max_attempts = Source::Env;
        }

        cfg.validate()?;
        Ok(cfg)
    }

    /// Refuse a policy that cannot work. Sibling in spirit to
    /// [`FleetConfig::security_check`]: a frozen rule with a frozen message,
    /// testable without a daemon.
    ///
    /// The rule is **deliberately not** a uniform "every field must be > 0" —
    /// `capture_window_ticks = 0` is documented, intentional, and what every
    /// unit test and single-developer project uses. A tidy all-fields-positive
    /// validator would break exactly that configuration.
    pub fn validate(&self) -> Result<(), FleetConfigError> {
        // `max_members = 0` is not a deadlock, it is an infinite loop of real
        // release builds. `maybe_start_build` has already returned if the queue
        // is empty, so `queue.len() >= 0` is unconditionally true: the capture
        // window is skipped, `take = len.min(0) = 0` drains nobody, and
        // `StartBuild` fires with an EMPTY roster. The driver then compiles the
        // bare base for 25-80 minutes, finishes, returns to Idle with the queue
        // still full, and fires again — forever, journalling nothing (the
        // recovery value is None while `in_flight` is empty).
        if self.lane.max_members == 0 {
            return Err(FleetConfigError::BadLaneSetting {
                origin: "lane policy",
                key: "max_members",
                value: "0".to_string(),
                why: "a lane that may carry nobody dispatches empty builds forever",
            });
        }
        // The increment precedes the `>=` check, so 0 ejects the whole roster
        // on the FIRST transient failure — the opposite of what the field is
        // for. `lane_policy.rs` already asserts the shipped default is non-zero.
        if self.lane.infra_max_attempts == 0 {
            return Err(FleetConfigError::BadLaneSetting {
                origin: "lane policy",
                key: "infra_max_attempts",
                value: "0".to_string(),
                why: "0 ejects the whole queue on the first transient failure",
            });
        }
        // "A zero backoff IS the hot loop" — the retry fires as fast as the
        // failure returns. Measured at ~one candidate attempt every 2.5s,
        // indefinitely, on the first real deployment.
        if self.lane.infra_backoff_ticks == 0 {
            return Err(FleetConfigError::BadLaneSetting {
                origin: "lane policy",
                key: "infra_backoff_ticks",
                value: "0".to_string(),
                why: "a zero backoff retries as fast as the failure returns",
            });
        }
        // `now >= expires_at_tick` is immediately true, so an ejection lapses
        // on the very next tick: the lane ejects a member and silently
        // readmits it, rebuilding the same red tree every cycle. A gate that
        // visibly ejects and invisibly readmits is indistinguishable from a bug.
        if self.lane.eject_ttl_ticks == 0 {
            return Err(FleetConfigError::BadLaneSetting {
                origin: "lane policy",
                key: "eject_ttl_ticks",
                value: "0".to_string(),
                why: "an ejection that lapses on the next tick never holds",
            });
        }
        // `capture_window_ticks = 0` is intentionally ABSENT from these checks:
        // it means "build immediately", which is what a single-developer repo
        // and every unit test want. Do not add it.
        Ok(())
    }
}

/// Parse a `[lane]` / `CARGOLESS_LANE_*` whole number.
///
/// Generic over the target so `usize`/`u64`/`u32` each parse natively and no
/// lossy cast is needed. Takes `key` and reports it — `parse_sampler_arg`
/// above hardcodes its key into the error, so reusing that shape for the
/// lane's five numeric settings would name the wrong one.
///
/// Returns a typed error rather than falling back to the default. The
/// `configured_batch_*` helpers in the `cargoless` binary do the opposite and
/// swallow typos; letting `max_members = "fourty"` boot silently as 10 is the
/// invisible-knob failure this whole section exists to remove.
fn parse_lane_num<T: FromStr>(
    origin: &'static str,
    key: &'static str,
    v: &str,
) -> Result<T, FleetConfigError> {
    T::from_str(v.trim()).map_err(|_| FleetConfigError::BadLaneSetting {
        origin,
        key,
        value: v.to_string(),
        why: "expects a whole number",
    })
}

/// Same tolerant-overlay contract as [`apply_tf_toml_overlay`] — unknown
/// sections/keys are ignored, owned keys' values are validated.
///
/// Owned keys (all under `[lane]`, all whole numbers):
/// - `max_members = <n>`
/// - `capture_window_ticks = <n>`
/// - `eject_ttl_ticks = <n>`
/// - `infra_backoff_ticks = <n>`
/// - `infra_max_attempts = <n>`
fn apply_lane_tf_toml_overlay(cfg: &mut LaneSettings, text: &str) -> Result<(), FleetConfigError> {
    let mut section = String::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = unquote(val.trim());
        // A malformed value from tf.toml reports as `BadTfToml` so the LINE
        // NUMBER survives — the same trade the telemetry sampler arm makes.
        // `BadLaneSetting` is for the env layer and for `validate`, where
        // there is no line to point at.
        macro_rules! lane_num {
            ($field:ident, $name:literal) => {{
                cfg.lane.$field = parse_lane_num(concat!("tf.toml [lane] ", $name), $name, &val)
                    .map_err(|_| FleetConfigError::BadTfToml {
                        line_no,
                        line: raw.trim().to_string(),
                        why: format!(
                            concat!("`", $name, "` expects a whole number, got `{}`"),
                            val
                        ),
                    })?;
                cfg.provenance.$field = Source::TfToml;
            }};
        }
        match (section.as_str(), key) {
            ("lane", "max_members") if !val.trim().is_empty() => {
                lane_num!(max_members, "max_members")
            }
            ("lane", "capture_window_ticks") if !val.trim().is_empty() => {
                lane_num!(capture_window_ticks, "capture_window_ticks")
            }
            ("lane", "eject_ttl_ticks") if !val.trim().is_empty() => {
                lane_num!(eject_ttl_ticks, "eject_ttl_ticks")
            }
            ("lane", "infra_backoff_ticks") if !val.trim().is_empty() => {
                lane_num!(infra_backoff_ticks, "infra_backoff_ticks")
            }
            ("lane", "infra_max_attempts") if !val.trim().is_empty() => {
                lane_num!(infra_max_attempts, "infra_max_attempts")
            }
            _ => {}
        }
    }
    Ok(())
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
        // Single-line whitespace only — a raw newline is not a valid
        // single-line `tf.toml` basic-string value (would split the
        // line-based reader); `\t`/spaces fully exercise the trim guard.
        for blank in ["", "   ", "\t", " \t "] {
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

    // ───────── #246 Wave-1 (5b) — TelemetryConfig tests ─────────
    //
    // Foundational property: defaults are a no-op (telemetry disabled).
    // Every other field validates layering precedence (default < tf.toml <
    // env < CLI) and the load-bearing `enabled()` predicate — the SINGLE
    // gate that prevents OTEL init from spinning up exporters when no
    // endpoint is configured.

    #[test]
    fn telemetry_defaults_are_disabled_no_endpoint() {
        // THE no-op invariant: no endpoint resolved ⇒ binary-side
        // init_telemetry is a no-op (zero OTEL overhead). This is the
        // load-bearing fail-soft guarantee from the plan.
        let c = TelemetryConfig::defaults();
        assert_eq!(c.otel_endpoint, None);
        assert_eq!(c.otel_headers, None);
        assert_eq!(c.otel_service_name, "cargoless");
        assert_eq!(c.otel_log_level, "warn");
        assert_eq!(c.otel_sampler_arg, 1.0);
        assert!(!c.enabled(), "defaults() MUST yield enabled=false");
    }

    #[test]
    fn telemetry_resolve_no_inputs_equals_defaults() {
        let tmp = std::env::temp_dir().join(format!("cl-otel-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let c =
            TelemetryConfig::resolve_layered(&tmp, TelemetryOverrides::default(), &no_env).unwrap();
        assert_eq!(c, TelemetryConfig::defaults());
        assert!(!c.enabled());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn telemetry_enabled_iff_endpoint_set() {
        // The single predicate that gates exporter init. Explicit
        // verification — a regression that flips enabled() semantics is
        // a load-bearing fail-soft regression.
        let mut c = TelemetryConfig::defaults();
        assert!(!c.enabled());
        c.otel_endpoint = Some("http://otel:4317".to_string());
        assert!(c.enabled());
        // Blank endpoint is treated as "not set" (no surprise no-op
        // bypass via empty string).
        c.otel_endpoint = Some("   ".to_string());
        assert!(!c.enabled(), "blank endpoint MUST NOT enable");
        c.otel_endpoint = Some(String::new());
        assert!(!c.enabled(), "empty endpoint MUST NOT enable");
    }

    #[test]
    fn telemetry_env_precedence_over_tf_toml() {
        let dir = std::env::temp_dir().join(format!("cl-otel-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tf.toml"),
            "[telemetry]\n\
             otel_endpoint = \"http://from-toml:4317\"\n\
             otel_service_name = \"toml-svc\"\n\
             otel_log_level = \"info\"\n\
             otel_sampler_arg = 0.5\n",
        )
        .unwrap();

        // toml-only baseline: every field comes from tf.toml.
        let c =
            TelemetryConfig::resolve_layered(&dir, TelemetryOverrides::default(), &no_env).unwrap();
        assert_eq!(c.otel_endpoint.as_deref(), Some("http://from-toml:4317"));
        assert_eq!(c.otel_service_name, "toml-svc");
        assert_eq!(c.otel_log_level, "info");
        assert_eq!(c.otel_sampler_arg, 0.5);
        assert_eq!(c.provenance.otel_endpoint, Source::TfToml);
        assert_eq!(c.provenance.otel_service_name, Source::TfToml);
        assert!(c.enabled());

        // env overrides toml across every field.
        let env = |k: &str| match k {
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some("http://from-env:4317".to_string()),
            "OTEL_SERVICE_NAME" => Some("env-svc".to_string()),
            "OTEL_LOG_LEVEL" => Some("debug".to_string()),
            "OTEL_TRACES_SAMPLER_ARG" => Some("0.25".to_string()),
            _ => None,
        };
        let c =
            TelemetryConfig::resolve_layered(&dir, TelemetryOverrides::default(), &env).unwrap();
        assert_eq!(c.otel_endpoint.as_deref(), Some("http://from-env:4317"));
        assert_eq!(c.otel_service_name, "env-svc");
        assert_eq!(c.otel_log_level, "debug");
        assert_eq!(c.otel_sampler_arg, 0.25);
        assert_eq!(c.provenance.otel_endpoint, Source::Env);
        assert_eq!(c.provenance.otel_service_name, Source::Env);
        assert_eq!(c.provenance.otel_log_level, Source::Env);
        assert_eq!(c.provenance.otel_sampler_arg, Source::Env);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn telemetry_cli_overrides_env_for_endpoint_and_service_name() {
        // The two CLI-injected fields (`--otel-endpoint`, `--otel-service-name`)
        // must beat env, matching FleetConfig's precedence rule.
        let env = |k: &str| match k {
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some("http://env:4317".to_string()),
            "OTEL_SERVICE_NAME" => Some("env-svc".to_string()),
            _ => None,
        };
        let ov = TelemetryOverrides {
            otel_endpoint: Some("http://cli:4317".to_string()),
            otel_service_name: Some("cli-svc".to_string()),
        };
        let c = TelemetryConfig::resolve_layered(std::path::Path::new("/nonexistent"), ov, &env)
            .unwrap();
        assert_eq!(c.otel_endpoint.as_deref(), Some("http://cli:4317"));
        assert_eq!(c.otel_service_name, "cli-svc");
        assert_eq!(c.provenance.otel_endpoint, Source::Cli);
        assert_eq!(c.provenance.otel_service_name, Source::Cli);
    }

    #[test]
    fn telemetry_headers_parse_comma_separated_kv() {
        let env = |k: &str| match k {
            "OTEL_EXPORTER_OTLP_HEADERS" => {
                Some("Authorization=Bearer xyz, x-tenant = acme,malformed".to_string())
            }
            _ => None,
        };
        let c = TelemetryConfig::resolve_layered(
            std::path::Path::new("/nonexistent"),
            TelemetryOverrides::default(),
            &env,
        )
        .unwrap();
        let h = c.otel_headers.expect("headers set from env");
        assert_eq!(
            h.get("Authorization").map(String::as_str),
            Some("Bearer xyz")
        );
        assert_eq!(h.get("x-tenant").map(String::as_str), Some("acme"));
        // Tolerant: malformed pair (no `=`) silently dropped — telemetry
        // MUST NOT wedge the daemon on a typo.
        assert!(!h.contains_key("malformed"));
        assert_eq!(h.len(), 2);
        assert_eq!(c.provenance.otel_headers, Source::Env);
    }

    #[test]
    fn telemetry_blank_env_endpoint_does_not_override_default() {
        // Mirror of the FleetConfig blank-token defense: a blank env value
        // is treated as "not set" — telemetry stays disabled rather than
        // silently engaging a malformed exporter.
        for blank in ["", " ", "\t  \t"] {
            let env = |k: &str| (k == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| blank.to_string());
            let c = TelemetryConfig::resolve_layered(
                std::path::Path::new("/nonexistent"),
                TelemetryOverrides::default(),
                &env,
            )
            .unwrap();
            assert_eq!(
                c.otel_endpoint, None,
                "blank env {blank:?} MUST NOT set endpoint"
            );
            assert!(!c.enabled(), "blank env {blank:?} ⇒ telemetry disabled");
        }
    }

    #[test]
    fn telemetry_malformed_sampler_arg_is_actionable_error() {
        let env = |k: &str| (k == "OTEL_TRACES_SAMPLER_ARG").then(|| "not-a-number".to_string());
        let err = TelemetryConfig::resolve_layered(
            std::path::Path::new("/nonexistent"),
            TelemetryOverrides::default(),
            &env,
        )
        .unwrap_err();
        // Reuses BadBool for sampler malformedness — same error shape
        // every consumer already handles.
        assert!(matches!(err, FleetConfigError::BadBool { .. }));
    }

    #[test]
    fn telemetry_tf_toml_unknown_keys_tolerantly_ignored() {
        // The [telemetry] section is a partial overlay over a shared
        // tf.toml; unknown keys MUST be silently ignored (a future
        // additive field like otel_compression should never break
        // existing parsers on older binaries).
        let dir = std::env::temp_dir().join(format!("cl-otel-tol-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tf.toml"),
            "[telemetry]\n\
             otel_endpoint = \"http://here:4317\"\n\
             otel_compression = \"gzip\"  # future key, today unknown\n\
             [unrelated_section]\n\
             totally_other = \"thing\"\n",
        )
        .unwrap();
        let c =
            TelemetryConfig::resolve_layered(&dir, TelemetryOverrides::default(), &no_env).unwrap();
        assert_eq!(c.otel_endpoint.as_deref(), Some("http://here:4317"));
        assert!(c.enabled());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn telemetry_blank_cli_overrides_do_not_clobber_env() {
        // Defense-in-depth: `--otel-endpoint ""` MUST NOT wipe a perfectly
        // good env-set endpoint (same blank-rejects-uniformly discipline
        // as the auth_token surface).
        let env = |k: &str| {
            (k == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "http://env-good:4317".to_string())
        };
        let ov = TelemetryOverrides {
            otel_endpoint: Some("   ".to_string()),
            otel_service_name: None,
        };
        let c = TelemetryConfig::resolve_layered(std::path::Path::new("/nonexistent"), ov, &env)
            .unwrap();
        assert_eq!(c.otel_endpoint.as_deref(), Some("http://env-good:4317"));
        // env stays the winning provenance (CLI blank did not promote).
        assert_eq!(c.provenance.otel_endpoint, Source::Env);
    }

    // ---- [lane] policy -------------------------------------------------

    /// A unique temp dir per test, matching the tf.toml convention above.
    fn lane_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cl-lane-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lane_defaults_match_lane_config_default() {
        // The no-behaviour-change guarantee: a project with no `[lane]` and no
        // env gets exactly the policy the lane shipped with. Analogue of
        // `defaults_are_v0`.
        let s = LaneSettings::defaults();
        assert_eq!(s.lane, crate::lane::LaneConfig::default());
        assert_eq!(s.provenance.max_members, Source::Default);
        assert_eq!(s.provenance.capture_window_ticks, Source::Default);
        assert_eq!(s.provenance.eject_ttl_ticks, Source::Default);
        assert_eq!(s.provenance.infra_backoff_ticks, Source::Default);
        assert_eq!(s.provenance.infra_max_attempts, Source::Default);
    }

    #[test]
    fn lane_resolve_with_no_inputs_equals_defaults() {
        let dir = lane_dir("noinput");
        let s = LaneSettings::resolve_layered(&dir, &no_env).unwrap();
        assert_eq!(s, LaneSettings::defaults());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lane_precedence_env_beats_tf_toml_beats_default() {
        let dir = lane_dir("prec");
        std::fs::write(
            dir.join("tf.toml"),
            "[lane]\nmax_members = 25\ncapture_window_ticks = 300\n",
        )
        .unwrap();

        // tf.toml only.
        let c = LaneSettings::resolve_layered(&dir, &no_env).unwrap();
        assert_eq!(c.lane.max_members, 25);
        assert_eq!(c.provenance.max_members, Source::TfToml);
        assert_eq!(c.lane.capture_window_ticks, 300);
        assert_eq!(c.provenance.capture_window_ticks, Source::TfToml);
        // Untouched fields keep the built-in default AND its provenance.
        assert_eq!(
            c.lane.eject_ttl_ticks,
            crate::lane::LaneConfig::default().eject_ttl_ticks
        );
        assert_eq!(c.provenance.eject_ttl_ticks, Source::Default);

        // env overrides tf.toml for max_members only.
        let env = |k: &str| match k {
            "CARGOLESS_LANE_MAX_MEMBERS" => Some("40".to_string()),
            _ => None,
        };
        let c = LaneSettings::resolve_layered(&dir, &env).unwrap();
        assert_eq!(c.lane.max_members, 40);
        assert_eq!(c.provenance.max_members, Source::Env);
        // The field env did NOT set must still come from tf.toml.
        assert_eq!(c.lane.capture_window_ticks, 300);
        assert_eq!(c.provenance.capture_window_ticks, Source::TfToml);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lane_tf_toml_overlay_is_tolerant_of_foreign_keys() {
        // The same shared file carries other readers' sections, and a future
        // additive `[lane]` key must never break an older binary.
        let dir = lane_dir("tolerant");
        std::fs::write(
            dir.join("tf.toml"),
            "[project]\nroot = \".\"\nstate_dir = \".cargoless\"\n\
             [cache]\ndir = \"/tmp/c\"\n\
             [fleet]\nrepo = \"/workspace/repo\"\n\
             [telemetry]\notel_endpoint = \"http://otel:4317\"\n\
             [serve]\nport = 8080\n\
             [lane]\nmax_members = 12\nmax_batch_bytes = 1\n",
        )
        .unwrap();
        let c = LaneSettings::resolve_layered(&dir, &no_env).unwrap();
        assert_eq!(c.lane.max_members, 12);
        assert_eq!(
            c.lane.capture_window_ticks,
            crate::lane::LaneConfig::default().capture_window_ticks
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lane_malformed_value_is_actionable_and_names_the_right_key() {
        // tf.toml keeps the line number.
        let dir = lane_dir("malformed");
        std::fs::write(dir.join("tf.toml"), "[lane]\nmax_members = \"ten\"\n").unwrap();
        let err = LaneSettings::resolve_layered(&dir, &no_env).unwrap_err();
        assert!(matches!(err, FleetConfigError::BadTfToml { .. }));
        assert!(
            err.to_string().contains("max_members"),
            "message must name the failing key: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);

        // env reports the key that ACTUALLY failed — the anti-`parse_sampler_arg`
        // assertion. A shared helper that hardcodes one key would name the wrong
        // field here, and five numeric settings make that a real hazard.
        let env = |k: &str| (k == "CARGOLESS_LANE_INFRA_MAX_ATTEMPTS").then(|| "nope".to_string());
        let err = LaneSettings::resolve_layered(Path::new("/nonexistent"), &env).unwrap_err();
        assert!(matches!(err, FleetConfigError::BadLaneSetting { .. }));
        let msg = err.to_string();
        assert!(
            msg.contains("infra_max_attempts"),
            "message must name the failing key: {msg}"
        );
        assert!(
            !msg.contains("`max_members` = `nope`"),
            "message must not name a different key: {msg}"
        );
    }

    #[test]
    fn lane_illegal_zeros_are_refused_but_a_zero_capture_window_is_legal() {
        // Deliberately NOT a uniform "every field must be > 0": a zero capture
        // window means "build immediately", which is what a single-developer
        // repo and every lane unit test use. The other four zeros each wedge
        // the lane in a different way.
        for key in [
            "max_members",
            "infra_max_attempts",
            "infra_backoff_ticks",
            "eject_ttl_ticks",
        ] {
            let dir = lane_dir(&format!("zero-{key}"));
            std::fs::write(dir.join("tf.toml"), format!("[lane]\n{key} = 0\n")).unwrap();
            let Err(err) = LaneSettings::resolve_layered(&dir, &no_env) else {
                panic!("`{key} = 0` must be refused");
            };
            assert!(
                err.to_string().contains(key),
                "refusal must name `{key}`: {err}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        let dir = lane_dir("zero-window");
        std::fs::write(dir.join("tf.toml"), "[lane]\ncapture_window_ticks = 0\n").unwrap();
        let c = LaneSettings::resolve_layered(&dir, &no_env)
            .expect("a zero capture window is the documented build-immediately mode");
        assert_eq!(c.lane.capture_window_ticks, 0);
        assert_eq!(c.provenance.capture_window_ticks, Source::TfToml);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lane_env_family_is_cargoless_lane_not_tf() {
        // The lane's env family is `CARGOLESS_LANE_*` (matching _PROFILE,
        // _BASE, _ARTIFACT...); `TF_*` is the older fleet-only prefix. Pins the
        // convention against a future "helpful" alias.
        let env = |k: &str| (k == "TF_LANE_MAX_MEMBERS").then(|| "99".to_string());
        let c = LaneSettings::resolve_layered(Path::new("/nonexistent"), &env).unwrap();
        assert_eq!(
            c.lane.max_members,
            crate::lane::LaneConfig::default().max_members
        );
        assert_eq!(c.provenance.max_members, Source::Default);
    }
}
