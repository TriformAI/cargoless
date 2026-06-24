#!/usr/bin/env bash
#
# Reference local pre-commit hook for cargoless.
#
# Contract (see docs/operator/pre-commit-hook-contract.md):
#
#   The local cargoless verdict is ADVISORY. Only one shape is the
#   right place for a pre-commit to hard-block:
#
#     * a real RED with diagnostic evidence (red_diagnostics > 0) AND
#       per-crate attribution (`crates[]` non-empty).
#
#   RA-native catches syntax / unresolved-name / type errors in seconds;
#   that is the fast feedback pre-commit is for, so it stays a hard
#   block.  Every OTHER shape is degraded infrastructure or a
#   non-attributable claim — the downstream compile-witness (CI gate)
#   is the authoritative gate for those, by design. A local hook that
#   hard-blocks on those produces a `--no-verify` bypass spiral, which
#   is exactly the symptom this contract eliminates.
#
# Behaviour
#
#   --advisory reifies the policy at the exit-code seam:
#
#     green                                 → exit 0
#     red + red_diagnostics > 0 + crates[]  → exit 1 (hard-block, justified)
#     red + (0 diagnostics OR no crates)    → exit 0 + advisory stderr line
#     unknown (any verdict_failure_class)   → exit 0 + advisory stderr line
#     ladder-exhausted / await-timeout      → exit 0 + advisory stderr line
#     unauthorized everywhere               → exit 2 (setup error — fix it)
#
#   The JSON wire shape is unchanged. The hook prints the advisory
#   skip line cargoless already emits; no per-class shell logic
#   required. If you want per-class operator copy, branch on the
#   `verdict_failure_class` JSON key (DaemonDegraded / Unwitnessable /
#   NonAttributable / TimeBudget).
#
# Install
#
#   ln -s ../../examples/pre-commit-advisory.sh .git/hooks/pre-commit
#
# Configure via env (no positional args; this is a git hook):
#
#   CARGOLESS_REMOTE      pool ingress URL (REQUIRED; supports failover
#                         via space-separated list, tried in order)
#   CARGOLESS_ROUTING_KEY X-Cargoless-Routing-Key value (REQUIRED for
#                         shard-affine routing; usually the repo name)
#   CARGOLESS_REPO        repo path (default: PWD)
#   CARGOLESS_BASE        --base ref (default: origin/main)

set -euo pipefail

remote_list="${CARGOLESS_REMOTE:-}"
if [[ -z "$remote_list" ]]; then
    echo "[pre-commit] CARGOLESS_REMOTE not set; skipping cargoless verdict" >&2
    exit 0
fi

routing_key="${CARGOLESS_ROUTING_KEY:-}"
if [[ -z "$routing_key" ]]; then
    echo "[pre-commit] CARGOLESS_ROUTING_KEY not set; skipping cargoless verdict" >&2
    exit 0
fi

repo="${CARGOLESS_REPO:-$PWD}"
base="${CARGOLESS_BASE:-origin/main}"

remote_flags=()
for r in $remote_list; do
    remote_flags+=(--remote "$r")
done

# --advisory is load-bearing. Without it, every Unknown / NonAttributable
# verdict hard-blocks the commit and you reach for --no-verify.
exec cargoless verdict \
    --advisory \
    --output json \
    --header "X-Cargoless-Routing-Key: $routing_key" \
    "${remote_flags[@]}" \
    --base "$base" \
    -- "$repo"
