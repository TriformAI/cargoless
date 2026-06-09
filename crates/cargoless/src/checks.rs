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
