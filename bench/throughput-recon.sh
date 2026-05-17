#!/usr/bin/env bash
# bench/throughput-recon.sh — Component 2 of the AC#7 throughput
# investigation: an INDEPENDENT second methodology cross-checking
# bench/throughput.py.
#
# Why a second methodology: lead's brief says two methodologies from
# the same measurer is still credible if each is documented
# independently. Different sampling code paths catch each-other's
# bugs. If Component 1 (Python /proc walk + per-edit sampling) and
# Component 2 (bash ps tree walk + fixed-interval sampling) converge
# on similar numbers, that's strong evidence the numbers reflect
# reality. If they diverge, we investigate which methodology is wrong
# BEFORE believing either.
#
# How this differs from Component 1 (bench/throughput.py):
#   * **Language**: bash + ps/awk vs Python — different parser path
#     for /proc data, different scheduling overhead during sampling.
#   * **Tree walk**: `ps --ppid` repeated breadth-first vs Python's
#     /proc/*/stat enumeration — different way to find descendants.
#   * **RSS source**: `ps -o rss=` (kernel resident-set field) vs
#     /proc/<pid>/statm column 2 — same data, different parser.
#   * **Edit driver**: `printf > file` direct overwrite (single
#     write(2) syscall) vs Python's open/write/fsync.
#   * **Sampling cadence**: fixed 5-second tick (regardless of
#     edit-cycle position) vs Python's per-edit snapshot — different
#     temporal alignment to the edit events.
#
# Output: one TPUT_RECON: line per tool, same shape as Component 1's
# TPUT_TOOL: lines so the report can table them side-by-side.

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"
fixture_src="$here/fixture"

# Defaults (overridable via env)
REPS=${REPS:-30}
INTER_EDIT_SEC=${INTER_EDIT_SEC:-10}
WARM_TIMEOUT_SEC=${WARM_TIMEOUT_SEC:-1200}
SAMPLE_TICK_SEC=${SAMPLE_TICK_SEC:-5}
CARGOLESS_BIN=${CARGOLESS_BIN:-$repo/target/release/tftrunk}
# Separate working copy + target dir for true isolation from Component 1.
RECON_SRC=${RECON_SRC:-/work/bench-lead-recon-src}
RECON_TARGET=${RECON_TARGET:-/cache/target-bench-lead-recon}

TARGET_REL="src/domain/model.rs"
ANCHOR='self.entries.len() /* BENCH_TRAIT_ANCHOR */'
FLIP_A='self.entries.len() /* BENCH_TRAIT_ANCHOR */ /* recon:a */'
FLIP_B='self.entries.len() /* BENCH_TRAIT_ANCHOR */ /* recon:b */'

hr() { printf '%s\n' "------------------------------------------------------------"; }
log() { echo "[recon] $*" >&2; }

usage() {
  cat <<EOF
USAGE: $0 <tool>...
where <tool> is one of: cargoless | trunk | bacon

Defaults (env-overridable):
  REPS=$REPS               INTER_EDIT_SEC=$INTER_EDIT_SEC
  WARM_TIMEOUT_SEC=$WARM_TIMEOUT_SEC  SAMPLE_TICK_SEC=$SAMPLE_TICK_SEC
  CARGOLESS_BIN=$CARGOLESS_BIN
  RECON_SRC=$RECON_SRC
  RECON_TARGET=$RECON_TARGET

Output: one TPUT_RECON: line per tool to stdout; live progress to stderr.
Exits 0 by design.
EOF
}

[ $# -gt 0 ] || { usage; exit 0; }
case "$1" in -h|--help|help) usage; exit 0 ;; esac

# ---------------------------------------------------------------------
# Set up isolated working copy + target dir
# ---------------------------------------------------------------------
log "preparing isolated working copy at $RECON_SRC (mirrors fixture content but separate inode)"
rm -rf "$RECON_SRC"
mkdir -p "$RECON_SRC"
cp -a "$fixture_src/." "$RECON_SRC/"
mkdir -p "$RECON_TARGET"
log "RECON_SRC=$RECON_SRC RECON_TARGET=$RECON_TARGET"

# Honest-size guard reasserted (same floor as run-comparative.sh)
rs_files=$(find "$RECON_SRC/src" -name '*.rs' 2>/dev/null | wc -l | tr -d ' ')
rs_loc=$(find "$RECON_SRC/src" -name '*.rs' -exec cat {} + 2>/dev/null | wc -l | tr -d ' ')
log "honest-size: ${rs_files} rust files, ${rs_loc} LOC"
if [ "${rs_files:-0}" -lt 12 ] || [ "${rs_loc:-0}" -lt 800 ]; then
  log "honest-size FAIL — refusing to report"
  echo "TPUT_RECON: name=ALL status=BLOCKED reason=honest-size-floor"
  exit 0
fi

# ---------------------------------------------------------------------
# Tool registry — bash arrays in lieu of structs
# ---------------------------------------------------------------------
# Returns argv (space-separated) for a given tool name. Quoted args
# in $CARGOLESS_BIN are not supported; if your path has spaces, fix it.
tool_argv() {
  case "$1" in
    cargoless) echo "$CARGOLESS_BIN watch" ;;
    trunk)     echo "trunk watch" ;;
    bacon)     echo "bacon --headless --job check" ;;
    *)         echo ""; return 1 ;;
  esac
}
# Returns extended-regex of "ready" substrings.
tool_ready_re() {
  case "$1" in
    cargoless) echo 'GREEN — tree compiles|GREEN — building|published ' ;;
    trunk)     echo 'success|applying new distribution' ;;
    # bacon 3.22.0 passes cargo's "Finished `dev`/`release`" completion
    # line + prints `error[`/"could not compile" on failure. It does NOT
    # emit literal "Success!"/"Warnings."/"Errors found". CRITICAL: bacon
    # emits ANSI color codes EVEN when piped (TUI framework, ignores
    # non-TTY), and the codes splice INTO the banner — raw bytes are
    # `ESC[1m ESC[32m    Finished ESC[0m \`dev\``. So a `Finished.*dev`
    # regex fails (the `ESC[0m ` is wedged in). The word "Finished" is
    # itself contiguous between `ESC[32m    ` and `ESC[0m`, so match the
    # BARE word — ANSI-safe. Same for cargo's `error[` (rustc emits it
    # contiguous). The grep below also runs the log through an ANSI
    # stripper for belt+suspenders.
    bacon)     echo 'Finished|could not compile|error\[' ;;
    *)         echo ""; return 1 ;;
  esac
}

# ---------------------------------------------------------------------
# Process-tree walker (ps-based, distinct from Component 1's /proc walk)
# ---------------------------------------------------------------------
pid_tree() {
  local root=$1
  local result="$root"
  local frontier="$root"
  while [ -n "$frontier" ]; do
    local next=""
    for p in $frontier; do
      local kids=$(ps -o pid= --ppid "$p" 2>/dev/null | tr '\n' ' ')
      [ -z "$kids" ] && continue
      result="$result $kids"
      next="$next $kids"
    done
    frontier="$next"
  done
  echo "$result"
}

# Sample the full pid tree, return: total_rss_kb total_cpu_jiffies alive_flag
sample_tree() {
  local root=$1
  local pids=$(pid_tree "$root")
  local total_rss=0
  local total_cpu_j=0
  local alive=0
  for pid in $pids; do
    if [ -d "/proc/$pid" ]; then
      alive=1
      # ps -o rss= prints RSS in KB (kernel-side same data as /proc/<pid>/statm)
      local rss=$(ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ')
      [ -n "$rss" ] && total_rss=$((total_rss + rss))
      # /proc/<pid>/stat fields 14+15 = utime, stime (jiffies)
      if [ -r "/proc/$pid/stat" ]; then
        local cpu=$(awk 'NR==1 {
          # comm field can contain spaces inside parens; slice past last ")"
          s=$0
          while (sub(/^[^)]*\) /, "", s) == 0) break
          n=split(s, f, " ")
          print f[12]+f[13]
        }' /proc/$pid/stat 2>/dev/null)
        [ -n "$cpu" ] && total_cpu_j=$((total_cpu_j + cpu))
      fi
    fi
  done
  echo "$total_rss $total_cpu_j $alive"
}

# ---------------------------------------------------------------------
# Edit driver: direct overwrite (single write(2)). printf > file is
# the editor-save shape every notify-rs watcher handles cleanly.
# ---------------------------------------------------------------------
flip_edit() {
  local rep=$1
  local target="$RECON_SRC/$TARGET_REL"
  local flip
  if [ $((rep % 2)) -eq 0 ]; then
    flip="$FLIP_A"
  else
    flip="$FLIP_B"
  fi
  # Use sed to substitute the anchor in-place; sed -i creates a temp
  # named target.XXXXXX (random suffix) and renames — empirically
  # works with cargoless's watcher (verified in #36 5th iteration
  # manual probe). NOT the same FS-event pattern as Component 1's
  # direct write — intentional methodology difference.
  sed -i "s|^.*BENCH_TRAIT_ANCHOR.*|        $flip|" "$target" 2>/dev/null
}

restore_edit() {
  local target="$RECON_SRC/$TARGET_REL"
  sed -i "s|^.*BENCH_TRAIT_ANCHOR.*|        $ANCHOR|" "$target" 2>/dev/null
}

# Sanity: make sure the anchor is present in the recon copy
ANCHOR_LINE=$(grep -c "BENCH_TRAIT_ANCHOR" "$RECON_SRC/$TARGET_REL" 2>/dev/null || echo 0)
[ "$ANCHOR_LINE" -gt 0 ] || {
  log "ERROR: anchor missing from recon source ($RECON_SRC/$TARGET_REL)"
  exit 1
}

# ---------------------------------------------------------------------
# Run a single tool: spawn, warm, sample, kill
# ---------------------------------------------------------------------
run_tool() {
  local name=$1
  local argv=$(tool_argv "$name")
  local ready_re=$(tool_ready_re "$name")
  [ -z "$argv" ] && { echo "TPUT_RECON: name=$name status=BAD-NAME"; return; }

  # Availability check
  local program=$(echo "$argv" | awk '{print $1}')
  if ! "$program" --version >/dev/null 2>&1 && ! command -v "$program" >/dev/null 2>&1; then
    echo "TPUT_RECON: name=$name status=UNAVAILABLE"
    return
  fi

  local logfile="/tmp/recon-${name}.log"
  : >"$logfile"
  log "--- tool: $name ---"
  log "argv: $argv"

  # Spawn in the recon dir, in its own session for clean group-kill
  ( cd "$RECON_SRC" && exec setsid $argv ) >"$logfile" 2>&1 &
  local pid=$!
  log "spawned pid=$pid"

  # Wait for warm via log grep
  local t0=$(date +%s)
  local warm_secs=$WARM_TIMEOUT_SEC
  while [ $(($(date +%s) - t0)) -lt $WARM_TIMEOUT_SEC ]; do
    # Strip ANSI CSI sequences before matching: bacon (TUI framework)
    # emits color codes even into a pipe and they splice into the
    # banner text, defeating a raw substring/regex match. `sed` ANSI
    # filter then grep — belt+suspenders with the bare-word patterns.
    if sed 's/\x1b\[[0-9;?]*[ -\/]*[@-~]//g' "$logfile" 2>/dev/null \
         | grep -qE "$ready_re" 2>/dev/null; then
      warm_secs=$(($(date +%s) - t0))
      break
    fi
    sleep 1
  done
  if [ "$warm_secs" -ge "$WARM_TIMEOUT_SEC" ]; then
    log "NO_READY after ${WARM_TIMEOUT_SEC}s"
    echo "TPUT_RECON: name=$name status=NO_READY warm_secs=$warm_secs"
    kill -9 -- "-$pid" 2>/dev/null || kill -9 "$pid" 2>/dev/null
    sleep 1
    return
  fi
  log "warm at ${warm_secs}s"

  # Settle
  sleep 2

  # Baseline sample
  local baseline=$(sample_tree "$pid")
  local baseline_rss=$(echo "$baseline" | awk '{print $1}')
  local baseline_cpu=$(echo "$baseline" | awk '{print $2}')
  log "baseline rss=${baseline_rss}kb cpu_j=${baseline_cpu}"

  local peak_rss=$baseline_rss
  local last_rss=$baseline_rss
  local last_cpu=$baseline_cpu
  local sample_count=0
  local meas_t0=$(date +%s)

  # The sampling tick runs INDEPENDENT of the edit cadence. We use a
  # background ticker that samples every $SAMPLE_TICK_SEC seconds and
  # appends to a CSV. After the rep loop completes we summarize from
  # the CSV — this catches CPU bursts BETWEEN edit-cycle samples
  # (Component 1 only samples at edit moments).
  local csv="/tmp/recon-${name}.csv"
  : >"$csv"
  ( while sleep "$SAMPLE_TICK_SEC"; do
      sn=$(sample_tree "$pid")
      sn_rss=$(echo "$sn" | awk '{print $1}')
      sn_cpu=$(echo "$sn" | awk '{print $2}')
      sn_alive=$(echo "$sn" | awk '{print $3}')
      printf '%s,%s,%s,%s\n' "$(date +%s)" "$sn_rss" "$sn_cpu" "$sn_alive" >>"$csv"
      [ "$sn_alive" = "0" ] && exit 0
    done
  ) &
  local ticker_pid=$!

  # Drive edit reps
  local rep
  for rep in $(seq 1 "$REPS"); do
    flip_edit "$rep"
    sleep "$INTER_EDIT_SEC"
    if [ $((rep % 5)) -eq 0 ] || [ "$rep" = "$REPS" ]; then
      local cur=$(sample_tree "$pid")
      local cur_rss=$(echo "$cur" | awk '{print $1}')
      local cur_cpu=$(echo "$cur" | awk '{print $2}')
      log "rep $rep/$REPS rss=${cur_rss}kb cpu_j=$cur_cpu"
      last_rss=$cur_rss
      last_cpu=$cur_cpu
      [ "$cur_rss" -gt "$peak_rss" ] && peak_rss=$cur_rss
    fi
  done

  local meas_t1=$(date +%s)
  local wall_secs=$((meas_t1 - meas_t0))

  # Stop ticker, restore fixture, kill tool
  kill "$ticker_pid" 2>/dev/null
  restore_edit
  kill -9 -- "-$pid" 2>/dev/null || kill -9 "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
  sleep 1

  # Compute summary using the CSV samples (more granular than per-rep)
  local csv_peak=$(awk -F, 'NR>0 {if ($2>m) m=$2} END {print m+0}' "$csv")
  [ "$csv_peak" -gt "$peak_rss" ] && peak_rss=$csv_peak

  local total_cpu_j=$((last_cpu - baseline_cpu))
  local clk_tck=$(getconf CLK_TCK 2>/dev/null || echo 100)
  local total_cpu_s=$(awk -v j="$total_cpu_j" -v t="$clk_tck" 'BEGIN{printf "%.2f", j/t}')
  local cpu_per_edit_s=$(awk -v j="$total_cpu_j" -v t="$clk_tck" -v r="$REPS" 'BEGIN{printf "%.3f", j/t/r}')
  local mean_cpu_pct=$(awk -v cs="$total_cpu_s" -v w="$wall_secs" 'BEGIN{
    if (w<=0) print "0.0"; else printf "%.1f", (cs/w)*100
  }')
  local rss_growth=$((last_rss - baseline_rss))

  echo "TPUT_RECON: name=$name reps=$REPS warm_secs=$warm_secs peak_rss_kb=$peak_rss rss_growth_kb=$rss_growth total_cpu_seconds=$total_cpu_s mean_cpu_pct=$mean_cpu_pct cpu_seconds_per_edit=$cpu_per_edit_s wall_secs=$wall_secs samples=$(wc -l <"$csv")"
}

# ---------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------
echo "=== bench/throughput-recon.sh (Component 2: independent ps/bash methodology) ==="
echo "config: reps=$REPS inter_edit_sec=$INTER_EDIT_SEC warm_timeout=$WARM_TIMEOUT_SEC sample_tick=$SAMPLE_TICK_SEC"
echo "isolated source: $RECON_SRC (separate inode from primary harness's tree)"
echo "isolated target: $RECON_TARGET (separate from CARGO_TARGET_DIR=$CARGO_TARGET_DIR)"
echo

# Use isolated CARGO_TARGET_DIR so we don't share cache with Component 1
export CARGO_TARGET_DIR="$RECON_TARGET"

for tool in "$@"; do
  hr
  run_tool "$tool"
done

hr
echo "=== recon run complete ==="
exit 0
