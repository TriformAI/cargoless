//! `cargoless preview` — the self-serve preview client.
//!
//! One command an agent runs in its worktree to get a live preview of the
//! current branch on a remote `app-serve` daemon. It is a thin client over the
//! daemon's control plane (`POST /instances` to register, `DELETE
//! /instances/<name>` to tear down) plus a poll of `/app` to follow the build
//! to green — no local cargo, no k8s. The daemon owns the build, the proxy, and
//! (via the Part-2 reconciler) the public `<name>.<domain>` route.
//!
//! Preconditions it checks up front, failing with a clear message rather than a
//! confusing remote error:
//! 1. the branch is **pushed** (the daemon serves a ref it can `git fetch`);
//! 2. a `cargoless.app.yaml` exists at the worktree (else every build is red).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use cargoless_core::appmanifest::load_app_manifest;
use cargoless_core::transport::http::HttpClient;

use crate::push::git_resolve_ref;
use crate::ui;

/// Typed options for `cargoless preview` (built from the CLI flags in `main`).
#[derive(Debug, Clone)]
pub struct PreviewOpts {
    /// `--remote <url>` — the app-serve daemon control plane.
    pub remote: String,
    /// Bearer token (CLI `--auth-token` or `CARGOLESS_AUTH_TOKEN`).
    pub auth_token: Option<String>,
    /// Repo root (the worktree the agent is in).
    pub repo: PathBuf,
    /// `--name <name>` — explicit preview name; default derived from branch.
    pub name: Option<String>,
    /// `--ref <ref>` — git ref the daemon tracks; default `origin/<branch>`.
    pub git_ref: Option<String>,
    /// `--env KEY=VALUE` repeated — extra non-secret env for the app child.
    pub env: Vec<String>,
    /// `--remove` — tear the preview down instead of registering it.
    pub remove: bool,
    /// `--own-db` — request an isolated per-branch DB (daemon may decline).
    pub own_db: bool,
    /// `--no-wait` — register and return without polling for green.
    pub no_wait: bool,
    /// `--ttl <secs>` — preview lifetime in seconds; the daemon auto-removes the
    /// preview when it expires. `None` ⇒ the daemon's default TTL.
    pub ttl_secs: Option<u64>,
}

/// Max time to follow a preview build before giving up the poll (the preview
/// keeps building daemon-side; only the client stops watching).
const FOLLOW_TIMEOUT: Duration = Duration::from_secs(1800);
const POLL_INTERVAL: Duration = Duration::from_secs(3);

pub fn run(opts: &PreviewOpts) -> std::process::ExitCode {
    use std::process::ExitCode;

    let client = match build_client(opts) {
        Ok(c) => c,
        Err(code) => return code,
    };

    // Resolve the preview name: explicit, else derived from the branch.
    let branch = match current_branch(&opts.repo) {
        Some(b) => b,
        None => {
            // Detached HEAD with no --name is unnameable.
            if opts.name.is_none() {
                ui::error(
                    "preview: could not determine the current branch (detached HEAD?) — \
                     pass --name <name>",
                );
                return ExitCode::from(2);
            }
            String::new()
        }
    };
    let name = opts
        .name
        .clone()
        .unwrap_or_else(|| branch.clone())
        .trim()
        .to_string();
    if name.is_empty() {
        ui::error("preview: empty preview name");
        return ExitCode::from(2);
    }

    // ── teardown path ────────────────────────────────────────────────
    if opts.remove {
        return match client.remove_preview(&name) {
            Ok(()) => {
                ui::ok(format!("preview `{name}` removed"));
                ExitCode::from(0)
            }
            Err(e) => {
                ui::error(format!("preview: remove `{name}` failed: {e}"));
                ExitCode::from(1)
            }
        };
    }

    // ── register path ────────────────────────────────────────────────
    // The ref the daemon tracks: explicit, else origin/<branch>.
    let git_ref = opts
        .git_ref
        .clone()
        .unwrap_or_else(|| format!("origin/{branch}"));

    // Precondition 1: a manifest must exist at the worktree, else every build
    // is red ("no cargoless.app.yaml at this sha"). Validate locally first.
    match load_app_manifest(&opts.repo) {
        Ok(Some(_)) => {}
        Ok(None) => {
            ui::error(format!(
                "preview: no cargoless.app.yaml at {} — app-serve has nothing to build",
                opts.repo.display()
            ));
            return ExitCode::from(2);
        }
        Err(e) => {
            ui::error(format!(
                "preview: invalid cargoless.app.yaml: {}",
                e.message
            ));
            return ExitCode::from(2);
        }
    }

    // Precondition 2: the branch must be pushed (the daemon fetches the ref).
    // Only meaningful for the origin/<branch> default; skip for an explicit ref.
    if opts.git_ref.is_none() && !branch.is_empty() && !branch_on_origin(&opts.repo, &branch) {
        ui::error(format!(
            "preview: branch `{branch}` is not on origin — push it first \
             (git push -u origin {branch})"
        ));
        return ExitCode::from(2);
    }

    // Local HEAD, purely informational for the operator.
    if let Ok(sha) = git_resolve_ref(&opts.repo, "HEAD") {
        ui::step(format!(
            "preview `{name}` → {git_ref} (local HEAD {})",
            short(&sha)
        ));
    }

    let env = parse_env_pairs(&opts.env);
    if let Err(msg) = env {
        ui::error(format!("preview: {msg}"));
        return ExitCode::from(2);
    }
    let env = env.expect("checked Ok above");

    if let Err(e) = client.register_preview(&name, &git_ref, &env, opts.own_db, opts.ttl_secs) {
        ui::error(format!("preview: register `{name}` failed: {e}"));
        return ExitCode::from(1);
    }
    match opts.ttl_secs {
        Some(ttl) => ui::ok(format!(
            "preview `{name}` registered (auto-removes in ~{ttl}s; re-run to renew)"
        )),
        None => ui::ok(format!(
            "preview `{name}` registered (auto-removes after the daemon's default TTL; \
             re-run to renew, or `--remove` to tear down now)"
        )),
    }

    if opts.no_wait {
        return ExitCode::from(0);
    }

    follow(&client, &name)
}

/// Poll `/app` and report the preview's phase transitions until it serves, goes
/// red, or the follow times out.
fn follow(client: &HttpClient, name: &str) -> std::process::ExitCode {
    use std::process::ExitCode;

    ui::wait(format!("following `{name}` — building…"));
    let start = Instant::now();
    let mut last_phase = String::new();
    while start.elapsed() < FOLLOW_TIMEOUT {
        match client.app_report() {
            Ok(Some(json)) => {
                if let Some(inst) = find_instance(&json, name) {
                    let phase = inst
                        .get("phase")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if phase != last_phase {
                        ui::step(format!("`{name}`: {phase}"));
                        last_phase = phase.clone();
                    }
                    // Serving (or probing while already serving) ⇒ green & up.
                    if inst
                        .get("serving_sha")
                        .and_then(|x| x.as_str())
                        .is_some_and(|s| !s.is_empty())
                    {
                        let host = inst
                            .get("public_host")
                            .and_then(|x| x.as_str())
                            .filter(|s| !s.is_empty());
                        match host {
                            Some(h) => ui::ok(format!("preview `{name}` live at https://{h}")),
                            None => ui::ok(format!(
                                "preview `{name}` serving (no public domain configured — \
                                 use the daemon's proxy port / port-forward)"
                            )),
                        }
                        return ExitCode::from(0);
                    }
                    // A fresh red for this build: report and stop following.
                    if let Some(reason) = inst
                        .get("last_red_reason")
                        .and_then(|x| x.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        if phase == "idle" {
                            ui::error(format!("preview `{name}` build failed: {reason}"));
                            return ExitCode::from(1);
                        }
                    }
                } else {
                    // Not yet visible — the control loop registers on its next
                    // tick; keep polling.
                }
            }
            Ok(None) => {
                ui::error("preview: remote is not an app-serve daemon (/app 404)");
                return ExitCode::from(1);
            }
            Err(e) => {
                ui::warn(format!("preview: /app poll error (retrying): {e}"));
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    ui::warn(format!(
        "preview `{name}` still building after {}s — it continues daemon-side; \
         re-check with: curl <remote>/app",
        FOLLOW_TIMEOUT.as_secs()
    ));
    ExitCode::from(0)
}

/// Build the bearer-or-open HTTP client (mirrors `push::run`).
fn build_client(opts: &PreviewOpts) -> Result<HttpClient, std::process::ExitCode> {
    let built = match opts.auth_token.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(token) => HttpClient::with_token(&opts.remote, token),
        None => HttpClient::new(&opts.remote),
    };
    built.map_err(|e| {
        ui::error(format!(
            "preview: HttpClient init failed for `{}`: {e}",
            opts.remote
        ));
        std::process::ExitCode::from(2)
    })
}

/// Find the instance object for `name` in the `/app` JSON.
fn find_instance(json: &str, name: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("instances")?
        .as_array()?
        .iter()
        .find(|i| i.get("name").and_then(|x| x.as_str()) == Some(name))
        .cloned()
}

/// Parse `KEY=VALUE` env pairs from the `--env` flags.
fn parse_env_pairs(raw: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::with_capacity(raw.len());
    for item in raw {
        let (k, v) = item
            .split_once('=')
            .ok_or_else(|| format!("--env `{item}` must be KEY=VALUE"))?;
        if k.trim().is_empty() {
            return Err(format!("--env `{item}` has an empty key"));
        }
        out.push((k.trim().to_string(), v.to_string()));
    }
    Ok(out)
}

/// The current branch short name, or `None` on detached HEAD / error.
fn current_branch(repo: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // `HEAD` (detached) is not a usable branch name.
    if name.is_empty() || name == "HEAD" {
        None
    } else {
        Some(name)
    }
}

/// Is `branch` present on `origin`? (`git ls-remote --exit-code`.)
fn branch_on_origin(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-remote", "--exit-code", "--heads", "origin", branch])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_pairs_splits_and_rejects() {
        assert_eq!(
            parse_env_pairs(&["A=1".into(), "B=x=y".into()]).unwrap(),
            vec![("A".into(), "1".into()), ("B".into(), "x=y".into())]
        );
        assert!(parse_env_pairs(&["noeq".into()]).is_err());
        assert!(parse_env_pairs(&["=v".into()]).is_err());
    }

    #[test]
    fn find_instance_picks_the_named_row() {
        let json = r#"{"instances":[
            {"name":"dev","phase":"serving","serving_sha":"g1"},
            {"name":"feat","phase":"building","public_host":"feat.tryform.wtf"}
        ],"ready":true}"#;
        let feat = find_instance(json, "feat").unwrap();
        assert_eq!(feat["phase"], "building");
        assert_eq!(feat["public_host"], "feat.tryform.wtf");
        assert!(find_instance(json, "nope").is_none());
    }

    #[test]
    fn short_caps_at_twelve() {
        assert_eq!(short("0123456789abcdef"), "0123456789ab");
        assert_eq!(short("abc"), "abc");
    }
}
