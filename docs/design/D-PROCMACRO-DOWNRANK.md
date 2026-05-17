# D-PROCMACRO-DOWNRANK — Tier-3: safe proc-macro-off (#126)

**Status:** default-off prototype LANDED on
`agent/dev-fixer-procmacro-downrank` (off `ebf8f5a` = spike + Tier-1/2 +
Tier-4; composes) + this load-bearing no-wrong-verdict proof. Completes
#118's v0.1-RAM-roadmap. Author: dev-fixer.

## 1. Why — the biggest single RAM lever

RA proc-macro-off ≈ **−53 % RSS** (#74 measurement) — the largest one
knob. The **safe half already ships**: #74's `detect_proc_macro`
auto-turns proc-macro off for projects with **no** proc-macro
dependency. #126 is the *validation pass* that makes proc-macro-off
**safe as a default on projects that DO use proc-macros** (Leptos
`view!`, serde derive, …) — the exact tasks cargoless's Rust+WASM
audience runs.

## 2. The hazard (why it isn't already a free default)

With RA `procMacro.enable=false` on a proc-macro project, RA cannot
expand the macros and emits **false** `severity:Error` ("unresolved",
type mismatches) with `source:"rust-analyzer"` for every
macro-generated item. The F8-redo rule (#55: *any-source*
`severity:Error` ⇒ per-file RED) folds that hallucination into a
**persistent false-RED on every proc-macro project**. Fail-safe (never
false-GREEN, AC#4 intact) but **unusable** — the tool reports RED on
healthy code.

## 3. The fix

A default-off flag `TF_RA_PROCMACRO_OFF=1` that does **two coupled
things** (they are only safe together):

1. `lsp.rs InitOpts::from_env_and_project`: force
   `proc_macro_enabled=false` regardless of `TF_PROC_MACRO` /
   auto-detect → the −53 % RSS.
2. `model.rs` `file_state_for` (pure, env-isolated): the authoritative
   per-file verdict is driven by **`source:"rustc"` (cargo-check) only**
   (`has_authoritative_error`) instead of the F8-redo any-source
   (`has_any_severity_error`). RA-native `severity:Error` is **demoted
   to advisory** — it still populates the `native` map, the diagnostics
   list, and the #21 advisory channel (visible to the user), it just no
   longer drives the authoritative tree colour. A bench/dogfood counter
   (`ModelSession::procmacro_downranked()`) tallies each suppressed
   would-be-false-RED.

## 4. No-wrong-verdict proof (load-bearing — in code via `file_state_for`)

The pivotal fact: **`cargo check` expands proc-macros itself**. The
flycheck is a real `cargo check` subprocess; its correctness is wholly
independent of RA's `procMacro.enable`. So the authoritative tier
remains the **complete** error oracle even with RA proc-macro-off.

- **No false-GREEN.** GREEN ⟺ `flycheck_done` ∧ no `auth` file Red.
  In downrank mode `auth` Red = `source:"rustc"` `severity:Error`. A
  green tree therefore means cargo-check *succeeded with proc-macros
  expanded*. Any real error (anywhere, incl. inside macro-generated
  code) ⇒ a rustc diagnostic ⇒ `auth` Red ⇒ not GREEN. GREEN semantics
  are **identical** to baseline. (Tested: `file_state_for(pd(1,0),
  true) = (Red,false)` — a real cargo-check error still drives RED.)
- **No false-RED.** RA-native hallucinations are `source:"rust-
  analyzer"`, excluded from `has_authoritative_error`, so they do not
  set `auth` Red. The Leptos-`view!` false-RED is removed. (Tested:
  `file_state_for(pd(0,1), true) = (Green,true)`.)
- **No missed real RED.** Every real compile error is caught by the
  complete cargo-check authority ⇒ rustc diagnostic ⇒ RED. Nothing
  real is lost.
- **never-publish-red intact.** `.cargoless/latest-green` advances only
  on a `BecameGreen` from `reconcile` (`flycheck_done` + zero `auth`
  Red) — unchanged. Eviction/downrank never advances it.
- **Default-off is byte-identical.** `TF_RA_PROCMACRO_OFF` unset ⇒
  `procmacro::enabled()==false` ⇒ `InitOpts` keeps #74 auto-detect and
  `file_state_for(_, false)` is the verbatim F8-redo any-source rule.
  (Tested: the three `downrank=false` cases equal pre-#126 behavior.)

### The honest residual — a **latency**, not a **correctness**, regression

F8-redo's *value-add* was *latency*: RA-native catches a genuine syntax
error *instantly*, before cargo-check finishes (the `let bad =`
reproducer). In downrank mode that accelerator is off, so a genuine
syntax error surfaces as RED at **cargo-check-completion** instead of
instantly. The **eventual verdict colour is identical**; only the
time-to-RED for a genuine syntax error increases. Under the agent-loop
model (batchy, long inter-batch gaps, not sub-second-latency-bound —
the D-A2 AC#2a/AC#2b framing) this is the right trade for −53 % RSS,
and it is **opt-in + measured**. It is explicitly *not* a correctness
regression: no false-GREEN, no false-RED, no missed RED, ever.

## 5. Bench / dogfood hook

`ModelSession::procmacro_downranked()` → count of folds where an
RA-native severity:Error was demoted out of the authoritative verdict
(the false-RED-suppression firing). Run a Leptos-`view!` fixture with
`TF_RA_PROCMACRO_OFF=1`: assert the tree is GREEN when cargo-check is
clean (baseline mode would be persistently RED), `procmacro_downranked
> 0`, and sample RA-child RSS for the −53 % delta. bench-lead correlates
this with the #116/#119 RSS curves; `(0)` when default-off so the
control arm is opt-in.

## 6. Verdict & roadmap placement

**FEASIBLE — prototype landed default-off, no-wrong-verdict proof
load-bearing in `file_state_for` + this doc, bench hook live.** This
closes #118's v0.1 RAM story:

| | v0 (shipped/default) | v0.1 (proven, default-off now, data-decides) |
|---|---|---|
| Working set | Tier-1 arena + Tier-2 LRU + #74 auto-detect | Tier-3 proc-macro-off −53 % (this) |
| Idle set | — | Tier-4 idle-evict ~2 GB/gap (#122) |
| Trigger | structural #112-A spectrum | + idle-evict compose |

Recommend bench-lead's Leptos-fixture RSS+correctness measurement
decide the v0-default question; the mechanism is correct and reversible
behind the flag regardless.
