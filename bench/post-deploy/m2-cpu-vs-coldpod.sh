#!/usr/bin/env bash
# M2 — Per-edit CPU vs the ACTUAL cold-pod status quo.
#
# Closes the Lane-C honest gap: the shipped 2.05× is measured vs `trunk`
# (a rebundle-every-save bundler), NOT vs the real status quo of N cold
# `check-remote` cargo-pod runs. This makes that delta a MEASURED number
# instead of an implied one.
#
# METRIC     CPU-s per agent-edit-BATCH (cutime+cstime, the §8.5
#            accounting invariant; per-agent-batch unit per §8.6 — NOT
#            per-keystroke).
# METHOD     A/B on one identical recorded agent-edit-batch trace:
#              arm-A  N cold `check-remote` cargo-pod runs + per-WT cold RA
#              arm-B  one `cargoless serve --repo`
#            measure cutime+cstime on BOTH sides; warm-cache parity where
#            the model legitimately warms; report median + two-source if
#            a second methodology is available.
# SUCCESS    arm-B <= ~0.5× arm-A CPU-s/edit-batch.
# FALSIFIER  arm-B >= arm-A, OR CAS identical-input skip-rate ≈ 0 in the
#            fleet (the green-edge model is not actually skipping cold
#            rebuilds at fleet scale).
set -uo pipefail
echo "M2 cpu-vs-coldpod — SCAFFOLD ONLY (not yet wired)."
echo "Exists specifically to convert the trunk-only-proxy 2.05× into a"
echo "cold-pod-status-quo MEASURED comparison. No number until it is real."
echo "M2_VERDICT: NOT-IMPLEMENTED (scaffold; a stub must never report success)"
exit 2
