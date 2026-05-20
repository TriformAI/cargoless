#!/usr/bin/env bash
#
# bench/m2-cpu-approx.sh — pre-deploy SYNTHETIC approximation of M2.
#
# WHAT THIS IS / ISN'T (the honesty boundary — carry verbatim into reports)
#   IS:    a SYNTHETIC pre-deploy proxy of the cold-pod check-remote
#          status quo, on the same bench/fixture Leptos substrate AC7 §8.5
#          and #15/#196 measured.
#   ISN'T: a measurement against tf-multiverse's actual `scripts/check-remote`
#          in a fresh K8s pod per edit. That pattern adds pod scheduling +
#          container init (~seconds per invocation) on top of the cargo
#          work — NOT captured here. This is the pre-deploy bracketing
#          number, not the post-deploy answer. The real M2 lives at
#          bench/post-deploy/m2-cpu-vs-coldpod.sh (still NOT-IMPLEMENTED).
#
# THE GAP THIS CLOSES
#   The shipped 2.05× per-edit CPU win is measured vs `trunk serve` (a
#   rebundle-every-save bundler — AC7 §8.5 two-source-confirmed). It is
#   NOT vs the actual cold-pod check-remote status quo: that gap remains
#   unmeasured. This script gives a pre-deploy bracket of it.
#
# ARMS
#   arm-A  cargoless WARM daemon — one `cargoless watch` warm, per-edit
#          CPU as Δ(Σ utime+stime+cutime+cstime over its whole descendant
#          subtree) → captures BOTH live and reaped cargo-check children.
#   arm-B  cold `cargo check` per edit, /target shared across edits
#          (matches the operator's mounted-volume pattern) — fresh
#          subprocess each edit; measured via /usr/bin/time -v.
#   arm-C  optional (ARM_C=1): cold ONE-SHOT `cargoless check` per edit
#          — captures the "no daemon reuse" cargoless pattern (cold RA
#          spawn every invocation). Off by default.
#
# BUILD-VEHICLE: no local cargo; runs in the cargoless-builder pod via
# the committed-script discipline (APPROVE prefix, pinned PATH, locked
# fetch with no-egress BLOCKER), same as bench/run.sh + modelr-fleet.sh.
#
# OUTPUT
#   per-edit CPU rows + median/p5/p95 per arm + RATIO_RESULT line +
#   DONE_SENTINEL. The verdict is the RATIO + the methodology caveats it
#   travels with — never the bare number.

set -u
export COPYFILE_DISABLE=1
export PATH="/usr/local/cargo/bin:${CARGO_HOME:-/cache/cargo}/bin:${PATH}"
APPROVE="${APPROVE:-TRIFORM_OPERATOR_APPROVED_BUILD=1}"

SRC="${SRC:-/work/src}"
REPS="${REPS:-10}"
INTER_EDIT_GAP="${INTER_EDIT_GAP:-8}"
WARM_TIMEOUT="${WARM_TIMEOUT:-600}"   # cold-Leptos first verdict is minutes
EDIT_TIMEOUT="${EDIT_TIMEOUT:-60}"
SETTLE_TIMEOUT="${SETTLE_TIMEOUT:-60}"
RUN_ARM_C="${ARM_C:-0}"
WORK="${WORK:-/tmp/m2approx}"

# edit anchor — same TRAIT-class anchor bench/run.sh uses for AC#2
TRAIT_FILE="src/domain/model.rs"
TRAIT_FIND='self.entries.len() /* BENCH_TRAIT_ANCHOR */'
TRAIT_REPL='self.entries.len_oops() /* BENCH_TRAIT_ANCHOR */'

say() { echo "[m2-approx $(date -u +%H:%M:%S)] $*"; }
die() { echo "RATIO_RESULT FAIL :: $*"; echo "DONE_SENTINEL"; exit 1; }

[ -d "$SRC/crates/cargoless" ] || die "no cargoless crate under SRC=$SRC"
[ -d "$SRC/bench/fixture/src" ] || die "no bench/fixture under SRC=$SRC"
command -v cargo   >/dev/null 2>&1 || die "cargo not on PATH"
command -v python3 >/dev/null 2>&1 || die "python3 not on PATH (edit driver)"
command -v /usr/bin/time >/dev/null 2>&1 || die "/usr/bin/time -v required for arm-B"

CLK_TCK="$(getconf CLK_TCK 2>/dev/null || echo 100)"
say "CLK_TCK=$CLK_TCK reps=$REPS inter-edit-gap=${INTER_EDIT_GAP}s"

# ── fixture scratch (per-run isolated; honest controls) ─────────────────
rm -rf "$WORK"; mkdir -p "$WORK"
cp -a "$SRC/bench/fixture" "$WORK/fixture"
find "$WORK/fixture" -name '._*' -type f -delete 2>/dev/null
FIX="$WORK/fixture"

# ── build cargoless (per-ref isolated CARGO_TARGET_DIR — #15 discipline) ─
REFTAG="$(cd "$SRC" && git rev-parse --short HEAD 2>/dev/null || echo "$(date +%s)")"
CL_TGT="/tmp/m2-cl-tgt-${REFTAG}"
say "building cargoless (release) @ streamed tree ..."
BUILD_START=$(date +%s)
( cd "$SRC" && CARGO_TARGET_DIR="$CL_TGT" $APPROVE cargo build -p cargoless --release --locked ) \
  > /tmp/m2-cl-build.log 2>&1 \
  || { tail -30 /tmp/m2-cl-build.log; die "cargoless build failed"; }
CL_BIN="$CL_TGT/release/cargoless"
[ -x "$CL_BIN" ] || die "cargoless binary missing at $CL_BIN"
BIN_MTIME=$(stat -c %Y "$CL_BIN" 2>/dev/null || echo 0)
[ "$BIN_MTIME" -ge "$BUILD_START" ] \
  || die "cargoless binary mtime $BIN_MTIME < build-start $BUILD_START — stale, refusing to measure"
say "cargoless = $CL_BIN  mtime=$BIN_MTIME ($("$CL_BIN" --version 2>/dev/null | head -1))"

# locked fetch on the fixture (Leptos deps) — honest BLOCKER if egress is absent
( cd "$FIX" && $APPROVE cargo fetch --locked ) > /tmp/m2-fetch.log 2>&1 \
  || die "cargo fetch --locked on bench/fixture failed (no crates.io egress?)"

# ── helpers (descendant-tree walk, not pgid — the §11-v1 setsid lesson) ─
kids() { echo "$1"; local c; for c in $(pgrep -P "$1" 2>/dev/null); do kids "$c"; done; }

# Σ utime+stime+cutime+cstime (clock ticks) across the entire live
# descendant subtree of $1. /proc/<pid>/stat fields after the (comm):
# state, ppid, pgrp, session, tty_nr, tpgid, flags, minflt, cminflt,
# majflt, cmajflt, utime(12), stime(13), cutime(14), cstime(15).
tree_cpu_jiffies() {
  local root=$1 tot=0 p s
  for p in $(kids "$root" | sort -u); do
    s=$(awk '{
      line=$0;
      sub(/^[^()]*\([^)]*\) */, "", line);     # strip "pid (comm) "
      n=split(line, a, " ");
      printf "%d", a[12]+a[13]+a[14]+a[15];
    }' "/proc/$p/stat" 2>/dev/null) || s=0
    tot=$((tot + ${s:-0}))
  done
  echo "$tot"
}

# cli-status reader: echoes "<updated>:<verdict>" or "NONE"
read_verdict() {
  local f="$1"
  [ -f "$f" ] || { echo "NONE"; return; }
  local u v
  u=$(awk -F= '$1=="updated"{print $2}' "$f")
  v=$(awk -F= '$1=="verdict"{print $2}' "$f")
  if [ -z "$u" ] || [ -z "$v" ]; then echo "NONE"; else echo "${u}:${v}"; fi
}

wait_verdict() {   # $1=expected $2=timeout $3=cli-status $4=updated-after-min
  local exp=$1 to=$2 f=$3 min=$4 dl=$(( $(date +%s) + to )) rv u v
  while [ $(date +%s) -lt $dl ]; do
    if [ -f "$f" ]; then
      rv=$(read_verdict "$f")
      if [ "$rv" != "NONE" ]; then
        u="${rv%%:*}"; v="${rv##*:}"
        [ "$v" = "$exp" ] && [ "$u" -gt "$min" ] && return 0
      fi
    fi
    sleep 0.5
  done
  return 1
}

# edit driver — literal substring swap, byte-faithful to bench/run.sh
do_edit() {  # $1=file $2=find $3=repl
  python3 - "$1" "$2" "$3" <<'PY'
import sys
p, f, r = sys.argv[1], sys.argv[2], sys.argv[3]
s = open(p).read()
if f not in s:
    sys.stderr.write("EDIT_DRIVER: anchor not found in " + p + "\n")
    sys.exit(7)
open(p, "w").write(s.replace(f, r))
PY
}

# ── ARM-A: cargoless WARM daemon ─────────────────────────────────────────
say "ARM-A: cargoless WARM daemon on bench/fixture (per-edit Δ CPU jiffies)"
rm -rf "$FIX/.cargoless"
( cd "$FIX" && "$CL_BIN" watch ) > /tmp/m2-watch.log 2>&1 &
WPID=$!
CLI="$FIX/.cargoless/cli-status"
say "  daemon pid=$WPID; waiting for first verdict (cap ${WARM_TIMEOUT}s)"
deadline=$(( $(date +%s) + WARM_TIMEOUT ))
warm=0
while [ $(date +%s) -lt $deadline ]; do
  kill -0 "$WPID" 2>/dev/null || { tail -25 /tmp/m2-watch.log; die "cargoless watch died during warm-up"; }
  if [ -f "$CLI" ]; then
    rv=$(read_verdict "$CLI")
    if [ "$rv" != "NONE" ] && [ "${rv##*:}" != "unknown" ]; then warm=1; break; fi
  fi
  sleep 1
done
if [ "$warm" -ne 1 ]; then
  tail -25 /tmp/m2-watch.log
  kill "$WPID" 2>/dev/null; sleep 1; kill -9 "$WPID" 2>/dev/null
  die "cargoless never warmed within ${WARM_TIMEOUT}s (cold Leptos cargo-check; bump WARM_TIMEOUT?)"
fi
say "  daemon warm; initial verdict $(read_verdict "$CLI")"

A_SAMPLES=""
A_OK=0
for i in $(seq 1 "$REPS"); do
  pre_rv=$(read_verdict "$CLI"); pre_u="${pre_rv%%:*}"
  pre_cpu=$(tree_cpu_jiffies "$WPID")
  do_edit "$FIX/$TRAIT_FILE" "$TRAIT_FIND" "$TRAIT_REPL" \
    || { say "  ARM-A rep $i: anchor-not-found during inject — aborting arm"; break; }
  if ! wait_verdict red "$EDIT_TIMEOUT" "$CLI" "${pre_u:-0}"; then
    say "  ARM-A rep $i: timed out waiting for RED (post-edit) — skipping rep"
    # revert anyway to leave the tree green for the next attempt
    do_edit "$FIX/$TRAIT_FILE" "$TRAIT_REPL" "$TRAIT_FIND" 2>/dev/null || true
    sleep "$INTER_EDIT_GAP"; continue
  fi
  post_cpu=$(tree_cpu_jiffies "$WPID")
  delta_jiffies=$(( post_cpu - pre_cpu ))
  delta_ms=$(( delta_jiffies * 1000 / CLK_TCK ))
  say "  ARM-A rep $i: Δjiffies=$delta_jiffies  per-edit_cpu_ms=$delta_ms"
  A_SAMPLES="${A_SAMPLES}${delta_ms}"$'\n'; A_OK=$((A_OK+1))
  green_rv=$(read_verdict "$CLI"); green_u="${green_rv%%:*}"
  do_edit "$FIX/$TRAIT_FILE" "$TRAIT_REPL" "$TRAIT_FIND" || true
  wait_verdict green "$SETTLE_TIMEOUT" "$CLI" "${green_u:-0}" \
    || say "  ARM-A rep $i: WARN green-back timed out — proceeding"
  sleep "$INTER_EDIT_GAP"
done
say "ARM-A complete: $A_OK measured reps"
kill "$WPID" 2>/dev/null
for _ in $(seq 1 10); do kill -0 "$WPID" 2>/dev/null || break; sleep 0.5; done
kill -9 "$WPID" 2>/dev/null || true
sleep 1

# ── ARM-B: cold cargo-check per edit (shared warm /target) ──────────────
say "ARM-B: cold cargo-check per edit, shared warm /target (status-quo synthetic)"
B_TGT="/tmp/m2-arm-b-tgt-${REFTAG}"
rm -rf "$B_TGT"
# warm /target ONCE — NOT measured (this is the operator's persistent-volume warmth)
( cd "$FIX" && CARGO_TARGET_DIR="$B_TGT" $APPROVE cargo check --locked --all-targets ) \
  > /tmp/m2-armb-warm.log 2>&1 \
  || { tail -15 /tmp/m2-armb-warm.log; die "ARM-B initial cold cargo-check failed (substrate broken)"; }
say "  /target warmed; ARM-B per-edit measurements begin"

B_SAMPLES=""
B_OK=0
for i in $(seq 1 "$REPS"); do
  do_edit "$FIX/$TRAIT_FILE" "$TRAIT_FIND" "$TRAIT_REPL" \
    || { say "  ARM-B rep $i: anchor-not-found — aborting arm"; break; }
  tf=/tmp/m2-armb-time-$i.log
  ( cd "$FIX" && CARGO_TARGET_DIR="$B_TGT" /usr/bin/time -v -o "$tf" \
      $APPROVE cargo check --locked --all-targets ) > /tmp/m2-armb-cargo-$i.log 2>&1
  # /usr/bin/time -v "User time (seconds)" + "System time (seconds)" (incl children — GNU time does this)
  u=$(awk -F': ' '/User time \(seconds\)/{print $2}' "$tf")
  s=$(awk -F': ' '/System time \(seconds\)/{print $2}' "$tf")
  ms=$(awk -v u="${u:-0}" -v s="${s:-0}" 'BEGIN{printf "%d", (u+s)*1000}')
  say "  ARM-B rep $i: user=${u}s  sys=${s}s  per-edit_cpu_ms=$ms"
  B_SAMPLES="${B_SAMPLES}${ms}"$'\n'; B_OK=$((B_OK+1))
  # revert + settle (NOT measured)
  do_edit "$FIX/$TRAIT_FILE" "$TRAIT_REPL" "$TRAIT_FIND" || true
  ( cd "$FIX" && CARGO_TARGET_DIR="$B_TGT" $APPROVE cargo check --locked --all-targets ) \
    > /tmp/m2-armb-settle-$i.log 2>&1 || true
  sleep "$INTER_EDIT_GAP"
done
say "ARM-B complete: $B_OK measured reps"

# ── ARM-C (optional): cold one-shot cargoless check per edit ─────────────
C_SAMPLES=""; C_OK=0
if [ "$RUN_ARM_C" = "1" ]; then
  say "ARM-C: cold one-shot 'cargoless check' per edit (synthetic option-2)"
  rm -rf "$FIX/.cargoless"
  ( cd "$FIX" && $APPROVE "$CL_BIN" check ) > /tmp/m2-armc-warm.log 2>&1 || true
  for i in $(seq 1 "$REPS"); do
    do_edit "$FIX/$TRAIT_FILE" "$TRAIT_FIND" "$TRAIT_REPL" \
      || { say "  ARM-C rep $i: anchor-not-found — aborting arm"; break; }
    tf=/tmp/m2-armc-time-$i.log
    ( cd "$FIX" && /usr/bin/time -v -o "$tf" $APPROVE "$CL_BIN" check ) \
      > /tmp/m2-armc-out-$i.log 2>&1
    u=$(awk -F': ' '/User time \(seconds\)/{print $2}' "$tf")
    s=$(awk -F': ' '/System time \(seconds\)/{print $2}' "$tf")
    ms=$(awk -v u="${u:-0}" -v s="${s:-0}" 'BEGIN{printf "%d", (u+s)*1000}')
    say "  ARM-C rep $i: user=${u}s sys=${s}s per-edit_cpu_ms=$ms"
    C_SAMPLES="${C_SAMPLES}${ms}"$'\n'; C_OK=$((C_OK+1))
    do_edit "$FIX/$TRAIT_FILE" "$TRAIT_REPL" "$TRAIT_FIND" || true
    sleep "$INTER_EDIT_GAP"
  done
  say "ARM-C complete: $C_OK measured reps"
fi

# ── summary ─────────────────────────────────────────────────────────────
# stats() — stdin: integers (one per line). Sort upstream so we don't
# rely on gawk's asort() (POSIX awk + mawk lack it).
stats() {
  sort -n | awk '{a[NR]=$1} END{
    if(NR==0){print "n=0 median=NA p5=NA p95=NA"; exit}
    med=a[int((NR+1)/2)];
    p5_i=int(NR*0.05)+1;  if(p5_i<1)  p5_i=1;  if(p5_i>NR)  p5_i=NR;
    p95_i=int(NR*0.95)+1; if(p95_i<1) p95_i=1; if(p95_i>NR) p95_i=NR;
    print "n="NR" median="med" p5="a[p5_i]" p95="a[p95_i]
  }'
}
A_STATS=$(printf '%s' "$A_SAMPLES" | sed '/^$/d' | stats)
B_STATS=$(printf '%s' "$B_SAMPLES" | sed '/^$/d' | stats)
C_STATS="(arm-C disabled)"
[ "$RUN_ARM_C" = "1" ] && C_STATS=$(printf '%s' "$C_SAMPLES" | sed '/^$/d' | stats)

A_MED=$(echo "$A_STATS" | sed -nE 's/.*median=([^ ]+).*/\1/p')
B_MED=$(echo "$B_STATS" | sed -nE 's/.*median=([^ ]+).*/\1/p')
RATIO="NA"
if [ "$A_MED" != "NA" ] && [ "$B_MED" != "NA" ] && [ "${B_MED:-0}" -gt 0 ]; then
  RATIO=$(awk -v a="$A_MED" -v b="$B_MED" 'BEGIN{printf "%.2f", a/b}')
fi

echo "============================================================"
echo "M2 PRE-DEPLOY CPU APPROXIMATION SUMMARY  sha=$REFTAG"
echo "  ARM-A cargoless WARM daemon       : $A_STATS"
echo "  ARM-B cold cargo-check / edit     : $B_STATS"
echo "  ARM-C cold cargoless check / edit : $C_STATS"
echo
echo "  RATIO_RESULT cargoless-A / cold-cargo-B = ${RATIO}x"
echo "  (>1 ⇒ cargoless costs MORE CPU per edit than cold cargo-check;"
echo "   <1 ⇒ cargoless wins on CPU too. RAM win (M1) is STRUCTURAL and"
echo "   independent of this ratio — Model R's load-bearing thesis is"
echo "   fleet-RAM flatness, not per-edit-CPU dominance vs every baseline.)"
echo
echo "  HONEST CAVEATS (carry into the report — methodology travels with"
echo "  the number, never just the number):"
echo "    1. SYNTHETIC: this brackets the cold-pod status quo; tf-multiverse's"
echo "       real K8s scripts/check-remote adds pod-scheduling + container"
echo "       init (~seconds per invocation) NOT synthesized here. The real"
echo "       gap is at least \${RATIO}× LARGER for arm-B's favour as a status"
echo "       quo (cargoless pays no pod-init cost; the cold pod does)."
echo "    2. arm-B's /target is shared+warm across edits (operator's mounted-"
echo "       volume pattern). A cold ephemeral pod would show LARGER arm-B"
echo "       numbers — this is the optimistic-for-status-quo end of the bracket."
echo "    3. arm-B has NO rust-analyzer cost (status quo doesn't run RA);"
echo "       arm-A includes RA in its per-edit work. The arms measure"
echo "       different shapes of work — same as AC7 §4 architectural-"
echo "       asymmetry. Treat ratio as 'what does cargoless cost per edit"
echo "       vs the rawer cold cargo-check baseline', not 'who is faster'."
echo "    4. fixture-dependent (bench/fixture Leptos honest-size); absolute"
echo "       ms differ on a larger workspace, but the ratio shape is"
echo "       informative as a bracket."
echo "============================================================"
echo "DONE_SENTINEL"
