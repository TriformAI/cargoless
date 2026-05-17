#!/usr/bin/env bash
# cargoless S1 / AC#2 latency harness driver.
#
# Plane: CWDL-22 (S1 spike) / CWDL-3 (AC#2) / decision D-A2 (RATIFIED GO).
#
# WHAT THIS MEASURES (ratified architecture):
#   The AUTHORITATIVE verdict tier — incremental `cargo check` — which is
#   what cargoless uses to decide RED/GREEN. The S1 spike proved
#   rust-analyzer NATIVE diagnostics (checkOnSave off) are BLIND to
#   type/trait/macro errors (0 diagnostics in 60s for a linked E0599), so
#   they can never be the verdict authority; they are at most an advisory
#   fast-hint. The committed bench/harness/ crate is retained as that
#   RA-native advisory probe (manual tooling) and is intentionally NOT on
#   this verdict path.
#
#   On a WARM daemon, the metric is the median wall-clock of an
#   incremental `cargo check --locked` after a single-file edit, for two
#   error classes:
#     * trait : a plain-Rust E0599 (unresolved method)        — RA-native blind
#     * view! : an error that only exists AFTER Leptos `view!`  — RA-native blind
#               proc-macro expansion (cargo check runs the full
#               front-end incl. proc-macro expansion, so it catches it)
#   Cold first-check (cold Leptos cache, minutes) is start-up / D-A1
#   setup, explicitly NOT the AC#2 metric.
#
# DESIGN: ALWAYS exits 0. The spike is evidence, not a CI gate — an honest
# regression must never turn `main` permanently red. The final stdout line
# `^S1_VERDICT:` is lifted into the Forgejo `s1-ac2-verdict` commit status
# (the only S1 channel readable via API on this build).
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fixture="$here/fixture"

# cargo location: PATH on the Forgejo runner; explicit on the dedicated
# builder (where it lives under /usr/local/cargo or $CARGO_HOME).
export PATH="/usr/local/cargo/bin:${CARGO_HOME:-/cache/cargo}/bin:${PATH}"
APPROVE="${APPROVE:-TRIFORM_OPERATOR_APPROVED_BUILD=1}"

REPS="${REPS:-5}"
AC2_BUDGET_MS="${AC2_BUDGET_MS:-1000}"
TRAIT_FILE="src/domain/model.rs"
VIEW_FILE="src/components/metrics.rs"
TRAIT_FIND='self.entries.len() /* BENCH_TRAIT_ANCHOR */'
TRAIT_REPL='self.entries.len_oops() /* BENCH_TRAIT_ANCHOR */'
VIEW_FIND='count.get() /* BENCH_MACRO_ANCHOR */'
VIEW_REPL='count.get_oops() /* BENCH_MACRO_ANCHOR */'

hr() { printf '%s\n' "------------------------------------------------------------"; }

# blocker $1=human-msg $2=kebab-token — prints the machine verdict LAST.
blocker() {
  hr
  echo "BLOCKER: $1"
  echo
  echo "D-A2 GO/NO-GO: BLOCKED — this run could not produce a measurement."
  echo "(run.sh exits 0 by design — evidence, not a CI gate.)"
  hr
  echo "S1_VERDICT: BLOCKER=${2:-spike-could-not-run} AC2=UNKNOWN D-A2=BLOCKED"
  exit 0
}

median() { # stdin: integers, one per line -> median (lower-mid for even n)
  sort -n | awk '{a[NR]=$1} END{ if(NR==0){print "NA"} else {print a[int((NR+1)/2)]} }'
}

echo "=== cargoless S1 / AC#2 authoritative-checker harness (bench/run.sh) ==="
echo "fixture: $fixture"
echo "reps=$REPS  ac2_budget=${AC2_BUDGET_MS}ms  basis=cargo-check/warm-incremental"
echo

# ---------------------------------------------------------------------
# Honest-size guard. A trivially tiny fixture would flatter the numbers
# and make the AC#2 / D-A2 verdict dishonest.
# ---------------------------------------------------------------------
rs_files=$(find "$fixture/src" -name '*.rs' ! -name '._*' 2>/dev/null | wc -l | tr -d ' ')
rs_loc=$(find "$fixture/src" -name '*.rs' ! -name '._*' -exec cat {} + 2>/dev/null | wc -l | tr -d ' ')
MIN_FILES=12
MIN_LOC=800
echo "honest-size guard: ${rs_files} rust files, ${rs_loc} LOC (floor: ${MIN_FILES}/${MIN_LOC})"
if [ "${rs_files:-0}" -lt "$MIN_FILES" ] || [ "${rs_loc:-0}" -lt "$MIN_LOC" ]; then
  echo "HONEST-SIZE GUARD FAILED — refusing to report a flattering number."
  blocker "fixture below honest-size floor (${rs_files}f/${rs_loc}L < ${MIN_FILES}f/${MIN_LOC}L)" fixture-below-honest-size-floor
fi

command -v cargo >/dev/null 2>&1 || blocker "cargo not found on PATH" cargo-unavailable
echo "cargo: $(command -v cargo)  ($(cargo --version 2>/dev/null))"
echo

cd "$fixture" || blocker "fixture dir missing at $fixture" fixture-dir-missing
[ -f Cargo.lock ] || blocker "fixture Cargo.lock missing (determinism + MSRV pin required)" fixture-lock-missing
grep -q '^resolver = "3"' Cargo.toml || echo "WARN: resolver=3 not found in Cargo.toml (MSRV float risk)"

# ---------------------------------------------------------------------
# Dependency fetch (locked). No crates.io egress => explicit BLOCKER,
# never a misleading slow/red number.
# ---------------------------------------------------------------------
echo "fetching fixture deps (cargo fetch --locked)..."
if ! eval "$APPROVE cargo fetch --locked" >/dev/null 2>&1; then
  blocker "cargo fetch --locked failed — most likely no crates.io egress in this runner." no-crates-io-egress
fi

# ---------------------------------------------------------------------
# COLD prime (NOT the AC#2 metric): one full check so the check-profile
# cache is warm. A clean fixture MUST check green here.
# ---------------------------------------------------------------------
echo "cold prime: cargo check --locked (warm-up, not measured; cold Leptos = minutes)..."
cold_s=$(date +%s%N)
if ! eval "$APPROVE cargo check --locked --quiet" >/tmp/s1_cold.err 2>&1; then
  echo "---- cold check stderr (head) ----"; head -20 /tmp/s1_cold.err
  blocker "clean fixture failed cargo check --locked (cold) — fixture/toolchain defect, not a verdict." fixture-cold-check-failed
fi
cold_ms=$(( ( $(date +%s%N) - cold_s ) / 1000000 ))
echo "cold prime done in ${cold_ms}ms (informational only)"
echo

# one timed incremental check; sets globals CK_MS / CK_RC / CK_ERRGREP
check_once() {
  : > /tmp/s1_c.err
  local s e
  s=$(date +%s%N)
  eval "$APPROVE cargo check --locked --quiet" >/dev/null 2>/tmp/s1_c.err
  CK_RC=$?
  e=$(date +%s%N)
  CK_MS=$(( (e - s) / 1000000 ))
}

# warm no-op floor
echo "=== warm no-op floor (x3) ==="
for i in 1 2 3; do check_once; echo "  noop[$i]=${CK_MS}ms rc=${CK_RC}"; done
echo

# measure one error class: $1 name, $2 file, $3 find, $4 repl, $5 expect-regex
SAMPLES=""; FID=0; ATT=0
measure_class() {
  local name="$1" file="$2" find="$3" repl="$4" expect="$5"
  cp "$file" /tmp/s1.bak
  SAMPLES=""; FID=0; ATT=0
  echo "=== $name : inject -> cargo check (x${REPS}) ==="
  local i
  for i in $(seq 1 "$REPS"); do
    cp /tmp/s1.bak "$file"
    # literal replace (anchor appears in code + a doc-comment mention;
    # replacing the comment copy too is harmless, the code error stands)
    python3 - "$file" "$find" "$repl" <<'PY'
import sys
p,f,r=sys.argv[1],sys.argv[2],sys.argv[3]
s=open(p).read()
open(p,"w").write(s.replace(f,r))
PY
    check_once
    ATT=$((ATT+1))
    local caught=""
    if [ "$CK_RC" -ne 0 ] && grep -qE "$expect" /tmp/s1_c.err; then
      caught="yes"; FID=$((FID+1)); SAMPLES="${SAMPLES}${CK_MS}"$'\n'
    else
      caught="NO(rc=${CK_RC})"
    fi
    echo "  $name[$i]=${CK_MS}ms caught=${caught}"
    cp /tmp/s1.bak "$file"
  done
  cp /tmp/s1.bak "$file"
  check_once   # settle back to green
  echo "  $name revert -> rc=${CK_RC} (${CK_MS}ms)"
  echo
}

measure_class TRAIT "$TRAIT_FILE" "$TRAIT_FIND" "$TRAIT_REPL" "E0599|no method named .?len_oops"
TR_MED=$(printf '%s' "$SAMPLES" | sed '/^$/d' | median)
TR_FID=$FID; TR_ATT=$ATT

measure_class VIEW "$VIEW_FILE" "$VIEW_FIND" "$VIEW_REPL" "E0599|no method named .?get_oops|get_oops"
VW_MED=$(printf '%s' "$SAMPLES" | sed '/^$/d' | median)
VW_FID=$FID; VW_ATT=$ATT

# ---------------------------------------------------------------------
# Verdict
# ---------------------------------------------------------------------
pass_of() { # $1 median $2 fid $3 att -> PASS/FAIL
  local m="$1" fid="$2" att="$3"
  if [ "$m" = "NA" ] || [ "$fid" -lt "$att" ] || [ "$att" -eq 0 ]; then echo FAIL; return; fi
  if [ "$m" -lt "$AC2_BUDGET_MS" ]; then echo PASS; else echo FAIL; fi
}
TR_P=$(pass_of "$TR_MED" "$TR_FID" "$TR_ATT")
VW_P=$(pass_of "$VW_MED" "$VW_FID" "$VW_ATT")
if [ "$TR_P" = PASS ] && [ "$VW_P" = PASS ]; then AC2=PASS; DA2=GO; else AC2=FAIL; DA2=NO-GO; fi

hr
echo "RESULTS (authoritative cargo-check, warm incremental, median of ${REPS})"
echo "  no-op floor              : see above"
echo "  trait-error  (E0599)     : median ${TR_MED}ms  fidelity ${TR_FID}/${TR_ATT}  -> ${TR_P}"
echo "  view!-macro  (post-exp.) : median ${VW_MED}ms  fidelity ${VW_FID}/${VW_ATT}  -> ${VW_P}"
echo "  AC#2 (<${AC2_BUDGET_MS}ms, full fidelity): ${AC2}    D-A2: ${DA2}"
echo "  NOTE: RA-native diagnostics are BLIND to these classes (S1 finding);"
echo "        cargo-check is the verdict authority, RA-native advisory-only."
hr

if [ "$DA2" = GO ]; then
  REWORD='median save->verdict <1s via authoritative cargo-check (incl. view! macros); RA-native advisory-only, never authoritative'
else
  REWORD='authoritative cargo-check median >=1s or fidelity gap; renegotiate AC#2 with evidence; RA-native advisory-only'
fi
echo "(run.sh exits 0 by design — evidence, not a CI gate)"
echo "S1_VERDICT: trait_err=${TR_MED}ms:${TR_P} view_macro=${VW_MED}ms:${VW_P} AC2=${AC2} D-A2=${DA2} basis=cargo-check/warm-incremental reword=\"${REWORD}\""
exit 0
