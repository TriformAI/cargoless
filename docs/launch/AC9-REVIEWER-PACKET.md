# AC#9 REVIEWER-READINESS PACKET

**Status:** PREP — **Model-R-reshaped (#164)**. Operational companion
to the in-draft checklist at `docs/launch/BLOG-DRAFT.md` → "Appendix —
reviewer checklist (AC#9)". This packet makes AC#9 **execute-ready the
moment the narrative finalizes** — it does not touch the narrative.
The §1 brief and §3 checklist below are reshaped for the Model-R
launch (one repo-scoped daemon, measured flat fleet-RAM); the AC#9
mechanism itself (≥2 reviewers, one outside, binary gate) is
**invariant**.

**AC#9 (verbatim):** *launch blog post reviewed by ≥2 people including
one outside the team.* Human-gated; lead owns the gate, this packet
makes it turnkey.

---

## 1. Reviewer brief (hand this to each reviewer as-is)

> **What you're reviewing:** the cargoless launch blog post
> (`docs/launch/BLOG-DRAFT.md`). cargoless is a headless Rust+WASM
> dev-loop tool whose **primary consumer is an AI agent writing whole
> files atomically** (not a human streaming keystrokes), positioned
> as a **fleet-of-any-scale agent-loop substrate**: one repo-scoped
> daemon (`serve --repo`) multiplexing a *single* warm rust-analyzer
> across every worktree (Model R). The agent-loop thesis is invariant;
> the scale framing widens to the fleet. Two differentiators: (1)
> **per-edit CPU throughput** (~½ of `trunk serve`, *unchanged under
> Model R*), and (2) the **measured flat fleet-RAM collapse** — one
> shared RA, ≈1 GiB total flat as worktrees multiply, with explicit
> honesty about its fixture-dependence and measured-to-N=20 bound.
>
> **What we need from you:** judge whether every claim is *true,
> traceable, and non-spin*. Specifically:
>
> 1. **No overclaim.** Each performance/capability statement must trace
>    to a v0 acceptance criterion or a cited measurement. Flag anything
>    that reads as marketing rather than evidence.
> 2. **Honesty intact (Model R).** The memory story must stay the
>    *measured structural flat collapse* — one shared rust-analyzer,
>    ≈1 GiB total flat across N∈{1,2,4,8,16,20} active worktrees
>    (`AC7-THROUGHPUT-REPORT §11.4`) — with its **fixture-dependence**
>    and **measured-to-N=20 / projected-beyond** caveats present; the
>    per-RA tier ladder is a *secondary* constant-factor, not the
>    fleet lever. Flag any drift toward "lean/low RSS by default", any
>    collapse of the structural-vs-absolute distinction, any
>    re-attribution of the fleet figure to a `D-FLEET` estimate or
>    "extrapolation", or any un-hedged 589-WT "validated" claim.
> 3. **Dual-tier latency.** Save→verdict is presented as two tiers
>    (fast RA hint ≤1s / authoritative cargo-check-bound), never a
>    single sub-1s headline. Flag any collapse back to one number.
> 4. **Scope honesty.** The launch = headless checker + latest-green
>    publisher delivered as one repo-scoped daemon (`serve --repo`).
>    No browser/HTTP/WebSocket — that is still the deferred next
>    phase. Model A (the per-worktree `watch` daemon) is a superseded
>    internal intermediate, **never publicly launched**. Flag any
>    "`trunk serve` drop-in" claim made about the launch, and any
>    "we're shipping v0 / v0 launch" residual copy.
> 5. **Agent-frame coherence.** Does the agent-loop positioning land,
>    and does it compose cleanly with the throughput story (not
>    contradict it)?
>
> **On the numbers (Model R):** the headline figures are **measured,
> not pending** — per-edit CPU is two-source-confirmed
> (`AC7-THROUGHPUT-REPORT §8.5`, Δ≈1%) and the flat fleet-RAM collapse
> is measured to N=20 (`§11.4`, Model-R Leg-C v4). What you are
> checking is **traceability and hedging**: every fleet-RAM figure
> must cite §11.4 (never a `D-FLEET` design *estimate*, never
> "extrapolation"); the 589/617-worktree fleet must read as a stated
> *projection*, never "validated"; the FF-A shutdown finding must be
> disclosed with its accurate mechanism (proven Supervisor reap not
> invoked on the serve-loop SIGTERM path — *not* a RAM leak, *not*
> #183/GracefulShutdown, no "~10 GiB"). Any publish-time link-liveness
> placeholder is filled by a separate small follow-up commit — review
> the claims and structure, not those placeholders.
>
> **Outside reviewer:** at least one of you must be **outside the
> agent team**. Your job is the cold-read: does this read as honest
> engineering communication to someone with no context? Marketing
> fluff, unsupported superlatives, and "wait, that doesn't follow"
> are exactly what we want flagged.
>
> **How to return feedback:** inline comments or a list keyed to
> section headers. Approve / approve-with-nits / changes-requested.

---

## 2. Reviewer selection (lead fills at gate time)

| Slot | Constraint | Candidate | Status |
|---|---|---|---|
| Reviewer 1 (in-team OK) | technical accuracy; knows the AC set | _lead-assigned_ | ⏳ |
| Reviewer 2 (**must be outside the agent team**) | cold-read; honest-communication judgment; no project context assumed | _lead/operator-sourced_ | ⏳ |

AC#9 is **not satisfiable by two in-team reviewers** — the
outside-reviewer constraint is load-bearing and explicitly re-checked
in the publish-time gate (§3).

---

## 3. Publish-time-edit checklist delta

The BLOG-DRAFT appendix already carries the per-claim checklist. This
is the **delta** the publish-time edit performs, in order. (The
appendix block itself is removed at publish — it is scaffolding.)

1. **Numbers gate (Model R).** Every fleet-RAM figure (≈1 GiB flat,
   ≈19–30× collapse, N∈{1,2,4,8,16,20}) traces to **`AC7-THROUGHPUT-
   REPORT §11.4`** (#15 Model-R Leg-C v4 *measured* — **NOT** a
   `D-FLEET` design estimate, **NOT** an extrapolation); per-edit CPU
   traces to **§8.5** (two-source, Δ≈1%). If *any* figure still reads
   as a `D-FLEET` estimate, an extrapolation, or `_PENDING_`, **do not
   publish** — partial is a blocker, not a ship-with-caveat. **Numeric
   re-traceability**: every cited figure re-checked against its named
   source via the post-#147 inline-baseline-naming pattern; drift
   across multiple occurrences is the marketing-creep signature (#145
   CATCH-1 class — the gate must catch FILLED→SOURCE-DRIFT, not just
   PENDING→FILLED). The numbers-gate *mechanism* is unchanged; only
   the authoritative source moved (two-source-PENDING → measured
   §11.4 for the fleet figure).
2. **Verbatim-block reconciliation.** The bench-lead Framing-C verbatim
   paragraph is reconciled with the confirmed numbers (kept verbatim
   until the numbers exist; not silently "improved" before then).
3. **Appendix strip.** Remove the entire
   "## Appendix — reviewer checklist (AC#9)" section.
4. **Link-liveness.** Every GitHub link 200s on the public repo:
   `ROADMAP.md`, `CONTRIBUTING.md`, `docs/dogfood/PHASE-2-REPORT.md`,
   `docs/design/D-A2-RENEGOTIATION.md`, `docs/design/D-OPENCLOSED.md`,
   `docs/design/D-RAM-TIERS.md` (the last two only if referenced in the
   final narrative).
5. **Repo-URL audit.** Zero `forgejo.triform.dev` URLs in
   contributor-facing copy (Forgejo is internal-CI only;
   contributor-facing = `github.com/TriformAI/cargoless`).
6. **D1-name consistency.** No `tftrunk` / `tf-cli` / `tf-proto` /
   `tf-cas` / `tf-core` / `<pubname>=TBD` in the published copy
   (post-#97 full one-token brand on `main`; D1-completeness
   CI-enforced forward by `scripts/d1-drift-guard` (#96), but the
   guard is out-of-scope for `docs/` by design — this checklist line
   is the publish-surface gate).
7. **Agent-frame consistency.** Title + lead + "what we are honest
   about" all consistent with the agent-loop positioning (no leftover
   "human live-reload replacement" phrasing).
8. **Tone read.** No marketing fluff; every promise traceable to an
   acceptance criterion or a cited measurement; the memory-honesty +
   measured-flat-fleet + dual-tier-latency + agent-frame bullets
   intact and not softened.
9. **Model-R delta sweep (added #164).** Three Model-R-specific
   checks, each binary:
   - **(a) Source-of-truth.** Every "≈1 GiB / ≈19–30× / corun" cell
     traces to `AC7-THROUGHPUT-REPORT §11.4` (#15 v4 *measured*), not
     a `D-FLEET §9` design estimate and not an extrapolation. (This
     is the PENDING→FILLED-SOURCE-DRIFT class the §3.1 gate exists to
     catch, applied to the Model-R numbers.)
   - **(b) Command-surface.** Every `cargoless serve --repo …` string
     matches the shipped #3 CLI exactly; the single-tree `watch` path
     is still documented (Model R subsumes, does not remove it).
   - **(c) No-v0-launch-residual.** Sweep for "ship v0" / "v0 launch"
     / "launching v0" copy — **zero survives** (Model A is a
     superseded internal intermediate, never publicly launched; the
     launch is Model R). The public version tag (`v1.0` vs `v0.2`)
     is left as the operator's call — no tag asserted in copy.
   - **(d) FF-A accurate-mechanism.** The shutdown finding is
     disclosed with the proven rust-analyzer Supervisor reap
     discipline (FF #3b/#44/#61/#128) not invoked on the serve-loop
     `SIGTERM` path; #198 (@`baeac6b`) restores it; zombies/0-RSS,
     **NOT a RAM leak**. Zero `#183`/GracefulShutdown mis-attribution;
     the retracted "~10 GiB" figure must not appear.
10. **Sign-off record.** Outside reviewer name + sign-off recorded in
   the publish-time-edit **commit message** (AC#9 evidence trail).

A single failed item = not publishable. AC#9 is binary.

---

## 4. Sign-off record (template — goes in the publish-time commit msg)

```
AC#9 sign-off
- Reviewer 1 (in-team): <name/agent>  — <approve|nits|changes> — <date>
- Reviewer 2 (OUTSIDE team): <name>   — <approve|nits|changes> — <date>
- Outside-reviewer constraint satisfied: YES
- Numbers gate: fleet-RAM traces to §11.4 measured (no D-FLEET-
  estimate / no extrapolation residual); CPU two-source §8.5: YES
  (commit <sha>)
- Checklist §3 1-10 (incl. Model-R delta sweep 9a-d): all PASS
Publish venue: <operator-chosen>   Frozen-copy SHA: <sha>
```

---

## 5. Readiness state

- Reviewer brief: **ready, Model-R-reshaped** (§1, hand-as-is).
- Publish-time delta: **ready, Model-R-reshaped** (§3, ordered,
  binary, 1-10 incl. the #164 Model-R delta sweep).
- Blocked on: #164 narrative finalization landing on `main` (the
  Model-R reshape of README/ROADMAP/BLOG-DRAFT) + the dev-fixer
  honesty-backstop + lead/operator sourcing the outside reviewer.
  Nothing in this packet is itself gated — it is turnkey the moment
  the Model-R narrative lands.

Cross-ref: `docs/design/D-FLEET-SHARED-DAEMON.md` (Model-R
architecture authority), `docs/bench/AC7-THROUGHPUT-REPORT.md §11.4`
(the measured fleet-RAM source the numbers gate enforces),
`docs/launch/BLOG-DRAFT.md` appendix (the per-claim checklist this
operationalizes), `docs/launch/SEQUENCE.md` (launch sequence this
gates into). The Model-R narrative diff-map
(`MODEL-R-NARRATIVE-DIFFMAP.md`) was the execution scaffold and is
**removed in the #164 finalization commit** (like the Phase-3
diff-map before it).
