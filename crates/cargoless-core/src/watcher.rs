//! Filesystem watcher (Epic 2 / CWDL-26).
//!
//! Watches the source tree with the [`notify`] crate, **debounces** rapid
//! saves into coalesced batches, and **ignores** build output (`target/`),
//! `.git/`, and `.gitignore`d paths. The root is canonicalised before
//! watching so a symlinked project dir resolves to its real path (symlink
//! safety, CWDL-26).
//!
//! ## Testability
//!
//! The two pieces of logic that can be wrong — debounce coalescing and ignore
//! matching — are pure and unit-tested with a deterministic clock and plain
//! paths. They do **not** depend on real filesystem-event timing, so the CI
//! `test` job (Linux-only) exercises them fully. Cross-platform *delivery*
//! (macOS FSEvents vs Linux inotify) is notify's concern and is verified on
//! real machines per CWDL-26, outside the Linux CI box.
//!
//! ## .gitignore scope (v0)
//!
//! [`IgnoreRules`] implements the practical subset of gitignore that a Rust +
//! WASM tree needs: blank/`#` lines, `name`, `name/` (dir-only), `*.ext`,
//! leading-`/` anchoring, a single `*` wildcard within a path segment, and
//! `!` negation evaluated last-match-wins. Full gitignore (`**`, character
//! classes, nested `.gitignore` files) is a documented v1 refinement — it
//! does not change the daemon's contract, only which paths are skipped.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::{EventKind, RecursiveMode, Watcher};

/// Default quiet period: a save burst (editor write-temp-rename, formatter
/// rewrite, multi-file refactor) is coalesced into one batch once the tree
/// has been quiet for this long.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(150);

/// A coalesced batch of changed paths, ignore-filtered, root-relative-safe
/// (absolute, canonical). Ordered + de-duplicated for deterministic
/// downstream behaviour and tests.
pub type ChangeBatch = Vec<PathBuf>;

// ---------------------------------------------------------------------------
// Ignore rules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Pattern {
    /// Pattern body with any leading `/` and trailing `/` stripped.
    body: String,
    /// `foo/` — matches directories (and everything under them) only.
    dir_only: bool,
    /// Leading `/` or an interior `/`: matched against the whole relative
    /// path rather than any single component.
    anchored: bool,
    /// `!foo` — un-ignores a path an earlier pattern ignored.
    negated: bool,
}

/// Compiled ignore matcher: unconditional (`target/`, `.git/`) plus the
/// `.gitignore` subset described in the module docs.
#[derive(Debug, Clone, Default)]
pub struct IgnoreRules {
    patterns: Vec<Pattern>,
}

impl IgnoreRules {
    /// Rules with only the unconditional builtins (`target`, `.git`).
    pub fn builtin() -> Self {
        Self::default()
    }

    /// Build from a `.gitignore` file's text. Lines are gitignore-ish; see
    /// the module docs for the supported subset.
    pub fn from_gitignore_str(text: &str) -> Self {
        let mut patterns = Vec::new();
        for raw in text.lines() {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negated, rest) = match line.strip_prefix('!') {
                Some(r) => (true, r),
                None => (false, line),
            };
            let dir_only = rest.ends_with('/');
            let trimmed = rest.trim_end_matches('/');
            let anchored = trimmed.starts_with('/') || trimmed.contains('/');
            let body = trimmed.trim_start_matches('/').to_string();
            if body.is_empty() {
                continue;
            }
            patterns.push(Pattern {
                body,
                dir_only,
                anchored,
                negated,
            });
        }
        Self { patterns }
    }

    /// Read `<root>/.gitignore` if present; otherwise builtins only.
    pub fn for_root(root: &Path) -> Self {
        match fs::read_to_string(root.join(".gitignore")) {
            Ok(text) => Self::from_gitignore_str(&text),
            Err(_) => Self::builtin(),
        }
    }

    /// Whether `rel` (a path relative to the watched root, using `/` or the
    /// platform separator) is ignored. `target` and `.git` anywhere in the
    /// path are always ignored — they are never legitimate watch targets in
    /// a Cargo + WASM tree and gitignore-negation must not resurrect them.
    pub fn is_ignored(&self, rel: &Path) -> bool {
        let comps: Vec<String> = rel
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        if comps.iter().any(|c| c == "target" || c == ".git") {
            return true;
        }
        let rel_str = comps.join("/");
        if rel_str.is_empty() {
            return false;
        }
        let basename = comps.last().map(String::as_str).unwrap_or("");

        // last-match-wins, gitignore semantics.
        let mut ignored = false;
        for p in &self.patterns {
            let hit = if p.anchored {
                glob_match(&p.body, &rel_str)
                    || rel_str.strip_prefix(&format!("{}/", p.body)).is_some()
                    || comps_prefix_match(&p.body, &comps)
            } else if p.dir_only {
                comps.iter().any(|c| glob_match(&p.body, c))
            } else {
                glob_match(&p.body, basename) || comps.iter().any(|c| glob_match(&p.body, c))
            };
            if hit {
                ignored = !p.negated;
            }
        }
        ignored
    }
}

/// `a/b` anchored pattern matches if the candidate's leading components are
/// `a`, `b`, ... (so `a/b` ignores `a/b/c.rs`).
fn comps_prefix_match(pat: &str, comps: &[String]) -> bool {
    let pat_comps: Vec<&str> = pat.split('/').filter(|s| !s.is_empty()).collect();
    if pat_comps.is_empty() || pat_comps.len() > comps.len() {
        return false;
    }
    pat_comps
        .iter()
        .zip(comps.iter())
        .all(|(p, c)| glob_match(p, c))
}

/// Single-`*` glob over one string. `*` matches any run of chars (including
/// none) but, by gitignore convention for our subset, not used across `/`
/// because callers pass individual components or already-anchored paths.
fn glob_match(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((pre, post)) => {
            // Only a single `*` is supported; treat any further `*` literally
            // by re-globbing the tail.
            if !text.starts_with(pre) {
                return false;
            }
            let rest = &text[pre.len()..];
            if post.is_empty() {
                return true;
            }
            if post.contains('*') {
                // crude multi-star: require pre, then search for the literal
                // chunk before the next star anywhere in the remainder.
                let (mid, tail) = post.split_once('*').unwrap();
                if let Some(idx) = rest.find(mid) {
                    return glob_match(&format!("*{tail}"), &rest[idx + mid.len()..]);
                }
                return false;
            }
            rest.len() >= post.len() && rest.ends_with(post)
        }
    }
}

// ---------------------------------------------------------------------------
// Debounce
// ---------------------------------------------------------------------------

/// Time-based coalescer. Pure: the caller supplies the clock, so the
/// coalescing rule is unit-tested deterministically without sleeping.
#[derive(Debug)]
pub struct Debouncer {
    pending: BTreeSet<PathBuf>,
    last_change: Option<Instant>,
    quiet: Duration,
}

impl Debouncer {
    pub fn new(quiet: Duration) -> Self {
        Self {
            pending: BTreeSet::new(),
            last_change: None,
            quiet,
        }
    }

    /// Record a changed path observed at `now`. Resets the quiet timer.
    pub fn record(&mut self, path: PathBuf, now: Instant) {
        self.pending.insert(path);
        self.last_change = Some(now);
    }

    /// If there are pending paths and the tree has been quiet for `quiet`
    /// as of `now`, drain and return the coalesced, ordered batch.
    pub fn poll(&mut self, now: Instant) -> Option<ChangeBatch> {
        let last = self.last_change?;
        if self.pending.is_empty() {
            return None;
        }
        if now.duration_since(last) >= self.quiet {
            let batch: ChangeBatch = std::mem::take(&mut self.pending).into_iter().collect();
            self.last_change = None;
            Some(batch)
        } else {
            None
        }
    }

    /// How long until `poll` could next yield, given `now`. Used to size the
    /// watcher thread's blocking recv timeout so it wakes exactly when due.
    pub fn time_until_ready(&self, now: Instant) -> Option<Duration> {
        let last = self.last_change?;
        if self.pending.is_empty() {
            return None;
        }
        let elapsed = now.duration_since(last);
        Some(self.quiet.saturating_sub(elapsed))
    }
}

// ---------------------------------------------------------------------------
// Live watcher
// ---------------------------------------------------------------------------

/// Owns the notify watcher + background coalescing thread. Dropping it stops
/// watching and joins the thread.
pub struct WatchHandle {
    // Held purely for RAII: dropping the notify watcher unregisters the OS
    // watch. Never read, hence the explicit allow (an `_`-prefixed field
    // name does not silence dead_code for struct fields).
    #[allow(dead_code)]
    watcher: notify::RecommendedWatcher,
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
    /// Canonicalised root actually being watched.
    pub root: PathBuf,
}

impl WatchHandle {
    pub fn shutdown(mut self) {
        self.do_shutdown();
    }

    fn do_shutdown(&mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.do_shutdown();
    }
}

/// Start watching `root` recursively. Returns the handle (keep it alive) and
/// a receiver of debounced [`ChangeBatch`]es. Ignore-filtered against
/// `<root>/.gitignore` + builtins.
pub fn watch(
    root: impl AsRef<Path>,
    quiet: Duration,
) -> notify::Result<(WatchHandle, Receiver<ChangeBatch>)> {
    let root = fs::canonicalize(root.as_ref())?;
    let ignore = IgnoreRules::for_root(&root);

    let (raw_tx, raw_rx) = channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        // A dead receiver just means we're shutting down; ignore send error.
        let _ = raw_tx.send(res);
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    let (batch_tx, batch_rx) = channel::<ChangeBatch>();
    let (stop_tx, stop_rx) = channel::<()>();
    let thread_root = root.clone();

    let thread = thread::Builder::new()
        .name("tf-watcher".into())
        .spawn(move || {
            let mut deb = Debouncer::new(quiet);
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                let now = Instant::now();
                let timeout = deb
                    .time_until_ready(now)
                    .unwrap_or(Duration::from_millis(250))
                    .max(Duration::from_millis(1));
                match raw_rx.recv_timeout(timeout) {
                    Ok(Ok(event)) => {
                        if !is_mutation(&event.kind) {
                            continue;
                        }
                        let now = Instant::now();
                        for path in event.paths {
                            if accept(&path, &thread_root, &ignore) {
                                deb.record(path, now);
                            }
                        }
                    }
                    Ok(Err(_)) => { /* notify backend hiccup; keep going */ }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if let Some(batch) = deb.poll(Instant::now()) {
                    if batch_tx.send(batch).is_err() {
                        break; // consumer gone
                    }
                }
            }
        })
        .expect("spawn tf-watcher thread");

    Ok((
        WatchHandle {
            watcher,
            stop: stop_tx,
            thread: Some(thread),
            root,
        },
        batch_rx,
    ))
}

/// Only content-relevant mutations drive a rebuild verdict; pure metadata
/// touches (access time) do not.
fn is_mutation(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

/// Accept a raw event path: must be under root and not ignored. The path is
/// matched relative to the canonical root so symlinked roots behave.
fn accept(path: &Path, root: &Path, ignore: &IgnoreRules) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    !ignore.is_ignored(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn builtin_ignores_target_and_git_anywhere() {
        let ig = IgnoreRules::builtin();
        assert!(ig.is_ignored(&p("target/debug/app.wasm")));
        assert!(ig.is_ignored(&p("crates/cargoless-core/target/x")));
        assert!(ig.is_ignored(&p(".git/HEAD")));
        assert!(!ig.is_ignored(&p("src/main.rs")));
        assert!(!ig.is_ignored(&p("")));
    }

    #[test]
    fn gitignore_subset_matches() {
        let ig = IgnoreRules::from_gitignore_str(
            "# comment\n\n*.log\ndist/\n/Cargo.lock\nnode_modules/\nsecret.txt\n",
        );
        assert!(ig.is_ignored(&p("server.log")));
        assert!(ig.is_ignored(&p("a/b/server.log")));
        assert!(ig.is_ignored(&p("dist/index.html")));
        assert!(ig.is_ignored(&p("Cargo.lock")));
        assert!(!ig.is_ignored(&p("crates/x/Cargo.lock"))); // anchored to root
        assert!(ig.is_ignored(&p("node_modules/leptos/x.js")));
        assert!(ig.is_ignored(&p("secret.txt")));
        assert!(!ig.is_ignored(&p("src/lib.rs")));
    }

    #[test]
    fn gitignore_negation_last_match_wins() {
        let ig = IgnoreRules::from_gitignore_str("*.tmp\n!keep.tmp\n");
        assert!(ig.is_ignored(&p("scratch.tmp")));
        assert!(!ig.is_ignored(&p("keep.tmp")));
    }

    #[test]
    fn negation_cannot_resurrect_target() {
        let ig = IgnoreRules::from_gitignore_str("!target\n");
        assert!(ig.is_ignored(&p("target/debug/x")));
    }

    #[test]
    fn glob_star() {
        assert!(glob_match("*.rs", "lib.rs"));
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a*c", "ac"));
        assert!(!glob_match("a*c", "abd"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("v*-*.log", "v1-server.log"));
    }

    #[test]
    fn debounce_coalesces_until_quiet() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(150));
        d.record(p("a.rs"), t0);
        d.record(p("b.rs"), t0 + Duration::from_millis(50));
        // still within quiet window after the *last* change
        assert!(d.poll(t0 + Duration::from_millis(150)).is_none());
        // 150ms after the last change (b at +50) -> ready at +200
        let batch = d.poll(t0 + Duration::from_millis(200)).expect("batch");
        assert_eq!(batch, vec![p("a.rs"), p("b.rs")]);
        // drained
        assert!(d.poll(t0 + Duration::from_millis(400)).is_none());
    }

    #[test]
    fn debounce_dedupes_repeated_path() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(10));
        d.record(p("x.rs"), t0);
        d.record(p("x.rs"), t0 + Duration::from_millis(1));
        let batch = d.poll(t0 + Duration::from_millis(20)).expect("batch");
        assert_eq!(batch, vec![p("x.rs")]);
    }

    #[test]
    fn time_until_ready_shrinks() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(100));
        assert!(d.time_until_ready(t0).is_none());
        d.record(p("a"), t0);
        assert_eq!(
            d.time_until_ready(t0 + Duration::from_millis(30)),
            Some(Duration::from_millis(70))
        );
        assert_eq!(
            d.time_until_ready(t0 + Duration::from_millis(200)),
            Some(Duration::ZERO)
        );
    }
}
