#!/usr/bin/env bash
# scripts/stage1/run-stage1.sh — Stage-1 acceptance suite (PLAN-LANE D /
# tasks #228, #239). The falsifiable, all-must-PASS-to-advance gate for
# the cargoless-first rollout — it validates the wired `cargoless serve
# --repo` daemon (#225) against the committed Leptos reference fixture
# `bench/fixture/` (a real Rust+WASM project — cargoless's actual
# workload). The suite builds a throwaway git repo from the fixture with
# nested worktrees; the operator tree is never touched.
#
# USAGE
#   scripts/stage1/run-stage1.sh --self-check     # readiness ONLY; runs no AC
#   scripts/stage1/run-stage1.sh --list           # list the ACs
#   S1_EXECUTE_GO=1 scripts/stage1/run-stage1.sh           # full suite (gated)
#   S1_EXECUTE_GO=1 scripts/stage1/run-stage1.sh --only AC3 # one AC
#   S1_EXECUTE_GO=1 scripts/stage1/run-stage1.sh --from AC5 # AC5..AC8
#
# GATING (structural — lib.sh require_go): without S1_EXECUTE_GO=1 the
# suite refuses to execute ANY criterion — accidental execution is
# impossible by construction.
#
# EXIT CODES
#   0  all selected ACs PASS
#   1  ≥1 non-STOP FAIL (incl. INCONCLUSIVE) — Stage-1 does NOT advance
#   2  harness/preflight error (bad config, missing bin/curl/git/fixture)
#  99  STOP-CLASS HALT — a definite PRODUCT wrong-verdict: false-GREEN /
#      cross-contamination / torn-pointer / RA-orphan / auth-bypass.
#      Rollout HALTS → route to team-lead → dev-fixer.
#
# STOP vs FAIL (the verdict-provenance discipline — task #239): a
# STOP-class HALT is raised ONLY on a definite wrong verdict. `unknown`
# (verdict unobservable) is INCONCLUSIVE → a plain FAIL, NEVER a STOP.
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

# ─── AC2 — verdict-parity vs oracle {clean / rustc-error / per-crate /
#           clippy-only} ────────────────────────────────────────────────
ac2_parity() {
  hdr "AC2 — verdict-parity {clean / rustc-error / per-crate / clippy-only}"
  local wt="${S1_WTS[0]}"
  # clean → GREEN  (activate first: Model R only checks active worktrees)
  wt_activate "$wt"
  wt_await_verdict "$wt" green || note AC2 "clean baseline slow to settle green"
  assert_parity AC2-clean "$wt" green
  if [ "${S1_CI_ORACLE}" = 1 ]; then
    local sha cv; sha="$(git -C "$wt" rev-parse HEAD)"; cv="$(ci_verdict_for_sha "$sha")"
    note AC2-clean "Forgejo-CI baseline oracle for $sha: $cv (coarse confirm)"
  fi
  # rustc-error → RED  (false-GREEN here is STOP-class, via assert_parity)
  inject_rustc_error "$wt"; wt_await_verdict "$wt" red || true
  assert_parity AC2-rustc "$wt" red
  # per-crate schema=2 attribution (bench/fixture is single-crate
  # `cargoless-bench-fixture`). STOP only on a definite green; `unknown`
  # is inconclusive → FAIL (bug-#3 discipline).
  local v cr; v="$(wt_verdict "$wt")"; cr="$(wt_crates "$wt")"
  if [ "$v" = green ]; then
    stop_class AC2-percrate "FALSE-GREEN: rustc error injected but verdict=green"
  elif [ "$v" != red ]; then
    fail AC2-percrate "INCONCLUSIVE: verdict unobservable ($v) — not a false-GREEN"
  elif echo "$cr" | grep -qiE 'cargoless-bench-fixture:red'; then
    pass AC2-percrate "schema=2 per-crate attribution: cargoless-bench-fixture:red ($cr)"
  else
    pass AC2-percrate "verdict=red authoritative; per-crate crates= line absent (crates='$cr') — schema=1-compatible"
  fi
  revert_inject "$wt"; wt_await_verdict "$wt" green || true
  assert_parity AC2-revert "$wt" green
  # clippy-only — ERA-SCOPED per Lane-B #221. Pre-Inc3-B: warning-severity
  # lint suppressed ⇒ GREEN is CORRECT (S1_CLIPPY_EXPECTED=green).
  inject_clippy_only "$wt"
  local want_c; [ "$S1_CLIPPY_EXPECTED" = red ] && want_c=red || want_c=green
  wt_await_verdict "$wt" "$want_c" || true
  local cg; cg="$(wt_verdict "$wt")"
  case "$S1_CLIPPY_EXPECTED" in
    green) [ "$cg" = green ] && pass AC2-clippy "warning-level lint ⇒ green — CORRECT shipped v0.2.0 behaviour pre-Inc3-B (#221; not a bug)" \
            || { [ "$cg" = unknown ] && fail AC2-clippy "INCONCLUSIVE: verdict unobservable" \
                 || fail AC2-clippy "expected green (pre-Inc3-B contract), got $cg"; } ;;
    red)   [ "$cg" = red ] && pass AC2-clippy "lint ⇒ red — post-Inc3-B / error-level" \
            || fail AC2-clippy "expected red, got $cg" ;;
    *)     note AC2-clippy "clippy-only ⇒ $cg — record-only (S1_CLIPPY_EXPECTED=fieldfinding)" ;;
  esac
  revert_inject "$wt"; wt_await_verdict "$wt" green || true
}

# ─── AC3 — no-wrong-verdict: spatial isolation, ≥2 WTs / one shared RA ─
# The load-bearing Model-R risk. Editing WT_k must flip ONLY WT_k; every
# sibling must STAY green. A sibling green→RED = cross-contamination =
# STOP-class. A sibling green→unknown = inconclusive = FAIL (bug-#3).
ac3_isolation() {
  hdr "AC3 — spatial isolation across ${#S1_WTS[@]} worktrees / ONE shared RA"
  local k="${S1_WTS[0]}" j
  for j in "${S1_WTS[@]}"; do wt_activate "$j"; done
  for j in "${S1_WTS[@]}"; do wt_await_verdict "$j" green || true; done
  # baseline: every non-k sibling must be observably green
  local i=0 baseline_ok=1
  declare -a SNAP=()
  for j in "${S1_WTS[@]}"; do
    if [ "$j" = "$k" ]; then SNAP+=("-"); i=$((i+1)); continue; fi
    local bv; bv="$(wt_verdict "$j")"; SNAP+=("$bv")
    [ "$bv" = green ] || { baseline_ok=0; note AC3 "sibling $j not green at baseline ($bv)"; }
    i=$((i+1))
  done
  if [ "$baseline_ok" != 1 ]; then
    fail AC3 "INCONCLUSIVE: not all siblings reached a green baseline — cannot judge isolation"
    return
  fi
  # induce RED in WT_k only
  inject_rustc_error "$k"
  wt_await_verdict "$k" red \
    || { revert_inject "$k"; fail AC3 "INCONCLUSIVE: WT_k never went red"; return; }
  # every non-k sibling must STILL be green
  local contam=0 flaky=0
  i=0
  for j in "${S1_WTS[@]}"; do
    if [ "$j" = "$k" ]; then i=$((i+1)); continue; fi
    local now; now="$(wt_verdict "$j")"
    if [ "$now" = red ]; then
      contam=1; note AC3 "WT $j flipped green→RED while ONLY WT_k was edited"
    elif [ "$now" != green ]; then
      flaky=1; note AC3 "WT $j green→$now (verdict became unobservable — inconclusive, not a clean contamination)"
    fi
    i=$((i+1))
  done
  revert_inject "$k"; wt_await_verdict "$k" green || true
  if [ "$contam" = 1 ]; then
    stop_class AC3 "CROSS-CONTAMINATION: a non-edited worktree flipped green→RED when only WT_k was edited — the shared RA bled a verdict across worktrees"
  fi
  if [ "$flaky" = 1 ]; then
    fail AC3 "INCONCLUSIVE: a sibling verdict became unobservable during WT_k's edit"
    return
  fi
  pass AC3 "WT_k isolated RED; all $(( ${#S1_WTS[@]} - 1 )) siblings stayed GREEN (no bleed through the one shared RA)"
}

# ─── AC4 — respawn-staleness (kill -9 the shared rust-analyzer) ───────
ac4_respawn() {
  hdr "AC4 — kill -9 the shared rust-analyzer → correct post-respawn verdicts"
  local wt="${S1_WTS[0]}" rapids
  wt_activate "$wt"; wt_await_verdict "$wt" green || true
  rapids="$(ra_children "$DAEMON_PID")"
  [ -n "$rapids" ] || { fail AC4 "INCONCLUSIVE: no rust-analyzer child found under the daemon"; return; }
  log "killing -9 RA pids: $rapids"
  for p in $rapids; do kill -KILL "$p" 2>/dev/null || true; done
  sleep 5   # let the Supervisor notice + respawn + mux.reset()
  inject_rustc_error "$wt"
  if wt_await_verdict "$wt" red; then
    revert_inject "$wt"
    if wt_await_verdict "$wt" green; then
      pass AC4 "post-respawn RA produced a correct red→green cycle (mux.reset seam holds)"
    else
      fail AC4 "post-respawn stuck red after revert (stale overlay — reset() suspect)"
    fi
  else
    revert_inject "$wt"
    # a definite green on an injected error after respawn = false-GREEN
    if [ "$(wt_verdict "$wt")" = green ]; then
      stop_class AC4 "FALSE-GREEN post-respawn: a definite injected error verdicts green (stale RA after kill -9)"
    else
      fail AC4 "INCONCLUSIVE: no red within grace after respawn (RA respawn / handshake latency)"
    fi
  fi
}

# ─── AC5 — never-publish-red (latest-green pointer byte-unmoved) ──────
# The never-publish-red pointer is a `build --watch` publisher artifact;
# the serve daemon does not emit one in v0.2.0. AC5 runs the publisher on
# a DEDICATED standalone fixture checkout (not a serve worktree — avoids
# the dual-watch cli-status guard). A red transition must leave the
# pointer identical on all four of sha256+inode+mtime+size; a zero/partial
# pointer is a torn write.
ac5_never_publish_red() {
  hdr "AC5 — never-publish-red: pointer byte-unmoved on red, atomic"
  local pub="$S1_WORK/ac5-pub"
  rm -rf "$pub"; mkdir -p "$pub"
  cp -R "$S1_FIXTURE"/. "$pub"/
  local G=(-c user.email=stage1@cargoless.local -c user.name=stage1)
  git -C "$pub" init -q
  git -C "$pub" "${G[@]}" add -A
  git -C "$pub" "${G[@]}" commit -q -m "ac5 publisher fixture"
  # start the publisher
  ( cd "$pub" && "$CARGOLESS_BIN" $S1_PUBLISH_ARGS "$S1_WORK/ac5-dist" \
      > "$S1_WORK/ac5-pub.out" 2>&1 & echo $! > "$S1_WORK/ac5-pub.pid" )
  local lim=$(( $(date +%s) + S1_VERDICT_GRACE * 4 ))
  while [ "$(date +%s)" -lt "$lim" ]; do [ -f "$(ptr_path "$pub")" ] && break; sleep 3; done
  local fp0; fp0="$(ptr_fp "$pub")"
  if [ "$fp0" = MISSING ]; then
    fail AC5 "no latest-green pointer produced — publisher never reached first green (see ac5-pub.out)"
    [ -f "$S1_WORK/ac5-pub.pid" ] && kill -KILL "$(cat "$S1_WORK/ac5-pub.pid")" 2>/dev/null || true
    return
  fi
  # induce RED — pointer MUST stay byte-identical
  inject_rustc_error "$pub"
  sleep "$S1_VERDICT_GRACE"
  local fpR pp; fpR="$(ptr_fp "$pub")"; pp="$(ptr_path "$pub")"
  if [ "$fpR" != "$fp0" ]; then
    stop_class AC5 "MOVED POINTER: latest-green changed during RED ($fp0 → $fpR) — never-publish-red VIOLATED"
  fi
  if [ ! -s "$pp" ]; then
    stop_class AC5 "TORN POINTER: latest-green is zero-byte/partial during red"
  fi
  # recover — pointer must advance atomically and stay non-empty
  revert_inject "$pub"
  sleep "$S1_VERDICT_GRACE"
  local fp2; fp2="$(ptr_fp "$pub")"
  [ -f "$S1_WORK/ac5-pub.pid" ] && kill -KILL "$(cat "$S1_WORK/ac5-pub.pid")" 2>/dev/null || true
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
  local c_no c_bad c_ok
  c_no=$(curl -s -o /dev/null -w '%{http_code}' "$base/worktrees")
  c_bad=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer WRONG" "$base/worktrees")
  c_ok=$(curl -s -o /dev/null -w '%{http_code}' "${AUTH[@]}" "$base/worktrees")
  if [ "$c_no" = 401 ] && [ "$c_bad" = 401 ] && [ "$c_ok" = 200 ]; then
    pass AC6-auth "no-token=401 wrong=401 correct=200 (#14 bearer seam enforced)"
  else
    # a protected route served WITHOUT a valid bearer is an auth bypass
    if [ "$c_no" = 200 ] || [ "$c_bad" = 200 ]; then
      stop_class AC6 "AUTH BYPASS: a protected route served without a valid bearer (no=$c_no wrong=$c_bad)"
    fi
    fail AC6-auth "auth codes off (no=$c_no wrong=$c_bad ok=$c_ok)"; ok=0
  fi
  local enc; enc=$(printf '%s' "$wt" | sed 's:/:%2F:g')
  if curl -s "${AUTH[@]}" "$base/worktrees" | grep -q '\[' \
     && curl -s "${AUTH[@]}" "$base/status?worktree=$enc" | grep -qiE 'verdict|green|red' \
     && curl -s "${AUTH[@]}" "$base/verdict?worktree=$enc" | grep -qiE 'green|red'; then
    pass AC6-routes "GET /worktrees /status /verdict well-formed payloads"
  else
    fail AC6-routes "a GET route returned malformed/empty payload"; ok=0
  fi
  # SSE: ≥1 frame per transition (induce 2: green→red→green)
  ( curl -sN "${AUTH[@]}" "$base/events" > "$S1_WORK/sse.out" 2>&1 & echo $! > "$S1_WORK/sse.pid" )
  sleep 2
  inject_rustc_error "$wt"; wt_await_verdict "$wt" red || true
  revert_inject "$wt"; wt_await_verdict "$wt" green || true
  sleep 3
  kill -KILL "$(cat "$S1_WORK/sse.pid")" 2>/dev/null || true
  local frames; frames=$(grep -c '^data:' "$S1_WORK/sse.out" 2>/dev/null || echo 0)
  if [ "$frames" -ge 2 ]; then
    pass AC6-sse "SSE emitted $frames data frames across ≥2 transitions"
  else
    fail AC6-sse "SSE frame count $frames < expected ≥2 (missed transitions)"; ok=0
  fi
  [ "$ok" = 1 ] || true
}

# ─── AC7 — FF-A SIGTERM reap (#198 live re-verify) ───────────────────
ac7_sigterm_reap() {
  hdr "AC7 — SIGTERM reaps every per-cluster rust-analyzer child (#198)"
  local before; before="$(ra_children "$DAEMON_PID")"
  [ -n "$before" ] || { fail AC7 "INCONCLUSIVE: no RA children to reap"; return; }
  log "RA children before SIGTERM: $before"
  kill -TERM "$DAEMON_PID" 2>/dev/null || true
  local lim=$(( $(date +%s) + S1_SIGTERM_GRACE ))
  while [ "$(date +%s)" -lt "$lim" ]; do kill -0 "$DAEMON_PID" 2>/dev/null || break; sleep 1; done
  sleep 2
  local survivors=""
  for p in $before; do kill -0 "$p" 2>/dev/null && survivors="$survivors $p"; done
  local stray; stray="$(ps -axo pid,ppid,command 2>/dev/null \
      | awk '($2==1)&&(/rust-analyzer/||/proc-macro-srv/){print $1}')"
  DAEMON_PID=""
  if [ -n "${survivors// /}" ]; then
    stop_class AC7 "RA-ORPHAN: rust-analyzer survived the daemon SIGTERM ($survivors) — #198 ReapOnDrop regressed at the serve seam"
  fi
  if [ -n "$stray" ]; then
    stop_class AC7 "RA-ORPHAN: a reparented rust-analyzer/proc-macro-srv (pid$stray) outlived the daemon SIGTERM"
  fi
  pass AC7 "every per-cluster RA child reaped on SIGTERM (no zombie/orphan; #198 holds)"
}

# ─── AC8 — overlay freshness (verdict tracks on-disk content) ────────
ac8_overlay_freshness() {
  hdr "AC8 — overlay freshness: verdict tracks on-disk content, never stale"
  serve_start; serve_wait_up || { fail AC8 "daemon did not come up for AC8"; return; }
  local wt="${S1_WTS[1]:-${S1_WTS[0]}}"
  wt_activate "$wt"; wt_await_verdict "$wt" green || true
  inject_rustc_error "$wt"
  if wt_await_verdict "$wt" red; then
    revert_inject "$wt"
    if wt_await_verdict "$wt" green; then
      pass AC8 "edit→red and revert→green both reflected within grace (no stale overlay)"
    else
      fail AC8 "revert not reflected — verdict stuck red (stale overlay after edit-back)"
    fi
  else
    revert_inject "$wt"
    if [ "$(wt_verdict "$wt")" = green ]; then
      stop_class AC8 "FALSE-GREEN: a definite injected error verdicts green (stale overlay — on-disk read not refreshed)"
    else
      fail AC8 "INCONCLUSIVE: no red within grace of a definite-error edit (latency/freshness)"
    fi
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
  guard_no_cargo && echo "  guard: no local cargo/rustc invocation in suite — OK"
  command -v "$CARGOLESS_BIN" >/dev/null 2>&1 \
    && echo "  cargoless bin: present ($CARGOLESS_BIN)" \
    || echo "  cargoless bin: ABSENT ($CARGOLESS_BIN) — preflight will fail until a wired binary is on PATH"
  command -v curl >/dev/null 2>&1 && echo "  curl: present" || { echo "  curl: ABSENT (AC6)"; rc=1; }
  command -v git  >/dev/null 2>&1 && echo "  git: present"  || { echo "  git: ABSENT";        rc=1; }
  echo "  ACs: $ACS"
  echo "  substrate: Leptos fixture (S1_FIXTURE='${S1_FIXTURE:-<repo>/bench/fixture}')"
  echo "  contract knobs: S1_STATUS_REL='$S1_STATUS_REL' S1_POINTER_REL='$S1_POINTER_REL' S1_INJECT_FILE='$S1_INJECT_FILE'"
  echo
  [ "$rc" = 0 ] && echo "  READY — runnable with S1_EXECUTE_GO=1 against the Leptos fixture." \
                || echo "  NOT READY — fix the above."
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
