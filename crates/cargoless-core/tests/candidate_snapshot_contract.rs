use std::collections::BTreeMap;

use cargoless_core::candidate_snapshot::{
    canonical_manifest_json, compute_candidate_tree_oid, decode_overlay_payload,
    parse_and_validate_manifest_json, validate_manifest_against_entry_maps,
};
use cargoless_proto::candidate_snapshot::{CandidateSnapshot, OverlayOperation, SnapshotEntry};

const GOLDEN: &str = r#"{
  "schema": "cargoless-candidate-snapshot/1",
  "git_object_format": "sha1",
  "comparison_base": {
    "commit_sha": "de16c5f7dd233165813ffa72719869e3181c554b",
    "tree_oid": "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
  },
  "candidate": {
    "kind": "overlay",
    "base": {
      "commit_sha": "de16c5f7dd233165813ffa72719869e3181c554b",
      "tree_oid": "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
    },
    "tree_oid": "08d60034cad9ce340c4d42748bf0bc1b2e34d830",
    "entry_count": 2,
    "entries": [
      {
        "path": "empty.bin",
        "mode": "100644",
        "blob_oid": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        "size": 0,
        "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
      },
      {
        "path": "script.sh",
        "mode": "100755",
        "blob_oid": "9766475a4185a151dc9d56d614ffb9aaea3bfd42",
        "size": 3,
        "sha256": "dc51b8c96c2d745df3bd5590d990230a482fd247123599548e0632fdbf97fc22"
      }
    ],
    "snapshot_digest": "sha256:365cc276607bc3209bd7346f8de4f765e42e68bba8fdaf1b22687b6a169118ed",
    "operation_count": 2,
    "operations": [
      {
        "op": "upsert",
        "path": "empty.bin",
        "mode": "100644",
        "blob_oid": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        "size": 0,
        "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "payload": {"encoding": "base64", "data": ""}
      },
      {
        "op": "upsert",
        "path": "script.sh",
        "mode": "100755",
        "blob_oid": "9766475a4185a151dc9d56d614ffb9aaea3bfd42",
        "size": 3,
        "sha256": "dc51b8c96c2d745df3bd5590d990230a482fd247123599548e0632fdbf97fc22",
        "payload": {"encoding": "base64", "data": "b2sK"}
      }
    ]
  },
  "manifest_digest": "sha256:a363a22a9ab3317a8d7d616ecb4ac66ef7d0f2d7dd46d8a1010f44a601b8377c"
}"#;

const GOLDEN_COMPACT: &str = concat!(
    r#"{"schema":"cargoless-candidate-snapshot/1","git_object_format":"sha1","comparison_base":{"commit_sha":"de16c5f7dd233165813ffa72719869e3181c554b","tree_oid":"4b825dc642cb6eb9a060e54bf8d69288fbee4904"},"candidate":{"kind":"overlay","base":{"commit_sha":"de16c5f7dd233165813ffa72719869e3181c554b","tree_oid":"4b825dc642cb6eb9a060e54bf8d69288fbee4904"},"tree_oid":"08d60034cad9ce340c4d42748bf0bc1b2e34d830","entry_count":2,"entries":[{"path":"empty.bin","mode":"100644","blob_oid":"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391","size":0,"sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"},{"path":"script.sh","mode":"100755","blob_oid":"9766475a4185a151dc9d56d614ffb9aaea3bfd42","size":3,"sha256":"dc51b8c96c2d745df3bd5590d990230a482fd247123599548e0632fdbf97fc22"}],"snapshot_digest":"sha256:365cc276607bc3209bd7346f8de4f765e42e68bba8fdaf1b22687b6a169118ed","operation_count":2,"operations":[{"op":"upsert","path":"empty.bin","mode":"100644","blob_oid":"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391","size":0,"sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","payload":{"encoding":"base64","data":""}},{"op":"upsert","path":"script.sh","mode":"100755","blob_oid":"9766475a4185a151dc9d56d614ffb9aaea3bfd42","size":3,"sha256":"dc51b8c96c2d745df3bd5590d990230a482fd247123599548e0632fdbf97fc22","payload":{"encoding":"base64","data":"b2sK"}}]},"manifest_digest":"sha256:a363a22a9ab3317a8d7d616ecb4ac66ef7d0f2d7dd46d8a1010f44a601b8377c"}"#,
);

fn code(json: &str) -> &'static str {
    parse_and_validate_manifest_json(json)
        .expect_err("fixture must fail closed")
        .code
}

fn replace_first(source: &str, from: &str, to: &str) -> String {
    assert!(source.contains(from), "fixture mutation target is absent");
    source.replacen(from, to, 1)
}

fn replace_last(source: &str, from: &str, to: &str) -> String {
    let offset = source
        .rfind(from)
        .expect("fixture mutation target is absent");
    format!(
        "{}{}{}",
        &source[..offset],
        to,
        &source[offset + from.len()..]
    )
}

#[test]
fn shared_sha1_golden_vector_is_byte_exact() {
    let manifest = parse_and_validate_manifest_json(GOLDEN).expect("shared vector validates");
    assert_eq!(
        compute_candidate_tree_oid(&manifest).unwrap(),
        "08d60034cad9ce340c4d42748bf0bc1b2e34d830"
    );
    assert_eq!(
        manifest.candidate.snapshot_digest(),
        "sha256:365cc276607bc3209bd7346f8de4f765e42e68bba8fdaf1b22687b6a169118ed"
    );
    assert_eq!(
        manifest.manifest_digest,
        "sha256:a363a22a9ab3317a8d7d616ecb4ac66ef7d0f2d7dd46d8a1010f44a601b8377c"
    );
    assert_eq!(canonical_manifest_json(&manifest).unwrap(), GOLDEN_COMPACT);

    let candidate: BTreeMap<_, _> = manifest
        .candidate
        .entries()
        .iter()
        .cloned()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    validate_manifest_against_entry_maps(&manifest, Some(&BTreeMap::new()), &candidate)
        .expect("entry-map seam verifies the complete overlay");
}

#[test]
fn json_is_closed_duplicate_safe_and_order_independent() {
    let duplicate = replace_first(
        GOLDEN,
        r#""schema": "cargoless-candidate-snapshot/1","#,
        r#""schema": "cargoless-candidate-snapshot/1", "schema": "cargoless-candidate-snapshot/1","#,
    );
    assert_eq!(code(&duplicate), "candidate_snapshot.json_duplicate_key");

    let unknown = replace_first(
        GOLDEN,
        r#""git_object_format": "sha1","#,
        r#""unexpected": 1, "git_object_format": "sha1","#,
    );
    assert_eq!(code(&unknown), "candidate_snapshot.field_invalid");

    let missing_schema = replace_first(
        GOLDEN,
        r#"  "schema": "cargoless-candidate-snapshot/1",
"#,
        "",
    );
    assert_eq!(
        code(&missing_schema),
        "candidate_snapshot.schema_unsupported"
    );

    let reordered = format!(
        r#"{{"manifest_digest":{},"candidate":{},"comparison_base":{},"git_object_format":"sha1","schema":"cargoless-candidate-snapshot/1"}}"#,
        serde_json::to_string(
            &serde_json::from_str::<serde_json::Value>(GOLDEN).unwrap()["manifest_digest"]
        )
        .unwrap(),
        serde_json::to_string(
            &serde_json::from_str::<serde_json::Value>(GOLDEN).unwrap()["candidate"]
        )
        .unwrap(),
        serde_json::to_string(
            &serde_json::from_str::<serde_json::Value>(GOLDEN).unwrap()["comparison_base"]
        )
        .unwrap(),
    );
    let parsed = parse_and_validate_manifest_json(&reordered).expect("JSON key order is semantic");
    assert_eq!(canonical_manifest_json(&parsed).unwrap(), GOLDEN_COMPACT);
}

#[test]
fn paths_counts_oids_modes_and_order_fail_closed() {
    for invalid_path in [
        "/absolute",
        "../escape",
        "a//b",
        "a\\b",
        ".git/config",
        "e\u{301}.txt",
        "control\u{7f}",
    ] {
        let mutated = replace_first(GOLDEN, "empty.bin", invalid_path);
        assert_eq!(
            code(&mutated),
            "candidate_snapshot.path_noncanonical",
            "path={invalid_path:?}"
        );
    }

    let uppercase_oid = replace_first(
        GOLDEN,
        "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
        "E69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
    );
    assert_eq!(code(&uppercase_oid), "candidate_snapshot.oid_invalid");

    let wrong_count = replace_first(GOLDEN, r#""entry_count": 2"#, r#""entry_count": 3"#);
    assert_eq!(code(&wrong_count), "candidate_snapshot.entries_mismatch");

    let unsupported_mode = replace_first(GOLDEN, r#""mode": "100644""#, r#""mode": "160000""#);
    assert_eq!(
        code(&unsupported_mode),
        "candidate_snapshot.gitlink_unsupported"
    );

    let unsorted = replace_first(GOLDEN, "empty.bin", "z-empty.bin");
    assert_eq!(code(&unsorted), "candidate_snapshot.entry_order");
}

#[test]
fn payloads_are_canonical_and_bound_to_size_sha_and_git_oid() {
    let manifest = parse_and_validate_manifest_json(GOLDEN).unwrap();
    let CandidateSnapshot::Overlay { operations, .. } = &manifest.candidate else {
        panic!("golden is overlay")
    };
    let OverlayOperation::Upsert { payload, .. } = &operations[1] else {
        panic!("second operation is upsert")
    };
    assert_eq!(decode_overlay_payload(payload).unwrap(), b"ok\n");

    let bad_base64 = replace_first(GOLDEN, r#""data": "b2sK""#, r#""data": "b2s_""#);
    assert_eq!(
        code(&bad_base64),
        "candidate_snapshot.payload_base64_invalid"
    );

    let bad_size = replace_last(
        GOLDEN,
        r#""size": 3,
        "sha256": "dc51"#,
        r#""size": 4,
        "sha256": "dc51"#,
    );
    assert_eq!(code(&bad_size), "candidate_snapshot.payload_size_mismatch");

    let bad_sha = replace_last(
        GOLDEN,
        "dc51b8c96c2d745df3bd5590d990230a482fd247123599548e0632fdbf97fc22",
        "ac51b8c96c2d745df3bd5590d990230a482fd247123599548e0632fdbf97fc22",
    );
    assert_eq!(code(&bad_sha), "candidate_snapshot.payload_sha256_mismatch");

    let bad_oid = replace_last(
        GOLDEN,
        "9766475a4185a151dc9d56d614ffb9aaea3bfd42",
        "0766475a4185a151dc9d56d614ffb9aaea3bfd42",
    );
    assert_eq!(code(&bad_oid), "candidate_snapshot.payload_oid_mismatch");
}

#[test]
fn overlay_entry_map_seam_distinguishes_delete_empty_mode_and_stale_base() {
    let manifest = parse_and_validate_manifest_json(GOLDEN).unwrap();
    let candidate: BTreeMap<String, SnapshotEntry> = manifest
        .candidate
        .entries()
        .iter()
        .cloned()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    validate_manifest_against_entry_maps(&manifest, Some(&BTreeMap::new()), &candidate).unwrap();
    assert_eq!(
        candidate["empty.bin"].size, 0,
        "empty upsert remains present"
    );

    let mut wrong_candidate = candidate.clone();
    wrong_candidate.remove("empty.bin");
    assert_eq!(
        validate_manifest_against_entry_maps(&manifest, Some(&BTreeMap::new()), &wrong_candidate)
            .unwrap_err()
            .code,
        "candidate_snapshot.entries_mismatch"
    );

    let mut stale_base = BTreeMap::new();
    stale_base.insert("script.sh".into(), candidate["script.sh"].clone());
    assert_eq!(
        validate_manifest_against_entry_maps(&manifest, Some(&stale_base), &candidate)
            .unwrap_err()
            .code,
        "candidate_snapshot.operation_noop"
    );
}
