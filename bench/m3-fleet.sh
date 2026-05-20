#!/usr/bin/env bash
#
# bench/m3-fleet.sh — M3 driver: spin a Model R `cargoless serve --repo
# --bind` daemon, invoke bench/m3-roundtrip.py against it at one or
# more fleet scales, reap cleanly between scales.
#
# WHY
#   The M3 measurement (per fluffy-dreaming-allen.md / Lane C #222):
#   measured p50/p95/p99 latency for the full push round-trip, split
#   transport-vs-RA, at N=1 (single WT) and N=20 (multi-WT — exposes
#   the daemon's per-WT state cardinality and corun batching pressure).
#
# USAGE (cargoless-builder pod, streamed tree under $SRC):
#   SRC=/work/src bench/m3-fleet.sh
#   NLIST="1 20" REPS=50 PORT=8080 bench/m3-fleet.sh
#
# OUTPUT
#   Per-scale: M3_RESULT block (p50/p95/p99 for transport / ra / total)
#   plus a DONE_SENTINEL. Inherits the modelr-fleet.sh discipline:
#   per-ref CARGO_TARGET_DIR, mtime provenance guard, graceful reap with
#   orphan-verify.
set -u
export COPYFILE_DISABLE=1
export PATH="/usr/local/cargo/bin:${CARGO_HOME:-/cache/cargo}/bin:${PATH}"

SRC="${SRC:-/work/src}"
NLIST="${NLIST:-1 20}"
REPS="${REPS:-50}"
PORT="${PORT:-8080}"
INTER_REP_GAP="${INTER_REP_GAP:-1.0}"
VERDICT_TIMEOUT="${VERDICT_TIMEOUT:-60}"
WARMUP_TIMEOUT="${WARMUP_TIMEOUT:-300}"
WORK="${WORK:-/tmp/m3fleet}"
MAXN="$(printf '%s\n' $NLIST | sort -n | tail -1)"

say() { echo "[m3-fleet $(date -u +%H:%M:%S)] $*"; }
die() { echo "M3_RESULT FAIL :: $*"; echo "DONE_SENTINEL"; exit 1; }

[ -d "$SRC/crates/cargoless" ] || die "no cargoless crate under SRC=$SRC"
[ -d "$SRC/bench/fixture/src" ] || die "no bench/fixture under SRC=$SRC"
[ -f "$SRC/bench/m3-roundtrip.py" ] || die "no bench/m3-roundtrip.py under SRC=$SRC"
command -v cargo >/dev/null 2>&1 || die "cargo not on PATH"
command -v python3 >/dev/null 2>&1 || die "python3 not on PATH"
command -v curl >/dev/null 2>&1 || die "curl not on PATH (used for /healthz readiness)"

# ── rust-analyzer presence (the verdict tier consumes it) ──────────────
RA_BIN="$(command -v rust-analyzer || true)"
if [ -z "$RA_BIN" ]; then
  say "rust-analyzer not on PATH — installing via rustup (operator-approved)"
  TRIFORM_OPERATOR_APPROVED_BUILD=1 rustup component add rust-analyzer >/dev/null 2>&1 || true
  RA_BIN="$(command -v rust-analyzer || true)"
fi
[ -x "$RA_BIN" ] || die "rust-analyzer unavailable — push→verdict cycle is unmeasurable without it"
export PATH="$(dirname "$RA_BIN"):$PATH"
say "rust-analyzer = $RA_BIN"

# ── build cargoless (per-ref isolated target — modelr-fleet discipline) ──
REFTAG="$(cd "$SRC" && git rev-parse --short HEAD 2>/dev/null || echo "$(date +%s)")"
CTGT="/tmp/m3-cl-tgt-${REFTAG}"
say "per-ref isolated CARGO_TARGET_DIR=$CTGT"
BUILD_START=$(date +%s)
say "building cargoless (release) ..."
( cd "$SRC" && CARGO_TARGET_DIR="$CTGT" TRIFORM_OPERATOR_APPROVED_BUILD=1 \
  cargo build -p cargoless --release --locked ) > /tmp/m3-build.log 2>&1 \
  || { tail -30 /tmp/m3-build.log; die "cargoless build failed"; }
BIN="$CTGT/release/cargoless"
[ -x "$BIN" ] || die "cargoless binary missing at $BIN"
BIN_MTIME=$(stat -c %Y "$BIN" 2>/dev/null || echo 0)
[ "$BIN_MTIME" -ge "$BUILD_START" ] || die "binary mtime $BIN_MTIME < build-start $BUILD_START — stale"
say "cargoless = $BIN  mtime=$BIN_MTIME ($("$BIN" --version 2>/dev/null | head -1))"

# ── fleet directory: base repo + MAXN sibling worktrees ────────────────
rm -rf "$WORK"; mkdir -p "$WORK"
REPO="$WORK/repo"
cp -a "$SRC/bench/fixture" "$REPO"
find "$REPO" -name '._*' -type f -delete 2>/dev/null
( cd "$REPO" && git init -q && git config user.email b@e && git config user.name b \
  && git add -A && git commit -qm "fixture fleet base" ) || die "git init failed"
for k in $(seq 1 "$MAXN"); do
  git -C "$REPO" worktree add -q -b "wt$k" "$WORK/wt$k" HEAD 2>/dev/null \
    || die "git worktree add wt$k failed"
  find "$WORK/wt$k" -name '._*' -type f -delete 2>/dev/null
done
say "fleet ready: base repo + $MAXN worktrees"

# ── helpers ────────────────────────────────────────────────────────────
kids() { echo "$1"; local c; for c in $(pgrep -P "$1" 2>/dev/null); do kids "$c"; done; }
reap() {
  local pid=$1
  [ -n "$pid" ] || return 0
  kill -TERM "$pid" 2>/dev/null
  for _ in $(seq 1 30); do kill -0 "$pid" 2>/dev/null || break; sleep 0.5; done
  kill -0 "$pid" 2>/dev/null && { kill -KILL "$pid" 2>/dev/null; sleep 1; }
  pkill -KILL -f "$REPO" 2>/dev/null || true
}

# Compose the comma-separated worktree-key list the Python harness consumes
# (canonical absolute paths — what the daemon's discover() indexes by).
wt_list() {
  local n=$1 out=""
  for k in $(seq 1 "$n"); do
    [ -n "$out" ] && out="${out},"
    out="${out}$WORK/wt$k"
  done
  echo "$out"
}

# ── per-scale measurement loop ─────────────────────────────────────────
echo "=== M3-FLEET BUILD OK $(date -u +%FT%TZ) bin=$("$BIN" --version 2>/dev/null|tr -d '\n') ==="

for N in $NLIST; do
  say "==== SCALE N=$N start ===="
  ADDR="127.0.0.1:$PORT"
  say "spawning serve --repo $REPO --bind $ADDR"
  "$BIN" serve --repo "$REPO" --bind "$ADDR" > "/tmp/m3-serve-$N.log" 2>&1 &
  SPID=$!

  # Wait for /healthz=200 (the #225 0d readiness latch)
  say "waiting for /healthz ready (cap 30s)"
  ready=0
  for _ in $(seq 1 60); do
    if [ -z "$(kill -0 "$SPID" 2>/dev/null; echo $?)" ] || ! kill -0 "$SPID" 2>/dev/null; then
      tail -20 "/tmp/m3-serve-$N.log"
      die "serve died during /healthz wait (N=$N)"
    fi
    code=$(curl -s -o /dev/null -w '%{http_code}' "http://${ADDR}/healthz" 2>/dev/null || true)
    if [ "$code" = "200" ]; then ready=1; break; fi
    sleep 0.5
  done
  [ "$ready" -eq 1 ] || { reap "$SPID"; tail -20 "/tmp/m3-serve-$N.log"; die "daemon /healthz never went 200 (N=$N)"; }
  say "daemon ready (pid=$SPID, /healthz=200)"

  # Hand off to the Python harness
  WTS="$(wt_list "$N")"
  py_log="/tmp/m3-py-$N.log"
  say "running m3-roundtrip.py (N=$N reps=$REPS) → $py_log"
  python3 "$SRC/bench/m3-roundtrip.py" \
    --base-url "http://${ADDR}" \
    --worktrees "$WTS" \
    --fixture-path "$WORK/wt1" \
    --reps "$REPS" \
    --inter-rep-gap "$INTER_REP_GAP" \
    --verdict-timeout "$VERDICT_TIMEOUT" \
    --warmup-timeout "$WARMUP_TIMEOUT" \
    --scale-label "N=$N" 2>&1 | tee "$py_log"
  py_ec=${PIPESTATUS[0]}
  say "m3-roundtrip.py exit=$py_ec"

  # Reap daemon between scales (orphan-verify per #128 lesson)
  reap "$SPID"
  sleep 2
  z=$(ps -eo pid,stat,comm 2>/dev/null | awk '$2 ~ /Z/ && $3 ~ /rust-analyz/' | wc -l)
  l=$(pgrep -x rust-analyzer 2>/dev/null | wc -l)
  say "post-reap N=$N: defunct=$z live_orphan=$l"
  echo
done

echo "=== M3-FLEET COMPLETE $(date -u +%FT%TZ) ==="
echo "DONE_SENTINEL"
