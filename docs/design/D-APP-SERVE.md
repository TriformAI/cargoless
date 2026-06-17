# D-APP-SERVE — run the apps cargoless certifies (multi-instance, never-serve-red)

**Status:** Shipping (Part A inc-1..5 landed on `agent/app-serve`, CI build/clippy/fmt green). inc-6 (hardening) + inc-7 (deploy) in progress; inc-8 (SIGHUP hot-reload) + per-push previews parked. Part B (tf-multiverse integration) goes through that repo's own gate.

**Provenance:** The original `trunk serve` framing in `CLAUDE.md` — cargoless does not just *check* a Cargo workspace, it can *run* the application it certifies. Settled with the operator: multi-instance from day one, latest-green per ref, public hosts per instance — `preview.triform.dev` for the `dev` canary (the bare-host special case) and `<other-instance>.preview.triform.dev` for every other ref. The read-only status surface (the daemon's `/app` + `/readyz`) is published at `cargoless.preview.triform.dev` so agents/operators can poll a rolling preview without holding the control-plane bearer. All preview pods deploy into `triform-staging` to reach the staging data plane.

---

## 1. The vision cut, restated for app-serve

> The codebase always knows what works — and *serves* what works, the moment it works, and never serves what doesn't.

"Never publish red" (AC#4 of the check tier) becomes **"never serve red"**: the running app is only ever advanced to a build that proved healthy under a live HTTP probe; a red build or a failed boot leaves the older green app **byte-untouched and still serving**.

This is target-general in spirit but the launch wedge is **Rust+WASM** (tf-multiverse): one daemon tracks N configured git refs, builds every new HEAD per ref through that ref's own `cargoless.app.yaml`, boots the result, health-probes it, and atomically swaps the public proxy to it — draining the previous child's in-flight connections (SSE/WebSocket) to completion.

---

## 2. Layering — pure core, effectful edge

The cut that makes the lifecycle testable without threads/sockets/subprocesses:

```
            ┌─────────────────────── cargoless-core (library, unit-tested) ───────────────────────┐
  events →  │  appstate::AppState        the PURE state machine: step(inst, event) -> [Action]     │
            │    Idle/Queued/Building/Probing + serving + red/drain/respawn, serialized build      │
            │    queue (newest-sha-wins), generation guards. ZERO I/O.                              │
            │  appdrv::Driver            executes Actions against injected seams; single promote    │
            │    site; port allocator. Whole lifecycle tested in-process with fakes.                │
            │  l4proxy::L4Proxy          std-only TCP byte-splice; atomic upstream slot; gauge.     │
            │  appbuild::build           checkout → ordered steps → harvest; Indeterminate guard.   │
            │  appsvc::AppServeState     VerdictService over the read plane (/app, /readyz).        │
            │  appstatefile             durable per-instance state mirror (crash recovery).         │
            │  appmanifest / appinstances  the two config parsers (yamlscan subset).                │
            └──────────────────────────────────────────────────────────────────────────────────────┘
            ┌─────────────────────── cargoless (bin, the OS glue) ───────────────────────┐
            │  appserve::run            ProcLauncher (real spawn/probe/SIGTERM-tree),     │
            │    ThreadBuildBackend (detached build threads), serve_loop (200ms tick,     │
            │    ref pollers, signal→flag shutdown), main.rs Cmd::AppServe.               │
            └─────────────────────────────────────────────────────────────────────────────┘
```

Every effectful operation the driver needs is a trait — `BuildBackend`, `ChildLauncher`, `EventSink` — so the production daemon wires real processes while the test suite wires in-process fakes and drives the *exact* action-execution + event-feedback wiring deterministically.

---

## 3. The per-instance lifecycle (per ref)

Two orthogonal axes per instance:

- **serving**: `Option<ServingChild>` — what the L4 proxy points at. The **only** writers are a successful probe (promote) and the serving child's own exit. A red build/boot cannot touch it.
- **pipeline**: `Idle → Queued{sha} → Building{sha,gen} → Probing{sha,gen} → Idle`.

```
HeadAdvanced(sha) ─▶ Queued ─(build slot)▶ Building ─green─▶ Probing ─200─▶ PROMOTE (flip proxy slot,
                                                │                                advance pointer, drain old)
                                                ├─red───────────────────────▶ RecordRed (serving untouched;
                                                │                                 sha never auto-retried)
                                                └─indeterminate──────────────▶ requeue once, then red
```

**Build-queue arbitration** is daemon-wide and serialized (one shared `CARGO_TARGET_DIR`; two concurrent 40 GB-target builds would just contend). FIFO across instances; within an instance the newest sha supersedes a queued older one in place. A *running* build is never cancelled. Probing does **not** hold the build slot — the next instance builds while this one boots.

**Generations**: every build/probe attempt carries a daemon-unique generation stamped by `AppState`. A late result from a superseded attempt is discarded (the hard-witness discipline — detached workers are never joined, so stale completions must be cheap to ignore).

**Crash recovery**: on boot, each instance with a durable `last_green` respawns that bundle and probes it *before any build* — service is restored in seconds, not a cold build.

---

## 4. The L4 proxy — why raw TCP, not HTTP

tf-multiverse serves WebSocket screencast and SSE chat — long-lived, bidirectional, non-request/response traffic. A byte-splice proxy is protocol-agnostic: it copies bytes each way until EOF, so connection upgrades, chunked bodies, and infinite streams all pass through unmodified. The swap invariant is correspondingly simple: **a connection is pinned to the upstream it was accepted against**; flipping the atomic slot only redirects *future* accepts. An open SSE stream therefore rides its old child to completion — exactly the drain semantics the state machine encodes (`StartDrain` → `DrainComplete`, gated on the per-generation connection gauge reaching zero).

The upstream slot packs `(generation << 16) | port` in a single `AtomicU64` so the splice hot path reads a torn-free `(gen, port)` pair on every new connection, and the single promote site is the only writer.

---

## 5. Config surfaces — two files, one subset

- **`cargoless.app.yaml`** (rides each commit, read from the instance worktree at the build sha): *how to build and run* — ordered build steps with timeouts, harvested artifacts, the run command + `port_env`, the health path + timeouts, the drain grace. A branch evolves its own pipeline; the change takes effect exactly when that branch's HEAD does. Carries a sha256 `manifest_hash` recorded in every bundle's provenance.
- **The instances file** (`--instances`, a daemon-side ConfigMap): *which refs to serve and where* — `{name, ref, app_bind, env}` per instance. Env values support strict `${VAR}` interpolation from the daemon's own environment, so a per-branch `DATABASE_URL` secret lives in the pod env (a k8s Secret) and **never** in the committed/ConfigMap'd file. An unresolved `${VAR}` is a startup parse error naming the variable — never a silent empty string.

Both use the same hand-rolled YAML subset (`yamlscan`, block form only) as `cargoless.checks.yaml`: version gate, unknown-key rejection at every level, line-attributed errors, no external YAML dependency.

---

## 6. The read plane — additive, gate stays byte-identical

The daemon's `--bind` control plane reuses the **same** hand-rolled HTTP server as the gate (`transport::http`). The only new route is `GET /app` (a JSON snapshot of every instance's phase/serving-sha/last-red/drain depth), exposed via one additive `VerdictService::app_report()` default method that returns `None` on the gate daemon — so `GET /app` on a gate is a `404` **byte-identical** to any unknown route (pinned by a guard test). `GET /readyz` reflects `AppServeState::ready()`: true once every instance that has *ever* gone green is currently serving (a never-green branch does not hold the pod un-ready; a red build never un-readies — the old green keeps serving).

---

## 7. Deployment (inc-7 / Part B)

- `deploy/cargoless-appserve.Dockerfile`: the gate's `cargoless` binary **plus** the WASM toolchain (wasm32 target, wasm-bindgen-cli 0.2.114, binaryen/wasm-opt 122, tailwindcss 4.2.1 — version-locked to tf-multiverse so the preview builds the same artifacts staging does), `CARGO_INCREMENTAL=1` for warm rebuilds.
- `deploy/cargoless-appserve.k8s.yaml` (reference; Flux-authoritative copy lives in tf-multiverse): one Deployment (1 replica, Recreate) in `triform-staging`, a ConfigMap instances file, a 250Gi PVC, per-instance app Services, and an Ingress with one public host per instance (SSE/WS pass-through annotations, per-host HTTP-01 TLS).
- **Accepted risks**: dev-instance migrations hit the shared staging DB ahead of the staging deploy (canary posture; blue/green means the NEW binary migrates while the OLD serves ⇒ migrations must stay expand/contract, already required by staging's rolling deploys). Both instances share NATS/Iggy/RustFS; the feature instance's DB isolation does not extend to queues (parked limitation). `STORED_FUNCTION_DRIFT_AUTOCORRECT` stays unset on the preview (BUGS-1531).

---

## 8. Non-goals / parking lot

Per-push (per-overlay) previews; SIGHUP hot-reload of the instance set (restart-to-reconfigure for now); seeding a feature DB from a staging snapshot; queue/stream isolation per instance; parallel per-instance target dirs (revisit only if serialized builds hurt at N>2); Windows.
