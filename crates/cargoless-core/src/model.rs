//! File-level green/red model + event bus, with #21 verdict provenance.
//!
//! The daemon's single source of truth for "what works". It folds the
//! per-file diagnostics from [`crate::lsp`] into the `cargoless_proto` contract:
//! level-triggered [`StateEvent::FileVerdict`] and edge-triggered
//! [`StateEvent::BecameGreen`] / [`StateEvent::BecameRed`].
//!
//! ## #21 — cargo-check is the verdict AUTHORITY (load-bearing for v0)
//!
//! S1 proved RA-native diagnostics are BLIND to the type/trait/method/macro
//! error class — only `cargo check` (RA's *flycheck*) produces it. A checker
//! that called such code GREEN off RA-native would violate the product's one
//! promise. So GREEN is gated strictly on a completed flycheck pass:
//!
//! * GREEN ⟺ at least one flycheck pass has COMPLETED and that pass left
//!   NO `severity == Error` diagnostic from ANY source.
//!   Pre-first-flycheck the tree is RED (never claim unproven green —
//!   the project-long invariant).
//!
//! ## #8-redo — severity:Error from ANY source drives RED (FIELD FINDING #55)
//!
//! The original #21 rule restricted RED to `source == "rustc"` errors and
//! treated RA-native errors as advisory-only. dogfood-lead's `let bad =`
//! reproducer broke this: an RA-native severity:Error on a parse failure
//! never reaches cargo-check (cargo errors before producing JSON
//! diagnostics for the broken file in some cases), so the rustc-only rule
//! reported GREEN on a tree that cargo check called RED. The fix tightens
//! the per-file rule to **any severity:Error from any source** while
//! leaving GREEN gating on flycheck-completion unchanged. The asymmetry
//! is honest: RA's "saw an error" is strictly stronger evidence than
//! "didn't see one" because RA's analysis is partial — so RA-native
//! severity:Error can drive RED (the bug evidence is real), but only
//! flycheck-completion + zero errors can drive GREEN (absence of
//! evidence must be backed by cargo's fuller analysis).
//!
//! [`ModelSession::subscribe_advisory`] / [`Verdict::provenance`] still
//! exist for the warning/info/hint advisory channel; only the verdict bit
//! has been broadened.
//!
//! ## Frozen-seam discipline
//!
//! `StateEvent` and the four `cargoless-proto` seams are byte-frozen. `check_once`,
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

use cargoless_proto::{
    BuildIdentity, CheckResult, ContentHash, Diagnostic, FileState, Profile, StateEvent,
    TargetTriple, TreeState,
};

use crate::lsp::LspEvent;

/// Hard ceiling for [`check_once`]/[`check_verdict`] (override:
/// `TF_CHECK_TIMEOUT_SECS`). A cold flycheck can take minutes; this only
/// bounds pathological hangs.
const CHECK_HARD_CAP: Duration = Duration::from_secs(180);
/// Quiet window: once events have arrived and none have for this long without
/// an authoritative pass, the one-shot check gives up (→ Red/Advisory).
const CHECK_SETTLE: Duration = Duration::from_secs(2);
/// Default debounce for the streaming [`watch`] pipeline. The save-burst
/// quiet window before a [`crate::watcher`] batch is emitted to the model.
/// Tuned to keep mid-edit reds quiet without making the post-save verdict
/// feel laggy; user-overridable via `TF_DEBOUNCE_MS` (set by the
/// `--debounce-ms` CLI flag — FIELD FINDING #5 / #49).
const DEFAULT_WATCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// Resolve the live debounce duration: `TF_DEBOUNCE_MS` env override iff
/// parseable as a positive `u64` milliseconds, else [`DEFAULT_WATCH_DEBOUNCE`].
/// Zero is rejected (would cause the watcher to spin); too-large values are
/// honored — dogfood tuning may legitimately want a long quiet window for
/// flicker-free large-refactor runs. Pure (only reads env, no side effects).
fn resolve_watch_debounce() -> Duration {
    std::env::var("TF_DEBOUNCE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_WATCH_DEBOUNCE)
}

fn ra_native_verdict_mode() -> bool {
    std::env::var("CARGOLESS_VERDICT_MODE")
        .ok()
        .map(|v| {
            let v = v.trim();
            v.eq_ignore_ascii_case("ra")
                || v.eq_ignore_ascii_case("advisory")
                || v.eq_ignore_ascii_case("development")
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// #21 additive provenance types (cargoless_core::model, serde-free — NOT cargoless-proto)
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

/// FIELD FINDING #6-NEG-A (#51): supervisor-lifecycle events surfaced to
/// the CLI so the watch stream is never silent during an AC#6 transparent
/// restart. Separate channel from [`StateEvent`] (which is the byte-frozen
/// cargoless-proto seam — must NOT grow) and [`Verdict`] (which is the #21
/// authoritative-vs-advisory verdict, not lifecycle). Additive only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleEvent {
    /// AC#6's supervisor just respawned the rust-analyzer child after a
    /// crash/kill. The next-verdict latency is "however long re-indexing
    /// takes" — typically tens of seconds on a real project. Emitted ONCE
    /// per transparent restart; NOT emitted on the initial spawn (the
    /// bring-up line covers that).
    AnalyzerRestarting,
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
    /// FIELD FINDING #2 additive surface: the most recent diagnostic list
    /// per file, indexed by path-from-URI. Replaced wholesale on every
    /// `publishDiagnostics` (RA's semantics — a publish supersedes the prior
    /// list for that file; an empty list clears it). The aggregated view is
    /// what the CLI prints; this is NOT part of the authoritative verdict
    /// rule (which still derives from `auth` + `flycheck_done`), just the
    /// human-facing detail the boolean verdict was hiding.
    diagnostics: BTreeMap<String, Vec<Diagnostic>>,
    /// FIELD FINDING #6-NEG-A (#51) additive surface: lifecycle event
    /// subscribers. Currently the only event is
    /// [`LifecycleEvent::AnalyzerRestarting`] emitted by the watch
    /// pipeline's `on_spawn` hook on every transparent RA restart (NOT on
    /// the initial spawn). Bounded by retain-on-send like the verdict
    /// channels so a dropped subscriber does not stall the producer.
    lifecycle_subscribers: Vec<Sender<LifecycleEvent>>,
    /// #126 Tier-3 bench/dogfood hook: count of `publishDiagnostics`
    /// folds where RA-native `severity:Error` was *demoted* out of the
    /// authoritative RED set (proc-macro-off downrank engaged). `0`
    /// when `TF_RA_PROCMACRO_OFF` is unset (default-off ⇒ F8-redo
    /// any-source rule, byte-identical). Lets bench-lead correlate the
    /// RSS delta with the false-RED-suppression actually firing.
    procmacro_downranked: u64,
}

/// #126 Tier-3 — the per-file authoritative-RED decision, factored
/// pure (no env, no `self`) so both modes are unit-tested
/// deterministically (the #88 discipline: isolate the env read, test
/// the logic). Returns `(file_state, native_downranked)`.
///
/// When `downrank == false` (default-off) this is the F8-redo (#55)
/// rule: ANY-source `severity:Error` ⇒ `Red` (RA-native parse errors
/// are real "cannot compile" evidence that beat cargo-check to the
/// punch). When `downrank == true` (`TF_RA_PROCMACRO_OFF=1`) RA
/// proc-macro is forced off, so RA-native hallucinates "unresolved"
/// for every macro-generated item — RED is driven from the
/// `source:"rustc"` (cargo-check) tier ONLY (`has_authoritative_error`).
/// cargo-check expands proc-macros itself, RA-independently, so it
/// stays the complete authority; RA-native is demoted to advisory
/// (still in `native`/diagnostics, just not verdict-driving).
/// `native_downranked` is `true` iff there WERE any-source errors but
/// NO authoritative one — i.e. this fold's RED was suppressed-as-false
/// (the bench/dogfood signal that the −53 % RAM mode is firing safely).
fn file_state_for(pd: &crate::lsp::PublishDiagnostics, downrank: bool) -> (FileState, bool) {
    if downrank {
        let st = if pd.has_authoritative_error() {
            FileState::Red
        } else {
            FileState::Green
        };
        let suppressed = pd.has_any_severity_error() && !pd.has_authoritative_error();
        (st, suppressed)
    } else {
        let st = if pd.has_any_severity_error() {
            FileState::Red
        } else {
            FileState::Green
        };
        (st, false)
    }
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
            diagnostics: BTreeMap::new(),
            lifecycle_subscribers: Vec::new(),
            procmacro_downranked: 0,
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

    /// Subscribe to the supervisor-lifecycle stream (FIELD FINDING #6-NEG-A
    /// / #51). Currently fires [`LifecycleEvent::AnalyzerRestarting`] once
    /// per transparent RA restart. Additive — distinct from `subscribe()`
    /// (the frozen StateEvent seam) and `subscribe_advisory()` (the #21
    /// verdict provenance channel).
    pub fn subscribe_lifecycle(&mut self) -> Receiver<LifecycleEvent> {
        let (tx, rx) = channel();
        self.lifecycle_subscribers.push(tx);
        rx
    }

    /// Fan-out a lifecycle event to every live subscriber; prune dropped
    /// ones. Producer is the watch pipeline's on_spawn hook.
    pub(crate) fn emit_lifecycle(&mut self, ev: LifecycleEvent) {
        self.lifecycle_subscribers.retain(|s| s.send(ev).is_ok());
    }

    /// Current AUTHORITATIVE aggregate verdict (frozen signature).
    pub fn tree_state(&self) -> TreeState {
        self.tree
    }

    /// #122 Tier-4: has ≥1 authoritative `cargo check` (flycheck) pass
    /// completed? The idle-evict trigger gates on this so the cold
    /// first authoritative pass is never interrupted by an eviction.
    pub fn flycheck_done(&self) -> bool {
        self.flycheck_done
    }

    /// #126 Tier-3 bench/dogfood hook: how many `publishDiagnostics`
    /// folds had RA-native `severity:Error` demoted out of the
    /// authoritative RED set (proc-macro-off downrank suppressing a
    /// would-be false-RED). `0` while `TF_RA_PROCMACRO_OFF` is unset.
    pub fn procmacro_downranked(&self) -> u64 {
        self.procmacro_downranked
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

    /// FIELD FINDING #2: the diagnostics last reported for `path`, in
    /// publish order. Empty iff RA has explicitly cleared this file (or
    /// never reported on it). The aggregate stream is
    /// [`Self::all_diagnostics`].
    pub fn file_diagnostics(&self, path: &str) -> &[Diagnostic] {
        self.diagnostics.get(path).map(Vec::as_slice).unwrap_or(&[])
    }

    /// FIELD FINDING #2: every known diagnostic, flattened across files in
    /// deterministic path order. This is what the CLI prints — pairing
    /// `tree_state` with `all_diagnostics` is the rich verdict the boolean
    /// `TreeState` alone could not surface.
    pub fn all_diagnostics(&self) -> Vec<Diagnostic> {
        let total: usize = self.diagnostics.values().map(Vec::len).sum();
        let mut out = Vec::with_capacity(total);
        for v in self.diagnostics.values() {
            out.extend(v.iter().cloned());
        }
        out
    }

    /// Fold one [`LspEvent`] into the model.
    pub fn apply_event(&mut self, ev: &LspEvent) {
        match ev {
            LspEvent::Diagnostics(pd) => {
                let Some(path) = crate::lsp::path_from_uri(&pd.uri) else {
                    return;
                };
                // FIELD FINDING #8-redo: per-file RED on ANY severity:Error
                // (rustc-tier OR rust-analyzer-native). The #21 design
                // restricted authoritative-RED to rustc-source only, but
                // dogfood-lead's `let bad =` reproducer showed that
                // RA-native parse errors don't make it to cargo-check (so
                // never publish as source:rustc) yet are unambiguous
                // evidence the file cannot compile. The asymmetry is
                // honest: RA's "saw an error" is strictly stronger
                // evidence than "didn't see one" because RA's analysis
                // is partial — so RA-native severity:Error can drive RED
                // (the bug evidence is real), but only flycheck-completion
                // + zero errors can drive GREEN (the absence of evidence
                // must be backed by cargo's fuller analysis).
                // #126 Tier-3: pure decision (env isolated). In
                // proc-macro-off downrank mode RED is rustc-source only
                // (cargo-check = complete RA-independent authority);
                // otherwise the F8-redo any-source rule. RA-native is
                // never lost — it still flows to `native`/advisory/
                // diagnostics below, just not the authoritative verdict.
                let (file_state, downranked) = file_state_for(pd, crate::procmacro::enabled());
                if downranked {
                    self.procmacro_downranked += 1;
                }
                let native_state = if pd.advisory_errors > 0 {
                    FileState::Red
                } else {
                    FileState::Green
                };
                self.auth.insert(path.clone(), file_state);
                self.native.insert(path.clone(), native_state);
                // FIELD FINDING #2: stash the rich diagnostic list for this
                // file. RA's `publishDiagnostics` replaces the list (an empty
                // list clears it) — mirror that exactly so the CLI's
                // aggregate view never shows a stale error for a fixed file.
                if pd.diagnostics.is_empty() {
                    self.diagnostics.remove(&path);
                } else {
                    self.diagnostics
                        .insert(path.clone(), pd.diagnostics.clone());
                }
                // FileVerdict is the authoritative per-file settle —
                // emits the broadened-per-#8-redo `file_state` (any
                // severity:Error from any source → Red), matching the
                // tree-derivation rule above.
                self.emit(StateEvent::FileVerdict {
                    path,
                    state: file_state,
                });
                self.reconcile();
                self.emit_advisory();
            }
            LspEvent::FlycheckEnded => {
                self.flycheck_done = true;
                self.reconcile();
                self.emit_advisory();
            }
            LspEvent::FlycheckFailed { message } => {
                let pd = crate::lsp::flycheck_failure_diagnostics(message.clone());
                self.apply_event(&LspEvent::Diagnostics(pd));
                self.flycheck_done = true;
                self.reconcile();
                self.emit_advisory();
            }
            LspEvent::IndexingEnded => {
                // FIELD FINDING #3a: the watch-mode model's authoritative
                // GREEN is already correctly gated on `flycheck_done` (the
                // #21 rule), and a real RA only fires a flycheck pass AFTER
                // indexing completes — so the model itself does not need to
                // gate on indexing here. The signal exists in this enum to
                // un-stick the one-shot `check_*` loops (which were
                // settle-early-breaking before the first flycheck on cold
                // RA); the watch loop sees it pass through and ignores it.
                // A future "still warming up" UI signal could light up off
                // this — out of #43 scope.
            }
        }
    }

    /// A document went away (deleted / gitignored).
    pub fn forget_file(&mut self, path: &str) {
        let a = self.auth.remove(path).is_some();
        let n = self.native.remove(path).is_some();
        // FIELD FINDING #2: keep the diagnostics map in sync — a deleted file
        // must not haunt the CLI's aggregate view with stale errors.
        let d = self.diagnostics.remove(path).is_some();
        if a || n || d {
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
            if ra_native_verdict_mode() {
                if self.auth.is_empty() {
                    return TreeState::Red;
                }
                return if self.auth.values().any(|s| *s == FileState::Red) {
                    TreeState::Red
                } else {
                    TreeState::Green
                };
            }
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
    // FIELD FINDING #3b: wrap the child in a `ReapOnDrop` BEFORE any
    // `?` early-return path so a failing LSP handshake (handshake EOF,
    // pipe error, etc.) no longer leaks the child + its proc-macro-srv
    // grandchildren on Unix.
    let mut guard = crate::analyzer::ReapOnDrop::new(cmd.spawn()?);
    let (stdin, stdout) = guard
        .take_stdio()
        .ok_or_else(|| io::Error::other("rust-analyzer stdio unavailable"))?;
    let root_str = root.to_string_lossy().into_owned();
    // FIELD FINDING #74: lean InitOpts derived from env vars
    // (TF_PROC_MACRO / TF_FEATURES — set by the CLI's --proc-macro /
    // --features flags) + Cargo.toml auto-detection at the project root.
    // The lean initializationOptions cut RA's idle/check cost ~30-50%
    // without breaking the F8-redo verdict gate (checkOnSave stays
    // enabled per Option B+).
    let init_opts = crate::lsp::InitOpts::from_env_and_project(&root);
    let (client, events) = crate::lsp::LspClient::initialize(stdin, stdout, &root_str, &init_opts)?;

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
    // FIELD FINDING #3a: RA fires advisory `publishDiagnostics` during
    // indexing (got_any=true), then goes quiet for 5-10s while the rest of
    // indexing/proc-macro-bringup completes; the old code's "if got_any
    // and Timeout, break early" path fired during THAT quiet and reported
    // the unproven red. Gate the early-break on `indexing_done` so cold
    // RA gets the project-ready signal it deserves before we give up.
    let mut indexing_done = false;
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
                    // FIELD FINDING #8-redo: severity:Error from ANY
                    // source flips the file Red — matches the model's
                    // apply_event rule. RA-native parse errors (e.g.
                    // `let bad =` at file scope, the dogfood reproducer)
                    // never make it to cargo-check, so the original
                    // rustc-source-only rule produced a silent-green
                    // verdict on a broken tree. Treating any severity:
                    // Error as authoritative-for-RED is the honest fix.
                    let s = if pd.has_any_severity_error() {
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
            Ok(LspEvent::FlycheckFailed { message }) => {
                let pd = crate::lsp::flycheck_failure_diagnostics(message);
                if let Some(p) = crate::lsp::path_from_uri(&pd.uri) {
                    auth.insert(p, FileState::Red);
                }
                flycheck_seen = true;
            }
            Ok(LspEvent::IndexingEnded) => {
                indexing_done = true;
            }
            Err(RecvTimeoutError::Timeout) => {
                // Only allow settle-early once RA has actually finished
                // indexing — otherwise the quiet IS the indexing window
                // and breaking here false-reds a green tree (#43).
                if got_any && indexing_done {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break, // RA exited
        }
    }
    // `guard` drops here — `ReapOnDrop` SIGKILLs RA's whole process group
    // on Unix (RA + every proc-macro-srv grandchild) and waits to reap;
    // on non-Unix it falls back to killing just the immediate child.
    // The explicit `drop(guard)` makes the scope-end deterministic and
    // documents the reap point.
    drop(guard);

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

/// FIELD FINDING #2: one-shot AUTHORITATIVE verdict for `root` PAIRED with
/// the diagnostic list (file/line/col/severity/code/message/source) the CLI
/// needs to print. Spins up rust-analyzer with flycheck on, opens every
/// workspace `.rs`, waits for a completed `cargo check` pass, then returns
/// the [`CheckResult`] (the [`TreeState`] every existing caller uses, plus
/// the per-file diagnostics every red tree carries).
///
/// **Additive alongside** [`check_once`] / [`check_verdict`]: those keep
/// their byte-frozen signatures (cli-ux's `watch`, bench-lead's harness, and
/// the #21 advisory channel are all unchanged). This is the parallel rich
/// API the CLI's `check` command binds to.
///
/// `Err` = setup/env failure (rust-analyzer missing, spawn/pipe error) — the
/// CLI must surface this distinctly from "code is red". A run that never sees
/// an authoritative flycheck pass yields `CheckResult { Red, diagnostics }`
/// with whatever diagnostics RA emitted before the timeout (never claim
/// unproven green — AC#4 stays inviolable here too).
pub fn check_once_with_diagnostics(root: &Path) -> io::Result<CheckResult> {
    let root = fs::canonicalize(root)?;
    let mut cmd = crate::analyzer::rust_analyzer_command()?;
    cmd.current_dir(&root);
    // FIELD FINDING #3b: same guard as `check_verdict` — Drop-based reap
    // covers every early-return path (handshake EOF, pipe error, etc.)
    // and on Unix takes out the whole RA process group so
    // `rust-analyzer-proc-macro-srv` grandchildren do not accumulate.
    let mut guard = crate::analyzer::ReapOnDrop::new(cmd.spawn()?);
    let (stdin, stdout) = guard
        .take_stdio()
        .ok_or_else(|| io::Error::other("rust-analyzer stdio unavailable"))?;
    let root_str = root.to_string_lossy().into_owned();
    // FIELD FINDING #74: lean InitOpts derived from env vars
    // (TF_PROC_MACRO / TF_FEATURES — set by the CLI's --proc-macro /
    // --features flags) + Cargo.toml auto-detection at the project root.
    // The lean initializationOptions cut RA's idle/check cost ~30-50%
    // without breaking the F8-redo verdict gate (checkOnSave stays
    // enabled per Option B+).
    let init_opts = crate::lsp::InitOpts::from_env_and_project(&root);
    let (client, events) = crate::lsp::LspClient::initialize(stdin, stdout, &root_str, &init_opts)?;

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
    // Reuse the model's per-file-replace semantics: a later publish for the
    // same file SUPERSEDES the earlier one (empty publish clears). This is
    // how `apply_event` does it; mirroring keeps the CLI's one-shot view
    // consistent with what `watch` shows live.
    let mut diagnostics: BTreeMap<String, Vec<Diagnostic>> = BTreeMap::new();
    let mut flycheck_seen = false;
    let mut got_any = false;
    // FIELD FINDING #3a: identical gate to `check_verdict`. The false-red
    // on a cold green tree happens because RA fires advisory diags during
    // indexing, then goes silent — the old settle-early-on-got_any path
    // mistook the silence for completion. `indexing_done` blocks the
    // early-break until RA has actually announced project-ready.
    let mut indexing_done = false;
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
                    // FIELD FINDING #8-redo: severity:Error from ANY
                    // source flips Red — same rule as check_verdict and
                    // the model's apply_event (see those for the full
                    // honest-asymmetry rationale: RA-native parse errors
                    // are real evidence; cargo-check is the only thing
                    // that can earn GREEN).
                    let s = if pd.has_any_severity_error() {
                        FileState::Red
                    } else {
                        FileState::Green
                    };
                    auth.insert(p.clone(), s);
                    if pd.diagnostics.is_empty() {
                        diagnostics.remove(&p);
                    } else {
                        diagnostics.insert(p, pd.diagnostics.clone());
                    }
                }
            }
            Ok(LspEvent::FlycheckEnded) => {
                flycheck_seen = true;
            }
            Ok(LspEvent::FlycheckFailed { message }) => {
                let pd = crate::lsp::flycheck_failure_diagnostics(message);
                if let Some(p) = crate::lsp::path_from_uri(&pd.uri) {
                    auth.insert(p.clone(), FileState::Red);
                    diagnostics.insert(p, pd.diagnostics);
                }
                flycheck_seen = true;
            }
            Ok(LspEvent::IndexingEnded) => {
                indexing_done = true;
            }
            Err(RecvTimeoutError::Timeout) => {
                if got_any && indexing_done {
                    break; // settled, project ready, no authoritative pass
                }
            }
            Err(RecvTimeoutError::Disconnected) => break, // RA exited
        }
    }
    // Same deterministic reap-point as `check_verdict` (see comment there).
    drop(guard);

    let tree = if flycheck_seen {
        if auth.values().any(|s| *s == FileState::Red) {
            TreeState::Red
        } else {
            TreeState::Green
        }
    } else {
        // No authoritative pass observed → unproven, never green (AC#4).
        TreeState::Red
    };
    let total: usize = diagnostics.values().map(Vec::len).sum();
    let mut flat = Vec::with_capacity(total);
    for v in diagnostics.values() {
        flat.extend(v.iter().cloned());
    }
    let project = crate::project_checks::run_dev(&root)?;
    flat.extend(project.diagnostics);
    let tree = if tree == TreeState::Red || project.tree == TreeState::Red {
        TreeState::Red
    } else {
        TreeState::Green
    };
    Ok(CheckResult {
        tree,
        diagnostics: flat,
    })
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
    /// #112 structural-trigger spike — bench-lead measurement hook
    /// (D-OPENCLOSED §4.2). Additive; not part of any frozen seam.
    structural_counters: Arc<crate::structural::StructuralCounters>,
    /// #122 Tier-4 idle-evict — bench-lead measurement hook
    /// (composes with #116 stage-3). Additive; no frozen seam.
    idle_evict_counters: Arc<crate::idle::IdleEvictCounters>,
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

    /// FIELD FINDING #2: every diagnostic the model has accumulated, across
    /// every reporting file. Returned in deterministic path order so the
    /// CLI's `watch` mode can re-print a stable view on each transition
    /// without flicker. Snapshot — the lock is released before the caller
    /// formats output, so this is safe to call on every event.
    pub fn current_diagnostics(&self) -> Vec<Diagnostic> {
        poisoned(&self.model).all_diagnostics()
    }

    /// FIELD FINDING #6-NEG-A (#51): subscribe to supervisor-lifecycle
    /// events. The CLI watch loop drains this on every iteration and prints
    /// a stream signal so the user is never staring at silence during an
    /// AC#6 transparent restart's 30-60s re-index window.
    pub fn subscribe_lifecycle(&self) -> Receiver<LifecycleEvent> {
        poisoned(&self.model).subscribe_lifecycle()
    }

    /// #112 structural-trigger spike — bench-lead measurement hook
    /// (D-OPENCLOSED §4.2). `(settled_batches, closed_batches)`:
    /// `1 − closed/settled` is the fraction of authoritative
    /// cargo-checks the structural gate eliminated per agent-edit-batch.
    /// `(0, 0)` while `TF_STRUCTURAL_TRIGGER` is unset (default-off ⇒ no
    /// counting, prior path byte-identical). Additive — touches no
    /// frozen seam.
    pub fn structural_counters(&self) -> (u64, u64) {
        self.structural_counters.snapshot()
    }

    /// #122 Tier-4 idle-evict — bench-lead measurement hook
    /// (composes with #116 stage-3 fleet-scale). `(evictions,
    /// suspended_ms)`: `suspended_ms` ≈ the time-averaged ~2 GB RA RSS
    /// reclaimed. `(0, 0)` while `TF_RA_IDLE_EVICT` is unset
    /// (default-off ⇒ no eviction, prior path byte-identical).
    /// Additive — touches no frozen seam.
    pub fn idle_evict_counters(&self) -> (u64, u64) {
        self.idle_evict_counters.snapshot()
    }

    /// #126 Tier-3 bench/dogfood hook: count of RA-native severity:Error
    /// folds demoted out of the authoritative verdict by the
    /// proc-macro-off downrank (the false-RED-suppression firing). `0`
    /// while `TF_RA_PROCMACRO_OFF` is unset (default-off, byte-
    /// identical). Additive — touches no frozen seam.
    pub fn procmacro_downranked(&self) -> u64 {
        poisoned(&self.model).procmacro_downranked()
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
    // FIELD FINDING #6-NEG-A (#51): per-watch counter of on_spawn calls.
    // n == 0 ⇒ the initial spawn (covered by the CLI's bring-up line — no
    // restart signal needed). n >= 1 ⇒ a transparent AC#6 restart after
    // RA crashed/was killed; emit `LifecycleEvent::AnalyzerRestarting`
    // BEFORE the LSP handshake starts (so the CLI sees the signal while
    // the model is still cold).
    let spawn_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook_spawn_count = Arc::clone(&spawn_count);
    let on_spawn = move |child: &mut std::process::Child| {
        let n = hook_spawn_count.fetch_add(1, Ordering::SeqCst);
        if n > 0 {
            // Transparent restart — tell the CLI so the stream doesn't go
            // silent for the 30-60s reindex window. Emit BEFORE the
            // handshake so a slow-handshake restart is visible immediately.
            poisoned(&hook_model).emit_lifecycle(LifecycleEvent::AnalyzerRestarting);
        }
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return;
        };
        // FIELD FINDING #74: same lean InitOpts as the one-shot check
        // paths above — read env vars (TF_PROC_MACRO / TF_FEATURES) +
        // auto-detect proc-macro from project Cargo.toml. On a
        // transparent AC#6 restart the env reads re-execute, so a CLI
        // change between restarts is respected by the new RA instance.
        let init_opts = crate::lsp::InitOpts::from_env_and_project(Path::new(&hook_root));
        let Ok((client, events)) =
            crate::lsp::LspClient::initialize(stdin, stdout, &hook_root, &init_opts)
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
    // #122 Tier-4 idle-evict: cheap clonable handle so the fs-batch
    // loop can suspend/resume RA without owning the Supervisor (which
    // ModelSession owns). Only ever exercised when `TF_RA_IDLE_EVICT=1`.
    let suspend_handle = supervisor.suspend_handle();

    let (watch_handle, batches) =
        crate::watcher::watch(&root, resolve_watch_debounce()).map_err(io::Error::other)?;
    // #112 structural-trigger spike: bench-lead measurement counters,
    // shared into the fs thread and exposed via ModelSession. Only move
    // while `TF_STRUCTURAL_TRIGGER=1` (the gated branch); dormant (0,0)
    // when default-off so an instrumented run is opt-in.
    let structural_counters = Arc::new(crate::structural::StructuralCounters::new());
    // #122 Tier-4 bench hook (composes with #116 stage-3). Dormant
    // (0,0) when default-off so an instrumented run is opt-in.
    let idle_counters = Arc::new(crate::idle::IdleEvictCounters::new());
    let mut threads = Vec::new();
    {
        let model = Arc::clone(&model);
        let stop = Arc::clone(&stop);
        let current = Arc::clone(&current);
        let structural = Arc::clone(&structural_counters);
        let idle_counters = Arc::clone(&idle_counters);
        let suspend_handle = suspend_handle.clone();
        threads.push(
            thread::Builder::new()
                .name("tf-model-fs".into())
                .spawn(move || {
                    let mut version: i64 = 2;
                    // #122 Tier-4 idle-evict tracking. Only consulted
                    // when TF_RA_IDLE_EVICT=1; inert otherwise.
                    let mut last_activity = std::time::Instant::now();
                    let mut suspended_since: Option<std::time::Instant> = None;
                    loop {
                        match batches.recv_timeout(Duration::from_millis(250)) {
                            Ok(batch) => {
                                // #122 Tier-4: if RA was idle-evicted,
                                // bring it back BEFORE touching the
                                // client. resume() respawns via the
                                // unchanged AC#6 path (LSP re-init +
                                // re-did_open at CURRENT content); wait
                                // (bounded by the AC#1 bring-up budget)
                                // for the fresh child so this batch's
                                // verdict comes from the SAME
                                // post-restart path ac6_kill9 proves
                                // correct. Zero syscalls when
                                // default-off.
                                if crate::idle::enabled() && suspend_handle.is_suspended() {
                                    suspend_handle.resume();
                                    let deadline =
                                        std::time::Instant::now() + Duration::from_secs(35);
                                    while !suspend_handle.child_alive()
                                        && std::time::Instant::now() < deadline
                                    {
                                        thread::sleep(Duration::from_millis(50));
                                    }
                                    if let Some(since) = suspended_since.take() {
                                        idle_counters.add_suspended(since.elapsed());
                                    }
                                }
                                last_activity = std::time::Instant::now();
                                let client = poisoned(&current).as_ref().cloned();
                                if crate::structural::enabled() {
                                    // #112 (D-OPENCLOSED): gate the cargo-check
                                    // spend on the coalesced batch being
                                    // structurally CLOSED. ALWAYS didChange
                                    // (RA re-parses → RA-native severity:Error
                                    // still flips per-file RED via the
                                    // UNTOUCHED F8-redo path); only didSave
                                    // (the flycheck/cargo-check trigger) when
                                    // every .rs file in the batch is CLOSED.
                                    // Closedness gates SPEND + publish-
                                    // eligibility, NEVER the verdict colour.
                                    let mut files: Vec<(String, String)> = Vec::new();
                                    for path in batch {
                                        if path.extension().is_none_or(|e| e != "rs") {
                                            continue;
                                        }
                                        let p = path.to_string_lossy().into_owned();
                                        match fs::read_to_string(&path) {
                                            Ok(text) => files.push((p, text)),
                                            Err(_) => poisoned(&model).forget_file(&p),
                                        }
                                    }
                                    // A batch is worth a cargo-check iff NO
                                    // file in it is OPEN (D-OPENCLOSED §2.4).
                                    let all_closed =
                                        files.iter().all(|(_, t)| crate::structural::is_closed(t));
                                    structural.record(all_closed);
                                    for (p, text) in &files {
                                        version += 1;
                                        if let Some(c) = client.as_ref() {
                                            let _ = c.did_change(p, text, version);
                                            if all_closed {
                                                let _ = c.did_save(p);
                                            }
                                        }
                                    }
                                } else {
                                    // DEFAULT-OFF: prior path, byte-identical.
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
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                if stop.load(Ordering::SeqCst) {
                                    break;
                                }
                                // #122 Tier-4: reclaim RA's ~2 GB during
                                // the long, provably check-free
                                // agent-idle gaps. Gated on
                                // `flycheck_done` so the cold first
                                // authoritative pass is NEVER
                                // interrupted; only once idle ≥ the
                                // window and not already suspended.
                                // Zero syscalls when default-off.
                                if crate::idle::enabled()
                                    && !suspend_handle.is_suspended()
                                    && poisoned(&model).flycheck_done()
                                    && last_activity.elapsed() >= crate::idle::idle_window()
                                {
                                    suspend_handle.suspend();
                                    idle_counters.record_eviction();
                                    suspended_since = Some(std::time::Instant::now());
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
        structural_counters,
        idle_evict_counters: idle_counters,
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
            // Counts-only helper: the per-file rich list isn't what these
            // #21 tests assert on; the dedicated FIELD FINDING #2 tests
            // below populate it via `diag_rich`.
            diagnostics: Vec::new(),
        })
    }

    fn diag_rich(uri: &str, ds: Vec<Diagnostic>) -> LspEvent {
        let mut auth = 0usize;
        let mut adv = 0usize;
        for d in &ds {
            if d.severity == cargoless_proto::Severity::Error {
                if d.source.as_deref() == Some("rustc") {
                    auth += 1;
                } else {
                    adv += 1;
                }
            }
        }
        LspEvent::Diagnostics(crate::lsp::PublishDiagnostics {
            uri: uri.into(),
            authoritative_errors: auth,
            advisory_errors: adv,
            total: ds.len(),
            diagnostics: ds,
        })
    }

    // -----------------------------------------------------------------
    // #126 Tier-3 — file_state_for: proc-macro-off RA-native-downrank.
    // Pure decision, env isolated (the #88 discipline); exhaustive.
    // -----------------------------------------------------------------

    #[test]
    fn file_state_for_downrank_vs_f8redo_default() {
        fn pd(auth: usize, adv: usize) -> crate::lsp::PublishDiagnostics {
            crate::lsp::PublishDiagnostics {
                uri: "file:///x.rs".into(),
                authoritative_errors: auth,
                advisory_errors: adv,
                total: auth + adv,
                diagnostics: Vec::new(),
            }
        }

        // DEFAULT (downrank=false) — F8-redo #55: ANY-source error ⇒ Red,
        // never "downranked". Byte-identical to pre-#126.
        assert_eq!(file_state_for(&pd(0, 0), false), (FileState::Green, false));
        assert_eq!(file_state_for(&pd(1, 0), false), (FileState::Red, false));
        assert_eq!(
            file_state_for(&pd(0, 1), false),
            (FileState::Red, false),
            "F8-redo: RA-native-only error still drives RED by default"
        );

        // DOWNRANK (proc-macro-off): rustc-source ONLY drives RED.
        assert_eq!(file_state_for(&pd(0, 0), true), (FileState::Green, false));
        assert_eq!(
            file_state_for(&pd(1, 0), true),
            (FileState::Red, false),
            "no false-GREEN: a real cargo-check error still drives RED"
        );
        assert_eq!(
            file_state_for(&pd(0, 1), true),
            (FileState::Green, true),
            "the fix: RA-native-only (proc-macro hallucination) is \
             demoted — would-be false-RED suppressed + counted"
        );
        assert_eq!(
            file_state_for(&pd(1, 1), true),
            (FileState::Red, false),
            "authoritative present ⇒ RED, NOT a suppression (rustc \
             evidence is real; nothing was downranked away)"
        );
    }

    fn mk_diag(
        path: &str,
        line: u32,
        col: u32,
        sev: cargoless_proto::Severity,
        code: Option<&str>,
        msg: &str,
        source: Option<&str>,
    ) -> Diagnostic {
        Diagnostic {
            file_path: std::path::PathBuf::from(path),
            line,
            col,
            severity: sev,
            code: code.map(str::to_owned),
            message: msg.to_owned(),
            source: source.map(str::to_owned),
        }
    }

    #[test]
    fn lifecycle_subscribers_receive_analyzer_restarting() {
        // FIELD FINDING #6-NEG-A (#51) — the model's lifecycle bus
        // delivers AnalyzerRestarting to every live subscriber, and a
        // dropped subscriber does not panic the producer.
        let mut m = model();
        let r1 = m.subscribe_lifecycle();
        {
            let r2 = m.subscribe_lifecycle();
            m.emit_lifecycle(LifecycleEvent::AnalyzerRestarting);
            assert_eq!(
                drain(&r1),
                vec![LifecycleEvent::AnalyzerRestarting],
                "r1 receives"
            );
            assert_eq!(
                drain(&r2),
                vec![LifecycleEvent::AnalyzerRestarting],
                "r2 receives"
            );
        }
        // r2 dropped; emit must not panic and r1 must still receive.
        m.emit_lifecycle(LifecycleEvent::AnalyzerRestarting);
        assert_eq!(drain(&r1).len(), 1);
    }

    #[test]
    fn lifecycle_no_emit_before_first_restart_is_silent() {
        // Sanity: just constructing a model does NOT emit
        // AnalyzerRestarting (that would falsely tell the CLI "restarting"
        // when the analyzer hasn't even started). The on_spawn hook is
        // responsible for the first-spawn-vs-restart distinction.
        let mut m = model();
        let rx = m.subscribe_lifecycle();
        assert!(drain(&rx).is_empty(), "no lifecycle events on quiet model");
        // The verdict bus also stays quiet — lifecycle and verdict are
        // strictly separate channels.
        m.apply_event(&LspEvent::FlycheckEnded);
        assert!(
            drain(&rx).is_empty(),
            "flycheck-end is a verdict event, not a lifecycle event"
        );
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
    fn ra_native_severity_error_post_flycheck_is_red_too() {
        // FIELD FINDING #8-redo (#55): dogfood-lead's smoking-gun case.
        // After a clean flycheck (tree green), a NEW edit with an
        // RA-native severity:Error (e.g. `let bad =` at file scope —
        // RA's parser catches it but cargo-check may not produce a
        // source:rustc diagnostic for it) MUST flip the tree Red. Under
        // the original #21 rustc-only rule this case was silently
        // green-on-broken — the worst possible v0 failure mode.
        let mut m = model();
        let rx = m.subscribe();
        // Get to authoritative green.
        m.apply_event(&diag("file:///p/lib.rs", 0, 0));
        m.apply_event(&LspEvent::FlycheckEnded);
        assert_eq!(m.tree_state(), TreeState::Green);
        let _ = drain(&rx);
        // User saves `let bad =` — RA-native severity:Error fires; cargo
        // hasn't re-checked yet (or won't, if cargo-check itself errors
        // before producing per-file JSON output for this file).
        m.apply_event(&diag("file:///p/lib.rs", 0, 1)); // 1 ra-native err
        assert_eq!(
            m.tree_state(),
            TreeState::Red,
            "RA-native severity:Error post-flycheck must flip tree Red"
        );
        // The transition event fires on the verdict bus.
        assert!(drain(&rx).contains(&StateEvent::BecameRed));
        // Provenance stays Authoritative (a flycheck has completed —
        // that's what the provenance flag tracks, not which tier the
        // current red comes from).
        assert_eq!(
            m.last_verdict().provenance,
            VerdictProvenance::Authoritative
        );
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

    // -----------------------------------------------------------------------
    // FIELD FINDING #5 (#49) — TF_DEBOUNCE_MS env override semantics
    //
    // Per-test env mutation is unavoidable here (the function reads the
    // process env on purpose) — `set_var` is `unsafe` on Edition 2024 due
    // to the multi-threaded-read hazard; serializing tests + the
    // remove_var-on-exit guard below keeps them deterministic.
    // -----------------------------------------------------------------------

    /// RAII guard: set `TF_DEBOUNCE_MS` for the test's scope, restore the
    /// pre-test value (or unset) on drop.
    struct EnvGuard {
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(value: &str) -> Self {
            let prev = std::env::var("TF_DEBOUNCE_MS").ok();
            // SAFETY: tests gated on `#[cfg(test)]`; this module's tests do
            // not spawn threads that read the env.
            unsafe { std::env::set_var("TF_DEBOUNCE_MS", value) };
            Self { prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("TF_DEBOUNCE_MS", v),
                    None => std::env::remove_var("TF_DEBOUNCE_MS"),
                }
            }
        }
    }

    #[test]
    fn watch_debounce_defaults_when_env_unset() {
        // Belt-and-braces: clear first, then probe the default.
        let _g = EnvGuard::set("");
        unsafe { std::env::remove_var("TF_DEBOUNCE_MS") };
        let d = resolve_watch_debounce();
        assert_eq!(d, Duration::from_millis(150), "default = 150ms");
        // Restore via guard's Drop (sets to "", which the parser rejects →
        // also falls back to default — that's fine for the next test).
    }

    #[test]
    fn watch_debounce_honors_valid_env_override() {
        let _g = EnvGuard::set("300");
        assert_eq!(resolve_watch_debounce(), Duration::from_millis(300));
    }

    #[test]
    fn watch_debounce_rejects_zero_and_garbage() {
        // Zero ⇒ spinloop hazard, rejected → default.
        let _g0 = EnvGuard::set("0");
        assert_eq!(resolve_watch_debounce(), Duration::from_millis(150));
        drop(_g0);
        // Non-numeric ⇒ rejected → default. (No fail-loud; the CLI parser
        // rejects bad input upstream; here we fail-safe on accidental env
        // pollution.)
        let _g1 = EnvGuard::set("nope");
        assert_eq!(resolve_watch_debounce(), Duration::from_millis(150));
    }

    #[test]
    fn watch_debounce_accepts_large_values_for_flicker_free_refactor() {
        // Sweet spot per F5 is 300-1000ms but dogfood may want longer for
        // a noisy multi-file refactor; no artificial upper bound.
        let _g = EnvGuard::set("2500");
        assert_eq!(resolve_watch_debounce(), Duration::from_millis(2500));
    }

    #[test]
    fn placeholder_identity_is_sentinel_dev_wasm() {
        let id = placeholder_identity();
        assert_eq!(id.profile, Profile::Dev);
        assert_eq!(id.target.as_str(), "wasm32-unknown-unknown");
        assert_eq!(id.source_tree, id.cargo_lock); // all the same sentinel
    }

    // -----------------------------------------------------------------------
    // FIELD FINDING #2 — diagnostic storage / aggregate view / forget sync
    // -----------------------------------------------------------------------

    #[test]
    fn diagnostics_accumulate_per_file_and_aggregate() {
        let mut m = model();
        // Two files, each with one rustc error + a position.
        m.apply_event(&diag_rich(
            "file:///p/src/a.rs",
            vec![mk_diag(
                "/p/src/a.rs",
                10,
                3,
                cargoless_proto::Severity::Error,
                Some("E0277"),
                "the trait bound not satisfied",
                Some("rustc"),
            )],
        ));
        m.apply_event(&diag_rich(
            "file:///p/src/b.rs",
            vec![mk_diag(
                "/p/src/b.rs",
                1,
                1,
                cargoless_proto::Severity::Warning,
                Some("unused_imports"),
                "unused import",
                Some("rust-analyzer"),
            )],
        ));
        let all = m.all_diagnostics();
        assert_eq!(all.len(), 2, "two diagnostics, one per file");
        assert_eq!(m.file_diagnostics("/p/src/a.rs").len(), 1);
        assert_eq!(m.file_diagnostics("/p/src/b.rs").len(), 1);
        // Codes survived end-to-end (the FIELD FINDING #2 ask).
        let codes: Vec<&str> = all.iter().filter_map(|d| d.code.as_deref()).collect();
        assert!(codes.contains(&"E0277"));
        assert!(codes.contains(&"unused_imports"));
    }

    #[test]
    fn later_publish_replaces_prior_per_file() {
        // RA's contract: publishDiagnostics REPLACES the prior list for that
        // file. Test that the model mirrors that (no stale errors after a
        // fix).
        let mut m = model();
        m.apply_event(&diag_rich(
            "file:///p/src/a.rs",
            vec![mk_diag(
                "/p/src/a.rs",
                1,
                1,
                cargoless_proto::Severity::Error,
                Some("E0277"),
                "first",
                Some("rustc"),
            )],
        ));
        assert_eq!(m.file_diagnostics("/p/src/a.rs").len(), 1);
        // A second publish with a different diagnostic supersedes the first.
        m.apply_event(&diag_rich(
            "file:///p/src/a.rs",
            vec![mk_diag(
                "/p/src/a.rs",
                2,
                2,
                cargoless_proto::Severity::Error,
                Some("E0308"),
                "second",
                Some("rustc"),
            )],
        ));
        let now = m.file_diagnostics("/p/src/a.rs");
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].code.as_deref(), Some("E0308"));
        // An empty publish CLEARS the file's diagnostics — the user fixed it.
        m.apply_event(&diag_rich("file:///p/src/a.rs", vec![]));
        assert!(m.file_diagnostics("/p/src/a.rs").is_empty());
    }

    #[test]
    fn forget_file_drops_diagnostics_too() {
        let mut m = model();
        m.apply_event(&diag_rich(
            "file:///p/src/a.rs",
            vec![mk_diag(
                "/p/src/a.rs",
                1,
                1,
                cargoless_proto::Severity::Error,
                Some("E0277"),
                "x",
                Some("rustc"),
            )],
        ));
        assert!(!m.all_diagnostics().is_empty());
        m.forget_file("/p/src/a.rs");
        assert!(
            m.all_diagnostics().is_empty(),
            "deleted file's diagnostics must be evicted from aggregate"
        );
    }

    #[test]
    fn aggregate_diagnostics_are_path_ordered() {
        // BTreeMap iteration is sorted by key — verify the aggregate flatten
        // preserves a deterministic file order so the CLI's `watch` mode
        // doesn't flicker between renders.
        let mut m = model();
        m.apply_event(&diag_rich(
            "file:///p/z.rs",
            vec![mk_diag(
                "/p/z.rs",
                1,
                1,
                cargoless_proto::Severity::Error,
                None,
                "z",
                Some("rustc"),
            )],
        ));
        m.apply_event(&diag_rich(
            "file:///p/a.rs",
            vec![mk_diag(
                "/p/a.rs",
                1,
                1,
                cargoless_proto::Severity::Error,
                None,
                "a",
                Some("rustc"),
            )],
        ));
        let all = m.all_diagnostics();
        assert_eq!(all.len(), 2);
        // /p/a.rs sorts before /p/z.rs.
        assert_eq!(all[0].file_path, std::path::PathBuf::from("/p/a.rs"));
        assert_eq!(all[1].file_path, std::path::PathBuf::from("/p/z.rs"));
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
