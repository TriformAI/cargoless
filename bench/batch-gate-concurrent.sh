#!/usr/bin/env bash
#
# Concurrent native batch-gate harness.
#
# Drives M simultaneous `cargoless batch-check` requests against an already
# running `serve --repo --bind` daemon. This is the load/AX companion to
# `batch-gate-throughput.sh`: throughput sweeps one batch at a time, while this
# script simulates many agents arriving in the same coalescing window.
#
# Required:
#   REMOTE=http://host:port
#   SERVER_ROOT=/workspace/repo
#
# Common 40-agent shapes:
#   REQUESTS=40 BATCH_SIZE=1   # current-style independent agents
#   REQUESTS=4  BATCH_SIZE=10  # shard-local grouped batches
#   REQUESTS=1  BATCH_SIZE=40  # one daemon proves the whole burst
#
# Optional:
#   CARGOLESS_BIN=./target/release/cargoless
#   BASE_REF=origin/main
#   REQUESTS=4
#   BATCH_SIZE=10
#   SCENARIO=green              # green | one-red | multi-red | interaction
#   EXPECT=green                # green | red | any
#   WORK=/tmp/cargoless-batch-concurrent
#   CARGOLESS_AUTH_TOKEN=...
#   DRY_RUN=1                  # generate JSON only, do not call remote
#
# Output:
#   BATCH_CONCURRENT_CELL ...
#   BATCH_CONCURRENT_SUMMARY ...
#   BATCH_CONCURRENT_RESULT PASS|RED|INDETERMINATE ...

set -uo pipefail

REMOTE="${REMOTE:-}"
SERVER_ROOT="${SERVER_ROOT:-}"
CARGOLESS_BIN="${CARGOLESS_BIN:-cargoless}"
BASE_REF="${BASE_REF:-origin/main}"
REQUESTS="${REQUESTS:-4}"
BATCH_SIZE="${BATCH_SIZE:-10}"
SCENARIO="${SCENARIO:-green}"
EXPECT="${EXPECT:-green}"
WORK="${WORK:-/tmp/cargoless-batch-concurrent}"
DRY_RUN="${DRY_RUN:-0}"

blocker() {
  echo "BATCH_CONCURRENT_RESULT BLOCKED reason=$1"
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
is_uint "$REQUESTS" || blocker "REQUESTS-must-be-integer"
is_uint "$BATCH_SIZE" || blocker "BATCH_SIZE-must-be-integer"
[ "$REQUESTS" -gt 0 ] || blocker "REQUESTS-must-be-positive"
[ "$BATCH_SIZE" -gt 0 ] || blocker "BATCH_SIZE-must-be-positive"
case "$SCENARIO" in green|one-red|multi-red|interaction) ;; *) blocker "bad-SCENARIO-$SCENARIO" ;; esac
case "$EXPECT" in green|red|any) ;; *) blocker "bad-EXPECT-$EXPECT" ;; esac
command -v "$CARGOLESS_BIN" >/dev/null 2>&1 || [ -x "$CARGOLESS_BIN" ] || blocker "cargoless-bin-not-found"
command -v python3 >/dev/null 2>&1 || blocker "python3-not-found"

rm -rf "$WORK"
mkdir -p "$WORK"

AUTH_ARGS=()
if [ -n "${CARGOLESS_AUTH_TOKEN:-}" ]; then
  AUTH_ARGS=(--auth-token "$CARGOLESS_AUTH_TOKEN")
fi

make_request() {
  local request_idx="$1" out="$2"
  python3 - "$request_idx" "$BATCH_SIZE" "$out" "$SERVER_ROOT" "$BASE_REF" "$SCENARIO" <<'PY'
import json, sys

request_idx = int(sys.argv[1])
batch_size = int(sys.argv[2])
out = sys.argv[3]
server_root = sys.argv[4]
base_ref = sys.argv[5]
scenario = sys.argv[6]

members = []
for member_idx in range(batch_size):
    global_idx = request_idx * batch_size + member_idx
    rel = f"bench/batch-concurrent/request_{request_idx:03d}/member_{member_idx:03d}.rs"
    content = (
        f"// generated concurrent batch member {global_idx}\n"
        f"pub fn batch_concurrent_{request_idx:03d}_{member_idx:03d}() -> usize {{ {global_idx} }}\n"
    )
    if scenario == "one-red" and request_idx == 0 and member_idx == 0:
        content += "// FAIL_BATCH\n"
    elif scenario == "multi-red" and member_idx == 0:
        content += "// FAIL_BATCH\n"
    elif scenario == "interaction" and member_idx in (0, 1):
        rel = f"bench/batch-concurrent/request_{request_idx:03d}/shared.rs"
        content = (
            f"// interaction candidate from member {member_idx}\n"
            f"pub fn shared_{member_idx}() -> usize {{ {member_idx} }}\n"
        )
    members.append({
        "worktree": f"batch-request-{request_idx:03d}-member-{member_idx:03d}",
        "files": [{"path": rel, "content": content}],
        "changed_files": [rel],
    })

body = {
    "op": "batch_check",
    "batch_id": f"batch-concurrent-{request_idx:03d}",
    "base_ref": base_ref,
    "members": members,
    "options": {
        "repo_relative": True,
        "analysis_root": server_root,
    },
    "corun": True,
}
with open(out, "w", encoding="utf-8") as f:
    json.dump(body, f, separators=(",", ":"))
PY
}

run_cell() {
  local request_idx="$1"
  local req="$WORK/request-$request_idx.json"
  local out="$WORK/report-$request_idx.json"
  local err="$WORK/report-$request_idx.err"
  local meta="$WORK/report-$request_idx.meta"
  make_request "$request_idx" "$req"
  if [ "$DRY_RUN" = "1" ]; then
    local bytes
    bytes=$(wc -c < "$req" | tr -d ' ')
    echo "BATCH_CONCURRENT_CELL request=$request_idx dry_run=1 request=$req request_bytes=$bytes" > "$meta"
    return 0
  fi

  local start_ns end_ns wall_ms rc
  start_ns=$(date +%s%N)
  "$CARGOLESS_BIN" batch-check --remote "$REMOTE" "${AUTH_ARGS[@]}" --request-json "$req" >"$out" 2>"$err"
  rc=$?
  end_ns=$(date +%s%N)
  wall_ms=$(( (end_ns - start_ns) / 1000000 ))
  echo "request=$request_idx rc=$rc wall_ms=$wall_ms request=$req report=$out stderr=$err" > "$meta"
  return 0
}

echo "BATCH_CONCURRENT_START remote=$REMOTE server_root=$SERVER_ROOT base_ref=$BASE_REF requests=$REQUESTS batch_size=$BATCH_SIZE scenario=$SCENARIO expect=$EXPECT dry_run=$DRY_RUN work=$WORK"

pids=()
i=0
while [ "$i" -lt "$REQUESTS" ]; do
  run_cell "$i" &
  pids+=("$!")
  i=$((i + 1))
done

overall_wait=0
for pid in "${pids[@]}"; do
  wait "$pid" || overall_wait=2
done

if [ "$DRY_RUN" = "1" ]; then
  cat "$WORK"/report-*.meta
  echo "BATCH_CONCURRENT_RESULT PASS dry_run=1 work=$WORK"
  exit 0
fi

python3 - "$WORK" "$REQUESTS" "$BATCH_SIZE" "$SCENARIO" "$EXPECT" "$overall_wait" <<'PY'
import json, os, sys

work, requests, batch_size, scenario, expect, overall_wait = sys.argv[1:]
requests = int(requests)
batch_size = int(batch_size)
overall_wait = int(overall_wait)

rc_counts = {}
verdict_counts = {}
provenance_counts = {}
walls = []
combined_checks = 0
solo_checks = 0
members = 0
parse_errors = []

for i in range(requests):
    meta_path = os.path.join(work, f"report-{i}.meta")
    meta = {}
    if os.path.exists(meta_path):
        for part in open(meta_path, encoding="utf-8").read().strip().split():
            if "=" in part:
                key, val = part.split("=", 1)
                meta[key] = val
    rc = int(meta.get("rc", "2"))
    wall = int(meta.get("wall_ms", "0"))
    rc_counts[rc] = rc_counts.get(rc, 0) + 1
    walls.append(wall)
    report_path = os.path.join(work, f"report-{i}.json")
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

walls.sort()
def pct(values, q):
    if not values:
        return 0
    idx = int((len(values) - 1) * q)
    return values[idx]

parts = [
    f"requests={requests}",
    f"batch_size={batch_size}",
    f"total_members={members}",
    f"scenario={scenario}",
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
    f"p50_wall_ms={pct(walls,0.50)}",
    f"p95_wall_ms={pct(walls,0.95)}",
    f"max_wall_ms={walls[-1] if walls else 0}",
]
print("BATCH_CONCURRENT_SUMMARY " + " ".join(parts))
for i in range(requests):
    meta_path = os.path.join(work, f"report-{i}.meta")
    if os.path.exists(meta_path):
        print("BATCH_CONCURRENT_CELL " + open(meta_path, encoding="utf-8").read().strip())

if overall_wait != 0 or parse_errors:
    print("BATCH_CONCURRENT_RESULT INDETERMINATE reason=parse-or-wait-error work=" + work)
    if parse_errors:
        print("BATCH_CONCURRENT_PARSE_ERRORS " + " ".join(parse_errors))
    sys.exit(2)

if expect == "any":
    print("BATCH_CONCURRENT_RESULT PASS work=" + work)
    sys.exit(0)
if expect == "green":
    ok = rc_counts.get(0, 0) == requests and verdict_counts.get("green", 0) == requests
elif expect == "red":
    ok = rc_counts.get(1, 0) > 0 and verdict_counts.get("red", 0) > 0 and verdict_counts.get("indeterminate", 0) == 0
else:
    ok = False

if ok:
    print("BATCH_CONCURRENT_RESULT PASS work=" + work)
    sys.exit(0)
print("BATCH_CONCURRENT_RESULT RED work=" + work)
sys.exit(1)
PY
