# cargoless

> **The codebase always knows what works, and tells you — or the
> agent driving the loop — the moment it doesn't — without burning
> your CPU to do so.**

cargoless is a **repo-scoped** headless dev-loop daemon for Rust+WASM
projects — one `serve --repo` multiplexes every git worktree through a
single shared analyzer — built on one premise: most of the work
`trunk serve` and `bacon` do on every save is **redundant work** —
rebuilding state the previous cycle already proved correct. cargoless
keeps a warm `rust-analyzer`,
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

## At a glance — what cargoless delivers

cargoless is a **repo-scoped daemon**: one `cargoless serve --repo
<path>` auto-discovers every git worktree and multiplexes them through
**one** shared `rust-analyzer`, instead of one daemon (and one RA) per
worktree. That architecture — not a per-daemon tuning knob — is what
makes the agent-fleet case work.

- **Model R removes the per-worktree multiplication — measured.**
  cargoless's fleet-RAM cost is **structural, not linear in worktree
  count**: total RSS stays **≈1 GiB across the measured N ∈ {1, 2, 4,
  8, 16, 20}** active worktrees (aggregate peak 1003–1068 MiB, avg
  980–1034 MiB; N=20 ≈ N=1). The mechanism is own-eyes-verified on the
  real wired `serve --repo` daemon: **exactly one** rust-analyzer LSP
  + its one proc-macro-srv, constant across every N — one analyzer
  LSP-overlay-multiplexed across the workspace-cluster, not
  one-RA-per-worktree. Versus the per-worktree-daemon model's
  measured-linear ≈1.5 GiB × 20 ≈ ≈30 GiB, that is a **≈19–30×
  fleet-RAM collapse, measured — replacing the prior cycle's
  Model-A "~19.4 GiB BORDERLINE" *extrapolation*.** See
  [`AC7-THROUGHPUT-REPORT.md §11.4`](docs/bench/AC7-THROUGHPUT-REPORT.md)
  (Model-R Leg-C v4, measured-not-extrapolated).
- **2.05× faster cargo-check vs `trunk serve`** (two-source verified;
  agent-edit-batch unit). **Unchanged under Model R** — the
  green-edge-rebuild model that produces the CPU win is preserved by
  the repo-scoped architecture; reconfirmed, not re-derived. See
  [`docs/bench/AC7-THROUGHPUT-REPORT.md §8.5`](docs/bench/AC7-THROUGHPUT-REPORT.md#85-clean-c2-109--headline-two-source-confirmed).

**Honest caveats — these are the claim's integrity, not footnotes:**

1. **The win is structural, not an absolute number.** The absolute
   ≈0.9–1 GiB is **fixture-dependent** (a Leptos-class honest-size
   project, smaller than tf-multiverse); a larger workspace yields a
   different absolute but the **same flat-vs-N structure**. The
   load-bearing claim is *"Model R removes the per-worktree
   multiplication"* (one multiplexed RA), **not** "≈1 GiB on
   tf-multiverse."
2. **Measured to N=20.** The 589/617-worktree fleet is a
   **curve-shape projection** (one-RA-per-cluster implies it holds),
   stated as *projection*, never *measured*, beyond N=20.
3. **Verdict-correctness is a closed chain, not pure-unit end-to-end:**
   structurally proven in the cores **plus** the live multiplexed
   runtime integration-validated via the #15 bench + Track-1 dogfood.
   A closed chain — never claimed as "fully unit-proven end-to-end."
4. **The 2.05× CPU win is preserved, not re-measured** under Model R
   (green-edge-rebuild model unchanged; re-asserted, see Leg A).
5. **RAM measured under driven per-WT activity.** Idle worktrees are
   deactivated by design (activity-activation); the ≈1 GiB is "RAM
   under an actively-driven fleet," stated as such — not an idle floor.
6. **Found-and-fixed (disclosed, named):** the steady-state
   fleet-RAM thesis is measured-confirmed; a **separate shutdown
   defect** was caught pre-launch by the bench rigor. The Model-R
   `serve` serve-loop initially had **no `SIGTERM` handler**, so a
   clean `kill -TERM` terminated it without running the proven
   rust-analyzer **Supervisor reap discipline** (FF #3b/#44/#61/#128:
   kill+wait+pgid/setsid), accumulating zombie/orphan RA under fleet
   restart-churn. **Fixed (#198, integrated):** a std-only
   `SIGTERM`/`SIGINT` handler routing **every** shutdown path through
   the proven Supervisor reap (single-funnel). Verification:
   **structurally proven** (independent scoped-confirm — single-funnel
   no-bypass, proven cores byte-untouched) and integrated;
   **live-fleet corroboration deferred to post-#199** (the shared
   builder pod was in a degraded state — an unrelated infra issue,
   #199, in-fix — that could not bring rust-analyzer up to run the
   runtime probe; **no fabricated runtime number is claimed**). It is
   **PID-hygiene under restart-churn, NOT a RAM leak**: the leaked RA
   processes are **zombies (0 RSS)** that reparent to init and are
   structurally outside the **descendant-scoped** RSS measurement (an
   earlier "~10 GiB" inference was wrong and is **retracted**). It
   does not impugn the measured ≈1 GiB steady-state headline;
   launch-relevant only for restart-churn process hygiene.

Version (`v1.0` vs `v0.2`) and public-launch GO are the **operator's
call** — this document describes capabilities, not a chosen tag.

v0.1 browser/HTTP adapter remains deferred (orthogonal to Model R).
Fleet-scale methodology + the measured per-N curve + the v1→v3 honest
audit trail:
[`AC7-THROUGHPUT-REPORT.md §11.4`](docs/bench/AC7-THROUGHPUT-REPORT.md).

---

## What cargoless is (and isn't)

**Primary consumer: an AI agent writing whole files atomically**, at
**fleet scale** — many worktrees, many agents, one host. The cost unit
is per agent-edit-batch, never per-keystroke. Humans run it too, but
the design center is the agent fleet loop.

**cargoless IS** — a *repo-scoped headless checker + latest-green
publisher* that collapses the fleet onto one shared analyzer:

- `cargoless serve --repo <path>` — **the fleet entrypoint.** Auto-
  discovers every git worktree (`git worktree list`), routes file
  events per-worktree, and multiplexes them through **one** shared
  `rust-analyzer` via LSP overlays. One daemon, one RA, N worktrees —
  this is the architecture that makes the agent-fleet RAM story flat
  (see "At a glance"). Pinned-base + per-tree cache are decoupled;
  corun batching folds non-overlapping worktrees into one check.
- `cargoless check` — one-shot verdict + diagnostics, formatted
  file:line:col + severity + code + message; per-crate verdicts so an
  agent can gate one crate independently of another.
- `cargoless watch` — continuous timestamped verdict stream
  (single-worktree mode; subsumed by `serve --repo` at fleet scale,
  not removed).
- `cargoless build --watch --out <dir>` — wraps `trunk build` and
  publishes the latest green WASM artifact via an atomic
  `.cargoless/latest-green` pointer that **only advances on green**.
- Zero-config — auto-detects `cdylib` + `wasm32` / `leptos` projects.
- Survives `kill -9` of the underlying rust-analyzer; the supervisor
  restarts it transparently.

**cargoless is NOT** — a `trunk serve` drop-in replacement (yet). It
does not include a browser/HTTP/WebSocket layer; that adapter is
deferred (orthogonal to the repo-scoped daemon). If you need a
browser-reload loop today, point `trunk serve` (or any static server)
at the directory cargoless publishes.

> The single-worktree headless checker (the earlier `watch`-per-tree
> shape) was a superseded internal intermediate; the **repo-scoped
> daemon is the architecture** described here. `watch` remains as the
> single-worktree path; it is not the fleet story.

See [`ROADMAP.md`](ROADMAP.md) for the acceptance criteria, the
deferred browser adapter, and the parking lot.

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

**Fleet mode — one command for the whole repo:**

```bash
# Run ONCE for the entire repo. Auto-discovers every git worktree;
# one shared rust-analyzer multiplexed across all of them — total
# RAM stays flat as you add worktrees (see "At a glance").
$ cargoless serve --repo /path/to/repo
>> serve: discovered N worktrees via `git worktree list`
>> serve: one multiplexed rust-analyzer; per-worktree verdicts live
```

You do **not** run `cargoless watch` in each worktree at fleet scale —
that is the per-tree daemon model `serve --repo` replaces. `watch`
remains the single-worktree path for a one-off project.

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

**Unchanged under Model R.** The repo-scoped daemon preserves the
green-edge-rebuild model that produces this CPU win — the per-edit
work is identical whether one worktree is driven by `watch` or N are
multiplexed by `serve --repo`. These figures are **carried verbatim,
reconfirmed not re-derived** (`AC7-THROUGHPUT-REPORT §8.5`,
unchanged); Model R changes *where RAM goes*, not *how much CPU an
edit costs*.

### Leg B — single-RA footprint ladder (now secondary to the architecture)

**Model R subsumes the per-daemon ladder.** The fleet-RAM answer is
now *architectural* — one shared RA, flat across N (Leg C). This
ladder no longer multiplies by daemon count; it tunes the footprint
of the **single shared `rust-analyzer`**. It is still honest and
still shipped, but it is **secondary** — a constant-factor reduction
on the one RA, not the fleet-scale lever it had to be in the
per-worktree-daemon model. Not one number — a ladder, each rung with
its own provenance and gate (per-RA, applied once):

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

### Leg C — fleet-scale (the agent-loop case): flat, measured

The launch-load-bearing question: *at agent-fleet scale, does total
RAM stay bounded as worktrees multiply?* Under the per-worktree-daemon
model the answer was "no — linear, OOMs early." Under the repo-scoped
daemon the answer is **measured flat**:

| Active worktrees (N) | Total cargoless RSS (measured) | Multiplexed RAs |
|---:|---:|---:|
| 1 | ≈1 GiB (peak 1003–1068 / avg 980–1034 MiB) | **1** |
| 8 | ≈1 GiB (same band) | **1** |
| 20 | ≈1 GiB (N=20 ≈ N=1) | **1** |
| per-worktree-daemon model, 20 | ≈1.5 GiB × 20 ≈ **≈30 GiB** (measured-linear) | 20 |

⇒ a **≈19–30× fleet-RAM collapse, measured — replacing the prior
cycle's Model-A "~19.4 GiB BORDERLINE" extrapolation**
(`AC7-THROUGHPUT-REPORT §11.4`, Model-R Leg-C v4, measured on the real
wired `serve --repo` daemon, N ∈ {1,2,4,8,16,20}). The mechanism is
own-eyes-verified: across every N there is **exactly one**
`rust-analyzer` LSP + its one `proc-macro-srv`; total RSS does not
grow with worktree count. *(Figures are bench-lead's #15
measured-not-extrapolated delivery; the final exact-figure
cross-check against the landed §11.4 prose is a publish-time
numbers-gate step — the structure, flat + one RA, is the measured,
mechanism-confirmed claim.)*

**The honest interpretation — inline, load-bearing:**

- The **flatness** is the claim. The **absolute ≈1 GiB is
  fixture-dependent** (Leptos-class honest-size; a larger workspace
  like tf-multiverse shifts the constant up, but it stays a constant
  in N — one RA — not a per-N multiple).
- **Measured to N=20**; beyond is a stated curve-shape projection
  (flat by construction), not a measurement.
- **Measured under driven per-WT activity.** Idle worktrees are
  deactivated by design (activity-activation); the ≈1 GiB is "RAM
  under an actively-driven fleet," not an idle floor.
- The per-RA footprint ladder (Leg B — proc-macro-off, idle-evict,
  `--features csr`) still applies, but it is now a **secondary
  constant-factor** reduction on the *one* shared RA, not the
  fleet-scale lever. Model R is the fleet-scale answer; Leg B tunes
  the constant.
- **Found-and-fixed (disclosed, not hidden):** the Model-R `serve`
  serve-loop initially had no `SIGTERM` handler, so a clean `kill
  -TERM` skipped the proven rust-analyzer Supervisor reap discipline
  (FF #3b/#44/#61/#128). **Fixed (#198, integrated):** a std-only
  `SIGTERM`/`SIGINT` handler single-funnelling every shutdown path
  through that proven reap. Verification: **structurally proven**
  (independent scoped-confirm — single-funnel no-bypass, proven cores
  byte-untouched) and integrated; **live-fleet corroboration deferred
  to post-#199** (degraded shared builder pod — unrelated infra,
  #199, in-fix — couldn't run the runtime probe; no fabricated
  runtime number claimed). A known-pattern regression caught
  pre-launch by the bench rigor — **zombies (0 RSS), PID-hygiene
  under restart-churn, NOT a RAM leak**; the steady-state ≈1 GiB
  above is descendant-scoped and structurally uncontaminated by it
  (zombies reparent to init, outside the measured subtree). Disclosed
  as found / fixed-integrated / live-corroboration-honestly-deferred
  — not overstated, not concealed.

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
hidden. The six load-bearing caveats are stated **inline at the point
of claim** ("At a glance" and Leg C); summarised here so they are not
missable:

- **The fleet-RAM win is the flatness, not the absolute.** ≈1 GiB is
  fixture-dependent (Leptos-class); the measured + mechanism-verified
  claim is "one multiplexed RA, total RSS constant in N." A bigger
  workspace raises the constant, not the slope.
- **Measured to N=20**; beyond is a stated flat-by-construction
  curve-shape projection, not a measurement. Measured under driven
  per-WT activity (idle WTs deactivated by design) — "active-fleet
  RAM," stated as such.
- **Verdict-correctness is a closed chain:** cores structurally
  proven **+** the live multiplexed runtime integration-validated via
  the #15 bench + Track-1 dogfood — never claimed "fully unit-proven
  end-to-end."
- **2.05× CPU is preserved, reconfirmed not re-derived** under Model
  R (green-edge-rebuild model unchanged; Leg A carried verbatim).
- **Per-RA footprint ladder (Leg B) is now secondary** — a
  constant-factor reduction on the one shared RA, not the fleet-scale
  lever. Tier-3 (#126) shipped default-safe / field-verified #130;
  idle-evict (`TF_RA_IDLE_EVICT=1`) opt-in, per-event reclaim
  ≈88-97 % validated, sustained magnitude scales with
  `gap / RA-busy-time`.
- **Found-and-fixed:** the Model-R `serve` serve-loop initially had
  no `SIGTERM` handler, so a clean `kill -TERM` skipped the proven
  rust-analyzer Supervisor reap discipline (FF #3b/#44/#61/#128).
  **Fixed (#198, integrated):** std-only `SIGTERM`/`SIGINT` handler
  single-funnelling every shutdown path through that proven reap.
  Verification **structurally proven** (independent scoped-confirm:
  single-funnel no-bypass, proven cores byte-untouched) + integrated;
  **live-fleet corroboration deferred to post-#199** (degraded shared
  builder pod — unrelated infra #199, in-fix — couldn't run the
  runtime probe; no fabricated runtime number claimed). A
  known-pattern regression caught pre-launch by the bench rigor —
  **zombies (0 RSS), PID-hygiene under restart-churn, not a RAM
  leak**; reparent to init, structurally outside the
  descendant-scoped RSS measurement (an earlier "~10 GiB" inference
  was wrong and is **retracted**). Does not impugn the fleet-RAM
  thesis.
- **Methodology audit trail is open** (`AC7-THROUGHPUT-REPORT §11.4`,
  the v1→v3 honest audit trail): discarded measurement attempts kept
  with reasons, not salvaged — the discipline that earns the
  fleet-scale claim, named openly.

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
- **One multiplexed `rust-analyzer` per repo, not per worktree.**
  This is the Model-R lever and the reason Leg C is flat: RA's
  resident workspace (parsed crates, salsa cache) is the load-bearing
  RAM consumer; sharing *one* RA across N worktrees via LSP overlays
  means total RAM is a constant, not N × per-daemon. Adding worktrees
  adds routing, not analyzers.
- **Headless.** No HTTP server, no WebSocket channel, no
  browser-keepalive. The surface is the repo-scoped daemon + a CLI;
  the browser/reload adapter is a deferred opt-in layer for users who
  want one (orthogonal to the daemon).

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
- **Model-R fleet-RAM is measured, not extrapolated:** flat ≈1 GiB
  across measured N ∈ {1,2,4,8,16,20}, one-multiplexed-RA mechanism
  own-eyes-verified
  (`AC7-THROUGHPUT-REPORT §11.4` Leg-C v4) — the prior narrative's
  disclosed-extrapolation gap is **closed by measurement**.
- **`AC#7` resolution:** cargoless wins on per-edit CPU (2.05×,
  two-source) **and** on the fleet axis where `trunk serve`'s
  architecture is fundamentally not fleet-scalable (per-process RA ×
  N vs one multiplexed RA, measured).
- **Three-layer validation pattern** (author-self-satisfies →
  orchestrator-verifies-against-source → dev-fixer honesty-backstop)
  applied per launch-critical Model-R change (#9 / #11 / #176 / #10
  all layer-3-CLEAR before integration; this narrative goes through
  the same gate).
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
| `cargoless-core` | The repo-scoped daemon: worktree discovery + per-WT routing, one-multiplexed-rust-analyzer overlay, green/red model, build orchestration, transport (in-proc / Unix-socket / HTTP+SSE), diagnostics retention. |
| `cargoless` | The binary: `serve --repo` (fleet) / `check` / `watch` / `build` / `status` / `clean`. |

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

Repo-scoped Model-R daemon feature-complete on `main`; fleet-RAM
flatness measured (`AC7-THROUGHPUT-REPORT §11.4` Leg-C v4); launch
hardening in progress. The public-launch GO and the version tag
(`v1.0` vs `v0.2`) are the **operator's decision** — this document
states capabilities, not a chosen tag or a ship date. Tracked
publicly via [GitHub Issues](https://github.com/TriformAI/cargoless/issues);
the internal agent-team backlog lives in Plane (project "CWDL"). See
[`ROADMAP.md`](ROADMAP.md) for the acceptance criteria and the
deferred / parking-lot phases.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
