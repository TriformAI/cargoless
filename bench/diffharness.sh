#!/usr/bin/env bash
# cargoless Increment-3 differential proof harness — the no-false-GREEN oracle.
#
# Home: the S1/#15 two-mode bench (sibling of bench/run.sh,
# bench/modelr-fleet.sh). Spec + matrix: bench/diffmatrix/.
#
# WHAT THIS GATES
#   Every Increment-3 (check / clippy authority) increment. For each
#   fixture in bench/diffmatrix/EXPECTED.tsv it compares cargoless's
#   verdict color to ground-truth cargo:
#       ground-truth := RED if ANY applicable oracle is RED else GREEN
#         oracle 1  cargo check  --all-targets --locked
#         oracle 2  cargo check  --all-targets --locked --target wasm32-unknown-unknown
#         oracle 3  cargo clippy --all-targets --locked -- -D warnings
#       cargoless    := exit(cargoless check)  0 GREEN | 1 RED | 2 BLOCKER
#
# CONTRACT — THIS IS A GATE, NOT EVIDENCE (the inverse of bench/run.sh,
# which always exits 0). Exit codes:
#       0  every case bit-identical (cargoless color ≡ required color)
#       3  >=1 FALSE-GREEN  (cargoless GREEN, ground-truth RED)  ← §9a class
#       4  >=1 FALSE-RED, no false-GREEN (cry-wolf)
#       5  --expect-baseline only: documented baseline drift (no false-GREEN)
#       2  BLOCKER — an oracle/cargoless/target could not run. NEVER
#          silently downgraded to a pass. Precedence: a real FALSE-GREEN
#          (3) always dominates a BLOCKER (2).
#
# MODES
#   (default, strict)      Increment-3 *completion* gate: pass requires
#                          cargoless color ≡ ground-truth for ALL cases.
#                          Fails today by design (3 GAP-INC3 false-GREENs).
#   --expect-baseline      day-one *regression* gate: pass requires
#                          cargoless color ≡ the documented per-case
#                          `cargoless_base` in EXPECTED.tsv. A MATCH-NOW
#                          case regressing into a false-GREEN still
#                          exits 3 in this mode (the §9a net never sleeps).
#
# BUILD-VEHICLE: no local cargo. Runs in the cargoless-builder pod / CI
# bench job (APPROVE prefix, pinned PATH, cargo fetch --locked egress
# BLOCKER) — same discipline as bench/run.sh.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"
matrix="$here/diffmatrix"
tsv="$matrix/EXPECTED.tsv"

export PATH="/usr/local/cargo/bin:${CARGO_HOME:-/cache/cargo}/bin:${PATH}"
APPROVE="${APPROVE:-TRIFORM_OPERATOR_APPROVED_BUILD=1}"
SCRATCH="${DIFF_SCRATCH:-/tmp/diffharness-$$}"
MODE="strict"
[ "${1:-}" = "--expect-baseline" ] && MODE="baseline"

mkdir -p "$SCRATCH"
hr() { printf '%s\n' "============================================================"; }

# A GATE blocker: LOUD, machine line last, NON-ZERO exit (2).
blocker() {
  hr
  echo "BLOCKER: $1"
  echo "(diffharness is a GATE — a BLOCKER is NOT a pass; exit 2.)"
  hr
  echo "DIFF_VERDICT: GATE=BLOCKER reason=${2:-could-not-run} mode=${MODE}"
  exit 2
}

echo "=== cargoless Increment-3 differential proof harness (bench/diffharness.sh) ==="
echo "mode=${MODE}  matrix=${tsv}  scratch=${SCRATCH}"
echo

[ -f "$tsv" ] || blocker "EXPECTED.tsv missing at $tsv" matrix-missing
command -v cargo >/dev/null 2>&1 || blocker "cargo not found on PATH" cargo-unavailable
command -v python3 >/dev/null 2>&1 || blocker "python3 not found (needed for the proc-macro anchor inject)" python3-unavailable
cargo clippy --version >/dev/null 2>&1 || blocker "cargo clippy unavailable (clippy oracle required)" clippy-unavailable
echo "cargo : $(cargo --version 2>/dev/null)"
echo "clippy: $(cargo clippy --version 2>/dev/null)"

# --- build cargoless ONCE (the unit under test) --------------------------
echo
echo "building cargoless (the verdict under test): cargo build -p cargoless --release --locked"
CL_TGT="$SCRATCH/cl-target"
if ! eval "$APPROVE CARGO_TARGET_DIR=$CL_TGT cargo build -p cargoless --release --locked" \
      >"$SCRATCH/cl-build.log" 2>&1; then
  echo "---- cargoless build log (tail) ----"; tail -25 "$SCRATCH/cl-build.log"
  if grep -qiE 'could not.*crates\.io|failed to (download|fetch)|no.*egress|registry' "$SCRATCH/cl-build.log"; then
    blocker "cargoless build failed — most likely no crates.io egress in this runner" no-crates-io-egress
  fi
  blocker "cargoless build failed (see log tail above) — cannot run the oracle without the unit under test" cargoless-build-failed
fi
CL_BIN="$CL_TGT/release/cargoless"
[ -x "$CL_BIN" ] || blocker "cargoless binary not at $CL_BIN after a successful build" cargoless-bin-missing
echo "cargoless: $CL_BIN"

# --- wasm32 target probe (honest: missing target BLOCKERs, never skips) ---
# We must run the wasm oracle for run_wasm=1 cases. If the wasm32 std is
# not installed, the load-bearing wasm-only-err case CANNOT be silently
# omitted — that omission would itself be a harness-level false-GREEN.
WASM_OK=0
if rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown; then
  WASM_OK=1
else
  # rustup may be absent (vendored toolchain) — probe via a real check
  # on the clean control fixture.
  if eval "$APPROVE CARGO_TARGET_DIR=$SCRATCH/wasm-probe cargo check --locked \
        --target wasm32-unknown-unknown --manifest-path $matrix/cases/clean/Cargo.toml" \
        >"$SCRATCH/wasm-probe.log" 2>&1; then
    WASM_OK=1
  fi
fi
echo "wasm32-unknown-unknown target available: $([ $WASM_OK -eq 1 ] && echo yes || echo NO)"
echo

# one oracle run: $1=label  $2=FULL cargo command (manifest already
# placed by the caller in the CORRECT position — clippy needs
# --manifest-path BEFORE the `--`, so this fn must NOT append it).
# -> echoes GREEN|RED ; sets ORACLE_LOG to the captured output path.
oracle() {
  local label="$1" cmd="$2"
  local log="$SCRATCH/oracle-${ORACLE_CASE}-${label}.log"
  ORACLE_LOG="$log"
  if eval "$APPROVE CARGO_TARGET_DIR=$ORACLE_TGT $cmd" >"$log" 2>&1; then
    echo GREEN
  else
    echo RED
  fi
}

# cargoless verdict for a project dir: $1=dir $2=env-prefix -> GREEN|RED|BLOCKER
cargoless_color() {
  local dir="$1" envp="$2"
  local log="$SCRATCH/cargoless-${ORACLE_CASE}.log"
  eval "$envp \"$CL_BIN\" check" >"$log" 2>&1 </dev/null
  case $? in
    0) echo GREEN ;;
    1) echo RED ;;
    *) echo BLOCKER ;;   # exit 2 = setup/env (rust-analyzer/spawn/bad root)
  esac
}

ANY_FALSE_GREEN=0     # strict: any cl=GREEN while truth=RED
ANY_FALSE_RED=0       # any cl=RED while truth=GREEN
ANY_BLOCKER=0         # cargoless could not produce a verdict (setup/env)
ANY_DRIFT=0           # baseline: any cl != documented EXPECTED.tsv base
ANY_REGRESSION_FG=0   # baseline §9a: a FALSE-GREEN the ledger did NOT
                      # predict (cl=GREEN, truth=RED, cl!=base) — the net
                      # that never sleeps even in baseline mode
ROWS=""

while IFS=$'\t' read -r case kind run_wasm run_clippy gt base inc3 klass inject; do
  case "$case" in ''|\#*) continue ;; esac
  ORACLE_CASE="$case"
  ORACLE_TGT="$SCRATCH/tgt-$case"
  hr
  echo "CASE $case  [kind=$kind klass=$klass]"

  # --- materialize the project + pick manifest/dir/env ------------------
  # envp="" (NOT ":") — a ":" prefix would make `eval ": cargoless check"`
  # run the `:` builtin (always exit 0) and turn EVERY crate case into a
  # silent GREEN: a false-GREEN generator inside the no-false-GREEN oracle.
  proj_dir=""; manifest=""; envp=""
  if [ "$kind" = "crate" ]; then
    proj_dir="$matrix/cases/$case"
    manifest="$proj_dir/Cargo.toml"
    [ -f "$manifest" ] || blocker "case $case: manifest missing at $manifest" fixture-missing
  elif [ "$kind" = "fixture-inject" ]; then
    proj_dir="$SCRATCH/proj-$case"
    rm -rf "$proj_dir"; cp -R "$here/fixture" "$proj_dir"
    manifest="$proj_dir/Cargo.toml"
    # inject = "ANCHOR_TOKEN|ENV=VAL"
    anchor="${inject%%|*}"; envp="${inject#*|}"
    # the anchor maps to run.sh's BENCH_MACRO_ANCHOR pair (post-view! err)
    if [ "$anchor" = "BENCH_MACRO_ANCHOR" ]; then
      tgtf="$proj_dir/src/components/metrics.rs"
      python3 - "$tgtf" 'count.get() /* BENCH_MACRO_ANCHOR */' \
                        'count.get_oops() /* BENCH_MACRO_ANCHOR */' <<'PY'
import sys
p,f,r=sys.argv[1],sys.argv[2],sys.argv[3]
s=open(p).read()
if f not in s:
    sys.stderr.write("ANCHOR_NOT_FOUND\n"); sys.exit(7)
open(p,"w").write(s.replace(f,r))
PY
      [ $? -eq 0 ] || blocker "case $case: BENCH_MACRO_ANCHOR not found in metrics.rs (fixture drift)" anchor-drift
    else
      blocker "case $case: unknown inject anchor '$anchor'" unknown-inject
    fi
    # fixture has real deps — locked fetch; no egress => honest BLOCKER
    if ! eval "$APPROVE CARGO_TARGET_DIR=$ORACLE_TGT cargo fetch --locked \
          --manifest-path $manifest" >"$SCRATCH/fetch-$case.log" 2>&1; then
      blocker "case $case: cargo fetch --locked failed (no crates.io egress?)" no-crates-io-egress
    fi
  else
    blocker "case $case: unknown kind '$kind'" unknown-kind
  fi

  # --- ground-truth oracles (full commands; manifest placed correctly:
  #     for clippy, --manifest-path MUST precede the `--`) --------------
  o_host=$(oracle host \
    "cargo check --all-targets --locked --manifest-path $manifest")
  echo "  oracle host  : $o_host"
  o_wasm="SKIP"
  if [ "$run_wasm" = "1" ]; then
    if [ "$WASM_OK" -ne 1 ]; then
      blocker "case $case: run_wasm=1 but wasm32 target unavailable — refusing to skip the load-bearing oracle (silent-skip == harness false-GREEN)" wasm-target-unavailable
    fi
    o_wasm=$(oracle wasm \
      "cargo check --all-targets --locked --target wasm32-unknown-unknown --manifest-path $manifest")
    echo "  oracle wasm  : $o_wasm"
  fi
  o_clip="SKIP"
  if [ "$run_clippy" = "1" ]; then
    o_clip=$(oracle clippy \
      "cargo clippy --all-targets --locked --manifest-path $manifest -- -D warnings")
    echo "  oracle clippy: $o_clip"
  fi

  truth=GREEN
  for o in "$o_host" "$o_wasm" "$o_clip"; do [ "$o" = "RED" ] && truth=RED; done
  if [ "$truth" != "$gt" ]; then
    blocker "case $case: computed ground-truth ($truth) != EXPECTED.tsv ground_truth ($gt) — the FIXTURE or an oracle is broken; a wrong oracle can never be allowed to certify cargoless" fixture-or-oracle-broken
  fi
  echo "  => ground-truth: $truth (EXPECTED.tsv: $gt) OK"

  # --- cargoless verdict ------------------------------------------------
  cl=$(cd "$proj_dir" && ORACLE_CASE="$case" cargoless_color "$proj_dir" "$envp")
  echo "  cargoless    : $cl   (env: ${envp})"

  # --- classify vs ground-truth (mode-independent facts) ---------------
  vs_truth="MATCH"
  if   [ "$cl" = "BLOCKER" ];                          then vs_truth="BLOCKER"; ANY_BLOCKER=1
  elif [ "$cl" = "$truth" ];                            then vs_truth="MATCH"
  elif [ "$cl" = "GREEN" ] && [ "$truth" = "RED" ];     then vs_truth="FALSE-GREEN"; ANY_FALSE_GREEN=1
  else                                                       vs_truth="FALSE-RED"; ANY_FALSE_RED=1
  fi

  # --- mode gate --------------------------------------------------------
  # strict   : pass iff cl ≡ ground-truth (the Increment-3 COMPLETION gate)
  # baseline : pass iff cl ≡ the documented EXPECTED.tsv base (the day-one
  #            REGRESSION gate). A documented GAP-INC3 false-GREEN is the
  #            EXPECTED state in baseline mode (cl==base) — it must NOT
  #            hard-fail there, or the dual-mode design is defeated. But a
  #            false-GREEN the ledger did NOT predict (cl!=base) is the
  #            §9a net firing — it never sleeps, in EITHER mode.
  verdict=""
  if [ "$MODE" = "strict" ]; then
    [ "$vs_truth" = "MATCH" ] && verdict="PASS" || verdict="FAIL($vs_truth)"
  else
    if [ "$cl" = "$base" ]; then
      verdict="PASS(baseline)"
    else
      ANY_DRIFT=1
      verdict="DRIFT(got=$cl base=$base)"
    fi
    if [ "$vs_truth" = "FALSE-GREEN" ] && [ "$cl" != "$base" ]; then
      ANY_REGRESSION_FG=1                       # §9a — dominates, exit 3
      verdict="REGRESSION-FALSE-GREEN"
    fi
  fi

  case "$vs_truth" in
    FALSE-GREEN) echo "  *** FALSE-GREEN — cargoless GREEN while ground-truth RED. §9a class. ***" ;;
    FALSE-RED)   echo "  ** FALSE-RED — cargoless RED while ground-truth GREEN (cry-wolf). **" ;;
    BLOCKER)     echo "  ** BLOCKER — cargoless could not produce a verdict (setup/env). **" ;;
  esac
  echo "  VERDICT: $case  truth=$truth  cargoless=$cl  vs=$vs_truth  -> $verdict"
  ROWS="${ROWS}${case}\t${truth}\t${cl}\t${vs_truth}\t${klass}\t${verdict}\n"
done < "$tsv"

# ---------------------------------------------------------------------
# Summary + GATE exit (LOUD; machine line last; non-zero on any fault)
# ---------------------------------------------------------------------
hr
echo "SUMMARY (mode=$MODE)"
printf 'case\ttruth\tcargoless\tvs-truth\tklass\tverdict\n'
printf '%b' "$ROWS"
hr

# Mode-aware exit. The §9a false-GREEN net (exit 3) dominates in BOTH
# modes; it only differs in WHICH false-GREEN counts:
#   strict   : ANY false-GREEN vs ground-truth (completion gate)
#   baseline : only a false-GREEN the ledger did NOT predict (regression)
if [ "$MODE" = "strict" ]; then
  if [ "$ANY_FALSE_GREEN" -eq 1 ]; then
    echo "GATE FAIL — FALSE-GREEN: cargoless GREEN on a broken tree."
    echo "The §9a-trap class this harness exists to catch. Close the"
    echo "Increment-3 authority gap (and flip that row's EXPECTED.tsv"
    echo "cargoless_base to ground_truth in the SAME commit)."
    echo "DIFF_VERDICT: GATE=FAIL class=FALSE-GREEN mode=strict"
    exit 3
  fi
  if [ "$ANY_BLOCKER" -eq 1 ]; then
    echo "GATE BLOCKER — a case could not be certified (cargoless setup/env)."
    echo "DIFF_VERDICT: GATE=BLOCKER class=cargoless-setup mode=strict"
    exit 2
  fi
  if [ "$ANY_FALSE_RED" -eq 1 ]; then
    echo "GATE FAIL — FALSE-RED (cry-wolf): cargoless RED on a green tree."
    echo "DIFF_VERDICT: GATE=FAIL class=FALSE-RED mode=strict"
    exit 4
  fi
  echo "GATE PASS — every case bit-identical to ground-truth."
  echo "(Increment-3 is COMPLETE: full check+clippy+wasm authority parity.)"
  echo "DIFF_VERDICT: GATE=PASS mode=strict"
  exit 0
else
  if [ "$ANY_REGRESSION_FG" -eq 1 ]; then
    echo "GATE FAIL — REGRESSION-FALSE-GREEN: a false-GREEN the EXPECTED.tsv"
    echo "ledger did NOT predict (cl != documented base). The §9a net never"
    echo "sleeps — a MATCH-NOW case broke, or a GAP-INC3 base is stale."
    echo "DIFF_VERDICT: GATE=FAIL class=REGRESSION-FALSE-GREEN mode=baseline"
    exit 3
  fi
  if [ "$ANY_BLOCKER" -eq 1 ]; then
    echo "GATE BLOCKER — a case could not be certified (cargoless setup/env)."
    echo "DIFF_VERDICT: GATE=BLOCKER class=cargoless-setup mode=baseline"
    exit 2
  fi
  if [ "$ANY_DRIFT" -eq 1 ]; then
    echo "GATE FAIL — baseline DRIFT: cargoless != the documented"
    echo "EXPECTED.tsv base (no unpredicted false-GREEN). If an Increment-3"
    echo "increment intentionally closed a GAP-INC3 case, flip that row's"
    echo "cargoless_base to ground_truth in the SAME commit. Else investigate"
    echo "(includes a MATCH-NOW case regressing into a FALSE-RED)."
    echo "DIFF_VERDICT: GATE=FAIL class=BASELINE-DRIFT mode=baseline"
    exit 5
  fi
  echo "GATE PASS — cargoless matches the documented honest baseline"
  echo "(reality == EXPECTED.tsv ledger, known GAP-INC3 gaps included)."
  echo "DIFF_VERDICT: GATE=PASS mode=baseline"
  exit 0
fi
