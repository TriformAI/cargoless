# shellcheck shell=bash
# scripts/stage1/lib.sh — shared machinery for the Stage-1 acceptance suite
# (PLAN-LANE D / task #228). Sourced by run-stage1.sh; never executed alone.
#
# This file holds: config (env-overridable defaults), portable OS helpers
# (macOS bash-3.2 + Linux), PASS/FAIL/STOP-class result accounting, the
# never-publish-red pointer fingerprint, the parity-oracle helpers, daemon
# lifecycle + rust-analyzer descendant discovery, ephemeral git-worktree
# setup, and the structural no-cargo / no-accidental-execute guards.
#
# DESIGN NOTE — the suite is authored against the v0.2.0 contract
# (`cargoless serve --repo`, GET-only HTTP+SSE, per-WT statusfile,
# never-publish-red pointer). Anything the Increment-0+1 wiring may move
# (pointer path, statusfile path, publish driver) is a NAMED env knob, so
# the suite is "ready to run the instant Inc0+1 land" by setting knobs,
# not by editing logic.

set -u

# ─────────────────────────────────────────────────────────────────────
# Config — every value env-overridable; safe, hermetic defaults.
# ─────────────────────────────────────────────────────────────────────
: "${CARGOLESS_BIN:=cargoless}"        # PATH binary; suite NEVER builds it
: "${S1_WORK:=/tmp/cl-stage1-run}"     # scratch root (must be under /tmp)
: "${S1_STATE_DIR:=${S1_WORK}/state}"  # dogfood-isolated state dir
: "${S1_CAS_DIR:=${S1_WORK}/cas}"      # dogfood-isolated CAS dir
: "${S1_BIND:=127.0.0.1:8717}"         # HTTP+SSE bind (AC6)
: "${S1_TOKEN:=}"                      # bearer; preflight mints if empty
: "${S1_BRINGUP_BUDGET:=30}"           # AC1 AC#1 budget seconds
: "${S1_VERDICT_GRACE:=30}"            # debounce+verdict latency window s
: "${S1_RESPAWN_GRACE:=40}"            # RA respawn settle window s (AC4)
: "${S1_SIGTERM_GRACE:=10}"            # post-SIGTERM reap window s (AC7)
: "${S1_NWT:=3}"                       # # ephemeral worktrees for AC3
: "${S1_CI_ORACLE:=0}"                 # 1 ⇒ also confirm baseline vs CI
: "${S1_CLIPPY_EXPECTED:=fieldfinding}" # red|green|fieldfinding (see AC2)
# Source repo: the suite clones THIS (hermetic) — never the operator tree.
: "${S1_SRC_REPO:=}"                   # auto-detected if empty
# Knobs Inc0+1 may relocate (NAMED so logic never changes):
: "${S1_POINTER_REL:=.cargoless/latest-green}"      # never-publish-red ptr
: "${S1_STATUS_GLOB:=.cargoless/**/cli-status .cargoless/cli-status}"
: "${S1_PUBLISH_ARGS:=build --watch --out}"          # publish driver (AC5)
# Injection target inside the cloned repo (a stable .rs file):
: "${S1_INJECT_FILE:=crates/cargoless/src/main.rs}"
: "${S1_INJECT_CRATE_FILE:=crates/cargoless-core/src/lib.rs}" # AC2 multicrate
: "${S1_EXECUTE_GO:=0}"                # STRUCTURAL gate — see require_go()

SUITE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS=()        # "ACn|PASS|detail" / "ACn|FAIL|detail"
FAILED=0
DAEMON_PID=""     # current serve daemon (for cleanup traps)

# ─────────────────────────────────────────────────────────────────────
# Output
# ─────────────────────────────────────────────────────────────────────
ts()  { date -u +%FT%TZ; }
log() { printf '[%s] %s\n' "$(ts)" "$*"; }
hdr() { printf '\n════════════════════════════════════════════\n%s\n════════════════════════════════════════════\n' "$*"; }
pass(){ RESULTS+=("$1|PASS|${2:-}"); log "[$1] ✅ PASS — ${2:-}"; }
fail(){ RESULTS+=("$1|FAIL|${2:-}"); FAILED=$((FAILED+1)); log "[$1] ❌ FAIL — ${2:-}"; }
note(){ log "[$1] · ${2:-}"; }

# STOP-class: an unrecoverable safety breach. Loud banner, sentinel file,
# distinct exit 99 ⇒ orchestrator aborts the whole rollout and the
# operator/team-lead route. The four triggers (false-GREEN /
# cross-contamination / torn-pointer / RA-orphan) are the bright line —
# never softened to a plain FAIL (STOP-guard structural-enforcement).
stop_class() {
  local ac="$1" reason="$2"
  RESULTS+=("$ac|STOP|$reason")
  printf '\n\n'
  printf '🛑🛑🛑 STOP-CLASS HALT 🛑🛑🛑\n'
  printf '  AC      : %s\n' "$ac"
  printf '  CLASS   : %s\n' "$reason"
  printf '  ACTION  : rollout HALTED — route to team-lead → dev-fixer.\n'
  printf '            DO NOT advance Stage-1. DO NOT proceed to Stage-2.\n\n'
  mkdir -p "$S1_WORK" 2>/dev/null || true
  { echo "STOP-CLASS HALT @ $(ts)"; echo "AC=$ac"; echo "CLASS=$reason"; } \
    > "$S1_WORK/STOP-CLASS-HALT.txt" 2>/dev/null || true
  cleanup
  exit 99
}

# ─────────────────────────────────────────────────────────────────────
# Portable OS helpers (macOS bash 3.2 + Linux; no bash-4 features)
# ─────────────────────────────────────────────────────────────────────
sha256_of() {  # file → bare sha256 hex
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}
finode() { if stat -c '%i' "$1" >/dev/null 2>&1; then stat -c '%i' "$1"; else stat -f '%i' "$1"; fi; }
fmtime() { if stat -c '%Y' "$1" >/dev/null 2>&1; then stat -c '%Y' "$1"; else stat -f '%m' "$1"; fi; }
fsize()  { if stat -c '%s' "$1" >/dev/null 2>&1; then stat -c '%s' "$1"; else stat -f '%z' "$1"; fi; }

# All transitive child PIDs of $1 (portable; ps is on macOS+Linux).
descendants() {
  local out="" frontier="$1" next
  while [ -n "$frontier" ]; do
    next=""
    for p in $frontier; do
      for c in $(ps -axo pid,ppid 2>/dev/null | awk -v P="$p" '$2==P{print $1}'); do
        out="$out $c"; next="$next $c"
      done
    done
    frontier="$next"
  done
  echo "$out"
}
# rust-analyzer + proc-macro-srv processes under a serve daemon (AC4/AC7).
ra_children() {
  local dpid="$1" kids; kids="$(descendants "$dpid")"
  [ -z "$kids" ] && return 0
  ps -axo pid,command 2>/dev/null | awk -v L="$kids" '
    BEGIN{n=split(L,a," ");for(i=1;i<=n;i++)s[a[i]]=1}
    ($1 in s) && (/rust-analyzer/ || /proc-macro-srv/) {print $1}'
}

# ─────────────────────────────────────────────────────────────────────
# Structural guards (the bright lines — never relaxed)
# ─────────────────────────────────────────────────────────────────────
# (1) No local cargo/rustc *invocation* anywhere in the suite (CI-only
#     ethos lock). Matches command invocations — `cargo <subcmd>` /
#     `rustc ` at a command position — NOT substrings/prose, so
#     `rustc-error`, the identifier `AC2-rustc`, and `cargoless check`
#     (the legit independent-instance oracle) never false-trip it. The
#     guard's own pattern-definition lines carry the S1_GUARD sentinel
#     and comment lines are excluded (self-reference safe).
guard_no_cargo() {                                                      # S1_GUARD
  local pat='(cargo (build|check|test|run|clippy|publish|fmt|metadata|install)|(^|[;&|]|\$\() *rustc )' # S1_GUARD
  if grep -nE "$pat" "$SUITE_DIR"/*.sh | grep -vE '^[^:]+:[0-9]+: *#| S1_GUARD'; then # S1_GUARD
    echo "FATAL: local cargo/rustc invocation in suite — CI-only ethos breach." >&2
    exit 2
  fi
}
# (2) Accidental-execution gate. The suite cannot run an AC unless the
#     operator/team-lead explicitly sets S1_EXECUTE_GO=1 *and* Inc0+1 is
#     declared landed. Default posture = self-check only. This is the
#     structural enforcement of "design+prep only, gated on GO".
require_go() {
  if [ "${S1_EXECUTE_GO}" != "1" ]; then
    cat <<EOF

GATED — Stage-1 execution is blocked by design.
  Requires ALL of:
    • Increment 0+1 landed (the central daemon + wiring)
    • explicit team-lead GO
    • S1_EXECUTE_GO=1 in the environment
  Until then this suite only runs --self-check (validates readiness,
  executes NO acceptance criterion, starts NO daemon, touches nothing).

  Re-run:   S1_EXECUTE_GO=1 $0 [--from ACn|--only ACn]
EOF
    exit 0
  fi
}

# ─────────────────────────────────────────────────────────────────────
# Preflight (runs only under require_go — i.e. only when executing)
# ─────────────────────────────────────────────────────────────────────
preflight() {
  guard_no_cargo
  case "$S1_WORK" in /tmp/*|/private/tmp/*) : ;; *)
    [ "${S1_FORCE:-0}" = 1 ] || { echo "REFUSE: S1_WORK ($S1_WORK) not under /tmp; set S1_FORCE=1 to override." >&2; exit 2; }
  esac
  # State-dir isolation: must live under S1_WORK (never the operator's
  # real .cargoless / .triform/cargoless) unless explicitly forced.
  case "$S1_STATE_DIR" in "$S1_WORK"/*) : ;; *)
    [ "${S1_FORCE:-0}" = 1 ] || { echo "REFUSE: S1_STATE_DIR not under S1_WORK — operator-state isolation breach; S1_FORCE=1 to override." >&2; exit 2; }
  esac
  command -v "$CARGOLESS_BIN" >/dev/null 2>&1 || { echo "FATAL: \$CARGOLESS_BIN ($CARGOLESS_BIN) not found — suite NEVER builds it." >&2; exit 2; }
  command -v curl >/dev/null 2>&1 || { echo "FATAL: curl required (AC6)." >&2; exit 2; }
  command -v git  >/dev/null 2>&1 || { echo "FATAL: git required (AC3 worktrees)." >&2; exit 2; }
  rm -rf "$S1_WORK"; mkdir -p "$S1_WORK" "$S1_STATE_DIR" "$S1_CAS_DIR"
  [ -n "$S1_TOKEN" ] || S1_TOKEN="s1-$(date +%s)-$RANDOM"
  if [ -z "$S1_SRC_REPO" ]; then
    S1_SRC_REPO="$(cd "$SUITE_DIR/../.." && git rev-parse --show-toplevel 2>/dev/null || true)"
  fi
  [ -n "$S1_SRC_REPO" ] && [ -d "$S1_SRC_REPO/.git" ] || { echo "FATAL: cannot resolve S1_SRC_REPO (the cargoless repo to clone hermetically)." >&2; exit 2; }
  log "preflight OK — bin=$($CARGOLESS_BIN --version 2>/dev/null || echo '?') src_repo=$S1_SRC_REPO work=$S1_WORK"
}

# Hermetic clone + N ephemeral worktrees of the cargoless repo itself.
# Local clone (no network, near-zero blast radius); the operator's
# checkout is never touched.
setup_repo() {
  S1_REPO="$S1_WORK/repo"
  git clone --local --no-hardlinks --quiet "$S1_SRC_REPO" "$S1_REPO"
  S1_WTS=()
  local i
  for i in $(seq 1 "$S1_NWT"); do
    local wt="$S1_WORK/wt$i"
    git -C "$S1_REPO" worktree add --quiet -b "s1-wt$i" "$wt" HEAD
    S1_WTS+=("$wt")
  done
  log "repo set up: $S1_REPO + ${#S1_WTS[@]} ephemeral worktrees"
}

# ─────────────────────────────────────────────────────────────────────
# Daemon lifecycle
# ─────────────────────────────────────────────────────────────────────
serve_start() {  # extra args... → exports DAEMON_PID, writes $S1_WORK/serve.out
  "$CARGOLESS_BIN" serve --repo "$S1_REPO" \
      --bind "$S1_BIND" --auth-token "$S1_TOKEN" \
      --state-dir "$S1_STATE_DIR" --cas-dir "$S1_CAS_DIR" "$@" \
      > "$S1_WORK/serve.out" 2>&1 &
  DAEMON_PID=$!
  log "serve --repo started pid=$DAEMON_PID bind=$S1_BIND"
}
serve_wait_up() {  # wait for the §3.3 banner (no verdict yet) up to budget
  local lim=$(( $(date +%s) + S1_BRINGUP_BUDGET ))
  while [ "$(date +%s)" -lt "$lim" ]; do
    grep -qiE 'repo-scoped (Model R )?daemon' "$S1_WORK/serve.out" 2>/dev/null && return 0
    kill -0 "$DAEMON_PID" 2>/dev/null || return 2   # died during bring-up
    sleep 1
  done
  return 1
}
serve_stop() {
  [ -n "$DAEMON_PID" ] || return 0
  kill -TERM "$DAEMON_PID" 2>/dev/null || true
  local lim=$(( $(date +%s) + S1_SIGTERM_GRACE ))
  while [ "$(date +%s)" -lt "$lim" ]; do kill -0 "$DAEMON_PID" 2>/dev/null || break; sleep 1; done
  kill -KILL "$DAEMON_PID" 2>/dev/null || true
  DAEMON_PID=""
}
cleanup() {
  [ -n "$DAEMON_PID" ] && kill -KILL "$DAEMON_PID" 2>/dev/null || true
  pkill -KILL -f "serve --repo $S1_REPO" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# ─────────────────────────────────────────────────────────────────────
# Per-worktree verdict + never-publish-red pointer
# ─────────────────────────────────────────────────────────────────────
# Resolve the per-WT statusfile then read its `verdict=` (schema=2).
wt_statusfile() {
  local wt="$1" g f
  for g in $S1_STATUS_GLOB; do
    for f in "$wt"/$g; do [ -f "$f" ] && { echo "$f"; return 0; }; done
  done
  return 1
}
wt_verdict() {  # wt → green|red|unknown
  local sf; sf="$(wt_statusfile "$1")" || { echo unknown; return; }
  awk -F= '/^verdict=/{print $2;exit}' "$sf" 2>/dev/null | tr -d '[:space:]' \
    | grep -qiE '^(green|red)$' && awk -F= '/^verdict=/{print tolower($2);exit}' "$sf" | tr -d '[:space:]' || echo unknown
}
wt_crates() {  # wt → the schema=2 `crates=` line (per-crate verdicts) or ""
  local sf; sf="$(wt_statusfile "$1")" || { echo ""; return; }
  awk -F= '/^crates=/{print $2;exit}' "$sf" 2>/dev/null
}
# Wait until $1's verdict == $2 (green|red), up to S1_VERDICT_GRACE.
wt_await_verdict() {
  local wt="$1" want="$2" lim=$(( $(date +%s) + S1_VERDICT_GRACE ))
  while [ "$(date +%s)" -lt "$lim" ]; do
    [ "$(wt_verdict "$wt")" = "$want" ] && return 0
    sleep 1
  done
  return 1
}
# Never-publish-red 4-tuple fingerprint (the proven AC#4/#5 invariant:
# sha256 + inode + mtime + size — a byte-unmoved pointer is identical on
# all four). Echoes "MISSING" if the pointer is absent.
ptr_path() { echo "${1%/}/$S1_POINTER_REL"; }
ptr_fp() {
  local p; p="$(ptr_path "$1")"
  [ -f "$p" ] || { echo "MISSING"; return; }
  echo "$(sha256_of "$p"):$(finode "$p"):$(fmtime "$p"):$(fsize "$p")"
}

# ─────────────────────────────────────────────────────────────────────
# Fault injection (operates ONLY inside the hermetic clone/worktrees)
# ─────────────────────────────────────────────────────────────────────
INJ_MARK_RUSTC='// S1-INJECT-RUSTC'
INJ_MARK_CLIPPY='// S1-INJECT-CLIPPY'
inject_rustc_error() {  # wt → appends a definite syntax/type error
  printf '\n%s\nlet __s1_bad =\n' "$INJ_MARK_RUSTC" >> "$1/$S1_INJECT_FILE"
}
revert_rustc_error() {  # precise revert by marker (no git needed)
  local f="$1/$S1_INJECT_FILE"
  sed -i.bak "/$INJ_MARK_RUSTC/,\$d" "$f" 2>/dev/null \
    || { sed "/$INJ_MARK_RUSTC/,\$d" "$f" > "$f.t" && mv "$f.t" "$f"; }
  rm -f "$f.bak"
}
inject_rustc_error_in_crate() {  # AC2 multicrate: error in cargoless-core
  printf '\n%s\nfn __s1_bad() -> u32 { "no" }\n' "$INJ_MARK_RUSTC" >> "$1/$S1_INJECT_CRATE_FILE"
}
revert_rustc_error_in_crate() {
  local f="$1/$S1_INJECT_CRATE_FILE"
  sed -i.bak "/$INJ_MARK_RUSTC/,\$d" "$f" 2>/dev/null \
    || { sed "/$INJ_MARK_RUSTC/,\$d" "$f" > "$f.t" && mv "$f.t" "$f"; }
  rm -f "$f.bak"
}
# Clippy-only: rustc-clean but `clippy -D warnings` red (unused import —
# CLAUDE.md heuristic). Whether cargoless's RA-flycheck verdict reflects
# this is the v0.2.0 contract question (only Severity::Error flips red);
# AC2 treats divergence per S1_CLIPPY_EXPECTED, NOT auto-STOP.
inject_clippy_only() {
  printf '\n%s\nuse std::collections::HashMap as _S1Unused;\n' "$INJ_MARK_CLIPPY" >> "$1/$S1_INJECT_FILE"
}
revert_clippy_only() {
  local f="$1/$S1_INJECT_FILE"
  sed -i.bak "/$INJ_MARK_CLIPPY/,\$d" "$f" 2>/dev/null \
    || { sed "/$INJ_MARK_CLIPPY/,\$d" "$f" > "$f.t" && mv "$f.t" "$f"; }
  rm -f "$f.bak"
}

# ─────────────────────────────────────────────────────────────────────
# Parity oracles
# ─────────────────────────────────────────────────────────────────────
# (b) Fast per-edit oracle: an INDEPENDENT cargoless instance (single-tree
#     `check`, NOT serve) on a *copy* of the worktree's exact tree state.
#     Returns green|red|unknown. This is a 2nd witness, never local cargo.
oracle_check_copy() {  # wt → verdict of an independent cargoless check
  local wt="$1" cp="$S1_WORK/oracle.$$"
  rm -rf "$cp"; cp -R "$wt" "$cp" 2>/dev/null || return 1
  ( cd "$cp" && "$CARGOLESS_BIN" check >/dev/null 2>&1 ); local rc=$?
  rm -rf "$cp"
  [ $rc -eq 0 ] && echo green || echo red
}
# (a) Authoritative-but-coarse oracle: the Forgejo CI verdict for a SHA
#     (per CLAUDE.md recipe). Baseline-confirm only (S1_CI_ORACLE=1).
ci_verdict_for_sha() {  # sha → success|failure|unknown
  local sha="$1" tok
  tok="$(printf 'protocol=https\nhost=forgejo.triform.dev\n\n' \
        | git credential fill 2>/dev/null | sed -n 's/^password=//p')"
  [ -n "$tok" ] || { echo unknown; return; }
  curl -s -H "Authorization: token $tok" \
    "https://forgejo.triform.dev/api/v1/repos/triform/cargoless/actions/tasks" 2>/dev/null \
    | tr ',' '\n' | grep -A4 "\"head_sha\":\"$sha\"" \
    | grep -oE '"status":"[a-z]+"' | head -1 | cut -d'"' -f4 || echo unknown
}
# Compare daemon per-WT verdict to the by-construction ground truth AND
# the independent-instance oracle. false-GREEN (daemon green where truth
# is red) is STOP-class; false-RED is a hard FAIL (not STOP).
assert_parity() {  # ac wt expected(green|red)
  local ac="$1" wt="$2" want="$3" got ora
  got="$(wt_verdict "$wt")"
  ora="$(oracle_check_copy "$wt")"
  if [ "$got" = "$want" ] && [ "$ora" = "$want" ]; then
    pass "$ac" "verdict=$got == truth=$want (independent oracle concurs)"
    return 0
  fi
  if [ "$want" = "red" ] && [ "$got" = "green" ]; then
    stop_class "$ac" "FALSE-GREEN: daemon=green but tree is definitively RED (oracle=$ora)"
  fi
  fail "$ac" "parity miss: daemon=$got oracle=$ora expected=$want"
  return 1
}
