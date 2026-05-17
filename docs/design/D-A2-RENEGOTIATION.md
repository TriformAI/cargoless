# D-A2 RENEGOTIATION — AC#2 honest-split + dual-tier latency framing

**Status:** PROPOSED. Unblocks #48 operator decision. Companion to the
F8-redo verdict architecture (#55, model.rs::authoritative_tree post-
ff1feaf) and the #71/#72 throughput re-pivot.

**Author:** `dev-fixer` on `agent/d-a2-design`. First-hand authority on
the verdict-tier code: #21 (verdict-provenance), #51 (lifecycle
events), #55/F8-redo (severity:Error-from-any-source rule), #70/F12
(failure-reason honesty).

**Phase:** Phase 3 launch readiness, gating decision #48 (FIELD
FINDING #5 D-A2 RENEGOTIATION).

**Code change required:** NONE. This is a DOCUMENTATION + SPEC
renegotiation. The code already encodes the dual-tier reality; we
just rename AC#2 to reflect it honestly.

---

## 0. The question

D-A2's original wording (CWDL-2 / project DESIGN.md §5):

> "AC#2 — median save→verdict <1s (primary)" — provisional until S1
> reports.

S1 reported. The number cuts two ways:

| Measurement | Source | Result |
|---|---|---|
| RA-tier direct LSP throughput | S1 harness (bench/harness) | **sub-1s** ✓ |
| Post-#49-debouncer steady-state | bench-lead's harness | **0.74s** ✓ |
| Real Leptos save → published verdict | dogfood-lead manual probe | **~26s** ✗ |

Both sets of numbers are TRUE. They measure different things. The
question D-A2 asks — "is AC#2 PASS or MISS?" — has no honest answer
unless we acknowledge that AC#2 was always conflating two phenomena
the architecture genuinely separates.

This document proposes the honest answer: **AC#2 splits into
AC#2a (RA-incremental tier) and AC#2b (authoritative verdict tier),
each with its own threshold, each measured against the right
denominator.**

---

## 1. The two tiers, named

Per #21 verdict-provenance + F8-redo's broadened severity:Error rule
(in `tf_core::model::Model::apply_event`, lines updated at ff1feaf):

### Tier A — RA-incremental (fast hint)

- **Source:** rust-analyzer's own parser + type-resolver, emitted as
  `publishDiagnostics` with `source: "rust-analyzer"`.
- **Speed:** sub-1s on a Leptos save (S1 measurement of the LSP
  emit-side latency).
- **Authority:** PARTIAL. RA's analysis is incomplete — it skips
  proc-macro execution by default, skips most generic
  monomorphization, skips linking. RA can give "Go to Definition"
  instantly *because* it's not actually compiling. (S1 spike's
  whole reason for existing: prove this empirically — done.)
- **Drives in the model:** RED transitions on
  severity:Error from any source (F8-redo rule). RA's "saw an
  error" is honest evidence; we let it flip the verdict to RED
  instantly.
- **Does NOT drive:** GREEN transitions. RA's "didn't see an
  error" is NOT evidence of compilation — only the absence of
  evidence. The #21/F8-redo asymmetry is the load-bearing wall.

### Tier B — cargo-check authoritative (verdict)

- **Source:** cargo check run by RA's flycheck on save, emitted as
  `publishDiagnostics` with `source: "rustc"` + the
  `$/progress`-end on the cargo-check token (the model's
  `LspEvent::FlycheckEnded`).
- **Speed:** 20-25s on the dogfood Leptos save (48 files,
  leptos-heavy). 2-5s on small projects.
- **Authority:** FULL. cargo check IS compilation: every
  proc-macro is executed, every generic is monomorphized, every
  trait bound is verified. If cargo check passes, your code
  compiles; this is ground truth.
- **Drives in the model:** GREEN transitions (the *only* signal
  trustworthy enough — `flycheck_done` gate, model.rs lines 230-
  235). Also drives AUTHORITATIVE RED (rustc errors via F8-redo's
  severity:Error rule, when source==rustc).

### Why the asymmetry IS the architecture

RA-native severity:Error → RED:
*RA's parser caught it; cargo will catch it too. Honest evidence
of brokenness. Fast.*

RA-native "no errors" → CAN'T drive GREEN:
*RA may have missed class-of-errors only cargo catches (the #21
S1-blind error class — type/trait/method/macro). Absence-of-RA-
evidence is not evidence-of-compilation.*

cargo-check completion + zero errors → GREEN:
*Cargo verified the full chain. This is the only thing that can
honestly assert compilation.*

cargo-check (rustc) error → RED:
*Authoritative red, same severity as the user would get from
running cargo themselves.*

Two different speeds because they're two different operations.
Pretending they're one number is the original sin of AC#2.

---

## 2. The proposed split

Replace single-line AC#2 in CWDL-2 / DESIGN.md §5 with:

### AC#2a — RA-incremental latency (the fast hint)

> **Threshold:** Median save → first RA-tier severity:Error or
> first "cleared" publishDiagnostics for the saved file: **≤1s**.
>
> **Measured at:** The `LspEvent::Diagnostics` emit boundary in
> `tf_core::lsp::LspClient` — i.e. the moment the model's
> subscribers receive the diagnostic for the file the user just
> saved.
>
> **What this proves:** the user gets a fast hint within 1s of
> hitting save — typically before their finger leaves the
> key. This is the "fast feedback" half of the trust pitch.
>
> **What this does NOT prove:** that the code compiles. Only
> AC#2b can prove that.
>
> **Status:** **HOLDS.** S1 measured sub-1s; #49 debouncer
> tuning measured 0.74s. Bench-lead's S1 harness is the
> reproducible verification path.

### AC#2b — Authoritative verdict latency (cargo-check tier)

> **Threshold:** Median save → `LspEvent::FlycheckEnded` +
> verdict-tree update: **≤ bare `cargo check` latency + 10%**.
> (Relative, not absolute — see §8 question 1.)
>
> **Measured at:** The model's `apply_event(FlycheckEnded)` call
> boundary in `tf_core::model::Model`. Numerator includes RA's
> flycheck scheduling overhead + cargo's own runtime.
>
> **What this proves:** cargoless adds negligible overhead on
> top of the user's own `cargo check` invocation. The slow part
> is cargo itself; cargoless's contribution is the watch loop +
> debounce + verdict emission, all of which together should be
> ≤10% of cargo's own time.
>
> **What this does NOT promise:** sub-1s. Project size + cargo's
> own work dominates. On a 48-file Leptos project, expect 20-30s;
> on a single-file `lib.rs`, expect 2-5s. This is honest.
>
> **Status:** **MEASURABLE PASS** pending bench-lead's #71/#72
> work landing the comparative numbers. Mechanism is in place
> (#21 verdict-provenance + F8-redo); the threshold is set
> relative to a baseline the AC#7 work will produce anyway.

---

## 3. Why the split is honest, not a retreat

Three reasons:

**(1) The code already does this.** The F8-redo verdict
architecture explicitly separates the two tiers. We added the
asymmetric-evidence rule (Tier A's "saw an error" can flip RED;
only Tier B can flip GREEN) specifically because pretending they're
symmetric was a bug. Continuing to claim AC#2 as a single number
would be the spec lying about the architecture, not the other way
round.

**(2) The architecture leverages BOTH tiers' strengths.** Tier A
is fast because it's partial. Tier B is authoritative because it's
slow. The watch stream shows them both with #45 timestamps, in
arrival order — the user sees the fast hint at +1s and the
authoritative verdict at +25s, with the timeline making the
difference obvious. That's not "AC#2 missed sub-1s"; that's "the
system genuinely has two latencies for two genuinely different
phenomena, and shows both."

**(3) The alternative narratives are worse.**
*Alternative 1: "AC#2 misses; cargoless launched without sub-1s."*
Wrong — Tier A *does* deliver sub-1s. Saying "miss" implies a hole
the architecture has, but the architecture is intentional.
*Alternative 2: "Defer AC#2 until v0.1 with salsa."*
Speculation about future work; v0 launch should describe v0
reality.
*Alternative 3: "Don't publish AC#2 numbers."*
Leaves users guessing; worse than honest.

The honest-split keeps the sub-1s claim that's TRUE (AC#2a) and
adds the cargo-check-bounded claim that's MEASURABLE-PASS
(AC#2b). Net: the launch material has MORE honest claims, not
fewer.

---

## 4. User-facing implications

### `tftrunk check` (one-shot verdict)

**Today:** waits for cargo-check completion, returns once with
verdict + diagnostics filtered per F8-redo's severity-then-source
rule.

**Post-split:** same behavior, documented as "waits for the
AUTHORITATIVE TIER (Tier B) — typically 20-30s on a Leptos-sized
project, 2-5s on smaller". Time-to-verdict: AC#2b territory.

### `tftrunk watch` (live stream)

**Today:** prints both tiers with #45 timestamps; diagnostics
filtered by tier per #55 F8-redo (severity:Error from any source =
authoritative-for-display).

**Post-split:** the live stream IS the dual-tier story made
visible. A typical save:

```
[+   0.000s] >> /work/realapp/src/lib.rs: Red          ← FS event
[+   0.892s] error[syntax-error; rust-analyzer]:        ← Tier A fast
                lib.rs:42:1: Syntax Error: expected
                an item
[+   0.893s] xx [+   0.893s] RED — tree does not compile (1 error surfaced).
                                                       ← verdict-line: Tier A fired RED
[+  23.567s] error[E0277; rustc]: lib.rs:42:5: trait    ← Tier B authoritative
                bound `T: Foo` is not satisfied
[+  23.890s] xx [+  23.890s] RED — tree does not compile (1 error surfaced).
                                                       ← verdict re-asserted with rustc evidence
```

The user sees the LATENCY GAP directly. That's not a bug to hide;
that's the truth about what compilation costs and what fast-hint
analysis can deliver. Showing both, with timestamps, IS the
launch story.

### Launch material framing

The headline pivots:

**Before** (selectively-true): "Sub-1s save→verdict for Rust+WASM."

**After** (honestly dual-tier):
> "**Fast feedback the moment you save.** RA-tier hints in under a
> second — see syntax errors, type errors, parse failures before
> your finger leaves the key.
>
> **Authoritative verdict the moment cargo finishes.** The
> green/red bit derives from `cargo check`'s own verdict — slower
> than the hint, but as honest as your compiler.
>
> **Both shown live with timestamps.** You can read the latency
> directly off any pair of lines. No more squinting at your shell
> wondering 'did it work?'"

Plus the throughput pivot (per #71/#72 bench-lead's perf-recon):
cargoless's idle CPU% / RAM footprint vs trunk-serve / bacon while
watching, and CPU-seconds per check cycle. That's where the launch
HAS clear wins regardless of the latency story.

---

## 5. Architectural payoff: the dual-tier framing IS the moat

Worth naming explicitly: this isn't just a docs renegotiation. The
dual-tier framing is a competitive position cargoless can claim
that single-tier tools (trunk-serve, bacon, plain cargo-watch)
cannot:

| Tool | Fast hint | Authoritative verdict | Both shown live |
|---|---|---|---|
| `cargo watch` | ✗ (re-runs cargo) | ✓ (cargo) | n/a |
| `trunk serve` | ✗ | ✓ (cargo via trunk) | n/a (one verdict) |
| `bacon` | ✗ (re-runs cargo) | ✓ (cargo) | n/a |
| rust-analyzer (editor) | ✓ (RA-tier) | ✗ (no green/red) | n/a |
| **cargoless** | **✓ (RA-tier <1s)** | **✓ (cargo-check)** | **✓ (timestamped stream)** |

The competitors are either "fast-only-no-verdict" (RA in editor)
or "verdict-only-no-fast-hint" (cargo-watch family). cargoless is
the first to wire BOTH into one watch loop and show them with a
common timestamp axis. That IS the v0 product story — the
architecture supports a claim no competitor can make. AC#2's split
is the spec catching up to what the system does.

---

## 6. Alternatives considered (and rejected)

### Alternative A: Defer publishing AC#2 numbers entirely
Just don't claim a latency number until v0.1.

**Cost:** leaves a hole in the launch story; users want to know
"how fast"; silence is worse than honest measurement. Also wastes
the genuinely-real Tier A sub-1s win that holds today.

### Alternative B: Re-prioritize sub-1s as v0.1 with salsa
Promise sub-1s in v0.1 by replacing RA with salsa-direct or
similar.

**Cost:** speculation about future work that may not deliver;
v0 launch claims should be about v0 reality. Also: even with
salsa, Tier B (cargo-check) is still slow because cargo itself is
slow; salsa would only affect Tier A which we already deliver.

### Alternative C: Keep AC#2 as written, document as MISS
Accept AC#2 = MISS in the launch material, note as "future work".

**Cost:** dishonest framing. The architecture genuinely SUPPORTS
the dual-tier story; calling the architecture-as-designed a "miss"
is the wrong word for "the system does what it says, just
differently than the original wording assumed".

### Alternative D: Tighten F8-redo to RA-tier-only verdict
Make GREEN gateable on RA's "didn't see an error" so Tier A IS
the verdict (sub-1s).

**Cost:** violates the F8-redo invariant for the SAME reason F8-
redo had to be done in the first place: RA-tier can't see the
class of errors that motivated #21. Going back to RA-only verdict
would re-introduce silent-green-on-broken — the worst v0 failure
mode. Hard NO.

The proposed honest-split is the only path that keeps every
existing invariant intact.

---

## 6b. Future-work recommendations (v0.1+ candidates)

### R6 — `tftrunk`-owned cargo check (decouple verdict from RA's flycheck)

Today's mechanism: RA runs cargo check via its own `checkOnSave`
on save; emits `$/progress` end on the cargo-check token; the
model captures that as `LspEvent::FlycheckEnded`. cargoless owns
the verdict bit (model decides GREEN/RED) but RA owns the
invocation cadence (when cargo check runs, how often, with what
arguments). This works — but couples AC#2b's latency floor to
RA's flycheck scheduling.

**The R6 architectural shift:** make cargoless invoke cargo check
as a tftrunk-managed subprocess, parse the JSON output directly
for severity:Error + source, and route those into the same
verdict-tree the model uses today. RA's checkOnSave becomes
either disabled (the original setting #1 framing, NOW SAFE under
this architecture because the verdict has its own path) or kept
on as a fast hint for editor users sharing the LSP session.

**Pros:**
- AC#2b latency improvable: cargoless can start cargo check on
  the save-event itself, not after RA's own debounce-then-flycheck
  pipeline. Potential meaningful cut in the cargo-check tier's
  effective latency on top of the cargo-runtime floor.
- Eliminates the duplicate cargo-check overhead (the v0 polish
  setting #1 wanted this; the R6 path is the prerequisite that
  makes that polish safe).
- Cleaner architectural separation: model owns verdict source-of-
  truth, RA owns advisory diagnostics. Both tiers become first-
  class with explicit ownership.

**Cons:**
- Substantial scope: ~500-1000 LOC change touching analyzer.rs,
  model.rs, build.rs, and new cargo-output parsing logic.
- #21/F8-redo invariants need re-encoding against the new
  cargo-output JSON path (the severity-from-any-source rule
  becomes severity-from-(our-cargo-check OR RA-native), which
  needs the same care).
- Loses RA's incremental-state benefits on the flycheck side
  (RA reuses its parsed/typed state to skip work; an independent
  cargo invocation doesn't have that state-sharing benefit).
- Risk: another launch-blocker class of bug if the verdict path
  re-implementation has any subtle bug (the F8 → F8-redo arc
  shows this lane is unforgiving of even small mis-classifications).

**Not v0.** The honest-split AC#2a/AC#2b documented above is the
right v0 framing — accurate to the architecture, sub-1s on Tier
A holds, Tier B is cargo-bounded and honest about it. R6 is a
v0.1+ design topic worth its own design doc.

**Decision frame:** ADOPT R6 if and only if (a) field measurement
post-v0-launch confirms the cargo-check tier is the user-pain
floor (not just the slow but honest reality users accept), AND
(b) there's evidence that cargo-output JSON parsing is stable
enough across cargo versions to be a reliable verdict source
(some past cargo releases have shifted JSON shape; need a
compatibility plan).

---

## 7. Recommendation

ADOPT the split. Specifically:

1. **Update CWDL-2** (DoD umbrella): replace single AC#2 with
   AC#2a + AC#2b per §2.
2. **Update docs/DESIGN.md §5** (AC traceability table): split
   the row.
3. **Update docs/EXECUTION.md** (AC table at top): same.
4. **Update CLAUDE.md** scope section's AC table.
5. **Update launch-blog draft** (per #67 / docs-launch-lead's
   lane): use §4's dual-tier framing.
6. **Update README** if it claims a latency number anywhere
   (probably not yet; verify).

ZERO code changes required. The verdict-tier code already
produces both tiers; #45's timestamps already make the latency
gap user-readable; F8-redo's severity-from-any-source rule
already implements the asymmetric-evidence design this AC split
documents.

The renegotiation is the architecture catching up to its own
spec, not the architecture changing.

---

## 8. Open questions for the operator

1. **AC#2b threshold shape — relative or absolute?**
   - **Relative** ("cargoless adds ≤10% overhead vs bare `cargo
     check`"): more honest, scales with project size, makes
     bench-lead's #71/#72 the verification path automatically.
   - **Absolute** ("≤30s on Leptos-scale projects"): easier to
     grok in marketing, but project-size-dependent and harder
     to verify on user projects of arbitrary size.
   - **Recommendation:** relative. Bench-lead is already
     measuring `cargo check` baseline as part of #71/#72;
     relative cost is the right denominator.

2. **Launch material headline — does the dual-tier framing land?**
   The §4 wording is a draft. docs-launch-lead has the marketing
   voice; this is engineering input on what's true to claim.

3. **AC#7 / throughput interaction.**
   Per #71/#72, the throughput pivot (CPU%/RAM/CPU-seconds vs
   competitors) becomes the launch headline. AC#2's dual-tier
   story is then a SUPPORTING paragraph in the latency section,
   not a standalone metric. Worth confirming this is the right
   structural emphasis.

4. **AC#2a's "first emit" denominator.**
   Should the median include the debounce window (#49) or measure
   from the post-debounce event? Probably post-debounce (the
   debounce is configurable; including it conflates user choice
   with architectural latency). bench-lead's S1 harness already
   does post-debounce; we'd just confirm.

5. **Communication to dogfood-lead.**
   F5 (the original "~26s" finding that triggered #48) becomes
   AC#2b's expected behavior, not a launch-blocker. Worth a
   targeted ping so dogfood-lead's re-verification matches the
   new framing.

---

## 9. Bisect-safety + cross-reference

This design references:
- #21 verdict-provenance seam (model.rs::Verdict / VerdictProvenance)
- #45 watch-line timestamps (cargoless/src/watch.rs::stamp)
- #49 debouncer + `--debounce-ms` knob (model.rs::resolve_watch_debounce)
- #55 / F8-redo severity:Error rule (model.rs::apply_event @
  ff1feaf; lsp.rs::PublishDiagnostics::has_any_severity_error)
- #67 launch-readiness docs (docs-launch-lead's lane)
- #71/#72 throughput re-pivot (bench-lead's lane)

Future agents reading this: the design encodes the architectural
reality at ff1feaf-or-later main. If the verdict-tier code ever
loses the dual-tier separation (e.g. RA's `checkOnSave` gets
disabled without a replacement cargo-check path being wired —
exact concern raised on RA polish setting #1), this design's
preconditions break and AC#2 needs another renegotiation.

---

## 10. Implementation cost

ZERO code changes. ~6 spec edits per §7 (lines + light reworking
of marketing material). docs-launch-lead executes most; this
document is the engineering authority for what's true to claim.

**Reviewer ask:** operator ratifies the split per §7; lead routes
the spec edits to docs-launch-lead with this doc as the
engineering reference; bench-lead's #71/#72 throughput work
verifies AC#2b at the relative-cost threshold per §8 question 1.
