#!/usr/bin/env bash
# cargoless S1 / AC#2 latency harness driver.
#
# Plane: CWDL-22 (S1 spike) / CWDL-3 (AC#2) / decision D-A2.
#
# Builds and runs the std-only LSP harness (bench/harness) against the
# committed Leptos reference fixture (bench/fixture), driving a real
# rust-analyzer, and prints:
#   * median save->publishDiagnostics latency per scenario
#   * PASS/FAIL vs AC#2's "<1s" budget
#   * a D-A2 GO/NO-GO recommendation
#
# DESIGN: this script ALWAYS exits 0. The S1 spike is evidence-gathering,
# not a CI gate — an honest "rust-analyzer cannot do sub-1s on Leptos
# macros" finding must NOT turn `main` permanently red (that would punish
# the truth). The verdict text in the job output is the deliverable. The
# CI `bench` job is informational by contract (.forgejo/workflows/ci.yml).
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fixture="$here/fixture"
harness="$here/harness"

hr() { printf '%s\n' "------------------------------------------------------------"; }

# Honest blocker: print why the spike could not produce a verdict, then
# exit 0 (see DESIGN above).
blocker() {
  hr
  echo "BLOCKER: $1"
  echo
  echo "D-A2 GO/NO-GO: BLOCKED — AC#2's sub-1s wording remains UNPROVEN."
  echo "This is the S1 gate (Plane CWDL-22); resolve before Sprint 2."
  echo "(run.sh exits 0 by design — evidence, not a CI gate.)"
  hr
  exit 0
}

echo "=== cargoless S1 / AC#2 latency harness (bench/run.sh) ==="
echo "fixture: $fixture"
echo "harness: $harness"
echo

# ---------------------------------------------------------------------
# Honest-size guard. A trivially tiny fixture would flatter rust-analyzer
# latency and make the AC#2 / D-A2 verdict a lie. If someone shrinks the
# reference project below a realistic floor, refuse to report a number.
# ---------------------------------------------------------------------
rs_files=$(find "$fixture/src" -name '*.rs' 2>/dev/null | wc -l | tr -d ' ')
rs_loc=$(find "$fixture/src" -name '*.rs' -exec cat {} + 2>/dev/null | wc -l | tr -d ' ')
MIN_FILES=12
MIN_LOC=800
echo "honest-size guard: ${rs_files} rust files, ${rs_loc} LOC (floor: ${MIN_FILES} files / ${MIN_LOC} LOC)"
if [ "${rs_files:-0}" -lt "$MIN_FILES" ] || [ "${rs_loc:-0}" -lt "$MIN_LOC" ]; then
  hr
  echo "HONEST-SIZE GUARD FAILED"
  echo
  echo "The reference fixture is below the realistic floor. Any latency"
  echo "number measured against it would flatter rust-analyzer and make"
  echo "the AC#2 / D-A2 verdict dishonest. Refusing to report a median."
  echo "Restore bench/fixture to a realistically-sized Leptos project."
  blocker "fixture below honest-size floor (${rs_files}f/${rs_loc}L < ${MIN_FILES}f/${MIN_LOC}L)"
fi
echo

# ---------------------------------------------------------------------
# Locate rust-analyzer. Prefer $RA_BIN, else the rustup component (this is
# how CI's rust:1.85-bookworm image gets it — same path as rustfmt/clippy
# in the other jobs), else whatever is on PATH.
# ---------------------------------------------------------------------
RA_BIN="${RA_BIN:-}"
if [ -z "$RA_BIN" ]; then
  if command -v rustup >/dev/null 2>&1; then
    echo "installing rust-analyzer rustup component..."
    rustup component add rust-analyzer >/dev/null 2>&1 || true
    if rustup which rust-analyzer >/dev/null 2>&1; then
      RA_BIN="$(rustup which rust-analyzer)"
    fi
  fi
fi
if [ -z "$RA_BIN" ] && command -v rust-analyzer >/dev/null 2>&1; then
  RA_BIN="$(command -v rust-analyzer)"
fi
if [ -z "$RA_BIN" ] || ! "$RA_BIN" --version >/dev/null 2>&1; then
  blocker "rust-analyzer not available. It is required for the S1 spike \
and must be installable in CI (rustup component add rust-analyzer)."
fi
echo "rust-analyzer: $RA_BIN ($("$RA_BIN" --version 2>/dev/null | head -1))"
echo

# ---------------------------------------------------------------------
# Warm the fixture's dependency graph up front. This both (a) makes the
# measured "warm daemon" latency honest (deps already fetched/built, the
# AC#2 precondition) and (b) surfaces a no-crates.io-egress CI as an
# explicit BLOCKER instead of a misleading slow number.
# ---------------------------------------------------------------------
echo "fetching fixture dependencies (leptos)..."
if ! ( cd "$fixture" && cargo fetch ) >/dev/null 2>&1; then
  blocker "cargo fetch failed in the fixture — most likely no crates.io \
egress in this CI runner. RA fidelity on Leptos cannot be measured \
without the real leptos proc-macro crate."
fi
echo "building fixture (warm cargo metadata + proc-macro artifacts)..."
( cd "$fixture" && cargo build ) >/dev/null 2>&1 \
  || echo "WARN: fixture cargo build did not fully succeed; RA may still \
analyze. Continuing — the harness measures RA, not cargo."
echo

# ---------------------------------------------------------------------
# Build the std-only harness.
# ---------------------------------------------------------------------
echo "building harness..."
if ! ( cd "$harness" && cargo build --release ) >/dev/null 2>&1; then
  blocker "harness failed to build."
fi
bin="$harness/target/release/ra-latency"
[ -x "$bin" ] || blocker "harness binary missing at $bin"
echo

# ---------------------------------------------------------------------
# Run it. The harness prints the full report + verdict and exits 0.
# ---------------------------------------------------------------------
RA_BIN="$RA_BIN" FIXTURE_DIR="$fixture" "$bin"
exit 0
