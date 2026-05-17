#!/usr/bin/env bash
# bench/run-throughput.sh — drives bench/throughput.py with the same
# fixture-warm + cargoless-bin discovery pattern bench/run-comparative.sh
# uses. Operator pivot 2026-05-17: AC#7 throughput axis (CPU/RAM
# efficiency) replaces the latency axis as the primary comparative
# dimension.
#
# Always exits 0 by design — evidence, not gate. The TPUT_TOOL / TPUT_VERDICT
# lines this prints are the deliverable; ci-gate publishes them as a
# commit status downstream.

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"
fixture="$here/fixture"

hr() { printf '%s\n' "------------------------------------------------------------"; }
blocker() {
  hr
  echo "BLOCKER: $1"
  hr
  echo "TPUT_VERDICT: BLOCKED — $1"
  exit 0
}

echo "=== cargoless throughput bench driver (run-throughput.sh) ==="
echo "fixture: $fixture"
echo "repo:    $repo"
echo

# Honest-size guard (reasserted; same floor as run.sh / run-comparative.sh).
rs_files=$(find "$fixture/src" -name '*.rs' 2>/dev/null | wc -l | tr -d ' ')
rs_loc=$(find "$fixture/src" -name '*.rs' -exec cat {} + 2>/dev/null | wc -l | tr -d ' ')
MIN_FILES=12; MIN_LOC=800
echo "honest-size guard: ${rs_files} rust files, ${rs_loc} LOC (floor: ${MIN_FILES} files / ${MIN_LOC} LOC)"
if [ "${rs_files:-0}" -lt "$MIN_FILES" ] || [ "${rs_loc:-0}" -lt "$MIN_LOC" ]; then
  blocker "fixture below honest-size floor (${rs_files}f/${rs_loc}L < ${MIN_FILES}f/${MIN_LOC}L)"
fi

# cargoless bin discovery (honors CARGO_TARGET_DIR — same as run-comparative.sh)
cargo_target_dir="${CARGO_TARGET_DIR:-$repo/target}"
CARGOLESS_BIN="${CARGOLESS_BIN:-}"
if [ -z "$CARGOLESS_BIN" ]; then
  if command -v tftrunk >/dev/null 2>&1; then
    CARGOLESS_BIN="$(command -v tftrunk)"
  elif [ -x "$cargo_target_dir/release/tftrunk" ]; then
    CARGOLESS_BIN="$cargo_target_dir/release/tftrunk"
  else
    echo "building cargoless (tftrunk)..."
    if ! ( cd "$repo" && cargo build --release -p tf-cli --features integration --locked 2>&1 | tail -50 ); then
      blocker "could not build tftrunk — cargo build failed."
    fi
    CARGOLESS_BIN="$cargo_target_dir/release/tftrunk"
  fi
fi
[ -x "$CARGOLESS_BIN" ] || blocker "cargoless binary missing at $CARGOLESS_BIN"
echo "cargoless bin: $CARGOLESS_BIN"

# Comparator detection (informational — throughput.py will mark UNAVAILABLE)
for t in trunk bacon python3; do
  if command -v "$t" >/dev/null 2>&1; then
    echo "tool: $t -> $($t --version 2>&1 | head -1)"
  else
    echo "tool: $t -> NOT INSTALLED"
  fi
done

# Fixture warm (cargo fetch + build + check) — different fingerprint sets
# than the cargoless workspace; per-tool first-spawn would pay this cost
# otherwise. Output suppressed (these are slow; only failure surfaces).
echo
echo "fetching fixture dependencies (leptos)..."
if ! ( cd "$fixture" && cargo fetch ) >/dev/null 2>&1; then
  blocker "cargo fetch failed in the fixture — no crates.io egress?"
fi
echo "warming fixture cargo build cache (binary profile)..."
( cd "$fixture" && cargo build ) >/dev/null 2>&1 \
  || echo "WARN: fixture cargo build did not fully succeed; bench will still try."
echo "warming fixture cargo check cache (cargoless + bacon's tier)..."
( cd "$fixture" && cargo check ) >/dev/null 2>&1 \
  || echo "WARN: fixture cargo check did not fully succeed; bench will still try."

# wasm-bindgen-cli for trunk. trunk auto-downloads a STATIC-PIE wasm-bindgen
# to ~/.cache/trunk/, and THIS pod's loader cannot exec static-PIE binaries
# ("Exec format error (os error 8)") — verified: pod is x86_64, binary is
# x86-64, the problem is the static-pie link mode, not the arch. Installing
# wasm-bindgen-cli via cargo puts a dynamically-linked binary on PATH;
# trunk prefers a PATH wasm-bindgen over its cached download, so this
# unblocks trunk's wasm pipeline. One-time (PVC-cached after first run).
# Pinned to 0.2.121 to match the version trunk's manifest resolved.
if command -v wasm-bindgen >/dev/null 2>&1; then
  echo "wasm-bindgen on PATH: $(wasm-bindgen --version 2>&1 | head -1)"
else
  echo "installing wasm-bindgen-cli@0.2.121 (dynamically-linked; trunk's static-pie cache won't exec in this pod)..."
  if ! cargo install wasm-bindgen-cli --version 0.2.121 >/dev/null 2>&1; then
    echo "WARN: wasm-bindgen-cli install failed; trunk's wasm pipeline will likely still error (env constraint, documented in the report)."
  else
    echo "wasm-bindgen now on PATH: $(wasm-bindgen --version 2>&1 | head -1)"
  fi
fi
# Also add the rustup wasm32 target if missing — trunk needs it to build
# the cdylib for the bench fixture.
if ! rustup target list --installed 2>/dev/null | grep -q wasm32-unknown-unknown; then
  echo "adding wasm32-unknown-unknown rustup target..."
  rustup target add wasm32-unknown-unknown >/dev/null 2>&1 \
    || echo "WARN: rustup target add wasm32-unknown-unknown failed."
fi
echo

# Invoke the python driver. Args after `--` go straight through.
hr
python3 "$here/throughput.py" \
  --fixture "$fixture" \
  --cargoless-bin "$CARGOLESS_BIN" \
  "${@}"
hr
exit 0
