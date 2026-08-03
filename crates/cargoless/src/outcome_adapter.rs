//! Semantic outcome adapters for local, non-HTTP command surfaces.
//!
//! A local command still gets exact request/attempt/execution identity and a
//! durable evidence bundle. Human CLI text remains presentation only; exit
//! behavior is derived from the serialized v3 reaction.

use std::fs;
use std::io;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use cargoless_core::evidence::{ArtifactKind, EvidenceBundle, EvidenceClass, EvidenceStore};
use cargoless_core::outcome::{
    Authority, CheckState, Conclusion, DiagnosticLocation, DiagnosticOrigin, DiagnosticRecord,
    DiagnosticSeverity, ExecutionId, FailureCause, InputIdentity, NonEmptyDiagnostics,
    NonEmptyText, OutcomeEnvelope, PassBasis, PathOverlap, Phase, PhaseRecord, Producer, Relation,
    RelationKind, Subject, Surface,
};
use cargoless_core::{CheckResult, Diagnostic, Severity, TreeState, sha256_hex};

use crate::verdict::attempt_context_from_env;

const LOCAL_TREE_CAP_BYTES: u64 = 64 * 1024 * 1024;

fn text(value: impl Into<String>) -> io::Result<NonEmptyText> {
    NonEmptyText::new(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn producer() -> io::Result<Producer> {
    Ok(Producer {
        daemon_build_id: text(cargoless_core::build_id())?,
        process_id: std::process::id(),
        process_generation: 1,
        pod_uid: std::env::var("POD_UID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(text)
            .transpose()?,
        rust_analyzer_generation: None,
    })
}

fn local_tree_identity(root: &Path) -> InputIdentity {
    match compute_local_tree_digest(root) {
        Ok(digest) => InputIdentity::ContentDigest {
            sha256: NonEmptyText::new(digest).expect("sha256 is non-empty"),
        },
        Err(error) => InputIdentity::Unavailable {
            explanation: NonEmptyText::new(format!(
                "exact local tree identity unavailable: {error}"
            ))
            .expect("identity explanation is non-empty"),
        },
    }
}

/// Hash HEAD's tree identity, Git's complete porcelain state, and the content
/// of every path named by that state. The byte cap is explicit: crossing it
/// produces `InputIdentity::Unavailable`, never a misleading partial digest.
fn compute_local_tree_digest(root: &Path) -> io::Result<String> {
    let run_git = |args: &[&str]| -> io::Result<Vec<u8>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "git {} exited {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    };

    let head_tree = run_git(&["rev-parse", "HEAD^{tree}"])?;
    let status = run_git(&[
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignore-submodules=none",
        "--",
        ".",
        ":(exclude).cargoless",
        ":(exclude)**/.cargoless/**",
    ])?;
    let mut canonical = Vec::new();
    append_capped(&mut canonical, b"head-tree\0", LOCAL_TREE_CAP_BYTES)?;
    append_capped(&mut canonical, &head_tree, LOCAL_TREE_CAP_BYTES)?;
    append_capped(&mut canonical, b"status\0", LOCAL_TREE_CAP_BYTES)?;
    append_capped(&mut canonical, &status, LOCAL_TREE_CAP_BYTES)?;

    for (index, raw_entry) in status.split(|byte| *byte == 0).enumerate() {
        if raw_entry.is_empty() {
            continue;
        }
        // A normal porcelain entry starts with two status bytes plus a space.
        // Rename/copy records add a second NUL-delimited path without them.
        let raw_path = if raw_entry.len() >= 3 && raw_entry[2] == b' ' {
            &raw_entry[3..]
        } else if index > 0 {
            raw_entry
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed git porcelain record",
            ));
        };
        let path = PathBuf::from(
            String::from_utf8(raw_path.to_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 git path"))?,
        );
        let absolute = root.join(&path);
        append_capped(&mut canonical, b"path\0", LOCAL_TREE_CAP_BYTES)?;
        append_capped(&mut canonical, raw_path, LOCAL_TREE_CAP_BYTES)?;
        append_capped(&mut canonical, b"\0", LOCAL_TREE_CAP_BYTES)?;
        match fs::symlink_metadata(&absolute) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                append_capped(&mut canonical, b"symlink\0", LOCAL_TREE_CAP_BYTES)?;
                let target = fs::read_link(&absolute)?;
                append_capped(
                    &mut canonical,
                    target.to_string_lossy().as_bytes(),
                    LOCAL_TREE_CAP_BYTES,
                )?;
            }
            Ok(metadata) if metadata.is_file() => {
                let bytes = fs::read(&absolute)?;
                append_capped(&mut canonical, b"file\0", LOCAL_TREE_CAP_BYTES)?;
                append_capped(&mut canonical, &bytes, LOCAL_TREE_CAP_BYTES)?;
            }
            Ok(metadata) if metadata.is_dir() => {
                // Submodules and gitlinks are represented by their path plus
                // current HEAD, not by recursively hashing an arbitrary tree.
                let nested_head = Command::new("git")
                    .arg("-C")
                    .arg(&absolute)
                    .args(["rev-parse", "HEAD"])
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .map(|output| output.stdout)
                    .unwrap_or_else(|| b"directory".to_vec());
                append_capped(&mut canonical, b"directory\0", LOCAL_TREE_CAP_BYTES)?;
                append_capped(&mut canonical, &nested_head, LOCAL_TREE_CAP_BYTES)?;
            }
            Ok(_) => append_capped(&mut canonical, b"special\0", LOCAL_TREE_CAP_BYTES)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                append_capped(&mut canonical, b"deleted\0", LOCAL_TREE_CAP_BYTES)?;
            }
            Err(error) => return Err(error),
        }
        append_capped(&mut canonical, b"\n", LOCAL_TREE_CAP_BYTES)?;
    }
    Ok(sha256_hex(&canonical))
}

fn append_capped(target: &mut Vec<u8>, bytes: &[u8], cap: u64) -> io::Result<()> {
    if (target.len() as u64).saturating_add(bytes.len() as u64) > cap {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("local identity input exceeds {cap} byte cap"),
        ));
    }
    target.extend_from_slice(bytes);
    Ok(())
}

fn diagnostic_record(diagnostic: &Diagnostic) -> io::Result<DiagnosticRecord> {
    let path = diagnostic.file_path.to_string_lossy();
    let origin = match diagnostic.source.as_deref() {
        Some("rustc") => DiagnosticOrigin::Rustc,
        Some(source) if source.contains("rust-analyzer") => DiagnosticOrigin::RustAnalyzerNative,
        Some("cargoless") => DiagnosticOrigin::SyntheticCheck,
        _ => DiagnosticOrigin::ProjectCheck,
    };
    let severity = match diagnostic.severity {
        Severity::Error => DiagnosticSeverity::Error,
        Severity::Warning => DiagnosticSeverity::Warning,
        Severity::Info => DiagnosticSeverity::Information,
        Severity::Hint => DiagnosticSeverity::Hint,
    };
    let location =
        if diagnostic.line > 0 && !path.trim().is_empty() && !path.starts_with("<cargoless-") {
            DiagnosticLocation::Located {
                file: text(path.as_ref())?,
                line: diagnostic.line,
                column: diagnostic.col,
            }
        } else {
            DiagnosticLocation::Unlocated {
                explanation: text("the producer did not retain a source location")?,
            }
        };
    let message = if diagnostic.message.trim().is_empty() {
        "producer emitted an empty diagnostic message"
    } else {
        diagnostic.message.as_str()
    };
    let fingerprint = sha256_hex(
        format!(
            "{origin:?}|{:?}|{}|{}|{}|{}|{}",
            diagnostic.severity,
            path,
            diagnostic.line,
            diagnostic.col,
            diagnostic.code.as_deref().unwrap_or("-"),
            message
        )
        .as_bytes(),
    );
    Ok(DiagnosticRecord {
        origin,
        severity,
        authority: if diagnostic.severity == Severity::Error {
            Authority::Blocking
        } else {
            Authority::Advisory
        },
        location,
        code: diagnostic
            .code
            .as_deref()
            .filter(|code| !code.trim().is_empty())
            .map(text)
            .transpose()?,
        message: text(message)?,
        fingerprint: text(fingerprint)?,
    })
}

/// Persist the terminal local check result and return the exact envelope whose
/// reaction must drive the CLI exit code.
pub fn persist_local_check(
    root: &Path,
    result: &io::Result<CheckResult>,
) -> io::Result<OutcomeEnvelope> {
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let tree = local_tree_identity(&canonical_root);
    let identity_seed = match &tree {
        InputIdentity::ContentDigest { sha256 } => sha256.as_str(),
        InputIdentity::Unavailable { explanation } => explanation.as_str(),
    };
    let context = attempt_context_from_env(identity_seed)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let execution_id = ExecutionId::new(format!(
        "exec.{}",
        &sha256_hex(
            format!(
                "{}:{}:{}",
                context.request_id.as_str(),
                context.attempt_id.as_str(),
                identity_seed
            )
            .as_bytes()
        )[..24]
    ))
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let subject = Subject::LocalCheck {
        canonical_root: text(canonical_root.to_string_lossy())?,
        tree,
        check_plan_digest: text(sha256_hex(
            b"local-check/v3:native-analyzer+required-project-checks",
        ))?,
    };
    let mut bundle = EvidenceBundle::default();
    let diagnostics: &[Diagnostic] = match result {
        Ok(check) => check.diagnostics.as_slice(),
        Err(_) => &[],
    };
    let diagnostic_records = diagnostics
        .iter()
        .map(diagnostic_record)
        .collect::<io::Result<Vec<_>>>()?;
    let diagnostic_bytes = serde_json::to_vec_pretty(&diagnostic_records)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    bundle.push(ArtifactKind::Diagnostics, diagnostic_bytes.clone());
    if let Err(error) = result {
        bundle.push(ArtifactKind::StderrTail, error.to_string().into_bytes());
    }

    let store = EvidenceStore::new(canonical_root.join(".cargoless"));
    let evidence = store.reference_for(&context.attempt_id, &bundle)?;
    let summary;
    let conclusion = match result {
        Ok(check) if check.tree == TreeState::Green => {
            summary = text("local analyzer and required project checks passed")?;
            Conclusion::Passed {
                basis: PassBasis::ChecksPassed {
                    requested_check_ids: Vec::new(),
                    executed_check_ids: Vec::new(),
                },
                evidence,
                summary: summary.clone(),
            }
        }
        Ok(check) => {
            let blocking = check
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.severity == Severity::Error)
                .map(diagnostic_record)
                .collect::<io::Result<Vec<_>>>()?;
            if let Some((first, rest)) = blocking.split_first() {
                summary = text(format!(
                    "local check failed with {} attributable blocking diagnostic{}",
                    blocking.len(),
                    if blocking.len() == 1 { "" } else { "s" }
                ))?;
                Conclusion::Failed {
                    cause: FailureCause::Diagnostics {
                        diagnostics: NonEmptyDiagnostics::new(first.clone(), rest.to_vec()),
                    },
                    path_overlap: PathOverlap::NotComputable,
                    evidence,
                    summary: summary.clone(),
                }
            } else {
                summary = text(
                    "local check is red but no attributable blocking diagnostic was retained",
                )?;
                Conclusion::Failed {
                    cause: FailureCause::UnlocatedDiagnosticReport {
                        origin: DiagnosticOrigin::CargolessPolicy,
                        authority: Authority::Blocking,
                        reported_count: NonZeroU32::new(1).expect("one is non-zero"),
                        producer: text("cargoless local check")?,
                        raw_report_digest: text(sha256_hex(&diagnostic_bytes))?,
                    },
                    path_overlap: PathOverlap::NotComputable,
                    evidence,
                    summary: summary.clone(),
                }
            }
        }
        Err(error) => {
            summary = text(format!("local check could not execute: {error}"))?;
            Conclusion::Rejected {
                cause: cargoless_core::outcome::IndeterminateCause::DependencyUnavailable {
                    component: cargoless_core::outcome::Component::RustAnalyzer,
                },
                retry: cargoless_core::outcome::RetryDirective::Never,
                evidence,
                summary: summary.clone(),
            }
        }
    };
    let now = now_unix_ms();
    let mut outcome = OutcomeEnvelope::new(
        context.request_id,
        context.attempt_id,
        context.trace_id,
        Surface::LocalCheck,
        subject,
        producer()?,
        conclusion,
    );
    outcome.execution_id = Some(execution_id.clone());
    outcome.timeline = vec![
        PhaseRecord {
            phase: Phase::Accepted,
            started_at_unix_ms: now,
            finished_at_unix_ms: Some(now),
        },
        PhaseRecord {
            phase: Phase::AnalyzerTransaction,
            started_at_unix_ms: now,
            finished_at_unix_ms: Some(now),
        },
        PhaseRecord {
            phase: Phase::Terminal,
            started_at_unix_ms: now,
            finished_at_unix_ms: Some(now),
        },
    ];
    outcome.relations.push(Relation {
        kind: RelationKind::ExecutedBy,
        attempt_id: None,
        execution_id: Some(execution_id),
    });
    if let Some(previous) = context.previous_attempt_id {
        outcome.relations.push(Relation {
            kind: RelationKind::RetriedFrom,
            attempt_id: Some(previous),
            execution_id: None,
        });
    }
    outcome
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let class = if outcome.reaction.state == CheckState::Success {
        EvidenceClass::Success
    } else {
        EvidenceClass::Terminal
    };
    store.persist(&outcome, class, &bundle)?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_failure_is_explicit_instead_of_hashing_a_prefix() {
        let mut target = Vec::new();
        append_capped(&mut target, b"abcd", 3).unwrap_err();
        assert!(target.is_empty());
    }
}
