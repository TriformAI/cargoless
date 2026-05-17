//! `clean` — remove the local content-addressed cache.
//!
//! The cache *location* is config-derived (`[cache] dir`, default
//! `<root>/.cargoless/cache`); the CAS contents/layout are build-cas's
//! concern, but removing the directory tree is layout-agnostic and safe to
//! own here. The tf-cli status file lives at `<root>/.cargoless/cli-status`
//! (a sibling, NOT under the cache dir) so it is intentionally left alone —
//! a `clean` while a daemon runs must not blind `status`.
//!
//! Safety: refuse a cache dir that is the root itself or escapes it, so a
//! mis-set `[cache] dir` can never delete something catastrophic.

use std::path::Path;
use std::process::ExitCode;

use crate::config::Config;
use crate::ui;

fn is_unsafe_cache_dir(root: &Path, cache: &Path) -> bool {
    if cache.as_os_str().is_empty() {
        return true;
    }
    let root: Vec<_> = root.components().collect();
    let cache: Vec<_> = cache.components().collect();
    if cache.len() <= root.len() {
        return true; // must be strictly deeper than root
    }
    root.iter().zip(&cache).any(|(r, c)| r != c) // must share root's prefix
}

pub fn run(cfg: &Config) -> ExitCode {
    let cache = &cfg.cache_dir;

    if is_unsafe_cache_dir(&cfg.root, cache) {
        ui::error(format!(
            "refusing to clean `{}` — the cache dir must be inside the \
             project root. Fix `[cache] dir` in tf.toml.",
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
    fn rejects_escaping_cache_dirs() {
        let root = PathBuf::from("/proj");
        assert!(is_unsafe_cache_dir(&root, &PathBuf::from("/proj")));
        assert!(is_unsafe_cache_dir(&root, &PathBuf::from("/")));
        assert!(is_unsafe_cache_dir(&root, &PathBuf::from("/etc")));
        assert!(is_unsafe_cache_dir(&root, &PathBuf::from("")));
        assert!(!is_unsafe_cache_dir(
            &root,
            &PathBuf::from("/proj/.cargoless/cache")
        ));
    }

    #[test]
    fn idempotent_then_wipes() {
        let mut root = std::env::temp_dir();
        root.push(format!("tf-cli-clean-{}", std::process::id()));
        let cache = root.join(".cargoless").join("cache");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cfg = Config {
            root: root.clone(),
            target: "wasm32-unknown-unknown".into(),
            cache_dir: cache.clone(),
            detection: Detection::AutoLeptosCdylib,
        };
        assert_eq!(run(&cfg), ExitCode::SUCCESS); // missing → ok
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("k"), b"v").unwrap();
        assert_eq!(run(&cfg), ExitCode::SUCCESS); // present → wiped
        assert!(std::fs::metadata(&cache).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
