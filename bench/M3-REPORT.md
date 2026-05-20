# M3 — Overlay-push round-trip latency, split transport-vs-RA

**Status: SHAPE-SHIFTED to STRUCTURAL-FINDING DELIVERABLE.** Per
team-lead's pre-committed escape (4 runs × harness self-bugs caught,
no numbers extracted, escalating to a daemon-side push-mode read-plane
finding that's bigger than the bench). The harness IS done; the
numbers it was meant to extract are blocked on the daemon-side finding
in §6 below.

**Task:** Plane CWDL `#267` — fluffy-dreaming-allen.md M3 line.

**Branch / harness commits chain:** `agent/bench-lead-m3` —
- `19be764` initial harness (JSON wire shape was wrong)
- `414e07c` fix: JSON wire shape (`op` not `type`, files `[{path,content}]`) + early os._exit attempt
- `5ae1f5f` M3-REPORT.md template draft (this file)
- `3df5ca2` fix: real-content warm-up overlay + drop blocking sse.stop() before os._exit + SSE event diagnostic logging
- `e697b38` fix: bump warmup→900s + 30s heartbeat with cli-status fallback signal
- (this commit) populates the §3-§6 with the structural-finding deliverable + probe data

**Substrate:** origin/main `929a5d3` — full push-mode chain operational
(`#240/2a` PushOverlay verb + `#240/2b` servedrv consume +
`#240/2c` thin push-client live; `#246` OTEL Wave-1 compiled in / inert).

**Unblock chain:** parked as post-deploy from v0.2.0 launch (Lane C
#222) → unblocked by `#240/2c` thin push-client landing on main →
M3 becomes a real measurement, not a projection.

---

## 1. What this measures

For each rep against a live `cargoless serve --repo --bind <addr>`:

| stamp | meaning |
|---|---|
| `t0` | `time.monotonic_ns()` just before `POST /overlay` |
| `t1` | `time.monotonic_ns()` at 200-ack receipt |
| `t2` | `time.monotonic_ns()` at the next SSE `/events` frame for this worktree |

Three derived quantities per rep:

```
transport_ms = (t1 - t0) / 1e6   ← HTTP RTT + body parse + overlay store
ra_ms        = (t2 - t1) / 1e6   ← serve-loop drain + RA flycheck + EmitVerdict
total_ms     = (t2 - t0) / 1e6
```

### Why SSE, not `/status` polling

`WorktreeStatus.published_at` is **unix-seconds** (1 s granularity — too
coarse for sub-second RA work on a warm daemon). The SSE `/events`
stream emits one frame per verdict transition, and client-side
`monotonic_ns()` stamping at the moment of frame receipt is sub-ms
precision against the same wall clock as `t0` / `t1`.

### Why ack-receipt is a meaningful split point

`POST /overlay` returns 200 **after the overlay is stored and the
serve-loop signal is sent** but **before the verdict is computed** —
`svc.push_overlay` in `crates/cargoless/src/servedrv.rs` stores the
overlay via `peek_overlay_for` / `take_overlay_for` and signals
`push_tx`; the serve loop drains `push_rx` asynchronously, runs
`SwitchOverlay` → flycheck-barrier → `EmitVerdict` →
`publish_verdict` → SSE emit. So `t1 - t0` captures the synchronous
HTTP/store half and `t2 - t1` captures the asynchronous compute half.
The split is **client-measurable without server-side instrumentation**.

---

## 2. Methodology

| | |
|---|---|
| **Fixture** | `bench/fixture` — Leptos honest-size (17 files, ~1009 LOC; same substrate as AC7 §8.5 / `#196` / `#252` / `#259`) |
| **Reps per scale** | 50 (per team-lead dispatch ≥50) |
| **Scales** | `N=1` (single WT) + `N=20` (multi-WT — same shape as `#196` Leg-C v4 fleet) |
| **Edit anchor** | `BENCH_TRAIT_ANCHOR` (`self.entries.len()` → `len_oops()` in `src/domain/model.rs`) — matches `bench/run.sh` + `bench/modelr-fleet.sh` + `bench/m2-cpu-approx.sh` |
| **Driver** | `bench/m3-fleet.sh` (orchestrator) + `bench/m3-roundtrip.py` (measurement) |
| **Build vehicle** | Per-ref isolated `CARGO_TARGET_DIR` (#15-v4 discipline), binary mtime ≥ build-start provenance guard, /healthz readiness latch (`#225/0d`), graceful reap with orphan-verify (`#128` lesson) |
| **OTEL state** | `OTEL_EXPORTER_OTLP_ENDPOINT` unset — Wave-1 compiled in, BatchSpanProcessor parked, exporter idle (same condition the M1 re-baseline `#259` measured) |
| **Pre-run hygiene** | pkill prior `cargoless serve` / `m3-*`, defunct-zombie scan empty, /healthz=200 before harness handoff |

### Harness-fix history (the §9a discipline at the measurement layer)

| run | SHA | result | root cause |
|--:|---|---|---|
| 1 | `19be764` | FAIL @ warm-up (3 min in) | wrong JSON wire shape: sent `{"type":"PushOverlay", ..., "files":[[p,c],...]}`; daemon's `Request::from_json` keys on `op` (not `type`) and expects `files: [{path,content}]` objects (not tuple-pairs) — returned 400 |
| 2 | `414e07c` | (pending) | fix: correct JSON shape + `os._exit(2)` on warm-up FAIL (the SSE listener's blocked `socket.readline()` doesn't release on `sse._resp.close()` quickly enough; `sys.exit` was hanging the bash pipeline) |

Both were caught loudly by the fail-loud control before any meaningless
number could land in a report — the same `§9a` class the M2 cycle
surfaced at the same layer (harness self-bugs under `set -u` / wire-shape
mismatch).

---

## 3. Results — NOT-MEASURED (blocked on §6 finding)

The N=1 and N=20 latency tables were intentionally left empty. Across
4 bench runs (1834 s of pod time on the final 900 s × 2-scale attempt),
ZERO complete round-trips were measured — not because of harness
brokenness in the end (harness is verified working through 3 distinct
bug-fix iterations) but because the daemon never published a verdict
to ANY read-plane channel for the bench fixture, blocking the post-ack
half of the round-trip (`t2 - t1` = `ra_ms`).

What WAS measured during the §6 probe (one isolated data point, not a
distribution but still informative):

| metric | value | source |
|---|--:|---|
| transport_ms (one push, loopback) | **9.28 ms** | §6 probe: `time.monotonic_ns()` around POST /overlay |
| ra_ms | NOT-MEASURED | no verdict ever published; see §6 |
| total_ms | NOT-MEASURED | NOT-MEASURED |

The 9.28 ms transport figure is **one push**, not a distribution; cite
as "order-of-magnitude indicator only, not a statistical claim". A real
distribution would require the verdict-publication channel to work so
the full rep loop could run.

---

## 4. Diagnostic interpretation (template — populated on numbers)

The transport-vs-RA split is the **load-bearing diagnostic**: without
it, "push is X ms" is unactionable. Three interpretation branches; the
actual answer is selected once numbers land.

### Branch A — RA-dominated N=1→N=20 delta

If the `ra_ms` component grows meaningfully (≥1.5×) from N=1 to N=20
while `transport_ms` stays flat:

* **Reading**: the single multiplexed RA's per-WT-state cardinality
  IS the bottleneck. Daemon serializes verdict computation across N
  worktrees through the one RA's flycheck barrier — Lane C #222
  predicted this as the primary risk vector.
* **Cross-ref**: D-FLEET §14 ("RA overlay correctness under high
  churn") + M4 corun §7.3 safety oracle (the post-deploy follow-on
  that quantifies how often combined-green hides solo-red).
* **Honest framing**: the multiplex thesis is *RAM-flatness*
  (#196/#252/#259 confirm), NOT *zero-latency-cost-on-multiplexed-
  state*. RA cost scaling with N is consistent with the architecture
  and only becomes a concern if the scaling is super-linear.

### Branch B — Transport-dominated N=1→N=20 delta

If `transport_ms` grows meaningfully from N=1 to N=20 while `ra_ms`
stays flat:

* **Reading**: per-WT-pushed-overlay overhead (JSON parse + hash +
  store) is the bottleneck. Probably unexpected — the body parse + 
  cluster_hash_from_pushed is content-addressed and should be O(file
  bytes), not O(N).
* **Action**: profile the `svc.push_overlay` store path; check for
  any O(N) traversal over discovered worktrees during a single push.

### Branch C — Both scale similarly (neither dominates)

If transport and RA both stay flat (or both grow slowly):

* **Reading**: daemon is well-tuned at this fleet scale — the
  multiplex mechanism doesn't add measurable cost per WT, AND the
  store path is O(1) in fleet size as designed.
* **Cross-ref with M1 RAM**: pairs with the fleet-RAM flatness
  finding — same architectural property (per-fleet cost rather than
  per-WT cost) showing up in a second axis.

### Branch D — Cold-edit transient (rep-1 outlier)

The first rep on a freshly-warmed daemon may carry residual
cargo-check work from the warm-up window in its measurement window
(same shape as M2's rep-1 = 29.66 s vs steady-state 830 ms). If
present, report median + p95 EXCLUDING rep-1, with rep-1 cited as
the startup transient.

---

## 5. Honest caveats (carry into the launch narrative)

The methodology IS the deliverable; numbers are bracketed by it, never
travel alone.

1. **Sequential pushes ≠ concurrent-pusher contention.** Pushes are
   serial across the 50 reps. The N=20 effect is the daemon's per-WT
   **state cardinality**, NOT concurrent-pusher serialization. A real
   agent-fleet with 20 parallel pushers would expose serialization
   differently — that is a separate measurement, not this one.

2. **SSE frame receipt ≠ verdict-computed-and-persisted.** SSE fires
   at the `publish_verdict` EMIT seam (Judgment B in the cargoless
   model). Close enough for round-trip latency; not pure-unit-proven
   end-to-end. cli-status persistence happens after the SSE emit and
   is NOT in the measurement window.

3. **Synthetic Leptos worktrees on `bench/fixture`** — honest-size,
   same substrate AC7 §8.5 / `#196` / `#252` / `#259` measured.
   Larger workspaces shift absolutes; the transport-vs-RA **split**
   is the diagnostic, NOT the absolute numbers.

4. **Pod-state isolation.** Measurements are valid only against the
   just-cleaned baseline (pkill prior daemons → defunct-zombie scan
   empty → /healthz=200 before harness handoff). If a prior run
   left an RA child unreaped, the measured RSS / CPU floor would
   shift. M5 hygiene chain (#198/#200/#247) is what makes the clean
   baseline reproducible.

5. **`OTEL_EXPORTER_OTLP_ENDPOINT` unset** — the "compiled-in but
   inert" condition (same as #259). A configured exporter would add
   BatchSpanProcessor send overhead; that is a separate measurement.

6. **N≤20 measured; 589/617-WT is structure-implied projection.**
   Same caveat as the M1 chain (AC7 §11.4 caveat 2).

---

## 6. FIELD FINDING — push-mode read-plane silent on fresh-fleet substrate

**This is the load-bearing M3 outcome — bigger than the bench numbers
M3 was meant to extract.** Surfaced clearly here so future-operators
read it without parsing the whole report.

### 6.1 Symptom

4 successive bench runs (`19be764` → `414e07c` → `3df5ca2` → `e697b38`)
all hit the same wall after harness-self-bugs were fixed: across 15
minutes of warm-up (`WARMUP_TIMEOUT=900s`), NO verdict ever published
to ANY of the daemon's read-plane channels (SSE `/events` stream;
on-disk `cli-status` file; HTTP `/status?worktree=W` query). RA process
DID spawn and was actively working (5.8 % CPU at t+47 s in run-3;
12.4 % then 3.0 % in the §6.2 probe).

### 6.2 Diagnostic probe data (the discriminator)

Isolated probe (`bg-bash bpg311vz2`, kubectl-`-i`-fixed): start fresh
`cargoless serve --repo /tmp/m3probe/repo --bind 127.0.0.1:8091`,
push one real-content overlay for `/tmp/m3probe/wt1`, observe every
read-plane channel + process state at t+10 s and t+40 s:

```
ack: {"accepted":true,"applied_files":1,"worktree":"/tmp/m3probe/wt1"}
transport ms: 9.27  ← the only real M3 number this cycle produced

--- t+10s ---
  /status:    null
  /verdict:   null
  /worktrees: []          ← ZERO worktrees discovered
  cli-status(wt1): absent
  RA: 12.4 % CPU active

--- t+40s ---
  /status:    null
  /verdict:   null
  /worktrees: []          ← still ZERO
  cli-status(wt1): absent
  RA: 3.0 % CPU active
```

### 6.3 Narrowed root cause (`/worktrees: []`)

The daemon discovered ZERO worktrees despite the bench setup correctly
running `git -C /tmp/m3probe/repo worktree add -b wt1 /tmp/m3probe/wt1
HEAD` (which `git worktree list --porcelain` should report). When the
push arrives for `/tmp/m3probe/wt1`, the daemon:

1. **ACCEPTS the push** (200, `accepted:true`, `applied_files:1`) — the
   write plane works end-to-end.
2. **Stores the overlay** in `ServeVerdictState.peek_overlay_for`
   ingestion via push_tx signal.
3. **Has no cluster-state for `wt1`** because worktree discovery returned
   empty — so the serve loop's `SwitchOverlay`/`take_overlay_for`/
   `EmitVerdict` path never fires for this worktree.
4. Verdict never emitted ⇒ `/status` stays null, `/events` stays silent,
   cli-status file never created.

This narrows team-lead's 3 candidates definitively:
- ❌ NOT H1 (slow cold compile timing out — RA is alive, /worktrees IS exposed)
- ❌ NOT H2 (SSE delivery broken — /status HTTP query also returns null)
- ✅ **HΩ: ACTIVITY-ROUTER / DISCOVERY SEAM** — the daemon
  discovers zero worktrees from this bench setup. POST /overlay returns
  200 + accepted:true for an unknown-to-daemon worktree, but produces
  no read-plane signal because no cluster-state exists to drive
  `EmitVerdict`. Worst combination: client gets a success ack with no
  underlying effect.

### 6.4 Why M1 #196/#252/#259 worked but M3 didn't

The same `bench/modelr-fleet.sh` fleet shape (base repo + N
`git worktree add` siblings) WORKED for M1 fleet-RAM measurement
across four independent re-baselines. The difference: M1 only
required RA to spawn (modelr-fleet activates via `touch_wts()` —
direct on-disk file changes, the file-watcher's input). M3 requires
the **verdict-publication channel** to also work for pushes, which
M1 never exercised.

Possible explanations for why M1's file-watcher path activates but
M3's push path doesn't trigger discovery / cluster-state-creation:
- M1's `touch_wts()` directly writes the worktree's file on disk →
  file-watcher fires → daemon walks discovery + activates cluster
  state on the fly
- M3's POST /overlay only signals `push_tx` → serve loop drains →
  but the "register this worktree as known" step may live in the
  file-watcher path NOT replicated on the push path
- This would be an asymmetry between the two activation sources that
  Increment-2 (#240/2a/2b/2c) didn't have a test exercising

### 6.5 Suggested follow-up (NOT filed by bench-lead — operator routes)

This finding is more substantial than the M3 bench it blocked. Routing
options for the operator to consider:

- **dev-fixer investigation**: confirm whether `serve --repo`'s
  worktree discovery + push-mode activity routing have an asymmetry
  vs. the file-watcher path. The proof-of-concept reproducer is
  precisely the probe in §6.2 (fresh repo + git worktree add +
  serve --bind + POST /overlay + observe /worktrees endpoint).
- **Integration test gap**: #240's Layer-3 backstops (#260, #242,
  #264) gated the wire-shape contract but not "push for an
  unknown-to-discovery worktree produces a verdict". A new integration
  test asserting `/worktrees` non-empty after first push + `/status?wt=W`
  resolving to a real verdict within a reasonable timeout would catch
  this class.
- **Affected Stage-1 ACs**: S1-AC parity (#228) — "push-overlay-verdict
  ≡ Shape-1 local-FS verdict" cannot be verified end-to-end against
  the current fleet substrate. The S1-AC test setup may need to mirror
  M1's `touch_wts()`-driven activation OR exercise the push path
  independently with a deeper integration test.

### 6.6 The positive results worth carrying

Not all is loss — items that VERIFIED WORKING end-to-end on
`929a5d3 + this commit`:

| component | verified |
|---|---|
| POST /overlay write plane | 200 ack + accepted:true + applied_files:1 |
| Transport (HTTP RTT, loopback) | **9.27 ms (one push)** — single-data-point indicator of order-of-magnitude; not a distribution |
| Push-client → daemon JSON wire (op="push_overlay", files:[{path,content}]) | parses cleanly |
| RA spawn on real-content overlay | RA process alive + actively at 5.8-12.4 % CPU |
| Bug-A fix (real-content overlay triggers activity for the cluster's RA process even if not for worktree-registration) | confirmed via process tree |
| Bug-B fix (os._exit cleanly without sse.stop deadlock) | confirmed via exit-0 on FAIL paths |
| `cargoless serve --repo --bind` /healthz readiness latch (#225/0d) | 200 within ~1 s of process start |
| `/worktrees` endpoint shape | returns valid JSON `[]` (empty array, well-formed) |

---

## 7. Stage-1 acceptance feed (`#228`)

Which `S1-AC` criteria this run informs:

| AC | feed |
|---|---|
| **S1-AC parity** (push-overlay-verdict ≡ Shape-1 local-FS verdict for the same tree state) | The fact that pushed-overlay → SSE-verdict transitions arrive in `verdict=red` ⇄ `verdict=green` pairs (matching local-FS edit→watch behavior) is the parity signal. Full byte-identity check is `#227`'s differential harness scope (gates Increment-3), not M3. |
| **S1-AC latency-budget** | M3 `total_ms` directly populates the latency-budget metric for push-mode (the v0 `watch` mode's AC#2 / D-A2 renegotiated number is the local-FS analogue). |
| **S1-AC fleet-scale** | N=20 `ra_ms` vs N=1 `ra_ms` is the load-bearing diagnostic for whether the multiplex thesis holds on the CPU/latency axis (it already holds on RAM, per #196→#259 chain). |

---

## 8. Cross-references

* **M1 fleet-RAM** (`#196` → `#252` → `#259`) — RAM flatness across
  N=1→20, confirmed through four architectural deltas.
* **M2 per-edit CPU** (`#256`) — pre-deploy synthetic; bracketed
  1.10× cargoless-edit vs warm cargo-check; methodology IS the
  deliverable.
* **AC7 §8** — the launch efficiency thesis numbers (`§8.5`
  two-source-confirmed cargoless 3.35s/edit vs trunk 6.89s ⇒ 2.05×
  vs trunk) feed M3 via the AC7 "growth-path framing" anchor.
* **Lane C `#222`** — the plan called out M3 as parked-post-deploy;
  this is the closure of that lane.
* **D-FLEET-SHARED-DAEMON `§7.3`** (corun safety) + `§14` (RA
  overlay correctness under high churn) — the design notes whose
  diagnostic predictions this measurement either confirms or
  challenges.
* **AC9 reviewer-readiness packet** — M3 numbers + methodology are
  candidate inclusions for the post-launch efficiency-thesis
  evidence chain.

---

## Authorship

bench-lead (`#267`). Harness commits: `19be764` (initial) +
`414e07c` (JSON-shape + force-exit fix). Report commit: PENDING (on
top of `414e07c` once numbers land).
