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

**Finding 2 — cross-tool vs trunk/bacon (RESOLVED → §7 MARGINAL;
headline TWO-SOURCE-CONFIRMED §8.5):** the first passes had a CPU/RSS
accounting bug (own-CPU-only + per-edit-RSS asymmetrically
under-counts the spawn-exit comparators vs cargoless's persistent RA
— §3 lesson 6). Caught by physical-impossibility sanity-check (bacon
6 jiffies for 20 cargo checks), harness fixed (`5d3caeb`:
+cutime/cstime +250ms bg RSS-peak), corrected re-run + Axis-B-unblock
complete, **then independently cross-verified by Component-2
(clean-C2 `c04cdf0`, §8.5): cargoless 3.35s vs trunk 6.89s CPU/edit
reproduced at Δ≈1% by a second methodology** (every first-pass
divergence root-caused + fixed + reconciled, none averaged).
**Corrected result flipped the headline:** trunk is the per-edit
**CPU hog** (~6.9s — it rebundles wasm every save), cargoless does
**~half** that on both the watch (3.4s) and build (3.7s) paths
because its state-model rebuilds only on a green-edge. cargoless's
real weakness is **RSS** (RA-resident ~2 GB default, loses to both).
Net: **clean two-source CPU win vs trunk on 2 axes, clean RSS loss
vs both, not like-for-like vs bacon (a checker, not a build+publish
tool) → MARGINAL** (§7), an
operator-reserved launch-scope decision.

**Headline numbers (corrected, complete):**

Cross-tool, default config (substrate post-D1 + Trunk.toml-fix,
reps=15):

| Tool | Peak RSS | CPU-s/edit (watch) | CPU-s/edit (build) |
|---|---:|---:|---:|
| **cargoless** | 2.1-2.3 GB | **3.39s** | **3.68s** |
| trunk | ~0.5-0.6 GB | 6.96s | 6.94s |
| bacon | 0.24 GB | 0.49s | n/a (checker) |

Within-cargoless RA-polish sweep (the launch-relevant tuning story):

| cargoless `watch` config | Peak RSS | CPU-s/edit | vs default |
|---|---:|---:|---|
| default (post-RA-polish) | 2.08 GB | 2.286s | baseline |
| `--proc-macro disabled`  | 0.97 GB | 0.727s | −53% / −68% |
| `--features csr`         | **0.53 GB** | **0.240s** | **−75% / −90%** — wins both axes vs trunk |

(All on `bench/fixture` — 17-file / ~1009-LOC Leptos CSR; honest-size
floor reasserted per `bench/run.sh`. Reps differ across passes —
30 pre-polish-baseline, 20 RA-sweep, 15 corrected-cross-tool — noted
in §6; the directional conclusions are robust to the rep count.)

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

## 2. Results — corrected accounting (CAPTURED, COMPLETE)

Substrate `agent/bench-lead` post-D1-rename + Trunk.toml-dist-fix
(`920fc20`/`67d444b` lineage). Accounting CORRECTED (`5d3caeb`):
CPU = utime+stime+cutime+cstime (reaped-child CPU counted); peak RSS =
max(per-edit, 250ms background tracker). reps=15, 8s inter-edit. The
earlier own-CPU-only / per-edit-RSS numbers are SUPERSEDED — the
correction changed the cross-tool picture *qualitatively* (see §3
lesson 6 for why the first numbers were physically impossible and how
the bias was caught).

### 2.1 Watch-path — the RA-verdict tier (cargoless carries RA; trunk/bacon don't)

`cargoless watch` = rust-analyzer + cargo-check verdict, **no wasm
build**. `trunk watch` = cargo incremental + wasm-bindgen bundle,
**no RA**. `bacon --headless --job check` = bare `cargo check`, **no
RA, no wasm**. These are deliberately different work shapes — this
axis measures "what each tool's chosen dev-loop costs", not a
like-for-like algorithm race.

| Tool | peak RSS | RSS growth | mean CPU% | **CPU-s/edit** |
|---|---:|---:|---:|---:|
| bacon `--headless check` | 238 MB | ~0 (8 KB) | 6.1% | **0.493s** |
| cargoless `watch` | 2.34 GB | 1.70 GB | 42.2% | **3.389s** |
| trunk `watch` | 519 MB | 13 MB | 86.8% | **6.963s** |

**The corrected accounting flipped the headline.** The biased first
pass showed trunk "cheapest" (0.096s — its transient cargo+wasm-bindgen
CPU was reaped into a parent we weren't summing). Corrected: **trunk is
the CPU HOG (6.96s/edit) — it re-bundles wasm on every save.**
cargoless does ~half that (3.39s) because its verdict path does not
bundle wasm. bacon is lightest (0.49s — bare cargo check, no RA, no
wasm) but does the least.

### 2.2 Build-path — the artifact tier (§4 CAS / state-model axis, apples-to-apples)

Both produce a wasm dist. `cargoless build --watch --out` =
RA+cargo-check verdict → on a **green-edge**, CAS build + atomic
publish. `trunk watch` = unconditional cargo+wasm-bindgen rebundle
**every save**. Trunk.toml-dist-fix (`6cb1b7a`) unblocked this — it
had failed every rep prior (cargoless's orchestrator hardcodes
`project_root/dist`; the fixture pinned `trunk-dist`).

| Tool | peak RSS | RSS growth | mean CPU% | **CPU-s/edit** |
|---|---:|---:|---:|---:|
| cargoless `build --watch --out` | 2.12 GB | 1.56 GB | 45.8% | **3.675s** |
| trunk `watch` | 582 MB | 14 MB | 86.5% | **6.937s** |

**CAS publish VERIFIED** — `.cargoless-tput-out/` materialized the real
33 KB wasm-bindgen JS artifact; `.cargoless/latest-green` advanced to
a real `input_hash`. **The §4 architectural-asymmetry is confirmed and
sharper than "CAS-dedupe":** across 15 green-staying edits cargoless
emitted exactly **1** `GREEN — building`/`published` — its **state
model rebuilds only on a green-EDGE (red→green transition), not on
every save**. trunk rebundles wasm on *every* save. So a developer
making a series of green edits gets 1 cargoless rebuild vs 15 trunk
rebundles. cargoless build-path CPU/edit (3.68s, ≈ its watch-path
verdict cost) is **~1.9× cheaper than trunk's (6.94s)**.

**Honest workload caveat:** the comment-toggle edit keeps the tree
always-green, which is the *most favorable* workload for cargoless's
edge-triggered model (1 rebuild total). A red↔green churn workload
(introduce error / fix it) would trigger a cargoless rebuild per
green-edge — still fewer than trunk's per-save rebundle, but the
margin is workload-dependent. Both workload shapes favor cargoless on
the build path; the *magnitude* is what varies. Stated so the number
is not over-claimed.

### 2.3 Component 2 (independent ps/bash cross-check)

The validation + Axis-B passes used Component 1 (Python). Component 2
(`bench/throughput-recon.sh`, bash+ps, isolated dir/cache) was written
+ ANSI/cutime-aware but, given the wall-clock already spent and that
the corrected Component-1 numbers are internally consistent + match
physical plausibility (trunk's 86% mean CPU during wasm rebundle is
exactly what a wasm-opt-adjacent pipeline should show; bacon's 0.49s ≈
a warm incremental cargo check), the second methodology is recorded as
**available-not-run**. Running it is the highest-value next step if the
operator wants the two-source cross-check hardened before the launch
blog cites these numbers; flagged as a recommendation, not a blocker.


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

## 7. Verdict — **MARGINAL** (honest, nuanced; operator-reserved launch-scope)

**AC#7 strict reading** ("cargoless better on ≥2 dimensions vs
trunk/bacon"): **MARGINAL** — a clean PASS against trunk on the CPU
axis, a clean LOSS against both on RSS, and not a like-for-like
contest against bacon at all. Synthesised from the corrected
complete data (§2) + the within-cargoless sweep (§5):

### Contested-dimension scorecard

| Dimension | cargoless | trunk | bacon | cargoless verdict |
|---|---:|---:|---:|---|
| CPU-s/edit (watch) | 3.39s | 6.96s | 0.49s | **WIN vs trunk**, LOSE vs bacon |
| CPU-s/edit (build) | 3.68s | 6.94s | n/a | **WIN vs trunk** (§4 confirmed) |
| Peak RSS | 2.1-2.3 GB | ~0.5-0.6 GB | 0.24 GB | LOSE vs both |
| RSS growth | 1.6-1.7 GB | ~13 MB | ~0 | LOSE vs both |
| CPU-s/edit, **tuned** `--features csr` | **0.24s** | 6.96s | 0.49s | **WIN vs BOTH** |
| Peak RSS, **tuned** `--features csr` | **0.53 GB** | ~0.55 GB | 0.24 GB | **~TIE/WIN vs trunk**, LOSE vs bacon |

### The honest synthesis

1. **cargoless decisively beats trunk on per-edit CPU on BOTH axes
   (watch 2.1×, build 1.9×)** — not by being a faster trunk, but by a
   *different shape of work*: cargoless rebuilds on a green-edge,
   trunk rebundles wasm on every save. This is the §4
   architectural-asymmetry, measured and confirmed. **2 contested
   wins vs trunk.**
2. **cargoless's default RSS is its real weakness** — ~2 GB,
   RA-resident, monotonic-growth-shaped. It loses RSS vs both trunk
   and bacon at default config. This is the F5/D-A2 cost in
   throughput terms; it is honest and must not be hidden.
3. **The shipped `--features csr` knob closes the RSS gap** — tuned,
   cargoless is 0.53 GB / 0.24s, which **wins both axes vs trunk** and
   approaches bacon. The default does NOT auto-narrow (proc-macro-auto
   keeps the `view!` server on); the concrete v0.1 path is
   auto-detect-and-narrow the project's actual feature set.
4. **bacon is the lightest tool on every axis** (0.24 GB / 0.49s) but
   does strictly less — bare `cargo check`, no wasm artifact, no
   publish, no headless verdict stream. It is not the same product
   category; a "cargoless loses to bacon" headline would compare a
   build+publish tool to a checker. Honest framing: cargoless's
   competitor for *what it does* is trunk, and it beats trunk on CPU.

### Recommended launch framing (operator-reserved decision)

**Defensible, no-spin:** *"cargoless does roughly half the per-edit CPU
of `trunk serve` — it rebuilds on confirmed-green edges, not blindly
on every keystroke like trunk re-bundles. Its memory footprint is
rust-analyzer-dominated (~2 GB default on proc-macro-heavy projects);
the shipped `--features` knob cuts that ~75% and a v0.1
auto-narrowing change moves the default there."*

This is a **growth-path, CPU-win, honest-RSS-caveat** story. It is NOT
a clean AC#7 PASS (RSS loses at default) and NOT a FAIL (a real,
measured, architecturally-grounded 2× CPU win vs the primary
competitor). Per the brief, a MARGINAL throughput verdict is an
**operator-reserved launch-scope decision** — surfaced with the
complete picture, not auto-resolved by this report. The three
operator-actionable outputs: (a) the CPU-win-vs-trunk claim is
citable as-is; (b) the RSS caveat must ship honestly; (c) the v0.1
auto-narrow-features change is the single highest-leverage follow-up.

### Methodology trust note

The cross-tool numbers are from corrected Component-1 accounting
(`5d3caeb`) AND now independently reproduced by Component-2
(clean-C2, `c04cdf0`, §8.5) at Δ≈1% on the headline. The
cargoless-vs-trunk CPU-win is **TWO-SOURCE-CONFIRMED**, not
single-source. The first-pass numbers were caught as
physically-impossible and superseded with full audit trail (§3
lesson 6); the first Component-2 pass's divergences were
root-caused, the root causes fixed, and the clean re-run reconciles
(§8.5) — none averaged. The report's trust rests on that
disclosure + cross-check discipline: every headline number now has
two-source provenance, and every divergence-along-the-way is on the
record with its cause.

---

## 8. Two-source cross-check (Component-2 independent methodology)

**Verdict line (UPDATED post clean-C2 #109): TWO-SOURCE-CONFIRMED —
the cargoless-vs-trunk CPU-win headline now reproduces within ~1%
across two independent methodologies; every first-pass divergence was
root-caused, the root causes fixed, and the corrected re-run
reconciles. See §8.5 for the clean-C2 result; §8.1–8.4 are retained
as the audit trail of how the divergences were found and eliminated
(NOT averaged-over).**

Component-2 (`bench/throughput-recon.sh`: bash+ps+awk, `ps --ppid` BFS,
`ps -o rss=`, `sed -i` edit, 5 s background ticker, isolated
`/work/bench-lead-recon-src` + `/cache/target-bench-lead-recon`) run
against the same corrected accounting (cutime+cstime parity fix
`670de75` — audited + confirmed in-tree before the run, the
pre-condition for a valid cross-check). reps=15, 8 s inter-edit.

### 8.1 Cross-source table

| metric | C1 (corrected) | C2 (recon) | Δ | verdict |
|---|---:|---:|---:|---|
| cargoless peak RSS | 2.34 GB | 2.14 GB | −9% | **TWO-SOURCE-CONFIRMED** |
| cargoless RSS growth | 1.70 GB | 1.36 GB | −20% | same order; directionally consistent |
| cargoless CPU-s/edit | 3.389s | 6.252s | **+84%** | DIVERGENT — cache-warmth (hypothesis) |
| bacon CPU-s/edit | 0.493s | 0.457s | −7% | numerically close but **C2-CONFOUNDED** (see 8.3) |
| bacon peak RSS | 238 MB | 67 MB | **−72%** | DIVERGENT — sampling cadence (pre-flagged, confirmed) |
| trunk (all) | 6.96s / 519 MB | **NO_READY** | — | **C2 COULD-NOT-MEASURE** (recon edit-driver bug) |

### 8.2 The one clean confirmation

**cargoless peak RSS is two-source-hardened: ~2.2 GB** (C1 2.34, C2
2.14, Δ−9% — well within cross-methodology variance). This is robust
*because* RSS is dominated by RA's resident footprint, which is
insensitive to both the cadence difference AND the recon edit-driver
bug (those change *what* RA analyzes, not *how much RAM it holds*).
The headline "cargoless's default footprint is RA-dominated ~2 GB" and
the §5 `--features csr`→0.53 GB mitigation are safe to cite with
two-source provenance.

### 8.3 Divergences — root-caused, NOT averaged (per the brief)

1. **cargoless CPU/edit +84% (3.39s→6.25s): cache-warmth.** C2 isolates
   `RECON_TARGET=/cache/target-bench-lead-recon` — a *cold* cargo
   target. Evidence: C2 cargoless warm=**19 s** (C1 4.5 s), baseline
   cpu_j already 7370 (RA indexing a cold workspace), mean CPU 77.5%
   (C1 42.2%). Cold-isolated cache makes RA + cargo-check do
   materially more work per edit. The *qualitative* "cargoless is
   RA-heavy" is confirmed by BOTH; the *absolute* CPU/edit is
   **cache-state-sensitive — report as a range: ~3.4 s warm-shared /
   ~6.3 s cold-isolated**, not a point estimate.
2. **bacon peak RSS −72% (238→67 MB): sampling cadence — the
   pre-flagged hypothesis, CONFIRMED.** Commit `670de75` predicted
   verbatim: "If RSS-peak diverges, the 5s-vs-250ms cadence is the
   ready hypothesis." bacon's `cargo check` subprocess peaks for
   <1 s; C1's 250 ms tracker catches it, C2's 5 s ticker misses it.
   **C1's 238 MB is the more accurate peak**; C2's 67 MB is a
   cadence artifact. Methodology-difference-explained, not a
   contradiction.
3. **trunk C2 NO_READY: a recon-harness bug, NOT a trunk fact.**
   The recon's `flip_edit` uses `sed -i 's|^.*BENCH_TRAIT_ANCHOR.*|…|'`
   — a **whole-line** replace, lossy vs C1's precise
   `str.replace(ANCHOR, FLIP, 1)` substring swap. It corrupted the
   isolated fixture source; trunk's *real* `cargo build
   --target=wasm32` then failed (`error: could not compile … expected
   one of ! or ::`, exit 101) → NO_READY at the 900 s ceiling.
   **Consequence: C2 never measured trunk**, so the headline
   **cargoless-CPU-win-vs-trunk is single-source (C1 only) — NOT
   two-source-hardened.**
4. **bacon CPU/edit −7% is C2-CONFOUNDED, not a clean confirmation.**
   bacon ran *after* cargoless's 15 sed-edits + restore on the shared
   recon copy; the lossy restore likely left the source corrupted
   (same bug as #3). bacon's `ready` patterns include `could not
   compile`/`error[`, so bacon "warmed" on the *broken* source and
   its 0.457 s is cargo-check-of-corrupt-source, not clean. The
   numeric closeness to C1's 0.493 s is **coincidental** (a fast
   parse-error check costs ≈ a fast clean incremental check) and
   must NOT be cited as a clean two-source confirmation.

### 8.4 Honest net + recommendation

- **Two-source-hardened, citable now:** cargoless default peak RSS
  ~2.2 GB (and therefore the RA-dominated-footprint framing + the §5
  `--features csr` 75% mitigation).
- **NOT two-source-hardened:** the cargoless-vs-trunk CPU-win
  headline. C1's number is internally consistent + physically
  plausible (trunk 86% mean CPU during wasm rebundle), but C2 could
  not reproduce trunk because the recon edit-driver corrupted the
  source. Until a clean C2 trunk number exists, the CPU-win claim
  must ship with **single-source (C1) provenance + the cache-state
  caveat**, not as a two-source fact.
- **Recon-harness bug is itself a launch-material finding** (per the
  brief: a divergence is a finding). Fix = replace the recon's
  whole-line `sed -i` with a precise substring swap (matching C1's
  driver) + run C2 on a warm-shared cache (C1-parity) so trunk
  actually builds. That is a ~30-45 min fix-and-rerun; it is the
  recommended hardening step **before** any launch blog cites the
  cross-tool CPU figures. Flagged for the operator launch-scope
  decision — NOT silently iterated, NOT averaged-over.

This §8.1–8.4 made the report's claims provenance-explicit at the
time: RSS two-source-solid; CPU-win single-source-pending-clean-C2.
That gate is now cleared — see §8.5.

### 8.5 Clean-C2 (#109) — headline TWO-SOURCE-CONFIRMED

Both first-pass divergences were root-caused in §8.3 and the root
causes fixed (`c04cdf0`):

- **recon edit-driver** lossy `sed -i` whole-line replace →
  **precise substring-swap from a captured clean baseline + single
  fsync'd write** (byte-for-byte Component-1's `FixtureEditor` op).
  This is the fix for the trunk-NO_READY (the lossy replace had
  corrupted the source so trunk's real `cargo build` failed).
- **cold isolated cache** → **warm SHARED cache + the live fixture**
  (the same `model.rs` + `/cache/target-bench-lead` C1 used).
  Methodology independence stays in the measurement *code*
  (bash+ps+awk, `ps --ppid` BFS, `ps -o rss=`, 5 s ticker), not the
  filesystem path. This is the fix for the +84% cargoless CPU
  divergence (it was 100% a cold-cache artifact).

Clean-C2 result (substrate `c04cdf0`, reps=15, 8 s inter-edit,
cutime+cstime accounting on BOTH sides):

| metric | C1 corrected | C2 clean | Δ | verdict |
|---|---:|---:|---:|---|
| cargoless CPU-s/edit | 3.389s | 3.348s | **−1.2%** | **TWO-SOURCE-CONFIRMED** |
| trunk CPU-s/edit | 6.963s | 6.887s | **−1.1%** | **TWO-SOURCE-CONFIRMED** |
| trunk peak RSS | 519 MB | 511 MB | −1.5% | **TWO-SOURCE-CONFIRMED** |
| bacon CPU-s/edit | 0.493s | 0.476s | −3.4% | **TWO-SOURCE-CONFIRMED** (clean source — no longer confounded) |
| cargoless peak RSS | 2.34 GB | 2.02 GB | −14% | CONSISTENT (RA-resident-dominant; within cross-method variance) |
| bacon peak RSS | 238 MB | 168 MB | −29% | PARTIAL — residual sampling-cadence gap (C1 250 ms catches more of bacon's <1 s `cargo check` peak than C2's 5 s ticker; C1's 238 MB is the truer peak). A documented methodology difference, not a contradiction. |

**The AC#7 headline is now two-source-hardened:** cargoless
**~3.35 s** CPU/edit vs trunk **~6.9 s** — independently reproduced at
**Δ≈1%** by two methodologies that share only the correctness
invariants (cutime+cstime accounting, precise edit, warm cache) and
differ in everything else (language, /proc-walk, RSS source, sampling
cadence). **cargoless does ≈2.05× less per-edit CPU than `trunk
serve`** — citable in launch material with two-source provenance.
The trunk peak RSS and bacon CPU figures are likewise two-source-
confirmed. The only residual divergence (bacon peak RSS, −29%) is a
fully-explained sampling-cadence artifact (C1's finer 250 ms tracker
is the more accurate peak); it does not touch the headline.

The §8.1–8.4 numbers (single-source, biased-recon) are SUPERSEDED by
§8.5 and retained only as the methodology audit trail. Honest beats
favorable — and here, with the root causes fixed, honest IS
favorable.

### 8.6 Honest comparison-frame note: per-agent-edit-batch, not per-keystroke

**Framing flag for launch materials (operator-clarified input model).**
The numbers above are measured per *edit* in a synthetic
comment-toggle loop — a reasonable proxy, but the **honest cost unit
for cargoless's actual primary input is per-AGENT-edit-BATCH**: an
agent writes a whole file (or a coherent multi-file change) atomically
and expects ONE meaningful verdict/build per logical commit — not a
verdict per simulated human keystroke. Under that frame cargoless's
green-edge / structural-completeness model dominates trunk's blind
"rebundle on every filesystem event" **even more strongly** than the
per-edit numbers show: an agent's multi-write batch is one cargoless
green-edge (one rebuild) but N trunk rebundles. The per-edit figures
here are therefore a *conservative lower bound* on cargoless's
relative CPU advantage in its real usage mode.

This note is a **framing-honesty flag only** — the harness was NOT
re-architected to the agent-edit-batch unit for this pass (the
methodology stays exactly as documented, so the two-source numbers
are clean). A future bench pass MAY re-express the comparison in the
agent-edit-batch unit; dev-fixer's #107/#110 design names the exact
agent-whole-file-write seam that pass would quantify. Launch
materials should lead with the agent-edit-batch frame as the honest
one, citing the per-edit two-source numbers as the conservative
floor.

---

## 9. Structural-trigger fired-check-reduction (#112 stage-1) — DELIVERED

The §8.6 agent-edit-batch frame's quantification, measured through
**dev-fixer's real structural-trigger seam @ `11519e6`**
(`TF_STRUCTURAL_TRIGGER=1`; `ModelSession::structural_counters() ->
(settled, closed)` read at the genuine `model::watch()`
notify→debounce→coalesce→`structural.record(all_closed)` site).
Harness: `crates/tf-core/tests/structural_trigger_bench.rs`
(public-API tf-core integration test, RA-spawning, builder-pod;
substrate `agent/bench-lead@280576b`, reps via the disclosed trace).

**Metric:** `fired_check_reduction = 1 − (closed_batches /
settled_batches)` — the fraction of authoritative cargo-checks the
structural trigger would *eliminate* per agent-edit-batch (each
eliminated check ≈ one full AC#7 ~3–7 s CPU authoritative check **and**
an idle window the stage-2 idle-evict-RA RAM lever can exploit).

### 9.1 Result — a disclosed spectrum, NOT a single number

`is_closed` is a pure syntactic balance scan, so the metric is
deterministic given the trace ⇒ reported as a spectrum over three
**fixed, fully-disclosed** agent-behaviour profiles (20 atomic
whole-file Write batches each; OPEN = interrupted/truncated/mid-string
draft; CLOSED = balanced; incl. split-multi-file in both states):

| Profile (disclosed OPEN-design) | settled | closed | **fired-check-reduction** |
|---|---:|---:|---:|
| CONSERVATIVE (~10 % open — agents almost always Write whole balanced files) | 20 | 19 | **5.0 %** |
| MODERATE (~30 % open — routine iterative drafting / interrupted tool calls) | 20 | 15 | **25.0 %** |
| AGGRESSIVE-DRAFT (~50 % open — heavy skeleton-then-fill multi-file authoring) | 20 | 11 | **45.0 %** |

Test **passed** (1/1); wall 73 s.

### 9.2 Seam validation (this is the launch-relevant proof)

`settled == 20` for **every** profile = the debounce pipeline
coalesced exactly one settle per agent-edit-batch — **the
notify→debounce→`structural.record` wiring is real and live, not
dormant/theoretical**. The reduction is **monotone** across the
disclosed OPEN-fractions (5 % → 25 % → 45 %) = the seam genuinely
tracks per-batch syntactic closedness. The test asserts these seam
properties (settled>0; monotone) — it does **not** assert a favourable
number; the figures are reported, never gated.

### 9.3 Honest reading (operator-actionable)

The structural trigger eliminates a fraction of authoritative checks
**≈ the agent-OPEN-batch fraction** — i.e. exactly the rate at which
agents land syntactically-incomplete intermediate whole-file Writes
(interrupted tool calls, skeleton-before-body, mid-string truncation).
The realised reduction is slightly below the nominal OPEN-design
(5 vs ~10, 25 vs ~30, 45 vs ~50 %) because the real `is_closed`
predicate classified a few "open-intended" drafts as still
syntactically balanced — **reported as the seam actually recorded it,
not as designed** (same disclosure discipline as the rest of this
report).

**The launch number is not ours to pick — it is the dogfood-observed
agent broken-intermediate rate mapped onto this validated bracket.**
Decision input for v0-default-vs-v0.1 (#101): if the field rate is
conservative (~10 %) the CPU saving is modest (~5 %) and the trigger
is a v0.1 nicety; if moderate-to-aggressive (≥30 %) it is a material
(~25–45 %) per-edit CPU cut **and** the enabler for idle-evict RAM
reclamation (the operator's #1, stage-2). Combined with the §8.5
two-source ~2× CPU-win vs trunk and the §8.6 conservative-floor
framing, stage-1 says: cargoless's green-edge/structural model has a
*tunable, real, seam-validated* additional CPU lever whose magnitude
scales with exactly the workload cargoless is built for (agents).

**This conditional is now field-resolved — see §9.5.** The
dogfood-observed rate for the actual cargoless agent fleet came in at
the *bottom* of the bracket (~0 % on `.rs`), which selects the
"conservative ⇒ v0.1-nicety" branch above, **not** the
"moderate-to-aggressive ⇒ material" one. Read §9.5 before citing
stage-1 in any launch decision: the lever is mechanism-real but its
*realised* benefit for how this fleet writes is ≈ 0 %.

### 9.4 Stage-2 (per-tier RSS-delta) — queued

Per the lead's two-stage split, stage-2 (per-tier RSS-delta vs the
§8.5 two-source ~2.0–2.3 GB baseline, for dev-fixer's #114 RAM tiers:
allocator/jemalloc, lru-cap + #74-knobs-default, proc-macro-off-default,
idle-evict-RA) is double-gated on the lead's "tiers-ready @ &lt;sha&gt;"
relay and runs as a SECOND pass on the same harness shape. Its
combined {fired-check-reduction, per-tier RSS-delta} is what reshapes
the held launch-scope (a material RSS drop shrinks/removes the §7
honest-RSS-caveat rather than merely `--features`-mitigating it). Not
blocking — stage-1 above is the gating CPU datum and is DELIVERED.

### 9.5 #117 field anchor — DELIVERED (the spectrum point for THIS fleet)

The §9.3 conditional's missing input — *where on the validated
5/25/45 % bracket does the real cargoless agent fleet actually sit?* —
measured by dogfood-lead (#117), folded here (bench-lead owns the
spectrum; dogfood-lead provides the field anchor; predicate stays the
single shared `is_closed`).

**Method (zero-drift-gated).** Local cargo is hook-blocked Mac-side,
so the pre-authorised fallback was used: a faithful Python mirror of
`is_closed`, **oracle-gated against `structural.rs`'s own
`#[cfg(test)]` block — all 35 assertions PASS before any datum was
classified** (drift ⇒ the classifier aborts; no measurement on a
drifted predicate). Dataset: the **survivorship-free** artifact
(bench-lead find) — N=16 fleet session `.jsonl` transcripts, an
append-only log that preserves OPEN intermediates git can never show
(only CLOSED survivors commit). **207 `Write` tool_use events**;
≤1 Write/assistant-turn for this fleet, so per-Write **IS** the §9.4
batch-AND figure for this population (not merely a lower bound).

**Result — the headline is a trap; the honest number is domain-scoped.**

| scope | total / open | % OPEN | meaning |
|---|---:|---:|---|
| all files (naïve) | 207 / 55 | **26.6 %** | **DO NOT ANCHOR** — Rust-lexer-on-non-Rust artifact |
| **`.rs` only** | **97 / 0** | **0.0 %** | the trigger's *actual* domain |

The 55 OPENs are almost entirely non-Rust files (`.sh` heredocs/`$()`
68 %, `.md` code-fences 42 %, `.yml` 100 %) where a Rust balance-lexer
*correctly* reads normal non-Rust syntax as "unbalanced." That is a
predicate-domain category error, not real OPEN intermediate code.
**Verified against the code-under-test:** `model.rs` at the
`structural.record(all_closed)` call-site filters the coalesced batch
to `.rs` (`path.extension() … e != "rs"`) *before* `is_closed` —
cargoless's trigger never evaluates `.sh`/`.md`/`.yml`. The
methodologically correct anchor is therefore the **`.rs` rate = 0 %
(0/97, mean 4.3 KB/file — substantive files, not stubs)**.

**Honest reading (revises §9.3 DOWN).** For the cargoless agent fleet
*as it actually writes* (Claude-family + whole-file-`Write`-dominant
toolset), realised fired-check-reduction ≈ **~0 %**. These agents
compose complete, syntactically-closed Rust and `Write` it atomically;
the OPEN-intermediate-Rust pattern the trigger is built to skip
(skeleton-then-body, interrupted mid-`.rs` tool calls) **is not how
this population writes.** The structural trigger is a *real mechanism*
whose realised benefit is **entirely edit-style-dependent**, and this
fleet's style gives it essentially nothing to skip. A different
population that emitted skeleton-then-body `.rs` would land higher on
the (still-valid) bracket — that is a different fleet, not this one.

**Caveats (stated, per discipline).** Single fleet; single agent
family; this specific toolset; **`Write`-only** — Edit-tool hunk
intermediate states are *not* captured (post-edit buffer
reconstruction was out of scope; a known honest gap, not a silent
one — an Edit-heavy agent doing many small broken-intermediate hunks
could differ, and that path is unmeasured). N = 16 sessions / 97 `.rs`
Writes is small-but-real. Survivorship-free for the `Write` path;
silent on the Edit path.

**Launch-scope implication (#101) — the honest, less-favourable one.**
The structural-trigger leg revises from "tunable real CPU lever" to
**"real mechanism, ≈ 0 % realised benefit for this fleet's edit
style."** It is a v0.1-nicety / conditional-benefit mechanism for the
cargoless agent population *as it writes today* — **not** a material
v0 CPU lever, and (since fired-check-reduction ≈ 0 here) **not** a
material idle-evict-RAM enabler for this fleet either (the RAM lever
that matters at fleet scale is Tier-4 idle-evict on the *idle*
window, measured directly in stage-2/§10 and stage-3/#116 — it does
not depend on the trigger firing). This does not weaken the §8.5
two-source ~2× CPU-win vs `trunk` (that stands on its own); it
narrows the *additional* structural-trigger claim to honest size.

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
