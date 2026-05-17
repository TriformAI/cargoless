//! File-level green/red model + event bus, with #21 verdict provenance.
//!
//! The daemon's single source of truth for "what works". It folds the
//! per-file diagnostics from [`crate::lsp`] into the `tf_proto` contract:
//! level-triggered [`StateEvent::FileVerdict`] and edge-triggered
//! [`StateEvent::BecameGreen`] / [`StateEvent::BecameRed`].
//!
//! ## #21 — cargo-check is the verdict AUTHORITY (load-bearing for v0)
//!
//! S1 proved RA-native diagnostics are BLIND to the type/trait/method/macro
//! error class — only `cargo check` (RA's *flycheck*) produces it. A checker
//! that called such code GREEN off RA-native would violate the product's one
//! promise. So the **authoritative** verdict derives ONLY from the
//! cargo-check (rustc-source) tier, observed at a flycheck-pass boundary:
//!
//! * GREEN ⟺ at least one flycheck pass has COMPLETED and that pass left no
//!   rustc-source error. Pre-first-flycheck the tree is RED (never claim
//!   unproven green — the project-long invariant).
//! * RA-native is at most an **advisory** "provisional" hint, surfaced on a
//!   separate, visibly-distinct channel ([`ModelSession::subscribe_advisory`]
//!   / [`Verdict::provenance`]); it is NEVER asserted as a `StateEvent` green.
//!
//! ## Frozen-seam discipline
//!
//! `StateEvent` and the four `tf-proto` seams are byte-frozen. `check_once`,
//! `watch`, `ModelSession::{subscribe,tree_state,shutdown}` keep their exact
//! signatures (cli-ux is wired to them). Provenance is ADDITIVE only:
//! [`Verdict`], [`VerdictProvenance`], [`check_verdict`],
//! [`ModelSession::last_verdict`], [`ModelSession::subscribe_advisory`].
//! Publish path is untouched (AC#4 stays a build-CAS concern).
//!
//! Pure and std-only: the bus is `std::sync::mpsc`; the verdict rules are
//! unit-tested by driving [`Model::apply_event`] + draining subscribers.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tf_proto::{
    BuildIdentity, ContentHash, FileState, Profile, StateEvent, TargetTriple, TreeState,
};

use crate::lsp::LspEvent;

/// Hard ceiling for [`check_once`]/[`check_verdict`] (override:
/// `TF_CHECK_TIMEOUT_SECS`). A cold flycheck can take minutes; this only
/// bounds pathological hangs.
const CHECK_HARD_CAP: Duration = Duration::from_secs(180);
/// Quiet window: once events have arrived and none have for this long without
/// an authoritative pass, the one-shot check gives up (→ Red/Advisory).
const CHECK_SETTLE: Duration = Duration::from_secs(2);
/// Debounce for the streaming [`watch`] pipeline.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(150);

// ---------------------------------------------------------------------------
// #21 additive provenance types (tf_core::model, serde-free — NOT tf-proto)
// ---------------------------------------------------------------------------

/// Where a verdict's authority comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictProvenance {
    /// Backed by a completed `cargo check` (flycheck) pass — trustworthy.
    Authoritative,
    /// RA-native only / no flycheck pass yet — a fast hint, NEVER a green.
    Advisory,
}

/// A reported verdict: the tree state plus how authoritative it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub tree: TreeState,
    pub provenance: VerdictProvenance,
}

// ---------------------------------------------------------------------------
// Identity seam (unchanged, frozen)
// ---------------------------------------------------------------------------

/// Supplies the current [`BuildIdentity`] at a green edge. Blanket-impl'd for
/// any `Fn() -> BuildIdentity`, so callers pass a closure/fn; the real
/// implementation lives behind the build-cas seam.
pub trait IdentityProvider: Send {
    fn current_identity(&self) -> BuildIdentity;
}

impl<F> IdentityProvider for F
where
    F: Fn() -> BuildIdentity + Send,
{
    fn current_identity(&self) -> BuildIdentity {
        self()
    }
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// The green/red state machine + subscriber buses. The authoritative tree
/// derives strictly from the cargo-check (rustc) tier gated on a completed
/// flycheck pass; RA-native feeds the advisory channel only.
pub struct Model {
    /// Authoritative per-file state from `source:"rustc"` (cargo-check).
    auth: BTreeMap<String, FileState>,
    /// Advisory per-file state from RA-native diagnostics.
    native: BTreeMap<String, FileState>,
    /// At least one flycheck (`cargo check`) pass has completed.
    flycheck_done: bool,
    /// Last emitted authoritative tree state (edge tracking).
    tree: TreeState,
    subscribers: Vec<Sender<StateEvent>>,
    advisory_subscribers: Vec<Sender<Verdict>>,
    identity: Box<dyn IdentityProvider>,
}

impl Model {
    /// New model: nothing proven ⇒ tree RED, provenance Advisory.
    pub fn new<I: IdentityProvider + 'static>(identity: I) -> Self {
        Self {
            auth: BTreeMap::new(),
            native: BTreeMap::new(),
            flycheck_done: false,
            tree: TreeState::Red,
            subscribers: Vec::new(),
            advisory_subscribers: Vec::new(),
            identity: Box::new(identity),
        }
    }

    /// Subscribe to the AUTHORITATIVE `StateEvent` stream (frozen seam).
    pub fn subscribe(&mut self) -> Receiver<StateEvent> {
        let (tx, rx) = channel();
        self.subscribers.push(tx);
        rx
    }

    /// Subscribe to the ADVISORY (provisional, visibly-distinct) verdict
    /// stream — the RA-native fast hint. Additive (#21).
    pub fn subscribe_advisory(&mut self) -> Receiver<Verdict> {
        let (tx, rx) = channel();
        self.advisory_subscribers.push(tx);
        rx
    }

    /// Current AUTHORITATIVE aggregate verdict (frozen signature).
    pub fn tree_state(&self) -> TreeState {
        self.tree
    }

    /// The full reported verdict incl. provenance. Additive (#21).
    pub fn last_verdict(&self) -> Verdict {
        Verdict {
            tree: self.tree,
            provenance: if self.flycheck_done {
                VerdictProvenance::Authoritative
            } else {
                VerdictProvenance::Advisory
            },
        }
    }

    /// Authoritative verdict for a specific document, if cargo-check has
    /// reported on it.
    pub fn file_state(&self, path: &str) -> Option<FileState> {
        self.auth.get(path).copied()
    }

    /// Fold one [`LspEvent`] into the model.
    pub fn apply_event(&mut self, ev: &LspEvent) {
        match ev {
            LspEvent::Diagnostics(pd) => {
                let Some(path) = crate::lsp::path_from_uri(&pd.uri) else {
                    return;
                };
                let auth_state = if pd.has_authoritative_error() {
                    FileState::Red
                } else {
                    FileState::Green
                };
                let native_state = if pd.advisory_errors > 0 {
                    FileState::Red
                } else {
                    FileState::Green
                };
                self.auth.insert(path.clone(), auth_state);
                self.native.insert(path.clone(), native_state);
                // FileVerdict is the authoritative per-file settle.
                self.emit(StateEvent::FileVerdict {
                    path,
                    state: auth_state,
                });
                self.reconcile();
                self.emit_advisory();
            }
            LspEvent::FlycheckEnded => {
                self.flycheck_done = true;
                self.reconcile();
                self.emit_advisory();
            }
        }
    }

    /// A document went away (deleted / gitignored).
    pub fn forget_file(&mut self, path: &str) {
        let a = self.auth.remove(path).is_some();
        let n = self.native.remove(path).is_some();
        if a || n {
            self.reconcile();
            self.emit_advisory();
        }
    }

    /// The #21 authoritative rule: RED until a flycheck pass has completed;
    /// then GREEN iff that pass left no rustc-source error (an empty clean
    /// pass is authoritatively green — `cargo check` succeeded with zero
    /// errors).
    fn authoritative_tree(&self) -> TreeState {
        if !self.flycheck_done {
            return TreeState::Red;
        }
        if self.auth.values().any(|s| *s == FileState::Red) {
            TreeState::Red
        } else {
            TreeState::Green
        }
    }

    fn reconcile(&mut self) {
        let next = self.authoritative_tree();
        if next == self.tree {
            return;
        }
        self.tree = next;
        match next {
            TreeState::Green => {
                let identity = self.identity.current_identity();
                self.emit(StateEvent::BecameGreen { identity });
            }
            TreeState::Red => self.emit(StateEvent::BecameRed),
        }
    }

    fn emit(&mut self, ev: StateEvent) {
        self.subscribers.retain(|s| s.send(ev.clone()).is_ok());
    }

    fn emit_advisory(&mut self) {
        let v = self.last_verdict();
        self.advisory_subscribers.retain(|s| s.send(v).is_ok());
    }
}

fn poisoned<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Recursively collect `*.rs` files under `root`, skipping ignored paths
/// (`target/`, `.git/`, `.gitignore`d) via [`crate::watcher::IgnoreRules`].
fn collect_rs_files(root: &Path, ignore: &crate::watcher::IgnoreRules) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if ignore.is_ignored(rel) {
                continue;
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    if path.extension().is_some_and(|e| e == "rs") {
                        out.push(path);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// cli-ux public surface
// ---------------------------------------------------------------------------

/// The display-only [`BuildIdentity`] for callers that consume the verdict
/// stream but never trigger a build. Not a real build key — fixed sentinel
/// hashes — so it can never alias a genuine artifact. Real identity is the
/// build-cas owner's to compute.
pub fn placeholder_identity() -> BuildIdentity {
    let sentinel = ContentHash::new("placeholder-display-only-not-a-build-key");
    BuildIdentity {
        source_tree: sentinel.clone(),
        cargo_lock: sentinel.clone(),
        rust_toolchain: sentinel.clone(),
        tf_config: sentinel,
        target: TargetTriple::new("wasm32-unknown-unknown"),
        profile: Profile::Dev,
    }
}

/// One-shot AUTHORITATIVE verdict for `root`: spin up rust-analyzer with
/// flycheck on, open every workspace `.rs`, wait for a completed `cargo
/// check` pass, and report it with provenance. Additive (#21).
///
/// `Err` = setup/env failure (rust-analyzer missing, spawn/pipe error) — the
/// CLI must surface this distinctly from "code is red". A run that never sees
/// an authoritative flycheck pass yields `Verdict { Red, Advisory }` (never
/// claim unproven green — AC#4).
pub fn check_verdict(root: &Path) -> io::Result<Verdict> {
    let root = fs::canonicalize(root)?;
    let mut cmd = crate::analyzer::rust_analyzer_command()?;
    cmd.current_dir(&root);
    let mut child = cmd.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("rust-analyzer stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("rust-analyzer stdout unavailable"))?;
    let root_str = root.to_string_lossy().into_owned();
    let (client, events) = crate::lsp::LspClient::initialize(stdin, stdout, &root_str)?;

    let ignore = crate::watcher::IgnoreRules::for_root(&root);
    for f in collect_rs_files(&root, &ignore) {
        if let Ok(text) = fs::read_to_string(&f) {
            let _ = client.did_open(&f.to_string_lossy(), &text, 1);
        }
    }

    let cap = std::env::var("TF_CHECK_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(CHECK_HARD_CAP);
    let deadline = Instant::now() + cap;
    let mut auth: BTreeMap<String, FileState> = BTreeMap::new();
    let mut flycheck_seen = false;
    let mut got_any = false;
    while !flycheck_seen {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait = CHECK_SETTLE.min(deadline - now);
        match events.recv_timeout(wait) {
            Ok(LspEvent::Diagnostics(pd)) => {
                got_any = true;
                if let Some(p) = crate::lsp::path_from_uri(&pd.uri) {
                    let s = if pd.has_authoritative_error() {
                        FileState::Red
                    } else {
                        FileState::Green
                    };
                    auth.insert(p, s);
                }
            }
            Ok(LspEvent::FlycheckEnded) => {
                flycheck_seen = true;
            }
            Err(RecvTimeoutError::Timeout) => {
                if got_any {
                    break; // settled without an authoritative pass
                }
            }
            Err(RecvTimeoutError::Disconnected) => break, // RA exited
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    if flycheck_seen {
        let tree = if auth.values().any(|s| *s == FileState::Red) {
            TreeState::Red
        } else {
            TreeState::Green
        };
        Ok(Verdict {
            tree,
            provenance: VerdictProvenance::Authoritative,
        })
    } else {
        // No authoritative pass observed → unproven, never green.
        Ok(Verdict {
            tree: TreeState::Red,
            provenance: VerdictProvenance::Advisory,
        })
    }
}

/// One-shot verdict for `root` (frozen signature — cli-ux is wired to this).
/// Thin wrapper over [`check_verdict`] discarding provenance.
pub fn check_once(root: &Path) -> io::Result<TreeState> {
    check_verdict(root).map(|v| v.tree)
}

/// A running watch pipeline: rust-analyzer + LSP + watcher feeding the model.
/// Drop = graceful shutdown (stop threads, stop watcher, kill RA).
pub struct ModelSession {
    model: Arc<Mutex<Model>>,
    stop: Arc<AtomicBool>,
    /// Manages rust-analyzer with AC#6 transparent restart.
    supervisor: Option<crate::analyzer::Supervisor>,
    watch: Option<crate::watcher::WatchHandle>,
    threads: Vec<JoinHandle<()>>,
}

impl ModelSession {
    /// Add another AUTHORITATIVE [`StateEvent`] subscriber (frozen seam).
    pub fn subscribe(&self) -> Receiver<StateEvent> {
        poisoned(&self.model).subscribe()
    }

    /// Subscribe to the ADVISORY provisional verdict stream (additive #21).
    pub fn subscribe_advisory(&self) -> Receiver<Verdict> {
        poisoned(&self.model).subscribe_advisory()
    }

    /// Current AUTHORITATIVE aggregate verdict (frozen signature).
    pub fn tree_state(&self) -> TreeState {
        poisoned(&self.model).tree_state()
    }

    /// Full reported verdict incl. provenance (additive #21).
    pub fn last_verdict(&self) -> Verdict {
        poisoned(&self.model).last_verdict()
    }

    /// Explicit graceful shutdown (also runs on drop).
    pub fn shutdown(mut self) {
        self.do_shutdown();
    }

    fn do_shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(sup) = self.supervisor.take() {
            sup.shutdown();
        }
        drop(self.watch.take());
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for ModelSession {
    fn drop(&mut self) {
        self.do_shutdown();
    }
}

/// Start the streaming pipeline for `root` (frozen signature). `identity`
/// supplies the [`BuildIdentity`] at authoritative green edges.
pub fn watch<I: IdentityProvider + 'static>(
    root: &Path,
    identity: I,
) -> io::Result<(ModelSession, Receiver<StateEvent>)> {
    let root = fs::canonicalize(root)?;
    let root_str = root.to_string_lossy().into_owned();

    let model = Arc::new(Mutex::new(Model::new(identity)));
    let events = poisoned(&model).subscribe();
    let stop = Arc::new(AtomicBool::new(false));

    // The LSP client for whichever rust-analyzer instance is currently alive;
    // the on_spawn hook swaps it on every (re)start (AC#6 transparent).
    let current: Arc<Mutex<Option<Arc<crate::lsp::LspClient>>>> = Arc::new(Mutex::new(None));

    let spawn_root = root.clone();
    let spawn = move || {
        let mut cmd = crate::analyzer::rust_analyzer_command()?;
        cmd.current_dir(&spawn_root);
        cmd.spawn()
    };

    let hook_root = root_str.clone();
    let hook_model = Arc::clone(&model);
    let hook_current = Arc::clone(&current);
    let on_spawn = move |child: &mut std::process::Child| {
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return;
        };
        let Ok((client, events)) = crate::lsp::LspClient::initialize(stdin, stdout, &hook_root)
        else {
            return; // RA broke during handshake; supervisor retries
        };
        let client = Arc::new(client);
        let ig = crate::watcher::IgnoreRules::for_root(Path::new(&hook_root));
        for f in collect_rs_files(Path::new(&hook_root), &ig) {
            if let Ok(text) = fs::read_to_string(&f) {
                let _ = client.did_open(&f.to_string_lossy(), &text, 1);
            }
        }
        *poisoned(&hook_current) = Some(Arc::clone(&client));
        let m = Arc::clone(&hook_model);
        // Detached: ends when this RA instance's stdout EOFs (it died); the
        // next on_spawn invocation starts a fresh forwarder.
        let _ = thread::Builder::new()
            .name("tf-model-events".into())
            .spawn(move || {
                while let Ok(ev) = events.recv() {
                    poisoned(&m).apply_event(&ev);
                }
            });
    };

    let supervisor = crate::analyzer::Supervisor::start_with_hook(spawn, on_spawn)?;

    let (watch_handle, batches) =
        crate::watcher::watch(&root, WATCH_DEBOUNCE).map_err(io::Error::other)?;
    let mut threads = Vec::new();
    {
        let model = Arc::clone(&model);
        let stop = Arc::clone(&stop);
        let current = Arc::clone(&current);
        threads.push(
            thread::Builder::new()
                .name("tf-model-fs".into())
                .spawn(move || {
                    let mut version: i64 = 2;
                    loop {
                        match batches.recv_timeout(Duration::from_millis(250)) {
                            Ok(batch) => {
                                let client = poisoned(&current).as_ref().cloned();
                                for path in batch {
                                    if path.extension().is_none_or(|e| e != "rs") {
                                        continue;
                                    }
                                    let p = path.to_string_lossy().into_owned();
                                    match fs::read_to_string(&path) {
                                        Ok(text) => {
                                            version += 1;
                                            if let Some(c) = client.as_ref() {
                                                let _ = c.did_change(&p, &text, version);
                                                let _ = c.did_save(&p);
                                            }
                                        }
                                        Err(_) => poisoned(&model).forget_file(&p),
                                    }
                                }
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                if stop.load(Ordering::SeqCst) {
                                    break;
                                }
                            }
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                })
                .expect("spawn tf-model-fs"),
        );
    }

    let session = ModelSession {
        model,
        stop,
        supervisor: Some(supervisor),
        watch: Some(watch_handle),
        threads,
    };
    Ok((session, events))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident() -> BuildIdentity {
        BuildIdentity {
            source_tree: ContentHash::new("src"),
            cargo_lock: ContentHash::new("lock"),
            rust_toolchain: ContentHash::new("tc"),
            tf_config: ContentHash::new("cfg"),
            target: TargetTriple::new("wasm32-unknown-unknown"),
            profile: Profile::Dev,
        }
    }

    fn model() -> Model {
        Model::new(ident)
    }

    fn diag(uri: &str, auth_err: usize, adv_err: usize) -> LspEvent {
        LspEvent::Diagnostics(crate::lsp::PublishDiagnostics {
            uri: uri.into(),
            authoritative_errors: auth_err,
            advisory_errors: adv_err,
            total: auth_err + adv_err,
        })
    }

    fn drain<T>(rx: &Receiver<T>) -> Vec<T> {
        let mut v = Vec::new();
        while let Ok(e) = rx.try_recv() {
            v.push(e);
        }
        v
    }

    #[test]
    fn starts_red_advisory() {
        let m = model();
        assert_eq!(m.tree_state(), TreeState::Red);
        assert_eq!(
            m.last_verdict(),
            Verdict {
                tree: TreeState::Red,
                provenance: VerdictProvenance::Advisory
            }
        );
        assert_eq!(m.file_state("x"), None);
    }

    #[test]
    fn native_only_clean_never_green_without_flycheck() {
        let mut m = model();
        let rx = m.subscribe();
        // RA-native says "no errors" for a file — but flycheck has NOT run.
        m.apply_event(&diag("file:///p/src/lib.rs", 0, 0));
        assert_eq!(m.tree_state(), TreeState::Red, "no green without flycheck");
        assert_eq!(m.last_verdict().provenance, VerdictProvenance::Advisory);
        // Only an authoritative FileVerdict, NEVER a BecameGreen.
        let evs = drain(&rx);
        assert!(
            evs.iter()
                .all(|e| !matches!(e, StateEvent::BecameGreen { .. }))
        );
        assert!(evs.contains(&StateEvent::FileVerdict {
            path: "/p/src/lib.rs".into(),
            state: FileState::Green
        }));
    }

    #[test]
    fn native_error_pre_flycheck_is_red_advisory_no_event_green() {
        let mut m = model();
        let rx = m.subscribe();
        let arx = m.subscribe_advisory();
        m.apply_event(&diag("file:///p/a.rs", 0, 3)); // 3 native errors
        assert_eq!(m.tree_state(), TreeState::Red);
        assert_eq!(
            m.last_verdict(),
            Verdict {
                tree: TreeState::Red,
                provenance: VerdictProvenance::Advisory
            }
        );
        assert!(
            drain(&rx)
                .iter()
                .all(|e| !matches!(e, StateEvent::BecameGreen { .. }))
        );
        // advisory channel got a provisional verdict
        assert!(!drain(&arx).is_empty());
    }

    #[test]
    fn completed_clean_flycheck_is_authoritative_green() {
        let mut m = model();
        let rx = m.subscribe();
        m.apply_event(&diag("file:///p/src/lib.rs", 0, 0)); // still red (no pass yet)
        assert_eq!(m.tree_state(), TreeState::Red);
        m.apply_event(&LspEvent::FlycheckEnded);
        assert_eq!(m.tree_state(), TreeState::Green);
        assert_eq!(
            m.last_verdict(),
            Verdict {
                tree: TreeState::Green,
                provenance: VerdictProvenance::Authoritative
            }
        );
        let evs = drain(&rx);
        assert!(evs.contains(&StateEvent::BecameGreen { identity: ident() }));
    }

    #[test]
    fn rustc_error_after_pass_is_authoritative_red() {
        let mut m = model();
        let rx = m.subscribe();
        // get to authoritative green
        m.apply_event(&diag("file:///p/a.rs", 0, 0));
        m.apply_event(&LspEvent::FlycheckEnded);
        assert_eq!(m.tree_state(), TreeState::Green);
        let _ = drain(&rx);
        // a later cargo-check error (E0599-class) flips authoritative red
        m.apply_event(&diag("file:///p/a.rs", 1, 0));
        assert_eq!(m.tree_state(), TreeState::Red);
        assert_eq!(
            m.last_verdict().provenance,
            VerdictProvenance::Authoritative
        );
        assert!(drain(&rx).contains(&StateEvent::BecameRed));
    }

    #[test]
    fn empty_clean_pass_is_green() {
        // flycheck ended with zero diagnostics at all ⇒ cargo check passed.
        let mut m = model();
        m.apply_event(&LspEvent::FlycheckEnded);
        assert_eq!(m.tree_state(), TreeState::Green);
        assert_eq!(
            m.last_verdict().provenance,
            VerdictProvenance::Authoritative
        );
    }

    #[test]
    fn forget_last_rustc_red_file_flips_green_post_pass() {
        let mut m = model();
        let rx = m.subscribe();
        m.apply_event(&diag("file:///p/keep.rs", 0, 0));
        m.apply_event(&diag("file:///p/scratch.rs", 1, 0)); // rustc error
        m.apply_event(&LspEvent::FlycheckEnded);
        assert_eq!(m.tree_state(), TreeState::Red);
        let _ = drain(&rx);
        m.forget_file("/p/scratch.rs");
        assert_eq!(m.tree_state(), TreeState::Green);
        assert!(drain(&rx).contains(&StateEvent::BecameGreen { identity: ident() }));
    }

    #[test]
    fn advisory_channel_receives_and_prunes() {
        let mut m = model();
        let a1 = m.subscribe_advisory();
        {
            let a2 = m.subscribe_advisory();
            m.apply_event(&diag("file:///p/a.rs", 0, 1));
            assert!(!drain(&a1).is_empty());
            assert!(!drain(&a2).is_empty());
        }
        // a2 dropped — emit must not panic, a1 still live
        m.apply_event(&LspEvent::FlycheckEnded);
        let got = drain(&a1);
        assert!(
            got.iter()
                .any(|v| v.provenance == VerdictProvenance::Authoritative)
        );
    }

    #[test]
    fn non_file_uri_ignored() {
        let mut m = model();
        m.apply_event(&diag("untitled:Untitled-1", 5, 5));
        assert_eq!(m.file_state("untitled:Untitled-1"), None);
        assert_eq!(m.tree_state(), TreeState::Red);
    }

    #[test]
    fn placeholder_identity_is_sentinel_dev_wasm() {
        let id = placeholder_identity();
        assert_eq!(id.profile, Profile::Dev);
        assert_eq!(id.target.as_str(), "wasm32-unknown-unknown");
        assert_eq!(id.source_tree, id.cargo_lock); // all the same sentinel
    }

    #[test]
    fn collect_rs_files_skips_target_git_and_gitignored() {
        let base = std::env::temp_dir().join(format!("tf-model-walk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let mk = |rel: &str, body: &str| {
            let p = base.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, body).unwrap();
        };
        mk(".gitignore", "ignored.rs\n");
        mk("src/lib.rs", "");
        mk("src/nested/m.rs", "");
        mk("ignored.rs", "");
        mk("target/debug/build.rs", "");
        mk(".git/hooks/pre.rs", "");
        mk("README.md", "");

        let root = fs::canonicalize(&base).unwrap();
        let ignore = crate::watcher::IgnoreRules::for_root(&root);
        let mut got: Vec<String> = collect_rs_files(&root, &ignore)
            .into_iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec!["src/lib.rs".to_string(), "src/nested/m.rs".to_string()]
        );
        let _ = fs::remove_dir_all(&base);
    }
}
