# cargoless launch blog post — DRAFT

> **Status:** DRAFT. Per AC#9, this post requires review by **≥2 people
> including one outside the team** before it ships publicly. Do NOT
> publish from this file directly; the operator's chosen venue
> (project blog, dev.to, personal blog, GitHub release notes, etc.)
> gets a frozen copy at publish time.
>
> **Numbers status:** **measured + mechanism-verified** (Model R).
> Headline ≈2.05× per-edit CPU win — **unchanged under Model R**
> (`AC7-THROUGHPUT-REPORT §8.5`, two-methodology Δ≈1%). The
> fleet-RAM story is now the **measured architectural collapse**:
> ≈1 GiB total, flat across N ∈ {1,2,4,8,16,20} active worktrees,
> ≈19–30× below the per-worktree-daemon model, one multiplexed
> rust-analyzer mechanism-verified — `AC7-THROUGHPUT-REPORT §11.4`
> (Model-R Leg-C v4), **replacing** the prior cycle's Model-A
> "~19.4 GiB BORDERLINE" *extrapolation*. The verbatim "~half / ~2 GB / ~75%"
> prose stays as bench-lead's exact wording (it describes the *one
> shared* RA's per-process footprint — still accurate); the
> measured-flat fleet table follows it.
>
> **Positioning:** **fleet-of-any-scale agent-loop substrate**
> (operator-resolved; Model R). The primary consumer is an AI agent
> writing whole files atomically across many worktrees; the cost unit
> is **per agent-edit-batch**, not per-keystroke; the architecture is
> one repo-scoped daemon (`serve --repo`) multiplexing one RA across
> the whole workspace. This composes with the Framing-C
> honest-throughput thesis (CPU win + measured-flat fleet RAM +
> honestly-bounded latency); the human single-tree reading still
> works, but the design center is the agent fleet loop.
>
> **Version & launch GO:** the public-launch decision and the version
> tag (`v1.0` vs `v0.2`) are the **operator's call** — this draft
> describes capabilities, not a chosen tag or ship date.

---

# cargoless: the dev loop that doesn't burn your CPU

Open three terminals. One for `trunk serve`. One for `bacon` or
`cargo-watch`. One for the actual code. Save a file. Listen.

The fans spin up. The battery drops a percentage. Your laptop is now
warm to the touch. `trunk serve` is rebuilding the whole WASM bundle —
again, just like the last time, even though the only thing that
changed was a comment. `bacon` is spawning a fresh `cargo check`
process — again, throwing away the type-graph it computed two saves
ago. Both tools are doing real work; neither is doing **necessary**
work.

This is what we mean when we say cargoless was built to a vision cut:

> **The codebase always knows what works, and tells you the moment it
> doesn't — without burning your CPU to do so.**

cargoless is the headless continuous checker and latest-green
publisher that does *less work per save* than the alternatives, on
the architectural bet that most saves don't change the picture and
shouldn't trigger a full rebuild — and, at agent-fleet scale, that
**one repo-scoped daemon should multiplex a single warm
rust-analyzer across every worktree** instead of paying for one per
tree. The differentiator
isn't sub-second save→verdict (cargo-check dominates that wall-clock,
regardless of who wraps it). The differentiator is **CPU-seconds per
save** — and therefore, over a day of dev cycles, how many edit-cycles
you get out of the same battery.

cargoless is intentionally small: one repo-scoped daemon watches the
whole workspace, keeps a single `rust-analyzer` warm and
LSP-overlay-multiplexed across every worktree, tells you the
green/red verdict on every save, and atomically publishes the latest
known-green WASM artifact to a pointer file. It does not serve a
browser. It does not hot-swap. It does not replace `cargo` or `trunk
build`. Those are deliberate cuts — every type and decision in the
project is justified
by either sharpening the codebase's self-knowledge or shortening the
latency from brokenness to signal. Anything that does neither isn't
here.

---

## The problem nobody benchmarks

A modern Rust+WASM inner loop has three latency dimensions and one
**throughput** dimension nobody talks about:

1. **Save→verdict latency** — how fast do I find out whether the file
   I just saved compiles?
2. **Save→artifact latency** — how fast can the WASM bundle that
   represents my latest green code be served to a consumer?
3. **Verdict→trust latency** — how confident can I be that the
   verdict is right and the artifact is the right artifact?
4. **Throughput: CPU-seconds per save** — how much work is being done,
   per save, that doesn't need to be done?

Existing tools optimize one or two of (1)-(3) and assume (4) away
entirely:

- **`trunk serve`** owns (2) — fast in-browser reload of the latest
  build. But it shells out to `cargo build` on every cycle and serves
  whatever falls out, red or green. CPU cost per save: the full WASM
  rebuild, every time. (3)-cost: serves red builds to your browser
  and to any CI consumer reading `dist/`.
- **`bacon`** owns (1) — fast save→verdict in a terminal. But it
  spawns a fresh `cargo check` per cycle, throwing away the
  type-graph each time. CPU cost per save: a fresh cargo process'
  worth of work. (2)-cost: nothing — there's no artifact.
- **`cargo-watch`** owns a slice of (1). Same fresh-cargo cost; same
  no-artifact result.

For a real Rust+WASM project the day-to-day result is: three terminal
windows, three running cargo-ish processes, fans audibly working,
battery dropping fast. Each tool tells a different fraction of the
truth — the verdict in the corner of your eye isn't an answer; it's
a heuristic you cross-check by hand. And worse, *most of that
ambient CPU cost is paying for work that's already been done.*

## The cargoless architecture: do less, trust more

cargoless picks **(1)+(3)+(4)** as the v0 target. The shape:

**One repo-scoped daemon with a single warm `rust-analyzer`.** Under
Model R cargoless runs *one* daemon per repository (`cargoless serve
--repo <path>`), auto-discovering every worktree via `git worktree
list` and LSP-overlay-multiplexing **one** rust-analyzer across all
of them. RA's multi-second cold start happens **once per repo** — not
once per worktree — and then never again; every save in any worktree
reuses the already-indexed type-graph. `trunk serve` doesn't use RA
at all (it shells out to cargo); `bacon` spawns a fresh cargo process
per cycle. cargoless's warm-LSP architecture is the single biggest
CPU-per-save reduction on the table — and because the one RA is
shared across the whole worktree fleet, it is *also* the structural
fleet-RAM collapse (Leg C).

**Content-addressed dedupe of the full input set.** cargoless hashes
the source tree + `Cargo.lock` + the rust toolchain version + the
target triple + the config — all of it. When you save a file with a
formatting-only change, or you `git checkout` a branch you had open
five minutes ago, the input hash matches a previous cycle's
**and the build is skipped entirely**. Not rebuilt-faster; **not
rebuilt at all**. `trunk serve` and `bacon` rebuild unconditionally,
no matter how identical the input.

**A continuous verdict stream with per-file granularity.** `cargoless
watch` is a long-running command whose stdout is a timestamped
stream of file-level verdicts. You see *which* file went red, not
just *that* the tree went red. The stream is plain text; you can
`grep`, `tee`, pipe into a tmux pane, or have an agent subscribe.

**An atomic `.cargoless/latest-green` pointer.** When a verdict turns
green, the build runs (or hits the CAS and skips), and the
latest-green pointer advances via a temp-file + fsync + rename
pattern. When the tree is red, the pointer **does not move** —
byte-unchanged. Anything reading the pointer (a static server, a CI
step, an agent inspector) is guaranteed never to see a broken
artifact. That's verdict→trust collapsed to zero.

**Verdict↔exit-code coherence.** `cargoless check` exits 0 on green,
non-zero on red, with diagnostics formatted file:line:col + severity
+ code + message. This is the boring corner the project spent the
most field-bug iterations on: the exit code and the printed
diagnostics derive from the same data stream, so you can't get an
"exit 0, but here are 22 errors" outcome.

What this **doesn't** include in v0: a browser, an HTTP server, a
WebSocket reload channel, a hot-swap mechanism, a `trunk build`
replacement. Those are real things people want; they're on the v0.1
roadmap and the v1 parking lot respectively. Shipping them all at
once would have meant launching a half-baked browser layer alongside
a half-baked verdict layer. Better to ship one honest small thing.

---

## Honest performance comparison

The metric that matters for cargoless's positioning is **CPU-seconds
per edit** — the headline throughput number. Latency is a secondary
concern; it's bounded by cargo-check no matter which tool you wrap
around it. The numbers tell a throughput story — and we tell the
memory story honestly, even though it isn't a win.

The honest one-paragraph summary, in bench-lead's own words (unedited):

> cargoless does ~half the per-edit CPU of `trunk serve` — it rebuilds
> on confirmed-green edges, not blindly every keystroke. Memory is
> rust-analyzer-dominated (~2 GB default on proc-macro projects); the
> `--features` knob cuts ~75% and a v0.1 auto-narrow change moves the
> default there.

The CPU half of that is the pitch. The memory half is a thing we
refuse to spin: cargoless keeps a warm rust-analyzer, and RA running
proc-macro expansion is a ~2 GB process whether it's inside cargoless
or inside your editor. cargoless does not make that smaller by
default in v0. The `--features` knob recovers most of it today; the
v0.1 auto-narrow change makes the narrowed config the default. Saying
"low RSS" here would be the kind of selectively-true marketing this
project exists to not do.

That summary is now **two-source-confirmed** for CPU
([`AC7-THROUGHPUT-REPORT §8.5`](https://github.com/TriformAI/cargoless/blob/main/docs/bench/AC7-THROUGHPUT-REPORT.md#85-clean-c2-109--headline-two-source-confirmed),
two independent methodologies that share only the correctness
invariants — Δ≈1%) and refined for memory into the **honest tiered
ladder** below.

### Leg A — per-edit CPU (the headline)

| Tool | CPU-seconds per edit (median) | Two-source verdict |
|---|---:|---|
| **cargoless** | **3.35 s** | TWO-SOURCE-CONFIRMED (3.389 / 3.348, Δ−1.2 %) |
| `trunk serve` | 6.89 s | TWO-SOURCE-CONFIRMED (6.963 / 6.887, Δ−1.1 %) |
| `bacon` † | 0.48 s | TWO-SOURCE-CONFIRMED (0.493 / 0.476, Δ−3.4 %) |

cargoless does **≈2.05× less per-edit CPU than `trunk serve`** —
citable with two-source provenance. † `bacon` is not a like-for-like
comparator: terminal save→verdict checker, not a build+publish loop.

**Unchanged under Model R.** Model R changes how rust-analyzer is
*shared* across the worktree fleet, not the confirmed-green-edge
rebuild model that produces this CPU figure. The 2.05× is re-asserted
from `AC7-THROUGHPUT-REPORT §8.5`, **not re-derived** — the per-edit
CPU headline is Model-R-invariant by construction.

### Leg B — per-RA RAM tiered ladder (secondary under Model R)

Not one number — a ladder, each rung with its own provenance and gate.
Numbers from
[`AC7-THROUGHPUT-REPORT §10`](https://github.com/TriformAI/cargoless/blob/main/docs/bench/AC7-THROUGHPUT-REPORT.md#10-stage-2--per-tier-rss-delta)
(per-tier RSS-delta factorial) +
[`D-RAM-TIERS.md`](https://github.com/TriformAI/cargoless/blob/main/docs/design/D-RAM-TIERS.md).

**Under Model R this ladder is a secondary constant-factor, not the
fleet-scale lever.** It applies to the *one* rust-analyzer the
repo-scoped daemon multiplexes across every worktree — Leg C (the
measured flat fleet-RAM collapse) is the launch-load-bearing memory
story; this ladder then shaves the per-process footprint of that
single shared RA further. The per-rung figures are the per-RA RSS on
the Leptos fixture and are unchanged by Model R (they describe one RA
either way):

| Rung | Per-daemon RSS (Leptos fixture) | What it costs | What it gates |
|---|---:|---|---|
| **default** (Tier-1/2 ON, shipped) | ≈1.71 GiB (≈**−19 %** vs pre-tier 2.12 GiB) | nothing — behaviour-neutral, no opt-in | the universal honest default |
| **+ proc-macro-off** (Tier-3, `TF_RA_PROCMACRO_OFF=1`, shipped default-safe; field-verified on real 38-`view!` Leptos — no false-GREEN) | ≈0.97 GiB (≈**−53 %** vs `AC7-THROUGHPUT-REPORT §5/A2` baseline 2.08 GB) | RA's view of `view!`-style macros; rustc still catches them on the verdict tier | the default RAM rung |
| **+ `--features csr`** (project-narrowable only) | ≈0.53 GiB (≈**−75 %** vs `§5/A2` baseline) + CPU collapse to ≈0.24 s/edit | requires the project to actually be narrowable | the v0.1 auto-narrow default |

**Tier-3 latency observation (n=1 caveat travels):** on the same real
38-`view!` Leptos, proc-macro-off was ≈5× faster to RED (5.1 s vs
25.8 s) — mechanistically expected (proc-macro `view!` expansion sat
on the verdict critical-path; removing it shortened it). We say *"no
latency penalty observed; faster on macro-heavy projects (n=1,
direction unambiguous + mechanistically expected)"* — never an
unqualified "proc-macro-off is faster" universal-speedup claim.

### Leg C — fleet-scale (the agent-loop case): flat, measured

The launch-load-bearing question: *at agent-fleet scale, does total
RAM stay bounded as worktrees multiply?* Under the
per-worktree-daemon model the answer was "no — linear, OOMs early."
Under Model R's repo-scoped daemon — **one** rust-analyzer
LSP-overlay-multiplexed across the workspace-cluster — the answer is
**measured flat**, on the real wired `serve --repo` daemon,
N ∈ {1,2,4,8,16,20}
([`AC7-THROUGHPUT-REPORT §11.4`](https://github.com/TriformAI/cargoless/blob/main/docs/bench/AC7-THROUGHPUT-REPORT.md)
— Model-R Leg-C v4, measured-not-extrapolated, replacing the prior
cycle's Model-A "~19.4 GiB BORDERLINE" extrapolation):

| Active worktrees (N) | Total cargoless RSS (measured) | Multiplexed RAs |
|---:|---:|---:|
| 1 | ≈1 GiB (aggregate peak 1003–1068 / avg 980–1034 MiB) | **1** |
| 8 | ≈1 GiB (same band) | **1** |
| 20 | ≈1 GiB (N=20 ≈ N=1) | **1** |
| per-worktree-daemon model, 20 | ≈1.5 GiB × 20 ≈ **≈30 GiB** (measured-linear) | 20 |

⇒ a **≈19–30× fleet-RAM collapse, measured**; mechanism
own-eyes-verified — exactly one `rust-analyzer` LSP + its one
`proc-macro-srv`, constant across every N.

**The honest interpretation — inline, the narrative's integrity:**

1. **The win is structural, not an absolute.** The absolute
   ≈0.9–1 GiB is fixture-dependent (Leptos-class honest-size); a
   larger workspace yields a *different absolute, same flat-vs-N
   structure*. The claim is *"Model R removes the per-worktree
   multiplication,"* **not** "≈1 GiB on tf-multiverse."
2. **Measured to N=20.** The 589/617-worktree fleet is a
   curve-shape **projection** (one-RA-per-cluster implies it holds),
   stated as projection, never measured, beyond N=20.
3. **Verdict-correctness is a closed chain** — cores structurally
   proven **+** this #15 bench integration-validating the live
   multiplexed runtime — never claimed pure-unit-proven end-to-end.
4. **2.05× CPU preserved, reconfirmed not re-derived** (Leg A
   unchanged; green-edge-rebuild model is Model-R-invariant).
5. **Measured under driven per-WT activity** (idle worktrees are
   deactivated by design — activity-activation); "active-fleet RAM,"
   stated as such, not an idle floor.
6. **Found-and-in-fix (disclosed, named):** the steady-state
   fleet-RAM thesis is measured-confirmed; a separate shutdown defect
   was caught pre-launch by the bench rigor — the proven
   rust-analyzer **Supervisor reap discipline** (FF #3b/#44/#61/#128:
   kill+wait+pgid/setsid) **was not invoked on the new serve-loop's
   `SIGTERM` path**, so a clean `kill -TERM` exited without reaping
   RA. #198 (@`baeac6b`, in-pipeline) structurally restores it
   (every shutdown path routed back through that proven reap). It is
   **zombies (0 RSS), PID-hygiene under restart-churn — NOT a RAM
   leak**; they reparent to init, structurally outside the
   descendant-scoped RSS measurement (an earlier "~10 GiB" inference
   was wrong and is **retracted**). A known-pattern regression caught
   and structurally restored — it does **not** impugn the measured
   ≈1 GiB headline.

The per-RA footprint ladder (Leg B) still applies, but under Model R
it is a **secondary constant-factor** reduction on the *one* shared
RA — no longer the fleet-scale lever it had to be per-daemon.
`TF_RA_IDLE_EVICT=1` remains the opt-in per-RA lever (per-event
reclaim ≈88–97 % validated; sustained magnitude scales with
`gap / RA-busy-time`).

### Latency: two tiers, not one number

Raw save→verdict is reported in **two tiers**
([`D-A2-RENEGOTIATION.md`](https://github.com/TriformAI/cargoless/blob/main/docs/design/D-A2-RENEGOTIATION.md)):
a **RA-incremental hint** (AC#2a — median ≤1 s, ≈0.74 s
field-measured; can flip RED instantly, does not by itself prove
compilation) and the **authoritative cargo-check verdict** (AC#2b —
bounded by `cargo check`; seconds on small projects, ≈20-30 s on a
Leptos-sized tree; the only tier that drives GREEN). cargoless shows
both, live, with timestamps — the latency gap is readable directly
off any pair of lines, not hidden.

On a real Leptos project the **authoritative** verdict (AC#2b) lands
within the cargo-check-bound band for all three tools — none of them
are racing each other on raw wall-clock there, because cargo's own
runtime dominates. cargoless's only latency edge is the AC#2a hint
tier, which the other two don't have at all; it shows that hint live
without pretending it's the verdict. **The interesting wins are on the
throughput rows.** When the input set hasn't changed, cargoless does
~zero work; the other two redo the full build's worth of work
because they don't know it's already been done. Over a day of
dev-cycles, that compounds — into measurable battery life, into
measurable fan-noise, into measurable thermal headroom for the rest
of your local stack.

**Unchanged under Model R.** LSP-overlay-multiplexing one
rust-analyzer across the worktree fleet does not change the
save→verdict tiering — the AC#2a RA-incremental hint / AC#2b
authoritative cargo-check split is **re-asserted, not re-derived**;
the dual-tier latency story is Model-R-invariant.

**Methodology** (because numbers without methodology are decoration):

- **Bench fixture:** a real Leptos CSR `cdylib + rlib` project, ≥17
  files / 922 LOC. The bench harness refuses to shrink the fixture
  for flatter numbers.
- **Throughput measurement:** CPU-seconds + peak RSS sampled from
  the OS over a full edit session (N saves at fixed interval,
  including a mix of substantive and no-op edits to exercise the
  CAS-skip path). Reported as per-save median and peak. Driver
  records `getrusage()` deltas around each save event.
- **Two-mode latency reporting:** checker mode (save→verdict) and
  artifact mode (save→publish) reported **separately**, never
  blended into a single "median latency" number. cargoless makes no
  sub-second artifact-publish claim.
- **Identical driver across tools:** the same save events on the
  same fixture feed cargoless, `trunk serve`, and `bacon`. Wall-clock
  measurements use a monotonic clock.
- **Independent cross-check:** a second host runs the same harness
  against the same fixture to confirm the numbers reproduce off the
  primary builder pod.
- **Reproducible:** the harness lives at `bench/run.sh` in the repo;
  rerun it on your machine and tell us if you see different numbers.

The full report and verdict commit-status live at `s1-ac2-verdict`
and `ac7-verdict` keys on the release SHA.

---

## Install and try it

> **Pre-release.** The release-tagged install commands below will work
> once `v0.1.0` is cut. Today, only the from-source GitHub install is
> supported — and it's been smoke-tested end-to-end on a clean Linux
> environment.

```bash
cargo install --git https://github.com/TriformAI/cargoless.git \
              cargoless --branch main --locked
```

Once `v0.1.0` ships, the install path becomes:

```bash
# Source build (universal)
cargo install <pubname>            # <pubname> = TBD per D1

# Prebuilt via cargo-binstall
cargo binstall <pubname>           # Linux x86_64-gnu + macOS aarch64/x86_64
```

Then, in any Rust+WASM project (auto-detected on `cdylib + wasm32` or
`leptos`):

```bash
$ cargoless check
ok green — every tracked file compiles

$ cargoless watch
>> [+   0.083s] daemon up, watching /work/my-app
>> [+   0.741s] /work/my-app/src/lib.rs: Green
^C
```

For the agent fleet — **one** daemon for the whole repository, every
worktree auto-discovered and multiplexed onto a **single**
rust-analyzer (this is the Model-R path the fleet-RAM headline
measures):

```bash
$ cargoless serve --repo /work/my-repo
>> [+   0.091s] repo daemon up — 1 rust-analyzer, discovering worktrees
>> [+   0.880s] /work/my-repo/wt-a/src/lib.rs: Green
>> [+   1.012s] /work/my-repo/wt-b/src/lib.rs: Green
^C
```

For the build/publish loop (requires the upstream `trunk` for the
WASM artifact step):

```bash
$ cargoless build --watch --out ./dist
>> publishing latest-green to .cargoless/latest-green → ./dist
ok green — latest-green @ <hash>
```

Edit a file with a real error. The next verdict line tells you
exactly what broke and where. Fix it; the latest-green pointer
advances. Introduce a syntax error; observe that the pointer **does
not move** until you fix it. That's the whole pitch in 30 seconds of
demo.

---

## Roadmap

cargoless is phased **launch → next → parking lot**. The public
version tag (`v1.0` vs `v0.2`) is the operator's call; this post
describes capability, not a chosen tag.

**The launch (today):** the headless continuous checker +
latest-green publisher, delivered as **one repo-scoped daemon**
(`cargoless serve --repo`) multiplexing a single warm rust-analyzer
across the whole worktree fleet (Model R). No browser, no HTTP, no
WebSocket. Nine acceptance criteria, field-verified on a real Leptos
project; the fleet-RAM collapse measured to N=20. The earlier
per-worktree-`watch` daemon was a superseded internal intermediate —
never publicly launched; Model R subsumes it (single-tree `watch`
stays as a documented convenience, not the architecture). This is the
launch story.

**v0.1 (next, deferred — no date commitment):** the optional live
HTTP/WebSocket dev-server that consumes cargoless's `.cargoless/latest-green`
pointer and full-reloads a browser when it advances. Trunk-compatible
reload protocol. Browser holding page during cold starts. **This is
what closes the `trunk serve` drop-in gap**, and the research
implementation already exists on a branch. We're shipping the
checker + publisher first so that the launch promise (verdict +
publish, honest) lands clean rather than buried under a half-finished
browser layer.

**v1 (parking lot — no commitments):** salsa / rust-analyzer-as-library
deep integration, remote shared CAS, team features + auth, multi-agent
build coordination, editor LSP plugin, symbol-level verdict
granularity, replacing `trunk build` internals, hot-swap WASM, CI
integration, Windows support. These are the ideas that earn their own
design pass if and when v0 / v0.1 prove the foundation.

Roadmap details: [`ROADMAP.md`](https://github.com/TriformAI/cargoless/blob/main/ROADMAP.md)
in the repo.

---

## What we are honest about

A launch is the moment to set up trust by being explicit about what
we deliberately did **not** do:

- **No browser in v0.** If you want browser-reload today, point
  `trunk serve` at the directory cargoless publishes; v0.1 closes
  the gap.
- **Not a `trunk build` replacement.** cargoless wraps `trunk build`
  for the WASM artifact step — `trunk` is doing the actual cargo +
  wasm-bindgen work; cargoless drives the watch and publish loop on
  top.
- **One repo-scoped daemon, single-machine (the agent fleet runs on
  it — see Leg C).** Remote CAS, shared caches, team auth are v1. The
  agent-fleet case the launch headline addresses is **one**
  `cargoless serve --repo` daemon multiplexing **one** rust-analyzer
  across N agent-driven worktrees on one host — *not* N independent
  daemons, and not a coordinated multi-agent build system. The
  shared-daemon multiplexing is exactly what makes the fleet RAM stay
  flat (Leg C).
- **Linux + macOS only.** Windows is v1 parking-lot per the design
  doc; `cargo install` works there on a best-effort basis but no
  prebuilt artifact, no CI coverage.
- **Memory is a measured structural story plus a secondary ladder,
  never one headline number.** At fleet scale the win is the
  *measured flat collapse* — one shared rust-analyzer, ≈1 GiB total
  flat across N ∈ {1,2,4,8,16,20} active worktrees, conditions cited
  inline (Leg C, `AC7-THROUGHPUT-REPORT §11.4`). cargoless still does
  not magically make a single RA small: the per-RA tier ladder
  (default ≈−19 % / Tier-3 ≈−53 % / `--features csr` ≈−75 %, per
  `AC7-THROUGHPUT-REPORT §5`/§10) is a **secondary constant-factor on
  that one shared RA**, no longer the fleet-scale lever it had to be
  under the old per-worktree-daemon model. Quoting the flat ≈1 GiB
  without its fixture-dependence caveat over-sells; quoting only the
  per-RA ladder under-sells. Both, with their gates, is the honest
  framing. Tier-3 is *shipped default-safe* (no flag needed;
  field-verified on real Leptos, no false-GREEN).
- **The fleet-RAM answer is measured to N=20, projected beyond — and
  one shutdown defect was caught pre-launch.** The ≈1 GiB-flat fleet
  figure is *measured*, not extrapolated, on the real wired `serve
  --repo` daemon at N ∈ {1,2,4,8,16,20} (`AC7-THROUGHPUT-REPORT
  §11.4`, Model-R Leg-C v4 — this **replaces** the prior cycle's
  Model-A "~19.4 GiB BORDERLINE" *extrapolation*). The 589/617-worktree
  fleet is a stated **projection** of the measured flat-vs-N
  structure (one-RA-per-cluster implies it holds), never claimed
  measured beyond N=20. Separately, the bench rigor caught a shutdown
  defect pre-launch: the proven rust-analyzer **Supervisor reap
  discipline** (FF #3b/#44/#61/#128: kill+wait+pgid/setsid) was **not
  invoked on the new serve-loop's `SIGTERM` path**, so a clean `kill
  -TERM` exited without reaping RA. #198 (@`baeac6b`, in-pipeline)
  structurally restores it (every shutdown path routed back through
  that proven reap). It is **zombies (0 RSS), PID-hygiene under
  restart-churn — NOT a RAM leak** (they reparent to init, outside
  the descendant-scoped RSS measurement; an earlier "~10 GiB"
  inference was wrong and is **retracted**). A known-pattern
  regression caught and structurally restored — it does not impugn
  the measured ≈1 GiB headline.
- **Idle-evict's 5 % sustained reduction is a workload-conservative
  floor, not a ceiling.** Mechanism (≈88–97 % per-event reclaim) is
  fully validated; sustained magnitude scales with `gap / RA-busy-
  time` (Leptos RA's 65-70 s re-index in a 75 s gap is the bench
  shape; real minute-scale agent-think-gaps shift it favorably).
  Default-off in v0; opt-in via `TF_RA_IDLE_EVICT=1`.
- **The benchmark is HONEST on raw speed.** We measured, the first
  passes had methodology bugs, we caught them via physical-
  impossibility sanity checks, we documented the audit trail
  (`AC7-THROUGHPUT-REPORT §11.4`, the v1→v3 honest audit trail —
  discarded measurement attempts kept with reasons, not salvaged).
  The
  save→verdict story is the honest dual-tier split (RA-hint ≤1 s +
  cargo-check-bound authoritative), not a single sub-1s headline;
  the throughput win is two-source-confirmed at ≈2.05× per-edit CPU
  vs `trunk serve`. Honest beats favorable — here, with the methodology
  bugs fixed, honest IS favorable.
- **The structural-trigger is a correctness property, not a v0 CPU
  win for the agent-write population.** Dogfood #117 (survivorship-
  free, N=16) found `.rs` OPEN ≈0 % for Claude `Write` — agents emit
  complete whole-file Rust. The trigger's value is *only-meaningful-
  states-cached* (it guarantees we never cache a half-written state)
  and the *conditional* benefit for fleets that do emit OPEN
  intermediate Rust. We do not claim it as a check-skip headline.

The launch-hardening process for this v0 was 12 field findings over
3 weeks of dogfooding a real Leptos project on a clean Linux box; 11
fixed before launch, 1 closed as a design question (`cargoless clean`
semantics — non-breaking, safe-either-way). The full evidence trail
is at [`docs/dogfood/PHASE-2-REPORT.md`](https://github.com/TriformAI/cargoless/blob/main/docs/dogfood/PHASE-2-REPORT.md).

For the launch fortnight, the maintainers commit to a 48-hour
acknowledgement window on every new issue and PR. After that, the
sustainable cadence is one-week acknowledgement, with launch-blocker
urgency preserved for verdict-honesty / never-publish-red / install
regressions. The commitment is in [`CONTRIBUTING.md`](https://github.com/TriformAI/cargoless/blob/main/CONTRIBUTING.md);
a missed acknowledgement is itself a GitHub issue.

---

## Acknowledgments

cargoless stands on the work of dozens of upstream Rust projects.
The ones we want to call out specifically:

- **Leptos** — for the substrate cargoless's defaults target. The
  Leptos project's tight `view!` macro / signal / control-flow shape
  is what made the auto-detection design feel inevitable.
- **rust-analyzer** — for the warm-LSP architecture that cargoless's
  whole verdict loop sits on top of. Every save→verdict measurement
  in this post is a measurement of how fast rust-analyzer can give us
  a diagnostic when poked correctly.
- **`trunk`** — for the WASM build pipeline cargoless wraps. v0.1
  also intends to be wire-compatible with Trunk's browser-reload
  protocol so the migration cost is one config line, not a rewrite.
- **`bacon`** — for proving that a tight save→verdict loop *matters*
  to Rust developers. cargoless's verdict stream is different from
  bacon's in form but identical in spirit.
- **The cargo and rustc teams** — for the workspace + Cargo.lock +
  build-cache infrastructure that makes deterministic Rust builds
  possible at all.

The agent team that built cargoless is documented in
[`.claude/`](https://github.com/TriformAI/cargoless/tree/main/.claude)
metadata — the team config, the per-role prompts, the build/test
discipline. Anyone curious about LLM-driven OSS engineering at this
scale is welcome to look. Outside maintainer review of this launch
post is requested and gratefully received.

---

## Try it, file what breaks, tell us what's missing

```bash
cargo install --git https://github.com/TriformAI/cargoless.git \
              cargoless --branch main --locked
```

Repository: [github.com/TriformAI/cargoless](https://github.com/TriformAI/cargoless)
Issues: [github.com/TriformAI/cargoless/issues](https://github.com/TriformAI/cargoless/issues)
Discussions: [github.com/TriformAI/cargoless/discussions](https://github.com/TriformAI/cargoless/discussions)

If something surprises you, that's a finding the launch sequence
wants to hear about. Verdict→trust only works if the people running
the tool report when it breaks.

---

## Appendix — reviewer checklist (AC#9)

> **Model-R-reshaped checklist.** This appendix is **not** stripped in
> the #164 narrative-finalization edit — it is removed in the
> *publish-time* edit (`AC9-REVIEWER-PACKET §3.3`), after the ≥2
> reviewers (one outside the team) use it for AC#9 sign-off. The items
> below are reshaped for the Model-R narrative (measured flat fleet
> RAM, repo-scoped daemon) so reviewers check the *current* claims.

- [ ] Headline value-prop matches the operator-locked **Framing C**
      ("cargoless doesn't burn your CPU" — throughput-not-speed) and
      the throughput-first comparison tables are intact; positioning
      reads as **fleet-of-any-scale agent-loop substrate** (Model R),
      not a human trunk-serve replacement.
- [ ] Every Model-R fleet-RAM figure (≈1 GiB flat, ≈19–30× collapse,
      N ∈ {1,2,4,8,16,20}) traces to **`AC7-THROUGHPUT-REPORT §11.4`**
      (Model-R Leg-C v4) — **NOT** a `D-FLEET` design estimate, **NOT**
      an extrapolation. No `_PENDING_` cell remains; no FILLED→SOURCE-
      DRIFT across occurrences (every recurrence cites §11.4).
- [ ] Verbatim "~half / ~2 GB / ~75%" prose kept char-for-char as
      bench-lead's wording, framed as the *one shared RA*'s per-process
      footprint (reconciled, not silently "improved").
- [ ] Throughput numbers (CPU-seconds per edit, peak RSS) intact;
      methodology paragraph matches the actual bench shape. Leg A
      2.05× per-edit CPU **re-asserted "unchanged under Model R", not
      re-derived** (`AC7-THROUGHPUT-REPORT §8.5`).
- [ ] Memory honesty intact: the fleet story is the **measured
      structural flat collapse** (one shared RA, ≈1 GiB, conditions
      cited inline) with its **fixture-dependence + measured-to-N=20 /
      projected-beyond** caveats present and not softened; the per-RA
      tier ladder (≈−19 % / −53 % / −75 %) is framed as a **secondary
      constant-factor**, no "low RSS by default" claim anywhere.
- [ ] FIELD FINDING A disclosed with the **accurate mechanism**: the
      proven rust-analyzer **Supervisor reap discipline** (FF
      #3b/#44/#61/#128) not invoked on the new serve-loop `SIGTERM`
      path; #198 (@`baeac6b`) restores it; zombies/0-RSS/PID-hygiene,
      **NOT a RAM leak**. **No `#183`/GracefulShutdown mis-attribution;
      the retracted "~10 GiB" figure does not appear.**
- [ ] Latency presented as the **dual-tier split** (AC#2a hint ≤1s /
      AC#2b cargo-check-bound authoritative), not a single sub-1s
      headline; no sub-second artifact-publish claim; re-asserted
      **"unchanged under Model R", not re-derived**.
- [ ] `cargoless serve --repo` command strings match the shipped #3
      CLI surface; the single-tree `watch` path is retained (Model R
      subsumes, does not remove it).
- [ ] **No "ship v0" / "v0 launch" residual** — Model A
      (per-worktree-`watch` daemon) is a superseded internal
      intermediate, never publicly launched; the launch is Model R.
      Public version tag (`v1.0` vs `v0.2`) left as the operator's
      call (no tag asserted in copy).
- [x] D1 product name resolved = `cargoless` (operator, 2026-05-17);
      `tftrunk`/`tf-cli` drift renamed to `cargoless` in the #87
      surgical rename-commit; internal crates renamed to
      `cargoless-proto`/`cargoless-cas`/`cargoless-core` in #97
      (post-#97 full one-token brand on `main`); D1-completeness
      CI-enforced forward by `scripts/d1-drift-guard` (#96).
- [ ] Install command verified to work in a clean environment on the
      target platforms (Linux x86_64, macOS aarch64, macOS x86_64).
- [ ] Repository URL audited; no `forgejo.triform.dev` URLs in
      contributor-facing copy (those are internal-CI only).
- [ ] `docs/dogfood/PHASE-2-REPORT.md` link verified live on GitHub.
- [ ] `ROADMAP.md` link verified live on GitHub.
- [ ] `CONTRIBUTING.md` link verified live on GitHub.
- [ ] Tone read: no marketing fluff, no unsupported speed claims,
      every promise traceable to an acceptance criterion or a cited
      measurement.
- [ ] Outside reviewer name + sign-off recorded in commit message of
      the publish-time edit.
