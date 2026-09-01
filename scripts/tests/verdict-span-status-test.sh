#!/usr/bin/env bash
#
# verdict-span-status-test.sh — a span status must measure the SPAN, not the
# verdict it carries.
#
# The bug class this closes (INFRA-819)
# -------------------------------------
# The `verdict.publish` span set `otel.status_code` from the resolved verdict:
#
#     let otel_status = match verdict {
#         Green                  => "OK",
#         Red | Unknown          => "ERROR",
#     };
#
# A non-green verdict is the daemon's normal, correct work product. The publish
# itself succeeded, so the span was stamped ERROR while carrying no failure
# payload at all — measured over 7d on the live install, all 2068 error spans
# had `exception.type` empty and `status_message` empty.
#
# The stated justification was a paging contract: Red and Unknown both belong in
# SigNoz's `hasError=true` filter so "dashboards that currently page on Red"
# also catch Unknown. Measured 2026-09-01, that contract had no consumer — 20
# alert rules exist cluster-wide, exactly one reads the traces signal, and it is
# scoped to triform-physics `http.route`. Meanwhile the ONE real consumer read
# the mark as a defect: the "Cargoless Gate Health" dashboard's `Daemon Error
# Spans` widget counts `has_error = true OR status_code = 2` and documents "0 =
# healthy". `verdict.publish` alone put a permanent floor of 2068 under it —
# 74.4% of all published verdicts — so the over-marking destroyed the very
# instrument that would show a real daemon fault.
#
# Why a guard and not just the fix
# --------------------------------
# The mapping was DELIBERATE and its comment argued for it persuasively. The
# next author reaching for "operators should see unknown verdicts in the error
# filter" will find the same reasoning just as compelling, because the reasoning
# is only wrong against a measurement that lives outside this repo. So the thing
# to pin is the rule, in the file, where that author is standing.
#
# What this asserts
# ------------------
# Behaviour of the span site, never wording:
#
#   1. `otel.status_code` is not set on the `verdict.publish` span at all
#   2. no status string is computed by matching on a `Verdict` — the specific
#      shape that reintroduces the defect even under a different attribute name
#   3. the verdict outcome still REACHES the wire, as span attributes
#      (`verdict_color` + `verdict_failure_reason`), so this fix can never be
#      "achieved" by deleting the signal instead of relocating it
#   4. the sibling `verdict.project_checks` span likewise sets no status
#
# Assertion 3 is the load-bearing one. Dropping the status is only correct
# because the outcome is carried elsewhere; a future edit that removes
# `verdict_color` would leave this guard green while making the daemon's
# outcome unqueryable — the same defect from the other side.
#
# Usage:      scripts/tests/verdict-span-status-test.sh [--self-test]
# Exit:       0 = span status measures the span, 1 = it does not, 2 = setup
#
# --self-test proves the assertions FIRE: it reintroduces the defect four ways
# in throwaway copies and requires this script to go red each time. A check that
# cannot fail is not a check.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DRV="${SERVEDRV_OVERRIDE:-$ROOT/crates/cargoless/src/servedrv.rs}"

[ -r "$DRV" ] || { echo "verdict-span-status: cannot read $DRV" >&2; exit 2; }

# The two span bodies, read out of the source. `info_span!(` ... `)` — the
# terminating line is the `)` at the macro's own indentation, so the awk range
# stops at the end of the macro call and not at some nested paren.
span_body() { # $1 = span name
    awk -v want="\"$1\"" '
        $0 ~ /tracing::info_span!\(/ { buf = $0; grab = 1; named = 0; next }
        grab {
            buf = buf "\n" $0
            if (index($0, want)) named = 1
            if ($0 ~ /^    \)/) { if (named) print buf; grab = 0 }
        }
    ' "$DRV"
}

# ── --self-test: prove the assertions FIRE ────────────────────────────────
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

    # 1. the original defect, restored verbatim.
    awk '
        /^    let _span = tracing::info_span!\($/ && !done {
            print "    let otel_status = match verdict {"
            print "        statusfile::Verdict::Green => \"OK\","
            print "        statusfile::Verdict::Red | statusfile::Verdict::Unknown => \"ERROR\","
            print "    };"
            done = 1
        }
        /^        analysed_at = now,$/ && !inj { print; print "        otel.status_code = otel_status,"; inj = 1; next }
        { print }
    ' "$DRV" >"$tmp/drv-restored.rs"
    ( SERVEDRV_OVERRIDE="$tmp/drv-restored.rs" ; export SERVEDRV_OVERRIDE
      probe "the verdict-keyed otel.status_code is restored" )

    # 2. the same defect under a different attribute name — the rule is
    #    "no status computed from a Verdict", not "this one identifier".
    awk '
        /^    let _span = tracing::info_span!\($/ && !done {
            print "    let span_state = match verdict {"
            print "        statusfile::Verdict::Green => \"OK\","
            print "        statusfile::Verdict::Red | statusfile::Verdict::Unknown => \"ERROR\","
            print "    };"
            done = 1
        }
        /^        analysed_at = now,$/ && !inj { print; print "        otel.status_code = span_state,"; inj = 1; next }
        { print }
    ' "$DRV" >"$tmp/drv-renamed.rs"
    ( SERVEDRV_OVERRIDE="$tmp/drv-renamed.rs" ; export SERVEDRV_OVERRIDE
      probe "a verdict-keyed status under a different local name" )

    # 3. the outcome stops reaching the wire — "fixed" by deleting the signal
    #    rather than relocating it.
    sed '/^        verdict_color = verdict.as_str(),$/d' "$DRV" >"$tmp/drv-no-color.rs"
    ( SERVEDRV_OVERRIDE="$tmp/drv-no-color.rs" ; export SERVEDRV_OVERRIDE
      probe "verdict_color stops being published" )

    # 4. the sibling span picks the defect up.
    awk '
        /"verdict.project_checks",$/ && !inj { print; print "        otel.status_code = \"ERROR\","; inj = 1; next }
        { print }
    ' "$DRV" >"$tmp/drv-sibling.rs"
    ( SERVEDRV_OVERRIDE="$tmp/drv-sibling.rs" ; export SERVEDRV_OVERRIDE
      probe "verdict.project_checks starts setting a status" )

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

publish_body="$(span_body verdict.publish)"
checks_body="$(span_body verdict.project_checks)"

[ -n "$publish_body" ] || { echo "verdict-span-status: verdict.publish span not found in $DRV" >&2; exit 2; }
[ -n "$checks_body" ]  || { echo "verdict-span-status: verdict.project_checks span not found in $DRV" >&2; exit 2; }

# ── 1. neither span sets an OTEL status ───────────────────────────────────
echo "== a lifecycle span carries no status of its own =="
for pair in "verdict.publish:$publish_body" "verdict.project_checks:$checks_body"; do
    name="${pair%%:*}"; body="${pair#*:}"
    if grep -q 'otel\.status_code' <<<"$body"; then
        fail "$name sets otel.status_code — a span status must measure the span's own execution, not the verdict it reports (INFRA-819)"
    else
        ok "$name sets no otel.status_code"
    fi
done

# ── 2. no status string is DERIVED from a verdict anywhere in the file ────
# Catches the rename path: the defect is the derivation, not the identifier.
# Scoped to a match on a `Verdict` whose arms produce OK/ERROR strings.
echo "== no span status is derived from a verdict outcome =="
derived="$(awk '
    /match verdict \{/ { buf = $0; grab = 1; n = 0; next }
    grab {
        buf = buf "\n" $0
        if ($0 ~ /"(OK|ERROR)"/) n++
        if ($0 ~ /^    \};/) { if (n > 0) print buf "\n--"; grab = 0 }
    }
' "$DRV")"
if [ -n "$derived" ]; then
    fail "a match on Verdict produces OK/ERROR status strings — this is the INFRA-819 defect regardless of what the local is called:"
    printf '%s\n' "$derived" >&2
else
    ok "no Verdict match yields an OK/ERROR status string"
fi

# ── 3. the outcome still reaches the wire ─────────────────────────────────
# Dropping the status is only correct because the outcome is carried as
# attributes. Without this, the fix could be "achieved" by deleting the signal.
echo "== the verdict outcome still reaches the wire as attributes =="
for attr in verdict_color verdict_failure_reason red_diagnostics; do
    if grep -q "^ *${attr} = " <<<"$publish_body"; then
        ok "verdict.publish publishes ${attr}"
    else
        fail "verdict.publish no longer publishes ${attr} — the outcome must be QUERYABLE, and relocating it off the span status is the only reason dropping that status is safe (INFRA-36 / INFRA-819)"
    fi
done

echo
if [ "$fails" -ne 0 ]; then
    echo "FAIL: ${fails} assertion(s) failed." >&2
    exit 1
fi
echo "PASS: span status measures the span; the verdict outcome rides attributes."
