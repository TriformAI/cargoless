//! In-process adapter (`D-FLEET-SHARED-DAEMON` §10.2, single-binary
//! mode). `cargoless watch` runs the daemon and the CLI in **one
//! process**: no socket, no TCP, no serialisation — the
//! [`TransportClient`] calls forward directly to the in-memory
//! [`VerdictService`]. Zero IPC overhead; the reference adapter the
//! Unix/HTTP adapters are validated against (same logical results, just
//! a wire in between).

use std::sync::Arc;
use std::sync::mpsc::Receiver;

use cargoless_proto::Diagnostic;

use super::{
    PushOverlayAck, TransitionEvent, TransportClient, TransportError, VerdictService,
    WorktreeStatus, WorktreeSummary,
};

/// Wraps any [`VerdictService`] and presents it as a [`TransportClient`].
/// Cloneable (it is just an `Arc`) so the single-binary CLI and the
/// in-process daemon loop can share one service cheaply.
#[derive(Clone)]
pub struct InProcClient {
    service: Arc<dyn VerdictService>,
}

impl InProcClient {
    pub fn new(service: Arc<dyn VerdictService>) -> Self {
        Self { service }
    }
}

impl TransportClient for InProcClient {
    fn get_status(&self, worktree: &str) -> Result<Option<WorktreeStatus>, TransportError> {
        Ok(self.service.get_status(worktree))
    }

    fn get_verdict(&self, worktree: &str) -> Result<Option<String>, TransportError> {
        Ok(self.service.get_verdict(worktree))
    }

    fn get_diagnostics(&self, worktree: &str) -> Result<Vec<Diagnostic>, TransportError> {
        Ok(self.service.get_diagnostics(worktree))
    }

    fn list_worktrees(&self) -> Result<Vec<WorktreeSummary>, TransportError> {
        Ok(self.service.list_worktrees())
    }

    fn subscribe(&self) -> Result<Receiver<TransitionEvent>, TransportError> {
        Ok(self.service.subscribe())
    }

    fn push_overlay(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
    ) -> Result<PushOverlayAck, TransportError> {
        // Single-binary mode: forward straight to the in-memory service —
        // infallible, zero IPC (the in-proc adapter's whole point).
        Ok(self.service.push_overlay(worktree, base_ref, files))
    }
}

#[cfg(test)]
pub(crate) mod testmock {
    //! A deterministic in-memory [`VerdictService`] shared by the
    //! adapter test suites (in-proc / Unix / HTTP) so all three are
    //! validated against the *same* logical oracle — the whole point of
    //! the abstraction is that the transport must not change the answer.

    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender, channel};

    use cargoless_proto::{Diagnostic, Severity};

    use super::super::{
        CrateVerdict, TransitionEvent, VerdictService, WorktreeStatus, WorktreeSummary,
    };

    #[derive(Default)]
    pub struct MockService {
        pub subs: Mutex<Vec<Sender<TransitionEvent>>>,
    }

    impl MockService {
        pub fn new() -> Self {
            Self::default()
        }
        /// Push a transition to every subscriber (drives the SSE/stream
        /// tests deterministically).
        pub fn emit(&self, ev: TransitionEvent) {
            self.subs
                .lock()
                .unwrap()
                .retain(|s| s.send(ev.clone()).is_ok());
        }
    }

    pub fn red_diag() -> Diagnostic {
        Diagnostic {
            file_path: PathBuf::from("physics/src/orbit.rs"),
            line: 142,
            col: 18,
            severity: Severity::Error,
            code: Some("E0308".into()),
            message: "expected `f64`, found `f32`".into(),
            source: Some("rustc".into()),
        }
    }

    impl VerdictService for MockService {
        fn get_status(&self, worktree: &str) -> Option<WorktreeStatus> {
            match worktree {
                "green-wt" => Some(WorktreeStatus {
                    worktree: worktree.into(),
                    verdict: "green".into(),
                    crates: vec![CrateVerdict {
                        name: "isolation".into(),
                        verdict: "green".into(),
                    }],
                    red_diagnostics: 0,
                    heartbeat_age_secs: 1,
                    published_at: 1000,
                }),
                "red-wt" => Some(WorktreeStatus {
                    worktree: worktree.into(),
                    verdict: "red".into(),
                    // Honesty case: unattributable error ⇒ empty crates;
                    // verdict stands alone (the #9/#11 invariant on the
                    // wire).
                    crates: vec![],
                    red_diagnostics: 1,
                    heartbeat_age_secs: 0,
                    published_at: 1001,
                }),
                _ => None,
            }
        }
        fn get_verdict(&self, worktree: &str) -> Option<String> {
            self.get_status(worktree).map(|s| s.verdict)
        }
        fn get_diagnostics(&self, worktree: &str) -> Vec<Diagnostic> {
            if worktree == "red-wt" {
                vec![red_diag()]
            } else {
                Vec::new()
            }
        }
        fn list_worktrees(&self) -> Vec<WorktreeSummary> {
            vec![
                WorktreeSummary {
                    worktree: "green-wt".into(),
                    verdict: "green".into(),
                    red_diagnostics: 0,
                },
                WorktreeSummary {
                    worktree: "red-wt".into(),
                    verdict: "red".into(),
                    red_diagnostics: 1,
                },
            ]
        }
        fn subscribe(&self) -> Receiver<TransitionEvent> {
            let (tx, rx) = channel();
            self.subs.lock().unwrap().push(tx);
            rx
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testmock::{MockService, red_diag};
    use super::*;
    use crate::transport::TransitionEvent;

    fn client() -> InProcClient {
        InProcClient::new(Arc::new(MockService::new()))
    }

    #[test]
    fn forwards_every_call_infallibly() {
        let c = client();
        assert_eq!(c.get_verdict("green-wt").unwrap(), Some("green".into()));
        assert_eq!(c.get_verdict("nope").unwrap(), None);
        assert_eq!(c.get_status("red-wt").unwrap().unwrap().red_diagnostics, 1);
        assert!(
            c.get_status("red-wt").unwrap().unwrap().crates.is_empty(),
            "honesty case preserved through the in-proc adapter"
        );
        assert_eq!(c.get_diagnostics("red-wt").unwrap(), vec![red_diag()]);
        assert!(c.get_diagnostics("green-wt").unwrap().is_empty());
        assert_eq!(c.list_worktrees().unwrap().len(), 2);
    }

    #[test]
    fn subscribe_delivers_emitted_transitions() {
        let svc = Arc::new(MockService::new());
        let c = InProcClient::new(svc.clone());
        let rx = c.subscribe().unwrap();
        let ev = TransitionEvent {
            worktree: "red-wt".into(),
            verdict: "red".into(),
            red_diagnostics: 1,
            published_at: 7,
        };
        svc.emit(ev.clone());
        assert_eq!(rx.recv().unwrap(), ev);
    }
}
