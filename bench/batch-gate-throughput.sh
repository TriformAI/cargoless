#!/usr/bin/env bash
#
# Native batch-gate throughput harness.
#
# Measures the API shape needed for a 40-agent submitter pool:
# one shared central daemon, one shared analysis root, and batch requests
# containing N independent submitter overlays. The harness does NOT build
# cargoless, start a daemon, or run cargo locally. It drives an already-running
# `serve --repo --bind` daemon through `cargoless batch-check`.
#
# Required:
#   REMOTE=http://host:port
#   SERVER_ROOT=/workspace/repo          # daemon-side analysis root
# Optional:
#   CARGOLESS_BIN=./target/release/cargoless
#   BASE_REF=origin/main
#   NLIST="1 2 4 8 16 20 40"
#   WORK=/tmp/cargoless-batch-throughput
#   CARGOLESS_AUTH_TOKEN=...
#   DRY_RUN=1                            # generate JSON only, do not call remote
#   REQUIRE_FAST_GREEN=1                 # fail green n>1 runs unless they use
#                                        # one combined check and no solos
#
# Output:
#   BATCH_CELL n=<N> exit=<rc> verdict=<green|red|indeterminate> ...
#   BATCH_RESULT PASS|RED|INDETERMINATE|BLOCKED ...
#
# Interpretation:
#   combined_checks=1 solo_checks=0 with verdict=green is the optimistic fast
#   path we want under concurrent healthy PRs. A red should identify one or
#   more `solo_red` members; an `interaction_red` means the batch union failed
#   while every solo passed.

set -uo pipefail

REMOTE="${REMOTE:-}"
SERVER_ROOT="${SERVER_ROOT:-}"
CARGOLESS_BIN="${CARGOLESS_BIN:-cargoless}"
BASE_REF="${BASE_REF:-origin/main}"
NLIST="${NLIST:-1 2 4 8 16 20 40}"
WORK="${WORK:-/tmp/cargoless-batch-throughput}"
DRY_RUN="${DRY_RUN:-0}"
REQUIRE_FAST_GREEN="${REQUIRE_FAST_GREEN:-1}"

say() { printf '[batch-gate %s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
blocker() {
  echo "BATCH_RESULT BLOCKED reason=$1"
  exit 2
}

[ -n "$REMOTE" ] || blocker "REMOTE-required"
[ -n "$SERVER_ROOT" ] || blocker "SERVER_ROOT-required"
command -v "$CARGOLESS_BIN" >/dev/null 2>&1 || [ -x "$CARGOLESS_BIN" ] || blocker "cargoless-bin-not-found"
command -v python3 >/dev/null 2>&1 || blocker "python3-not-found"

rm -rf "$WORK"
mkdir -p "$WORK"

AUTH_ARGS=()
if [ -n "${CARGOLESS_AUTH_TOKEN:-}" ]; then
  AUTH_ARGS=(--auth-token "$CARGOLESS_AUTH_TOKEN")
fi

make_request() {
  local n="$1" out="$2"
  python3 - "$n" "$out" "$SERVER_ROOT" "$BASE_REF" <<'PY'
import json, sys
n = int(sys.argv[1])
out = sys.argv[2]
server_root = sys.argv[3]
base_ref = sys.argv[4]
members = []
for i in range(n):
    rel = f"bench/batch-throughput/member_{i:03d}.rs"
    members.append({
        "worktree": f"batch-member-{i:03d}",
        "files": [{
            "path": rel,
            "content": f"// generated batch throughput member {i}\npub fn batch_member_{i:03d}() -> usize {{ {i} }}\n",
        }],
        "changed_files": [rel],
    })
body = {
    "op": "batch_check",
    "batch_id": f"batch-throughput-{n}",
    "base_ref": base_ref,
    "members": members,
    "options": {
        "repo_relative": True,
        "analysis_root": server_root,
        "gate": True,
    },
    "corun": True,
}
with open(out, "w", encoding="utf-8") as f:
    json.dump(body, f, separators=(",", ":"))
PY
}

summarise_report() {
  python3 - "$1" <<'PY'
import json, sys
try:
    report = json.load(open(sys.argv[1], encoding="utf-8"))
except Exception as e:
    print(f"parse_error={e}")
    sys.exit(0)
members = report.get("members") or []
counts = {}
for m in members:
    counts[m.get("provenance", "unknown")] = counts.get(m.get("provenance", "unknown"), 0) + 1
parts = [
    f"verdict={report.get('verdict','unknown')}",
    f"members={len(members)}",
    f"combined_checks={report.get('combined_checks',0)}",
    f"solo_checks={report.get('solo_checks',0)}",
    f"duration_ms_reported={report.get('duration_ms',0)}",
]
for key in ["combined_green", "solo_green", "solo_red", "interaction_red", "indeterminate"]:
    parts.append(f"{key}={counts.get(key,0)}")
print(" ".join(parts))
PY
}

require_fast_green() {
  python3 - "$1" "$2" <<'PY'
import json, sys
n = int(sys.argv[1])
path = sys.argv[2]
report = json.load(open(path, encoding="utf-8"))
members = report.get("members") or []
if n <= 1 or report.get("verdict") != "green":
    sys.exit(0)
combined = int(report.get("combined_checks", 0) or 0)
solo = int(report.get("solo_checks", 0) or 0)
combined_green = sum(1 for m in members if m.get("provenance") == "combined_green")
ok = (
    len(members) == n
    and combined == 1
    and solo == 0
    and combined_green == n
)
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

overall=0
say "REMOTE=$REMOTE SERVER_ROOT=$SERVER_ROOT BASE_REF=$BASE_REF NLIST=[$NLIST] DRY_RUN=$DRY_RUN"
for n in $NLIST; do
  req="$WORK/request-$n.json"
  out="$WORK/report-$n.json"
  err="$WORK/report-$n.err"
  make_request "$n" "$req"
  if [ "$DRY_RUN" = "1" ]; then
    bytes=$(wc -c < "$req" | tr -d ' ')
    echo "BATCH_CELL n=$n dry_run=1 request=$req request_bytes=$bytes"
    continue
  fi

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
  echo "BATCH_CELL n=$n exit=$rc wall_ms=$wall_ms $summary require_fast_green=$REQUIRE_FAST_GREEN report=$out stderr=$err"
  case "$rc" in
    0) ;;
    1) overall=1 ;;
    *) [ "$overall" -eq 0 ] && overall=2 ;;
  esac
done

if [ "$DRY_RUN" = "1" ]; then
  echo "BATCH_RESULT PASS dry_run=1 work=$WORK"
  exit 0
fi

case "$overall" in
  0) echo "BATCH_RESULT PASS work=$WORK"; exit 0 ;;
  1) echo "BATCH_RESULT RED work=$WORK"; exit 1 ;;
  *) echo "BATCH_RESULT INDETERMINATE work=$WORK"; exit 2 ;;
esac
