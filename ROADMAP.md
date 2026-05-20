# Roadmap

> **Status snapshot (2026-05-20):** the **repo-scoped Model-R daemon**
> is feature-complete and **v0.2.0 is tagged** (operator-decided
> 2026-05-19; ratified on `main`). Since the tag, the **central
> in-cluster topology** workstream has landed substantively on `main`
> (Increment-0 read-plane wiring + Increment-1 deploy manifest design
> + Increment-2 overlay-push ingest 2a/2b/2c + Increment-4
> de-WASM-gate + Wave-1 OTEL traces + #247 STOP-class fix + operator
> pre-stage runbook) — that workstream is documented below in the new
> "Post-v0.2.0 (in-flight)" section, parallel to (NOT before) the
> still-deferred v0.1 browser-reload adapter. Fleet RAM remains
> **measured flat** — ≈1 GiB across N ∈ {1,2,4,8,16,20} active
> worktrees, ≈19–30× collapse vs the per-worktree-daemon model, one
> multiplexed rust-analyzer mechanism-verified
> ([`AC7-THROUGHPUT-REPORT.md §11.4`](docs/bench/AC7-THROUGHPUT-REPORT.md),
> Model-R Leg-C v4) — this *replaces* the prior cycle's Model-A
> "~19.4 GiB BORDERLINE" extrapolation. The ≈2.05× per-edit CPU win vs
> `trunk serve` is **unchanged under Model R** (two-source-confirmed,
> §8.5). The browser/HTTP adapter remains deferred (orthogonal to the
> daemon and to the central in-cluster topology). **The
> public-launch GO remains the operator's decision** — this roadmap
> describes capabilities, not a ship date.

cargoless's earlier per-worktree single-tree checker (the `watch`-per-WT
shape) was a **superseded internal intermediate**; the repo-scoped
daemon (`serve --repo`, one multiplexed RA) is the architecture.
Post-v0.2.0, **two parallel workstreams** exist beyond v0: the
**central in-cluster topology** (Increment-5 umbrella — already
substantively landed on `main`, see "Post-v0.2.0" below) and the
**v0.1 browser-reload adapter** (still deferred, orthogonal to both
the daemon and the central topology). The parking lot is the rest of
the ambitious-but-not-now ideas. Anything that doesn't sharpen the
codebase's self-knowledge or reduce the latency from brokenness to
signal is **not** in the launch surface — it goes to one of those
parallel workstreams or to the parking lot.

> **Name:** the product, the published crate, and the binary are all
> **`cargoless`** (operator decision D1, 2026-05-17). Internal library
> crates are `cargoless-proto` / `cargoless-cas` / `cargoless-core`
> (post-#97 full one-token brand on `main`). The capabilities
> described here are unaffected.

---

## v0 — what just shipped (headless continuous checker + latest-green publisher)

v0 is **single-developer, single-machine, headless** — for **any Cargo
workspace** (native Rust or Rust+WASM). It always knows what's green;
for Rust+WASM workspaces (`cdylib + wasm32` / `leptos`) it also
**publishes** the latest green WASM artifact to a pointer file. The
check / serve / watch tiers are target-general (any Cargo workspace);
the `build --watch --out` WASM-artifact publisher tier engages only on
WASM workspaces — by nature there is no WASM artifact to publish on a
native target. v0 does **not** serve a browser — the live
HTTP/WebSocket dev-server is v0.1.

### v0 capabilities (available today on `main`)

- **`cargoless check`** — one-shot verdict. Exit code 0 on green, non-zero on
  red; diagnostics formatted file:line:col + severity + code + message.
- **`cargoless watch`** — continuous headless verdict stream with per-line
  relative timestamps, in two tiers shown live: a **RA-incremental hint**
  (median ≤1s — field-measured at ~0.74s on the dogfood Leptos project
  after the debouncer fix) and the **authoritative cargo-check verdict**
  (bounded by `cargo check` itself — seconds on small projects, ~20-30s on
  a Leptos-sized tree; no sub-1s promise). The fast hint can flip RED
  instantly; only the authoritative tier drives GREEN. Rationale:
  [`docs/design/D-A2-RENEGOTIATION.md`](docs/design/D-A2-RENEGOTIATION.md);
  evidence: [dogfood report](docs/dogfood/PHASE-2-REPORT.md).
- **`cargoless build --watch --out <dir>`** — continuous build that publishes
  the latest green WASM artifact via an atomic `.cargoless/latest-green`
  pointer. Requires the upstream `trunk` binary to perform the actual WASM
  build (cargoless wraps it; see the install note in the README).
- **`cargoless status`** — daemon liveness + last verdict + latest-green hash.
- **`cargoless clean`** — clear the content-addressed cache.
- **Zero-config auto-detection** — any Cargo workspace is auto-detected
  on first run; native Rust and `cdylib` + `wasm32` / `leptos` projects
  all work without flags (#241 de-WASM-gate, landed on `main`). The
  WASM-artifact publisher tier (`build --watch --out`) engages only
  when the workspace is `cdylib` + `wasm32` — by nature, native targets
  have no WASM artifact to publish.
- **Agent-edit-batch as the cost unit.** The primary consumer is an AI
  agent writing whole files atomically (`Write`/`Edit` of a complete
  file). cargoless optimizes for the per-batch verdict, not
  per-keystroke; the structural-completeness trigger
  (`TF_STRUCTURAL_TRIGGER=1`, default-off v0 spike) is the seam that
  makes "only-meaningful-states-cached" a guarantee. See
  [`docs/design/D-OPENCLOSED.md`](docs/design/D-OPENCLOSED.md).

### The nine acceptance criteria (v0 definition of done)

| # | Promise | Status |
|---|---------|--------|
| 1 | Zero-config headless startup within 30s — daemon up + config auto-detected + watch→verdict pipeline live, zero manual config | ✅ field-PASS (~0.08s to streaming on the dogfood project) |
| 2a | RA-incremental **hint** median ≤1s on a committed reference project (fast hint; does not by itself prove the tree compiles) | ✅ field-PASS post-debouncer-fix (~0.74s on Leptos) |
| 2b | **Authoritative** verdict median ≤ bare `cargo check` +10% (cargo-check tier; no sub-1s promise — cargo's own runtime dominates) | ✅ MEASURABLE-PASS (`AC7-THROUGHPUT-REPORT §8.5`: cargoless 3.35s vs trunk 6.89s CPU/edit, two-source-confirmed) |
| 3 | Median green-save → latest-green artifact *published* latency, threshold from evidence (no sub-second artifact claim) | ✅ AC#4 publish-cycle empirical PASS; AC#7 §2.2 artifact-tier measured |
| 4 | Never publish red — `.cargoless/latest-green` only advances on green; a red tree or failed build never moves it | ✅ PASS (publish-cycle empirical test + structural verification) |
| 5 | CAS dedupe — identical source state is a cache hit, build skipped | ✅ structural PASS (integration tested) |
| 6 | Survives `kill -9` of rust-analyzer — daemon survives + transparently restarts | ✅ field-PASS (RA respawn under 1s; restart line surfaces to the watch stream) |
| 7 | Published two-mode benchmarks (checker save→verdict + artifact save→publish, reported separately) | ✅ MARGINAL→PASS-with-compound-framing (`AC7-THROUGHPUT-REPORT` §8.5 + §11 — 2-of-2 dimensions clearly better vs `trunk serve`: per-edit CPU + fleet-capability) |
| 8 | README / ROADMAP / CONTRIBUTING / LICENSE present | ✅ (this commit) |
| 9 | Launch blog post reviewed by ≥2 people incl. one outside the team | ⏳ draft ready; reviewer gate pre-publish per [`AC9-REVIEWER-PACKET.md`](docs/launch/AC9-REVIEWER-PACKET.md) |

For the full evidence trail of the field-PASSes, residual issues, and the
production-hardening sweep that closed 11 of 12 dogfood findings, see
[`docs/dogfood/PHASE-2-REPORT.md`](docs/dogfood/PHASE-2-REPORT.md).

### v0 limits — what cargoless deliberately does NOT do (yet)

These are not bugs or oversights — they are **intentional v0 scope cuts**
that protect the launch surface. Each is on the v0.1 or v1 list.

- **No browser, no HTTP, no WebSocket.** For the Rust+WASM case
  cargoless does not serve your WASM bundle. If you need that today,
  run `trunk serve` (or a static server like `miniserve`) against the
  directory cargoless publishes via `cargoless build --watch --out
  <dir>`. The integrated dev-server is v0.1.
- **Not a `trunk serve` drop-in replacement in v0** (for the Rust+WASM
  case). cargoless replaces the *verdict* and *latest-green-publisher*
  surfaces, not the browser-facing serve loop. v0.1 closes that gap.
  (For native-Rust workspaces there is no `trunk serve` analog; the
  relevant comparison is `cargo check` / `bacon` — see below.)
- **Not a `trunk build` replacement.** `cargoless build --watch --out`
  wraps `trunk build` (which calls cargo + wasm-bindgen + post-processing).
  cargoless drives it and adds the watch/publish loop on top.
- **Not a novel native-Rust checker.** For native-Rust workspaces the
  check tier is rust-analyzer flycheck wrapping host-triple
  `cargo check` — *the same checker `bacon` runs*. cargoless's
  differentiator for the native case is **not** a new checker; it is
  the shared-RA fleet-RAM property (one multiplexed RA across N
  worktrees — flat in N, see the status snapshot), the
  verdict-provenance discipline (per-crate + diagnostics retention),
  and the **central in-cluster topology** (Increment-5 umbrella —
  substantively landed on `main`; see "Post-v0.2.0" below for what's
  shipped vs in-flight).
- **No hot-swap, no symbol-level granularity, no editor LSP plugin.**
  Per the v1 parking list.

---

## Post-v0.2.0 (in-flight on `main`) — central in-cluster topology

The Increment-5 umbrella (Plane #245) — a workstream that emerged
**after** v0.2.0 tagged, distinct from the v0.1 browser-reload
adapter (see below). Where v0.1 targets the local human-developer
browser-reload loop, this workstream targets the **in-cluster
agent-fleet / CI** consumer: a long-lived `cargoless serve` deployed
as a Kubernetes Service that other in-cluster consumers query via
HTTP+SSE+bearer for verdicts and ingest via `POST /overlay` for
overlay-pushed (no-shared-FS) workloads. The two workstreams are
parallel and orthogonal — both layer on top of the v0 cores, neither
blocks the other.

### What's LANDED on `main` (substantively shipped)

- **Increment-0 — read-plane wiring** (#225): `servedrv` constructs
  a `VerdictService` over its per-WT state, binds `HttpServer::bind`
  with `authorizer_for(&cfg)`, emits `TransitionEvent` from the sole
  `EmitVerdict` site, exposes the unauthenticated `/healthz`
  readiness route (#236).
- **Increment-1 — deploy manifest design** (#226): the long-lived
  cargoless-serve Service shape (Namespace + bearer-auth Secret +
  RWO state PVC + Deployment with `terminationGracePeriodSeconds=90`
  + ClusterIP Service + Ingress/Egress NetworkPolicy). Parked on
  `agent/builder-infra-serve-k8s` for the rebase + image-bake
  pipeline activation; the design is durable.
- **Increment-2 — overlay-push ingest** (#240): additive
  `Request::PushOverlay { worktree, base_ref, files }` proto verb +
  the HTTP server's first body-reading bearer-gated `POST /overlay`
  route + `ServeVerdictState::push_overlay` override + serve-loop
  drain consuming pushed overlays into the existing `SwitchOverlay`
  arm + thin push-client (`cargoless push --remote <url>`). The pure
  `overlay::diff()` core is **byte-unchanged** — only the OverlaySet
  *source* swaps from disk-read to pushed payload.
- **Increment-4 — de-WASM-gate** (#241): `cargoless check`/`serve`
  accepts ANY Cargo workspace (native Rust or Rust+WASM). The check
  / serve / watch tiers are target-general; the `build --watch
  --out` WASM-artifact publisher remains WASM-specific by nature.
- **Increment-5 Wave-1 — OTEL telemetry foundation + keystone spans**
  (#246): `crates/cargoless/src/telemetry.rs` OTEL+SigNoz init via
  OTLP HTTP/protobuf + 5 keystone spans/events (`ra.spawn`,
  `ra.respawn`, `overlay.reset`, `overlay.switch`, `verdict.publish`)
  in `servedrv.rs`. Fail-soft contract (no endpoint ⇒ inert handle,
  zero overhead). 5s shutdown timeout. Resource attrs:
  `service.name` / `service.version` / `cargoless.build_id`.
- **#247 STOP-class structural fix**: `ClusterDriver::reset_after_respawn`
  + `Ctrl::Spawned` wire fix — guarantees every RA-(re)spawn is
  followed by `OverlayMultiplexer::reset()` before any subsequent
  `switch_to` for that cluster. This is the source-structural layer
  of the AC4 three-layer defense (see [`D-INC2-OBSERVABILITY.md`](docs/design/D-INC2-OBSERVABILITY.md) §1.5).
- **Operator pre-stage runbook** (lands alongside this ROADMAP
  refresh as `docs/operator/DEPLOY-MILESTONE-PRESTAGE.md` — currently
  parked on `agent/docs-launch-lead-prestage`, pending integration):
  the operator-actionable checklist for `REGISTRY_TRIFORM_USER`/
  `REGISTRY_TRIFORM_TOKEN` Forgejo repo secrets + the
  `cargoless-otel-config` ConfigMap shape + the optional
  `cargoless-otel-headers` Secret + verification + activation
  sequence. Makes the deploy-milestone executable, not just designed.

### What's IN FLIGHT (not yet on `main`)

- **Increment-5 Wave-2 — metrics layer (5d)** + broader 5c spans +
  AC4 keystone-invariant test. Off `agent/dev-fixer-w2`. Adds
  counters (`cargoless_overlay_reset_total` /
  `cargoless_ra_restart_total` / `cargoless_initial_spawn_total` /
  `cargoless_verdict_total` / ...), histograms
  (`cargoless_save_to_verdict_seconds`), gauges
  (`cargoless_ra_resident_bytes` — the headline "fleet-RAM flat
  across N" claim made VISIBLE). When Wave-2 lands, the AC4 metric
  divergence sentry becomes live-fireable (operator runbook for
  that alert ships alongside this ROADMAP refresh as
  `docs/observability/AC4-DIVERGENCE-RUNBOOK.md`).
- **Image-bake pipeline**: a release-pipeline workflow producing
  `registry.triform.cloud/cargoless/cargoless-serve:<version>` from
  the integ-build artifact. The pre-stage runbook's
  `REGISTRY_TRIFORM_*` secrets feed this.
- **#235 operator pre-stage activation**: the operator authorising
  the deploy milestone — pre-stage runbook is the bridge.

### Design anchors

- [`docs/design/D-FLEET-SHARED-DAEMON.md`](docs/design/D-FLEET-SHARED-DAEMON.md)
  — the central-service architecture this workstream realises.
- [`docs/design/D-INC2-2B.md`](docs/design/D-INC2-2B.md) — the
  Increment-2 2b servedrv-consume implementation anchor (the
  overlay-push contract that 2a/2b/2c implement; promoted from
  spike with the Inc-2 land, so it covers the same scope the
  original `D-PUSHOVERLAY.md` design-ahead spec did).
- `docs/design/D-INC2-OBSERVABILITY.md` (lands alongside this
  ROADMAP refresh — currently parked on `agent/docs-launch-lead-w2-docs`,
  pending integration) — the Increment-5 implementation-anchor; the
  Wave-1 vs Wave-2 distinction; the 5 design invariants
  (cores-stay-log-free, fail-soft, default-no-op, 5s shutdown
  budget, AC4 regression-sentry).
- `docs/observability/cargoless-dashboard.json` (same parked branch
  as above) — the 23-panel SigNoz/Grafana-import dashboard sketch;
  each panel marks WAVE-1-LIVE vs WAVE-2-PENDING.

### Honest scope

This workstream is **post-v0.2.0 and pre-launch-GO**. It does NOT
change the v0 capabilities described above; it ADDS the in-cluster
deployment shape on top of the same v0 cores. The launch GO remains
the operator's decision and is orthogonal to this workstream's
completion — operators can choose to ship v0.2.0 (local-only) and
activate the central in-cluster topology independently when
business needs justify it.

---

## v0.1 — optional live server / browser-reload adapter (deferred)

A thin adapter on top of the v0 latest-green publisher. It consumes the
published `.cargoless/latest-green` output and adds the browser.
**Distinct from the central in-cluster topology above** — v0.1 targets
the local human-developer browser-reload loop; the central topology
targets the in-cluster agent-fleet / CI consumer. The two are
parallel, neither blocks the other. **None of this is required for
the v0 launch — it is the next obvious step for the
local-developer-browser case, not a shipping promise with a date.**

- HTTP static server over the latest-green directory.
- WebSocket channel to the browser; Trunk-compatible reload protocol,
  full-reload, browser reload shim.
- Cold-start holding page (browser-facing).
- Browser "never serve red" — the server keeps serving last-green while
  the tree is red (the browser-facing consumer of v0's never-publish-red
  guarantee).
- `cargoless serve` command (one-command drop-in for `trunk serve`).

The std-only implementation already exists as research on branches
`agent/devserver` and `agent/devserver-bundle` — preserved, not deleted, so
v0.1 is a wiring exercise rather than a rewrite.

**Why v0.1 is deferred rather than shipped together:** the v0 promise
(verdict + publish, headless) is fully testable without a browser. Folding
the browser/HTTP surface into v0 would have doubled the launch-hardening
surface area for a feature that is strictly additive on top of the
publisher contract. Better to ship v0 honest and small than v0 + v0.1
half-done.

### Single-RA RAM follow-up — the tiered ladder (full design in `D-RAM-TIERS.md`)

> Applies to BOTH the post-v0.2.0 central in-cluster topology AND
> the still-deferred v0.1 browser-reload adapter — both ride on the
> same single multiplexed rust-analyzer, so any ladder reduction
> applies once across all consumer shapes. The section title used to
> read "v0.1 perf follow-up"; the ladder is in fact orthogonal to
> which post-v0 workstream is shipping.

**The launch fleet-RAM story is Model R's measured architectural
collapse** (one multiplexed rust-analyzer, total RSS flat in worktree
count — see the status snapshot + `AC7-THROUGHPUT-REPORT §11.4`). The
per-RA tiered ladder below is now **secondary**: it tunes the
footprint of the *single shared* analyzer (a constant-factor
reduction), it is no longer the fleet-scale lever it had to be in the
per-worktree-daemon model. The single shared RA's steady-state
footprint is still rust-analyzer-dominated (~1.5-2 GB on
proc-macro-heavy projects such as Leptos, because RA runs proc-macro
expansion by default), and the honest tiered ladder still applies to
that one process (numbers from
[`AC7-THROUGHPUT-REPORT §10`](docs/bench/AC7-THROUGHPUT-REPORT.md#10-stage-2--per-tier-rss-delta)
+ [`D-RAM-TIERS.md`](docs/design/D-RAM-TIERS.md) verdict table):

- **default** Tier-1/2 (shipped, behaviour-neutral, no opt-in) —
  ≈**−19 %** RSS vs pre-tier baseline. The `MALLOC_ARENA_MAX=2`
  glibc-arena cap is the entire delta (RA-thread fragmentation
  reclaim; zero functional effect).
- **Tier-3 proc-macro-off as default** (`TF_RA_PROCMACRO_OFF=1`) —
  ≈**−53 %** RSS (`AC7-THROUGHPUT-REPORT §5/A3` vs A2 baseline 2.08 GB).
  **Shipped default-safe** (#126 RA-native-downrank
  + no-wrong-verdict proof); field-verified on real 38-`view!`-site
  Leptos (#130 — no false-GREEN; ≈5× faster-to-RED, n=1-scoped, not
  a universal speedup). Under Model R this is a **secondary
  constant-factor reduction on the one shared RA**, not the
  fleet-scale lever (the architecture is the fleet-scale answer).
- **Tier-4 idle-evict** (`TF_RA_IDLE_EVICT=1`) — designed +
  prototyped + measured (#122/#125, default-off in v0). Per-event
  ≈88-97 % RA-RSS reclaim validated; sustained reduction is
  workload-shape-dependent (function of `gap / RA-busy-time` —
  ≈5 % on the bench's tight-gap Leptos regime, larger at real
  minute-scale agent-think-gaps).
- **`--features csr`** (project-narrowable only) — ≈**−75 %** RSS  
  (`AC7-THROUGHPUT-REPORT §5/A4` vs same A2 baseline) + CPU collapse.
  The v0.1 auto-narrow change makes the narrowed
  configuration the **auto-detected default** rather than an opt-in
  flag, so the memory win lands without the user having to know the
  knob exists.

The fleet-scale answer is **no longer compound, and no longer an
extrapolation**: Model R's repo-scoped daemon multiplexes one
rust-analyzer across all worktrees, so total fleet RAM is **measured
flat — ≈1 GiB across N ∈ {1,2,4,8,16,20}, ≈19–30× below the
per-worktree-daemon model**, mechanism own-eyes-verified (one RA LSP +
one proc-macro-srv constant across N). This *replaces* the prior
cycle's Model-A "Tier-3 → ≈19.4 GiB borderline / Tier-3+idle-evict →
≈14-18 GiB / `--features csr` → ≈10.6 GiB" **compound extrapolation**.
The ladder above still reduces the single shared RA's footprint
(secondary constant-factor); it is not the existence answer — the
architecture is. See
[`AC7-THROUGHPUT-REPORT §11.4`](docs/bench/AC7-THROUGHPUT-REPORT.md)
for the measured per-N curve + the v1→v3 honest audit trail. Full
single-RA-ladder design tracked in
[`D-RAM-TIERS.md`](docs/design/D-RAM-TIERS.md).

---

## v1 — parking lot (not v0, not v0.1, not the central in-cluster topology)

The long-horizon list. These are deliberately **not** on a roadmap with
dates; they're the ideas that would justify their own design pass and
their own sprint if and when the v0 + post-v0.2.0 + v0.1 surfaces
prove the foundation:

- salsa / rust-analyzer-as-library deep integration
- remote / shared CAS backend
- **team features + remote auth** — PARTIALLY ADDRESSED by the
  bearer-auth `Authorizer` shipped in Wave-1 (the network seam exists
  + is fail-closed; multi-tenant team primitives still parking-lot).
- **multi-agent build coordination** — PARTIALLY ADDRESSED by the
  central in-cluster topology (one shared service many agents
  consume); deeper cross-agent coordination still parking-lot.
- editor LSP-style interface (cargoless-as-LSP)
- symbol-level green/red granularity
- replacing `trunk build` internals
- hot-swap WASM
- **CI integration (cargoless-as-CI-driver)** — PARTIALLY ADDRESSED
  by the Increment-2 thin push-client (`cargoless push --remote
  <url>`) which is the natural CI-stage shape; deeper integration
  (cargoless-as-the-CI-platform) still parking-lot.
- Windows support

If you find yourself wanting one of these, open an issue — community
demand is the strongest signal we have for what graduates from v1 to a
later v0.x or v1.0. (For the three "PARTIALLY ADDRESSED" items above:
the partial-coverage is the post-v0.2.0 central-topology landed work;
the parking-lot entry remains for the deeper feature beyond what's
shipped.)

---

## Where work is tracked

- **Public:** [GitHub Issues](https://github.com/TriformAI/cargoless/issues) — the canonical surface for outside contributors.
- **Internal:** the agent team uses a Plane project ("CWDL") that mirrors
  the structure above; GitHub Issues is authoritative for community-facing
  work and the Plane copy is for the agent-driven dev loop. The two are
  reconciled by maintainers.

If you want to influence the v0.1 / v1 priorities, the most effective
thing is to open a GitHub issue describing the *use case* (what you're
trying to build, where cargoless's current shape falls short). That's
worth more than a vote on a list of feature names.
