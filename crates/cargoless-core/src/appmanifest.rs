//! `cargoless.app.yaml` — the app-serve build/run manifest.
//!
//! The manifest **rides the commit**: `app-serve` reads it from the instance
//! worktree at the sha it is about to build, never from daemon config. A
//! branch can therefore evolve its own pipeline (add a build step, change the
//! health path) and the change takes effect exactly when that branch's HEAD
//! does — the same provenance discipline as `cargoless.checks.yaml`
//! ([`crate::project_checks`]): version gate, unknown-key rejection, and a
//! sha256 `manifest_hash` recorded with every build.
//!
//! Uses the same hand-rolled YAML subset (`yamlscan`) — **block form only**,
//! no inline `{...}` maps:
//!
//! ```yaml
//! version: 1
//! app:
//!   name: triform-server
//! build:
//!   steps:
//!     - id: server
//!       command: ["./scripts/build-server.sh"]
//!       timeout_ms: 2700000
//!   artifacts: ["target/release/triform-server", "site"]
//! run:
//!   command: ["./scripts/run.sh"]
//!   env:
//!     RUST_LOG: info
//!   port_env: PORT
//! health:
//!   path: /health
//!   ready_timeout_ms: 120000
//!   interval_ms: 1000
//! drain:
//!   grace_ms: 30000
//! ```
//!
//! Everything except `version` and `run.command` has a default; the parser
//! rejects what it does not understand rather than guessing (a typo'd key in
//! a build pipeline must fail the build *loudly*, not silently skip a step).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cargoless_cas::sha256_hex;

use crate::yamlscan::{
    ParseError, YamlNode, check_env_key, check_label, get_string, get_string_list, get_u64,
    parse_yaml_value, reject_unknown, required_string,
};

/// File name probed at the worktree root, mirroring `cargoless.checks.yaml`.
pub const APP_MANIFEST_NAME: &str = "cargoless.app.yaml";

/// Parsed, validated `cargoless.app.yaml`. Plain data — the daemon in the
/// `cargoless` bin crate interprets it; nothing here does I/O beyond loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppManifest {
    /// `app.name` — telemetry/log label. Defaults to `"app"`.
    pub app_name: String,
    pub build: BuildSpec,
    pub run: RunSpec,
    pub health: HealthSpec,
    pub drain: DrainSpec,
    /// sha256 of the manifest text — recorded in bundle meta so every build
    /// names the exact pipeline that produced it.
    pub manifest_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSpec {
    /// Ordered steps, run sequentially in the worktree; first failure ⇒ red.
    pub steps: Vec<BuildStep>,
    /// Worktree-relative paths harvested into the bundle after a green build.
    pub artifacts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildStep {
    pub id: String,
    pub command: Vec<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    pub command: Vec<String>,
    /// Extra environment for the app child (instance env overlays on top).
    pub env: BTreeMap<String, String>,
    /// Env var through which the daemon hands the app its allocated port.
    pub port_env: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthSpec {
    /// HTTP path polled on the app port; any 200 ⇒ healthy.
    pub path: String,
    /// Total budget from spawn to first 200 before the boot is declared red.
    pub ready_timeout_ms: u64,
    pub interval_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainSpec {
    /// How long a demoted child keeps its existing connections before SIGTERM.
    pub grace_ms: u64,
}

/// Manifest load/parse failure, attributed to file + line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppManifestError {
    pub path: PathBuf,
    pub line: usize,
    pub message: String,
}

impl fmt::Display for AppManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.path.display(), self.line, self.message)
    }
}

impl std::error::Error for AppManifestError {}

/// Read `<root>/cargoless.app.yaml`. `Ok(None)` ⇒ the repo does not opt in
/// to app-serve at this sha (absence is a state, not an error — the
/// `load_manifest` convention).
pub fn load_app_manifest(root: &Path) -> Result<Option<AppManifest>, AppManifestError> {
    let path = root.join(APP_MANIFEST_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(AppManifestError {
                path: PathBuf::from(APP_MANIFEST_NAME),
                line: 1,
                message: format!("could not read {APP_MANIFEST_NAME}: {e}"),
            });
        }
    };
    parse_app_manifest(PathBuf::from(APP_MANIFEST_NAME), &text).map(Some)
}

pub fn parse_app_manifest(path: PathBuf, text: &str) -> Result<AppManifest, AppManifestError> {
    parse_inner(text)
        .map(|mut m| {
            m.manifest_hash = sha256_hex(text.as_bytes());
            m
        })
        .map_err(|e| AppManifestError {
            path,
            line: e.line,
            message: e.message,
        })
}

fn parse_inner(text: &str) -> Result<AppManifest, ParseError> {
    let root = parse_yaml_value(text)?;
    let map = root.expect_map("manifest root")?;
    reject_unknown(
        map,
        &["version", "app", "build", "run", "health", "drain"],
        root.line(),
    )?;
    let version = map
        .get("version")
        .and_then(YamlNode::as_i64)
        .ok_or(ParseError {
            line: 1,
            message: "version: 1 is required".to_string(),
        })?;
    if version != 1 {
        return Err(ParseError {
            line: 1,
            message: format!("unsupported app manifest version {version}"),
        });
    }
    Ok(AppManifest {
        app_name: parse_app(map.get("app"))?,
        build: parse_build(map.get("build"))?,
        run: parse_run(map.get("run"))?,
        health: parse_health(map.get("health"))?,
        drain: parse_drain(map.get("drain"))?,
        manifest_hash: String::new(), // filled by parse_app_manifest
    })
}

fn parse_app(node: Option<&YamlNode>) -> Result<String, ParseError> {
    let Some(node) = node else {
        return Ok("app".to_string());
    };
    let map = node.expect_map("app")?;
    reject_unknown(map, &["name"], node.line())?;
    let name = required_string(map, "name", node.line())?;
    if name.trim().is_empty() {
        return Err(ParseError {
            line: node.line(),
            message: "app.name must not be empty".to_string(),
        });
    }
    Ok(name)
}

fn parse_build(node: Option<&YamlNode>) -> Result<BuildSpec, ParseError> {
    let Some(node) = node else {
        return Ok(BuildSpec {
            steps: Vec::new(),
            artifacts: Vec::new(),
        });
    };
    let map = node.expect_map("build")?;
    reject_unknown(map, &["steps", "artifacts"], node.line())?;
    let mut steps = Vec::new();
    if let Some(list) = map.get("steps") {
        for item in list.expect_list("build.steps")? {
            let step = item.expect_map("build.steps[]")?;
            reject_unknown(step, &["id", "command", "timeout_ms"], item.line())?;
            let id = required_string(step, "id", item.line())?;
            check_label(&id, "build.steps[].id", item.line())?;
            let command = get_string_list(step, "command")?.unwrap_or_default();
            check_command(&command, "build.steps[].command", item.line())?;
            let timeout_ms = get_u64(step, "timeout_ms")?.unwrap_or(1_800_000);
            if timeout_ms == 0 {
                return Err(ParseError {
                    line: item.line(),
                    message: "build.steps[].timeout_ms must be greater than zero".to_string(),
                });
            }
            steps.push(BuildStep {
                id,
                command,
                timeout_ms,
            });
        }
    }
    let mut ids = BTreeSet::new();
    for step in &steps {
        if !ids.insert(step.id.clone()) {
            return Err(ParseError {
                line: node.line(),
                message: format!("duplicate build step id `{}`", step.id),
            });
        }
    }
    let artifacts = get_string_list(map, "artifacts")?.unwrap_or_default();
    for a in &artifacts {
        check_rel_path(a, "build.artifacts", node.line())?;
    }
    Ok(BuildSpec { steps, artifacts })
}

fn parse_run(node: Option<&YamlNode>) -> Result<RunSpec, ParseError> {
    let node = node.ok_or(ParseError {
        line: 1,
        message: "run: section is required".to_string(),
    })?;
    let map = node.expect_map("run")?;
    reject_unknown(map, &["command", "env", "port_env"], node.line())?;
    let command = get_string_list(map, "command")?.unwrap_or_default();
    check_command(&command, "run.command", node.line())?;
    let port_env = get_string(map, "port_env")?.unwrap_or_else(|| "PORT".to_string());
    check_env_key(&port_env, "run.port_env", node.line())?;
    let mut env = BTreeMap::new();
    if let Some(env_node) = map.get("env") {
        for (key, value) in env_node.expect_map("run.env")? {
            check_env_key(key, "run.env key", env_node.line())?;
            env.insert(key.clone(), value.expect_string()?);
        }
    }
    if env.contains_key(&port_env) {
        return Err(ParseError {
            line: node.line(),
            message: format!(
                "run.env must not set `{port_env}` — the daemon owns the port assignment"
            ),
        });
    }
    Ok(RunSpec {
        command,
        env,
        port_env,
    })
}

fn parse_health(node: Option<&YamlNode>) -> Result<HealthSpec, ParseError> {
    let mut spec = HealthSpec {
        path: "/".to_string(),
        ready_timeout_ms: 120_000,
        interval_ms: 1_000,
    };
    let Some(node) = node else {
        return Ok(spec);
    };
    let map = node.expect_map("health")?;
    reject_unknown(
        map,
        &["path", "ready_timeout_ms", "interval_ms"],
        node.line(),
    )?;
    if let Some(path) = get_string(map, "path")? {
        if !path.starts_with('/') {
            return Err(ParseError {
                line: node.line(),
                message: "health.path must start with `/`".to_string(),
            });
        }
        spec.path = path;
    }
    if let Some(v) = get_u64(map, "ready_timeout_ms")? {
        spec.ready_timeout_ms = v;
    }
    if let Some(v) = get_u64(map, "interval_ms")? {
        spec.interval_ms = v;
    }
    if spec.ready_timeout_ms == 0 || spec.interval_ms == 0 {
        return Err(ParseError {
            line: node.line(),
            message: "health timings must be greater than zero".to_string(),
        });
    }
    Ok(spec)
}

fn parse_drain(node: Option<&YamlNode>) -> Result<DrainSpec, ParseError> {
    let Some(node) = node else {
        return Ok(DrainSpec { grace_ms: 30_000 });
    };
    let map = node.expect_map("drain")?;
    reject_unknown(map, &["grace_ms"], node.line())?;
    Ok(DrainSpec {
        grace_ms: get_u64(map, "grace_ms")?.unwrap_or(30_000),
    })
}

fn check_command(command: &[String], what: &str, line: usize) -> Result<(), ParseError> {
    if command.is_empty() || command[0].trim().is_empty() {
        return Err(ParseError {
            line,
            message: format!("{what} must be a non-empty command list"),
        });
    }
    Ok(())
}

/// Bundle-harvest paths must stay inside the worktree: relative, no `..`.
fn check_rel_path(s: &str, what: &str, line: usize) -> Result<(), ParseError> {
    let bad = s.is_empty()
        || s.starts_with('/')
        || s.split('/').any(|part| part == "..")
        || s.contains('\\');
    if bad {
        return Err(ParseError {
            line,
            message: format!("{what} entries must be worktree-relative without `..`, got `{s}`"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<AppManifest, AppManifestError> {
        parse_app_manifest(PathBuf::from(APP_MANIFEST_NAME), text)
    }

    const FULL: &str = r#"
version: 1
app:
  name: triform-server
build:
  steps:
    - id: server
      command: ["./scripts/build-server.sh"]
      timeout_ms: 2700000
    - id: portal
      command: ["./scripts/build-portal.sh", "--release"]
  artifacts: ["target/release/triform-server", "site"]
run:
  command: ["./scripts/run.sh"]
  env:
    RUST_LOG: info
    TRIFORM_ENV: staging
  port_env: TRIFORM_PORT
health:
  path: /health
  ready_timeout_ms: 120000
  interval_ms: 1000
drain:
  grace_ms: 30000
"#;

    #[test]
    fn full_manifest_parses_with_provenance_hash() {
        let m = parse(FULL).unwrap();
        assert_eq!(m.app_name, "triform-server");
        assert_eq!(m.build.steps.len(), 2);
        assert_eq!(m.build.steps[0].id, "server");
        assert_eq!(m.build.steps[0].timeout_ms, 2_700_000);
        assert_eq!(m.build.steps[1].timeout_ms, 1_800_000, "default applies");
        assert_eq!(
            m.build.artifacts,
            vec!["target/release/triform-server", "site"]
        );
        assert_eq!(m.run.command, vec!["./scripts/run.sh"]);
        assert_eq!(m.run.port_env, "TRIFORM_PORT");
        assert_eq!(m.run.env["RUST_LOG"], "info");
        assert_eq!(m.health.path, "/health");
        assert_eq!(m.drain.grace_ms, 30_000);
        assert_eq!(m.manifest_hash, sha256_hex(FULL.as_bytes()));
    }

    #[test]
    fn minimal_manifest_gets_defaults() {
        let m = parse("version: 1\nrun:\n  command: [\"./app\"]\n").unwrap();
        assert_eq!(m.app_name, "app");
        assert!(m.build.steps.is_empty());
        assert!(m.build.artifacts.is_empty());
        assert_eq!(m.run.port_env, "PORT");
        assert_eq!(m.health.path, "/");
        assert_eq!(m.health.ready_timeout_ms, 120_000);
        assert_eq!(m.health.interval_ms, 1_000);
        assert_eq!(m.drain.grace_ms, 30_000);
    }

    #[test]
    fn version_gate_and_unknown_keys_reject() {
        let no_version = parse("run:\n  command: [\"./app\"]\n").unwrap_err();
        assert!(no_version.message.contains("version: 1 is required"));

        let v2 = parse("version: 2\nrun:\n  command: [\"./app\"]\n").unwrap_err();
        assert!(v2.message.contains("unsupported app manifest version 2"));

        let unknown = parse("version: 1\nserve: {}\nrun:\n  command: [\"./app\"]\n").unwrap_err();
        assert!(unknown.message.contains("unknown key `serve`"));

        let nested =
            parse("version: 1\nrun:\n  command: [\"./app\"]\n  restart: always\n").unwrap_err();
        assert!(nested.message.contains("unknown key `restart`"));
    }

    #[test]
    fn run_section_is_required_and_validated() {
        let missing = parse("version: 1\n").unwrap_err();
        assert!(missing.message.contains("run: section is required"));

        let empty_cmd = parse("version: 1\nrun:\n  command: []\n").unwrap_err();
        assert!(empty_cmd.message.contains("non-empty command list"));

        let port_clash =
            parse("version: 1\nrun:\n  command: [\"./app\"]\n  env:\n    PORT: 80\n").unwrap_err();
        assert!(
            port_clash
                .message
                .contains("daemon owns the port assignment")
        );
    }

    #[test]
    fn build_steps_validate_ids_timeouts_and_artifacts() {
        let dup = parse(
            "version: 1\nbuild:\n  steps:\n    - id: a\n      command: [\"x\"]\n    - id: a\n      command: [\"y\"]\nrun:\n  command: [\"./app\"]\n",
        )
        .unwrap_err();
        assert!(dup.message.contains("duplicate build step id `a`"));

        let zero = parse(
            "version: 1\nbuild:\n  steps:\n    - id: a\n      command: [\"x\"]\n      timeout_ms: 0\nrun:\n  command: [\"./app\"]\n",
        )
        .unwrap_err();
        assert!(
            zero.message
                .contains("timeout_ms must be greater than zero")
        );

        let escape = parse(
            "version: 1\nbuild:\n  artifacts: [\"../secrets\"]\nrun:\n  command: [\"./app\"]\n",
        )
        .unwrap_err();
        assert!(escape.message.contains("without `..`"));

        let absolute = parse(
            "version: 1\nbuild:\n  artifacts: [\"/etc/passwd\"]\nrun:\n  command: [\"./app\"]\n",
        )
        .unwrap_err();
        assert!(absolute.message.contains("worktree-relative"));
    }

    #[test]
    fn health_validates_path_and_timings() {
        let bad_path = parse("version: 1\nrun:\n  command: [\"./app\"]\nhealth:\n  path: health\n")
            .unwrap_err();
        assert!(bad_path.message.contains("must start with `/`"));

        let zero = parse("version: 1\nrun:\n  command: [\"./app\"]\nhealth:\n  interval_ms: 0\n")
            .unwrap_err();
        assert!(zero.message.contains("greater than zero"));
    }

    #[test]
    fn load_absent_manifest_is_none_not_error() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("cargoless-appmanifest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(load_app_manifest(&dir).unwrap(), None);

        fs::write(
            dir.join(APP_MANIFEST_NAME),
            "version: 1\nrun:\n  command: [\"./app\"]\n",
        )
        .unwrap();
        let m = load_app_manifest(&dir).unwrap().expect("manifest present");
        assert_eq!(m.run.command, vec!["./app"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
