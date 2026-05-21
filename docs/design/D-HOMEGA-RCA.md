# D-HΩ-RCA — push-mode "success-ack-with-no-underlying-effect"

**ADR / Architecture Decision Record.** Author: `architect` (paired with
`rca`). Status: **RCA CONFIRMED — fix design ratified**. Scope: HΩ, the
field finding from `agent/bench-lead-m3` (M3-REPORT §6). Code traced
against `origin/main @ bddd1c4`.

---

## Decision

Root-cause of HΩ is **NOT a worktree-discovery / registration gap**
(the M3-REPORT §6.3 field hypothesis). It is a **spawn-readiness race
with no retry on the one-shot push event path**:

> A `POST /overlay` is a single discrete event. The serve loop drains it,
> registers the worktree, spawns the cluster's rust-analyzer, and feeds
> the worktree's one-and-only `DriverEvent::RoutedBatch` — **all on the
> same loop iteration**. At that instant `ClusterState.lsp` is `None`
> (the RA's LSP handshake completes strictly later, asynchronously, via
> `Ctrl::Spawned`). The `SwitchOverlay` action therefore hits the
> `let Some(lsp) = cs.lsp.clone() else { return; }` early-return in
> `exec()` and does **nothing** — no `did_save`, so no flycheck is ever
> triggered. When `Ctrl::Spawned` later lands, its handler calls
> `driver.reset_after_respawn()`, which **drops the in-flight
> transaction**. Nothing ever re-feeds a `RoutedBatch` for that
> worktree. The flycheck barrier therefore never settles ⇒
> `ClusterAction::EmitVerdict` is never produced ⇒ `publish_verdict()`
> is never called ⇒ `api.publish()` is never called ⇒
> `ServeVerdictState.statuses` stays empty ⇒ `list_worktrees()` returns
> `[]` and no verdict ever reaches any read-plane channel.

The fix restores one invariant at the wire seam: **every registered
worktree with a pending check must have its `RoutedBatch` delivered to a
`SwitchOverlay` that executes against a *ready* RA.** The proven cores
(`clusterdrv`, `multiplex`, `barrier`, `activitymgr`) are byte-untouched
— the defect is purely in `crates/cargoless/src/servedrv.rs`.

---

## Context — what required the trace

`agent/bench-lead-m3` M3-REPORT §6.2 reproduced: `POST /overlay` returns
`200 {accepted:true, applied_files:1}`, but `GET /worktrees` → `[]` at
t+10s and t+40s, `/status` and `/verdict` stay `null`, the `cli-status`
file is never created — while the rust-analyzer process is demonstrably
alive and burning CPU (12.4 % → 3.0 %). The write-plane works; the
verdict read-plane is silent for push-sourced activity. The file-watcher
path does flow to a verdict; the push path does not.

The M3-REPORT offered a field hypothesis (§6.3): *"the daemon discovers
ZERO worktrees … no cluster-state exists to drive `EmitVerdict`."* That
hypothesis is **falsified by the code** and by the report's own §6.6
evidence — see "Corrections to the field hypothesis" below.

---

## Trace — the verified causal chain

### Files

- `crates/cargoless/src/serveapi.rs` — `ServeVerdictState`, `push_overlay`
  (:252), `list_worktrees` (:219), `publish` (:128).
- `crates/cargoless/src/servedrv.rs` — the serve loop `run()`, the push
  drain (:394–432), the `Ctrl::Spawned` handler (:355–378), `exec()`'s
  `SwitchOverlay` arm (:728–820), `publish_verdict()` (:842).
- `crates/cargoless-core/src/clusterdrv.rs` — `ClusterDriver`,
  `on_routed_batch`, `on_lsp` (the sole `EmitVerdict` site),
  `reset_after_respawn`.

### Step-by-step (push for a never-before-seen worktree)

1. **Client** `POST /overlay` → `ServeVerdictState::push_overlay`
   (`serveapi.rs:252`). Stores the `(base_ref, files)` pair into
   `self.pushed`, sends the worktree key on the `push_signal` channel,
   returns `accepted=true`. **Write-plane: correct.**

2. **Serve loop, push drain** (`servedrv.rs:394`). `push_rx.try_recv()`
   yields the worktree key. The drain:
   - registers the worktree: `wt_hash.insert(wt, h)` where
     `h = cluster_hash_from_pushed(...)` — **registration happens here,
     it is not skipped**;
   - `activity.touch(wt, now)`;
   - `lifecycle.activate(...)` returns `SpawnRa` (0→1 edge) ⇒
     `spawn_cluster(...)` — inserts a `ClusterState { lsp: None, .. }`
     into `clusters` **synchronously** and starts the `Supervisor`;
   - calls `step(clusters, &h, DriverEvent::RoutedBatch { wt }, ..)`.

3. **`step` → `ClusterDriver::on_event(RoutedBatch)`**
   (`clusterdrv.rs:169`). `current.is_none()` ⇒ opens an `ActiveTxn`
   with `FlycheckBarrier::arm(false)`, returns
   `ClusterAction::SwitchOverlay { wt }`.

4. **`step` → `exec(SwitchOverlay)`** (`servedrv.rs:728`).
   - `api.take_overlay_for(wt)` **consumes** the pushed overlay
     (pop-on-consume) — *before* the readiness guard;
   - then `let Some(lsp) = cs.lsp.clone() else { return; }` — `cs.lsp`
     is **`None`** (the RA handshake has not completed; we are in the
     same iteration that spawned it). **Early-return.** No
     `mux.switch_to`, no LSP verbs, **no `did_save` ⇒ no flycheck
     triggered**.

5. **Next loop iteration — `Ctrl::Spawned` drain** (`servedrv.rs:355`).
   The RA finished its handshake; `on_spawn` sent `Ctrl::Spawned`. The
   handler runs `cs.driver.reset_after_respawn()` —
   **`current = None`** (`clusterdrv.rs:335`) — then `cs.mux.reset()`,
   `cs.lsp = Some(client)`. The in-flight transaction from step 3 is
   now **dropped**.

6. **No re-feed.** Nothing in the loop ever feeds another `RoutedBatch`
   for this worktree:
   - the FS watcher cannot fire — a pushed overlay is **in-memory
     only**, there is no on-disk change;
   - `activity.tick` only emits `Deactivated`, never a re-check;
   - the `Ctrl::Spawned` handler does **not** replay routed batches;
   - `push_rx` is empty (the push was a single event).

7. **Terminal state.** No flycheck ⇒ no `LspEvent::FlycheckEnded` ⇒
   `FlycheckBarrier` never reaches `Settled` ⇒ `on_lsp`'s `Settled` arm
   (the **sole** `EmitVerdict` site, Judgment B) never runs ⇒
   `publish_verdict()` never runs ⇒ `api.publish()` never runs ⇒
   `statuses` stays empty.

8. **Symptom.** `list_worktrees()` (`serveapi.rs:219`) maps over
   `statuses.values()` — empty ⇒ `GET /worktrees` → `[]`. `get_status`
   / `get_verdict` resolve `None` ⇒ `/status` / `/verdict` → `null`.
   `statusfile::write` never runs ⇒ no `cli-status` file. **Exactly the
   M3-REPORT §6.2 observations.**

### The asymmetry vs the file-watcher path

The FS path (`servedrv.rs:463–486`) is structurally the *same* —
`pending_batch.insert` + `lifecycle.activate`/`spawn_cluster` +
`step(RoutedBatch)` — and has the **same** latent spawn-window race.
It does not manifest because the FS watcher produces a **continuous
stream** of `RoutedBatch` events: a batch lost in the RA-spawn window is
naturally followed by later batches that land after `cs.lsp` is `Some`,
so the FS path **self-heals by retry-through-continued-activity**. The
push path is a **discrete one-shot** with zero follow-up — the single
lost batch is permanent. *The asymmetry is the continuity of the event
source, not the registration mechanics.*

This is why M1 (#196/#252/#259 fleet-RAM) worked on the same fleet
substrate while M3 did not: M1's `touch_wts()` drives on-disk file
changes (continuous FS activity); M3 exercises the push path, which M1
never did.

---

## Corrections to the M3-REPORT field hypothesis

The M3-REPORT is a valuable field finding; two of its §6.3 conclusions
are corrected by the code trace:

1. **"The daemon discovers ZERO worktrees … no cluster-state exists."**
   False. The push drain self-registers the worktree
   (`wt_hash.insert`, `servedrv.rs:399–403`) — it does **not** depend on
   startup `RepoScope::discover`. And `spawn_cluster` *did* run: the
   report's own §6.6 confirms "RA spawn on real-content overlay — RA
   process alive", which is only reachable *after* registration +
   `lifecycle.activate == SpawnRa`. Cluster-state **does** exist; the RA
   **is** running. The defect is downstream of registration.

2. **`/worktrees: []` interpreted as "zero worktrees discovered."**
   `list_worktrees()` is **verdict-derived, not discovery-derived** — it
   maps over `statuses` (the published-verdict map). A worktree appears
   in `/worktrees` only *after* it has received a verdict. `[]`
   therefore means "no worktree has a verdict yet" — a *symptom* of HΩ,
   never independent evidence of a discovery failure.

**Secondary architectural finding (note, not in HΩ fix scope):** there
is no read-plane surface exposing the daemon's *registration / cluster*
state (`wt_hash`, `clusters`) independent of verdicts. That absence is
what made HΩ hard to diagnose from the outside. A `/worktrees` that also
listed registered-but-unverdicted worktrees — or a separate
`/registered` surface — would have localised this in one probe. Routed
to the `comms` lane as an observability follow-up ticket.

---

## The two coupled defects

- **D1 (primary) — one-shot push has no retry/replay after RA-ready.**
  The push path feeds its single `RoutedBatch` while `cs.lsp` may be
  `None`, and never re-feeds it once the RA becomes ready.
  `servedrv.rs` push drain (:394–432) + `Ctrl::Spawned` handler
  (:355–378).

- **D2 (compounding) — overlay consumed before the readiness guard.**
  In `exec()`'s `SwitchOverlay` arm (`servedrv.rs:772`),
  `api.take_overlay_for(wt)` (pop-on-consume) is called **before** the
  `let Some(lsp) = cs.lsp.clone() else { return; }` guard. A not-ready
  early-return therefore **permanently consumes and discards** the
  pushed overlay. Even a hypothetical replay would find the store
  empty. D2 must be fixed for any D1 fix to be sound.

---

## Options considered

### Option A — defer the push's `RoutedBatch`; replay at the RA-ready seam *(CHOSEN)*

In the push drain, gate the `RoutedBatch` feed on RA-readiness:
- if `clusters.get(&h)` has `lsp.is_some()` → `step(RoutedBatch)` now
  (the cluster was already up from prior push/FS activity);
- else record the worktree in a new
  `deferred_push: BTreeMap<WorkspaceConfigHash, BTreeSet<WtId>>`.

In the `Ctrl::Spawned` handler, *after* `reset_after_respawn` +
`mux.reset` + `lsp = Some(client)`, drain `deferred_push.remove(&h)` and
feed `step(RoutedBatch)` for each deferred worktree (RA is now ready ⇒
`SwitchOverlay` executes fully ⇒ `did_save` ⇒ flycheck ⇒ settle ⇒
verdict).

- **Pros:** minimal; changes only *when* the wire feeds `RoutedBatch`,
  never the core logic; the proven cores are byte-untouched; the
  overlay store naturally holds the content until a ready consume, so
  D2's blast radius is bounded; mirrors the existing wire-seam
  precondition-restore discipline (#190 `mux.reset`, #198 RA-reap, #247
  `reset_after_respawn`).
- **Cons:** a new piece of loop state (`deferred_push`); needs a small
  borrow-checker restructure in the `Ctrl::Spawned` handler (collect
  the deferred set, end the `clusters.get_mut` borrow, then `step`).

### Option B — `Ctrl::Spawned` replays a `RoutedBatch` for all active WTs of the cluster

The handler already knows the cluster hash `h`; it could re-feed
`RoutedBatch` for every `wt_hash` entry mapped to `h`.

- **Pros:** no new state.
- **Cons:** broader — also re-checks FS worktrees that did not need it;
  conflates "this WT had a pending push" with "this WT exists"; harder
  to reason about and to test. Rejected in favour of A's tighter scope.

### Option C — block the push ack until the RA is ready

Make `push_overlay` synchronously wait for RA-ready before returning.

- **Rejected.** Violates the D-PUSHOVERLAY §2.3 contract ("the ack does
  NOT block on the verdict"); a cold RA spawn is seconds; couples the
  HTTP request lifetime to RA process startup. Non-starter.

---

## Decision — the fix

**Adopt Option A + the D2 reorder.** Concretely, in
`crates/cargoless/src/servedrv.rs` only:

1. **D2 fix — reorder `exec()`'s `SwitchOverlay` arm** so the
   `cs.lsp` readiness check happens *before* `api.take_overlay_for(wt)`.
   A not-ready early-return must **not** consume the pushed overlay. The
   span field-recording (`file_count`, `overlay_size_bytes`) is
   reconciled by recording `0`/`0` on the not-ready early-return path
   (it is honest: nothing was switched).

2. **D1 fix — add `deferred_push` loop state + readiness-gated feed.**
   - new local `deferred_push: BTreeMap<WorkspaceConfigHash,
     BTreeSet<WtId>>`;
   - in the push drain: after the cluster is ensured, feed
     `step(RoutedBatch)` immediately iff
     `clusters.get(&h).is_some_and(|cs| cs.lsp.is_some())`, else insert
     into `deferred_push[h]`;
   - in the `Ctrl::Spawned` handler: after `cs.lsp = Some(client)`,
     drain `deferred_push.remove(&h)` and `step(RoutedBatch)` each.

3. **Keystone test (contract, not impl).** Assert: *given a push for a
   worktree whose cluster RA becomes ready strictly **after** the push
   was received, the worktree still reaches a published verdict.* This
   is the contract — "a pushed worktree reaches a published verdict
   regardless of RA-spawn timing." It must be **falsifiable**: it fails
   on `bddd1c4` (RED = HΩ confirmed) and passes after the fix. A future
   refactor that preserves the contract still passes; one that
   re-introduces the spawn-window drop fails exactly here.
   ([[keystone-test-assert-the-contract-not-the-code]])

### Consequences

- The push path becomes verdict-symmetric with the FS path: a pushed
  worktree appears in `/worktrees` and publishes a verdict on all
  read-plane channels — the central-daemon's push→verdict round-trip
  works for the first time.
- The proven cores keep their structural proofs intact — `clusterdrv`
  Judgments A/B, `multiplex` spatial isolation, `barrier` temporal
  ordering, #247's no-false-GREEN reset are all untouched. The fix is a
  pure wire-seam precondition-restore.
- The FS path's identical latent spawn-window race is **not** fixed by
  this change (it self-heals in practice). It should be noted in the
  ticket as a known-benign latent; a follow-up may apply the same
  deferral to the FS path for completeness. **Not** HΩ-blocking.
- D-PUSHOVERLAY (the parked Increment-2 design-ahead spec) should gain
  an addendum: the push event is one-shot and **must** be deferral-safe
  against RA-spawn latency. Routed to docs as a follow-up.
- The "no registration-state read surface" secondary finding → a
  `comms`-lane observability ticket.

---

## Candidate fix locations (for the `rca` lane)

| # | File | Site | Change |
|---|------|------|--------|
| D2 | `crates/cargoless/src/servedrv.rs` | `exec()` `SwitchOverlay` arm (~:772) | Move the `cs.lsp` readiness guard *before* `api.take_overlay_for`; record `0` span fields on the not-ready early-return. |
| D1a | `crates/cargoless/src/servedrv.rs` | loop-local state (~:235) | Add `deferred_push: BTreeMap<WorkspaceConfigHash, BTreeSet<WtId>>`. |
| D1b | `crates/cargoless/src/servedrv.rs` | push drain (~:425–431) | Feed `step(RoutedBatch)` only if `cs.lsp.is_some()`, else `deferred_push[h].insert(wt)`. |
| D1c | `crates/cargoless/src/servedrv.rs` | `Ctrl::Spawned` handler (~:355–378) | After `cs.lsp = Some(client)`, drain `deferred_push.remove(&h)` → `step(RoutedBatch)` each. |
| KS | `crates/cargoless/src/servedrv.rs` or an integration test | new | The HΩ keystone test — push-before-RA-ready still reaches a verdict. |

`rca` owns the implementation + the keystone (tasks #1 fix surface +
#2 keystone). `architect` reviews as the non-author Layer-3 backstop.

---

## Honest boundary

The keystone test must exercise the **timing** — a push received while
`cs.lsp == None`. The serve loop's `run()` is hard to unit-test (real
RA, real `notify`). Recommended: extract the deferral decision into a
small pure helper (testable: "push + lsp-not-ready ⇒ deferred; spawned ⇒
replayed") *or* write an integration test under the `tf-cli`
`integration` feature with a real RA where the assertion is
`/worktrees` non-empty + `/status?wt=W` resolves within a generous
bound. The pure-helper route is preferred (deterministic, no RA-timing
flake); the integration test is the end-to-end confirmation. `rca` +
`test-replace` pick the exact surface.
