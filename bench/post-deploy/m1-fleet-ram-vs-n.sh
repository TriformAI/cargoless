#!/usr/bin/env bash
# M1 — Fleet RAM vs N (the headline-claim falsifier).
#
# METRIC     aggregate RSS of one `cargoless serve --repo` across the
#            REAL cluster (tf-multiverse, NOT the Leptos fixture) at
#            N = 1,2,4,8,16,20 and the operator's real active subset.
# METHOD     extend bench/modelr-fleet.sh discipline verbatim:
#              * per-ref CARGO_TARGET_DIR (no shared-/cache/target class)
#              * binary-mtime >= build-start provenance guard
#              * RSS = Σ VmRSS over the `serve` pid + recursive
#                `pgrep -P` descendant subtree — NEVER pgid (§11-v1
#                setsid lesson)
#              * RA-absence ⇒ explicit FAIL, never a fabricated number
#              * capture the full descendant PROCTREE concurrently with
#                each RSS sample; assert exactly one /bin/rust-analyzer
# SUCCESS    aggregate peak stays within ~1.5× of N=1 across the range.
# FALSIFIER  aggregate RSS grows >= linearly with N, OR `lsp=`>1 at any
#            cell while worktrees share Cargo.toml/Cargo.lock
#            (workspace-cluster cardinality blow-up, D-FLEET §14).
# NOT-A-FALSIFIER  a higher absolute GiB than the fixture's ~1 GiB on a
#            larger real workspace — only the flat-vs-N STRUCTURE is the
#            claim (AC7-THROUGHPUT-REPORT §11.4 caveat 1).
set -uo pipefail
echo "M1 fleet-RAM-vs-N — SCAFFOLD ONLY (not yet wired; needs the central deploy)."
echo "See header for metric/method/success/falsifier. Refuses to emit a number"
echo "until it can measure a real one."
echo "M1_VERDICT: NOT-IMPLEMENTED (scaffold; a stub must never report success)"
exit 2
