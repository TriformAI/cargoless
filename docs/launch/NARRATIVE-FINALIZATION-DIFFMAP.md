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
honest facts preserved exactly) or **ADD** (new growth-path / spectrum
paragraph, numbers PENDING). No entry is "rewrite the Framing-C
verbatim block" or "re-open the dual-tier split" — those are frozen.

---

## 1. Gate preconditions (BOTH required before executing this map)

1. **Operator launch-scope decision** — v0-only vs v0+v0.1 (CWDL /
   #101). Determines whether the v0.1-RAM-roadmap + auto-narrow appear
   as *shipped-next* or *roadmap-only*.
2. **Combined numbers landed** — bench-lead Component-2 two-source
   confirmation (#102/#116/#119): the `~half` / `~2 GB` / `~75%`
   estimates → confirmed figures; the structural-trigger
   5/25/45% spectrum (#115/#116) anchored by dogfood-lead's real
   agent-OPEN-batch rate (#117). Until both, every numeric cell stays
   `_PENDING_` (unchanged discipline). **The #117 anchor is, per
   dogfood-lead's honest flag, a small-N number with survivorship-bias
   caveat — the narrative presents it as a caveated bracket-locator
   ("real loops sit near the low/mid end of bench-lead's validated
   5/25/45% spectrum"), NEVER as a confident point estimate. The
   diff-map slot for it is a *caveated-anchor* slot, not a number
   slot.**

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
| `## Performance vs alternatives` (L154) | **ADD (numbers) + REFRAME (lead only)** | The verbatim bench-lead Framing-C block + the dual-tier AC#2a/2b paragraphs are **FROZEN — do not touch**. Add *above* the qualitative table one paragraph: "cost unit is per agent-edit-batch; the structural-completeness trigger cargo-checks only confirmed-CLOSED batches" → cite the spectrum **as a caveated bracket (real loops near low/mid of 5/25/45%), not a confident point** (§3 caveated-anchor rule). Fill `_PENDING_` cells only when gate-2 clears. | `~half`/`~2 GB`/`~75%` (#102); 5/25/45% spectrum (#115/#116) × #117 caveated anchor |
| `## Workspace` (L286) | none | crate-name table is post-#97 (builder-infra). Out of scope. | — |
| `## Status` (L315) | **REFRAME** | If gate-1 = v0-only: status line stays. If v0+v0.1: add the v0.1-RAM-roadmap one-liner (Tier-1/2 shipped → Tier-3 validated + Tier-4 **prototyped+measured** per #122/#125 → fleet-scale RAM answer). | gate-1 |

### 2.2 `ROADMAP.md`

| Anchor (header) | Action | Compose-rule | PENDING-dep |
|---|---|---|---|
| `### v0 capabilities (available today on main)` (L31) | **REFRAME** | Add a bullet: agent-edit-batch as the cost unit; structural-trigger seam is **default-off spike in v0** (#113), not a v0 claim. | none |
| `### The nine acceptance criteria` (L53) | none | AC#2a/2b split already staged (C1). Frozen. | — |
| `### v0.1 perf follow-up — auto-narrow --features` (L119) | **ADD** | Extend this section into the full **#118 v0.1-RAM-roadmap growth-path**: v0 = Tier-1/2 (landed, verdict-neutral) + structural-trigger spectrum + two-source ~2× CPU; **v0.1 = validated Tier-3 (proc-macro-off-default + RA-native-downrank proof) + Tier-4 idle-evict-RA (~2 GB reclaimed per idle gap) = the fleet-scale RAM answer.** Source: `docs/design/D-RAM-TIERS.md` verdict table. Keep auto-narrow as the named single highest-leverage *flag* change; Tier-4 idle-evict as the highest-leverage *architectural* change. **Tier-4 framing precision (lead steer):** dev-fixer is pulling Tier-4 forward under #122/#125 as a **default-off prototype + no-wrong-verdict proof + measured RSS delta** — so the growth-path reads "**designed + prototyped + measured**", NOT "designed only". This strengthens the roadmap claim from aspiration to demonstrated-mechanism-deferred-by-scope; do not overstate it as *shipped* in v0 (it is default-off prototype, v0.1-DESIGN verdict). | D-RAM-TIERS lands on main (dev-fixer/bench ff); Tier-3 numbers PENDING; Tier-4 = #122/#125 prototype+proof+RSS-delta PENDING |
| `## v1 — parking lot` (L135) | none | unchanged | — |

### 2.3 `docs/launch/BLOG-DRAFT.md`

| Anchor (header) | Action | Compose-rule | PENDING-dep |
|---|---|---|---|
| Title `# cargoless v0: the dev loop that doesn't burn your CPU` (L26) | **REFRAME (candidate)** | Offer the lead a title variant that leads with the agent frame, e.g. *"the dev loop your agents can trust"* / keep CPU subtitle. **Do not unilaterally retitle** — present both; operator/lead picks (this is the headline, narrative-finalization-gated). | gate-1 + lead/operator title call |
| `## The problem nobody benchmarks` (L65) | **REFRAME** | Add the agent-input framing: the three-terminals-human picture still opens, but the turn is "and now the loop's primary user is an agent emitting whole-file writes in batches — per-keystroke optimization is the wrong axis entirely." Composes with existing throughput thesis. | none |
| `## The cargoless architecture: do less, trust more` (L101) | **REFRAME + ADD** | Recenter on the **agent-edit-batch / structural-completeness** model (D-OPENCLOSED): CLOSED-batch-gated cargo-check, OPEN/NEUTRAL skip, the F8-redo asymmetry preserved. This is the architectural spine of the agent frame. ADD the structural-trigger spectrum as the quantified payoff — **as a caveated bracket-locator, not a confident point (§3 caveated-anchor rule); honesty about the small-N #117 measurement is itself on-brand for the agent audience**. | D-OPENCLOSED on main; spectrum PENDING; #117 anchor = caveated small-N |
| `## Honest performance comparison` (L152) | **FROZEN** | The verbatim Framing-C block, dual-tier latency tables, memory-honesty bullet, bacon footnote, PENDING cells — **all frozen exactly as staged in C1.** Only fill `_PENDING_` when gate-2 clears. No prose rewrite. | gate-2 numbers |
| `## Roadmap` (L325) | **ADD** | Mirror ROADMAP.md §2.2: the v0.1-RAM-roadmap growth-path one-paragraph. Keep v0/v0.1/v1 phasing exactly. | gate-1; D-RAM-TIERS |
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
| 5/25/45% structural-trigger spectrum | README/BLOG architecture | bench-lead #115/#116 | spectrum validated |
| real agent-OPEN-batch rate — **honest-caveated small-N anchor, NOT a confident point** | same paragraph as spectrum | dogfood-lead #117 | #117 field-measure lands |
| Tier-3/Tier-4 RAM deltas | ROADMAP/BLOG v0.1-roadmap | dev-fixer #119/#122/#125; D-RAM-TIERS | tiers measured + D-RAM-TIERS ff'd to main |
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

If executing this map would require violating 1-6, **stop and
re-confirm with the lead** — the frame composes with the honest work;
it never overrides it.

---

## 5. One-commit execution checklist (when gates clear)

1. Branch off then-current main; `git fetch`; re-grep landed state.
2. Apply §2 REFRAME/ADD entries in file order; fill only gate-cleared
   PENDING slots from §3.
3. Verify §4 invariants intact (grep the verbatim block unchanged;
   grep no "lean by default"; dual-tier intact).
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
