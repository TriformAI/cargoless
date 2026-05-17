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
use std::sync::mpsc::{Receiver, Sender, channel};

use tf_proto::{BuildIdentity, FileState, StateEvent, TreeState};

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
        if !self.files.is_empty() && self.files.values().all(|s| *s == FileState::Green) {
            TreeState::Green
        } else {
            TreeState::Red
        }
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
}
