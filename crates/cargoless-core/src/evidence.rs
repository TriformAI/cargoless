//! Bounded, durable evidence bundles for semantic outcomes.
//!
//! Telemetry is an index and exploration surface, not the only copy of the
//! proof. Bundles are written under the daemon state directory using
//! temp-directory + atomic-rename publication. Repeated RA log lines are
//! expected to be aggregated before they reach this module.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cargoless_proto::outcome::{
    AttemptId, Conclusion, EvidenceAvailability, EvidenceId, EvidenceRef, NonEmptyText,
    OutcomeEnvelope, RequestId, Subject, Surface, TraceId,
};
use serde::{Deserialize, Serialize};

use crate::sha256_hex;

pub const DEFAULT_SUCCESS_TTL_SECS: u64 = 24 * 60 * 60;
pub const DEFAULT_TERMINAL_TTL_SECS: u64 = 7 * 24 * 60 * 60;
pub const DEFAULT_MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_STORE_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const ATTEMPT_ADMISSION_SCHEMA: &str = "cargoless.attempt-admission/v1";
static ATTEMPT_ADMISSION_TOKEN: AtomicU64 = AtomicU64::new(0);

/// Complete semantic identity bound to one attempt id before execution.
///
/// This is a daemon-private persistence shape, not a transport contract. Its
/// explicit versioned on-disk wrapper lets a future daemon fail closed rather
/// than guessing if the admission format changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptAdmissionIdentity {
    pub request_id: RequestId,
    pub attempt_id: AttemptId,
    pub trace_id: TraceId,
    pub previous_attempt_id: Option<AttemptId>,
    pub attempt_number: u32,
    pub maximum_attempts: u32,
    pub retry_after_ms: u64,
    pub surface: Surface,
    pub subject: Subject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AttemptAdmissionState {
    Reserved,
    Accepted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AttemptAdmissionDiskV1 {
    schema: String,
    state: AttemptAdmissionState,
    owner_token: String,
    identity: AttemptAdmissionIdentity,
    pending_outcome: Option<OutcomeEnvelope>,
}

/// An existing durable admission. Callers may replay only an exact accepted
/// identity; a reserved record is an incomplete prior submission and must fail
/// closed after a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptAdmissionRecord {
    identity: AttemptAdmissionIdentity,
    pending_outcome: Option<OutcomeEnvelope>,
    accepted: bool,
}

impl AttemptAdmissionRecord {
    pub fn identity(&self) -> &AttemptAdmissionIdentity {
        &self.identity
    }

    pub fn pending_outcome(&self) -> Option<&OutcomeEnvelope> {
        self.pending_outcome.as_ref()
    }

    pub fn is_accepted(&self) -> bool {
        self.accepted
    }
}

/// Ownership token for one newly-created reservation. Rollback and acceptance
/// both re-read and match this token, so a failing caller cannot remove a
/// record created or replaced by another submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptAdmissionReservation {
    attempt_id: AttemptId,
    owner_token: String,
    identity: AttemptAdmissionIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptAdmissionDecision {
    Reserved(AttemptAdmissionReservation),
    Existing(Box<AttemptAdmissionRecord>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceClass {
    Success,
    Terminal,
}

impl EvidenceClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Terminal => "terminal",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "success" => Some(Self::Success),
            "terminal" => Some(Self::Terminal),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EvidencePolicy {
    pub success_ttl_secs: u64,
    pub terminal_ttl_secs: u64,
    pub max_bundle_bytes: u64,
    pub max_store_bytes: u64,
}

impl Default for EvidencePolicy {
    fn default() -> Self {
        Self {
            success_ttl_secs: DEFAULT_SUCCESS_TTL_SECS,
            terminal_ttl_secs: DEFAULT_TERMINAL_TTL_SECS,
            max_bundle_bytes: DEFAULT_MAX_BUNDLE_BYTES,
            max_store_bytes: DEFAULT_MAX_STORE_BYTES,
        }
    }
}

impl EvidencePolicy {
    /// Resolve deployment overrides without allowing zero/invalid values to
    /// disable retention accidentally. Invalid or absent values retain the
    /// documented defaults.
    pub fn from_env() -> Self {
        let defaults = Self::default();
        let read = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(default)
        };
        Self {
            success_ttl_secs: read(
                "CARGOLESS_EVIDENCE_SUCCESS_TTL_SECS",
                defaults.success_ttl_secs,
            ),
            terminal_ttl_secs: read(
                "CARGOLESS_EVIDENCE_TERMINAL_TTL_SECS",
                defaults.terminal_ttl_secs,
            ),
            max_bundle_bytes: read(
                "CARGOLESS_EVIDENCE_MAX_BUNDLE_BYTES",
                defaults.max_bundle_bytes,
            ),
            max_store_bytes: read(
                "CARGOLESS_EVIDENCE_MAX_STORE_BYTES",
                defaults.max_store_bytes,
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    Events,
    Processes,
    RustAnalyzerSummary,
    Diagnostics,
    BatchReport,
    StdoutTail,
    StderrTail,
    Stack(u32),
    ProjectCheckResult(u32),
}

impl ArtifactKind {
    fn filename(self) -> String {
        match self {
            Self::Events => "events.ndjson".into(),
            Self::Processes => "processes.json".into(),
            Self::RustAnalyzerSummary => "ra-summary.json".into(),
            Self::Diagnostics => "diagnostics.json".into(),
            Self::BatchReport => "batch-report.json".into(),
            Self::StdoutTail => "stdout.tail".into(),
            Self::StderrTail => "stderr.tail".into(),
            Self::Stack(sequence) => format!("stack-{sequence:03}.txt"),
            Self::ProjectCheckResult(sequence) => {
                format!("project-check-result-{sequence:03}.json")
            }
        }
    }

    fn is_mandatory(self) -> bool {
        matches!(self, Self::ProjectCheckResult(_))
    }
}

#[derive(Debug, Clone)]
pub struct EvidenceArtifact {
    pub kind: ArtifactKind,
    pub bytes: Vec<u8>,
}

/// One canonical bundle-digest input. `meta.json` serializes the complete
/// inventory with this exact shape so clients can independently recompute the
/// terminal [`EvidenceRef`] identity even when an ordinary artifact was capped
/// from the persisted/fetchable subset.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EvidenceInventoryEntry {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

/// Compute the canonical evidence bundle digest from a complete inventory.
///
/// This is the single algorithm used by both the evidence writer and typed
/// clients: sorted `(name, NUL, len, NUL, sha256, LF)` records hashed with
/// SHA-256. Callers are responsible for rejecting duplicate/incomplete
/// inventories before treating the digest as authoritative.
pub fn canonical_evidence_bundle_digest(entries: &[EvidenceInventoryEntry]) -> String {
    let mut entries = entries.to_vec();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let mut canonical = Vec::new();
    for entry in entries {
        canonical.extend_from_slice(entry.name.as_bytes());
        canonical.push(0);
        canonical.extend_from_slice(entry.bytes.to_string().as_bytes());
        canonical.push(0);
        canonical.extend_from_slice(entry.sha256.as_bytes());
        canonical.push(b'\n');
    }
    sha256_hex(&canonical)
}

#[derive(Debug, Clone, Default)]
pub struct EvidenceBundle {
    artifacts: Vec<EvidenceArtifact>,
}

impl EvidenceBundle {
    pub fn push(&mut self, kind: ArtifactKind, bytes: impl Into<Vec<u8>>) {
        self.artifacts.push(EvidenceArtifact {
            kind,
            bytes: bytes.into(),
        });
    }

    fn inventory(&self) -> Vec<EvidenceInventoryEntry> {
        self.artifacts
            .iter()
            .map(|artifact| EvidenceInventoryEntry {
                name: artifact.kind.filename(),
                bytes: artifact.bytes.len() as u64,
                sha256: sha256_hex(&artifact.bytes),
            })
            .collect()
    }

    fn digest(&self) -> String {
        canonical_evidence_bundle_digest(&self.inventory())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneReport {
    pub removed_success: u64,
    pub removed_terminal: u64,
    pub bytes_after: u64,
}

#[derive(Debug, Clone)]
pub struct EvidenceStore {
    root: PathBuf,
    policy: EvidencePolicy,
}

impl EvidenceStore {
    pub fn new(state_dir: impl AsRef<Path>) -> Self {
        Self::with_policy(state_dir, EvidencePolicy::from_env())
    }

    pub fn with_policy(state_dir: impl AsRef<Path>, policy: EvidencePolicy) -> Self {
        Self {
            root: state_dir.as_ref().join("evidence-v3"),
            policy,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Atomically reserve an attempt id for its complete semantic identity.
    /// Existing records are parsed and validated before being returned; any
    /// corrupt or unreadable record is an error, never an idempotent success.
    pub fn reserve_attempt_admission(
        &self,
        identity: &AttemptAdmissionIdentity,
    ) -> io::Result<AttemptAdmissionDecision> {
        let admissions = self.root.join(".admissions");
        fs::create_dir_all(&admissions)?;
        let path = attempt_admission_path(&admissions, &identity.attempt_id);
        let owner_token = format!(
            "{}.{}.{}",
            std::process::id(),
            now_unix_nanos(),
            ATTEMPT_ADMISSION_TOKEN.fetch_add(1, Ordering::Relaxed)
        );
        let disk = AttemptAdmissionDiskV1 {
            schema: ATTEMPT_ADMISSION_SCHEMA.to_string(),
            state: AttemptAdmissionState::Reserved,
            owner_token: owner_token.clone(),
            identity: identity.clone(),
            pending_outcome: None,
        };
        let bytes = serde_json::to_vec_pretty(&disk)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let tmp = admission_temp_path(&admissions, &identity.attempt_id, &owner_token);
        let write_result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        match fs::hard_link(&tmp, &path) {
            Ok(()) => {
                fs::remove_file(&tmp)?;
                sync_directory(&admissions)?;
                Ok(AttemptAdmissionDecision::Reserved(
                    AttemptAdmissionReservation {
                        attempt_id: identity.attempt_id.clone(),
                        owner_token,
                        identity: identity.clone(),
                    },
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&tmp);
                Ok(AttemptAdmissionDecision::Existing(Box::new(
                    read_attempt_admission_path(&path)?,
                )))
            }
            Err(error) => {
                let _ = fs::remove_file(&tmp);
                Err(error)
            }
        }
    }

    /// Publish the accepted pending outcome before dispatch. A subsequent
    /// exact retry can therefore recover its identity and honest pending state
    /// after a process restart without re-running Git or queue side effects.
    pub fn accept_attempt_admission(
        &self,
        reservation: &AttemptAdmissionReservation,
        pending_outcome: &OutcomeEnvelope,
    ) -> io::Result<()> {
        validate_admission_outcome(&reservation.identity, pending_outcome)?;
        let admissions = self.root.join(".admissions");
        let path = attempt_admission_path(&admissions, &reservation.attempt_id);
        let current = read_attempt_admission_disk(&path)?;
        if current.state != AttemptAdmissionState::Reserved
            || current.owner_token != reservation.owner_token
            || current.identity != reservation.identity
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "attempt admission reservation ownership changed",
            ));
        }
        let accepted = AttemptAdmissionDiskV1 {
            schema: ATTEMPT_ADMISSION_SCHEMA.to_string(),
            state: AttemptAdmissionState::Accepted,
            owner_token: reservation.owner_token.clone(),
            identity: reservation.identity.clone(),
            pending_outcome: Some(pending_outcome.clone()),
        };
        write_attempt_admission_atomic(&admissions, &path, &accepted)
    }

    /// Remove only the admission still owned by `reservation`. This is used
    /// when a submission fails before it can return an accepted ack.
    pub fn rollback_attempt_admission(
        &self,
        reservation: &AttemptAdmissionReservation,
    ) -> io::Result<()> {
        let admissions = self.root.join(".admissions");
        let path = attempt_admission_path(&admissions, &reservation.attempt_id);
        let current = match read_attempt_admission_disk(&path) {
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if current.owner_token != reservation.owner_token
            || current.identity != reservation.identity
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "attempt admission rollback does not own the current record",
            ));
        }
        fs::remove_file(path)?;
        sync_directory(&admissions)
    }

    pub fn read_attempt_admission(
        &self,
        attempt_id: &AttemptId,
    ) -> io::Result<Option<AttemptAdmissionRecord>> {
        let path = attempt_admission_path(&self.root.join(".admissions"), attempt_id);
        match read_attempt_admission_path(&path) {
            Ok(record) => Ok(Some(record)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn reference_for(
        &self,
        attempt_id: &AttemptId,
        bundle: &EvidenceBundle,
    ) -> io::Result<EvidenceRef> {
        let evidence_id = EvidenceId::new(format!("ev.{}", attempt_id.as_str()))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let digest = bundle.digest();
        Ok(EvidenceRef {
            evidence_id,
            sha256: NonEmptyText::new(digest)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?,
            relative_uri: NonEmptyText::new(format!(
                "/v3/attempts/{}/evidence",
                attempt_id.as_str()
            ))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?,
            availability: EvidenceAvailability::Durable,
        })
    }

    /// Persist an already-finalized outcome and its detail artifacts.
    ///
    /// Ordinary artifacts that would exceed the per-bundle cap are omitted
    /// deterministically and named in `meta.json`. Project-check result
    /// documents are mandatory: capacity for their complete set is reserved
    /// before any write, and an overflow fails atomically. `outcome.json` is
    /// always retained; if it alone exceeds the cap the call fails explicitly.
    pub fn persist(
        &self,
        outcome: &OutcomeEnvelope,
        class: EvidenceClass,
        bundle: &EvidenceBundle,
    ) -> io::Result<EvidenceRef> {
        outcome
            .validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let expected_ref = self.reference_for(&outcome.attempt_id, bundle)?;
        let actual_ref = evidence_ref(&outcome.conclusion);
        if actual_ref != &expected_ref {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "outcome evidence reference does not match bundle contents",
            ));
        }

        let outcome_bytes = serde_json::to_vec_pretty(outcome)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if outcome_bytes.len() as u64 > self.policy.max_bundle_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "outcome alone exceeds evidence bundle cap",
            ));
        }

        let mut artifacts: Vec<&EvidenceArtifact> = bundle.artifacts.iter().collect();
        artifacts.sort_by_key(|artifact| artifact.kind.filename());
        let mut bundle_artifacts = bundle.inventory();
        bundle_artifacts.sort_by(|left, right| left.name.cmp(&right.name));
        let mut mandatory_bytes = outcome_bytes.len() as u64;
        for artifact in artifacts
            .iter()
            .filter(|artifact| artifact.kind.is_mandatory())
        {
            mandatory_bytes = mandatory_bytes.saturating_add(artifact.bytes.len() as u64);
            if mandatory_bytes > self.policy.max_bundle_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "mandatory evidence artifact {} exceeds evidence bundle cap",
                        artifact.kind.filename()
                    ),
                ));
            }
        }

        fs::create_dir_all(&self.root)?;
        let final_dir = self.root.join(outcome.attempt_id.as_str());
        let tmp_dir = self.root.join(format!(
            ".{}.tmp.{}",
            outcome.attempt_id.as_str(),
            std::process::id()
        ));
        if tmp_dir.exists() {
            fs::remove_dir_all(&tmp_dir)?;
        }
        fs::create_dir(&tmp_dir)?;

        let persist_result = (|| -> io::Result<()> {
            write_synced(&tmp_dir.join("outcome.json"), &outcome_bytes)?;
            let mut bytes_written = outcome_bytes.len() as u64;
            let mut mandatory_bytes_remaining =
                mandatory_bytes.saturating_sub(outcome_bytes.len() as u64);
            let mut omitted = Vec::new();
            let mut written = Vec::new();
            for artifact in artifacts {
                let filename = artifact.kind.filename();
                let size = artifact.bytes.len() as u64;
                if artifact.kind.is_mandatory() {
                    mandatory_bytes_remaining = mandatory_bytes_remaining.saturating_sub(size);
                } else if bytes_written
                    .saturating_add(size)
                    .saturating_add(mandatory_bytes_remaining)
                    > self.policy.max_bundle_bytes
                {
                    omitted.push(filename);
                    continue;
                }
                write_synced(&tmp_dir.join(&filename), &artifact.bytes)?;
                bytes_written += size;
                written.push(EvidenceInventoryEntry {
                    name: filename,
                    bytes: size,
                    sha256: sha256_hex(&artifact.bytes),
                });
            }
            let created_at = now_unix();
            let meta = serde_json::to_vec_pretty(&serde_json::json!({
                "schema": "cargoless.evidence/v3",
                "attempt_id": outcome.attempt_id.as_str(),
                "class": class.as_str(),
                "created_at_unix": created_at,
                "artifact_digest": bundle.digest(),
                "bytes": bytes_written,
                "artifacts": written,
                "bundle_artifacts": bundle_artifacts,
                "omitted_due_to_cap": omitted,
            }))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            write_synced(&tmp_dir.join("meta.json"), &meta)?;

            if final_dir.exists() {
                let previous = self.root.join(format!(
                    ".{}.previous.{}",
                    outcome.attempt_id.as_str(),
                    std::process::id()
                ));
                fs::rename(&final_dir, &previous)?;
                fs::rename(&tmp_dir, &final_dir)?;
                let _ = fs::remove_dir_all(previous);
            } else {
                fs::rename(&tmp_dir, &final_dir)?;
            }
            Ok(())
        })();
        if persist_result.is_err() {
            let _ = fs::remove_dir_all(&tmp_dir);
        }
        persist_result?;
        let _ = self.prune(now_unix());
        Ok(expected_ref)
    }

    pub fn read_outcome(&self, attempt_id: &AttemptId) -> io::Result<Option<OutcomeEnvelope>> {
        match fs::read(self.root.join(attempt_id.as_str()).join("outcome.json")) {
            Ok(bytes) => {
                let outcome: OutcomeEnvelope = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                outcome
                    .validate()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                Ok(Some(outcome))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn read_artifact(
        &self,
        attempt_id: &AttemptId,
        kind: ArtifactKind,
    ) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.root.join(attempt_id.as_str()).join(kind.filename())) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Read a named document from an attempt bundle. The allow-list makes
    /// traversal and accidental exposure of temp/previous directories
    /// unrepresentable at this boundary.
    pub fn read_named(&self, attempt_id: &AttemptId, name: &str) -> io::Result<Option<Vec<u8>>> {
        let allowed = matches!(
            name,
            "meta.json"
                | "outcome.json"
                | "events.ndjson"
                | "processes.json"
                | "ra-summary.json"
                | "diagnostics.json"
                | "batch-report.json"
                | "stdout.tail"
                | "stderr.tail"
        ) || name
            .strip_prefix("stack-")
            .and_then(|rest| rest.strip_suffix(".txt"))
            .is_some_and(|digits| {
                digits.len() == 3 && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
            || name
                .strip_prefix("project-check-result-")
                .and_then(|rest| rest.strip_suffix(".json"))
                .is_some_and(|digits| {
                    digits.len() == 3 && digits.bytes().all(|byte| byte.is_ascii_digit())
                });
        if !allowed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported evidence artifact name",
            ));
        }
        match fs::read(self.root.join(attempt_id.as_str()).join(name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn prune(&self, now_unix: u64) -> io::Result<PruneReport> {
        let mut entries = self.entries()?;
        let mut removed_success = 0;
        let mut removed_terminal = 0;
        for entry in &mut entries {
            let ttl = match entry.class {
                EvidenceClass::Success => self.policy.success_ttl_secs,
                EvidenceClass::Terminal => self.policy.terminal_ttl_secs,
            };
            if now_unix.saturating_sub(entry.created_at) > ttl {
                fs::remove_dir_all(&entry.path)?;
                self.remove_attempt_admission_for_evidence_entry(entry)?;
                entry.removed = true;
                match entry.class {
                    EvidenceClass::Success => removed_success += 1,
                    EvidenceClass::Terminal => removed_terminal += 1,
                }
            }
        }

        let mut bytes_after: u64 = entries
            .iter()
            .filter(|entry| !entry.removed)
            .map(|entry| entry.bytes)
            .sum();
        if bytes_after > self.policy.max_store_bytes {
            entries.sort_by_key(|entry| {
                (
                    entry.class != EvidenceClass::Success,
                    entry.created_at,
                    entry.path.clone(),
                )
            });
            for entry in &mut entries {
                if bytes_after <= self.policy.max_store_bytes {
                    break;
                }
                if entry.removed {
                    continue;
                }
                fs::remove_dir_all(&entry.path)?;
                self.remove_attempt_admission_for_evidence_entry(entry)?;
                entry.removed = true;
                bytes_after = bytes_after.saturating_sub(entry.bytes);
                match entry.class {
                    EvidenceClass::Success => removed_success += 1,
                    EvidenceClass::Terminal => removed_terminal += 1,
                }
            }
        }
        Ok(PruneReport {
            removed_success,
            removed_terminal,
            bytes_after,
        })
    }

    fn remove_attempt_admission_for_evidence_entry(&self, entry: &StoredEntry) -> io::Result<()> {
        let Some(attempt_id) = entry.path.file_name().and_then(|name| name.to_str()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "evidence entry has no UTF-8 attempt id",
            ));
        };
        let admissions = self.root.join(".admissions");
        let path = attempt_admission_path_str(&admissions, attempt_id);
        match fs::remove_file(path) {
            Ok(()) => sync_directory(&admissions),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn entries(&self) -> io::Result<Vec<StoredEntry>> {
        let read_dir = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut result = Vec::new();
        for entry in read_dir {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.')
            {
                continue;
            }
            let meta_bytes = match fs::read(path.join("meta.json")) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let meta: serde_json::Value = match serde_json::from_slice(&meta_bytes) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(class) = meta
                .get("class")
                .and_then(serde_json::Value::as_str)
                .and_then(EvidenceClass::parse)
            else {
                continue;
            };
            let created_at = meta
                .get("created_at_unix")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            result.push(StoredEntry {
                bytes: directory_bytes(&path)?,
                path,
                class,
                created_at,
                removed: false,
            });
        }
        Ok(result)
    }
}

fn evidence_ref(conclusion: &Conclusion) -> &EvidenceRef {
    match conclusion {
        Conclusion::Passed { evidence, .. }
        | Conclusion::Failed { evidence, .. }
        | Conclusion::Indeterminate { evidence, .. }
        | Conclusion::Rejected { evidence, .. }
        | Conclusion::Cancelled { evidence, .. }
        | Conclusion::Superseded { evidence, .. } => evidence,
        Conclusion::Pending { .. } => {
            panic!("pending outcomes cannot be finalized into an evidence bundle")
        }
    }
}

fn attempt_admission_path(root: &Path, attempt_id: &AttemptId) -> PathBuf {
    attempt_admission_path_str(root, attempt_id.as_str())
}

fn attempt_admission_path_str(root: &Path, attempt_id: &str) -> PathBuf {
    root.join(format!("{}.json", sha256_hex(attempt_id.as_bytes())))
}

fn admission_temp_path(root: &Path, attempt_id: &AttemptId, owner_token: &str) -> PathBuf {
    root.join(format!(
        ".{}.tmp.{}",
        sha256_hex(attempt_id.as_str().as_bytes()),
        owner_token
    ))
}

fn read_attempt_admission_disk(path: &Path) -> io::Result<AttemptAdmissionDiskV1> {
    let bytes = fs::read(path)?;
    let disk: AttemptAdmissionDiskV1 = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if disk.schema != ATTEMPT_ADMISSION_SCHEMA {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported attempt admission schema {}", disk.schema),
        ));
    }
    if disk.identity.attempt_number == 0
        || disk.identity.maximum_attempts == 0
        || disk.identity.attempt_number > disk.identity.maximum_attempts
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "attempt admission contains an invalid retry ordinal",
        ));
    }
    match (disk.state, disk.pending_outcome.as_ref()) {
        (AttemptAdmissionState::Reserved, None) => {}
        (AttemptAdmissionState::Accepted, Some(outcome)) => {
            validate_admission_outcome(&disk.identity, outcome)?;
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "attempt admission state and pending outcome disagree",
            ));
        }
    }
    Ok(disk)
}

fn read_attempt_admission_path(path: &Path) -> io::Result<AttemptAdmissionRecord> {
    let disk = read_attempt_admission_disk(path)?;
    Ok(AttemptAdmissionRecord {
        identity: disk.identity,
        pending_outcome: disk.pending_outcome,
        accepted: disk.state == AttemptAdmissionState::Accepted,
    })
}

fn validate_admission_outcome(
    identity: &AttemptAdmissionIdentity,
    outcome: &OutcomeEnvelope,
) -> io::Result<()> {
    outcome
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    if !matches!(&outcome.conclusion, Conclusion::Pending { .. })
        || outcome.request_id != identity.request_id
        || outcome.attempt_id != identity.attempt_id
        || outcome.trace_id != identity.trace_id
        || outcome.surface != identity.surface
        || outcome.subject != identity.subject
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "attempt admission pending outcome does not match its identity",
        ));
    }
    Ok(())
}

fn write_attempt_admission_atomic(
    root: &Path,
    path: &Path,
    disk: &AttemptAdmissionDiskV1,
) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(disk)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let tmp = root.join(format!(
        ".{}.tmp.{}.{}",
        sha256_hex(disk.identity.attempt_id.as_str().as_bytes()),
        std::process::id(),
        ATTEMPT_ADMISSION_TOKEN.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        write_synced(&tmp, &bytes)?;
        fs::rename(&tmp, path)?;
        sync_directory(root)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn directory_bytes(path: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[derive(Debug)]
struct StoredEntry {
    path: PathBuf,
    class: EvidenceClass,
    created_at: u64,
    bytes: u64,
    removed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    use cargoless_proto::outcome::{
        Authority, DiagnosticOrigin, FailureCause, InputIdentity, PathOverlap, Phase, Producer,
        Subject, Surface, TraceId,
    };

    fn text(value: &str) -> NonEmptyText {
        NonEmptyText::new(value).unwrap()
    }

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cargoless-evidence-{tag}-{}", std::process::id()))
    }

    fn failed_outcome(
        store: &EvidenceStore,
        attempt: &str,
        bundle: &EvidenceBundle,
    ) -> OutcomeEnvelope {
        let attempt_id = AttemptId::new(attempt).unwrap();
        let evidence = store.reference_for(&attempt_id, bundle).unwrap();
        OutcomeEnvelope::new(
            cargoless_proto::outcome::RequestId::new(format!("req-{attempt}")).unwrap(),
            attempt_id,
            TraceId::new(format!("trace-{attempt}")).unwrap(),
            Surface::ProjectCheck,
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
                rust_analyzer_generation: Some(2),
            },
            Conclusion::Failed {
                cause: FailureCause::UnlocatedDiagnosticReport {
                    origin: DiagnosticOrigin::ProjectCheck,
                    authority: Authority::Blocking,
                    reported_count: NonZeroU32::new(1).unwrap(),
                    producer: text("witness"),
                    raw_report_digest: text("report"),
                },
                path_overlap: PathOverlap::NotComputable,
                evidence,
                summary: text("one locationless diagnostic"),
            },
        )
    }

    #[test]
    fn persists_and_reopens_an_exact_attempt() {
        let root = scratch("persist");
        let _ = fs::remove_dir_all(&root);
        let store = EvidenceStore::new(&root);
        let mut bundle = EvidenceBundle::default();
        bundle.push(
            ArtifactKind::RustAnalyzerSummary,
            b"{\"errors\":80000}".to_vec(),
        );
        bundle.push(ArtifactKind::Stack(1), b"frame_a\nframe_b\n".to_vec());
        let outcome = failed_outcome(&store, "attempt-1", &bundle);
        let reference = store
            .persist(&outcome, EvidenceClass::Terminal, &bundle)
            .unwrap();
        assert_eq!(
            store.read_outcome(&outcome.attempt_id).unwrap().unwrap(),
            outcome
        );
        assert_eq!(
            store
                .read_artifact(&outcome.attempt_id, ArtifactKind::Stack(1))
                .unwrap()
                .unwrap(),
            b"frame_a\nframe_b\n"
        );
        assert_eq!(reference, *evidence_ref(&outcome.conclusion));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn admission_is_atomically_addressed_and_pruned_with_terminal_evidence() {
        let root = scratch("admission-lifecycle");
        let _ = fs::remove_dir_all(&root);
        let store = EvidenceStore::with_policy(
            &root,
            EvidencePolicy {
                success_ttl_secs: 1,
                terminal_ttl_secs: 1,
                ..EvidencePolicy::default()
            },
        );
        let bundle = EvidenceBundle::default();
        let terminal = failed_outcome(&store, "attempt.admission:1", &bundle);
        let identity = AttemptAdmissionIdentity {
            request_id: terminal.request_id.clone(),
            attempt_id: terminal.attempt_id.clone(),
            trace_id: terminal.trace_id.clone(),
            previous_attempt_id: None,
            attempt_number: 1,
            maximum_attempts: 1,
            retry_after_ms: 0,
            surface: terminal.surface,
            subject: terminal.subject.clone(),
        };
        let AttemptAdmissionDecision::Reserved(reservation) =
            store.reserve_attempt_admission(&identity).unwrap()
        else {
            panic!("first admission must own the reservation");
        };
        let AttemptAdmissionDecision::Existing(existing) =
            store.reserve_attempt_admission(&identity).unwrap()
        else {
            panic!("concurrent reservation must observe the complete existing record");
        };
        assert!(!existing.is_accepted());
        let pending = OutcomeEnvelope::new(
            terminal.request_id.clone(),
            terminal.attempt_id.clone(),
            terminal.trace_id.clone(),
            terminal.surface,
            terminal.subject.clone(),
            terminal.producer.clone(),
            Conclusion::Pending {
                phase: Phase::Queued,
                retry: None,
                summary: text("accepted and queued"),
            },
        );
        store
            .accept_attempt_admission(&reservation, &pending)
            .unwrap();
        let admission_files = fs::read_dir(store.root().join(".admissions"))
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .collect::<Vec<_>>();
        assert_eq!(admission_files.len(), 1);
        assert_eq!(
            admission_files[0].file_name().to_string_lossy(),
            format!(
                "{}.json",
                sha256_hex(terminal.attempt_id.as_str().as_bytes())
            ),
            "attempt ids never become path components"
        );

        store
            .persist(&terminal, EvidenceClass::Terminal, &bundle)
            .unwrap();
        store.prune(now_unix().saturating_add(2)).unwrap();
        assert!(
            store
                .read_attempt_admission(&terminal.attempt_id)
                .unwrap()
                .is_none(),
            "terminal evidence and its replay admission share one retention lifecycle"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn persists_sequenced_project_check_results_byte_for_byte() {
        let root = scratch("project-check-results");
        let _ = fs::remove_dir_all(&root);
        let store = EvidenceStore::new(&root);
        let first =
            b"{\n  \"schema\": \"cargoless.check-result/v2\",\n  \"check_id\": \"policy-a\"\n}\n";
        let second = b"{\"schema\":\"cargoless.check-result/v2\",\"check_id\":\"policy-b\"}\n";
        let mut bundle = EvidenceBundle::default();
        bundle.push(ArtifactKind::ProjectCheckResult(1), first.to_vec());
        bundle.push(ArtifactKind::ProjectCheckResult(2), second.to_vec());
        let outcome = failed_outcome(&store, "attempt-project-checks", &bundle);

        store
            .persist(&outcome, EvidenceClass::Terminal, &bundle)
            .unwrap();

        assert_eq!(
            store
                .read_named(&outcome.attempt_id, "project-check-result-001.json")
                .unwrap()
                .unwrap(),
            first
        );
        assert_eq!(
            store
                .read_artifact(&outcome.attempt_id, ArtifactKind::ProjectCheckResult(2))
                .unwrap()
                .unwrap(),
            second
        );
        assert!(
            store
                .read_named(&outcome.attempt_id, "../project-check-result-001.json")
                .is_err(),
            "named evidence reads remain traversal-safe"
        );
        assert!(
            store
                .read_named(&outcome.attempt_id, "project-check-result-01.json")
                .is_err(),
            "result sequence names are exactly three digits"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mandatory_project_check_result_cap_overflow_fails_atomically() {
        let root = scratch("project-check-results-cap");
        let _ = fs::remove_dir_all(&root);
        let first = b"first verified candidate result\n";
        let second = b"second verified candidate result crosses the remaining cap\n";
        let mut bundle = EvidenceBundle::default();
        bundle.push(ArtifactKind::ProjectCheckResult(1), first.to_vec());
        bundle.push(ArtifactKind::ProjectCheckResult(2), second.to_vec());

        let sizing_store = EvidenceStore::new(&root);
        let outcome = failed_outcome(&sizing_store, "attempt-project-check-cap", &bundle);
        let outcome_bytes = serde_json::to_vec_pretty(&outcome).unwrap();
        let store = EvidenceStore::with_policy(
            &root,
            EvidencePolicy {
                max_bundle_bytes: outcome_bytes.len() as u64 + first.len() as u64,
                ..EvidencePolicy::default()
            },
        );

        let error = store
            .persist(&outcome, EvidenceClass::Terminal, &bundle)
            .expect_err("mandatory result 002 must not be silently omitted");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("project-check-result-002.json"),
            "the failure identifies the mandatory result that crossed the cap: {error}"
        );
        assert!(
            !store.root().join(outcome.attempt_id.as_str()).exists(),
            "an incomplete terminal bundle must never become durable"
        );
        assert!(store.read_outcome(&outcome.attempt_id).unwrap().is_none());
        assert!(
            store
                .read_named(&outcome.attempt_id, "meta.json")
                .unwrap()
                .is_none(),
            "no metadata may claim durability for a partially retained mandatory result set"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cap_omits_detail_but_never_the_outcome() {
        let root = scratch("cap");
        let _ = fs::remove_dir_all(&root);
        let store = EvidenceStore::with_policy(
            &root,
            EvidencePolicy {
                max_bundle_bytes: 8_192,
                ..EvidencePolicy::default()
            },
        );
        let mut bundle = EvidenceBundle::default();
        bundle.push(ArtifactKind::StderrTail, vec![b'x'; 16_384]);
        let outcome = failed_outcome(&store, "attempt-cap", &bundle);
        store
            .persist(&outcome, EvidenceClass::Terminal, &bundle)
            .unwrap();
        assert!(store.read_outcome(&outcome.attempt_id).unwrap().is_some());
        assert!(
            store
                .read_artifact(&outcome.attempt_id, ArtifactKind::StderrTail)
                .unwrap()
                .is_none()
        );
        let meta = fs::read_to_string(
            store
                .root()
                .join(outcome.attempt_id.as_str())
                .join("meta.json"),
        )
        .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
        assert_eq!(
            meta["bundle_artifacts"][0]["name"], "stderr.tail",
            "the full digest inventory retains capped artifacts"
        );
        assert!(meta["artifacts"].as_array().unwrap().is_empty());
        assert_eq!(meta["omitted_due_to_cap"][0], "stderr.tail");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn global_cap_prunes_success_before_terminal() {
        let root = scratch("priority");
        let _ = fs::remove_dir_all(&root);
        let writer = EvidenceStore::with_policy(
            &root,
            EvidencePolicy {
                max_bundle_bytes: 64 * 1024,
                max_store_bytes: u64::MAX,
                success_ttl_secs: u64::MAX,
                terminal_ttl_secs: u64::MAX,
            },
        );
        let mut bundle = EvidenceBundle::default();
        bundle.push(ArtifactKind::Events, b"event".to_vec());
        let first = failed_outcome(&writer, "success-shaped", &bundle);
        writer
            .persist(&first, EvidenceClass::Success, &bundle)
            .unwrap();
        let second = failed_outcome(&writer, "terminal-shaped", &bundle);
        writer
            .persist(&second, EvidenceClass::Terminal, &bundle)
            .unwrap();
        let store = EvidenceStore::with_policy(
            &root,
            EvidencePolicy {
                max_bundle_bytes: 64 * 1024,
                max_store_bytes: 1,
                success_ttl_secs: u64::MAX,
                terminal_ttl_secs: u64::MAX,
            },
        );
        let report = store.prune(now_unix()).unwrap();
        assert!(report.removed_success >= 1);
        let _ = fs::remove_dir_all(root);
    }
}
