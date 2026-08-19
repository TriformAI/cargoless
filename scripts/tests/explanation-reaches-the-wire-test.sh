#!/usr/bin/env bash
#
# explanation-reaches-the-wire-test.sh — a computed reason must be published.
#
# The bug class this closes
# -------------------------
# `EjectReason::describe_for` is documented as "the product surface — what an
# author sees when their PR stops moving". It was contract-tested, it was
# correct, and it never left the process: its only caller was
# `LaneAction::Report`, which `LaneDriver::execute` discards. `GET /lane`
# published the enum tags it is derived FROM and not the sentence itself.
#
# That is not a documentation gap, because the tags cannot be decoded alone:
#   * `files: []`      = "could not identify them" (unattributed)
#                      = "nothing was compiled"    (infrastructure)
#   * `shared_with`    = the OTHER co-owners       (attributed)
#                      = the OTHER held members    (unattributed/infrastructure)
#   * re-admission differs per kind, and infrastructure also lapses on TTL.
#
# So every consumer re-derived the sentence by hand, and they drifted. The cost
# landed on whoever read a status next: it was only interpretable by someone who
# had privately written down what the fields mean.
#
# What this asserts
# -----------------
# Enum COVERAGE and reachability, never wording — rewording a sentence must
# never fail this, or it becomes a thing people delete.
#
#   1. every EjectReason variant is reachable from describe_for
#   2. every EjectionCause variant is reachable from describe_for
#   3. EjectionView carries `why`, populated from the shared describe(), NOT a
#      second match (a parallel match is how the wire text drifts from the
#      reported text)
#   4. GET /lane serializes `why`
#   5. GET /lane serializes `now`, the clock `expires_at_tick` is measured
#      against — a deadline with no clock beside it is not readable
#
# Usage:      scripts/tests/explanation-reaches-the-wire-test.sh [--self-test]
# Exit:       0 = the explanation reaches the wire, 1 = it does not, 2 = setup
#
# --self-test proves the assertions FIRE: it drops `why` from the serializer in
# a throwaway copy and requires this script to go red. A check that cannot fail
# is not a check.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
LANE="${LANE_OVERRIDE:-$ROOT/crates/cargoless-core/src/lane.rs}"
HOST="${LANEHOST_OVERRIDE:-$ROOT/crates/cargoless-core/src/lanehost.rs}"
API="${SERVEAPI_OVERRIDE:-$ROOT/crates/cargoless/src/serveapi.rs}"

for f in "$LANE" "$HOST" "$API"; do
    [ -r "$f" ] || { echo "explanation-reaches-the-wire: cannot read $f" >&2; exit 2; }
done

# ── --self-test: prove the assertions FIRE ────────────────────────────────
# Runs this script against throwaway copies with one thing broken each time and
# requires a RED. Without this, "PASS" only ever proves the script ran.
if [ "${1:-}" = "--self-test" ]; then
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    self_fails=0

    probe() {
        if "$0" >/dev/null 2>&1; then
            echo "  SELF-TEST FAIL — $1 did not go red" >&2
            self_fails=$((self_fails + 1))
        else
            echo "  ok — caught: $1"
        fi
    }

    echo "== self-test: each mutation must be caught =="

    # 1. the serializer drops `why` — the original bug, exactly.
    sed '/"why": e\.why,/d' "$API" >"$tmp/api-no-why.rs"
    ( SERVEAPI_OVERRIDE="$tmp/api-no-why.rs" ; export SERVEAPI_OVERRIDE
      probe "GET /lane drops why" )

    # 2. the projection drops the field entirely.
    sed '/pub why: String,/d' "$HOST" >"$tmp/host-no-field.rs"
    ( LANEHOST_OVERRIDE="$tmp/host-no-field.rs" ; export LANEHOST_OVERRIDE
      probe "EjectionView loses why" )

    # 3. `why` is re-derived instead of reusing describe() — the drift path.
    sed 's/why: e\.describe(),/why: String::new(),/' "$HOST" >"$tmp/host-parallel.rs"
    ( LANEHOST_OVERRIDE="$tmp/host-parallel.rs" ; export LANEHOST_OVERRIDE
      probe "why stops using the shared describe()" )

    # 4. a new EjectReason variant with no sentence.
    sed 's/^pub enum EjectReason {/pub enum EjectReason {\
    Quarantined { shared_with: Vec<String> },/' "$LANE" >"$tmp/lane-new-variant.rs"
    ( LANE_OVERRIDE="$tmp/lane-new-variant.rs" ; export LANE_OVERRIDE
      probe "a new EjectReason variant with no sentence" )

    # 5. the clock disappears, making every deadline unreadable.
    sed '/"now": s\.now,/d' "$API" >"$tmp/api-no-now.rs"
    ( SERVEAPI_OVERRIDE="$tmp/api-no-now.rs" ; export SERVEAPI_OVERRIDE
      probe "GET /lane drops the clock" )

    echo
    if [ "$self_fails" -ne 0 ]; then
        echo "SELF-TEST FAIL: ${self_fails} mutation(s) survived — this guard has no teeth." >&2
        exit 1
    fi
    echo "SELF-TEST PASS: every mutation was caught."
    exit 0
fi

fails=0
fail() { echo "  FAIL — $1" >&2; fails=$((fails + 1)); }
ok()   { echo "  ok — $1"; }

# ── 1+2. every variant of both enums is reachable from the sentence ────────
# The variants are read out of the source rather than hardcoded, so a NEW
# variant is covered the day it is added instead of silently bypassing this.
describe_body="$(awk '/pub fn describe_for/,/^    }$/' "$LANE")"
[ -n "$describe_body" ] || { echo "explanation-reaches-the-wire: describe_for not found in $LANE" >&2; exit 2; }

reason_variants="$(awk '/^pub enum EjectReason/,/^}$/' "$LANE" \
    | sed -n 's/^    \([A-Z][A-Za-z0-9]*\) *[{(].*/\1/p')"
cause_variants="$(awk '/^pub enum EjectionCause/,/^}$/' "$LANE" \
    | sed -n 's/^    \([A-Z][A-Za-z0-9]*\),.*/\1/p')"

[ -n "$reason_variants" ] || { echo "explanation-reaches-the-wire: no EjectReason variants parsed" >&2; exit 2; }
[ -n "$cause_variants" ]  || { echo "explanation-reaches-the-wire: no EjectionCause variants parsed" >&2; exit 2; }

echo "== every ejection variant has a sentence =="
for v in $reason_variants; do
    if grep -q "EjectReason::${v}" <<<"$describe_body"; then
        ok "EjectReason::${v} is explained"
    else
        fail "EjectReason::${v} has no arm in describe_for — an author who hits it gets an enum tag and no sentence"
    fi
done
# A cause needs its OWN arm only when the sentence must differ from the
# reason-derived one. `AlreadyLanded` ("no author action is required") and
# `MergeConflict` ("push a resolved head") are those; `BuildFailure` and
# `Infrastructure` deliberately fall through to the EjectReason arms, which
# already say the right thing. So this asserts every cause is REACHABLE —
# either named explicitly, or covered by the fall-through — rather than
# demanding an arm each, which would be a wording rule in disguise.
fallthrough="$(grep -c 'EjectionCause::' <<<"$describe_body")"
for v in $cause_variants; do
    if grep -q "EjectionCause::${v}" <<<"$describe_body"; then
        ok "EjectionCause::${v} has its own sentence"
    elif [ "$fallthrough" -gt 0 ]; then
        ok "EjectionCause::${v} falls through to the reason-derived sentence"
    else
        fail "EjectionCause::${v} is not reachable from describe_for at all"
    fi
done

# ── 3. the projection carries the sentence, from the shared helper ─────────
echo "== the snapshot projection carries it =="
if grep -q 'pub why: String' "$HOST"; then
    ok "EjectionView.why exists"
else
    fail "EjectionView has no \`why\` — the sentence stops before the wire, which is the original bug"
fi

if grep -qE 'why: e\.describe\(\)' "$HOST"; then
    ok "why comes from the shared Ejection::describe()"
else
    fail "EjectionView.why is not populated from e.describe() — deriving it a second way is how the published text drifts from the reported text"
fi

# ── 4+5. the wire publishes both ───────────────────────────────────────────
echo "== GET /lane publishes it =="
snapshot_body="$(awk '/fn lane_snapshot/,/^    }$/' "$API")"
[ -n "$snapshot_body" ] || { echo "explanation-reaches-the-wire: lane_snapshot not found in $API" >&2; exit 2; }

if grep -q '"why"' <<<"$snapshot_body"; then
    ok "GET /lane serializes why"
else
    fail "GET /lane does not serialize \`why\` — it publishes the fields the sentence is DERIVED from and not the sentence, so every consumer must re-derive it"
fi

if grep -q '"now"' <<<"$snapshot_body"; then
    ok "GET /lane serializes now (the clock expires_at_tick is measured against)"
else
    fail "GET /lane does not serialize \`now\` — expires_at_tick is then a bare deadline and 'how long until this clears' is uncomputable"
fi

echo
if [ "$fails" -ne 0 ]; then
    echo "FAIL: ${fails} check(s) failed — a reason is computed and not published." >&2
    echo "      A status a reader cannot decode without private notes is not a status." >&2
    exit 1
fi
echo "PASS: every ejection variant is explained and the explanation reaches the wire."
