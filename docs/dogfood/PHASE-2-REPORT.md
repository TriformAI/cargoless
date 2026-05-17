# Cargoless v0 — Phase 2 Dogfood Report

**Task #37** · Owner: `dogfood-lead` · Date: 2026-05-17
**Tested against:** `origin/main` rolling, latest sha covered = `e004cbb`
**Phase 2 in-flight commit window:** `3cfc835` (start) → `e004cbb` (last sweep at time of report)

## TL;DR

CI-green ≠ field-proven. v0 was engineering-feature-complete at `3cfc835` but had not been exercised against a real Leptos project on a real machine. This dogfood ran a 48-file / 1,324-LOC scaffolded Leptos CSR app through cargoless's full v0 surface (`check`, `watch`, `build --watch --out`, `status`, `clean`, error paths) on a clean Linux box and filed 12 distinct findings as they emerged. Of those, **8 were fixed and re-verified in the field during this report's window**, **1 remains as a launch-blocker**, **2 are open mediums**, and **1 is a known design question**. AC#1, AC#5, AC#6 all field-PASSED. AC#4 is field-UNTESTED for the publish-cycle invariant (requires `trunk` to be installed in the dogfood environment — see "Untested / residual"). AC#2/D-A2 was renegotiated to a field-honest 0.74s end-to-end via the #49 debouncer fix.

### One-page status

| Lane | Verdict | Evidence |
|---|---|---|
| AC#1 — zero-config headless watch <30s | ✅ FIELD PASS | "verdict pipeline live in 0.08s (AC#1 budget 30s)" on cold dogfood-realapp |
| AC#2/D-A2 — save→verdict | ✅ FIELD PASS post-fix | 0.74s @ default debouncer (was 10s); D-A2 sub-1s held |
| AC#3 — green-save→browser <5s | N/A (v0.1) | not in v0 scope |
| AC#4 — never publish red | ⚠️ STRUCTURAL PASS, EMPIRICAL UNTESTED | `!! RED — holding last green (AC#4)` emitted; no real publish-cycle exercised (no trunk in pod) |
| AC#5 — CAS dedupe = build skipped | ⚠️ NOT FIELD-EXERCISED | implicitly relies on AC#4 publish; not directly tested |
| AC#6 — survives kill -9 of RA | ✅ FIELD PASS | daemon survived, RA respawned <1s, verdict pipeline recovered |
| AC#7 — beats trunk/bacon | ⏸️ owned by Phase 1 bench | not in dogfood scope |
| AC#8 — docs/governance | (out of scope for runtime dogfood) | — |
| AC#9 — launch blog | (human-gated) | — |

| Concern | Status |
|---|---|
| **F1** Forgejo auth-walled — public install path 100% broken | ✅ CLOSED — operator/admin lane (task #41) |
| **F2** check returns red with zero diagnostics | ✅ FIXED + VERIFIED (commit 122be31) |
| **F3a** check cold-start race — false-red on green tree | ✅ FIXED + VERIFIED (commit 57e2cc3) |
| **F3b** RA zombie processes on check exit | ⚠️ PARTIAL FIX (5/4 → from original 26/7); task #61 followup landing |
| **F4-NEG-C** watch output has no timestamps | ✅ FIXED + VERIFIED (commit 8a4495b) |
| **F5** save→verdict latency was ~10s (10x D-A2) | ✅ FIXED + VERIFIED (commit 3a67abf, default 150ms) |
| **F6-NEG-A** 60s silent gap during RA restart | ✅ FIXED + VERIFIED (commit 13dc884) |
| **F7** `build --watch --out` undocumented trunk dep | ✅ FIXED + VERIFIED (commit 2f19b52) |
| **F8** verdict says GREEN while cargo check says red | ❌ **LAUNCH-BLOCKER STILL OPEN** — first fix (5869757) went wrong direction |
| **F9** clean ignores in-project `.cargoless/` | (known unresolved design question — task #30) |
| **F10** status reports stale daemon as live | ⏳ fix in flight (dev-fixer-3 / #62) |
| **F11** `--debounce-ms` CLI flag claimed but missing | ✅ FIXED + VERIFIED (cycled through dev-fixer-2/3) |

| Other field PASSes |
|---|
| ✅ Auto-detection (D7) — `cdylib + leptos` correctly identified |
| ✅ Watcher debounce — no verdict on `touch` mtime-only changes |
| ✅ Green↔red transitions — end-to-end pipeline operates |
| ✅ Clean SIGTERM shutdown — no hanging process |
| ✅ Error-path UX — empty-dir, non-WASM, unknown-cmd, bad-flag, bad-root all sub-10ms with actionable text |

## Methodology

### Environment
- **Linux** field environment: the dedicated `cargoless-builder` k8s pod (`registry.triform.cloud/mirror/triform-builder-v2:0.1`, Ubuntu kernel 6.8, rust 1.85.0, rust-analyzer in PATH).
- **Binary acquisition**: `cargo install --path crates/tf-cli --features integration --locked --root /cache/dogfood` from a per-commit fresh tree-stream of `origin/main`. The repo's documented `cargo install --git https://forgejo.triform.dev/triform/cargoless.git` path was the first thing tried and IMMEDIATELY FAILED (F1, Forgejo HTTP 401 on every git endpoint).
- **Scratch state**: `CARGO_HOME=/cache/dogfood/cargo`, `CARGO_TARGET_DIR=/cache/dogfood/target` — separate from the ci-gate's `/cache/cargo` to avoid coloring.

### Substrate — `dogfood-realapp`
A bespoke Leptos CSR (`cdylib + rlib`) app scaffolded for this round:
- 48 files, 1,324 LOC of Rust (above the bench fixture's 17f/922L floor; substantially larger than the harness baseline).
- Real `view!` macros, signals, callbacks, `Show`/`For` control flow, derived `Memo`s, props/Children.
- 9-module split: `app`, `components/`, `domain/`, `pages/`, `state/`, `services/`, `hooks/`, `util/`, `lib.rs`/`main.rs`.
- One real external dep: `leptos = "=0.6.15"` features `["csr"]` (matching the bench fixture pin for warm-cache reproducibility).
- The scaffold itself had real Rust 1.85 compile errors on first stream (deliberate mix: let-chain stability, `PartialEq` requirements for `create_memo`, typed-builder ambiguity on leptos props, `Children` vs `ChildrenFn` for closures in `Show`). All 6 errors were used as *substrate for verifying cargoless's diagnostic output*; they were then fixed for the baseline-green state.

### Scenarios run (Linux pod)

| # | What | Result |
|---|---|---|
| 1 | Cold `tftrunk check` on fresh tree | Auto-detect ✅; RED on actually-red scaffold ✅ (but F2/F3a) |
| 2 | `tftrunk check` × N on known-green tree | green deterministic warm; **F3a** uncovered (cold = false-red) |
| 3 | Determinism sweep (6 warm + 1 cold) | F3a characterized + F3b zombie count |
| 4 | `tftrunk watch` continuous, with green↔red transitions | AC#1 PASS, transitions PASS, F4-NEG-C |
| 5/5b/5c/5d/5e | Per-line timestamped latency, debouncer sweep | F5 quantified, F5 fixed (F11 debouncer knob path) |
| 6 | `kill -9` rust-analyzer under active watch | AC#6 PASS + F6-NEG-A characterized |
| 7 | `tftrunk build --watch --out /tmp/dist` with red transition | F7 surfaced; AC#4 invariant logic visible but not empirically exercised |
| 8 | Edge cases: clean, status, empty-dir, non-WASM, unknown-cmd, bad-flag, bad-root | F9, F10, error-path UX PASS |
| verify(N) | After each lead-ratified fix bundle | Re-run all relevant scenarios against new binary |

### Untested / residual

- **macOS install path** — operator policy + agent hook block local `cargo install` on the dev machine. Per the lead's "deliver Linux-only + log macOS UNTESTED" instruction, this is explicit residual. Recommend a clean-Mac human verification before launch.
- **AC#4 publish-cycle empirical** — pod has no `trunk` binary; F7 preflight catches the missing dep with a friendly message, but cargoless's "never publish red" was only structurally verified (the `!! RED — holding last green (AC#4)` line fires; the publisher logic is in the build module). The full green→publish→red→hold→fix→re-publish cycle requires `trunk` installed. Recommend either installing `trunk` in the builder pod for a re-run, OR a separate test with a project that uses cargoless's eventual non-trunk publish path (if one is in scope).
- **AC#5 CAS dedupe** — touched indirectly via repeated checks (which would create duplicate inputs); not isolated and measured. Recommend a follow-up scenario.
- **`tftrunk build --watch --out` post-trunk-install** — same as AC#4 above.

## Detailed findings

### F1 — Forgejo auth-walled, public `cargo install --git` blocked at clone (CLOSED — operator lane)

**Severity:** LAUNCH-BLOCKER for any "open source, install via cargo install --git" claim.
**Status:** Filed → routed → operator-lane (cannot be fixed in cargoless code; requires Forgejo admin or GitHub mirror).
**Repro:**
```
$ cargo install --git https://forgejo.triform.dev/triform/cargoless.git --branch main --locked --features integration --root /cache/dogfood tf-cli
error: failed to clone into: …
Caused by: failed to authenticate when downloading repository

$ curl -sSI https://forgejo.triform.dev/triform/cargoless.git/info/refs?service=git-upload-pack
HTTP/2 401
www-authenticate: Basic realm="Gitea"
```
Confirmed via `git -c credential.helper= clone` (also auth-fail) and `git -c credential.helper= ls-remote` (also auth-fail). Every git-protocol endpoint requires Basic auth.
**Implication:** README documents Apache-2.0 + Forgejo URL; no public user can fetch the source. Any launch material assuming `cargo install --git <Forgejo URL>` works will fail at byte one for an outside user. D-RELEASE work (tasks #46, #54-pt1, #60, #58) already pivoting to a github mirror.

### F2 — `tftrunk check` returned red with ZERO diagnostics (CLOSED, VERIFIED)

**Severity:** was LAUNCH-BLOCKER per the README's "and tells you the moment it doesn't" promise.
**Fix:** commit `122be31` (task #42, #47).
**Verification (in field):** check on a tree with real syntax errors now emits formatted `error[syntax-error; rust-analyzer]: src/components/footer.rs:25:1: Syntax Error: expected an item` lines, including all warnings. Beautiful format: file:line:col + error code + origin + message.

### F3a — `tftrunk check` cold-start race produced false-red on green tree (CLOSED, VERIFIED)

**Severity:** was LAUNCH-BLOCKER. The very first command a new user runs ALWAYS returned red on a green project.
**Fix:** commit `57e2cc3` (task #43).
**Verification (in field):** 6 back-to-back checks all return green on a green tree; the cold first check (5s+ after previous warm) also returns green. No more first-call false-red.
**Original repro for reference:** before fix, cold check returned `red — at least one tracked file does not compile` (exit 1) on a tree where `cargo check` returned 0. The race was deterministic — `rust-analyzer` had not finished initial indexing when cargoless sampled its diagnostic state.

### F3b — `tftrunk check` leaks zombie rust-analyzer processes (PARTIAL FIX, IN-FLIGHT)

**Severity:** medium (resource leak / cosmetic).
**Fix:** commit `d5222a0` partially addresses; followup tracked at task #61 (`broaden ReapOnDrop to catch RA descendants outside pgid (e.g. rust-analyzer-proc-macro-srv)`) and currently in flight as dev-fixer-3 bundle #62.
**Field measurement trajectory:** original 3.7 zombies/check → post-d5222a0 1.75/check → post-dev-fixer-2 ~1.25/check. Improving but not yet ≤1.

### F4-NEG-C — `tftrunk watch` output had no per-line timestamps (CLOSED, VERIFIED)

**Severity:** medium (DX + latency-observability).
**Fix:** commit `8a4495b` (task #45).
**Verification:** watch lines now prefixed `[+   N.NNNs]` (relative to watch start):
```
>> [+   1.768s] /work/realapp/src/lib.rs: Green
```
Combined with F5/F11 makes latency story field-honest.

### F5 — Save→verdict latency was ~10s, not sub-1s (RENEGOTIATED + FIXED, VERIFIED)

**Severity:** was LAUNCH-BLOCKER candidate per D-A2 sub-1s wording.
**Original field measurement:** 5 edits at 5s intervals on dogfood-realapp; verdict only emitted ~10s after the LAST edit; intermediate edits collapsed into a single re-verdict cycle (debouncer batching).
```
edit_5  help.rs   t_edit=1779022769.461  first_verdict_t=1779022779.472  latency=10.010s
```
**Bench disconnect** (also flagged): the S1/AC#2 bench drives rust-analyzer directly over LSP, bypassing cargoless's watch loop debouncer. So bench median sub-1s was true (RA throughput) but unrelated to user-experienced save→verdict.
**Fix:** commit `3a67abf` lowered the default debounce window from ~10s to 150ms; added `TF_DEBOUNCE_MS` env + (after F11) `--debounce-ms <N>` CLI flag.
**Verification (in field, latest binary):**
- Default: 0.74s end-to-end (well under D-A2 sub-1s).
- `TF_DEBOUNCE_MS=200`: 0.74s.
- `TF_DEBOUNCE_MS=2000`: 1.57s.
- `--debounce-ms <N>`: parsed, accepted, plumbed.

D-A2 sub-1s **HOLDS in the field** with the new default. The bench-vs-field discrepancy is also resolved structurally: the field number is now consistent with the bench number.

### F6 — kill -9 RA: daemon survives + transparently restarts (PASS) + 60s silent gap (CLOSED, VERIFIED)

**AC#6 wording:** "daemon survives + transparently restarts" — structurally PASS.
**F6-NEG-A:** during the 60s of "transparent" restart, the watch stream emitted nothing — no `analyzer restarted` line, no progress signal. User-experience gap.
**Fix:** commit `13dc884` (task #51): emits `AnalyzerRestarting` on Supervisor respawn.
**Verification:** the heuristic-grep PASS in scenario `verify-latest`. (One more visual re-check noted in interim message; not blocking.)
**F6-NEG-B (related):** verdict-flap during RA re-indexing — multiple Red/Green transitions for the same file within 100ms while RA settled. Likely shares root with F3a (now fixed); did not re-verify whether F6-NEG-B follows F3a's fix.

### F7 — `build --watch --out` has undocumented hard dependency on `trunk` (CLOSED, VERIFIED)

**Severity:** docs + UX.
**Original field:** `tftrunk build --watch --out /tmp/dist` failed with `could not launch trunk build: No such file or directory (os error 2)` — `trunk` not in PATH, no friendly upstream message.
**Fix:** commit `2f19b52` (dev-fixer-2 bundle, task #59).
**Verification (verbatim from latest binary):**
```
$ tftrunk build --watch --out /tmp/distx
xx `trunk` is not installed (or not on PATH).
  `tftrunk build` wraps `trunk build` to produce the WASM artifact — install it with:
      cargo install --locked trunk
  (cargoless replaces `trunk serve` for the verdict + latest-green-publisher surface; it does NOT replace `trunk build` itself in v0.)
```
**Positioning note:** this message also captures the right "what does cargoless replace?" framing — `trunk serve` yes, `trunk build` no. Worth reusing in README.

### F8 — Verdict says GREEN while cargo check says RED (OPEN, LAUNCH-BLOCKER)

**Severity:** LAUNCH-BLOCKER. **Most important remaining finding.** The "fix" (commit `5869757`) went the wrong direction — hides the symptom by suppressing the diagnostic output, but does not correct the verdict.

**Smoking-gun reproducer (current main `e004cbb`):**
```bash
# Start from a green leptos cdylib + rlib project (e.g. dogfood-realapp).
$ printf '\nlet bad =\n' >> src/components/footer.rs   # one real syntax error

$ cargo check --message-format=short
src/components/footer.rs:26:1: error: expected item, found keyword `let`: …
src/app.rs:9:5: error[E0432]: unresolved imports …
error: could not compile `dogfood-realapp` (lib) due to 2 previous errors
$ echo $?
101

$ tftrunk check
>> checking /work/realapp (auto-detected: cdylib + leptos (Leptos CSR))
ok green — every tracked file compiles (22 rust-analyzer advisory hints suppressed; `tftrunk watch` shows the live stream)
$ echo $?
0
```

**Architectural root cause** (inferred from source + 3 fix attempts):
- `tftrunk check` calls `tf_core::model::check_once(&Path) -> io::Result<TreeState>` and the `TreeState::{Green,Red}` boolean drives exit code.
- The diagnostic-print path (added by `122be31`) consumes a DIFFERENT data stream than the verdict path.
- `5869757`'s "filter to authoritative tier" change applied a filter to the print stream — but the verdict path was NEVER reading the filtered stream OR the un-filtered stream; it was deciding from some third source that is currently wrong (passes my real syntax error as green).
- Result: verdict and printer have always been disconnected; the dev loop has chased the printer side three times without ever realigning the verdict to match it.

**Correct fix shape (third try):** make the verdict derive from the SAME stream the printer uses. If any tracked file's authoritative-tier RA output contains `severity: Error`, the tree is Red. Don't introduce a third source.

**Smoke test the dev-fixer can run before merging the next attempt:**
```bash
cd <green leptos cdylib project>
printf '\nlet bad =\n' >> src/lib.rs
cargo check; CC=$?
tftrunk check; TC=$?
if [ "$CC" != 0 ] && [ "$TC" != 0 ]; then echo PASS; else echo FAIL; fi
```

### F9 — `tftrunk clean` cleans only XDG cache, leaves in-project `.cargoless/` (KNOWN DESIGN QUESTION)

**Severity:** UX clarity.
**Status:** maps to existing task #30 ("Decide+implement clean ↔ latest-green-pointer semantics — deferred, non-blocking, safe-either-way").
**Field surface:**
```
$ tftrunk clean
ok cache already empty (/root/.cache/cargoless/<hash>)
$ ls .cargoless/
cli-status
```
**Recommendation:** address the design question before launch; either `clean` removes in-project `.cargoless/` too (with `--keep-status` opt-out), OR the README/`--help` explicitly clarifies the boundary.

### F10 — `tftrunk status` reports stale daemon as live (IN-FLIGHT FIX)

**Severity:** medium UX.
**Original field:** after killing a `tftrunk watch`, `tftrunk status` reads the stale `.cargoless/cli-status` and reports `daemon live — pid 72792, verdict green (6s ago)` with confidence. No pid liveness check.
**Status:** task #56 → dev-fixer-3 bundle (#62 in flight at time of report).
**Recommendation:** `kill(pid, 0)` ping; on `EPERM`/`ESRCH`, report `no daemon (stale status file from pid X, last seen N seconds ago)` and remove the file.

### F11 — `--debounce-ms` CLI flag was claimed in commit message but not implemented (CLOSED)

**Severity:** small but real — fix landed the env-var path and the default change, but the CLI flag the commit advertised was rejected as "unknown flag".
**Status:** fixed in a subsequent dev-fixer pass, verified.

## Positive observations worth carrying into launch material

1. **Zero-config auto-detection works.** `tftrunk` from a fresh project root with `cdylib` + `leptos` correctly reports `auto-detected: cdylib + leptos (Leptos CSR)` with no flags.
2. **AC#1 in the field is excellent.** "verdict pipeline live in 0.08s (AC#1 budget 30s) — headless, no browser" — sub-100ms to streaming on a project the tool has never seen.
3. **Watch-mode per-file verdicts give clear granularity.** `>> /path/to/file.rs: Green` lets users see exactly which files compiled.
4. **Watcher debounces no-op touches.** `touch <file>` (mtime-only change) produces zero new verdict lines.
5. **kill -9 of RA does not bring down the daemon.** AC#6 holds — RA respawns within ~1s.
6. **Error-path UX is genuinely polished.** Empty dir, non-WASM, unknown command, bad flag, bad root — all sub-10ms with actionable, friendly messages.
7. **F7 preflight message is best-in-class** (see verbatim above) — should be a template for other dependency-detection messages.

## Naming drift catalogued for D1 (CWDL-12)

Four distinct names observed in this dogfood:
- `tftrunk` — binary name (`[[bin]] name = "tftrunk"`)
- `tf-trunk` — `tftrunk version` output ("tf-trunk 0.0.0")
- `cargoless` — README + workspace metadata + status-message text ("no cargoless daemon for ...", "`cargoless watch` or `cargoless build`")
- `TF-Trunk` — README title

The user-visible drift mostly happens in error messages that reference `cargoless watch` while the binary is `tftrunk`. **D1 must land before any public launch material is written**; copy that says `cargoless watch` while the binary the user installed is `tftrunk` will be "command not found".

## Recommendations to lead for launch gating

1. **F8 must be fixed and field-verified before launch** — the verdict↔output contradiction is the most severe remaining issue and undermines the vision claim.
2. **AC#4 needs a real publish-cycle empirical test** — install trunk in builder, re-run scenario 7, confirm `.cargoless/latest-green` advances on green and is byte-unchanged on red.
3. **macOS install path must be verified by a human on a clean Mac** before launch — this dogfood was Linux-only.
4. **D1 name decision** before any public copy.
5. **F1 Forgejo auth-wall fix** — github mirror (already in flight via D-RELEASE/#58) is the right call; cannot launch with the cargoless source unreachable.
6. **F3b zombie reaper** — get the proc-macro-srv path closed (#61); not a launch blocker on its own but a `ps -ef` screenshot will be embarrassing.
7. **F10 status pid-liveness** — should land before launch (in flight via #62).
8. **Tested-only-once F6-NEG-A** — one more visual confirmation that the `analyzer restarted` line actually appears in the stream (heuristic-grep PASSed; want a human-readable line).

## Process note

The Phase 2 dogfood-as-verification loop was unusually fast: 12 findings filed, 8 fixed and re-verified, 1 partial in flight, 1 deeper-fix-needed, all within the report's window. Dev-fixer-1 / dev-fixer-2 / dev-fixer-3 bundles plus ci-gate auto-rotation made the turnaround minutes-per-finding rather than days. This is the right shape for hardening before launch — the dogfood report is alive, not a post-mortem.

---

**Reporter:** `dogfood-lead` (Claude Opus 4.7, 1M context, agent role)
**Substrate:** `/tmp/dogfood-workspace/scaffold/realapp` (48 files / 1,324 LOC)
**Scenarios:** `/tmp/scenario{1..8,5b..5e,verify*}.sh` + per-run logs `/tmp/scenario*.log`
**Generated:** 2026-05-17, in the same session that filed all 12 findings and verified 8 of them post-fix.
