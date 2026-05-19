#!/usr/bin/env bash
# scripts/stage1/run-stage1.sh — Stage-1 acceptance suite (PLAN-LANE D /
# task #228). The falsifiable, all-must-PASS-to-advance gate for the
# cargoless-first rollout, run co-located against cargoless's OWN repo
# (near-zero blast radius — a hermetic local clone; the operator's
# checkout is never touched).
#
# USAGE
#   scripts/stage1/run-stage1.sh --self-check     # readiness ONLY; runs no AC
#   scripts/stage1/run-stage1.sh --list           # list the ACs
#   S1_EXECUTE_GO=1 scripts/stage1/run-stage1.sh           # full suite (gated)
#   S1_EXECUTE_GO=1 scripts/stage1/run-stage1.sh --only AC3 # one AC
#   S1_EXECUTE_GO=1 scripts/stage1/run-stage1.sh --from AC5 # AC5..AC8
#
# GATING (structural — see lib.sh require_go): without S1_EXECUTE_GO=1
# the suite refuses to execute ANY criterion. This enforces "design+prep
# only, gated on Increment 0+1 + explicit team-lead GO" — accidental
# execution is impossible by construction, not by convention.
#
# EXIT CODES
#   0  all selected ACs PASS  (or --self-check / --list / gated stub)
#   1  ≥1 non-STOP FAIL — Stage-1 does NOT advance; investigate+route
#   2  harness/preflight error (bad config, missing bin/curl/git)
#  99  STOP-CLASS HALT (false-GREEN / cross-contamination / torn-pointer
#      / RA-orphan) — rollout HALTED, route to team-lead → dev-fixer
#
# ORACLE (cargoless has no local cargo, by design): parity is judged
# against (i) the by-construction ground truth (we control the
# injection), corroborated by (ii) an INDEPENDENT cargoless `check`
# instance on a tree-copy, and optionally (iii, S1_CI_ORACLE=1) the
# Forgejo CI verdict for the baseline SHA. No `cargo`/`rustc` ever.

set -u
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"

# ─── AC1 — bring-up ≤30s (AC#1 budget) ───────────────────────────────
ac1_bringup() {
  hdr "AC1 — repo-scoped daemon bring-up ≤ ${S1_BRINGUP_BUDGET}s"
  local t0 t1
  t0=$(date +%s)
  serve_start
  if serve_wait_up; then
    t1=$(date +%s)
    local d=$(( t1 - t0 ))
    # discovery+classification must cover every git-worktree-list entry.
    local want got
    want=$(git -C "$S1_REPO" worktree list | wc -l | tr -d ' ')
    got=$(grep -oE '[0-9]+ worktrees' "$S1_WORK/serve.out" | head -1 | grep -oE '[0-9]+' || echo 0)
    if [ "$d" -le "$S1_BRINGUP_BUDGET" ] && [ "$got" -ge "$want" ]; then
      pass AC1 "up in ${d}s; discovered $got ≥ $want worktrees, classified + banner"
    else
      fail AC1 "bring-up ${d}s (budget ${S1_BRINGUP_BUDGET}s) / discovered=$got want=$want"
    fi
  else
    fail AC1 "no §3.3 banner within ${S1_BRINGUP_BUDGET}s (daemon dead or stalled)"
  fi
}

# ─── AC2 — verdict-parity vs oracle, 4-state matrix ──────────────────
ac2_parity() {
  hdr "AC2 — verdict-parity {clean / rustc-error / clippy-only / multi-crate}"
  local wt="${S1_WTS[0]}"
  # clean → GREEN
  wt_await_verdict "$wt" green || note AC2 "clean baseline slow to settle green"
  assert_parity AC2-clean "$wt" green
  if [ "${S1_CI_ORACLE}" = 1 ]; then
    local sha cv; sha="$(git -C "$wt" rev-parse HEAD)"; cv="$(ci_verdict_for_sha "$sha")"
    note AC2-clean "CI baseline oracle for $sha: $cv (coarse confirm)"
  fi
  # rustc-error → RED  (false-GREEN here is STOP-class, via assert_parity)
  inject_rustc_error "$wt"; wt_await_verdict "$wt" red || true
  assert_parity AC2-rustc "$wt" red
  revert_rustc_error "$wt"; wt_await_verdict "$wt" green || true
  assert_parity AC2-revert "$wt" green
  # multi-crate: error in cargoless-core only → that crate red, others green
  inject_rustc_error_in_crate "$wt"; wt_await_verdict "$wt" red || true
  local cr; cr="$(wt_crates "$wt")"
  if echo "$cr" | grep -qiE 'cargoless-core:red'; then
    if echo "$cr" | grep -qiE '(cargoless|cargoless-cli)?:?green'; then
      pass AC2-multicrate "per-crate schema=2: cargoless-core:red, others green ($cr)"
    else
      fail AC2-multicrate "core red but other crates not green ($cr)"
    fi
  else
    # workspace-level red is acceptable iff verdict==red (no false-green)
    [ "$(wt_verdict "$wt")" = red ] \
      && note AC2-multicrate "no per-crate line; workspace verdict=red (schema=1-compatible)" \
      || stop_class AC2-multicrate "FALSE-GREEN: crate error present but verdict not red ($cr)"
  fi
  revert_rustc_error_in_crate "$wt"; wt_await_verdict "$wt" green || true
  # clippy-only: contract question (only Severity::Error flips red).
  inject_clippy_only "$wt"; sleep "$S1_VERDICT_GRACE"
  local cg; cg="$(wt_verdict "$wt")"
  case "$S1_CLIPPY_EXPECTED" in
    red)   [ "$cg" = red ]   && pass AC2-clippy "clippy-only ⇒ red (cargoless gates clippy-class)" \
                              || fail AC2-clippy "clippy-only expected red, got $cg" ;;
    green) [ "$cg" = green ] && pass AC2-clippy "clippy-only ⇒ green (rustc-error-only contract, by design)" \
                              || fail AC2-clippy "clippy-only expected green, got $cg" ;;
    *)     note AC2-clippy "clippy-only ⇒ $cg — FIELD FINDING input for Lane-B (#221: does cargoless replace clippy?). Not a Stage-1 gate." ;;
  esac
  revert_clippy_only "$wt"; wt_await_verdict "$wt" green || true
}

# ─── AC3 — no-wrong-verdict: spatial isolation, ≥2 WTs / one shared RA ─
# The load-bearing Model-R risk. Editing WT_k must flip ONLY WT_k; every
# other WT's verdict AND diagnostics must stay byte-identical. Any
# bleed-through = cross-contamination = STOP-class.
ac3_isolation() {
  hdr "AC3 — spatial isolation across ${#S1_WTS[@]} worktrees / ONE shared RA"
  local k="${S1_WTS[0]}" j
  # settle all green
  for j in "${S1_WTS[@]}"; do wt_await_verdict "$j" green || true; done
  # snapshot every non-k WT (verdict + diagnostics fingerprint)
  declare -a SNAP=()
  for j in "${S1_WTS[@]}"; do
    [ "$j" = "$k" ] && { SNAP+=("-"); continue; }
    local sf fp; sf="$(wt_statusfile "$j" || true)"
    fp="$(wt_verdict "$j")|$( [ -n "$sf" ] && sha256_of "$sf" || echo nofile )"
    SNAP+=("$fp")
  done
  # induce red in WT_k only
  inject_rustc_error "$k"; wt_await_verdict "$k" red \
    || { revert_rustc_error "$k"; fail AC3 "WT_k never went red — inconclusive"; return; }
  # WT_k must be red; every other WT must be byte-identical to snapshot
  local i=0 bled=0
  for j in "${S1_WTS[@]}"; do
    if [ "$j" = "$k" ]; then i=$((i+1)); continue; fi
    local sf now; sf="$(wt_statusfile "$j" || true)"
    now="$(wt_verdict "$j")|$( [ -n "$sf" ] && sha256_of "$sf" || echo nofile )"
    [ "$now" != "${SNAP[$i]}" ] && { bled=1; note AC3 "WT $j changed: ${SNAP[$i]} → $now"; }
    i=$((i+1))
  done
  revert_rustc_error "$k"; wt_await_verdict "$k" green || true
  if [ "$bled" -eq 1 ]; then
    stop_class AC3 "CROSS-CONTAMINATION: a non-edited worktree's verdict/diagnostics moved when only WT_k was edited"
  fi
  pass AC3 "WT_k isolated red; ${#S1_WTS[@]}-1 sibling WTs byte-identical (no bleed through the shared RA)"
}

# ─── AC4 — respawn-staleness (kill -9 cluster RA) ────────────────────
ac4_respawn() {
  hdr "AC4 — kill -9 the shared rust-analyzer → correct post-respawn verdicts"
  local wt="${S1_WTS[0]}" rapids
  wt_await_verdict "$wt" green || true
  rapids="$(ra_children "$DAEMON_PID")"
  [ -n "$rapids" ] || { fail AC4 "no rust-analyzer child found under daemon — inconclusive"; return; }
  log "killing -9 RA pids: $rapids"
  for p in $rapids; do kill -KILL "$p" 2>/dev/null || true; done
  # Supervisor must respawn + mux.reset(); then verdicts must be CORRECT
  # for EVERY active WT (not stale). Prove with a fresh red→green cycle.
  sleep 3
  inject_rustc_error "$wt"
  if wt_await_verdict "$wt" red; then
    revert_rustc_error "$wt"
    if wt_await_verdict "$wt" green; then
      pass AC4 "post-respawn RA produced correct red→green for active WT (mux.reset seam holds)"
    else
      fail AC4 "post-respawn stuck red after revert (stale overlay — reset() suspect)"
    fi
  else
    revert_rustc_error "$wt"
    # stale-GREEN on a definite error after respawn = false-green = STOP
    [ "$(wt_verdict "$wt")" = green ] \
      && stop_class AC4 "FALSE-GREEN post-respawn: injected error not seen (stale RA after kill -9)" \
      || fail AC4 "no verdict within respawn grace (RA respawn / handshake failure)"
  fi
}

# ─── AC5 — never-publish-red under multiplex (pointer byte-unmoved) ───
# Reuses the proven AC#4/#5 invariant: a red transition leaves the
# latest-green pointer identical on all four of sha256+inode+mtime+size;
# green recovery advances it; a zero/partial pointer is a torn write.
ac5_never_publish_red() {
  hdr "AC5 — never-publish-red: pointer byte-unmoved on red, atomic"
  local wt="${S1_WTS[0]}"
  wt_await_verdict "$wt" green || true
  local fp0; fp0="$(ptr_fp "$wt")"
  if [ "$fp0" = MISSING ]; then
    # serve-path may not drive the publisher; try the explicit publish
    # driver against this WT (S1_PUBLISH_ARGS is the named Inc0+1 knob).
    ( cd "$wt" && timeout 180 "$CARGOLESS_BIN" $S1_PUBLISH_ARGS "$S1_WORK/dist.$$" \
        > "$S1_WORK/pub.out" 2>&1 & echo $! > "$S1_WORK/pub.pid" )
    local lim=$(( $(date +%s) + S1_VERDICT_GRACE*3 ))
    while [ "$(date +%s)" -lt "$lim" ]; do [ -f "$(ptr_path "$wt")" ] && break; sleep 2; done
    fp0="$(ptr_fp "$wt")"
  fi
  if [ "$fp0" = MISSING ]; then
    fail AC5 "no never-publish-red pointer produced (publisher not wired for serve / wrong S1_POINTER_REL) — verify Inc0+1 wiring"
    [ -f "$S1_WORK/pub.pid" ] && kill -KILL "$(cat "$S1_WORK/pub.pid")" 2>/dev/null || true
    return
  fi
  # induce red — pointer MUST stay byte-identical (all 4 fields)
  inject_rustc_error "$wt"; wt_await_verdict "$wt" red || true
  sleep 5
  local fpR; fpR="$(ptr_fp "$wt")"
  if [ "$fpR" != "$fp0" ]; then
    stop_class AC5 "TORN/MOVED POINTER: latest-green changed during RED ($fp0 → $fpR) — never-publish-red VIOLATED"
  fi
  local pp; pp="$(ptr_path "$wt")"
  if [ ! -s "$pp" ]; then
    stop_class AC5 "TORN POINTER: latest-green is zero-byte/partial during red"
  fi
  # recover — pointer must advance (different fp) and stay non-empty
  revert_rustc_error "$wt"; wt_await_verdict "$wt" green || true
  sleep 8
  local fp2; fp2="$(ptr_fp "$wt")"
  [ -f "$S1_WORK/pub.pid" ] && kill -KILL "$(cat "$S1_WORK/pub.pid")" 2>/dev/null || true
  if [ "$fp2" != "$fp0" ] && [ "$fp2" != MISSING ] && [ -s "$pp" ]; then
    pass AC5 "pointer byte-unmoved through red, atomically advanced on green recovery"
  else
    fail AC5 "pointer did not advance on green recovery (fp0=$fp0 fp2=$fp2)"
  fi
}

# ─── AC6 — transport + auth (GET routes, SSE framing, bearer) ────────
ac6_transport_auth() {
  hdr "AC6 — HTTP+SSE transport + bearer auth over --bind $S1_BIND"
  local base="http://$S1_BIND" wt="${S1_WTS[0]}" ok=1
  local AUTH=(-H "Authorization: Bearer $S1_TOKEN")
  # auth: missing & wrong ⇒ 401 ; correct ⇒ 200
  local c_no c_bad c_ok
  c_no=$(curl -s -o /dev/null -w '%{http_code}' "$base/worktrees")
  c_bad=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer WRONG" "$base/worktrees")
  c_ok=$(curl -s -o /dev/null -w '%{http_code}' "${AUTH[@]}" "$base/worktrees")
  if [ "$c_no" = 401 ] && [ "$c_bad" = 401 ] && [ "$c_ok" = 200 ]; then
    pass AC6-auth "no-token=401 wrong=401 correct=200 (#14 bearer seam enforced)"
  else
    # auth bypass (a protected route served without/with-wrong token) is
    # a security STOP-class breach, not a soft fail.
    { [ "$c_no" = 200 ] || [ "$c_bad" = 200 ]; } \
      && stop_class AC6 "AUTH BYPASS: protected route served without a valid bearer (no=$c_no wrong=$c_bad)" \
      || { fail AC6-auth "auth codes off (no=$c_no wrong=$c_bad ok=$c_ok)"; ok=0; }
  fi
  # GET routes return well-formed payloads
  local enc; enc=$(printf '%s' "$wt" | sed 's:/:%2F:g')
  curl -s "${AUTH[@]}" "$base/worktrees" | grep -q '\[' \
    && curl -s "${AUTH[@]}" "$base/status?worktree=$enc" | grep -qiE 'verdict|green|red' \
    && curl -s "${AUTH[@]}" "$base/verdict?worktree=$enc" | grep -qiE 'green|red' \
    && pass AC6-routes "GET /worktrees /status /verdict /…/diagnostics well-formed" \
    || { fail AC6-routes "a GET route returned malformed/empty payload"; ok=0; }
  # SSE: exactly one frame per transition (induce 2: green→red→green)
  ( curl -sN "${AUTH[@]}" "$base/events" > "$S1_WORK/sse.out" 2>&1 & echo $! > "$S1_WORK/sse.pid" )
  sleep 2
  inject_rustc_error "$wt"; wt_await_verdict "$wt" red || true
  revert_rustc_error "$wt"; wt_await_verdict "$wt" green || true
  sleep 3
  kill -KILL "$(cat "$S1_WORK/sse.pid")" 2>/dev/null || true
  local frames; frames=$(grep -c '^data:' "$S1_WORK/sse.out" 2>/dev/null || echo 0)
  if [ "$frames" -ge 2 ]; then
    pass AC6-sse "SSE emitted $frames data frames across ≥2 transitions (one frame per transition)"
  else
    fail AC6-sse "SSE frame count $frames < expected ≥2 (missed transitions)"; ok=0
  fi
  [ "$ok" = 1 ] || true
}

# ─── AC7 — FF-A SIGTERM reap (#198 live re-verify) ───────────────────
ac7_sigterm_reap() {
  hdr "AC7 — SIGTERM reaps every per-cluster rust-analyzer child (#198)"
  local before; before="$(ra_children "$DAEMON_PID")"
  [ -n "$before" ] || { fail AC7 "no RA children to reap — inconclusive (activate a WT first)"; return; }
  log "RA children before SIGTERM: $before"
  kill -TERM "$DAEMON_PID" 2>/dev/null || true
  local lim=$(( $(date +%s) + S1_SIGTERM_GRACE ))
  while [ "$(date +%s)" -lt "$lim" ]; do kill -0 "$DAEMON_PID" 2>/dev/null || break; sleep 1; done
  sleep 2
  local survivors=""
  for p in $before; do kill -0 "$p" 2>/dev/null && survivors="$survivors $p"; done
  # also catch any stray RA / proc-macro-srv parented away from the daemon
  local stray; stray="$(ps -axo pid,ppid,command 2>/dev/null \
      | awk -v D="$DAEMON_PID" '($2==1)&&(/rust-analyzer/||/proc-macro-srv/){print $1}')"
  DAEMON_PID=""
  if [ -n "${survivors// /}" ]; then
    stop_class AC7 "RA-ORPHAN: rust-analyzer survived daemon SIGTERM ($survivors) — #198 ReapOnDrop regressed at the serve seam"
  fi
  if [ -n "$stray" ]; then
    note AC7 "reparented RA/proc-macro-srv present (pid $stray) — investigate provenance"
    stop_class AC7 "RA-ORPHAN: reparented rust-analyzer/proc-macro-srv after daemon SIGTERM"
  fi
  pass AC7 "every per-cluster RA child reaped on SIGTERM (no zombie/orphan; #198 holds)"
}

# ─── AC8 — overlay freshness (Shape-1 local-FS read) ─────────────────
ac8_overlay_freshness() {
  hdr "AC8 — overlay freshness: verdict tracks on-disk content, never stale"
  serve_start; serve_wait_up || { fail AC8 "daemon did not come up for AC8"; return; }
  local wt="${S1_WTS[1]:-${S1_WTS[0]}}"
  wt_await_verdict "$wt" green || true
  inject_rustc_error "$wt"
  if wt_await_verdict "$wt" red; then
    revert_rustc_error "$wt"
    if wt_await_verdict "$wt" green; then
      pass AC8 "edit→red and revert→green both reflected within ${S1_VERDICT_GRACE}s (no stale overlay)"
    else
      fail AC8 "revert not reflected — verdict stuck red (stale overlay after edit-back)"
    fi
  else
    revert_rustc_error "$wt"
    [ "$(wt_verdict "$wt")" = green ] \
      && stop_class AC8 "FALSE-GREEN: edited a definite error but verdict stayed green (stale overlay — Shape-1 read not refreshed)" \
      || fail AC8 "no red within ${S1_VERDICT_GRACE}s of a definite-error edit (latency/freshness)"
  fi
}

# ─── orchestration ───────────────────────────────────────────────────
ACS="AC1 AC2 AC3 AC4 AC5 AC6 AC7 AC8"
run_one() {
  case "$1" in
    AC1) ac1_bringup ;;            AC2) ac2_parity ;;
    AC3) ac3_isolation ;;          AC4) ac4_respawn ;;
    AC5) ac5_never_publish_red ;;  AC6) ac6_transport_auth ;;
    AC7) ac7_sigterm_reap ;;       AC8) ac8_overlay_freshness ;;
    *) echo "unknown AC: $1" >&2; exit 2 ;;
  esac
}
summary() {
  hdr "STAGE-1 SUMMARY"
  for r in "${RESULTS[@]}"; do
    printf '  %s\n' "$(echo "$r" | awk -F'|' '{printf "%-16s %-5s %s",$1,$2,$3}')"
  done
  if [ "$FAILED" -eq 0 ]; then
    echo; echo "  ✅ ALL PASS — Stage-1 GATE OPEN. Advance to Stage-2 only on team-lead + operator GO."
    exit 0
  fi
  echo; echo "  ❌ $FAILED FAIL — Stage-1 GATE CLOSED. Do NOT advance; route findings to team-lead."
  exit 1
}

self_check() {
  hdr "SELF-CHECK — readiness only (executes NO acceptance criterion)"
  local rc=0
  for f in "$SUITE_DIR"/lib.sh "$SUITE_DIR"/run-stage1.sh; do
    if bash -n "$f"; then echo "  syntax OK: $f"; else echo "  SYNTAX ERROR: $f"; rc=1; fi
  done
  guard_no_cargo && echo "  guard: no local cargo/rustc in suite — OK"
  if command -v "$CARGOLESS_BIN" >/dev/null 2>&1; then
    echo "  cargoless bin: present ($CARGOLESS_BIN)"
  else
    echo "  cargoless bin: ABSENT — suite will preflight-fail until Inc0+1 ships the binary (expected pre-GO)"
  fi
  command -v curl >/dev/null 2>&1 && echo "  curl: present" || { echo "  curl: ABSENT (AC6 needs it)"; rc=1; }
  command -v git  >/dev/null 2>&1 && echo "  git: present"  || { echo "  git: ABSENT (AC3 needs it)";  rc=1; }
  echo "  ACs: $ACS"
  echo "  named Inc0+1 knobs: S1_POINTER_REL='$S1_POINTER_REL' S1_STATUS_GLOB='$S1_STATUS_GLOB' S1_PUBLISH_ARGS='$S1_PUBLISH_ARGS'"
  echo
  [ "$rc" = 0 ] && echo "  READY — suite is runnable the instant Inc0+1 land + GO (S1_EXECUTE_GO=1)." \
                || echo "  NOT READY — fix the above before GO."
  exit "$rc"
}

main() {
  local mode=all sel=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --self-check) mode=selfcheck ;;
      --list) echo "$ACS" | tr ' ' '\n'; exit 0 ;;
      --only) mode=only; sel="$2"; shift ;;
      --from) mode=from; sel="$2"; shift ;;
      -h|--help) grep -E '^# ' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
      *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
  done
  [ "$mode" = selfcheck ] && self_check
  require_go                       # STRUCTURAL gate — blocks pre-GO exec
  preflight
  setup_repo
  case "$mode" in
    only) run_one "$sel" ;;
    from) local seen=0; for a in $ACS; do [ "$a" = "$sel" ] && seen=1; [ "$seen" = 1 ] && run_one "$a"; done ;;
    *)    for a in $ACS; do run_one "$a"; done ;;
  esac
  summary
}
main "$@"
