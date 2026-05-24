//! Native project-check engine.
//!
//! This is intentionally generic: repositories declare fast project rules in
//! `cargoless.checks.yaml`; cargoless compiles that into Rust checks, caches
//! per-check results, and emits ordinary diagnostics with source
//! `cargoless-check:<id>`. The engine knows nothing about tf-multiverse,
//! chemistry, portal, or generated-code policy.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use cargoless_cas::sha256_hex;
use cargoless_proto::{Diagnostic, Severity, TreeState};

const MANIFEST_NAME: &str = "cargoless.checks.yaml";
const ENGINE_VERSION: &str = "cargoless/project-checks/v2";
const TIMEOUT_DIAGNOSTIC_CODE: &str = "project-check.timeout";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCheckReport {
    pub tree: TreeState,
    pub diagnostics: Vec<Diagnostic>,
    pub results: Vec<ProjectCheckResult>,
    pub skipped: Vec<CheckSummary>,
    pub duration_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCheckResult {
    pub id: String,
    pub title: String,
    pub required: bool,
    pub tree: TreeState,
    pub diagnostics: Vec<Diagnostic>,
    pub duration_ms: u128,
    pub cache_hit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckSummary {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub tier: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckExplanation {
    pub summary: CheckSummary,
    pub triggers: Vec<String>,
    pub inputs: Vec<String>,
    pub timeout_ms: u64,
    pub cache: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError {
    pub path: PathBuf,
    pub line: usize,
    pub message: String,
}

impl ManifestError {
    fn diagnostic(&self, root: &Path) -> Diagnostic {
        Diagnostic {
            file_path: root.join(&self.path),
            line: self.line as u32,
            col: 1,
            severity: Severity::Error,
            code: Some("project-checks.manifest".to_string()),
            message: self.message.clone(),
            source: Some("cargoless-check:manifest".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectChecksManifest {
    profiles: BTreeMap<String, ProfileConfig>,
    checks: Vec<CheckConfig>,
    manifest_hash: String,
}

impl ProjectChecksManifest {
    pub fn summaries(&self) -> Vec<CheckSummary> {
        self.checks
            .iter()
            .map(|c| CheckSummary {
                id: c.id.clone(),
                title: c.title.clone(),
                kind: c.kind.as_str().to_string(),
                tier: c.tier.clone(),
                required: c.required,
            })
            .collect()
    }

    pub fn explain(&self, id: &str) -> Option<CheckExplanation> {
        self.checks
            .iter()
            .find(|c| c.id == id)
            .map(|c| CheckExplanation {
                summary: CheckSummary {
                    id: c.id.clone(),
                    title: c.title.clone(),
                    kind: c.kind.as_str().to_string(),
                    tier: c.tier.clone(),
                    required: c.required,
                },
                triggers: c.triggers.clone(),
                inputs: c.inputs.clone(),
                timeout_ms: c.timeout_ms,
                cache: c.cache.clone(),
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileConfig {
    include: Vec<String>,
    timeout_ms: u64,
    max_parallel: usize,
    on_timeout: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckConfig {
    id: String,
    title: String,
    tier: String,
    required: bool,
    kind: CheckKind,
    triggers: Vec<String>,
    inputs: Vec<String>,
    timeout_ms: u64,
    cache: String,
    source_root: Option<String>,
    mirrors: Vec<MirrorConfig>,
    patterns: Vec<PatternRule>,
    rules: Vec<DataRule>,
    paths: Vec<String>,
    command: Vec<String>,
    read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CheckKind {
    MirrorDrift,
    ForbiddenPatterns,
    RequiredPatterns,
    YamlRules,
    JsonRules,
    FileExists,
    Command,
}

impl CheckKind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "mirror_drift" => Some(Self::MirrorDrift),
            "forbidden_patterns" => Some(Self::ForbiddenPatterns),
            "required_patterns" => Some(Self::RequiredPatterns),
            "yaml_rules" => Some(Self::YamlRules),
            "json_rules" => Some(Self::JsonRules),
            "file_exists" => Some(Self::FileExists),
            "command" => Some(Self::Command),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::MirrorDrift => "mirror_drift",
            Self::ForbiddenPatterns => "forbidden_patterns",
            Self::RequiredPatterns => "required_patterns",
            Self::YamlRules => "yaml_rules",
            Self::JsonRules => "json_rules",
            Self::FileExists => "file_exists",
            Self::Command => "command",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MirrorConfig {
    root: String,
    include: Vec<String>,
    exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PatternRule {
    code: String,
    message: String,
    literal: Option<String>,
    word: Option<String>,
    regex: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DataRule {
    code: String,
    message: String,
    require_path: Option<String>,
    forbid_path: Option<String>,
    equals_path: Option<String>,
    equals: Option<String>,
    one_of: Vec<String>,
}

#[derive(Debug, Clone)]
struct RepoSnapshot {
    files: Vec<FileInfo>,
}

#[derive(Debug, Clone)]
struct FileInfo {
    rel: String,
    abs: PathBuf,
}

impl RepoSnapshot {
    fn build(root: &Path) -> io::Result<Self> {
        let mut files = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(v) => v,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                let rel = rel_path(root, &path);
                if ignored_rel(&rel) {
                    continue;
                }
                if meta.is_dir() {
                    stack.push(path);
                } else if meta.is_file() {
                    files.push(FileInfo { rel, abs: path });
                }
            }
        }
        files.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(Self { files })
    }

    fn matching(&self, patterns: &[String]) -> Vec<FileInfo> {
        if patterns.is_empty() {
            return self.files.clone();
        }
        self.files
            .iter()
            .filter(|f| patterns.iter().any(|p| glob_match_path(p, &f.rel)))
            .cloned()
            .collect()
    }

    fn files_under(&self, root_rel: &str, include: &[String], exclude: &[String]) -> Vec<FileInfo> {
        let prefix = root_rel.trim_matches('/');
        self.files
            .iter()
            .filter_map(|f| {
                let local = f.rel.strip_prefix(prefix)?;
                let local = local.strip_prefix('/').unwrap_or(local);
                if local.is_empty() {
                    return None;
                }
                if !include.is_empty() && !include.iter().any(|p| glob_match_path(p, local)) {
                    return None;
                }
                if exclude.iter().any(|p| glob_match_path(p, local)) {
                    return None;
                }
                Some(f.clone())
            })
            .collect()
    }
}

pub fn load_manifest(root: &Path) -> Result<Option<ProjectChecksManifest>, ManifestError> {
    let path = root.join(MANIFEST_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ManifestError {
                path: PathBuf::from(MANIFEST_NAME),
                line: 1,
                message: format!("could not read {MANIFEST_NAME}: {e}"),
            });
        }
    };
    parse_manifest(PathBuf::from(MANIFEST_NAME), &text).map(Some)
}

pub fn list(root: &Path) -> Result<Vec<CheckSummary>, ManifestError> {
    Ok(load_manifest(root)?
        .map(|m| m.summaries())
        .unwrap_or_default())
}

pub fn explain(root: &Path, id: &str) -> Result<Option<CheckExplanation>, ManifestError> {
    Ok(load_manifest(root)?.and_then(|m| m.explain(id)))
}

pub fn run_dev(root: &Path) -> io::Result<ProjectCheckReport> {
    run_profile(root, "dev", None)
}

pub fn run_dev_with_changes(
    root: &Path,
    changed_files: Option<&[String]>,
) -> io::Result<ProjectCheckReport> {
    run_profile_with_changes(root, "dev", None, changed_files)
}

pub fn run_profile(
    root: &Path,
    profile_name: &str,
    only_id: Option<&str>,
) -> io::Result<ProjectCheckReport> {
    run_profile_with_changes(root, profile_name, only_id, None)
}

pub fn run_profile_with_changes(
    root: &Path,
    profile_name: &str,
    only_id: Option<&str>,
    changed_files: Option<&[String]>,
) -> io::Result<ProjectCheckReport> {
    let started = Instant::now();
    let root = fs::canonicalize(root)?;
    let manifest = match load_manifest(&root) {
        Ok(Some(m)) => m,
        Ok(None) => {
            return Ok(ProjectCheckReport {
                tree: TreeState::Green,
                diagnostics: Vec::new(),
                results: Vec::new(),
                skipped: Vec::new(),
                duration_ms: started.elapsed().as_millis(),
            });
        }
        Err(e) => {
            let diagnostic = e.diagnostic(&root);
            return Ok(ProjectCheckReport {
                tree: TreeState::Red,
                diagnostics: vec![diagnostic.clone()],
                results: vec![ProjectCheckResult {
                    id: "manifest".to_string(),
                    title: "project check manifest".to_string(),
                    required: true,
                    tree: TreeState::Red,
                    diagnostics: vec![diagnostic],
                    duration_ms: 0,
                    cache_hit: false,
                }],
                skipped: Vec::new(),
                duration_ms: started.elapsed().as_millis(),
            });
        }
    };

    let profile = match manifest.profiles.get(profile_name) {
        Some(v) => v.clone(),
        None => ProfileConfig {
            include: vec!["*".to_string()],
            timeout_ms: 12_000,
            max_parallel: 8,
            on_timeout: "red".to_string(),
        },
    };
    let selected: Vec<CheckConfig> = manifest
        .checks
        .iter()
        .filter(|c| only_id.is_none_or(|id| c.id == id))
        .filter(|c| profile_includes(&profile, c, profile_name))
        .cloned()
        .collect();
    let (selected, skipped) = select_for_changes(&root, selected, only_id, changed_files);
    let snapshot = Arc::new(RepoSnapshot::build(&root)?);
    let ctx = Arc::new(RunContext {
        root: root.clone(),
        snapshot,
        manifest_hash: manifest.manifest_hash.clone(),
        profile_name: profile_name.to_string(),
        changed_files: changed_files.map(|files| normalize_changed_files(&root, files)),
    });
    let results = run_parallel(
        ctx,
        selected,
        profile.max_parallel.max(1),
        profile.timeout_ms,
    );
    let mut diagnostics = Vec::new();
    let mut red = false;
    for result in &results {
        diagnostics.extend(result.diagnostics.iter().cloned());
        if result.required && result.tree == TreeState::Red {
            red = true;
        }
    }
    Ok(ProjectCheckReport {
        tree: if red {
            TreeState::Red
        } else {
            TreeState::Green
        },
        diagnostics,
        results,
        skipped,
        duration_ms: started.elapsed().as_millis(),
    })
}

fn select_for_changes(
    root: &Path,
    checks: Vec<CheckConfig>,
    only_id: Option<&str>,
    changed_files: Option<&[String]>,
) -> (Vec<CheckConfig>, Vec<CheckSummary>) {
    if only_id.is_some() {
        return (checks, Vec::new());
    }
    let Some(changed_files) = changed_files else {
        return (checks, Vec::new());
    };
    let changed = normalize_changed_files(root, changed_files);
    if changed.iter().any(|p| p == MANIFEST_NAME) {
        return (checks, Vec::new());
    }
    let mut run = Vec::new();
    let mut skipped = Vec::new();
    for check in checks {
        if check_matches_changes(&check, &changed) {
            run.push(check);
        } else {
            skipped.push(check_summary(&check));
        }
    }
    (run, skipped)
}

fn check_matches_changes(check: &CheckConfig, changed: &[String]) -> bool {
    let patterns = trigger_patterns(check);
    if patterns.is_empty() {
        return true;
    }
    changed.iter().any(|path| {
        patterns
            .iter()
            .any(|pattern| glob_match_path(pattern, path))
    })
}

fn trigger_patterns(check: &CheckConfig) -> Vec<String> {
    if !check.triggers.is_empty() {
        return check.triggers.clone();
    }
    input_patterns(check)
}

fn check_summary(check: &CheckConfig) -> CheckSummary {
    CheckSummary {
        id: check.id.clone(),
        title: check.title.clone(),
        kind: check.kind.as_str().to_string(),
        tier: check.tier.clone(),
        required: check.required,
    }
}

fn normalize_changed_files(root: &Path, changed_files: &[String]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for path in changed_files {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        let candidate = Path::new(trimmed);
        let rel = if candidate.is_absolute() {
            let candidate = candidate
                .canonicalize()
                .unwrap_or_else(|_| candidate.to_path_buf());
            candidate
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| trimmed.to_string())
        } else {
            trimmed.trim_start_matches("./").to_string()
        };
        let rel = rel.replace('\\', "/").trim_matches('/').to_string();
        if !rel.is_empty() {
            out.insert(rel);
        }
    }
    out.into_iter().collect()
}

fn profile_includes(profile: &ProfileConfig, check: &CheckConfig, profile_name: &str) -> bool {
    if profile.include.is_empty() {
        return check.tier == profile_name;
    }
    profile
        .include
        .iter()
        .any(|i| i == "*" || i == &check.id || i == &check.tier)
}

struct RunContext {
    root: PathBuf,
    snapshot: Arc<RepoSnapshot>,
    manifest_hash: String,
    profile_name: String,
    changed_files: Option<Vec<String>>,
}

fn run_parallel(
    ctx: Arc<RunContext>,
    checks: Vec<CheckConfig>,
    max_parallel: usize,
    profile_timeout_ms: u64,
) -> Vec<ProjectCheckResult> {
    let start = Instant::now();
    let mut pending: VecDeque<CheckConfig> = checks.into();
    let (tx, rx) = mpsc::channel();
    let mut in_flight = 0usize;
    let mut out = Vec::new();

    while !pending.is_empty() || in_flight > 0 {
        while in_flight < max_parallel && !pending.is_empty() {
            if start.elapsed() >= Duration::from_millis(profile_timeout_ms) {
                break;
            }
            let check = pending.pop_front().expect("pending not empty");
            let tx = tx.clone();
            let ctx = ctx.clone();
            in_flight += 1;
            thread::spawn(move || {
                let result = run_one(&ctx, check);
                let _ = tx.send(result);
            });
        }
        if in_flight == 0 {
            break;
        }
        match rx.recv() {
            Ok(result) => {
                out.push(result);
                in_flight -= 1;
            }
            Err(_) => break,
        }
    }

    for check in pending {
        out.push(timeout_result(
            &ctx.root,
            &check,
            "profile timeout reached before this check started",
        ));
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn run_one(ctx: &RunContext, check: CheckConfig) -> ProjectCheckResult {
    let started = Instant::now();
    if let Some(mut cached) = cache_get(ctx, &check) {
        cached.cache_hit = true;
        return cached;
    }

    let mut result = match check.kind {
        CheckKind::MirrorDrift => check_mirror_drift(ctx, &check),
        CheckKind::ForbiddenPatterns => check_forbidden_patterns(ctx, &check),
        CheckKind::RequiredPatterns => check_required_patterns(ctx, &check),
        CheckKind::YamlRules => check_yaml_rules(ctx, &check),
        CheckKind::JsonRules => check_json_rules(ctx, &check),
        CheckKind::FileExists => check_file_exists(ctx, &check),
        CheckKind::Command => check_command(ctx, &check),
    };
    result.duration_ms = started.elapsed().as_millis();
    if result.duration_ms > u128::from(check.timeout_ms) {
        result = timeout_result(
            &ctx.root,
            &check,
            &format!(
                "check exceeded timeout: {}ms > {}ms",
                result.duration_ms, check.timeout_ms
            ),
        );
    }
    cache_put(ctx, &check, &result);
    result
}

fn result_from_diags(
    check: &CheckConfig,
    diagnostics: Vec<Diagnostic>,
    duration_ms: u128,
) -> ProjectCheckResult {
    ProjectCheckResult {
        id: check.id.clone(),
        title: check.title.clone(),
        required: check.required,
        tree: if diagnostics.iter().any(|d| d.severity == Severity::Error) {
            TreeState::Red
        } else {
            TreeState::Green
        },
        diagnostics,
        duration_ms,
        cache_hit: false,
    }
}

fn timeout_result(root: &Path, check: &CheckConfig, message: &str) -> ProjectCheckResult {
    let mut result = result_from_diags(
        check,
        vec![diag(
            root,
            check,
            MANIFEST_NAME,
            1,
            1,
            TIMEOUT_DIAGNOSTIC_CODE,
            message,
        )],
        u128::from(check.timeout_ms),
    );
    if !check.required {
        for d in &mut result.diagnostics {
            d.severity = Severity::Warning;
        }
        result.tree = TreeState::Green;
    }
    result
}

fn diag(
    root: &Path,
    check: &CheckConfig,
    rel: &str,
    line: u32,
    col: u32,
    code: &str,
    message: &str,
) -> Diagnostic {
    Diagnostic {
        file_path: root.join(rel),
        line,
        col,
        severity: if check.required {
            Severity::Error
        } else {
            Severity::Warning
        },
        code: Some(code.to_string()),
        message: message.to_string(),
        source: Some(format!("cargoless-check:{}", check.id)),
    }
}

fn check_mirror_drift(ctx: &RunContext, check: &CheckConfig) -> ProjectCheckResult {
    let Some(source_root) = check.source_root.as_deref() else {
        return result_from_diags(
            check,
            vec![diag(
                &ctx.root,
                check,
                MANIFEST_NAME,
                1,
                1,
                "mirror_drift.missing_source_root",
                "mirror_drift check requires source_root",
            )],
            0,
        );
    };
    let mut diagnostics = Vec::new();
    for mirror in &check.mirrors {
        let source_files = ctx
            .snapshot
            .files_under(source_root, &mirror.include, &mirror.exclude);
        let mut expected = BTreeSet::new();
        for source in source_files {
            let local = source
                .rel
                .strip_prefix(source_root.trim_matches('/'))
                .unwrap_or(&source.rel)
                .trim_start_matches('/');
            expected.insert(local.to_string());
            let target_rel = join_rel(&mirror.root, local);
            let target_abs = ctx.root.join(&target_rel);
            if !target_abs.exists() {
                diagnostics.push(diag(
                    &ctx.root,
                    check,
                    &target_rel,
                    1,
                    1,
                    "mirror_drift.missing",
                    &format!("mirror file is missing for {local}"),
                ));
                continue;
            }
            let source_bytes = fs::read(&source.abs).unwrap_or_default();
            let target_bytes = fs::read(&target_abs).unwrap_or_default();
            if source_bytes != target_bytes {
                diagnostics.push(diag(
                    &ctx.root,
                    check,
                    &target_rel,
                    1,
                    1,
                    "mirror_drift.changed",
                    &format!("mirror file differs from {}/{}", source_root, local),
                ));
            }
        }
        for mirror_file in ctx
            .snapshot
            .files_under(&mirror.root, &mirror.include, &mirror.exclude)
        {
            let local = mirror_file
                .rel
                .strip_prefix(mirror.root.trim_matches('/'))
                .unwrap_or(&mirror_file.rel)
                .trim_start_matches('/');
            if !expected.contains(local) {
                diagnostics.push(diag(
                    &ctx.root,
                    check,
                    &mirror_file.rel,
                    1,
                    1,
                    "mirror_drift.extra",
                    &format!("mirror file has no source counterpart: {local}"),
                ));
            }
        }
    }
    result_from_diags(check, diagnostics, 0)
}

fn check_forbidden_patterns(ctx: &RunContext, check: &CheckConfig) -> ProjectCheckResult {
    let mut diagnostics = Vec::new();
    for file in ctx.snapshot.matching(&input_patterns(check)) {
        let Ok(text) = fs::read_to_string(&file.abs) else {
            continue;
        };
        for rule in &check.patterns {
            if let Some(pos) = pattern_find(rule, &text) {
                let (line, col) = line_col(&text, pos);
                diagnostics.push(diag(
                    &ctx.root,
                    check,
                    &file.rel,
                    line,
                    col,
                    &rule.code,
                    &rule.message,
                ));
            }
        }
    }
    result_from_diags(check, diagnostics, 0)
}

fn check_required_patterns(ctx: &RunContext, check: &CheckConfig) -> ProjectCheckResult {
    let mut diagnostics = Vec::new();
    for file in ctx.snapshot.matching(&input_patterns(check)) {
        let Ok(text) = fs::read_to_string(&file.abs) else {
            continue;
        };
        for rule in &check.patterns {
            if pattern_find(rule, &text).is_none() {
                diagnostics.push(diag(
                    &ctx.root,
                    check,
                    &file.rel,
                    1,
                    1,
                    &rule.code,
                    &rule.message,
                ));
            }
        }
    }
    result_from_diags(check, diagnostics, 0)
}

fn check_yaml_rules(ctx: &RunContext, check: &CheckConfig) -> ProjectCheckResult {
    let mut diagnostics = Vec::new();
    for file in ctx.snapshot.matching(&input_patterns(check)) {
        let Ok(text) = fs::read_to_string(&file.abs) else {
            continue;
        };
        let value = match parse_yaml_value(&text) {
            Ok(v) => v,
            Err(e) => {
                diagnostics.push(diag(
                    &ctx.root,
                    check,
                    &file.rel,
                    e.line as u32,
                    1,
                    "yaml_rules.parse",
                    &e.message,
                ));
                continue;
            }
        };
        apply_data_rules(&mut diagnostics, &ctx.root, check, &file.rel, &value, None);
    }
    result_from_diags(check, diagnostics, 0)
}

fn check_json_rules(ctx: &RunContext, check: &CheckConfig) -> ProjectCheckResult {
    let mut diagnostics = Vec::new();
    for file in ctx.snapshot.matching(&input_patterns(check)) {
        let Ok(text) = fs::read_to_string(&file.abs) else {
            continue;
        };
        let value = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => v,
            Err(e) => {
                diagnostics.push(diag(
                    &ctx.root,
                    check,
                    &file.rel,
                    e.line() as u32,
                    e.column() as u32,
                    "json_rules.parse",
                    &e.to_string(),
                ));
                continue;
            }
        };
        apply_data_rules(
            &mut diagnostics,
            &ctx.root,
            check,
            &file.rel,
            &YamlNode::Null(1),
            Some(&value),
        );
    }
    result_from_diags(check, diagnostics, 0)
}

fn check_file_exists(ctx: &RunContext, check: &CheckConfig) -> ProjectCheckResult {
    let mut diagnostics = Vec::new();
    for path in &check.paths {
        if !ctx.root.join(path).exists() {
            diagnostics.push(diag(
                &ctx.root,
                check,
                path,
                1,
                1,
                "file_exists.missing",
                &format!("required file does not exist: {path}"),
            ));
        }
    }
    result_from_diags(check, diagnostics, 0)
}

fn check_command(ctx: &RunContext, check: &CheckConfig) -> ProjectCheckResult {
    if ctx.profile_name == "dev" && !check.read_only {
        return result_from_diags(
            check,
            vec![diag(
                &ctx.root,
                check,
                MANIFEST_NAME,
                1,
                1,
                "command.not_read_only",
                "dev command checks must set read_only: true",
            )],
            0,
        );
    }
    if check.command.is_empty() {
        return result_from_diags(
            check,
            vec![diag(
                &ctx.root,
                check,
                MANIFEST_NAME,
                1,
                1,
                "command.empty",
                "command check requires command: [...]",
            )],
            0,
        );
    }
    let mut child = match Command::new(&check.command[0])
        .args(&check.command[1..])
        .current_dir(&ctx.root)
        .env("CARGOLESS", "1")
        .env("CARGOLESS_CHECK_ID", &check.id)
        .env("CARGOLESS_PROFILE", &ctx.profile_name)
        .env("CARGOLESS_WORKTREE", &ctx.root)
        .env(
            "CARGOLESS_CHANGED_FILES",
            ctx.changed_files
                .as_ref()
                .map(|files| files.join("\n"))
                .unwrap_or_default(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return result_from_diags(
                check,
                vec![diag(
                    &ctx.root,
                    check,
                    MANIFEST_NAME,
                    1,
                    1,
                    "command.spawn",
                    &format!("could not spawn command: {e}"),
                )],
                0,
            );
        }
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_thread = thread::spawn(move || read_pipe(&mut stdout));
    let err_thread = thread::spawn(move || read_pipe(&mut stderr));
    let deadline = Instant::now() + Duration::from_millis(check.timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("command timed out".to_string());
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(e) => break Err(format!("could not wait for command: {e}")),
        }
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    let combined = format!("{stdout}\n{stderr}");
    let mut command_diagnostics = parse_command_diagnostics(&ctx.root, check, &combined);
    match status {
        Ok(s) if s.success() => result_from_diags(check, command_diagnostics, 0),
        Ok(s) => {
            if !command_diagnostics
                .iter()
                .any(|d| d.severity == Severity::Error)
            {
                let tail = tail_lines(&combined, 12);
                command_diagnostics.push(diag(
                    &ctx.root,
                    check,
                    MANIFEST_NAME,
                    1,
                    1,
                    "command.failed",
                    &format!("command exited with {s}; output:\n{tail}"),
                ));
            }
            result_from_diags(check, command_diagnostics, 0)
        }
        Err(message) => result_from_diags(
            check,
            vec![diag(
                &ctx.root,
                check,
                MANIFEST_NAME,
                1,
                1,
                "command.timeout",
                &message,
            )],
            0,
        ),
    }
}

fn parse_command_diagnostics(root: &Path, check: &CheckConfig, text: &str) -> Vec<Diagnostic> {
    text.lines()
        .filter_map(|line| parse_command_diagnostic_line(root, check, line))
        .collect()
}

fn parse_command_diagnostic_line(
    root: &Path,
    check: &CheckConfig,
    line: &str,
) -> Option<Diagnostic> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let schema = value
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !schema.is_empty() && schema != "cargoless.check-diagnostic/v1" {
        return None;
    }
    if let Some(id) = value.get("check").and_then(serde_json::Value::as_str) {
        if id != check.id {
            return None;
        }
    }
    let message = value.get("message")?.as_str()?.to_string();
    let message = match value.get("suggestion").and_then(serde_json::Value::as_str) {
        Some(suggestion) if !suggestion.is_empty() => format!("{message}\nhelp: {suggestion}"),
        _ => message,
    };
    let file_path = value
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        })
        .unwrap_or_else(|| root.join(MANIFEST_NAME));
    Some(Diagnostic {
        file_path,
        line: value
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1) as u32,
        col: value
            .get("column")
            .or_else(|| value.get("col"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1) as u32,
        severity: severity_from_str(
            value
                .get("severity")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("error"),
        ),
        code: value
            .get("code")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        message,
        source: value
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(format!("cargoless-check:{}", check.id))),
    })
}

fn severity_from_str(value: &str) -> Severity {
    match value {
        "warning" => Severity::Warning,
        "info" | "information" => Severity::Info,
        "hint" => Severity::Hint,
        _ => Severity::Error,
    }
}

fn read_pipe(pipe: &mut Option<impl Read>) -> String {
    let mut out = String::new();
    if let Some(pipe) = pipe {
        let _ = pipe.read_to_string(&mut out);
    }
    out
}

fn apply_data_rules(
    diagnostics: &mut Vec<Diagnostic>,
    root: &Path,
    check: &CheckConfig,
    rel: &str,
    yaml: &YamlNode,
    json: Option<&serde_json::Value>,
) {
    for rule in &check.rules {
        if let Some(path) = &rule.require_path {
            let exists = json
                .map(|v| json_value_at_path(v, path).is_some())
                .unwrap_or_else(|| yaml.value_at_path(path).is_some());
            if !exists {
                diagnostics.push(diag(root, check, rel, 1, 1, &rule.code, &rule.message));
            }
        }
        if let Some(path) = &rule.forbid_path {
            let exists = json
                .map(|v| json_value_at_path(v, path).is_some())
                .unwrap_or_else(|| yaml.value_at_path(path).is_some());
            if exists {
                diagnostics.push(diag(root, check, rel, 1, 1, &rule.code, &rule.message));
            }
        }
        if let Some(path) = &rule.equals_path {
            let got = json
                .and_then(|v| json_value_at_path(v, path).map(json_scalar_string))
                .or_else(|| yaml.value_at_path(path).map(YamlNode::scalar_string));
            if let Some(expect) = &rule.equals {
                if got.as_deref() != Some(expect.as_str()) {
                    diagnostics.push(diag(root, check, rel, 1, 1, &rule.code, &rule.message));
                }
            }
            if !rule.one_of.is_empty()
                && got
                    .as_ref()
                    .is_none_or(|v| !rule.one_of.iter().any(|allowed| allowed == v))
            {
                diagnostics.push(diag(root, check, rel, 1, 1, &rule.code, &rule.message));
            }
        }
    }
}

fn pattern_find(rule: &PatternRule, text: &str) -> Option<usize> {
    if let Some(lit) = &rule.literal {
        return text.find(lit);
    }
    if let Some(word) = &rule.word {
        return find_word(text, word);
    }
    if let Some(regex) = &rule.regex {
        return simple_regex_find(text, regex);
    }
    None
}

fn find_word(text: &str, word: &str) -> Option<usize> {
    let mut start = 0usize;
    while let Some(pos) = text[start..].find(word) {
        let abs = start + pos;
        let before = text[..abs].chars().next_back();
        let after = text[abs + word.len()..].chars().next();
        if before.is_none_or(|c| !is_word_char(c)) && after.is_none_or(|c| !is_word_char(c)) {
            return Some(abs);
        }
        start = abs + word.len();
    }
    None
}

fn simple_regex_find(text: &str, regex: &str) -> Option<usize> {
    let trimmed = regex.trim();
    if let Some(inner) = trimmed
        .strip_prefix(r"\b(")
        .and_then(|s| s.strip_suffix(r")\b"))
    {
        return inner.split('|').filter_map(|w| find_word(text, w)).min();
    }
    if let Some(inner) = trimmed
        .strip_prefix(r"\b")
        .and_then(|s| s.strip_suffix(r"\b"))
    {
        return find_word(text, inner);
    }
    trimmed
        .split('|')
        .filter(|s| !s.is_empty())
        .filter_map(|s| text.find(&unescape_regex_literal(s)))
        .min()
}

fn unescape_regex_literal(s: &str) -> String {
    s.replace(r"\.", ".")
        .replace(r"\-", "-")
        .replace(r"\_", "_")
        .replace(r"\/", "/")
        .replace('\\', "")
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn line_col(text: &str, offset: usize) -> (u32, u32) {
    let mut line = 1u32;
    let mut col = 1u32;
    for (idx, ch) in text.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn input_patterns(check: &CheckConfig) -> Vec<String> {
    if !check.inputs.is_empty() {
        return check.inputs.clone();
    }
    if !check.triggers.is_empty() {
        return check.triggers.clone();
    }
    match check.kind {
        CheckKind::MirrorDrift => {
            let mut out = Vec::new();
            if let Some(root) = &check.source_root {
                out.push(format!("{}/**", root.trim_end_matches('/')));
            }
            for mirror in &check.mirrors {
                out.push(format!("{}/**", mirror.root.trim_end_matches('/')));
            }
            out
        }
        CheckKind::FileExists => check.paths.clone(),
        _ => Vec::new(),
    }
}

fn cache_dir(root: &Path) -> PathBuf {
    root.join(".cargoless")
        .join("tree.cache")
        .join("project-checks")
}

fn cache_key(ctx: &RunContext, check: &CheckConfig) -> String {
    let mut preimage = String::new();
    preimage.push_str(ENGINE_VERSION);
    preimage.push('\n');
    preimage.push_str(&ctx.manifest_hash);
    preimage.push('\n');
    preimage.push_str(&ctx.profile_name);
    preimage.push('\n');
    preimage.push_str(&check.id);
    preimage.push('\n');
    preimage.push_str(&sha256_hex(format!("{check:?}").as_bytes()));
    preimage.push('\n');
    for (path, hash) in input_fingerprints(ctx, check) {
        preimage.push_str(&path);
        preimage.push('\0');
        preimage.push_str(&hash);
        preimage.push('\n');
    }
    sha256_hex(preimage.as_bytes())
}

fn input_fingerprints(ctx: &RunContext, check: &CheckConfig) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for file in ctx.snapshot.matching(&input_patterns(check)) {
        if seen.insert(file.rel.clone()) {
            out.push((file.rel, file_hash(&file.abs)));
        }
    }
    for path in &check.paths {
        if seen.insert(path.clone()) {
            let abs = ctx.root.join(path);
            let hash = if abs.exists() {
                file_hash(&abs)
            } else {
                "ABSENT".to_string()
            };
            out.push((path.clone(), hash));
        }
    }
    out.sort();
    out
}

fn file_hash(path: &Path) -> String {
    fs::read(path)
        .map(|b| sha256_hex(&b))
        .unwrap_or_else(|_| "UNREADABLE".to_string())
}

fn cache_get(ctx: &RunContext, check: &CheckConfig) -> Option<ProjectCheckResult> {
    if check.cache == "none" {
        return None;
    }
    let path = cache_dir(&ctx.root).join(cache_key(ctx, check));
    let text = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tree = match value.get("tree")?.as_str()? {
        "green" => TreeState::Green,
        "red" => TreeState::Red,
        _ => return None,
    };
    let diagnostics: Vec<Diagnostic> = value
        .get("diagnostics")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| diagnostic_from_json(v, &ctx.root))
                .collect()
        })
        .unwrap_or_default();
    if has_timeout_diagnostic(&diagnostics) {
        return None;
    }
    Some(ProjectCheckResult {
        id: check.id.clone(),
        title: check.title.clone(),
        required: check.required,
        tree,
        diagnostics,
        duration_ms: 0,
        cache_hit: true,
    })
}

fn cache_put(ctx: &RunContext, check: &CheckConfig, result: &ProjectCheckResult) {
    if check.cache == "none" {
        return;
    }
    if has_timeout_diagnostic(&result.diagnostics) {
        return;
    }
    let dir = cache_dir(&ctx.root);
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(cache_key(ctx, check));
    let tmp = path.with_extension("tmp");
    let diagnostics: Vec<serde_json::Value> = result
        .diagnostics
        .iter()
        .map(|d| {
            serde_json::json!({
                "file": d.file_path.strip_prefix(&ctx.root).unwrap_or(&d.file_path).to_string_lossy(),
                "line": d.line,
                "col": d.col,
                "severity": d.severity.as_str(),
                "code": d.code,
                "message": d.message,
                "source": d.source,
            })
        })
        .collect();
    let body = serde_json::json!({
        "tree": if result.tree == TreeState::Green { "green" } else { "red" },
        "diagnostics": diagnostics,
    })
    .to_string();
    if fs::write(&tmp, body).is_ok() {
        let _ = fs::rename(tmp, path);
    }
}

fn has_timeout_diagnostic(diagnostics: &[Diagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|d| d.code.as_deref() == Some(TIMEOUT_DIAGNOSTIC_CODE))
}

fn diagnostic_from_json(v: &serde_json::Value, root: &Path) -> Option<Diagnostic> {
    Some(Diagnostic {
        file_path: root.join(v.get("file")?.as_str()?),
        line: v
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1) as u32,
        col: v
            .get("col")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1) as u32,
        severity: match v
            .get("severity")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("error")
        {
            "warning" => Severity::Warning,
            "info" => Severity::Info,
            "hint" => Severity::Hint,
            _ => Severity::Error,
        },
        code: v
            .get("code")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        message: v
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        source: v
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

fn parse_manifest(path: PathBuf, text: &str) -> Result<ProjectChecksManifest, ManifestError> {
    let root = parse_yaml_value(text).map_err(|e| ManifestError {
        path: path.clone(),
        line: e.line,
        message: e.message,
    })?;
    let map = root.as_map().ok_or_else(|| ManifestError {
        path: path.clone(),
        line: 1,
        message: "manifest root must be a map".to_string(),
    })?;
    for key in map.keys() {
        if !matches!(key.as_str(), "version" | "profiles" | "checks") {
            return Err(ManifestError {
                path: path.clone(),
                line: 1,
                message: format!("unknown top-level key `{key}`"),
            });
        }
    }
    let version = map
        .get("version")
        .and_then(YamlNode::as_i64)
        .ok_or_else(|| ManifestError {
            path: path.clone(),
            line: 1,
            message: "version: 1 is required".to_string(),
        })?;
    if version != 1 {
        return Err(ManifestError {
            path,
            line: 1,
            message: format!("unsupported project check manifest version {version}"),
        });
    }
    let profiles = parse_profiles(map.get("profiles")).map_err(|e| ManifestError {
        path: path.clone(),
        line: e.line,
        message: e.message,
    })?;
    let checks = parse_checks(map.get("checks")).map_err(|e| ManifestError {
        path: path.clone(),
        line: e.line,
        message: e.message,
    })?;
    let mut ids = BTreeSet::new();
    for check in &checks {
        if !ids.insert(check.id.clone()) {
            return Err(ManifestError {
                path: path.clone(),
                line: 1,
                message: format!("duplicate check id `{}`", check.id),
            });
        }
    }
    Ok(ProjectChecksManifest {
        profiles,
        checks,
        manifest_hash: sha256_hex(text.as_bytes()),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParseError {
    line: usize,
    message: String,
}

fn parse_profiles(node: Option<&YamlNode>) -> Result<BTreeMap<String, ProfileConfig>, ParseError> {
    let mut out = BTreeMap::new();
    let Some(node) = node else {
        out.insert(
            "dev".to_string(),
            ProfileConfig {
                include: Vec::new(),
                timeout_ms: 12_000,
                max_parallel: 8,
                on_timeout: "red".to_string(),
            },
        );
        return Ok(out);
    };
    for (name, profile) in node.expect_map("profiles")? {
        let map = profile.expect_map(&format!("profiles.{name}"))?;
        reject_unknown(
            map,
            &["include", "timeout_ms", "max_parallel", "on_timeout"],
            profile.line(),
        )?;
        out.insert(
            name.clone(),
            ProfileConfig {
                include: get_string_list(map, "include")?.unwrap_or_default(),
                timeout_ms: get_u64(map, "timeout_ms")?.unwrap_or(12_000),
                max_parallel: get_u64(map, "max_parallel")?.unwrap_or(8) as usize,
                on_timeout: get_string(map, "on_timeout")?.unwrap_or_else(|| "red".to_string()),
            },
        );
    }
    Ok(out)
}

fn parse_checks(node: Option<&YamlNode>) -> Result<Vec<CheckConfig>, ParseError> {
    let node = node.ok_or(ParseError {
        line: 1,
        message: "checks: list is required".to_string(),
    })?;
    let mut out = Vec::new();
    for item in node.expect_list("checks")? {
        let map = item.expect_map("checks[]")?;
        reject_unknown(
            map,
            &[
                "id",
                "title",
                "tier",
                "required",
                "kind",
                "triggers",
                "inputs",
                "timeout_ms",
                "cache",
                "source_root",
                "mirrors",
                "patterns",
                "rules",
                "paths",
                "command",
                "read_only",
            ],
            item.line(),
        )?;
        let id = required_string(map, "id", item.line())?;
        let kind_text = required_string(map, "kind", item.line())?;
        let kind = CheckKind::parse(&kind_text).ok_or(ParseError {
            line: item.line(),
            message: format!("unknown check kind `{kind_text}`"),
        })?;
        out.push(CheckConfig {
            title: get_string(map, "title")?.unwrap_or_else(|| id.clone()),
            tier: get_string(map, "tier")?.unwrap_or_else(|| "dev".to_string()),
            required: get_bool(map, "required")?.unwrap_or(true),
            triggers: get_string_list(map, "triggers")?.unwrap_or_default(),
            inputs: get_string_list(map, "inputs")?.unwrap_or_default(),
            timeout_ms: get_u64(map, "timeout_ms")?.unwrap_or(3_000),
            cache: get_string(map, "cache")?.unwrap_or_else(|| "inputs".to_string()),
            source_root: get_string(map, "source_root")?,
            mirrors: parse_mirrors(map.get("mirrors"))?,
            patterns: parse_patterns(map.get("patterns"))?,
            rules: parse_rules(map.get("rules"))?,
            paths: get_string_list(map, "paths")?.unwrap_or_default(),
            command: get_string_list(map, "command")?.unwrap_or_default(),
            read_only: get_bool(map, "read_only")?.unwrap_or(false),
            id,
            kind,
        });
    }
    Ok(out)
}

fn parse_mirrors(node: Option<&YamlNode>) -> Result<Vec<MirrorConfig>, ParseError> {
    let Some(node) = node else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in node.expect_list("mirrors")? {
        let map = item.expect_map("mirrors[]")?;
        reject_unknown(map, &["root", "include", "exclude"], item.line())?;
        out.push(MirrorConfig {
            root: required_string(map, "root", item.line())?,
            include: get_string_list(map, "include")?.unwrap_or_default(),
            exclude: get_string_list(map, "exclude")?.unwrap_or_default(),
        });
    }
    Ok(out)
}

fn parse_patterns(node: Option<&YamlNode>) -> Result<Vec<PatternRule>, ParseError> {
    let Some(node) = node else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in node.expect_list("patterns")? {
        let map = item.expect_map("patterns[]")?;
        reject_unknown(
            map,
            &["code", "message", "literal", "word", "regex"],
            item.line(),
        )?;
        out.push(PatternRule {
            code: get_string(map, "code")?.unwrap_or_else(|| "pattern".to_string()),
            message: get_string(map, "message")?
                .unwrap_or_else(|| "pattern rule matched".to_string()),
            literal: get_string(map, "literal")?,
            word: get_string(map, "word")?,
            regex: get_string(map, "regex")?,
        });
    }
    Ok(out)
}

fn parse_rules(node: Option<&YamlNode>) -> Result<Vec<DataRule>, ParseError> {
    let Some(node) = node else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in node.expect_list("rules")? {
        let map = item.expect_map("rules[]")?;
        reject_unknown(
            map,
            &[
                "code",
                "message",
                "require_path",
                "forbid_path",
                "equals_path",
                "equals",
                "one_of",
            ],
            item.line(),
        )?;
        out.push(DataRule {
            code: get_string(map, "code")?.unwrap_or_else(|| "data_rule".to_string()),
            message: get_string(map, "message")?.unwrap_or_else(|| "data rule failed".to_string()),
            require_path: get_string(map, "require_path")?,
            forbid_path: get_string(map, "forbid_path")?,
            equals_path: get_string(map, "equals_path")?,
            equals: get_string(map, "equals")?,
            one_of: get_string_list(map, "one_of")?.unwrap_or_default(),
        });
    }
    Ok(out)
}

fn reject_unknown(
    map: &BTreeMap<String, YamlNode>,
    allowed: &[&str],
    line: usize,
) -> Result<(), ParseError> {
    for key in map.keys() {
        if !allowed.iter().any(|a| a == key) {
            return Err(ParseError {
                line,
                message: format!("unknown key `{key}`"),
            });
        }
    }
    Ok(())
}

fn required_string(
    map: &BTreeMap<String, YamlNode>,
    key: &str,
    line: usize,
) -> Result<String, ParseError> {
    get_string(map, key)?.ok_or(ParseError {
        line,
        message: format!("required key `{key}` is missing"),
    })
}

fn get_string(map: &BTreeMap<String, YamlNode>, key: &str) -> Result<Option<String>, ParseError> {
    map.get(key).map(YamlNode::expect_string).transpose()
}

fn get_bool(map: &BTreeMap<String, YamlNode>, key: &str) -> Result<Option<bool>, ParseError> {
    map.get(key).map(YamlNode::expect_bool).transpose()
}

fn get_u64(map: &BTreeMap<String, YamlNode>, key: &str) -> Result<Option<u64>, ParseError> {
    map.get(key)
        .map(|v| v.expect_int().map(|i| i.max(0) as u64))
        .transpose()
}

fn get_string_list(
    map: &BTreeMap<String, YamlNode>,
    key: &str,
) -> Result<Option<Vec<String>>, ParseError> {
    map.get(key).map(YamlNode::expect_string_list).transpose()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum YamlNode {
    Map(BTreeMap<String, YamlNode>, usize),
    List(Vec<YamlNode>, usize),
    String(String, usize),
    Bool(bool, usize),
    Int(i64, usize),
    Null(usize),
}

impl YamlNode {
    fn line(&self) -> usize {
        match self {
            Self::Map(_, l)
            | Self::List(_, l)
            | Self::String(_, l)
            | Self::Bool(_, l)
            | Self::Int(_, l)
            | Self::Null(l) => *l,
        }
    }

    fn as_map(&self) -> Option<&BTreeMap<String, YamlNode>> {
        match self {
            Self::Map(v, _) => Some(v),
            _ => None,
        }
    }

    fn expect_map(&self, name: &str) -> Result<&BTreeMap<String, YamlNode>, ParseError> {
        self.as_map().ok_or(ParseError {
            line: self.line(),
            message: format!("{name} must be a map"),
        })
    }

    fn expect_list(&self, name: &str) -> Result<&[YamlNode], ParseError> {
        match self {
            Self::List(v, _) => Ok(v),
            _ => Err(ParseError {
                line: self.line(),
                message: format!("{name} must be a list"),
            }),
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(v, _) => Some(*v),
            _ => None,
        }
    }

    fn expect_string(&self) -> Result<String, ParseError> {
        match self {
            Self::String(v, _) => Ok(v.clone()),
            Self::Int(v, _) => Ok(v.to_string()),
            Self::Bool(v, _) => Ok(v.to_string()),
            _ => Err(ParseError {
                line: self.line(),
                message: "expected string scalar".to_string(),
            }),
        }
    }

    fn expect_bool(&self) -> Result<bool, ParseError> {
        match self {
            Self::Bool(v, _) => Ok(*v),
            _ => Err(ParseError {
                line: self.line(),
                message: "expected boolean scalar".to_string(),
            }),
        }
    }

    fn expect_int(&self) -> Result<i64, ParseError> {
        match self {
            Self::Int(v, _) => Ok(*v),
            _ => Err(ParseError {
                line: self.line(),
                message: "expected integer scalar".to_string(),
            }),
        }
    }

    fn expect_string_list(&self) -> Result<Vec<String>, ParseError> {
        match self {
            Self::List(items, _) => items.iter().map(YamlNode::expect_string).collect(),
            Self::String(v, _) => Ok(vec![v.clone()]),
            _ => Err(ParseError {
                line: self.line(),
                message: "expected string list".to_string(),
            }),
        }
    }

    fn value_at_path(&self, path: &str) -> Option<&YamlNode> {
        let mut cur = self;
        for part in path.trim_start_matches("$.").split('.') {
            if part.is_empty() {
                continue;
            }
            let map = cur.as_map()?;
            cur = map.get(part)?;
        }
        Some(cur)
    }

    fn scalar_string(&self) -> String {
        match self {
            Self::String(v, _) => v.clone(),
            Self::Bool(v, _) => v.to_string(),
            Self::Int(v, _) => v.to_string(),
            Self::Null(_) => "null".to_string(),
            Self::Map(_, _) => "<map>".to_string(),
            Self::List(_, _) => "<list>".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct YamlLine {
    indent: usize,
    text: String,
    line_no: usize,
}

fn parse_yaml_value(text: &str) -> Result<YamlNode, ParseError> {
    let mut lines = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        if raw.contains('\t') {
            return Err(ParseError {
                line: idx + 1,
                message: "tabs are not supported in project-check YAML".to_string(),
            });
        }
        let no_comment = strip_yaml_comment(raw);
        if no_comment.trim().is_empty() {
            continue;
        }
        let indent = no_comment.chars().take_while(|c| *c == ' ').count();
        if indent % 2 != 0 {
            return Err(ParseError {
                line: idx + 1,
                message: "indentation must use multiples of two spaces".to_string(),
            });
        }
        lines.push(YamlLine {
            indent,
            text: no_comment.trim().to_string(),
            line_no: idx + 1,
        });
    }
    if lines.is_empty() {
        return Ok(YamlNode::Null(1));
    }
    let (node, idx) = parse_block(&lines, 0, lines[0].indent)?;
    if idx != lines.len() {
        return Err(ParseError {
            line: lines[idx].line_no,
            message: "unexpected extra YAML content".to_string(),
        });
    }
    Ok(node)
}

fn parse_block(
    lines: &[YamlLine],
    idx: usize,
    indent: usize,
) -> Result<(YamlNode, usize), ParseError> {
    if idx >= lines.len() {
        return Ok((YamlNode::Null(1), idx));
    }
    if lines[idx].indent < indent {
        return Ok((YamlNode::Null(lines[idx].line_no), idx));
    }
    if lines[idx].indent > indent {
        return Err(ParseError {
            line: lines[idx].line_no,
            message: "unexpected indentation".to_string(),
        });
    }
    if lines[idx].text.starts_with("- ") || lines[idx].text == "-" {
        parse_list(lines, idx, indent)
    } else {
        parse_map(lines, idx, indent)
    }
}

fn parse_map(
    lines: &[YamlLine],
    mut idx: usize,
    indent: usize,
) -> Result<(YamlNode, usize), ParseError> {
    let line_no = lines[idx].line_no;
    let mut map = BTreeMap::new();
    while idx < lines.len() {
        let line = &lines[idx];
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            return Err(ParseError {
                line: line.line_no,
                message: "unexpected indentation in map".to_string(),
            });
        }
        if line.text.starts_with("- ") || line.text == "-" {
            break;
        }
        let (key, val) = split_key_value(&line.text).ok_or(ParseError {
            line: line.line_no,
            message: "expected `key: value`".to_string(),
        })?;
        idx += 1;
        let node = if val.is_empty() {
            if idx < lines.len() && lines[idx].indent > indent {
                let (child, next) = parse_block(lines, idx, indent + 2)?;
                idx = next;
                child
            } else {
                YamlNode::Null(line.line_no)
            }
        } else {
            parse_scalar(val, line.line_no)?
        };
        map.insert(key.to_string(), node);
    }
    Ok((YamlNode::Map(map, line_no), idx))
}

fn parse_list(
    lines: &[YamlLine],
    mut idx: usize,
    indent: usize,
) -> Result<(YamlNode, usize), ParseError> {
    let line_no = lines[idx].line_no;
    let mut out = Vec::new();
    while idx < lines.len() {
        let line = &lines[idx];
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            return Err(ParseError {
                line: line.line_no,
                message: "unexpected indentation in list".to_string(),
            });
        }
        if !(line.text.starts_with("- ") || line.text == "-") {
            break;
        }
        let rest = line.text.strip_prefix("-").unwrap().trim();
        idx += 1;
        if rest.is_empty() {
            let (child, next) = parse_block(lines, idx, indent + 2)?;
            idx = next;
            out.push(child);
        } else if let Some((key, val)) = split_key_value(rest) {
            let mut map = BTreeMap::new();
            let first = if val.is_empty() {
                if idx < lines.len() && lines[idx].indent > indent {
                    let (child, next) = parse_block(lines, idx, indent + 2)?;
                    idx = next;
                    child
                } else {
                    YamlNode::Null(line.line_no)
                }
            } else {
                parse_scalar(val, line.line_no)?
            };
            map.insert(key.to_string(), first);
            while idx < lines.len() && lines[idx].indent == indent + 2 {
                if lines[idx].text.starts_with("- ") || lines[idx].text == "-" {
                    break;
                }
                let (k, v) = split_key_value(&lines[idx].text).ok_or(ParseError {
                    line: lines[idx].line_no,
                    message: "expected map entry".to_string(),
                })?;
                let item_line = lines[idx].line_no;
                idx += 1;
                let node = if v.is_empty() {
                    if idx < lines.len() && lines[idx].indent > indent + 2 {
                        let (child, next) = parse_block(lines, idx, indent + 4)?;
                        idx = next;
                        child
                    } else {
                        YamlNode::Null(item_line)
                    }
                } else {
                    parse_scalar(v, item_line)?
                };
                map.insert(k.to_string(), node);
            }
            out.push(YamlNode::Map(map, line.line_no));
        } else {
            out.push(parse_scalar(rest, line.line_no)?);
        }
    }
    Ok((YamlNode::List(out, line_no), idx))
}

fn split_key_value(s: &str) -> Option<(&str, &str)> {
    let mut in_single = false;
    let mut in_double = false;
    for (idx, ch) in s.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ':' if !in_single && !in_double => {
                let key = s[..idx].trim();
                let value = s[idx + 1..].trim();
                if key.is_empty() {
                    return None;
                }
                return Some((key, value));
            }
            _ => {}
        }
    }
    None
}

fn parse_scalar(s: &str, line: usize) -> Result<YamlNode, ParseError> {
    if s == "true" {
        return Ok(YamlNode::Bool(true, line));
    }
    if s == "false" {
        return Ok(YamlNode::Bool(false, line));
    }
    if s == "null" || s == "~" {
        return Ok(YamlNode::Null(line));
    }
    if s.starts_with('|') || s.starts_with('>') || s.starts_with('&') || s.starts_with('*') {
        return Err(ParseError {
            line,
            message: "YAML anchors, aliases, and multiline scalars are not supported".to_string(),
        });
    }
    if let Some(inner) = s.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        let mut items = Vec::new();
        if !inner.trim().is_empty() {
            for item in split_inline_list(inner) {
                items.push(parse_scalar(item.trim(), line)?);
            }
        }
        return Ok(YamlNode::List(items, line));
    }
    if let Ok(v) = s.parse::<i64>() {
        return Ok(YamlNode::Int(v, line));
    }
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        return Ok(YamlNode::String(unquote_yaml(s), line));
    }
    Ok(YamlNode::String(s.to_string(), line))
}

fn strip_yaml_comment(s: &str) -> String {
    let mut in_single = false;
    let mut in_double = false;
    let mut out = String::new();
    for ch in s.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                out.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                out.push(ch);
            }
            '#' if !in_single && !in_double => break,
            _ => out.push(ch),
        }
    }
    out
}

fn split_inline_list(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    for (idx, ch) in s.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ',' if !in_single && !in_double => {
                out.push(&s[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn unquote_yaml(s: &str) -> String {
    let inner = &s[1..s.len() - 1];
    if s.starts_with('"') {
        inner
            .replace(r#"\""#, r#"""#)
            .replace(r"\n", "\n")
            .replace(r"\t", "\t")
            .replace(r"\\", "\\")
    } else {
        inner.replace("''", "'")
    }
}

fn json_value_at_path<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let mut cur = value;
    for part in path.trim_start_matches("$.").split('.') {
        if part.is_empty() {
            continue;
        }
        cur = cur.get(part)?;
    }
    Some(cur)
}

fn json_scalar_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(v) => v.clone(),
        serde_json::Value::Bool(v) => v.to_string(),
        serde_json::Value::Number(v) => v.to_string(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Array(_) => "<list>".to_string(),
        serde_json::Value::Object(_) => "<map>".to_string(),
    }
}

fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn join_rel(a: &str, b: &str) -> String {
    let a = a.trim_end_matches('/');
    let b = b.trim_start_matches('/');
    if a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else {
        format!("{a}/{b}")
    }
}

fn ignored_rel(rel: &str) -> bool {
    let rel = rel.trim_matches('/');
    if rel == ".claude/worktrees" || rel.starts_with(".claude/worktrees/") {
        return true;
    }
    rel.split('/').any(|part| {
        matches!(
            part,
            "target"
                | ".git"
                | "dist"
                | ".cargoless"
                | "node_modules"
                | ".direnv"
                | ".venv"
                | "venv"
                | "__pycache__"
                | ".pytest_cache"
        )
    })
}

fn glob_match_path(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_matches('/');
    let path = path.trim_matches('/');
    let p: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let t: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    glob_segments(&p, &t)
}

fn glob_segments(pattern: &[&str], text: &[&str]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    if pattern[0] == "**" {
        return glob_segments(&pattern[1..], text)
            || (!text.is_empty() && glob_segments(pattern, &text[1..]));
    }
    !text.is_empty()
        && segment_match(pattern[0], text[0])
        && glob_segments(&pattern[1..], &text[1..])
}

fn segment_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.split_once('*') {
        None => pattern == text,
        Some((pre, post)) => {
            text.starts_with(pre)
                && (post.is_empty()
                    || text[pre.len()..].find(post).is_some_and(|idx| {
                        let rest = &text[pre.len() + idx + post.len()..];
                        !post.contains('*') || segment_match(&format!("*{rest}"), rest)
                    })
                    || text.ends_with(post))
        }
    }
}

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "cargoless-project-checks-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn snapshot_ignores_local_execution_state() {
        assert!(ignored_rel(".claude/worktrees/agent-a/src/lib.rs"));
        assert!(ignored_rel("portal/target/debug/build.log"));
        assert!(ignored_rel("node_modules/package/index.js"));
        assert!(ignored_rel(".cargoless/tree.cache/project-checks/key"));
        assert!(!ignored_rel(".claude/CLAUDE.md"));
        assert!(!ignored_rel("chemistry/checks/inventory.yaml"));
    }

    #[test]
    fn yaml_subset_parses_manifest_shape() {
        let text = r#"
version: 1
profiles:
  dev:
    include: ["portal"]
    timeout_ms: 1000
    max_parallel: 2
checks:
  - id: portal
    title: Portal guard
    tier: dev
    required: true
    kind: forbidden_patterns
    inputs:
      - portal/**/*.rs
    patterns:
      - code: portal.bad
        word: auth-policy
        message: Do not hardcode element names.
"#;
        let manifest = parse_manifest(PathBuf::from(MANIFEST_NAME), text).unwrap();
        assert_eq!(manifest.checks.len(), 1);
        assert_eq!(manifest.profiles["dev"].include, vec!["portal"]);
    }

    #[test]
    fn forbidden_pattern_blocks_dev_profile() {
        let root = scratch("forbidden");
        fs::create_dir_all(root.join("portal/src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        fs::write(
            root.join("portal/src/lib.rs"),
            "const X: &str = \"auth-policy\";",
        )
        .unwrap();
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: portal-agnostic
    kind: forbidden_patterns
    inputs: ["portal/**/*.rs"]
    patterns:
      - code: portal.element_specific
        word: auth-policy
        message: Portal must stay element agnostic.
"#,
        )
        .unwrap();
        let report = run_profile(&root, "dev", None).unwrap();
        assert_eq!(report.tree, TreeState::Red);
        assert_eq!(
            report.diagnostics[0].code.as_deref(),
            Some("portal.element_specific")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn yaml_required_path_blocks_when_missing() {
        let root = scratch("yaml");
        fs::create_dir_all(root.join("chemistry/elements/foo")).unwrap();
        fs::write(
            root.join("chemistry/elements/foo/definition.yaml"),
            "name: foo\n",
        )
        .unwrap();
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: yaml-contract
    kind: yaml_rules
    inputs: ["chemistry/**/*.yaml"]
    rules:
      - code: yaml.meta_intention
        require_path: $.meta.intention
        message: YAML definitions must declare meta.intention.
"#,
        )
        .unwrap();
        let report = run_profile(&root, "dev", None).unwrap();
        assert_eq!(report.tree, TreeState::Red);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cache_hit_reuses_result() {
        let root = scratch("cache");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "ok").unwrap();
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: required
    kind: required_patterns
    inputs: ["src/*.rs"]
    patterns:
      - code: required.ok
        literal: ok
        message: missing ok
"#,
        )
        .unwrap();
        let first = run_profile(&root, "dev", None).unwrap();
        assert!(!first.results[0].cache_hit);
        let second = run_profile(&root, "dev", None).unwrap();
        assert!(second.results[0].cache_hit);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn timeout_results_are_not_cached() {
        let root = scratch("timeout-cache");
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: slow
    kind: command
    read_only: true
    command: ["bash", "-lc", "printf x >> counter; sleep 0.05"]
    timeout_ms: 1
    cache: inputs
"#,
        )
        .unwrap();
        let first = run_profile(&root, "dev", None).unwrap();
        assert_eq!(first.tree, TreeState::Red);
        assert!(!first.results[0].cache_hit);
        let second = run_profile(&root, "dev", None).unwrap();
        assert_eq!(second.tree, TreeState::Red);
        assert!(!second.results[0].cache_hit);
        assert_eq!(fs::read_to_string(root.join("counter")).unwrap(), "xx");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn changed_files_select_only_matching_checks() {
        let root = scratch("changed-select");
        fs::create_dir_all(root.join("portal/src")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("portal/src/lib.rs"), "ok").unwrap();
        fs::write(root.join("docs/readme.md"), "ok").unwrap();
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: portal
    kind: required_patterns
    inputs: ["portal/**/*.rs"]
    patterns:
      - code: portal.ok
        literal: ok
        message: missing ok
  - id: docs
    kind: required_patterns
    inputs: ["docs/**/*.md"]
    patterns:
      - code: docs.ok
        literal: ok
        message: missing ok
"#,
        )
        .unwrap();
        let changed = vec!["docs/readme.md".to_string()];
        let report = run_profile_with_changes(&root, "dev", None, Some(&changed)).unwrap();
        assert_eq!(
            report.results.iter().map(|r| &r.id).collect::<Vec<_>>(),
            vec!["docs"]
        );
        assert_eq!(
            report.skipped.iter().map(|s| &s.id).collect::<Vec<_>>(),
            vec!["portal"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn triggers_override_inputs_for_changed_file_selection() {
        let root = scratch("changed-triggers");
        fs::create_dir_all(root.join("portal/src")).unwrap();
        fs::create_dir_all(root.join("chemistry")).unwrap();
        fs::write(root.join("portal/src/lib.rs"), "ok").unwrap();
        fs::write(root.join("chemistry/spec.yaml"), "name: spec").unwrap();
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: generated
    kind: required_patterns
    triggers: ["chemistry/**/*.yaml"]
    inputs: ["portal/**/*.rs"]
    patterns:
      - code: generated.ok
        literal: ok
        message: missing generated output
"#,
        )
        .unwrap();
        let portal_change = vec!["portal/src/lib.rs".to_string()];
        let skipped = run_profile_with_changes(&root, "dev", None, Some(&portal_change)).unwrap();
        assert!(skipped.results.is_empty());
        assert_eq!(skipped.skipped[0].id, "generated");

        let chemistry_change = vec!["chemistry/spec.yaml".to_string()];
        let ran = run_profile_with_changes(&root, "dev", None, Some(&chemistry_change)).unwrap();
        assert_eq!(ran.results[0].id, "generated");
        assert!(ran.skipped.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn manifest_change_or_only_id_forces_selected_checks() {
        let root = scratch("changed-forced");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "ok").unwrap();
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: required
    kind: required_patterns
    inputs: ["src/*.rs"]
    patterns:
      - code: required.ok
        literal: ok
        message: missing ok
"#,
        )
        .unwrap();
        let manifest_change = vec![MANIFEST_NAME.to_string()];
        let all = run_profile_with_changes(&root, "dev", None, Some(&manifest_change)).unwrap();
        assert_eq!(all.results[0].id, "required");
        assert!(all.skipped.is_empty());

        let unrelated = vec!["README.md".to_string()];
        let forced =
            run_profile_with_changes(&root, "dev", Some("required"), Some(&unrelated)).unwrap();
        assert_eq!(forced.results[0].id, "required");
        assert!(forced.skipped.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn changed_files_are_normalized_and_exported_to_command_checks() {
        let root = scratch("changed-env");
        fs::create_dir_all(root.join("portal/src")).unwrap();
        fs::write(root.join("portal/src/lib.rs"), "ok").unwrap();
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: changed-env
    kind: command
    read_only: true
    inputs: ["portal/**/*.rs"]
    command: ["bash", "-lc", "printf '%s' \"$CARGOLESS_CHANGED_FILES\" > changed.out"]
    cache: none
"#,
        )
        .unwrap();
        let changed = vec![
            root.join("portal/src/lib.rs")
                .to_string_lossy()
                .into_owned(),
        ];
        let report = run_profile_with_changes(&root, "dev", None, Some(&changed)).unwrap();
        assert_eq!(report.tree, TreeState::Green);
        assert_eq!(
            fs::read_to_string(root.join("changed.out")).unwrap(),
            "portal/src/lib.rs"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn glob_star_star_matches_nested_paths() {
        assert!(glob_match_path("portal/**/*.rs", "portal/src/lib.rs"));
        assert!(glob_match_path("*.rs", "lib.rs"));
        assert!(!glob_match_path("*.rs", "src/lib.rs"));
    }

    #[test]
    fn command_jsonl_diagnostics_map_to_project_diagnostics() {
        let manifest = parse_manifest(
            PathBuf::from(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: generated-fast
    kind: command
    read_only: true
    command: ["./scripts/check-generated"]
"#,
        )
        .unwrap();
        let check = &manifest.checks[0];
        let root = PathBuf::from("/repo");
        let diagnostics = parse_command_diagnostics(
            &root,
            check,
            r#"{"schema":"cargoless.check-diagnostic/v1","check":"generated-fast","severity":"error","path":"src/generated/types.rs","line":7,"column":3,"code":"generated.drift","message":"generated output is stale","suggestion":"run ./scripts/devctl codegen"}"#,
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].file_path,
            PathBuf::from("/repo/src/generated/types.rs")
        );
        assert_eq!(diagnostics[0].line, 7);
        assert_eq!(diagnostics[0].col, 3);
        assert_eq!(diagnostics[0].severity, Severity::Error);
        assert_eq!(diagnostics[0].code.as_deref(), Some("generated.drift"));
        assert!(
            diagnostics[0]
                .message
                .contains("run ./scripts/devctl codegen")
        );
        assert_eq!(
            diagnostics[0].source.as_deref(),
            Some("cargoless-check:generated-fast")
        );
    }
}
