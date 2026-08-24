//! Validation and canonical identity for cargoless-candidate-snapshot/1.
//!
//! This module is deliberately independent of a checkout. Git-backed callers
//! resolve immutable objects themselves, then pass exact base/candidate entry
//! maps through validate_manifest_against_entry_maps. No ambient working tree
//! is consulted here.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use cargoless_cas::sha256_hex;
use cargoless_proto::candidate_snapshot::{
    CANDIDATE_SNAPSHOT_SCHEMA_V1, CandidateSnapshot, CandidateSnapshotManifest, GitObjectFormat,
    GitTreeRef, OverlayOperation, OverlayPayload, SnapshotEntry,
};
use unicode_normalization::UnicodeNormalization as _;

const MAX_MANIFEST_BYTES: usize = 128 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_OPERATIONS: usize = 65_536;
const MAX_UPSERT_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOTAL_UPSERT_BYTES: usize = 64 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_JSON_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateSnapshotError {
    pub code: &'static str,
    pub message: String,
}

impl CandidateSnapshotError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for CandidateSnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl Error for CandidateSnapshotError {}

type Result<T> = std::result::Result<T, CandidateSnapshotError>;

/// Parse a closed v1 manifest and verify every checkout-independent invariant.
pub fn parse_and_validate_manifest_json(json: &str) -> Result<CandidateSnapshotManifest> {
    if json.len() > MAX_MANIFEST_BYTES {
        return Err(err(
            "candidate_snapshot.limit_exceeded",
            "manifest exceeds the 128 MiB v1 limit",
        ));
    }

    let manifest: CandidateSnapshotManifest = serde_json::from_str(json).map_err(map_json_error)?;
    validate_candidate_snapshot_manifest(&manifest)?;
    Ok(manifest)
}

/// Serialize a validated manifest as compact JSON in the frozen field order.
pub fn canonical_manifest_json(manifest: &CandidateSnapshotManifest) -> Result<String> {
    validate_candidate_snapshot_manifest(manifest)?;
    serde_json::to_string(manifest).map_err(|error| {
        err(
            "candidate_snapshot.json_invalid",
            format!("manifest serialization failed: {error}"),
        )
    })
}

/// Verify all invariants that do not require opening a Git repository.
pub fn validate_candidate_snapshot_manifest(manifest: &CandidateSnapshotManifest) -> Result<()> {
    if manifest.schema != CANDIDATE_SNAPSHOT_SCHEMA_V1 {
        return Err(err(
            "candidate_snapshot.schema_unsupported",
            format!(
                "unsupported candidate snapshot schema {:?}",
                manifest.schema
            ),
        ));
    }

    validate_tree_ref(&manifest.comparison_base, manifest.git_object_format)?;
    validate_candidate_shape(&manifest.candidate, manifest.git_object_format)?;

    let computed_tree = compute_candidate_tree_oid(manifest)?;
    if computed_tree != manifest.candidate.tree_oid() {
        return Err(err(
            "candidate_snapshot.tree_oid_mismatch",
            format!(
                "advertised tree {} differs from computed tree {computed_tree}",
                manifest.candidate.tree_oid()
            ),
        ));
    }

    let computed_snapshot = compute_snapshot_digest(manifest)?;
    if computed_snapshot != manifest.candidate.snapshot_digest() {
        return Err(err(
            "candidate_snapshot.snapshot_digest_mismatch",
            format!(
                "advertised snapshot digest {} differs from computed digest {computed_snapshot}",
                manifest.candidate.snapshot_digest()
            ),
        ));
    }

    validate_digest(&manifest.manifest_digest, "manifest_digest")?;
    let computed_manifest = compute_manifest_digest(manifest)?;
    if computed_manifest != manifest.manifest_digest {
        return Err(err(
            "candidate_snapshot.manifest_digest_mismatch",
            format!(
                "advertised manifest digest {} differs from computed digest {computed_manifest}",
                manifest.manifest_digest
            ),
        ));
    }

    Ok(())
}

/// Verify a manifest against Git-resolved entry maps without walking a
/// filesystem. base_entries is required only for an overlay candidate.
pub fn validate_manifest_against_entry_maps(
    manifest: &CandidateSnapshotManifest,
    base_entries: Option<&BTreeMap<String, SnapshotEntry>>,
    candidate_entries: &BTreeMap<String, SnapshotEntry>,
) -> Result<()> {
    validate_candidate_snapshot_manifest(manifest)?;
    validate_external_map(candidate_entries, manifest.git_object_format)?;

    let advertised = entry_map(manifest.candidate.entries());
    if candidate_entries != &advertised {
        return Err(err(
            "candidate_snapshot.entries_mismatch",
            "resolved candidate entries differ from the complete advertised entries",
        ));
    }

    let CandidateSnapshot::Overlay { operations, .. } = &manifest.candidate else {
        return Ok(());
    };

    let base_entries = base_entries.ok_or_else(|| {
        err(
            "candidate_snapshot.base_commit_missing",
            "overlay validation requires the exact base entry map",
        )
    })?;
    validate_external_map(base_entries, manifest.git_object_format)?;
    let mut resolved = base_entries.clone();

    for operation in operations {
        match operation {
            OverlayOperation::Delete {
                path,
                base_mode,
                base_blob_oid,
            } => {
                let base = resolved.get(path).ok_or_else(|| {
                    err(
                        "candidate_snapshot.delete_missing",
                        format!("delete path {path:?} is absent from the exact base"),
                    )
                })?;
                if !is_regular_mode(&base.mode) {
                    return Err(err(
                        "candidate_snapshot.overlay_mode_unsupported",
                        format!(
                            "overlay delete cannot mutate mode {} at {path:?}",
                            base.mode
                        ),
                    ));
                }
                if base.mode != *base_mode || base.blob_oid != *base_blob_oid {
                    return Err(err(
                        "candidate_snapshot.delete_precondition_mismatch",
                        format!("delete precondition differs from exact base at {path:?}"),
                    ));
                }
                resolved.remove(path);
            }
            OverlayOperation::Upsert {
                path,
                mode,
                blob_oid,
                size,
                sha256,
                ..
            } => {
                if let Some(base) = resolved.get(path) {
                    if !is_regular_mode(&base.mode) {
                        return Err(err(
                            "candidate_snapshot.overlay_mode_unsupported",
                            format!(
                                "overlay upsert cannot replace mode {} at {path:?}",
                                base.mode
                            ),
                        ));
                    }
                    if base.mode == *mode && base.blob_oid == *blob_oid {
                        return Err(err(
                            "candidate_snapshot.operation_noop",
                            format!("upsert at {path:?} does not change mode or blob"),
                        ));
                    }
                }
                resolved.insert(
                    path.clone(),
                    SnapshotEntry {
                        path: path.clone(),
                        mode: mode.clone(),
                        blob_oid: blob_oid.clone(),
                        size: *size,
                        sha256: sha256.clone(),
                    },
                );
            }
        }
    }

    if resolved != advertised {
        return Err(err(
            "candidate_snapshot.entries_mismatch",
            "applying overlay operations to the exact base did not reproduce advertised entries",
        ));
    }
    Ok(())
}

/// Recompute the Git-native candidate root tree object ID.
pub fn compute_candidate_tree_oid(manifest: &CandidateSnapshotManifest) -> Result<String> {
    let mut root = TreeNode::default();
    for entry in manifest.candidate.entries() {
        root.insert(entry, manifest.git_object_format)?;
    }
    Ok(hex_lower(&root.oid(manifest.git_object_format)?))
}

/// Recompute the kind-independent complete snapshot digest.
pub fn compute_snapshot_digest(manifest: &CandidateSnapshotManifest) -> Result<String> {
    let mut preimage = b"cargoless-candidate-snapshot\0v1\0".to_vec();
    push_lp(
        &mut preimage,
        manifest.git_object_format.as_str().as_bytes(),
    )?;
    push_lp(
        &mut preimage,
        &hex_decode(manifest.candidate.tree_oid(), "candidate tree_oid")?,
    )?;
    push_u64(&mut preimage, manifest.candidate.entry_count());
    for entry in manifest.candidate.entries() {
        push_entry_record(&mut preimage, entry)?;
    }
    Ok(format!("sha256:{}", sha256_hex(&preimage)))
}

/// Recompute the manifest digest, excluding the manifest digest field itself.
pub fn compute_manifest_digest(manifest: &CandidateSnapshotManifest) -> Result<String> {
    let mut preimage = b"cargoless-candidate-manifest\0v1\0".to_vec();
    push_lp(
        &mut preimage,
        manifest.git_object_format.as_str().as_bytes(),
    )?;
    push_lp(&mut preimage, manifest.candidate.kind().as_bytes())?;
    push_tree_ref_record(&mut preimage, &manifest.comparison_base)?;
    match &manifest.candidate {
        CandidateSnapshot::Tree { commit_sha, .. } => {
            push_lp(
                &mut preimage,
                &hex_decode(commit_sha, "candidate commit_sha")?,
            )?;
        }
        CandidateSnapshot::Index { base, .. } | CandidateSnapshot::Overlay { base, .. } => {
            push_tree_ref_record(&mut preimage, base)?;
        }
    }
    push_lp(
        &mut preimage,
        &hex_decode(manifest.candidate.tree_oid(), "candidate tree_oid")?,
    )?;
    let snapshot_hex = manifest
        .candidate
        .snapshot_digest()
        .strip_prefix("sha256:")
        .ok_or_else(|| {
            err(
                "candidate_snapshot.field_invalid",
                "snapshot_digest must begin with sha256:",
            )
        })?;
    push_lp(
        &mut preimage,
        &hex_decode(snapshot_hex, "candidate snapshot_digest")?,
    )?;
    push_u64(&mut preimage, manifest.candidate.entry_count());
    for entry in manifest.candidate.entries() {
        push_entry_record(&mut preimage, entry)?;
    }
    push_u64(&mut preimage, manifest.candidate.operation_count());
    for operation in manifest.candidate.operations() {
        push_operation_record(&mut preimage, operation)?;
    }
    Ok(format!("sha256:{}", sha256_hex(&preimage)))
}

/// Decode canonical RFC 4648 section 4 base64 without accepting aliases.
pub fn decode_overlay_payload(payload: &OverlayPayload) -> Result<Vec<u8>> {
    if payload.encoding != "base64" {
        return Err(err(
            "candidate_snapshot.payload_base64_invalid",
            format!("unsupported payload encoding {:?}", payload.encoding),
        ));
    }
    decode_base64(&payload.data)
}

fn validate_candidate_shape(candidate: &CandidateSnapshot, format: GitObjectFormat) -> Result<()> {
    match candidate {
        CandidateSnapshot::Tree {
            commit_sha,
            tree_oid,
            entry_count,
            entries,
            snapshot_digest,
        } => {
            validate_oid(commit_sha, format, "candidate commit_sha")?;
            validate_oid(tree_oid, format, "candidate tree_oid")?;
            validate_entries(*entry_count, entries, format)?;
            validate_digest(snapshot_digest, "snapshot_digest")?;
        }
        CandidateSnapshot::Index {
            base,
            tree_oid,
            entry_count,
            entries,
            snapshot_digest,
        } => {
            validate_tree_ref(base, format)?;
            validate_oid(tree_oid, format, "candidate tree_oid")?;
            validate_entries(*entry_count, entries, format)?;
            validate_digest(snapshot_digest, "snapshot_digest")?;
        }
        CandidateSnapshot::Overlay {
            base,
            tree_oid,
            entry_count,
            entries,
            snapshot_digest,
            operation_count,
            operations,
        } => {
            validate_tree_ref(base, format)?;
            validate_oid(tree_oid, format, "candidate tree_oid")?;
            validate_entries(*entry_count, entries, format)?;
            validate_digest(snapshot_digest, "snapshot_digest")?;
            validate_operations(*operation_count, operations, format)?;
        }
    }
    Ok(())
}

fn validate_tree_ref(reference: &GitTreeRef, format: GitObjectFormat) -> Result<()> {
    validate_oid(&reference.commit_sha, format, "commit_sha")?;
    validate_oid(&reference.tree_oid, format, "tree_oid")
}

fn validate_entries(count: u64, entries: &[SnapshotEntry], format: GitObjectFormat) -> Result<()> {
    validate_json_integer(count, "entry_count")?;
    if entries.len() > MAX_ENTRIES {
        return Err(err(
            "candidate_snapshot.limit_exceeded",
            format!(
                "entry count {} exceeds v1 limit {MAX_ENTRIES}",
                entries.len()
            ),
        ));
    }
    if count != entries.len() as u64 {
        return Err(err(
            "candidate_snapshot.entries_mismatch",
            format!(
                "entry_count {count} differs from array length {}",
                entries.len()
            ),
        ));
    }

    let mut previous: Option<&str> = None;
    for entry in entries {
        validate_path(&entry.path)?;
        if let Some(previous) = previous {
            match previous.as_bytes().cmp(entry.path.as_bytes()) {
                Ordering::Equal => {
                    return Err(err(
                        "candidate_snapshot.entry_duplicate",
                        format!("duplicate entry path {:?}", entry.path),
                    ));
                }
                Ordering::Greater => {
                    return Err(err(
                        "candidate_snapshot.entry_order",
                        format!(
                            "entry {:?} is not strictly after previous path {previous:?}",
                            entry.path
                        ),
                    ));
                }
                Ordering::Less => {}
            }
        }
        previous = Some(&entry.path);
        validate_entry(entry, format)?;
    }
    Ok(())
}

fn validate_entry(entry: &SnapshotEntry, format: GitObjectFormat) -> Result<()> {
    match entry.mode.as_str() {
        "100644" | "100755" | "120000" => {}
        "160000" => {
            return Err(err(
                "candidate_snapshot.gitlink_unsupported",
                format!("gitlink entry is unsupported at {:?}", entry.path),
            ));
        }
        _ => {
            return Err(err(
                "candidate_snapshot.mode_unsupported",
                format!("unsupported leaf mode {:?} at {:?}", entry.mode, entry.path),
            ));
        }
    }
    validate_oid(&entry.blob_oid, format, "entry blob_oid")?;
    validate_json_integer(entry.size, "entry size")?;
    validate_sha256_hex(&entry.sha256, "entry sha256")
}

fn validate_operations(
    count: u64,
    operations: &[OverlayOperation],
    format: GitObjectFormat,
) -> Result<()> {
    validate_json_integer(count, "operation_count")?;
    if operations.len() > MAX_OPERATIONS {
        return Err(err(
            "candidate_snapshot.limit_exceeded",
            format!(
                "operation count {} exceeds v1 limit {MAX_OPERATIONS}",
                operations.len()
            ),
        ));
    }
    if count != operations.len() as u64 {
        return Err(err(
            "candidate_snapshot.entries_mismatch",
            format!(
                "operation_count {count} differs from array length {}",
                operations.len()
            ),
        ));
    }
    if operations.is_empty() {
        return Err(err(
            "candidate_snapshot.operation_noop",
            "overlay operations must be a non-empty delta",
        ));
    }

    let mut total_payload_bytes = 0usize;
    let mut previous: Option<&str> = None;
    for operation in operations {
        let path = operation.path();
        validate_path(path)?;
        if let Some(previous) = previous {
            match previous.as_bytes().cmp(path.as_bytes()) {
                Ordering::Equal => {
                    return Err(err(
                        "candidate_snapshot.operation_duplicate",
                        format!("duplicate overlay operation path {path:?}"),
                    ));
                }
                Ordering::Greater => {
                    return Err(err(
                        "candidate_snapshot.operation_order",
                        format!("overlay operation {path:?} is not strictly after {previous:?}"),
                    ));
                }
                Ordering::Less => {}
            }
        }
        previous = Some(path);

        match operation {
            OverlayOperation::Delete {
                base_mode,
                base_blob_oid,
                ..
            } => {
                validate_overlay_mode(base_mode, path)?;
                validate_oid(base_blob_oid, format, "delete base_blob_oid")?;
            }
            OverlayOperation::Upsert {
                mode,
                blob_oid,
                size,
                sha256,
                payload,
                ..
            } => {
                validate_overlay_mode(mode, path)?;
                validate_oid(blob_oid, format, "upsert blob_oid")?;
                validate_json_integer(*size, "upsert size")?;
                validate_sha256_hex(sha256, "upsert sha256")?;
                if payload.data.len() > ((MAX_UPSERT_BYTES + 2) / 3) * 4 {
                    return Err(err(
                        "candidate_snapshot.limit_exceeded",
                        format!("encoded payload at {path:?} exceeds v1 limit"),
                    ));
                }
                let bytes = decode_overlay_payload(payload)?;
                if bytes.len() > MAX_UPSERT_BYTES {
                    return Err(err(
                        "candidate_snapshot.limit_exceeded",
                        format!("decoded payload at {path:?} exceeds 32 MiB"),
                    ));
                }
                total_payload_bytes =
                    total_payload_bytes
                        .checked_add(bytes.len())
                        .ok_or_else(|| {
                            err(
                                "candidate_snapshot.limit_exceeded",
                                "decoded payload size sum overflowed",
                            )
                        })?;
                if total_payload_bytes > MAX_TOTAL_UPSERT_BYTES {
                    return Err(err(
                        "candidate_snapshot.limit_exceeded",
                        "decoded overlay payloads exceed 64 MiB",
                    ));
                }
                if *size != bytes.len() as u64 {
                    return Err(err(
                        "candidate_snapshot.payload_size_mismatch",
                        format!(
                            "payload at {path:?} has {} bytes, advertised {size}",
                            bytes.len()
                        ),
                    ));
                }
                let computed_sha256 = sha256_hex(&bytes);
                if computed_sha256 != *sha256 {
                    return Err(err(
                        "candidate_snapshot.payload_sha256_mismatch",
                        format!("payload SHA-256 differs at {path:?}"),
                    ));
                }
                let computed_oid = git_object_oid(format, "blob", &bytes);
                if computed_oid != *blob_oid {
                    return Err(err(
                        "candidate_snapshot.payload_oid_mismatch",
                        format!("payload Git blob OID differs at {path:?}"),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<()> {
    let invalid = path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.bytes().any(|byte| byte <= 0x1f || byte == 0x7f)
        || !path.nfc().eq(path.chars())
        || path.split('/').any(|component| {
            component.is_empty()
                || component == "."
                || component == ".."
                || component.eq_ignore_ascii_case(".git")
        });
    if invalid {
        Err(err(
            "candidate_snapshot.path_noncanonical",
            format!("noncanonical repository path {path:?}"),
        ))
    } else {
        Ok(())
    }
}

fn validate_external_map(
    entries: &BTreeMap<String, SnapshotEntry>,
    format: GitObjectFormat,
) -> Result<()> {
    if entries.len() > MAX_ENTRIES {
        return Err(err(
            "candidate_snapshot.limit_exceeded",
            "resolved entry map exceeds the v1 limit",
        ));
    }
    for (path, entry) in entries {
        if path != &entry.path {
            return Err(err(
                "candidate_snapshot.entries_mismatch",
                format!(
                    "entry-map key {path:?} differs from embedded path {:?}",
                    entry.path
                ),
            ));
        }
        validate_path(path)?;
        validate_entry(entry, format)?;
    }
    Ok(())
}

fn entry_map(entries: &[SnapshotEntry]) -> BTreeMap<String, SnapshotEntry> {
    entries
        .iter()
        .cloned()
        .map(|entry| (entry.path.clone(), entry))
        .collect()
}

fn validate_overlay_mode(mode: &str, path: &str) -> Result<()> {
    if is_regular_mode(mode) {
        Ok(())
    } else {
        Err(err(
            "candidate_snapshot.overlay_mode_unsupported",
            format!("overlay cannot mutate mode {mode:?} at {path:?}"),
        ))
    }
}

fn is_regular_mode(mode: &str) -> bool {
    matches!(mode, "100644" | "100755")
}

fn validate_oid(value: &str, format: GitObjectFormat, field: &str) -> Result<()> {
    if value.len() != format.oid_hex_len()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(err(
            "candidate_snapshot.oid_invalid",
            format!(
                "{field} must be exactly {} lowercase hexadecimal characters",
                format.oid_hex_len()
            ),
        ));
    }
    Ok(())
}

fn validate_sha256_hex(value: &str, field: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(err(
            "candidate_snapshot.field_invalid",
            format!("{field} must be exactly 64 lowercase hexadecimal characters"),
        ));
    }
    Ok(())
}

fn validate_digest(value: &str, field: &str) -> Result<()> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(err(
            "candidate_snapshot.field_invalid",
            format!("{field} must begin with sha256:"),
        ));
    };
    validate_sha256_hex(hex, field)
}

fn validate_json_integer(value: u64, field: &str) -> Result<()> {
    if value > MAX_JSON_INTEGER {
        Err(err(
            "candidate_snapshot.limit_exceeded",
            format!("{field} exceeds the maximum exact JSON integer"),
        ))
    } else {
        Ok(())
    }
}

fn map_json_error(error: serde_json::Error) -> CandidateSnapshotError {
    let message = error.to_string();
    let code = if message.contains("duplicate field") {
        "candidate_snapshot.json_duplicate_key"
    } else if message.contains("missing field `schema`") {
        "candidate_snapshot.schema_unsupported"
    } else if message.contains("unknown variant")
        && message.contains("sha1")
        && message.contains("sha256")
    {
        "candidate_snapshot.object_format_unsupported"
    } else if error.is_data() {
        "candidate_snapshot.field_invalid"
    } else {
        "candidate_snapshot.json_invalid"
    };
    err(code, message)
}

fn err(code: &'static str, message: impl Into<String>) -> CandidateSnapshotError {
    CandidateSnapshotError::new(code, message)
}

fn push_u64(target: &mut Vec<u8>, value: u64) {
    target.extend_from_slice(&value.to_be_bytes());
}

fn push_lp(target: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    let len = u64::try_from(bytes.len()).map_err(|_| {
        err(
            "candidate_snapshot.limit_exceeded",
            "length-prefixed field cannot fit in u64",
        )
    })?;
    push_u64(target, len);
    target.extend_from_slice(bytes);
    Ok(())
}

fn push_entry_record(target: &mut Vec<u8>, entry: &SnapshotEntry) -> Result<()> {
    push_lp(target, entry.path.as_bytes())?;
    push_lp(target, entry.mode.as_bytes())?;
    push_lp(target, &hex_decode(&entry.blob_oid, "entry blob_oid")?)?;
    push_u64(target, entry.size);
    push_lp(target, &hex_decode(&entry.sha256, "entry sha256")?)
}

fn push_tree_ref_record(target: &mut Vec<u8>, reference: &GitTreeRef) -> Result<()> {
    push_lp(target, &hex_decode(&reference.commit_sha, "commit_sha")?)?;
    push_lp(target, &hex_decode(&reference.tree_oid, "tree_oid")?)
}

fn push_operation_record(target: &mut Vec<u8>, operation: &OverlayOperation) -> Result<()> {
    match operation {
        OverlayOperation::Delete {
            path,
            base_mode,
            base_blob_oid,
        } => {
            push_lp(target, b"delete")?;
            push_lp(target, path.as_bytes())?;
            push_lp(target, base_mode.as_bytes())?;
            push_lp(target, &hex_decode(base_blob_oid, "delete base_blob_oid")?)
        }
        OverlayOperation::Upsert {
            path,
            mode,
            blob_oid,
            size,
            sha256,
            ..
        } => {
            push_lp(target, b"upsert")?;
            push_lp(target, path.as_bytes())?;
            push_lp(target, mode.as_bytes())?;
            push_lp(target, &hex_decode(blob_oid, "upsert blob_oid")?)?;
            push_u64(target, *size);
            push_lp(target, &hex_decode(sha256, "upsert sha256")?)
        }
    }
}

fn hex_decode(value: &str, field: &str) -> Result<Vec<u8>> {
    if value.len() % 2 != 0 {
        return Err(err(
            "candidate_snapshot.field_invalid",
            format!("{field} has odd hexadecimal length"),
        ));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0]).ok_or_else(|| {
                err(
                    "candidate_snapshot.field_invalid",
                    format!("{field} contains non-hexadecimal input"),
                )
            })?;
            let low = hex_nibble(pair[1]).ok_or_else(|| {
                err(
                    "candidate_snapshot.field_invalid",
                    format!("{field} contains non-hexadecimal input"),
                )
            })?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn decode_base64(input: &str) -> Result<Vec<u8>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    if input.len() % 4 != 0 {
        return Err(err(
            "candidate_snapshot.payload_base64_invalid",
            "base64 length must be a multiple of four",
        ));
    }

    let chunks = input.as_bytes().chunks_exact(4);
    let chunk_count = chunks.len();
    let mut out = Vec::with_capacity((input.len() / 4) * 3);
    for (index, chunk) in chunks.enumerate() {
        let last = index + 1 == chunk_count;
        let a = base64_value(chunk[0]);
        let b = base64_value(chunk[1]);
        if a.is_none() || b.is_none() {
            return base64_error();
        }
        let a = a.unwrap_or_default();
        let b = b.unwrap_or_default();
        let c_pad = chunk[2] == b'=';
        let d_pad = chunk[3] == b'=';
        if (!last && d_pad) || (c_pad && !d_pad) {
            return base64_error();
        }
        let c = if c_pad {
            0
        } else {
            base64_value(chunk[2]).ok_or_else(base64_error_value)?
        };
        let d = if d_pad {
            0
        } else {
            base64_value(chunk[3]).ok_or_else(base64_error_value)?
        };
        if (c_pad && b & 0x0f != 0) || (d_pad && !c_pad && c & 0x03 != 0) {
            return base64_error();
        }

        out.push((a << 2) | (b >> 4));
        if !c_pad {
            out.push((b << 4) | (c >> 2));
        }
        if !d_pad {
            out.push((c << 6) | d);
        }
    }
    Ok(out)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn base64_error<T>() -> Result<T> {
    Err(base64_error_value())
}

fn base64_error_value() -> CandidateSnapshotError {
    err(
        "candidate_snapshot.payload_base64_invalid",
        "payload is not canonical RFC 4648 section 4 base64",
    )
}

#[derive(Default)]
struct TreeNode {
    children: BTreeMap<String, TreeChild>,
}

enum TreeChild {
    Directory(TreeNode),
    Leaf { mode: String, oid: Vec<u8> },
}

impl TreeNode {
    fn insert(&mut self, entry: &SnapshotEntry, format: GitObjectFormat) -> Result<()> {
        let mut components = entry.path.split('/').peekable();
        let mut node = self;
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                if node.children.contains_key(component) {
                    return Err(err(
                        "candidate_snapshot.path_noncanonical",
                        format!("entry path conflicts with another path at {:?}", entry.path),
                    ));
                }
                node.children.insert(
                    component.to_owned(),
                    TreeChild::Leaf {
                        mode: entry.mode.clone(),
                        oid: {
                            validate_oid(&entry.blob_oid, format, "entry blob_oid")?;
                            hex_decode(&entry.blob_oid, "entry blob_oid")?
                        },
                    },
                );
            } else {
                let child = node
                    .children
                    .entry(component.to_owned())
                    .or_insert_with(|| TreeChild::Directory(TreeNode::default()));
                match child {
                    TreeChild::Directory(directory) => node = directory,
                    TreeChild::Leaf { .. } => {
                        return Err(err(
                            "candidate_snapshot.path_noncanonical",
                            format!("entry path has a file prefix at {component:?}"),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn oid(&self, format: GitObjectFormat) -> Result<Vec<u8>> {
        let mut items = Vec::with_capacity(self.children.len());
        for (name, child) in &self.children {
            match child {
                TreeChild::Directory(directory) => items.push(TreeItem {
                    name,
                    mode: "40000",
                    oid: directory.oid(format)?,
                    directory: true,
                }),
                TreeChild::Leaf { mode, oid } => items.push(TreeItem {
                    name,
                    mode,
                    oid: oid.clone(),
                    directory: false,
                }),
            }
        }
        items.sort_by_key(TreeItem::sort_key);

        let mut body = Vec::new();
        for item in items {
            body.extend_from_slice(item.mode.as_bytes());
            body.push(b' ');
            body.extend_from_slice(item.name.as_bytes());
            body.push(0);
            body.extend_from_slice(&item.oid);
        }
        Ok(git_object_hash(format, "tree", &body))
    }
}

struct TreeItem<'a> {
    name: &'a str,
    mode: &'a str,
    oid: Vec<u8>,
    directory: bool,
}

impl TreeItem<'_> {
    fn sort_key(&self) -> Vec<u8> {
        let mut key = self.name.as_bytes().to_vec();
        if self.directory {
            key.push(b'/');
        }
        key
    }
}

fn git_object_oid(format: GitObjectFormat, kind: &str, body: &[u8]) -> String {
    hex_lower(&git_object_hash(format, kind, body))
}

fn git_object_hash(format: GitObjectFormat, kind: &str, body: &[u8]) -> Vec<u8> {
    let mut object = Vec::with_capacity(kind.len() + 32 + body.len());
    object.extend_from_slice(kind.as_bytes());
    object.push(b' ');
    object.extend_from_slice(body.len().to_string().as_bytes());
    object.push(0);
    object.extend_from_slice(body);
    match format {
        GitObjectFormat::Sha1 => sha1(&object).to_vec(),
        GitObjectFormat::Sha256 => hex_decode(&sha256_hex(&object), "internal sha256")
            .expect("sha256_hex always emits valid lowercase hexadecimal"),
    }
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h = [
        0x6745_2301u32,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut words = [0u32; 80];
        for (word, bytes) in words.iter_mut().take(16).zip(chunk.chunks_exact(4)) {
            *word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (index, word) in words.into_iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (bytes, word) in out.chunks_exact_mut(4).zip(h) {
        bytes.copy_from_slice(&word.to_be_bytes());
    }
    out
}
