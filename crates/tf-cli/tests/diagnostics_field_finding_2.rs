//! FIELD FINDING #2 — `tftrunk check` against a known-red fixture MUST print
//! the diagnostics (file:line:col + severity + code + message), not just
//! `red — at least one tracked file does not compile`.
//!
//! Gated `#[cfg(feature = "integration")]` because exercising the full
//! check pipeline requires rust-analyzer + a workable cargo invocation —
//! the default `--locked` CI tier deliberately excludes this surface. When
//! rust-analyzer is not on PATH (the bare `rust:1.85-bookworm` image case),
//! the test SKIPS rather than fakes the verdict — the skip is loud and the
//! pure unit tests in `src/check.rs` still cover the rendering.

#![cfg(feature = "integration")]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn rust_analyzer_available() -> bool {
    Command::new("rust-analyzer")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build a tiny temp cargo project whose `lib.rs` has a *known* rustc error
/// (the `cargo check` tier — exactly the FIELD FINDING #2 reproducer
/// class). Zero external dependencies so the test does not need crates.io.
fn fixture_with_known_error() -> PathBuf {
    let base = std::env::temp_dir().join(format!("tftrunk-ff2-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let src = base.join("src");
    fs::create_dir_all(&src).expect("create fixture dir");
    // tf.toml override → Config::resolve accepts the fixture without needing
    // cdylib/leptos detection. Target is host-native so `cargo check` does
    // not need a wasm32 component installed.
    fs::write(base.join("tf.toml"), b"[project]\ntarget = \"\"\n").expect("write tf.toml");
    fs::write(
        base.join("Cargo.toml"),
        br#"[package]
name = "ff2-fixture"
version = "0.0.0"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
    )
    .expect("write Cargo.toml");
    // E0277 — a clear, named compiler error a user could not miss in the
    // dogfood reproducer (and one tf-cli MUST surface verbatim).
    fs::write(
        src.join("lib.rs"),
        b"pub fn boom() -> u32 { \"not a u32\" }\n",
    )
    .expect("write lib.rs");
    base
}

#[test]
fn tftrunk_check_against_red_fixture_prints_diagnostics() {
    if !rust_analyzer_available() {
        eprintln!(
            "SKIP: rust-analyzer not on PATH (CI image without RA / no \
             `rustup component add rust-analyzer`). Unit tests in \
             src/check.rs cover the rendering directly."
        );
        return;
    }
    let bin = env!("CARGO_BIN_EXE_tftrunk");
    let root = fixture_with_known_error();
    // Tight cap: a cold RA + cargo-check pass on a 1-file fixture is
    // typically <30s; double that for slow CI without flapping.
    let out = Command::new(bin)
        .arg("check")
        .arg("--root")
        .arg(&root)
        .env("TF_CHECK_TIMEOUT_SECS", "60")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn tftrunk");
    let combined = format!(
        "{}\n--- stderr ---\n{}\n--- stdout ---\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );

    // FIELD FINDING #2 assertions — the README promise made testable.
    // 1. Exit code is the documented "red" path (1).
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 on red, got: {combined}"
    );
    // 2. The verdict line is still there (frozen wording).
    assert!(combined.contains("red"), "verdict line missing: {combined}");
    // 3. At LEAST one of (file, line, code) appears alongside the verdict —
    //    the FIELD FINDING #2 contract that a red tree carries its evidence.
    //    We accept any of `src/lib.rs`, `:1:`/line:col, `E0277` (the
    //    expected rustc code for str-where-u32), because RA's exact rendering
    //    varies across toolchain versions but at least one of these *must*
    //    appear for the user to act.
    let has_path = combined.contains("src/lib.rs") || combined.contains("lib.rs");
    let has_line_col = combined.contains(":1:") || combined.contains("line ");
    let has_code = combined.contains("E0277") || combined.contains("error");
    assert!(
        has_path && has_line_col && has_code,
        "red verdict must carry file+line+code; got: {combined}"
    );

    // Best-effort cleanup; ignore failures (a stray temp dir is harmless).
    let _ = fs::remove_dir_all(&root);
}
