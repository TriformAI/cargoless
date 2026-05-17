//! Project configuration: `tf.toml` (decision **D6**) + **zero-config
//! auto-detection** (decision **D7**).
//!
//! The whole point of D7 is that the headline command works with *no* config
//! file: point cargoless at a Rust + WASM (Leptos CSR) project and it infers
//! sane defaults. `tf.toml`, when present, is the explicit override (D6) and
//! is authoritative over auto-detection.
//!
//! ## Why a hand-rolled `tf.toml` reader (no `toml`/`serde`)
//!
//! The schema is a dozen scalar keys. Pulling `toml` + `serde` would add a
//! large proc-macro dependency tree to the one crate that is the cold-build
//! entry point users measure AC#1 against, for a parser we can write in ~40
//! lines. This matches the house style (tf-proto is dependency-free; the
//! watcher hand-rolls its gitignore/debounce rather than pulling crates).
//! The reader supports exactly the documented surface and rejects the rest
//! with an actionable error rather than silently ignoring it.
//!
//! ## Detection is structural, not a full TOML parse
//!
//! Auto-detection only needs two yes/no facts out of `Cargo.toml`: does the
//! crate produce a `cdylib`, and does it depend on `leptos`. A
//! section-scoped key scan answers both without taking on a TOML dependency
//! or pretending to understand arbitrary manifests.

use std::fmt;
use std::path::{Path, PathBuf};

/// Resolved, ready-to-run project configuration. Every field has a defined
/// value after [`Config::resolve`] — auto-detection or `tf.toml` fills them,
/// never `Option` at the point of use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Host the holding page / dev server binds.
    pub host: String,
    /// Port the holding page / dev server binds.
    pub port: u16,
    /// Project + watch root (the directory containing `Cargo.toml`).
    pub root: PathBuf,
    /// Build target triple (always `wasm32-unknown-unknown` for v0 Leptos CSR).
    pub target: String,
    /// Local content-addressed cache directory (what `clean` wipes).
    pub cache_dir: PathBuf,
    /// How this project's identity was established (drives `serve`/`check` UX).
    pub detection: Detection,
}

/// How the project was identified — surfaced verbatim so the user can see
/// *why* cargoless thinks this is a valid target (or trust the explicit file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detection {
    /// `tf.toml` was present and parsed (D6 — explicit, authoritative).
    TfToml,
    /// D7: a `cdylib` crate-type **and** a `leptos` dependency — the canonical
    /// Leptos CSR shape. Highest-confidence auto-detection.
    AutoLeptosCdylib,
    /// `cdylib` crate-type, no `leptos` dependency named (still a WASM lib —
    /// proceed, but say so).
    AutoCdylib,
    /// A `leptos` dependency but no explicit `crate-type` (Trunk-style
    /// `index.html` projects sometimes omit it) — proceed on the dep signal.
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

/// Configuration failure — every variant renders an *actionable* message
/// (what cargoless looked for, where, and the one concrete fix), because a
/// zero-config tool's error is its entire onboarding UX.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// No `Cargo.toml` under the chosen root and no `tf.toml`.
    NoManifest { root: PathBuf },
    /// `Cargo.toml` exists but nothing says "this is a Rust + WASM project".
    NotWasmProject { root: PathBuf },
    /// `tf.toml` present but malformed (line is quoted for the user).
    BadTfToml {
        line_no: usize,
        line: String,
        why: String,
    },
    /// A `tf.toml` value was the wrong shape (e.g. port not a number).
    BadTfTomlValue { key: String, why: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NoManifest { root } => write!(
                f,
                "no Cargo.toml found in {} (and no tf.toml).\n  \
                 cargoless replaces `trunk serve` — run it from your Rust + WASM \
                 project root, or pass --root <dir>.",
                root.display()
            ),
            ConfigError::NotWasmProject { root } => write!(
                f,
                "{}/Cargo.toml is not a recognisable Rust + WASM project.\n  \
                 cargoless looked for a `cdylib` crate-type or a `leptos` \
                 dependency and found neither.\n  Fix one of:\n    \
                 - add `crate-type = [\"cdylib\"]` under [lib], or\n    \
                 - add a `leptos` dependency, or\n    \
                 - create a tf.toml to configure the project explicitly.",
                root.display()
            ),
            ConfigError::BadTfToml { line_no, line, why } => write!(
                f,
                "tf.toml: {why} (line {line_no}: `{line}`).\n  \
                 Supported: [serve] host/port, [project] root/target, \
                 [cache] dir. Comments start with `#`."
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
            host: "127.0.0.1".to_string(),
            port: 8080,
            root,
            target: "wasm32-unknown-unknown".to_string(),
            cache_dir,
            detection,
        }
    }

    /// Resolve configuration for `root`.
    ///
    /// Order (D6 over D7): if `<root>/tf.toml` exists it is parsed and is
    /// authoritative; otherwise `<root>/Cargo.toml` is structurally inspected
    /// for the Rust + WASM signal. Either way every field ends up populated.
    pub fn resolve(root: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let root = root.as_ref().to_path_buf();
        let tf_toml = root.join("tf.toml");

        if let Ok(text) = std::fs::read_to_string(&tf_toml) {
            // tf.toml authoritative (D6). Default to TfToml detection; if a
            // Cargo.toml is also present we still trust the explicit file.
            let mut cfg = Self::defaults(root.clone(), Detection::TfToml);
            apply_tf_toml(&mut cfg, &text)?;
            return Ok(cfg);
        }

        // Zero-config (D7): structural detection from Cargo.toml.
        let manifest = root.join("Cargo.toml");
        let Ok(cargo_text) = std::fs::read_to_string(&manifest) else {
            return Err(ConfigError::NoManifest { root });
        };
        match detect_from_cargo_toml(&cargo_text) {
            Some(detection) => Ok(Self::defaults(root, detection)),
            None => Err(ConfigError::NotWasmProject { root }),
        }
    }
}

/// Structural Rust + WASM detection from a `Cargo.toml`'s text (D7).
///
/// Returns the [`Detection`] confidence, or `None` if neither a `cdylib`
/// crate-type nor a `leptos` dependency is present. Pure over the text so it
/// is exhaustively unit-tested without a filesystem.
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
        // crate-type can live under [lib] or [[bin]]-adjacent [lib]; we only
        // care that *some* [lib]/[lib]-ish section declares cdylib.
        if section == "lib" && line.starts_with("crate-type") && line.contains("cdylib") {
            has_cdylib = true;
        }
        // `leptos = ...` or `leptos.workspace = true` under any dependency
        // table ([dependencies], [dev-dependencies], [target.'cfg'...]).
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

/// Strip a `#` comment, respecting `#` inside a double-quoted string so
/// `dir = "a#b"` is not truncated.
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

/// Apply a parsed `tf.toml` over an already-defaulted [`Config`]. Unknown
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
            if !matches!(section.as_str(), "serve" | "project" | "cache") {
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
            ("serve", "host") => cfg.host = val,
            ("serve", "port") => {
                cfg.port = val.parse().map_err(|_| ConfigError::BadTfTomlValue {
                    key: "serve.port".to_string(),
                    why: format!("must be a number 1-65535, got `{val}`"),
                })?
            }
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

/// Strip one matching pair of surrounding double quotes, if present.
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
        let m = r#"
[package]
name = "app"
[lib]
crate-type = ["cdylib", "rlib"]
[dependencies]
leptos = { version = "0.6", features = ["csr"] }
"#;
        assert_eq!(detect_from_cargo_toml(m), Some(Detection::AutoLeptosCdylib));
    }

    #[test]
    fn detects_cdylib_without_leptos_and_leptos_without_cdylib() {
        let cdylib_only = "[lib]\ncrate-type = [\"cdylib\"]\n";
        assert_eq!(
            detect_from_cargo_toml(cdylib_only),
            Some(Detection::AutoCdylib)
        );

        let leptos_only = "[dependencies]\nleptos = \"0.6\"\n";
        assert_eq!(
            detect_from_cargo_toml(leptos_only),
            Some(Detection::AutoLeptosDep)
        );

        let workspace_dep = "[dependencies]\nleptos.workspace = true\n";
        assert_eq!(
            detect_from_cargo_toml(workspace_dep),
            Some(Detection::AutoLeptosDep)
        );
    }

    #[test]
    fn plain_binary_is_not_a_wasm_project() {
        let m = "[package]\nname = \"x\"\n[dependencies]\nserde = \"1\"\n";
        assert_eq!(detect_from_cargo_toml(m), None);
    }

    #[test]
    fn comment_stripping_respects_strings() {
        assert_eq!(strip_comment("port = 8080 # the port"), "port = 8080 ");
        assert_eq!(strip_comment(r#"dir = "a#b""#), r#"dir = "a#b""#);
    }

    #[test]
    fn tf_toml_overrides_defaults() {
        let mut cfg = Config::defaults(PathBuf::from("/p"), Detection::TfToml);
        apply_tf_toml(
            &mut cfg,
            "# my project\n[serve]\nhost = \"0.0.0.0\"\nport = 3000\n[project]\ntarget = \"wasm32-unknown-unknown\"\n[cache]\ndir = \"/tmp/c\"\n",
        )
        .expect("valid tf.toml");
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.port, 3000);
        assert_eq!(cfg.cache_dir, PathBuf::from("/tmp/c"));
    }

    #[test]
    fn tf_toml_rejects_unknown_section_key_and_bad_port() {
        let mut cfg = Config::defaults(PathBuf::from("/p"), Detection::TfToml);
        assert!(matches!(
            apply_tf_toml(&mut cfg, "[nope]\nx = 1\n"),
            Err(ConfigError::BadTfToml { .. })
        ));
        assert!(matches!(
            apply_tf_toml(&mut cfg, "[serve]\nwidth = 5\n"),
            Err(ConfigError::BadTfToml { .. })
        ));
        assert!(matches!(
            apply_tf_toml(&mut cfg, "[serve]\nport = \"notanumber\"\n"),
            Err(ConfigError::BadTfTomlValue { .. })
        ));
    }

    #[test]
    fn errors_are_actionable() {
        let e = ConfigError::NoManifest {
            root: PathBuf::from("/x"),
        };
        let msg = e.to_string();
        assert!(msg.contains("Cargo.toml"));
        assert!(msg.contains("--root"));

        let e2 = ConfigError::NotWasmProject {
            root: PathBuf::from("/x"),
        };
        assert!(e2.to_string().contains("cdylib"));
        assert!(e2.to_string().contains("leptos"));
    }

    #[test]
    fn defaults_are_v0_sane() {
        let c = Config::defaults(PathBuf::from("/proj"), Detection::AutoLeptosCdylib);
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 8080);
        assert_eq!(c.target, "wasm32-unknown-unknown");
        assert_eq!(c.cache_dir, PathBuf::from("/proj/.cargoless/cache"));
    }
}
