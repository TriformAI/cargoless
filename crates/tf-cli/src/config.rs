//! Project configuration: zero-config auto-detection (decision **D7**) +
//! optional `tf.toml` override (decision **D6**).
//!
//! v0 is headless, so there is no host/port — config is just: where is the
//! project, what target, where does the cache live, and how did we identify
//! it. `tf.toml`, when present, overrides the inferred defaults.
//!
//! Hand-rolled reader (no `toml`/`serde`): the schema is a handful of scalar
//! keys; pulling a proc-macro TOML stack into the cold-build entry point
//! users measure AC#1 against — and into a `--locked` lock we cannot
//! regenerate locally — is not worth it. Matches the house style.

use std::fmt;
use std::path::{Path, PathBuf};

/// Resolved, ready-to-run project configuration. Every field is populated
/// after [`Config::resolve`] (detection or `tf.toml`), never optional at use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Project + watch root (directory containing `Cargo.toml`).
    pub root: PathBuf,
    /// Build target triple (v0 Leptos CSR ⇒ `wasm32-unknown-unknown`).
    pub target: String,
    /// Local content-addressed cache directory (what `clean` wipes).
    pub cache_dir: PathBuf,
    /// How the project's identity was established (surfaced in output).
    pub detection: Detection,
}

/// How the project was identified — shown so the user sees *why* cargoless
/// accepted (or was told about) this directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detection {
    /// `tf.toml` present and parsed (explicit, authoritative — D6).
    TfToml,
    /// D7: `cdylib` crate-type **and** a `leptos` dependency (Leptos CSR).
    AutoLeptosCdylib,
    /// `cdylib` crate-type, no `leptos` dependency named.
    AutoCdylib,
    /// `leptos` dependency but no explicit `crate-type`.
    AutoLeptosDep,
}

impl Detection {
    pub fn describe(self) -> &'static str {
        match self {
            Detection::TfToml => "tf.toml (explicit configuration)",
            Detection::AutoLeptosCdylib => "auto-detected: cdylib + leptos (Leptos CSR)",
            Detection::AutoCdylib => "auto-detected: cdylib crate-type (WASM library)",
            Detection::AutoLeptosDep => "auto-detected: leptos dependency",
        }
    }
}

/// Configuration failure — every variant renders one actionable message
/// (what was looked for, where, the concrete fix): a zero-config tool's
/// error *is* its onboarding UX.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    NoManifest {
        root: PathBuf,
    },
    NotWasmProject {
        root: PathBuf,
    },
    BadTfToml {
        line_no: usize,
        line: String,
        why: String,
    },
    BadTfTomlValue {
        key: String,
        why: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NoManifest { root } => write!(
                f,
                "no Cargo.toml in {} (and no tf.toml).\n  \
                 run cargoless from your Rust + WASM project root, or pass \
                 --root <dir>.",
                root.display()
            ),
            ConfigError::NotWasmProject { root } => write!(
                f,
                "{}/Cargo.toml is not a recognisable Rust + WASM project.\n  \
                 looked for a `cdylib` crate-type or a `leptos` dependency \
                 and found neither.\n  Fix one of:\n    \
                 - add `crate-type = [\"cdylib\"]` under [lib], or\n    \
                 - add a `leptos` dependency, or\n    \
                 - create a tf.toml to configure the project explicitly.",
                root.display()
            ),
            ConfigError::BadTfToml { line_no, line, why } => write!(
                f,
                "tf.toml: {why} (line {line_no}: `{line}`).\n  \
                 Supported: [project] root/target, [cache] dir. `#` comments."
            ),
            ConfigError::BadTfTomlValue { key, why } => {
                write!(f, "tf.toml: key `{key}` {why}.")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    fn defaults(root: PathBuf, detection: Detection) -> Self {
        let cache_dir = root.join(".cargoless").join("cache");
        Self {
            root,
            target: "wasm32-unknown-unknown".to_string(),
            cache_dir,
            detection,
        }
    }

    /// Resolve config for `root`. `tf.toml` (D6) is authoritative if present;
    /// otherwise `Cargo.toml` is structurally inspected for the WASM signal
    /// (D7). Every field ends up populated.
    pub fn resolve(root: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let root = root.as_ref().to_path_buf();

        if let Ok(text) = std::fs::read_to_string(root.join("tf.toml")) {
            let mut cfg = Self::defaults(root.clone(), Detection::TfToml);
            apply_tf_toml(&mut cfg, &text)?;
            return Ok(cfg);
        }

        let Ok(cargo) = std::fs::read_to_string(root.join("Cargo.toml")) else {
            return Err(ConfigError::NoManifest { root });
        };
        match detect_from_cargo_toml(&cargo) {
            Some(d) => Ok(Self::defaults(root, d)),
            None => Err(ConfigError::NotWasmProject { root }),
        }
    }
}

/// Structural Rust + WASM detection from `Cargo.toml` text (D7). Pure over
/// the text so it is exhaustively unit-tested without a filesystem.
pub fn detect_from_cargo_toml(text: &str) -> Option<Detection> {
    let mut section = String::new();
    let mut has_cdylib = false;
    let mut has_leptos = false;

    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        if section == "lib" && line.starts_with("crate-type") && line.contains("cdylib") {
            has_cdylib = true;
        }
        if section.contains("dependencies") {
            let key = line.split(['=', '.']).next().unwrap_or("").trim();
            if key == "leptos" {
                has_leptos = true;
            }
        }
    }

    match (has_cdylib, has_leptos) {
        (true, true) => Some(Detection::AutoLeptosCdylib),
        (true, false) => Some(Detection::AutoCdylib),
        (false, true) => Some(Detection::AutoLeptosDep),
        (false, false) => None,
    }
}

/// Strip a `#` comment, respecting `#` inside a double-quoted string.
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

/// Apply a parsed `tf.toml` over a defaulted [`Config`]. Unknown
/// keys/sections are a hard error (a silently-ignored typo in a zero-config
/// tool is a support nightmare).
pub fn apply_tf_toml(cfg: &mut Config, text: &str) -> Result<(), ConfigError> {
    let mut section = String::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_string();
            if !matches!(section.as_str(), "project" | "cache") {
                return Err(ConfigError::BadTfToml {
                    line_no,
                    line: raw.trim().to_string(),
                    why: format!("unknown section `[{section}]`"),
                });
            }
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            return Err(ConfigError::BadTfToml {
                line_no,
                line: raw.trim().to_string(),
                why: "expected `key = value`".to_string(),
            });
        };
        let key = key.trim();
        let val = unquote(val.trim());
        match (section.as_str(), key) {
            ("project", "root") => cfg.root = PathBuf::from(&val),
            ("project", "target") => cfg.target = val,
            ("cache", "dir") => cfg.cache_dir = PathBuf::from(&val),
            _ => {
                return Err(ConfigError::BadTfToml {
                    line_no,
                    line: raw.trim().to_string(),
                    why: format!("unknown key `{key}` in `[{section}]`"),
                });
            }
        }
    }
    Ok(())
}

fn unquote(s: &str) -> String {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_canonical_leptos_csr() {
        let m = "[lib]\ncrate-type = [\"cdylib\"]\n[dependencies]\nleptos = \"0.6\"\n";
        assert_eq!(detect_from_cargo_toml(m), Some(Detection::AutoLeptosCdylib));
    }

    #[test]
    fn detects_partial_signals() {
        assert_eq!(
            detect_from_cargo_toml("[lib]\ncrate-type=[\"cdylib\"]\n"),
            Some(Detection::AutoCdylib)
        );
        assert_eq!(
            detect_from_cargo_toml("[dependencies]\nleptos.workspace = true\n"),
            Some(Detection::AutoLeptosDep)
        );
        assert_eq!(
            detect_from_cargo_toml("[dependencies]\nserde = \"1\"\n"),
            None
        );
    }

    #[test]
    fn comment_strip_respects_strings() {
        assert_eq!(strip_comment("a = 1 # c"), "a = 1 ");
        assert_eq!(strip_comment(r#"dir = "a#b""#), r#"dir = "a#b""#);
    }

    #[test]
    fn tf_toml_overrides_and_rejects_unknown() {
        let mut c = Config::defaults(PathBuf::from("/p"), Detection::TfToml);
        apply_tf_toml(
            &mut c,
            "[project]\ntarget = \"wasm32-unknown-unknown\"\n[cache]\ndir = \"/tmp/c\"\n",
        )
        .unwrap();
        assert_eq!(c.cache_dir, PathBuf::from("/tmp/c"));
        assert!(matches!(
            apply_tf_toml(&mut c, "[serve]\nport = 1\n"),
            Err(ConfigError::BadTfToml { .. })
        ));
    }

    #[test]
    fn errors_are_actionable() {
        assert!(
            ConfigError::NoManifest {
                root: PathBuf::from("/x")
            }
            .to_string()
            .contains("--root")
        );
        assert!(
            ConfigError::NotWasmProject {
                root: PathBuf::from("/x")
            }
            .to_string()
            .contains("cdylib")
        );
    }
}
