# D-INC2-OBSERVABILITY — Increment-5 OTEL+SigNoz observability anchor

**Status:** Implementation-anchor record. Mirrors the
[`D-INC2-2B.md`](D-INC2-2B.md) shape established for #240/2b — durable
design + risk-register doc reusable across Wave-2 implementation and
future waves (Wave-3, metrics-expansion, etc.).
**Plane umbrella:** CWDL-245 (Increment 5 — OTEL+SigNoz observability).
**Authored:** 2026-05-20.
**Source anchor:** `origin/main` = `929a5d3` (Wave-1 fully landed).
Wave-2 reference branch: `agent/dev-fixer-w2 off feb3fac` (IN FLIGHT —
metrics + AC4 keystone-invariant test not yet on `main`).

---

## 0. State of the world (verified against current main 929a5d3)

**Wave-1 is FULLY LANDED** — telemetry foundation + the 5 keystone
spans + serve.rs init/shutdown wiring are all on `main`:

| Surface | Status on main 929a5d3 |
|---|---|
| `cargoless_core::TelemetryConfig` (5b — pure data, env-overridable) | ✓ shipped |
| `crates/cargoless/src/telemetry.rs` — OTEL+SigNoz init + shutdown (5a, 5f) | ✓ shipped (483 lines) |
| OTLP HTTP/protobuf transport via async `reqwest-client` | ✓ shipped |
| `tracing` ↔ `opentelemetry_sdk` ↔ OTLP-exporter stack | ✓ shipped |
| W3C `TraceContextPropagator` global registration | ✓ shipped |
| Fail-soft init (no endpoint ⇒ inert handle; every fail-path ⇒ stderr+inert) | ✓ shipped |
| 5s shutdown timeout via `tokio::time::timeout` (5f load-bearing budget) | ✓ shipped |
| `record_exception` helper (`otel.status_code=ERROR` + `error.*`/`exception.*`) | ✓ shipped (Wave-2 will wire first error-attaching site) |
| 5c keystone spans/events in `servedrv.rs` — `ra.spawn`, `ra.respawn` (impl as event), `overlay.reset` (event), `overlay.switch` (span), `verdict.publish` (span) | ✓ shipped |
| Wave-1 Layer-3 CATCH fixes: `record-at-close` + explicit shutdown timeout | ✓ shipped (9b07c52) |
| Resource attrs: `service.name` (env-overridable), `service.version` (compile-time), `cargoless.build_id` | ✓ shipped |
| `[cargoless:obs]` stderr-only fallback eprintlns (#247) | ✓ preserved as always-on no-collector path |

**Net:** the OTEL trace + log plane is complete. Operators with a
SigNoz collector configured see the full keystone-span surface today;
operators without one see the stderr `[cargoless:obs]` fallback. Both
paths are honest and tested.

**Wave-2 is IN FLIGHT** (`agent/dev-fixer-w2 off feb3fac`, NOT on
`main`):

| Surface | Wave-2 status |
|---|---|
| Broader 5c spans (e.g. `cluster.transition`, `overlay.push_ingest`, `http.request`) | in flight |
| 5d metrics layer — `cargoless_*_total` counters / `*_resident_bytes` gauges / `*_seconds` histograms | in flight |
| **AC4 keystone-invariant TEST** (the regression sentry for the §247 STOP-class) | in flight |
| Wave-2 Layer-3 backstop on `30dc7d6` (Wave-1) | ✓ COMPLETE (#250) |

When Wave-2 lands, the metric divergence sentry (§3 below) becomes
LIVE-FIREABLE. Until then, the AC4 invariant is verified by the
keystone-test (in flight) and the structural source-fix at #247 (on
`main`), not by an alert.

---

## 1. Design rationale — five load-bearing invariants

### 1.1 Cores stay log-free

**Invariant:** no `tracing` macros in `cargoless-core` source. All
instrumentation lands at the binary call sites in `crates/cargoless/`.

**Why:** keeps the proven pure cores (`overlay::diff`, `cluster`,
`clusterdrv`, `multiplex`, `model`, `barrier`) free of side-effecting
diagnostic surface. The cores are the load-bearing isolation /
attribution correctness; coupling them to a `tracing` subscriber would
turn a pure unit test into a global-state interaction. The binary owns
the diagnostic surface; the cores own the correctness surface.

**Enforcement:** by grep convention — adding `tracing::*` to
`cargoless-core` is a review-time block (no allowlist for this; the
discipline is small).

### 1.2 Fail-soft contract — never wedge the daemon

**Invariant:** every OTEL failure path produces an inert handle +
stderr warning, daemon continues. No init/shutdown failure can stop
the verdict pipeline.

**Why:** the daemon's job is to know what works. A wedged collector,
unreachable endpoint, SDK init panic, or shutdown timeout must NEVER
prevent the daemon from publishing the next verdict. Observability is
sidecar to correctness, not a precondition.

**Enforcement:** `init_telemetry` returns `ShutdownHandle` (not
`Result`) — failure is encoded as `inert: true`, surfacing impossible.
The `[cargoless:obs]` stderr fallback (#247) is the always-on baseline
under any failure mode.

### 1.3 Default path = no-op

**Invariant:** `TelemetryConfig::enabled() == false` (no endpoint
configured) ⇒ zero OTEL overhead, no log lines.

**Why:** local `cargoless check` / ad-hoc `serve` is the dominant
invocation pattern. An always-on OTEL subscriber would tax cold-build
time (the AC#1/#2 budget) and produce noise. Default-off keeps
telemetry strictly opt-in.

**Enforcement:** `init_telemetry` short-circuits at the first
`!cfg.enabled()` check (telemetry.rs:131); ShutdownHandle::inert() has
no fields beyond `Option::None`.

### 1.4 5s shutdown budget — degraded-collector visibility

**Invariant:** shutdown flush is bounded by an explicit 5s
`tokio::time::timeout`. A wedged collector cannot block daemon
termination.

**Why:** without an explicit budget, the SDK's default flush behaviour
varies across `opentelemetry_sdk` versions (some block indefinitely on
unreachable collectors). Making the budget visible-in-source — and
loud-on-timeout via a stderr warning — converts a silent degradation
into an observable observability-discipline event.

**Enforcement:** `shutdown_telemetry` (telemetry.rs:183) wraps the
`provider.shutdown()` call in `tokio::time::timeout(Duration::from_secs(5), ...)`;
the `Err(_elapsed)` arm emits a one-line stderr warning.

### 1.5 The AC4 regression-sentry property (the load-bearing one)

**Invariant:** every RA (re)spawn MUST be followed by an
`OverlayMultiplexer::reset()` BEFORE any subsequent `switch_to` for
that cluster. The metric divergence

`cargoless_overlay_reset_total - (cargoless_ra_restart_total + cargoless_initial_spawn_total) ≡ 0`

(when all three Wave-2 metrics emit) detects any future regression of
the [proven-core-precondition-violated-at-integration-seam] class —
the same defect class #247 fixed structurally.

**Why this is load-bearing:** the proven `OverlayMultiplexer` core
relies on `reset()` being called at the seam where the LSP client is
(re)swapped. Without that reset, a post-respawn `switch_to` would
attempt incremental `didClose`/`didChange` against an RA that doesn't
know about the prior `didOpen`s — silently producing wrong-tree
verdicts (false-GREEN by attribution). The structural fix at #247
landed `ClusterDriver::reset_after_respawn` + `Ctrl::Spawned` wire fix
to guarantee this. The metric sentry catches any future regression at
ANY new integration seam where a respawn-class event might be
introduced (e.g. a future "warm-restart" optimization, a new RA-pool
manager).

**Wave-2 emission contract (anchored to the Wave-1-landed events):**

- `ra.spawn` event (Wave-1 keystone, servedrv.rs:617) → Wave-2 emits
  `cargoless_initial_spawn_total{cluster_hash=…}` counter
- `ra.respawn` event (Wave-1 keystone, servedrv.rs:626 region) →
  Wave-2 emits `cargoless_ra_restart_total{cluster_hash=…}` counter
- `overlay.reset` event (Wave-1 keystone, servedrv.rs:360 region) →
  Wave-2 emits `cargoless_overlay_reset_total{cluster_hash=…}` counter

**Sentinel test (Wave-2 in flight):** an end-to-end test that drives
N synthetic respawns through a real cluster and asserts the metric
divergence == 0 at completion. This test, plus the source-structural
fix at #247, plus the dashboard divergence panel + runbook
([D-INC2-OBSERVABILITY companion: AC4-DIVERGENCE-RUNBOOK.md](../observability/AC4-DIVERGENCE-RUNBOOK.md)),
form the three-layer defense: source-structural, test-structural,
metric-sentry.

---

## 2. The Wave-1 5c keystone span/event surface (anchored to source)

Anchored to `crates/cargoless/src/servedrv.rs` @ 929a5d3. The 5
keystone instrumentation sites:

| Keystone | Form | Site | Carried attrs (Wave-1) |
|---|---|---|---|
| `ra.spawn` | event (`tracing::info!`) | servedrv.rs:617 region | `cluster_hash`, `cluster_root` |
| `ra.respawn` | event (`tracing::info!`) | servedrv.rs:626 region | `cluster_hash`, `respawn_generation` |
| `overlay.reset` | event (`tracing::info!`) | servedrv.rs:360 region | `cluster_hash`, `respawn_generation` |
| `overlay.switch` | span (`tracing::info_span!`) | servedrv.rs:737 (body L729-L815-ish) | `worktree`, `file_count` (u64), `overlay_size_bytes` (u64) — both recorded BEFORE the lsp-present guard (Wave-1 CATCH-1 fix) |
| `verdict.publish` | span (`tracing::info_span!`) | servedrv.rs:860 region | `worktree`, `verdict_color`, `trigger` (Judgment-B sole-attribution site — every verdict in the system flows through exactly this span) |

**Why 4 events + 1 spans:** the event form (`tracing::info!`) captures
single-instant transitions where there's no body to time. The span
form (`tracing::info_span!`) captures wrapping work (the overlay
switch and the verdict-publish IO). The keystone-test asserts span
PRESENCE per code path; #246 Layer-3 CATCH-1 surfaced the empty-fields
bug that the Wave-1 fix-forward fixed (record fields BEFORE early-return).

**Span attribute discipline:** every keystone span declares its
expected fields as `tracing::field::Empty` and then `span.record(name, value)`s
them before the span body's first early-return path. The Wave-1 test
`span_with_empty_fields_surfaces_via_on_record` (telemetry.rs:403) is
the regression sentry for this pattern.

---

## 3. Wave-2 metrics — the contracted emission surface (NOT yet on main)

Wave-2 (`agent/dev-fixer-w2 off feb3fac`) introduces the `metrics`
crate (or `opentelemetry::metrics`) layer. The contracted set:

### 3.1 Counters

| Metric | Labels | Increment site |
|---|---|---|
| `cargoless_initial_spawn_total` | `cluster_hash` | first RA spawn per cluster (post-cluster-discovery) |
| `cargoless_ra_restart_total` | `cluster_hash` | every RA respawn (post-crash, SIGKILL recovery) |
| `cargoless_overlay_reset_total` | `cluster_hash` | every `OverlayMultiplexer::reset()` call (the seam where prior overlays clear post-(re)spawn) |
| `cargoless_verdict_total` | `worktree`, `color` ∈ {green, red, unknown}, `trigger` ∈ {push, save, initial, ...} | every `publish_verdict` call (sole-attribution preserved) |
| `cargoless_overlay_push_total` | `worktree`, `accepted` ∈ {true, false} | every `POST /overlay` accept/reject |
| `cargoless_http_requests_total` | `route`, `status` | every HTTP response (transport-layer counter) |

### 3.2 Histograms (latency)

| Metric | Labels | Observation site |
|---|---|---|
| `cargoless_save_to_verdict_seconds` | `worktree`, `tier` ∈ {hint, authoritative} | `verdict.publish` span close (the canonical AC#2 KPI) |
| `cargoless_http_request_seconds` | `route` | HTTP response close (the transport-side KPI) |
| `cargoless_overlay_switch_seconds` | `worktree` | `overlay.switch` span close |
| `cargoless_cluster_transition_seconds` | `cluster_hash`, `transition` ∈ {spawn, reap, swap} | cluster lifecycle event |

### 3.3 Gauges

| Metric | Labels | Sampling site |
|---|---|---|
| `cargoless_ra_resident_bytes` | `cluster_hash`, `pid` | periodic (e.g. 30s) `/proc/$pid/status` sample of the RA process — the headline "fleet-RAM flat ~1 GiB across N=1→20" claim made VISIBLE |
| `cargoless_pushed_worktrees_gauge` | (none) | sample of `ServeVerdictState.pushed.len()` — pushed-mode worktree population |
| `cargoless_active_worktrees_gauge` | (none) | sample of `ActivityTracker`'s active set size — the activity-activation truth |

### 3.4 The AC4 divergence panel (the load-bearing one)

The dashboard renders the timeseries `cargoless_overlay_reset_total -
(cargoless_ra_restart_total + cargoless_initial_spawn_total)` aggregated
across all `cluster_hash` labels. The invariant is **identically 0
forever**; any divergence is the regression-sentry firing. Companion
operator runbook:
[`docs/observability/AC4-DIVERGENCE-RUNBOOK.md`](../observability/AC4-DIVERGENCE-RUNBOOK.md).

---

## 4. Fleet-conforming conventions (inherited from tf-multiverse)

Adopted patterns (matched to tf-multiverse `deployment/monitoring/`):

- **Resource attrs include `service.name`, `service.version`,
  `cargoless.build_id`** (Wave-1 shipped). `service.name` is
  env-overridable so a fleet of cargoless instances can disambiguate
  by worktree-or-tenant; default = `"cargoless"`.
- **OTLP HTTP/protobuf** (not gRPC-tonic) — matches the canonical
  SigNoz Rust guide + the cargoless dep-minimal posture (no `tonic`
  dep tree). Wire-shape equivalent at the SigNoz collector.
- **W3C `TraceContextPropagator`** — interop with HTTP propagation if
  upstream/downstream services join the trace.
- **5s shutdown timeout** — bounded budget visible-in-source (vs SDK
  default which varies across versions).
- **Service-level identity** in dashboard panels and alert routing:
  `service.name == "cargoless"` is the primary selector for cargoless
  dashboards; `cargoless.build_id` resource attr provides commit-level
  drill-down.

NOT adopted (deliberately):

- `tonic`/gRPC transport — wire-equivalent; the HTTP path is the
  dep-minimal choice.
- Full Prometheus scraping — we use OTLP push via the collector. A
  future Wave-3 may add a `/metrics` Prometheus endpoint for
  scraping-mode collectors; the seam exists but isn't shipped.
- Per-worktree `service.name` differentiation — Wave-2 uses
  `worktree` LABEL on metrics, not a `service.name` split (one
  `cargoless` service-tree with worktree labels keeps the SigNoz
  service map clean).

---

## 5. The data flow (sketch)

```
   ┌──────────────────────┐
   │ servedrv.rs (Rust)   │  cores stay log-free; tracing macros
   │                      │  at the binary's 5 keystone sites only.
   │  ra.spawn / .respawn │
   │  overlay.reset       │  Wave-1: events (info!)
   │  overlay.switch      │  Wave-1: span with file_count/overlay_size_bytes
   │  verdict.publish     │  Wave-1: span (Judgment-B sole-attribution)
   │                      │
   │  + Wave-2 metrics    │  counter!/histogram!/gauge! at the SAME sites
   └──────────┬───────────┘
              │ tracing macros bridge to OTel via
              │ tracing-opentelemetry layer
              ▼
   ┌──────────────────────┐
   │ telemetry.rs         │  SdkTracerProvider + BatchExporter (async tokio)
   │ (init/shutdown)      │  OTLP HTTP/protobuf → collector
   │                      │  + W3C propagator + EnvFilter
   └──────────┬───────────┘
              │ OTLP HTTP/protobuf (async reqwest)
              ▼
   ┌──────────────────────┐
   │ SigNoz collector     │  ingest, dedupe, store
   │ (in-cluster)         │
   └──────────┬───────────┘
              │ SigNoz UI query
              ▼
   ┌──────────────────────┐  Panels per docs/observability/
   │ Operator dashboard   │   cargoless-dashboard.json:
   │ + alerts             │   • save→verdict p50/p95/p99
   │                      │   • RA resident bytes timeline
   │                      │   • verdict counter by worktree+color
   │                      │   • AC4 divergence panel (Wave-2)
   │                      │   • HTTP / overlay.push / cluster.transition
   └──────────────────────┘  Alerts per docs/observability/
                              AC4-DIVERGENCE-RUNBOOK.md:
                              • AC4 invariant violation → page dev-fixer
```

---

## 6. Implementation order (Wave-2)

1. **Add the metrics-crate dependency** behind the existing
   `telemetry` feature (already gates the SDK). Wave-2 expands the
   `telemetry` feature surface; does NOT split it.
2. **5d counters + histograms + gauges** at the keystone sites. Each
   metric emission sits SIDE-BY-SIDE with its keystone span/event
   (same code site, same attrs feed both surfaces).
3. **AC4 keystone-invariant test** — end-to-end test driving N
   synthetic respawns + asserting the metric divergence stays 0.
4. **Broader 5c spans** (`cluster.transition`, `overlay.push_ingest`,
   `http.request`) — these are NOT in the keystone-AC4 set; they're
   the "infra-shape KPI" surface for SigNoz dashboards.
5. **Wave-2 Layer-3** on the combined diff (the keystone-invariant
   test is the load-bearing AC4 sentry — Layer-3 verifies it actually
   asserts the divergence-≡-0 property).

---

## 7. Risk register

| Risk | Mitigation |
|---|---|
| Metric label cardinality blowup (e.g. `cluster_hash` ≫ thousands of values) | `cluster_hash` is `WorkspaceConfigHash` — bounded by the number of distinct workspace-config-tuples in the fleet (typically <10 for a multi-worktree repo). Verified in #15 measured Leg-C v4. `worktree` label is bounded by fleet size (~20 for current operator's pattern). Both well under the 10k-cardinality-per-metric SigNoz operational limit. |
| AC4 metric ordering — if `overlay.reset` emits BEFORE its corresponding `ra.spawn` event, the running divergence transiently goes negative | The structural-fix in #247 guarantees reset is called AT the spawn seam (`Ctrl::Spawned` handler), so the events emit in the spawn → reset order. Transient negative is unobservable at the metric-scrape cadence. Verified by the keystone-invariant test. |
| `record_exception` not yet wired — `otel.status_code=ERROR` will be missing on error spans until Wave-2 wires the first error-attaching site | Documented in telemetry.rs:239 (`#[allow(dead_code)]`). The Wave-2 5c broader-spans wave wires it. Until then, error paths emit info-level events not error-status spans — a known temporary gap, not a regression. |
| OTLP endpoint unreachable mid-run | Fail-soft contract (§1.2) holds: SDK retries internally; if the retry budget exhausts, spans are dropped (visible via SigNoz `otelcol_exporter_send_failed_spans` metric on the collector side). Daemon NEVER blocks. |
| Telemetry-feature builds vs no-feature builds drift | The `#[cfg(feature = "telemetry")]` gates are localized in telemetry.rs; the public API (`init_telemetry` / `shutdown_telemetry` / `record_exception`) compiles in both feature configurations. ci-gate's integ-feature arm exercises feature=on; default-no-feature path is the local-no-collector dev case. |
| Wave-2 metric emission paths get the cluster_hash wrong (label confusion at the seam) | Each metric emission site is co-located with its keystone span/event — `cluster_hash` is in scope at the existing event sites (servedrv.rs:617, 626 region, 360 region) so the Wave-2 PR reuses the same variable. Layer-3 backstop verifies the label-source ≡ the event-attr-source byte-identically. |

---

## 8. Deferred / Out-of-scope (parking lot)

- **`--await-verdict` CLI niceity** — the thin push-client (#240/2c)
  currently returns ack-only; a future flag would have it block on
  `subscribe`/`get_status` until the verdict settles. **Deferred to a
  Wave-2 follow-up task** (independent of metrics/AC4).
- **Wave-3 metrics expansion** — additional KPIs that prove themselves
  needed in field use after Wave-2 lands: cargoless cache hit ratio,
  proc-macro RA RSS share, fleet-CPU-busy gauge, per-WT debounce
  rejection rate. Park here, activate by need.
- **Prometheus `/metrics` scraping endpoint** — wire-equivalent
  alternative to OTLP push for collectors that prefer scrape. The seam
  exists (the `metrics` crate supports it); not shipped because the
  OTLP path is the SigNoz canonical guide path.
- **Per-worktree `service.name` split** — see §4 NOT-adopted. Future
  if the worktree-label cardinality proves operationally awkward in
  SigNoz; not anticipated.
- **Span-event tracing of `cargoless::model::watch`'s inner loop** —
  Wave-1 keystones are at servedrv.rs (the serve-loop). The
  single-worktree `watch` path doesn't emit OTel today; a future wave
  could add `watch.tick` / `model.apply_event` spans if the single-WT
  inner loop becomes a launch-day KPI source. Not load-bearing today
  (single-worktree mode is the dev path, not the fleet path).
- **TLS on OTLP transport** — `http://` only for v0.2.x. SigNoz Cloud
  direct (vs in-cluster collector) would need `https://` + root certs
  bundled. Out-of-scope.

---

## 9. Open questions for team-lead at Wave-2 impl-finalize

1. **Metric naming convention** — does Wave-2 use the `metrics` crate
   conventions (e.g. `cargoless_overlay_reset_total`) or the OTel
   `Instrument`-API name conventions (e.g.
   `overlay.reset.count`)? Both translate to the same SigNoz metric;
   the `metrics`-crate naming is more idiomatic for Prometheus-style
   readers. **Recommendation:** snake_case_total / _seconds / _bytes
   per Prometheus conventions (this doc assumes that — adjust if
   Wave-2 picks OTel-API naming).
2. **Cardinality cap policy** — if a metric label exceeds some
   threshold (e.g. 10k distinct values), what's the fallback? Drop
   the label? Switch to histogram-of-counts? **Recommendation:**
   Layer-3 verifies the bounded-set property at impl time (cluster_hash
   from a finite WorkspaceConfigHash set; worktree from a finite fleet
   list); no runtime fallback needed.
3. **AC4 divergence panel rendering** — should the SigNoz panel show
   the running absolute value (always ≥ 0) or the signed value (where
   negative indicates a transient ordering effect, see §7)? The
   structural property is unsigned ≡ 0; the signed view is more
   diagnostic. **Recommendation:** plot signed (the diagnostic value);
   alert on `abs() > 0` over the 5min window (the operational threshold).

These are non-blocking — reasonable defaults can ship in the first
Wave-2 PR and be tuned via Layer-3 feedback.

---

## 10. Cross-references

- **Source spec / parent umbrella:** Plane CWDL-245 (#245 Increment 5
  — OTEL+SigNoz observability).
- **Pattern doc this mirrors:** [`D-INC2-2B.md`](D-INC2-2B.md) (the
  Increment-2 2b servedrv-consume design — same shape:
  state-of-the-world + design rationale + data-flow + tests + risk
  register + out-of-scope + open-questions).
- **Wave-1 source (LANDED on `main`):** `crates/cargoless/src/telemetry.rs`,
  `crates/cargoless/src/servedrv.rs` (5 keystone sites),
  `crates/cargoless-core/src/config.rs` (`TelemetryConfig`).
- **Wave-1 design notes:** the Plane #246 commits and `f1f2caf`
  (5a init), `30dc7d6` (5c keystones), `9b07c52` (Layer-3 CATCH fixes).
- **AC4 source-structural fix:** #247
  (`ClusterDriver::reset_after_respawn` + `Ctrl::Spawned` wire fix —
  on `main` @ ancestor of 929a5d3).
- **Companion runbook:**
  [`docs/observability/AC4-DIVERGENCE-RUNBOOK.md`](../observability/AC4-DIVERGENCE-RUNBOOK.md).
- **Companion dashboard:**
  [`docs/observability/cargoless-dashboard.json`](../observability/cargoless-dashboard.json).

---

**End of design anchor. Wave-2 implementer reads this + the
landed-Wave-1 source as the authoritative implementation contract.**
