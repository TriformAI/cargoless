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

# 5. The infra retry has no backoff. THIS IS WHAT SHIPPED, and it reached a
#    real deployment: every candidate failed to materialize, the requeued
#    members were eligible again instantly, and the lane rebuilt roughly every
#    2.5 seconds forever while `GET /lane` showed a steady `phase=building` —
#    indistinguishable from a slow compile. No test failed, because no test
#    asserted that a retry WAITS.
mutate "infra retry has no backoff (the hot loop)" \
  's = s.replace("self.infra_retry_after =\n                    Some(self.now.saturating_add(self.cfg.infra_backoff_ticks));", "self.infra_retry_after = None;", 1)' \
  "retries an infrastructure failure instantly and forever, burning the machine while reporting a phase indistinguishable from a long build"

# 6. The attempt cap never trips, so a PERMANENT infra failure retries forever.
#    A member whose head commit the daemon cannot reach never becomes buildable
#    by waiting; retrying it blocks every later submission behind it.
mutate "infra attempts are unbounded" \
  's = s.replace("if self.infra_failures >= self.cfg.infra_max_attempts {", "if false {", 1)' \
  "never gives up on a permanently-broken candidate, so the queue is blocked indefinitely by a build that cannot succeed"

# 7. The failure streak survives a good build. Then a lane hitting the odd
#    transient over hours eventually ejects a member that never did anything
#    wrong, for failures spread across unrelated builds.
mutate "infra streak is not reset by a non-infra outcome" \
  's = s.replace("if !matches!(outcome, LaneBuildOutcome::Infra { .. }) {\n            self.infra_failures = 0;\n            self.infra_retry_after = None;\n        }", "", 1)' \
  "accumulates infra failures across successful builds, so an occasional transient eventually ejects an innocent member"

# 8. An infra ejection reports as Unattributed. Those two mean opposite things
#    to the author who reads them: unattributed says the tree is red and the
#    owner is unknown (their code IS implicated), infrastructure says nothing
#    was ever compiled. Conflating them sends someone hunting a bug that was
#    never diagnosed.
#    Swapping only the variant head works because both variants carry
#    `shared_with` — the tail of the initialiser type-checks unchanged, so this
#    compiles and produces exactly the wrong classification rather than a build
#    error (a mutation that fails to compile proves nothing).
mutate "infra ejection masquerades as an unattributed code red" \
  's = s.replace("reason: EjectReason::Infrastructure {\n                                reason: reason.clone(),\n                                attempts,", "reason: EjectReason::Unattributed {\n                                fingerprints: Default::default(),", 1)' \
  "reports a build that never ran as a code verdict, so an author debugs a failure that was never diagnosed"

# ── the staging engine ───────────────────────────────────────────────────
#
# `stage` / `fail_fast` / `target_key` live in project_checks.rs, not lane.rs,
# and their tests are unit tests in that file rather than in lane_policy.rs.
# Same reasoning applies though: these three decide whether an expensive build
# starts at all, so a suite that passes regardless of whether they work would
# convert "we skip the release build on a type error" from a property into a
# hope.
#
# They mutate a different file and run a different suite, so they get their own
# backup/runner rather than being forced through the lane.rs `mutate` above.

ENGINE="$ROOT/crates/cargoless-core/src/project_checks.rs"
ENGINE_BACKUP=""
if [ -f "$ENGINE" ]; then
    ENGINE_BACKUP="$(mktemp)"
    cp "$ENGINE" "$ENGINE_BACKUP"
    # Extend the existing trap so BOTH files are restored on any exit path.
    restore() {
        cp "$BACKUP" "$SRC"; rm -f "$BACKUP"
        [ -n "$ENGINE_BACKUP" ] && cp "$ENGINE_BACKUP" "$ENGINE" && rm -f "$ENGINE_BACKUP"
    }
    trap restore EXIT
fi

run_engine_suite() {
    (cd "$ROOT" && cargo test -p cargoless-core --lib project_checks --locked) \
        >/tmp/lane-mutation-engine.log 2>&1
}

# $1 = name, $2 = python mutation on project_checks.rs, $3 = what it proves
mutate_engine() {
    local name="$1" py="$2" proves="$3"
    [ -n "$ENGINE_BACKUP" ] || return 0
    cp "$ENGINE_BACKUP" "$ENGINE"
    python3 - "$ENGINE" <<PY
import sys, pathlib
p = pathlib.Path(sys.argv[1])
s = p.read_text()
${py}
p.write_text(s)
PY
    if ! cmp -s "$ENGINE" "$ENGINE_BACKUP"; then
        if run_engine_suite; then
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

if [ -n "$ENGINE_BACKUP" ]; then
    echo "== staging-engine mutations =="

    echo "== baseline: the engine suite must PASS on unmutated code =="
    if ! run_engine_suite; then
        echo "FAIL: the engine suite is red BEFORE any mutation — fix that first" >&2
        tail -30 /tmp/lane-mutation-engine.log >&2
        exit 1
    fi
    echo "  ok — baseline green"

    # 5. Stages stop gating: a failed stage no longer stops the next one.
    #    This is THE property — without it the expensive release build runs
    #    even though the cheap check already rejected the candidate, and the
    #    whole staging feature is decoration.
    mutate_engine "stages do not gate (a red stage no longer halts the next)" \
      's = s.replace("if stage_red {\n            halted = Some(stage);\n        }", "if false {\n            halted = Some(stage);\n        }", 1)' \
      "runs a later stage after an earlier stage failed, so an expensive build is paid for a candidate already known bad"

    # 6. Fail-fast never trips. The in-flight sibling runs to completion on a
    #    candidate that cannot land — the exact waste the flag exists to stop.
    mutate_engine "fail_fast never trips" \
      's = s.replace("let tripped = fail_fast && result.required && result.tree == TreeState::Red;", "let tripped = false;", 1)' \
      "never cancels in-flight work after a required check has already gone red"

    # 7. Fail-fast trips on ANY red, including a non-required one. That would
    #    silently promote every advisory into a gate.
    mutate_engine "fail_fast trips on a non-required red" \
      's = s.replace("let tripped = fail_fast && result.required && result.tree == TreeState::Red;", "let tripped = fail_fast && result.tree == TreeState::Red;", 1)' \
      "cancels a build over a FAILING ADVISORY, promoting non-required checks into gates"

    # 8. target_key stops separating dirs, so every command check shares one
    #    target again and declared-parallel legs silently serialize.
    mutate_engine "target_key ignored (all legs share one target dir)" \
      's = s.replace("if check.target_key.is_empty() {\n        return base;\n    }", "if true {\n        return base;\n    }", 1)' \
      "puts every leg back on one CARGO_TARGET_DIR, so legs declared parallel serialize on cargo's .cargo-lock and the parallelism is imaginary"

    # 9. A skipped check is reported GREEN rather than red. Fails OPEN: a gate
    #    would read "did not run" as "passed".
    mutate_engine "skipped checks report green" \
      's = s.replace("fn skipped_stage_result(root: &Path, check: &CheckConfig, message: &str) -> ProjectCheckResult {", "fn skipped_stage_result(root: &Path, check: &CheckConfig, message: &str) -> ProjectCheckResult {\n    if true { let mut r = result_from_diags(check, Vec::new(), 0); r.tree = TreeState::Green; return r; }", 1)' \
      "reports a check that never ran as GREEN, so a gate reads 'unknown' as 'passed'"
fi

echo
if [ "$survivors" -ne 0 ]; then
    echo "FAIL: ${survivors} mutation(s) survived — the lane tests do not actually" >&2
    echo "      check the rule those mutations break." >&2
    exit 1
fi
echo "PASS: every mutation was caught; the lane policy tests have teeth."
