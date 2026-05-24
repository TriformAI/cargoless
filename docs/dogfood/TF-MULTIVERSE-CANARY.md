# tf-multiverse cargoless canary

This canary validates cargoless against a tf-multiverse-shaped agent
fleet without using the operator's live worktrees as test subjects. The
harness reads `/Users/iggy/Documents/GitHub/tf-multiverse`, then creates
a shared clone and detached canary worktrees under
`/private/tmp/cargoless-tf-multiverse-canary`.

## Recovered state, 2026-05-22

- Replacement pivot, 2026-05-22 evening: the intended tf-multiverse path is
  no longer "wrap Cargo inside Cargoless" for `check-remote` and
  `clippy-remote`. The default development path is now Cargoless continuous
  RA-native verdicts. `cargo check` / `cargo clippy` are reserved for
  explicit compile, build, test, PR, or deploy gates, or
  `CHECK_REMOTE_ENGINE=legacy` rollback.
- The canary harness treats the live tf-multiverse checkout as source
  input only. The explicit live integration edit is
  `scripts/check-remote`; the temp worktrees carry the build/test load.
- tf-multiverse still owns the current remote cargo path through
  `scripts/check-remote`, `scripts/clippy-remote`, and
  `scripts/ci/deploy-check-gate.sh`. `scripts/check-remote` now defaults
  to the cargoless engine (`CHECK_REMOTE_ENGINE=cargoless`) for both
  check and clippy entrypoints; `clippy-remote` remains a thin alias, but
  it does not run `cargo clippy` unless the legacy lane is explicitly
  selected.
- cargoless main has the local builder gate and a repo-scoped
  `serve --repo` path. The in-cluster `cargoless-serve` deployment is
  not yet the production replacement for tf-multiverse's builder pods.
- Current live smoke, 2026-05-22: the LaunchAgent daemon is running
  `CARGOLESS_VERDICT_MODE=ra`, `TF_RA_CHECK_DISABLED=1`, and
  `CARGOLESS_PUSH_ONLY=1`. Default `check-remote -p ui-elements` and
  `clippy-remote -p ui-elements` both returned green from RA-native
  Cargoless verdicts; serve logs showed no `cargo-check-started` or
  `cargo-clippy-started` for those runs.
- The next canary goal is to prove request correlation, diagnostics
  detail, and multi-worktree throughput against the 20-agent path before
  removing the legacy rollback lane.
- The next coverage expansion should use the generic project-check manifest
  in `docs/design/D-PROJECT-CHECKS.md`: fast generated-code, architecture,
  CSS, and contract checks become Cargoless project checks; slower full
  `build-all.sh` coverage stays in a gate profile until it has a bounded
  no-write check mode.

## Cargo-profile compatibility

This section documents retained compatibility for explicit compile-gate
and rollback lanes. It is not the default `check-remote` /
`clippy-remote` path.

The cargoless CLI accepts the cargo-shaped selectors used by the
tf-multiverse remote-check surface:

```bash
cargoless check \
  -p triform-server \
  --target wasm32-unknown-unknown \
  --features "ssr-frontend telephony" \
  --no-default-features \
  --release
```

The same flags are parsed for `check`, `watch`, `serve`, and `push`.
Package selectors normalize the tf-multiverse `check-remote` shorthands
(`alchemy` -> `triform-alchemy`, etc.) before reaching rust-analyzer or
the direct Cargo fallback. `push` also accepts `--auth-token`, with
`CARGOLESS_AUTH_TOKEN` as the env fallback, so the same harness can be
moved from loopback to a protected daemon.

`push --cargo-subcommand check|clippy` selects the authoritative Cargo
subcommand for a pushed overlay. Omitted subcommand remains wire-compatible
with older clients and defaults to `check`.

`push --await-verdict` blocks until the daemon publishes a fresh verdict
for that worktree. For no-diff package checks, `push` still sends the
workspace config files so the remote daemon can run the requested
per-invocation profile.

For legacy or compile-gate package-scoped profiles, cargoless can run two
verdict feeds:

- rust-analyzer overlay diagnostics for fast red feedback.
- an explicit `cargo check` or `cargo clippy` invocation with
  `--message-format=json` in the selected worktree to prove the final
  green.

`scripts/tf-multiverse-canary serve-local` defaults `CARGO_TARGET_DIR` to
`/private/tmp/cargoless-tf-multiverse-canary/target` so the 20 worktrees
share Cargo artifacts. Production rollout should make the same cache
choice explicitly per repo/cluster before default-on.

The canary also defaults `TF_RA_CHECK_DISABLED=1` for the push daemon.
That prevents rust-analyzer from launching a workspace-wide Cargo
flycheck; explicit direct Cargo profiles are reserved for compile-gate
or rollback checks, not default `check-remote`.

## Local canary flow

Build the current branch binary:

```bash
cargo build -p cargoless --locked
```

Inspect state without mutation:

```bash
scripts/tf-multiverse-canary preflight
```

Create the temp clone and 20 detached worktrees from `origin/dev`:

```bash
scripts/tf-multiverse-canary prepare
```

Terminal 1, start the loopback daemon:

```bash
scripts/tf-multiverse-canary serve-local
```

Terminal 2, confirm the remote read path:

```bash
scripts/tf-multiverse-canary status-remote
```

After synthetic edits or agent edits in the temp worktrees, push every
overlay:

```bash
scripts/tf-multiverse-canary push-all
```

Run the actual tf-multiverse wrapper across the whole canary fleet:

```bash
scripts/tf-multiverse-canary check-remote-all -p alchemy
```

Run the same wrapper fleet against clippy by pointing the canary at the
thin clippy wrapper:

```bash
CARGOLESS_TFM_CHECK_REMOTE_SCRIPT=/Users/iggy/Documents/GitHub/tf-multiverse/scripts/clippy-remote \
scripts/tf-multiverse-canary check-remote-all -p alchemy
```

Exercise the actual tf-multiverse wrapper in strict cargoless mode:

```bash
cd /private/tmp/cargoless-tf-multiverse-canary/worktrees/wt-01
CHECK_REMOTE_ENGINE=cargoless \
CARGOLESS_BIN=/Users/iggy/Documents/GitHub/cargoless/target/debug/cargoless \
CARGOLESS_REMOTE=http://127.0.0.1:8787 \
CARGOLESS_BASE_REF="$(cat /private/tmp/cargoless-tf-multiverse-canary/base-sha)" \
CARGOLESS_AWAIT_TIMEOUT_SECS=900 \
CHECK_REMOTE_INTENT_BANNER=0 \
/Users/iggy/Documents/GitHub/tf-multiverse/scripts/check-remote -p alchemy
```

Clean only the temp canary root:

```bash
scripts/tf-multiverse-canary clean
```

## Acceptance criteria

- `preflight` reports source checkout, existing worktree, builder, and
  binary state but performs no mutation.
- `prepare` creates 20 detached canary worktrees under the temp root.
- `serve-local` discovers the canary topology and exposes loopback
  status.
- `status-remote` reaches the daemon through the shipped remote path.
- `push-all` is green on clean worktrees with "no changes" and submits
  overlays after edits.
- `scripts/check-remote -p alchemy` succeeds in
  `CHECK_REMOTE_ENGINE=cargoless` mode from a canary worktree, without
  entering the kubectl builder path.
- `scripts/clippy-remote -p alchemy` succeeds in
  `CHECK_REMOTE_ENGINE=cargoless` mode from a canary worktree, without
  entering the kubectl builder path.
- `scripts/tf-multiverse-canary check-remote-all -p alchemy` succeeds
  across all 20 canary worktrees.
- The canary creates no new files or worktrees in the source
  tf-multiverse checkout beyond the intentional `scripts/check-remote`
  integration diff.

## Local canary result, 2026-05-22

- Prepared 20 detached worktrees from `origin/dev` at
  `76488ca059e49d5c2325fd91db252789b5252a2d`.
- Loopback remote push/status was exercised through
  `http://127.0.0.1:8787`.
- A synthetic broken edit in `wt-01` published a red verdict in about
  13 seconds.
- A clean harmless edit in `wt-01` published green after 146.651 seconds.
  The delay was Cargo compiling `librocksdb-sys`, not cargoless routing.
- The green path used the tf-multiverse shorthand `-p alchemy`, normalized
  it to `triform-alchemy`, and completed
  `cargo check -p triform-alchemy --message-format=json`.
- After disabling rust-analyzer Cargo flycheck for the push daemon, the
  same no-diff `-p alchemy` profile published green in 126.033 seconds.
  The only Cargo child was
  `cargo check -p triform-alchemy --message-format=json`; no
  rust-analyzer workspace Cargo check was spawned.
- The tf-multiverse wrapper path succeeded in strict
  `CHECK_REMOTE_ENGINE=cargoless` mode from `wt-01`; the warmed follow-up
  verdict arrived via the event stream with `published_at=1779460247`.
- The 20-worktree wrapper canary then ran
  `scripts/check-remote -p alchemy` through
  `scripts/tf-multiverse-canary check-remote-all -p alchemy` with
  `CARGOLESS_TFM_CANARY_JOBS=20`. All 20 invocations exited 0, all
  verdicts were green, and every client received the fresh verdict via
  the event stream. The daemon published the first green at
  `published_at=1779460786` and the twentieth at `1779460794`; warmed
  direct Cargo checks were about 0.3s each after the first 1.045s check.
- Repeated awaited pushes exposed one HTTP transport bug before rollout:
  accepted streams could inherit nonblocking mode from the listener, so a
  large overlay POST could surface `WouldBlock` as a false short body.
  The server now forces accepted streams back to blocking mode and sends
  SSE keepalives to drain closed event subscribers.
- After that fix, default `CHECK_REMOTE_ENGINE=auto` selected cargoless
  and succeeded for `scripts/check-remote -p alchemy`. The full
  20-worktree wrapper canary was rerun and all 20 worktrees were green;
  the final run published green verdicts from `published_at=1779461426`
  through `1779461434`.

## Local-host default result, 2026-05-22

- Installed the current branch binary to `/Users/iggy/.cargo/bin/cargoless`
  as `cargoless 0.2.0`.
- Started the tf-multiverse daemon through the user LaunchAgent
  `dev.triform.cargoless.tf-multiverse`, bound to
  `http://127.0.0.1:8787`, with state/CAS/target under
  `/Users/iggy/Library/Caches/cargoless/tf-multiverse`.
- The first production-cache `scripts/check-remote -p alchemy` run used
  default `CHECK_REMOTE_ENGINE=auto`, selected cargoless, and published
  green after the cold direct Cargo check completed in 220.060 seconds.
- Two repeat checks then published green through the same default path
  with warmed direct Cargo checks in 1.637 seconds and 0.619 seconds.
- The final verified run after adding tf-multiverse `.gitignore` coverage
  for `.cargoless/` published green at `published_at=1779463753`.
- Cargoless now carries the cargo subcommand in the pushed profile, so
  `scripts/check-remote` can route both `check` and `clippy` through the
  same remote push/await path. The local-host daemon and wrapper defaults
  use a 900 second wait/check timeout to cover cold clippy builds.
- After the cargo-subcommand change, default auto-mode
  `scripts/check-remote -p alchemy` published green at
  `published_at=1779467143`.
- Default auto-mode `scripts/clippy-remote -p alchemy` selected cargoless
  and published green at `published_at=1779467160`; the daemon log shows
  the direct Cargo run as
  `cargo clippy -p triform-alchemy --message-format=json` with
  `elapsed_ms=11049`.
- Strict mode also passed: `CHECK_REMOTE_ENGINE=cargoless
  scripts/check-remote -p alchemy` published green at
  `published_at=1779467171`, and `CHECK_REMOTE_ENGINE=cargoless
  scripts/clippy-remote -p alchemy` published green at
  `published_at=1779467179` with a warmed clippy direct run of
  `elapsed_ms=530`.
- The production-daemon 20-worktree canary passed for both wrapper
  entrypoints: `scripts/tf-multiverse-canary check-remote-all -p
  alchemy` completed `total=20 failed=0`, and the same harness pointed at
  `scripts/clippy-remote` also completed `total=20 failed=0`.
- A live `scripts/check-remote -p triform-portal` rerun after installing
  the subcommand-aware client completed green at `published_at=1779467764`;
  the daemon direct Cargo run was
  `cargo check -p triform-portal --message-format=json` with
  `elapsed_ms=366489`.
- `scripts/check-remote` now probes `cargoless push --help` for
  `--cargo-subcommand` before taking the cargoless path. In `auto` mode,
  a stale client falls back to the legacy builder with an explicit skip
  message instead of failing on an unknown flag; strict `cargoless` mode
  fails with a reinstall message.
- After that guard, an isolated canary worktree strict clippy smoke
  (`wt-01`, `scripts/clippy-remote -p alchemy`) published green at
  `published_at=1779468381`; the daemon log shows
  `cargo clippy -p triform-alchemy --message-format=json` with
  `elapsed_ms=872`.
- To move more check/clippy traffic off the legacy builder, the pushed
  profile now carries a bounded `extra_args` list for Cargo selectors
  beyond package/target/features/release. `scripts/check-remote` forwards
  `--manifest-path`, `--lib`, `--tests`, `--all-targets`, `--locked`,
  and related check/clippy selectors through cargoless instead of falling
  back. Strict smokes after reinstall/restart passed:
  `scripts/check-remote -- --manifest-path chemistry/generated/types/Cargo.toml --lib --locked`
  green at `published_at=1779472593` (`elapsed_ms=19930`), and
  `scripts/clippy-remote -- --manifest-path chemistry/generated/types/Cargo.toml --lib --locked`
  green at `published_at=1779472624` (`elapsed_ms=9496`).
- The wrapper now appends one JSONL engine-decision metric per invocation
  (`CHECK_REMOTE_STATS_LOG`, default
  `~/Library/Logs/triform/check-remote-engine.jsonl` on macOS) and ships a
  `scripts/check-remote-stats` summarizer in tf-multiverse. This captures
  total wrapper traffic, selected engine, fallback reason, result code, and
  elapsed time; daemon logs alone only count the calls that already reached
  cargoless.
- A strict smoke exposed a shell-probe bug in the stale-client guard: under
  `set -o pipefail`, `cargoless push --help | grep -q -- --cargo-subcommand`
  can report failure when `grep -q` closes the pipe early. The wrapper now
  captures help text before matching. After that fix, strict smokes passed
  again through the extra-args path: `check-remote -- --manifest-path
  chemistry/generated/types/Cargo.toml --lib --locked` green at
  `published_at=1779473219` (`elapsed_ms=83538`), and `clippy-remote --
  --manifest-path chemistry/generated/types/Cargo.toml --lib --locked`
  green at `published_at=1779473302` (`elapsed_ms=78557`).
- Default auto-mode was rechecked with the same extra-args profile and
  selected cargoless without env overrides, publishing green at
  `published_at=1779473487` (`elapsed_ms=53747`).
- The permanent wrapper ledger was seeded with two real default-auto rows:
  `-p ui-elements` selected cargoless and returned red in `elapsed_ms=2081`
  against the current dirty live tree, then the known-green generated-types
  manifest profile selected cargoless and published green at
  `published_at=1779473621` (`elapsed_ms=500`).
  `scripts/check-remote-stats --since 24h` reports both as
  `check/cargoless` with no fallback reason.
- Observed caveat: `push --await-verdict` freshness is currently keyed by
  worktree, not by a per-request id. Concurrent pushed checks for the same
  worktree can satisfy another client's wait with a later verdict. The
  supported 20-agent path is one worktree per agent; adding request-id
  correlation is the follow-up before encouraging multiple simultaneous
  checks from one worktree.
- Two production-only rollout issues were fixed before defaulting:
  `serve` now supports an explicit managed-service mode so launchers can
  detach without tripping the parent-orphan guard, and the tf-multiverse
  daemon runs in push-only mode so unrelated live agent worktree saves do
  not start profile-less watch transactions.

## Local-host promoted result, 2026-05-24

- The canary is promoted to the default development path. In
  tf-multiverse, `scripts/check-remote` defaults to
  `CHECK_REMOTE_ENGINE=cargoless`, and `scripts/clippy-remote` delegates
  to the same path with `CHECK_CARGO_SUBCOMMAND=clippy`.
- The default `check` and `clippy` entrypoints no longer run
  `cargo check` or `cargo clippy`. They push the current worktree overlay
  to the local Cargoless daemon and wait for a fresh continuous
  RA-native verdict plus the configured fast project-check profile.
  Cargo selectors such as `-p triform-portal` are accepted and recorded
  for workflow compatibility, but they do not select a Cargo package in
  the replacement path.
- The local daemon profile now keeps hot worktrees resident for 6 hours
  (`TF_WT_IDLE_SECS=21600`) and deactivates them after 24 hours total
  quiet time (`TF_WT_DEACTIVATE_SECS=64800`). This addresses the observed
  long tail where quiet periods forced a rust-analyzer/cargo-metadata
  cold start before the next verdict.
- Wrapper metrics now classify outcomes with `result_class` and
  `result_detail`, so timeout/setup/red verdicts are separated from
  ordinary red code failures and from historical unclassified return
  codes.
- Post-promotion smoke:
  `CARGOLESS_AWAIT_TIMEOUT_SECS=900 scripts/check-remote -p
  triform-portal` completed green in 27.62 seconds after daemon restart
  and cold warm-up.
- Hot-path smoke immediately after warm-up:
  `scripts/check-remote -p triform-portal` completed green in 3.90
  seconds, and `scripts/clippy-remote -p triform-portal` completed green
  in 3.88 seconds. Both were recorded as `selected_engine=cargoless`,
  `result_class=success`, with no fallback reason.
- The tf-multiverse wrapper now resolves the owner checkout when invoked
  from `.claude/worktrees/<agent>/scripts/check-remote`, so agent
  worktrees find the sibling `/cargoless/target/release/cargoless`
  binary instead of falling back to stale `~/.cargo/bin/cargoless`.
- Claude Code sessions now get the same promoted defaults from a
  SessionStart hook:
  `CHECK_REMOTE_ENGINE=cargoless`,
  `CARGOLESS_REMOTE=http://127.0.0.1:8787`,
  `CARGOLESS_AWAIT_TIMEOUT_SECS=900`, and the resolved `CARGOLESS_BIN`.
- `scripts/check-remote-stats --since 2m` after promotion reported 3
  events, 100% cargoless, 100% success, p50 3 seconds, max 27 seconds.
- The 24h ledger at promotion time contained 13 events: 13/13 selected
  cargoless, 0 fallback. The only recent classified failure was the
  intentional 90 second timeout that exposed the cold rust-analyzer
  restart behavior before the warm-retention profile was applied.

## Rollout plan

- Run the local-host production daemon through the checked-in control
  script. It pins the repo, loopback remote, persistent state/CAS
  directories, shared `CARGO_TARGET_DIR`, `TF_RA_CHECK_DISABLED=1`, and
  `CARGOLESS_MANAGED_SERVICE=1` so the pidfile-managed daemon survives
  the short-lived launcher process. On macOS it installs/runs the daemon
  as a user LaunchAgent for durability across shell exits. It also
  defaults `CARGOLESS_PUSH_ONLY=1`, because this replacement path should
  only publish verdicts requested by `cargoless push --await-verdict`;
  unrelated live agent worktree saves must not start profile-less watch
  transactions in the check-remote daemon:

```bash
scripts/tf-multiverse-daemon start
scripts/tf-multiverse-daemon status
```

- Export the default tf-multiverse check environment from the same
  script when running agents or manual checks:

```bash
eval "$(scripts/tf-multiverse-daemon env)"
```

- Watch the wrapper-side rollout ledger with:

```bash
/Users/iggy/Documents/GitHub/tf-multiverse/scripts/check-remote-stats --since 24h
```

- Keep `CHECK_REMOTE_ENGINE=cargoless` as the default. Do not route
  default development `check` or `clippy` traffic back through Cargo.
- Use `CHECK_REMOTE_ENGINE=legacy`, `cargo test`, explicit builder pins,
  or build/deploy scripts only for compile/test gates and emergency
  rollback.
- Continue watching `check-remote-stats --since 24h` until the ledger has
  enough normal agent traffic to evaluate p95/p99 behavior under the live
  20-agent pattern.
- If p95 grows again, first inspect whether the daemon has restarted or a
  worktree was deactivated; do not infer that Cargoless project checks are
  slow without checking the daemon log's `project-checks` line.

## Still operator-owned

- Decide where the `scripts/tf-multiverse-daemon env` exports should live
  permanently for the agent launcher/shell profile now that the local
  Cargoless daemon is the default path.
- Build and deploy the Kubernetes `cargoless-serve` image/manifest only
  after source/worktree mirroring is designed; the current local-host
  daemon is the default replacement path.
- Run the full 20-worktree check and clippy canaries through the
  production daemon endpoint after the promoted local-host profile has
  accumulated enough live traffic.
- Add per-request verdict correlation if the rollout needs multiple
  simultaneous `check-remote`/`clippy-remote` invocations from the same
  worktree. The 20-agent replacement path should keep one agent per
  worktree.
- Rotate/remove any legacy credentials found in older tf-multiverse CI
  helper scripts.
- Reconcile stale Plane project items with the recovered branch state.
