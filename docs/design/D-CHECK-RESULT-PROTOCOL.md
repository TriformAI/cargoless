# Structured project-check results

`cargoless.check-result/v1` is the result-file protocol for project checks
whose outcome cannot be represented honestly by process exit alone. It keeps
code failure, allowed dependency degradation, intentional skip, and missing or
untrustworthy evidence distinct while preserving Cargoless's existing
green/red tree seam.

## Manifest contract

Only `kind: command` checks may opt in:

```yaml
- id: reuse-intelligence
  title: Repository reuse intelligence
  kind: command
  tier: dev
  required: true
  read_only: true
  command: ["scripts/reuse-intelligence", "check"]
  result_protocol: cargoless.check-result/v1
  on_degraded:
    provider_unavailable: warn
```

`on_degraded` is fail-closed. Each reason must map to `warn`, `fail`, or
`indeterminate`; an absent reason maps to `indeterminate`. A `warn` mapping is
an explicit policy decision, not a successful semantic analysis.

Structured checks are not coalesced and are not stored in Cargoless's project-
check cache. The producer may maintain a model-aware cache of its own.

## Command environment

Cargoless resolves and pins an immutable comparison before starting the
command, then exports:

| Variable | Meaning |
| --- | --- |
| `CARGOLESS_CHECK_RESULT_PATH` | Unique file the command must create atomically |
| `CARGOLESS_SOURCE_SHA` | Exact candidate commit being evaluated |
| `CARGOLESS_BASE_SHA` | Exact comparison commit |

A structured command exits zero after writing any honest typed result,
including `failed`, `degraded`, `skipped`, or `indeterminate`. A non-zero exit
means the producer itself failed, so Cargoless classifies the check as
indeterminate. Missing, unreadable, malformed, inconsistent, or subject-
mismatched result files are also indeterminate.

## Result document

```json
{
  "schema": "cargoless.check-result/v1",
  "check_id": "reuse-intelligence",
  "status": "failed",
  "summary": "2 blocking duplicate clusters exceed policy",
  "subject": {
    "source_sha": "0123456789abcdef",
    "base_sha": "fedcba9876543210",
    "engine": "reuse-intelligence",
    "engine_version": "1.0.0",
    "policy_hash": "sha256:...",
    "provider": "openai-compatible",
    "model": "nomic-embed-text",
    "model_revision": "sha256:...",
    "dimensions": 768
  },
  "findings": [
    {
      "fingerprint": "sha256:...",
      "blocking": true,
      "severity": "error",
      "code": "reuse.exact.new_cluster",
      "path": "portal/src/example.rs",
      "line": 42,
      "col": 1,
      "end_line": 59,
      "message": "new exact duplicate exceeds the blocking threshold",
      "data": {"cluster_size": 3}
    }
  ],
  "degradation": null,
  "metrics": {"functions_indexed": 1234},
  "artifacts": [{"kind": "report", "path": ".reuse/report.json"}]
}
```

Required top-level fields are `schema`, `check_id`, `status`, `summary`,
`subject`, and `findings`. Required subject fields are `source_sha`,
`base_sha`, `engine`, `engine_version`, and `policy_hash`. The provider/model
fields are optional because purely deterministic checks have no model.

Each finding requires `fingerprint`, `code`, and `message`. `blocking`
defaults to false, `severity` to warning, and source coordinates to line 1,
column 1. A `passed` result may not contain a blocking finding; a `failed`
result must contain at least one. Those consistency checks prevent a summary
or color from overriding the machine-readable evidence.

## Status mapping

| Producer status | Rich outcome | Legacy tree | Meaning |
| --- | --- | --- | --- |
| `passed` | Passed | Green | Authoritative evidence passed |
| `failed` | Failed | Red | Authoritative blocking finding |
| `degraded` + `warn` policy | Degraded | Green | Explicit WARN, never semantic green |
| `degraded` + `fail` policy | Failed | Red | Policy treats dependency degradation as blocking |
| unapproved degradation | Indeterminate | Red | Evidence unavailable without an explicit policy |
| `skipped` | Skipped | Green | Producer intentionally found no applicable work |
| `indeterminate` | Indeterminate | Red | Producer cannot make an authoritative decision |

Rich consumers must inspect the outcome before interpreting the frozen legacy
tree. The standalone CLI exits 75 for indeterminate evidence, emits WARN for an
allowed degradation, and returns ordinary failure only for authoritative
failed checks. Daemon and batch consumers likewise preserve indeterminate as a
separate verdict rather than relabeling it as a code RED.
