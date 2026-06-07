//! Native batch-gate attribution for agent fleets.
//!
//! This is the diagnostic-carrying sibling of [`crate::corun`]. `corun` owns the
//! minimal green/red cache algorithm over overlay hashes; this module owns the
//! merge-gate shape a product wrapper needs: optimistic combined green,
//! solo-fallback red attribution, interaction-red detection, and
//! indeterminate infra failures.
//!
//! The checker is deliberately a trait. Unit tests can prove the attribution
//! state machine without launching rust-analyzer or running project commands,
//! while the live daemon can later back the trait with pushed overlays plus
//! `project_checks`.

use std::time::Instant;

use cargoless_proto::{Diagnostic, Severity, TreeState};

use crate::corun::CorunPolicy;
use crate::project_checks::ProjectCheckReport;

/// One member of a batch request. `files` is the pushed overlay payload;
/// `changed_files` is the trigger-pruning view of the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchMember {
    pub worktree: String,
    pub files: Vec<(String, String)>,
    pub changed_files: Vec<String>,
}

impl BatchMember {
    #[must_use]
    pub fn new(worktree: impl Into<String>) -> Self {
        Self {
            worktree: worktree.into(),
            files: Vec::new(),
            changed_files: Vec::new(),
        }
    }
}

/// Green/red/indeterminate as reported to a submitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchVerdict {
    Green,
    Red,
    Indeterminate,
}

impl BatchVerdict {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
            Self::Indeterminate => "indeterminate",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "green" => Some(Self::Green),
            "red" => Some(Self::Red),
            "indeterminate" => Some(Self::Indeterminate),
            _ => None,
        }
    }
}

/// How a member verdict was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchProvenance {
    /// The union of the batch checked green. Optimistic and accepted by policy,
    /// but distinct from a solo proof.
    CombinedGreen,
    /// This member checked green by itself during fallback.
    SoloGreen,
    /// This member checked red by itself during fallback.
    SoloRed,
    /// The combined batch failed, but no member failed alone. The batch is not
    /// safe to merge as-is, and Cargoless must not blame a single submitter.
    InteractionRed,
    /// Cargoless could not produce a trustworthy code verdict.
    Indeterminate,
}

impl BatchProvenance {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CombinedGreen => "combined_green",
            Self::SoloGreen => "solo_green",
            Self::SoloRed => "solo_red",
            Self::InteractionRed => "interaction_red",
            Self::Indeterminate => "indeterminate",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "combined_green" => Some(Self::CombinedGreen),
            "solo_green" => Some(Self::SoloGreen),
            "solo_red" => Some(Self::SoloRed),
            "interaction_red" => Some(Self::InteractionRed),
            "indeterminate" => Some(Self::Indeterminate),
            _ => None,
        }
    }
}

/// One submitter-facing result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchMemberResult {
    pub worktree: String,
    pub verdict: BatchVerdict,
    pub provenance: BatchProvenance,
    pub diagnostics: Vec<Diagnostic>,
    pub duration_ms: u128,
}

/// Whole-batch result plus the counters needed for throughput reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchReport {
    pub batch_id: String,
    pub verdict: BatchVerdict,
    pub members: Vec<BatchMemberResult>,
    pub combined_checks: u32,
    pub solo_checks: u32,
    pub duration_ms: u128,
}

/// The execution seam: given the union of N members or one member alone,
/// produce a project-check report or an infra/setup failure string.
pub trait BatchChecker {
    fn check_combined(&self, members: &[BatchMember]) -> Result<ProjectCheckReport, String>;
    fn check_solo(&self, member: &BatchMember) -> Result<ProjectCheckReport, String>;
}

#[derive(Debug, Clone)]
struct SoloOutcome {
    member: BatchMember,
    result: Result<ProjectCheckReport, String>,
}

/// Run one batch using optimistic combined green and solo fallback on red.
#[must_use]
pub fn run_batch(
    batch_id: impl Into<String>,
    members: &[BatchMember],
    checker: &dyn BatchChecker,
    policy: CorunPolicy,
) -> BatchReport {
    let started = Instant::now();
    let batch_id = batch_id.into();
    if members.is_empty() {
        return BatchReport {
            batch_id,
            verdict: BatchVerdict::Green,
            members: Vec::new(),
            combined_checks: 0,
            solo_checks: 0,
            duration_ms: started.elapsed().as_millis(),
        };
    }

    if policy == CorunPolicy::NoCorun || members.len() == 1 {
        let outcomes: Vec<SoloOutcome> = members
            .iter()
            .cloned()
            .map(|member| {
                let result = checker.check_solo(&member);
                SoloOutcome { member, result }
            })
            .collect();
        return report_from_solos(batch_id, outcomes, 0, started);
    }

    match checker.check_combined(members) {
        Ok(report) if report.tree == TreeState::Green => BatchReport {
            batch_id,
            verdict: BatchVerdict::Green,
            members: members
                .iter()
                .map(|member| BatchMemberResult {
                    worktree: member.worktree.clone(),
                    verdict: BatchVerdict::Green,
                    provenance: BatchProvenance::CombinedGreen,
                    diagnostics: Vec::new(),
                    duration_ms: report.duration_ms,
                })
                .collect(),
            combined_checks: 1,
            solo_checks: 0,
            duration_ms: started.elapsed().as_millis(),
        },
        Ok(combined_red) => {
            let outcomes: Vec<SoloOutcome> = members
                .iter()
                .cloned()
                .map(|member| {
                    let result = checker.check_solo(&member);
                    SoloOutcome { member, result }
                })
                .collect();
            let mut report = report_from_solos(batch_id, outcomes, 1, started);
            if report.verdict == BatchVerdict::Green {
                // Combined red + every solo green is an interaction failure.
                report.verdict = BatchVerdict::Red;
                for member in &mut report.members {
                    member.verdict = BatchVerdict::Red;
                    member.provenance = BatchProvenance::InteractionRed;
                    member.diagnostics = combined_red.diagnostics.clone();
                    member.duration_ms = combined_red.duration_ms;
                }
            }
            report
        }
        Err(message) => BatchReport {
            batch_id,
            verdict: BatchVerdict::Indeterminate,
            members: members
                .iter()
                .map(|member| BatchMemberResult {
                    worktree: member.worktree.clone(),
                    verdict: BatchVerdict::Indeterminate,
                    provenance: BatchProvenance::Indeterminate,
                    diagnostics: vec![indeterminate_diagnostic(&member.worktree, &message)],
                    duration_ms: 0,
                })
                .collect(),
            combined_checks: 1,
            solo_checks: 0,
            duration_ms: started.elapsed().as_millis(),
        },
    }
}

fn report_from_solos(
    batch_id: String,
    outcomes: Vec<SoloOutcome>,
    combined_checks: u32,
    started: Instant,
) -> BatchReport {
    let mut any_red = false;
    let mut any_indeterminate = false;
    let members: Vec<BatchMemberResult> = outcomes
        .into_iter()
        .map(|outcome| match outcome.result {
            Ok(report) if report.tree == TreeState::Green => BatchMemberResult {
                worktree: outcome.member.worktree,
                verdict: BatchVerdict::Green,
                provenance: BatchProvenance::SoloGreen,
                diagnostics: Vec::new(),
                duration_ms: report.duration_ms,
            },
            Ok(report) => {
                any_red = true;
                BatchMemberResult {
                    worktree: outcome.member.worktree,
                    verdict: BatchVerdict::Red,
                    provenance: BatchProvenance::SoloRed,
                    diagnostics: report.diagnostics,
                    duration_ms: report.duration_ms,
                }
            }
            Err(message) => {
                any_indeterminate = true;
                BatchMemberResult {
                    diagnostics: vec![indeterminate_diagnostic(&outcome.member.worktree, &message)],
                    worktree: outcome.member.worktree,
                    verdict: BatchVerdict::Indeterminate,
                    provenance: BatchProvenance::Indeterminate,
                    duration_ms: 0,
                }
            }
        })
        .collect();
    BatchReport {
        batch_id,
        verdict: if any_indeterminate {
            BatchVerdict::Indeterminate
        } else if any_red {
            BatchVerdict::Red
        } else {
            BatchVerdict::Green
        },
        solo_checks: members.len() as u32,
        combined_checks,
        members,
        duration_ms: started.elapsed().as_millis(),
    }
}

fn indeterminate_diagnostic(worktree: &str, message: &str) -> Diagnostic {
    Diagnostic {
        file_path: std::path::PathBuf::from(worktree),
        line: 1,
        col: 1,
        severity: Severity::Error,
        code: Some("batch.indeterminate".to_string()),
        message: message.to_string(),
        source: Some("cargoless-batch".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    #[derive(Default)]
    struct MockChecker {
        combined: RefCell<Vec<Result<ProjectCheckReport, String>>>,
        solos: RefCell<BTreeMap<String, Result<ProjectCheckReport, String>>>,
        calls: RefCell<Vec<String>>,
    }

    impl MockChecker {
        fn with_combined(self, report: Result<ProjectCheckReport, String>) -> Self {
            self.combined.borrow_mut().push(report);
            self
        }

        fn solo(self, worktree: &str, report: Result<ProjectCheckReport, String>) -> Self {
            self.solos.borrow_mut().insert(worktree.to_string(), report);
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl BatchChecker for MockChecker {
        fn check_combined(&self, members: &[BatchMember]) -> Result<ProjectCheckReport, String> {
            let names = members
                .iter()
                .map(|m| m.worktree.as_str())
                .collect::<Vec<_>>()
                .join("+");
            self.calls.borrow_mut().push(format!("combined:{names}"));
            self.combined
                .borrow_mut()
                .pop()
                .unwrap_or_else(|| Ok(report(TreeState::Green, "combined")))
        }

        fn check_solo(&self, member: &BatchMember) -> Result<ProjectCheckReport, String> {
            self.calls
                .borrow_mut()
                .push(format!("solo:{}", member.worktree));
            self.solos
                .borrow()
                .get(&member.worktree)
                .cloned()
                .unwrap_or_else(|| Ok(report(TreeState::Green, &member.worktree)))
        }
    }

    fn members(names: &[&str]) -> Vec<BatchMember> {
        names.iter().map(|name| BatchMember::new(*name)).collect()
    }

    fn report(tree: TreeState, label: &str) -> ProjectCheckReport {
        ProjectCheckReport {
            tree,
            diagnostics: if tree == TreeState::Red {
                vec![diag(label)]
            } else {
                Vec::new()
            },
            results: Vec::new(),
            skipped: Vec::new(),
            duration_ms: 7,
        }
    }

    fn diag(label: &str) -> Diagnostic {
        Diagnostic {
            file_path: std::path::PathBuf::from(format!("{label}.rs")),
            line: 12,
            col: 3,
            severity: Severity::Error,
            code: Some("E0000".to_string()),
            message: format!("{label} failed"),
            source: Some("rustc".to_string()),
        }
    }

    #[test]
    fn combined_green_returns_green_for_all_with_optimistic_provenance() {
        let checker = MockChecker::default().with_combined(Ok(report(TreeState::Green, "all")));
        let out = run_batch(
            "b1",
            &members(&["a", "b", "c"]),
            &checker,
            CorunPolicy::Corun,
        );

        assert_eq!(out.verdict, BatchVerdict::Green);
        assert_eq!(out.combined_checks, 1);
        assert_eq!(out.solo_checks, 0);
        assert!(out
            .members
            .iter()
            .all(|m| m.verdict == BatchVerdict::Green
                && m.provenance == BatchProvenance::CombinedGreen));
        assert_eq!(checker.calls(), vec!["combined:a+b+c"]);
    }

    #[test]
    fn run_batch_preserves_member_order_for_combined_green_and_fallback() {
        let checker = MockChecker::default().with_combined(Ok(report(TreeState::Green, "all")));
        let out = run_batch(
            "order-green",
            &members(&["first", "second", "third"]),
            &checker,
            CorunPolicy::Corun,
        );
        assert_eq!(
            out.members
                .iter()
                .map(|member| member.worktree.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );

        let checker = MockChecker::default()
            .with_combined(Ok(report(TreeState::Red, "combined")))
            .solo("second", Ok(report(TreeState::Red, "second")));
        let out = run_batch(
            "order-fallback",
            &members(&["first", "second", "third"]),
            &checker,
            CorunPolicy::Corun,
        );
        assert_eq!(
            out.members
                .iter()
                .map(|member| (member.worktree.as_str(), member.provenance))
                .collect::<Vec<_>>(),
            vec![
                ("first", BatchProvenance::SoloGreen),
                ("second", BatchProvenance::SoloRed),
                ("third", BatchProvenance::SoloGreen),
            ]
        );
    }

    #[test]
    fn forty_member_fast_path_is_one_combined_check() {
        let checker = MockChecker::default().with_combined(Ok(report(TreeState::Green, "all")));
        let members: Vec<BatchMember> = (0..40)
            .map(|idx| BatchMember::new(format!("agent-{idx:02}")))
            .collect();

        let out = run_batch("b40", &members, &checker, CorunPolicy::Corun);

        assert_eq!(out.verdict, BatchVerdict::Green);
        assert_eq!(out.members.len(), 40);
        assert_eq!(out.combined_checks, 1);
        assert_eq!(out.solo_checks, 0);
        assert!(out
            .members
            .iter()
            .all(|m| m.verdict == BatchVerdict::Green
                && m.provenance == BatchProvenance::CombinedGreen));
        assert_eq!(checker.calls().len(), 1);
    }

    #[test]
    fn combined_red_falls_back_to_solo_and_attributes_one_culprit() {
        let checker = MockChecker::default()
            .with_combined(Ok(report(TreeState::Red, "combined")))
            .solo("b", Ok(report(TreeState::Red, "b")));
        let out = run_batch(
            "b1",
            &members(&["a", "b", "c"]),
            &checker,
            CorunPolicy::Corun,
        );

        let by: BTreeMap<_, _> = out
            .members
            .iter()
            .map(|m| (m.worktree.as_str(), (m.verdict, m.provenance)))
            .collect();
        assert_eq!(out.verdict, BatchVerdict::Red);
        assert_eq!(out.combined_checks, 1);
        assert_eq!(out.solo_checks, 3);
        assert_eq!(by["a"], (BatchVerdict::Green, BatchProvenance::SoloGreen));
        assert_eq!(by["b"], (BatchVerdict::Red, BatchProvenance::SoloRed));
        assert_eq!(by["c"], (BatchVerdict::Green, BatchProvenance::SoloGreen));
    }

    #[test]
    fn forty_member_red_fallback_preserves_single_culprit_attribution() {
        let checker = MockChecker::default()
            .with_combined(Ok(report(TreeState::Red, "combined")))
            .solo("agent-17", Ok(report(TreeState::Red, "agent-17")));
        let members: Vec<BatchMember> = (0..40)
            .map(|idx| BatchMember::new(format!("agent-{idx:02}")))
            .collect();

        let out = run_batch("b40-red", &members, &checker, CorunPolicy::Corun);

        assert_eq!(out.verdict, BatchVerdict::Red);
        assert_eq!(out.members.len(), 40);
        assert_eq!(out.combined_checks, 1);
        assert_eq!(out.solo_checks, 40);
        let red_members: Vec<_> = out
            .members
            .iter()
            .filter(|member| member.verdict == BatchVerdict::Red)
            .collect();
        assert_eq!(red_members.len(), 1);
        assert_eq!(red_members[0].worktree, "agent-17");
        assert_eq!(red_members[0].provenance, BatchProvenance::SoloRed);
        assert!(out.members.iter().all(|member| {
            member.worktree == "agent-17"
                || (member.verdict == BatchVerdict::Green
                    && member.provenance == BatchProvenance::SoloGreen)
        }));
    }

    #[test]
    fn combined_red_can_attribute_multiple_culprits() {
        let checker = MockChecker::default()
            .with_combined(Ok(report(TreeState::Red, "combined")))
            .solo("a", Ok(report(TreeState::Red, "a")))
            .solo("c", Ok(report(TreeState::Red, "c")));
        let out = run_batch(
            "b1",
            &members(&["a", "b", "c"]),
            &checker,
            CorunPolicy::Corun,
        );
        let reds: BTreeSet<_> = out
            .members
            .iter()
            .filter(|m| m.verdict == BatchVerdict::Red)
            .map(|m| m.worktree.as_str())
            .collect();
        assert_eq!(reds, BTreeSet::from(["a", "c"]));
    }

    #[test]
    fn combined_red_with_all_solos_green_is_interaction_red() {
        let checker = MockChecker::default().with_combined(Ok(report(TreeState::Red, "combined")));
        let out = run_batch("b1", &members(&["a", "b"]), &checker, CorunPolicy::Corun);

        assert_eq!(out.verdict, BatchVerdict::Red);
        assert!(out.members.iter().all(|m| m.verdict == BatchVerdict::Red
            && m.provenance == BatchProvenance::InteractionRed
            && !m.diagnostics.is_empty()));
    }

    #[test]
    fn no_corun_forces_solo_checks_only() {
        let checker = MockChecker::default().with_combined(Ok(report(TreeState::Green, "unused")));
        let out = run_batch("b1", &members(&["a", "b"]), &checker, CorunPolicy::NoCorun);

        assert_eq!(out.verdict, BatchVerdict::Green);
        assert_eq!(out.combined_checks, 0);
        assert_eq!(out.solo_checks, 2);
        assert!(out
            .members
            .iter()
            .all(|m| m.provenance == BatchProvenance::SoloGreen));
        assert_eq!(checker.calls(), vec!["solo:a", "solo:b"]);
    }

    #[test]
    fn combined_setup_failure_is_indeterminate_for_every_member() {
        let checker = MockChecker::default().with_combined(Err("builder unavailable".to_string()));
        let out = run_batch("b1", &members(&["a", "b"]), &checker, CorunPolicy::Corun);

        assert_eq!(out.verdict, BatchVerdict::Indeterminate);
        assert!(out
            .members
            .iter()
            .all(|m| m.verdict == BatchVerdict::Indeterminate
                && m.provenance == BatchProvenance::Indeterminate));
    }

    #[test]
    fn solo_failure_during_fallback_is_indeterminate_for_that_member() {
        let checker = MockChecker::default()
            .with_combined(Ok(report(TreeState::Red, "combined")))
            .solo("b", Err("solo workspace missing".to_string()));
        let out = run_batch("b1", &members(&["a", "b"]), &checker, CorunPolicy::Corun);

        let b = out.members.iter().find(|m| m.worktree == "b").unwrap();
        assert_eq!(out.verdict, BatchVerdict::Indeterminate);
        assert_eq!(b.verdict, BatchVerdict::Indeterminate);
        assert_eq!(b.provenance, BatchProvenance::Indeterminate);
        let a = out.members.iter().find(|m| m.worktree == "a").unwrap();
        assert_eq!(a.verdict, BatchVerdict::Green);
        assert_eq!(a.provenance, BatchProvenance::SoloGreen);
    }
}
