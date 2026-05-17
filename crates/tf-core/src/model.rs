//! File-level green/red model + internal event bus (Epic 2 / CWDL #5, D4).
//!
//! This is the daemon's single source of truth for "what works". It folds the
//! per-file verdicts coming out of [`crate::lsp`] (RA diagnostics) into the
//! `tf_proto` contract: a level-triggered [`StateEvent::FileVerdict`] per file
//! settle, and the edge-triggered [`StateEvent::BecameGreen`] /
//! [`StateEvent::BecameRed`] the rest of the system is built around — the
//! "tells you the moment it doesn't" signal.
//!
//! ## D4 granularity
//!
//! v0 is **file-level** (decision D4). The model maps each document to
//! [`FileState`]; the tree is [`TreeState::Green`] iff at least one file is
//! known and *every* known file is green. Until something is proven green the
//! tree is [`TreeState::Red`] — the daemon never claims green it hasn't seen
//! (this is the model half of AC#4 "never serve red").
//!
//! ## Ownership seam: BuildIdentity is injected, not computed here
//!
//! `BecameGreen` carries a [`BuildIdentity`] so the build/CAS layer can act
//! without a round-trip. Computing that identity (hashing the source tree,
//! `Cargo.lock`, toolchain, `tf.toml`) is the **CAS/build owner's** job
//! (`tf-cas` / `tf-core::build`), not the model's. The model takes an
//! [`IdentityProvider`] and asks it at the green edge — keeping the
//! daemon-core / build-cas boundary a `tf-proto` type, never a reach-across.
//!
//! Pure and std-only: the bus is `std::sync::mpsc`; every transition rule is
//! unit-tested by draining a subscriber.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tf_proto::{BuildIdentity, FileState, StateEvent, TreeState};

/// Hard ceiling for [`check_once`] (override: `TF_CHECK_TIMEOUT_SECS`). A
/// cold rust-analyzer flycheck (`cargo check`) can take minutes; this only
/// bounds pathological hangs.
const CHECK_HARD_CAP: Duration = Duration::from_secs(180);
/// Quiet window: once diagnostics have arrived and none have for this long,
/// the verdict is considered settled.
const CHECK_SETTLE: Duration = Duration::from_secs(2);
/// Debounce for the streaming [`watch`] pipeline.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// Supplies the current [`BuildIdentity`] at a green edge. Blanket-impl'd for
/// any `Fn() -> BuildIdentity`, so callers usually just pass a closure; the
/// real implementation lives behind the build-cas seam.
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

/// The green/red state machine + subscriber bus.
pub struct Model {
    files: BTreeMap<String, FileState>,
    tree: TreeState,
    subscribers: Vec<Sender<StateEvent>>,
    identity: Box<dyn IdentityProvider>,
}

impl Model {
    /// New model: no files known yet ⇒ tree is [`TreeState::Red`] (nothing
    /// proven green). `identity` is consulted only at a red→green edge.
    pub fn new<I: IdentityProvider + 'static>(identity: I) -> Self {
        Self {
            files: BTreeMap::new(),
            tree: TreeState::Red,
            subscribers: Vec::new(),
            identity: Box::new(identity),
        }
    }

    /// Subscribe to the verdict stream. Each subscriber gets every event from
    /// now on; slow/closed subscribers are pruned on the next send.
    pub fn subscribe(&mut self) -> Receiver<StateEvent> {
        let (tx, rx) = channel();
        self.subscribers.push(tx);
        rx
    }

    /// Current aggregate verdict.
    pub fn tree_state(&self) -> TreeState {
        self.tree
    }

    /// Verdict for a specific document path, if known.
    pub fn file_state(&self, path: &str) -> Option<FileState> {
        self.files.get(path).copied()
    }

    /// Apply a settled per-file verdict. Emits a (level-triggered)
    /// `FileVerdict` always — re-emitting an unchanged state is allowed by the
    /// contract and keeps late subscribers correct — then a `BecameGreen` /
    /// `BecameRed` iff the tree aggregate just crossed the boundary.
    pub fn apply_file(&mut self, path: impl Into<String>, state: FileState) {
        let path = path.into();
        self.files.insert(path.clone(), state);
        self.emit(StateEvent::FileVerdict { path, state });
        self.reconcile_tree();
    }

    /// A document went away (deleted / gitignored). Drops it from the tree;
    /// may flip the aggregate green (last red file removed).
    pub fn forget_file(&mut self, path: &str) {
        if self.files.remove(path).is_some() {
            self.reconcile_tree();
        }
    }

    /// Convenience: fold an [`crate::lsp::PublishDiagnostics`] in directly.
    /// `error_count == 0` ⇒ green. A non-`file:` URI is ignored (the model
    /// only tracks workspace files).
    pub fn apply_publish(&mut self, pd: &crate::lsp::PublishDiagnostics) {
        let Some(path) = crate::lsp::path_from_uri(&pd.uri) else {
            return;
        };
        let state = if pd.is_green() {
            FileState::Green
        } else {
            FileState::Red
        };
        self.apply_file(path, state);
    }

    fn aggregate(&self) -> TreeState {
        tree_from_states(&self.files)
    }

    fn reconcile_tree(&mut self) {
        let next = self.aggregate();
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
}

/// The D4 tree rule, single source of truth: green iff at least one file is
/// known and every known file is green; otherwise red.
pub(crate) fn tree_from_states(files: &BTreeMap<String, FileState>) -> TreeState {
    if !files.is_empty() && files.values().all(|s| *s == FileState::Green) {
        TreeState::Green
    } else {
        TreeState::Red
    }
}

fn poisoned<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Recursively collect `*.rs` files under `root`, skipping ignored paths
/// (`target/`, `.git/`, `.gitignore`d) via the watcher's [`crate::watcher::IgnoreRules`].
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
// cli-ux public surface: one-shot check + streaming subscription
// ---------------------------------------------------------------------------

/// One-shot verdict for `root`: spin up rust-analyzer, open every workspace
/// `.rs` file, wait for diagnostics to settle, and fold them into the D4 tree
/// rule. The `tf check` entrypoint cli-ux consumes.
///
/// Returns `io::Result` (not bare [`TreeState`]) deliberately: a missing
/// rust-analyzer or spawn failure is *not* "the code is red" — it is an error
/// the CLI must surface distinctly. A successful run with no provable-green
/// files yields [`TreeState::Red`] (never claim unproven green — AC#4).
pub fn check_once(root: &Path) -> io::Result<TreeState> {
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
    let (client, diags) = crate::lsp::LspClient::initialize(stdin, stdout, &root_str)?;

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
    let mut states: BTreeMap<String, FileState> = BTreeMap::new();
    let mut got_any = false;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait = CHECK_SETTLE.min(deadline - now);
        match diags.recv_timeout(wait) {
            Ok(pd) => {
                got_any = true;
                if let Some(p) = crate::lsp::path_from_uri(&pd.uri) {
                    let s = if pd.is_green() {
                        FileState::Green
                    } else {
                        FileState::Red
                    };
                    states.insert(p, s);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if got_any {
                    break; // settled
                }
            }
            Err(RecvTimeoutError::Disconnected) => break, // RA exited
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(tree_from_states(&states))
}

/// A running watch pipeline: rust-analyzer + LSP + watcher feeding the model.
/// Drop = graceful shutdown (stop threads, stop watcher, kill RA).
pub struct ModelSession {
    model: Arc<Mutex<Model>>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<std::process::Child>>>,
    watch: Option<crate::watcher::WatchHandle>,
    threads: Vec<JoinHandle<()>>,
}

impl ModelSession {
    /// Add another [`StateEvent`] subscriber to the live stream.
    pub fn subscribe(&self) -> Receiver<StateEvent> {
        poisoned(&self.model).subscribe()
    }

    /// Current aggregate verdict.
    pub fn tree_state(&self) -> TreeState {
        poisoned(&self.model).tree_state()
    }

    /// Explicit graceful shutdown (also runs on drop).
    pub fn shutdown(mut self) {
        self.do_shutdown();
    }

    fn do_shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Threads loop on 250ms recv timeouts checking `stop`, so they exit
        // promptly regardless of channel state — safe to join before the
        // watcher/child are torn down.
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        drop(self.watch.take()); // stops the notify watcher + its thread
        if let Some(mut c) = poisoned(&self.child).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for ModelSession {
    fn drop(&mut self) {
        self.do_shutdown();
    }
}

/// Start the streaming pipeline for `root` and return the session plus the
/// initial [`StateEvent`] subscription. `identity` supplies the
/// [`BuildIdentity`] at green edges (build-cas's seam — see module docs);
/// callers that only display green/red can pass a trivial provider.
pub fn watch<I: IdentityProvider + 'static>(
    root: &Path,
    identity: I,
) -> io::Result<(ModelSession, Receiver<StateEvent>)> {
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
    let (client, diags) = crate::lsp::LspClient::initialize(stdin, stdout, &root_str)?;
    let client = Arc::new(client);

    let model = Arc::new(Mutex::new(Model::new(identity)));
    let events = poisoned(&model).subscribe();

    let ignore = crate::watcher::IgnoreRules::for_root(&root);
    for f in collect_rs_files(&root, &ignore) {
        if let Ok(text) = fs::read_to_string(&f) {
            let _ = client.did_open(&f.to_string_lossy(), &text, 1);
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();

    // Diagnostics → model.
    {
        let model = Arc::clone(&model);
        let stop = Arc::clone(&stop);
        threads.push(
            thread::Builder::new()
                .name("tf-model-diags".into())
                .spawn(move || {
                    loop {
                        match diags.recv_timeout(Duration::from_millis(250)) {
                            Ok(pd) => poisoned(&model).apply_publish(&pd),
                            Err(RecvTimeoutError::Timeout) => {
                                if stop.load(Ordering::SeqCst) {
                                    break;
                                }
                            }
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                })
                .expect("spawn tf-model-diags"),
        );
    }

    // Filesystem changes → re-sync RA so it re-checks.
    let (watch_handle, batches) =
        crate::watcher::watch(&root, WATCH_DEBOUNCE).map_err(io::Error::other)?;
    {
        let model = Arc::clone(&model);
        let stop = Arc::clone(&stop);
        let client = Arc::clone(&client);
        threads.push(
            thread::Builder::new()
                .name("tf-model-fs".into())
                .spawn(move || {
                    let mut version: i64 = 2;
                    loop {
                        match batches.recv_timeout(Duration::from_millis(250)) {
                            Ok(batch) => {
                                for path in batch {
                                    if path.extension().is_none_or(|e| e != "rs") {
                                        continue;
                                    }
                                    let p = path.to_string_lossy().into_owned();
                                    match fs::read_to_string(&path) {
                                        Ok(text) => {
                                            version += 1;
                                            let _ = client.did_change(&p, &text, version);
                                            let _ = client.did_save(&p);
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
        child: Arc::new(Mutex::new(Some(child))),
        watch: Some(watch_handle),
        threads,
    };
    Ok((session, events))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_proto::{ContentHash, Profile, TargetTriple};

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

    fn drain(rx: &Receiver<StateEvent>) -> Vec<StateEvent> {
        let mut v = Vec::new();
        while let Ok(e) = rx.try_recv() {
            v.push(e);
        }
        v
    }

    #[test]
    fn starts_red_with_no_files() {
        let m = model();
        assert_eq!(m.tree_state(), TreeState::Red);
        assert_eq!(m.file_state("x"), None);
    }

    #[test]
    fn single_green_file_crosses_to_green_once() {
        let mut m = model();
        let rx = m.subscribe();
        m.apply_file("src/lib.rs", FileState::Green);
        let evs = drain(&rx);
        assert_eq!(
            evs,
            vec![
                StateEvent::FileVerdict {
                    path: "src/lib.rs".into(),
                    state: FileState::Green
                },
                StateEvent::BecameGreen { identity: ident() },
            ]
        );
        assert_eq!(m.tree_state(), TreeState::Green);

        // Re-applying the same green state: level FileVerdict re-emits, but
        // NO second BecameGreen (edge already crossed).
        m.apply_file("src/lib.rs", FileState::Green);
        assert_eq!(
            drain(&rx),
            vec![StateEvent::FileVerdict {
                path: "src/lib.rs".into(),
                state: FileState::Green
            }]
        );
    }

    #[test]
    fn one_red_file_holds_tree_red_then_green_edge_when_fixed() {
        let mut m = model();
        let rx = m.subscribe();
        m.apply_file("a.rs", FileState::Green);
        m.apply_file("b.rs", FileState::Red); // tree still Red (b red)
        assert_eq!(m.tree_state(), TreeState::Red);
        let evs = drain(&rx);
        // a: FileVerdict + BecameGreen (a alone made it all-green),
        // b: FileVerdict + BecameRed (b reds the tree).
        assert_eq!(
            evs,
            vec![
                StateEvent::FileVerdict {
                    path: "a.rs".into(),
                    state: FileState::Green
                },
                StateEvent::BecameGreen { identity: ident() },
                StateEvent::FileVerdict {
                    path: "b.rs".into(),
                    state: FileState::Red
                },
                StateEvent::BecameRed,
            ]
        );

        m.apply_file("b.rs", FileState::Green); // now all green again
        assert_eq!(
            drain(&rx),
            vec![
                StateEvent::FileVerdict {
                    path: "b.rs".into(),
                    state: FileState::Green
                },
                StateEvent::BecameGreen { identity: ident() },
            ]
        );
        assert_eq!(m.tree_state(), TreeState::Green);
    }

    #[test]
    fn forgetting_last_red_file_flips_green() {
        let mut m = model();
        let rx = m.subscribe();
        m.apply_file("keep.rs", FileState::Green);
        m.apply_file("scratch.rs", FileState::Red);
        let _ = drain(&rx);
        assert_eq!(m.tree_state(), TreeState::Red);

        m.forget_file("scratch.rs");
        assert_eq!(m.tree_state(), TreeState::Green);
        assert_eq!(
            drain(&rx),
            vec![StateEvent::BecameGreen { identity: ident() }]
        );

        // Forgetting an unknown file is a no-op (no event).
        m.forget_file("never.rs");
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn apply_publish_maps_uri_and_severity() {
        let mut m = model();
        let rx = m.subscribe();
        m.apply_publish(&crate::lsp::PublishDiagnostics {
            uri: "file:///proj/src/main.rs".into(),
            error_count: 0,
            total: 1, // a warning only ⇒ still green
        });
        assert_eq!(m.file_state("/proj/src/main.rs"), Some(FileState::Green));

        m.apply_publish(&crate::lsp::PublishDiagnostics {
            uri: "file:///proj/src/main.rs".into(),
            error_count: 2,
            total: 2,
        });
        assert_eq!(m.file_state("/proj/src/main.rs"), Some(FileState::Red));

        // Non-file URI ignored.
        m.apply_publish(&crate::lsp::PublishDiagnostics {
            uri: "untitled:Untitled-1".into(),
            error_count: 5,
            total: 5,
        });
        assert_eq!(m.file_state("untitled:Untitled-1"), None);

        let evs = drain(&rx);
        assert!(evs.contains(&StateEvent::BecameGreen { identity: ident() }));
        assert!(evs.contains(&StateEvent::BecameRed));
    }

    #[test]
    fn multiple_subscribers_all_receive_and_closed_are_pruned() {
        let mut m = model();
        let rx1 = m.subscribe();
        {
            let rx2 = m.subscribe();
            m.apply_file("a.rs", FileState::Green);
            // both get the two events
            assert_eq!(drain(&rx1).len(), 2);
            assert_eq!(drain(&rx2).len(), 2);
        } // rx2 dropped here
        m.apply_file("a.rs", FileState::Red);
        // rx1 still live; the pruned rx2 doesn't panic the emit
        assert_eq!(
            drain(&rx1),
            vec![
                StateEvent::FileVerdict {
                    path: "a.rs".into(),
                    state: FileState::Red
                },
                StateEvent::BecameRed,
            ]
        );
    }

    #[test]
    fn tree_rule_matches_model_invariant() {
        let mut m: BTreeMap<String, FileState> = BTreeMap::new();
        assert_eq!(tree_from_states(&m), TreeState::Red); // empty ⇒ red
        m.insert("a.rs".into(), FileState::Green);
        assert_eq!(tree_from_states(&m), TreeState::Green);
        m.insert("b.rs".into(), FileState::Red);
        assert_eq!(tree_from_states(&m), TreeState::Red);
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
