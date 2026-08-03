//! The versioned, semantic result contract shared by every Cargoless surface.
//!
//! This module deliberately does not expose a colour plus optional strings.
//! Facts, conclusions, retry policy, evidence, and causal relationships are
//! distinct types. Human summaries are carried for display only; consumers
//! make decisions exclusively from the tagged enums.

use std::fmt;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "cargoless.outcome/v3";
pub const PROTOCOL_HEADER_VALUE: &str = "outcome-v3";

macro_rules! opaque_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidContractValue> {
                let value = value.into();
                if value.is_empty()
                    || value.len() > 128
                    || !value.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':')
                    })
                {
                    Err(InvalidContractValue(concat!(
                        stringify!($name),
                        " must be 1-128 safe ASCII identifier characters"
                    )))
                } else {
                    Ok(Self(value))
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidContractValue;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidContractValue(&'static str);

impl fmt::Display for InvalidContractValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InvalidContractValue {}

opaque_id!(RequestId);
opaque_id!(AttemptId);
opaque_id!(ExecutionId);
opaque_id!(EvidenceId);
opaque_id!(TraceId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct NonEmptyText(String);

impl NonEmptyText {
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidContractValue> {
        let value = value.into();
        if value.trim().is_empty() {
            Err(InvalidContractValue("text must not be empty"))
        } else {
            Ok(Self(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NonEmptyText {
    type Error = InvalidContractValue;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for NonEmptyText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InputIdentity {
    ContentDigest { sha256: NonEmptyText },
    Unavailable { explanation: NonEmptyText },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Subject {
    Overlay {
        repository: NonEmptyText,
        worktree_key: NonEmptyText,
        base_ref: NonEmptyText,
        base_sha: NonEmptyText,
        overlay_digest: NonEmptyText,
        changed_files_digest: NonEmptyText,
        check_plan_digest: NonEmptyText,
    },
    Batch {
        batch_id: NonEmptyText,
        base_sha: NonEmptyText,
        ordered_member_digest: NonEmptyText,
        check_plan_digest: NonEmptyText,
    },
    LaneCandidate {
        base_sha: NonEmptyText,
        candidate_tree_sha: NonEmptyText,
        ordered_member_digest: NonEmptyText,
    },
    AppBuild {
        instance: NonEmptyText,
        git_ref: NonEmptyText,
        sha: NonEmptyText,
        manifest_digest: NonEmptyText,
    },
    LocalCheck {
        canonical_root: NonEmptyText,
        tree: InputIdentity,
        check_plan_digest: NonEmptyText,
    },
    ArtifactBuild {
        input_hash: NonEmptyText,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Producer {
    pub daemon_build_id: NonEmptyText,
    pub process_id: u32,
    pub process_generation: u64,
    pub pod_uid: Option<NonEmptyText>,
    pub rust_analyzer_generation: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    CausedBy,
    ExecutedBy,
    CoalescedWith,
    RetriedFrom,
    SupersededBy,
    ConcurrentWith,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    pub kind: RelationKind,
    pub attempt_id: Option<AttemptId>,
    pub execution_id: Option<ExecutionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    LocalCheck,
    Watch,
    ArtifactBuild,
    Overlay,
    NativeAnalyzer,
    ProjectCheck,
    Batch,
    BuildLane,
    AppBuild,
    AppProbe,
    AppPromote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Accepted,
    Queued,
    Materializing,
    AnalyzerTransaction,
    PlanningChecks,
    WaitingForExecutionSlot,
    Executing,
    Attributing,
    Composing,
    Publishing,
    Retrying,
    Probing,
    Promoting,
    Draining,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseRecord {
    pub phase: Phase,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticOrigin {
    Rustc,
    RustAnalyzerNative,
    RustAnalyzerFlycheck,
    ProjectCheck,
    SyntheticCheck,
    BuildStep,
    CargolessPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    Blocking,
    Advisory,
    Operational,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DiagnosticLocation {
    Located {
        file: NonEmptyText,
        line: u32,
        column: u32,
    },
    Unlocated {
        explanation: NonEmptyText,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticRecord {
    pub origin: DiagnosticOrigin,
    pub severity: DiagnosticSeverity,
    pub authority: Authority,
    pub location: DiagnosticLocation,
    pub code: Option<NonEmptyText>,
    pub message: NonEmptyText,
    pub fingerprint: NonEmptyText,
}

/// A non-empty diagnostic collection whose invalid empty state cannot be
/// serialized or constructed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NonEmptyDiagnostics {
    pub first: DiagnosticRecord,
    #[serde(default)]
    pub rest: Vec<DiagnosticRecord>,
}

impl NonEmptyDiagnostics {
    pub fn new(first: DiagnosticRecord, rest: Vec<DiagnosticRecord>) -> Self {
        Self { first, rest }
    }

    pub fn len(&self) -> usize {
        1 + self.rest.len()
    }

    /// This collection is non-empty by construction.
    pub const fn is_empty(&self) -> bool {
        false
    }

    pub fn iter(&self) -> impl Iterator<Item = &DiagnosticRecord> {
        std::iter::once(&self.first).chain(self.rest.iter())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    RustAnalyzer,
    Cargo,
    Rustc,
    ProjectCheck,
    BatchCoalescer,
    Overlay,
    Git,
    BuildStep,
    ArtifactHarvester,
    AppChild,
    HealthProbe,
    Lander,
    TelemetryExporter,
    EvidenceStore,
    Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessTermination {
    ExitCode { code: i32 },
    Signal { signal: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "family", rename_all = "snake_case")]
pub enum FailureCause {
    Diagnostics {
        diagnostics: NonEmptyDiagnostics,
    },
    /// A producer explicitly reported one or more blocking diagnostics but
    /// did not provide item-level location/message records. This preserves
    /// current blocking policy without pretending the result is a compiler
    /// diagnostic.
    UnlocatedDiagnosticReport {
        origin: DiagnosticOrigin,
        authority: Authority,
        reported_count: NonZeroU32,
        producer: NonEmptyText,
        raw_report_digest: NonEmptyText,
    },
    Timeout {
        component: Component,
        budget_ms: u64,
    },
    ProcessExit {
        component: Component,
        termination: ProcessTermination,
    },
    SpawnFailure {
        component: Component,
    },
    Configuration {
        component: Component,
        code: NonEmptyText,
    },
    ResourceExhausted {
        component: Component,
        resource: NonEmptyText,
    },
    PolicyViolation {
        policy: NonEmptyText,
    },
    ArtifactFailure {
        component: Component,
        stage: NonEmptyText,
    },
    HealthProbe {
        component: Component,
    },
    InternalContractViolation {
        invariant: NonEmptyText,
    },
}

impl FailureCause {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Diagnostics { .. } => "failure.diagnostics",
            Self::UnlocatedDiagnosticReport { .. } => "failure.diagnostics_unlocated",
            Self::Timeout { .. } => "failure.timeout",
            Self::ProcessExit { .. } => "failure.process_exit",
            Self::SpawnFailure { .. } => "failure.spawn",
            Self::Configuration { .. } => "failure.configuration",
            Self::ResourceExhausted { .. } => "failure.resource_exhausted",
            Self::PolicyViolation { .. } => "failure.policy",
            Self::ArtifactFailure { .. } => "failure.artifact",
            Self::HealthProbe { .. } => "failure.health_probe",
            Self::InternalContractViolation { .. } => "failure.contract",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "family", rename_all = "snake_case")]
pub enum IndeterminateCause {
    /// The analyzer itself entered a repeated internal-error loop. This is a
    /// daemon/analyzer fault, never evidence that the submitted tree failed
    /// to compile.
    AnalyzerPathology {
        component: Component,
        signature: NonEmptyText,
        repeated_events: u64,
    },
    /// The selected analyzer cannot observe the relevant expanded program;
    /// an authoritative compiler witness is required before a code verdict
    /// exists.
    CompilerWitnessRequired {
        component: Component,
        limitation: NonEmptyText,
    },
    DependencyUnavailable {
        component: Component,
    },
    StateChanged {
        component: Component,
    },
    ProcessLost {
        component: Component,
        respawned: bool,
    },
    QueueExpired {
        queue: NonEmptyText,
    },
    BudgetExhausted {
        component: Component,
        budget: NonEmptyText,
    },
    ProtocolViolation {
        invariant: NonEmptyText,
    },
    AttributionUnavailable {
        producer: NonEmptyText,
    },
    InternalContractViolation {
        invariant: NonEmptyText,
    },
}

impl IndeterminateCause {
    pub fn code(&self) -> &'static str {
        match self {
            Self::AnalyzerPathology { .. } => "indeterminate.analyzer_pathology",
            Self::CompilerWitnessRequired { .. } => "indeterminate.compiler_witness_required",
            Self::DependencyUnavailable { .. } => "indeterminate.dependency",
            Self::StateChanged { .. } => "indeterminate.state_changed",
            Self::ProcessLost { .. } => "indeterminate.process_lost",
            Self::QueueExpired { .. } => "indeterminate.queue_expired",
            Self::BudgetExhausted { .. } => "indeterminate.budget_exhausted",
            Self::ProtocolViolation { .. } => "indeterminate.protocol",
            Self::AttributionUnavailable { .. } => "indeterminate.attribution",
            Self::InternalContractViolation { .. } => "indeterminate.contract",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetryDirective {
    Automatic {
        attempt: u32,
        maximum_attempts: u32,
        after_ms: u64,
    },
    NewInputRequired,
    OperatorRequired,
    Never,
}

impl RetryDirective {
    pub fn has_automatic_attempt_remaining(&self) -> bool {
        matches!(
            self,
            Self::Automatic {
                attempt,
                maximum_attempts,
                ..
            } if attempt < maximum_attempts
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathOverlap {
    AllPathsOverlap,
    SomePathsOverlap,
    NoPathsOverlap,
    NotComputable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EvidenceAvailability {
    Durable,
    Unavailable { explanation: NonEmptyText },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub evidence_id: EvidenceId,
    pub sha256: NonEmptyText,
    pub relative_uri: NonEmptyText,
    pub availability: EvidenceAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PassBasis {
    DiagnosticsClear {
        origin: DiagnosticOrigin,
    },
    ChecksPassed {
        requested_check_ids: Vec<NonEmptyText>,
        executed_check_ids: Vec<NonEmptyText>,
    },
    BuildProducedArtifact {
        artifact_digest: NonEmptyText,
    },
    PolicySatisfied {
        policy: NonEmptyText,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Conclusion {
    Pending {
        phase: Phase,
        retry: Option<RetryDirective>,
        summary: NonEmptyText,
    },
    Passed {
        basis: PassBasis,
        evidence: EvidenceRef,
        summary: NonEmptyText,
    },
    Failed {
        cause: FailureCause,
        path_overlap: PathOverlap,
        evidence: EvidenceRef,
        summary: NonEmptyText,
    },
    Indeterminate {
        cause: IndeterminateCause,
        retry: RetryDirective,
        evidence: EvidenceRef,
        summary: NonEmptyText,
    },
    Rejected {
        cause: IndeterminateCause,
        retry: RetryDirective,
        evidence: EvidenceRef,
        summary: NonEmptyText,
    },
    Cancelled {
        cause: IndeterminateCause,
        evidence: EvidenceRef,
        summary: NonEmptyText,
    },
    Superseded {
        successor_attempt_id: AttemptId,
        evidence: EvidenceRef,
        summary: NonEmptyText,
    },
}

impl Conclusion {
    pub fn semantic_code(&self) -> &'static str {
        match self {
            Self::Pending { .. } => "pending",
            Self::Passed { .. } => "passed",
            Self::Failed { cause, .. } => cause.code(),
            Self::Indeterminate { cause, .. } | Self::Rejected { cause, .. } => cause.code(),
            Self::Cancelled { .. } => "cancelled",
            Self::Superseded { .. } => "superseded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeEnvelope {
    pub schema: NonEmptyText,
    pub request_id: RequestId,
    pub attempt_id: AttemptId,
    pub execution_id: Option<ExecutionId>,
    pub trace_id: TraceId,
    pub surface: Surface,
    pub subject: Subject,
    pub producer: Producer,
    #[serde(default)]
    pub relations: Vec<Relation>,
    pub timeline: Vec<PhaseRecord>,
    pub conclusion: Conclusion,
    /// The required external reaction, derived by the contract's sole
    /// conclusion-to-reaction mapping. Consumers must use this field instead
    /// of reconstructing state from prose or partial cause data.
    pub reaction: ReactionDecision,
}

impl OutcomeEnvelope {
    pub fn new(
        request_id: RequestId,
        attempt_id: AttemptId,
        trace_id: TraceId,
        surface: Surface,
        subject: Subject,
        producer: Producer,
        conclusion: Conclusion,
    ) -> Self {
        let reaction = reaction_for(&conclusion);
        Self {
            schema: NonEmptyText::new(SCHEMA).expect("schema constant is non-empty"),
            request_id,
            attempt_id,
            execution_id: None,
            trace_id,
            surface,
            subject,
            producer,
            relations: Vec::new(),
            timeline: Vec::new(),
            conclusion,
            reaction,
        }
    }

    pub fn validate(&self) -> Result<(), InvalidContractValue> {
        if self.schema.as_str() != SCHEMA {
            return Err(InvalidContractValue("unsupported outcome schema"));
        }
        if self.timeline.windows(2).any(|pair| {
            pair[0].started_at_unix_ms > pair[1].started_at_unix_ms
                || pair[0]
                    .finished_at_unix_ms
                    .is_some_and(|end| end < pair[0].started_at_unix_ms)
        }) {
            return Err(InvalidContractValue("outcome timeline is not monotonic"));
        }
        let automatic_retry = match &self.conclusion {
            Conclusion::Pending {
                retry: Some(retry), ..
            }
            | Conclusion::Indeterminate { retry, .. }
            | Conclusion::Rejected { retry, .. } => Some(retry),
            _ => None,
        };
        if let Some(RetryDirective::Automatic {
            attempt,
            maximum_attempts,
            ..
        }) = automatic_retry
        {
            if *attempt == 0 || *maximum_attempts == 0 || attempt > maximum_attempts {
                return Err(InvalidContractValue("invalid automatic retry bounds"));
            }
        }
        if matches!(
            &self.conclusion,
            Conclusion::Indeterminate {
                cause: IndeterminateCause::AnalyzerPathology {
                    repeated_events: 0,
                    ..
                },
                ..
            } | Conclusion::Rejected {
                cause: IndeterminateCause::AnalyzerPathology {
                    repeated_events: 0,
                    ..
                },
                ..
            }
        ) {
            return Err(InvalidContractValue(
                "an analyzer pathology requires at least one observed event",
            ));
        }
        if self.reaction != reaction_for(&self.conclusion) {
            return Err(InvalidContractValue(
                "outcome reaction does not match its conclusion",
            ));
        }
        if matches!(
            &self.conclusion,
            Conclusion::Passed {
                evidence: EvidenceRef {
                    availability: EvidenceAvailability::Unavailable { .. },
                    ..
                },
                ..
            }
        ) {
            return Err(InvalidContractValue(
                "a passed outcome requires durable evidence",
            ));
        }
        Ok(())
    }

    /// Replace the conclusion and atomically refresh its required reaction.
    /// Keeping this mutation on the envelope prevents producers from
    /// publishing a semantically contradictory conclusion/reaction pair.
    pub fn conclude(&mut self, conclusion: Conclusion) {
        self.reaction = reaction_for(&conclusion);
        self.conclusion = conclusion;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Pending,
    Success,
    Failure,
    Error,
    NoUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionDecision {
    pub state: CheckState,
    pub code: NonEmptyText,
    pub summary: NonEmptyText,
}

/// The sole conclusion → required-check mapping. Forge-specific adapters may
/// truncate the display summary but must not reinterpret the state or code.
pub fn reaction_for(conclusion: &Conclusion) -> ReactionDecision {
    let (state, code, summary) = match conclusion {
        Conclusion::Pending { summary, .. } => (CheckState::Pending, "pending", summary.clone()),
        Conclusion::Passed { summary, .. } => (CheckState::Success, "passed", summary.clone()),
        Conclusion::Failed { cause, summary, .. } => {
            (CheckState::Failure, cause.code(), summary.clone())
        }
        Conclusion::Indeterminate {
            cause,
            retry,
            summary,
            ..
        }
        | Conclusion::Rejected {
            cause,
            retry,
            summary,
            ..
        } => {
            let state = if retry.has_automatic_attempt_remaining() {
                CheckState::Pending
            } else {
                CheckState::Error
            };
            (state, cause.code(), summary.clone())
        }
        Conclusion::Cancelled { summary, .. } => (CheckState::Error, "cancelled", summary.clone()),
        Conclusion::Superseded { summary, .. } => {
            (CheckState::NoUpdate, "superseded", summary.clone())
        }
    };
    ReactionDecision {
        state,
        code: NonEmptyText::new(code).expect("reaction codes are non-empty"),
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> NonEmptyText {
        NonEmptyText::new(value).unwrap()
    }

    fn evidence() -> EvidenceRef {
        EvidenceRef {
            evidence_id: EvidenceId::new("ev-1").unwrap(),
            sha256: text("abc123"),
            relative_uri: text("/v3/attempts/attempt-1/evidence"),
            availability: EvidenceAvailability::Durable,
        }
    }

    #[test]
    fn empty_semantic_values_are_rejected_on_the_wire() {
        assert!(RequestId::new(" ").is_err());
        assert!(NonEmptyText::new("").is_err());
        assert!(serde_json::from_str::<RequestId>("\"\"").is_err());
    }

    #[test]
    fn diagnostic_failure_cannot_be_empty() {
        let first = DiagnosticRecord {
            origin: DiagnosticOrigin::Rustc,
            severity: DiagnosticSeverity::Error,
            authority: Authority::Blocking,
            location: DiagnosticLocation::Located {
                file: text("src/lib.rs"),
                line: 7,
                column: 3,
            },
            code: Some(text("E0382")),
            message: text("use of moved value"),
            fingerprint: text("rustc|E0382|src/lib.rs|use of moved value"),
        };
        let conclusion = Conclusion::Failed {
            cause: FailureCause::Diagnostics {
                diagnostics: NonEmptyDiagnostics::new(first, Vec::new()),
            },
            path_overlap: PathOverlap::AllPathsOverlap,
            evidence: evidence(),
            summary: text("rustc emitted E0382 at src/lib.rs:7:3"),
        };
        let wire = serde_json::to_string(&conclusion).unwrap();
        let decoded: Conclusion = serde_json::from_str(&wire).unwrap();
        assert_eq!(decoded, conclusion);
        assert_eq!(reaction_for(&decoded).state, CheckState::Failure);
    }

    #[test]
    fn locationless_block_is_explicit_and_remains_a_failure() {
        let conclusion = Conclusion::Failed {
            cause: FailureCause::UnlocatedDiagnosticReport {
                origin: DiagnosticOrigin::ProjectCheck,
                authority: Authority::Blocking,
                reported_count: NonZeroU32::new(1).unwrap(),
                producer: text("wasm-compiler-witness"),
                raw_report_digest: text("sha256:report"),
            },
            path_overlap: PathOverlap::NotComputable,
            evidence: evidence(),
            summary: text("one blocking diagnostic was reported; location unavailable"),
        };
        let reaction = reaction_for(&conclusion);
        assert_eq!(reaction.state, CheckState::Failure);
        assert_eq!(reaction.code.as_str(), "failure.diagnostics_unlocated");
    }

    #[test]
    fn analyzer_pathology_and_compiler_escalation_have_distinct_reactions() {
        let pathology = Conclusion::Indeterminate {
            cause: IndeterminateCause::AnalyzerPathology {
                component: Component::RustAnalyzer,
                signature: text("inference-desugared-expr"),
                repeated_events: 42_000,
            },
            retry: RetryDirective::OperatorRequired,
            evidence: evidence(),
            summary: text("rust-analyzer is flooding an internal error"),
        };
        let pathology_reaction = reaction_for(&pathology);
        assert_eq!(pathology_reaction.state, CheckState::Error);
        assert_eq!(
            pathology_reaction.code.as_str(),
            "indeterminate.analyzer_pathology"
        );

        let witness = Conclusion::Indeterminate {
            cause: IndeterminateCause::CompilerWitnessRequired {
                component: Component::RustAnalyzer,
                limitation: text("proc-macro expansion is not authoritative"),
            },
            retry: RetryDirective::NewInputRequired,
            evidence: evidence(),
            summary: text("an authoritative compiler witness is required"),
        };
        let witness_reaction = reaction_for(&witness);
        assert_eq!(witness_reaction.state, CheckState::Error);
        assert_eq!(
            witness_reaction.code.as_str(),
            "indeterminate.compiler_witness_required"
        );
    }

    #[test]
    fn retryable_indeterminate_stays_pending_then_errors() {
        let make = |attempt| Conclusion::Indeterminate {
            cause: IndeterminateCause::ProcessLost {
                component: Component::RustAnalyzer,
                respawned: true,
            },
            retry: RetryDirective::Automatic {
                attempt,
                maximum_attempts: 2,
                after_ms: 1_000,
            },
            evidence: evidence(),
            summary: text("rust-analyzer restarted during evaluation"),
        };
        assert_eq!(reaction_for(&make(1)).state, CheckState::Pending);
        assert_eq!(reaction_for(&make(2)).state, CheckState::Error);
    }

    #[test]
    fn envelope_rejects_invalid_retry_bounds_and_non_monotonic_timeline() {
        let producer = Producer {
            daemon_build_id: text("build"),
            process_id: 1,
            process_generation: 1,
            pod_uid: None,
            rust_analyzer_generation: None,
        };
        let subject = Subject::LocalCheck {
            canonical_root: text("/repo"),
            tree: InputIdentity::ContentDigest {
                sha256: text("tree"),
            },
            check_plan_digest: text("plan"),
        };
        let mut envelope = OutcomeEnvelope::new(
            RequestId::new("request").unwrap(),
            AttemptId::new("attempt").unwrap(),
            TraceId::new("trace").unwrap(),
            Surface::LocalCheck,
            subject,
            producer,
            Conclusion::Pending {
                phase: Phase::Retrying,
                retry: Some(RetryDirective::Automatic {
                    attempt: 2,
                    maximum_attempts: 1,
                    after_ms: 10,
                }),
                summary: text("retrying"),
            },
        );
        assert!(envelope.validate().is_err());
        envelope.conclude(Conclusion::Pending {
            phase: Phase::Executing,
            retry: None,
            summary: text("executing"),
        });
        envelope.timeline = vec![
            PhaseRecord {
                phase: Phase::Queued,
                started_at_unix_ms: 20,
                finished_at_unix_ms: Some(30),
            },
            PhaseRecord {
                phase: Phase::Executing,
                started_at_unix_ms: 10,
                finished_at_unix_ms: None,
            },
        ];
        assert!(envelope.validate().is_err());
    }

    #[test]
    fn passed_outcome_cannot_claim_unavailable_evidence() {
        let mut missing = evidence();
        missing.availability = EvidenceAvailability::Unavailable {
            explanation: text("disk full"),
        };
        let envelope = OutcomeEnvelope::new(
            RequestId::new("request").unwrap(),
            AttemptId::new("attempt").unwrap(),
            TraceId::new("trace").unwrap(),
            Surface::LocalCheck,
            Subject::LocalCheck {
                canonical_root: text("/repo"),
                tree: InputIdentity::ContentDigest {
                    sha256: text("tree"),
                },
                check_plan_digest: text("plan"),
            },
            Producer {
                daemon_build_id: text("build"),
                process_id: 1,
                process_generation: 1,
                pod_uid: None,
                rust_analyzer_generation: None,
            },
            Conclusion::Passed {
                basis: PassBasis::DiagnosticsClear {
                    origin: DiagnosticOrigin::Rustc,
                },
                evidence: missing,
                summary: text("passed"),
            },
        );
        assert!(envelope.validate().is_err());
    }
}
