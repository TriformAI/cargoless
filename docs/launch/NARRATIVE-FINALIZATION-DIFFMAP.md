# NARRATIVE-FINALIZATION DIFF-MAP — the WHERE, not the edit

**Status:** PREP. Staged on `agent/docs-launch-lead-prep` (#124). This
document makes **no narrative edits** — it is the exact (file, anchor,
compose-rule, PENDING-dep) map so that, the instant the two gates
clear, launch-narrative finalization is **one fast coherent commit**
rather than a from-scratch pass under time pressure. Same discipline
that turned D1 into a small surgical commit: pre-stage the WHERE.

**Author:** `docs-launch-lead`. **Do not execute this map** until §1's
gates are both green and the lead signals.

---

## 0. The single composition rule (read first)

The operator's agent-loop frame is a **positioning-lead superset**, not
a rewrite of the staged honest-throughput work. Concretely:

> The verdict-honesty, ~½-CPU-vs-`trunk serve`, dual-tier AC#2a/2b, and
> RA-dominated-memory facts are **consumer-agnostic and already staged
> verbatim** (commit `1c46958`). They do **not** change. What changes
> is the *positioning lead*: cargoless is an **agent-dev-loop
> substrate** — its primary consumer is an AI agent writing whole
> files atomically (`Write`/`Edit` of a complete file), cost unit =
> **per agent-edit-batch, never per-keystroke** — *not* a human
> live-reload `trunk serve` replacement.

Every entry below is either **REFRAME** (positioning-lead only; staged
honest facts preserved exactly) or **ADD** (new growth-path paragraph,
numbers PENDING). No entry is "rewrite the Framing-C verbatim block" or
"re-open the dual-tier split" — those are frozen.

**Two landed dogfood findings (§1 gate-2) additionally constrain the
trigger and RAM entries to *honest-deflation* reframes:** the
structural-trigger is positioned as an idle-evict *enabler /
correctness property*, never a CPU-savings %; the RAM claim is a
*tiered ladder with per-rung gates*, never a single default number.
Composition-invariants 7 + 8 (§4) make these load-bearing.

---

## 1. Gate preconditions (BOTH required before executing this map)

1. **Operator launch-scope decision** — v0-only vs v0+v0.1 (CWDL /
   #101). Determines whether the v0.1-RAM-roadmap + auto-narrow appear
   as *shipped-next* or *roadmap-only*.
2. **Combined numbers landed** — the complete two-source picture:
   bench-lead stage-2 v2/§8 + #116 fleet-RAM curve, dogfood Item-3-redo
   deployed-field RAM ladder, #117 trigger-domain finding, dev-fixer
   #122 Tier-4 + #126. Until complete, every numeric cell stays
   `_PENDING_` (unchanged discipline).

   **Two landed dogfood findings reframed this map (honest deflation of
   our own hopes — on-brand for the agent audience; the narrative is
   *stronger* told straight):**

   - **The structural-trigger is NOT a direct-CPU headline.** #117
     (survivorship-free, N=16 fleet, 35/35-oracle-gated): for the
     trigger's *actual domain* — `.rs` files — this fleet's OPEN-rate
     is **0/97 = 0.0%**. Claude's `Write` emits complete whole-file
     Rust essentially always; it does not stream the
     skeleton-then-body OPEN intermediates the trigger skips. The
     naive **26.6% all-files figure is a Rust-lexer-on-non-Rust
     predicate-domain artifact** (`.sh`/`.md`/`.yml` heredocs/fences)
     and **must never appear as the trigger's expected savings
     anywhere in the narrative.** The trigger's launch positioning =
     *"enables safe idle-evict (the CLOSED∧quiescent boundary) +
     guarantees only-meaningful-states-cached"* — a correctness
     property, NOT a "skips N% of checks" CPU win. Its quantified
     value is the idle-evict fleet-RAM reclaim (bench-lead #116,
     PENDING), slotted there — never as a standalone CPU number.
   - **The RAM claim is a TIERED LADDER, not the default number.**
     dogfood Item-3-redo (deployed-field): default RA-polish
     ≈ **−20%** (a FINDING — the default *under*-delivers vs the
     30-50% hope); proc-macro-off ≈ **−56%** (gated on #126 making it
     safe for proc-macro projects); features-narrowed ≈ **−78%** +
     CPU collapse (v0.1 auto-narrow). The launch RAM headline is the
     **honest tiered ladder** with each rung's gate, NOT a single
     number (only −20% under-sells; −56/−78 as "default" over-sells).

The finalization commit is branched off **then-current main** (which
will include this prep branch via builder-infra's post-Phase-C docs
bundle ff + the RAM/structural-trigger work via their own ffs), not
off this stale prep branch. Re-grep landed state first (standing
discipline).

---

## 2. Per-file diff-map

Anchors are section headers as they exist on `agent/docs-launch-lead-prep`
HEAD (post-C1). Re-confirm line numbers at execution (C1/other docs
bundles may shift them; anchor on the **header text**, not the number).

### 2.1 `README.md`

| Anchor (header) | Action | Compose-rule | PENDING-dep |
|---|---|---|---|
| `# cargoless` + the `> **The codebase always knows…**` epigraph (L1-4) | **REFRAME** | Keep the vision line verbatim; add a one-clause gloss that the "you" includes **the agent**: "…tells you — or the agent driving the loop — the moment it doesn't." Do **not** drop the human reading; widen it. | none (frame is known) |
| `## What cargoless v0 is (and isn't)` (L33) | **REFRAME** | Add a single lead sentence: primary consumer = an agent writing whole files atomically; the `check`/`watch`/`build` surface is the agent-edit-batch verdict loop. Existing v0/v0.1 bullets unchanged. | none |
| `## Performance vs alternatives` (L154) | **ADD + REFRAME (lead only)** | The verbatim bench-lead Framing-C block + the dual-tier AC#2a/2b paragraphs are **FROZEN — do not touch**. Add *above* the qualitative table one paragraph: "cost unit is per agent-edit-batch; the structural-completeness trigger cargo-checks only confirmed-CLOSED batches — for whole-file agent writes (Claude `Write`) `.rs` OPEN is ≈0%, so the trigger's value is **correctness (only-meaningful-states-cached) + it enables safe idle-evict**, not a check-skip %." **Never quote a fired-check-reduction % as expected savings** (§4 inv-7). RAM presented as the §4-inv-8 tiered ladder, not a single number. Fill `_PENDING_` cells only when gate-2 clears. | RAM ladder (dogfood Item-3-redo + bench #119, PENDING); idle-evict reclaim (#116, PENDING); `~half`/`~2 GB`/`~75%` (#102) |
| `## Workspace` (L286) | none | crate-name table is post-#97 (builder-infra). Out of scope. | — |
| `## Status` (L315) | **REFRAME** | If gate-1 = v0-only: status line stays. If v0+v0.1: add the v0.1-RAM one-liner as the **ladder** (default ≈−20% today → proc-macro-off ≈−56% [#126/#127-gated] → features ≈−78% [v0.1 auto-narrow]; Tier-4 idle-evict prototyped+measured per #122/#125), never a single number. | gate-1 |

### 2.2 `ROADMAP.md`

| Anchor (header) | Action | Compose-rule | PENDING-dep |
|---|---|---|---|
| `### v0 capabilities (available today on main)` (L31) | **REFRAME** | Add a bullet: agent-edit-batch as the cost unit; structural-trigger seam is **default-off spike in v0** (#113), not a v0 claim. | none |
| `### The nine acceptance criteria` (L53) | none | AC#2a/2b split already staged (C1). Frozen. | — |
| `### v0.1 perf follow-up — auto-narrow --features` (L119) | **ADD** | Extend this section into the full **#118 v0.1-RAM-roadmap growth-path as the honest tiered ladder** (§4 inv-8): **v0** = Tier-1/2 landed (verdict-neutral) + structural-trigger as the **idle-evict enabler / only-meaningful-states-cached correctness property** (NOT a CPU-savings %; .rs OPEN ≈0% per #117) + two-source ~2× per-edit CPU; **the RAM ladder** = default RA-polish ≈**−20%** (deployed-field FINDING — the default under-delivers vs the 30-50% hope; state this plainly) → proc-macro-off ≈**−56%** *(v0.1, #126-gated to be safe on proc-macro projects)* → features-narrowed ≈**−78%** + CPU collapse *(v0.1 auto-narrow — the named single highest-leverage flag change)*; **v0.1 architectural** = Tier-4 idle-evict-RA (~2 GB reclaimed per idle gap; the structural-trigger's CLOSED∧quiescent boundary is what makes it safe — composition, not coincidence). Source: `docs/design/D-RAM-TIERS.md` verdict table + dogfood Item-3-redo (deployed-field) + bench #119 (harness per-tier) — **distinct sources, composed not conflated**. **Tier-4 framing precision:** dev-fixer pulls Tier-4 forward under #122/#125 as a **default-off prototype + no-wrong-verdict proof + measured RSS delta** — reads "**designed + prototyped + measured**", NOT "designed only"; do **not** overstate as v0-shipped (v0.1-DESIGN, default-off). | D-RAM-TIERS ff'd to main; RAM ladder rungs+gates PENDING (dogfood Item-3-redo + bench #119 + #126 for −56% + v0.1-auto-narrow for −78%); Tier-4 = #122/#125 prototype+proof+RSS-delta PENDING |
| `## v1 — parking lot` (L135) | none | unchanged | — |

### 2.3 `docs/launch/BLOG-DRAFT.md`

| Anchor (header) | Action | Compose-rule | PENDING-dep |
|---|---|---|---|
| Title `# cargoless v0: the dev loop that doesn't burn your CPU` (L26) | **REFRAME (candidate)** | Offer the lead a title variant that leads with the agent frame, e.g. *"the dev loop your agents can trust"* / keep CPU subtitle. **Do not unilaterally retitle** — present both; operator/lead picks (this is the headline, narrative-finalization-gated). | gate-1 + lead/operator title call |
| `## The problem nobody benchmarks` (L65) | **REFRAME** | Add the agent-input framing: the three-terminals-human picture still opens, but the turn is "and now the loop's primary user is an agent emitting whole-file writes in batches — per-keystroke optimization is the wrong axis entirely." Composes with existing throughput thesis. | none |
| `## The cargoless architecture: do less, trust more` (L101) | **REFRAME + ADD** | Recenter on the **agent-edit-batch / structural-completeness** model (D-OPENCLOSED): CLOSED-batch-gated cargo-check, OPEN/NEUTRAL skip, the F8-redo asymmetry preserved. Spine of the agent frame. Position the structural-trigger as **the idle-evict enabler + only-meaningful-states-cached correctness property** — NOT a check-skip headline: #117 (survivorship-free, N=16, oracle-gated) found `.rs` OPEN ≈**0%** for whole-file agent writes, so there is *no honest CPU-skip % to quote*; the naive 26.6% all-files figure is a Rust-lexer-on-non-Rust artifact (§4 inv-7). Tell the 0% straight — "for the way agents actually write (whole files), the trigger almost never *skips*; its job is to make idle-evict safe and guarantee we never cache a half-written state" — that honesty is on-brand. Quantified payoff lands as the idle-evict fleet-RAM reclaim (#116), in the RAM-ladder paragraph, not here. | D-OPENCLOSED on main; idle-evict reclaim #116 PENDING; .rs≈0% is a LANDED #117 conservative floor (cite with the Rust-lexer caveat) |
| `## Honest performance comparison` (L152) | **FROZEN** | The verbatim Framing-C block, dual-tier latency tables, memory-honesty bullet, bacon footnote, PENDING cells — **all frozen exactly as staged in C1.** Only fill `_PENDING_` when gate-2 clears. No prose rewrite. | gate-2 numbers |
| `## Roadmap` (L325) | **ADD** | Mirror ROADMAP.md §2.2: the v0.1-RAM-roadmap growth-path **as the honest tiered ladder** (default ≈−20% deployed-field FINDING → proc-macro-off ≈−56% [#126/#127-gated] → features ≈−78% [v0.1 auto-narrow] + Tier-4 idle-evict prototyped+measured). Never a single number (§4 inv-8). Keep v0/v0.1/v1 phasing exactly. | gate-1; D-RAM-TIERS; ladder rungs PENDING complete picture |
| `## What we are honest about` (L354) | **REFRAME** | Add one bullet: "Built for an agent loop; the human-facing `trunk serve` browser experience is explicitly v0.1, not v0 — we did not pretend the agent tool is a human live-reload replacement." Composes with the existing memory-honesty + INCONCLUSIVE-speed bullets (do not weaken those). Also fold dogfood-lead's **§gap-3 flagged→fixed→field-verified** data point as a "the two-tier method worked end-to-end" credibility line. | none (both inputs known) |
| `## Appendix — reviewer checklist (AC#9)` (L447) | **see AC9-REVIEWER-PACKET.md** | The packet supplies the delta; do not hand-edit here twice. | — |

### 2.4 `docs/DESIGN.md` / `docs/EXECUTION.md` / CWDL-2 (Plane)

- **DESIGN.md §5 / EXECUTION.md AC table:** AC#2a/2b split already
  staged (C1). At finalization, only **add** an "input model: agent
  whole-file-write; cost unit per-batch" one-liner cross-ref to
  D-OPENCLOSED — no AC-row changes.
- **CWDL-2 (Plane):** lead/operator lane (the lead folds Plane +
  CLAUDE.md in one coherent pass once scope decides). Not a
  staging-branch file edit; **flagged, not in this map's execution.**

---

## 3. PENDING-input ledger (each slot → source → what unblocks)

| Slot | Where it lands | Source | Unblock signal |
|---|---|---|---|
| `~half` per-edit CPU (→ confirmed ×) | README/BLOG perf tables | bench-lead #102 Component-2 | two-source CPU confirmation |
| `~2 GB` default RSS | README/BLOG memory framing | bench-lead #102/#119 §8.5 | two-source RSS (already solid; confirm wording) |
| `~75%` `--features` cut | README/BLOG + ROADMAP | bench-lead #102 | two-source |
| structural-trigger = **idle-evict enabler + only-meaningful-states-cached correctness** (NOT a CPU-skip %) | BLOG architecture / README perf | D-OPENCLOSED + dogfood #117 (LANDED: `.rs` OPEN 0/97 ≈0%, conservative floor, Rust-lexer caveat) | quantified value = idle-evict fleet-RAM reclaim, bench-lead #116 PENDING |
| **RAM tiered ladder** (composed, not conflated): default ≈−20% · proc-macro-off ≈−56% · features ≈−78% | ROADMAP §2.2 / README Status / BLOG Roadmap | default+rungs = dogfood Item-3-redo (deployed-field, LANDED FINDING); per-tier cross-source = bench-lead #119 (harness); D-RAM-TIERS verdict table | each rung's gate: −56% ⇒ #126/#127 safe-for-proc-macro; −78% ⇒ v0.1 auto-narrow; final published figures PENDING complete picture (bench stage-2 v2 + #116 + #126) |
| Tier-4 idle-evict (~2 GB/idle-gap) — designed+prototyped+measured | ROADMAP §2.2 / README Status | dev-fixer #122/#125 (default-off prototype + no-wrong-verdict proof + RSS delta) | #122/#125 RSS-delta PENDING; composes with the structural-trigger CLOSED∧quiescent boundary (not coincidence) |
| v0-only vs v0+v0.1 framing | README Status / BLOG Roadmap / title | operator (CWDL/#101) | scope decision |

**Rule:** any slot still PENDING at execution time stays `_PENDING_`
with its source noted inline — partial-fill is allowed (fill what's
confirmed, leave the rest), a single-source estimate is **not**.

---

## 4. Composition invariants (must NOT change at finalization)

1. The bench-lead **verbatim** Framing-C block — char-for-char as in
   `1c46958`. Never "improved."
2. Dual-tier **AC#2a/2b** split — frozen (C1). The agent frame does
   not collapse it back to one number.
3. **No "lean by default"** memory claim, anywhere. The agent frame
   does not resurrect it (agents care about fleet RSS *more*, not
   less — honesty is load-bearing for the agent audience too).
4. **PENDING discipline** — two-source rule; no single-source numeric
   substitution.
5. **Scope honesty** — v0.1 RAM-roadmap / structural-trigger are
   roadmap/spike, not v0 shipping claims, unless gate-1 says v0+v0.1.
6. No crate-name touches (post-#97, builder-infra lane).
7. **Structural-trigger is never a CPU-skip %.** Never quote a
   fired-check-reduction percentage as a this-fleet *expected* number.
   The honest anchor is `.rs` OPEN ≈**0%** (dogfood #117,
   survivorship-free, oracle-gated) — a CONSERVATIVE floor stated
   *with* the Rust-lexer-on-non-Rust caveat (the 26.6% all-files
   figure is a predicate-domain artifact, never the trigger's
   savings). The trigger's launch value is *correctness
   (only-meaningful-states-cached) + it enables safe idle-evict*;
   its quantified payoff lives only in the idle-evict fleet-RAM slot
   (#116).
8. **RAM is the honest tiered ladder, never one number.** Always
   default ≈−20% (deployed-field FINDING — the default under-delivers
   vs the 30-50% hope; say so) → proc-macro-off ≈−56% (v0.1,
   #126/#127-gated) → features-narrowed ≈−78% (v0.1 auto-narrow),
   each rung with its gate + provenance, deployed-field and harness
   sources **composed not conflated**. Quoting only −20% under-sells;
   quoting −56/−78 as the default over-sells. This *reinforces*
   inv-3 (the −20% default is the opposite of "lean by default").

If executing this map would require violating 1-8, **stop and
re-confirm with the lead** — the frame composes with the honest work;
it never overrides it. Inv-7/8 encode two landed dogfood findings
(honest deflation of our own hopes); the narrative is *stronger* told
straight.

---

## 5. One-commit execution checklist (when gates clear)

1. Branch off then-current main; `git fetch`; re-grep landed state.
2. Apply §2 REFRAME/ADD entries in file order; fill only gate-cleared
   PENDING slots from §3.
3. Verify §4 invariants intact: grep the verbatim block unchanged;
   grep no "lean by default"; dual-tier intact; **grep no
   structural-trigger fired-check-reduction % presented as expected
   savings (inv-7); RAM appears only as the tiered ladder with
   per-rung gates, never a lone default number (inv-8)**.
4. Run the EXECUTION.md self-gate checklist (docs-only ⇒ no rustfmt;
   confirm pure-`.md` change set).
5. One commit, conventional message, `Co-Authored-By` trailer; report
   branch+SHA to the lead for the AC#9 review gate.
6. Hand to AC#9 per `AC9-REVIEWER-PACKET.md`.

Cross-ref: `docs/design/D-A2-RENEGOTIATION.md` (dual-tier authority),
`docs/design/D-OPENCLOSED.md` (agent-input model authority),
`docs/design/D-RAM-TIERS.md` (v0.1-RAM-roadmap authority),
`docs/launch/AC9-REVIEWER-PACKET.md` (the review gate), C1 `1c46958`
(frozen Framing-C/dual-tier baseline).
