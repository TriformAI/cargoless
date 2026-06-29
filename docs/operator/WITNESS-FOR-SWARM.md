# Arming the cargo-check witness for a swarm (macro-aware verdicts)

**Status:** the mechanism is fully implemented and shipping; this is the
operator recipe to turn it on. Phase 2 of "make cargoless usable for a swarm of
agents in tf-multiverse." See `docs/design/D-PROJECT-CHECKS.md` for the manifest
design and `docs/dogfood/TF-MULTIVERSE-CANARY.md` for the validation ladder.

## The problem this solves

In the default serve mode (`CARGOLESS_VERDICT_MODE=ra`, RA flycheck off), a
served **GREEN means only "rust-analyzer's native pass found no parse / early-
resolution error."** RA-native is **blind to Leptos `view!`/`#[component]`
expansion and to the whole type/trait/method error class (E0599/E0382)**. So a
component edit that fails to compile can publish GREEN — a false green a swarm
agent cannot trust. (And on this path a real RA-native error becomes `unknown`,
never `red` — `statusfile::from_bool_unattributed` returns only Green/Unknown.)

The fix is **not** "turn RA proc-macros on" (RA still can't type-check). It is to
run a real **`cargo check`** for pushes that touch macro code: `cargo check`
expands proc-macros itself and is the *complete* compiler authority, independent
of RA's ABI. That is the **cargo-check witness** — a `kind: command` project-
check, escalated to only on macro-touching pushes so non-macro pushes keep the
fast ~2s RA-native path.

## What's already built (no cargoless code change needed)

- The witness command-check, Hard-mode verdict (`compose_hard_mode_payload` →
  real `VerdictPayload::red(n)`), the off-loop supervisor + watchdog, the
  per-run scratch worktree, and the macro-blind escalation logic
  (`compute_macro_blind_hit`, `effective_project_checks_mode`) all ship on
  `main`.
- tf-multiverse already **commits** the `ssr-compiler-witness` check in its
  `cargoless.checks.yaml` (see the example below).

So arming this is **pure configuration in the live tf-multiverse Flux manifest**
(`deployment/cargoless-builder/{serve,shards}.yaml` — *not* the reference copies
in this repo, which Flux never reads).

## The arming recipe (live tf-multiverse Flux serve/shards manifest)

```yaml
env:
  # Base mode MUST be warn — escalation only promotes Warn→Hard, never
  # Off→Hard. (Off + escalate = no-op: you'd get the green→unknown downgrade
  # but never a witness.) The live daemons already run warn.
  - { name: CARGOLESS_PROJECT_CHECKS_MODE, value: "warn" }
  # Changed-file globs (comma-separated, segment-glob: ** spans segments).
  # Mirror the witness check's own triggers so the blind classification and
  # the witness agree on what "macro-touching" means.
  - { name: CARGOLESS_MACRO_BLIND_PATHS,
      value: "portal/**,server/src/**,chemistry/generated/**,physics/src/**,runtime-types/**" }
  # Escalate a blind-path push to the real witness (vs only downgrading its
  # green to unknown). Strict "1".
  - { name: CARGOLESS_MACRO_BLIND_ESCALATE, value: "1" }
  # Optional: narrow false-fires to files that actually invoke view!/#[component]
  # (content scan on top of the path glob). Omit for pure path-glob.
  - { name: CARGOLESS_MACRO_BLIND_MACROS, value: "view" }
  # Already set live — the witness script self-skips without it.
  - { name: CARGOLESS_REMOTE_PROJECT_CHECKS, value: "1" }
```

Prerequisite: the `ssr-compiler-witness` check must be on the **served** ref
(tf-multiverse `origin/dev`, checked out at `/workspace/tf-multiverse`), not only
in agent worktrees — the daemon loads `cargoless.checks.yaml` from the served
repo root.

## Example: the SSR witness check (`cargoless.checks.yaml`, repo root)

A complete `kind: command` witness. tf-multiverse ships this; reproduced here as
the canonical reference (an example file also lives at
`docs/operator/examples/cargoless.checks.ssr-witness.yaml`).

```yaml
version: 1
checks:
  - id: "ssr-compiler-witness"
    title: "Rust compiler witness (SSR: server + portal)"
    tier: "dev"
    required: true            # a failing required check turns the tree RED
    kind: "command"
    read_only: true           # mandatory for kind: command in the dev profile
    # Host-native cargo check that EXPANDS macros + type-checks the SSR path.
    # --release is load-bearing: the dev profile resolves the dep graph
    # differently and false-REDs (BUGS-2428). triform-portal's default
    # feature is "ssr", so the portal SSR render path compiles with no extra
    # --features beyond the server's.
    command:
      - "cargo"
      - "check"
      - "--release"
      - "-p"
      - "triform-server"
      - "-p"
      - "triform-portal"
      - "--features"
      - "ssr-frontend,telephony,webrtc"
      - "--message-format=json"
    # Only run when one of these changed (the macro/compile surface). Keep in
    # sync with CARGOLESS_MACRO_BLIND_PATHS above.
    triggers:
      - "Cargo.toml"
      - "Cargo.lock"
      - "server/src/**/*.rs"
      - "portal/src/**/*.rs"
      - "chemistry/generated/**/*.rs"
      - "physics/src/**/*.rs"
      - "runtime-types/**/*.rs"
    inputs:                   # cache key: re-runs only when these change
      - "Cargo.toml"
      - "Cargo.lock"
      - "server/src/**/*.rs"
      - "portal/src/**/*.rs"
    timeout_ms: 1200000       # 20m per-check ceiling (watchdog caps below)
    cache: "inputs"
```

Notes:
- The daemon pins `CARGO_TARGET_DIR` to a per-run scratch (`.cargoless-target`)
  — host-native, correct for SSR, and isolated so concurrent witnesses can't
  corrupt each other's `incremental/`/`.fingerprint/` (CGLS-24).
- Production wraps the raw `cargo check` in
  `scripts/ci/check-ssr-compiler-witness.sh`, which converts cargo's JSON to the
  `cargoless.check-diagnostic/v1` line protocol. Either form works; the script
  gives richer diagnostics. A non-zero cargo exit with no parsed error becomes a
  synthetic error → RED.

## How it composes (never a false green)

Two layers, escalation strictly stronger than the downgrade:

| Push | `MACRO_BLIND_PATHS` only | `+ ESCALATE=1` (base = warn) |
|---|---|---|
| touches macro paths | RA-green **downgraded to `unknown(ra_blind_path_green_unwitnessed)`** | **promoted to the witness** → real GREEN (compiles) / RED (`compose_hard_mode_payload`) |
| witness times out / crashes | — | `unknown` (never green) |
| no macro paths | fast ~2s RA-native verdict | fast ~2s RA-native verdict |

So: a trustworthy GREEN on macro code only ever comes from a passing `cargo
check`; anything the witness can't confirm is `unknown`, never a false green.

## Cost & the swarm caveat (measure before scaling)

The witness runs **off the serve loop** (never blocks other worktrees' verdicts)
under a 25-min daemon watchdog (a 540s script `timeout` binds first; observed
SSR witness ~139s). But two cost properties matter for a ~50-agent swarm and are
**not** bounded by this code:

1. **No global concurrency cap on Hard witnesses** — N macro-touching pushes ⇒ N
   concurrent cold `cargo check --release` storms.
2. **No cross-run target reuse** — each witness gets a fresh scratch worktree
   with its own `CARGO_TARGET_DIR`, destroyed at cleanup. So each macro push pays
   a **full cold** compile (no incremental).

Per the validation-first decision, **measure these on the test instance under
realistic load before scaling**, then decide whether to bound them (shard fan-
out, or the in-flight `agent/per-lane-build-slot` / `agent/witness-target-dir`
refinements). Do not rely on the witness path to self-throttle a swarm.

## Validation before promoting to live

Use the differential tool (Phase 1) on the Gate-2 test instance:

```bash
# Test daemon has the arming env above; live daemon does not (yet).
scripts/tf-multiverse-canary diff-remotes \
  http://cargoless-serve.cargoless-builder.svc:8787 \
  http://cargoless-serve-test.cargoless-builder.svc:8787
```

A macro-touching worktree with a real post-`view!` type error must go RED (with
the actual error in `/worktrees/<wt>/diagnostics`) on the test daemon while the
live (un-armed) daemon shows green/unknown — that delta is the proof the witness
is catching real compile errors. Confirm no *net-new* reds on clean worktrees.
Then promote the env to the tf-multiverse Flux manifest.
