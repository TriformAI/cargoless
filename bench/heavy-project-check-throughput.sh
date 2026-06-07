#!/usr/bin/env bash
#
# Real project-check throughput harness for heavy Cargoless checks.
#
# This is intentionally different from `batch-gate-throughput.sh`: it creates
# repo-relative overlays under paths that a project's own
# `cargoless.checks.yaml` treats as real heavy-check triggers. In
# tf-multiverse, the defaults hit the actual compiler witnesses:
#
#   ssr      -> server/src/**/*.rs        (SSR compiler witness only)
#   wasm     -> portal/src/**/*.rs        (WASM + SSR; Triform has no
#                                          WASM-only trigger class)
#   isolator -> isolator/src/**/*.rs      (isolator vsock compiler witness)
#   all      -> runtime-types/src/**/*.rs (SSR + WASM + isolator)
#   mixed    -> alternates every configured path prefix
#
# Required:
#   REMOTE=http://host:port
#   SERVER_ROOT=/workspace/repo          # daemon-side analysis root
#
# Optional:
#   CARGOLESS_BIN=./target/release/cargoless
#   BASE_REF=origin/main
#   MODE=sweep                           # sweep | concurrent
#   SCENARIOS="ssr wasm isolator all mixed"
#   NLIST="1 2 4 8 16 40"                # sweep mode
#   REQUESTS=4                           # concurrent mode
#   BATCH_SIZE=10                        # concurrent mode
#   SCENARIO_PATHS="ssr=server/src/cargoless_heavy_bench wasm=portal/src/cargoless_heavy_bench isolator=isolator/src/cargoless_heavy_bench all=runtime-types/src/cargoless_heavy_bench"
#   FILE_EXT=rs
#   WORK=/tmp/cargoless-heavy-project-check
#   CARGOLESS_AUTH_TOKEN=...
#   DRY_RUN=1                            # generate JSON only; do not call remote
#   COALESCE_KEY='tf-heavy:{scenario}:origin-dev'
#                                        # opt into daemon-side coalescing;
#                                        # {scenario} is replaced per request
#   EXPECT=green                         # green | red | any
#   REQUIRE_FAST_GREEN=1                 # green multi-member batches must be
#                                        # one combined check, zero solos
#
# Optional real-red probe:
#   FAIL_SCENARIO=one-red
#   RED_MEMBER=0
#   RED_PATH=portal/src/lib.rs           # or another compiled file
#
# Output:
#   HEAVY_CELL ...
#   HEAVY_SUMMARY ...
#   HEAVY_RESULT PASS|RED|INDETERMINATE|BLOCKED ...
#
# Safety:
# - The default green overlays create unreferenced `.rs` files. They trigger
#   project-check manifests by path without changing compiled code.
# - `DRY_RUN=1` is the default-safe way to validate request construction before
#   starting any real heavy checks.

set -uo pipefail

REMOTE="${REMOTE:-}"
SERVER_ROOT="${SERVER_ROOT:-}"
CARGOLESS_BIN="${CARGOLESS_BIN:-cargoless}"
BASE_REF="${BASE_REF:-origin/main}"
MODE="${MODE:-sweep}"
SCENARIOS="${SCENARIOS:-ssr wasm isolator all mixed}"
NLIST="${NLIST:-1 2 4 8 16 40}"
REQUESTS="${REQUESTS:-4}"
BATCH_SIZE="${BATCH_SIZE:-10}"
SCENARIO_PATHS="${SCENARIO_PATHS:-ssr=server/src/cargoless_heavy_bench wasm=portal/src/cargoless_heavy_bench isolator=isolator/src/cargoless_heavy_bench all=runtime-types/src/cargoless_heavy_bench}"
FILE_EXT="${FILE_EXT:-rs}"
WORK="${WORK:-/tmp/cargoless-heavy-project-check}"
DRY_RUN="${DRY_RUN:-0}"
COALESCE_KEY="${COALESCE_KEY:-}"
EXPECT="${EXPECT:-green}"
REQUIRE_FAST_GREEN="${REQUIRE_FAST_GREEN:-1}"
FAIL_SCENARIO="${FAIL_SCENARIO:-green}"
RED_MEMBER="${RED_MEMBER:-0}"
RED_PATH="${RED_PATH:-}"

say() { printf '[heavy-project-check %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
blocker() {
  echo "HEAVY_RESULT BLOCKED reason=$1"
  exit 2
}

is_uint() {
  case "$1" in
    ''|*[!0-9]*) return 1 ;;
    *) return 0 ;;
  esac
}

[ -n "$REMOTE" ] || blocker "REMOTE-required"
[ -n "$SERVER_ROOT" ] || blocker "SERVER_ROOT-required"
case "$MODE" in sweep|concurrent) ;; *) blocker "bad-MODE-$MODE" ;; esac
case "$EXPECT" in green|red|any) ;; *) blocker "bad-EXPECT-$EXPECT" ;; esac
case "$FAIL_SCENARIO" in green|one-red) ;; *) blocker "bad-FAIL_SCENARIO-$FAIL_SCENARIO" ;; esac
is_uint "$REQUESTS" || blocker "REQUESTS-must-be-integer"
is_uint "$BATCH_SIZE" || blocker "BATCH_SIZE-must-be-integer"
is_uint "$RED_MEMBER" || blocker "RED_MEMBER-must-be-integer"
[ "$REQUESTS" -gt 0 ] || blocker "REQUESTS-must-be-positive"
[ "$BATCH_SIZE" -gt 0 ] || blocker "BATCH_SIZE-must-be-positive"
if [ "$FAIL_SCENARIO" = "one-red" ] && [ -z "$RED_PATH" ]; then
  blocker "RED_PATH-required-for-one-red"
fi
command -v python3 >/dev/null 2>&1 || blocker "python3-not-found"
if [ "$DRY_RUN" != "1" ]; then
  command -v "$CARGOLESS_BIN" >/dev/null 2>&1 || [ -x "$CARGOLESS_BIN" ] || blocker "cargoless-bin-not-found"
fi

rm -rf "$WORK"
mkdir -p "$WORK"

AUTH_ARGS=()
if [ -n "${CARGOLESS_AUTH_TOKEN:-}" ]; then
  AUTH_ARGS=(--auth-token "$CARGOLESS_AUTH_TOKEN")
fi

make_request() {
  local scenario="$1" n="$2" request_idx="$3" out="$4"
  python3 - "$scenario" "$n" "$request_idx" "$out" "$SERVER_ROOT" "$BASE_REF" \
    "$SCENARIO_PATHS" "$FILE_EXT" "$FAIL_SCENARIO" "$RED_MEMBER" "$RED_PATH" \
    "$COALESCE_KEY" <<'PY'
import json
import re
import sys

scenario = sys.argv[1]
n = int(sys.argv[2])
request_idx = int(sys.argv[3])
out = sys.argv[4]
server_root = sys.argv[5]
base_ref = sys.argv[6]
scenario_paths_raw = sys.argv[7]
file_ext = sys.argv[8].lstrip(".")
fail_scenario = sys.argv[9]
red_member = int(sys.argv[10])
red_path = sys.argv[11]
coalesce_key_template = sys.argv[12]

def slug(s):
    return re.sub(r"[^A-Za-z0-9_]", "_", s)

paths = {}
for part in scenario_paths_raw.split():
    if "=" not in part:
        continue
    name, prefix = part.split("=", 1)
    name = name.strip()
    prefix = prefix.strip().strip("/")
    if name and prefix:
        paths[name] = prefix

if not paths:
    raise SystemExit("SCENARIO_PATHS parsed empty")

if scenario == "mixed":
    active = list(paths.items())
elif scenario in paths:
    active = [(scenario, paths[scenario])]
else:
    raise SystemExit(f"unknown scenario `{scenario}`; known={','.join(sorted(paths))},mixed")

members = []
for i in range(n):
    global_idx = request_idx * n + i
    name, prefix = active[i % len(active)]
    rel = f"{prefix}/request_{request_idx:03d}_member_{i:03d}.{file_ext}"
    ident = slug(f"cargoless_heavy_bench_{name}_{request_idx}_{i}")
    content = (
        f"// generated by bench/heavy-project-check-throughput.sh\n"
        f"// scenario={scenario} request={request_idx} member={i} global={global_idx}\n"
        f"#[allow(dead_code)]\n"
        f"const {ident.upper()}: &str = \"{scenario}:{request_idx}:{i}\";\n"
    )
    if fail_scenario == "one-red" and global_idx == red_member:
        rel = red_path
        content = (
            f"// generated intentional compiler failure for Cargoless heavy witness attribution\n"
            f"pub fn cargoless_heavy_bench_red_member_{global_idx}() {{\n"
            f"    let _x = ;\n"
            f"}}\n"
        )
    members.append({
        "worktree": f"heavy-{scenario}-request-{request_idx:03d}-member-{i:03d}",
        "files": [{"path": rel, "content": content}],
        "changed_files": [rel],
    })

body = {
    "op": "batch_check",
    "batch_id": f"heavy-{scenario}-request-{request_idx:03d}-n-{n}",
    "base_ref": base_ref,
    "members": members,
    "options": {
        "repo_relative": True,
        "analysis_root": server_root,
        "gate": True,
    },
    "corun": True,
}
if coalesce_key_template.strip():
    body["coalesce_key"] = coalesce_key_template.replace("{scenario}", scenario)
with open(out, "w", encoding="utf-8") as f:
    json.dump(body, f, separators=(",", ":"))
PY
}

summarise_report() {
  python3 - "$1" <<'PY'
import json
import sys
try:
    report = json.load(open(sys.argv[1], encoding="utf-8"))
except Exception as e:
    print(f"parse_error={e}")
    sys.exit(0)
members = report.get("members") or []
counts = {}
for member in members:
    key = member.get("provenance", "unknown")
    counts[key] = counts.get(key, 0) + 1
parts = [
    f"verdict={report.get('verdict','unknown')}",
    f"members={len(members)}",
    f"combined_checks={report.get('combined_checks',0)}",
    f"solo_checks={report.get('solo_checks',0)}",
    f"duration_ms_reported={report.get('duration_ms',0)}",
    f"queue_wait_ms={report.get('queue_wait_ms',0)}",
    f"executed_members={report.get('executed_members',len(members))}",
    f"executed_batch_id={report.get('executed_batch_id','')}",
]
for key in ["combined_green", "solo_green", "solo_red", "interaction_red", "indeterminate"]:
    parts.append(f"{key}={counts.get(key,0)}")
print(" ".join(parts))
PY
}

require_fast_green() {
  python3 - "$1" "$2" <<'PY'
import json
import sys
n = int(sys.argv[1])
path = sys.argv[2]
report = json.load(open(path, encoding="utf-8"))
members = report.get("members") or []
if n <= 1 or report.get("verdict") != "green":
    sys.exit(0)
combined = int(report.get("combined_checks", 0) or 0)
solo = int(report.get("solo_checks", 0) or 0)
combined_green = sum(1 for m in members if m.get("provenance") == "combined_green")
ok = len(members) == n and combined == 1 and solo == 0 and combined_green == n
if not ok:
    print(
        "fast_path_violation="
        f"members:{len(members)} expected:{n} "
        f"combined_checks:{combined} solo_checks:{solo} "
        f"combined_green:{combined_green}",
        file=sys.stderr,
    )
    sys.exit(1)
PY
}

run_one_batch() {
  local scenario="$1" n="$2" request_idx="$3" label="$4"
  local req="$WORK/request-$label.json"
  local out="$WORK/report-$label.json"
  local err="$WORK/report-$label.err"
  make_request "$scenario" "$n" "$request_idx" "$req"
  if [ "$DRY_RUN" = "1" ]; then
    local bytes
    bytes=$(wc -c < "$req" | tr -d ' ')
    echo "HEAVY_CELL label=$label scenario=$scenario n=$n request_idx=$request_idx dry_run=1 request=$req request_bytes=$bytes"
    return 0
  fi

  local start_ns end_ns wall_ms rc summary fast_rc
  start_ns=$(date +%s%N)
  "$CARGOLESS_BIN" batch-check --remote "$REMOTE" "${AUTH_ARGS[@]}" --request-json "$req" >"$out" 2>"$err"
  rc=$?
  end_ns=$(date +%s%N)
  wall_ms=$(( (end_ns - start_ns) / 1000000 ))
  summary="$(summarise_report "$out")"
  fast_rc=0
  if [ "$REQUIRE_FAST_GREEN" = "1" ] && [ "$rc" -eq 0 ]; then
    require_fast_green "$n" "$out" 2>>"$err" || fast_rc=$?
  fi
  if [ "$fast_rc" -ne 0 ]; then
    rc=1
  fi
  echo "HEAVY_CELL label=$label scenario=$scenario n=$n request_idx=$request_idx exit=$rc wall_ms=$wall_ms $summary require_fast_green=$REQUIRE_FAST_GREEN report=$out stderr=$err"
  return "$rc"
}

run_sweep() {
  local overall=0
  for scenario in $SCENARIOS; do
    for n in $NLIST; do
      local label="${scenario}-n-${n}"
      run_one_batch "$scenario" "$n" 0 "$label"
      rc=$?
      case "$rc" in
        0) ;;
        1) overall=1 ;;
        *) [ "$overall" -eq 0 ] && overall=2 ;;
      esac
    done
  done
  return "$overall"
}

run_concurrent() {
  local scenario="$1"
  local pids=()
  local i=0
  while [ "$i" -lt "$REQUESTS" ]; do
    (
      label="${scenario}-request-${i}-n-${BATCH_SIZE}"
      run_one_batch "$scenario" "$BATCH_SIZE" "$i" "$label"
      echo "rc=$?" > "$WORK/rc-$i"
    ) &
    pids+=("$!")
    i=$((i + 1))
  done
  local wait_rc=0
  for pid in "${pids[@]}"; do
    wait "$pid" || wait_rc=2
  done
  if [ "$DRY_RUN" = "1" ]; then
    return 0
  fi
  python3 - "$WORK" "$REQUESTS" "$BATCH_SIZE" "$scenario" "$EXPECT" "$wait_rc" <<'PY'
import json
import os
import sys

work, requests, batch_size, scenario, expect, wait_rc = sys.argv[1:]
requests = int(requests)
batch_size = int(batch_size)
wait_rc = int(wait_rc)

rc_counts = {}
verdict_counts = {}
provenance_counts = {}
walls = []
combined_checks = 0
solo_checks = 0
members = 0
parse_errors = []

for i in range(requests):
    rc_path = os.path.join(work, f"rc-{i}")
    try:
        rc = int(open(rc_path, encoding="utf-8").read().strip().split("=", 1)[1])
    except Exception:
        rc = 2
    rc_counts[rc] = rc_counts.get(rc, 0) + 1
    report_path = os.path.join(work, f"report-{scenario}-request-{i}-n-{batch_size}.json")
    try:
        report = json.load(open(report_path, encoding="utf-8"))
    except Exception as exc:
        parse_errors.append(f"request={i}:{exc}")
        continue
    verdict = report.get("verdict", "unknown")
    verdict_counts[verdict] = verdict_counts.get(verdict, 0) + 1
    combined_checks += int(report.get("combined_checks", 0) or 0)
    solo_checks += int(report.get("solo_checks", 0) or 0)
    for member in report.get("members") or []:
        members += 1
        provenance = member.get("provenance", "unknown")
        provenance_counts[provenance] = provenance_counts.get(provenance, 0) + 1
    # wall_ms is reported in HEAVY_CELL stdout, not duplicated here.

parts = [
    f"scenario={scenario}",
    f"requests={requests}",
    f"batch_size={batch_size}",
    f"total_members={members}",
    f"expect={expect}",
    f"rc0={rc_counts.get(0,0)}",
    f"rc1={rc_counts.get(1,0)}",
    f"rc2={sum(v for k,v in rc_counts.items() if k not in (0,1))}",
    f"green_reports={verdict_counts.get('green',0)}",
    f"red_reports={verdict_counts.get('red',0)}",
    f"indeterminate_reports={verdict_counts.get('indeterminate',0)}",
    f"combined_checks={combined_checks}",
    f"solo_checks={solo_checks}",
    f"combined_green={provenance_counts.get('combined_green',0)}",
    f"solo_green={provenance_counts.get('solo_green',0)}",
    f"solo_red={provenance_counts.get('solo_red',0)}",
    f"interaction_red={provenance_counts.get('interaction_red',0)}",
    f"indeterminate_members={provenance_counts.get('indeterminate',0)}",
]
print("HEAVY_SUMMARY " + " ".join(parts))
if wait_rc != 0 or parse_errors:
    if parse_errors:
        print("HEAVY_PARSE_ERRORS " + " ".join(parse_errors))
    sys.exit(2)
if expect == "any":
    sys.exit(0)
if expect == "green":
    ok = rc_counts.get(0, 0) == requests and verdict_counts.get("green", 0) == requests
elif expect == "red":
    ok = rc_counts.get(1, 0) > 0 and verdict_counts.get("red", 0) > 0 and verdict_counts.get("indeterminate", 0) == 0
else:
    ok = False
sys.exit(0 if ok else 1)
PY
}

say "MODE=$MODE REMOTE=$REMOTE SERVER_ROOT=$SERVER_ROOT BASE_REF=$BASE_REF SCENARIOS=[$SCENARIOS] DRY_RUN=$DRY_RUN WORK=$WORK"
case "$MODE" in
  sweep)
    run_sweep
    overall=$?
    ;;
  concurrent)
    overall=0
    for scenario in $SCENARIOS; do
      run_concurrent "$scenario"
      rc=$?
      case "$rc" in
        0) ;;
        1) overall=1 ;;
        *) [ "$overall" -eq 0 ] && overall=2 ;;
      esac
    done
    ;;
esac

if [ "$DRY_RUN" = "1" ]; then
  echo "HEAVY_RESULT PASS dry_run=1 mode=$MODE work=$WORK"
  exit 0
fi

case "$overall" in
  0) echo "HEAVY_RESULT PASS mode=$MODE work=$WORK"; exit 0 ;;
  1) echo "HEAVY_RESULT RED mode=$MODE work=$WORK"; exit 1 ;;
  *) echo "HEAVY_RESULT INDETERMINATE mode=$MODE work=$WORK"; exit 2 ;;
esac
