use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8(out.stdout).ok()?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../cargoless/src");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    let sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = git(&["status", "--porcelain", "--untracked-files=normal"])
        .map(|status| {
            if status.is_empty() {
                "false".to_string()
            } else {
                "true".to_string()
            }
        })
        .unwrap_or_else(|| "unknown".to_string());
    let build_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());

    println!("cargo:rustc-env=CARGOLESS_GIT_SHA={sha}");
    println!("cargo:rustc-env=CARGOLESS_GIT_DIRTY={dirty}");
    println!("cargo:rustc-env=CARGOLESS_BUILD_UNIX={build_ts}");
}
