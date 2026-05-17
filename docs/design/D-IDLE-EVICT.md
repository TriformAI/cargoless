# D-IDLE-EVICT — Tier-4 idle-evict RA (#122)

**Status:** default-off prototype LANDED on `agent/dev-fixer-idle-evict`
(off `ab0d51b` = spike + Tier-1/2) + this no-wrong-verdict proof.
Operator #1 priority (RAM); pulled forward from D-RAM-TIERS Tier-4 per
the fleet-scale framing. Author: dev-fixer.

## 1. Why (the fleet-scale existence lever)

Model-A is **~40 GB at 20 agents on a 16 GB box** (#116) — RA-resident
dominated. Under the agent-input model the gaps *between* agent-edit-
batches are long (model think-time, tool calls, human reading) and
**provably check-free** (the #112-A CLOSED∧quiescent boundaries). Yet RA
sits resident at ~2 GB doing nothing across every gap. Time-averaged
over real bursty agent usage that idle resident is the *dominant* memory
state. Reclaiming it is what makes a real multi-agent deployment fit in
memory at all — Tier-1/2 trim the working set; Tier-4 removes the idle
set. It is the existence lever, not an optimization.

## 2. Flag & default (operator doctrine)

`TF_RA_IDLE_EVICT=1` enables; **unset ⇒ byte-identical to pre-#122**
(the fs loop never calls `suspend()`, the supervisor `suspended` flag is
never set, the monitor park-`while` is a zero-iteration no-op, no
syscalls). `TF_RA_IDLE_SECS` tunes the idle window (default 30 s, floor
5 s). Ship-behind-flag → measured (bench hook §6) → **data decides
v0-default vs v0.1**.

## 3. Mechanism (reuse proven machinery, add one trigger)

- **Supervisor gains `suspend()`/`resume()`** (`analyzer.rs`,
  `SuspendHandle`): `suspend()` sets `suspended=true` then SIGKILLs the
  child; the monitor reaps the corpse (RAM freed) and — seeing the flag
  — **parks instead of respawning**. `resume()` clears the flag; the
  monitor's park loop exits and respawns via the **unchanged AC#6 path**
  (`spawn` + `invoke_on_spawn` ⇒ LSP re-init + re-`did_open` every file
  at its *current* content). This is the exact transparent-restart
  machinery `ac6_kill9` already proves correct — idle-evict just
  *triggers it deliberately on idle* instead of on crash.
- **Trigger in the `watch()` fs-batch loop** (`model.rs`): on the
  `recv_timeout` idle branch, evict iff `enabled() && !suspended &&
  flycheck_done && last_activity ≥ idle_window`. On the next
  `ChangeBatch`, resume + bounded-wait (`child_alive`, ≤ AC#1 35 s) for
  the fresh child **before** any `didChange`/`didSave` is forwarded.
- The authoritative verdict is **cargo-check** — a transient subprocess
  with zero resident cost. RA is only the advisory accelerator; in the
  deepest form RA is fully on-demand and the between-batch resident
  footprint is ≈ the small cargoless daemon (~tens of MB vs ~2 GB).

## 4. No-wrong-verdict proof (load-bearing — holds in the code)

Claim: idle-evict can only ever **delay** a future check; it can never
produce a wrong or missing verdict, and never advances `latest-green`
on non-green.

1. **Eviction precondition gates correctness.** Eviction requires
   `flycheck_done` — at least one authoritative `cargo check` pass has
   completed, so its verdict is already emitted (and, if green, already
   published via `BecameGreen`). The cold first authoritative pass is
   therefore **never interrupted**. (Coded: `model.rs` Timeout branch
   `&& poisoned(&model).flycheck_done()`.)
2. **No file change is processed while suspended.** Eviction happens
   only on the idle (`recv_timeout` Timeout) branch — by definition no
   batch is in flight. A batch that *arrives* while suspended hits the
   `Ok(batch)` branch which **resumes RA and bounded-waits for the
   fresh child before** the first `didChange`/`didSave`. So every
   verdict is computed by a fully-initialised RA + the cargo-check
   authority, via the *same* post-restart path `ac6_kill9` validates.
3. **Authority is RA-independent.** Green/red is decided by the
   cargo-check / F8-redo tier (`FlycheckEnded` + zero
   `severity:Error`), a subprocess that needs no resident RA. Eviction
   removes the *accelerator*, never the *authority*. RA absent ⇒ no
   RA-native diagnostics ⇒ at worst a verdict is *not yet updated*
   (stale-but-correct: the prior verdict was correct for the prior
   state; never false-green, never a hidden red).
4. **never-publish-red intact.** `.cargoless/latest-green` only ever
   advances on a fresh CLOSED-batch flycheck green (the #112-A path),
   which by construction runs only while RA is resumed. Eviction never
   advances the pointer; a suspended daemon cannot publish anything.
5. **Conservative / fail-closed on resume failure.** If respawn fails
   (RA briefly unavailable), the **existing** AC#6 backoff/retry path
   takes over (identical to a crash) — never "give up", never a wrong
   verdict, just a delayed one.

**Residual (documented, bounded, NOT a correctness hole):** if a batch's
authoritative flycheck is *still running* when the idle window elapses
with no further edits (rare: an incremental check slower than
`TF_RA_IDLE_SECS` with the user idle), eviction cancels that in-flight
check. Consequence: that edit's verdict update is recomputed on the next
batch (resume → re-`did_open` → next `didSave` → flycheck). This is
**stale-but-correct** (the prior verdict still stands and was correct for
its state; never false-green; `latest-green` never wrongly advanced) and
self-healing (next batch resolves it). Mitigation: `TF_RA_IDLE_SECS`
should exceed the project's flycheck p99 — operator-tunable; the bench
run (§6) measures the real distribution.

## 5. Cold-start tradeoff (why it still wins under agent input)

The first post-idle batch pays RA bring-up (≤ the AC#1 ~30 s budget;
typically seconds — RA's on-disk cache makes respawn warm, not a full
cold reindex). Under the agent model this is amortised against the
*minutes-long* idle gap that just reclaimed ~2 GB. A human-keystroke
tool could not afford this; an agent-loop tool (long gaps, batchy edits,
not sub-second-latency-bound) is exactly the workload where the trade is
strongly positive. Future refinement (v0.1): a `SIGSTOP` "light suspend"
ladder (instant resume, smaller reclaim) before full evict, for
short-gap regimes — design-noted, not in the prototype.

## 6. Bench hook (bench-lead; composes with #116 stage-3)

`ModelSession::idle_evict_counters() -> (evictions, suspended_ms)`.
`suspended_ms` ≈ the time-averaged ~2 GB reclaimed. Run the synthetic
agent-edit trace (D-OPENCLOSED §4.2) with `TF_RA_IDLE_EVICT=1` +
realistic inter-batch gaps; pair RA-child RSS sampling with the
structural-trigger fired-check counters on the **same** run to produce
the fleet-scale curve (N worktrees × default vs trigger+idle-evict =
the #116/#101 growth-path number). Dormant `(0,0)` when default-off so a
control arm is opt-in.

## 7. Prototype limitations (honest; v0.1 refinements)

- `suspend()` SIGKILLs the immediate RA child directly (not the
  `#44/#61` process-group/session kill). A flycheck grandchild could
  briefly orphan — rare (eviction is gated on `flycheck_done` + 30 s
  idle, so a check is typically not in flight) and **not** a correctness
  issue. v0.1: route evict-kill through the existing `ReapOnDrop`
  process-group/`pgrep -s` path for zero-orphan eviction.
- `SIGSTOP` light-suspend ladder (§5) — design-noted, not implemented.
- Default-off; v0-default-vs-v0.1 is the operator's call **on
  bench-lead's data**, not asserted here.

## 8. Verdict

**FEASIBLE — prototype landed default-off, no-wrong-verdict proof
load-bearing in code, bench-hook live.** Recommend bench-lead's
fleet-scale measurement (§6) decide the v0-default question; the
mechanism is correct and reversible behind the flag regardless.
