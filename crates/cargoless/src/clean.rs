//! `clean` — remove the local content-addressed cache.
//!
//! The cache is **out-of-tree** (build-cas mandate — see
//! [`crate::config::cache_root`]): by default
//! `${XDG_CACHE_HOME:-$HOME/.cache}/cargoless/<project-key>`. Removing the
//! directory tree is layout-agnostic and safe to own here. The cargoless status
//! file lives in-tree at `<root>/.cargoless/cli-status` — a different place
//! entirely — so `clean` never blinds a running daemon's `status`.
//!
//! Safety: because the cache is now out-of-tree (no longer "must be inside
//! the project"), the guard is **namespace-based** — refuse to recursively
//! delete any path that is not within cargoless's own `cargoless/` cache
//! namespace. That blocks `/`, `$HOME`, the project root, and a careless
//! `[cache] dir` override, while still allowing the real cache.

use std::path::{Component, Path};
use std::process::ExitCode;

use crate::config::Config;
use crate::ui;

/// Unsafe unless the path contains a `cargoless` directory component — i.e.
/// it lives in cargoless's own cache namespace. Empty paths are unsafe.
fn is_unsafe_cache_dir(cache: &Path) -> bool {
    if cache.as_os_str().is_empty() {
        return true;
    }
    !cache
        .components()
        .any(|c| matches!(c, Component::Normal(n) if n == "cargoless"))
}

pub fn run(cfg: &Config) -> ExitCode {
    let cache = &cfg.cache_dir;

    if is_unsafe_cache_dir(cache) {
        ui::error(format!(
            "refusing to clean `{}` — only a path inside cargoless's own \
             cache namespace (a `cargoless/…` directory) is removable. \
             Fix `[cache] dir` in tf.toml or unset it to use the default.",
            cache.display()
        ));
        return ExitCode::from(2);
    }

    match std::fs::metadata(cache) {
        Err(_) => {
            ui::ok(format!("cache already empty ({})", cache.display()));
            ExitCode::SUCCESS
        }
        Ok(_) => match std::fs::remove_dir_all(cache) {
            Ok(()) => {
                ui::ok(format!("cache wiped ({})", cache.display()));
                ExitCode::SUCCESS
            }
            Err(e) => {
                ui::error(format!(
                    "could not wipe cache {}: {e}. Check permissions or remove it manually.",
                    cache.display()
                ));
                ExitCode::from(1)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Detection;
    use std::path::PathBuf;

    #[test]
    fn rejects_paths_outside_cargoless_namespace() {
        assert!(is_unsafe_cache_dir(&PathBuf::from("")));
        assert!(is_unsafe_cache_dir(&PathBuf::from("/")));
        assert!(is_unsafe_cache_dir(&PathBuf::from("/home/u")));
        assert!(is_unsafe_cache_dir(&PathBuf::from("/proj")));
        assert!(!is_unsafe_cache_dir(&PathBuf::from(
            "/home/u/.cache/cargoless/deadbeef"
        )));
        assert!(!is_unsafe_cache_dir(&PathBuf::from(
            "/tmp/cargoless/abc123"
        )));
    }

    #[test]
    fn idempotent_then_wipes() {
        let mut base = std::env::temp_dir();
        base.push(format!("cargoless/clean-{}", std::process::id()));
        let cache = base.clone();
        let _ = std::fs::remove_dir_all(&base);
        let cfg = Config {
            root: PathBuf::from("/proj"),
            target: "wasm32-unknown-unknown".into(),
            cache_dir: cache.clone(),
            detection: Detection::AutoLeptosCdylib,
        };
        assert_eq!(run(&cfg), ExitCode::SUCCESS); // missing → ok
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("k"), b"v").unwrap();
        assert_eq!(run(&cfg), ExitCode::SUCCESS); // present → wiped
        assert!(std::fs::metadata(&cache).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
