# Pre-commit hook contract

The local cargoless verdict is **advisory**. The authoritative gate is the
downstream compile-witness (your CI gate). A local pre-commit hook that
hard-blocks on every non-green verdict produces a `--no-verify` bypass
spiral — exactly the symptom this contract eliminates.

`cargoless verdict --advisory` reifies the contract at the exit-code seam.

## Exit-code mapping

| Verdict shape | Plain `cargoless verdict` | `--advisory` |
|---|---|---|
| `green` | 0 | 0 |
| `red` with `red_diagnostics > 0` AND non-empty `crates[]` | 1 | **1** (justified hard-block) |
| `red` with `red_diagnostics == 0` | 1 | **0** + `[cargoless:advisory]` stderr line |
| `red` with empty `crates[]` (no per-crate attribution) | 1 | **0** + `[cargoless:advisory]` stderr line |
| `unknown` (any `verdict_failure_class`) | 75 | **0** + `[cargoless:advisory]` stderr line |
| Ladder exhausted / await timeout | 75 | **0** + `[cargoless:advisory]` stderr line |
| Unauthorized everywhere (config error) | 2 | **2** (still a setup error) |

The JSON wire shape on stdout is unchanged. `--advisory` only changes the
exit-code mapping; programmatic consumers see the same
`verdict` / `verdict_failure_class` / `red_diagnostics` / `crates[]` keys.

## Why the protective case is `red + diagnostics + crates`

RA-native catches syntax errors, unresolved-name errors, and type errors in
seconds. That is the fast feedback pre-commit is *for*; hard-blocking that
shape is the right call. Every other shape is either:

- **infrastructure trouble** (the daemon couldn't evaluate this push), or
- **non-attributable** (the RED can't be pinned on this submitter — it could
  be another agent's push on a shared shard, an interaction-red, or a
  `red_claimed_without_evidence` honesty case).

Neither shape is the local hook's call. The compile-witness has the full
build, the full base, the full diagnostics — it decides.

## The `verdict_failure_class` axis

When the verdict is `unknown` (or a degraded `red`) the JSON carries an
additive `verdict_failure_class` key, one of:

- **`DaemonDegraded`** — infra couldn't run the gate (setup, overlay-apply,
  spawn, worker died, batch-missing-member, batch-indeterminate).
- **`Unwitnessable`** — cargoless ran fine but the code isn't witnessable
  (RA-blind path, vacuous witness, RA-native unattributed, timer-settled
  with no flycheck activity).
- **`NonAttributable`** — a real result exists but cannot be pinned on this
  submitter (interaction-red, red-claimed-without-evidence,
  red-without-diagnostics).
- **`TimeBudget`** — witness ran out of wall-clock.

The advisory stderr line includes the class so an operator can grep for the
degraded path without digging through reasons. The fine-grained
`verdict_failure_reason` (the original free-text string) is still on the
wire — preserved for SigNoz dashboards that key off it.

## Reference hook

See [`examples/pre-commit-advisory.sh`](../../examples/pre-commit-advisory.sh).
It is three substantive lines (build the `--remote` flags, exec
`cargoless verdict --advisory --output json ... -- $repo`). All the policy
lives in cargoless, not in the shell.

## What this contract does NOT do

- It does not weaken your CI gate. The compile-witness still hard-blocks
  on RED.
- It does not silence advisories. Every degraded path emits one structured
  stderr line; operators see them.
- It does not change the JSON wire shape. Consumers that already parse the
  verdict object see no diff.
- It does not change the exit-code mapping for plain `cargoless verdict`
  (no `--advisory`). Pollers and CI gates already keyed off the legacy
  `0` / `1` / `75` ladder are byte-identical to today.
