# D-MERGE-QUEUE — hot-trunk landing: the witness cancel-collision and the missing serializer

**Status:** ANALYSIS + PROPOSAL. The cargoless-side fixes (Parts 1–3 below)
are landed on the `agent/hot-trunk-no-cancel` branch. Part 4 (the
tf-multiverse merge queue) is a **design spec only** — it lives in a
different repo (`tf-multiverse`) and is implemented there, not here.

**Why this exists.** Operators and agents report that during a crowded
("hot") trunk it is hard to land anything: the required CI "witness" check
(`incremental compile check / incremental witness (pull_request)`, backed by
the cargoless daemon) keeps showing `Has been cancelled` and never concludes,
and sometimes computes a GREEN verdict that "can't bind to the SHA"
(`attributed to <absent>, want <sha>`) and exits 1. Agents then burn long
stretches deciding between waiting and an operator force-merge.

The whole class reduces to one governing principle, stated by the operator:

> **Never cancel in-progress work. Only collapse the queue.** A new push
> supersedes *redundant queued* work for the same logical unit; it must never
> kill a run, witness, or build that is already executing.

A cancelled Forgejo run is recorded as **permanently not-green**, so any
"cancel on newer push" turns a re-push or a hot-trunk push storm into an
**unsatisfiable required gate**. The anti-pattern appears at three layers; the
first three are fixed in cargoless, the fourth is the tf-multiverse spec.

---

## 0. Root cause — three worktree-keyed collisions in the daemon

The cargoless daemon serves as the witness: a tf-multiverse workflow POSTs the
PR's overlay (diff over `main`) with `gate=true` + `base_sha=$COMMIT`, then
polls `GET /status?worktree=W` for ≤1900s until a verdict whose `base_sha`
echoes its `$COMMIT`. Green→merge, red→fail, no-attributed-verdict-in-budget→
error+exit 1.

On a hot trunk **every concurrent PR for one repo maps to the same analysis
worktree key** on the daemon, and three pieces of state were keyed by worktree
path alone, so independent PRs collided destructively:

1. **Single verdict slot** — `statuses: BTreeMap<wt, WorktreeStatus>`. PR B's
   verdict overwrote PR A's; a slow A-poller then read B's verdict.
2. **Worktree-keyed publish guard** — `hard_witness_generation: BTreeMap<wt,
   u64>`. Any newer push bumped the generation, so PR A's *finished* witness
   was dropped (`stale-witness-dropped`) — even though A compiled cleanly in
   its own isolated overlay. A never got a verdict → poll timed out → exit 1.
3. **Worktree-keyed attribution** — a newer push could evict A's `base_sha`,
   publishing `base_sha=""` → the poller's `<absent>` log.

The generation guard's "last-writer-wins" is correct when "newer push" means
*you* superseding *your own* overlay — but wrong when it means a *different
PR*. They shared one key, so independent witnesses cancelled each other. This
is the same "cancel in-progress" anti-pattern, inside the daemon.

---

## 1. cargoless CI — `cancel-in-progress: false` (LANDED)

`.forgejo/workflows/ci.yml` had `cancel-in-progress: true`: every re-push to a
branch cancelled its own running 5-job matrix, and the cancelled run is
permanently not-green. Flipped to `false`; the `concurrency.group` is kept so
*queued* duplicates still collapse. (The old comment claimed the group
de-duped the push-vs-pull_request double matrix; it never could — `push` →
`refs/heads/…` and `pull_request` → `refs/pull/N/merge` are different groups —
so cancellation only ever killed the branch's own re-pushes.)

## 2. cargoless daemon — SHA-addressable witnesses (LANDED)

Key the three structures by `(worktree, base_sha)`:
- `statuses: BTreeMap<wt, RecentVerdicts>` — `latest` keeps the historical
  single-slot read byte-for-byte; `by_sha` (bounded FIFO,
  `CARGOLESS_VERDICT_HISTORY_MAX=16`) lets a SHA-pinned poller retrieve ITS
  verdict after newer pushes published.
- `hard_witness_generation: BTreeMap<WitnessKey{wt,base_sha}, u64>` — a newer
  *different*-base push no longer drops an older witness; a *same*-base re-push
  still supersedes.
- `GET /status` gains an optional `base_sha` selector (absent ⇒ latest; fully
  backward-compatible) via a `VerdictService::get_status_for` trait default.
- Hard-mode witnesses gain a **blocking** concurrency permit
  (`CARGOLESS_PROJECT_CHECKS_HARD_MAX_PARALLEL=4`): once different-base
  witnesses coexist they can compile in parallel, so the fan-out is bounded —
  a gate push waits its turn, never silently skips.

The same-base `BatchCoalescer` (its key already includes `base_ref`) is
untouched; it remains the complement — same-base concurrent pushes coalesce,
different-base pushes coexist. Per-`(wt,base_sha)` *attribution* was evaluated
and deliberately **not** changed: the witness captures its attribution by value
at EmitVerdict before the worker runs (the `ClusterDriver` serializes
SwitchOverlay), so it cannot be cross-stamped under the current serialization.

## 3. cargoless landing — `scripts/land-ff` retry wrapper (LANDED)

`scripts/triple-guard-ff` is correct but one-shot: it `die`s if `origin/main`
moved off the recorded base (g2a), forcing a full manual re-gate. On a hot
trunk you can lose that race repeatedly. `scripts/land-ff` wraps `ci-gate` +
`triple-guard-ff` **without weakening any guard**: on a g2a base-moved loss it
re-fetches, replays the commits onto the new tip (pure linear rebase, never a
merge, no `--force`), re-gates, and retries with jittered backoff and a bounded
attempt count. The guard stays the sole push authority; a real rebase conflict
aborts and is surfaced (genuine coordination, not a retry); a gate RED stops
immediately (code fault, not a race).

---

## 4. tf-multiverse merge queue — DESIGN SPEC (implemented in tf-multiverse)

Parts 1–3 make each witness *able to conclude* and let a single agent retry a
lost land. The remaining systemic gap is structural: **tf-multiverse has no
merge queue.** Every agent runs `scripts/dev-merge` against the live tip
independently; the readiness gate (`scripts/fj` `pr_status`, consumed by
`scripts/dev-merge`) maps `cancelled`/`error`/`failure` all to `BLOCKED` with
no auto-retrigger. Agents race; losers wait or force-merge. A merge queue
removes the race **by construction**.

### 4.1 Verified current behavior (tf-multiverse, file:line)

- **No serialization.** No `merge_queue`/`merge_group` config; the only
  "tip-lock" artifact (`.triform/guides/synchronous-tip-lock.md`) is an
  unadopted human SendMessage convention, not wired into `dev-merge`/`fj`.
  Contention is dampened only by retry jitter
  (`dev_merge_jittered_sleep`, `scripts/dev-merge`).
- **Cancelled treated as failed.** `scripts/dev-merge` greps `fj pr status`
  stdout for `^BLOCKED:`; the computation in `scripts/fj` `pr_status()` folds
  any non-`success` required-context state into `missing` → `BLOCKED`. It
  refuses to *reconcile* a `cancelled`/`error`/`failure` run (`run_concl !=
  "cancelled"` guard) but has **no branch that classifies cancelled/error as
  retryable** — to the gate they are indistinguishable from a real red.
- **No auto-retrigger.** `dev-merge` only polls; on a self-advanced head it
  re-runs the whole gate (`dev_merge_rerun_after_trunk_refresh`) relying on
  Forgejo to auto-fire CI — it never re-dispatches the required witness.
- **Witness is required** (`forgejo-branch-map.yaml` `required_ci_checks_
  override`), and its workflow is already `cancel-in-progress: false` (good) —
  so the cancellation the operator sees is Forgejo cancelling a *superseded
  `pull_request` merge-ref run* when the base advances, plus the daemon's
  BatchCoalescer ancestry-collapsing the superseded SHA so the poll never gets
  an attributed verdict. The cargoless Part 2 fix removes the daemon half; the
  queue removes the base-advance-recompute half.

### 4.2 Proposed design

**A. Serialize landings (FIFO admission to a single "landing" slot).**
- One PR at a time is admitted to *landing*. The queue tests it against the
  **post-merge tip** — recompute `base ⊕ head` **once, at the front of the
  queue**, not re-raced on every unrelated base advance.
- On green → fast-forward land, advance the queue. On red → eject that PR
  (with its real reason), admit the next. FIFO ordering gives fairness; the
  race is gone because only the front PR is ever tested against the tip.
- Minimal viable form: a lease/lock (a Forgejo issue label, a row in a small
  state store, or a `Lease`-style object) that `dev-merge` must acquire before
  entering its gate→land critical section; acquisition is FIFO by enqueue
  time. This is the smaller-blast-radius option vs adopting a full
  bors/merge-queue product, and reuses the existing `dev-merge` land path.

**B. Classify transient vs real (the cancelled≠failed fix).**
- In `scripts/fj` `pr_status()`: introduce a third state besides
  green/blocked — `RETRYABLE` — for required contexts whose latest run
  concluded `cancelled` or `error` (as opposed to `failure`). Emit it as a
  distinct stdout marker (e.g. `RETRYABLE:`), not `BLOCKED:`.
- In `scripts/dev-merge`: on `RETRYABLE`, re-enqueue / re-trigger rather than
  treating it as a hard block. A genuine `failure` (real red) still blocks.
- This directly removes the "cancel-loop reads as permanent red" trap and is
  independently valuable even before the full queue lands.

**C. Auto-retrigger the required witness on a fresh head.**
- When the queue admits a PR and the witness has no terminal *attributed*
  verdict for the current head, re-dispatch the witness workflow for that head
  (Forgejo `workflow_dispatch` or an empty re-trigger commit) instead of
  forcing the operator to choose wait-vs-force. Bounded retries; visible
  (logged), never silent.

**D. Keep the axiom end-to-end.** The witness workflow stays
`cancel-in-progress: false`; the daemon (Part 2) no longer drops different-base
witnesses; the queue ensures the front PR gets an uncontested tip long enough
to conclude. No layer cancels in-progress work; every layer only collapses
redundant queued work.

### 4.3 Migration path
1. Land **B** first (cancelled/error → `RETRYABLE`) — smallest change, removes
   the most acute symptom (cancel-loop reads as red), no new infra.
2. Land **C** (auto-retrigger) — removes the manual wait-vs-force decision.
3. Land **A** (FIFO queue) — removes the race structurally; can start as a
   lease around `dev-merge`'s critical section and grow into a full queue if
   warranted.
4. Once A is in, **C** narrows to "retrigger only the front PR" and the fleet's
   retry-jitter can be relaxed (the queue, not jitter, now orders landings).

### 4.4 Non-goals
- No change to the witness's *correctness* posture (strict attribution,
  fail-closed on unattributed green) — those are right; the queue is about
  *scheduling*, not verdict semantics.
- Not a bors adoption mandate: the lease-around-dev-merge form is the
  recommended first step; a packaged merge-queue is an option, not a
  requirement.

---

## 5. Cross-cutting principle

Every fix here is the same discipline: **never cancel in-progress work; collapse
the queue.** CI stops cancelling re-pushes (Part 1); the daemon stops letting a
different PR cancel your witness (Part 2); landing retries instead of dying once
(Part 3); and the merge queue gives the front PR an uncontested tip so the
witness can actually conclude (Part 4). A cancelled run is permanently
not-green — so on a hot trunk, *cancelling is the bug*.
