//! Bounded, durable evidence bundles for semantic outcomes.
//!
//! Telemetry is an index and exploration surface, not the only copy of the
//! proof. Bundles are written under the daemon state directory using
//! temp-directory + atomic-rename publication. Repeated RA log lines are
//! expected to be aggregated before they reach this module.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use cargoless_proto::outcome::{
    AttemptId, Conclusion, EvidenceAvailability, EvidenceId, EvidenceRef, NonEmptyText,
    OutcomeEnvelope,
};

use crate::sha256_hex;

pub const DEFAULT_SUCCESS_TTL_SECS: u64 = 24 * 60 * 60;
pub const DEFAULT_TERMINAL_TTL_SECS: u64 = 7 * 24 * 60 * 60;
pub const DEFAULT_MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_STORE_BYTES: u64 = 20 * 1024 * 1024 * 1024;

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
        }
    }
}

#[derive(Debug, Clone)]
pub struct EvidenceArtifact {
    pub kind: ArtifactKind,
    pub bytes: Vec<u8>,
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

    fn digest(&self) -> String {
        let mut entries: Vec<(String, String, usize)> = self
            .artifacts
            .iter()
            .map(|artifact| {
                (
                    artifact.kind.filename(),
                    sha256_hex(&artifact.bytes),
                    artifact.bytes.len(),
                )
            })
            .collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let mut canonical = Vec::new();
        for (name, digest, len) in entries {
            canonical.extend_from_slice(name.as_bytes());
            canonical.push(0);
            canonical.extend_from_slice(len.to_string().as_bytes());
            canonical.push(0);
            canonical.extend_from_slice(digest.as_bytes());
            canonical.push(b'\n');
        }
        sha256_hex(&canonical)
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
    /// Artifacts that would exceed the per-bundle cap are omitted
    /// deterministically and named in `meta.json`. `outcome.json` is always
    /// retained; if it alone exceeds the cap the call fails explicitly.
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
            let mut omitted = Vec::new();
            let mut written = Vec::new();
            let mut artifacts: Vec<&EvidenceArtifact> = bundle.artifacts.iter().collect();
            artifacts.sort_by_key(|artifact| artifact.kind.filename());
            for artifact in artifacts {
                let filename = artifact.kind.filename();
                let size = artifact.bytes.len() as u64;
                if bytes_written.saturating_add(size) > self.policy.max_bundle_bytes {
                    omitted.push(filename);
                    continue;
                }
                write_synced(&tmp_dir.join(&filename), &artifact.bytes)?;
                bytes_written += size;
                written.push(serde_json::json!({
                    "name": filename,
                    "bytes": size,
                    "sha256": sha256_hex(&artifact.bytes),
                }));
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
        Authority, DiagnosticOrigin, FailureCause, InputIdentity, PathOverlap, Producer, Subject,
        Surface, TraceId,
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
        assert!(meta.contains("stderr.tail"));
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
