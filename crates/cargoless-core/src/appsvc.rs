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
//! - `ready()` → the `/readyz` verdict: **can this daemon accept and serve
//!   work?** See [`readiness`] for the exact rule. The short version: at least
//!   one instance is actually serving, and no *live* instance is sitting on a
//!   green it has failed to serve. "Live" is a **liveness test** — a fresh
//!   observable state change — not merely "this slot exists and is not green".
//!   A slot nobody has touched in a long time ages out of the calculation
//!   instead of holding the whole pod un-ready forever.
//!
//! The driver ([`appdrv`] in the bin crate) owns the live
//! [`crate::appstate::AppState`]; it publishes an immutable snapshot here
//! after every transition via [`AppServeState::publish`]. The HTTP server
//! threads only ever read the snapshot — no lock is held across a build, and
//! the read plane can never be blocked by the build worker (the sync_lock
//! lesson, applied to app-serve).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Wall-clock unix-seconds instant this instance's *observable state last
    /// actually changed* — the liveness stamp `/readyz` ages against.
    ///
    /// NOT set by [`InstanceReport::from_state`] (which is pure and knows no
    /// clock): it is stamped by [`AppServeState::publish`], which compares each
    /// incoming row against the one it replaces and **carries the old stamp
    /// forward when nothing changed**. So a slot that is merely re-published
    /// (every event on *any* instance republishes *all* of them) does not
    /// refresh its own stamp — only a real transition does.
    pub last_change_unix: u64,
}

impl InstanceReport {
    /// Build a report row from the live state. Order of the `(name, state)`
    /// pairs is the instances-file (boot) order, preserved into the report.
    ///
    /// `last_change_unix` is left `0` on purpose — see the field docs; the
    /// stamp is [`AppServeState::publish`]'s job so this stays clock-free.
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
            last_change_unix: 0,
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

    /// Every reported field EXCEPT the liveness stamp. This is what
    /// [`AppServeState::publish`] diffs to decide "did this instance actually
    /// transition, or is this just another republish?".
    fn same_state(&self, other: &Self) -> bool {
        self.name == other.name
            && self.phase == other.phase
            && self.serving_sha == other.serving_sha
            && self.last_green == other.last_green
            && self.last_red_sha == other.last_red_sha
            && self.last_red_reason == other.last_red_reason
            && self.pending_sha == other.pending_sha
            && self.draining == other.draining
    }

    /// Age of the liveness stamp in seconds. Saturating, so a clock that
    /// steps backwards reads as age 0 (**live**) rather than a huge age —
    /// fail toward "this instance still counts", never toward silently
    /// excusing it.
    fn age_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.last_change_unix)
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
    /// Wall clock (unix seconds), injected so the liveness horizon is testable
    /// without sleeping. Used for two things and nothing else: stamping
    /// `last_change_unix` on a real transition, and ageing that stamp when
    /// `/readyz` is read.
    now: fn() -> u64,
    /// Self-serve control channel to the single-mutator control loop. The
    /// `POST/DELETE /instances` routes enqueue a [`PreviewControl`] here; the
    /// loop drains it. Wired after the channel exists (`set_control`); a daemon
    /// that never calls it stays read-only and `app_preview_control` ⇒ false.
    control: std::sync::Mutex<Option<Sender<PreviewControl>>>,
    /// Public routing facts per runtime preview, keyed by instance name. Set by
    /// the control loop at proxy-bind, cleared on remove. Merged into `/app`.
    routes: std::sync::Mutex<BTreeMap<String, PreviewRoute>>,
    /// Disk-pressure self-heal counters, surfaced on `/app` so a wedge that the
    /// daemon is relieving is VISIBLE instead of silent. `pressure_prunes` is
    /// the lifetime count of ENOSPC-triggered emergency prunes; `last_removed`
    /// is how many bundles the most recent one shed. Lock-free (daemon-level,
    /// set by the single control thread, read by HTTP threads).
    pressure_prunes: AtomicU64,
    last_pressure_prune_removed: AtomicU64,
}

/// Production clock for the liveness horizon: wall-clock unix seconds. A
/// pre-1970 clock (only reachable by gross misconfiguration) reads as `0`,
/// which makes every stamp maximally *old*… so it is deliberately paired with
/// the saturating [`InstanceReport::age_secs`]: stamps written with the same
/// broken clock still compare as age 0 = live.
fn wall_clock_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Default for AppServeState {
    fn default() -> Self {
        Self::with_clock(wall_clock_unix)
    }
}

impl AppServeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Same as [`AppServeState::new`] but with an injected clock — the seam the
    /// liveness-horizon tests drive (no sleeping, no flake).
    pub fn with_clock(now: fn() -> u64) -> Self {
        Self {
            reports: std::sync::Mutex::new(Arc::new(Vec::new())),
            now,
            control: std::sync::Mutex::new(None),
            routes: std::sync::Mutex::new(BTreeMap::new()),
            pressure_prunes: AtomicU64::new(0),
            last_pressure_prune_removed: AtomicU64::new(0),
        }
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
    /// transition).
    ///
    /// The one subtlety, and the reason the liveness stamp is trustworthy: the
    /// driver republishes **every** instance on **every** event, including
    /// events belonging to other instances. If publishing refreshed each row's
    /// stamp unconditionally, a busy `dev` would keep a dead `merge` looking
    /// eternally fresh and the liveness test would be a no-op — the exact
    /// "widened until it could never fire" failure this design exists to
    /// avoid. So each incoming row is diffed against the row it replaces
    /// ([`InstanceReport::same_state`]) and only a row that **actually
    /// changed** gets a new stamp; an unchanged row inherits its old one and
    /// keeps ageing. A brand-new instance (no prior row) is stamped now — it
    /// is live by definition, and starts with a full horizon to prove itself.
    ///
    /// `/readyz` is NOT latched here. It is computed at read time in
    /// [`AppServeState::ready`], because the verdict is a function of elapsed
    /// time: a degraded instance goes stale by the clock advancing, not by any
    /// event arriving — and a wedged daemon publishes nothing at all, which is
    /// precisely when a latch would freeze at its last value.
    pub fn publish(&self, reports: Vec<InstanceReport>) {
        let now = (self.now)();
        let mut slot = self.reports.lock().expect("appsvc reports lock");
        // Clone the Arc (a pointer bump — autoderef through the guard, same as
        // `snapshot`) rather than borrowing through it, so reading the old
        // snapshot and writing the new one are trivially disjoint.
        let prev: Arc<Vec<InstanceReport>> = slot.clone();
        let stamped: Vec<InstanceReport> = reports
            .into_iter()
            .map(|mut r| {
                let carried = prev
                    .iter()
                    .find(|p| p.name == r.name && p.same_state(&r))
                    .map(|p| p.last_change_unix);
                r.last_change_unix = carried.unwrap_or(now);
                r
            })
            .collect();
        *slot = Arc::new(stamped);
    }

    /// Record a disk-pressure relief prune (control loop, on an ENOSPC red):
    /// bump the lifetime count and store how many bundles it shed. Surfaced on
    /// `/app` under `disk` so the self-heal is observable. Lock-free.
    pub fn set_pressure_prune(&self, removed: usize) {
        self.pressure_prunes.fetch_add(1, Ordering::Relaxed);
        self.last_pressure_prune_removed
            .store(removed as u64, Ordering::Relaxed);
    }

    /// Current snapshot (cheap Arc clone).
    pub fn snapshot(&self) -> Arc<Vec<InstanceReport>> {
        self.reports.lock().expect("appsvc reports lock").clone()
    }

    /// Render the `/app` JSON body.
    fn render_json(&self) -> String {
        let now = (self.now)();
        let reports = self.snapshot();
        let routes = self.routes.lock().expect("appsvc routes lock").clone();
        // The instances whose liveness stamp has aged past the horizon AND
        // which are degraded — i.e. exactly those that would be holding
        // `/readyz` red if they were still live. This is the honest other half
        // of the `/readyz` answer: readiness ignores them, `/app` names them,
        // so a genuinely abandoned slot is visible rather than silently
        // excused. (A stale-but-healthy instance is unremarkable — a slot that
        // built once and has served happily ever since is simply quiet.)
        let stale_degraded: Vec<&str> = reports
            .iter()
            .filter(|r| is_degraded(r) && is_stale(r, now))
            .map(|r| r.name.as_str())
            .collect();
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
                    // Liveness surface: when this instance last actually
                    // transitioned, how long ago that was, and whether it has
                    // aged past the `/readyz` horizon. An operator reading a
                    // 200 from `/readyz` can see here exactly which slots were
                    // excused from that answer and how dead they are.
                    "last_change_unix": r.last_change_unix,
                    "idle_secs": r.age_secs(now),
                    "stale": is_stale(r, now),
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
            "ready": readiness(&reports, now),
            // Why `/readyz` said what it said. `stale_degraded` is the list of
            // instances that ARE failing to serve a green they own but were
            // aged out of the readiness calculation — the ones that would have
            // wedged the probe under the old all-instances rule. Empty on a
            // healthy daemon; non-empty means "ready, but these are abandoned".
            "readiness": {
                "stale_after_secs": stale_after_secs(),
                "stale_degraded": stale_degraded,
            },
            // Disk-pressure self-heal observability: how many ENOSPC-triggered
            // emergency prunes have run, and how many bundles the last one shed.
            // A climbing `pressure_prunes` is the visible signal that the PVC is
            // full and the daemon is self-relieving (vs. silently wedged).
            "disk": {
                "pressure_prunes": self.pressure_prunes.load(Ordering::Relaxed),
                "last_pressure_prune_removed":
                    self.last_pressure_prune_removed.load(Ordering::Relaxed),
            },
        })
        .to_string()
    }
}

/// How long an instance may sit in an unchanged state before it stops gating
/// `/readyz`. Read via [`stale_after_secs`] — never compare two constants in an
/// assertion (`clippy::assertions_on_constants` folds that to `assert!(true)`).
///
/// One hour. The reasoning is a window, from both ends:
/// - **Lower bound:** it must be far longer than any single build/probe cycle,
///   or an instance merely *recovering* would be mistaken for dead. A cold
///   Leptos build is minutes; an hour is an order of magnitude above that.
/// - **Upper bound:** while an instance is gating, the whole pod is NotReady,
///   which delists its *healthy* siblings too (one readinessProbe, all
///   endpoints). Every extra hour of that is pure collateral damage on slots
///   that are fine. An hour is long enough for a k8s probe (15s × 4 ⇒ ~1 min
///   to NotReady) to raise a loud, alertable, sustained red, and short enough
///   that the blast radius does not run for weeks.
const STALE_AFTER_SECS: u64 = 3600;

/// The staleness horizon in seconds — a fn, so tests can assert against a
/// resolved value instead of folding a const-vs-const comparison away.
pub fn stale_after_secs() -> u64 {
    STALE_AFTER_SECS
}

/// Has this instance's observable state stood still long enough to stop
/// counting? See [`STALE_AFTER_SECS`]. Note this only ever *removes* an
/// instance's ability to veto readiness; it can never make one ready.
fn is_stale(r: &InstanceReport, now: u64) -> bool {
    r.age_secs(now) >= STALE_AFTER_SECS
}

/// An instance that is failing to hold up its end **right now**: it has gone
/// green at least once, so a bundle it should be serving exists, and it is not
/// serving. (A never-green instance makes no claim — a permanently red branch
/// has nothing it is failing to keep up.)
fn is_degraded(r: &InstanceReport) -> bool {
    r.ever_green() && !r.currently_serving()
}

/// THE `/readyz` verdict: **can this daemon accept and serve work?**
///
/// ```text
///   ready  ⟺  some instance is currently serving
///         AND  no LIVE instance is degraded
/// ```
///
/// Two clauses, each guarding against one of the two useless probes:
///
/// 1. **`any_serving` — the floor that stops "always green".** It is a fact
///    about the *present* (the proxy slot points at a live child; a child that
///    exits clears it), so it can never go stale and can never be aged out. A
///    daemon with nothing serving is not ready, full stop, no matter how quiet
///    every slot has become.
///
/// 2. **`live && degraded` — the clause that stops "wedged red".** A degraded
///    instance vetoes readiness only while it is *live*: its observable state
///    changed within [`STALE_AFTER_SECS`]. This is a liveness test — a fresh
///    phase transition — not "does this slot exist and is it non-green", which
///    is precisely the widening that once made a tf-multiverse watchdog
///    permanently unable to fire once six 41-day-dead slots counted as busy.
///
/// Why the liveness test discriminates correctly: an instance on somebody's
/// critical path *refreshes its own stamp simply by being used*. Every push
/// moves its ref, which advances its phase, which is a state change. So a
/// broken `dev` that people are actively pushing to keeps re-arming its veto
/// and holds the pod NotReady for as long as it is broken. A `merge` slot
/// nobody has pushed to since 2026-08-02 emits no transitions at all, goes
/// quiet, and ages out. Dead slots age out **by name-independent rule** — no
/// allowlist, no special-casing, nothing to keep in sync with the instance
/// set.
///
/// The aged-out instance is not swept under the rug: it is still reported on
/// `/app` with its `phase`, `last_red_*`, `last_change_unix`, and an explicit
/// `stale` flag, and it is named in `readiness.stale_degraded`. The
/// split is deliberate — `/readyz` answers "is this daemon working", `/app`
/// answers "…and here is exactly what is not". Stale state must never be the
/// thing that reports a problem.
///
/// `now` is passed in (never read from a clock here) so this stays pure and
/// the whole matrix is unit-testable without sleeping.
fn readiness(reports: &[InstanceReport], now: u64) -> bool {
    let mut any_serving = false;
    for r in reports {
        if r.currently_serving() {
            any_serving = true;
        }
        if is_degraded(r) && !is_stale(r, now) {
            return false; // a live instance that should be up is down
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

    /// `/readyz`. Computed from the current snapshot **and the current time**
    /// (not a latch — see [`AppServeState::publish`]), so a degraded instance
    /// can age out of the verdict without any event needing to arrive.
    fn ready(&self) -> bool {
        readiness(&self.snapshot(), (self.now)())
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

    /// A shared test clock. `AppServeState::with_clock` takes a `fn()` (not a
    /// closure), so the clock has to live somewhere static; each test that
    /// drives it calls [`clock_reset`] first and holds [`CLOCK_LOCK`] for its
    /// duration, so `cargo test`'s parallel threads cannot interleave.
    static TEST_CLOCK: AtomicU64 = AtomicU64::new(0);
    static CLOCK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_now() -> u64 {
        TEST_CLOCK.load(Ordering::Relaxed)
    }

    /// Take the clock lock and set the starting instant. Returns the guard —
    /// hold it for the whole test. `Result::unwrap_or_else(PoisonError::into_inner)`
    /// keeps one panicking test from cascading into every other clock test.
    fn clock_reset(t: u64) -> std::sync::MutexGuard<'static, ()> {
        let guard = CLOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        TEST_CLOCK.store(t, Ordering::Relaxed);
        guard
    }

    fn clock_advance(d: u64) {
        TEST_CLOCK.fetch_add(d, Ordering::Relaxed);
    }

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
            last_change_unix: 0,
        }
    }

    /// A report row with an explicit liveness stamp, for the pure `readiness`
    /// tests that do not go through `publish`.
    fn stamped(
        name: &str,
        serving: Option<&str>,
        green: Option<&str>,
        last_change_unix: u64,
    ) -> InstanceReport {
        InstanceReport {
            last_change_unix,
            ..report(name, serving, green)
        }
    }

    #[test]
    fn readyz_is_false_until_the_first_instance_serves() {
        // Cold: nothing serving anywhere.
        assert!(!readiness(&[report("dev", None, None)], 0));
        // First instance serving ⇒ ready (even if a second is still cold and
        // never-green — it doesn't gate).
        assert!(readiness(
            &[
                report("dev", Some("g1"), Some("g1")),
                report("feature-x", None, None),
            ],
            0
        ));
    }

    #[test]
    fn readyz_drops_when_a_live_ever_green_instance_stops_serving() {
        // dev has been green but is not currently serving (its child died and
        // no replacement yet) ⇒ NOT ready, even though feature-x is up. The
        // stamp is fresh (age 0 at `now = 0`), so dev is live and vetoes.
        assert!(!readiness(
            &[
                stamped("dev", None, Some("g1"), 0),
                stamped("feature-x", Some("f1"), Some("f1"), 0),
            ],
            0
        ));
    }

    // ── the liveness horizon ─────────────────────────────────────────────
    //
    // THE BUG this fixes: on the tf-multiverse preview, `merge` last built
    // 2026-08-02 and `feature-x` 2026-06-24. Under the old rule ("every
    // ever-green instance must be serving") those two held `/readyz` at 503
    // forever, which delisted the pod — and with it the healthy `dev` and
    // `lane` — for as long as nobody bothered to delete them. A readiness
    // probe that can be wedged by state nobody is looking at reports nothing.

    /// A realistic wall-clock "now" for the pure-`readiness` tests. Far enough
    /// past the epoch that subtracting weeks stays in range — `u64` subtraction
    /// in a test binary panics on underflow rather than saturating.
    const NOW: u64 = 1_754_000_000;

    #[test]
    fn one_live_slot_and_one_long_dead_slot_is_ready() {
        // dev is serving and healthy. `merge` went green months ago, lost its
        // child, and has not transitioned since — it is off everyone's
        // critical path. The daemon CAN accept and serve work, so it is ready.
        let reports = [
            stamped("dev", Some("g1"), Some("g1"), NOW),
            stamped("merge", None, Some("m1"), NOW - 41 * 86_400),
        ];
        assert!(
            readiness(&reports, NOW),
            "a 41-day-dead slot must not hold the whole daemon un-ready"
        );
    }

    #[test]
    fn a_dead_slot_ages_out_but_a_live_one_still_vetoes() {
        // Same shape, same brokenness — the ONLY difference is how recently
        // each one moved. That is the whole discrimination: liveness, not
        // identity, and not "exists and is non-green".
        let horizon = stale_after_secs();
        let live = [
            stamped("dev", Some("g1"), Some("g1"), NOW),
            stamped("other", None, Some("m1"), NOW - (horizon - 1)),
        ];
        assert!(
            !readiness(&live, NOW),
            "just under the horizon ⇒ still gates"
        );

        let dead = [
            stamped("dev", Some("g1"), Some("g1"), NOW),
            stamped("other", None, Some("m1"), NOW - horizon),
        ];
        assert!(
            readiness(&dead, NOW),
            "at exactly the horizon ⇒ aged out (the comparison is `>=`)"
        );
    }

    #[test]
    fn a_broken_daemon_is_not_ready_however_stale_it_is() {
        // THE INVERSE of the fix, and the reason it is not just "always
        // green": ageing can only ever REMOVE a veto, never manufacture a
        // green. With nothing serving, no amount of staleness makes the daemon
        // ready — because `any_serving` is a fact about the present that has
        // no stamp to age.
        let ancient = 10 * stale_after_secs();
        assert!(
            !readiness(&[stamped("dev", None, Some("g1"), 0)], ancient),
            "a single ancient dead instance is NOT ready"
        );
        assert!(
            !readiness(
                &[
                    stamped("dev", None, Some("g1"), 0),
                    stamped("merge", None, Some("m1"), 0),
                    stamped("feature-x", None, None, 0),
                ],
                ancient
            ),
            "every instance down + ancient is still NOT ready"
        );
        // And the empty daemon: nothing to serve ⇒ nothing serving.
        assert!(!readiness(&[], ancient), "no instances ⇒ not ready");
    }

    #[test]
    fn a_live_instance_that_keeps_breaking_never_ages_out() {
        // The failure mode on the other side: a slot people ARE using but
        // which is broken must hold the probe red. It re-arms its own stamp
        // every time it transitions, so it can never drift past the horizon
        // while it is in use. Driven through `publish` because the carry-
        // forward rule is what makes this true.
        let _g = clock_reset(1_000_000);
        let svc = AppServeState::with_clock(test_now);
        svc.publish(vec![
            report("dev", Some("g1"), Some("g1")),
            report("lane", Some("l1"), Some("l1")),
        ]);
        assert!(svc.ready(), "both serving ⇒ ready");

        // dev loses its child and starts churning: build → idle → build …
        // Each cycle is a real transition, and each takes ALMOST a full
        // horizon — so only the re-arming keeps it gating.
        let almost = stale_after_secs() - 1;
        for i in 0..20 {
            clock_advance(almost);
            let phase = if i % 2 == 0 { "building" } else { "idle" };
            svc.publish(vec![
                InstanceReport {
                    phase: phase.into(),
                    ..report("dev", None, Some("g1"))
                },
                report("lane", Some("l1"), Some("l1")),
            ]);
            assert!(
                !svc.ready(),
                "an actively-transitioning broken instance keeps vetoing (cycle {i})"
            );
        }

        // Now it goes quiet. One horizon of silence and it ages out.
        clock_advance(stale_after_secs());
        assert!(
            svc.ready(),
            "once it stops transitioning it ages out and stops gating"
        );
    }

    #[test]
    fn a_dead_slot_does_not_get_its_stamp_refreshed_by_a_busy_sibling() {
        // The trap that would silently disarm the whole liveness test: the
        // driver republishes EVERY instance on EVERY event, so if `publish`
        // re-stamped unconditionally, a busy `dev` would keep a dead `merge`
        // eternally "fresh" and nothing would ever age out. (This is the
        // watchdog-widening failure in miniature.)
        let _g = clock_reset(1_000_000);
        let svc = AppServeState::with_clock(test_now);
        svc.publish(vec![
            report("dev", Some("g1"), Some("g1")),
            report("merge", Some("m1"), Some("m1")),
        ]);
        // merge's child dies once, then it is never touched again.
        clock_advance(10);
        svc.publish(vec![
            report("dev", Some("g1"), Some("g1")),
            report("merge", None, Some("m1")),
        ]);
        assert!(!svc.ready(), "merge just died and is live ⇒ not ready");
        let died_at = TEST_CLOCK.load(Ordering::Relaxed);

        // 41 days of dev churn — hundreds of republishes of the same dead
        // merge row.
        for _ in 0..200 {
            clock_advance(41 * 86_400 / 200);
            svc.publish(vec![
                InstanceReport {
                    phase: "building".into(),
                    ..report("dev", Some("g1"), Some("g1"))
                },
                report("merge", None, Some("m1")),
            ]);
            svc.publish(vec![
                report("dev", Some("g1"), Some("g1")),
                report("merge", None, Some("m1")),
            ]);
        }
        assert!(svc.ready(), "dead merge aged out ⇒ ready");
        let merge = svc
            .snapshot()
            .iter()
            .find(|r| r.name == "merge")
            .cloned()
            .expect("merge is still reported");
        assert_eq!(
            merge.last_change_unix, died_at,
            "merge's stamp is the instant IT last changed — not refreshed by dev"
        );
        assert!(
            merge.age_secs(TEST_CLOCK.load(Ordering::Relaxed)) > 40 * 86_400,
            "…so its reported idle time is the real one"
        );
    }

    #[test]
    fn publish_refreshes_the_stamp_on_any_real_field_change() {
        // Every reported field is part of "did this instance transition".
        // A field left out of `same_state` would be a transition the liveness
        // test cannot see, i.e. a slot that looks dead while it is working.
        let base = report("dev", Some("g1"), Some("g1"));
        let mutations: Vec<InstanceReport> = vec![
            InstanceReport {
                phase: "building".into(),
                ..base.clone()
            },
            InstanceReport {
                serving_sha: Some("g2".into()),
                ..base.clone()
            },
            InstanceReport {
                last_green: Some("g2".into()),
                ..base.clone()
            },
            InstanceReport {
                last_red_sha: Some("bad".into()),
                ..base.clone()
            },
            InstanceReport {
                last_red_reason: Some("boom".into()),
                ..base.clone()
            },
            InstanceReport {
                pending_sha: Some("p1".into()),
                ..base.clone()
            },
            InstanceReport {
                draining: 1,
                ..base.clone()
            },
        ];
        for changed in mutations {
            let _g = clock_reset(100);
            let svc = AppServeState::with_clock(test_now);
            svc.publish(vec![base.clone()]);
            assert_eq!(svc.snapshot()[0].last_change_unix, 100);
            clock_advance(900);
            svc.publish(vec![changed.clone()]);
            assert_eq!(
                svc.snapshot()[0].last_change_unix,
                1000,
                "a change in this row must refresh the stamp: {changed:?}"
            );
        }

        // …and a byte-identical republish must NOT.
        let _g = clock_reset(100);
        let svc = AppServeState::with_clock(test_now);
        svc.publish(vec![base.clone()]);
        clock_advance(900);
        svc.publish(vec![base.clone()]);
        assert_eq!(
            svc.snapshot()[0].last_change_unix,
            100,
            "an unchanged row carries its old stamp and keeps ageing"
        );
    }

    #[test]
    fn a_brand_new_instance_is_stamped_now_not_born_stale() {
        // If a new instance inherited the default stamp of 0 it would be born
        // with an enormous age — instantly excused from readiness, exactly
        // backwards. A hot-added preview gets a full horizon to prove itself.
        let _g = clock_reset(10 * stale_after_secs());
        let svc = AppServeState::with_clock(test_now);
        // Boot recovery: a durable last_green, nothing serving yet.
        svc.publish(vec![report("boot", None, Some("g1"))]);
        assert_eq!(
            svc.snapshot()[0].last_change_unix,
            TEST_CLOCK.load(Ordering::Relaxed),
            "a first-seen instance is stamped now(), not 0"
        );
        assert!(!svc.ready(), "…so it gates while it comes up");
        clock_advance(stale_after_secs() - 1);
        assert!(!svc.ready(), "still gating just inside the horizon");
    }

    #[test]
    fn an_instance_never_inherits_a_different_instances_stamp() {
        // The carry-forward is keyed on name. A removed-then-added instance,
        // or a rename, must not silently pick up a stranger's age.
        let _g = clock_reset(100);
        let svc = AppServeState::with_clock(test_now);
        svc.publish(vec![report("a", Some("s"), Some("s"))]);
        clock_advance(4900);
        svc.publish(vec![report("b", Some("s"), Some("s"))]);
        assert_eq!(
            svc.snapshot()[0].last_change_unix,
            5000,
            "different name ⇒ fresh stamp even though the row shape matches"
        );
    }

    #[test]
    fn a_backwards_clock_fails_toward_not_ready() {
        // Fail toward reporting a problem when uncertain. `age_secs`
        // saturates, so a clock that jumps backwards reads every stamp as age
        // 0 = live: degraded instances keep vetoing rather than all being
        // excused at once.
        let _g = clock_reset(1_000_000);
        let svc = AppServeState::with_clock(test_now);
        svc.publish(vec![
            report("dev", Some("g1"), Some("g1")),
            report("dead", None, Some("d1")),
        ]);
        assert!(!svc.ready());
        TEST_CLOCK.store(5, Ordering::Relaxed); // clock steps back years
        assert!(
            !svc.ready(),
            "a backwards clock must not mass-excuse every instance"
        );
    }

    #[test]
    fn readyz_is_not_latched_it_recomputes_with_the_clock() {
        // The wedged-daemon case: a daemon that has stopped publishing (its
        // control loop is stuck) must still be able to change its answer as
        // time passes — a latch written at the last publish would freeze
        // forever at whatever it happened to be. Here: ONE publish, then only
        // the clock moves.
        let _g = clock_reset(1_000);
        let svc = AppServeState::with_clock(test_now);
        svc.publish(vec![
            report("dev", Some("g1"), Some("g1")),
            report("gone", None, Some("m1")),
        ]);
        assert!(!svc.ready(), "at publish time the dead slot is live ⇒ 503");
        clock_advance(stale_after_secs());
        assert!(
            svc.ready(),
            "no further publish, only the clock moved ⇒ the verdict updates"
        );
    }

    #[test]
    fn the_real_preview_shape_reports_ready_with_two_abandoned_slots() {
        // The production state that motivated this: dev + lane on the critical
        // path and healthy; merge and feature-x months dead.
        let _g = clock_reset(1_754_000_000);
        let svc = AppServeState::with_clock(test_now);
        let all_up = || {
            vec![
                report("dev", Some("d78"), Some("d78")),
                report("feature-x", Some("1d9"), Some("1d9")),
                report("lane", Some("2f2"), Some("2f2")),
                report("merge", Some("d22"), Some("d22")),
            ]
        };
        svc.publish(all_up());
        assert!(svc.ready(), "all four serving ⇒ ready");

        // merge + feature-x lose their children and are never touched again.
        let two_dead = || {
            vec![
                report("dev", Some("d78"), Some("d78")),
                report("feature-x", None, Some("1d9")),
                report("lane", Some("2f2"), Some("2f2")),
                report("merge", None, Some("d22")),
            ]
        };
        clock_advance(60);
        svc.publish(two_dead());
        assert!(
            !svc.ready(),
            "the moment they die the daemon says so — a fresh fault is a fault"
        );

        // 40 days of dev/lane churn later they have aged out.
        clock_advance(40 * 86_400);
        svc.publish(two_dead());
        assert!(svc.ready(), "abandoned slots aged out ⇒ ready");

        // They are NOT hidden: `/app` names them.
        let v: serde_json::Value = serde_json::from_str(&svc.app_report().unwrap()).unwrap();
        assert_eq!(v["ready"], true);
        let stale: Vec<&str> = v["readiness"]["stale_degraded"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert_eq!(
            stale,
            vec!["feature-x", "merge"],
            "the aged-out slots are reported, not swept away"
        );
        assert_eq!(v["readiness"]["stale_after_secs"], stale_after_secs());

        // And a break on a LIVE lane still turns the probe red — dead
        // siblings never mask a real fault.
        clock_advance(60);
        svc.publish(vec![
            report("dev", Some("d78"), Some("d78")),
            report("feature-x", None, Some("1d9")),
            report("lane", None, Some("2f2")),
            report("merge", None, Some("d22")),
        ]);
        assert!(
            !svc.ready(),
            "lane breaking ⇒ NOT ready, even with two aged-out siblings"
        );
    }

    #[test]
    fn app_report_exposes_per_instance_liveness() {
        let _g = clock_reset(50_000);
        let svc = AppServeState::with_clock(test_now);
        svc.publish(vec![
            report("dev", Some("g1"), Some("g1")),
            report("merge", None, Some("m1")),
        ]);
        clock_advance(stale_after_secs() + 25);
        // Republish unchanged: both keep their stamps and age.
        svc.publish(vec![
            report("dev", Some("g1"), Some("g1")),
            report("merge", None, Some("m1")),
        ]);
        let v: serde_json::Value = serde_json::from_str(&svc.app_report().unwrap()).unwrap();
        let inst = v["instances"].as_array().unwrap();
        assert_eq!(inst[0]["name"], "dev");
        assert_eq!(inst[0]["last_change_unix"], 50_000);
        assert_eq!(inst[0]["idle_secs"], stale_after_secs() + 25);
        // A quiet-but-HEALTHY instance is flagged stale too — `stale` is a
        // pure liveness fact. Only `stale_degraded` combines it with a fault.
        assert_eq!(inst[0]["stale"], true);
        assert_eq!(inst[1]["name"], "merge");
        assert_eq!(inst[1]["stale"], true);
        let stale: Vec<&str> = v["readiness"]["stale_degraded"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert_eq!(
            stale,
            vec!["merge"],
            "only the DEGRADED stale instance is called out"
        );
    }

    #[test]
    fn never_green_instance_does_not_hold_the_pod_unready() {
        // feature-x has never gone green (permanently red branch). dev is up.
        // The pod is ready — feature-x being down is expected, not a fault.
        // Both stamps are fresh (`now = 0`), so this is decided by the
        // ever-green rule alone, not by ageing.
        assert!(readiness(
            &[
                stamped("dev", Some("g1"), Some("g1"), 0),
                stamped("feature-x", None, None, 0),
            ],
            0
        ));
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
    fn app_report_includes_the_disk_pressure_block() {
        let svc = AppServeState::new();
        svc.publish(vec![report("dev", Some("g1"), Some("g1"))]);

        // Fresh daemon: zero prunes.
        let v: serde_json::Value = serde_json::from_str(&svc.app_report().unwrap()).unwrap();
        assert_eq!(v["disk"]["pressure_prunes"], 0);
        assert_eq!(v["disk"]["last_pressure_prune_removed"], 0);

        // Two relief prunes; the block reflects the count + the last shed total.
        svc.set_pressure_prune(3);
        svc.set_pressure_prune(1);
        let v: serde_json::Value = serde_json::from_str(&svc.app_report().unwrap()).unwrap();
        assert_eq!(v["disk"]["pressure_prunes"], 2, "lifetime count");
        assert_eq!(
            v["disk"]["last_pressure_prune_removed"], 1,
            "most recent prune shed 1 bundle"
        );
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
    fn ready_reflects_publish_on_the_real_clock() {
        // The unchanged base behaviour, on the production wall clock: every
        // stamp written here is seconds old, so nothing is anywhere near the
        // horizon and readiness is decided purely by serving/ever-green.
        let svc = AppServeState::new();
        assert!(!svc.ready(), "fresh service is not ready");
        svc.publish(vec![report("dev", None, None)]);
        assert!(!svc.ready(), "nothing serving ⇒ not ready");
        svc.publish(vec![report("dev", Some("g1"), Some("g1"))]);
        assert!(svc.ready(), "serving ⇒ ready");
        // A freshly-broken instance is red immediately — no grace period on
        // the fault itself, only on how long it may keep vetoing.
        svc.publish(vec![report("dev", None, Some("g1"))]);
        assert!(!svc.ready(), "serving child lost, still live ⇒ not ready");
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
        assert_eq!(
            r.last_change_unix, 0,
            "from_state stays clock-free; `publish` owns the stamp"
        );
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
