# cargoless v0 — Execution Playbook

Working name **cargoless** (repo/binary placeholder). Shipping product name is
open decision **D1** (Plane CWDL-12). Authoritative backlog: Plane project
**CWDL** (82 issues; CWDL-1 is the Definition-of-Done umbrella with the 9 ACs as
sub-issues).

## Vision cut (apply to every change)

> The codebase always knows what works, and tells you the moment it doesn't.

If a change does not sharpen the codebase's self-knowledge or reduce the latency
from brokenness to signal, it is **not v0** — it goes to the v1 parking lot.

## Build model — push-to-build, NO local cargo

`cargo`/`rustc`/`sccache` are not run locally. The **only** build/test path is
Forgejo CI (`.forgejo/workflows/ci.yml`) on `triform/cargoless`. Workflow:

1. Branch `agent/<role>` off `main`.
2. Touch only your owned crate(s) — ownership is disjoint (see map).
3. `git add <explicit paths>` (never `-A`), commit small, push the branch.
4. CI runs per-branch. Read job status via the Forgejo tasks API
   (`/api/v1/repos/triform/cargoless/actions/tasks`) — this build does not
   expose logs over REST, so CI is **one job per check** (build / test / fmt /
   clippy) and the failing job name *is* the diagnosis.
5. Report branch + commit to the lead. The lead keeps `main` always-green and
   merges CI-green branches. Agents do **not** push to `main`, do not
   pull/merge/rebase (the lead integrates).

`rustfmt <files>` may be run locally to make formatting deterministic before a
push (it is the one allowed local toolchain command).

## Crate ownership (disjoint)

| Crate | Owner role | Epic |
|---|---|---|
| `tf-proto` | proto-contracts | 1 / D8 — the cross-crate contract seam |
| `tf-cas` | build-cas | 3 — ContentStore trait + local-disk |
| `tf-core` | daemon-core + devserver + build-cas | 2 / 3 / 4 |
| `tf-cli` | cli-ux | 5 |
| `bench/` | ra-bench | S1 spike + AC#2 harness |

`tf-core` is shared by three roles — they own **disjoint modules** inside it
(`watcher`/`analyzer`/`model` = daemon-core; `build` = build-cas; `server` =
devserver) and communicate only through `tf-proto` types.

## Acceptance criteria → verifying work

| AC | What proves it | Owner |
|---|---|---|
| 1 zero-config serve <30s (D-A1 redefined: daemon up + holding page) | clean-env integration test | cli-ux + integration |
| 2 median save→verdict <1s | committed CI bench harness (also S1) | ra-bench |
| 3 green-save→browser <5s | bench harness | ra-bench + devserver |
| 4 never serve red | integration test | devserver |
| 5 CAS dedupe = build skipped | integration test (skeleton already has the dedupe primitive) | build-cas |
| 6 survives kill -9 of rust-analyzer | integration test | daemon-core |
| 7 benchmarks beat trunk/bacon on ≥2 dims | comparative bench, README | ra-bench |
| 8 README/ROADMAP/CONTRIBUTING/LICENSE | present (this repo) + governance | proto-contracts (docs) |
| 9 launch blog reviewed by ≥2 incl. outside | human-gated — will NOT close this session | lead |

ACs 4/5/6 are the realistically-closable-this-session set; 2/3/7 depend on the
bench harness; 1 depends on D-A1; 8 is largely done; 9 is human-gated.

## Scope decisions already taken (do not relitigate)

- **D-A1**: AC#1 "works in <30s" is impossible for a cold Leptos build → it
  means *daemon up + config auto-detected + holding page, zero manual config*;
  first-green when the cold build finishes.
- **D-A2**: AC#2's sub-1s wording is provisional until the S1 bench reports;
  renegotiate on evidence, do not silently miss it.
- **D-A3**: the benchmark substrate is pulled forward (it is the S1 harness),
  not deferred to a late epic.
- v1 parking lot (NOT v0): salsa/RA-as-library, remote CAS, team/auth,
  multi-agent, editor plugin, symbol-level granularity, replacing trunk build,
  hot-swap, CI integration, Windows.

## Sequencing

Wave 1 (no cross-deps): `proto-contracts` (unblocks everyone — go first),
`ra-bench` (independent), `daemon-core` (watcher needs no rich proto).
Wave 2 (after the tf-proto contract is merged green): `build-cas`,
`devserver`, `cli-ux`, `integration`.
Recycle: shut an agent down once its epic is merged green; re-spawn/re-engage
on demand. Keep the team ≤10 and lean.

## Dedicated build gate (`scripts/ci-gate`)

The shared Forgejo Actions runner is a single serial runner contended by
tf-multiverse + ~12 agents (frequently ~40min backlogged) and its job logs
are **not** readable over the Forgejo REST API. It is therefore unusable as a
fast pre-integration merge gate. cargoless has its own.

**What it is.** A dedicated, isolated Kubernetes builder
(`deploy/cargoless-builder.k8s.yaml`): namespace `cargoless-builder`, a
single `Deployment/cargoless-builder` pod running the pre-baked
`registry.triform.cloud/mirror/triform-builder-v2` toolchain image, a
40Gi RWO PVC (`cargoless-cache`) holding the warm `CARGO_HOME`,
`CARGO_TARGET_DIR` and a PVC-resident rustup `1.85.0` toolchain (matches CI's
`rust:1.85-bookworm` and `rust-toolchain.toml`), plus a namespace-scoped
egress `NetworkPolicy`. It is **fully independent** of tf-multiverse's
`triform-builder` namespace and of the Forgejo runner — nothing in
`triform-builder` is referenced or modified.

**Why it works as the gate.** `kubectl exec` stdout *is* readable, so the
real per-check PASS/FAIL and the actual compiler/test output land on your
terminal — the unreadable-Forgejo-logs problem disappears.

**How the lead / agents invoke it** (from the cargoless repo root):

```
scripts/ci-gate <ref>        # verify a branch/sha before integration
scripts/ci-gate origin/main  # default ref if omitted
scripts/ci-gate --provision  # (re)apply the builder manifest if it drifts
```

The `<ref>` must exist in the local clone — `ci-gate` never fetches/pulls
(agents don't; the lead already has the branches). The tree at that ref is
`git archive`-streamed into the pod over `kubectl exec` stdin (tracked files
only, no in-pod git, no registry auth, deterministic). It then runs, in the
streamed tree, the exact four checks `.forgejo/workflows/ci.yml` runs:

| check | command |
|---|---|
| build  | `cargo build --workspace --all-targets` |
| test   | `cargo test --workspace` |
| fmt    | `cargo fmt --all -- --check` |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` |

It prints a per-check verdict table and exits `0` iff all four are green
(`1` if any red, `2` on transport/setup error). All four run even if an
early one fails (`CI_GATE_KEEP_GOING=0` to stop at first failure). Remote
cargo invocations carry the operator-sanctioned
`TRIFORM_OPERATOR_APPROVED_BUILD=1` escape (the operator explicitly
authorised cargoless its own kubectl/remote-cargo builder).

**Integration protocol.** An agent reports `branch + commit` to the lead as
before; the lead runs `scripts/ci-gate <branch>` and merges into `main` only
on `ALL GREEN`. The warm PVC target cache makes repeat runs a relink, not a
from-scratch rebuild. Forgejo CI still runs per-branch as the durable record;
`ci-gate` is the fast readable gate that actually unblocks merges.
