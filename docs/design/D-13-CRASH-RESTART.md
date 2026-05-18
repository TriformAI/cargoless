# D-13 — Model R crash + restart handling (Stream D / #13)

**Status:** Design note (study deliverable, GO'd parallel-start alongside #8).
Scope-bounded exactly like #174/#175: this note designs against the **existing
AC#6 supervisor surface** (cited by symbol) and the **frozen #7 cache seam**;
it makes **no #5-contract speculation** — the parts that bind to #5's overlay
producer and to a signal mechanism are named as explicit, deferred seams.

**Owner:** bench-lead (Stream D). **Tracks:** Plane #13 / task #179.

---

## 1. The three legs and their true dependency state

team-lead's brief: "base-RA respawn (extend AC#6 supervisor), per-WT queue
replay, SIGTERM graceful." Studied against
`crates/cargoless-core/src/analyzer.rs`, the legs decompose as:

| Leg | What it needs | Dependency state |
|---|---|---|
| **A. base-RA respawn on crash** | A supervised child that transparently restarts (incl. `kill -9`) with capped backoff + a post-respawn re-init hook | **Already exists** — AC#6 `Supervisor`. #13 = *wiring*, not new mechanism. |
| **B. per-WT queue replay on reconnect** | On respawn, re-apply every *active* worktree's overlay-set so the restart is transparent across all WTs, not just one | **Partial #5 dep.** The replay *contract* (ordering, idempotency, queue) is #5-independent pure-core; the actual overlay re-application binds to #5's LSP-overlay seam. |
| **C. SIGTERM graceful shutdown** | Catch SIGTERM, stop intake, drain in-flight verdicts/diagnostics to `tree.cache`, then terminate the child cleanly | **#5-independent.** Needs a signal mechanism (no signal dep in the crate today — see §4). The drain *state machine* is pure-core + testable now. |

The headline finding: **leg A is essentially free.** AC#6 already delivers the
hard part (transparent respawn). #13's real new engineering is the *Model-R
multiplexing* of that guarantee (leg B) and the *clean-stop* path (leg C).

---

## 2. Leg A — base-RA respawn is AC#6, reused verbatim

The Model R "base-RA" is one repo-scoped rust-analyzer. It is supervised by
the **same** `Supervisor` that v0 already ships:

- `Supervisor::start_with_hook(spawn, on_spawn)`
  (`analyzer.rs:89`) — monitor thread transparently restarts the child on any
  unexpected exit incl. external `kill -9`; capped backoff
  (`MIN_BACKOFF`=50 ms → `MAX_BACKOFF`=2 s, `analyzer.rs:35-36`);
  `restart_count()` observable (`analyzer.rs:144`).
- `on_spawn: OnSpawnFn` (`analyzer.rs:53`) — invoked against every
  (re)spawned child *before it is stored*, **without the state lock held** so
  it may block on the LSP `initialize` handshake. This is the documented seam
  "where the LSP initialize handshake + document re-open will be re-run"
  (`analyzer.rs:30-31, 46-52`).
- `SuspendHandle` (`analyzer.rs:204`) — Tier-4 idle-evict already proves the
  respawn path is correct under deliberate eviction; its **no-wrong-verdict
  contract** (`analyzer.rs:189-202`) is the exact invariant leg B must
  preserve: a respawn (for *any* reason — crash, kill -9, idle-evict) is only
  ever allowed to *delay* a check, never change a verdict, because the
  authoritative green/red is the transient cargo-check / F8-redo tier, not the
  resident RA.

**#13 leg-A deliverable = zero new supervisor code.** The Model R daemon
constructs its base-RA as `Supervisor::start_with_hook(spawn_repo_ra,
model_r_on_spawn)` where `model_r_on_spawn` is leg B's replay callback. The
AC#6 contract is inherited, not re-implemented — the same compose-don't-reinvent
discipline as #7-on-cas and #8-on-#7.

---

## 3. Leg B — per-WT queue replay (the new Model-R piece)

### 3.1 Problem

v0 AC#6's `on_spawn` re-`did_open`s *one* worktree's documents. Model R
multiplexes N active worktrees through one RA (design §6). After a base-RA
respawn the RA's overlay state is empty — **every active worktree's overlay-set
must be re-applied**, or the first post-respawn check for WT-k would silently
run against base-without-WT-k's-edits (a wrong verdict — exactly the failure
class this product exists to prevent).

### 3.2 The #5-independent pure-core contract (buildable now)

What is *not* #5-dependent and can be built + ci-gated now (pending a
follow-on build GO, like #8's was):

- **`ReplayQueue`** — an ordered, idempotent set of "active worktree overlay
  identities to re-apply on next (re)spawn", keyed by `WorktreeId` (the #8
  newtype) → `OverlayHash` (the #7 newtype). Properties, all unit-testable
  against mocks:
  - *Idempotent*: enqueuing the same `(WorktreeId, OverlayHash)` twice is one
    entry (a respawn must not double-apply).
  - *Latest-wins per WT*: if a WT's overlay advances while RA is down,
    only the newest `OverlayHash` is replayed (stale intermediate overlays
    are never re-applied — they were never authoritative).
  - *Deterministic order*: replay order is `WorktreeId`-sorted so a respawn is
    reproducible (the determinism discipline `cargoless-cas::tree` already
    holds for hashing, applied here to replay).
  - *Generation-fenced*: each respawn bumps a generation counter; a replay
    tagged with an older generation is dropped (defends the
    respawn-during-replay race — a second `kill -9` mid-replay must not apply
    a half-set).
- **`RecoverySink` trait** — the #5 seam, mocked in tests, the *only* thing
  assumed of #5: `fn reapply(&mut self, wt: &WorktreeId, overlay: &OverlayHash)
  -> io::Result<()>`. #13 owns the *queue + ordering + generation fence*; #5's
  LSP-overlay multiplexer implements `reapply` later. Zero #5-contract
  speculation — `reapply` is the inherent, inevitable shape.

### 3.3 The bind point (deferred, named — no speculation)

`model_r_on_spawn(child)` = `for (wt, hw) in replay_queue.drain_sorted(gen) {
sink.reapply(wt, hw) }` then the existing LSP `initialize`. The `sink` is #5's
overlay multiplexer. This wiring lands when #5 GOs; the queue + fence + their
tests land now (own-test-identity gated).

---

## 4. Leg C — SIGTERM graceful shutdown

### 4.1 What exists

`Supervisor::shutdown()` / `Drop` (`analyzer.rs:159-182`) already does the
*child* side cleanly: set shutdown flag → join monitor → kill+reap child,
idempotent. The gap is purely the **daemon-level orchestration**: catch
SIGTERM, *stop intake first*, *drain in-flight per-WT verdicts/diagnostics to
`<wt>/tree.cache`* (so a restarted daemon re-attaches to accurate state — the
#7 decoupled-lifecycle cache is the durability substrate), *then*
`Supervisor::shutdown()`.

### 4.2 Pure-core, #5-independent: the drain state machine

`GracefulShutdown` — a small state machine, fully testable now:
`Running → Draining(stop intake; flush pending tree.cache writes; barrier) →
Stopped(Supervisor::shutdown)`. Idempotent (double-SIGTERM = one drain);
bounded (a drain deadline → force-stop, so a wedged flush can't hang
termination — same "never let cleanup hang the process" discipline as the
Tier-4 reap). The flush target is #7's `worktree_tree_cache(wt)` /
`tree_cache_dir()` (frozen seam) — no #5 dependency.

### 4.3 The signal mechanism — explicit deferred decision (D-13-Q1)

The crate has **no signal dependency today** (std has no portable signal API;
`cargoless-core` deps are `notify` + `serde_json` only — see its `Cargo.toml`).
Options, surfaced for the operator/lead (not silently chosen):

| Option | Cost | Note |
|---|---|---|
| (a) self-pipe + minimal `libc::sigaction` | one tiny dep (`libc`) | smallest correct Unix-only path; matches "minimal-dep" CLAUDE.md posture |
| (b) `signal-hook` | one well-known dep | ergonomic, battle-tested; heavier than (a) |
| (c) no handler; rely on `Drop` on SIGTERM-default-terminate | zero dep | **insufficient** — default SIGTERM disposition terminates *before* `Drop` runs ⇒ no drain. Rejected unless intake/flush is made crash-safe by construction (then SIGTERM-graceful degrades to "crash recovery is the graceful path", which is actually a defensible v1 stance given #7's atomic tree.cache + AC#6). |

**Recommendation:** design the drain state machine + tree.cache flush to be
**crash-safe by construction** (atomic temp+rename per #7's discipline) so that
even an *un-drained* SIGTERM/kill is recoverable on restart via leg A + B —
then the explicit signal handler (option a) becomes a latency optimization
(clean stop is faster to recover than crash-replay), not a correctness
requirement. This keeps leg C's *correctness* zero-dep and #5-independent, and
makes the `libc` dep an opt-in polish. Defer the dep decision to lead/operator
as **D-13-Q1**.

---

## 5. Deliverable plan (mirrors #8's pure-core/wire split)

1. **Now (this note):** the study + design + dependency decomposition + the
   D-13-Q1 signal-dep question surfaced. No code, no speculation.
2. **Follow-on build (on GO, like #8 was):** `cargoless-core::recovery` pure-core
   — `ReplayQueue` (idempotent, latest-wins, sorted, generation-fenced) +
   `RecoverySink` trait + `GracefulShutdown` drain state machine, all over the
   frozen #7/#8 seams + mocked #5 sink. own-test-identity gated (count
   `recovery_*` tests, assert zero foreign-stream tests in-block — the interim
   standard until builder-infra's per-ref `CARGO_TARGET_DIR` fix).
3. **Wire (when #5 GOs):** bind `RecoverySink` to #5's overlay multiplexer and
   `model_r_on_spawn` to the live `Supervisor::start_with_hook`. No #13 code
   change — just the seam connection, same as #8→#5.

## 6. Honest caveats

- Leg A claims "free" because AC#6 is proven (the #122 idle-evict + AC#6
  kill-9 integration test exercise the exact respawn path). That is a *reuse*
  claim, not an untested assertion — its correctness rides existing green
  tests, and leg B's generation fence is precisely the new race AC#6 alone
  doesn't cover (concurrent respawn during multi-WT replay).
- Leg B's pure-core is genuinely #5-independent, but it delivers *no
  end-to-end value* until #5's `reapply` exists — like #8, it is a
  scope-bounded parallel-start, not a shippable slice on its own. Stated
  plainly so it isn't mistaken for a complete #13.
- D-13-Q1 (signal dep) is a real open decision, not hand-waved: leg C's
  *correctness* is designed to not depend on its resolution; only the
  clean-stop *latency* does.
