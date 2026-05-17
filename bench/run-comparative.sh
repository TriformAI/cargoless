#!/usr/bin/env bash
# cargoless two-mode comparative bench driver (AC#7 / AC#3 input).
#
# Plane: CWDL Epic AC#7 (cargoless vs trunk/bacon on ≥2 dims) + AC#3
# (artifact publish latency, reported SEPARATELY from AC#2 per D-A2).
# Companion to the existing single-tool `bench/run.sh` (S1/AC#2,
# rust-analyzer latency only) — that script is preserved unchanged so the
# `s1-ac2-verdict` commit-status pipeline keeps working.
#
# DESIGN: always exits 0 in the same spirit as bench/run.sh — comparative
# evidence is the deliverable, not a CI gate. The gate that consumes this
# output is team task #36 (Phase 1 GATE), downstream of this script.
#
# WHAT THIS DOES
#   1. Re-assert the bench/fixture honest-size guard (MIN_FILES/MIN_LOC).
#      A fixture shrunk below the floor would flatter cargoless's numbers
#      and make AC#7 a lie — the script REFUSES to report instead of
#      silently passing.
#   2. Ensure `tftrunk` (cargoless binary) is built and available.
#      In the dedicated ci-gate builder pod, cargo is operator-approved
#      (TRIFORM_OPERATOR_APPROVED_BUILD=1). Locally, cargo is blocked by
#      the cargo-safety hook — this script is intended to run in the pod.
#   3. Detect comparative tools (`trunk`, `bacon`). MISSING tools are
#      REPORTED as UNAVAILABLE — the comparative still runs, the verdict
#      line names the gaps honestly. Auto-install is NOT attempted here:
#      a cargo install in the bench path would muddy the very latency the
#      bench measures and bloat the warm cache unpredictably.
#   4. Build + run `cargoless-bench all` on the fixture. Output streams
#      to stdout (the ci-gate pod's kubectl-exec stdout IS readable —
#      that's the observability route the project's Forgejo CI lacks).
#   5. Exits 0 unconditionally (evidence, not gate). The verdict lines
#      (`AC2_VERDICT:` / `AC3_VERDICT:` / `AC7_VERDICT:`) are the
#      deliverable; task #36 publishes them as Forgejo commit statuses.

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"
fixture="$here/fixture"
harness="$here/harness"

hr() { printf '%s\n' "------------------------------------------------------------"; }

blocker() {
  hr
  echo "BLOCKER: $1"
  echo
  echo "AC#7 comparative: BLOCKED — no comparative measurement produced."
  echo "AC#3 publish latency: BLOCKED — see above."
  echo "(run-comparative.sh exits 0 by design — evidence, not a gate.)"
  hr
  # Emit verdict-shaped lines anyway so downstream tooling never sees
  # a silently empty section.
  echo "AC2_VERDICT: BLOCKED — comparative harness could not run; see BLOCKER above"
  echo "AC3_VERDICT: BLOCKED — comparative harness could not run; see BLOCKER above"
  echo "AC7_VERDICT: BLOCKED — comparative harness could not run; see BLOCKER above"
  exit 0
}

echo "=== cargoless two-mode COMPARATIVE bench (run-comparative.sh) ==="
echo "fixture: $fixture"
echo "harness: $harness"
echo "repo:    $repo"
echo

# ---------------------------------------------------------------------
# 1. Honest-size guard — REASSERTED from bench/run.sh on purpose.
#    bench/fixture is 17 files / ~1009 LOC today; the floor is set well
#    below that so a real Leptos project edit doesn't trip it, but high
#    enough that a "tiny example" shrink would. NEVER LOWER THE FLOOR
#    to flatter the numbers — that is the explicit warning in the brief.
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
  echo "bench/fixture is below the realistic floor. A latency number"
  echo "measured against a tiny fixture would flatter cargoless against"
  echo "trunk/bacon — the AC#7 claim would be a lie. Refusing to report."
  echo "Restore bench/fixture to its realistically-sized Leptos shape."
  blocker "fixture below honest-size floor (${rs_files}f/${rs_loc}L < ${MIN_FILES}f/${MIN_LOC}L)"
fi
echo

# ---------------------------------------------------------------------
# 2. Build / locate the cargoless binary (`tftrunk`).
#    In the ci-gate pod, cargo is operator-approved
#    (TRIFORM_OPERATOR_APPROVED_BUILD=1). Locally it is hook-blocked.
#
#    HONORS $CARGO_TARGET_DIR. When the caller isolates the per-branch
#    target dir (e.g. `CARGO_TARGET_DIR=/cache/target-bench-lead`) to
#    avoid stomping a shared `/cache/target` that other agents use,
#    the binary lands at `$CARGO_TARGET_DIR/release/tftrunk`, NOT
#    `$repo/target/release/tftrunk`. The earlier hardcoded path was a
#    real bug — fixed here so per-branch isolation is the easy path.
# ---------------------------------------------------------------------
cargo_target_dir="${CARGO_TARGET_DIR:-$repo/target}"
CARGOLESS_BIN="${CARGOLESS_BIN:-}"
if [ -z "$CARGOLESS_BIN" ]; then
  if command -v tftrunk >/dev/null 2>&1; then
    CARGOLESS_BIN="$(command -v tftrunk)"
  elif [ -x "$cargo_target_dir/release/tftrunk" ]; then
    CARGOLESS_BIN="$cargo_target_dir/release/tftrunk"
  else
    echo "building cargoless (tftrunk) with --features integration..."
    # NOTE: the cargo invocation inherits $CARGO_TARGET_DIR from this env;
    # cargo writes the binary under $cargo_target_dir/release/, which we
    # then point CARGOLESS_BIN at — regardless of whether $cargo_target_dir
    # is the project-default `$repo/target` or an external isolated dir.
    # Stream cargo output through `tee` so a build failure shows up in the
    # report instead of being swallowed by `>/dev/null` (the old shape hid
    # silent build successes-that-failed-to-place-the-binary as confusing
    # "missing binary" blockers).
    if ! ( cd "$repo" && cargo build --release -p tf-cli --features integration --locked 2>&1 | tail -50 ); then
      blocker "could not build tftrunk — cargo build failed. (In the ci-gate \
pod set TRIFORM_OPERATOR_APPROVED_BUILD=1; locally use the dedicated builder \
via scripts/ci-gate.)"
    fi
    CARGOLESS_BIN="$cargo_target_dir/release/tftrunk"
  fi
fi
[ -x "$CARGOLESS_BIN" ] || blocker "cargoless binary missing/non-exec at $CARGOLESS_BIN \
(checked CARGO_TARGET_DIR=$cargo_target_dir; if you set CARGO_TARGET_DIR externally, \
the build is expected to land under \$CARGO_TARGET_DIR/release/, not \$repo/target/release/)"
echo "cargoless bin: $CARGOLESS_BIN"
"$CARGOLESS_BIN" --version 2>/dev/null || "$CARGOLESS_BIN" help 2>/dev/null || true
echo

# ---------------------------------------------------------------------
# 3. Comparative tool detection. We DO NOT auto-install — installing
#    bacon/trunk on the fly would burn 10+ min of bench-run time and
#    pollute the warm cache. CI provisions them in the builder PVC.
# ---------------------------------------------------------------------
for t in trunk bacon; do
  if command -v "$t" >/dev/null 2>&1; then
    echo "comparator: $t -> $($t --version 2>&1 | head -1)"
  else
    echo "comparator: $t -> NOT INSTALLED (will be reported UNAVAILABLE in the verdict)"
  fi
done
echo

# ---------------------------------------------------------------------
# 4. Warm fixture deps (matches bench/run.sh — gives RA/cargo-check
#    timing that reflects "warm daemon" precondition, not "first cold
#    download").
# ---------------------------------------------------------------------
echo "fetching fixture dependencies (leptos)..."
if ! ( cd "$fixture" && cargo fetch ) >/dev/null 2>&1; then
  blocker "cargo fetch failed in the fixture — most likely no crates.io \
egress in this runner. The comparative bench depends on a warmed dep graph."
fi
# Warm BOTH `cargo build` (binary profile — covers any tool that uses it)
# AND `cargo check` (check profile — what cargoless's authoritative tier,
# bacon, and trunk's typecheck-pass all run). cargo's per-profile
# fingerprint cache means these are DISJOINT — warming one does not warm
# the other. The 4th comparative run hit bacon NO_READY at 300s because
# cargo check was cold; this fix removes that surface.
echo "warming fixture build cache (cargo build — release of any binary target)..."
( cd "$fixture" && cargo build ) >/dev/null 2>&1 \
  || echo "WARN: fixture cargo build did not fully succeed; bench will still try."
echo "warming fixture check cache (cargo check — cargoless + bacon's tier)..."
( cd "$fixture" && cargo check ) >/dev/null 2>&1 \
  || echo "WARN: fixture cargo check did not fully succeed; bench will still try."
echo

# ---------------------------------------------------------------------
# 5. Build the harness binary (std-only — same constraints as run.sh).
#    HONORS $CARGO_TARGET_DIR — same fix shape as the tftrunk block
#    above. bench/harness is its own [workspace] so its default target
#    dir is `$harness/target`, but if the caller has set an external
#    CARGO_TARGET_DIR (per-branch isolation), cargo writes there
#    instead and we must look for the binary there too.
# ---------------------------------------------------------------------
harness_target_dir="${CARGO_TARGET_DIR:-$harness/target}"
echo "building cargoless-bench harness..."
# Surface build output on failure — silent /dev/null hid a real build
# error as a confusing "missing binary" blocker in an earlier rev.
if ! ( cd "$harness" && cargo build --release 2>&1 | tail -50 ); then
  blocker "harness failed to build."
fi
bin="$harness_target_dir/release/cargoless-bench"
[ -x "$bin" ] || blocker "harness binary missing at $bin \
(checked CARGO_TARGET_DIR=$harness_target_dir; if you set CARGO_TARGET_DIR \
externally, the harness binary lands there, not at \$harness/target/release/)"
echo "harness bin: $bin"
echo

# ---------------------------------------------------------------------
# 6. Run it. The harness prints the full report + verdict lines and exits 0.
# ---------------------------------------------------------------------
hr
"$bin" all \
  --fixture "$fixture" \
  --cargoless-bin "$CARGOLESS_BIN" \
  --out "$fixture/.cargoless-bench-out" \
  "${@}"
hr
exit 0
