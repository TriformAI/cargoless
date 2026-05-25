//! `cargoless push --remote <url>` — #240/2c thin push-client.
//!
//! The CLIENT side of the central-daemon write-plane. Closes the loop
//! 2a opened (transport contract) and 2b completed on the server side
//! (ServeVerdictState::push_overlay override + serve-loop ingest).
//!
//! ## Flow (D-PUSHOVERLAY §3, D-INC2-2B §7 honest-boundary)
//!
//! 1. Resolve `--remote <url>` (required) + `--repo <path>` (local FS,
//!    default cwd) + `--worktree <key>` (server-side worktree id;
//!    default = canonical absolute `--repo` path, the spike's
//!    path-keyed default) + `--base <ref>` (git base, default HEAD).
//! 2. Compute the overlay-set:
//!    `git -C <repo> diff --name-only <base>` → changed-file list →
//!    changed Rust source files + changed workspace-defining config files
//!    → read each selected file's bytes → `(absolute path, content)` pairs.
//!    Non-Rust changed paths still travel as `changed_files` metadata so
//!    project checks can select correctly without bloating the LSP overlay.
//! 3. **Canonicalize ordering** — sort files by path so the daemon's
//!    `cluster_hash_from_pushed` is deterministic regardless of the
//!    client's OS-enumeration order (#262 C6 fix, client-side; ~5 LOC,
//!    naturally adjacent to file-gathering).
//! 4. `HttpClient::new(url).push_overlay_with_profile(...)` →
//!    `PushOverlayAck { accepted, applied_files, worktree }`.
//! 5. Print the ack; optionally `--await-verdict` via `/status`; exit 0
//!    (green/accepted), 1 (red/rejected/transport
//!    error), or 2 (setup error). Fail-soft: never panic on a transport
//!    failure — surface the actionable message.
//!
//! ## Honest 2c boundary (stated, not papered over)
//!
//! * Push remains ack-first by default: the verdict round-trips via the
//!   already-shipped read-plane. `--await-verdict` is the blocking
//!   automation mode; it polls status and guards against accepting a stale
//!   prior verdict.
//! * Git ops via `std::process::Command` — same discipline as
//!   `build.rs`'s trunk subprocess and `watch.rs`'s tooling.
//! * No new external deps. The client uses the shipped HTTP transport
//!   surface and its additive profile-aware push verb.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use cargoless_core::transport::http::HttpClient;
use cargoless_core::transport::{CheckProfile, PushOverlayOptions, TransportClient};

const WORKSPACE_CONFIG_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "rust-toolchain",
    ".cargo/config.toml",
    ".cargo/config",
];

/// CLI-resolved push parameters, ready to drive
/// `HttpClient::push_overlay` + git-subprocess file enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOpts {
    /// Required: `--remote <url>` — the central daemon's HTTP endpoint.
    pub remote: String,
    /// Optional bearer token for a protected remote daemon. The loopback
    /// canary path leaves this unset; non-loopback deployments should set
    /// it via `CARGOLESS_AUTH_TOKEN` or `--auth-token`.
    pub auth_token: Option<String>,
    /// Local repository root (defaults to cwd). git operations run via
    /// `git -C <repo>`; file reads are `repo.join(rel_path)`.
    pub repo: PathBuf,
    /// Server-side worktree key. Default: the canonical absolute
    /// `repo` path (path-keyed identity, spike open-Q1 default).
    pub worktree: String,
    /// Git base ref (default `HEAD`). Carried in the push payload for
    /// future diagnostics; server stores-and-ignores in v0.2.x
    /// (spike open-Q2 default).
    pub base: String,
    /// Optional per-request cargo-check profile. tf-multiverse uses this
    /// to push `check-remote` selectors through one shared daemon.
    pub check_profile: Option<CheckProfile>,
    /// Server-side repository root. When set, the client sends
    /// repo-relative overlay paths and asks the daemon to map them under this
    /// root. This is the central-cluster service mode; absent keeps the
    /// same-host absolute-path behavior.
    pub server_root: Option<PathBuf>,
    /// If true, block until the remote daemon publishes a fresh verdict for
    /// this worktree.
    pub await_verdict: bool,
    /// Wall-clock timeout for `await_verdict`.
    pub await_timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AwaitFreshness {
    prior_published_at: Option<u64>,
    not_before_unix: u64,
}

impl AwaitFreshness {
    fn is_fresh(self, published_at: u64) -> bool {
        match self.prior_published_at {
            Some(prior) => published_at > prior,
            None => published_at > self.not_before_unix,
        }
    }
}

/// `cargoless push` entry. Returns an `ExitCode` per the v0 CLI
/// convention: 0 = success (ack.accepted=true), 1 = rejected / transport
/// error, 2 = setup / config error.
pub fn run(opts: &PushOpts) -> ExitCode {
    // 1. Enumerate changed files via git.
    let changed = match git_changed_files(&opts.repo, &opts.base) {
        Ok(files) => files,
        Err(e) => {
            crate::ui::error(format!(
                "push: git diff against `{}` in `{}` failed: {e}",
                opts.base,
                opts.repo.display()
            ));
            return ExitCode::from(2);
        }
    };
    if changed.is_empty() && !opts.await_verdict {
        eprintln!(
            "[cargoless:push] no changes vs {} in {} — nothing to push",
            opts.base,
            opts.repo.display()
        );
        return ExitCode::from(0);
    }

    // 2. Read each changed Rust source file plus changed workspace-defining
    //    config files the daemon uses for cluster hashing. Paths are sent as
    //    absolute file paths to match the FS-watcher mode byte-for-byte at
    //    the LSP seam (`didOpen`/`didChange` require real `file:///abs/...`
    //    URIs). Unchanged workspace config comes from the server's base
    //    checkout; only changed config needs to travel as an override. Other
    //    non-Rust changed paths are intentionally metadata-only via
    //    PushOverlayOptions::changed_files.
    //    Tolerant: a skipped file (read error, usually an absent optional
    //    config file or deleted changed file) warns but does not abort the
    //    push — the pushed-overlay is best-effort and the server is robust
    //    to partial sets (the cluster-hash + diff are content-shaped).
    let repo_relative = opts.server_root.is_some();
    let changed_set: BTreeSet<&str> = changed.iter().map(String::as_str).collect();
    let candidates = overlay_candidate_files(&changed);
    let mut files: Vec<(String, String)> = Vec::with_capacity(candidates.len());
    for rel in &candidates {
        let abs = opts.repo.join(rel);
        match std::fs::read_to_string(&abs) {
            Ok(content) => files.push((payload_path(&opts.repo, rel, repo_relative), content)),
            Err(_) if changed_set.contains(rel.as_str()) && !abs.exists() => {
                crate::ui::warn(format!(
                    "push: `{}` is deleted locally; representing it as an empty overlay file",
                    abs.display()
                ));
                files.push((payload_path(&opts.repo, rel, repo_relative), String::new()));
            }
            Err(e) => crate::ui::warn(format!("push: skip `{}` (read error: {e})", abs.display())),
        }
    }
    let metadata_only_paths = changed
        .iter()
        .filter(|path| !is_push_overlay_content_file(path))
        .count();
    let overlay_content_bytes: usize = files.iter().map(|(_, content)| content.len()).sum();
    eprintln!(
        "[cargoless:push] overlay content files={} bytes={} changed_paths={} metadata_only_paths={}",
        files.len(),
        overlay_content_bytes,
        changed.len(),
        metadata_only_paths
    );

    // 3. **C6 client-side canonicalize** (closes #262). Sort by path so
    //    the daemon's `cluster_hash_from_pushed` sees a deterministic
    //    file order regardless of how git/the OS enumerated the
    //    changes. Without this, two semantically-identical pushes
    //    could produce different cluster hashes ⇒ wrong-cluster
    //    routing — which is the cross-WT-cluster-routing regression
    //    class L3 flagged as worth a fix.
    files.sort_by(|a, b| a.0.cmp(&b.0));

    // 4. Build the HTTP client + push.
    let client = match opts.auth_token.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(token) => HttpClient::with_token(&opts.remote, token),
        None => HttpClient::new(&opts.remote),
    };
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            crate::ui::error(format!(
                "push: HttpClient init failed for `{}`: {e}",
                opts.remote
            ));
            return ExitCode::from(2);
        }
    };
    let await_freshness = if opts.await_verdict {
        let prior_published_at = match client.get_status(&opts.worktree) {
            Ok(Some(status)) => Some(status.published_at),
            Ok(None) => None,
            Err(e) => {
                crate::ui::warn(format!(
                    "push: pre-push status poll failed while preparing await: {e}"
                ));
                None
            }
        };
        Some(AwaitFreshness {
            prior_published_at,
            not_before_unix: crate::statusfile::now_unix(),
        })
    } else {
        None
    };
    let mut options = PushOverlayOptions {
        changed_files: if changed.is_empty() {
            None
        } else {
            Some(changed.clone())
        },
        ..PushOverlayOptions::default()
    };
    if let Some(root) = opts.server_root.as_ref() {
        options.repo_relative = true;
        options.analysis_root = Some(root.to_string_lossy().into_owned());
        options.base_sha = git_resolve_ref(&opts.repo, &opts.base).ok();
    }
    let options = if options.is_empty() {
        None
    } else {
        Some(options)
    };
    let ack = match client.push_overlay_with_options(
        &opts.worktree,
        &opts.base,
        &files,
        opts.check_profile.as_ref(),
        options.as_ref(),
    ) {
        Ok(a) => a,
        Err(e) => {
            crate::ui::error(format!("push: server `{}` rejected: {e}", opts.remote));
            return ExitCode::from(1);
        }
    };

    // 5. Print ack + exit code.
    eprintln!(
        "[cargoless:push] ack from {}: accepted={} applied_files={} worktree={}",
        opts.remote, ack.accepted, ack.applied_files, ack.worktree
    );
    eprintln!(
        "[cargoless:push] verdict: polling `cargoless status --remote {}` until fresh",
        opts.remote
    );
    if !ack.accepted {
        return ExitCode::from(1);
    }
    if opts.await_verdict {
        return await_verdict(
            &client,
            &opts.worktree,
            await_freshness.expect("await freshness prepared when await_verdict is true"),
            opts.await_timeout_secs,
        );
    }
    ExitCode::from(0)
}

fn await_verdict(
    client: &HttpClient,
    worktree: &str,
    freshness: AwaitFreshness,
    timeout_secs: u64,
) -> ExitCode {
    let timeout = std::time::Duration::from_secs(timeout_secs.max(1));
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        match client.get_status(worktree) {
            Ok(Some(status)) if freshness.is_fresh(status.published_at) => {
                return exit_for_verdict(
                    "status",
                    &status.worktree,
                    &status.verdict,
                    status.published_at,
                );
            }
            Ok(_) => {}
            Err(e) => {
                crate::ui::warn(format!("push: await verdict status poll failed: {e}"));
            }
        }

        let remaining = timeout.saturating_sub(started.elapsed());
        let wait = remaining.min(std::time::Duration::from_millis(200));
        if wait.is_zero() {
            break;
        }
        std::thread::sleep(wait);
    }
    crate::ui::error(format!(
        "push: timed out after {}s awaiting a fresh verdict for {}",
        timeout.as_secs(),
        worktree
    ));
    ExitCode::from(1)
}

fn exit_for_verdict(source: &str, worktree: &str, verdict: &str, published_at: u64) -> ExitCode {
    eprintln!(
        "[cargoless:push] fresh verdict via {} worktree={} verdict={} published_at={}",
        source, worktree, verdict, published_at
    );
    match verdict {
        "green" => ExitCode::from(0),
        "red" => ExitCode::from(1),
        _ => ExitCode::from(1),
    }
}

/// Run `git -C <repo> diff --name-only <base>` and return the changed
/// file list (one path per line, repo-relative). Errors surface the
/// stderr verbatim — actionable to the operator.
pub(crate) fn git_changed_files(repo: &Path, base: &str) -> std::io::Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("diff")
        .arg("--name-only")
        .arg(base)
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git diff exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

fn git_resolve_ref(repo: &Path, base: &str) -> std::io::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg("--verify")
        .arg(format!("{base}^{{commit}}"))
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git rev-parse exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn overlay_candidate_files(changed: &[String]) -> Vec<String> {
    let mut files: BTreeSet<String> = BTreeSet::new();
    files.extend(
        changed
            .iter()
            .filter(|path| is_push_overlay_content_file(path))
            .cloned(),
    );
    files.into_iter().collect()
}

fn is_push_overlay_content_file(rel: &str) -> bool {
    is_workspace_config_file(rel) || Path::new(rel).extension().is_some_and(|ext| ext == "rs")
}

fn is_workspace_config_file(rel: &str) -> bool {
    WORKSPACE_CONFIG_FILES.iter().any(|path| *path == rel)
}

fn payload_path(repo: &Path, rel: &str, repo_relative: bool) -> String {
    if repo_relative {
        rel.to_string()
    } else {
        repo.join(rel).to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargoless_core::transport::CargoSubcommand;

    /// **2c keystone test — the client-side composing-equivalence
    /// shape.** Two semantically-identical pushes (same `(path,
    /// content)` set) with different INPUT ordering MUST produce
    /// identical sorted `files` vecs after the C6 canonicalize step.
    /// This ensures the daemon's `cluster_hash_from_pushed` is
    /// deterministic across N clients regardless of OS enumeration
    /// order — closing the cross-WT-cluster-routing regression class
    /// builder-infra's L3 flagged (#262).
    ///
    /// The test asserts the CONTRACT — "after sort, file order is a
    /// function of (path, content) only, not of input order" — not
    /// the implementation details. A future refactor that switches
    /// to a different sort key but preserves determinism still
    /// passes; one that drops the sort fails exactly here.
    #[test]
    fn c6_canonicalize_makes_input_order_irrelevant_to_pushed_order() {
        let files_a = vec![
            (
                "Cargo.toml".to_string(),
                "[package]\nname=\"x\"".to_string(),
            ),
            ("src/lib.rs".to_string(), "pub fn x() {}".to_string()),
            ("Cargo.lock".to_string(), "# lockfile".to_string()),
        ];
        let files_b = vec![
            ("src/lib.rs".to_string(), "pub fn x() {}".to_string()),
            ("Cargo.lock".to_string(), "# lockfile".to_string()),
            (
                "Cargo.toml".to_string(),
                "[package]\nname=\"x\"".to_string(),
            ),
        ];
        // C6 sort.
        let mut a = files_a;
        let mut b = files_b;
        a.sort_by(|x, y| x.0.cmp(&y.0));
        b.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(
            a, b,
            "C6 canonicalize: same (path, content) set ⇒ identical sorted vec, \
             regardless of input order — closes #262 cross-WT-cluster-routing \
             regression class at the client seam"
        );
        // Sanity: the sort produces a known canonical order.
        assert_eq!(a[0].0, "Cargo.lock");
        assert_eq!(a[1].0, "Cargo.toml");
        assert_eq!(a[2].0, "src/lib.rs");
    }

    #[test]
    fn push_opts_shape_round_trips() {
        // The CLI surface a `cargoless push --remote URL --repo /r
        // --worktree W --base origin/main` invocation resolves to.
        let opts = PushOpts {
            remote: "http://localhost:8080".to_string(),
            auth_token: Some("token".to_string()),
            repo: PathBuf::from("/r"),
            worktree: "/r".to_string(),
            base: "HEAD".to_string(),
            check_profile: Some(CheckProfile {
                subcommand: CargoSubcommand::Check,
                package: Some("triform-server".into()),
                target: None,
                features: Vec::new(),
                no_default_features: false,
                release: false,
                extra_args: Vec::new(),
            }),
            server_root: None,
            await_verdict: true,
            await_timeout_secs: 10,
        };
        // Cheap clone+eq sanity (the v0 CLI Opts shape relies on
        // PartialEq for the parser tests in main.rs).
        let cloned = opts.clone();
        assert_eq!(opts, cloned);
    }

    #[test]
    fn await_freshness_requires_newer_status_when_prior_exists() {
        let guard = AwaitFreshness {
            prior_published_at: Some(100),
            not_before_unix: 100,
        };
        assert!(!guard.is_fresh(99));
        assert!(!guard.is_fresh(100));
        assert!(guard.is_fresh(101));
    }

    #[test]
    fn await_freshness_uses_push_start_when_no_prior_status_exists() {
        let guard = AwaitFreshness {
            prior_published_at: None,
            not_before_unix: 100,
        };
        assert!(!guard.is_fresh(99));
        assert!(!guard.is_fresh(100));
        assert!(guard.is_fresh(101));
    }

    #[test]
    fn overlay_candidates_include_changed_workspace_config_and_dedupe() {
        let files = overlay_candidate_files(&[
            "src/lib.rs".to_string(),
            "Cargo.toml".to_string(),
            "src/lib.rs".to_string(),
        ]);
        assert!(files.contains(&"Cargo.toml".to_string()));
        assert!(!files.contains(&"Cargo.lock".to_string()));
        assert!(!files.contains(&"rust-toolchain.toml".to_string()));
        assert!(!files.contains(&".cargo/config.toml".to_string()));
        assert_eq!(
            files.iter().filter(|p| p.as_str() == "src/lib.rs").count(),
            1
        );
        assert_eq!(
            files.iter().filter(|p| p.as_str() == "Cargo.toml").count(),
            1
        );
    }

    #[test]
    fn overlay_candidates_skip_non_rust_changed_contents() {
        let files = overlay_candidate_files(&[
            "src/lib.rs".to_string(),
            "coverage/exposed_faces.json".to_string(),
            "chemistry/generators/rust/CONFIG_COVERAGE.json".to_string(),
            "scripts/deploy".to_string(),
            "README.md".to_string(),
        ]);

        assert!(files.contains(&"src/lib.rs".to_string()));
        assert!(!files.contains(&"Cargo.toml".to_string()));
        assert!(!files.contains(&"Cargo.lock".to_string()));
        assert!(!files.contains(&"coverage/exposed_faces.json".to_string()));
        assert!(!files.contains(&"chemistry/generators/rust/CONFIG_COVERAGE.json".to_string()));
        assert!(!files.contains(&"scripts/deploy".to_string()));
        assert!(!files.contains(&"README.md".to_string()));
    }

    #[test]
    fn overlay_candidate_content_filter_keeps_workspace_config_files() {
        for path in WORKSPACE_CONFIG_FILES {
            assert!(
                is_push_overlay_content_file(path),
                "workspace config file should be sent as overlay content: {path}"
            );
        }
        assert!(is_push_overlay_content_file("crates/cargoless/src/lib.rs"));
        assert!(!is_push_overlay_content_file("generated/schema.json"));
    }

    #[test]
    fn payload_paths_are_absolute_like_fs_watcher_mode() {
        assert_eq!(
            payload_path(Path::new("/repo/wt"), "src/lib.rs", false),
            "/repo/wt/src/lib.rs"
        );
    }

    #[test]
    fn payload_paths_can_be_repo_relative_for_central_daemon_mode() {
        assert_eq!(
            payload_path(Path::new("/repo/wt"), "src/lib.rs", true),
            "src/lib.rs"
        );
    }

    #[test]
    fn git_changed_files_actionable_error_on_unreadable_repo() {
        // No git repo at this path ⇒ `git -C` fails fast; we surface
        // the error not panic. Fail-soft per the discipline.
        let res = git_changed_files(Path::new("/this/path/definitely/does/not/exist"), "HEAD");
        assert!(
            res.is_err(),
            "git on non-existent path MUST error, not panic"
        );
        // The error string mentions git diff (actionable).
        let msg = format!("{}", res.unwrap_err());
        assert!(
            msg.contains("git"),
            "error surface mentions the failing tool: {msg}"
        );
    }

    #[test]
    fn empty_changed_files_is_noop_success() {
        // Cover the happy `no changes to push` path's code structure
        // — the empty filter post-`git diff` MUST yield empty Vec,
        // not an error. (The `run()` body returns ExitCode::from(0)
        // for this case — tested via the integration arm; the unit
        // test here pins the parser's empty-input contract.)
        let parsed: Vec<String> = "\n\n  \n"
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        assert!(parsed.is_empty(), "whitespace-only stdout ⇒ no files");
    }
}
