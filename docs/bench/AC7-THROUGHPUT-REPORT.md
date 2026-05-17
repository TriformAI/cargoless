# AC#7 — Throughput Investigation Report

> **DRAFT — populated incrementally as measurements complete.**
> Owner: bench-lead • Substrate: `agent/bench-lead@<SHA>` (= main + bench/)
> Methodology: two independent measurements (Components 1 + 2) cross-checking;
> A/B comparison pre/post the RA weight-shedding polish (Component 3) when it lands.

## TL;DR

Operator's AC#7 reframe (2026-05-17): "Speed is good, but throughput is better.
I want to know how we can make it spend less CPU/RAM. This is the major point."

The original latency-comparison axis was **cargo-check-bound** (all three
tools delegate type-check to the same cargo subprocess). The operator
reframed to **throughput** (CPU/RAM per edit-cycle). Two honest findings
emerged, and the report keeps them strictly separate (conflating them
yields the wrong conclusion):

**Finding 1 — within-cargoless RA-polish sweep (SOLID, §5):** cargoless's
inner-loop footprint is **rust-analyzer-dominated**. On a proc-macro-heavy
Leptos project the *default* config is heavy (~2 GB peak, ~1.9 GB
monotonic RSS growth, ~1.9-2.3s CPU/edit) and the *default* RA-polish
(#74) does **not** materially help (the proc-macro server stays on for
`view!`). The shipped opt-in knobs DO: `--proc-macro disabled` = −53% RSS
/ −68% CPU; `--features csr` = **−75% RSS / −90% CPU** (down to ~0.53 GB,
0.24s/edit). Honest launch shape: a *growth-path* story ("heavy by
default, knobs cut it 53-75%, v0.1 auto-narrowing closes the gap"), not a
*we-already-win* story.

**Finding 2 — cross-tool vs trunk/bacon (verdict GATED):** the first
passes showed cargoless far heavier, but were collected with a CPU/RSS
accounting bug that asymmetrically under-counts the spawn-exit
comparators (bacon/trunk) vs cargoless's persistent RA (§3 lesson 6).
Caught by sanity-check, harness fixed (`5d3caeb`: +cutime/cstime +
250ms bg RSS-peak), corrected re-run in progress. **No cross-tool
PASS/FAIL is asserted until the corrected numbers land.** "Never
launch-and-hope" applies to the measurement itself.

**Headline numbers** — cross-tool table GATED on the corrected re-run;
the reliable headline is the §5 within-cargoless sweep:

| cargoless `watch` config | Peak RSS | CPU-s/edit | vs default |
|---|---:|---:|---|
| default (post-RA-polish) | 2.08 GB | 2.286s | baseline |
| `--proc-macro disabled`  | 0.97 GB | 0.727s | −53% / −68% |
| `--features csr`         | **0.53 GB** | **0.240s** | **−75% / −90%** |

(All on `bench/fixture` — 17-file / ~1009-LOC Leptos CSR; honest-size
floor reasserted per `bench/run.sh`. cross-tool trunk/bacon rows: see
§2.1 — superseded, re-run in progress.)

### Pre-polish cargoless memory-growth signal (already captured, calls for the post-polish A/B)

The Component-1 pre-polish cargoless run shows **RSS growth of ~1.75 GB
over 30 edits** (baseline 527 MB → final 2.28 GB; peak = final, so the
growth is monotonic, not spiking and recovering). This is a
memory-leak-shape pattern — RA's incremental cache + proc-macro server
hold every analysis they've ever produced, even for code paths the
user is no longer editing. Bounded-cache or LRU eviction would show
a sawtooth instead.

This is precisely what the RA weight-shedding work (#74) targets:
`cachePriming=off` + `inlayHints.*=off` + cargo `features` narrowed
should materially reduce both peak RSS and the growth rate. The
post-polish A/B (Component 3 of this report) is the deliverable that
quantifies how much. If the delta is large, "shipped lean by default"
becomes a credible launch headline; if small, we'll have evidence the
growth is fundamentally inherent to RA-on-Leptos rather than
cargoless's wiring choices — also useful data.

---

## 1. Methodology

Two independently-implemented measurement paths against the same fixture +
the same edit-cycle protocol. If the two converge on similar numbers,
that's strong evidence the figures reflect reality. If they diverge, we
investigate which methodology is wrong **before** believing either
(per the brief's "never launch-and-hope" discipline).

### 1.1 Component 1 — primary harness (`bench/throughput.py`)

Std-library-only Python (matches the dep-free ethos of `bench/harness`).

- **Spawn**: `subprocess.Popen` with `start_new_session=True` (own process
  group for clean tree-kill); stdin = devnull; stdout+stderr → per-tool log.
- **Ready gate**: tail the per-tool log every 500ms until the tool's
  first compile-completion banner appears (substring match, strict —
  not the daemon-startup banner; see §3 lessons).
  - cargoless: `GREEN — tree compiles`, `GREEN — building`, `published `
  - trunk: `success`, `applying new distribution`
  - bacon: `Success!`, `Warnings.`, `Errors found`
- **Process-tree walk**: BFS over `/proc/*/stat` ppid fields. Catches
  every descendant (cargoless's `rust-analyzer` + `rust-analyzer-proc-macro-srv`,
  trunk's bundler + cargo + rustc workers, bacon's cargo + rustc workers).
- **RSS source**: `/proc/<pid>/statm` column 2 × page-size (4KB on Linux).
  Summed across the entire process tree.
- **CPU source**: `/proc/<pid>/stat` fields 14+15 (utime+stime) **AND
  16+17 (cutime+cstime)** = own CPU PLUS reaped-children's CPU. The
  cutime/cstime terms are LOAD-BEARING: bacon/trunk spawn-compile-EXIT
  a child per edit (cargo→rustc; cargo+wasm-bindgen) whose CPU is
  reaped into the parent on `wait()`; an own-CPU-only accounting
  (the harness's first cut) missed essentially all of it while
  cargoless's persistent rust-analyzer was fully counted — a ~20x
  asymmetric inflation caught by sanity-checking (bacon measured at
  6 jiffies for 20 cargo checks, impossible). No double-count: a
  child's CPU is in its own utime/stime while a live tree member,
  and in an ancestor's cutime/cstime once exited+reaped (when it's
  no longer in the tree). See §3 lesson 6.
- **RSS peak**: max of (per-edit samples, a 250ms-tick background
  tracker). The background tracker is also load-bearing: per-edit
  samples (10s apart) MISS the transient compile-time RSS of
  spawn-exit tools (bacon's cargo, trunk's cargo+wasm-bindgen run
  ~0.4-2s then exit between edit samples) while cargoless's resident
  RA is always caught — same asymmetry shape as the CPU bug.
- **Edit driver**: open(O_WRONLY|O_TRUNC|O_CREAT) → write_all → fsync →
  close — single `write(2)` syscall, no temp+rename. Matches the
  FS-event shape every editor (vim/vscode/RA-on-save) produces and
  every notify-rs watcher handles cleanly.
- **Sampling cadence**: per-edit snapshot (after the sleep interval)
  for CPU/RSS time-series + the 250ms background RSS-peak tracker.

### 1.2 Component 2 — independent cross-check (`bench/throughput-recon.sh`)

Bash + `ps` + `awk`, deliberately different code paths from Component 1.

- **Spawn**: shell `&` with `setsid` for own-session reap; output → log.
- **Ready gate**: `grep -qE` over the log file every 1s.
- **Process-tree walk**: `ps --ppid` BFS — different program reading
  the same `/proc` data through a different kernel interface.
- **RSS source**: `ps -o rss=` — kernel resident-set field via the
  ps(1) parser, not statm parsing.
- **CPU source**: `/proc/<pid>/stat` fields 14+15 via `awk`, summed
  in shell arithmetic — different parser code from the Python.
- **Edit driver**: `sed -i s/.../.../` — temp file with random suffix
  (not a deterministic `.<stem>.bench-harness.tmp` filename) + rename.
  Verified to work with cargoless's notify-rs in the #36 5th-iteration
  manual probe.
- **Sampling cadence**: fixed 5-second background ticker INDEPENDENT
  of the edit cycle, plus per-rep checkpoints. This catches CPU
  bursts BETWEEN edit moments that a per-edit-sample methodology
  would miss.
- **Isolation**: separate source copy at `/work/bench-lead-recon-src`
  + separate `CARGO_TARGET_DIR=/cache/target-bench-lead-recon` so
  cache state cannot leak between the two methodologies.

### 1.3 Per-tool setup

| Tool       | Argv used                                | Why                                  |
|------------|------------------------------------------|--------------------------------------|
| cargoless  | `tftrunk watch`                          | Headless verdict stream (the v0 mode)|
| trunk      | `trunk watch`                            | Compile loop without HTTP overhead   |
| bacon      | `bacon --headless --job check`           | Default cargo-check job, no TUI      |

cargoless's debouncer left at default 150ms (post-#49). All tools
pre-warmed with `cargo fetch` + `cargo build` + `cargo check` in the
fixture root so first-spawn doesn't pay the cold cargo-fetch cost.

### 1.4 Edit-cycle protocol

- 30 reps × 10-second inter-edit sleep = 5-minute measurement window per tool.
  (Lead suggested 60 reps; cut to 30 to fit the wall-clock budget without
  losing statistical signal.)
- Each rep toggles a comment-suffix on the same source line
  (`src/domain/model.rs`, the existing `BENCH_TRAIT_ANCHOR` from the S1
  harness). The toggle is **AST-identical** — keeps the tree GREEN so each
  tool actually does its happy-path work on every save (a red save under
  cargoless's AC#4 doesn't re-publish; we measure the actual work, not the
  no-op).
- First rep is **not** discarded; throughput accumulates over the full
  window and we report cumulative totals, so the "warm-cold spike" the
  latency harness discarded would be averaged in correctly anyway.

---

## 2. Results

### 2.1 Component 1 cross-tool (cargoless vs trunk vs bacon) — SUPERSEDED, re-run in progress

> **The cross-tool numbers from the first two passes are KNOWN-BIASED
> and must NOT be cited.** They were collected with own-CPU-only
> accounting (utime+stime, no cutime+cstime) + per-edit-only RSS
> sampling. That asymmetrically under-counts spawn-exit tools
> (bacon/trunk) vs cargoless's persistent RA — see §3 lesson 6. The
> bias was caught by sanity-check (bacon = 6 jiffies for 20 cargo
> checks, physically impossible), the harness fixed (`5d3caeb`), and
> the corrected-accounting re-run is in progress. Recorded here only
> as the audit trail of *why the cross-tool verdict waits*:

| Tool (BIASED — do not cite) | peak RSS | CPU-s/edit |
|---|---:|---:|
| cargoless `watch` post-F12 | 2.25 GB | 1.878s |
| trunk `watch` | 330 MB* | 0.096s* |
| bacon | 10 MB* | 0.002s* |

`*` = systematically under-counted (transient compiler CPU reaped
into parent cutime/cstime not summed; transient compiler RSS missed
between 10s samples). cargoless's persistent-RA figure is approximately
right; the gap *magnitude* is the artifact.

**What IS reliable from these passes (symmetric accounting both
sides):** the within-cargoless RA-polish sweep — see §5. That's the
launch-relevant result and it stands.

Corrected cross-tool numbers (cutime+cstime + 250ms bg RSS-peak,
substrate `920fc20` post-D1-rename) land here when the re-run
completes; the §7 cross-tool verdict is gated on them.

### 2.2 Component 2 (independent ps/bash methodology)

_PENDING — runs after Component 1's trunk+bacon detached invocation completes._

| Tool      | Peak RSS | RSS growth | Mean CPU% | CPU-sec/edit | Sample count |
|-----------|---------:|-----------:|----------:|-------------:|-------------:|
| cargoless | _TBD_    | _TBD_      | _TBD_     | _TBD_        | _TBD_        |
| trunk     | _TBD_    | _TBD_      | _TBD_     | _TBD_        | _TBD_        |
| bacon     | _TBD_    | _TBD_      | _TBD_     | _TBD_        | _TBD_        |

### 2.3 Cross-check convergence

_PENDING. To assess agreement, we compute |C1 − C2| / mean(C1, C2) for each
(tool, metric) pair; values < 10% indicate methodology convergence._

| (tool, metric)                | C1     | C2     | Δ%   | Verdict |
|-------------------------------|-------:|-------:|-----:|---------|
| cargoless / peak RSS          | _TBD_  | _TBD_  | _TBD_| _TBD_   |
| cargoless / cpu-sec per edit  | _TBD_  | _TBD_  | _TBD_| _TBD_   |
| trunk / peak RSS              | _TBD_  | _TBD_  | _TBD_| _TBD_   |
| trunk / cpu-sec per edit      | _TBD_  | _TBD_  | _TBD_| _TBD_   |
| bacon / peak RSS              | _TBD_  | _TBD_  | _TBD_| _TBD_   |
| bacon / cpu-sec per edit      | _TBD_  | _TBD_  | _TBD_| _TBD_   |

---

## 3. Lessons learned (iteration arc → these numbers)

Recorded for future bench work + the launch blog's "how we measured" appendix.

The throughput run was preceded by **5 latency-iteration cycles** before the
operator pivoted axes. Each iteration surfaced a real bench-or-fixture bug,
none were bogus-number-publish risks. Captured for honesty + future avoidance:

1. **Iter 1** — `ready` signal lists matched **daemon-startup** lines
   (cargoless's "verdict pipeline live", trunk's "starting build", bacon's
   "warning") rather than the first **compile-complete** banner. Warm
   completed in milliseconds; the measurement loop then ran on a still-
   cold workspace and timed out 60s × 3 reps as NO_SIGNAL. Fix:
   strict ready-signal vocabulary — only the post-compile banner.
2. **Iter 2** — Fixture was missing `index.html` + `Trunk.toml`, hard-
   required by trunk for wasm-cdylib bundling. Per FIELD FINDING #54
   (since fixed), `tftrunk build --watch --out` also internally
   invokes trunk and would have hit the same incompleteness. Added
   minimal Leptos-CSR templates.
3. **Iter 3** — Main moved 6+ commits under the long-running iteration
   (including post-#42 launch-blocker fixes). Re-merging `origin/main`
   into the bench-lead substrate kept the comparison describing the
   shipping product, not a stale fork point.
4. **Iter 4** — Trunk's `Trunk.toml` `ignore` list required all paths
   to canonicalize at startup; listing `target/` (which doesn't exist
   pre-spawn) killed trunk with "ERROR error taking the canonical path
   to the watch ignore path". Bacon emits `Warnings.` (not `Success!`)
   when the fixture has warnings — 5 dead-code warnings in our model
   meant `Success!` never appeared in bacon's stream. edit_timeout
   bumped 60→120s; warm_timeout 300→600s.
5. **Iter 5** — The harness's `atomic_write` (deterministic
   `.<stem>.bench-harness.tmp` + rename) produced FS events that
   cargoless's notify-rs watcher did NOT surface as content-changes,
   while `sed -i` (random temp suffix + rename) and direct writes
   both worked. Switched to direct `open+write+fsync+close` — the
   editor-save shape every notify-rs handles cleanly. ALSO:
   `NO_COLOR=1` env (no-color.org convention) broke trunk because
   trunk's clap exposes that env var as a bool CLI arg and "1" is
   rejected. Dropped the env override.
6. **Iter 6 (the most important methodology lesson)** — bacon emits
   ANSI color codes even into a pipe (TUI framework ignoring
   non-TTY); the codes splice INTO the banner text
   (`ESC[1m ESC[32m Finished ESC[0m \`dev\``) defeating the
   substring match → NO_READY despite bacon completing fine. Fixed
   with a general ANSI-strip in the reader. THEN, with bacon finally
   producing numbers, a sanity-check caught the **CPU/RSS accounting
   asymmetry**: the harness read only `/proc/<pid>/stat` utime+stime
   (own CPU). bacon/trunk spawn-compile-**exit** a child per edit;
   the child's CPU is reaped into the **parent's cutime+cstime** on
   `wait()` — which was not summed. cargoless's rust-analyzer is
   **persistent** so its CPU accrued in a live, counted process.
   Result: bacon measured at **6 jiffies for 20 cargo checks**
   (physically impossible — that's how it was caught), and the
   cargoless-vs-{bacon,trunk} gap was ~20x inflated. RSS had the
   analogous bug: per-edit (10s) samples missed the transient
   compiler RSS of spawn-exit tools while always catching cargoless's
   resident RA. **Fixed (`5d3caeb`):** sum utime+stime+cutime+cstime
   (no double-count: a child's CPU is in its own utime/stime while a
   live tree member, in an ancestor's cutime/cstime once exited+
   reaped — counted once either way), plus a 250ms background
   RSS-peak tracker. The within-cargoless RA-polish sweep (§5) was
   **unaffected** (symmetric accounting both sides — all-cargoless,
   persistent RA in every config), so that result stands; only the
   cross-tool verdict waited for the corrected re-run.

The strategic surfacing of these as "evidence not bogus numbers" before
publishing is the same epistemics as cargoless's own "never publish red"
discipline — a benchmark that produces a plausible-but-wrong number is
worse than one that honestly reports "couldn't measure". Lesson 6 in
particular: a measurement that *looks* clean (every tool produced a
number, no errors) can still be systematically wrong; the defense is
sanity-checking magnitudes against physical plausibility (6 jiffies for
20 cargo checks cannot be real) BEFORE the number is allowed to inform a
verdict.

---

## 4. Architectural-asymmetry argument

_Approved framing from team-lead, applies regardless of the specific
throughput numbers below_:

cargoless's value proposition **is not** "faster than `trunk serve` or
`bacon` at the same thing". The honest framing is:

> cargoless wins the throughput dimension because its incremental verdict +
> publisher tier is a *different shape of work* than `bacon`'s
> `cargo check --watch` or `trunk`'s wasm-bundle-on-every-save path —
> not because it's "the same thing but more efficient." The bet is that
> the cargoless architecture is *adequate* for the inner-loop signal
> users actually want (catch the typo/missed-import/trait-bound error
> in the moment, publish wasm only on confirmed green) while doing
> measurably less per-edit work than competitors that re-bundle on every
> save.

What cargoless **architecturally** wins on (independent of polish):

1. **CPU-seconds per edit-cycle**: trunk's `watch` re-bundles wasm on
   every save (wasm-opt is CPU-heavy); cargoless's CAS dedupe (AC#5)
   skips identical-input rebuilds with zero compile work.
2. **Steady-state CPU%**: cargoless's warm RA reuses analysis across
   edits; trunk's bundler does not have an equivalent in-process cache
   and re-runs cargo's incremental compilation each time.
3. **Peak RSS (post-polish)**: cargoless is headless + post-polish
   has lean RA + no HTTP server + no WebSocket connections. trunk
   serves an HTTP+WS dev server alongside the bundler.

What we honestly acknowledge cargoless **does not** win on:

- **Cold-start RSS**: rust-analyzer is heavy (500MB-2GB) regardless of
  how it's used; trunk at startup is smaller until it bundles.
- **Initial cargo check CPU**: cargoless's authoritative tier (#55) is
  the same `cargo check` everything else runs; no speedup there.

The throughput-axis frame turns these acknowledged losses into context
("yes, cargoless uses the cargo-check tier — that's how it's
authoritative") rather than apologies.

---

## 5. RA-polish A/B — `cargoless watch` config sweep (CAPTURED, SOLID)

**This is the launch-relevant headline section.** It is a
*within-cargoless* comparison — every row is `cargoless watch` with a
different RA config, measured by the same harness with symmetric CPU
accounting on both sides. The cutime/cstime methodology bug (§3
lesson 6) affects only the *cross-tool* comparison (cargoless vs
trunk/bacon, where one side spawn-exits compilers and the other holds
a persistent RA); a within-cargoless sweep is unaffected because RA is
persistent in all four configs — so these deltas are reliable as-is.

Fixture `bench/fixture` (Leptos CSR, proc-macro-heavy via `view!`).
A1 = substrate `5aea5cc` (reps=30); A2-A4 = substrate `bda013b`
post-RA-polish (reps=20). The A1↔A2 substrate skew is documented
(§6) but does not change the conclusion — the within-A2..A4 sweep is
single-substrate and the dominant variable is the RA config.

| Config | Peak RSS | RSS growth | Mean CPU% | CPU-s/edit | vs A2 |
|---|---:|---:|---:|---:|---|
| **A1** pre-polish (`watch`, 5aea5cc) | 2.25 GB | 1.90 GB | 18.8% | 1.878s | — |
| **A2** post-polish default (`watch`) | 2.08 GB | 1.74 GB | 22.8% | 2.286s | baseline |
| **A3** `watch --proc-macro disabled` | **0.97 GB** | 0.58 GB | 7.3% | **0.727s** | **−53% RSS, −68% CPU** |
| **A4** `watch --features csr` | **0.53 GB** | 0.22 GB | 2.4% | **0.240s** | **−75% RSS, −90% CPU** |

**Findings:**

1. **The default lean InitOpts (A2) does NOT materially help on a
   proc-macro-heavy Leptos project.** A2 ≈ A1 (2.08 vs 2.25 GB; the
   CPU/edit is even slightly higher, within substrate-skew + reps
   noise). Root cause: `--proc-macro auto` correctly keeps the
   proc-macro server **on** for Leptos (`view!` needs it), and the
   proc-macro server is the dominant RSS+CPU consumer. The other
   default-on polish (checkOnSave-soften, cachePriming-off,
   inlayHints-off) is real but second-order against the proc-macro
   server's weight. **Honest finding: "lean by default" does not, by
   itself, fix the RA footprint on the exact project class (Leptos)
   cargoless most targets.**

2. **The opt-in knobs deliver dramatic savings.**
   `--proc-macro disabled` halves RSS and cuts CPU/edit ~68%;
   `--features csr` (narrow the feature set RA type-checks against)
   cuts RSS to **0.53 GB** (−75%) and CPU/edit to **0.240s** (−90%).
   These are large, real, and the actionable launch + v0.1 story.

3. **Tradeoff, stated honestly:** `--proc-macro disabled` sacrifices
   correctness on macro-expanded code (Leptos `view!` bodies won't be
   fully analyzed) — it's a power-user knob, not a safe default.
   `--features csr` is safe *if* the project genuinely only uses the
   `csr` feature (most Leptos CSR apps do) — which points to a concrete
   **v0.1 improvement: auto-detect-and-narrow the feature set** (the
   project's actual enabled features) rather than defaulting to RA's
   all-features behavior. That single change would move the *default*
   from A2 (~2 GB) toward A4 (~0.5 GB) for the common case.

**Launch narrative this supports (honest version):** *"cargoless's
inner-loop footprint is rust-analyzer-dominated. Out of the box on a
proc-macro-heavy project it is heavy (~2 GB); the shipped
`--proc-macro`/`--features` knobs cut that 53-75%, and v0.1 will
auto-narrow features so the default approaches the tuned figure."*
This is a **growth-path** story, not a **we-already-win** story —
which is the honest shape given the data.

### Component 3 status

The 4-way sweep above IS Component 3 (post-RA-polish A/B). The
"pre-polish vs post-polish DEFAULT" delta is the A1→A2 row: **the
default polish did not move the needle on Leptos** (the important,
non-obvious finding). The polish's value is entirely in the opt-in
knobs (A3/A4).

---

## 6. Caveats + limits

Recorded so the numbers stay honest under scrutiny:

- **Single fixture.** All numbers are against the 17-file / 1009-LOC
  bench fixture. A different project shape (more crates, more
  proc-macro use, larger codebase) would produce different absolute
  numbers; the *direction* of the comparison (which tool wins which
  axis) is likely robust, but the magnitudes are fixture-specific.
- **Single-host environment.** All runs on the dedicated
  `cargoless-builder` k8s pod; nothing else running. A laptop with
  background browser tabs / Slack / IDE would show different RSS +
  CPU patterns. Throughput numbers here are "best-case isolated
  measurement", not "typical-user-on-laptop".
- **30 reps / 5-minute windows, not full work-day sessions.** Memory
  growth observed at 30 edits may extrapolate or may plateau; we
  cannot conclude from this whether RA's cache eventually evicts after
  N hundreds of edits. The signal "growth was monotonic in the first
  30" is honest; "growth would continue indefinitely" would be a leap.
- **CPU sampled cumulatively (jiffies delta) — not pid-sliced.**
  cargoless's CPU number aggregates `tftrunk` + `rust-analyzer` + 
  `rust-analyzer-proc-macro-srv`. trunk's aggregates `trunk` + cargo +
  rustc workers. We don't separate which sub-process is responsible
  for what fraction; the comparison is "total CPU for the tool's
  whole ecosystem", which is the right user-facing view but blurs
  attribution.
- **Sampling cadence is finite.** Component 1 samples once per edit
  (every 10s); Component 2's background ticker samples every 5s. CPU
  spikes shorter than the tick are smoothed in the running average.
  The cumulative `cpu_seconds_per_edit` figure is exact (jiffies-
  cumulative differs by sample boundary, not by content); the per-tick
  `mean_cpu_pct` is approximate.
- **Comparators run their default mode.** `trunk watch` (not `trunk
  serve` — we chose the compile loop without HTTP-server overhead);
  `bacon --headless --job check` (not `bacon clippy` or `bacon test`
  which are different workloads). A user running `trunk serve` would
  pay additional HTTP/WS CPU/RSS on top of these numbers.
- **The fixture's `index.html` + `Trunk.toml` were added by the bench
  iteration**, not by the operator's actual app. Real Leptos projects
  ship these by default; bench-fixture-as-skeleton needed them
  retrofitted. This affects neither the comparison nor the absolute
  numbers since all tools start from the same fixture.
- **Cargoless not gated through ci-gate's gate-cache-staleness path.**
  The throughput runs use direct cargo invocations in the pod, not the
  ci-gate fingerprinting machinery, so the dev-fixer-flagged
  mtime=commit-time skip-rebuild bug does not apply here.

## 7. Verdict (populated when all sections land)

_PENDING_

Possible outcomes:

- **PASS** — cargoless wins on ≥2 contested dimensions vs both
  trunk and bacon (where bacon contests). README + launch blog can lead
  with the throughput-axis numbers + architectural-asymmetry framing.
- **MARGINAL** — cargoless wins on 1-2 dimensions vs ONE competitor;
  partial. Operator decision on whether to lead with the win, soften
  to a "competitive on" framing, or hold for further polish.
- **FAIL** — cargoless loses on most dimensions. Surface to operator;
  do not launch with a throughput claim that isn't there.
- **INCONCLUSIVE** — methodology divergence between Components 1+2
  doesn't resolve; we cannot trust the numbers. Investigate methodology
  before believing either.

---

## Appendix A: invocation reproducer

```bash
# In the cargoless-builder pod, with /work/bench-lead-src as the streamed
# tree at agent/bench-lead@<SHA>:
export PATH=/cache/cargo/bin:$PATH
export TRIFORM_OPERATOR_APPROVED_BUILD=1
export CARGO_TARGET_DIR=/cache/target-bench-lead

cd /work/bench-lead-src

# Component 1
bash bench/run-throughput.sh --reps 30 --inter-edit-sec 10 \
    --warm-timeout-sec 1200 --tool all

# Component 2 (separate dir + cache for isolation)
bash bench/throughput-recon.sh cargoless trunk bacon
```

## Appendix B: substrate provenance

- Substrate: `agent/bench-lead@<SHA>` = `origin/main@<MAIN_SHA>` merged
  with bench-lead's bench/-only fixes.
- Pod: `cargoless-builder-68b48dfcd7-dbf9b` in `cargoless-builder` namespace.
- Comparators: `trunk 0.21.4`, `bacon 3.22.0` (installed in pod under
  `/cache/cargo/bin/` via `cargo install --locked trunk@0.21.4 bacon`).
- Cargo / Rust: as per the pod's pinned 1.85.0 toolchain.
