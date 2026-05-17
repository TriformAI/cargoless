# Cargoless v0 — Phase 2 Dogfood Report: Post-Report Addendum

**Companion to** [`PHASE-2-REPORT.md`](PHASE-2-REPORT.md). Durable record of the
post-report verification round, the RA-polish RAM A/B, and the #117
agent-OPEN-rate field anchor. Author: `dogfood-lead`. Date window:
2026-05-17 → 2026-05-18. Binary era: `tftrunk` (pre-D1) → `cargoless`
(post-D1 #87). SHAs spanned: `3cfc835` … `3baf659`.

> The original report closed with F8 + F12 as open launch-blockers and AC#4
> empirically untested. This addendum records: every one of those closing
> out, the field-verification of the fix loop, the launch RAM number, and
> the structural-trigger field anchor — so the honest provenance is a
> durable artifact, not just message traffic.

---

## 1. Post-report verification round — fix-loop field-verified

Every fix routed from the original report's findings was re-verified
in-field against the deployed binary (not just CI-green):

| Finding | Fix | Field-verification result |
|---|---|---|
| F1 Forgejo auth-walled | GitHub mirror | ✅ anon `cargo install --git github.com/TriformAI/cargoless` works end-to-end |
| F2 no diagnostics on red | 122be31 | ✅ file:line:col + error code surfaced |
| F3a cold-start race | 57e2cc3 | ✅ cold check returns green on green tree |
| F3b RA zombie leak | d5222a0 + eff42c1 | ⚠️ improved 3.7→1.0 zombies/check; residual is v1 double-fork class |
| F4-NEG-C no timestamps | 8a4495b | ✅ `[+ N.NNNs]` prefix present |
| F5 ~10s save→verdict | 3a67abf | ✅ 0.74s default (D-A2 sub-1s held); `--debounce-ms`/`TF_DEBOUNCE_MS` wired |
| F6-NEG-A silent RA-restart | 13dc884 | ✅ analyzer-restart signal emitted |
| F7 trunk dep undocumented | 2f19b52 | ✅ friendly preflight: "install with: cargo install --locked trunk" |
| **F8 verdict↔output contradiction** | 327f64e+0be545b | ✅ **CLOSED** — `cargo check`=101 ↔ `cargoless check`=1 agree; "N errors, M warnings surfaced" |
| F9 clean vs in-project .cargoless | (task #30) | known design question — deferred, documented |
| F10 stale daemon as live | 7e3e46b | ✅ stale-pid detected, exit 3, sub-10ms, actionable msg |
| F11 `--debounce-ms` missing | (cycled) | ✅ flag accepted + documented in `--help` |
| **F12 build misreads cargo success** | 470cd37+2945278+3c8ee8e | ✅ **CLOSED** — see §3 |
| F13a dual-watch cli-status race | #93 | ✅ second watcher refused |
| F13b orphan daemon survives kill | #88 | ✅ fixed |
| F13c symlink unlinked-file flood | (RA-polish didn't target) | ⚠️ persists, low-sev (verdict still correctly GREEN; noisy per-file annotations on symlinked trees only) |
| §gap-3 BUILD_ID/banner drift | #89 | ✅ **field-verified** — `--version`/`--help`/`watch`-banner all render `cargoless 0.0.0` consistently; compound-drift case from the field cross-check RESOLVED |
| D1 name decision | #87 = `cargoless` | ✅ field-verified — zero `tftrunk`/`tf-trunk` residual in version/help/banner |

**12 original findings → 10 fully closed+field-verified, 1 partial (F3b,
v1 territory), 1 deferred-design-question (F9). Plus F13a/b/c (all fixed
bar low-sev F13c) and §gap-3 (fixed+field-verified) surfaced and closed in
the post-report round.** No finding was closed on CI-green alone — each was
re-exercised against the deployed binary on the real Leptos project.

## 2. Item 2 — post-RA-polish functional verification

Against `cargoless` @ 3baf659 (RA-polish #74/#90 + D1 #87 + BUILD_ID #89):

| Check | Result |
|---|---|
| AC#1 cold-start | ✅ pipeline live 0.07s; first GREEN 3.5s (budget 30s) |
| **flycheck still fires post-RA-polish** | ✅ syntax error → RED in **1.0s** (the softened `checkOnSave` did NOT break the verdict transition — the headline RA-polish risk, explicitly disproven) |
| red→green recovery | ✅ recovers, verdict not stuck |
| F8 smoking-gun still holds | ✅ cargo=101 ↔ cargoless=1, agree |
| F2 diagnostics still emitted | ✅ surfaced |
| F13c symlink flood | ⚠️ persists (176 hints; RA-polish out of scope for it) |

**Net: RA-polish is functionally safe. No verdict regressions.**

## 3. AC#4 — empirical publish-cycle (closed the report's gap)

The original report left AC#4 "STRUCTURAL PASS, EMPIRICAL UNTESTED". After
F12 fix + a pod-environment fix (corrupted `wasm-bindgen` in trunk's cache
— `file` reported it as `data`, not ELF; `rm -rf ~/.cache/trunk` forced a
valid re-download — a setup-note, not a cargoless bug), the full cycle was
exercised:

- **F12 CLOSED**: post-fix `cargoless build --watch --out` genuinely
  publishes — pointer written (`cargoless-latest-green/v1` + input_hash +
  target + profile + published_at), `dist/` materialized (35 KB JS + 3.5 MB
  wasm + index.html), stream line `ok published <hash> → <dir> (at Ns)`.
  F12b also fixed: "nothing published yet (--out unchanged)" replaces the
  misleading "holding last green" when there is no prior green.
- **AC#4 invariant — BOTH HALVES field-verified ✅**:
  - red transition → pointer **byte-unmoved** (sha + inode + mtime + size
    all unchanged); `!! RED — holding last green (AC#4)` fires.
  - green recovery → pointer **advances atomically** (~5s latency).
- **AC#5 CAS dedupe ✅**: edit+revert to identical content → pointer
  unchanged (input_hash identity; silent dedupe).
- **Item 6(a) mid-build SIGINT ✅**: pointer byte-identical post-interrupt;
  no torn write; dist intact.

## 4. Item 3 / 3-redo — RA-polish RAM A/B (the launch RAM number)

**Provenance: deployed-binary FIELD number** (`cargoless` real
dogfood-realapp Leptos, paced-edit-loop, per-process RSS breakdown).
Distinct from the harness per-tier isolation (bench-lead #119) — the two
compose under the §8.5 two-source discipline; they do not conflict.

| Config | RSS_sum peak | Δ vs pre-polish baseline | RA peak | proc-macro-srv | CPU mean |
|---|---|---|---|---|---|
| pre-polish baseline (`tftrunk`) | 2135.9 MiB | — (anchor) | 1989.2 | ~146 | 21.0% |
| post-RA-polish **default** | 1710.3 MiB | **−19.9%** | 1597.5 | 109.6 | 26.7% |
| + `--proc-macro disabled` | 947.0 MiB | **−55.7%** | 943.9 | 0.0 | 20.0% |
| + `--features csr` | 473.1 MiB | **−77.8%** | 444.3 | 25.7 | 7.6% |

**FINDING — default RA-polish delivered −19.9%, BELOW the 30-50% expectation.**
Reported, not buried. The lean InitOpts default moves ~20%; RA itself is
still ~1.6 GiB on a macro-heavy Leptos project. The 30-50%+ only
materializes with the proc-macro lever (−56%) and feature-narrowing
(−78%, CPU collapses to 7.6%).

**Launch-narrative recommendation:** the honest RAM claim is the **tiered
ladder** (default −20% / proc-macro-off −56% / features-narrowed −78%),
NOT the weak single default number. The proc-macro-off −56% is the
launch-worthy figure **iff** task #126 (RA-native-downrank no-wrong-verdict
proof) makes proc-macro-off safe for proc-macro projects. #126 is the
pivotal unlock that converts this measured −56% into a shippable claim.
CPU caveat: default mean CPU rose slightly (21.0→26.7%, within noise);
proc-macro-off and csr both also reduce CPU.

## 5. #117 — agent-OPEN-batch rate field anchor

**Method integrity:** local-cargo hook-blocked Mac-side → bench-lead's
pre-authorized fallback: faithful Python reimplementation of
`tf_core::structural::is_closed` (agent/bench-lead @ 91247cf),
**zero-drift-gated against structural.rs's own 35-assertion `#[cfg(test)]`
oracle — 35/35 PASS before any real data classified**. Mac-local,
read-only, aggregates-only; no transcript content persisted/quoted.

**Dataset:** N=16 fleet session transcripts, 207 `Write` tool_use events,
append-only (survivorship-free — intermediate-OPEN states ARE captured,
unlike git commit-tips which only preserve CLOSED survivors).

**Headline trap, and the decomposition that inverts it:**

Naive all-files per-Write OPEN-rate = **26.57% (55/207)** → would map
MODERATE (≈25%). **This is a trap — do not anchor on it.**

| ext | total/open | %open |
|---|---|---|
| **`.rs`** | **97 / 0** | **0.0%** |
| `.toml` `.py` `.yaml` `.html` `.json` | 16 / 0 | 0.0% |
| `.sh` | 44 / 30 | 68.2% |
| `.md` | 41 / 17 | 41.5% |
| `.yml` `.draft` `.gitignore` | 6 / 6 | 100% |

`is_closed` is a **Rust** balance-lexer. The 55 OPENs are almost entirely
**non-Rust files** (`.sh` heredocs, `.md` fences, `.yml`) where the Rust
lexer legitimately reads their normal syntax as "unbalanced" — a
**predicate-domain artifact, not real OPEN intermediate code**.
cargoless's structural-trigger only ever evaluates the files cargo-check
processes — `.rs` (code-validated by bench-lead against `model.rs`'s
`structural.record` call-site, which filters the batch to `.rs` before
`is_closed`). This fleet's **`.rs` OPEN-rate = 0% (0/97, mean 4.3 KB/file
— substantive files, not stubs)**. Batch-rate == per-Write rate (this
fleet emits ≤1 Write per assistant turn; 0 multi-write turns).

**Anchor (folded into bench-lead §9.5 @ a57f343):** cargoless-team fleet,
N=16, 97 `.rs` Writes, survivorship-free → **`.rs` OPEN-rate ≈ 0% →
CONSERVATIVE floor**. The structural-trigger's fired-check-reduction for
this fleet's edit style ≈ ~0% — these agents (Claude + whole-file Write)
emit complete, syntactically-closed Rust essentially always. The trigger
is a real *mechanism* (the idle-evict enabler + only-meaningful-states
cached correctness property) but **not a material direct-CPU lever for
this population**. The 26.6% all-files figure must NOT anchor the
trigger's expected savings.

**Honest caveats:** single fleet, single agent-family (Claude), this
toolset; Write-only (the methodologically clean primary per the §8.6/§9
whole-file-atomic-Write input model); Edit-hunk path unmeasured —
documented gap (an Edit-heavy agent doing many small broken intermediate
hunks would land higher; different population; v0.1 follow-up if the
operator wants it, not launch-gating). N small-but-real.

## 6. Honest residuals (carried forward)

- **F13c** — symlinked source tree → RA `unlinked-file` hint flood.
  Low-sev; verdict still correctly GREEN; affects symlink-based monorepo
  layouts only. Open, documented, not launch-gating.
- **macOS install path** — UNTESTED. Local-cargo is hook-blocked on this
  agent; requires a non-hook-blocked clean-Mac human verification.
  Continuing residual, honestly stated, not delegable from this team.
- **#117 Edit-tool path** — unmeasured (Write-only primary). Documented
  gap, not launch-gating.
- **F3b RA-zombie residual** — improved to ~1.0/check; the remaining
  double-fork-class escapees are v1 territory.

## 7. #126 — proc-macro-off-default safety field-verify (Leg-B RAM rung)

Field-validation that converts the §4 measured −56% RSS
(`--proc-macro disabled` = −55.7%) from *measured-but-unsafe-as-default*
into a **shippable launch RAM claim**. SCOPE: correctness/safety, NOT a
re-measurement. Binary `cargoless 0.0.0` @ main `493173a` (Tier-3
`33f0838`, flag `TF_RA_PROCMACRO_OFF=1`, default-off). Substrate:
dogfood-realapp Leptos — **20 files / 38 `view!` proc-macro call-sites**
(the genuine worst case for proc-macro-off). Ground truth: `cargo check`
rc=0 on the green tree.

| Test | Result |
|---|---|
| CONTROL — default (proc-macro ON), green tree | green, rc=0, 4.82s |
| **(a)** `TF_RA_PROCMACRO_OFF=1`, green tree → must stay GREEN | ✅ **PASS** — `ok green — every tracked file compiles`, rc=0; **zero `view!`/unresolved/macro mentions leaked into the verdict**. RA cannot expand `view!` without the proc-macro server, but the verdict is rustc-sourced and stays correctly GREEN — no hallucination. |
| **(a-watch)** `TF_RA_PROCMACRO_OFF=1 watch`, steady-state | ✅ **PASS** — settles `GREEN — tree compiles`; **0 RED-summary lines** on the known-green view!-heavy tree (no false-RED flap/transient) |
| **(b)** real error + `TF_RA_PROCMACRO_OFF=1` → must RED | ✅ **PASS** — `cargo check` rc=101, cargoless rc=1, both RED, agree; diagnostics rustc/syntax-sourced. No false-GREEN. |

**Verdict: proc-macro-off-as-default is SAFE on real `view!`-macro-heavy
Leptos.** dev-fixer's no-wrong-verdict proof holds in the field on the
real project (not just the fixture): does not false-RED a green Leptos
tree, does not false-GREEN a broken one. **Leg-B (−56% RAM safety rung)
CONFIRMED.**

### (c) Latency — predicted tradeoff INVERTED to a bonus

| Mode | latency edit→RED |
|---|---|
| default (proc-macro ON) | 25.8s |
| `TF_RA_PROCMACRO_OFF=1` | 5.1s |

proc-macro-off was ~5× **faster** to the RED verdict, not slower —
mechanistically, the proc-macro server's `view!` expansion sits on the
verdict critical-path on macro-heavy code; removing it shortens the path.
The design proof conservatively predicted a fast-RED latency *cost*; the
field shows a latency *improvement* here.

> **Load-bearing honest-scoping caveat (must travel with the number):**
> this is the dogfood-realapp measurement — ONE real `view!`-heavy
> project, **n=1 per mode**. The direction is unambiguous and
> mechanistically expected on macro-heavy code, but the claim is framed
> as *"no latency penalty observed; faster on macro-heavy projects"* —
> **NOT a universal speedup guarantee**. A non-macro-heavy project would
> not show this inversion and could exhibit the originally-predicted
> small fast-RED latency cost; that case is unmeasured here.

**Launch-claim shape (recommended):** *"proc-macro-off default: −56% RSS
on a real view!-heavy Leptos project, verdict-correctness preserved
(rustc authority), with faster (not slower) red-verdict latency on
macro-heavy code"* — with the n=1 caveat above attached. The tiered RAM
ladder's middle rung (§4) is now load-bearing-safe.

**Provenance:** aggregate verdicts + latencies only; no raw
transcript/source content. Verification: `/tmp/verify_126.sh` (+ logs);
reproducible from the committed scaffold.

---

**Reporter:** `dogfood-lead` (Claude Opus 4.7, 1M context, agent role)
**Substrate:** `/tmp/dogfood-workspace/scaffold/realapp` (48 files / 1,324 LOC, leptos 0.6.15 csr)
**Companion source-of-truth docs:** [`PHASE-2-REPORT.md`](PHASE-2-REPORT.md) ·
[`../launch/NAMING-DRIFT-FIELD-CROSS-CHECK.md`](../launch/NAMING-DRIFT-FIELD-CROSS-CHECK.md) ·
bench-lead §9.5 (#117 anchor) · D-RAM-TIERS / #118 / #126 (RAM ladder)
**Generated:** 2026-05-18, in the session that produced all the above
measurements. §1–6 committed @ `493173a`; §7 (#126 field-verify) appended
post-tier3-relay, same session, routed for the same docs-pickup pattern.
