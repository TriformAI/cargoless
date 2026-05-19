#!/usr/bin/env bash
# M5 — Restart-churn RA-reap hygiene (FF-A #198 regression watch).
#
# FF-A: the serve-loop once did not reap its rust-analyzer child on
# clean SIGTERM. FIXED @ #200, fleet-corroborated POSITIVE @ #206. It is
# zombie/PID-hygiene under restart-churn (RA reparents to init, 0 RSS,
# descendant-scoped) — NOT a RAM leak; the retracted "~10 GiB" inference
# must never reappear. This metric watches that the fix STAYS fixed at
# the new deploy seam (restart/respawn/reconnect — the exact seam class
# the proven-core-precondition memory warns desyncs silently).
#
# METRIC     count of `stat=Z` rust-analyzer + total PID growth across
#            repeated `serve` start→SIGTERM→exit cycles under deploy
#            restart-churn.
# METHOD     drive K restart cycles; after each exit sample the process
#            table for zombie RA and reparented-to-init RA; track PID
#            high-water across cycles.
# SUCCESS    zombie RA count returns to 0 after EVERY `serve` exit;
#            no monotonic PID growth across cycles.
# FALSIFIER  monotonic zombie / PID growth across restart cycles (the
#            #3b/#44/#61/#128 ReapOnDrop discipline regressing at the
#            deploy seam).
# SCOPE      process-hygiene ONLY — a regression here does NOT impugn the
#            steady-state fleet-RAM thesis (AC7 §11.4 caveat 4).
set -uo pipefail
echo "M5 restart-reap-hygiene — SCAFFOLD ONLY (not yet wired)."
echo "Watches the FF-A fix at the deploy restart/respawn seam (the"
echo "precondition-desync-at-the-seam class). Zombies are 0-RSS; this is"
echo "PID hygiene, never a RAM-leak claim."
echo "M5_VERDICT: NOT-IMPLEMENTED (scaffold; a stub must never report success)"
exit 2
