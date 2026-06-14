//! `appsvc` — the read-plane [`VerdictService`] for the app-serve daemon.
//!
//! The app-serve daemon reuses the **same** hand-rolled HTTP server as the
//! gate ([`crate::transport::http::HttpServer`]): the control-plane bind
//! (`--bind`) exposes `/healthz`, `/readyz`, and the new `/app` report. This
//! type is the `VerdictService` behind that bind. It does **not** answer the
//! verdict routes (`/status`, `/verdict`, `/worktrees`) — an app-serve daemon
//! has no check worktrees — so those return their honest empty/None, exactly
//! as the trait defaults intend.
//!
//! What it *does* own:
//! - `app_report()` → the `/app` JSON: every instance's phase, serving sha,
//!   last red, drain depth. This is the route the gate daemon 404s (its
//!   `app_report` is the `None` default); ours returns `Some(json)`.
//! - `ready()` → the `/readyz` latch: **true once every configured instance
//!   that has ever gone green is currently serving**. Cold start is ready as
//!   soon as the first instance serves (k8s marks the pod ready and routes
//!   traffic only when there is something to route).
//!
//! The driver ([`appdrv`] in the bin crate) owns the live
//! [`crate::appstate::AppState`]; it publishes an immutable snapshot here
//! after every transition via [`AppServeState::publish`]. The HTTP server
//! threads only ever read the snapshot — no lock is held across a build, and
//! the read plane can never be blocked by the build worker (the sync_lock
//! lesson, applied to app-serve).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};

use crate::Diagnostic;
use crate::appstate::{InstanceState, Pipeline};
use crate::transport::{TransitionEvent, VerdictService, WorktreeStatus, WorktreeSummary};

/// An immutable, cheap-to-clone snapshot of one instance for the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceReport {
    pub name: String,
    pub phase: String,
    pub serving_sha: Option<String>,
    pub last_green: Option<String>,
    pub last_red_sha: Option<String>,
    pub last_red_reason: Option<String>,
    pub pending_sha: Option<String>,
    pub draining: usize,
}

impl InstanceReport {
    /// Build a report row from the live state. Order of the `(name, state)`
    /// pairs is the instances-file (boot) order, preserved into the report.
    pub fn from_state(name: &str, inst: &InstanceState) -> Self {
        Self {
            name: name.to_string(),
            phase: phase_label(inst).to_string(),
            serving_sha: inst.serving.as_ref().map(|s| s.sha.clone()),
            last_green: inst.last_green.clone(),
            last_red_sha: inst.last_red.as_ref().map(|(s, _)| s.clone()),
            last_red_reason: inst.last_red.as_ref().map(|(_, r)| r.clone()),
            pending_sha: inst.pending.clone(),
            draining: inst.draining.len(),
        }
    }

    /// Whether this instance "has a green to keep up": it has gone green at
    /// least once. A never-green instance does not gate `/readyz` — a single
    /// permanently-red branch must not hold the whole pod un-ready.
    fn ever_green(&self) -> bool {
        self.last_green.is_some()
    }

    fn currently_serving(&self) -> bool {
        self.serving_sha.is_some()
    }
}

/// The same one-word phase label the state file uses, kept here so the report
/// and the on-disk mirror agree.
fn phase_label(inst: &InstanceState) -> &'static str {
    match (&inst.pipeline, inst.serving.is_some()) {
        (Pipeline::Building { .. }, _) => "building",
        (Pipeline::Queued { .. }, _) => "queued",
        (Pipeline::Probing { .. }, true) => "probing+serving",
        (Pipeline::Probing { .. }, false) => "probing",
        (Pipeline::Idle, true) => "serving",
        (Pipeline::Idle, false) => "idle",
    }
}

/// A self-serve preview request, enqueued by the `POST /instances` /
/// `DELETE /instances/<name>` routes for the control loop to perform. The
/// HTTP thread only expresses *intent* — the control loop owns all the
/// effectful setup/teardown (proxy bind, port alloc, git worktree), so it is
/// the only mutator of the live instance set (the single-mutator discipline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewRequest {
    /// Add a preview for `git_ref` named `name`, with `env` overlay and an
    /// optional own database (else the shared dev DB).
    Add {
        name: String,
        git_ref: String,
        env: Vec<(String, String)>,
        own_db: bool,
    },
    /// Tear down the named preview instance and reclaim its resources.
    Remove { name: String },
}

/// The shared read state. The driver publishes a fresh `Vec<InstanceReport>`
/// after every transition; the HTTP threads clone-and-read it. An
/// `arc-swap`-free design (no external dep): a `Mutex<Arc<Vec<…>>>` where the
/// lock is held only for the pointer swap/clone, never across any real work.
///
/// It also carries the **control channel** to the loop: the `POST/DELETE
/// /instances` routes enqueue a [`PreviewRequest`] here, and the control loop
/// drains it. The sender is wired in after the channel exists (`set_control`);
/// before that (or on a non-self-serve daemon) requests are refused.
#[derive(Debug)]
pub struct AppServeState {
    reports: std::sync::Mutex<Arc<Vec<InstanceReport>>>,
    ready: AtomicBool,
    control: std::sync::Mutex<Option<std::sync::mpsc::Sender<PreviewRequest>>>,
}

impl Default for AppServeState {
    fn default() -> Self {
        Self {
            reports: std::sync::Mutex::new(Arc::new(Vec::new())),
            ready: AtomicBool::new(false),
            control: std::sync::Mutex::new(None),
        }
    }
}

impl AppServeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a fresh snapshot (the driver calls this after every
    /// transition). Recomputes the `/readyz` latch from the snapshot.
    pub fn publish(&self, reports: Vec<InstanceReport>) {
        let ready = readiness(&reports);
        *self.reports.lock().expect("appsvc reports lock") = Arc::new(reports);
        // Readiness only ever latches *up* within a boot: once the pod has
        // served traffic it stays ready even if a later build goes red (the
        // old green keeps serving — never-un-ready on red, the whole point of
        // never-serve-red). It can only drop if an instance loses its serving
        // child with no replacement, which `readiness` reflects.
        self.ready.store(ready, Ordering::Release);
    }

    /// Current snapshot (cheap Arc clone).
    pub fn snapshot(&self) -> Arc<Vec<InstanceReport>> {
        self.reports.lock().expect("appsvc reports lock").clone()
    }

    /// Wire the control channel to the loop (called once at daemon startup,
    /// after the loop's `Sender<PreviewRequest>` exists). A daemon that never
    /// calls this refuses self-serve requests with `false` from
    /// [`request_preview`].
    pub fn set_control(&self, tx: std::sync::mpsc::Sender<PreviewRequest>) {
        *self.control.lock().expect("appsvc control lock") = Some(tx);
    }

    /// Enqueue a self-serve preview request for the control loop. Returns
    /// `false` if no control channel is wired (not a self-serve daemon) or the
    /// loop has shut down (send failed) — the route then answers 503/409. The
    /// request is performed asynchronously on the control thread; this only
    /// expresses intent (the loop owns proxy/port/worktree setup).
    pub fn request_preview(&self, req: PreviewRequest) -> bool {
        match &*self.control.lock().expect("appsvc control lock") {
            Some(tx) => tx.send(req).is_ok(),
            None => false,
        }
    }

    /// Render the `/app` JSON body.
    fn render_json(&self) -> String {
        let reports = self.snapshot();
        let instances: Vec<serde_json::Value> = reports
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "phase": r.phase,
                    "serving_sha": r.serving_sha,
                    "last_green": r.last_green,
                    "last_red_sha": r.last_red_sha,
                    "last_red_reason": r.last_red_reason,
                    "pending_sha": r.pending_sha,
                    "draining": r.draining,
                })
            })
            .collect();
        serde_json::json!({
            "instances": instances,
            "ready": self.ready.load(Ordering::Acquire),
        })
        .to_string()
    }
}

/// `/readyz` is true when **every ever-green instance is currently serving**,
/// and at least one instance is serving (cold start: not ready until the
/// first green serves). A never-green instance is ignored — a permanently
/// broken branch cannot hold the pod un-ready forever.
fn readiness(reports: &[InstanceReport]) -> bool {
    let mut any_serving = false;
    for r in reports {
        if r.currently_serving() {
            any_serving = true;
        }
        if r.ever_green() && !r.currently_serving() {
            return false; // an instance that should be up is down
        }
    }
    any_serving
}

impl VerdictService for AppServeState {
    // No check worktrees on an app-serve daemon: the verdict routes answer
    // honestly empty (the same shape the trait documents for an unknown wt).
    fn get_status(&self, _worktree: &str) -> Option<WorktreeStatus> {
        None
    }
    fn get_verdict(&self, _worktree: &str) -> Option<String> {
        None
    }
    fn get_diagnostics(&self, _worktree: &str) -> Vec<Diagnostic> {
        Vec::new()
    }
    fn list_worktrees(&self) -> Vec<WorktreeSummary> {
        Vec::new()
    }
    fn subscribe(&self) -> Receiver<TransitionEvent> {
        // No check-transition stream on an app-serve daemon. Hand back a
        // live-but-empty receiver (its sender drops immediately), so a
        // `GET /events` SSE client connects and simply receives nothing —
        // honest, and never a panic.
        channel().1
    }

    /// THE override: the gate returns `None` here (→ 404); we return the JSON.
    fn app_report(&self) -> Option<String> {
        Some(self.render_json())
    }

    /// Self-serve: translate the transport-level [`PreviewControl`] into a
    /// [`PreviewRequest`] and enqueue it for the control loop.
    fn app_preview_control(&self, request: crate::transport::PreviewControl) -> bool {
        let req = match request {
            crate::transport::PreviewControl::Add {
                name,
                git_ref,
                env,
                own_db,
            } => PreviewRequest::Add {
                name,
                git_ref,
                env,
                own_db,
            },
            crate::transport::PreviewControl::Remove { name } => PreviewRequest::Remove { name },
        };
        self.request_preview(req)
    }

    /// `/readyz` latch.
    fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appstate::ServingChild;

    fn report(name: &str, serving: Option<&str>, green: Option<&str>) -> InstanceReport {
        InstanceReport {
            name: name.into(),
            phase: if serving.is_some() { "serving" } else { "idle" }.into(),
            serving_sha: serving.map(String::from),
            last_green: green.map(String::from),
            last_red_sha: None,
            last_red_reason: None,
            pending_sha: None,
            draining: 0,
        }
    }

    #[test]
    fn readyz_is_false_until_the_first_instance_serves() {
        // Cold: nothing serving anywhere.
        assert!(!readiness(&[report("dev", None, None)]));
        // First instance serving ⇒ ready (even if a second is still cold and
        // never-green — it doesn't gate).
        assert!(readiness(&[
            report("dev", Some("g1"), Some("g1")),
            report("feature-x", None, None),
        ]));
    }

    #[test]
    fn readyz_drops_when_an_ever_green_instance_stops_serving() {
        // dev has been green but is not currently serving (its child died and
        // no replacement yet) ⇒ NOT ready, even though feature-x is up.
        assert!(!readiness(&[
            report("dev", None, Some("g1")),
            report("feature-x", Some("f1"), Some("f1")),
        ]));
    }

    #[test]
    fn never_green_instance_does_not_hold_the_pod_unready() {
        // feature-x has never gone green (permanently red branch). dev is up.
        // The pod is ready — feature-x being down is expected, not a fault.
        assert!(readiness(&[
            report("dev", Some("g1"), Some("g1")),
            report("feature-x", None, None),
        ]));
    }

    #[test]
    fn app_report_is_some_json_with_every_instance() {
        let svc = AppServeState::new();
        svc.publish(vec![
            report("dev", Some("g1"), Some("g1")),
            InstanceReport {
                last_red_sha: Some("bad".into()),
                last_red_reason: Some("step `x` exited 1".into()),
                ..report("feature-x", None, None)
            },
        ]);
        let json = svc.app_report().expect("app-serve service reports Some");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let instances = v["instances"].as_array().unwrap();
        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0]["name"], "dev");
        assert_eq!(instances[0]["serving_sha"], "g1");
        assert_eq!(instances[1]["name"], "feature-x");
        assert_eq!(instances[1]["last_red_sha"], "bad");
        assert_eq!(v["ready"], true, "dev serving ⇒ ready");
    }

    #[test]
    fn ready_latch_reflects_publish() {
        let svc = AppServeState::new();
        assert!(!svc.ready(), "fresh service is not ready");
        svc.publish(vec![report("dev", None, None)]);
        assert!(!svc.ready(), "nothing serving ⇒ not ready");
        svc.publish(vec![report("dev", Some("g1"), Some("g1"))]);
        assert!(svc.ready(), "serving ⇒ ready");
    }

    #[test]
    fn preview_request_refused_until_control_wired_then_enqueues() {
        let svc = AppServeState::new();
        let req = PreviewRequest::Add {
            name: "feat".into(),
            git_ref: "origin/feat".into(),
            env: vec![],
            own_db: false,
        };
        // No control channel ⇒ refused (a non-self-serve daemon answers 404).
        assert!(
            !svc.request_preview(req.clone()),
            "refused before set_control"
        );

        // Wire the loop's receiver; now requests enqueue.
        let (tx, rx) = std::sync::mpsc::channel();
        svc.set_control(tx);
        assert!(
            svc.request_preview(req.clone()),
            "enqueued after set_control"
        );
        assert_eq!(
            rx.recv().unwrap(),
            req,
            "the control loop receives it verbatim"
        );

        // The VerdictService seam maps PreviewControl → PreviewRequest.
        use crate::transport::{PreviewControl, VerdictService};
        assert!(svc.app_preview_control(PreviewControl::Remove {
            name: "feat".into()
        }));
        assert_eq!(
            rx.recv().unwrap(),
            PreviewRequest::Remove {
                name: "feat".into()
            }
        );
    }

    #[test]
    fn from_state_maps_every_field() {
        let inst = InstanceState {
            serving: Some(ServingChild {
                sha: "s1".into(),
                generation: 2,
            }),
            pipeline: Pipeline::Building {
                sha: "b1".into(),
                generation: 3,
            },
            pending: Some("p1".into()),
            last_green: Some("g1".into()),
            last_red: Some(("r1".into(), "boom".into())),
            draining: vec![1, 2],
            ..Default::default()
        };
        let r = InstanceReport::from_state("dev", &inst);
        assert_eq!(r.name, "dev");
        assert_eq!(r.phase, "building"); // building dominates the label
        assert_eq!(r.serving_sha.as_deref(), Some("s1"));
        assert_eq!(r.pending_sha.as_deref(), Some("p1"));
        assert_eq!(r.last_green.as_deref(), Some("g1"));
        assert_eq!(r.last_red_sha.as_deref(), Some("r1"));
        assert_eq!(r.draining, 2);
    }

    #[test]
    fn verdict_routes_are_honestly_empty() {
        let svc = AppServeState::new();
        assert_eq!(svc.get_status("anything"), None);
        assert_eq!(svc.get_verdict("anything"), None);
        assert!(svc.get_diagnostics("anything").is_empty());
        assert!(svc.list_worktrees().is_empty());
    }
}
