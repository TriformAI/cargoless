//! `cargoless checks` — inspect and run native project checks.

use std::io::Write as _;
use std::process::ExitCode;

use crate::config::Config;
use crate::ui;

pub fn run(
    cfg: &Config,
    action: Option<&str>,
    id: Option<&str>,
    profile: Option<&str>,
) -> ExitCode {
    match action.unwrap_or("list") {
        "list" => list(cfg),
        "run" => run_checks(cfg, id, profile.unwrap_or("dev")),
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

fn run_checks(cfg: &Config, id: Option<&str>, profile: &str) -> ExitCode {
    match cargoless_core::project_checks::run_profile(&cfg.root, profile, id) {
        Ok(report) => {
            let mut err = std::io::stderr();
            let _ = crate::check::render_diagnostics(&mut err, &cfg.root, &report.diagnostics);
            let _ = err.flush();
            let failed = report
                .results
                .iter()
                .filter(|r| r.required && r.tree == cargoless_core::TreeState::Red)
                .count();
            let cache_hits = report.results.iter().filter(|r| r.cache_hit).count();
            let ran = report.results.len();
            if report.tree == cargoless_core::TreeState::Green {
                ui::ok(format!(
                    "project checks green — {ran} check{} evaluated, {} skipped ({cache_hits} cache hit{}) in {}ms",
                    if ran == 1 { "" } else { "s" },
                    report.skipped.len(),
                    if cache_hits == 1 { "" } else { "s" },
                    report.duration_ms,
                ));
                ExitCode::SUCCESS
            } else {
                ui::error(format!(
                    "project checks red — {failed} required check{} failed out of {ran} ({} skipped) in {}ms",
                    if failed == 1 { "" } else { "s" },
                    report.skipped.len(),
                    report.duration_ms,
                ));
                ExitCode::from(1)
            }
        }
        Err(e) => {
            ui::error(format!("could not run project checks: {e}"));
            ExitCode::from(2)
        }
    }
}
