#!/usr/bin/env bash
#
# lane-attribution-mutation-test.sh — prove the lane policy tests can FAIL.
#
# `tests/lane_policy.rs` is the only thing standing between the lane and an
# unjust ejection. A test suite that passes whether or not the code works is
# worse than no suite: it converts "unverified" into "verified", which is how
# this fleet has repeatedly shipped machinery that exits 0 while doing nothing.
#
# So this harness breaks the attribution deliberately, in four different ways,
# and asserts the suite goes RED for each. If a mutation survives, the tests are
# not actually checking the rule that mutation violates, and the gap is named in
# the failure message rather than left for someone to discover in production.
#
# The four mutations map one-to-one onto the operator's stated rules:
#
#   1. never attribute      — always report "no culprit". Catches tests that
#                             would accept holding the whole queue on every red.
#   2. always attribute     — blame every member whether or not they touched a
#                             failing file. Catches missing innocence checks.
#   3. cancel in flight     — let a new arrival preempt the running build.
#                             Catches a lost "never cancel" guarantee.
#   4. line-sensitive id    — put line back into the ejection identity. Catches
#                             the regression attribution.rs explicitly warns
#                             about: an insertion above an error re-blames it.
#
# Usage:  scripts/tests/lane-attribution-mutation-test.sh
# Exit:   0 = every mutation was caught, 1 = at least one survived

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SRC="$ROOT/crates/cargoless-core/src/lane.rs"
[ -f "$SRC" ] || { echo "FAIL: $SRC missing" >&2; exit 1; }

if ! command -v cargo >/dev/null 2>&1; then
    echo "SKIP: no cargo on PATH (this harness is CI-only by design)" >&2
    exit 0
fi

BACKUP="$(mktemp)"
cp "$SRC" "$BACKUP"
restore() { cp "$BACKUP" "$SRC"; rm -f "$BACKUP"; }
trap restore EXIT

run_suite() {
    (cd "$ROOT" && cargo test -p cargoless-core --test lane_policy --locked) \
        >/tmp/lane-mutation.log 2>&1
}

echo "== baseline: the suite must PASS on unmutated code =="
if ! run_suite; then
    echo "FAIL: the suite is red BEFORE any mutation — fix that first" >&2
    tail -30 /tmp/lane-mutation.log >&2
    exit 1
fi
echo "  ok — baseline green"

survivors=0

# $1 = human name, $2 = python mutation applied to lane.rs, $3 = what it proves
mutate() {
    local name="$1" py="$2" proves="$3"
    cp "$BACKUP" "$SRC"
    python3 - "$SRC" <<PY
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
${py}
p.write_text(s)
PY
    if ! cmp -s "$SRC" "$BACKUP"; then
        if run_suite; then
            echo "  SURVIVED — $name" >&2
            echo "      the suite passes on code that $proves" >&2
            survivors=$((survivors + 1))
        else
            echo "  ok — caught: $name"
        fi
    else
        echo "  SURVIVED — $name (mutation did not apply; the pattern moved)" >&2
        echo "      re-anchor this mutation on the current source" >&2
        survivors=$((survivors + 1))
    fi
}

echo "== mutations =="

# 1. Attribution never finds an owner -> everything becomes Unattributed.
mutate "never attribute (owners always empty)" \
  's = s.replace("if m.touches(&d.file_path) {", "if false {", 1)' \
  "never attributes a red to anyone, so every failure holds the whole queue"

# 2. Attribution blames everyone, touched or not.
mutate "always attribute (every member owns every error)" \
  's = s.replace("if m.touches(&d.file_path) {", "if true {", 1)' \
  "ejects members who did not touch any failing file"

# 3. A new arrival preempts the running build.
mutate "cancel in flight (arrivals preempt)" \
  's = s.replace("if self.phase == LanePhase::Building || self.queue.is_empty() {", "if self.queue.is_empty() {", 1)' \
  "lets a new arrival cancel a build that could still go green"

# 4. Ejection identity becomes line-sensitive.
#    fingerprint_counts is the shared, line-free identity; swapping in a
#    line-bearing key is exactly the regression attribution.rs warns about.
mutate "line-sensitive ejection identity" \
  's = s.replace("let fingerprints = fingerprint_counts(&self.root, &owned);", "let mut fingerprints = fingerprint_counts(&self.root, &owned); for d in &owned { fingerprints.insert(format!(\"line:{}\", d.line), 1); }", 1)' \
  "puts line numbers back into the ejection identity, so an unrelated insertion above an error re-blames it"

echo
if [ "$survivors" -ne 0 ]; then
    echo "FAIL: ${survivors} mutation(s) survived — the lane tests do not actually" >&2
    echo "      check the rule those mutations break." >&2
    exit 1
fi
echo "PASS: every mutation was caught; the lane policy tests have teeth."
