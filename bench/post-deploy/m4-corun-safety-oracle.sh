#!/usr/bin/env bash
# M4 — corun §7.3 combined-green-hides-solo-red safety + effectiveness.
#
# corun's N-fold throughput is real ONLY if combined-green never hides a
# solo-red under cross-worktree deps (D-FLEET-SHARED-DAEMON §7.3). This
# is a CORRECTNESS oracle first, a throughput meter second.
#
# METRIC     (a) solo-fallback rate + solo-fallback latency in real
#                dogfood; (b) any combined-green worktree that is
#                independently solo-RED.
# METHOD     for every corun batch that emits combined-GREEN, run an
#            INDEPENDENT per-WT solo `cargo check` oracle; flag any WT
#            that is solo-red while it was reported combined-green AND
#            was acted on (merged / unblocked).
# SUCCESS    ZERO combined-green-hides-solo-red escapes AND throughput
#            ≥ ~0.7×N.
# FALSIFIER  any acted-on combined-green WT that is solo-red (gates the
#            `--no-corun` default), OR fallback so frequent throughput
#            collapses to ≈ 1× (corun adds no value at this fleet's
#            cross-dep density).
set -uo pipefail
echo "M4 corun-safety-oracle — SCAFFOLD ONLY (not yet wired)."
echo "Correctness (no hidden solo-red) dominates throughput here; the"
echo "independent solo oracle is the load-bearing half, not the rate."
echo "M4_VERDICT: NOT-IMPLEMENTED (scaffold; a stub must never report success)"
exit 2
