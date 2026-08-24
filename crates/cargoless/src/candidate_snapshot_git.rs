//! Git-backed construction of complete candidate-snapshot manifests.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cargoless_core::{
    CANDIDATE_SNAPSHOT_SCHEMA_V1, CandidateSnapshot, CandidateSnapshotError,
    CandidateSnapshotManifest, GitObjectFormat, GitTreeRef, OverlayOperation, OverlayPayload,
    SnapshotEntry, canonical_manifest_json, compute_candidate_tree_oid, compute_manifest_digest,
    compute_snapshot_digest, parse_and_validate_manifest_json,
    validate_manifest_against_entry_maps,
};

const ZERO_DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
const MAX_TREE_ENTRIES: usize = 1_000_000;
const MAX_OVERLAY_OPERATIONS: usize = 65_536;
const MAX_UPSERT_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOTAL_UPSERT_BYTES: usize = 64 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_JSON_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_LS_TREE_RECORD_BYTES: usize = MAX_PATH_BYTES + 128;
static TEMP_INDEX_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) struct CandidateSnapshotGitError {
    message: String,
}

impl CandidateSnapshotGitError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CandidateSnapshotGitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CandidateSnapshotGitError {}

impl From<std::io::Error> for CandidateSnapshotGitError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl From<CandidateSnapshotError> for CandidateSnapshotGitError {
    fn from(error: CandidateSnapshotError) -> Self {
        Self::new(error.to_string())
    }
}

type Result<T> = std::result::Result<T, CandidateSnapshotGitError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedGitTree {
    pub(crate) tree_oid: String,
    pub(crate) entries: BTreeMap<String, SnapshotEntry>,
    pub(crate) retained_blobs: BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedCommitSnapshot {
    pub(crate) git_object_format: GitObjectFormat,
    pub(crate) reference: GitTreeRef,
    pub(crate) entries: BTreeMap<String, SnapshotEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuiltCandidateOverlay {
    pub(crate) manifest: CandidateSnapshotManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntryMetadata {
    mode: String,
    blob_oid: String,
    advertised_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedGitTree {
    tree_oid: String,
    entries: BTreeMap<String, TreeEntryMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PayloadPlan {
    retained_oids: BTreeSet<String>,
}

struct StreamedBlobs {
    identities: BTreeMap<String, (u64, String)>,
    retained: BTreeMap<String, Vec<u8>>,
}

struct TemporaryIndex {
    directory: PathBuf,
    path: PathBuf,
}

impl TemporaryIndex {
    fn create() -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| CandidateSnapshotGitError::new(error.to_string()))?
            .as_nanos();
        for _ in 0..128 {
            let sequence = TEMP_INDEX_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "cargoless-candidate-index-{}-{nanos}-{sequence}",
                std::process::id()
            ));
            match std::fs::create_dir(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        path: directory.join("index"),
                        directory,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(CandidateSnapshotGitError::new(
            "could not allocate a unique temporary Git index directory",
        ))
    }
}

impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Resolve a revision once to an immutable commit/tree and its complete entry map.
pub(crate) fn resolve_commit_snapshot(
    repo: &Path,
    revision: &str,
) -> Result<ResolvedCommitSnapshot> {
    let git_object_format = git_object_format(repo)?;
    let commit_sha = git_text(
        repo,
        None,
        ["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
    )?;
    let tree_oid = git_text(
        repo,
        None,
        ["rev-parse", "--verify", &format!("{commit_sha}^{{tree}}")],
    )?;
    let tree = resolve_tree_snapshot(repo, git_object_format, &tree_oid)?;
    Ok(ResolvedCommitSnapshot {
        git_object_format,
        reference: GitTreeRef {
            commit_sha,
            tree_oid: tree.tree_oid,
        },
        entries: tree.entries,
    })
}

/// Resolve one immutable tree object while streaming its blobs into entry identities.
pub(crate) fn resolve_tree_snapshot(
    repo: &Path,
    git_object_format: GitObjectFormat,
    tree_oid: &str,
) -> Result<ResolvedGitTree> {
    let parsed = parse_tree_metadata(repo, git_object_format, tree_oid)?;
    hydrate_tree_snapshot(repo, parsed, &BTreeSet::new())
}

/// Build the complete worktree candidate through an isolated temporary index.
pub(crate) fn build_overlay_manifest(
    repo: &Path,
    comparison_base: &str,
) -> Result<Option<BuiltCandidateOverlay>> {
    let base = resolve_commit_snapshot(repo, comparison_base)?;
    let temporary_index = TemporaryIndex::create()?;
    git_output(
        repo,
        Some(&temporary_index.path),
        ["read-tree", base.reference.commit_sha.as_str()],
    )?;
    git_output(repo, Some(&temporary_index.path), ["add", "-A", "--", "."])?;
    let candidate_tree_oid = git_text(repo, Some(&temporary_index.path), ["write-tree"])?;
    let candidate_metadata =
        parse_tree_metadata(repo, base.git_object_format, &candidate_tree_oid)?;
    let payload_plan = plan_changed_payloads(&base.entries, &candidate_metadata.entries)?;
    let candidate = hydrate_tree_snapshot(repo, candidate_metadata, &payload_plan.retained_oids)?;
    let operations = overlay_operations(&base, &candidate)?;
    if operations.is_empty() {
        return Ok(None);
    }

    let entries: Vec<SnapshotEntry> = candidate.entries.values().cloned().collect();
    let mut manifest = CandidateSnapshotManifest {
        schema: CANDIDATE_SNAPSHOT_SCHEMA_V1.to_string(),
        git_object_format: base.git_object_format,
        comparison_base: base.reference.clone(),
        candidate: CandidateSnapshot::Overlay {
            base: base.reference.clone(),
            tree_oid: candidate_tree_oid.clone(),
            entry_count: entries.len() as u64,
            entries,
            snapshot_digest: ZERO_DIGEST.to_string(),
            operation_count: operations.len() as u64,
            operations,
        },
        manifest_digest: ZERO_DIGEST.to_string(),
    };

    let computed_tree_oid = compute_candidate_tree_oid(&manifest)?;
    if computed_tree_oid != candidate_tree_oid {
        return Err(CandidateSnapshotGitError::new(format!(
            "candidate_snapshot.tree_oid_mismatch: Git wrote {candidate_tree_oid}, core computed {computed_tree_oid}"
        )));
    }
    let snapshot_digest = compute_snapshot_digest(&manifest)?;
    let CandidateSnapshot::Overlay {
        snapshot_digest: advertised,
        ..
    } = &mut manifest.candidate
    else {
        unreachable!("builder creates an overlay candidate")
    };
    *advertised = snapshot_digest;
    manifest.manifest_digest = compute_manifest_digest(&manifest)?;
    validate_manifest_against_entry_maps(&manifest, Some(&base.entries), &candidate.entries)?;
    let canonical = canonical_manifest_json(&manifest)?;
    let manifest = parse_and_validate_manifest_json(&canonical)?;

    Ok(Some(BuiltCandidateOverlay { manifest }))
}

fn overlay_operations(
    base: &ResolvedCommitSnapshot,
    candidate: &ResolvedGitTree,
) -> Result<Vec<OverlayOperation>> {
    let paths: BTreeSet<&String> = base
        .entries
        .keys()
        .chain(candidate.entries.keys())
        .collect();
    let upsert = |path: &String, candidate_entry: &SnapshotEntry| -> Result<OverlayOperation> {
        let bytes = candidate
            .retained_blobs
            .get(&candidate_entry.blob_oid)
            .ok_or_else(|| {
                CandidateSnapshotGitError::new(format!(
                    "candidate_snapshot.blob_missing: missing candidate blob {}",
                    candidate_entry.blob_oid
                ))
            })?;
        Ok(OverlayOperation::Upsert {
            path: path.clone(),
            mode: candidate_entry.mode.clone(),
            blob_oid: candidate_entry.blob_oid.clone(),
            size: candidate_entry.size,
            sha256: candidate_entry.sha256.clone(),
            payload: OverlayPayload {
                encoding: "base64".to_string(),
                data: encode_base64(bytes),
            },
        })
    };
    let mut operations = Vec::new();
    for path in paths {
        match (base.entries.get(path), candidate.entries.get(path)) {
            (Some(base_entry), None) => operations.push(OverlayOperation::Delete {
                path: path.clone(),
                base_mode: base_entry.mode.clone(),
                base_blob_oid: base_entry.blob_oid.clone(),
            }),
            (None, Some(candidate_entry)) => {
                operations.push(upsert(path, candidate_entry)?);
            }
            (Some(base_entry), Some(candidate_entry))
                if base_entry.mode != candidate_entry.mode
                    || base_entry.blob_oid != candidate_entry.blob_oid =>
            {
                operations.push(upsert(path, candidate_entry)?);
            }
            (Some(_), Some(_)) => {}
            (None, None) => unreachable!("path originated in one entry map"),
        }
    }
    Ok(operations)
}

fn parse_tree_metadata(
    repo: &Path,
    git_object_format: GitObjectFormat,
    tree_oid: &str,
) -> Result<ParsedGitTree> {
    validate_git_oid(git_object_format, tree_oid, "tree")?;
    let mut command = git_command(repo, None);
    command.args(["ls-tree", "-rz", "--full-tree", "-l", tree_oid]);
    let debug = format!("{command:?}");
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CandidateSnapshotGitError::new("git ls-tree stdout unavailable"))?;
    let mut reader = BufReader::new(stdout);
    let mut entries = BTreeMap::new();
    let parsed = (|| -> Result<()> {
        while let Some(record) = read_delimited_record(
            &mut reader,
            b'\0',
            MAX_LS_TREE_RECORD_BYTES,
            "git ls-tree record",
        )? {
            if entries.len() >= MAX_TREE_ENTRIES {
                return Err(CandidateSnapshotGitError::new(format!(
                    "candidate_snapshot.limit_exceeded: tree entry count exceeds v1 limit {MAX_TREE_ENTRIES}"
                )));
            }
            let (path, entry) = parse_tree_record(git_object_format, &record)?;
            if entries.insert(path.clone(), entry).is_some() {
                return Err(CandidateSnapshotGitError::new(format!(
                    "candidate_snapshot.entry_duplicate: duplicate Git path {path:?}"
                )));
            }
        }
        Ok(())
    })();
    drop(reader);
    if let Err(error) = parsed {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    let status = child.wait()?;
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr)?;
    }
    if !status.success() {
        return Err(CandidateSnapshotGitError::new(format!(
            "{debug} exited {:?}: {}",
            status.code(),
            stderr.trim()
        )));
    }

    Ok(ParsedGitTree {
        tree_oid: tree_oid.to_string(),
        entries,
    })
}

fn parse_tree_record(
    git_object_format: GitObjectFormat,
    record: &[u8],
) -> Result<(String, TreeEntryMetadata)> {
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| {
            CandidateSnapshotGitError::new("git ls-tree returned a record without a path")
        })?;
    let metadata = std::str::from_utf8(&record[..tab]).map_err(|error| {
        CandidateSnapshotGitError::new(format!("git ls-tree returned non-UTF-8 metadata: {error}"))
    })?;
    let mut fields = metadata.split_ascii_whitespace();
    let mode = fields
        .next()
        .ok_or_else(|| CandidateSnapshotGitError::new("git ls-tree omitted entry mode"))?;
    let object_type = fields
        .next()
        .ok_or_else(|| CandidateSnapshotGitError::new("git ls-tree omitted object type"))?;
    let blob_oid = fields
        .next()
        .ok_or_else(|| CandidateSnapshotGitError::new("git ls-tree omitted object id"))?;
    let advertised_size = fields
        .next()
        .ok_or_else(|| CandidateSnapshotGitError::new("git ls-tree omitted object size"))?;
    if fields.next().is_some() {
        return Err(CandidateSnapshotGitError::new(
            "git ls-tree returned unexpected entry metadata",
        ));
    }
    if object_type != "blob" {
        return Err(CandidateSnapshotGitError::new(format!(
            "candidate_snapshot.gitlink_unsupported: Git entry mode {mode} has object type {object_type}"
        )));
    }
    validate_git_oid(git_object_format, blob_oid, "blob")?;
    let advertised_size: u64 = advertised_size.parse().map_err(|error| {
        CandidateSnapshotGitError::new(format!(
            "candidate_snapshot.blob_missing: invalid advertised size for {blob_oid}: {error}"
        ))
    })?;
    if advertised_size > MAX_JSON_INTEGER {
        return Err(CandidateSnapshotGitError::new(format!(
            "candidate_snapshot.limit_exceeded: blob {blob_oid} exceeds the v1 integer limit"
        )));
    }
    let path_bytes = &record[tab + 1..];
    if path_bytes.len() > MAX_PATH_BYTES {
        return Err(CandidateSnapshotGitError::new(format!(
            "candidate_snapshot.limit_exceeded: Git path exceeds {MAX_PATH_BYTES} bytes"
        )));
    }
    let path = String::from_utf8(path_bytes.to_vec()).map_err(|error| {
        CandidateSnapshotGitError::new(format!(
            "candidate_snapshot.path_noncanonical: Git path is not UTF-8: {error}"
        ))
    })?;
    Ok((
        path,
        TreeEntryMetadata {
            mode: mode.to_string(),
            blob_oid: blob_oid.to_string(),
            advertised_size,
        },
    ))
}

fn plan_changed_payloads(
    base: &BTreeMap<String, SnapshotEntry>,
    candidate: &BTreeMap<String, TreeEntryMetadata>,
) -> Result<PayloadPlan> {
    let paths: BTreeSet<&String> = base.keys().chain(candidate.keys()).collect();
    let mut operation_count = 0usize;
    let mut total_upsert_bytes = 0u64;
    let mut retained_oids = BTreeSet::new();
    for path in paths {
        let changed = match (base.get(path), candidate.get(path)) {
            (Some(_), None) => true,
            (None, Some(_)) => true,
            (Some(base_entry), Some(candidate_entry)) => {
                base_entry.mode != candidate_entry.mode
                    || base_entry.blob_oid != candidate_entry.blob_oid
            }
            (None, None) => unreachable!("path originated in one metadata map"),
        };
        if !changed {
            continue;
        }
        operation_count = operation_count.checked_add(1).ok_or_else(|| {
            CandidateSnapshotGitError::new(
                "candidate_snapshot.limit_exceeded: operation count overflowed",
            )
        })?;
        if operation_count > MAX_OVERLAY_OPERATIONS {
            return Err(CandidateSnapshotGitError::new(format!(
                "candidate_snapshot.limit_exceeded: operation count exceeds v1 limit {MAX_OVERLAY_OPERATIONS}"
            )));
        }
        let Some(candidate_entry) = candidate.get(path) else {
            continue;
        };
        if candidate_entry.advertised_size > MAX_UPSERT_BYTES as u64 {
            return Err(CandidateSnapshotGitError::new(format!(
                "candidate_snapshot.limit_exceeded: upsert at {path:?} exceeds 32 MiB"
            )));
        }
        total_upsert_bytes = total_upsert_bytes
            .checked_add(candidate_entry.advertised_size)
            .ok_or_else(|| {
                CandidateSnapshotGitError::new(
                    "candidate_snapshot.limit_exceeded: aggregate upsert size overflowed",
                )
            })?;
        if total_upsert_bytes > MAX_TOTAL_UPSERT_BYTES as u64 {
            return Err(CandidateSnapshotGitError::new(
                "candidate_snapshot.limit_exceeded: overlay upserts exceed 64 MiB",
            ));
        }
        retained_oids.insert(candidate_entry.blob_oid.clone());
    }
    Ok(PayloadPlan { retained_oids })
}

fn validate_git_oid(format: GitObjectFormat, oid: &str, object: &str) -> Result<()> {
    if oid.len() == format.oid_hex_len()
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    Err(CandidateSnapshotGitError::new(format!(
        "candidate_snapshot.oid_invalid: invalid {} {object} object id {oid:?}",
        format.as_str()
    )))
}

fn read_delimited_record<R: BufRead>(
    reader: &mut R,
    delimiter: u8,
    maximum_bytes: usize,
    description: &str,
) -> Result<Option<Vec<u8>>> {
    let mut record = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if record.is_empty() {
                Ok(None)
            } else {
                Err(CandidateSnapshotGitError::new(format!(
                    "{description} ended without its delimiter"
                )))
            };
        }
        let delimiter_at = available.iter().position(|byte| *byte == delimiter);
        let consumed = delimiter_at.unwrap_or(available.len());
        if record.len().saturating_add(consumed) > maximum_bytes {
            return Err(CandidateSnapshotGitError::new(format!(
                "candidate_snapshot.limit_exceeded: {description} exceeds {maximum_bytes} bytes"
            )));
        }
        record.extend_from_slice(&available[..consumed]);
        reader.consume(consumed + usize::from(delimiter_at.is_some()));
        if delimiter_at.is_some() {
            return Ok(Some(record));
        }
    }
}

fn git_object_format(repo: &Path) -> Result<GitObjectFormat> {
    match git_text(repo, None, ["rev-parse", "--show-object-format"])?.as_str() {
        "sha1" => Ok(GitObjectFormat::Sha1),
        "sha256" => Ok(GitObjectFormat::Sha256),
        format => Err(CandidateSnapshotGitError::new(format!(
            "candidate_snapshot.object_format_unsupported: unsupported Git object format {format:?}"
        ))),
    }
}

fn git_command(repo: &Path, index: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo);
    command.env("LC_ALL", "C");
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
    command.env("GIT_NO_LAZY_FETCH", "1");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
        "GIT_QUARANTINE_PATH",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        command.env_remove(key);
    }
    for (key, _) in std::env::vars_os() {
        let key_text = key.to_string_lossy();
        if key_text.starts_with("GIT_CONFIG_KEY_") || key_text.starts_with("GIT_CONFIG_VALUE_") {
            command.env_remove(key);
        }
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command
}

fn git_output<I, S>(repo: &Path, index: Option<&Path>, args: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = git_command(repo, index);
    command.args(args);
    let debug = format!("{command:?}");
    let output = command.output()?;
    if !output.status.success() {
        return Err(CandidateSnapshotGitError::new(format!(
            "{debug} exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn git_text<I, S>(repo: &Path, index: Option<&Path>, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_output(repo, index, args)?;
    Ok(String::from_utf8(output)
        .map_err(|error| CandidateSnapshotGitError::new(error.to_string()))?
        .trim()
        .to_string())
}

fn hydrate_tree_snapshot(
    repo: &Path,
    parsed: ParsedGitTree,
    retained_oids: &BTreeSet<String>,
) -> Result<ResolvedGitTree> {
    let mut advertised_sizes = BTreeMap::new();
    for entry in parsed.entries.values() {
        match advertised_sizes.insert(entry.blob_oid.clone(), entry.advertised_size) {
            Some(previous) if previous != entry.advertised_size => {
                return Err(CandidateSnapshotGitError::new(format!(
                    "candidate_snapshot.blob_size_mismatch: blob {} has conflicting advertised sizes {previous} and {}",
                    entry.blob_oid, entry.advertised_size
                )));
            }
            _ => {}
        }
    }
    let streamed = stream_blobs(repo, &advertised_sizes, retained_oids)?;
    let mut entries = BTreeMap::new();
    for (path, metadata) in parsed.entries {
        let (size, sha256) = streamed.identities.get(&metadata.blob_oid).ok_or_else(|| {
            CandidateSnapshotGitError::new(format!(
                "candidate_snapshot.blob_missing: cat-file omitted {}",
                metadata.blob_oid
            ))
        })?;
        entries.insert(
            path.clone(),
            SnapshotEntry {
                path,
                mode: metadata.mode,
                blob_oid: metadata.blob_oid,
                size: *size,
                sha256: sha256.clone(),
            },
        );
    }
    Ok(ResolvedGitTree {
        tree_oid: parsed.tree_oid,
        entries,
        retained_blobs: streamed.retained,
    })
}

fn stream_blobs(
    repo: &Path,
    advertised_sizes: &BTreeMap<String, u64>,
    retained_oids: &BTreeSet<String>,
) -> Result<StreamedBlobs> {
    if advertised_sizes.is_empty() {
        return Ok(StreamedBlobs {
            identities: BTreeMap::new(),
            retained: BTreeMap::new(),
        });
    }
    let mut child = git_command(repo, None)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| CandidateSnapshotGitError::new("git cat-file stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CandidateSnapshotGitError::new("git cat-file stdout unavailable"))?;
    let mut reader = BufReader::new(stdout);
    let mut identities = BTreeMap::new();
    let mut blobs = BTreeMap::new();
    let streamed = (|| -> Result<()> {
        for (expected_oid, advertised_size) in advertised_sizes {
            writeln!(stdin, "{expected_oid}")?;
            stdin.flush()?;
            let header = read_delimited_record(&mut reader, b'\n', 256, "git cat-file header")?
                .ok_or_else(|| {
                    CandidateSnapshotGitError::new(format!(
                        "candidate_snapshot.blob_missing: cat-file ended before {expected_oid}"
                    ))
                })?;
            let header = std::str::from_utf8(&header).map_err(|error| {
                CandidateSnapshotGitError::new(format!(
                    "candidate_snapshot.blob_missing: non-UTF-8 cat-file header for {expected_oid}: {error}"
                ))
            })?;
            let fields: Vec<&str> = header.split_ascii_whitespace().collect();
            if fields.len() != 3 || fields[0] != expected_oid || fields[1] != "blob" {
                return Err(CandidateSnapshotGitError::new(format!(
                    "candidate_snapshot.blob_missing: unexpected cat-file response {header:?} for {expected_oid}"
                )));
            }
            let size: u64 = fields[2].parse().map_err(|error| {
                CandidateSnapshotGitError::new(format!(
                    "candidate_snapshot.blob_missing: invalid cat-file size for {expected_oid}: {error}"
                ))
            })?;
            if size != *advertised_size {
                return Err(CandidateSnapshotGitError::new(format!(
                    "candidate_snapshot.blob_size_mismatch: cat-file reports {size} bytes for {expected_oid}, ls-tree advertised {advertised_size}"
                )));
            }

            let retain = retained_oids.contains(expected_oid);
            if retain && size > MAX_UPSERT_BYTES as u64 {
                return Err(CandidateSnapshotGitError::new(format!(
                    "candidate_snapshot.limit_exceeded: upsert blob {expected_oid} exceeds 32 MiB"
                )));
            }
            let mut hasher = StreamingSha256::new();
            let bytes = if retain {
                let size = usize::try_from(size).map_err(|_| {
                    CandidateSnapshotGitError::new(
                        "candidate_snapshot.limit_exceeded: upsert size does not fit memory",
                    )
                })?;
                let mut bytes = vec![0; size];
                reader.read_exact(&mut bytes)?;
                hasher.update(&bytes);
                Some(bytes)
            } else {
                stream_hash_exact(&mut reader, size, &mut hasher)?;
                None
            };
            let mut delimiter = [0];
            reader.read_exact(&mut delimiter)?;
            if delimiter != [b'\n'] {
                return Err(CandidateSnapshotGitError::new(format!(
                    "candidate_snapshot.blob_missing: invalid cat-file delimiter for {expected_oid}"
                )));
            }
            identities.insert(expected_oid.clone(), (size, hasher.finish_hex()));
            if let Some(bytes) = bytes {
                blobs.insert(expected_oid.clone(), bytes);
            }
        }
        Ok(())
    })();
    drop(stdin);
    drop(reader);
    if let Err(error) = streamed {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    let status = child.wait()?;
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr)?;
    }
    if !status.success() {
        return Err(CandidateSnapshotGitError::new(format!(
            "git cat-file exited {:?}: {}",
            status.code(),
            stderr.trim()
        )));
    }
    Ok(StreamedBlobs {
        identities,
        retained: blobs,
    })
}

fn stream_hash_exact<R: Read>(
    reader: &mut R,
    mut remaining: u64,
    hasher: &mut StreamingSha256,
) -> Result<()> {
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(buffer.len() as u64))
            .expect("bounded stream chunk fits usize");
        reader.read_exact(&mut buffer[..chunk])?;
        hasher.update(&buffer[..chunk]);
        remaining -= chunk as u64;
    }
    Ok(())
}

struct StreamingSha256 {
    state: [u32; 8],
    block: [u8; 64],
    buffered: usize,
    byte_len: u64,
}

impl StreamingSha256 {
    const INITIAL_STATE: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    #[rustfmt::skip]
    const ROUND_CONSTANTS: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: Self::INITIAL_STATE,
            block: [0; 64],
            buffered: 0,
            byte_len: 0,
        }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.byte_len = self.byte_len.wrapping_add(bytes.len() as u64);
        if self.buffered > 0 {
            let copied = (64 - self.buffered).min(bytes.len());
            self.block[self.buffered..self.buffered + copied].copy_from_slice(&bytes[..copied]);
            self.buffered += copied;
            bytes = &bytes[copied..];
            if self.buffered == 64 {
                let block = self.block;
                self.compress(&block);
                self.buffered = 0;
            }
            if bytes.is_empty() {
                return;
            }
        }
        let mut chunks = bytes.chunks_exact(64);
        for chunk in &mut chunks {
            let block: &[u8; 64] = chunk.try_into().expect("exact SHA-256 block");
            self.compress(block);
        }
        let remainder = chunks.remainder();
        self.block[..remainder.len()].copy_from_slice(remainder);
        self.buffered = remainder.len();
    }

    fn finish_hex(mut self) -> String {
        let bit_len = self.byte_len.wrapping_mul(8);
        self.block[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > 56 {
            self.block[self.buffered..].fill(0);
            let block = self.block;
            self.compress(&block);
            self.block = [0; 64];
        } else {
            self.block[self.buffered..56].fill(0);
        }
        self.block[56..].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.block;
        self.compress(&block);

        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut hex = String::with_capacity(64);
        for byte in self.state.into_iter().flat_map(u32::to_be_bytes) {
            hex.push(HEX[(byte >> 4) as usize] as char);
            hex.push(HEX[(byte & 0x0f) as usize] as char);
        }
        hex
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut words = [0u32; 64];
        for (word, bytes) in words.iter_mut().take(16).zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for (constant, word) in Self::ROUND_CONSTANTS.iter().zip(words) {
            let upper_sigma = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let first = h
                .wrapping_add(upper_sigma)
                .wrapping_add(choice)
                .wrapping_add(*constant)
                .wrapping_add(word);
            let lower_sigma = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let second = lower_sigma.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(first);
            d = c;
            c = b;
            b = a;
            a = first.wrapping_add(second);
        }
        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargoless_core::{
        CandidateSnapshot, GitObjectFormat, OverlayOperation, canonical_manifest_json,
        decode_overlay_payload, parse_and_validate_manifest_json,
    };
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_repo(tag: &str) -> PathBuf {
        temp_repo_with_object_format(tag, None)
    }

    fn temp_repo_with_object_format(tag: &str, object_format: Option<&str>) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cargoless-candidate-snapshot-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut init = Command::new("git");
        init.arg("-C").arg(&root).arg("init").arg("-q");
        if let Some(object_format) = object_format {
            init.arg(format!("--object-format={object_format}"));
        }
        let output = init.env("LC_ALL", "C").output().unwrap();
        assert!(
            output.status.success(),
            "fixture Git must support object format {:?}; git init failed: {}",
            object_format,
            String::from_utf8_lossy(&output.stderr)
        );
        git(&root, &["config", "user.name", "Candidate Snapshot Test"]);
        git(
            &root,
            &["config", "user.email", "candidate-snapshot@example.invalid"],
        );
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["config", "core.hooksPath", "/dev/null"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        git(&root, &["config", "core.filemode", "true"]);
        root
    }

    fn metadata(oid: &str, advertised_size: u64) -> TreeEntryMetadata {
        TreeEntryMetadata {
            mode: "100644".to_string(),
            blob_oid: oid.to_string(),
            advertised_size,
        }
    }

    fn write(root: &Path, rel: &str, bytes: impl AsRef<[u8]>) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn git_text(root: &Path, args: &[&str]) -> String {
        String::from_utf8(git(root, args))
            .unwrap()
            .trim()
            .to_string()
    }

    fn operation<'a>(operations: &'a [OverlayOperation], path: &str) -> &'a OverlayOperation {
        operations
            .iter()
            .find(|operation| operation.path() == path)
            .unwrap_or_else(|| panic!("missing operation for {path}"))
    }

    #[test]
    fn changed_payload_plan_rejects_oversized_single_before_payload_allocation() {
        let base = BTreeMap::new();
        let candidate = BTreeMap::from([(
            "too-large.bin".to_string(),
            metadata(&"a".repeat(40), MAX_UPSERT_BYTES as u64 + 1),
        )]);

        let error = plan_changed_payloads(&base, &candidate).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("candidate_snapshot.limit_exceeded")
        );
        assert!(error.to_string().contains("32 MiB"));
    }

    #[test]
    fn changed_payload_plan_rejects_aggregate_before_payload_allocation() {
        let base = BTreeMap::new();
        let candidate = BTreeMap::from([
            (
                "first.bin".to_string(),
                metadata(&"a".repeat(40), MAX_UPSERT_BYTES as u64),
            ),
            (
                "second.bin".to_string(),
                metadata(&"b".repeat(40), MAX_UPSERT_BYTES as u64),
            ),
            ("overflow.bin".to_string(), metadata(&"c".repeat(40), 1)),
        ]);

        let error = plan_changed_payloads(&base, &candidate).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("candidate_snapshot.limit_exceeded")
        );
        assert!(error.to_string().contains("64 MiB"));
    }

    #[test]
    fn streaming_sha256_matches_canonical_hash_across_chunk_boundaries() {
        let bytes = vec![b'a'; 1_000_000];
        let mut streamed = StreamingSha256::new();
        for chunk in bytes.chunks(7_919) {
            streamed.update(chunk);
        }

        assert_eq!(streamed.finish_hex(), cargoless_core::sha256_hex(&bytes));
    }

    #[test]
    fn unchanged_large_blob_is_streamed_but_not_retained_as_overlay_payload() {
        let root = temp_repo("unchanged-large");
        let large_path = root.join("unchanged-large.bin");
        let large = std::fs::File::create(&large_path).unwrap();
        large.set_len(MAX_UPSERT_BYTES as u64 + 1).unwrap();
        write(&root, "changed.txt", b"base\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);
        write(&root, "changed.txt", b"candidate\n");

        let base_tree = git_text(&root, &["rev-parse", "HEAD^{tree}"]);
        let resolved = resolve_tree_snapshot(&root, GitObjectFormat::Sha1, &base_tree).unwrap();
        assert!(resolved.entries.contains_key("unchanged-large.bin"));
        assert!(
            resolved.retained_blobs.is_empty(),
            "tree resolution must retain no payloads"
        );

        let built = build_overlay_manifest(&root, "HEAD")
            .unwrap()
            .expect("small changed file creates an overlay");
        let CandidateSnapshot::Overlay { operations, .. } = &built.manifest.candidate else {
            panic!("producer must emit an overlay")
        };
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].path(), "changed.txt");
    }

    #[cfg(unix)]
    #[test]
    fn sha256_git_repository_produces_a_valid_complete_overlay() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_repo_with_object_format("sha256-roundtrip", Some("sha256"));
        write(&root, "delete.txt", b"delete\n");
        write(&root, "mode.sh", b"#!/bin/sh\necho base\n");
        write(&root, "unchanged.txt", b"unchanged\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);

        std::fs::remove_file(root.join("delete.txt")).unwrap();
        write(&root, "added.bin", [0, 1, 2, 0xff]);
        let mode_path = root.join("mode.sh");
        let mut permissions = std::fs::metadata(&mode_path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&mode_path, permissions).unwrap();

        let built = build_overlay_manifest(&root, "HEAD")
            .unwrap()
            .expect("SHA-256 fixture has a delta");
        assert_eq!(built.manifest.git_object_format, GitObjectFormat::Sha256);
        assert_eq!(built.manifest.comparison_base.commit_sha.len(), 64);
        assert_eq!(built.manifest.comparison_base.tree_oid.len(), 64);

        let CandidateSnapshot::Overlay {
            tree_oid,
            entries,
            operations,
            snapshot_digest,
            ..
        } = &built.manifest.candidate
        else {
            panic!("producer must emit an overlay")
        };
        assert_eq!(tree_oid.len(), 64);
        assert!(entries.iter().all(|entry| entry.blob_oid.len() == 64));
        assert!(operations.iter().all(|operation| match operation {
            OverlayOperation::Delete { base_blob_oid, .. } => base_blob_oid.len() == 64,
            OverlayOperation::Upsert { blob_oid, .. } => blob_oid.len() == 64,
        }));
        assert!(matches!(
            operation(operations, "delete.txt"),
            OverlayOperation::Delete { .. }
        ));
        assert!(matches!(
            operation(operations, "added.bin"),
            OverlayOperation::Upsert { .. }
        ));
        assert!(matches!(
            operation(operations, "mode.sh"),
            OverlayOperation::Upsert { .. }
        ));
        assert_eq!(
            compute_candidate_tree_oid(&built.manifest).unwrap(),
            *tree_oid
        );
        assert_eq!(
            compute_snapshot_digest(&built.manifest).unwrap(),
            *snapshot_digest
        );
        assert_eq!(
            compute_manifest_digest(&built.manifest).unwrap(),
            built.manifest.manifest_digest
        );
        let canonical = canonical_manifest_json(&built.manifest).unwrap();
        assert_eq!(
            parse_and_validate_manifest_json(&canonical).unwrap(),
            built.manifest
        );
    }

    #[cfg(unix)]
    #[test]
    fn temp_index_overlay_preserves_delete_empty_mode_binary_and_rename_semantics() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_repo("complete-overlay");
        write(&root, "delete.txt", b"delete me\n");
        write(&root, "empty.txt", b"not empty\n");
        write(&root, "index-proof.txt", b"base\n");
        write(&root, "mode.sh", b"#!/bin/sh\necho mode\n");
        write(&root, "old-name.txt", b"renamed bytes\n");
        write(&root, "unchanged.txt", b"same\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);

        let base_commit = git_text(&root, &["rev-parse", "HEAD"]);
        let base_tree = git_text(&root, &["rev-parse", "HEAD^{tree}"]);

        write(&root, "index-proof.txt", b"real index bytes\n");
        git(&root, &["add", "index-proof.txt"]);
        write(&root, "index-proof.txt", b"candidate worktree bytes\n");
        std::fs::remove_file(root.join("delete.txt")).unwrap();
        write(&root, "empty.txt", b"");
        std::fs::rename(root.join("old-name.txt"), root.join("new-name.txt")).unwrap();
        write(&root, "binary.bin", [0x00, 0xff, b'\n', 0x80, b'B']);
        let mode_path = root.join("mode.sh");
        let mut permissions = std::fs::metadata(&mode_path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&mode_path, permissions).unwrap();

        let real_index_before = git(&root, &["ls-files", "--stage", "-z"]);
        let real_tree_before = git_text(&root, &["write-tree"]);

        let built = build_overlay_manifest(&root, "HEAD")
            .expect("build typed overlay")
            .expect("fixture has a delta");

        assert_eq!(built.manifest.schema, "cargoless-candidate-snapshot/1");
        assert_eq!(built.manifest.git_object_format, GitObjectFormat::Sha1);
        assert_eq!(built.manifest.comparison_base.commit_sha, base_commit);
        assert_eq!(built.manifest.comparison_base.tree_oid, base_tree);
        assert_eq!(
            canonical_manifest_json(&built.manifest).unwrap(),
            canonical_manifest_json(
                &parse_and_validate_manifest_json(
                    &canonical_manifest_json(&built.manifest).unwrap()
                )
                .unwrap()
            )
            .unwrap()
        );

        let CandidateSnapshot::Overlay {
            base,
            entries,
            operations,
            operation_count,
            ..
        } = &built.manifest.candidate
        else {
            panic!("push candidate must be a typed overlay");
        };
        assert_eq!(base, &built.manifest.comparison_base);
        assert_eq!(*operation_count, operations.len() as u64);
        assert!(entries.iter().any(|entry| entry.path == "binary.bin"));
        assert!(entries.iter().any(|entry| entry.path == "empty.txt"));
        assert!(entries.iter().any(|entry| entry.path == "new-name.txt"));
        assert!(!entries.iter().any(|entry| entry.path == "delete.txt"));
        assert!(!entries.iter().any(|entry| entry.path == "old-name.txt"));

        assert!(matches!(
            operation(operations, "delete.txt"),
            OverlayOperation::Delete { .. }
        ));
        let OverlayOperation::Upsert { size, payload, .. } = operation(operations, "empty.txt")
        else {
            panic!("empty tracked file must be an upsert, not a delete");
        };
        assert_eq!(*size, 0);
        assert_eq!(decode_overlay_payload(payload).unwrap(), b"");

        let OverlayOperation::Upsert { mode, payload, .. } = operation(operations, "mode.sh")
        else {
            panic!("mode-only change must be an upsert");
        };
        assert_eq!(mode, "100755");
        assert_eq!(
            decode_overlay_payload(payload).unwrap(),
            b"#!/bin/sh\necho mode\n"
        );

        let OverlayOperation::Upsert { payload, .. } = operation(operations, "binary.bin") else {
            panic!("binary file must be carried as a typed upsert");
        };
        assert_eq!(
            decode_overlay_payload(payload).unwrap(),
            [0x00, 0xff, b'\n', 0x80, b'B']
        );
        assert!(matches!(
            operation(operations, "old-name.txt"),
            OverlayOperation::Delete { .. }
        ));
        assert!(matches!(
            operation(operations, "new-name.txt"),
            OverlayOperation::Upsert { .. }
        ));

        let candidate_bytes = match operation(operations, "index-proof.txt") {
            OverlayOperation::Upsert { payload, .. } => decode_overlay_payload(payload).unwrap(),
            OverlayOperation::Delete { .. } => panic!("index proof must be an upsert"),
        };
        assert_eq!(candidate_bytes, b"candidate worktree bytes\n");
        assert_eq!(
            git(&root, &["ls-files", "--stage", "-z"]),
            real_index_before
        );
        assert_eq!(git_text(&root, &["write-tree"]), real_tree_before);

        let resolved_base = resolve_commit_snapshot(&root, &base_commit).unwrap();
        assert_eq!(resolved_base.git_object_format, GitObjectFormat::Sha1);
        assert_eq!(resolved_base.reference.commit_sha, base_commit);
        assert_eq!(resolved_base.reference.tree_oid, base_tree);
        assert!(resolved_base.entries.contains_key("delete.txt"));
    }
}
