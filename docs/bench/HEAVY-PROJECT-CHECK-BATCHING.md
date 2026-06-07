# Heavy project-check batching proof runbook

This runbook is for sizing and validating Cargoless batching with real,
expensive project checks. It exists because lightweight batch-gate checks prove
the attribution protocol, but they do not prove whether a 40-agent fleet can
share heavy compiler witnesses cheaply enough.

## What this proves

Use `bench/heavy-project-check-throughput.sh` to generate and run native
`batch_check` requests whose changed files intentionally match a project's
heavy `cargoless.checks.yaml` trigger patterns.

For `tf-multiverse`, the default scenarios are:

| Scenario | Synthetic path | Expected Triform witnesses |
|---|---|---|
| `ssr` | `server/src/cargoless_heavy_bench/*.rs` | `ssr-compiler-witness` |
| `wasm` | `portal/src/cargoless_heavy_bench/*.rs` | `wasm-compiler-witness` plus SSR, because Triform has no WASM-only trigger class |
| `isolator` | `isolator/src/cargoless_heavy_bench/*.rs` | `isolator-vsock-compiler-witness` |
| `all` | `runtime-types/src/cargoless_heavy_bench/*.rs` | SSR, WASM, and isolator witnesses |
| `mixed` | Alternates all configured prefixes | Mixed 40-agent pressure |

The green overlays add unreferenced `.rs` files. That is intentional: the
project-check manifest runs because the path matches the trigger, while the
compiled crate behavior stays unchanged.

## Dry-run proof

Run this first. It validates request construction without contacting a daemon
or starting heavy checks.

```bash
DRY_RUN=1 REMOTE=http://127.0.0.1:9 SERVER_ROOT=/workspace/tf-multiverse NLIST='1 2 4' bench/heavy-project-check-throughput.sh
```

Concurrent dry-run:

```bash
DRY_RUN=1 MODE=concurrent REMOTE=http://127.0.0.1:9 SERVER_ROOT=/workspace/tf-multiverse REQUESTS=4 BATCH_SIZE=10 bench/heavy-project-check-throughput.sh
```

Daemon-side coalescing dry-run:

```bash
DRY_RUN=1 MODE=concurrent REMOTE=http://127.0.0.1:9 SERVER_ROOT=/workspace/tf-multiverse REQUESTS=40 BATCH_SIZE=1 COALESCE_KEY='tf-heavy:{scenario}:origin-dev' bench/heavy-project-check-throughput.sh
```

Inspect the generated JSON under `$WORK` and confirm:

- `op=batch_check`
- `coalesce_key` is absent for explicit preassembled batch tests, or present
  and scenario-scoped for daemon-side coalescing tests
- `options.repo_relative=true`
- `options.analysis_root` points to the daemon-side checkout
- each member has both `files[].path/content` and matching `changed_files`
- the scenario paths match the intended heavy witness trigger classes

## Live/heavy proof matrix

Do not run this against shared infrastructure without explicit operator
approval. Heavy witness runs start real compiler checks.

Minimum matrix:

- Sweep mode: `NLIST='1 2 4 8 16 40'`
- Concurrent mode: `(REQUESTS,BATCH_SIZE)` of `(40,1)`, `(8,5)`, `(4,10)`, `(1,40)`
- Queue mode: repeat concurrent `(40,1)` with `COALESCE_KEY='tf-heavy:{scenario}:origin-dev'`
- Scenarios: `ssr wasm isolator all mixed`
- Red attribution probe: `FAIL_SCENARIO=one-red` with a compiled file path
  such as `RED_PATH=server/src/main.rs` or another scenario-appropriate file

Record:

- raw request JSON for every run
- raw batch report JSON for every run
- wall-clock duration printed by the harness
- top-level `verdict`, `combined_checks`, `solo_checks`, and `duration_ms`
- `queue_wait_ms`, `executed_members`, and `executed_batch_id` from each
  submitter-facing report; these are the AX/tuning fields that answer "how
  long did this agent wait to join the shared run?" and "how many members paid
  for the same physical check?"
- per-member `verdict`, `provenance`, diagnostics count, and duration
- daemon project-check log rows, especially `checks=`, `skipped=`,
  `cache_hits=`, `duration_ms=`, and `slowest=`
- host/pod CPU and memory pressure during the run

## Interpretation

Green fleet fast path:

- `verdict=green`
- `combined_checks=1`
- `solo_checks=0`
- every member provenance is `combined_green`

Real red attribution:

- combined batch may go red first
- fallback solos should identify one or more `solo_red` members
- unrelated members should get `solo_green`

Interaction red:

- combined red plus every solo green is `interaction_red`
- do not blame a single submitter
- the agent-facing wrapper should present this as a hold/retry/escalation, not
  as a normal code red

Known diagnostic caveat for Triform at the time this runbook was written:
`scripts/ci/cargo_json_to_cargoless_diagnostics.py` hardcodes the SSR check id
for emitted rustc diagnostics. Top-level red/green and durations are still
useful, but per-check compiler diagnostic attribution should not be trusted
until that script accepts the running check id.

## What the benchmark should decide

The real data should size the server-side coalescing policy:

- debounce window: how long to wait for a burst before dispatch
- max-wait window: the largest acceptable extra submitter wait
- max members per batch: the point where overlay size, reset/materialize time,
  or fallback cost stops improving throughput
- concurrency: how many heavy batches the infrastructure can handle without
  cache thrash or CPU/memory saturation

The agent-experience target is not just fastest global throughput. A good
window is one where each submitter sees a terse, local answer (`green`, `red`,
or `indeterminate`) plus enough shared-run metadata to explain the wait:

- `queue_wait_ms` should stay small relative to the avoided compiler witness
  time.
- `executed_members` should rise during bursts (proof the run was shared) but
  must not grow past the point where fallback on red makes everyone slower.
- `executed_batch_id` should let operators correlate several submitter reports
  back to the same daemon log row without teaching agents what a "batch" is.

Only after this matrix is captured should Cargoless make native queueing the
default behavior for real heavy checks.
