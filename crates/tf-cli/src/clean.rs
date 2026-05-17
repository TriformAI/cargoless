//! `clean` — wipe the local content-addressed cache.
//!
//! The cache *location* is config-derived (`[cache] dir`, default
//! `<root>/.cargoless/cache`). The CAS *contents/layout* are owned by
//! build-cas (`tf-cas::LocalDiskStore`); `clean` only needs to remove the
//! directory tree, which is layout-agnostic and safe to own here. (The
//! canonical default root must stay aligned with build-cas — flagged to the
//! lead so the two never drift.)
//!
//! Safety: we refuse to remove a path that is not under the project root, so
//! a mis-set `[cache] dir = "/"` can never `rm -rf` something catastrophic.

use std::path::Path;
use std::process::ExitCode;

use crate::config::Config;
use crate::ui;

/// True if `cache` is the root itself or escapes it (`..`, absolute
/// elsewhere). Such a cache dir is refused rather than deleted.
fn is_unsafe_cache_dir(root: &Path, cache: &Path) -> bool {
    // Compare lexically; the dir may not exist yet so canonicalize can fail.
    let root = root.components().collect::<Vec<_>>();
    let cache_c = cache.components().collect::<Vec<_>>();
    if cache.as_os_str().is_empty() {
        return true;
    }
    // Cache must be strictly deeper than root and share its prefix.
    if cache_c.len() <= root.len() {
        return true;
    }
    root.iter().zip(&cache_c).any(|(r, c)| r != c)
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
    use std::path::PathBuf;

    #[test]
    fn rejects_cache_dirs_that_escape_the_root() {
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
    fn clean_is_idempotent_on_a_missing_then_present_cache() {
        let mut root = std::env::temp_dir();
        root.push(format!("tf-cli-clean-{}", std::process::id()));
        let cache = root.join(".cargoless").join("cache");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let cfg = Config {
            host: "127.0.0.1".into(),
            port: 8080,
            root: root.clone(),
            target: "wasm32-unknown-unknown".into(),
            cache_dir: cache.clone(),
            detection: crate::config::Detection::AutoLeptosCdylib,
        };

        // Missing → success (idempotent).
        assert_eq!(run(&cfg), ExitCode::SUCCESS);

        // Present with content → wiped.
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("deadbeef"), b"artifact").unwrap();
        assert_eq!(run(&cfg), ExitCode::SUCCESS);
        assert!(std::fs::metadata(&cache).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }
}
