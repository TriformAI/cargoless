//! `cargoless checks run --base <ref>` must pass a real changed-file set into
//! the project-check scheduler. This is the shared-gate resource guard: branch
//! protection should not run every configured check for a docs/YAML-only diff.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn run(root: &Path, program: &str, args: &[&str]) {
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap_or_else(|e| panic!("spawn {program}: {e}"));
    assert!(status.success(), "{program} {args:?} failed: {status}");
}

fn fixture() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "cargoless-checks-base-prune-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "checks-base-prune"
version = "0.0.0"
edition = "2021"
"#,
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").unwrap();
    fs::write(root.join("docs/readme.md"), "ok\n").unwrap();
    fs::write(
        root.join("cargoless.checks.yaml"),
        r#"
version: 1
checks:
  - id: rust-surface
    kind: required_patterns
    inputs: ["src/**/*.rs"]
    patterns:
      - code: rust.ok
        literal: "ok"
        message: missing ok
  - id: docs-surface
    kind: required_patterns
    inputs: ["docs/**/*.md"]
    patterns:
      - code: docs.ok
        literal: "ok"
        message: missing ok
"#,
    )
    .unwrap();
    run(&root, "git", &["init", "-q"]);
    run(
        &root,
        "git",
        &["config", "user.email", "cargoless@example.invalid"],
    );
    run(&root, "git", &["config", "user.name", "Cargoless Test"]);
    run(&root, "git", &["add", "."]);
    run(&root, "git", &["commit", "-q", "-m", "baseline"]);
    fs::write(root.join("docs/readme.md"), "ok\nchanged\n").unwrap();
    root
}

#[test]
fn checks_run_base_prunes_untriggered_checks() {
    let bin = env!("CARGO_BIN_EXE_cargoless");
    let root = fixture();
    let out = Command::new(bin)
        .arg("checks")
        .arg("--root")
        .arg(&root)
        .arg("run")
        .arg("--profile")
        .arg("dev")
        .arg("--base")
        .arg("HEAD")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn cargoless");
    let combined = format!(
        "{}\n--- stderr ---\n{}\n--- stdout ---\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(out.status.code(), Some(0), "{combined}");
    assert!(
        combined.contains("1 check evaluated, 1 skipped"),
        "diff-scoped check run should skip the unrelated Rust check: {combined}"
    );
    assert!(
        combined.contains("scope=changed base=HEAD changed_paths=1 skipped_untriggered=1"),
        "verdict should expose the changed-file pruning scope: {combined}"
    );
    let _ = fs::remove_dir_all(&root);
}
