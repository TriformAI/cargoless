# Roadmap

> **Status snapshot (2026-05-17):** v0 is feature-complete and undergoing
> launch hardening. v0.1 (browser/HTTP adapter) is deferred. v1 is the
> long-horizon parking lot. Headline performance numbers from the AC#7
> comparative benchmark are pending and will be published in the README and
> launch blog when they land.

cargoless is phased **v0 → v0.1 → v1**. The scope of each phase is
deliberately tight: v0 is the smallest thing that delivers the vision claim
(*"the codebase always knows what works, and tells you the moment it
doesn't"*), v0.1 is the obvious next adapter, v1 is the rest of the
ambitious-but-not-now ideas. Anything that doesn't sharpen the codebase's
self-knowledge or reduce the latency from brokenness to signal is **not**
v0 — it goes to v0.1 or v1.

> **Name:** the product, the published crate, and the binary are all
> **`cargoless`** (operator decision D1, 2026-05-17). Internal library
> crates remain `tf-proto` / `tf-cas` / `tf-core`. The capabilities
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

### The nine acceptance criteria (v0 definition of done)

| # | Promise | Status |
|---|---------|--------|
| 1 | Zero-config headless startup within 30s — daemon up + config auto-detected + watch→verdict pipeline live, zero manual config | ✅ field-PASS (~0.08s to streaming on the dogfood project) |
| 2a | RA-incremental **hint** median ≤1s on a committed reference project (fast hint; does not by itself prove the tree compiles) | ✅ field-PASS post-debouncer-fix (~0.74s on Leptos) |
| 2b | **Authoritative** verdict median ≤ bare `cargo check` +10% (cargo-check tier; no sub-1s promise — cargo's own runtime dominates) | ⏳ MEASURABLE-PASS pending the comparative bench landing the relative-cost number ([D-A2](docs/design/D-A2-RENEGOTIATION.md) §2/§8-Q1) |
| 3 | Median green-save → latest-green artifact *published* latency, threshold from evidence (no sub-second artifact claim) | ⏳ measured by the AC#7 comparative bench; threshold published when bench reports |
| 4 | Never publish red — `.cargoless/latest-green` only advances on green; a red tree or failed build never moves it | ⏳ verification-in-flight (publish-cycle empirical test landing; structural PASS already verified) |
| 5 | CAS dedupe — identical source state is a cache hit, build skipped | ✅ structural PASS (integration tested) |
| 6 | Survives `kill -9` of rust-analyzer — daemon survives + transparently restarts | ✅ field-PASS (RA respawn under 1s; restart line surfaces to the watch stream) |
| 7 | Published two-mode benchmarks (checker save→verdict + artifact save→publish, reported separately) | ⏳ in flight; results published with caveats |
| 8 | README / ROADMAP / CONTRIBUTING / LICENSE present | ✅ (this commit) |
| 9 | Launch blog post reviewed by ≥2 people incl. one outside the team | ⏳ draft pending review |

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

### v0.1 perf follow-up — auto-narrow `--features` (single highest-leverage perf change)

The **single highest-leverage performance follow-up** is **auto-narrow
`--features`**: cargoless's steady-state footprint is rust-analyzer-dominated
(~2 GB default on proc-macro-heavy projects such as Leptos, because RA runs
proc-macro expansion by default). The `--features` knob already cuts that
materially (≈75%, *pending* bench-lead's two-source confirmation); v0.1
makes the narrowed configuration the **auto-detected default** rather than
an opt-in flag, so the memory win lands without the user having to know the
knob exists. This is *not* a v0 claim — v0 ships honest about RA-dominated
memory; the auto-narrow change is the v0.1 work that moves the default. It
is the largest single lever on the perf story and is named here so the
roadmap reflects where the throughput follow-up effort goes first.

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
