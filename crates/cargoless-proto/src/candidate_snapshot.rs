//! Frozen cross-crate contract for an immutable candidate tree snapshot.
//!
//! The wire format is `cargoless-candidate-snapshot/1`. Every object is
//! closed: serde rejects unknown and duplicate fields instead of silently
//! discarding identity-bearing input.

use serde::{Deserialize, Serialize};

pub const CANDIDATE_SNAPSHOT_SCHEMA_V1: &str = "cargoless-candidate-snapshot/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitObjectFormat {
    Sha1,
    Sha256,
}

impl GitObjectFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }

    pub const fn oid_hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTreeRef {
    pub commit_sha: String,
    pub tree_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotEntry {
    pub path: String,
    pub mode: String,
    pub blob_oid: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayPayload {
    pub encoding: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum OverlayOperation {
    Delete {
        path: String,
        base_mode: String,
        base_blob_oid: String,
    },
    Upsert {
        path: String,
        mode: String,
        blob_oid: String,
        size: u64,
        sha256: String,
        payload: OverlayPayload,
    },
}

impl OverlayOperation {
    pub fn path(&self) -> &str {
        match self {
            Self::Delete { path, .. } | Self::Upsert { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CandidateSnapshot {
    Tree {
        commit_sha: String,
        tree_oid: String,
        entry_count: u64,
        entries: Vec<SnapshotEntry>,
        snapshot_digest: String,
    },
    Index {
        base: GitTreeRef,
        tree_oid: String,
        entry_count: u64,
        entries: Vec<SnapshotEntry>,
        snapshot_digest: String,
    },
    Overlay {
        base: GitTreeRef,
        tree_oid: String,
        entry_count: u64,
        entries: Vec<SnapshotEntry>,
        snapshot_digest: String,
        operation_count: u64,
        operations: Vec<OverlayOperation>,
    },
}

impl CandidateSnapshot {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Tree { .. } => "tree",
            Self::Index { .. } => "index",
            Self::Overlay { .. } => "overlay",
        }
    }

    pub fn tree_oid(&self) -> &str {
        match self {
            Self::Tree { tree_oid, .. }
            | Self::Index { tree_oid, .. }
            | Self::Overlay { tree_oid, .. } => tree_oid,
        }
    }

    pub const fn entry_count(&self) -> u64 {
        match self {
            Self::Tree { entry_count, .. }
            | Self::Index { entry_count, .. }
            | Self::Overlay { entry_count, .. } => *entry_count,
        }
    }

    pub fn entries(&self) -> &[SnapshotEntry] {
        match self {
            Self::Tree { entries, .. }
            | Self::Index { entries, .. }
            | Self::Overlay { entries, .. } => entries,
        }
    }

    pub fn snapshot_digest(&self) -> &str {
        match self {
            Self::Tree {
                snapshot_digest, ..
            }
            | Self::Index {
                snapshot_digest, ..
            }
            | Self::Overlay {
                snapshot_digest, ..
            } => snapshot_digest,
        }
    }

    pub fn base(&self) -> Option<&GitTreeRef> {
        match self {
            Self::Tree { .. } => None,
            Self::Index { base, .. } | Self::Overlay { base, .. } => Some(base),
        }
    }

    pub fn operations(&self) -> &[OverlayOperation] {
        match self {
            Self::Overlay { operations, .. } => operations,
            Self::Tree { .. } | Self::Index { .. } => &[],
        }
    }

    pub const fn operation_count(&self) -> u64 {
        match self {
            Self::Overlay {
                operation_count, ..
            } => *operation_count,
            Self::Tree { .. } | Self::Index { .. } => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateSnapshotManifest {
    pub schema: String,
    pub git_object_format: GitObjectFormat,
    pub comparison_base: GitTreeRef,
    pub candidate: CandidateSnapshot,
    pub manifest_digest: String,
}
