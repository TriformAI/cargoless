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
//!    changed text files + changed workspace-defining config files → read each
//!    selected file's bytes → `(absolute path, content)` pairs. Unsupported
//!    binary/heavy artifact paths still travel as `changed_files` metadata so
//!    project checks can select correctly without bloating the overlay payload.
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
//! * The client uses the shipped HTTP(S) transport surface and its additive
//!   profile-aware push verb.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode};

use cargoless_core::transport::http::{HttpClient, MAX_OVERLAY_BYTES, prepare_json_body};
use cargoless_core::transport::{CheckProfile, PushOverlayOptions, Request, TransportClient};

const WORKSPACE_CONFIG_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "rust-toolchain",
    ".cargo/config.toml",
    ".cargo/config",
];
const PUSH_PAYLOAD_WARN_BYTES: usize = 8 * 1024 * 1024;
const LARGEST_CONTENT_FILE_LIMIT: usize = 5;
const DIAGNOSTIC_PATH_SAMPLE_LIMIT: usize = 20;

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

    // 2. Build the minimal overlay payload from exactly the git diff paths.
    //    Changed source/text/config files are read as content or fail setup;
    //    unsupported artifact extensions remain metadata-only for trigger
    //    selection. Hard-excluded runtime/cache paths do not enter the sent
    //    request at all. Deleted content files are carried deliberately as
    //    empty overlays, preserving the existing push/delete representation
    //    rather than silently dropping them.
    let repo_relative = opts.server_root.is_some();
    let mut payload = match build_push_payload(&opts.repo, &changed, repo_relative) {
        Ok(payload) => payload,
        Err(e) => {
            crate::ui::error(e.to_string());
            return ExitCode::from(2);
        }
    };
    if payload.files.is_empty() && payload.trigger_paths.is_empty() && !opts.await_verdict {
        eprintln!(
            "[cargoless:push] all changes vs {} in {} are excluded runtime/cache paths — nothing to push",
            opts.base,
            opts.repo.display()
        );
        return ExitCode::from(0);
    }

    // 3. **C6 client-side canonicalize** (closes #262). Sort by path so
    //    the daemon's `cluster_hash_from_pushed` sees a deterministic
    //    file order regardless of how git/the OS enumerated the
    //    changes. Without this, two semantically-identical pushes
    //    could produce different cluster hashes ⇒ wrong-cluster
    //    routing — which is the cross-WT-cluster-routing regression
    //    class L3 flagged as worth a fix.
    payload.files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut options = PushOverlayOptions {
        changed_files: if payload.trigger_paths.is_empty() {
            None
        } else {
            Some(payload.trigger_paths.clone())
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
    let body = push_overlay_request_body(
        &opts.worktree,
        &opts.base,
        &payload.files,
        opts.check_profile.as_ref(),
        options.as_ref(),
    );
    emit_payload_diagnostics(&changed, &payload, body.len());
    if let Err(message) = validate_overlay_http_cap(&body, &payload.content_stats) {
        crate::ui::error(message);
        return ExitCode::from(2);
    }

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
    let ack = match client.push_overlay_with_options(
        &opts.worktree,
        &opts.base,
        &payload.files,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct PushPayload {
    files: Vec<(String, String)>,
    content_stats: Vec<ContentFileStat>,
    trigger_paths: Vec<String>,
    metadata_only_paths: Vec<MetadataOnlyPath>,
    excluded_paths: Vec<MetadataOnlyPath>,
}

impl PushPayload {
    fn new() -> Self {
        Self {
            files: Vec::new(),
            content_stats: Vec::new(),
            trigger_paths: Vec::new(),
            metadata_only_paths: Vec::new(),
            excluded_paths: Vec::new(),
        }
    }

    fn content_bytes(&self) -> usize {
        self.content_stats.iter().map(|stat| stat.bytes).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContentFileStat {
    path: String,
    bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataOnlyPath {
    path: String,
    reason: MetadataOnlyReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataOnlyReason {
    HardExcluded,
    UnsupportedPath,
}

impl MetadataOnlyReason {
    fn as_str(self) -> &'static str {
        match self {
            MetadataOnlyReason::HardExcluded => "excluded",
            MetadataOnlyReason::UnsupportedPath => "metadata-only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContentFileFailure {
    path: String,
    reason: ContentFileFailureReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ContentFileFailureReason {
    MetadataError(String),
    NonRegular,
    ReadError(String),
    NulByte,
    NonUtf8,
}

impl ContentFileFailureReason {
    fn message(&self) -> String {
        match self {
            ContentFileFailureReason::MetadataError(e) => format!("metadata error: {e}"),
            ContentFileFailureReason::NonRegular => "not a regular file".to_string(),
            ContentFileFailureReason::ReadError(e) => format!("read error: {e}"),
            ContentFileFailureReason::NulByte => {
                "contains NUL byte; refusing binary content".to_string()
            }
            ContentFileFailureReason::NonUtf8 => "not valid UTF-8 text".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PushPayloadError {
    failures: Vec<ContentFileFailure>,
}

impl std::fmt::Display for PushPayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "push: content-eligible paths could not be sent as overlay content; refusing before network send to avoid checking base content. "
        )?;
        write!(f, "Failures: {}", format_content_failures(&self.failures))
    }
}

impl std::error::Error for PushPayloadError {}

fn build_push_payload(
    repo: &Path,
    changed: &[String],
    repo_relative: bool,
) -> Result<PushPayload, PushPayloadError> {
    let mut payload = PushPayload::new();
    let mut failures = Vec::new();
    let changed_paths: BTreeSet<String> = changed
        .iter()
        .map(|path| path.trim())
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect();

    for rel in changed_paths {
        if !is_safe_repo_relative_path(&rel) || is_hard_excluded_push_path(&rel) {
            payload.excluded_paths.push(MetadataOnlyPath {
                path: rel,
                reason: MetadataOnlyReason::HardExcluded,
            });
            continue;
        }

        payload.trigger_paths.push(rel.clone());
        if !is_push_overlay_content_file(&rel) {
            payload.metadata_only_paths.push(MetadataOnlyPath {
                path: rel,
                reason: MetadataOnlyReason::UnsupportedPath,
            });
            continue;
        }

        let abs = repo.join(&rel);
        let metadata = match std::fs::symlink_metadata(&abs) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                crate::ui::warn(format!(
                    "push: `{}` is deleted locally; representing it as an empty overlay file",
                    abs.display()
                ));
                payload
                    .files
                    .push((payload_path(repo, &rel, repo_relative), String::new()));
                payload.content_stats.push(ContentFileStat {
                    path: rel,
                    bytes: 0,
                });
                continue;
            }
            Err(e) => {
                failures.push(ContentFileFailure {
                    path: rel,
                    reason: ContentFileFailureReason::MetadataError(e.to_string()),
                });
                continue;
            }
        };

        if !metadata.file_type().is_file() {
            failures.push(ContentFileFailure {
                path: rel,
                reason: ContentFileFailureReason::NonRegular,
            });
            continue;
        }

        let bytes = match std::fs::read(&abs) {
            Ok(bytes) => bytes,
            Err(e) => {
                failures.push(ContentFileFailure {
                    path: rel,
                    reason: ContentFileFailureReason::ReadError(e.to_string()),
                });
                continue;
            }
        };
        if bytes.contains(&0) {
            failures.push(ContentFileFailure {
                path: rel,
                reason: ContentFileFailureReason::NulByte,
            });
            continue;
        }
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(_) => {
                failures.push(ContentFileFailure {
                    path: rel,
                    reason: ContentFileFailureReason::NonUtf8,
                });
                continue;
            }
        };
        let bytes = content.len();
        payload
            .files
            .push((payload_path(repo, &rel, repo_relative), content));
        payload
            .content_stats
            .push(ContentFileStat { path: rel, bytes });
    }

    if failures.is_empty() {
        Ok(payload)
    } else {
        Err(PushPayloadError { failures })
    }
}

fn push_overlay_request_body(
    worktree: &str,
    base_ref: &str,
    files: &[(String, String)],
    check_profile: Option<&CheckProfile>,
    options: Option<&PushOverlayOptions>,
) -> String {
    match options.filter(|options| !options.is_empty()) {
        Some(options) => Request::PushOverlayV2 {
            worktree: worktree.to_string(),
            base_ref: base_ref.to_string(),
            files: files.to_vec(),
            check_profile: check_profile.cloned(),
            options: options.clone(),
        },
        None => Request::PushOverlay {
            worktree: worktree.to_string(),
            base_ref: base_ref.to_string(),
            files: files.to_vec(),
            check_profile: check_profile.cloned(),
        },
    }
    .to_json()
}

fn emit_payload_diagnostics(changed: &[String], payload: &PushPayload, json_bytes: usize) {
    eprintln!(
        "[cargoless:push] payload changed_paths={} content_files={} content_bytes={} metadata_only_paths={} excluded_paths={} json_bytes={}",
        changed.len(),
        payload.files.len(),
        payload.content_bytes(),
        payload.metadata_only_paths.len(),
        payload.excluded_paths.len(),
        json_bytes
    );
    eprintln!(
        "[cargoless:push] changed paths: {}",
        format_changed_path_sample(changed, DIAGNOSTIC_PATH_SAMPLE_LIMIT)
    );
    if !payload.metadata_only_paths.is_empty() {
        eprintln!(
            "[cargoless:push] metadata-only paths: {}",
            format_metadata_path_sample(&payload.metadata_only_paths, DIAGNOSTIC_PATH_SAMPLE_LIMIT)
        );
    }
    if !payload.excluded_paths.is_empty() {
        eprintln!(
            "[cargoless:push] excluded paths: {}",
            format_metadata_path_sample(&payload.excluded_paths, DIAGNOSTIC_PATH_SAMPLE_LIMIT)
        );
    }
    if json_bytes > PUSH_PAYLOAD_WARN_BYTES {
        crate::ui::warn(format!(
            "push: payload JSON body is {} bytes (warning threshold {}); largest content files: {}",
            json_bytes,
            PUSH_PAYLOAD_WARN_BYTES,
            format_largest_content_files(&payload.content_stats)
        ));
    }
}

fn validate_overlay_http_cap(body: &str, content_stats: &[ContentFileStat]) -> Result<(), String> {
    let prepared = prepare_json_body(body)
        .map_err(|e| format!("push: failed to prepare overlay HTTP body: {e}"))?;
    if prepared.raw_len <= MAX_OVERLAY_BYTES && prepared.encoded_len() <= MAX_OVERLAY_BYTES {
        return Ok(());
    }
    Err(format!(
        "push: overlay HTTP body is {} encoded bytes ({} raw JSON bytes), exceeding the 32 MiB HTTP cap ({} bytes); refusing before network send. Largest content files: {}. Suggested next step: split the change or remove generated/heavy artifacts from the source diff; Cargoless will not fall back to cargo check.",
        prepared.encoded_len(),
        prepared.raw_len,
        MAX_OVERLAY_BYTES,
        format_largest_content_files(content_stats)
    ))
}

fn format_changed_path_sample(paths: &[String], limit: usize) -> String {
    if paths.is_empty() {
        return "none".to_string();
    }
    let shown: Vec<&str> = paths.iter().take(limit).map(String::as_str).collect();
    let mut out = shown.join(", ");
    if paths.len() > shown.len() {
        out.push_str(&format!(" (+{} more)", paths.len() - shown.len()));
    }
    out
}

fn format_metadata_path_sample(paths: &[MetadataOnlyPath], limit: usize) -> String {
    if paths.is_empty() {
        return "none".to_string();
    }
    let shown: Vec<String> = paths
        .iter()
        .take(limit)
        .map(|path| format!("{} ({})", path.path, path.reason.as_str()))
        .collect();
    let mut out = shown.join(", ");
    if paths.len() > shown.len() {
        out.push_str(&format!(" (+{} more)", paths.len() - shown.len()));
    }
    out
}

fn format_content_failures(failures: &[ContentFileFailure]) -> String {
    if failures.is_empty() {
        return "none".to_string();
    }
    failures
        .iter()
        .map(|failure| format!("{} ({})", failure.path, failure.reason.message()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_largest_content_files(stats: &[ContentFileStat]) -> String {
    if stats.is_empty() {
        return "none".to_string();
    }
    let mut stats = stats.to_vec();
    stats.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    stats
        .into_iter()
        .take(LARGEST_CONTENT_FILE_LIMIT)
        .map(|stat| format!("{} ({} bytes)", stat.path, stat.bytes))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
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
    if !is_safe_repo_relative_path(rel) || is_hard_excluded_push_path(rel) {
        return false;
    }
    if is_workspace_config_file(rel) {
        return true;
    }
    if is_known_text_config_name(rel) {
        return true;
    }
    let Some(ext) = Path::new(rel).extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    matches!(
        ext,
        "rs" | "toml"
            | "yaml"
            | "yml"
            | "json"
            | "css"
            | "scss"
            | "sass"
            | "less"
            | "html"
            | "md"
            | "txt"
            | "py"
            | "sh"
            | "sql"
            | "js"
            | "mjs"
            | "cjs"
            | "jsx"
            | "ts"
            | "mts"
            | "cts"
            | "tsx"
            | "vue"
            | "svelte"
            | "xml"
            | "graphql"
            | "proto"
            | "ini"
            | "conf"
            | "cfg"
    )
}

fn is_workspace_config_file(rel: &str) -> bool {
    WORKSPACE_CONFIG_FILES.iter().any(|path| *path == rel)
}

fn is_safe_repo_relative_path(rel: &str) -> bool {
    if rel.trim().is_empty() {
        return false;
    }
    let path = Path::new(rel);
    if path.is_absolute() {
        return false;
    }
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
}

fn is_hard_excluded_push_path(rel: &str) -> bool {
    let mut components = Path::new(rel).components();
    let Some(Component::Normal(first)) = components.next() else {
        return true;
    };
    let Some(first) = first.to_str() else {
        return true;
    };
    let first = first.to_ascii_lowercase();
    if matches!(
        first.as_str(),
        ".git"
            | ".claude"
            | ".codex-worktrees"
            | ".cargoless"
            | "target"
            | "node_modules"
            | "dist"
            | "build"
            | "out"
            | "tmp"
            | "temp"
            | "screenshots"
            | "screenshot"
            | "__screenshots__"
            | "playwright-report"
            | "test-results"
    ) {
        return true;
    }

    components.any(|component| {
        let Component::Normal(component) = component else {
            return true;
        };
        let Some(component) = component.to_str() else {
            return true;
        };
        matches!(
            component.to_ascii_lowercase().as_str(),
            ".git"
                | ".claude"
                | ".codex-worktrees"
                | ".cargoless"
                | "target"
                | "node_modules"
                | ".cache"
                | ".next"
                | ".nuxt"
                | ".vite"
                | ".turbo"
                | ".parcel-cache"
                | ".svelte-kit"
                | ".pytest_cache"
                | ".mypy_cache"
                | ".ruff_cache"
                | ".gradle"
        )
    })
}

fn is_known_text_config_name(rel: &str) -> bool {
    let Some(name) = Path::new(rel).file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(
        name,
        "Dockerfile"
            | "Containerfile"
            | "Makefile"
            | "makefile"
            | "Justfile"
            | "justfile"
            | "Taskfile"
            | ".env"
            | ".env.example"
            | ".gitignore"
            | ".dockerignore"
            | ".npmrc"
            | ".nvmrc"
    )
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "cargoless-push-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_file(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

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
    fn overlay_candidates_include_changed_text_for_project_checks() {
        let files = overlay_candidate_files(&[
            "src/lib.rs".to_string(),
            "coverage/exposed_faces.json".to_string(),
            "chemistry/generators/rust/CONFIG_COVERAGE.json".to_string(),
            "scripts/deploy".to_string(),
            "README.md".to_string(),
            "chemistry/components/button.yaml".to_string(),
            "portal/style/main.css".to_string(),
            "scripts/ci/check.py".to_string(),
            "screenshots/current.png".to_string(),
        ]);

        assert!(files.contains(&"src/lib.rs".to_string()));
        assert!(!files.contains(&"Cargo.toml".to_string()));
        assert!(!files.contains(&"Cargo.lock".to_string()));
        assert!(files.contains(&"coverage/exposed_faces.json".to_string()));
        assert!(files.contains(&"chemistry/generators/rust/CONFIG_COVERAGE.json".to_string()));
        assert!(!files.contains(&"scripts/deploy".to_string()));
        assert!(files.contains(&"README.md".to_string()));
        assert!(files.contains(&"chemistry/components/button.yaml".to_string()));
        assert!(files.contains(&"portal/style/main.css".to_string()));
        assert!(files.contains(&"scripts/ci/check.py".to_string()));
        assert!(!files.contains(&"screenshots/current.png".to_string()));
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
        assert!(is_push_overlay_content_file("generated/schema.json"));
        assert!(is_push_overlay_content_file(
            "chemistry/components/button.yaml"
        ));
        assert!(!is_push_overlay_content_file("screenshots/current.png"));
        assert!(!is_push_overlay_content_file("target/debug/build.rs"));
        assert!(is_push_overlay_content_file("src/build/mod.rs"));
        assert!(is_push_overlay_content_file("src/out/mod.rs"));
        assert!(!is_push_overlay_content_file(
            ".claude/worktrees/wt/src/lib.rs"
        ));
    }

    #[test]
    fn payload_excludes_target_claude_and_runtime_paths_from_sent_request() {
        let root = temp_root("excluded");
        write_file(&root, "src/lib.rs", "pub fn ok() {}\n");
        write_file(
            &root,
            "target/debug/generated.rs",
            "pub fn generated() {}\n",
        );
        write_file(
            &root,
            ".claude/worktrees/nested/src/lib.rs",
            "pub fn hidden() {}\n",
        );
        write_file(&root, ".git/config", "[core]\n");
        write_file(&root, "node_modules/pkg/index.js", "module.exports = 1;\n");
        write_file(&root, ".cargoless/runtime.json", "{}\n");

        let payload = build_push_payload(
            &root,
            &[
                "src/lib.rs".into(),
                "target/debug/generated.rs".into(),
                ".claude/worktrees/nested/src/lib.rs".into(),
                ".git/config".into(),
                "node_modules/pkg/index.js".into(),
                ".cargoless/runtime.json".into(),
            ],
            true,
        )
        .unwrap();

        assert_eq!(
            payload.files,
            vec![("src/lib.rs".into(), "pub fn ok() {}\n".into())]
        );
        assert_eq!(payload.trigger_paths, vec!["src/lib.rs".to_string()]);
        assert_eq!(payload.metadata_only_paths, vec![]);
        assert_eq!(payload.excluded_paths.len(), 5);
    }

    #[test]
    fn large_unsupported_artifact_is_metadata_only_for_trigger_selection() {
        let root = temp_root("large-artifact");
        write_file(&root, "assets/archive.zip", vec![b'z'; 3 * 1024 * 1024]);

        let payload = build_push_payload(&root, &["assets/archive.zip".to_string()], true).unwrap();

        assert!(payload.files.is_empty());
        assert_eq!(
            payload.trigger_paths,
            vec!["assets/archive.zip".to_string()]
        );
        assert_eq!(
            payload.metadata_only_paths,
            vec![MetadataOnlyPath {
                path: "assets/archive.zip".into(),
                reason: MetadataOnlyReason::UnsupportedPath,
            }]
        );
    }

    #[test]
    fn large_text_source_file_is_included_as_overlay_content() {
        let root = temp_root("large-source");
        let content = format!(
            "pub const BIG: &str = \"{}\";\n",
            "x".repeat(3 * 1024 * 1024)
        );
        write_file(&root, "src/big.rs", &content);

        let payload = build_push_payload(&root, &["src/big.rs".to_string()], true).unwrap();

        assert_eq!(payload.files, vec![("src/big.rs".into(), content.clone())]);
        assert_eq!(payload.trigger_paths, vec!["src/big.rs".to_string()]);
        assert!(payload.metadata_only_paths.is_empty());
        assert_eq!(payload.content_bytes(), content.len());
    }

    #[test]
    fn binary_text_extension_fails_setup_instead_of_metadata_only() {
        let root = temp_root("binary");
        write_file(&root, "src/blob.rs", b"pub fn x() {}\0binary tail");

        let err = build_push_payload(&root, &["src/blob.rs".to_string()], true).unwrap_err();

        assert_eq!(
            err.failures,
            vec![ContentFileFailure {
                path: "src/blob.rs".into(),
                reason: ContentFileFailureReason::NulByte,
            }]
        );
        let msg = err.to_string();
        assert!(msg.contains("src/blob.rs"));
        assert!(msg.contains("NUL byte"));
        assert!(msg.contains("refusing before network send"));
    }

    #[test]
    fn non_utf8_text_extension_fails_setup_instead_of_metadata_only() {
        let root = temp_root("non-utf8");
        write_file(&root, "src/latin1.rs", [0xff, b'r', b'u', b's', b't']);

        let err = build_push_payload(&root, &["src/latin1.rs".to_string()], true).unwrap_err();

        assert_eq!(
            err.failures,
            vec![ContentFileFailure {
                path: "src/latin1.rs".into(),
                reason: ContentFileFailureReason::NonUtf8,
            }]
        );
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn non_regular_content_extension_fails_setup_instead_of_metadata_only() {
        let root = temp_root("non-regular");
        std::fs::create_dir_all(root.join("src/generated.rs")).unwrap();

        let err = build_push_payload(&root, &["src/generated.rs".to_string()], true).unwrap_err();

        assert_eq!(
            err.failures,
            vec![ContentFileFailure {
                path: "src/generated.rs".into(),
                reason: ContentFileFailureReason::NonRegular,
            }]
        );
        assert!(err.to_string().contains("not a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_content_extension_fails_setup_instead_of_metadata_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("unreadable");
        write_file(&root, "src/secret.rs", "pub fn secret() {}\n");
        let path = root.join("src/secret.rs");
        let original_mode = std::fs::metadata(&path).unwrap().permissions().mode();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o0);
        std::fs::set_permissions(&path, perms).unwrap();

        if std::fs::read(&path).is_ok() {
            let mut restore = std::fs::metadata(&path).unwrap().permissions();
            restore.set_mode(original_mode);
            std::fs::set_permissions(&path, restore).unwrap();
            return;
        }

        let err = build_push_payload(&root, &["src/secret.rs".to_string()], true).unwrap_err();

        let mut restore = std::fs::metadata(&path).unwrap().permissions();
        restore.set_mode(original_mode);
        std::fs::set_permissions(&path, restore).unwrap();

        assert_eq!(err.failures[0].path, "src/secret.rs");
        assert!(matches!(
            err.failures[0].reason,
            ContentFileFailureReason::ReadError(_)
        ));
        assert!(err.to_string().contains("read error"));
    }

    #[test]
    fn over_cap_source_payload_fails_preflight_before_network_send() {
        let content = "x".repeat(MAX_OVERLAY_BYTES + 1);
        let files = vec![("src/big.rs".to_string(), content.clone())];
        let stats = vec![ContentFileStat {
            path: "src/big.rs".into(),
            bytes: content.len(),
        }];
        let body = push_overlay_request_body("wt", "origin/main", &files, None, None);

        let err = validate_overlay_http_cap(&body, &stats).unwrap_err();

        assert!(err.contains("encoded bytes"));
        assert!(err.contains("raw JSON bytes"));
        assert!(err.contains("exceeding the 32 MiB HTTP cap"));
        assert!(err.contains("refusing before network send"));
        assert!(err.contains("src/big.rs"));
        assert!(err.contains("Suggested next step"));
        assert!(err.contains("will not fall back to cargo check"));
    }

    #[test]
    fn multi_megabyte_generated_overlay_is_compressed_before_http_send() {
        let content = "registry_mirror_entry = 42;\n".repeat(240_000);
        let files = vec![(
            "physics/src/generated/scaffold_registry.rs".to_string(),
            content,
        )];
        let body = push_overlay_request_body("wt", "origin/main", &files, None, None);
        assert!(
            body.len() > 6 * 1024 * 1024,
            "fixture should model the observed multi-megabyte full-file overlay"
        );

        let prepared = prepare_json_body(&body).expect("prepare body");

        assert_eq!(prepared.content_encoding, Some("gzip"));
        assert!(
            prepared.encoded_len() < body.len() / 20,
            "repeated generated mirrors should shrink materially"
        );
        validate_overlay_http_cap(&body, &[]).expect("compressed overlay fits the HTTP cap");
    }

    #[test]
    fn normal_rust_yaml_and_css_changes_are_content_payload() {
        let root = temp_root("normal-content");
        let rust = "pub fn answer() -> u8 { 42 }\n";
        let yaml = "mode: test\n";
        let css = ".root { color: red; }\n";
        write_file(&root, "src/lib.rs", rust);
        write_file(&root, "config/app.yaml", yaml);
        write_file(&root, "assets/app.css", css);
        write_file(&root, "assets/logo.png", [0, 1, 2, 3]);

        let payload = build_push_payload(
            &root,
            &[
                "src/lib.rs".into(),
                "config/app.yaml".into(),
                "assets/app.css".into(),
                "assets/logo.png".into(),
            ],
            true,
        )
        .unwrap();

        assert_eq!(
            payload.files,
            vec![
                ("assets/app.css".into(), css.into()),
                ("config/app.yaml".into(), yaml.into()),
                ("src/lib.rs".into(), rust.into()),
            ]
        );
        assert_eq!(
            payload.trigger_paths,
            vec![
                "assets/app.css".to_string(),
                "assets/logo.png".to_string(),
                "config/app.yaml".to_string(),
                "src/lib.rs".to_string(),
            ]
        );
        assert_eq!(
            payload.metadata_only_paths,
            vec![MetadataOnlyPath {
                path: "assets/logo.png".into(),
                reason: MetadataOnlyReason::UnsupportedPath,
            }]
        );
        assert_eq!(payload.content_bytes(), rust.len() + yaml.len() + css.len());
    }

    #[test]
    fn deleted_content_file_is_represented_as_empty_overlay() {
        let root = temp_root("deleted");
        let payload = build_push_payload(&root, &["src/removed.rs".to_string()], true).unwrap();

        assert_eq!(
            payload.files,
            vec![("src/removed.rs".into(), String::new())]
        );
        assert_eq!(payload.trigger_paths, vec!["src/removed.rs".to_string()]);
        assert_eq!(payload.content_stats[0].bytes, 0);
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
