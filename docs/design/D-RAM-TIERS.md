# D-RAM-TIERS — tiered RA RAM-reduction (operator #1 priority)

**Status:** DESIGN + partial implementation (Tier-1/2 landed in the #112
spike branch; Tier-3/4 design-only). Same architectural seam as the
structural-trigger spike (#112-A). Author: dev-fixer. Branch
`agent/dev-fixer-structural-trigger`.

**Operator directive:** *"RAM consumption is the most important, how can
we lower that?"* The ~2 GB resident is **rust-analyzer-dominated**. The
cargoless daemon itself is small; RA is the cost centre.

**Hard invariant (every tier):** none of this may change the **verdict
colour**. The authoritative green/red is the **cargo-check / F8-redo**
tier (`FlycheckEnded` + zero `severity:Error`); RA is the fast advisory
accelerator. `never-publish-red` / AC#4 stays fail-closed. Where a tier
*could* touch the verdict, that is called out and gated.

---

## Verdict summary

| Tier | What | Verdict |
|---|---|---|
| **1** | glibc arena cap (`MALLOC_ARENA_MAX=2`) + opt-in jemalloc preload, at the RA spawn site | **IMPLEMENT-NOW** ✅ landed, default-on, RSS-only, zero verdict effect |
| **2** | RA salsa LRU cap (`lru.capacity`, default 64) | **IMPLEMENT-NOW** ✅ landed, correctness-neutral (recompute) |
| **2′** | `cargo.buildScripts.enable=false` | **NEEDS-VALIDATION** — can cause spurious RA-native false-RED (verdict-colour effect, fail-safe but unusable on build.rs projects) |
| **2″** | `numThreads` cap | **v0.1-DESIGN / opt-in** — RSS↔throughput, correctness-neutral; bench-lead to sweep |
| **3** | proc-macro-off **as default** (−53% in #74) | **NEEDS-VALIDATION-PASS** — the auto-detect win is *already shipped*; unconditional-off needs an F8-redo RA-native-downrank change + proof |
| **4** | idle-evict RA between agent-edit-batches | **v0.1-DESIGN** — highest leverage (~2 GB reclaimed per idle gap); composes with the part-A trigger seam + AC#6; no-wrong-verdict proof below |

---

## Tier-1 — allocator (IMPLEMENTED, default-on, RSS-only)

**Site:** `crates/tf-core/src/analyzer.rs` — `rust_analyzer_command()`
now calls `apply_ra_allocator_env(&mut cmd)` before returning the
`Command`.

RA is heavily multithreaded; glibc malloc grows up to `8 × ncpu`
per-thread arenas and arena fragmentation is a dominant RSS contributor
with **zero** functional effect — RA *upstream ships jemalloc precisely
for this*, but the rustup/distro binary cargoless spawns links system
glibc malloc. `MALLOC_ARENA_MAX=2` is read by glibc malloc only; musl /
macOS ignore it (harmless no-op).

- Default: `MALLOC_ARENA_MAX=2` unless the operator already set it.
- Opt-in jemalloc preload: `TF_RA_JEMALLOC=1` (+ discovered
  `libjemalloc.so.2` or `TF_RA_JEMALLOC_SO`), never clobbering an
  existing `LD_PRELOAD`. Allocator *swap* is empirically safe (RA ships
  it) but kept opt-in for the spike pending bench-lead's delta.
- Escape hatch: `TF_RA_ALLOC=off`.

**No-wrong-verdict proof:** the env vars only change the *child's heap
arena strategy*. Analysis output (diagnostics, flycheck) is
byte-identical; the cargo-check/F8-redo authority and never-publish-red
are untouched. There is no path from arena count to verdict.

**Reclaim estimate:** glibc arena fragmentation on a many-thread process
is commonly 25–50 % of RSS; the cap typically reclaims a large fraction
of *that*. Exact figure = bench-lead (§Measurement).

---

## Tier-2 — RA cache bounding (LRU IMPLEMENTED; others analysed)

**Site:** `crates/tf-core/src/lsp.rs` — `lean_init_options()` now emits
`"lru": { "capacity": ra_lru_capacity() }` (default **64** = half RA's
built-in 128; `TF_RA_LRU_CAP` override, floor 16).

**No-wrong-verdict proof (LRU):** salsa LRU eviction → the evicted query
**recomputes** on next access → **identical** result. It cannot change a
diagnostic, the F8-redo verdict, or never-publish-red — it only trades a
little recompute latency/CPU for a hard memory cap. Under cargoless's
*batchy* agent-edit access pattern (one analysis per agent-edit-batch,
not per-keystroke), the recompute cost lands in the gap between batches
where latency is a non-issue. Correctness-neutral by construction.

**#74 knobs already default (verified, no change needed):**
`cachePriming.enable=false`, all `inlayHints.* = false/never`,
`hover/lens/completion/assist/references` trimmed,
`workspace.symbol` narrowed, `cargo.allFeatures=false`. These already
ship; Tier-2 adds only the LRU cap on top.

**Tier-2′ `cargo.buildScripts.enable=false` — NEEDS-VALIDATION (not
defaulted):** disabling RA's build-script execution does **not** break
the authoritative tier (the real `cargo check` flycheck subprocess runs
build scripts itself). BUT RA-native analysis then can't see
build-script-generated items → RA emits **false** `severity:Error`
("unresolved") with `source:"rust-analyzer"`. F8-redo's
`has_any_severity_error()` folds *any-source* severity:Error into
per-file RED ⇒ a build.rs project would flip **persistently RED while
cargo-check is green**. That is fail-*safe* (never false-GREEN, AC#4
intact) but a **verdict-colour change** that makes the tool report RED
on healthy code — violates the hard invariant. Excluded from default;
only viable bundled with the Tier-3 RA-native-downrank design.

**Tier-2″ `numThreads` cap — v0.1-design / opt-in:** caps RA's worker
pool → fewer thread stacks/arenas → lower peak RSS, at a throughput
cost. Correctness-neutral (slower, same result). Recommend an opt-in
`TF_RA_THREADS` knob and a bench-lead RSS/latency sweep before any
default; not implemented now to keep the landed default strictly
provably-safe.

---

## Tier-3 — proc-macro-off as DEFAULT (NEEDS-VALIDATION-PASS)

#74 measured RA proc-macro-off at **−53 % RSS** — the single heaviest
knob. The load-bearing argument the operator gave is correct: the
authoritative verdict is cargo-check/F8-redo, **not** RA-native, so
losing RA proc-macro fidelity (weak on `view!`-style macros anyway)
should be acceptable.

**The hard-gate finding (proof obligation discharged — it does NOT
trivially hold):**

- **GREEN gate is safe.** F8-redo GREEN ⟺ `FlycheckEnded` + zero
  `severity:Error`. The flycheck is a *real `cargo check` subprocess*
  that expands proc-macros itself, wholly independent of RA's
  `procMacro.enable`. A genuinely-green proc-macro project still
  produces a clean flycheck. never-publish-red / AC#4 unaffected.
- **RED gate is NOT safe by default.** F8-redo's broadened rule
  (`has_any_severity_error()`, the #55/F8-redo fix) makes **any-source**
  severity:Error drive per-file RED. With RA proc-macro-off, RA emits
  *false* unresolved/type errors for proc-macro-generated items
  (`source:"rust-analyzer"`) ⇒ **persistent false-RED on every
  proc-macro project** (Leptos `view!`, serde derive, …) — i.e. the
  dominant Rust+WASM use-case cargoless targets. That is a verdict-colour
  regression (fail-safe, but unusable).

**Crucial mitigating fact: the safe win is ALREADY SHIPPED.** #74's
`detect_proc_macro` / `InitOpts::from_env_and_project` already
*auto-detects* proc-macro deps from `Cargo.toml` and sets
`procMacro.enable=false` **whenever the project has no proc-macro
dependency**. So the −53 % is already captured for the non-proc-macro
case **today**. "proc-macro-off as default" only adds value for
projects that *do* use proc-macros — exactly the case where the false-RED
regression bites.

**Path to capture it there too (the validation pass):** make the
verdict's RED set provenance-aware *only when RA proc-macro is forced
off* — i.e. RED driven by `source:"rustc"` (authoritative) while
RA-native severity:Error is demoted to advisory in that mode. This is a
real, careful change to the F8-redo asymmetric-evidence rule (it
re-opens the exact #21↔F8-redo tension) and **requires its own
validation pass with the never-publish-red + no-false-GREEN proof**.
Flagged as a distinct task, not folded into this spike.

**Verdict:** NEEDS-VALIDATION-PASS. Recommend: keep the auto-detect
default (already optimal for non-proc-macro projects); open a separate
task for the proc-macro-projects case (RA-native-downrank + proof)
before any unconditional default flip.

---

## Tier-4 — idle-evict RA (v0.1-DESIGN, highest leverage, on-thesis)

**Observation (composes directly with part-A):** under the agent-input
model, between agent-edit-batches **no checks fire** (the part-A
CLOSED∧quiescent boundaries). Agent loops have long gaps (model
think-time, tool calls, the human reading) during which RA sits resident
at ~2 GB doing **nothing**. That idle resident is the dominant memory
state under real agent usage.

**Design:** after the last CLOSED batch's flycheck completes and the
tree has been idle for `T_idle` (configurable; no `ChangeBatch` and no
pending flycheck), **evict** the RA process. On the next `ChangeBatch`,
respawn before processing it.

**Leverage what already exists:** the AC#6 `Supervisor` +
`on_spawn` hook + `ReapOnDrop` (analyzer.rs) are *already* a transparent
kill-and-respawn machine — #3b/#44/#61 hardened it and `ac6_kill9`
tests that a `kill -9`'d RA is transparently restarted with files
re-`did_open`ed. Idle-evict is simply *triggering that same restart path
deliberately on idle* and deferring the respawn to the next batch. No
new lifecycle machinery; a new trigger into proven machinery.

**Deepest variant (recommended target):** cargo-check is a *transient
subprocess with zero resident cost* and is the sole authoritative
verdict source; RA is purely the advisory accelerator. So RA can be
fully on-demand/evictable and the **between-batch resident footprint
drops to ≈ the small cargoless daemon only** (~tens of MB vs ~2 GB).
The authoritative tier is unaffected — it never required RA resident.

**No-wrong-verdict proof:** eviction occurs **only at idle** (no pending
batch, last flycheck already completed → verdict already emitted and, if
green, already published). On the next batch the AC#6 path respawns RA,
the `on_spawn` hook re-`did_open`s every `.rs` (existing code), then the
normal didChange/(didSave-if-CLOSED)→flycheck→`FlycheckEnded`→verdict
flow runs — the **same** path as a post-AC#6-restart today, which
`ac6_kill9` already proves correct. The verdict is still produced solely
by the cargo-check authority; eviction changes *when RA is resident*,
never *what colour is computed*. never-publish-red holds (a stale green
pointer is only ever advanced by a fresh CLOSED-batch flycheck, never by
eviction).

**Tradeoff:** the first post-idle batch pays RA cold bring-up (the AC#1
~30 s budget worst-case). Under the agent model this is acceptable —
the agent is not blocked sub-second, and the cold-start is amortised
against the (typically minutes-long) idle gap that just reclaimed ~2 GB.
Mitigations to design: (a) **warm-evict** — preserve RA's on-disk
persistent cache dir so respawn is warm, not cold-reindex; (b) a
two-stage ladder: `SIGSTOP` suspend first (instant resume, modest RSS
reclaim via swap-out pressure) → full evict after a longer idle. The
recommended v0.1 design is full-evict + RA persistent-cache warm
restart.

**Reclaim estimate:** ≈ the entire RA RSS (~2 GB) for the duration of
every idle gap — under bursty agent usage (compute in bursts, long
think/tool gaps) this is the *dominant* memory state, so the
time-averaged reclaim is large.

**Verdict:** v0.1-DESIGN. Architectural, highest-leverage, on-thesis
with part-A; needs its own implement+validate cycle. Proof provided;
not in the bounded spike.

---

## Measurement (bench-lead — same harness run, gated on seam-ready)

bench-lead measures RA-child RSS (the Supervisor's child pid) over the
synthetic agent-edit trace (the §4.2 D-OPENCLOSED trace), comparing:

1. **control** — defaults pre-#112-B (`TF_RA_ALLOC=off`,
   `TF_RA_LRU_CAP=128`).
2. **Tier-1** — arena cap (default).
3. **Tier-1+jemalloc** — `TF_RA_JEMALLOC=1`.
4. **Tier-2 sweep** — `TF_RA_LRU_CAP` ∈ {32, 64, 128}; record RSS *and*
   per-batch recompute latency (the trade curve).
5. **Tier-1+2 combined** — the shipped default.

Metric: peak + steady RA RSS delta vs control, paired with the part-A
`structural_counters()` fired-check-reduction on the **same** run (one
harness pass yields both the CPU-check-elimination and the RSS curves).
Tier-3/4 are projected estimates here; bench-lead measures 1–5 (landed).

The bench hook for part-A (`ModelSession::structural_counters()`) and
the Tier-1/2 env levers are all live as of the seam-ready SHA.
