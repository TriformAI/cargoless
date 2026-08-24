//! Git-backed construction of complete candidate-snapshot manifests.

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
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cargoless-candidate-snapshot-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
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
