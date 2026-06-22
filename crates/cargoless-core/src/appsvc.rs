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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::Diagnostic;
use crate::appstate::{InstanceState, Pipeline};
use crate::transport::{
    PreviewControl, TransitionEvent, VerdictService, WorktreeStatus, WorktreeSummary,
};

/// The public routing facts for one runtime-registered preview, set by the
/// control loop when it binds the instance's proxy. Held in a side-map keyed
/// by instance name (NOT on `InstanceReport`, so the pure `appstate`/`appdrv`
/// cores stay free of proxy-port/host concerns). The Part-2 reconciler reads
/// these off `/app` to ensure one Service+Ingress per preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRoute {
    /// The loopback/host port the instance's `L4Proxy` actually bound (the
    /// reconciler's Service `targetPort`).
    pub proxy_port: u16,
    /// The public host this preview answers on, e.g. `feat-x.tryform.wtf`.
    /// `None` when no `--preview-domain` is configured (the feature is inert).
    pub public_host: Option<String>,
    /// Unix-seconds instant this preview self-expires (TTL). `0` ⇒ no expiry
    /// recorded (e.g. a static instance that somehow got a route). Surfaced on
    /// `/app` as `expires_at` so agents/operators can see remaining lifetime.
    pub expires_at: u64,
}

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

/// The shared read state. The driver publishes a fresh `Vec<InstanceReport>`
/// after every transition; the HTTP threads clone-and-read it. An
/// `arc-swap`-free design (no external dep): a `Mutex<Arc<Vec<…>>>` where the
/// lock is held only for the pointer swap/clone, never across any real work.
#[derive(Debug)]
pub struct AppServeState {
    reports: std::sync::Mutex<Arc<Vec<InstanceReport>>>,
    ready: AtomicBool,
    /// Self-serve control channel to the single-mutator control loop. The
    /// `POST/DELETE /instances` routes enqueue a [`PreviewControl`] here; the
    /// loop drains it. Wired after the channel exists (`set_control`); a daemon
    /// that never calls it stays read-only and `app_preview_control` ⇒ false.
    control: std::sync::Mutex<Option<Sender<PreviewControl>>>,
    /// Public routing facts per runtime preview, keyed by instance name. Set by
    /// the control loop at proxy-bind, cleared on remove. Merged into `/app`.
    routes: std::sync::Mutex<BTreeMap<String, PreviewRoute>>,
}

impl Default for AppServeState {
    fn default() -> Self {
        Self {
            reports: std::sync::Mutex::new(Arc::new(Vec::new())),
            ready: AtomicBool::new(false),
            control: std::sync::Mutex::new(None),
            routes: std::sync::Mutex::new(BTreeMap::new()),
        }
    }
}

impl AppServeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire the control channel (called once at daemon startup, after the
    /// loop's `Sender<PreviewControl>` exists). Until this is called,
    /// `app_preview_control` refuses (→ 404) — the self-serve routes are
    /// inert on a daemon that did not opt in.
    pub fn set_control(&self, tx: Sender<PreviewControl>) {
        *self.control.lock().expect("appsvc control lock") = Some(tx);
    }

    /// Enqueue a runtime preview request for the control loop. `false` ⇒ no
    /// channel wired (not a self-serve daemon) or the loop is gone. The work
    /// (proxy bind, port alloc, worktree) happens on the control thread; this
    /// only hands off intent.
    fn enqueue_control(&self, request: PreviewControl) -> bool {
        match &*self.control.lock().expect("appsvc control lock") {
            Some(tx) => tx.send(request).is_ok(),
            None => false,
        }
    }

    /// Record the public routing facts for a freshly-bound preview (control
    /// loop, at proxy-bind). Surfaced on `/app` for the Part-2 reconciler.
    pub fn set_preview_route(&self, name: &str, route: PreviewRoute) {
        self.routes
            .lock()
            .expect("appsvc routes lock")
            .insert(name.to_string(), route);
    }

    /// Drop a preview's routing facts (control loop, on remove). Idempotent.
    pub fn clear_preview_route(&self, name: &str) {
        self.routes.lock().expect("appsvc routes lock").remove(name);
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

    /// Render the `/app` JSON body.
    fn render_json(&self) -> String {
        let reports = self.snapshot();
        let routes = self.routes.lock().expect("appsvc routes lock").clone();
        let instances: Vec<serde_json::Value> = reports
            .iter()
            .map(|r| {
                // Merge the per-preview routing side-map: `proxy_port` +
                // `public_host` are present for runtime previews (what the
                // reconciler reads) and null for the static/zero-config
                // instances that have no dynamic route.
                let route = routes.get(&r.name);
                serde_json::json!({
                    "name": r.name,
                    "phase": r.phase,
                    "serving_sha": r.serving_sha,
                    "last_green": r.last_green,
                    "last_red_sha": r.last_red_sha,
                    "last_red_reason": r.last_red_reason,
                    "pending_sha": r.pending_sha,
                    "draining": r.draining,
                    "proxy_port": route.map(|x| x.proxy_port),
                    "public_host": route.and_then(|x| x.public_host.clone()),
                    // Self-serve preview TTL: the unix-seconds expiry instant
                    // (null/absent for static instances). Lets agents see how
                    // long their preview has left before auto-removal.
                    "expires_at": route.and_then(|x| (x.expires_at != 0).then_some(x.expires_at)),
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

    /// `/readyz` latch.
    fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Self-serve override: enqueue the runtime instance request for the
    /// control loop. `false` (→ 404) until `set_control` is wired.
    fn app_preview_control(&self, request: PreviewControl) -> bool {
        self.enqueue_control(request)
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
    fn preview_control_refuses_until_wired_then_enqueues() {
        let svc = AppServeState::new();
        // Not wired ⇒ refuse (→ the route 404s).
        assert!(!svc.app_preview_control(PreviewControl::Remove { name: "x".into() }));
        // Wire it; the request now lands on the channel.
        let (tx, rx) = channel::<PreviewControl>();
        svc.set_control(tx);
        assert!(svc.app_preview_control(PreviewControl::Add {
            name: "feat".into(),
            git_ref: "origin/feat".into(),
            env: vec![("K".into(), "v".into())],
            own_db: false,
            ttl_secs: Some(3600),
        }));
        match rx.recv().expect("enqueued") {
            PreviewControl::Add { name, git_ref, .. } => {
                assert_eq!(name, "feat");
                assert_eq!(git_ref, "origin/feat");
            }
            other => panic!("expected Add, got {other:?}"),
        }
    }

    #[test]
    fn render_json_merges_preview_route_fields() {
        let svc = AppServeState::new();
        svc.publish(vec![
            report("feat", Some("g1"), Some("g1")),
            report("dev", None, None),
        ]);
        // A runtime preview has a route; the static `dev` does not.
        svc.set_preview_route(
            "feat",
            PreviewRoute {
                proxy_port: 8201,
                public_host: Some("feat.tryform.wtf".into()),
                expires_at: 1_700_000_000,
            },
        );
        let v: serde_json::Value = serde_json::from_str(&svc.app_report().unwrap()).unwrap();
        let inst = v["instances"].as_array().unwrap();
        assert_eq!(inst[0]["name"], "feat");
        assert_eq!(inst[0]["proxy_port"], 8201);
        assert_eq!(inst[0]["public_host"], "feat.tryform.wtf");
        assert_eq!(inst[0]["expires_at"], 1_700_000_000_u64);
        // The route-less instance reports nulls (not absent keys).
        assert_eq!(inst[1]["name"], "dev");
        assert!(inst[1]["proxy_port"].is_null());
        assert!(inst[1]["public_host"].is_null());
        assert!(inst[1]["expires_at"].is_null());
        // Clearing drops the fields back to null.
        svc.clear_preview_route("feat");
        let v2: serde_json::Value = serde_json::from_str(&svc.app_report().unwrap()).unwrap();
        assert!(v2["instances"][0]["proxy_port"].is_null());
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
