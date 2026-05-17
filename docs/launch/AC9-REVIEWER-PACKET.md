# AC#9 REVIEWER-READINESS PACKET

**Status:** PREP. Staged on `agent/docs-launch-lead-prep` (#124).
Operational companion to the in-draft checklist at
`docs/launch/BLOG-DRAFT.md` → "Appendix — reviewer checklist (AC#9)".
This packet makes AC#9 **execute-ready the moment the narrative
finalizes** — it does not touch the narrative.

**AC#9 (verbatim):** *launch blog post reviewed by ≥2 people including
one outside the team.* Human-gated; lead owns the gate, this packet
makes it turnkey.

---

## 1. Reviewer brief (hand this to each reviewer as-is)

> **What you're reviewing:** the cargoless v0 launch blog post
> (`docs/launch/BLOG-DRAFT.md`). cargoless is a headless Rust+WASM
> dev-loop tool whose **primary consumer is an AI agent writing whole
> files atomically** (not a human streaming keystrokes). The post's
> positioning leads with that; the differentiator is **per-edit CPU
> throughput** (~½ of `trunk serve`), with explicit honesty that
> steady-state memory is rust-analyzer-dominated and **not** a v0 win.
>
> **What we need from you:** judge whether every claim is *true,
> traceable, and non-spin*. Specifically:
>
> 1. **No overclaim.** Each performance/capability statement must trace
>    to a v0 acceptance criterion or a cited measurement. Flag anything
>    that reads as marketing rather than evidence.
> 2. **Honesty intact.** The memory story must stay "RA-dominated ~2 GB
>    default, `--features` cuts it, v0.1 auto-narrows the default" —
>    flag any drift toward "lean/low RSS by default."
> 3. **Dual-tier latency.** Save→verdict is presented as two tiers
>    (fast RA hint ≤1s / authoritative cargo-check-bound), never a
>    single sub-1s headline. Flag any collapse back to one number.
> 4. **Scope honesty.** v0 = headless checker + latest-green publisher.
>    No browser/HTTP/WebSocket in v0; that's v0.1. Flag any
>    "`trunk serve` drop-in" claim made about v0.
> 5. **Agent-frame coherence.** Does the agent-loop positioning land,
>    and does it compose cleanly with the throughput story (not
>    contradict it)?
>
> **What is intentionally still blank:** numeric cells marked
> `_PENDING_` are gated on a two-source benchmark confirmation and are
> filled by a separate small follow-up commit — **review the claims
> and structure around them, not the absence of numbers.** A claim
> like "~half the CPU (PENDING two-source confirmation)" is reviewable
> *as a claim*; you are checking it is appropriately hedged, not that
> the number is final.
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

1. **Numbers gate.** Every `_PENDING_` replaced with the confirmed
   two-source figure (bench-lead #102/#116/#119 + dogfood-lead #117
   anchor). If *any* remain PENDING, **do not publish** — partial is a
   blocker, not a ship-with-caveat.
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
6. **D1-name consistency.** No `tftrunk` / `tf-cli` / `<pubname>=TBD`
   in the published copy (the `<pubname>` install-block residual is
   builder-infra's #96 lane — confirm it landed before publish).
7. **Agent-frame consistency.** Title + lead + "what we are honest
   about" all consistent with the agent-loop positioning (no leftover
   "human live-reload replacement" phrasing).
8. **Tone read.** No marketing fluff; every promise traceable to a v0
   AC; the memory-honesty + INCONCLUSIVE-speed + agent-frame bullets
   intact and not softened.
9. **Sign-off record.** Outside reviewer name + sign-off recorded in
   the publish-time-edit **commit message** (AC#9 evidence trail).

A single failed item = not publishable. AC#9 is binary.

---

## 4. Sign-off record (template — goes in the publish-time commit msg)

```
AC#9 sign-off
- Reviewer 1 (in-team): <name/agent>  — <approve|nits|changes> — <date>
- Reviewer 2 (OUTSIDE team): <name>   — <approve|nits|changes> — <date>
- Outside-reviewer constraint satisfied: YES
- Numbers gate: all _PENDING_ resolved two-source: YES (commit <sha>)
- Checklist §3 1-9: all PASS
Publish venue: <operator-chosen>   Frozen-copy SHA: <sha>
```

---

## 5. Readiness state

- Reviewer brief: **ready** (§1, hand-as-is).
- Publish-time delta: **ready** (§3, ordered, binary).
- Blocked on: narrative finalization (the diff-map gates) +
  lead/operator sourcing the outside reviewer. Nothing in this packet
  is itself gated — it is turnkey the moment the narrative lands.

Cross-ref: `docs/launch/NARRATIVE-FINALIZATION-DIFFMAP.md` (what
finalization does), `docs/launch/BLOG-DRAFT.md` appendix (the
per-claim checklist this operationalizes), `docs/launch/SEQUENCE.md`
(launch sequence this gates into).
