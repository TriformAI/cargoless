//! `cargoless checks` — inspect and run native project checks.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::ui;
use cargoless_core::project_checks::{ProjectCheckReport, ProjectCheckResult};
use cargoless_core::{Diagnostic, TreeState};

pub fn run(
    cfg: &Config,
    action: Option<&str>,
    id: Option<&str>,
    profile: Option<&str>,
    base: Option<&str>,
    allow_existing_red: bool,
    report_json: Option<&Path>,
) -> ExitCode {
    match action.unwrap_or("list") {
        "list" => list(cfg),
        "run" => run_checks(
            cfg,
            id,
            profile.unwrap_or("dev"),
            base,
            allow_existing_red,
            report_json,
        ),
        "explain" => explain(cfg, id),
        other => {
            ui::error(format!(
                "unknown checks action: {other} (expected list, run, or explain)"
            ));
            ExitCode::from(2)
        }
    }
}

fn list(cfg: &Config) -> ExitCode {
    match cargoless_core::project_checks::list(&cfg.root) {
        Ok(items) if items.is_empty() => {
            ui::ok("no cargoless.checks.yaml manifest found");
            ExitCode::SUCCESS
        }
        Ok(items) => {
            for item in items {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    item.id,
                    item.kind,
                    item.tier,
                    if item.required {
                        "required"
                    } else {
                        "advisory"
                    },
                    item.title
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            ui::error(format!("{}:{}: {}", e.path.display(), e.line, e.message));
            ExitCode::from(2)
        }
    }
}

fn explain(cfg: &Config, id: Option<&str>) -> ExitCode {
    let Some(id) = id else {
        ui::error("checks explain requires a check id");
        return ExitCode::from(2);
    };
    match cargoless_core::project_checks::explain(&cfg.root, id) {
        Ok(Some(e)) => {
            println!("id: {}", e.summary.id);
            println!("title: {}", e.summary.title);
            println!("kind: {}", e.summary.kind);
            println!("tier: {}", e.summary.tier);
            println!("required: {}", e.summary.required);
            println!("timeout_ms: {}", e.timeout_ms);
            println!("cache: {}", e.cache);
            println!("triggers:");
            for t in e.triggers {
                println!("  - {t}");
            }
            println!("inputs:");
            for input in e.inputs {
                println!("  - {input}");
            }
            ExitCode::SUCCESS
        }
        Ok(None) => {
            ui::error(format!("unknown project check id: {id}"));
            ExitCode::from(2)
        }
        Err(e) => {
            ui::error(format!("{}:{}: {}", e.path.display(), e.line, e.message));
            ExitCode::from(2)
        }
    }
}

fn run_checks(
    cfg: &Config,
    id: Option<&str>,
    profile: &str,
    base: Option<&str>,
    allow_existing_red: bool,
    report_json: Option<&Path>,
) -> ExitCode {
    let changed_files = match base {
        Some(base) => match crate::push::git_changed_files(&cfg.root, base) {
            Ok(files) => Some((base.to_string(), files)),
            Err(e) => {
                ui::error(format!(
                    "could not determine changed files for project-check pruning against `{base}`: {e}"
                ));
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let changed_slice = changed_files.as_ref().map(|(_, files)| files.as_slice());
    let current = match cargoless_core::project_checks::run_profile_with_changes(
        &cfg.root,
        profile,
        id,
        changed_slice,
    ) {
        Ok(report) => report,
        Err(e) => {
            ui::error(format!("could not run project checks: {e}"));
            return ExitCode::from(2);
        }
    };

    let classifications = if allow_existing_red && current.tree == TreeState::Red {
        let Some((base_ref, _)) = changed_files.as_ref() else {
            ui::error("checks run --allow-existing-red requires --base <ref>");
            return ExitCode::from(2);
        };
        match classify_required_reds_at_base(&cfg.root, base_ref, profile, &current, changed_slice)
        {
            Ok(v) => v,
            Err(e) => {
                ui::error(format!(
                    "could not compare project-check reds against `{base_ref}`: {e}"
                ));
                return ExitCode::from(2);
            }
        }
    } else {
        Vec::new()
    };

    if let Some(path) = report_json {
        if let Err(e) = write_report_json(
            path,
            profile,
            changed_files.as_ref(),
            &current,
            &classifications,
            allow_existing_red,
        ) {
            ui::error(format!("could not write check report JSON: {e}"));
            return ExitCode::from(2);
        }
    }

    {
        let mut err = std::io::stderr();
        let _ = crate::check::render_diagnostics(&mut err, &cfg.root, &current.diagnostics);
        let _ = err.flush();
        let failed = current
            .results
            .iter()
            .filter(|r| r.required && r.tree == TreeState::Red)
            .count();
        let existing = classifications
            .iter()
            .filter(|c| c.classification == RedClass::Existing)
            .count();
        let new = classifications
            .iter()
            .filter(|c| c.classification == RedClass::New)
            .count();
        let cache_hits = current.results.iter().filter(|r| r.cache_hit).count();
        let ran = current.results.len();
        let scope = check_scope_summary(changed_files.as_ref(), current.skipped.len());
        if current.tree == TreeState::Green {
            ui::ok(format!(
                "project checks green — {ran} check{} evaluated, {} skipped ({cache_hits} cache hit{}) in {}ms{scope}",
                if ran == 1 { "" } else { "s" },
                current.skipped.len(),
                if cache_hits == 1 { "" } else { "s" },
                current.duration_ms,
            ));
            ExitCode::SUCCESS
        } else if allow_existing_red && new == 0 {
            ui::ok(format!(
                "project checks green-with-existing-red — {existing} inherited required red{} accepted; {ran} check{} evaluated, {} skipped ({cache_hits} cache hit{}) in {}ms{scope}",
                if existing == 1 { "" } else { "s" },
                if ran == 1 { "" } else { "s" },
                current.skipped.len(),
                if cache_hits == 1 { "" } else { "s" },
                current.duration_ms,
            ));
            ExitCode::SUCCESS
        } else {
            ui::error(format!(
                "project checks red — {failed} required check{} failed out of {ran} ({} skipped) in {}ms{scope}",
                if failed == 1 { "" } else { "s" },
                current.skipped.len(),
                current.duration_ms,
            ));
            if allow_existing_red {
                ui::error(format!(
                    "base comparison: {new} new/worsened required red{}, {existing} inherited required red{}",
                    if new == 1 { "" } else { "s" },
                    if existing == 1 { "" } else { "s" },
                ));
            }
            ExitCode::from(1)
        }
    }
}

fn check_scope_summary(changed_files: Option<&(String, Vec<String>)>, skipped: usize) -> String {
    let Some((base, changed)) = changed_files else {
        return " [scope=full]".to_string();
    };
    format!(
        " [scope=changed base={} changed_paths={} skipped_untriggered={}]",
        base,
        changed.len(),
        skipped
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RedClassification {
    id: String,
    title: String,
    classification: RedClass,
    current_fingerprints: BTreeMap<String, usize>,
    base_fingerprints: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedClass {
    Existing,
    New,
}

impl RedClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Existing => "existing",
            Self::New => "new",
        }
    }
}

fn classify_required_reds_at_base(
    root: &Path,
    base: &str,
    profile: &str,
    current: &ProjectCheckReport,
    changed_files: Option<&[String]>,
) -> std::io::Result<Vec<RedClassification>> {
    let reds: Vec<&ProjectCheckResult> = current
        .results
        .iter()
        .filter(|r| r.required && r.tree == TreeState::Red)
        .collect();
    if reds.is_empty() {
        return Ok(Vec::new());
    }

    let worktree = BaseWorktree::create(root, base)?;
    let mut out = Vec::new();
    for red in reds {
        let base_report = cargoless_core::project_checks::run_profile_with_changes(
            &worktree.path,
            profile,
            Some(red.id.as_str()),
            changed_files,
        )?;
        let base_result = base_report
            .results
            .iter()
            .find(|r| r.id == red.id && r.required && r.tree == TreeState::Red);
        let current_fingerprints =
            cargoless_core::attribution::fingerprint_counts(root, &red.diagnostics);
        let base_fingerprints = base_result
            .map(|r| {
                cargoless_core::attribution::fingerprint_counts(&worktree.path, &r.diagnostics)
            })
            .unwrap_or_default();
        let classification = if cargoless_core::attribution::are_inherited(
            &current_fingerprints,
            &base_fingerprints,
        ) {
            RedClass::Existing
        } else {
            RedClass::New
        };
        out.push(RedClassification {
            id: red.id.clone(),
            title: red.title.clone(),
            classification,
            current_fingerprints,
            base_fingerprints,
        });
    }
    Ok(out)
}

struct BaseWorktree {
    repo: PathBuf,
    path: PathBuf,
}

impl BaseWorktree {
    fn create(root: &Path, base: &str) -> std::io::Result<Self> {
        let repo = fs::canonicalize(root)?;
        // Warn loudly if `<base>` resolves to a local tracking ref that is far
        // behind real origin — a phantom-count classification under
        // `--allow-existing-red` against a stale `--base` is the exact wedge
        // PYAML hit on donut (~5 false-block dev-merges). Detect-only; the
        // operator decides whether to `git fetch` and retry. ls-remote is
        // read-only and does not advance any local ref.
        if let Some(report) = base_freshness_warning(&repo, base) {
            ui::warn(report);
        }
        let path = std::env::temp_dir().join(format!(
            "cargoless-check-base-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "--detach", "--quiet"])
            .arg(&path)
            .arg(base)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(std::io::Error::other(format!(
                "git worktree add failed for `{base}`: {stderr}{stdout}"
            )));
        }
        Ok(Self { repo, path })
    }
}

/// Inspect `<base>` against the configured `origin` remote and return a
/// warning string when the local tracking ref is meaningfully behind real
/// origin. Policy: when `rev-list --count` succeeds (objects in odb),
/// warn only if drift ≥ threshold. When `rev-list --count` fails (objects
/// not in odb — the common case when local hasn't fetched recently), warn
/// regardless, because the user is reasoning against a base they cannot
/// even fully describe locally. Returns `None` whenever the check is
/// inconclusive (offline, detached base SHA with no matching tracking
/// ref, no origin remote, env override disables) — this is advisory only,
/// never fatal.
fn base_freshness_warning(repo: &Path, base: &str) -> Option<String> {
    if std::env::var("CARGOLESS_BASE_STALE_CHECK").as_deref() == Ok("0") {
        return None;
    }
    // Default 5 commits: covers the PYAML wedge (~5 dev-merge false-blocks
    // came from a local origin/dev many commits behind real origin) while
    // staying quiet on the normal hot-trunk case where origin/dev legitimately
    // advances a few commits between fetches. Tune lower in CI environments
    // where every drift counts; tune higher on a slow trunk.
    let threshold: u32 = std::env::var("CARGOLESS_BASE_STALE_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    let tracking_ref = resolve_tracking_ref(repo, base)?;
    let short = tracking_ref.strip_prefix("refs/remotes/origin/")?;
    let local_tip = git_rev_parse(repo, &tracking_ref)?;
    let remote_tip = ls_remote_origin_branch(repo, short)?;
    if local_tip == remote_tip {
        return None;
    }
    // `rev-list --count <local>..<remote>` only works when the remote object
    // is already in the local odb. In the exact production wedge — local
    // never fetched recently — it won't be, and the count fails. Two regimes:
    //   Some(n): use threshold.
    //   None:    we cannot count, but tips differ. Warn anyway (drift is
    //            real); the message just says "unknown number of commits".
    // Without this fallback the helper would silently no-warn precisely in
    // the case it was written to catch.
    let behind = git_rev_list_count(repo, &local_tip, &remote_tip);
    if behind.is_some_and(|n| n < threshold) {
        return None;
    }
    let drift = match behind {
        Some(n) => format!("{n} commit(s)"),
        None => "an unknown number of commits".to_string(),
    };
    let local = short_sha(&local_tip);
    let remote = short_sha(&remote_tip);
    Some(format!(
        "stale-base: local `{short}` is {drift} behind `origin/{short}` \
         ({local} vs {remote}). Running `--allow-existing-red` classification against \
         a stale base can report phantom counts for whole-tree fingerprint checks. \
         Run `git fetch origin {short}` and retry, or set \
         CARGOLESS_BASE_STALE_CHECK=0 to silence."
    ))
}

/// Map `<base>` to a `refs/remotes/origin/<branch>` tracking ref when
/// possible. Recognises three shapes: an explicit `origin/<branch>` /
/// `refs/remotes/origin/<branch>`; a local SHA whose closest descendant
/// tracking ref we can name; or a bare branch name that has a tracking
/// counterpart. Returns `None` for anything else (HEAD, tags, detached
/// commits with no tracking descendant).
fn resolve_tracking_ref(repo: &Path, base: &str) -> Option<String> {
    if let Some(name) = git_symbolic_full_name(repo, base) {
        if name.starts_with("refs/remotes/origin/") {
            return Some(name);
        }
        if let Some(branch) = name.strip_prefix("refs/heads/") {
            let candidate = format!("refs/remotes/origin/{branch}");
            if git_rev_parse(repo, &candidate).is_some() {
                return Some(candidate);
            }
        }
    }
    let sha = git_rev_parse(repo, base)?;
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "--contains",
            &sha,
            "refs/remotes/origin/",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|l| !l.is_empty() && !l.ends_with("/HEAD"))
        .map(str::to_string)
}

fn git_symbolic_full_name(repo: &Path, refish: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--symbolic-full-name", refish])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() { None } else { Some(name) }
}

fn git_rev_parse(repo: &Path, refish: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{refish}^{{commit}}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

fn ls_remote_origin_branch(repo: &Path, branch: &str) -> Option<String> {
    // Null stdin so a credential prompt cannot wedge the child; HTTP
    // low-speed bound caps a black-hole TCP read; terminal-prompt off
    // suppresses askpass on cred lookups.
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-remote", "--exit-code", "origin"])
        .arg(format!("refs/heads/{branch}"))
        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1000")
        .env("GIT_HTTP_LOW_SPEED_TIME", "10")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .to_string();
    line.split_whitespace().next().map(str::to_string)
}

fn git_rev_list_count(repo: &Path, from: &str, to: &str) -> Option<u32> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--count"])
        .arg(format!("{from}..{to}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(10).collect()
}

impl Drop for BaseWorktree {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .status();
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn write_report_json(
    path: &Path,
    profile: &str,
    changed_files: Option<&(String, Vec<String>)>,
    report: &ProjectCheckReport,
    classifications: &[RedClassification],
    allow_existing_red: bool,
) -> std::io::Result<()> {
    let new_required_reds = classifications
        .iter()
        .filter(|c| c.classification == RedClass::New)
        .count();
    let existing_required_reds = classifications
        .iter()
        .filter(|c| c.classification == RedClass::Existing)
        .count();
    let required_reds = report
        .results
        .iter()
        .filter(|r| r.required && r.tree == TreeState::Red)
        .count();
    let decision = if report.tree == TreeState::Green {
        "green"
    } else if allow_existing_red && new_required_reds == 0 {
        "green_with_existing_red"
    } else {
        "red"
    };
    let (base, changed_paths) = changed_files
        .map(|(base, files)| (Some(base.as_str()), files.as_slice()))
        .unwrap_or((None, &[]));
    let classifications_json: Vec<serde_json::Value> = classifications
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "title": c.title,
                "classification": c.classification.as_str(),
                "current_fingerprints": c.current_fingerprints,
                "base_fingerprints": c.base_fingerprints,
            })
        })
        .collect();
    let results_json: Vec<serde_json::Value> = report
        .results
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "title": r.title,
                "required": r.required,
                "tree": if r.tree == TreeState::Green { "green" } else { "red" },
                "duration_ms": r.duration_ms,
                "cache_hit": r.cache_hit,
                "diagnostics": diagnostics_json(&r.diagnostics),
            })
        })
        .collect();
    let value = serde_json::json!({
        "decision": decision,
        "profile": profile,
        "base": base,
        "changed_paths": changed_paths,
        "evaluated": report.results.len(),
        "skipped": report.skipped.len(),
        "duration_ms": report.duration_ms,
        "allow_existing_red": allow_existing_red,
        "required_reds": required_reds,
        "new_required_reds": new_required_reds,
        "existing_required_reds": existing_required_reds,
        "red_classifications": classifications_json,
        "results": results_json,
    });
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let text =
        serde_json::to_string_pretty(&value).map_err(|e| std::io::Error::other(e.to_string()))?;
    fs::write(path, format!("{text}\n"))
}

fn diagnostics_json(diagnostics: &[Diagnostic]) -> Vec<serde_json::Value> {
    diagnostics
        .iter()
        .map(|d| {
            serde_json::json!({
                "file_path": d.file_path.to_string_lossy(),
                "line": d.line,
                "col": d.col,
                "severity": d.severity.as_str(),
                "code": d.code,
                "source": d.source,
                "message": d.message,
            })
        })
        .collect()
}

#[cfg(test)]
mod stale_base_tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "cargoless-checks-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn git(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit_file(repo: &Path, name: &str, content: &str, message: &str) -> String {
        std::fs::write(repo.join(name), content).unwrap();
        git(repo, &["add", name]);
        git(repo, &["commit", "-q", "-m", message]);
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Two-repo fixture: an upstream `origin` bare repo that the local clone
    /// tracks. We can advance origin then check whether the local clone's
    /// `refs/remotes/origin/dev` is detected as stale relative to a fresh
    /// `git ls-remote` — the exact wedge classify-at-base hits in production.
    fn fixture() -> (PathBuf, PathBuf, PathBuf) {
        let scratch = temp_root("stale-base");
        let upstream_seed = scratch.join("seed");
        let upstream_bare = scratch.join("origin.git");
        let local = scratch.join("local");

        std::fs::create_dir_all(&upstream_seed).unwrap();
        git(&upstream_seed, &["init", "-q", "-b", "dev"]);
        commit_file(&upstream_seed, "README.md", "seed\n", "seed");
        git(
            &upstream_seed,
            &[
                "clone",
                "--bare",
                "-q",
                upstream_seed.to_str().unwrap(),
                upstream_bare.to_str().unwrap(),
            ],
        );
        git(
            local.parent().unwrap(),
            &[
                "clone",
                "-q",
                upstream_bare.to_str().unwrap(),
                local.to_str().unwrap(),
            ],
        );
        (scratch, upstream_bare, local)
    }

    fn cleanup(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    /// **Happy path.** Local `origin/dev` matches real origin → no warning.
    /// Proves the freshness check is silent when there is no drift,
    /// guarding against a "warn on every invocation" regression that would
    /// turn the signal into noise.
    #[test]
    fn freshness_silent_when_local_tracks_origin() {
        let (scratch, _origin, local) = fixture();
        let canon = std::fs::canonicalize(&local).unwrap();
        let warning = base_freshness_warning(&canon, "origin/dev");
        assert!(warning.is_none(), "no drift ⇒ no warning, got: {warning:?}");
        cleanup(&scratch);
    }

    /// **The wedge case — local lacks remote objects.** Origin advances 6
    /// commits while the local clone holds its old tracking ref. Because
    /// the local clone never `git fetch`ed, its odb does NOT contain the
    /// new remote object → `rev-list --count` cannot count and we fall
    /// through to the "unknown number of commits behind" message. This is
    /// the production wedge: detection must still fire, even without a
    /// precise count, because the tips differ and that's enough to know
    /// the base classification is unreliable.
    #[test]
    fn freshness_warns_when_local_lacks_remote_objects() {
        let (scratch, origin_bare, local) = fixture();
        let canon = std::fs::canonicalize(&local).unwrap();

        let pusher = scratch.join("pusher");
        git(
            &scratch,
            &[
                "clone",
                "-q",
                origin_bare.to_str().unwrap(),
                pusher.to_str().unwrap(),
            ],
        );
        for i in 0..6 {
            commit_file(
                &pusher,
                "README.md",
                &format!("rev {i}\n"),
                &format!("c{i}"),
            );
        }
        git(&pusher, &["push", "-q", "origin", "dev"]);

        let warning = base_freshness_warning(&canon, "origin/dev")
            .expect("local origin/dev is behind real origin/dev — must warn");
        assert!(warning.contains("stale-base"), "{warning}");
        assert!(warning.contains("git fetch origin dev"), "{warning}");
        assert!(
            warning.contains("unknown number of commits behind"),
            "objects unavailable ⇒ fallback wording: {warning}"
        );
        cleanup(&scratch);
    }

    /// **Count-known path.** Local fetched the remote objects (so the odb
    /// has them) but its tracking ref is held back. `rev-list --count`
    /// succeeds → the warning surfaces the precise drift count. Mirrors
    /// the "background fetch landed objects, no ref update" scenario.
    #[test]
    fn freshness_warns_with_count_when_objects_available() {
        let (scratch, origin_bare, local) = fixture();
        let canon = std::fs::canonicalize(&local).unwrap();

        let pusher = scratch.join("pusher");
        git(
            &scratch,
            &[
                "clone",
                "-q",
                origin_bare.to_str().unwrap(),
                pusher.to_str().unwrap(),
            ],
        );
        for i in 0..6 {
            commit_file(
                &pusher,
                "README.md",
                &format!("rev {i}\n"),
                &format!("c{i}"),
            );
        }
        git(&pusher, &["push", "-q", "origin", "dev"]);

        // Snapshot old local tip, then fetch objects, then rewind ref.
        let old = Command::new("git")
            .arg("-C")
            .arg(&canon)
            .args(["rev-parse", "refs/remotes/origin/dev"])
            .output()
            .unwrap();
        let old_sha = String::from_utf8_lossy(&old.stdout).trim().to_string();
        git(&canon, &["fetch", "-q", "origin", "dev"]);
        git(
            &canon,
            &["update-ref", "refs/remotes/origin/dev", old_sha.as_str()],
        );

        let warning = base_freshness_warning(&canon, "origin/dev")
            .expect("objects present, ref held back — must warn with count");
        assert!(
            warning.contains("6 commit(s) behind"),
            "warning surfaces precise count: {warning}"
        );
        cleanup(&scratch);
    }

    /// **Threshold honoured (count-known path).** Local has fetched the
    /// remote objects but the tracking ref is held back by 1 commit. The
    /// count IS known → threshold gating applies → 1 < default 5 ⇒ silent.
    /// Protects against the hot-trunk noise case where origin/dev
    /// legitimately advances 1–2 commits between local fetches and the
    /// user has already pulled them. Note: when objects are NOT in the
    /// odb (the more common case for stale clones), we cannot count and
    /// the helper warns regardless — that's intentional and is exercised
    /// by `freshness_warns_when_local_lacks_remote_objects`.
    #[test]
    fn freshness_silent_when_drift_below_threshold_and_count_known() {
        let (scratch, origin_bare, local) = fixture();
        let canon = std::fs::canonicalize(&local).unwrap();
        let pusher = scratch.join("pusher");
        git(
            &scratch,
            &[
                "clone",
                "-q",
                origin_bare.to_str().unwrap(),
                pusher.to_str().unwrap(),
            ],
        );
        commit_file(&pusher, "README.md", "rev 1\n", "c1");
        git(&pusher, &["push", "-q", "origin", "dev"]);

        // Snapshot old tip, fetch objects, rewind ref by 1.
        let old = Command::new("git")
            .arg("-C")
            .arg(&canon)
            .args(["rev-parse", "refs/remotes/origin/dev"])
            .output()
            .unwrap();
        let old_sha = String::from_utf8_lossy(&old.stdout).trim().to_string();
        git(&canon, &["fetch", "-q", "origin", "dev"]);
        git(
            &canon,
            &["update-ref", "refs/remotes/origin/dev", old_sha.as_str()],
        );

        let warning = base_freshness_warning(&canon, "origin/dev");
        assert!(
            warning.is_none(),
            "1-commit drift, count known ⇡ below default threshold 5 ⇒ silent, got: {warning:?}"
        );
        cleanup(&scratch);
    }

    /// **Unknown ref.** A base that does not resolve to any
    /// `refs/remotes/origin/*` tracking ref returns `None` — the check
    /// cannot say anything, and silently no-ops rather than crashing.
    /// Covers raw-SHA bases that aren't reachable from any origin branch
    /// (eg. a local-only commit).
    #[test]
    fn freshness_silent_for_unresolvable_base() {
        let (scratch, _origin, local) = fixture();
        let canon = std::fs::canonicalize(&local).unwrap();
        // Detached local commit not pushed anywhere.
        commit_file(&local, "local-only.txt", "x\n", "local-only");
        let head = Command::new("git")
            .arg("-C")
            .arg(&canon)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let local_sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
        let warning = base_freshness_warning(&canon, &local_sha);
        assert!(
            warning.is_none(),
            "local-only sha has no origin tracking ref ⇒ silent, got: {warning:?}"
        );
        cleanup(&scratch);
    }
}
