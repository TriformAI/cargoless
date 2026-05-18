# Roadmap

> **Status snapshot (2026-05-18):** v0 is feature-complete; launch
> hardening complete and AC#7 numbers landed (two-source-confirmed in
> [`docs/bench/AC7-THROUGHPUT-REPORT.md`](docs/bench/AC7-THROUGHPUT-REPORT.md):
> ≈2.05× per-edit CPU vs `trunk serve`, the RAM ladder, and the
> fleet-scale compound-fit table). v0.1 (browser/HTTP adapter +
> auto-narrow features + Tier-3/4 RAM-roadmap maturation) is deferred.
> v1 is the long-horizon parking lot.

cargoless is phased **v0 → v0.1 → v1**. The scope of each phase is
deliberately tight: v0 is the smallest thing that delivers the vision claim
(*"the codebase always knows what works, and tells you the moment it
doesn't"*), v0.1 is the obvious next adapter, v1 is the rest of the
ambitious-but-not-now ideas. Anything that doesn't sharpen the codebase's
self-knowledge or reduce the latency from brokenness to signal is **not**
v0 — it goes to v0.1 or v1.

> **Name:** the product, the published crate, and the binary are all
> **`cargoless`** (operator decision D1, 2026-05-17). Internal library
> crates are `cargoless-proto` / `cargoless-cas` / `cargoless-core`
> (post-#97 full one-token brand on `main`). The capabilities
> described here are unaffected.

---

## v0 — what just shipped (headless continuous checker + latest-green publisher)

v0 is **single-developer, single-machine, headless**. It always knows
what's green and **publishes** the latest green build to a pointer file.
It does **not** serve a browser — the live HTTP/WebSocket dev-server is
v0.1.

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
- **Zero-config auto-detection** — a `cdylib` + `wasm32` / `leptos` project
  needs no flags; auto-detected on first run.
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

- **No browser, no HTTP, no WebSocket.** cargoless does not serve your
  WASM bundle. If you need that today, run `trunk serve` (or a static
  server like `miniserve`) against the directory cargoless publishes via
  `cargoless build --watch --out <dir>`. The integrated dev-server is v0.1.
- **Not a `trunk serve` drop-in replacement in v0.** cargoless replaces
  the *verdict* and *latest-green-publisher* surfaces, not the
  browser-facing serve loop. v0.1 closes that gap.
- **Not a `trunk build` replacement.** `cargoless build --watch --out`
  wraps `trunk build` (which calls cargo + wasm-bindgen + post-processing).
  cargoless drives it and adds the watch/publish loop on top.
- **No hot-swap, no symbol-level granularity, no editor LSP plugin.**
  Per the v1 parking list.

---

## v0.1 — optional live server / browser-reload adapter (deferred)

A thin adapter on top of the v0 latest-green publisher. It consumes the
published `.cargoless/latest-green` output and adds the browser. **None of
this is required for the v0 launch — it is the next obvious step, not a
shipping promise with a date.**

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

### v0.1 perf follow-up — the RAM ladder (full design in `D-RAM-TIERS.md`)

cargoless's steady-state footprint is rust-analyzer-dominated
(~1.5-2 GB per daemon on proc-macro-heavy projects such as Leptos,
because RA runs proc-macro expansion by default). The launch RAM
story is **the honest tiered ladder** (numbers from
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
  a universal speedup). This is the **load-bearing existence-rung**
  for the fleet-scale case.
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

The fleet-scale existence answer (20 agents on a 16 GB host) is
**compound**: Tier-3 (already shipped default-safe) brings the
extrapolated 20-agent footprint to ≈19.4 GiB (borderline); Tier-3 +
idle-evict at real minute-gaps to ≈14-18 GiB (probably yes);
`--features csr` where narrowable to ≈10.6 GiB (comfortable).
See [`AC7-THROUGHPUT-REPORT §11`](docs/bench/AC7-THROUGHPUT-REPORT.md#11-stage-3--fleet-scale-curve)
for the per-N curve and compound-fit table verbatim. Full v0.1 design
work tracked in [`D-RAM-TIERS.md`](docs/design/D-RAM-TIERS.md).

---

## v1 — parking lot (not v0, not v0.1)

The long-horizon list. These are deliberately **not** on a roadmap with
dates; they're the ideas that would justify their own design pass and
their own sprint if and when v0 / v0.1 prove the foundation:

- salsa / rust-analyzer-as-library deep integration
- remote / shared CAS backend
- team features + remote auth
- multi-agent build coordination
- editor LSP-style interface (cargoless-as-LSP)
- symbol-level green/red granularity
- replacing `trunk build` internals
- hot-swap WASM
- CI integration (cargoless-as-CI-driver)
- Windows support

If you find yourself wanting one of these, open an issue — community
demand is the strongest signal we have for what graduates from v1 to a
later v0.x or v1.0.

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
