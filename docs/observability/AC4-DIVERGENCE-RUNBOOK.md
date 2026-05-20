# AC4 Divergence Alert Runbook

**Audience:** operator on-call for cargoless production fleet (incl.
future-you, future-operators who weren't in this session).
**Companion design doc:** [`docs/design/D-INC2-OBSERVABILITY.md`](../design/D-INC2-OBSERVABILITY.md) §1.5 + §3.4.
**Status:** runbook authored 2026-05-20. The alert that drives this
runbook becomes LIVE-FIREABLE when Wave-2 (Increment-5 5d) metrics
land. Until then, the AC4 invariant is verified by structural source
fix (#247) + keystone-invariant test (Wave-2 in flight) — NOT by an
alert. See "Pre-Wave-2 verification status" below.

---

## TL;DR

**If this alert fires: STOP. Do NOT restart the daemon. Do NOT clear
the alert. Page dev-fixer (or the equivalent code-owner on-call) — a
proven-core-precondition is violated at some integration seam, and the
fix is a source-level change at that seam.** Detailed steps follow.

---

## 1. The alert

### 1.1 Definition

```
ALERT  cargoless_ac4_divergence
EXPR   sum(cargoless_overlay_reset_total)
       - (sum(cargoless_ra_restart_total) + sum(cargoless_initial_spawn_total))
       != 0
FOR    5m
LABELS severity=critical, signal=stop-class-regression
```

Aggregation is across all `cluster_hash` labels (the fleet-wide
divergence). Per-cluster slicing is available via the dashboard's AC4
panel (see [`cargoless-dashboard.json`](cargoless-dashboard.json) →
"AC4 Divergence" panel).

The invariant is **identically 0 forever** under correct operation —
every RA (re)spawn MUST be followed by exactly one
`OverlayMultiplexer::reset()` BEFORE any subsequent `switch_to` for
that cluster. The metric divergence detects any future regression of
that structural invariant.

### 1.2 What "5m" means

Five minutes is the observation window. The structural invariant is
instantaneous (per-event ordering); the 5m window absorbs transient
ordering effects (e.g. the metric scrape captures a `ra.spawn` counter
update before its companion `overlay.reset` counter update — a normal
sub-second ordering that resolves within one scrape cycle). A
DIVERGENCE THAT PERSISTS BEYOND 5m IS A REAL DEFECT, not a scrape
race.

---

## 2. What this alert means

cargoless's verdict-correctness is anchored by a load-bearing
invariant — see [`docs/design/D-INC2-OBSERVABILITY.md`](../design/D-INC2-OBSERVABILITY.md)
§1.5 for the design rationale. In short:

- The proven `OverlayMultiplexer` core relies on `reset()` being
  called at the seam where the LSP client is (re)swapped.
- Without that reset, a post-respawn `switch_to` would attempt
  incremental `didClose`/`didChange` against an RA that doesn't know
  about the prior `didOpen`s — silently producing **wrong-tree
  verdicts** (false-GREEN by attribution).
- The structural fix at #247 (`ClusterDriver::reset_after_respawn`
  + `Ctrl::Spawned` wire fix) landed this contract.
- This alert detects any future regression of the contract at any
  NEW integration seam (e.g. a future "warm-restart" optimization, a
  new RA-pool manager, a refactor that splits the spawn-and-reset
  pairing).

**The defect class:** "proven-core-precondition-violated-at-integration-seam"
(see operator-fleet memory shards). The fix-shape is **always**:
restructure the seam so the precondition is RESTORED, never silenced
by adding a defensive guard inside the proven core.

**What this alert is NOT:**

- NOT a "restart the daemon" runbook. Restart will MASK the divergence
  by resetting the counters at process boundary, hiding the underlying
  defect.
- NOT a "the daemon is wedged" alert. The daemon is producing verdicts
  — they're just (potentially) wrong-tree, which is worse than wedged.
  Verdict-honesty is the load-bearing claim cargoless makes; this
  alert is its trip-wire.
- NOT a "tweak the threshold" alert. The threshold is `!= 0`. There is
  no operational drift that would legitimately produce a non-zero
  divergence.

---

## 3. Diagnose

### 3.1 Confirm the alert is genuine (not a metric-source bug)

Before paging anyone: verify the divergence reflects a real
event-ordering violation, not a metric-emission bug.

```
# Sanity: each of the three counters should be ≥ 0 and non-decreasing.
sum(cargoless_overlay_reset_total)
sum(cargoless_ra_restart_total)
sum(cargoless_initial_spawn_total)

# The fleet's total span emission for the corresponding keystone events
# (Wave-1 surface, unaffected by the metric emission bugs Wave-2 could
# introduce):
count(rate({service.name="cargoless", name="overlay.reset"}[5m]))
count(rate({service.name="cargoless", name="ra.spawn"}[5m]))
count(rate({service.name="cargoless", name="ra.respawn"}[5m]))
```

If the keystone-EVENT counts agree with each other (event_overlay.reset
== event_ra.spawn + event_ra.respawn) but the METRIC counts diverge,
it's a metric-emission defect, NOT a structural defect. File a
DOCS/BUG ticket on the Wave-2 metric instrumentation, NOT a source-fix
on the cluster driver.

If the keystone-EVENT counts ALSO diverge — proceed to §3.2.

### 3.2 Find the offending respawn

The structural property is per-cluster: every cluster_hash should
satisfy `overlay_reset == ra_spawn + ra_restart` independently. Drill
into the per-cluster divergence:

```
sum by (cluster_hash) (cargoless_overlay_reset_total)
- (sum by (cluster_hash) (cargoless_ra_restart_total)
   + sum by (cluster_hash) (cargoless_initial_spawn_total))
```

One or more `cluster_hash` values will show non-zero divergence.
Pick the one with the largest absolute divergence (the most-violated
cluster). For that cluster_hash, in SigNoz trace search:

```
service.name = "cargoless" AND cluster_hash = "<the_hash>" AND
name IN ("ra.respawn", "ra.spawn", "overlay.reset")
ORDER BY timestamp DESC
LIMIT 100
```

You're looking for the **specific `ra.respawn` (or `ra.spawn`) event
that is NOT followed by an `overlay.reset` event for the same
cluster_hash before the NEXT `overlay.switch` span for that cluster**.

### 3.3 Find the consequent false-attribution span

For the offending respawn at time T, search for the NEXT
`verdict.publish` span on a worktree belonging to the affected
cluster_hash, anywhere in `[T, T + 5min]`:

```
service.name = "cargoless" AND cluster_hash = "<the_hash>" AND
name = "verdict.publish" AND timestamp > <T>
ORDER BY timestamp ASC
LIMIT 5
```

The `respawn_generation` attribute (Wave-1 carries; Wave-2 will
elevate to a metric label) on this `verdict.publish` span SHOULD have
advanced after the respawn. If it hasn't — that verdict's attribution
is suspect, AND that worktree's recent green/red transitions may be
wrong-tree.

### 3.4 Trace correlation — full causal chain

In SigNoz trace view, follow the `trace_id` from the offending
`ra.respawn` event forward. The expected chain is:

```
ra.respawn (event)
  → Ctrl::Spawned channel ingestion  (no span — internal channel; visible via cargoless:obs stderr line)
    → overlay.reset (event)          ← MUST EXIST; its absence IS the bug
      → overlay.switch (span)        ← first switch for this cluster post-respawn
        → did_open / did_change / did_close (LSP traffic — not currently spanned)
        → did_save (flycheck trigger)
        → verdict.publish (span)     ← Judgment-B sole-attribution
```

If `overlay.reset` is missing from this chain for the affected
cluster_hash — **THAT is the defect, at the seam between
`Ctrl::Spawned` ingestion and the next `switch_to`.**

---

## 4. Fix

### 4.1 What the fix IS

**A source-level structural change at the seam.** Examples (by
analogy to the #247 fix-shape):

- The new seam (e.g. a "warm-restart" code path) didn't wire through
  to `OverlayMultiplexer::reset()` — add the explicit call at the
  spawn-completion site, mirroring the existing
  `ClusterDriver::reset_after_respawn` pattern.
- A refactor extracted the reset logic into a helper that's not
  called on every spawn-event path — restructure so the reset is
  unconditional at the seam, not behind a code path that can be
  skipped.
- A new integration seam (e.g. a serve-loop alternative path)
  bypasses the `Ctrl::Spawned` handler — restructure so EVERY
  RA-(re)spawn flows through the same single funnel that fires
  `overlay.reset`.

The fix is **always** "make the precondition structurally
unviolatable at the seam", **never** "add a defensive guard inside
the proven core".

### 4.2 What the fix is NOT

- NOT a retry-loop in `OverlayMultiplexer::switch_to` that detects
  stale state and resets itself. The core is proven by precondition;
  weakening it to handle a violated precondition would just MASK
  future regressions, not prevent them.
- NOT a global "reset on every switch" defensive call. Resetting an
  already-fresh multiplexer wastes work and pollutes the AC4 metric
  (you'd see `overlay_reset_total > (ra_restart + initial_spawn)` —
  the SAME divergence, opposite sign, equally invariant-violating).
- NOT a "bump the metric, ignore the underlying defect" patch. The
  divergence is the sentry; silencing it without source-fix
  re-introduces the false-GREEN class.

### 4.3 Source pointers (for the on-call dev-fixer)

- Seam owner: `crates/cargoless/src/servedrv.rs`'s `Ctrl::Spawned`
  ingestion handler. The 929a5d3 line numbers are in the
  servedrv.rs:617 / :360 / :626 region.
- Proven core: `cargoless_core::multiplex::OverlayMultiplexer::reset`
  (DO NOT MODIFY — the proof's preconditions are baked into the
  source structure).
- Coordinating driver:
  `cargoless_core::clusterdrv::ClusterDriver::reset_after_respawn`
  (the #247 structural fix landed here — any new seam should call
  through this method, not duplicate its logic).
- Telemetry sites: the same servedrv.rs sites that emit the
  Wave-1 keystone events (#246 5c) — the fix should emit the metric
  AT the seam, side-by-side with the existing keystone event, NOT at
  a separate site.

---

## 5. Do NOT clear this alert until source fix lands

The alert MUST stay firing until:

1. A source fix lands on `main` at the violating seam.
2. The Wave-2 keystone-invariant test (the regression sentry test)
   passes against the fixed source.
3. A new measured `cargoless_ac4_divergence` value returns to 0 and
   STAYS at 0 for at least one 5min observation window post-fix.

**Clearing the alert via dashboard toggle or by restarting the daemon
WITHOUT fixing the source is a P0-class incident** — it re-arms the
false-GREEN attribution defect class against future operators.

If you absolutely MUST silence the alert for operational reasons
(e.g. a pager storm), file a CRITICAL ticket in Plane with:

- The offending `cluster_hash`
- The offending `respawn_generation` value
- The trace_id of the missing-`overlay.reset` chain
- Why silencing was operationally necessary
- The hard deadline by which source-fix lands

— and re-arm the alert post-fix.

---

## 6. Pre-Wave-2 verification status (the honest framing)

**At authoring time (2026-05-20), Wave-2 metrics are IN FLIGHT but
NOT on `main`.** The metrics this alert depends on
(`cargoless_overlay_reset_total`, `cargoless_ra_restart_total`,
`cargoless_initial_spawn_total`) don't emit yet. This alert is
**dormant** until Wave-2 lands.

In the interim, the AC4 invariant is verified by:

1. **Source-structural fix at #247** — the
   `ClusterDriver::reset_after_respawn` + `Ctrl::Spawned` wire
   guarantee the per-event ordering structurally. On `main`.
2. **Keystone-event presence in Wave-1 traces** — `overlay.reset`
   emits at the seam (servedrv.rs:360 region) per #246/5c; an
   operator inspecting SigNoz trace search can manually verify the
   `ra.respawn → overlay.reset` pairing today.
3. **The Wave-2 keystone-invariant TEST** (in flight on
   `agent/dev-fixer-w2`) — when it lands, it asserts the divergence ≡ 0
   property at unit-test time, before any metric emits.

Once Wave-2 lands, the three-layer defense is complete:
source-structural (#247) + test-structural (Wave-2 keystone-invariant
test) + metric-sentry (this alert).

---

## 7. Operator quick-reference card

```
┌────────────────────────────────────────────────────────────────┐
│ AC4 DIVERGENCE ALERT FIRED                                     │
├────────────────────────────────────────────────────────────────┤
│ 1. DON'T restart the daemon.                                   │
│ 2. DON'T clear the alert.                                      │
│ 3. PAGE dev-fixer (or current code-owner on-call).             │
│                                                                │
│ Diagnose:                                                      │
│  • SigNoz dashboard → AC4 Divergence panel → per-cluster slice │
│  • Identify the offending cluster_hash                         │
│  • Trace search: ra.respawn → overlay.reset chain — find the   │
│    respawn without a following reset before the next           │
│    overlay.switch                                              │
│  • Note the trace_id + respawn_generation for the fix ticket   │
│                                                                │
│ Hand-off to dev-fixer:                                         │
│  • Offending cluster_hash                                      │
│  • Trace_id with the missing overlay.reset                     │
│  • Source pointer: the new code path that bypassed the         │
│    Ctrl::Spawned funnel (likely a recent commit on `main`)     │
│  • Required fix: structural seam change (see §4.1)             │
│  • Required: keystone-invariant test passing post-fix          │
│  • Forbidden: defensive guard inside OverlayMultiplexer core   │
│                                                                │
│ Resolve:                                                       │
│  • Source fix lands on main                                    │
│  • Keystone-invariant test passes                              │
│  • Alert returns to 0 for ≥5min                                │
│  • THEN clear ticket + reset alert annotation                  │
└────────────────────────────────────────────────────────────────┘
```

---

**End of runbook. Authored against `origin/main = 929a5d3`. Update
this doc IF the metric names change in Wave-2 final
(open-question #1 in [D-INC2-OBSERVABILITY.md](../design/D-INC2-OBSERVABILITY.md) §9).**
