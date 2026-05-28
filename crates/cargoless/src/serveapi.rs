//! Increment 0 (Model R #10 read-plane wiring) — the live serve-loop's
//! [`VerdictService`].
//!
//! v0.2.0 shipped a **complete, exhaustively-unit-tested transport library**
//! ([`cargoless_core::transport`]: the logical [`VerdictService`] +
//! in-proc/Unix/HTTP adapters + the `--remote` discovery chain + the #14
//! auth seam) that **nothing in the binary wires**. This module is the
//! missing wire on the *server* side: a [`VerdictService`] backed by the
//! serve-loop's live per-worktree verdict state, so `serve --repo --bind
//! <addr>` actually exposes the shipped HTTP+SSE surface.
//!
//! ## Faithful-composition discipline (NOT a transport reshape)
//!
//! The transport contract (`transport/{mod,http,discovery,inproc}.rs`) is
//! frozen and unit-tested; this is *wiring*, not redesign. The load-bearing
//! property is reused, not weakened:
//!
//! * **Single verdict site preserved (Judgment B as composed).** servedrv
//!   already attributes a verdict at EXACTLY ONE site —
//!   `servedrv::publish_verdict`, the sole `ClusterAction::EmitVerdict`
//!   arm. [`ServeVerdictState::publish`] is called *from that same one
//!   site*, alongside the existing durable `statusfile::write`. We do NOT
//!   introduce a second verdict-attribution path — the in-memory service
//!   and the SSE bus are a faithful *mirror* of the one authoritative
//!   write-plane, so the proven `#189`/`#198` composition story is intact.
//! * **Subscribe-emit from the same one site (0b).** The transition-event
//!   fan-out happens in `publish` too — one event per real verdict,
//!   never a fabricated one.
//!
//! ## Honest Increment-0 boundary (stated, not papered over)
//!
//! `red_diagnostics` is `0` and `crates` is empty here — *exactly* as the
//! existing `statusfile`/`publish_verdict` v0 path already writes them
//! (servedrv's `Status` carries `red_diagnostics: 0, crates: Vec::new()`).
//! Per-crate roll-up (#9 `cratemap`) and queryable diagnostics retention
//! (#11 `diagnostics_store`) are real surfaces but their *serve-loop
//! wiring* is a later increment; mirroring the same zeros the durable path
//! already emits keeps the read-plane consistent with the write-plane
//! rather than fabricating detail the loop does not yet compute.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use cargoless_core::Diagnostic;
use cargoless_core::transport::{
    CheckProfile, DaemonActivity, PushOverlayAck, PushOverlayOptions, TransitionEvent,
    VerdictService, WorktreeStatus, WorktreeSummary,
};

/// Poison-tolerant lock (same discipline as `model::poisoned` /
/// `inproc::testmock`): a panicked verdict path must not wedge the read
/// plane — recover the guard and carry on (best-effort transport ethos).
fn poisoned<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A pushed overlay set carried in `ServeVerdictState::pushed`. Stored
/// pair-shape (`Vec<(String, String)>`) instead of [`OverlaySet`] so the
/// consumer in servedrv.rs's `SwitchOverlay` arm can re-build with
/// `OverlaySet::from_pairs(pushed.files)` — byte-identical to the FS
/// path's construction (the composing-equivalence assertion 2b's
/// load-bearing test pins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushedOverlay {
    /// Client-supplied base_ref (typically e.g. `origin/main`). v0.2.x:
    /// stored for diagnostics + future "diff vs base_ref" features;
    /// server does NOT act on it in 2b (spike open-question #2 default).
    pub base_ref: String,
    /// Whole-file `(path, content)` pairs — the same shape the FS path
    /// builds via `std::fs::read_to_string` per file.
    pub files: Vec<(String, String)>,
    /// Server-side root for central-daemon pushes. When set, the serve loop
    /// uses this as the rust-analyzer workspace root while keeping `worktree`
    /// as the client-visible status key.
    pub analysis_root: Option<PathBuf>,
    /// Client's resolved base SHA, diagnostics-only. The server fetch/reset
    /// result remains authoritative.
    pub base_sha: Option<String>,
    /// Unix timestamp of the push receipt. Diagnostics-only for 2b;
    /// future idle-evict policy (Wave-2) reads this.
    pub last_push_unix: u64,
    /// Repo-relative files changed by the client diff. Project-check
    /// trigger filtering uses this instead of the overlay file list because
    /// overlays include extra workspace config files for cluster routing.
    pub changed_files: Option<Vec<String>>,
    /// Optional per-push Cargo check profile. This lets a single
    /// repo-scoped daemon accept tf-multiverse's per-invocation
    /// `check-remote` selectors without restarting RA per package.
    pub check_profile: Option<CheckProfile>,
    /// `true` ⇒ this push requested the authoritative witness-gated verdict
    /// (warn-fast/witness-gated hybrid). When set, the serve loop runs the
    /// project-check witness and publishes the gated verdict even if the
    /// daemon default mode is `warn`. Default `false` preserves warn behavior.
    pub gate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectCheckRunContext {
    pub root: PathBuf,
    pub changed_files: Option<Vec<String>>,
    pub base_ref: String,
    pub overlay_files: Vec<(String, String)>,
    pub materialize_overlay: bool,
    /// `true` ⇒ the originating push requested the witness-gated verdict; the
    /// serve loop forces hard-mode behavior for this worktree's verdict.
    pub gate: bool,
}

/// The serve-loop's live verdict state, presented as the shipped logical
/// [`VerdictService`]. `Send + Sync` (the trait demands it so the
/// HTTP/Unix adapters can share one service across connection threads):
/// the four `Mutex`-guarded fields satisfy that by construction.
#[derive(Default)]
pub struct ServeVerdictState {
    /// worktree-key → last published status. Keyed by the SAME string
    /// `servedrv::publish_verdict` uses (`wt.to_string_lossy()`), so a
    /// remote `get_status(<wt>)` resolves the exact tree the loop
    /// attributed.
    statuses: Mutex<BTreeMap<String, WorktreeStatus>>,
    /// Live transition-event subscribers (the SSE / in-proc fan-out).
    /// Retain-on-send like `model`'s buses so a dropped subscriber never
    /// stalls the (single) producer.
    subs: Mutex<Vec<Sender<TransitionEvent>>>,
    /// #240/2b — pushed-overlay store. worktree-key →
    /// [`PushedOverlay`]. Populated by `push_overlay` (the
    /// [`VerdictService`] write-plane ingest), consumed once by
    /// `take_overlay_for` (the serve loop's SwitchOverlay arm). The
    /// `take` is **pop-on-consume semantic** (spike open-question #3
    /// default): once consumed, the WT falls back to the FS path until
    /// a fresh push arrives. Per-WT serialization (a new push for the
    /// same WT REPLACES the prior overlay before consumption) is the
    /// natural BTreeMap semantic.
    pushed: Mutex<BTreeMap<String, PushedOverlay>>,
    /// Serializes central-daemon mirror fetch/reset operations. The HTTP
    /// adapter can accept several requests concurrently; the checked-out
    /// mirror is one mutable filesystem and must move one base at a time.
    sync_lock: Mutex<()>,
    /// #240/2b — push-arrival signal channel. The serve loop drains
    /// this alongside ctrl_rx; each received worktree-key is the
    /// wakeup signal that a push needs servicing. `Option<Sender>`
    /// because `new()` constructs without a channel; the loop wires
    /// one in via [`Self::attach_push_signal`] at startup, BEFORE
    /// `HttpServer::bind` exposes `push_overlay` to clients (so no
    /// push can race the channel-not-yet-attached window).
    push_signal: Mutex<Option<Sender<String>>>,
    /// Admin drain state. A restart requests quiesce through HTTP; after
    /// that, new pushes are refused while accepted pushed worktrees stay
    /// active until their next authoritative verdict is published.
    drain: Mutex<DrainState>,
    /// Project-check context captured when a pushed overlay is consumed.
    /// The verdict arm runs later, after rust-analyzer settles, so the
    /// changed-file trigger set and central-daemon analysis root need a
    /// small handoff store keyed by the client-visible worktree.
    project_check_context: Mutex<BTreeMap<String, ProjectCheckRunContext>>,
}

#[derive(Default)]
struct DrainState {
    quiescing: bool,
    active_worktrees: BTreeSet<String>,
}

impl ServeVerdictState {
    /// Construct empty. Returns `Self` (NOT `Arc<Self>`) on purpose —
    /// `fn new() -> Arc<Self>` trips `clippy::new_ret_no_self` under the
    /// `-D warnings` gate; callers wrap in `Arc` (the house pattern, cf.
    /// `inproc::testmock::MockService`).
    pub fn new() -> Self {
        Self::default()
    }

    /// The SOLE verdict-mirror entry point — invoked from servedrv's one
    /// `publish_verdict` (the `ClusterAction::EmitVerdict` arm, Judgment B
    /// as composed), right after the durable `statusfile::write`. Updates
    /// the in-memory status map AND fans out one [`TransitionEvent`]
    /// (subscribe-emit, plan 0b). One real verdict ⇒ one map update ⇒ one
    /// event; never a fabricated transition.
    pub fn publish(&self, wt: &Path, authoritative_error: bool) {
        let worktree = wt.to_string_lossy().into_owned();
        let verdict = if authoritative_error { "red" } else { "green" };
        let published_at = crate::statusfile::now_unix();
        let status = WorktreeStatus {
            worktree: worktree.clone(),
            verdict: verdict.to_string(),
            daemon_build_id: cargoless_core::build_id().to_string(),
            // Honest Inc-0 boundary: identical to the zeros the durable
            // `statusfile`/`publish_verdict` path already writes (see
            // module doc). Not fabricated detail.
            crates: Vec::new(),
            red_diagnostics: 0,
            // Freshly published ⇒ age computed at read time (get_status)
            // from `published_at` so a remote reader sees an honest age.
            heartbeat_age_secs: 0,
            published_at,
        };
        poisoned(&self.statuses).insert(worktree.clone(), status);
        let ev = TransitionEvent {
            worktree: worktree.clone(),
            verdict: verdict.to_string(),
            red_diagnostics: 0,
            published_at,
        };
        poisoned(&self.subs).retain(|s| s.send(ev.clone()).is_ok());
        self.mark_worktree_published(&worktree);
    }

    /// #240/2b — wire the push-arrival signal channel. Called ONCE by
    /// the serve loop at startup, BEFORE `HttpServer::bind` exposes the
    /// `push_overlay` ingest route. After this, every `push_overlay`
    /// call sends the WT key on `tx`; the serve loop's drain wakes up
    /// and synthesizes a `DriverEvent::RoutedBatch` for that WT.
    ///
    /// **Best-effort by construction:** a wedged `tx` (closed receiver)
    /// produces a silent send-error; the push is still STORED in
    /// `pushed`, only the wakeup is lost. The next push or activity
    /// tick will eventually surface the stored overlay — the
    /// fail-soft transport ethos applied to the write-plane wakeup.
    pub fn attach_push_signal(&self, tx: Sender<String>) {
        *poisoned(&self.push_signal) = Some(tx);
    }

    /// #240/2b — consume-semantic reader for the SwitchOverlay arm.
    /// Returns the pushed overlay for `wt_key` (matching
    /// `wt.to_string_lossy()` from servedrv) AND removes it from the
    /// store. If no push is pending, returns `None` and the SwitchOverlay
    /// arm falls through to the FS-read path. The pop-on-consume
    /// semantic (spike open-question #3 default) means each push
    /// services exactly one SwitchOverlay cycle; FS path resumes if no
    /// fresh push arrives.
    pub fn take_overlay_for(&self, wt_key: &str) -> Option<PushedOverlay> {
        poisoned(&self.pushed).remove(wt_key)
    }

    /// #240/2b — non-consuming peek. Used by the serve loop's first-push
    /// cluster-hash derivation (`cluster_hash_from_pushed`) which needs
    /// to read the pushed workspace-config files WITHOUT consuming the
    /// overlay (the consume happens later in the SwitchOverlay arm via
    /// `take_overlay_for`). Returns a clone; the store is unchanged.
    pub fn peek_overlay_for(&self, wt_key: &str) -> Option<PushedOverlay> {
        poisoned(&self.pushed).get(wt_key).cloned()
    }

    /// Server-side analysis root for a pending pushed overlay, if the client
    /// supplied one. The serve loop uses this before consuming the overlay so
    /// first-push cluster spawn uses the daemon's mirror path, not the
    /// client's pod-local worktree key.
    pub fn analysis_root_for(&self, wt_key: &str) -> Option<PathBuf> {
        poisoned(&self.pushed)
            .get(wt_key)
            .and_then(|p| p.analysis_root.clone())
    }

    /// Record the per-worktree project-check context the `EmitVerdict` arm
    /// later consumes. Takes the built [`ProjectCheckRunContext`] by value
    /// rather than its fields one-by-one — the field set grew past clippy's
    /// `too_many_arguments` threshold (and a bag of positional args mirroring
    /// a struct's fields is exactly what that lint warns against).
    pub(crate) fn record_project_check_context(
        &self,
        worktree: &str,
        context: ProjectCheckRunContext,
    ) {
        poisoned(&self.project_check_context).insert(worktree.to_string(), context);
    }

    pub(crate) fn take_project_check_context(
        &self,
        worktree: &str,
    ) -> Option<ProjectCheckRunContext> {
        poisoned(&self.project_check_context).remove(worktree)
    }

    pub(crate) fn with_project_check_overlay<T>(
        &self,
        context: &ProjectCheckRunContext,
        f: impl FnOnce(&Path) -> T,
    ) -> Result<T, String> {
        if !context.materialize_overlay {
            return Ok(f(&context.root));
        }

        let _guard = poisoned(&self.sync_lock);
        reset_analysis_root(&context.root, &context.base_ref)?;
        materialize_overlay_files(&context.root, &context.overlay_files)?;
        let result = f(&context.root);
        if let Err(e) = reset_analysis_root(&context.root, &context.base_ref) {
            eprintln!(
                "[cargoless:obs] project-check-overlay-cleanup root={} error={}",
                context.root.display(),
                e
            );
        }
        Ok(result)
    }

    pub fn quiescing(&self) -> bool {
        poisoned(&self.drain).quiescing
    }

    pub fn drain_complete(&self) -> bool {
        let drain = poisoned(&self.drain);
        drain.quiescing && drain.active_worktrees.is_empty() && poisoned(&self.pushed).is_empty()
    }

    fn mark_push_active(&self, worktree: &str) -> bool {
        let mut drain = poisoned(&self.drain);
        if drain.quiescing {
            return false;
        }
        drain.active_worktrees.insert(worktree.to_string());
        true
    }

    fn mark_worktree_published(&self, worktree: &str) {
        poisoned(&self.drain).active_worktrees.remove(worktree);
    }

    fn activity_snapshot(&self) -> DaemonActivity {
        let drain = poisoned(&self.drain);
        DaemonActivity {
            quiescing: drain.quiescing,
            active_worktrees: drain.active_worktrees.len() as u32,
            pending_pushes: poisoned(&self.pushed).len() as u32,
        }
    }
}

impl VerdictService for ServeVerdictState {
    fn get_status(&self, worktree: &str) -> Option<WorktreeStatus> {
        let g = poisoned(&self.statuses);
        let mut s = g.get(worktree).cloned()?;
        // Age is derived at read time from the publish timestamp — the
        // stored `heartbeat_age_secs` is a placeholder; the honest age is
        // "seconds since this verdict was attributed".
        let now = crate::statusfile::now_unix();
        s.heartbeat_age_secs = now.saturating_sub(s.published_at);
        Some(s)
    }

    fn get_verdict(&self, worktree: &str) -> Option<String> {
        poisoned(&self.statuses)
            .get(worktree)
            .map(|s| s.verdict.clone())
    }

    fn get_diagnostics(&self, _worktree: &str) -> Vec<Diagnostic> {
        // Honest Inc-0 boundary: the serve loop does not yet thread
        // `diagnostics_store` retention (a later increment). Empty here is
        // the *correct* answer for the state the loop computes — never a
        // fabricated diagnostic. (`get_diagnostics` empty ⇒ "no detail",
        // the same contract `transport` documents for green/unknown.)
        Vec::new()
    }

    fn list_worktrees(&self) -> Vec<WorktreeSummary> {
        poisoned(&self.statuses)
            .values()
            .map(|s| WorktreeSummary {
                worktree: s.worktree.clone(),
                verdict: s.verdict.clone(),
                daemon_build_id: s.daemon_build_id.clone(),
                red_diagnostics: s.red_diagnostics,
            })
            .collect()
    }

    fn subscribe(&self) -> Receiver<TransitionEvent> {
        let (tx, rx) = channel();
        poisoned(&self.subs).push(tx);
        rx
    }

    /// #240/2b — overlay-push ingest. The WRITE-PLANE entry for the
    /// pushed-mode central-daemon topology (D-PUSHOVERLAY §2.4 / §4).
    ///
    /// 1. Record the `(base_ref, files)` pair in the per-WT pushed
    ///    store. A subsequent push for the same WT REPLACES (latest
    ///    wins; per-WT serialization is the natural BTreeMap semantic).
    /// 2. Signal the serve loop via the attached `push_signal` channel
    ///    (best-effort: a wedged send leaves the overlay stored, only
    ///    the wakeup is lost). The loop synthesizes a
    ///    `DriverEvent::RoutedBatch` for this WT, which feeds the
    ///    proven core EXACTLY as if it came from the FS watcher path
    ///    — same event shape, no new emission seam.
    /// 3. Return an ack: `accepted=true` + `applied_files` count. The
    ///    ack does NOT block on the verdict; the client uses the
    ///    already-shipped subscribe (SSE) or `get_status` for the
    ///    verdict (D-PUSHOVERLAY §2.3 — no new verdict-egress surface).
    fn push_overlay(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
    ) -> PushOverlayAck {
        self.push_overlay_with_profile(worktree, base_ref, files, None)
    }

    fn push_overlay_with_profile(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
        check_profile: Option<&CheckProfile>,
    ) -> PushOverlayAck {
        self.push_overlay_with_options(worktree, base_ref, files, check_profile, None)
    }

    fn push_overlay_with_options(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
        check_profile: Option<&CheckProfile>,
        options: Option<&PushOverlayOptions>,
    ) -> PushOverlayAck {
        if self.quiescing() {
            return rejected_push(worktree, "daemon is quiescing");
        }
        let mut mapped_files = files.to_vec();
        let mut analysis_root = None;
        let mut base_sha = None;
        let mut changed_files = None;
        let mut gate = false;
        if let Some(options) = options {
            changed_files = options.changed_files.clone();
            gate = options.gate;
            analysis_root = options
                .analysis_root
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from);
            base_sha = options
                .base_sha
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            if options.repo_relative {
                let Some(root) = analysis_root.as_ref() else {
                    return rejected_push(worktree, "repo-relative push missing analysis_root");
                };
                mapped_files = match map_repo_relative_files(root, files) {
                    Ok(files) => files,
                    Err(e) => return rejected_push(worktree, &e),
                };
            }

            if let Some(root) = analysis_root.as_ref() {
                let base = base_ref.trim();
                if !base.is_empty() {
                    let _guard = poisoned(&self.sync_lock);
                    if let Err(e) = sync_analysis_root(root, base) {
                        return rejected_push(worktree, &e);
                    }
                }
            }
        }

        if !self.mark_push_active(worktree) {
            return rejected_push(worktree, "daemon is quiescing");
        }

        let applied_files = files.len() as u32;
        let pushed = PushedOverlay {
            base_ref: base_ref.to_string(),
            files: mapped_files,
            analysis_root,
            base_sha,
            last_push_unix: crate::statusfile::now_unix(),
            changed_files,
            check_profile: check_profile.cloned(),
            gate,
        };
        poisoned(&self.pushed).insert(worktree.to_string(), pushed);
        // Wake the serve loop (best-effort — see attach_push_signal doc).
        if let Some(tx) = poisoned(&self.push_signal).as_ref() {
            let _ = tx.send(worktree.to_string());
        }
        PushOverlayAck {
            worktree: worktree.to_string(),
            accepted: true,
            applied_files,
        }
    }

    fn daemon_activity(&self) -> DaemonActivity {
        self.activity_snapshot()
    }

    fn request_quiesce(&self) -> DaemonActivity {
        {
            let mut drain = poisoned(&self.drain);
            drain.quiescing = true;
        }
        self.activity_snapshot()
    }
}

fn rejected_push(worktree: &str, why: &str) -> PushOverlayAck {
    eprintln!("[cargoless:push] rejected worktree={worktree}: {why}");
    PushOverlayAck {
        worktree: worktree.to_string(),
        accepted: false,
        applied_files: 0,
    }
}

fn map_repo_relative_files(
    root: &Path,
    files: &[(String, String)],
) -> Result<Vec<(String, String)>, String> {
    files
        .iter()
        .map(|(path, content)| {
            let rel = safe_repo_relative_path(path)?;
            Ok((
                root.join(rel).to_string_lossy().into_owned(),
                content.clone(),
            ))
        })
        .collect()
}

fn safe_repo_relative_path(path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(format!("repo-relative push carried absolute path `{path}`"));
    }
    let mut out = PathBuf::new();
    for component in p.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("repo-relative path escapes repo root: `{path}`"));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err("repo-relative push carried an empty path".to_string());
    }
    Ok(out)
}

fn sync_analysis_root(root: &Path, base_ref: &str) -> Result<(), String> {
    if !root.join(".git").exists() {
        return Err(format!(
            "analysis_root `{}` is not a git checkout",
            root.display()
        ));
    }
    let fetch_ref = base_ref
        .strip_prefix("origin/")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(base_ref);
    run_git(root, &["fetch", "--prune", "origin", fetch_ref])?;
    reset_analysis_root(root, base_ref)?;
    Ok(())
}

fn reset_analysis_root(root: &Path, base_ref: &str) -> Result<(), String> {
    run_git(root, &["reset", "--hard", base_ref])?;
    run_git(root, &["clean", "-fd", "-e", ".cargoless"])?;
    Ok(())
}

fn materialize_overlay_files(root: &Path, files: &[(String, String)]) -> Result<(), String> {
    for (path, content) in files {
        let path = Path::new(path);
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        if !abs.starts_with(root) {
            return Err(format!(
                "overlay path `{}` escapes analysis_root `{}`",
                abs.display(),
                root.display()
            ));
        }
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "could not create overlay parent `{}`: {e}",
                    parent.display()
                )
            })?;
        }
        std::fs::write(&abs, content)
            .map_err(|e| format!("could not materialize overlay `{}`: {e}", abs.display()))?;
    }
    Ok(())
}

fn run_git(root: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| {
            format!(
                "git {:?} in `{}` failed to start: {e}",
                args,
                root.display()
            )
        })?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "git {:?} in `{}` exited {:?}: {}",
        args,
        root.display(),
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use cargoless_core::transport::http::{HttpClient, HttpServer};
    use cargoless_core::transport::{
        AllowAll, CargoSubcommand, PushOverlayOptions, TransportClient, VerdictService,
    };

    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cargoless-serveapi-{label}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// THE Increment-0 GATE differential test: a **remote** read of the
    /// real [`ServeVerdictState`] (over the shipped HTTP+SSE adapter) is
    /// byte-equivalent to the **local** in-proc read for the SAME tree
    /// state — across a GREEN→RED transition — AND the subscribe-emit
    /// (0b) delivers identical [`TransitionEvent`]s on both the in-proc
    /// receiver and the HTTP SSE receiver. Run against the production
    /// `ServeVerdictState`, not a mock — this proves the *wire*, which is
    /// what Increment 0 ships.
    #[test]
    fn remote_verdict_equiv_local_for_same_tree_state_and_subscribe_emits() {
        let api = Arc::new(ServeVerdictState::new());
        let wt = Path::new("/repo/wt-a");
        let key = wt.to_string_lossy().into_owned();

        // Local (in-proc) subscriber, registered before any publish.
        let local_rx = api.subscribe();

        // Real HTTP server over the real ServeVerdictState (#10 posture:
        // AllowAll — the auth seam is exercised separately in transport's
        // own unit suite; here we prove the verdict wire).
        let srv = HttpServer::bind(
            "127.0.0.1:0",
            Arc::clone(&api) as Arc<dyn VerdictService>,
            Arc::new(AllowAll),
        )
        .expect("bind ephemeral");
        std::thread::sleep(Duration::from_millis(50));
        let client =
            HttpClient::new(&format!("http://{}", srv.addr())).expect("client for ephemeral addr");
        // Remote SSE subscriber (server-side svc.subscribe()).
        let remote_rx = client.subscribe().expect("remote subscribe");
        std::thread::sleep(Duration::from_millis(80)); // subscriber registers

        // ── tree state 1: GREEN ──────────────────────────────────────
        api.publish(wt, /*authoritative_error=*/ false);
        let local_v = api.get_verdict(&key);
        let remote_v = client.get_verdict(&key).expect("remote get_verdict");
        assert_eq!(local_v.as_deref(), Some("green"), "local sees GREEN");
        assert_eq!(
            remote_v, local_v,
            "remote verdict ≡ local verdict for the same tree state (GREEN)"
        );
        let lev = local_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("local transition event");
        let rev = remote_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("remote SSE transition event");
        assert_eq!(lev.verdict, "green");
        assert_eq!(
            rev, lev,
            "remote TransitionEvent ≡ local TransitionEvent (subscribe-emit, 0b)"
        );

        // ── tree state 2: RED (same wt — a real transition) ───────────
        api.publish(wt, /*authoritative_error=*/ true);
        let local_s = api.get_status(&key).map(|s| s.verdict);
        let remote_s = client
            .get_status(&key)
            .expect("remote get_status")
            .map(|s| s.verdict);
        assert_eq!(local_s.as_deref(), Some("red"), "local sees RED");
        assert_eq!(
            remote_s, local_s,
            "remote status verdict ≡ local for the same tree state (RED)"
        );
        let lev2 = local_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let rev2 = remote_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(lev2.verdict, "red");
        assert_eq!(
            rev2, lev2,
            "the GREEN→RED transition is mirrored remote ≡ local"
        );

        // Unknown worktree resolves identically (None) on both transports
        // — the 404/None path is part of "remote ≡ local".
        assert_eq!(api.get_verdict("nope"), None);
        assert_eq!(client.get_verdict("nope").unwrap(), None);

        // list_worktrees agrees across the wire.
        let local_list = api.list_worktrees();
        let remote_list = client.list_worktrees().expect("remote list");
        assert_eq!(local_list, remote_list, "list_worktrees remote ≡ local");
        assert_eq!(local_list.len(), 1);
        assert_eq!(local_list[0].verdict, "red");

        drop(srv);
    }

    #[test]
    fn get_diagnostics_is_honest_empty_inc0_boundary() {
        // The stated Inc-0 boundary, pinned: no fabricated diagnostics —
        // empty is the correct answer for the state the loop computes.
        let api = ServeVerdictState::new();
        api.publish(Path::new("/r/wt"), true);
        assert!(
            api.get_diagnostics("/r/wt").is_empty(),
            "Inc-0: diagnostics-retention wiring is a later increment"
        );
    }

    // ──────────── #240/2b — overlay-push ingest tests ────────────

    #[test]
    fn push_overlay_stores_files_signals_and_acks() {
        let api = ServeVerdictState::new();
        let (tx, rx) = channel::<String>();
        api.attach_push_signal(tx);

        let files = vec![
            ("/wt-a/src/lib.rs".to_string(), "pub fn x() {}".to_string()),
            (
                "/wt-a/Cargo.toml".to_string(),
                "[package]\nname=\"x\"\n".to_string(),
            ),
        ];
        let ack = api.push_overlay("/wt-a", "origin/main", &files);

        // Ack: accepted=true + applied_files=N.
        assert_eq!(ack.worktree, "/wt-a");
        assert!(
            ack.accepted,
            "VerdictService override returns accepted=true"
        );
        assert_eq!(ack.applied_files, 2);

        // Store contains the overlay (peek doesn't consume).
        let peeked = api.peek_overlay_for("/wt-a").expect("stored");
        assert_eq!(peeked.base_ref, "origin/main");
        assert_eq!(peeked.files.len(), 2);
        assert_eq!(peeked.files, files);
        assert_eq!(peeked.check_profile, None);

        // Signal fired with the WT key.
        let signal = rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .expect("push_signal wakeup");
        assert_eq!(signal, "/wt-a");
    }

    #[test]
    fn push_overlay_with_profile_stores_per_request_cargo_profile() {
        let api = ServeVerdictState::new();
        let profile = CheckProfile {
            subcommand: CargoSubcommand::Check,
            package: Some("alchemy".into()),
            target: Some("wasm32-unknown-unknown".into()),
            features: vec!["hydrate".into()],
            no_default_features: true,
            release: true,
            extra_args: vec!["--tests".into()],
        };
        let files = vec![("/wt/Cargo.toml".to_string(), "[workspace]\n".to_string())];

        let ack = api.push_overlay_with_profile("/wt", "origin/dev", &files, Some(&profile));

        assert!(ack.accepted);
        let pushed = api.peek_overlay_for("/wt").expect("stored");
        assert_eq!(pushed.check_profile, Some(profile));
    }

    #[test]
    fn push_overlay_with_options_maps_repo_relative_paths_to_analysis_root() {
        let api = ServeVerdictState::new();
        let files = vec![("src/lib.rs".to_string(), "pub fn x() {}".to_string())];
        let options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some("/workspace/tf-multiverse".into()),
            base_sha: Some("abc123".into()),
            changed_files: Some(vec!["src/lib.rs".into()]),
            gate: false,
        };

        let ack = api.push_overlay_with_options("/client/wt", "", &files, None, Some(&options));

        assert!(ack.accepted);
        let pushed = api.peek_overlay_for("/client/wt").expect("stored");
        assert_eq!(
            pushed.files,
            vec![(
                "/workspace/tf-multiverse/src/lib.rs".to_string(),
                "pub fn x() {}".to_string()
            )]
        );
        assert_eq!(
            pushed.analysis_root.as_deref(),
            Some(Path::new("/workspace/tf-multiverse"))
        );
        assert_eq!(pushed.base_sha.as_deref(), Some("abc123"));
        assert_eq!(pushed.changed_files, Some(vec!["src/lib.rs".into()]));
    }

    #[test]
    fn project_check_overlay_materializes_changed_files_then_cleans_root() {
        let root = temp_root("project-overlay");
        git(&root, &["init"]);
        git(
            &root,
            &["config", "user.email", "cargoless@example.invalid"],
        );
        git(&root, &["config", "user.name", "Cargoless Test"]);
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".cargoless/tree.cache")).unwrap();
        std::fs::write(root.join(".cargoless/tree.cache/keep"), "cached\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn old() {}\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        let base = String::from("HEAD");

        let api = ServeVerdictState::new();
        let context = ProjectCheckRunContext {
            root: root.clone(),
            changed_files: Some(vec!["src/lib.rs".into(), "new.yaml".into()]),
            base_ref: base,
            overlay_files: vec![
                (
                    root.join("src/lib.rs").to_string_lossy().into_owned(),
                    "pub fn changed() {}\n".to_string(),
                ),
                (
                    root.join("new.yaml").to_string_lossy().into_owned(),
                    "value: changed\n".to_string(),
                ),
            ],
            materialize_overlay: true,
            gate: false,
        };

        let seen = api
            .with_project_check_overlay(&context, |root| {
                (
                    std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
                    std::fs::read_to_string(root.join("new.yaml")).unwrap(),
                )
            })
            .unwrap();

        assert_eq!(seen.0, "pub fn changed() {}\n");
        assert_eq!(seen.1, "value: changed\n");
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            "pub fn old() {}\n"
        );
        assert!(!root.join("new.yaml").exists());
        assert_eq!(
            std::fs::read_to_string(root.join(".cargoless/tree.cache/keep")).unwrap(),
            "cached\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn push_overlay_with_options_rejects_escaping_repo_relative_paths() {
        let api = ServeVerdictState::new();
        let files = vec![("../outside.rs".to_string(), "bad".to_string())];
        let options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some("/workspace/tf-multiverse".into()),
            base_sha: None,
            changed_files: None,
            gate: false,
        };

        let ack = api.push_overlay_with_options("/client/wt", "", &files, None, Some(&options));

        assert!(!ack.accepted);
        assert_eq!(ack.applied_files, 0);
        assert!(api.peek_overlay_for("/client/wt").is_none());
    }

    #[test]
    fn take_overlay_for_is_pop_on_consume() {
        let api = ServeVerdictState::new();
        let files = vec![("/wt/x".to_string(), "y".to_string())];
        api.push_overlay("/wt", "main", &files);

        // First take: consumes.
        let first = api.take_overlay_for("/wt");
        assert!(first.is_some(), "first take returns the stored overlay");
        assert_eq!(first.unwrap().files, files);

        // Second take: None (consumed). FS-mode resumes for this WT
        // until a fresh push arrives.
        assert!(
            api.take_overlay_for("/wt").is_none(),
            "second take returns None — pop-on-consume semantic"
        );
        // peek also None.
        assert!(api.peek_overlay_for("/wt").is_none());
    }

    /// **THE load-bearing composing-equivalence assertion (2b spec §5.3).**
    ///
    /// For the SAME `(path, content)` set, the `Vec<OverlayOp>` produced
    /// by `overlay::diff(prev, next)` is byte-identical whether `next`
    /// was built from FS-read pairs OR from pushed pairs. This proves
    /// that `overlay::diff` is source-agnostic — the proven isolation
    /// core (multiplex/clusterdrv/barrier) sees no difference between
    /// pushed-mode and FS-mode, and the §190/#247
    /// precondition-restore story stays intact through the 2b ingest seam.
    ///
    /// This is the structural-correctness guarantee 2b ships. A future
    /// regression that introduces source-asymmetry (e.g. trimming pushed
    /// content) would flip exactly this assertion.
    #[test]
    fn composing_equivalence_pushed_vs_fs_pairs_yield_identical_overlay_ops() {
        use cargoless_core::overlay::{OverlaySet, diff};

        let prev = OverlaySet::from_pairs(vec![(
            "/wt-a/src/old.rs".to_string(),
            "fn old() {}".to_string(),
        )]);

        // Same content, two construction paths:
        //   - FS-mode: the SwitchOverlay arm reads (path, content) from
        //     disk and builds OverlaySet::from_pairs.
        //   - Pushed-mode: the SwitchOverlay arm reads (path, content)
        //     from api.take_overlay_for(wt) and builds OverlaySet::from_pairs.
        // Both produce IDENTICAL OverlaySet → IDENTICAL diff output.
        let pairs = vec![
            (
                "/wt-a/src/lib.rs".to_string(),
                "pub fn new() {}".to_string(),
            ),
            (
                "/wt-a/src/util.rs".to_string(),
                "pub fn util() {}".to_string(),
            ),
        ];

        let fs_next = OverlaySet::from_pairs(pairs.iter().cloned());
        let fs_ops = diff(&prev, &fs_next);

        // Pushed-mode: store + take + reconstruct OverlaySet exactly as
        // the SwitchOverlay arm does.
        let api = ServeVerdictState::new();
        api.push_overlay("/wt-a", "origin/main", &pairs);
        let pushed = api.take_overlay_for("/wt-a").expect("pushed");
        let pushed_next = OverlaySet::from_pairs(pushed.files.iter().cloned());
        let pushed_ops = diff(&prev, &pushed_next);

        assert_eq!(
            fs_ops, pushed_ops,
            "overlay::diff output MUST be byte-identical regardless of \
             source (FS vs pushed) — the load-bearing composing-equivalence \
             assertion (D-PUSHOVERLAY §5.3). A regression here breaks the \
             pushed-mode no-wrong-verdict guarantee."
        );
    }

    #[test]
    fn push_overlay_no_signal_attached_is_safe() {
        // Fail-soft: a push that arrives BEFORE the loop wires its
        // push_signal (or AFTER the receiver was dropped) must store
        // the overlay AND not panic. The loop can still service the
        // push on its next activity tick or next push.
        let api = ServeVerdictState::new();
        // No attach_push_signal called.
        let files = vec![("/wt/f".to_string(), "x".to_string())];
        let ack = api.push_overlay("/wt", "main", &files);
        assert!(
            ack.accepted,
            "no-signal-attached ⇒ push is still accepted + stored"
        );
        assert!(
            api.peek_overlay_for("/wt").is_some(),
            "overlay still in store despite no signal"
        );

        // Dropped-receiver case: attach, drop rx, push again — still safe.
        let (tx, rx) = channel::<String>();
        api.attach_push_signal(tx);
        drop(rx);
        let ack2 = api.push_overlay("/wt-b", "main", &files);
        assert!(
            ack2.accepted,
            "dropped-receiver ⇒ push still accepted + stored (best-effort signal)"
        );
        assert!(api.peek_overlay_for("/wt-b").is_some());
    }

    #[test]
    fn multiple_pushes_same_wt_latest_wins() {
        // Per-WT serialization: a fresh push for the same WT REPLACES the
        // prior stored overlay (BTreeMap::insert semantic). N rapid
        // pushes coalesce — the SwitchOverlay arm services exactly the
        // latest state. The push_signal still fires per push (each wakeup
        // services whatever the CURRENT stored state is — natural coalesce
        // on the consume side via pop-on-consume).
        let api = ServeVerdictState::new();
        let (tx, rx) = channel::<String>();
        api.attach_push_signal(tx);

        let v1 = vec![("/wt/x".to_string(), "version-1".to_string())];
        let v2 = vec![("/wt/x".to_string(), "version-2".to_string())];
        let v3 = vec![("/wt/x".to_string(), "version-3".to_string())];
        api.push_overlay("/wt", "main", &v1);
        api.push_overlay("/wt", "main", &v2);
        api.push_overlay("/wt", "main", &v3);

        // Store has the LATEST content (v3), not v1/v2.
        let consumed = api.take_overlay_for("/wt").expect("stored");
        assert_eq!(
            consumed.files, v3,
            "latest push wins (BTreeMap::insert replace semantic)"
        );
        // Subsequent take: None (consumed once; v1/v2/v3 collapsed).
        assert!(api.take_overlay_for("/wt").is_none());

        // All 3 signals fired (the wakeup channel is per-push, not
        // coalesced). The serve loop's drain sees 3 wakeups, but each
        // take_overlay_for after the first returns None — natural
        // idempotency.
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(
            count, 3,
            "3 signals (one per push) — consume-side coalesces"
        );
    }

    #[test]
    fn quiesce_refuses_new_pushes_and_drains_on_publish() {
        let api = ServeVerdictState::new();
        let files = vec![("/wt/src/lib.rs".to_string(), "pub fn x() {}".to_string())];

        let ack = api.push_overlay("/wt", "main", &files);
        assert!(ack.accepted);
        assert_eq!(
            api.daemon_activity(),
            DaemonActivity {
                quiescing: false,
                active_worktrees: 1,
                pending_pushes: 1,
            }
        );

        let activity = api.request_quiesce();
        assert!(activity.quiescing);
        assert_eq!(activity.active_worktrees, 1);
        assert_eq!(activity.pending_pushes, 1);

        let rejected = api.push_overlay("/wt-2", "main", &files);
        assert!(
            !rejected.accepted,
            "quiescing daemon refuses fresh pushed work"
        );
        assert!(api.peek_overlay_for("/wt-2").is_none());

        let consumed = api.take_overlay_for("/wt").expect("pending overlay");
        assert_eq!(consumed.files, files);
        assert_eq!(api.daemon_activity().pending_pushes, 0);
        assert!(
            !api.drain_complete(),
            "publishing the accepted push's verdict is the drain boundary"
        );

        api.publish(Path::new("/wt"), false);
        assert_eq!(
            api.daemon_activity(),
            DaemonActivity {
                quiescing: true,
                active_worktrees: 0,
                pending_pushes: 0,
            }
        );
        assert!(api.drain_complete());
    }
}
