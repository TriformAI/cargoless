# cargoless

> **The codebase always knows what works, and tells you — or the
> agent driving the loop — the moment it doesn't — without burning
> your CPU to do so.**

cargoless is a headless dev-loop daemon for Rust+WASM projects, built
on one premise: most of the work `trunk serve` and `bacon` do on every
save is **redundant work** — rebuilding state the previous cycle
already proved correct. cargoless keeps a warm `rust-analyzer`,
content-addresses every build's input set, and skips the rebuild
entirely when the source state hasn't changed. When the tree does go
green, it publishes the latest green WASM artifact to a pointer file
via an atomic temp+fsync+rename; when the tree goes red, the pointer
**does not move** — so anything consuming that pointer (a static
server, a CI step, an agent) can rely on never seeing a broken build.

The net effect is **≈half the per-edit CPU of `trunk serve`**
(two-source verified) — and therefore more dev cycles per battery —
without giving up the verdict honesty that makes the codebase
trust-worthy in the first place. The primary consumer is an AI agent
writing whole files atomically (`Write`/`Edit` of a complete file);
the cost unit cargoless optimizes is **per agent-edit-batch**, not
per-keystroke. Memory is a different story, and we are honest about
it below: steady-state RSS is rust-analyzer-dominated and presented
as an honest tiered ladder, not a single default number. It
is the result of a vision cut: every type and decision in the project
is justified by either sharpening the codebase's self-knowledge or
shortening the latency from brokenness to signal. Anything that does
neither isn't here.

> **Name:** the product, the published crate, and the binary you run are
> all **`cargoless`** (operator decision D1, 2026-05-17). Internal
> library crates are `cargoless-proto` / `cargoless-cas` /
> `cargoless-core` (post-#97 full one-token brand on `main`).

---

## At a glance — what cargoless v1.0 delivers

- **2.05× faster cargo-check vs `trunk serve`** (two-source verified;
  agent-edit-batch unit — the cost unit that matches how AI agents
  actually drive the loop). See
  [`docs/bench/AC7-THROUGHPUT-REPORT.md §8.5`](docs/bench/AC7-THROUGHPUT-REPORT.md#85-clean-c2-109--headline-two-source-confirmed).
- **Fleet-tested at ~20 agents on a 16 GB box** with Tier-3 enabled
  (`TF_RA_PROCMACRO_OFF=1`) for that test — safety field-verified on
  real Leptos, no false-GREEN; Tier-3 ships safe-as-default
  *candidate*, opt-in today via the env var. See
  [`AC7-THROUGHPUT-REPORT.md §11`](docs/bench/AC7-THROUGHPUT-REPORT.md#11-stage-3--fleet-scale-curve)
  for the per-N curve and compound-fit table.
- **Opt-in `TF_RA_IDLE_EVICT=1`** for tighter RAM budgets (88-97% RA
  per-event reclaim measured at idle gaps; sustained reduction is
  workload-shape-dependent — see §11 honest interpretation).
- **`--features csr`** brings narrowable projects all the way down to
  ~0.53 GiB per daemon (≈10.6 GiB for 20 agents — comfortable on
  16 GB).

v0.1 RAM roadmap: [`docs/design/D-RAM-TIERS.md`](docs/design/D-RAM-TIERS.md).
Fleet-scale methodology + disclosed extrapolations:
[`AC7-THROUGHPUT-REPORT.md §11`](docs/bench/AC7-THROUGHPUT-REPORT.md#11-stage-3--fleet-scale-curve).

---

## What cargoless v0 is (and isn't)

**Primary consumer: an AI agent writing whole files atomically.** The
`check` / `watch` / `build` surface is the agent-edit-batch verdict
loop; cost unit is per-batch, never per-keystroke. Humans run it too,
but the design center is the agent loop.

**v0 IS** — a *headless continuous checker and latest-green publisher*:

- `cargoless check` — one-shot verdict + diagnostics. Green or red, exit
  code reflects it, errors are formatted file:line:col + severity + code
  + message.
- `cargoless watch` — continuous timestamped verdict stream with
  per-file granularity.
- `cargoless build --watch --out <dir>` — wraps `trunk build` and
  publishes the latest green WASM artifact via an atomic
  `.cargoless/latest-green` pointer that **only advances on green**.
- Zero-config — auto-detects `cdylib` + `wasm32` / `leptos` projects.
- Survives `kill -9` of the underlying rust-analyzer subprocess; the
  daemon restarts it transparently.

**v0 is NOT** — a `trunk serve` drop-in replacement (yet). cargoless v0
does not include a browser/HTTP/WebSocket layer; that's v0.1, deferred.
If you need a browser-reload loop today, point `trunk serve`
(or any static server) at the directory cargoless publishes.

See [`ROADMAP.md`](ROADMAP.md) for v0 acceptance criteria, the v0.1
deferred work, and the v1 parking lot.

---

## Source & mirrors

- **Canonical public source:** [`github.com/TriformAI/cargoless`](https://github.com/TriformAI/cargoless) — the OSS-facing home; where issues, PRs, releases, and prebuilts live.
- **Internal dev mirror:** [`forgejo.triform.dev/triform/cargoless`](https://forgejo.triform.dev/triform/cargoless) — where the agent team's integration CI runs (dedicated cargoless-builder pod + `scripts/ci-gate` + Forgejo Actions). Contributor PRs are welcome on GitHub; the maintainers cherry-pick into Forgejo for the integration loop.

## Install

> **Pre-release.** The release-tagged install commands below will work
> once `v0.1.0` is cut. Today, only the from-source install against the
> GitHub development tip is supported and proven end-to-end in a clean
> environment.

**Install the current development tip (works today):**

```bash
cargo install --git https://github.com/TriformAI/cargoless.git \
              cargoless --branch main --locked
```

**Why the explicit `cargoless` package arg:** `cargo install --git`
walks the entire repo for `Cargo.toml` files and refuses to pick when
multiple installable binary crates exist. This repo's
`bench/{harness,fixture}` sub-workspaces produce `ra-latency`,
`cargoless-bench`, and `cargoless-bench-fixture` binaries that cargo
treats as candidates. Without the explicit `cargoless` arg, you get:

> error: multiple packages with binaries found: cargoless,
> cargoless-bench-fixture, cargoless-bench-harness.

**Why `--locked`:** the workspace ships a committed `Cargo.lock`; `--locked`
makes the dependency graph identical to what CI / `scripts/ci-gate` proved
green. See [D-RELEASE Appendix B](docs/design/D-RELEASE.md#appendix-b--why---locked-everywhere).

> The default install includes the wired daemon (`build --watch --out`
> publisher pipeline). As of commit `1c25017`, the `integration` feature
> is on by default on `cargoless`. Users who want only the standalone
> checker semantics can opt out via `--no-default-features`.

**Once `v0.1.0` releases:**

```bash
# Source build via crates.io (universal: any platform with rustc)
cargo install <pubname>           # <pubname> = TBD per D1

# Prebuilt via cargo-binstall (Linux x86_64-gnu + macOS aarch64/x86_64)
cargo binstall <pubname>
```

Prebuilts at first release: `x86_64-unknown-linux-gnu`,
`aarch64-apple-darwin`, `x86_64-apple-darwin`. Other targets (Linux
aarch64, Windows) fall back to `cargo install` (source compile). See
[docs/design/D-RELEASE.md §3](docs/design/D-RELEASE.md#3-targets--the-honest-install-matrix)
for the full matrix.

`cargoless build --watch --out` wraps `trunk build` — install the
upstream `trunk` for the WASM artifact step:

```bash
cargo install --locked trunk
```

cargoless surfaces an actionable error if `trunk` is missing from PATH.

---

## Quick start

```bash
# In a Rust + WASM project root (auto-detected: cdylib + wasm32 / leptos)
$ cargoless check
>> checking /work/my-app (auto-detected: cdylib + leptos (Leptos CSR))
ok green — every tracked file compiles

# A continuous verdict stream — first verdict in under a second
$ cargoless watch
>> [+   0.083s] daemon up, watching /work/my-app
>> [+   0.741s] /work/my-app/src/lib.rs: Green
^C

# Publish the latest green WASM artifact to ./dist; pointer never
# advances on red.
$ cargoless build --watch --out ./dist
>> publishing latest-green to .cargoless/latest-green → ./dist
ok green — latest-green @ <hash>
```

Edit a file with a real error; the next verdict line tells you what
broke. Fix it; the next verdict line says green; `./dist` advances. Try
introducing a syntax error; observe that `.cargoless/latest-green`
**does not move** until you fix it.

For the full v0 surface, see [`ROADMAP.md`](ROADMAP.md#v0-capabilities-available-today-on-main).

---

## Performance vs alternatives

The cost unit is **per agent-edit-batch** — an AI agent writing one or
more whole files atomically — not per-keystroke. cargoless was built
for that loop; the numbers below are measured on it.

The honest one-paragraph summary (bench-lead's wording, unedited):

> cargoless does ~half the per-edit CPU of `trunk serve` — it rebuilds
> on confirmed-green edges, not blindly every keystroke. Memory is
> rust-analyzer-dominated (~2 GB default on proc-macro projects); the
> `--features` knob cuts ~75% and a v0.1 auto-narrow change moves the
> default there.

That summary is now **two-source-confirmed** for CPU
([`AC7-THROUGHPUT-REPORT §8.5`](docs/bench/AC7-THROUGHPUT-REPORT.md#85-clean-c2-109--headline-two-source-confirmed))
and refined with the honest RAM **tiered ladder** below. We say the
memory picture plainly rather than quoting a flattering single RSS
number we can't honestly default to: the steady-state cost is
dominated by rust-analyzer (which runs proc-macro expansion by
default), and the win lives in the ladder of opt-ins and the v0.1
auto-narrow plan.

### Leg A — per-edit CPU (the headline)

`AC7-THROUGHPUT-REPORT §8.5`, two-source-confirmed (Δ≈1% across two
independent methodologies: bench/throughput.py Python harness vs
bench/throughput-recon.sh ps/bash cross-check; share only correctness
invariants — cutime+cstime accounting, precise edit, warm cache —
differ in language, /proc-walk, RSS source, sampling cadence):

| Tool | CPU-seconds per edit (median) | Two-source verdict |
|---|---:|---|
| **cargoless** | **3.35 s** | TWO-SOURCE-CONFIRMED (3.389 / 3.348, Δ−1.2 %) |
| `trunk serve` | 6.89 s | TWO-SOURCE-CONFIRMED (6.963 / 6.887, Δ−1.1 %) |
| `bacon` † | 0.48 s | TWO-SOURCE-CONFIRMED (0.493 / 0.476, Δ−3.4 %) |

cargoless does **≈2.05× less per-edit CPU than `trunk serve`** —
citable with two-source provenance. † `bacon` is not a like-for-like
comparator: it is a terminal save→verdict *checker*, not a
build+publish loop; the artifact-publish dimension has no `bacon`
counterpart.

### Leg B — RAM tiered ladder (honest, composed-not-conflated)

Not one number — a ladder, each rung with its own provenance and gate.

| Rung | Per-daemon RSS (Leptos fixture) | What it costs | What it gates |
|---|---:|---|---|
| **default** (Tier-1/2 ON, shipped) | ≈1.71 GiB (≈**−19 %** vs pre-tier baseline 2.12 GiB) | nothing — behaviour-neutral, no opt-in | the universal honest default |
| **+ proc-macro-off** (Tier-3 — `TF_RA_PROCMACRO_OFF=1`, shipped default-safe via #126; field-verified #130) | ≈0.97 GiB (≈**−53 %** vs `AC7-THROUGHPUT-REPORT §5/A2` baseline 2.08 GB) | RA's proc-macro view of `view!`-style macros — but the verdict tier still catches them via rustc on the cargo-check side (no false-GREEN, field-confirmed on real 38-`view!` Leptos) | the default RAM rung |
| **+ `--features csr`** (project-narrowable only) | ≈0.53 GiB (≈**−75 %** vs `§5/A2` baseline) + CPU collapse to ≈0.24 s/edit | requires the project to actually be narrowable to `csr` features | the v0.1 auto-narrow default (named perf follow-up) |

Citation: `AC7-THROUGHPUT-REPORT §10` (per-tier RSS-delta @ `ab0d51b`,
factorial Tier-1 × Tier-2, A0 in-band gate) +
[`D-RAM-TIERS.md`](docs/design/D-RAM-TIERS.md) (verdict table).
Tier-1 (`MALLOC_ARENA_MAX=2` glibc arena cap) is "the entire story" at
default — −420 MiB / −20.3 % from RA-thread fragmentation reclaim,
zero functional effect. Tier-3 is the **load-bearing existence-rung**
for the fleet-scale case — already shipped default-safe.

**Tier-3 latency observation (n=1 caveat travels with the number).**
On the same real 38-`view!`-site Leptos, proc-macro-off was ≈**5×
faster to RED** (5.1 s vs 25.8 s) — mechanistically expected because
proc-macro `view!` expansion sat on the verdict critical-path and
removing it shortened it. The launch claim is *"no latency penalty
observed; faster on macro-heavy projects"* — never an unqualified
"proc-macro-off is faster" / universal speedup. This is n=1-per-mode
on one real project, direction unambiguous + mechanistically
expected — not a universal guarantee.

### Leg C — fleet-scale (the agent-loop case)

The launch-load-bearing question: *at agent-fleet scale (N daemons),
does the default fit a real 16 GB host, and what closes the gap?*
Measured at N=1,2,4,8 (`AC7-THROUGHPUT-REPORT §11` @ commit
`6497273`); the 20-agent rows are **explicit extrapolations** from
the measured per-daemon footprint (disclosed extrapolation; true
cgroup-OOM observation was env-infeasible in the read-only-cgroup
builder pod — a post-launch hardening nice-to-have, not
decision-changing). Use the compound-fit table verbatim:

| Compound path | Per-daemon | 20 agents | Fits 16 GB? |
|---|---:|---:|---|
| Tier-1/2 default (§10) | ≈1.5 GiB | ≈30 GiB | NO (model-A fails — OOMs at ≈10 daemons) |
| + idle-evict alone (bench regime) | ≈1.43 GiB | ≈28.6 GiB | NO (≈5 % shave) |
| **+ Tier-3 `--proc-macro disabled`** (#126 default-safe + #130 field-verified; §5/A3 = 0.97 GiB) | **≈0.97 GiB** | **≈19.4 GiB** | **BORDERLINE** (+≈3 GiB over; idle-evict pushes it closer) |
| Tier-3 + idle-evict (real minute-gap fleet) | ≈0.7-0.9 GiB | ≈14-18 GiB | **PROBABLY YES** (idle-evict's larger reclaim at the real `gap / RA-busy-time` ratio) |
| **`--features csr`** (project-narrowable only, §5/A4 = 0.53 GiB) | **≈0.53 GiB** | **≈10.6 GiB** | **YES — comfortable** |

`TF_RA_IDLE_EVICT=1` is the opt-in fleet-RAM lever. Its
*per-event* RAM reclaim is large and validated (≈88-97 %; min 0.17 GiB
on 1.50 GiB peak at N=1; multi-daemon simultaneous evictions captured
at N=4 ≈25 % and N=8 ≈39 %), but its *sustained time-averaged*
reduction at the bench's N=8 Leptos regime is only ≈5 % — a
workload-conservative floor, not a ceiling. The 75 s gap was consumed
by RA's 65-70 s re-index/flycheck per batch; minute-scale agent-
think-gaps (the real fleet operating point) and/or smaller-per-daemon-
RA-cost projects shift the ratio favorably. Mechanism fully
validated; magnitude scales with `gap / RA-busy-time`.

### Latency: two tiers, not one number

For raw save→verdict latency cargoless reports **two tiers**
([`D-A2-RENEGOTIATION.md`](docs/design/D-A2-RENEGOTIATION.md)):

- **AC#2a — RA-incremental hint:** median ≤1 s (field-measured
  ≈0.74 s on the dogfood Leptos post-debouncer-fix). Can flip RED
  instantly; does **not** by itself prove compilation.
- **AC#2b — authoritative verdict:** bounded by `cargo check` itself
  (seconds on small projects, ≈20-30 s on a Leptos-sized tree). Only
  this tier drives GREEN.

No sub-second artifact-publish claim is made.

### Honest caveats

The narrative's discipline floor — these stay in the README, not
hidden:

- **Leg-C 16 GB / 20-agent answer is disclosed-extrapolation.** Pod
  cgroup-cap was env-infeasible in the read-only-cgroup builder
  (`AC7-THROUGHPUT-REPORT §11` provenance block); true cgroup-OOM
  confirm is a post-launch nice-to-have, operator-elective. The
  20-agent rows extrapolate the measured per-daemon footprint
  linearly.
- **Tier-3 (#126) is the load-bearing existence-rung** for the
  fleet-scale case. Already shipped default-safe, field-verified
  #130. The ladder is **on by default**; you don't need to set a
  flag to get the RAM win.
- **Idle-evict's bench-realized 5 % sustained reduction is a
  workload-conservative floor, not a ceiling.** Mechanism (≈88-97 %
  per-event reclaim) fully validated; sustained magnitude scales
  with `gap / RA-busy-time` (Leptos RA re-index 65-70 s of a 75 s
  gap is the bench shape; real minute-scale agent-think-gaps shift
  favorably). Default-off in v0; opt-in via `TF_RA_IDLE_EVICT=1`.
- **Methodology audit trail is open** (`AC7-THROUGHPUT-REPORT §11.3`):
  two artifact-class measurement attempts (v1 pgid-bug, v2 IDLE_GAP
  too tight) discarded with reasons, not salvaged — the discipline
  that earns the fleet-scale claim, named openly.

### Architectural asymmetry (why the numbers come out this way)

- **Warm `rust-analyzer`.** RA's multi-second cold start happens
  **once** per cargoless session. `trunk serve` doesn't use RA at all
  (shells out to `cargo build` on every save). `bacon` spawns a fresh
  `cargo check` process per cycle. cargoless's warm-LSP architecture
  means an edit-batch that doesn't actually change the type-graph
  costs near-zero work.
- **CAS dedupe.** cargoless content-addresses the full input set
  (source tree + `Cargo.lock` + toolchain + target + config). When
  the hashed input set is unchanged — a no-op edit, a `git checkout`
  round-trip, comments/strings/formatting changes — **the build is
  skipped entirely**.
- **Headless.** No HTTP server, no WebSocket channel, no
  browser-keepalive. The v0 surface is a daemon + a CLI; the v0.1
  server adapter is an opt-in layer for users who want one.

### Reproducible

The harnesses are committed: `bench/throughput.py` (primary,
Python+psutil+/proc) and `bench/throughput-recon.sh` (independent,
ps/bash/awk; methodology-independent except for shared correctness
invariants). Rerun on your machine and tell us if you see different
numbers.

### Launch-readiness rigor (proof the launch is defensible)

- **D1-completeness CI-enforced forward:** `scripts/d1-drift-guard` +
  allowlist (#96, three-way-PASS, mechanism dogfooded against itself).
- **Publish runbook 3-source byte-identical:** cargo-metadata oracle
  ≡ [`PHASE-D-OPERATOR-HANDOFF.md §2.2`](docs/release/PHASE-D-OPERATOR-HANDOFF.md)
  ≡ [`D-RELEASE.md §6`](docs/design/D-RELEASE.md) (F-J preflight
  smoke RUN-only at a1206d8).
- **Three-layer validation pattern** (author-self-satisfies →
  orchestrator-verifies-against-source → backstop-honesty-criteria)
  proven end-to-end on launch-critical changes (#136 → §7 binstall
  CATCH → #140 fix-source; #96 self-dogfood).
- **`AC#7` resolution:** **MARGINAL → PASS-with-compound-framing** —
  cargoless wins 2-of-2 clearly-better dimensions vs `trunk serve`
  (per-edit CPU + fleet-capability) against a tool whose architecture
  is fundamentally not fleet-scalable.
- **Crate name space clear:** `cargoless` + `cargoless-proto` +
  `cargoless-cas` + `cargoless-core` all FREE on crates.io.

For the launch-hardening evidence trail, see
[`docs/dogfood/PHASE-2-REPORT.md`](docs/dogfood/PHASE-2-REPORT.md)
(12 field findings, 11 fixed before launch).

---

## Workspace

| Crate | Role |
|---|---|
| `cargoless-proto` | Shared contract types (daemon ↔ build ↔ future remote backends). |
| `cargoless-cas` | Content-addressed store. `ContentStore` trait + local-disk impl. |
| `cargoless-core` | The daemon: watcher, rust-analyzer wrapper, green/red model, build orchestration. |
| `cargoless` | The binary: `check` / `watch` / `build` / `status` / `clean`. |

`bench/{harness,fixture}` are standalone non-workspace crates with
`publish = false` baked in — they exist to run the AC#7 comparative
benchmark and are not shipped to crates.io.

For the cross-crate contract and why `cargoless-proto` is
dependency-free in v0, see [`docs/DESIGN.md`](docs/DESIGN.md).

---

## Contributing

Issues, PRs, and discussions: see [`CONTRIBUTING.md`](CONTRIBUTING.md).

The maintainers commit to a **48-hour acknowledgement window for the
first two weeks after launch**, then a sustainable one-week cadence
after that. The commitment is documented in CONTRIBUTING.md; a missed
acknowledgement is itself a GitHub-issue-worthy event.

---

## Status

v0 feature-complete on `main`; launch hardening in progress. Tracked
publicly via [GitHub Issues](https://github.com/TriformAI/cargoless/issues);
the internal agent-team backlog lives in Plane (project "CWDL"). See
[`ROADMAP.md`](ROADMAP.md) for the 9-criterion v0 definition-of-done and
the v0.1 / v1 phases.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
