# Handover 2026-07-28 — RA reaps strand witness pushes in silence

Reported from a downstream operator deployment running the witness tier
(`CARGOLESS_PROJECT_CHECKS_MODE=warn`) against a large Leptos workspace.

**Image under test:** `cd4b0a6` — this *is* `origin/main` tip, so every line
reference below is exact, not approximate.

Deployment-specific values (namespace, pod name, node, auth token) are read
from `.env` — see `.env.example` and "Reproducing" below. This document keeps
only the values that are properties of cargoless itself.

---

## Summary

rust-analyzer balloons to 60-86 GiB on this workspace and is SIGKILLed by an
external reaper in the deployment's pod manifest. The daemon respawns it and RA
re-indexes from scratch. Each cycle takes ~7 min.

The result is not a clean failure. Verdicts **do** publish — but 22, 55, and 65
minutes after push, against an 85-minute CI budget. When the reap rate wins, the
push is stranded silently and CI burns its full budget against a frozen mailbox.

Two asks, neither of which requires fixing RA's memory behaviour:

| | Ask |
|---|---|
| **P0** | A stranded push must publish `unknown` instead of going silent. |
| **P1** | A native Supervisor-side RA memory cap, so the deployment stops needing an external SIGKILL loop. |

P1 without P0 just reproduces today's silent loop with better logging.

---

## Evidence

### RA growth rate

`/proc`-level RSS, sampled ~10s apart on the witness pod:

```
19,616,924 KB   (t+0s)
37,901,400 KB   (t+11s)
55,224,516 KB   (t+21s)
```

**~1.8 GB/second.** Between checks RA sits stable at ~890 MB. This is
check-triggered, not a steady leak — RA is fine until asked to analyze this
overlay.

### The reap loop

The deployment SIGKILLs any `rust-analyzer` over 60 GiB RSS. Observed reaps,
with the daemon respawning after each — cadence ~7 min, sustained over hours,
**0 pod restarts**:

```
11:58:24  rss=77033Mi
12:42:43  rss=74028Mi
12:56:44  rss=63250Mi
13:05:00  rss=79220Mi
13:08:30  rss=85902Mi
```

Every `check-started` is followed 47-75s later by a reap.

### Verdict latency

| `verdict-latency` | minutes | % of the 5100s CI poll budget |
|---|---|---|
| 1 340 237 ms | 22.3 | 26% |
| 3 322 330 ms | 55.4 | 65% |
| 3 904 220 ms | 65.1 | 77% |

Correlated against the timeline:

```
11:57:20 check-started → 1 reap  → verdict 12:17:46   (1226s span)
12:41:52 check-started → 2 reaps → verdict 13:02:25   (1233s span)
```

Each reap forces a full re-index. **Latency is a function of the reap rate** — a
verdict lands only when a reap-free window happens to be long enough to
complete.

### And when it loses

`/admin/active` sampled four times over 45s, completely static:

```
pending_pushes=2  active_worktrees=1  inflight_batch_runs=0
```

Two overlays accepted and queued, zero inflight, nothing draining, no
cargo/rustc process anywhere in the fleet, RA idle at 894 MB. `/status` served a
frozen entry — same `base_sha`, same `published_at` on five consecutive polls —
while `heartbeat_age_secs` climbed 909 → 1179 → 1637 against a stall threshold
of 900.

> **Calibration:** `pending_pushes` later drained to 0 without a pod restart.
> The stall is **intermittent, not permanent** — treat it as a latency
> distribution with a bad tail, not a hard outage.

---

## Root cause

`ClusterDriver::reset_after_respawn` (`crates/cargoless-core/src/clusterdrv.rs:416-419`)
sets `self.current = None`. Its own doc states the consequence:

> "no `LspEvent` … can produce a `ClusterAction::EmitVerdict` until a fresh
> `DriverEvent::RoutedBatch` arrives … **Honest fail-safe**: silent until the
> next file-change re-routes through the watcher, never a false verdict."

That is correct on the axis it was designed for — it will never emit a false
verdict. What it lacks is a **liveness counterpart**. The overlay was already
consumed, the txn is dropped, `EmitVerdict` never fires, and nothing re-drives
it. The push is stranded and the only symptom is silence.

```
RA balloons on check → reaper SIGKILLs → Supervisor respawns
  → reset_after_respawn() drops the in-flight txn
  → consumed push stranded, never publishes
  → mailbox frozen → CI polls a stale entry for its whole budget
```

---

## P0 — a stranded push must publish `unknown`

Highest value, and independent of anything RA-related: a plain segfault-respawn
strands a push identically.

On RA loss, publish an honest `unknown` for every stranded worktree. That turns
an 85-minute CI timeout into an immediate, nameable infra error — `cargoless
verdict` already maps `unknown` → exit 75 EX_TEMPFAIL
(`crates/cargoless/src/verdict.rs:118-119`), which is exactly the right
semantics: infra couldn't decide, escalate, not a code red.

Seams found while tracing:

- **The stranded set already has a source of truth** — the `push_attribution`
  map (`crates/cargoless/src/serveapi.rs:1549` records at overlay-consume,
  `:1586` removes at EmitVerdict-dispatch). A lingering entry *is* the
  definition of stranded. A `drain_stranded_attributions()` — drain, not peek,
  so a second reap cannot double-publish — is ~10 lines.
- **Keep the publish helper structurally narrow**: take no verdict parameter and
  construct `VerdictPayload::unknown(...)` internally, so green and red are
  *unrepresentable* on this path. That makes it a strictly-safe widening of the
  "exactly one verdict site" doctrine (`crates/cargoless/src/servedrv.rs:39`)
  rather than a weakening — enforced by the signature, not by discipline. Worth
  amending that module doc rather than letting it go quietly false.
- **No race with the Hard witness**: the witness only starts *after*
  `take_push_attribution` pops the entry, so "has an attribution" and "has a
  witness running" are mutually exclusive by construction.
- **Use a stable classifier prefix** (e.g. `ra_memory_cap_reaped: …`) so
  dashboards and downstream CI classifiers can group on it.

---

## P1 — native Supervisor-side RA memory cap

- `monitor_loop` (`crates/cargoless-core/src/analyzer.rs:257-337`) already polls
  at 40ms and already owns the `Child` — no new thread needed.
- Suggested knob `CARGOLESS_RA_MAX_RSS_MB`, **default 0 = off**, matching the
  repo convention (`CARGOLESS_WITNESS_MAX_INFLIGHT` defaults 0,
  `CARGOLESS_WITNESS_WARM_TARGET` defaults off). Put the zero-check *inside* a
  pure `should_reap(rss, cap)` so default-off is provable by unit test rather
  than by reading the call site.
- The kill needs the **process group**, not just the child: extract the
  group-kill body of `ReapOnDrop::drop`
  (`crates/cargoless-core/src/analyzer.rs:539-568`) into a reusable
  `kill_process_group(pid)`. `SuspendHandle::suspend` uses only `c.kill()`,
  which would leave `proc-macro-srv` descendants behind.
- `/proc/<pid>/status` is Linux-only and would be this repo's first
  `target_os` cfg. Keep the *parser* un-cfg'd and pure so it unit-tests on macOS
  dev machines; non-Linux returns `None` ⇒ cap never fires ⇒ byte-identical
  behaviour.
- Sizing is the main hazard: this is a **runaway detector, not a budget**. Too
  low ⇒ a permanent reap loop where RA never finishes indexing. Derive it from
  the deployment's own memory limit; do not hard-code a constant (the in-repo
  reference manifests say 48Gi, this deployment runs 240Gi).
- Tests: model on `WitnessInflightGate` (`crates/cargoless/src/serveapi.rs:4286+`)
  — construct the struct in-test with explicit fields, keep env reads in
  `Default`. Key cases: cap=0 never reaps at any RSS incl. `u64::MAX`; the `>=`
  boundary; `VmRSS` parsed and not the adjacent larger `VmHWM`; malformed ⇒
  `None` ⇒ never reaps.

---

## P0b — operator break-glass (deployment-side, not a cargoless change)

Recorded here because the obvious move is the wrong one.

The reaper's 60 GiB cap sits below RA's observed ~83 GiB peak, so raising it to
~96 GiB would let a check finish and unfreeze the mailbox. Measure the blast
radius before doing that. On this deployment:

| | |
|---|---|
| node allocatable | 251 GiB |
| witness pod memory **limit** | 240 GiB (**96% of the whole node**) |
| witness pod memory *request* | 8 GiB |
| co-tenant pods on that node | 73 |

The limit is not a real ceiling — it is 96% of the node, backed by an 8 GiB
request. A runaway RA allowed past ~96 GiB does not hit a container OOM first;
it competes with co-tenants for node memory and the kubelet starts evicting by
QoS. On this deployment the entire canonical + shard fleet shares that node.

So the safer break-glass is a **targeted pod restart** — clears the stranded
state and the frozen mailbox immediately, costs one RA warm-up. If the cap is
raised at all, raise the memory **request** alongside it so the scheduler
accounts for the reservation; otherwise the risk lands on the neighbours rather
than the witness.

Check your own numbers before applying any of this — set
`CARGOLESS_WITNESS_NODE` in `.env` and compare node allocatable against the
pod's limit and request.

## P2 — observability gaps

Each small, each independently useful. These are what made the diagnosis take
hours:

- **No witness-start log line.** Only terminal outcomes are logged, so inflight
  state and duration cannot be reconstructed from stderr.
- **`WitnessInflightGate` exposes nothing** — no counter, no getter, no log on
  any path, *including* its silent fail-open after the 600s queue budget
  (`crates/cargoless/src/serveapi.rs:686-693`). You cannot distinguish
  "compiled 80 min" from "queued 10, failed open, compiled 70".
- **Overlay accepts are not logged.** We initially concluded from logs that no
  `/overlay` POST had reached the pod; `/admin/active` proved otherwise. That
  wrong inference cost real time.
- **`verdict.project_checks` span is skipped on the coalesced lane** — the early
  return at `crates/cargoless/src/servedrv.rs:2279` precedes the span at
  `:2316`, and the coalesced lane is dominant in central-daemon push mode. On
  the direct lane the span is entered *after* the compile, so it carries ~zero
  duration. `duration_ms` exists only on the direct lane.
- **`verdict.publish` has no witness-vs-RA-native discriminator**, so no
  dashboard query can split the lanes for a per-lane SLO.

## P3 — the 10s SLO is a category error for the witness lane

> **RESOLVED 2026-08-25** — scoped per-lane: `publish_verdict` now scores
> `gated_checks_ran`-non-empty verdicts against `CARGOLESS_WITNESS_SLO_MS`
> (default 45m, between the ~20-40m warm range and the 4800s wall) and
> everything else against the original `CARGOLESS_VERDICT_SLO_MS` (10s);
> the `verdict-latency` line gained `class=witness|ra-native` and the
> `verdict.publish` span gained `witness_compile`/`gated_checks_ran` attrs
> so dashboard queries can split the lanes (P2). Historical text below.

`CARGOLESS_VERDICT_SLO_MS` (default 10s,
`crates/cargoless/src/servedrv.rs:1676-1685`) was authored against the ~2s
RA-native budget, but `publish_verdict` is shared by all modes. So **every**
witness verdict prints `slo_breach=true` by construction — all three samples
above did. The witness's real budget is the 4800s wall.

Either scope the SLO per-lane or stop emitting the bit for witness verdicts.
As-is it is noise, and "p90 22.9 min breaches a 10s SLO" should not be read as a
finding — it is an artifact.

## P4 — separate ticket: same-SHA stale-verdict acceptance

`status_is_acceptable` (`crates/cargoless/src/verdict.rs:460`, see the doc block
above it) documents: *"both sides carry a SHA and they MATCH ⇒ accept, freshness
ignored (idempotent re-run fast-path)"*. Combined with the `verdict_history`
ring, a re-push of the same commit can accept a **pre-push stale verdict**.
Likely why `/status` returned 200 with an unchanged `published_at` rather than
404 while the push was stranded. Distinct correctness question — may be the
difference between CI seeing a stale *green* and CI seeing nothing.

---

## Ruled out — do not spend cycles here

Verified on the live pod via `GET /daemon`:

```
proc_macro_requested = false
proc_macro_resolved  = false
cargo_check_enabled  = false
```

**RA proc-macro expansion is already disabled and the balloon happens anyway.**
The first mitigation most people reach for — "turn off `view!` expansion" — is
already the deployed state. The ~54k `inference diagnostic in desugared expr`
ERRORs come from builtin desugaring (`?`, `format_args!`, for-loops), not macro
expansion.

- **`diagnostics.enable=false`** — RA-native diagnostics are the *only* verdict
  signal in this mode; disabling them yields permanent `unknown`.
- **`files.excludeDirs`** — excluded paths return `unknown` for exactly the
  paths CI gates on.
- **`numThreads` / `lru.capacity`** — cheap to try, but speculative: a 1.8 GB/s
  climb is one runaway query's transient allocation, which an LRU cap on
  *memoized results* does not bound.
- **`CARGOLESS_WITNESS_MAX_INFLIGHT` is not the constraint.** Checked
  specifically: the gate had a free slot and two waiting pushes and still ran
  nothing. Raising it changes nothing about this wedge. Independently, one slot
  already runs up to 4 concurrent `cargo check --release` since the `dev`
  profile is `max_parallel: 6`.
- **`CARGOLESS_BATCH_MAX_WAIT_MS` is parsed-but-inert**
  (`crates/cargoless/src/serveapi.rs:539-543`, `#[allow(dead_code)]`, zero
  production readers) — worth either wiring up or warning at startup when an
  operator sets it, since today it silently does nothing.

---

## Log noise — the opt-out already exists

54,881 lines in 2h; 53,959 are RA's `inference diagnostic in desugared expr`.
This was nearly filed as a daemon bug. It is not:

- The string is the **documented signature of the balloon** — the deployment's
  own manifest comment records a prior incident of "1.8Gi → 115Gi in ~90s (storm
  ×14k)". The noise and the OOM are one phenomenon.
- `CARGOLESS_RA_STDERR=null` (`crates/cargoless-core/src/analyzer.rs:378-381`)
  already exists. This deployment never set it. Operator-side gap, not a bug.
- `Stdio::inherit()` is deliberate (commit `5914e1c`, "make RA death visible"),
  and silencing it blind would hide the very crash loop that is the actual
  problem. Set it only *after* P1 lands, so the cap's own structured line
  preserves visibility.

One documentation ask: **`CARGOLESS_RA_LOG_FILE` and `CARGOLESS_RA_STDERR` are
independent** — setting the former still inherits stderr. Correct behaviour, but
undocumented outside a passing mention in
`docs/dogfood/TF-MULTIVERSE-CANARY.md:128`.

---

## Corrections to earlier downstream reports

Two claims that reached cargoless from this deployment were **wrong**, and both
were our bugs, not cargoless's:

**1. "77% of witness failures are overlay contention" — withdrawn.** The
downstream CI counter incremented once per poll tick against a 3s poll loop, so
it measured elapsed polling time ÷ 3, never peer verdicts. Every sample divides
out to the poll interval: 77/246s = 3.19s; 209/680s = 3.25s; 893 counts = 44.6
min on *one unchanged entry*. Any earlier report citing that figure, or "N
verdicts for other SHAs", should be discarded — it also sent us chasing
`CARGOLESS_WITNESS_MAX_INFLIGHT`, which was provably idle. Fixed downstream.

**2. A "false red" claim — withdrawn.** We suspected the witness was returning
spurious reds. Independent SSR-check data refuted it: on four earlier heads the
SSR check *agreed* with the witness, and four genuine defects were traced
(missing comma → 21 errors; `Some()` double-wrap → 18; route arity >16 → 17;
missing `.clone()` → 1). Identical error counts across SHAs were the same real
defects persisting. **The witness was telling the truth.** Its problem is
latency and silence, not correctness.

---

## Reproducing

Deployment-specific values come from `.env` (gitignored). Copy `.env.example`
and fill in your own:

```sh
cp .env.example .env
# edit .env, then:
set -a && . ./.env && set +a
```

```sh
# 1. The queue. pending_pushes > 0 with inflight_batch_runs = 0 is the stall.
kubectl -n "$CARGOLESS_NS" exec "$CARGOLESS_WITNESS_POD" -c serve -- sh -c \
  'curl -s -H "Authorization: Bearer $CARGOLESS_AUTH_TOKEN" localhost:8787/admin/active'

# 2. The mailbox. published_at must ADVANCE between polls ~30s apart;
#    heartbeat_age_secs must stay under 900.
kubectl -n "$CARGOLESS_NS" exec "$CARGOLESS_WITNESS_POD" -c serve -- sh -c \
  'curl -s -H "Authorization: Bearer $CARGOLESS_AUTH_TOKEN" -G \
   --data-urlencode "worktree=$CARGOLESS_WORKTREE" localhost:8787/status'

# 3. The reap loop. Should trend to 0.
kubectl -n "$CARGOLESS_NS" logs "$CARGOLESS_WITNESS_POD" -c serve --since=1h \
  | grep -c "ra-reaper: killing"

# 4. RA memory. Should settle near the fleet's 4-6GB steady state.
kubectl -n "$CARGOLESS_NS" exec "$CARGOLESS_WITNESS_POD" -c serve -- sh -c \
  'ps -eo pid,etimes,rss,comm | grep rust-analyzer'
```

**Success for P0:** a reaped witness publishes `unknown` with a stable
classifier within seconds, instead of CI polling a frozen mailbox for 85
minutes.

**Success for P1:** reap count trends to zero and the external reaper can be
removed from the deployment manifest.

Raw captured evidence (reap/respawn timeline + 24h verdict inventory) is in
[`evidence/2026-07-28-witness-ra-reap.txt`](evidence/2026-07-28-witness-ra-reap.txt).
Pod logs rotate, so that capture is the durable record.

---

## Why this deployment and not the others

Other pods in the same fleet run RA at healthy steady state:

| pod | RA resident set | state |
|---|---|---|
| canonical `cargoless-serve` | 6.2 GB | steady |
| `cargoless-serve-shard-0` | 3.9 GB | steady |
| `cargoless-serve-shard-extra-0` | 6.2 GB | steady |
| **witness tier** | **60-86 GB** | **reaped** |

The witness is the only pod running `CARGOLESS_PROJECT_CHECKS_MODE=warn` —
canonical and shards run `off` — so it is the only one materializing gated PR
overlays into RA. That is the differentiator, and why this reproduces on the
witness tier specifically.
