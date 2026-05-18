# MODEL-R NARRATIVE-FINALIZATION DIFF-MAP

**Status:** PREP groundwork (Stream E, #164). Staged on
`agent/docs-launch-lead-e2`. **This is scaffolding, not the narrative** —
it does not edit `README.md` / `ROADMAP.md` / `BLOG-DRAFT.md` /
`AC9-REVIEWER-PACKET.md`. It maps, per file:section, the Model-A→Model-R
edits and splits them into **SCOPE-INVARIANT (executable now)** vs
**NUMBER-GATED (PENDING #15 measured Model-R Leg-C v4)**. It is removed at
publish, exactly as the Phase-3 `NARRATIVE-FINALIZATION-DIFFMAP.md` was
(scaffolding served its purpose, then deleted in the finalization commit).

Authority for every Model-R claim: `docs/design/D-FLEET-SHARED-DAEMON.md`
(architecture, §1/§3/§9/§12/§16) and `/Users/iggy/.claude/plans/
fluffy-dreaming-allen.md` → "Launch-narrative rework" table. Cite by
file:section; never paraphrase a number.

---

## 0. The hard gate (carried verbatim from the Phase-3 discipline)

Per the plan exit criteria: the Model-R fleet-RAM story must be
**measured-not-extrapolated** (#15 bench characterization, Leg-C **v4**).
The Phase-3 narrative's `16 GB / 20-agent` projection carried a
**disclosed-extrapolation** caveat (`BLOG-DRAFT.md` L12-14); Model R's
"~1 GiB total / ~19×" is a **`D-FLEET §9` design estimate**, NOT yet
measured. **No NUMBER-GATED cell may be filled from D-FLEET; only from
#15's published v4 figures.** Filling one early is the
PENDING→FILLED-SOURCE-DRIFT class the AC9 §3 numbers gate exists to catch.

---

## 1. SCOPE-INVARIANT edits (executable now — fixed by design, not numbers)

These depend only on the *architecture* (frozen in D-FLEET) and the
*command surface* (frozen by #3 `serve --repo`), not on any measured
figure. Safe to draft now; they will not churn when #15 lands.

| # | File:section (cited anchor) | Model-A today | Model-R edit (scope-invariant) |
|---|---|---|---|
| S1 | `README.md` §"What cargoless v0 is (and isn't)" (L64) + ROADMAP L26 "v0 — what just shipped" | per-worktree headless checker; `cargoless watch` per WT | architecture = **one repo-scoped daemon**: `cargoless serve --repo <path>`, auto-discovers worktrees via `git worktree list`, LSP-overlay-multiplexes one RA, pinned-base+tree.cache decoupled, corun batching (`D-FLEET §3.1/§5/§6/§7`). *Description only — no RAM number here.* |
| S2 | `README.md` §"Quick start" (L160) — `cargoless watch` example | run `cargoless watch` in a project root | add the fleet path: `cargoless serve --repo <repo>` once for the whole repo (`D-FLEET §3.3`). v0 single-WT `watch` stays documented (Model R subsumes, does not remove it). |
| S3 | `README.md` §"Performance vs alternatives" → **Leg A** (L213-227) | 2.05× per-edit CPU vs `trunk serve`, two-source-confirmed | **UNCHANGED — carried verbatim.** Model R preserves the green-edge-rebuild model; the CPU headline is invariant (`D-FLEET §13` "Model R preserves the green-edge-rebuild model"). Explicitly re-assert "unchanged", do not re-derive. |
| S4 | `README.md` §"Performance vs alternatives" → **Leg C** header (L261 "fleet-scale (the agent-loop case)") | per-daemon ladder, BORDERLINE-fit framing | reframe the *prose* to the architectural lever (sharing one RA across N WTs is the structural win beyond per-daemon tuning, `D-FLEET §1/§2`). **The table cells stay NUMBER-GATED (§2 below).** |
| S5 | `ROADMAP.md` L3-8 status snapshot + L10-16 phase model + L101 "v0.1 deferred" | v0 shipped / v0.1 deferred / v1 parking lot | reshape to Model-R-as-launch: v0 (Model A) = superseded intermediate, never publicly launched; **Model R is the launch** (`plan` "Phase 4 public launch deferred until Model R lands"). v0.1 browser-adapter stays deferred (orthogonal). |
| S6 | `README.md` L39 "what cargoless **v1.0** delivers" **vs** `ROADMAP.md` L3 "v0 is feature-complete … v1 is the long-horizon parking lot" | **cross-surface version skew already on main** | **Reconciliation item (scope-invariant, operator-gated).** README already says v1.0; ROADMAP says v0/v1-parking. Model-R reshape must converge both to one story. Final tag (v0.2 vs **v1.0**) is the **operator decision** (`plan` "operator decides v0.2 vs v1.0; recommend v1.0"). Diff-map flags it; does not pick it. |
| S7 | `BLOG-DRAFT.md` §"The cargoless architecture: do less, trust more" (L102) + §"What we are honest about" (L378) | Model-A architecture; Tier-3-ladder-load-bearing caveat | architecture prose → Model R (`D-FLEET §3`); honesty caveat *reshapes* (per-daemon ladder → "Model R subsumes the ladder; per-daemon tuning is secondary") but **stays honest** — the reshape is structural framing, any residual per-cluster RA RAM figure is NUMBER-GATED. |
| S8 | `BLOG-DRAFT.md` §"Latency: two tiers, not one number" (L244) | dual-tier RA-hint ≤1s / cargo-check authoritative | **UNCHANGED — carried verbatim.** D-A2 dual-tier framing is Model-R-invariant (overlay multiplexing does not change the save→verdict tiering). Re-assert unchanged. |
| S9 | `AC9-REVIEWER-PACKET.md` §1 reviewer brief (L15) | "cargoless v0 … primary consumer is an AI agent" | add Model-R framing sentence: positioning is **fleet-of-any-scale agent-loop substrate** (`D-FLEET §1` 589-WT topology), repo-scoped daemon. The agent-loop thesis is invariant; the scale framing widens. *No number in the brief.* |
| S10 | `AC9-REVIEWER-PACKET.md` §3 publish-time checklist (L76) | per-claim numbers gate + appendix strip + link-liveness + D1 | add Model-R items: (a) every "~1 GiB / ~19× / Nx corun" cell traces to **#15 v4 published figure** (not D-FLEET); (b) `serve --repo` command strings match the shipped #3 CLI; (c) "v0/Model-A" residual-mention sweep (Model A is never publicly launched — no "ship v0" copy survives). Numbers-gate mechanism itself is unchanged. |

S3 + S8 are the **invariant-carry** items — the discipline is to *explicitly re-assert "unchanged"*, never silently re-derive (the FILLED→SOURCE-DRIFT class).

---

## 2. NUMBER-GATED cells — DO NOT FILL until #15 publishes measured Leg-C v4

Every cell below stays the literal token `PENDING #15 (measured Model-R
Leg-C v4 — NOT D-FLEET estimate, NOT extrapolation)` until bench-lead's
#15 lands with a cited source (`docs/bench/AC7-THROUGHPUT-REPORT.md §11`
v4, by the post-#147 inline-baseline-naming pattern). Reviewable *as
hedged claims* now; not final until sourced.

| # | File:section | The claim (D-FLEET estimate, for structure only — NOT to publish) | Gate |
|---|---|---|---|
| N1 | `README.md` Leg C table (L261+) / `BLOG-DRAFT.md` Leg C (L218) | fleet RAM ≈1 GiB total regardless of WT count (`D-FLEET §9` *estimate*) | PENDING #15 v4 measured @ ~20-WT tf-mv-shape |
| N2 | same | ~19× reduction vs Model-A naive per-WT-daemon (`D-FLEET §9`) | PENDING #15 v4 (paired with the measured Model-A baseline, same run) |
| N3 | `BLOG-DRAFT.md` Leg C prose | corun batching N×-multiplies verdict throughput on the common case (`D-FLEET §7`) | PENDING #15 v4 corun hit-rate / solo-fallback-rate measurement |
| N4 | `README.md`/`BLOG-DRAFT.md` use-case framing (S4/S9) | "validated at 589-worktree topology" | PENDING #15 topology-check evidence (the *word* "validated" is the gate, per the Phase-3 AC9 hedging rule) |
| N5 | `ROADMAP.md` Model-R section | per-cluster RA cardinality / RAM-profile fleet-active vs fleet-idle | PENDING #15 (workspace-cluster cardinality is an open empirical Q — `D-FLEET §14`) |

---

## 3. Invariants that travel (carried from Phase-3 honesty discipline)

1. **Leg A 2.05× CPU is frozen** — re-assert "unchanged under Model R", never re-measure/re-word (S3).
2. **Dual-tier latency is frozen** — D-A2 framing Model-R-invariant (S8).
3. **n=1-macro-heavy scope travels with any proc-macro-off claim** — if the Leg-B Tier-3 line survives the reshape, its `n=1` caveat (`BLOG-DRAFT.md` L210) travels with it.
4. **RAM is a ladder/architecture story, never one number** — Model R's "~1 GiB" is a *measured total under stated conditions*, not a headline scalar; conditions cited inline.
5. **Honesty caveats reshape but never soften** — "Model R subsumes the ladder" is a stronger *and still honest* claim; the methodology audit trail (`BLOG-DRAFT.md` L425) survives.
6. **Model A is never publicly launched** — no "ship v0" / "v0 launch" copy survives the reshape (S10c sweep); v0 = superseded internal intermediate.
7. **Three-layer discipline applies** — author self-satisfies → orchestrator STOP-class verify → dev-fixer honesty-backstop, on the *eventual* #164 narrative (not this scaffold; this is the diff-map the backstop will check the narrative against).

---

## 4. Execution order when #164 fires (post Model-R-functional + #15)

1. Apply all **§1 SCOPE-INVARIANT** edits (already specifiable now).
2. Fill **§2 NUMBER-GATED** cells from #15 v4 *only* — each cell's source cited inline (post-#147 inline-baseline-naming).
3. Reconcile **S6** to the operator's final version decision (v0.2 vs v1.0).
4. AC9-packet (§3) Model-R items applied; appendix strip; link-liveness; D1 + "no v0-launch residual" sweep.
5. Three-layer: self-satisfy → route diff to dev-fixer honesty-backstop with §3 invariants pre-loaded as the falsifiable criteria.
6. Delete THIS file in the finalization commit (scaffolding, like Phase-3's).

Cross-ref: `docs/design/D-FLEET-SHARED-DAEMON.md` (Model-R authority),
`docs/launch/AC9-REVIEWER-PACKET.md` (the packet this pre-stages a
Model-R delta for), `docs/launch/SEQUENCE.md` (launch sequence this
gates into), `docs/bench/AC7-THROUGHPUT-REPORT.md` §11 (the #15 v4
source for §2).
