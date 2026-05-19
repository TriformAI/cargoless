#!/usr/bin/env bash
# M3 — Overlay-push round-trip latency, split transport-vs-RA.
#
# METRIC     wall-clock client-push-timestamp → SSE-frame-received,
#            DECOMPOSED into (transport RTT) vs (RA + cargo-check
#            re-derive) via daemon-side spans. p50/p95/p99.
# METHOD     instrument the overlay-push path: client emits a push with a
#            monotonic stamp; daemon records receive / overlay-diff /
#            didChange / publishDiagnostics / SSE-emit spans; client
#            records frame-receive. Run N=1 and N=20 concurrent, BOTH
#            local-split (unix sock) and network-split (HTTP+SSE).
# SUCCESS    p95 transport-component ≤ 100 ms (network) / ≤ 5 ms (local);
#            RA-component p95 at N=20 not worse than the single-WT
#            `watch` baseline.
# FALSIFIER  RA-component p95 degrades > 2× from N=1 → N=20 — the
#            PRIMARY RISK (one RA is a serialization point across N
#            multiplexed worktrees) realized, NOT a transport problem.
set -uo pipefail
echo "M3 overlay-latency-split — SCAFFOLD ONLY (not yet wired)."
echo "The split (transport vs RA-rederive) is the deliverable: it isolates"
echo "whether a latency regression is the network or RA serialization."
echo "M3_VERDICT: NOT-IMPLEMENTED (scaffold; a stub must never report success)"
exit 2
