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

`rustfmt` may be run locally to make formatting deterministic before a push
(it is the one allowed local toolchain command). **Always pass
`--edition 2024`** — this is an Edition-2024 workspace and the CI `fmt`
gate runs `cargo fmt` (which infers `style_edition = 2024`). Bare
`rustfmt <file>` silently defaults to **edition 2015**, whose import-sort
rules differ — it will *regress* already-correct code and turn the `fmt`
gate RED while build/test/clippy stay green (a confusing, builder-round-
wasting failure that has bitten ≥2 agents: dev-fixer #93,
docs-launch-lead pre-#87).

**Self-gate checklist — before every push, in addition to running
`scripts/ci-gate`:**

```
1. rustfmt --edition 2024 --check crates/**/*.rs   # MUST be clean (exit 0, no diff)
2. scripts/ci-gate > full.log 2>&1 ; grep -nE 'error|warning|FAIL' full.log
3. inserted a free `fn` next to a documented item? re-read the diff hunk
```

1. **rustfmt edition pre-gate.** Free, instant, catches the exact
   edition-2015-vs-2024 trap above before a `fmt` builder round is spent.
   To *fix* (not just check): `rustfmt --edition 2024 <files>`.
2. **Never pipe/tail the gate — grep the file.** Redirect `scripts/ci-gate`
   to a file and grep the file; do **not** `| tail`/`| head`/`| grep`
   inline. A streamed filter can drop the one clippy diagnostic that is
   the actual diagnosis (this build exposes no REST logs, so the gate's
   own full output is the only ground truth — dev-fixer lost a clippy
   diagnosis to its own inline filter twice). Read `full.log` whole.
3. **Free-fn-next-to-doc-item: verify the rustdoc didn't split.** When you
   insert a free `fn` adjacent to a documented item, anchoring an `Edit`
   on `pub fn X` can land the new fn *between* `X`'s `///` block and `X`,
   silently producing `clippy::doc_lazy_continuation` — `fmt`/`build`
   stay green, only `clippy` reddens. Re-read the post-edit hunk and
   confirm every `///` block still abuts its item (root cause of 2 of
   dev-fixer's 4 self-gate red rounds on #114).

## Crate ownership (disjoint)

| Crate | Owner role | Epic |
|---|---|---|
| `tf-proto` | proto-contracts | 1 / D8 — the cross-crate contract seam |
| `tf-cas` | build-cas | 3 — ContentStore trait + local-disk |
| `tf-core` | daemon-core + devserver + build-cas | 2 / 3 / 4 |
| `tf-cli` | cli-ux | 5 |
| `bench/` | ra-bench | S1 spike + AC#2 harness |

`tf-core` is shared by disjoint modules — `watcher`/`analyzer`/`model` =
daemon-core; `build` + the latest-green publisher = build-cas. The `server`
module is **v0.1** (live HTTP/WS dev-server), not v0; it is preserved on
`agent/devserver*` branches and consumes the v0 published output. All
cross-module data flows only through `tf-proto` types.

## Phasing (v0 / v0.1 / v1) — Plane CWDL-1 is the single source of truth

- **v0** = headless continuous checker + latest-green publisher. No browser,
  no HTTP. This is the launch gate.
- **v0.1** = optional live server/browser-reload adapter over the v0 published
  `.cargoless/latest-green` (HTTP, WebSocket, Trunk-compatible reload D3,
  browser shim, holding page, browser "never serve red", `serve`). Deferred,
  not deleted.
- **v1** = parking lot (below).

## Acceptance criteria → verifying work (mirrors Plane CWDL-1)

| AC | What proves it | Owner |
|---|---|---|
| 1 zero-config **headless** startup <30s (D-A1: daemon up + auto-detected + watch→verdict pipeline live; NO browser) | clean-env integration test | cli-ux + integration |
| 2 median save→verdict <1s (primary) | committed CI bench harness (also S1) | ra-bench |
| 3 median green-save → latest-green artifact **published** latency (no sub-second claim; D-A2 sets threshold) | two-mode bench harness, artifact mode | ra-bench + build-cas |
| 4 never **publish** red — `.cargoless/latest-green` only advances on green | publisher integration test (headless, not a browser) | build-cas |
| 5 CAS dedupe = build skipped | integration test | build-cas |
| 6 survives kill -9 of rust-analyzer | integration test | daemon-core |
| 7 benchmarks beat trunk/bacon on ≥2 dims, **two-mode** (checker vs artifact, never blended) | comparative bench, README | ra-bench |
| 8 README/ROADMAP/CONTRIBUTING/LICENSE | present (this repo) + governance | docs |
| 9 launch blog reviewed by ≥2 incl. outside | human-gated | lead |

ACs 4/5/6 are the realistically-closable set; 2/3/7 depend on the two-mode
bench harness; 1 depends on D-A1 (headless); 8 is largely done; 9 is
human-gated. **Integrity rule:** an AC is Done only when its verifying test is
green on `main` — branch-only work is In Progress, never Done.

## Scope decisions already taken (do not relitigate)

- **D-A1**: AC#1 is **headless** — *daemon up + config auto-detected +
  watch→verdict pipeline live, zero manual config* within 30s; first-green
  when the cold build finishes. No browser, no holding page (that moved to
  v0.1).
- **D-A2**: AC#2's sub-1s wording is provisional until the S1 bench reports;
  AC#3's publish-latency threshold is likewise set from S1/bench evidence —
  **no sub-second artifact claim**. Renegotiate on evidence, never silently
  miss it.
- **D-A3**: the benchmark substrate is pulled forward (the S1 harness), not
  deferred; it is two-mode (checker save→verdict + artifact save→publish,
  reported separately).
- **Contract status (ratified ledger)**: the four `tf-proto` seams
  (StateEvent / BuildTrigger / BuildResult / ArtifactMeta) are frozen on
  `main` and unchanged. The latest-green publisher is the **only additive v0
  contract surface**: additive serde-free types `PublishedArtifact { artifact:
  ArtifactMeta, published_at: UnixSeconds }` + `UnixSeconds(u64)`; the
  `.cargoless/latest-green` pointer is written atomically (temp + fsync +
  rename) and surfaces input_hash/profile/target/timestamp human-readable; the
  v0 data-flow ends at the publisher (no browser sink). `server::Bundle` is
  **not** in v0. Per-step `Cargo.lock` discipline (committed lock, `--locked`
  CI) applies to every change. Sequencing follows the #26 integration plan.
- v0.1 (NOT v0): HTTP/WS dev-server, reload protocol, browser shim, holding
  page, browser never-serve-red, `serve`.
- v1 parking lot (NOT v0 or v0.1): salsa/RA-as-library, remote CAS, team/auth,
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
scripts/ci-gate <ref>          # verify a branch/sha before integration
scripts/ci-gate origin/main    # default ref if omitted
scripts/ci-gate --bench <ref>  # run S1/AC#2 bench + publish s1-ac2-verdict
scripts/ci-gate --provision    # (re)apply the builder manifest if it drifts
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

If the streamed tree's `crates/tf-cli/Cargo.toml` declares an `integration`
feature (the converged set does; plain `main` does not), the gate ALSO runs
`build`/`test`/`clippy` for `-p tf-cli --features integration` — the wired
daemon that the default workspace build deliberately excludes. On a tree
without that feature those three rows show `SKIPPED` (not a failure). This
makes a single `scripts/ci-gate <convergence-branch>` cover BOTH the
default/standalone semantics and the wired-daemon build in one run.

It prints a per-check verdict table and exits `0` iff all checks are green
(`1` if any red, `2` on transport/setup error). All run even if an
early one fails (`CI_GATE_KEEP_GOING=0` to stop at first failure). Remote
cargo invocations carry the operator-sanctioned
`TRIFORM_OPERATOR_APPROVED_BUILD=1` escape (the operator explicitly
authorised cargoless its own kubectl/remote-cargo builder).

**Integration protocol.** An agent reports `branch + commit` to the lead as
before; the lead runs `scripts/ci-gate <branch>` and merges into `main` only
on `ALL GREEN`. The warm PVC target cache makes repeat runs a relink, not a
from-scratch rebuild. Forgejo CI still runs per-branch as the durable record;
`ci-gate` is the fast readable gate that actually unblocks merges.
