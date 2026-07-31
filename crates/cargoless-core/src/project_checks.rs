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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use cargoless_cas::sha256_hex;
use cargoless_proto::{Diagnostic, Severity, TreeState};

use crate::yamlscan::{
    ParseError, YamlNode, get_bool, get_string, get_string_list, get_u64, parse_yaml_value,
    reject_unknown, required_string,
};

const MANIFEST_NAME: &str = "cargoless.checks.yaml";
const ENGINE_VERSION: &str = "cargoless/project-checks/v2";
const TIMEOUT_DIAGNOSTIC_CODE: &str = "project-check.timeout";
/// A check that never ran because an earlier required check failed.
///
/// Deliberately its own code rather than reusing the timeout one: "we ran out
/// of time" and "we stopped on purpose because the verdict was already decided"
/// are different facts, and an operator reading a truncated run needs to know
/// which happened. It is also what keeps these out of the result cache — a skip
/// records nothing about the check itself and must never be replayed as one.
const SKIPPED_DIAGNOSTIC_CODE: &str = "project-check.skipped";

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
pub struct ProjectCheckPlan {
    pub fingerprint: String,
    pub manifest_hash: String,
    pub profile_name: String,
    pub selected: Vec<CheckSummary>,
    pub skipped: Vec<CheckSummary>,
    pub coalesceable: bool,
    pub non_coalesce_reason: Option<String>,
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
    /// Stop the run as soon as a **required** check goes red, killing whatever
    /// is still in flight.
    ///
    /// Off by default, deliberately. For a normal check profile you want the
    /// full picture — reporting one red and hiding the other nine makes a
    /// developer fix them one round-trip at a time. It earns its keep only
    /// where the remaining work is expensive and certain to be discarded, which
    /// is the build lane: once a required leg is red the candidate cannot land,
    /// so a 25-minute release build that is already doomed is pure waste.
    ///
    /// Non-required checks never trip it. A red advisory is information, not a
    /// verdict, and cancelling a build over one would be a lie about severity.
    fail_fast: bool,
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
    /// Which stage this check belongs to. Stages run in ascending order, and a
    /// stage starts only if every earlier stage was green.
    ///
    /// Default `0` puts every existing check in one stage, which is exactly
    /// today's behaviour — an unstaged manifest is unaffected.
    ///
    /// The point is early failure on an expensive pipeline. A build lane that
    /// runs `cargo check` in stage 1 and `cargo build --release` in stage 2
    /// pays ~8 minutes to reject a type error instead of ~39, because the
    /// release codegen never starts.
    stage: u32,
    /// Which `CARGO_TARGET_DIR` this check compiles into, when a warm target is
    /// in play. Checks sharing a key share a dir and are serialized; checks
    /// with different keys get different dirs and run genuinely in parallel.
    ///
    /// Empty (the default) means "the run's one shared target dir" — the
    /// historical behaviour, where every command check is one serial class.
    ///
    /// This is not a micro-optimisation. Under a warm target every `command`
    /// check was mutually exclusive, so declaring two compile legs in parallel
    /// bought nothing: they serialized on cargo's own `.cargo-lock`. Distinct
    /// keys are what make parallel legs actually parallel. The deploy this
    /// models has always used separate target dirs for its native and wasm
    /// builds; this closes the gap rather than opening a new risk.
    target_key: String,
    /// How to read structured diagnostics out of a `kind: command` check.
    ///
    /// * absent / `"check-diagnostic"` — cargoless's own
    ///   `cargoless.check-diagnostic/v1` line protocol (the default; unchanged).
    /// * `"cargo-json"` — the check emits `cargo --message-format=json`, parsed
    ///   by [`crate::cargodiag`].
    ///
    /// Without this, a check that pipes raw cargo JSON matches no v1 line and
    /// yields ZERO diagnostics; the non-zero exit then produces one synthetic
    /// diagnostic pinned at the manifest. Correctly red, but with every file,
    /// line and code discarded — and those are exactly what the build lane needs
    /// to decide whose change broke the build.
    output: CheckOutput,
}

/// Diagnostic format a `kind: command` check emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum CheckOutput {
    /// `cargoless.check-diagnostic/v1` — the historical default.
    #[default]
    CheckDiagnostic,
    /// `cargo … --message-format=json`.
    CargoJson,
}

impl CheckOutput {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "check-diagnostic" | "" => Some(CheckOutput::CheckDiagnostic),
            "cargo-json" => Some(CheckOutput::CargoJson),
            _ => None,
        }
    }
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

/// Every profile the manifest declares, sorted. Empty when there is no
/// manifest at all.
///
/// Exists so a caller can REFUSE an unknown profile name instead of inheriting
/// [`profile_for`]'s fallback. That fallback synthesises `include: ["*"]` with
/// a 12-second budget, and `"*"` matches every check regardless of tier — so a
/// typo silently selects the entire manifest, including any 25-80 minute
/// release build, and times almost all of it out. Tolerable for an interactive
/// `checks run --profile x`; catastrophic for the build lane, where the
/// resulting flood of manifest-pinned timeout diagnostics is attributable to
/// nobody and ejects the whole queue.
pub fn profile_names(root: &Path) -> Result<Vec<String>, ManifestError> {
    Ok(load_manifest(root)?
        .map(|m| m.profiles.keys().cloned().collect())
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

/// CGLS-26 — like [`run_dev_with_changes`] but with an optional WARM,
/// persistent, shared `CARGO_TARGET_DIR` for the compile (`None` = the
/// historical per-run cold dir). Only the serve daemon's witness path passes
/// `Some`, and only when its warm-target safety lock is held; every other
/// caller uses the plain form and is byte-identical to before.
pub fn run_dev_with_changes_in(
    root: &Path,
    changed_files: Option<&[String]>,
    witness_target_dir: Option<&Path>,
) -> io::Result<ProjectCheckReport> {
    run_profile_with_changes_in(root, "dev", None, changed_files, witness_target_dir)
}

pub fn run_profile(
    root: &Path,
    profile_name: &str,
    only_id: Option<&str>,
) -> io::Result<ProjectCheckReport> {
    run_profile_with_changes(root, profile_name, only_id, None)
}

pub fn plan_dev_with_changes(
    root: &Path,
    changed_files: Option<&[String]>,
) -> io::Result<ProjectCheckPlan> {
    plan_profile_with_changes(root, "dev", None, changed_files)
}

pub fn plan_profile_with_changes(
    root: &Path,
    profile_name: &str,
    only_id: Option<&str>,
    changed_files: Option<&[String]>,
) -> io::Result<ProjectCheckPlan> {
    let root = fs::canonicalize(root)?;
    let manifest = load_manifest(&root).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}:{}: {}", e.path.display(), e.line, e.message),
        )
    })?;
    let Some(manifest) = manifest else {
        let fingerprint =
            sha256_hex(format!("{ENGINE_VERSION}\nempty\n{profile_name}\n").as_bytes());
        return Ok(ProjectCheckPlan {
            fingerprint,
            manifest_hash: String::new(),
            profile_name: profile_name.to_string(),
            selected: Vec::new(),
            skipped: Vec::new(),
            coalesceable: true,
            non_coalesce_reason: None,
        });
    };
    let ids_filter: Vec<String> = only_id.into_iter().map(str::to_string).collect();
    let profile = profile_for(&manifest, profile_name);
    let profile_selected = checks_for_profile(&manifest, &profile, profile_name, &ids_filter);
    let manifest_changed = changed_files
        .map(|files| normalize_changed_files(&root, files))
        .is_some_and(|changed| changed.iter().any(|p| p == MANIFEST_NAME));
    let (selected, skipped) =
        select_for_changes(&root, profile_selected, &ids_filter, changed_files);
    let selected_summaries = selected.iter().map(check_summary).collect::<Vec<_>>();
    let fingerprint = project_check_plan_fingerprint(
        &manifest.manifest_hash,
        profile_name,
        &ids_filter,
        &selected,
    );
    Ok(ProjectCheckPlan {
        fingerprint,
        manifest_hash: manifest.manifest_hash,
        profile_name: profile_name.to_string(),
        selected: selected_summaries,
        skipped,
        coalesceable: !manifest_changed,
        non_coalesce_reason: manifest_changed
            .then(|| format!("{MANIFEST_NAME} changed; plan must be evaluated after overlay")),
    })
}

pub fn run_profile_with_changes(
    root: &Path,
    profile_name: &str,
    only_id: Option<&str>,
    changed_files: Option<&[String]>,
) -> io::Result<ProjectCheckReport> {
    run_profile_with_changes_in(root, profile_name, only_id, changed_files, None)
}

/// CGLS-26 — [`run_profile_with_changes`] with an optional WARM shared
/// `CARGO_TARGET_DIR` threaded into the compile. `None` is byte-identical to
/// the plain form (per-run cold dir).
pub fn run_profile_with_changes_in(
    root: &Path,
    profile_name: &str,
    only_id: Option<&str>,
    changed_files: Option<&[String]>,
    witness_target_dir: Option<&Path>,
) -> io::Result<ProjectCheckReport> {
    let ids: Vec<String> = only_id.into_iter().map(str::to_string).collect();
    run_profile_inner(root, profile_name, &ids, changed_files, witness_target_dir)
}

/// Run a profile restricted to an explicit SET of check ids (the merge-gate
/// witness-only lane). `ids` empty ⇒ whole profile (identical to
/// `run_profile_with_changes(.., None, ..)`); non-empty ⇒ exactly those ids
/// (their intersection with the profile), change-filtering disabled. Added
/// for the per-check witness gate: a gated push runs only its requested
/// compile witnesses (ssr/wasm/isolator-vsock) instead of the whole dev
/// profile, so the gate proves the witnesses ran without dragging the ~97
/// governance/coverage checks (and their environmental reds) into a gating
/// verdict. No-vacuous-green holds because each witness id is `tier:dev` and
/// so is a member of the dev profile — the id∧profile AND selects exactly the
/// requested witnesses, never nothing.
pub fn run_profile_with_ids(
    root: &Path,
    profile_name: &str,
    ids: &[String],
    changed_files: Option<&[String]>,
) -> io::Result<ProjectCheckReport> {
    run_profile_inner(root, profile_name, ids, changed_files, None)
}

/// CGLS-26 — [`run_profile_with_ids`] with an optional WARM shared
/// `CARGO_TARGET_DIR` (the gated witness-only lane is exactly the compile
/// path warmth targets). `None` = cold, byte-identical to the plain form.
pub fn run_profile_with_ids_in(
    root: &Path,
    profile_name: &str,
    ids: &[String],
    changed_files: Option<&[String]>,
    witness_target_dir: Option<&Path>,
) -> io::Result<ProjectCheckReport> {
    run_profile_inner(root, profile_name, ids, changed_files, witness_target_dir)
}

/// Shared inner: `ids_filter` empty ⇒ no id filter (whole profile, change-
/// filtering active); non-empty ⇒ run EXACTLY those ids with change-filtering
/// disabled. The public entry points differ only in how they express the
/// filter (`Option<&str>` single-id vs `&[String]` multi-id) and whether they
/// thread a CGLS-26 warm target dir.
fn run_profile_inner(
    root: &Path,
    profile_name: &str,
    ids_filter: &[String],
    changed_files: Option<&[String]>,
    witness_target_dir: Option<&Path>,
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

    let profile = profile_for(&manifest, profile_name);
    let selected = checks_for_profile(&manifest, &profile, profile_name, ids_filter);
    let (selected, skipped) = select_for_changes(&root, selected, ids_filter, changed_files);
    let snapshot = Arc::new(RepoSnapshot::build(&root)?);
    let ctx = Arc::new(RunContext {
        root: root.clone(),
        snapshot,
        manifest_hash: manifest.manifest_hash.clone(),
        profile_name: profile_name.to_string(),
        changed_files: changed_files.map(|files| normalize_changed_files(&root, files)),
        witness_target_dir: witness_target_dir.map(Path::to_path_buf),
        cancel: Arc::new(AtomicBool::new(false)),
    });
    let results = run_parallel(
        ctx,
        selected,
        profile.max_parallel.max(1),
        profile.timeout_ms,
        profile.fail_fast,
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

fn profile_for(manifest: &ProjectChecksManifest, profile_name: &str) -> ProfileConfig {
    match manifest.profiles.get(profile_name) {
        Some(v) => v.clone(),
        None => ProfileConfig {
            include: vec!["*".to_string()],
            timeout_ms: 12_000,
            max_parallel: 8,
            on_timeout: "red".to_string(),
            // An undeclared profile never fail-fasts. It is a fallback, so the
            // caller has not asked for anything; reporting the full picture is
            // the safer default when we are guessing at intent.
            fail_fast: false,
        },
    }
}

fn checks_for_profile(
    manifest: &ProjectChecksManifest,
    profile: &ProfileConfig,
    profile_name: &str,
    ids_filter: &[String],
) -> Vec<CheckConfig> {
    manifest
        .checks
        .iter()
        // Empty `ids_filter` ⇒ no id restriction (whole profile). Non-empty ⇒
        // keep only checks whose id is in the set (the witness-only gate lane).
        .filter(|c| ids_filter.is_empty() || ids_filter.iter().any(|id| id == &c.id))
        .filter(|c| profile_includes(profile, c, profile_name))
        .cloned()
        .collect()
}

fn project_check_plan_fingerprint(
    manifest_hash: &str,
    profile_name: &str,
    ids_filter: &[String],
    selected: &[CheckConfig],
) -> String {
    let mut preimage = String::new();
    preimage.push_str(ENGINE_VERSION);
    preimage.push('\n');
    preimage.push_str(manifest_hash);
    preimage.push('\n');
    preimage.push_str(profile_name);
    preimage.push('\n');
    // Preserve the prior single-id/no-id serialization exactly (no filter ⇒
    // "*"), so existing plan-cache tokens don't churn. A multi-id filter
    // joins the ids with ',' — a shape the single-id path could never emit,
    // so the two id spaces stay distinct without a version bump.
    if ids_filter.is_empty() {
        preimage.push('*');
    } else {
        preimage.push_str(&ids_filter.join(","));
    }
    preimage.push('\n');
    for check in selected {
        preimage.push_str(&check.id);
        preimage.push('\0');
        preimage.push_str(&check_config_hash(check));
        preimage.push('\n');
    }
    sha256_hex(preimage.as_bytes())
}

fn check_config_hash(check: &CheckConfig) -> String {
    sha256_hex(format!("{check:?}").as_bytes())
}

fn select_for_changes(
    root: &Path,
    checks: Vec<CheckConfig>,
    ids_filter: &[String],
    changed_files: Option<&[String]>,
) -> (Vec<CheckConfig>, Vec<CheckSummary>) {
    // An explicit id filter means "run exactly these" — trigger-glob
    // change-filtering is intentionally bypassed so a requested witness is
    // never additionally skipped for not matching the changed-file globs.
    if !ids_filter.is_empty() {
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
    /// CGLS-26 — optional WARM, persistent, shared `CARGO_TARGET_DIR` for the
    /// witness compile. `None` (the default and every non-witness caller)
    /// keeps the historical per-run `ctx.root/.cargoless-target`, byte-for-
    /// byte. `Some(dir)` redirects the compile at a keyed dir under the
    /// daemon state dir that survives across runs so a small diff reuses
    /// cached `deps/` rlibs + `.fingerprint/` instead of a cold rebuild. The
    /// serve daemon resolves this (with its lock/fallback safety) and only
    /// passes `Some` when the warm-target flag is on and the dir is safely
    /// held; see `ServeVerdictState::with_project_check_scratch_overlay`.
    witness_target_dir: Option<PathBuf>,
    /// Set when a `fail_fast` profile has already seen a required red, so the
    /// remaining work is certain to be discarded.
    ///
    /// Running commands poll this on the same 20ms tick they use for their own
    /// timeout and kill their process tree when it flips. Cancellation is
    /// therefore cooperative and bounded, and it reuses the kill path that
    /// timeouts already exercise rather than inventing a second one.
    cancel: Arc<AtomicBool>,
}

/// The `CARGO_TARGET_DIR` a given check compiles into.
///
/// Without a `target_key` this is the run's one dir — the warm dir the daemon
/// resolved, or the per-run scratch — exactly as before. With a `target_key`
/// it is a sibling *beside* that dir, so two legs that declare different keys
/// never touch each other's `deps/`, `.fingerprint/` or `incremental/`.
///
/// The key is sanitised rather than trusted: it comes from a manifest and ends
/// up as a path component, so `../` in a key must not be able to place a target
/// dir outside the parent. Anything that is not `[A-Za-z0-9._-]` becomes `_`.
fn resolve_target_dir(ctx: &RunContext, check: &CheckConfig) -> PathBuf {
    let base = ctx
        .witness_target_dir
        .clone()
        .unwrap_or_else(|| ctx.root.join(".cargoless-target"));
    if check.target_key.is_empty() {
        return base;
    }
    let safe: String = check
        .target_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // `.` and `..` survive the char filter but are still traversal; refuse them
    // by name rather than trying to be clever about the path.
    if safe.is_empty() || safe == "." || safe == ".." {
        return base;
    }
    let mut dir = base.clone();
    let leaf = base
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cargoless-target".to_string());
    dir.set_file_name(format!("{leaf}-{safe}"));
    dir
}

/// True for checks that will compile into a SHARED `CARGO_TARGET_DIR` — i.e.
/// the ones that must not run concurrently *with each other*.
///
/// Only `command` checks ever inherit `CARGO_TARGET_DIR` (see `check_command`);
/// every other kind is pure in-process analysis and is unaffected. And only a
/// `Some` warm dir is shared — with `None` each run compiles into its own
/// per-run `<root>/.cargoless-target`, so there is nothing to contend on.
///
/// Returns the dir that would be contended, so the scheduler can serialize
/// *per dir* instead of across every command check at once. Two legs with
/// different `target_key`s return different dirs and are free to run together;
/// two legs with the same key still take turns, which is correct — they really
/// would block each other on cargo's `.cargo-lock`.
fn warm_target_class(ctx: &RunContext, check: &CheckConfig) -> Option<PathBuf> {
    if ctx.witness_target_dir.is_none() || check.kind != CheckKind::Command {
        return None;
    }
    Some(resolve_target_dir(ctx, check))
}

/// Run `checks` with at most `max_parallel` in flight — EXCEPT that checks
/// sharing the warm target dir form a **serial class**: at most one of those
/// runs at a time, while everything else keeps fanning out normally.
///
/// ## Why the serial class exists
///
/// It does not buy wall-clock. Those compiles were ALREADY serialized, just
/// invisibly and wastefully: cargo takes an exclusive `flock` on
/// `<target>/<layout>/.cargo-lock` for the build, so N concurrent `cargo`s
/// against one target dir means one builder and N-1 processes parked in
/// `locks_lock_inode_wait`. Observed live on the witness pod (2026-07-30),
/// three witnesses on one warm dir:
///
/// ```text
/// FLOCK ADVISORY WRITE 91447 …2767534        <- wasm holds release/
///    -> FLOCK ADVISORY WRITE 91467 …2767534  <- csr blocked
///    -> FLOCK ADVISORY WRITE 91489 …2767534  <- ssr blocked
/// ```
///
/// The two waiters had burned 256s of wall for ~0.5s of CPU. Admitting them
/// only to have the kernel park them costs a thread, a live scratch worktree,
/// and a process that *looks* like running work in `ps` and in the operator's
/// mental model. Holding them in `pending` instead is the same execution
/// order with none of that.
///
/// So this is a **clarity and resource fix, not a latency fix** — expect
/// verdict latency to be unchanged. If it improves, the convoy was costing
/// more than the lock analysis predicted and that is worth understanding
/// rather than just banking.
///
/// ## Why not `max_parallel: 1` in the manifest
///
/// `max_parallel` is per-PROFILE, and the `dev` profile carries 111 checks —
/// the witnesses are 3 of them. Capping the profile would serialize the whole
/// advisory governance lane. The constraint is a property of the shared target
/// dir, not of the profile, so it belongs here where that dir is known.
///
/// ## Stages
///
/// Checks are grouped by `stage` and the groups run in ascending order. A stage
/// starts only if every earlier stage was green on its **required** checks, so
/// a cheap `cargo check` in stage 1 can reject a candidate before an expensive
/// `cargo build --release` in stage 2 ever starts. An unstaged manifest has one
/// stage and behaves exactly as before.
///
/// Skipped stages are still REPORTED, as reds carrying the reason. Dropping
/// them would make a truncated run indistinguishable from a shorter pipeline —
/// the caller would see fewer results and no explanation, and a lane deciding
/// whom to eject would be reasoning from a silently incomplete picture.
fn run_parallel(
    ctx: Arc<RunContext>,
    checks: Vec<CheckConfig>,
    max_parallel: usize,
    profile_timeout_ms: u64,
    fail_fast: bool,
) -> Vec<ProjectCheckResult> {
    let start = Instant::now();
    let mut out = Vec::new();

    // BTreeMap so stages run in ascending numeric order regardless of the
    // order they appear in the manifest.
    let mut stages: BTreeMap<u32, Vec<CheckConfig>> = BTreeMap::new();
    for check in checks {
        stages.entry(check.stage).or_default().push(check);
    }

    let mut halted: Option<u32> = None;
    for (stage, group) in stages {
        if let Some(failed_at) = halted {
            // An earlier stage failed. Report the rest rather than dropping it.
            for check in group {
                out.push(skipped_stage_result(
                    &ctx.root,
                    &check,
                    &format!(
                        "stage {stage} did not run: stage {failed_at} failed, \
                         and a later stage cannot change that verdict"
                    ),
                ));
            }
            continue;
        }
        let results = run_stage(
            ctx.clone(),
            group,
            max_parallel,
            profile_timeout_ms,
            start,
            fail_fast,
        );
        let stage_red = results
            .iter()
            .any(|r| r.required && r.tree == TreeState::Red);
        out.extend(results);
        if stage_red {
            halted = Some(stage);
        }
    }

    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Run one stage's checks with at most `max_parallel` in flight.
///
/// `start` is the PROFILE's clock, not this stage's — the profile timeout
/// bounds the whole run, so a late stage inherits whatever budget is left
/// rather than getting a fresh one.
fn run_stage(
    ctx: Arc<RunContext>,
    checks: Vec<CheckConfig>,
    max_parallel: usize,
    profile_timeout_ms: u64,
    start: Instant,
    fail_fast: bool,
) -> Vec<ProjectCheckResult> {
    let mut pending: VecDeque<CheckConfig> = checks.into();
    let (tx, rx) = mpsc::channel();
    let mut in_flight = 0usize;
    // Target dirs currently being compiled into. A check whose dir is in here
    // must wait — it would only park on cargo's `.cargo-lock` anyway.
    let mut warm_in_flight: BTreeSet<PathBuf> = BTreeSet::new();
    let mut out = Vec::new();
    let mut cancelled = false;

    while !pending.is_empty() || in_flight > 0 {
        while in_flight < max_parallel && !pending.is_empty() && !cancelled {
            if start.elapsed() >= Duration::from_millis(profile_timeout_ms) {
                break;
            }
            // Scan for the first admissible check rather than always taking
            // the head: a blocked warm check must not stall the non-warm
            // checks queued behind it (that would be the profile-wide
            // serialization this design exists to avoid). Order within each
            // class is still FIFO, and `out` is sorted by id at the end, so
            // the reported result order is unaffected either way.
            let Some(idx) = pending.iter().position(|c| {
                warm_target_class(&ctx, c).is_none_or(|dir| !warm_in_flight.contains(&dir))
            }) else {
                // Everything left wants a dir that is already busy: wait for
                // one to free rather than spinning.
                break;
            };
            let check = pending.remove(idx).expect("index from position()");
            let warm = warm_target_class(&ctx, &check);
            if let Some(dir) = warm.clone() {
                warm_in_flight.insert(dir);
            }
            let tx = tx.clone();
            let ctx = ctx.clone();
            in_flight += 1;
            thread::spawn(move || {
                let result = run_one(&ctx, check);
                // Report the class back so the scheduler releases the serial
                // slot on the SAME path it took it — including when `run_one`
                // returns an error result.
                let _ = tx.send((warm, result));
            });
        }
        if in_flight == 0 {
            break;
        }
        match rx.recv() {
            Ok((warm, result)) => {
                let tripped = fail_fast && result.required && result.tree == TreeState::Red;
                out.push(result);
                in_flight -= 1;
                if let Some(dir) = warm {
                    warm_in_flight.remove(&dir);
                }
                if tripped && !cancelled {
                    // Signal the running commands to kill their process trees.
                    // We keep draining `rx` so every in-flight check still
                    // reports — a cancelled check that never returns a result
                    // would leave a hole the caller cannot distinguish from a
                    // check that was never selected.
                    cancelled = true;
                    ctx.cancel.store(true, Ordering::SeqCst);
                }
            }
            Err(_) => break,
        }
    }

    for check in pending {
        if cancelled {
            out.push(skipped_stage_result(
                &ctx.root,
                &check,
                "did not run: an earlier required check in this stage failed \
                 and the profile is fail_fast",
            ));
        } else {
            out.push(timeout_result(
                &ctx.root,
                &check,
                "profile timeout reached before this check started",
            ));
        }
    }
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

/// A check that never ran because the verdict was already decided.
///
/// Reported RED for a required check, because the honest answer to "is this
/// check green?" is "we do not know", and a gate must not read unknown as
/// pass. A non-required check degrades to a warning on a green tree — the same
/// shape `timeout_result` uses, for the same reason: an advisory that did not
/// run cannot fail a build.
fn skipped_stage_result(root: &Path, check: &CheckConfig, message: &str) -> ProjectCheckResult {
    let mut result = result_from_diags(
        check,
        vec![diag(
            root,
            check,
            MANIFEST_NAME,
            1,
            1,
            SKIPPED_DIAGNOSTIC_CODE,
            message,
        )],
        0,
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

/// SIGKILL a timed-out command's ENTIRE process tree, not just the immediate
/// child. `check_command` spawns the command (e.g. `cargo check`) as its own
/// process-group + session leader; on timeout this kills `-pgid` (every
/// descendant that inherited the group — the common case: `cargo`'s `rustc`
/// children) then sweeps the session for any `setpgid` escapees. Mirrors the
/// proven reaper in `analyzer::ReapOnDrop`. Without this, a timed-out `cargo`
/// leaks `rustc` grandchildren that reparent to init and keep compiling past
/// the deadline (the leak the live warn soak surfaced 2026-06-08).
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // Snapshot session members BEFORE killing (the child is a session
        // leader via setsid, so sid == pid). Missing pgrep degrades to a
        // no-op here; the pgid kill below still runs.
        let session_members = command_session_members(pid);
        unsafe {
            unsafe extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            const SIGKILL: i32 = 9;
            // Fast path: SIGKILL the whole process group (negative pid).
            let _ = kill(-pid, SIGKILL);
            // Defense in depth: SIGKILL setpgid escapees still in the session.
            for m in session_members {
                if m != pid {
                    let _ = kill(m, SIGKILL);
                }
            }
        }
    }
    // Belt-and-braces: also kill the immediate child directly (on non-unix
    // this is the only step; on unix the SIGKILL above usually already did it).
    let _ = child.kill();
}

/// Every PID in `sid`'s session via `pgrep -s`. Empty on any failure (pgrep
/// missing / non-zero / no output) — all safe degradations since the pgid
/// SIGKILL in [`kill_process_tree`] is the load-bearing step.
#[cfg(unix)]
fn command_session_members(sid: i32) -> Vec<i32> {
    let Ok(output) = Command::new("pgrep")
        .arg("-s")
        .arg(sid.to_string())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<i32>().ok())
        .collect()
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
    // Pin CARGO_TARGET_DIR. The DEFAULT (`witness_target_dir == None`) is a
    // path inside this run's scratch worktree so witness builds in DIFFERENT
    // runs cannot clobber each other's `incremental/`, `.fingerprint/`, or
    // encoded-metadata files (CGLS-24: `failed to create encoded metadata
    // from file: os error 2`). The scratch is per-run by construction
    // (`run-<pid>-<seq>/`) and is `git worktree remove --force`'d at cleanup,
    // so the target subtree is auto-collected with it.
    //
    // NOTE the granularity, because it is easy to over-read: this dir is
    // per-RUN, not per-CHECK. Every `command` check in one run — the ssr,
    // wasm and csr witnesses together — shares it. Isolation ACROSS runs is
    // what this line buys; isolation WITHIN a run has always come from
    // cargo's own blocking `<target>/<layout>/.cargo-lock`, plus (since the
    // CGLS-24/26 follow-up) the warm serial class in `run_parallel`.
    //
    // CGLS-26 — when the daemon resolves a WARM, persistent, shared target
    // dir (witness serialization removed the cross-RUN concurrency hazard, so
    // a shared dir is safe; the daemon holds a per-key lock and falls back to
    // cold on any doubt), it passes it here and the compile reuses cached
    // artifacts across runs instead of a cold rebuild. Either way this env
    // set wins over any ambient `CARGO_TARGET_DIR` on the daemon pod (e.g.
    // the `/workspace/target` default in `cargoless-serve.k8s.yaml`) and over
    // any workspace `.cargo/config.toml` `[build] target-dir` — cargo's
    // resolution order is env > config > default.
    let cargo_target_dir = resolve_target_dir(ctx, check);
    let mut cmd = Command::new(&check.command[0]);
    cmd.args(&check.command[1..])
        .current_dir(&ctx.root)
        .env("CARGOLESS", "1")
        .env("CARGOLESS_CHECK_ID", &check.id)
        .env("CARGOLESS_PROFILE", &ctx.profile_name)
        .env("CARGOLESS_WORKTREE", &ctx.root)
        .env("CARGO_TARGET_DIR", &cargo_target_dir)
        .env(
            "CARGOLESS_CHANGED_FILES",
            ctx.changed_files
                .as_ref()
                .map(|files| files.join("\n"))
                .unwrap_or_default(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Run the command (e.g. `cargo check`) as the leader of its own process
    // GROUP + SESSION so a timeout can SIGKILL the WHOLE tree, not just the
    // immediate child. Without this, a timed-out `cargo` leaves its `rustc`
    // grandchildren reparented to init (ppid=1), still compiling for minutes
    // past the deadline — the leak the live warn soak surfaced (2026-06-08).
    // Mirrors `analyzer::rust_analyzer_command`'s proven pgid+setsid setup.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
        // SAFETY: pre_exec runs post-fork/pre-exec in a single-threaded child;
        // setsid(2) is async-signal-safe. EPERM (already a session leader) is
        // swallowed — process_group(0) above is the load-bearing line.
        unsafe {
            cmd.pre_exec(|| {
                unsafe extern "C" {
                    fn setsid() -> i32;
                }
                let _ = setsid();
                Ok(())
            });
        }
    }
    let mut child = match cmd.spawn() {
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
                // Kill the whole process tree, not just the immediate child:
                // SIGKILL the process group, then sweep any setpgid escapees
                // still in our session. `child.kill()` alone leaks `rustc`
                // grandchildren (the warn-soak leak). Falls back to a plain
                // child kill on non-unix.
                kill_process_tree(&mut child);
                let _ = child.wait();
                break Err("command timed out".to_string());
            }
            Ok(None) if ctx.cancel.load(Ordering::SeqCst) => {
                // A required check in this stage already failed and the profile
                // is fail_fast, so this compile cannot change the verdict.
                // Same kill path as a timeout — the reason differs, the
                // grandchild-reaping problem does not.
                kill_process_tree(&mut child);
                let _ = child.wait();
                break Err("cancelled".to_string());
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
                let (code, message) = classify_exit(s, &tail);
                command_diagnostics.push(diag(
                    &ctx.root,
                    check,
                    MANIFEST_NAME,
                    1,
                    1,
                    code,
                    &message,
                ));
            }
            result_from_diags(check, command_diagnostics, 0)
        }
        // A cancelled check is NOT a timeout and must not be reported as one.
        // We killed it on purpose because the stage's verdict was already
        // decided; saying "timed out" would blame the check for a decision the
        // scheduler made, and would send someone hunting a performance problem
        // that does not exist. `skipped_stage_result` also keeps it out of the
        // cache, so the check runs normally on the next attempt.
        Err(message) if message == "cancelled" => skipped_stage_result(
            &ctx.root,
            check,
            "cancelled: an earlier required check in this stage failed and the \
             profile is fail_fast — this check did not finish and has no verdict",
        ),
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

/// Translate a wrapped check command's non-zero exit into a structured
/// diagnostic. Generic across check kinds (rustc, python, bash) — the bash
/// idiom `exit "$rc"` propagates a signal-coded child as a 128+N exit code,
/// which lands here as a numeric exit, NOT a unix signal on the bash process.
///
/// Calling this out separately matters for verdict trust: a 141 (SIGPIPE) /
/// 137 (SIGKILL) is the OS killing the wrapped command, not an honest tool-
/// reported failure. Rendering both as `command.failed` hides that distinction
/// and forces an operator to grep the source code to know whether a witness
/// found a real RED or merely got pipe-raced on a deadline. The synthetic
/// `command.process_killed` code surfaces it as a discrete class.
fn classify_exit(status: std::process::ExitStatus, tail: &str) -> (&'static str, String) {
    // Unix-signal-terminated child: ExitStatus::signal() returns Some(N).
    // (`s.code()` is None in that case; bash also re-encodes as 128+N when it
    // propagates its child's signal — we cover that case below.)
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            let name = signal_name(sig);
            return (
                "command.process_killed",
                format!(
                    "command was killed by SIG{name} (signal {sig}); not an honest tool verdict.\noutput:\n{tail}"
                ),
            );
        }
    }
    // Numeric exit codes from `bash -c '... ; exit "$rc"'` carry the wrapped
    // child's signal as 128+N (SIGPIPE=13 → 141, SIGKILL=9 → 137,
    // SIGTERM=15 → 143). Treat those exactly like the signal() case.
    if let Some(code) = status.code().filter(|c| (128..192).contains(c)) {
        let sig = code - 128;
        let name = signal_name(sig);
        return (
            "command.process_killed",
            format!(
                "command exited with status {code} (wrapped child killed by SIG{name}, signal {sig}); not an honest tool verdict.\noutput:\n{tail}"
            ),
        );
    }
    (
        "command.failed",
        format!("command exited with {status}; output:\n{tail}"),
    )
}

fn signal_name(sig: i32) -> &'static str {
    match sig {
        1 => "HUP",
        2 => "INT",
        3 => "QUIT",
        9 => "KILL",
        13 => "PIPE",
        15 => "TERM",
        _ => "?",
    }
}

fn parse_command_diagnostics(root: &Path, check: &CheckConfig, text: &str) -> Vec<Diagnostic> {
    match check.output {
        // The check speaks cargo's own JSON.
        //
        // `source` stays exactly "rustc" — NOT "rustc:<check-id>". Two things
        // key on that string: the daemon's authoritative-vs-advisory split, and
        // `attribution::fingerprint`, whose identity is
        // `source|code|path|message`. Qualifying it would demote real compiler
        // errors to the advisory tier AND make the same error fingerprint
        // differently depending on which leg happened to report it — so a
        // change that only moved a check id would read as a brand-new failure.
        CheckOutput::CargoJson => crate::cargodiag::parse_cargo_json(root, text),
        CheckOutput::CheckDiagnostic => text
            .lines()
            .filter_map(|line| parse_command_diagnostic_line(root, check, line))
            .collect(),
    }
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

/// True for results that describe the RUN rather than the CHECK, and so must
/// never enter or be served from the cache.
///
/// A timeout says the machine was busy; a skip says we stopped early. Neither
/// is a property of the check's inputs, so caching one would replay a transient
/// condition as a verdict — a check that timed out once would stay red, and a
/// check skipped behind a failure would stay skipped after the failure was
/// fixed.
fn has_timeout_diagnostic(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(|d| {
        matches!(
            d.code.as_deref(),
            Some(TIMEOUT_DIAGNOSTIC_CODE) | Some(SKIPPED_DIAGNOSTIC_CODE)
        )
    })
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
                fail_fast: false,
            },
        );
        return Ok(out);
    };
    for (name, profile) in node.expect_map("profiles")? {
        let map = profile.expect_map(&format!("profiles.{name}"))?;
        reject_unknown(
            map,
            &[
                "include",
                "timeout_ms",
                "max_parallel",
                "on_timeout",
                "fail_fast",
            ],
            profile.line(),
        )?;
        out.insert(
            name.clone(),
            ProfileConfig {
                include: get_string_list(map, "include")?.unwrap_or_default(),
                timeout_ms: get_u64(map, "timeout_ms")?.unwrap_or(12_000),
                max_parallel: get_u64(map, "max_parallel")?.unwrap_or(8) as usize,
                on_timeout: get_string(map, "on_timeout")?.unwrap_or_else(|| "red".to_string()),
                fail_fast: get_bool(map, "fail_fast")?.unwrap_or(false),
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
                "output",
                "stage",
                "target_key",
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
            stage: {
                let raw = get_u64(map, "stage")?.unwrap_or(0);
                // Bound it rather than truncating a u64 into a u32. A manifest
                // asking for stage 5_000_000_000 is a mistake, and silently
                // wrapping it to some other stage would reorder the pipeline
                // without saying so.
                u32::try_from(raw).map_err(|_| ParseError {
                    line: item.line(),
                    message: format!("stage {raw} is out of range (max {})", u32::MAX),
                })?
            },
            target_key: get_string(map, "target_key")?.unwrap_or_default(),
            output: {
                let raw = get_string(map, "output")?.unwrap_or_default();
                // An unrecognised value is an ERROR, never a silent fallback:
                // typoing `cargo_json` would otherwise leave every diagnostic
                // unparsed and the lane unable to attribute a single red.
                CheckOutput::parse(&raw).ok_or(ParseError {
                    line: item.line(),
                    message: format!(
                        "unknown output {raw:?}; expected \"check-diagnostic\" or \"cargo-json\""
                    ),
                })?
            },
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

/// Slash-segmented glob match (`*` within a segment, `**` spanning
/// segments), the matcher behind manifest `triggers:` patterns. `pub`
/// since #A8: the serve layer reuses it to classify pushed
/// `changed_files` against the operator's macro-blind path globs
/// (`CARGOLESS_MACRO_BLIND_PATHS`) — one matcher, one semantics, so a
/// pattern that scopes a project check matches identically as a
/// blind-path annotation.
pub fn glob_match_path(pattern: &str, path: &str) -> bool {
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
        // The command appends its marker FIRST, then blocks far past the
        // timeout. Earlier this used `timeout_ms: 1` with `sleep 0.05`, which
        // raced process startup: under CI load the SIGKILL could land during
        // bash's startup before `printf` ran, leaving `counter` short of "xx"
        // (the load-flake that reddened the `test` job + main). A timeout
        // comfortably above startup jitter (but far below the 30s block)
        // guarantees the marker is written every run AND that the check still
        // times out, so the result is not cached and the command re-runs.
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: slow
    kind: command
    read_only: true
    command: ["bash", "-c", "printf x >> counter; sleep 30"]
    timeout_ms: 200
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
    fn project_check_plan_fingerprint_tracks_selected_checks() {
        let root = scratch("plan-fingerprint");
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

        let docs_change = vec!["docs/readme.md".to_string()];
        let docs_again = vec!["./docs/readme.md".to_string()];
        let portal_change = vec!["portal/src/lib.rs".to_string()];
        let docs = plan_dev_with_changes(&root, Some(&docs_change)).unwrap();
        let docs_same = plan_dev_with_changes(&root, Some(&docs_again)).unwrap();
        let portal = plan_dev_with_changes(&root, Some(&portal_change)).unwrap();

        assert_eq!(docs.fingerprint, docs_same.fingerprint);
        assert_ne!(docs.fingerprint, portal.fingerprint);
        assert_eq!(
            docs.selected.iter().map(|s| &s.id).collect::<Vec<_>>(),
            vec!["docs"]
        );
        assert!(docs.coalesceable);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_check_plan_marks_manifest_edits_non_coalesceable() {
        let root = scratch("plan-manifest-edit");
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

        let changed = vec![MANIFEST_NAME.to_string()];
        let plan = plan_dev_with_changes(&root, Some(&changed)).unwrap();
        assert!(!plan.coalesceable);
        assert_eq!(plan.selected[0].id, "required");
        assert!(plan.non_coalesce_reason.unwrap().contains(MANIFEST_NAME));
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
    fn run_profile_with_ids_selects_exactly_the_requested_witnesses() {
        // The merge-gate witness-only lane: a gated push requests a SET of ids
        // (the compile witnesses) and must run EXACTLY that set's intersection
        // with the profile, with change-filtering disabled — never the full
        // profile, and never (the no-vacuous-green invariant) nothing.
        let root = scratch("run-with-ids");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "ok").unwrap();
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: witness-a
    kind: required_patterns
    inputs: ["src/*.rs"]
    patterns:
      - code: a.ok
        literal: ok
        message: missing ok
  - id: witness-b
    kind: required_patterns
    inputs: ["src/*.rs"]
    patterns:
      - code: b.ok
        literal: ok
        message: missing ok
  - id: governance
    kind: required_patterns
    inputs: ["src/*.rs"]
    patterns:
      - code: g.ok
        literal: ok
        message: missing ok
"#,
        )
        .unwrap();

        // Two-witness filter selects EXACTLY those two, dropping `governance` —
        // even though `governance` is in the dev profile and its inputs match.
        // `changed_files=None` here, but the filter also disables change-
        // filtering: an unrelated changed-file list must NOT skip a witness.
        let unrelated = vec!["README.md".to_string()];
        let ids = vec!["witness-a".to_string(), "witness-b".to_string()];
        let ran = run_profile_with_ids(&root, "dev", &ids, Some(&unrelated)).unwrap();
        let mut ran_ids: Vec<&str> = ran.results.iter().map(|r| r.id.as_str()).collect();
        ran_ids.sort_unstable();
        assert_eq!(
            ran_ids,
            vec!["witness-a", "witness-b"],
            "gated set must run exactly its intersection with the profile"
        );
        assert!(
            ran.skipped.is_empty(),
            "id-filter disables change-filtering: nothing is trigger-skipped"
        );

        // No-vacuous-green: an id absent from the manifest is simply not
        // selected — the present witness still runs (never selects nothing).
        let with_ghost = vec!["witness-a".to_string(), "does-not-exist".to_string()];
        let ran = run_profile_with_ids(&root, "dev", &with_ghost, None).unwrap();
        assert_eq!(
            ran.results
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["witness-a"],
            "a ghost id is dropped; the real witness still runs (no vacuous green)"
        );

        // Empty filter ⇒ whole profile (identical to the None single-id path).
        let all = run_profile_with_ids(&root, "dev", &[], None).unwrap();
        assert_eq!(
            all.results.len(),
            3,
            "empty id set runs the full profile, not a filtered subset"
        );
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

    /// CGLS-24: `check_command` must set `CARGO_TARGET_DIR` to a path inside
    /// its scratch root so concurrent witness builds cannot collide on a
    /// shared `incremental/`. A regression that drops the env line would
    /// leave the spawned process inheriting the daemon-pod default and
    /// reintroduce the `os error 2` race.
    #[test]
    fn command_check_isolates_cargo_target_dir_per_scratch_root() {
        let root = scratch("target-dir-isolate");
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: target-dir-isolate
    kind: command
    read_only: true
    command: ["bash", "-lc", "printf '%s' \"$CARGO_TARGET_DIR\" > target-dir.out"]
    cache: none
"#,
        )
        .unwrap();
        let report = run_profile(&root, "dev", None).unwrap();
        assert_eq!(report.tree, TreeState::Green);
        // Canonicalized: run_profile canonicalizes ctx.root, so on macOS a
        // /var/... temp dir surfaces to the child as /private/var/... .
        let expected = fs::canonicalize(&root).unwrap().join(".cargoless-target");
        assert_eq!(
            fs::read_to_string(root.join("target-dir.out")).unwrap(),
            expected.to_string_lossy(),
        );
        let _ = fs::remove_dir_all(root);
    }

    /// CGLS-26: with a warm dir passed (`run_*_in(Some(dir))`), the child
    /// process sees CARGO_TARGET_DIR == that warm dir; with `None` it sees the
    /// historical per-run `<root>/.cargoless-target`. This is the whole
    /// mechanism — a persistent shared dir survives across runs so a small
    /// diff reuses cached artifacts.
    #[test]
    fn command_check_uses_warm_target_dir_when_some_else_cold() {
        let manifest = r#"
version: 1
checks:
  - id: warm-target-echo
    kind: command
    read_only: true
    command: ["bash", "-lc", "printf '%s' \"$CARGO_TARGET_DIR\" > target-dir.out"]
    cache: none
"#;
        // Some(warm) ⇒ child sees the warm dir.
        let root = scratch("warm-target-some");
        fs::write(root.join(MANIFEST_NAME), manifest).unwrap();
        let warm = scratch("warm-shared-dir");
        let report = run_dev_with_changes_in(&root, None, Some(&warm)).unwrap();
        assert_eq!(report.tree, TreeState::Green);
        assert_eq!(
            fs::read_to_string(root.join("target-dir.out")).unwrap(),
            warm.to_string_lossy(),
            "warm dir passed ⇒ CARGO_TARGET_DIR is the warm dir"
        );
        let _ = fs::remove_dir_all(&warm);
        let _ = fs::remove_dir_all(&root);

        // None ⇒ byte-identical to today (per-run cold dir). Compare against
        // the canonicalized root: `run_profile` canonicalizes ctx.root, so on
        // macOS the child sees `/private/var/...` for a `/var/...` temp dir.
        let root2 = scratch("warm-target-none");
        fs::write(root2.join(MANIFEST_NAME), manifest).unwrap();
        let report2 = run_dev_with_changes_in(&root2, None, None).unwrap();
        assert_eq!(report2.tree, TreeState::Green);
        let expected2 = fs::canonicalize(&root2).unwrap().join(".cargoless-target");
        assert_eq!(
            fs::read_to_string(root2.join("target-dir.out")).unwrap(),
            expected2.to_string_lossy(),
            "None ⇒ cold per-run dir, unchanged from CGLS-24 behavior"
        );
        let _ = fs::remove_dir_all(&root2);
    }

    /// CGLS-24: two concurrent `check_command` invocations against distinct
    /// scratch roots must see distinct `CARGO_TARGET_DIR`s in their child
    /// processes — the per-scratch isolation must hold under the actual
    /// witness lane's thread-fanout shape (`servedrv.rs:1832-1834` spawns
    /// one detached thread per gate push, deliberately un-slotted).
    #[test]
    fn concurrent_command_checks_get_distinct_cargo_target_dirs() {
        let root_a = scratch("target-dir-conc-a");
        let root_b = scratch("target-dir-conc-b");
        let manifest = r#"
version: 1
checks:
  - id: target-dir-conc
    kind: command
    read_only: true
    command: ["bash", "-lc", "printf '%s' \"$CARGO_TARGET_DIR\" > td.out"]
    cache: none
"#;
        fs::write(root_a.join(MANIFEST_NAME), manifest).unwrap();
        fs::write(root_b.join(MANIFEST_NAME), manifest).unwrap();
        let (a, b) = std::thread::scope(|s| {
            let h_a = s.spawn(|| run_profile(&root_a, "dev", None).unwrap());
            let h_b = s.spawn(|| run_profile(&root_b, "dev", None).unwrap());
            (h_a.join().unwrap(), h_b.join().unwrap())
        });
        assert_eq!(a.tree, TreeState::Green);
        assert_eq!(b.tree, TreeState::Green);
        // Canonicalized for the macOS /var → /private/var symlink (see
        // command_check_isolates_cargo_target_dir_per_scratch_root).
        assert_eq!(
            fs::read_to_string(root_a.join("td.out")).unwrap(),
            fs::canonicalize(&root_a)
                .unwrap()
                .join(".cargoless-target")
                .to_string_lossy(),
        );
        assert_eq!(
            fs::read_to_string(root_b.join("td.out")).unwrap(),
            fs::canonicalize(&root_b)
                .unwrap()
                .join(".cargoless-target")
                .to_string_lossy(),
        );
        let _ = fs::remove_dir_all(root_a);
        let _ = fs::remove_dir_all(root_b);
    }

    /// Manifest emitting `<id>` enter/exit markers around a sleep, so a test
    /// can prove NON-OVERLAP from the trace rather than infer it from timing.
    /// Interleaved (`a+ b+ a- b-`) ⇒ concurrent; nested-free (`a+ a- b+ b-`)
    /// ⇒ serialized. `>>` on one line per marker keeps appends atomic enough
    /// for this size under PIPE_BUF.
    fn overlap_manifest(ids: &[&str]) -> String {
        let mut m = String::from("version: 1\nchecks:\n");
        for id in ids {
            m.push_str(&format!(
                r#"  - id: {id}
    kind: command
    read_only: true
    command: ["bash", "-lc", "echo {id}+ >> trace.out; sleep 0.4; echo {id}- >> trace.out"]
    cache: none
"#
            ));
        }
        m
    }

    fn trace_of(root: &Path) -> Vec<String> {
        fs::read_to_string(root.join("trace.out"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// True iff no two checks' [enter, exit] windows interleave.
    fn trace_is_serialized(trace: &[String]) -> bool {
        let mut open: Option<&str> = None;
        for ev in trace {
            let (id, entering) = match ev.strip_suffix('+') {
                Some(id) => (id, true),
                None => (ev.trim_end_matches('-'), false),
            };
            match (entering, open) {
                // A second check opened while one was still open ⇒ overlap.
                (true, Some(_)) => return false,
                (true, None) => open = Some(id),
                (false, Some(o)) if o == id => open = None,
                (false, _) => return false,
            }
        }
        true
    }

    /// CGLS-24/26 follow-up: with a WARM (shared) target dir, `command` checks
    /// form a serial class — they never overlap, because they would otherwise
    /// pile up on cargo's own `<target>/<layout>/.cargo-lock` and sit in
    /// `locks_lock_inode_wait` burning a thread and a scratch worktree each.
    #[test]
    fn warm_target_serializes_command_checks() {
        let root = scratch("warm-serial");
        fs::write(root.join(MANIFEST_NAME), overlap_manifest(&["a", "b", "c"])).unwrap();
        let warm = scratch("warm-serial-target");

        let report = run_dev_with_changes_in(&root, None, Some(&warm)).unwrap();
        assert_eq!(report.tree, TreeState::Green);
        assert_eq!(report.results.len(), 3);

        let trace = trace_of(&root);
        assert_eq!(trace.len(), 6, "3 checks × enter+exit, got {trace:?}");
        assert!(
            trace_is_serialized(&trace),
            "warm dir shared ⇒ command checks must not overlap; trace {trace:?}"
        );

        let _ = fs::remove_dir_all(&warm);
        let _ = fs::remove_dir_all(&root);
    }

    /// The cold path stays byte-identical: `witness_target_dir == None` means
    /// each run owns `<root>/.cargoless-target`, nothing is shared, and the
    /// profile's `max_parallel` fan-out is preserved. Guards against the
    /// serial class leaking onto every non-witness caller — the regression
    /// that would silently serialize the 111-check `dev` governance lane.
    #[test]
    fn cold_target_keeps_command_checks_concurrent() {
        let root = scratch("cold-concurrent");
        fs::write(root.join(MANIFEST_NAME), overlap_manifest(&["a", "b", "c"])).unwrap();

        let report = run_dev_with_changes_in(&root, None, None).unwrap();
        assert_eq!(report.tree, TreeState::Green);

        let trace = trace_of(&root);
        assert_eq!(trace.len(), 6, "3 checks × enter+exit, got {trace:?}");
        assert!(
            !trace_is_serialized(&trace),
            "no warm dir ⇒ default max_parallel fan-out must be preserved; \
             trace {trace:?} shows full serialization"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// The class is scoped to what actually shares the target dir. A non-
    /// `command` check never inherits `CARGO_TARGET_DIR`, so it must keep
    /// running alongside a warm-class compile instead of queueing behind it.
    ///
    /// Asserted structurally, not by wall-clock: the non-command checks write
    /// their own markers INTO the same trace, so "they ran while the warm
    /// check was open" is a property of the interleaving. A timing bound would
    /// be the wrong instrument here — the `test` job is load-flaky, and a
    /// threshold that passes on an idle laptop turns into a phantom red under
    /// a loaded CI runner.
    #[test]
    fn warm_target_does_not_serialize_non_command_checks() {
        let root = scratch("warm-mixed");
        // `slow` is the warm-class compile (sleeps 0.4s). `fast` is a second
        // command check — also warm class, so it must NOT overlap `slow`.
        // The `p*` checks are a non-command kind: not warm class, so they are
        // free to run inside `slow`'s window.
        let mut manifest = overlap_manifest(&["slow", "fast"]);
        for i in 0..8 {
            manifest.push_str(&format!(
                r#"  - id: p{i}
    kind: forbidden_patterns
    inputs:
      - "*.txt"
    patterns:
      - code: never.match
        word: ZZZ_NO_MATCH_ZZZ
        message: unreachable
    cache: none
"#
            ));
        }
        fs::write(root.join(MANIFEST_NAME), manifest).unwrap();
        fs::write(root.join("f.txt"), "nothing\n").unwrap();
        let warm = scratch("warm-mixed-target");

        let report = run_dev_with_changes_in(&root, None, Some(&warm)).unwrap();
        assert_eq!(report.tree, TreeState::Green);
        assert_eq!(report.results.len(), 10, "2 command + 8 pattern checks");

        // The two command checks are the only trace writers, and they are both
        // warm class ⇒ their windows must not interleave.
        let trace = trace_of(&root);
        assert_eq!(
            trace.len(),
            4,
            "2 command checks × enter+exit, got {trace:?}"
        );
        assert!(
            trace_is_serialized(&trace),
            "command checks share the warm dir ⇒ must not overlap; trace {trace:?}"
        );
        // And the pattern checks still completed — they were never admitted
        // into the serial class, so a blocked warm check could not strand them
        // past the profile timeout.
        for i in 0..8 {
            let id = format!("p{i}");
            let r = report
                .results
                .iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| panic!("missing result for {id}"));
            assert_eq!(
                r.tree,
                TreeState::Green,
                "{id} must run concurrently with the warm class, not queue behind it"
            );
        }

        let _ = fs::remove_dir_all(&warm);
        let _ = fs::remove_dir_all(&root);
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

    /// Regression for the warn-soak leak (2026-06-08): a timed-out command
    /// must kill its ENTIRE process tree, not just the immediate child. The
    /// command backgrounds a grandchild that appends to a marker file every
    /// 100ms; after the check times out we record the marker size, wait, and
    /// assert it stopped growing (the grandchild was reaped, not orphaned).
    /// Driven through `run_profile` so it exercises the real spawn path
    /// (process_group + setsid) and `kill_process_tree`.
    #[cfg(unix)]
    #[test]
    fn timed_out_command_kills_the_whole_process_tree() {
        let root = scratch("tree-kill");
        fs::write(root.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        let marker = root.join("grandchild.marker");
        let marker_str = marker.to_string_lossy().into_owned();
        // Outer sh backgrounds a grandchild loop, then sleeps long. The
        // grandchild keeps writing until SIGKILL'd — if only the immediate
        // child (outer sh) were killed, it would survive and keep appending.
        let script = format!(
            "( while true; do echo x >> '{m}'; sleep 0.1; done ) & sleep 600",
            m = marker_str
        );
        fs::write(
            root.join(MANIFEST_NAME),
            format!(
                r#"
version: 1
checks:
  - id: tree-kill
    kind: command
    read_only: true
    timeout_ms: 600
    command: ["sh", "-c", "{script}"]
"#,
                script = script.replace('"', "\\\"")
            ),
        )
        .unwrap();

        let report = run_profile(&root, "dev", Some("tree-kill")).unwrap();
        // It timed out → red with a timeout diagnostic.
        assert_eq!(report.tree, TreeState::Red);
        assert!(
            has_timeout_diagnostic(&report.diagnostics),
            "expected a timeout diagnostic, got: {:?}",
            report.diagnostics
        );
        // Record marker size right after the kill, wait well past the
        // grandchild's 100ms cadence, and confirm it did NOT grow.
        let size_after_kill = fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        thread::sleep(Duration::from_millis(800));
        let size_later = fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            size_after_kill, size_later,
            "grandchild kept writing after timeout — process tree NOT killed \
             (orphan leak): {size_after_kill} -> {size_later}"
        );
    }

    #[cfg(unix)]
    fn exit_with_code(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    #[cfg(unix)]
    fn exit_with_signal(sig: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(sig)
    }

    #[cfg(unix)]
    #[test]
    fn classify_exit_treats_141_as_process_killed_sigpipe() {
        let (code, msg) = classify_exit(exit_with_code(141), "tail content");
        assert_eq!(code, "command.process_killed");
        assert!(msg.contains("SIGPIPE"), "msg: {msg}");
        assert!(msg.contains("141"), "msg: {msg}");
        assert!(msg.contains("not an honest tool verdict"), "msg: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn classify_exit_treats_137_as_process_killed_sigkill() {
        let (code, msg) = classify_exit(exit_with_code(137), "");
        assert_eq!(code, "command.process_killed");
        assert!(msg.contains("SIGKILL"), "msg: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn classify_exit_treats_raw_unix_signal_as_process_killed() {
        let (code, msg) = classify_exit(exit_with_signal(13), "tail");
        assert_eq!(code, "command.process_killed");
        assert!(msg.contains("SIGPIPE"), "msg: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn classify_exit_preserves_command_failed_for_normal_nonzero() {
        let (code, msg) = classify_exit(exit_with_code(1), "honest red");
        assert_eq!(code, "command.failed");
        assert!(msg.contains("honest red"), "msg: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn classify_exit_preserves_command_failed_for_high_nonsignal_code() {
        let (code, _) = classify_exit(exit_with_code(200), "");
        assert_eq!(code, "command.failed");
    }

    // ── stages, fail-fast, per-leg target dirs ──────────────────────────────
    //
    // These three features exist to make an expensive pipeline fail early
    // without lying about what it proved. The tests below are the only thing
    // standing between that and a lane that reports green on work it skipped.

    fn result_for<'a>(report: &'a ProjectCheckReport, id: &str) -> &'a ProjectCheckResult {
        report
            .results
            .iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("no result for {id}; got {:?}", report.results))
    }

    fn has_code(result: &ProjectCheckResult, code: &str) -> bool {
        result
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some(code))
    }

    /// The core promise: an expensive stage never starts once a cheap one has
    /// failed. Without this the whole feature is decoration — the build would
    /// still pay for codegen it is going to throw away.
    #[test]
    fn a_later_stage_does_not_run_after_an_earlier_stage_fails() {
        let root = scratch("stage-halt");
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
profiles:
  dev:
    include: ["dev"]
    timeout_ms: 30000
checks:
  - id: cheap
    kind: command
    read_only: true
    stage: 1
    command: ["bash", "-lc", "exit 1"]
    cache: none
  - id: expensive
    kind: command
    read_only: true
    stage: 2
    command: ["bash", "-lc", "echo ran > expensive.out"]
    cache: none
"#,
        )
        .unwrap();

        let report = run_profile(&root, "dev", None).unwrap();
        assert_eq!(report.tree, TreeState::Red);
        assert!(
            !root.join("expensive.out").exists(),
            "stage 2 must not execute after stage 1 failed"
        );

        // Reported, not silently dropped: a caller that sees fewer results
        // than checks cannot tell a truncated run from a shorter pipeline.
        let skipped = result_for(&report, "expensive");
        assert_eq!(skipped.tree, TreeState::Red);
        assert!(has_code(skipped, SKIPPED_DIAGNOSTIC_CODE));

        let _ = fs::remove_dir_all(root);
    }

    /// Stage order comes from the NUMBER, not from manifest position. Relying
    /// on declaration order would make a harmless reordering of the YAML
    /// silently reorder the pipeline.
    #[test]
    fn stages_run_in_ascending_order_regardless_of_declaration_order() {
        let root = scratch("stage-order");
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
profiles:
  dev:
    include: ["dev"]
    timeout_ms: 30000
checks:
  - id: second
    kind: command
    read_only: true
    stage: 9
    command: ["bash", "-lc", "echo second >> trace.out"]
    cache: none
  - id: first
    kind: command
    read_only: true
    stage: 2
    command: ["bash", "-lc", "echo first >> trace.out"]
    cache: none
"#,
        )
        .unwrap();

        let report = run_profile(&root, "dev", None).unwrap();
        assert_eq!(report.tree, TreeState::Green);
        assert_eq!(trace_of(&root), vec!["first", "second"]);

        let _ = fs::remove_dir_all(root);
    }

    /// An unstaged manifest is one stage and behaves exactly as before. This is
    /// the compatibility guard: every existing repo declares no `stage`, and a
    /// default that accidentally split them would serialize whole profiles.
    #[test]
    fn checks_without_a_stage_all_share_stage_zero() {
        let root = scratch("stage-default");
        fs::write(root.join(MANIFEST_NAME), overlap_manifest(&["a", "b", "c"])).unwrap();

        let report = run_dev_with_changes_in(&root, None, None).unwrap();
        assert_eq!(report.tree, TreeState::Green);
        let trace = trace_of(&root);
        assert_eq!(trace.len(), 6, "3 checks × enter+exit, got {trace:?}");
        assert!(
            !trace_is_serialized(&trace),
            "one implicit stage ⇒ normal fan-out must be preserved; trace {trace:?}"
        );

        let _ = fs::remove_dir_all(root);
    }

    /// Fail-fast kills the in-flight sibling rather than waiting it out, and
    /// says so honestly: the killed check reports SKIPPED, never a red of its
    /// own. Blaming a check we cancelled would send someone debugging a
    /// failure that never happened.
    #[test]
    fn fail_fast_cancels_the_in_flight_sibling_and_does_not_blame_it() {
        let root = scratch("fail-fast-cancel");
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
profiles:
  lane:
    include: ["lane"]
    timeout_ms: 60000
    max_parallel: 4
    fail_fast: true
checks:
  - id: aa-slow
    kind: command
    tier: lane
    read_only: true
    timeout_ms: 50000
    command: ["bash", "-lc", "sleep 30; echo finished > slow.out"]
    cache: none
  - id: bb-fails
    kind: command
    tier: lane
    read_only: true
    timeout_ms: 50000
    command: ["bash", "-lc", "sleep 0.3; exit 1"]
    cache: none
"#,
        )
        .unwrap();

        let started = Instant::now();
        let report = run_profile(&root, "lane", None).unwrap();
        let elapsed = started.elapsed();

        assert_eq!(report.tree, TreeState::Red);
        // The slow leg sleeps 30s. If cancellation did not work we would sit
        // here for all of it, so a generous 20s ceiling still proves the point
        // without being flaky on a loaded CI box.
        assert!(
            elapsed < Duration::from_secs(20),
            "fail_fast must cancel the in-flight sibling; took {elapsed:?}"
        );
        assert!(
            !root.join("slow.out").exists(),
            "the cancelled command must not have run to completion"
        );

        let slow = result_for(&report, "aa-slow");
        assert!(
            has_code(slow, SKIPPED_DIAGNOSTIC_CODE),
            "a cancelled check reports SKIPPED, not a verdict of its own: {:?}",
            slow.diagnostics
        );
        assert!(
            !has_code(slow, "command.timeout"),
            "cancellation is not a timeout — saying so blames the check for a \
             scheduler decision: {:?}",
            slow.diagnostics
        );

        let _ = fs::remove_dir_all(root);
    }

    /// A NON-required red is information, not a verdict. Cancelling a build
    /// over a failing advisory would silently promote it to a gate.
    #[test]
    fn a_non_required_red_never_trips_fail_fast() {
        let root = scratch("fail-fast-advisory");
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
profiles:
  lane:
    include: ["lane"]
    timeout_ms: 60000
    max_parallel: 4
    fail_fast: true
checks:
  - id: aa-advisory
    kind: command
    tier: lane
    read_only: true
    required: false
    timeout_ms: 20000
    command: ["bash", "-lc", "exit 1"]
    cache: none
  - id: bb-real
    kind: command
    tier: lane
    read_only: true
    timeout_ms: 20000
    command: ["bash", "-lc", "sleep 6; echo ran > real.out"]
    cache: none
"#,
        )
        .unwrap();

        let report = run_profile(&root, "lane", None).unwrap();
        assert_eq!(
            report.tree,
            TreeState::Green,
            "a failing advisory must not red the tree"
        );
        // The load-bearing assertion, and the reason `bb-real` sleeps 6s rather
        // than 1s: the advisory exits INSTANTLY, so a `fail_fast` that wrongly
        // counted non-required reds would fire while the required check is
        // still running and kill it mid-sleep — leaving `real.out` unwritten.
        //
        // With a 1s sleep the mutation survived: the required check finished
        // before the wrongful cancel could reach it, so the test passed either
        // way and proved nothing. Caught by the mutation harness reporting
        // "SURVIVED — fail_fast trips on a non-required red", which is exactly
        // the job that harness exists to do.
        assert!(
            root.join("real.out").exists(),
            "the required check must run to COMPLETION — a failing advisory \
             must never cancel in-flight work, or every non-required check \
             silently becomes a gate"
        );

        let _ = fs::remove_dir_all(root);
    }

    /// Without fail-fast, everything still runs. The feature is opt-in so the
    /// 100+ check governance profiles keep reporting a full picture.
    #[test]
    fn without_fail_fast_a_red_does_not_cancel_anything() {
        let root = scratch("no-fail-fast");
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
profiles:
  dev:
    include: ["dev"]
    timeout_ms: 30000
    max_parallel: 4
checks:
  - id: aa-fails
    kind: command
    read_only: true
    timeout_ms: 20000
    command: ["bash", "-lc", "exit 1"]
    cache: none
  - id: bb-runs
    kind: command
    read_only: true
    timeout_ms: 20000
    command: ["bash", "-lc", "sleep 6; echo ran > other.out"]
    cache: none
"#,
        )
        .unwrap();

        let report = run_profile(&root, "dev", None).unwrap();
        assert_eq!(report.tree, TreeState::Red);
        // 6s, not 1s, for the same reason as the advisory test above: the
        // failing check exits instantly, so a regression that made fail_fast
        // the DEFAULT would cancel this one mid-sleep. With a 1s sleep it
        // finishes first and the test passes either way.
        assert!(
            root.join("other.out").exists(),
            "default profiles must keep running every check to completion — \
             fail_fast is opt-in, and a profile that never asked for it must \
             never have work cancelled underneath it"
        );

        let _ = fs::remove_dir_all(root);
    }

    /// The parallelism payoff. Two legs with DIFFERENT `target_key`s compile
    /// into different dirs, so they may overlap even with a warm target —
    /// where previously every command check was one serial class and declaring
    /// them parallel bought exactly nothing.
    #[test]
    fn distinct_target_keys_run_concurrently_under_a_warm_target() {
        let root = scratch("target-key-parallel");
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: a
    kind: command
    read_only: true
    target_key: "native"
    command: ["bash", "-lc", "echo a+ >> trace.out; sleep 0.4; echo a- >> trace.out"]
    cache: none
  - id: b
    kind: command
    read_only: true
    target_key: "wasm"
    command: ["bash", "-lc", "echo b+ >> trace.out; sleep 0.4; echo b- >> trace.out"]
    cache: none
"#,
        )
        .unwrap();
        let warm = scratch("target-key-parallel-target");

        let report = run_dev_with_changes_in(&root, None, Some(&warm)).unwrap();
        assert_eq!(report.tree, TreeState::Green);

        let trace = trace_of(&root);
        assert_eq!(trace.len(), 4, "2 checks × enter+exit, got {trace:?}");
        assert!(
            !trace_is_serialized(&trace),
            "different target_key ⇒ different CARGO_TARGET_DIR ⇒ no contention, \
             so these must overlap; trace {trace:?}"
        );

        let _ = fs::remove_dir_all(&warm);
        let _ = fs::remove_dir_all(&root);
    }

    /// The other half: the SAME key still serializes. Two legs sharing a
    /// target dir really would block each other on cargo's `.cargo-lock`, so
    /// admitting both only parks a thread in the kernel.
    #[test]
    fn a_shared_target_key_still_serializes() {
        let root = scratch("target-key-serial");
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: a
    kind: command
    read_only: true
    target_key: "native"
    command: ["bash", "-lc", "echo a+ >> trace.out; sleep 0.4; echo a- >> trace.out"]
    cache: none
  - id: b
    kind: command
    read_only: true
    target_key: "native"
    command: ["bash", "-lc", "echo b+ >> trace.out; sleep 0.4; echo b- >> trace.out"]
    cache: none
"#,
        )
        .unwrap();
        let warm = scratch("target-key-serial-target");

        let report = run_dev_with_changes_in(&root, None, Some(&warm)).unwrap();
        assert_eq!(report.tree, TreeState::Green);

        let trace = trace_of(&root);
        assert_eq!(trace.len(), 4, "2 checks × enter+exit, got {trace:?}");
        assert!(
            trace_is_serialized(&trace),
            "same target_key ⇒ one dir ⇒ must not overlap; trace {trace:?}"
        );

        let _ = fs::remove_dir_all(&warm);
        let _ = fs::remove_dir_all(&root);
    }

    /// A `target_key` becomes a path component, so a manifest must not be able
    /// to escape the parent directory with it. Sanitised, not trusted.
    #[test]
    fn a_traversing_target_key_cannot_escape_the_parent_dir() {
        let root = scratch("target-key-escape");
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
checks:
  - id: escape
    kind: command
    read_only: true
    target_key: "../../etc"
    command: ["bash", "-lc", "printf '%s' \"$CARGO_TARGET_DIR\" > target-dir.out"]
    cache: none
"#,
        )
        .unwrap();

        let report = run_profile(&root, "dev", None).unwrap();
        assert_eq!(report.tree, TreeState::Green);

        let seen = fs::read_to_string(root.join("target-dir.out")).unwrap();

        // The property is CONTAINMENT, not the absence of dots. `../../etc`
        // sanitises to the leaf `.._.._etc`, which still contains ".." as a
        // substring while being a perfectly ordinary directory name — the dots
        // are no longer path components, which is the whole point.
        //
        // An earlier version of this test asserted `!seen.contains("..")` and
        // failed on correct output. That is the worse kind of wrong test: it
        // reddens on a working sanitiser, so the natural "fix" is to weaken the
        // sanitiser until the test passes.
        let canonical_root = fs::canonicalize(&root).unwrap();
        let resolved = Path::new(&seen);
        assert!(
            resolved.starts_with(&canonical_root),
            "target dir {seen} escaped the run root {}",
            canonical_root.display()
        );
        // No component may be a traversal — this is the check that actually
        // forbids escaping, and it holds regardless of how the leaf is spelled.
        assert!(
            !resolved
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "no path COMPONENT may be `..`, got {seen}"
        );

        let _ = fs::remove_dir_all(root);
    }

    /// A skipped result describes the RUN, not the CHECK, so it must never be
    /// cached. Caching one would keep a check skipped after the failure that
    /// skipped it was fixed.
    #[test]
    fn skipped_results_are_not_cached() {
        let root = scratch("skip-cache");
        fs::write(
            root.join(MANIFEST_NAME),
            r#"
version: 1
profiles:
  dev:
    include: ["dev"]
    timeout_ms: 30000
checks:
  - id: gate
    kind: command
    read_only: true
    stage: 1
    timeout_ms: 20000
    command: ["bash", "-lc", "test -f pass.marker"]
  - id: later
    kind: command
    read_only: true
    stage: 2
    timeout_ms: 20000
    command: ["bash", "-lc", "echo ran >> later.out"]
"#,
        )
        .unwrap();

        // First run: the gate fails, so `later` is skipped and never executes.
        let first = run_profile(&root, "dev", None).unwrap();
        assert_eq!(first.tree, TreeState::Red);
        assert!(has_code(
            result_for(&first, "later"),
            SKIPPED_DIAGNOSTIC_CODE
        ));
        assert!(!root.join("later.out").exists());

        // Fix the gate. If the skip had been cached, `later` would replay as
        // skipped and never run — the bug this guards.
        fs::write(root.join("pass.marker"), "").unwrap();
        let second = run_profile(&root, "dev", None).unwrap();
        assert_eq!(second.tree, TreeState::Green);
        assert!(
            root.join("later.out").exists(),
            "a skipped check must run once the earlier stage passes"
        );

        let _ = fs::remove_dir_all(root);
    }
}
