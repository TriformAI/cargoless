//! The app-serve **instances file** — the daemon-side operator config that
//! names *which* refs to serve and *where*. The per-commit
//! `cargoless.app.yaml` ([`crate::appmanifest`]) answers *how to build and
//! run*; this file answers *what* and is read once at daemon startup (a
//! ConfigMap in the k8s deployment):
//!
//! ```yaml
//! version: 1
//! instances:
//!   - name: dev
//!     ref: origin/dev
//!     app_bind: "0.0.0.0:8080"
//!     env:
//!       TRIFORM_PUBLIC_BASE_URL: "https://dev.preview.triform.dev"
//!   - name: feature-x
//!     ref: origin/feature/x
//!     app_bind: "0.0.0.0:8081"
//!     env:
//!       DATABASE_URL: "${PREVIEW_FEATURE_X_DATABASE_URL}"
//! ```
//!
//! ## `${VAR}` interpolation — secrets stay in pod env
//!
//! Env *values* may reference the daemon's own environment as `${VAR}`. The
//! file itself never holds a secret: the k8s Secret exposes
//! `PREVIEW_FEATURE_X_DATABASE_URL` to the pod, and the ConfigMap'd instances
//! file stays committable/loggable. Interpolation is **strict** — an
//! unresolvable `${VAR}` is a startup error, never an empty string. A
//! preview app silently booting with `DATABASE_URL=""` would fail far from
//! the actual mistake (the missing Secret key); failing at parse names it.
//!
//! Same hand-rolled YAML subset as every other cargoless config
//! (`yamlscan`): version gate, unknown keys rejected, line-attributed errors.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::yamlscan::{
    ParseError, YamlNode, check_env_key, check_label, parse_yaml_value, reject_unknown,
    required_string,
};

/// One configured instance: a named ref served on a fixed bind address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpec {
    /// Unique short name (`[A-Za-z0-9_-]+`) — keys worktrees, state dirs,
    /// `/status?worktree=<name>`, and telemetry attributes.
    pub name: String,
    /// Git ref the instance tracks (e.g. `origin/dev`). The daemon resolves
    /// it to a sha each poll; the ref is never interpreted by this module.
    pub git_ref: String,
    /// Where this instance's L4 proxy listens. Fixed per instance (a k8s
    /// Service targets it); the *app* port behind the proxy is
    /// daemon-allocated and never appears here.
    pub app_bind: SocketAddr,
    /// Per-instance env overlay for the app child, `${VAR}` already resolved.
    pub env: BTreeMap<String, String>,
}

/// The parsed instances file. Order is preserved (boot order = file order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancesFile {
    pub instances: Vec<InstanceSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancesError {
    pub path: PathBuf,
    pub line: usize,
    pub message: String,
}

impl fmt::Display for InstancesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.path.display(), self.line, self.message)
    }
}

impl std::error::Error for InstancesError {}

/// Load and parse `path`, resolving `${VAR}` references through `lookup`
/// (production: `|k| std::env::var(k).ok()`; tests inject a map). Unlike the
/// manifests, an instances file the operator pointed at must exist —
/// absence is an error, not `None`.
pub fn load_instances(
    path: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<InstancesFile, InstancesError> {
    let text = fs::read_to_string(path).map_err(|e| InstancesError {
        path: path.to_path_buf(),
        line: 1,
        message: format!("could not read instances file: {e}"),
    })?;
    parse_instances(path.to_path_buf(), &text, lookup)
}

pub fn parse_instances(
    path: PathBuf,
    text: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<InstancesFile, InstancesError> {
    parse_inner(text, lookup).map_err(|e| InstancesError {
        path,
        line: e.line,
        message: e.message,
    })
}

fn parse_inner(
    text: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<InstancesFile, ParseError> {
    let root = parse_yaml_value(text)?;
    let map = root.expect_map("instances file root")?;
    reject_unknown(map, &["version", "instances"], root.line())?;
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
            message: format!("unsupported instances file version {version}"),
        });
    }
    let list = map.get("instances").ok_or(ParseError {
        line: 1,
        message: "instances: list is required".to_string(),
    })?;
    let mut instances = Vec::new();
    for item in list.expect_list("instances")? {
        let entry = item.expect_map("instances[]")?;
        reject_unknown(entry, &["name", "ref", "app_bind", "env"], item.line())?;
        let name = required_string(entry, "name", item.line())?;
        check_label(&name, "instances[].name", item.line())?;
        let git_ref = required_string(entry, "ref", item.line())?;
        if git_ref.trim().is_empty() {
            return Err(ParseError {
                line: item.line(),
                message: "instances[].ref must not be empty".to_string(),
            });
        }
        let bind_text = required_string(entry, "app_bind", item.line())?;
        let app_bind: SocketAddr = bind_text.parse().map_err(|_| ParseError {
            line: item.line(),
            message: format!("instances[].app_bind must be `<ip>:<port>`, got `{bind_text}`"),
        })?;
        let mut env = BTreeMap::new();
        if let Some(env_node) = entry.get("env") {
            for (key, value) in env_node.expect_map("instances[].env")? {
                check_env_key(key, "instances[].env key", env_node.line())?;
                let raw = value.expect_string()?;
                let resolved = interpolate(&raw, lookup).map_err(|message| ParseError {
                    line: env_node.line(),
                    message: format!("instances[].env `{key}`: {message}"),
                })?;
                env.insert(key.clone(), resolved);
            }
        }
        instances.push(InstanceSpec {
            name,
            git_ref,
            app_bind,
            env,
        });
    }
    if instances.is_empty() {
        return Err(ParseError {
            line: list.line(),
            message: "instances: list must not be empty".to_string(),
        });
    }
    let mut names = BTreeSet::new();
    let mut binds = BTreeSet::new();
    for inst in &instances {
        if !names.insert(inst.name.clone()) {
            return Err(ParseError {
                line: 1,
                message: format!("duplicate instance name `{}`", inst.name),
            });
        }
        if !binds.insert(inst.app_bind) {
            return Err(ParseError {
                line: 1,
                message: format!(
                    "duplicate app_bind `{}` (instance `{}`)",
                    inst.app_bind, inst.name
                ),
            });
        }
    }
    Ok(InstancesFile { instances })
}

/// Resolve every `${VAR}` in `raw` via `lookup`. Strict: an unknown var or a
/// malformed reference is an `Err(message)`. No escape syntax — a literal
/// `${` cannot appear in a value (acceptable for this config surface, and
/// far better than an ambiguous escape rule).
fn interpolate(raw: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("unclosed `${{` in `{raw}`"));
        };
        let var = &after[..end];
        if var.is_empty() || !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("invalid variable reference `${{{var}}}`"));
        }
        match lookup(var) {
            Some(value) => out.push_str(&value),
            None => {
                return Err(format!("`${{{var}}}` is not set in the daemon environment"));
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn parse(text: &str, env: &[(&str, &str)]) -> Result<InstancesFile, InstancesError> {
        let look = lookup(env);
        parse_instances(PathBuf::from("instances.yaml"), text, &look)
    }

    const TWO: &str = r#"
version: 1
instances:
  - name: dev
    ref: origin/dev
    app_bind: "0.0.0.0:8080"
    env:
      TRIFORM_PUBLIC_BASE_URL: "https://dev.preview.triform.dev"
  - name: feature-x
    ref: origin/feature/x
    app_bind: "0.0.0.0:8081"
    env:
      DATABASE_URL: "${PREVIEW_DB}"
      MIXED: "postgres://${PREVIEW_DB_USER}@db/${PREVIEW_DB_NAME}"
"#;

    #[test]
    fn two_instances_parse_with_interpolation() {
        let f = parse(
            TWO,
            &[
                ("PREVIEW_DB", "postgres://preview"),
                ("PREVIEW_DB_USER", "u1"),
                ("PREVIEW_DB_NAME", "preview_feature_x"),
            ],
        )
        .unwrap();
        assert_eq!(f.instances.len(), 2);

        let dev = &f.instances[0];
        assert_eq!(dev.name, "dev");
        assert_eq!(dev.git_ref, "origin/dev");
        assert_eq!(dev.app_bind, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(
            dev.env["TRIFORM_PUBLIC_BASE_URL"],
            "https://dev.preview.triform.dev"
        );

        let fx = &f.instances[1];
        assert_eq!(fx.env["DATABASE_URL"], "postgres://preview");
        assert_eq!(fx.env["MIXED"], "postgres://u1@db/preview_feature_x");
    }

    #[test]
    fn missing_variable_fails_loudly_never_empty() {
        let err = parse(TWO, &[("PREVIEW_DB", "x")]).unwrap_err();
        assert!(
            err.message.contains("PREVIEW_DB_USER")
                && err.message.contains("not set in the daemon environment"),
            "names the missing variable: {}",
            err.message
        );
    }

    #[test]
    fn malformed_references_are_rejected() {
        let unclosed = parse(
            "version: 1\ninstances:\n  - name: a\n    ref: origin/a\n    app_bind: \"127.0.0.1:1\"\n    env:\n      X: \"${OOPS\"\n",
            &[],
        )
        .unwrap_err();
        assert!(unclosed.message.contains("unclosed"));

        let bad_name = parse(
            "version: 1\ninstances:\n  - name: a\n    ref: origin/a\n    app_bind: \"127.0.0.1:1\"\n    env:\n      X: \"${BAD-NAME}\"\n",
            &[],
        )
        .unwrap_err();
        assert!(bad_name.message.contains("invalid variable reference"));
    }

    #[test]
    fn duplicate_names_and_binds_are_rejected() {
        let dup_name = parse(
            "version: 1\ninstances:\n  - name: a\n    ref: origin/a\n    app_bind: \"127.0.0.1:1\"\n  - name: a\n    ref: origin/b\n    app_bind: \"127.0.0.1:2\"\n",
            &[],
        )
        .unwrap_err();
        assert!(dup_name.message.contains("duplicate instance name `a`"));

        let dup_bind = parse(
            "version: 1\ninstances:\n  - name: a\n    ref: origin/a\n    app_bind: \"127.0.0.1:1\"\n  - name: b\n    ref: origin/b\n    app_bind: \"127.0.0.1:1\"\n",
            &[],
        )
        .unwrap_err();
        assert!(dup_bind.message.contains("duplicate app_bind"));
    }

    #[test]
    fn structural_errors_are_attributed() {
        let no_version = parse("instances:\n  - name: a\n", &[]).unwrap_err();
        assert!(no_version.message.contains("version: 1 is required"));

        let empty = parse("version: 1\ninstances: []\n", &[]).unwrap_err();
        assert!(empty.message.contains("must not be empty"));

        let unknown = parse(
            "version: 1\ninstances:\n  - name: a\n    ref: origin/a\n    app_bind: \"127.0.0.1:1\"\n    port: 99\n",
            &[],
        )
        .unwrap_err();
        assert!(unknown.message.contains("unknown key `port`"));

        let bad_bind = parse(
            "version: 1\ninstances:\n  - name: a\n    ref: origin/a\n    app_bind: \"not-an-addr\"\n",
            &[],
        )
        .unwrap_err();
        assert!(bad_bind.message.contains("app_bind must be"));

        let bad_label = parse(
            "version: 1\ninstances:\n  - name: \"a/b\"\n    ref: origin/a\n    app_bind: \"127.0.0.1:1\"\n",
            &[],
        )
        .unwrap_err();
        assert!(bad_label.message.contains("[A-Za-z0-9_-]+"));
    }

    #[test]
    fn load_missing_file_is_an_error() {
        let look = lookup(&[]);
        let err = load_instances(Path::new("/nonexistent/instances.yaml"), &look).unwrap_err();
        assert!(err.message.contains("could not read instances file"));
    }
}
