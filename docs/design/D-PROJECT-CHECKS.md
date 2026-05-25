# Project Checks

Cargoless should be able to include project-specific correctness checks in
the same continuous verdict path as rust-analyzer diagnostics without
hardcoding any one repository's rules. The product contract is: projects
declare fast, parallel, non-mutating checks in a standard manifest; Cargoless
runs the applicable checks for a worktree overlay, turns their output into
diagnostics, and combines those diagnostics with the RA-native verdict.

This is the path for checks such as generated-code freshness, schema/contract
drift, CSS policy, routing policy, or domain architecture rules. In
tf-multiverse terms, "YAML -> generators -> implementations", "generated code
is not hand-edited", "element-agnostic physics/portal", and frontend structure
rules should all become project checks. In product terms, they are just
declared checks with triggers, budgets, cache keys, and diagnostics.

## Goals

- Keep `check-remote` / `clippy-remote` as Cargoless replacement paths, not
  Cargo wrappers.
- Add non-Rust correctness to the continuous dev verdict when it is fast and
  parallel enough.
- Let projects add checks without changing Cargoless source code.
- Make checks observable: every red has a check id, path, message, duration,
  and whether it was required or advisory.
- Avoid the long tail: dev checks have explicit budgets and changed-file
  triggers; slower checks stay in `gate` profiles.

## Non-Goals

- Cargoless does not learn tf-multiverse concepts such as elements,
  chemistry, portal, or physics.
- The default dev loop does not run arbitrary build-all pipelines unless a
  project exposes a fast verification mode.
- Check commands in the continuous path do not mutate the worktree. A fix mode
  can exist separately, but verdict checks are read-only.

## Manifest

The implemented product manifest is `cargoless.checks.yaml` at the repo root.
Cargoless parses a small owned YAML subset: maps, lists, quoted/unquoted scalar
strings, integers, booleans, and `#` comments. This keeps the fast path
dependency-light while still matching the way tf-multiverse already describes
domain structure.

```yaml
version: 1

profiles:
  dev:
    include: ["generated-fast", "contracts-fast", "style-fast"]
    timeout_ms: 12000
    max_parallel: 6
    on_timeout: red
  gate:
    include: ["*"]
    timeout_ms: 900000
    max_parallel: 8
    on_timeout: red

checks:
  - id: generated-fast
    title: generated outputs match sources
    tier: dev
    kind: command
    required: true
    read_only: true
    command: ["./scripts/check-generated", "--fast", "--format=cargoless-jsonl"]
    triggers: ["schema/**/*.yaml", "generators/**", "src/generated/**"]
    inputs: ["schema/**/*.yaml", "generators/**", "src/generated/**"]
    timeout_ms: 8000
    cache: inputs

  - id: element-agnostic-portal
    title: portal stays element agnostic
    tier: dev
    kind: forbidden_patterns
    inputs: ["portal/**/*.rs"]
    patterns:
      - code: portal.element_specific
        regex: "\\b(auth|commerce|finance)\\b"
        message: Portal code must not hardcode element names.

  - id: yaml-contracts
    title: YAML definitions expose required metadata
    tier: dev
    kind: yaml_rules
    inputs: ["chemistry/**/*.yaml"]
    rules:
      - code: chemistry.meta_intention
        require_path: $.meta.intention
        message: YAML definitions must declare meta.intention.

  - id: deep-codegen
    title: full generated-code verification
    tier: gate
    kind: command
    required: true
    read_only: true
    command: ["./scripts/build-all.sh", "--check"]
    triggers: ["schema/**", "generators/**", "src/generated/**"]
    timeout_ms: 300000
    cache: inputs
```

The manifest is intentionally generic. Cargoless supplies scheduling,
freshness, caching, timeouts, verdict composition, and diagnostics. Projects
own domain knowledge either through built-in declarative checks or through
read-only command checks.

## Check Fields

- `id`: stable identifier. Used in logs, diagnostics, cache keys, and status.
- `title`: human-readable label.
- `tier`: `dev`, `background`, or `gate`.
- `kind`: implemented built-ins are `mirror_drift`, `forbidden_patterns`,
  `required_patterns`, `yaml_rules`, `json_rules`, `file_exists`, and
  `command`.
- `required`: `true` makes failure contribute red to profiles that include it.
  `false` emits advisory diagnostics only.
- `command`: argv array, run from the worktree root for `kind: command`.
- `read_only`: required for `kind: command` in the `dev` profile.
- `triggers`: globs that decide whether a changed overlay should run the
  check. If no trigger matches and a fresh cache result exists, Cargoless reuses
  the cached result.
- `inputs`: files that define cache freshness. If omitted, `triggers` are used.
- `timeout_ms`: per-check hard budget.
- `cache`: `none` disables caching; other values use the current input-hash
  cache. The cache key includes engine version, manifest hash, profile, check
  id, check configuration, and input file hashes.
- `source_root` / `mirrors`: used by `mirror_drift`.
- `patterns`: used by `forbidden_patterns` and `required_patterns`.
- `rules`: used by `yaml_rules` and `json_rules`.
- `paths`: used by `file_exists`.

## Output Protocol

Exit code is enough for simple checks:

- `0`: green.
- `1`: red, expected policy/check failure.
- `2`: setup error, reported as infrastructure red unless the profile marks
  setup errors advisory.
- `124`: timeout, or Cargoless-enforced timeout, reported as timeout red.

For rich diagnostics, checks emit JSON Lines to stdout or stderr:

```json
{"schema":"cargoless.check-diagnostic/v1","check":"generated-fast","severity":"error","path":"src/generated/types.rs","line":1,"code":"generated.drift","message":"generated output is stale","suggestion":"run ./scripts/devctl codegen"}
```

Fields:

- `schema`: `cargoless.check-diagnostic/v1`.
- `check`: check id from the manifest.
- `severity`: `error`, `warning`, or `info`.
- `path`: repo-relative or absolute path.
- `line` / `column`: optional 1-based location.
- `code`: stable machine-readable issue code.
- `message`: concise human message.
- `suggestion`: optional remediation.

If a command emits no JSON diagnostics and exits nonzero, Cargoless wraps the
tail of stdout/stderr as one diagnostic attached to the check id.

## Verdict Composition

For a profile, the final Cargoless development verdict is:

```text
RA-native diagnostics
+ required project checks included by the active profile
--------------------------------------------------------
green only when all required sources are green
```

Advisory checks are visible in status but do not flip the exit code. Gate-only
checks do not run in the default dev profile.

Every verdict should expose:

- RA verdict and diagnostic count.
- Project-check summary: green/red/advisory/skipped/timed-out.
- Slowest checks.
- Cache hit/miss counts.
- Active profile.

Current implementation exposes rich project-check diagnostics in
`cargoless check` and `cargoless checks run`. The repo-scoped daemon folds the
same required project-check result into the published green/red verdict and
logs project-check summaries; extending the remote status payload with the full
diagnostic list is the next observability increment.

## Scheduler Semantics

- Checks run in parallel up to `profiles.<name>.max_parallel`.
- In the current implementation, one-shot `cargoless check` runs project checks
  after RA diagnostics settle, and the repo-scoped daemon runs them at the
  verdict attribution boundary before publishing. The cache makes the no-change
  path cheap; running project checks concurrently with RA is the next latency
  improvement.
- The current implementation selects checks by profile and reuses a cached
  result when the manifest, check definition, profile, and input fingerprints
  match. `triggers` are presently used as the input set when `inputs` is
  omitted; changed-overlay trigger pruning is a follow-up optimization.
- A cached green can be reused only when the cache key includes every file in
  `inputs` and the `command` argv.
- Required checks with no cache entry run, so a new worktree never gets a
  fabricated green.
- Timeouts are bounded by the smaller of the check timeout and the profile
  remaining budget.

## Team Resource Discipline

Cargoless runs in shared team environments. A local check invocation can affect
other agents by consuming daemon CPU, project-check slots, cluster bandwidth, or
the shared Kubernetes builder cache. Operators and agents must treat every gate
run as shared infrastructure work, not as a private local command.

Current behavior to remember: profile selection is config-driven. The branch
protection project-check profile may run every included check even for a change
that only edits YAML or documentation, because changed-overlay trigger pruning
is not implemented yet. A pure-YAML branch-protection validation observed 54
project checks. That was fast enough, but it is still real shared work.

Until trigger pruning lands:

- Do not repeatedly run broad profiles just to "see what happens".
- Prefer the narrow command that answers the question: one named check, one
  profile, or one normal merge-path invocation.
- If a change is outside Rust/workspace-config surfaces, expect the current
  profile to still run all included checks unless the command explicitly limits
  it.
- When adding checks to shared profiles, include tight `inputs`, `triggers`,
  `timeout_ms`, and a conservative `max_parallel` expectation. A check that is
  cheap alone can become expensive across a 20-agent fleet.
- Treat path-trigger short-circuiting as a high-priority product improvement,
  but do not rely on it until the implementation proves it in live gate output.

## Safety

Project checks execute repository code. That is acceptable for local trusted
worktrees, but it must be explicit for product safety:

- Local daemon default: checks are enabled when a manifest is present.
- Non-loopback daemon default: project checks are disabled unless the operator
  starts the daemon with an explicit trust flag or config value.
- Continuous checks must be non-mutating. Cargoless should snapshot modified
  mtimes or use a future scratch checkout to detect and reject mutating checks.
- Commands receive `CARGOLESS=1`, `CARGOLESS_CHECK_ID`,
  `CARGOLESS_PROFILE`, `CARGOLESS_WORKTREE`, and
  `CARGOLESS_CHANGED_FILES`. The current implementation inherits the operator
  environment; a minimal-env daemon mode is a follow-up hardening step.

## tf-multiverse Mapping

tf-multiverse should consume this as data, not as Cargoless code:

- `generated-fast`: a fast no-write generated-code drift check for YAML and
  generated outputs.
- `element-agnostic-physics`: policy check over physics source and generated
  dispatch APIs.
- `element-agnostic-portal`: policy check over portal source and generated
  APIs.
- `css-contracts`: CSS/structure rule checks that are already scriptable.
- `contracts-fast`: existing contract/schema drift scripts that complete under
  the dev budget.
- Full `build-all.sh`: `gate` profile until it has a fast `--check` mode.

The immediate product requirement is not to port every build-all check. It is
to split checks into fast dev checks versus gate checks using the same manifest
surface, so the default path can grow coverage without recreating Cargo's long
tail.

## Implementation Plan

1. Done: manifest discovery and a small hand-rolled YAML subset parser for
   `cargoless.checks.yaml`.
2. Done: `ProjectCheckReport`, `ProjectCheckResult`, summaries, and
   explanations in `cargoless-core`.
3. Done: built-in Rust checks plus read-only command checks with timeout,
   stdout/stderr capture, and JSONL diagnostic parsing.
4. Done: per-worktree check cache keyed by engine version, manifest hash,
   profile, check id, check configuration, and input file hashes.
5. Done: one-shot `cargoless check` and repo-scoped daemon verdict publishing
   fold required project checks into the green/red verdict.
6. Done: `cargoless checks list`, `cargoless checks run [id] --profile <name>`,
   and `cargoless checks explain <id>`.
7. Next: run project checks concurrently with RA settling in the daemon rather
   than at publish time.
8. Next: extend remote status with project-check summaries and diagnostics.
9. Next: convert tf-multiverse's first fast scripts into a manifest and
   measure the 20-agent path before promoting any gate-only check into `dev`.
