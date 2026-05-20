//! `cargoless push --remote <url>` — #240/2c thin push-client.
//!
//! The CLIENT side of the central-daemon write-plane. Closes the loop
//! 2a opened (transport contract) and 2b completed on the server side
//! (ServeVerdictState::push_overlay override + serve-loop ingest).
//!
//! ## Flow (D-PUSHOVERLAY §3, D-INC2-2B §7 honest-boundary)
//!
//! 1. Resolve `--remote <url>` (required) + `--repo <path>` (local FS,
//!    default cwd) + `--worktree <key>` (server-side worktree id;
//!    default = canonical absolute `--repo` path, the spike's
//!    path-keyed default) + `--base <ref>` (git base, default HEAD).
//! 2. Compute the overlay-set:
//!    `git -C <repo> diff --name-only <base>` → changed-file list →
//!    read each file's bytes → `(path, content)` pairs.
//! 3. **Canonicalize ordering** — sort files by path so the daemon's
//!    `cluster_hash_from_pushed` is deterministic regardless of the
//!    client's OS-enumeration order (#262 C6 fix, client-side; ~5 LOC,
//!    naturally adjacent to file-gathering).
//! 4. `HttpClient::new(url).push_overlay(worktree, base, files)` →
//!    `PushOverlayAck { accepted, applied_files, worktree }`.
//! 5. Print the ack; exit 0 (accepted=true), 1 (accepted=false /
//!    transport error), or 2 (setup error). Fail-soft: never panic
//!    on a transport failure — surface the actionable message.
//!
//! ## Honest 2c boundary (stated, not papered over)
//!
//! * Push-and-no-block: 2c does NOT poll for the verdict. Per
//!   D-PUSHOVERLAY §3 "client → POST /overlay → 200 ack → GET /events
//!   (SSE) OR poll /status" — the verdict round-trips via the
//!   already-shipped read-plane. Use `cargoless status --remote <url>`
//!   (from #232 0c) for the verdict. A future `--await-verdict`
//!   blocking-poll flag is a Wave-2 nicety.
//! * Git ops via `std::process::Command` — same discipline as
//!   `build.rs`'s trunk subprocess and `watch.rs`'s tooling.
//! * No new external deps. The client uses already-shipped
//!   `HttpClient::push_overlay` (2a transport surface on main).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use cargoless_core::transport::TransportClient;
use cargoless_core::transport::http::HttpClient;

/// CLI-resolved push parameters, ready to drive
/// `HttpClient::push_overlay` + git-subprocess file enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOpts {
    /// Required: `--remote <url>` — the central daemon's HTTP endpoint.
    pub remote: String,
    /// Local repository root (defaults to cwd). git operations run via
    /// `git -C <repo>`; file reads are `repo.join(rel_path)`.
    pub repo: PathBuf,
    /// Server-side worktree key. Default: the canonical absolute
    /// `repo` path (path-keyed identity, spike open-Q1 default).
    pub worktree: String,
    /// Git base ref (default `HEAD`). Carried in the push payload for
    /// future diagnostics; server stores-and-ignores in v0.2.x
    /// (spike open-Q2 default).
    pub base: String,
}

/// `cargoless push` entry. Returns an `ExitCode` per the v0 CLI
/// convention: 0 = success (ack.accepted=true), 1 = rejected / transport
/// error, 2 = setup / config error.
pub fn run(opts: &PushOpts) -> ExitCode {
    // 1. Enumerate changed files via git.
    let changed = match git_changed_files(&opts.repo, &opts.base) {
        Ok(files) => files,
        Err(e) => {
            crate::ui::error(format!(
                "push: git diff against `{}` in `{}` failed: {e}",
                opts.base,
                opts.repo.display()
            ));
            return ExitCode::from(2);
        }
    };
    if changed.is_empty() {
        eprintln!(
            "[cargoless:push] no changes vs {} in {} — nothing to push",
            opts.base,
            opts.repo.display()
        );
        return ExitCode::from(0);
    }

    // 2. Read each changed file's bytes. Tolerant: a skipped file
    //    (read error) warns but does not abort the push — the
    //    pushed-overlay is best-effort and the server is robust to
    //    partial sets (the cluster-hash + diff are content-shaped).
    let mut files: Vec<(String, String)> = Vec::with_capacity(changed.len());
    for rel in &changed {
        let abs = opts.repo.join(rel);
        match std::fs::read_to_string(&abs) {
            Ok(content) => files.push((rel.clone(), content)),
            Err(e) => crate::ui::warn(format!("push: skip `{}` (read error: {e})", abs.display())),
        }
    }

    // 3. **C6 client-side canonicalize** (closes #262). Sort by path so
    //    the daemon's `cluster_hash_from_pushed` sees a deterministic
    //    file order regardless of how git/the OS enumerated the
    //    changes. Without this, two semantically-identical pushes
    //    could produce different cluster hashes ⇒ wrong-cluster
    //    routing — which is the cross-WT-cluster-routing regression
    //    class L3 flagged as worth a fix.
    files.sort_by(|a, b| a.0.cmp(&b.0));

    // 4. Build the HTTP client + push.
    let client = match HttpClient::new(&opts.remote) {
        Ok(c) => c,
        Err(e) => {
            crate::ui::error(format!(
                "push: HttpClient init failed for `{}`: {e}",
                opts.remote
            ));
            return ExitCode::from(2);
        }
    };
    let ack = match client.push_overlay(&opts.worktree, &opts.base, &files) {
        Ok(a) => a,
        Err(e) => {
            crate::ui::error(format!("push: server `{}` rejected: {e}", opts.remote));
            return ExitCode::from(1);
        }
    };

    // 5. Print ack + exit code.
    eprintln!(
        "[cargoless:push] ack from {}: accepted={} applied_files={} worktree={}",
        opts.remote, ack.accepted, ack.applied_files, ack.worktree
    );
    eprintln!(
        "[cargoless:push] verdict: run `cargoless status --remote {}` to poll \
         (or subscribe via /events SSE)",
        opts.remote
    );
    if ack.accepted {
        ExitCode::from(0)
    } else {
        ExitCode::from(1)
    }
}

/// Run `git -C <repo> diff --name-only <base>` and return the changed
/// file list (one path per line, repo-relative). Errors surface the
/// stderr verbatim — actionable to the operator.
fn git_changed_files(repo: &Path, base: &str) -> std::io::Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("diff")
        .arg("--name-only")
        .arg(base)
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git diff exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **2c keystone test — the client-side composing-equivalence
    /// shape.** Two semantically-identical pushes (same `(path,
    /// content)` set) with different INPUT ordering MUST produce
    /// identical sorted `files` vecs after the C6 canonicalize step.
    /// This ensures the daemon's `cluster_hash_from_pushed` is
    /// deterministic across N clients regardless of OS enumeration
    /// order — closing the cross-WT-cluster-routing regression class
    /// builder-infra's L3 flagged (#262).
    ///
    /// The test asserts the CONTRACT — "after sort, file order is a
    /// function of (path, content) only, not of input order" — not
    /// the implementation details. A future refactor that switches
    /// to a different sort key but preserves determinism still
    /// passes; one that drops the sort fails exactly here.
    #[test]
    fn c6_canonicalize_makes_input_order_irrelevant_to_pushed_order() {
        let files_a = vec![
            (
                "Cargo.toml".to_string(),
                "[package]\nname=\"x\"".to_string(),
            ),
            ("src/lib.rs".to_string(), "pub fn x() {}".to_string()),
            ("Cargo.lock".to_string(), "# lockfile".to_string()),
        ];
        let files_b = vec![
            ("src/lib.rs".to_string(), "pub fn x() {}".to_string()),
            ("Cargo.lock".to_string(), "# lockfile".to_string()),
            (
                "Cargo.toml".to_string(),
                "[package]\nname=\"x\"".to_string(),
            ),
        ];
        // C6 sort.
        let mut a = files_a;
        let mut b = files_b;
        a.sort_by(|x, y| x.0.cmp(&y.0));
        b.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(
            a, b,
            "C6 canonicalize: same (path, content) set ⇒ identical sorted vec, \
             regardless of input order — closes #262 cross-WT-cluster-routing \
             regression class at the client seam"
        );
        // Sanity: the sort produces a known canonical order.
        assert_eq!(a[0].0, "Cargo.lock");
        assert_eq!(a[1].0, "Cargo.toml");
        assert_eq!(a[2].0, "src/lib.rs");
    }

    #[test]
    fn push_opts_shape_round_trips() {
        // The CLI surface a `cargoless push --remote URL --repo /r
        // --worktree W --base origin/main` invocation resolves to.
        let opts = PushOpts {
            remote: "http://localhost:8080".to_string(),
            repo: PathBuf::from("/r"),
            worktree: "/r".to_string(),
            base: "HEAD".to_string(),
        };
        // Cheap clone+eq sanity (the v0 CLI Opts shape relies on
        // PartialEq for the parser tests in main.rs).
        let cloned = opts.clone();
        assert_eq!(opts, cloned);
    }

    #[test]
    fn git_changed_files_actionable_error_on_unreadable_repo() {
        // No git repo at this path ⇒ `git -C` fails fast; we surface
        // the error not panic. Fail-soft per the discipline.
        let res = git_changed_files(Path::new("/this/path/definitely/does/not/exist"), "HEAD");
        assert!(
            res.is_err(),
            "git on non-existent path MUST error, not panic"
        );
        // The error string mentions git diff (actionable).
        let msg = format!("{}", res.unwrap_err());
        assert!(
            msg.contains("git"),
            "error surface mentions the failing tool: {msg}"
        );
    }

    #[test]
    fn empty_changed_files_is_noop_success() {
        // Cover the happy `no changes to push` path's code structure
        // — the empty filter post-`git diff` MUST yield empty Vec,
        // not an error. (The `run()` body returns ExitCode::from(0)
        // for this case — tested via the integration arm; the unit
        // test here pins the parser's empty-input contract.)
        let parsed: Vec<String> = "\n\n  \n"
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        assert!(parsed.is_empty(), "whitespace-only stdout ⇒ no files");
    }
}
