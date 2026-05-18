#!/usr/bin/env bash
#
# bench/modelr-fleet.sh — Model R #15 measured Leg-C v4 fleet-RAM harness.
#
# WHY THIS EXISTS
#   AC7-THROUGHPUT-REPORT §11 (prior cycle) extrapolated Model-A fleet RAM
#   from a 1.02 GiB single-daemon footprint (~1.5 GiB/daemon × N ⇒ ~19-30
#   GiB @ 20). Model R's thesis is one rust-analyzer multiplexed across N
#   worktrees ⇒ ~FLAT ~1 GiB regardless of N. #15 replaces the
#   extrapolation with a MEASURED curve on the real wired daemon
#   (`cargoless serve --repo`, servedrv capstone-wire driver).
#
#   This is the integration-validation half of the closed correctness
#   chain (cores structurally-proven upstream; this proves the live
#   multiplexed runtime actually behaves — RAM AND per-WT verdict
#   isolation). It NEVER claims pure-unit-end-to-end proof.
#
# SANCTIONED BUILD VEHICLE
#   Runs IN the cargoless-builder pod (operator-authorised builder +
#   remote cargo, CLAUDE.md "OPERATOR AUTHORISATION"). A committed script
#   like bench/run.sh / scripts/ci-gate: the cargo invocation lives here,
#   not in a local command string. Operator-approved build escape is
#   honoured explicitly below.
#
# USAGE (from the pod, streamed tree at the gated SHA under $SRC):
#   SRC=/work/src bench/modelr-fleet.sh
#   NLIST="1 2 4 8 16 20"  WARM_SECS=45  SAMPLE_SECS=30  bench/modelr-fleet.sh
#
# OUTPUT
#   One `CELL N=<n> ...` line per fleet size + a FLEET_RESULT summary +
#   a DONE sentinel. distinct_ra is the LOAD-BEARING structural proof of
#   the one-multiplexed-RA thesis: expect 1 for a shared-Cargo.toml
#   cluster regardless of N. >1 (without a cluster reason) FALSIFIES the
#   flat-RAM headline — reported honestly, never massaged.
#
# HONEST CONTROLS (carried from the §11 cycle's hard lessons)
#   * RSS = Σ VmRSS over the serve pid + ALL recursive descendants
#     (the `kids()` recursion — NEVER pgid; the setsid-pgid bug).
#   * per-N graceful SIGTERM + orphan-verify (the SIGKILL-orphans-
#     cli-status #128 lesson) — never leave a daemon/RA across cells.
#   * AppleDouble `._*` stripped (silent substrate contaminant).
#   * RA absence ⇒ explicit FAIL, never a fabricated number.
#   * peak AND steady-avg over a post-warm window (active-fleet
#     resident footprint = the honest headline; idle-evict OFF by
#     default — that is a separate, disclosed lever).

set -u
export COPYFILE_DISABLE=1
export PATH="/usr/local/cargo/bin:${CARGO_HOME:-/cache/cargo}/bin:${PATH}"

SRC="${SRC:-/work/src}"
NLIST="${NLIST:-1 2 4 8 16 20}"
WARM_SECS="${WARM_SECS:-45}"
SAMPLE_SECS="${SAMPLE_SECS:-30}"
SETTLE_SECS="${SETTLE_SECS:-20}"
WORK="${WORK:-/tmp/mrfleet}"
MAXN="$(printf '%s\n' $NLIST | sort -n | tail -1)"

say() { echo "[modelr-fleet $(date -u +%H:%M:%S)] $*"; }
die() { echo "FLEET_RESULT FAIL :: $*"; echo "DONE_SENTINEL"; exit 1; }

# ── recursive descendant RSS (the validated method, NOT pgid) ───────────
kids() { echo "$1"; local c; for c in $(pgrep -P "$1" 2>/dev/null); do kids "$c"; done; }
tree_rss_kb() {
  local root=$1 tot=0 p r
  for p in $(kids "$root" | sort -u); do
    r=$(awk '/^VmRSS:/{print $2}' "/proc/$p/status" 2>/dev/null)
    tot=$((tot + ${r:-0}))
  done
  echo "$tot"
}
distinct_ra() {
  local root=$1 n=0 p
  for p in $(kids "$root" | sort -u); do
    grep -qa 'rust-analyzer' "/proc/$p/cmdline" 2>/dev/null && n=$((n+1))
  done
  echo "$n"
}
# v3: decompose the RA processes — the load-bearing mechanism check.
# ra_lsp = rust-analyzer language servers (cmdline has 'rust-analyzer'
#          but NOT 'proc-macro'); the THESIS = this is 1, constant
#          across N (one server multiplexed). ra_pmsrv = its
#          proc-macro-srv child(ren). N-scaling ra_lsp ⇒ thesis FALSE.
ra_decomp() {  # prints "lsp=<n> pmsrv=<n> other=<n>"
  local root=$1 lsp=0 pm=0 oth=0 p cl
  for p in $(kids "$root" | sort -u); do
    cl=$(tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null)
    case "$cl" in
      *proc-macro*) pm=$((pm+1)) ;;
      *rust-analyzer*) lsp=$((lsp+1)) ;;
      *) oth=$((oth+1)) ;;
    esac
  done
  echo "lsp=$lsp pmsrv=$pm other=$oth"
}
dump_tree() {  # full descendant (pid rss cmdline) — own-eyes process identity
  local root=$1 p cl rss
  for p in $(kids "$root" | sort -u); do
    cl=$(tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null)
    rss=$(awk '/^VmRSS:/{print $2}' "/proc/$p/status" 2>/dev/null)
    printf '    pid=%-7s rss=%-9s %s\n' "$p" "${rss:-?}kB" "${cl:0:150}"
  done
}
zombie_ra_count() {  # surviving defunct/live rust-analyzer anywhere (post-reap)
  local z l
  z=$(ps -eo pid,stat,comm 2>/dev/null | awk '$2 ~ /Z/ && $3 ~ /rust-analyz/' | wc -l)
  l=$(pgrep -x rust-analyzer 2>/dev/null | wc -l)
  echo "defunct=$z live_orphan=$l"
}

say "SRC=$SRC NLIST=[$NLIST] MAXN=$MAXN WARM=${WARM_SECS}s SAMPLE=${SAMPLE_SECS}s"
[ -d "$SRC/crates/cargoless" ] || die "no cargoless crate under SRC=$SRC"
[ -d "$SRC/bench/fixture/src" ] || die "no bench/fixture under SRC=$SRC"

# ── rust-analyzer presence (the headline IS RA-resident RAM) ────────────
RA_BIN="$(command -v rust-analyzer || true)"
if [ -z "$RA_BIN" ]; then
  say "rust-analyzer not on PATH — installing via rustup component (operator-approved)"
  TRIFORM_OPERATOR_APPROVED_BUILD=1 rustup component add rust-analyzer >/dev/null 2>&1 || true
  RA_BIN="$(command -v rust-analyzer || true)"
  [ -z "$RA_BIN" ] && RA_BIN="$(rustc --print sysroot 2>/dev/null)/bin/rust-analyzer"
fi
[ -x "$RA_BIN" ] || die "rust-analyzer unavailable in builder image — fleet-RAM (RA-resident) is unmeasurable here; NOT fabricating a number"
export PATH="$(dirname "$RA_BIN"):$PATH"
say "rust-analyzer = $RA_BIN"

# ── build the real wired daemon ────────────────────────────────────────
# PER-REF ISOLATED target dir (the ci-gate 74b04df discipline, applied to
# bench/): the pod pins a SHARED CARGO_TARGET_DIR=/cache/target that
# accumulates stale binaries from every other agent's ci-gate. Building +
# selecting there risks measuring an unprovenanced binary (the v1 #15 bug:
# a stale debug #3-park-skeleton got picked). A ref-keyed private target
# kills the ambiguity entirely — we measure ONLY what we just built here.
REFTAG="$(cd "$SRC" && git rev-parse --short HEAD 2>/dev/null || echo "$(date +%s)")"
CTGT="/tmp/mrf-tgt-${REFTAG}"
say "per-ref isolated CARGO_TARGET_DIR=$CTGT (shared /cache/target NOT used)"
BUILD_START=$(date +%s)
say "building cargoless (release) @ streamed tree ..."
( cd "$SRC" && CARGO_TARGET_DIR="$CTGT" TRIFORM_OPERATOR_APPROVED_BUILD=1 \
  cargo build -p cargoless --release --locked ) \
  > /tmp/mrfleet-build.log 2>&1 || { tail -30 /tmp/mrfleet-build.log; die "cargoless build failed"; }
BIN="$CTGT/release/cargoless"
[ -x "$BIN" ] || die "release binary absent at $BIN post-build (build broken)"
# Provenance guard: the binary MUST be newer than build-start — never
# measure a stale/foreign artifact (the v1 #15 root-cause, now impossible
# with a private target dir, asserted belt-and-braces).
BIN_MTIME=$(stat -c %Y "$BIN" 2>/dev/null || echo 0)
[ "$BIN_MTIME" -ge "$BUILD_START" ] \
  || die "binary mtime $BIN_MTIME < build-start $BUILD_START — stale/unprovenanced, refusing to measure"
say "cargoless = $BIN  mtime=$BIN_MTIME (≥ build-start $BUILD_START ✓ fresh)  ($("$BIN" --version 2>/dev/null | head -1))"

# ── fixture fleet: ONE shared-Cargo.toml cluster, MAXN worktrees ────────
rm -rf "$WORK"; mkdir -p "$WORK"
REPO="$WORK/repo"
cp -a "$SRC/bench/fixture" "$REPO"
find "$REPO" -name '._*' -type f -delete 2>/dev/null
( cd "$REPO" && git init -q && git config user.email b@e && git config user.name b \
  && git add -A && git commit -qm "fixture fleet base" ) || die "git init fixture failed"
for k in $(seq 1 "$MAXN"); do
  git -C "$REPO" worktree add -q -b "wt$k" "$WORK/wt$k" HEAD 2>/dev/null \
    || die "git worktree add wt$k failed"
  find "$WORK/wt$k" -name '._*' -type f -delete 2>/dev/null
done
say "fleet ready: 1 base repo + $MAXN worktrees (shared Cargo.toml/Cargo.lock = one cluster)"

clean_state() {
  find "$REPO" "$WORK"/wt* -maxdepth 2 -name '.cargoless' -o -name '.triform' 2>/dev/null \
    | xargs -r rm -rf 2>/dev/null
}
reap() {
  local pid=$1
  [ -n "$pid" ] || return 0
  kill -TERM "$pid" 2>/dev/null
  for _ in $(seq 1 20); do kill -0 "$pid" 2>/dev/null || break; sleep 0.5; done
  kill -0 "$pid" 2>/dev/null && { kill -KILL "$pid" 2>/dev/null; sleep 1; }
  # orphan-verify: no surviving rust-analyzer from our tree
  pkill -KILL -f "$REPO" 2>/dev/null || true
}

echo "=== MODELR-FLEET BUILD OK $(date -u +%FT%TZ) bin=$("$BIN" --version 2>/dev/null|tr -d '\n') ==="

touch_wts() {  # trigger routed-change activity in base + first $1 worktrees
  echo "// fleet-activate $(date -u +%s%N)" >> "$REPO/src/main.rs"
  for k in $(seq 1 "$1"); do
    echo "// fleet-activate $(date -u +%s%N) wt$k" >> "$WORK/wt$k/src/main.rs"
  done
}

for N in $NLIST; do
  clean_state
  say "CELL N=$N start"
  "$BIN" serve --repo "$REPO" > "/tmp/mrfleet-serve-$N.log" 2>&1 &
  SPID=$!
  # let discovery+classification settle (serve log shows ~0.01s; be generous)
  sleep 8
  kill -0 "$SPID" 2>/dev/null || { echo "CELL N=$N RESULT=FAIL reason=serve-died-pre-activity"; tail -20 "/tmp/mrfleet-serve-$N.log"; reap "$SPID"; continue; }
  # ACTIVITY-FIRST (v1 fix): the capstone-wire servedrv spawns the
  # per-cluster RA on a ROUTED FILE-CHANGE, not at startup. Trigger
  # activity in base + N worktrees, THEN wait for RA — the reverse
  # ordering was the v1 ra-never-spawned bug (waited for RA before
  # causing the activity that spawns it).
  touch_wts "$N"
  warmed=0; ra_spawn_s=-1
  for s in $(seq 1 "$WARM_SECS"); do
    sleep 1
    kill -0 "$SPID" 2>/dev/null || { say "serve died during warm (N=$N)"; tail -20 "/tmp/mrfleet-serve-$N.log"; break; }
    # keep nudging activity every 10s in case the first batch debounced
    [ $((s % 10)) -eq 0 ] && touch_wts "$N"
    if [ "$(distinct_ra "$SPID")" -ge 1 ]; then warmed=1; ra_spawn_s=$s; break; fi
  done
  if [ "$warmed" -ne 1 ]; then
    echo "CELL N=$N RESULT=FAIL reason=ra-never-spawned-within-${WARM_SECS}s-post-activity"
    tail -8 "/tmp/mrfleet-serve-$N.log"
    reap "$SPID"; continue
  fi
  say "CELL N=$N: RA spawned at +${ra_spawn_s}s post-activity"
  # all N worktrees active; let the one multiplexed RA index + settle
  touch_wts "$N"
  sleep "$SETTLE_SECS"
  # sample peak + avg over the steady window
  peak=0; sum=0; cnt=0; ra_seen=0
  mid=$(( SAMPLE_SECS / 2 )); [ "$mid" -lt 1 ] && mid=1
  for s in $(seq 1 "$SAMPLE_SECS"); do
    kill -0 "$SPID" 2>/dev/null || break
    r=$(tree_rss_kb "$SPID"); d=$(distinct_ra "$SPID")
    [ "$r" -gt "$peak" ] && peak=$r
    sum=$((sum + r)); cnt=$((cnt + 1))
    [ "$d" -gt "$ra_seen" ] && ra_seen=$d
    # v3: CONCURRENT process-identity capture — same live daemon
    # instance as the RSS sample (eliminates the probe-reconciliation
    # gap). Decompose + full cmdline tree at mid-window.
    if [ "$s" -eq "$mid" ]; then
      decomp=$(ra_decomp "$SPID")
      echo "CELL N=$N MIDSAMPLE ra_decomp=[$decomp] (lsp=1 constant-across-N ⇒ one-multiplexed-RA MECHANISM-confirmed; lsp scaling-with-N ⇒ thesis FALSE)"
      echo "CELL N=$N PROCTREE:"
      dump_tree "$SPID"
    fi
    sleep 1
  done
  avg=0; [ "$cnt" -gt 0 ] && avg=$((sum / cnt))
  peak_mib=$((peak / 1024)); avg_mib=$((avg / 1024))
  echo "CELL N=$N RESULT=OK peak_kb=$peak avg_kb=$avg peak_MiB=$peak_mib avg_MiB=$avg_mib distinct_ra=$ra_seen ra_decomp=[${decomp:-NA}] ra_spawn_s=$ra_spawn_s samples=$cnt"
  reap "$SPID"
  sleep 3
  echo "CELL N=$N POSTREAP $(zombie_ra_count) (zombie/orphan-RA-on-shutdown characterization — FIELD-FINDING-#3b/#128 class)"
done

echo "=== MODELR-FLEET COMPLETE $(date -u +%FT%TZ) ==="
echo "FLEET_RESULT DONE :: parse CELL lines above (distinct_ra=1 across N ⇒ one-multiplexed-RA thesis MEASURED-confirmed)"
echo "DONE_SENTINEL"
