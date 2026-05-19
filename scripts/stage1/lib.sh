# shellcheck shell=bash
# scripts/stage1/lib.sh — shared machinery for the Stage-1 acceptance suite
# (PLAN-LANE D / tasks #228, #239). Sourced by run-stage1.sh; never run alone.
#
# SUBSTRATE (task #239): the Stage-1 suite validates the wired `cargoless
# serve --repo` daemon against a REAL Rust+WASM project — the committed
# Leptos reference fixture `bench/fixture/`. cargoless is a build tool FOR
# Leptos/trunk WASM apps; its own native-CLI source is NOT a recognisable
# Rust+WASM project (cargoless's D7 detection gates on cdylib-or-leptos),
# so cargoless cannot dogfood its own repo — that was the substrate error
# the first run surfaced. The suite builds a throwaway git repo whose
# content IS the Leptos fixture and discovers nested worktrees of it.
#
# This file holds: config, portable OS helpers (macOS bash-3.2 + Linux),
# PASS / FAIL / STOP-class accounting, the never-publish-red pointer
# fingerprint, parity oracles, daemon lifecycle + rust-analyzer descendant
# discovery, the Leptos-substrate builder, and the structural guards.

set -u

# ─────────────────────────────────────────────────────────────────────
# Config — every value env-overridable; safe, hermetic defaults.
# ─────────────────────────────────────────────────────────────────────
: "${CARGOLESS_BIN:=cargoless}"        # wired (#225) binary; suite NEVER builds it
: "${S1_WORK:=/tmp/cl-stage1-run}"     # scratch root (must be under /tmp)
: "${S1_STATE_DIR:=${S1_WORK}/state}"  # dogfood-isolated daemon state dir
: "${S1_CAS_DIR:=${S1_WORK}/cas}"      # dogfood-isolated CAS dir
: "${S1_BIND:=127.0.0.1:8717}"         # HTTP+SSE bind (AC6)
: "${S1_TOKEN:=}"                      # bearer; preflight mints if empty
: "${S1_BRINGUP_BUDGET:=30}"           # AC1 AC#1 budget seconds
: "${S1_VERDICT_GRACE:=180}"           # verdict-await window s (pod cold-leptos
                                       # runs override higher, e.g. 480)
: "${S1_RESPAWN_GRACE:=60}"            # RA respawn settle window s (AC4)
: "${S1_SIGTERM_GRACE:=12}"            # post-SIGTERM reap window s (AC7)
: "${S1_NWT:=3}"                       # # ephemeral nested worktrees (AC3)
: "${S1_CI_ORACLE:=0}"                 # 1 ⇒ also confirm baseline vs Forgejo CI
# AC2 clippy-only expectation — ERA-SCOPED (Lane-B #221 ruling, verified
# from source). Stage-1 runs PRE-Increment-3-B; shipped v0.2.0 flycheck is
# hardcoded `command:"check"` (clippy is NOT a flycheck): a warning-
# severity lint (the unused-import AC2 injects) is advisory/suppressed ⇒
# cargoless verdict GREEN — CORRECT shipped behaviour, NOT a bug; asserting
# RED here would be a false-alarm. A clippy/rustc *error* (Severity::Error)
# ⇒ RED. ⇒ default green for the Stage-1 era; FLIPS to red once Inc3-B
# lands (clippy-as-flycheck + `-D warnings` promotes the lint to Error).
# dev-fixer owns final post-Inc3-B semantics.
: "${S1_CLIPPY_EXPECTED:=green}"       # red|green|fieldfinding — era-scoped
# Substrate: the cargoless repo (to locate the fixture + the suite) and
# the Leptos fixture inside it. The suite builds a throwaway repo FROM the
# fixture — the operator tree / cargoless repo are never mutated.
: "${S1_SRC_REPO:=}"                   # cargoless repo; auto-detected if empty
: "${S1_FIXTURE:=}"                    # Leptos fixture; defaults to <repo>/bench/fixture
# Named contract knobs (v0.2.0 verified-from-source):
: "${S1_STATUS_REL:=.cargoless/cli-status}"     # per-WT verdict statusfile
: "${S1_POINTER_REL:=.cargoless/latest-green}"  # never-publish-red pointer
: "${S1_PUBLISH_ARGS:=build --watch --out}"      # AC5 publisher driver
: "${S1_INJECT_FILE:=src/components/counter.rs}" # fixture file fault-injected
: "${S1_EXECUTE_GO:=0}"                # STRUCTURAL gate — see require_go()

SUITE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS=()        # "ACn|PASS|detail" / "ACn|FAIL|detail" / "ACn|STOP|detail"
FAILED=0
DAEMON_PID=""     # current serve daemon (for cleanup traps)
S1_REPO=""        # the throwaway Leptos substrate repo (set by setup_repo)

# ─────────────────────────────────────────────────────────────────────
# Output
# ─────────────────────────────────────────────────────────────────────
ts()  { date -u +%FT%TZ; }
log() { printf '[%s] %s\n' "$(ts)" "$*"; }
hdr() { printf '\n════════════════════════════════════════════\n%s\n════════════════════════════════════════════\n' "$*"; }
pass(){ RESULTS+=("$1|PASS|${2:-}"); log "[$1] ✅ PASS — ${2:-}"; }
fail(){ RESULTS+=("$1|FAIL|${2:-}"); FAILED=$((FAILED+1)); log "[$1] ❌ FAIL — ${2:-}"; }
note(){ log "[$1] · ${2:-}"; }

# STOP-class: an unrecoverable PRODUCT safety breach. Loud banner, sentinel
# file, distinct exit 99 ⇒ rollout HALTS and team-lead routes.
#
# BUG-#3 DISCIPLINE (the verdict-provenance principle — task #239): a
# STOP-class HALT is ONLY ever raised on a *definite wrong verdict* — a
# `green` verdict on a definitely-broken tree (false-GREEN), a definite
# cross-worktree verdict flip, a torn pointer, an RA-orphan, an auth
# bypass. It is NEVER raised on `unknown` (verdict unobservable). "Could
# not observe a verdict" is INCONCLUSIVE → a plain FAIL, never a STOP.
# Conflating `unknown` with `green` is exactly the harness false-positive
# the first run produced; callers must gate STOP on `== green`, never on
# `!= red`.
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
#     `rustc ` at a command position — NOT substrings/prose; the guard's
#     own pattern lines carry the S1_GUARD sentinel (self-reference safe).
guard_no_cargo() {                                                      # S1_GUARD
  local pat='(cargo (build|check|test|run|clippy|publish|fmt|metadata|install)|(^|[;&|]|\$\() *rustc )' # S1_GUARD
  if grep -nE "$pat" "$SUITE_DIR"/*.sh | grep -vE '^[^:]+:[0-9]+: *#| S1_GUARD'; then # S1_GUARD
    echo "FATAL: local cargo/rustc invocation in suite — CI-only ethos breach." >&2
    exit 2
  fi
}
# (2) Accidental-execution gate. No AC runs unless S1_EXECUTE_GO=1 is
#     explicitly set (team-lead's GO). Default posture = self-check only.
require_go() {
  if [ "${S1_EXECUTE_GO}" != "1" ]; then
    cat <<EOF

GATED — Stage-1 execution is blocked by design.
  Requires explicit team-lead GO: S1_EXECUTE_GO=1 in the environment.
  Without it this suite only runs --self-check (validates readiness,
  executes NO acceptance criterion, starts NO daemon, touches nothing).

  Re-run:   S1_EXECUTE_GO=1 $0 [--from ACn|--only ACn]
EOF
    exit 0
  fi
}

# ─────────────────────────────────────────────────────────────────────
# Preflight
# ─────────────────────────────────────────────────────────────────────
preflight() {
  guard_no_cargo
  case "$S1_WORK" in /tmp/*|/private/tmp/*) : ;; *)
    [ "${S1_FORCE:-0}" = 1 ] || { echo "REFUSE: S1_WORK ($S1_WORK) not under /tmp; set S1_FORCE=1 to override." >&2; exit 2; }
  esac
  case "$S1_STATE_DIR" in "$S1_WORK"/*) : ;; *)
    [ "${S1_FORCE:-0}" = 1 ] || { echo "REFUSE: S1_STATE_DIR not under S1_WORK — operator-state isolation breach; S1_FORCE=1 to override." >&2; exit 2; }
  esac
  command -v "$CARGOLESS_BIN" >/dev/null 2>&1 || { echo "FATAL: \$CARGOLESS_BIN ($CARGOLESS_BIN) not found — suite NEVER builds it." >&2; exit 2; }
  command -v curl >/dev/null 2>&1 || { echo "FATAL: curl required (AC6)." >&2; exit 2; }
  command -v git  >/dev/null 2>&1 || { echo "FATAL: git required (worktrees)." >&2; exit 2; }
  rm -rf "$S1_WORK"; mkdir -p "$S1_WORK" "$S1_STATE_DIR" "$S1_CAS_DIR"
  [ -n "$S1_TOKEN" ] || S1_TOKEN="s1-$(date +%s)-$RANDOM"
  if [ -z "$S1_SRC_REPO" ]; then
    S1_SRC_REPO="$(cd "$SUITE_DIR/../.." && git rev-parse --show-toplevel 2>/dev/null || true)"
  fi
  [ -n "$S1_SRC_REPO" ] && [ -d "$S1_SRC_REPO/.git" ] || { echo "FATAL: cannot resolve S1_SRC_REPO (the cargoless repo)." >&2; exit 2; }
  [ -n "$S1_FIXTURE" ] || S1_FIXTURE="$S1_SRC_REPO/bench/fixture"
  # The substrate MUST be a recognisable Rust+WASM project, else cargoless
  # refuses it (D7 detection) — the exact failure the first run hit.
  [ -f "$S1_FIXTURE/Cargo.toml" ] || { echo "FATAL: S1_FIXTURE ($S1_FIXTURE) has no Cargo.toml." >&2; exit 2; }
  grep -q 'leptos' "$S1_FIXTURE/Cargo.toml" 2>/dev/null \
    || { echo "FATAL: S1_FIXTURE is not a Leptos project (no leptos dep) — cargoless would D7-refuse it." >&2; exit 2; }
  [ -f "$S1_FIXTURE/$S1_INJECT_FILE" ] || { echo "FATAL: inject target $S1_FIXTURE/$S1_INJECT_FILE missing." >&2; exit 2; }
  log "preflight OK — bin=$($CARGOLESS_BIN --version 2>/dev/null || echo '?') fixture=$S1_FIXTURE work=$S1_WORK"
}

# Build the Leptos substrate: a throwaway git repo whose content IS the
# `bench/fixture/` Leptos app, plus N worktrees NESTED under repo_root.
#
# BUG-#4 FIX (task #239): the v0.2.0 wired daemon installs ONE file-watcher
# rooted at repo_root (servedrv `raw_repo_watch(&repo_root)`); a sibling
# worktree's edits are never observed. The first run placed worktrees as
# siblings ⇒ zero activity ⇒ zero verdicts. Worktrees here are NESTED
# under `$S1_REPO/.s1-wt/` so the single raw watcher sees their edits —
# matching D-FLEET-SHARED-DAEMON §4's dominant `.claude/worktrees/`
# topology. `.s1-wt/` is gitignored so the base RA workspace skips it
# (the raw watcher is unfiltered ⇒ still sees the nested edits — the §4
# gitignore-inversion).
setup_repo() {
  S1_REPO="$S1_WORK/repo"
  mkdir -p "$S1_REPO"
  cp -R "$S1_FIXTURE"/. "$S1_REPO"/
  printf '/.s1-wt/\n/.cargoless/\n/target/\n/dist/\n' > "$S1_REPO/.gitignore"
  local G=(-c user.email=stage1@cargoless.local -c user.name=stage1)
  git -C "$S1_REPO" init -q
  git -C "$S1_REPO" "${G[@]}" add -A
  git -C "$S1_REPO" "${G[@]}" commit -q -m "stage1 leptos substrate"
  S1_WTS=()
  local i wt
  for i in $(seq 1 "$S1_NWT"); do
    wt="$S1_REPO/.s1-wt/wt$i"     # NESTED under repo_root (bug-#4 fix)
    git -C "$S1_REPO" "${G[@]}" worktree add -q -b "s1-wt$i" "$wt" HEAD
    S1_WTS+=("$wt")
  done
  log "leptos substrate: $S1_REPO (from $S1_FIXTURE) + ${#S1_WTS[@]} NESTED worktrees"
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
serve_wait_up() {  # wait for the §3.3 bring-up banner up to budget
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
  [ -n "$S1_REPO" ] && pkill -KILL -f "serve --repo $S1_REPO" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# ─────────────────────────────────────────────────────────────────────
# Per-worktree verdict + never-publish-red pointer
# ─────────────────────────────────────────────────────────────────────
# Per-WT statusfile = `<wt>/.cargoless/cli-status` — the EXACT v0.2.0 path
# (`statusfile::path()` verified from source). BUG-#1 FIX (task #239): the
# prior `**`-glob resolver silently relied on `globstar` (off by default)
# ⇒ never recursed. The path is exact — no glob needed.
wt_statusfile() {
  local f="${1%/}/$S1_STATUS_REL"
  [ -f "$f" ] && { echo "$f"; return 0; }
  return 1
}
wt_verdict() {  # wt → green|red|unknown  (`unknown` = unobservable, NOT green)
  local sf; sf="$(wt_statusfile "$1")" || { echo unknown; return; }
  local v; v="$(awk -F= '/^verdict=/{print tolower($2);exit}' "$sf" 2>/dev/null | tr -d '[:space:]')"
  case "$v" in green|red) echo "$v" ;; *) echo unknown ;; esac
}
wt_crates() {  # wt → the schema=2 `crates=` value or ""
  local sf; sf="$(wt_statusfile "$1")" || { echo ""; return; }
  awk -F= '/^crates=/{print $2;exit}' "$sf" 2>/dev/null
}
# Wait until $1's verdict == $2 (green|red), up to S1_VERDICT_GRACE.
wt_await_verdict() {
  local wt="$1" want="$2" lim=$(( $(date +%s) + S1_VERDICT_GRACE ))
  while [ "$(date +%s)" -lt "$lim" ]; do
    [ "$(wt_verdict "$wt")" = "$want" ] && return 0
    sleep 2
  done
  return 1
}
# Activity trigger: Model R is activity-driven — an idle worktree is never
# checked. Append a trailing newline (content-neutral for Rust: stays
# green) to produce a watcher event so the daemon activates + checks it.
wt_activate() { printf '\n' >> "$1/$S1_INJECT_FILE"; }

# Never-publish-red 4-tuple fingerprint (the AC#4/#5 invariant: sha256 +
# inode + mtime + size — a byte-unmoved pointer is identical on all four).
ptr_path() { echo "${1%/}/$S1_POINTER_REL"; }
ptr_fp() {
  local p; p="$(ptr_path "$1")"
  [ -f "$p" ] || { echo "MISSING"; return; }
  echo "$(sha256_of "$p"):$(finode "$p"):$(fmtime "$p"):$(fsize "$p")"
}

# ─────────────────────────────────────────────────────────────────────
# Fault injection (operates ONLY inside the throwaway substrate worktrees)
# ─────────────────────────────────────────────────────────────────────
# BUG-#2 FIX (task #239): revert is `git checkout --` (the worktree is a
# git checkout) — deterministic, exact. The prior sed-by-marker revert
# embedded `//`-prefixed markers that collided with sed's `/` delimiter
# (`sed: char 3: unknown command /`) ⇒ reverts silently failed.
inject_rustc_error() {  # wt → a definite module-scope syntax error ⇒ RED
  printf '\n// S1-INJECT-RUSTC\nlet __s1_bad =\n' >> "$1/$S1_INJECT_FILE"
}
# Clippy-only: a DELIBERATELY warning-severity lint (unused import). Under
# shipped v0.2.0 flycheck (plain `check`, Severity::Error-only) it is
# suppressed ⇒ cargoless GREEN — the correct pre-Inc3-B verdict (#221).
inject_clippy_only() {
  printf '\n// S1-INJECT-CLIPPY\nuse std::collections::HashMap as _S1Unused;\n' >> "$1/$S1_INJECT_FILE"
}
revert_inject() {  # wt → restore the inject target exactly (git checkout)
  git -C "$1" checkout -- "$S1_INJECT_FILE" 2>/dev/null
}

# ─────────────────────────────────────────────────────────────────────
# Parity oracles
# ─────────────────────────────────────────────────────────────────────
# (b) Fast per-edit oracle: an INDEPENDENT cargoless instance (single-tree
#     `check`, NOT serve) on a *copy* of the worktree's exact tree state.
#     Returns green|red. A 2nd witness — never local cargo.
oracle_check_copy() {  # wt → verdict of an independent cargoless check
  local wt="$1" cp="$S1_WORK/oracle.$$"
  rm -rf "$cp"; cp -R "$wt" "$cp" 2>/dev/null || { echo unknown; return; }
  rm -rf "$cp/.git" "$cp/.cargoless" "$cp/target"   # clean slate for the check
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
# Compare the daemon's per-WT verdict to the by-construction ground truth
# AND the independent-instance oracle.
#   • false-GREEN (daemon == green, truth == red)  → STOP-class.
#   • false-RED   (daemon == red,   truth == green) → hard FAIL (not STOP).
#   • unknown (verdict unobservable)                → hard FAIL (not STOP).
# BUG-#3 DISCIPLINE: STOP fires ONLY on an exact `green`, never on `!= red`.
assert_parity() {  # ac wt expected(green|red)
  local ac="$1" wt="$2" want="$3" got ora
  got="$(wt_verdict "$wt")"
  ora="$(oracle_check_copy "$wt")"
  if [ "$got" = "$want" ] && [ "$ora" = "$want" ]; then
    pass "$ac" "verdict=$got == truth=$want (independent oracle concurs)"
    return 0
  fi
  if [ "$want" = "red" ] && [ "$got" = "green" ]; then
    stop_class "$ac" "FALSE-GREEN: daemon=green but the tree is definitively RED (oracle=$ora)"
  fi
  if [ "$got" = "unknown" ]; then
    fail "$ac" "INCONCLUSIVE: daemon verdict unobservable (oracle=$ora expected=$want) — not a false-GREEN"
  else
    fail "$ac" "parity miss: daemon=$got oracle=$ora expected=$want"
  fi
  return 1
}
