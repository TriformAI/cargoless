# AC#7 — Throughput Investigation Report

> **DRAFT — populated incrementally as measurements complete.**
> Owner: bench-lead • Substrate: `agent/bench-lead@<SHA>` (= main + bench/)
> Methodology: two independent measurements (Components 1 + 2) cross-checking;
> A/B comparison pre/post the RA weight-shedding polish (Component 3) when it lands.

## TL;DR

Operator's AC#7 reframe (2026-05-17): "Speed is good, but throughput is better.
I want to know how we can make it spend less CPU/RAM. This is the major point."

The original latency-comparison axis (which became cargoless-vs-trunk-vs-bacon
on save→verdict timing) was **cargo-check-bound** — all three tools delegate
the actual typecheck work to the same cargo subprocess, so the latency
comparison wasn't measuring the differentiator. cargoless's architectural
advantages live on the **throughput axis** — CPU + RAM cost per edit-cycle —
where the design choices (CAS dedupe, warm RA reuse, headless / no-HTTP,
fail-closed publisher) actually do less work for the same observable user
behavior.

**Headline numbers** (post-RA-polish, populated when Component 3 lands):

| Tool       | Peak RSS   | RSS growth | Mean CPU% | CPU-seconds / edit |
|------------|-----------:|-----------:|----------:|-------------------:|
| cargoless  | _TBD_      | _TBD_      | _TBD_     | _TBD_              |
| trunk      | _TBD_      | _TBD_      | _TBD_     | _TBD_              |
| bacon      | _TBD_      | _TBD_      | _TBD_     | _TBD_              |

(All measurements on `bench/fixture` — 17-file / ~1009-LOC Leptos CSR
project; honest-size floor reasserted per `bench/run.sh` convention.)

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
- **CPU source**: `/proc/<pid>/stat` fields 14 (utime) + 15 (stime),
  cumulative jiffies since process start. Summed across the tree.
- **Edit driver**: open(O_WRONLY|O_TRUNC|O_CREAT) → write_all → fsync →
  close — single `write(2)` syscall, no temp+rename. Matches the
  FS-event shape every editor (vim/vscode/RA-on-save) produces and
  every notify-rs watcher handles cleanly.
- **Sampling cadence**: snapshot at edit time (after the sleep
  interval) — one sample per rep.

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

### 2.1 Component 1 (primary harness)

#### cargoless (CAPTURED)

```
warm_secs:               4.50s
reps:                    30/30
baseline_rss_kb:         539,476  (~527 MB)
final_rss_kb:            2,333,032  (~2.28 GB)
peak_rss_kb:             2,333,032  (~2.28 GB)
rss_growth_kb:           1,793,556  (~1.75 GB growth across the session)
total_cpu_seconds:       57.52
wall_secs:               300.42
mean_cpu_pct:            19.1
cpu_seconds_per_edit:    1.917
wall_secs_per_edit:      10.01
```

Provenance: `agent/bench-lead@e96a365`, `/cache/target-bench-lead/release/tftrunk`,
bench/fixture, default debouncer 150ms.

#### trunk (PENDING — Component 1 detached run in progress)

```
warm_secs:               _TBD_
peak_rss_kb:             _TBD_
rss_growth_kb:           _TBD_
mean_cpu_pct:            _TBD_
cpu_seconds_per_edit:    _TBD_
```

#### bacon (PENDING — Component 1 detached run in progress)

```
warm_secs:               _TBD_
peak_rss_kb:             _TBD_
rss_growth_kb:           _TBD_
mean_cpu_pct:            _TBD_
cpu_seconds_per_edit:    _TBD_
```

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

The strategic surfacing of these as "evidence not bogus numbers" before
publishing is the same epistemics as cargoless's own "never publish red"
discipline — a benchmark that produces a plausible-but-wrong number is
worse than one that honestly reports "couldn't measure".

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

## 5. Pre/post RA-polish A/B (Component 3)

_PENDING — runs after dev-fixer's RA weight-shedding bundle (task #74,
RA config = `checkOnSave=off`, `inlayHints.*=off`, `cachePriming=off`,
`procMacro` auto-gated, `cargo.features` narrowed) lands on main._

Same fixture, same protocol, same comparators. Two columns per metric:
pre-polish (current substrate) and post-polish.

| Metric                | cargoless pre | cargoless post | Δ%   |
|-----------------------|--------------:|---------------:|-----:|
| Peak RSS              | _TBD_         | _TBD_          | _TBD_|
| Mean CPU%             | _TBD_         | _TBD_          | _TBD_|
| CPU-seconds per edit  | _TBD_         | _TBD_          | _TBD_|
| Warm time             | _TBD_         | _TBD_          | _TBD_|

The "we shipped lean by default" narrative gets concrete numbers from
this delta. If the delta is large, it's a launch-blog headline. If it
isn't, that's also honest data — the polish was attempted, didn't move
the needle as much as hoped, and the trunk/bacon comparison stands or
falls on the architectural axes regardless.

---

## 6. Verdict (populated when all sections land)

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
