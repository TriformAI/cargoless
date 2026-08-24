# D-CANDIDATE-SNAPSHOT — immutable candidate overlays

Status: adopted for CGLS-47
Wire version: cargoless-candidate-snapshot/1

## Decision

Every policy-bearing check of unpublished work is bound to one immutable,
complete candidate tree. Cargoless transports a typed candidate snapshot
manifest instead of treating path/content pairs or CARGOLESS_CHANGED_FILES as
the candidate.

This fixes four identity losses in the legacy overlay:

- deletion was indistinguishable from an empty file;
- executable mode was discarded;
- binary, symlink, and gitlink identity was not represented;
- a changed-file hint could be mistaken for the complete candidate tree.

The manifest is additive. Existing exact-Git witnessing is unchanged:
source_ref remains the reachability and fetch proof, source_sha remains the
verified commit, and Cargoless continues to fetch and check out that exact
commit in an isolated scratch tree. A tree manifest describes the
already-verified candidate; it does not replace or weaken exact-Git proof.

## Ownership and dependency order

cargoless-proto owns the closed wire types. cargoless-core owns
checkout-independent validation and identity. The binary owns Git resolution,
transport, materialization, and environment lifetime.

Activation is ordered:

1. Land the types, validator, digests, and shared golden fixture dormant.
2. Produce a typed manifest client-side from NUL-safe Git plumbing.
3. Carry it unchanged through codecs and per-run state.
4. Resolve the exact base, verify the complete candidate, and materialize its
   immutable bytes server-side.
5. Only then activate candidate-backed checks such as Portal-as-YAML.

Candidate evidence is per source and non-coalescible. The legacy union overlay
may continue for checks that claim no candidate identity, but it cannot be
evidence authority for a candidate-backed policy verdict.

## Closed wire model

Every object is closed. Unknown or duplicate fields, missing fields, null,
wrong types, floating counts, and boolean counts fail closed. Conditional
fields are absent when inapplicable.

~~~text
CandidateManifestV1 = {
  "schema": "cargoless-candidate-snapshot/1",
  "git_object_format": "sha1" | "sha256",
  "comparison_base": GitTreeRef,
  "candidate": TreeCandidate | IndexCandidate | OverlayCandidate,
  "manifest_digest": Digest
}

GitTreeRef = {
  "commit_sha": GitOid,
  "tree_oid": GitOid
}

TreeCandidate = {
  "kind": "tree",
  "commit_sha": GitOid,
  "tree_oid": GitOid,
  "entry_count": Integer,
  "entries": [SnapshotEntry...],
  "snapshot_digest": Digest
}

IndexCandidate = {
  "kind": "index",
  "base": GitTreeRef,
  "tree_oid": GitOid,
  "entry_count": Integer,
  "entries": [SnapshotEntry...],
  "snapshot_digest": Digest
}

OverlayCandidate = {
  "kind": "overlay",
  "base": GitTreeRef,
  "tree_oid": GitOid,
  "entry_count": Integer,
  "entries": [SnapshotEntry...],
  "snapshot_digest": Digest,
  "operation_count": Integer,
  "operations": [Delete | Upsert...]
}

SnapshotEntry = {
  "path": CanonicalPath,
  "mode": "100644" | "100755" | "120000",
  "blob_oid": GitOid,
  "size": Integer,
  "sha256": Sha256Hex
}

Delete = {
  "op": "delete",
  "path": CanonicalPath,
  "base_mode": "100644" | "100755",
  "base_blob_oid": GitOid
}

Upsert = {
  "op": "upsert",
  "path": CanonicalPath,
  "mode": "100644" | "100755",
  "blob_oid": GitOid,
  "size": Integer,
  "sha256": Sha256Hex,
  "payload": {"encoding": "base64", "data": Base64}
}
~~~

Digest is sha256: plus exactly 64 lowercase hexadecimal characters.
Sha256Hex is exactly 64 lowercase hexadecimal characters. GitOid is exactly 40
lowercase hexadecimal characters for SHA-1 and 64 for SHA-256. Counts are
exact JSON integers and equal their array lengths.

Entries are the complete tracked candidate tree, never the changed or governed
subset and never a materialized-directory listing.

## Candidate and comparison identities

- Tree: commit_sha resolves as a commit and tree_oid is its root tree. In the
  exact-Git path commit_sha equals verified source_sha.
- Index: base is the exact HEAD commit and root tree observed while reading a
  stage-zero, non-intent-to-add index. Working-tree and untracked bytes are
  excluded.
- Overlay: base is the exact commit and root tree to which the complete,
  non-empty operation delta applies. No commit SHA is synthesized.
- Comparison base: the immutable tree used by ratchets. It is independent of
  candidate/source identity and legacy base_sha. It must be equal to or an
  ancestor of the candidate commit or base commit.

## Paths, modes, payloads, and limits

A path is non-empty NFC UTF-8, at most 4096 bytes, repository-relative, and has
no leading/trailing slash, empty/dot/dot-dot component, backslash, NUL, ASCII
control or DEL byte, or case-insensitive .git component. Case is preserved.
Entries and operations are strictly ascending by raw UTF-8 bytes and unique.
A rename is delete(old) plus upsert(new).

Modes 100644 and 100755 are supported and identity-distinct. Mode 120000 is
allowed only as an existing tree/index entry: bytes are the link-target blob
and are never dereferenced. Overlay mutation of 120000 is rejected. Mode
160000 is always rejected; every other leaf mode is invalid.

Upsert payload is canonical RFC 4648 section 4 base64: standard alphabet,
required padding, no whitespace or URL-safe alphabet, zero discarded pad bits,
and exact decode/re-encode equality. For decoded bytes B:

~~~text
size     = len(B)
blob_oid = HASH("blob " || ASCII(decimal(len(B))) || NUL || B)
sha256   = lowercase_hex(SHA256(B))
~~~

HASH follows git_object_format. Empty upsert is a zero-byte entry, not a
deletion.

| Item | v1 limit |
|---|---:|
| Raw uncompressed JSON | 128 MiB |
| Entries | 1,000,000 |
| Operations | 65,536 |
| One decoded upsert | 32 MiB |
| Sum of decoded upserts | 64 MiB |
| One path | 4096 UTF-8 bytes |
| JSON integer | 9,007,199,254,740,991 |

Limits are checked before large allocation.

## Preconditions and the entry-map seam

Delete requires an existing regular base path with exact base_mode and
base_blob_oid. Upsert requires a valid payload triple, cannot replace a
symlink or gitlink, and cannot be a mode/blob no-op. A mode-only change is an
upsert with the same blob OID and a different mode.

The core seam accepts exact BTreeMap<String, SnapshotEntry> base and candidate
maps. It requires the candidate map to equal all advertised entries, applies
operations to the base identity, and requires the result to equal the
advertised candidate. It never walks a filesystem. The Git-owning caller
separately verifies commits, commit-to-tree equality, comparison ancestry, and
object type and bytes.

After resolution the server recomputes complete sorted entries, the Git-native
root tree OID, snapshot digest, and manifest digest.

## Canonical identities

All integer framing is unsigned big-endian 64-bit.

~~~text
LP(bytes) = U64_BE(length(bytes)) || bytes

EntryRecord(e) =
    LP(UTF8(e.path))
 || LP(ASCII(e.mode))
 || LP(hex_decode(e.blob_oid))
 || U64_BE(e.size)
 || LP(hex_decode(e.sha256))

SnapshotPreimage =
    ASCII("cargoless-candidate-snapshot") || NUL || ASCII("v1") || NUL
 || LP(ASCII(git_object_format))
 || LP(hex_decode(candidate.tree_oid))
 || U64_BE(entry_count)
 || concat(EntryRecord(e) for e in entries)

snapshot_digest = "sha256:" || lowercase_hex(SHA256(SnapshotPreimage))
~~~

Kind and base are absent from SnapshotPreimage, so equivalent tree, index, and
overlay states share tree and snapshot identity.

~~~text
TreeRefRecord(r) =
    LP(hex_decode(r.commit_sha))
 || LP(hex_decode(r.tree_oid))

DeleteRecord(op) =
    LP(ASCII("delete"))
 || LP(UTF8(op.path))
 || LP(ASCII(op.base_mode))
 || LP(hex_decode(op.base_blob_oid))

UpsertRecord(op) =
    LP(ASCII("upsert"))
 || LP(UTF8(op.path))
 || LP(ASCII(op.mode))
 || LP(hex_decode(op.blob_oid))
 || U64_BE(op.size)
 || LP(hex_decode(op.sha256))

CandidateIdentityRecord =
    tree    => LP(hex_decode(candidate.commit_sha))
    index   => TreeRefRecord(candidate.base)
    overlay => TreeRefRecord(candidate.base)

ManifestPreimage =
    ASCII("cargoless-candidate-manifest") || NUL || ASCII("v1") || NUL
 || LP(ASCII(git_object_format))
 || LP(ASCII(candidate.kind))
 || TreeRefRecord(comparison_base)
 || CandidateIdentityRecord
 || LP(hex_decode(candidate.tree_oid))
 || LP(hex_decode(candidate.snapshot_digest without "sha256:"))
 || U64_BE(entry_count)
 || concat(EntryRecord(e) for e in entries)
 || U64_BE(operation_count_or_zero)
 || concat(OperationRecord(op) for op in operations)

manifest_digest = "sha256:" || lowercase_hex(SHA256(ManifestPreimage))
~~~

Payload bytes are committed through verified size, blob OID, and SHA-256.
JSON whitespace and key order do not affect identity.

Git tree OIDs use native Git tree serialization. Each immediate entry is
ASCII(mode), space, basename, NUL, and raw object OID; directory mode is 40000;
ordering compares basename bytes with slash as the directory terminator; each
body is hashed as a Git tree object.

## Shared SHA-1 vector

The normative overlay adds empty.bin as 100644 with empty bytes and script.sh
as 100755 with bytes ok plus newline to the empty tree:

~~~text
base commit      de16c5f7dd233165813ffa72719869e3181c554b
base tree        4b825dc642cb6eb9a060e54bf8d69288fbee4904
candidate tree   08d60034cad9ce340c4d42748bf0bc1b2e34d830
snapshot digest  sha256:365cc276607bc3209bd7346f8de4f765e42e68bba8fdaf1b22687b6a169118ed
manifest digest  sha256:a363a22a9ab3317a8d7d616ecb4ac66ef7d0f2d7dd46d8a1010f44a601b8377c
~~~

The exact JSON is pinned in
crates/cargoless-core/tests/candidate_snapshot_contract.rs and must remain
byte-identical across producer and consumer repositories.

## Protected execution lifetime

The canonical manifest persisted under daemon state is a lifecycle artifact,
not the pathname authority consumed by a policy child. Immediately before each
command spawn, Cargoless opens that file once with no symlink following,
validates the opened regular file's recorded device/inode, link count, mode,
and canonical bytes, then confirms the named artifact still denotes that same
file. A path replacement or byte change fails as
`candidate_snapshot.environment_unsafe` before child execution.

On Linux, the child authority is a fresh `memfd` containing those exact
canonical bytes and sealed against writes, shrink, growth, and further seal
changes. The daemon retains `CLOEXEC`; only the intended post-fork child clears
it immediately before exec. The exported path is `/proc/self/fd/N`, and the
daemon holds the descriptor through successful spawn. Platforms without a
native immutable descriptor fail closed; a mutable named-file or `/dev/fd`
fallback is not a candidate authority.

Candidate state requires an explicitly configured external daemon state root.
The root is canonical, owner-controlled, and not group/world writable. Typed
candidate scratch and sidecars use the dedicated `candidate-project-check-runs`
and `candidate-snapshots` namespaces; both namespaces and each unpredictable
per-run directory are exclusive mode 0700 directories with an exact canonical
parent. The separate `project-check-runs` namespace remains legacy/exact-Git
authority and may retain its safe historical 0755 mode. Cargoless records each
run directory's device/inode. Cleanup first
atomically renames the recorded object to an unpredictable quarantine name in
the verified namespace, verifies that the moved object retains the recorded
identity, and only then recursively removes it. A substituted path is
preserved and candidate execution fails as `candidate_snapshot.cleanup_failed`.
Legacy and exact-Git cleanup remains best-effort telemetry and does not replace
the already-computed result.

Startup validates the external root, both typed namespaces, and every
recoverable typed run before deleting either candidate scratch or candidate
sidecar state. Unsafe external state fails startup without deletion. The
repo-relative default keeps typed candidates disabled, leaves both typed
namespaces untouched, and retains legacy/exact-Git `project-check-runs`
recovery.

## Stable failure semantics and rollout proof

Failures use candidate_snapshot.* codes. They distinguish JSON and schema
errors, invalid OIDs and paths, order and duplicates, limits, base/comparison
identity, index/object failures, unsupported modes, payload triple failures,
delete/no-op preconditions, entry/tree/digest mismatches, missing manifests,
changed-hint mismatch, unsafe environments, ambient access, and forbidden
coalescing. Callers branch on CandidateSnapshotError.code; message is
actionable context, not a discriminator.

Consumer activation requires all of:

- shared tree, snapshot, and manifest golden identities pass byte-for-byte;
- closed JSON and canonical paths fail closed;
- delete versus empty, executable bit, binary payload, rename normalization,
  SHA-256 repository, and comparison-base independence pass;
- symlink/gitlink, payload, precondition, ordering, digest, ambient-access, and
  coalescing negatives return stable codes;
- existing exact-Git witnesses remain green;
- deployed producer and server prove one real candidate-backed check.
