# cargoless v0 — Operator Fleet Deployment Report

**Task #153** · Owner: operator (orchestrator supports + routes findings) · Date: _TBD_
**Tested against:** `origin/main` from `4687e3c` onward
**Phase window:** _start-sha_ → _end-sha_
**Plan:** `/Users/iggy/.claude/plans/fluffy-dreaming-allen.md`

## TL;DR

_To be filled when phases complete._

cargoless v0 was launch-narrative-integrated on `main @ 4687e3c`. The operator chose **internal fleet dogfood first** (public Phase 4 deferred) with **comprehensive mini-dogfood** depth. This report captures the operator's fleet-deployment validation across 4 phases (install + smoke, mini-dogfood on actual project, fleet-scale ramp, orchestration integration), closes the 3 known-untested surfaces (macOS install if applicable, AC#4 full publish round-trip with trunk, F8 reconciliation post-#55-redo field-verify), and records the operator's Phase-4 decision.

### One-page status

| Lane | Verdict | Evidence |
|---|---|---|
| Phase 0 — install + single-worktree smoke | _TBD_ | _commit + measurement_ |
| Phase 1 — mini-dogfood on operator's actual project | _TBD_ | _per-finding refs_ |
| Phase 1 — AC#4 full publish round-trip (known-untested) | _TBD_ | _step-by-step round-trip log_ |
| Phase 1 — F8 reconciliation field-verify post-#55-redo | _TBD_ | _stream output + verdict cross-check_ |
| Phase 0 — macOS install if applicable (known-untested) | _TBD / N/A_ | _Mac install + smoke result_ |
| Phase 2 — fleet RSS at ~20 agents on operator's host | _TBD_ | _ps-summed RSS over 1+h_ |
| Phase 2 — stability over 1+h | _TBD_ | _crash count, orphan count, zombie trend_ |
| Phase 3 — orchestration integration | _TBD_ | _agent layer → cli-status/latest-green/exit-codes wired_ |

| Finding | Status |
|---|---|
| _(populate as findings surface)_ | _OPEN / FIXED / RESIDUAL / DEFERRED_ |

## Methodology

### Environment

- **Operator's dev host:** macOS aarch64 (Darwin 25.3.0, Apple Silicon); RAM budget _TBD_
- **Binary acquisition:** `cargo install --git https://github.com/TriformAI/cargoless.git cargoless --branch main --locked --features integration` (works today on `main @ 4687e3c` per `README.md:62-119`; no Phase 4 required)
- **Trunk binary:** already installed at `/Users/iggy/.cargo/bin/trunk` (required for `cargoless build --watch --out`; hard dep per FIELD FINDING #7 — satisfied, not a deferred gate)
- **Substrate:** operator's actual real project (NOT the Leptos bench fixture) — _project name, file count, LOC, dep profile_

### Scenarios run

| # | Phase | What | Result |
|---|---|---|---|
| 1 | 0 | `cargoless --version` after install | _TBD_ |
| 2 | 0 | Single-worktree `cargoless watch` cold-start → `.cargoless/cli-status` appears within ~30s | _TBD_ |
| 3 | 0 | Single-worktree edit `.rs` → verdict observable via `cargoless status` + stream output | _TBD_ |
| 4 | 0 | macOS smoke (if operator's host is Mac) — install + watch + verdict round-trip | _TBD / N/A_ |
| 5 | 1 | Baseline measurements on operator's project (cold-start latency, AC#2a + AC#2b save→verdict, per-daemon RSS) | _TBD_ |
| 6 | 1 | Workload-representative exercise (real agent-edit patterns, multi-file bursts, save-during-RA-reindex) | _TBD_ |
| 7 | 1 | AC#4 full publish round-trip (the known-untested surface) — see explicit log below | _TBD_ |
| 8 | 1 | F8 reconciliation field-verify post-#55-redo (commit `327f64e`) — verdict GREEN ≠ RA ERROR in stream | _TBD_ |
| 9 | 1 | Stability stress: dual-watch race (#93 guard), parent-shell SIGKILL (#88/#129), RA kill -9 (AC#6) | _TBD_ |
| 10 | 2 | Ramp to ~20 agents with chosen fleet config (Tier-3 default-safe + TF_RA_IDLE_EVICT=1 + --features csr where applicable) | _TBD_ |
| 11 | 2 | Fleet RSS measurement: `ps -o pid,rss,comm \| awk '/cargoless/ {sum+=$2} END {print sum/1024/1024 " GB"}'` over 1+h | _TBD_ |
| 12 | 2 | Stability over time (crashes, orphans, zombies trending) | _TBD_ |
| 13 | 3 | Orchestration integration: agent layer reads `.cargoless/cli-status` (`statusfile.rs:10-93`) | _TBD_ |
| 14 | 3 | Orchestration integration: agent layer reads `.cargoless/latest-green` (`cargoless-proto/src/lib.rs:422-474`) | _TBD_ |
| 15 | 3 | Orchestration integration: exit codes propagate (0/1/2 check, 0/3 status) | _TBD_ |

### Untested / residual (carry-over from PHASE-2-REPORT)

- _Update as Phase 0/1 closes the macOS / AC#4 / F8 gaps._

---

## Phase 0 — Install + single-worktree smoke

**Window:** _start-time_ → _end-time_

### Install path

```bash
TRIFORM_OPERATOR_APPROVED_BUILD=1 \
  cargo install --git https://github.com/TriformAI/cargoless.git cargoless \
  --branch main --locked --features integration
```

(`TRIFORM_OPERATOR_APPROVED_BUILD=1` required because the tf-multiverse `cargo-safety.sh` hook intercepts agent-driven cargo invocations by default; the operator's explicit `go ahead :)` is the sanctioned override path documented in the hook's own error message.)

**Result (2026-05-18, operator dev host):**
- Exit code: 0
- Binary location: `/Users/iggy/.cargo/bin/cargoless`
- Version: `cargoless 0.0.0` (pre-tag; `v0.1.0` not yet cut per Phase 4 deferral)
- Build commit: `4687e3c` (`Installed package cargoless v0.0.0 (https://github.com/TriformAI/cargoless.git?branch=main#4687e3c5)`)
- Build time: `7.14s release-profile optimized` (mostly warm cache; cold install will be longer)

**`trunk` binary:** ✅ ALREADY INSTALLED — `/Users/iggy/.cargo/bin/trunk` (27 MB ELF, Dec 28 2025 build), confirmed present by Track-1 recon 2026-05-18. **Not** a deferred operator gate (corrects the prior planning assumption). Hard dep per FIELD FINDING #7 is satisfied; **AC#4 full publish round-trip (§1.3) is UNBLOCKED NOW** — no operator `cargo install --locked trunk` step required on the critical exit path.

### Single-worktree smoke

- **Cold-start latency** (AC#1 = daemon up + watch-pipeline live, ≤30s per `D-A2-RENEGOTIATION.md` — NOT first-green): _measurement_
- **Edit → verdict observable:** _stream-output excerpt_
- **`cargoless status` output:** _key=value capture_
- **`.cargoless/cli-status` content** (post-startup): _file contents (verify schema=1, pid, root, started, updated, verdict)_

### macOS smoke (CLOSED for install + binary-runs; watch+verdict still pending)

**Operator host platform:** macOS aarch64 (Darwin 25.3.0, Apple Silicon).

- **Install result:** ✅ PASS (2026-05-18). `cargo install --git ... --features integration` succeeded under `TRIFORM_OPERATOR_APPROVED_BUILD=1` override; binary at `/Users/iggy/.cargo/bin/cargoless`; `cargoless --help` renders the documented 5-command surface (check / watch / build --watch --out / status / clean) with all documented flags (`--root`, `--debounce-ms`, `--proc-macro`, `--features`).
- **Watch + verdict round-trip:** _PENDING — requires a test project + operator authorization to spawn the watch daemon_.
- _Findings to date: none. Closes the install half of the macOS-untested-surface; full watch+verdict still needed for the runtime half._

---

## Phase 1 — Comprehensive mini-dogfood on operator's actual project

**Window:** _start-sha_ → _end-sha_
**Substrate:** _operator's project name, file count, LOC, key deps_

### 1.1 Baseline measurements

| Metric | Value | Notes |
|---|---|---|
| Cold-start latency (AC#1) | _TBD_ | _budget 30s; daemon up + pipeline live_ |
| Save→verdict AC#2a (RA hint) | _TBD_ | _target <1s per `D-A2-RENEGOTIATION.md §2`_ |
| Save→verdict AC#2b (cargo-check authoritative) | _TBD_ | _project-size-dependent; Leptos bench 20-25s_ |
| Per-daemon RSS at steady state | _TBD_ | _expect ~0.97 GiB at Tier-3 default-safe_ |

### 1.2 Workload-representative exercise

- **Real agent-edit patterns:** _what the operator's agents actually do (whole-file Writes / multi-file edits / etc)_
- **Multi-file edit bursts:** _behavior, latency_
- **Save while RA reindexing:** _race exposure, observed behavior_

### 1.3 AC#4 full publish round-trip (known-untested surface, closes `PHASE-2-REPORT.md:80` gap)

**Prerequisites:** `trunk` already installed (`/Users/iggy/.cargo/bin/trunk`, confirmed 2026-05-18 — no operator install step); `cargoless build --watch --out <DIR>` running.

Explicit step-by-step log:

| Step | Expected | Observed |
|---|---|---|
| 1. Green state established; `.cargoless/latest-green` present | green verdict in `cli-status`; pointer file exists | _TBD_ |
| 2. Capture `.cargoless/latest-green` byte hash | sha256 of file before any edit | _TBD_ |
| 3. Introduce compile error in source | _edit description_ | _TBD_ |
| 4. Red verdict observed in `cli-status` + stream | `verdict=red` within debounce + check latency | _TBD_ |
| 5. **`.cargoless/latest-green` byte-untouched** (AC#4 fail-closed) | sha256 unchanged from step 2 | _TBD_ |
| 6. Fix the compile error | _edit description_ | _TBD_ |
| 7. Green verdict observed | `verdict=green` within debounce + check + build latency | _TBD_ |
| 8. `.cargoless/latest-green` ADVANCES (new pointer) | sha256 differs from step 2; `published_at` newer | _TBD_ |

**Outcome:** _PASS / FAIL with details_

### 1.4 F8 reconciliation field-verify post-#55-redo (commit `327f64e`)

**Premise:** Per `PHASE-2-REPORT.md` F8, an earlier defect emitted `verdict=GREEN` simultaneously with `severity:Error` lines in the stream output. The fix re-aligned both to "severity:Error from ANY source drives RED."

**Test:** Run cargoless watch through operator's actual edit workload. Capture stream output. Cross-check:

- For every `verdict=green` interval: are there any `severity:Error` lines in the stream window? _Yes → CATCH (regression); No → PASS_
- For every RA-restart event: does the stream emit the "analyzer restarted" signal (FIELD FINDING #6 fix)? _Yes → PASS_

**Outcome:** _PASS / FAIL with stream-output excerpts_

### 1.5 Stability stress

- **Dual-watch race on same root** (`#93` guard expected to refuse cleanly): _observed_
- **Parent-shell SIGKILL** (orphan-daemon handling per `#88`/`#129`): _observed; stale `.cargoless/cli-status` auto-recovery worked? next `watch` proceeded?_
- **RA `kill -9`** (AC#6 supervisor respawn): _observed; daemon survived, RA respawned <1s_

### 1.6 Findings filed in Phase 1

_(populate as they surface — see FIELD FINDING template at end of report)_

---

## Phase 2 — Fleet-scale ramp to ~20 agents

**Window:** _start-time_ → _end-time_
**Worktree count at peak:** _N_
**Host RAM budget:** _16 GB / _other_

### 2.1 Fleet config chosen

- [ ] Tier-3 `--proc-macro disabled` (already default-safe per `#126` field-verified `#130`)
- [ ] `TF_RA_IDLE_EVICT=1` opt-in (mechanism per `docs/design/D-IDLE-EVICT.md`)
- [ ] `--features csr` (if narrowable; not all projects)
- [ ] Skip `TF_STRUCTURAL_TRIGGER=1` (per `AC7-THROUGHPUT-REPORT §9.5` `#117` anchor — agent fleet shows ~0% OPEN-rate so trigger gives ~0% CPU benefit)

### 2.2 Fleet RSS measurement

```bash
ps -o pid,rss,comm | awk '/cargoless/ {sum+=$2} END {print sum/1024/1024 " GB"}'
```

| Sample | Wall-time | RSS sum | Notes |
|---|---|---|---|
| 1 | T+0 (post-ramp) | _TBD_ | _initial state_ |
| 2 | T+30min | _TBD_ | _after idle period_ |
| 3 | T+60min | _TBD_ | _during active agent work_ |
| 4 | T+90min+ | _TBD_ | _sustained_ |

**Per-daemon RSS at steady state:** _average_, _max_

**Compared to AC7 §11.2 extrapolation:** _within / exceeds_ the ~19.4 GiB BORDERLINE at Tier-3 default-safe.

### 2.3 If fleet exceeds budget — decision tree

- [ ] First lever: enable `TF_RA_IDLE_EVICT=1` (if not already)
- [ ] Second lever: enable `--features csr` (if projects narrowable)
- [ ] Escalate to v1 Model B+ (`docs/design/D-FLEET-SHARED-DAEMON.md`) — operator may fleet-deploy at <20 agents pending B+, or accept higher RAM budget

### 2.4 Stability over time

| Metric | T+30min | T+60min | T+90min+ |
|---|---|---|---|
| Daemon crashes (cumulative) | _TBD_ | _TBD_ | _TBD_ |
| Orphan `.cargoless/cli-status` files | _TBD_ | _TBD_ | _TBD_ |
| Zombie RA count per check (per `#61` trajectory 3.7→1.75→~1.25) | _TBD_ | _TBD_ | _TBD_ |

---

## Phase 3 — Orchestration integration

**Window:** _start-time_ → _end-time_

### 3.1 `.cargoless/cli-status` consumption

Operator's orchestration layer parses `.cargoless/cli-status` (schema=1 key=value text per `crates/cargoless/src/statusfile.rs:10-93`). Validate:

- [ ] schema=1 field detected; orchestrator handles schema-version forward-compat
- [ ] `pid` field used for liveness probe (`kill(pid, 0) == 0` per `statusfile.rs:245`)
- [ ] `updated` field heartbeat tracked (5s cadence; stale-after 15s per `statusfile.rs:37,41`)
- [ ] `verdict` field consumed for agent next-action handoff
- [ ] Atomic read assumed (temp+rename pattern per `statusfile.rs:122-133` — never torn)

### 3.2 `.cargoless/latest-green` consumption

Operator's orchestration layer parses `.cargoless/latest-green` (`cargoless-latest-green/v1` versioned text codec per `crates/cargoless-proto/src/lib.rs:422-474`). Validate:

- [ ] schema header (`cargoless-latest-green/v1`) detected; orchestrator handles forward-compat
- [ ] `input_hash` consumed as CAS key for downstream artifact lookup
- [ ] `published_at` consumed as artifact-freshness anchor
- [ ] AC#4 fail-closed contract honored: when present, pointer reflects the latest verified-green build; pointer absence = no green build ever succeeded (not "stale red")

### 3.3 CLI exit code consumption

| Command | Exit codes | Orchestrator handles |
|---|---|---|
| `cargoless check` | 0=green, 1=red, 2=setup-error | _TBD_ |
| `cargoless status` | 0=live, 3=not-live | _TBD_ |
| `cargoless watch` | 0=clean-shutdown, 1=pipeline-disconnect, 2=setup-error | _TBD_ |
| `cargoless clean` | 0=done, 1=fs-error, 2=unsafe-path | _TBD_ |

### 3.4 Dual-tier latency expectation encoded in orchestrator

- [ ] AC#2a hint (<1s) used for quick agent-feedback (fast-fail-on-syntax-error)
- [ ] AC#2b authoritative (project-size-dependent) used for agent ready-to-proceed gate

---

## Exit criteria (gating Phase-4 public-launch decision)

- [ ] Phase-0 install + single-worktree smoke GREEN on operator's actual project + dev host (both macOS aarch64 + Linux x86_64 if fleet spans both)
- [ ] Phase-1 AC#4 full publish round-trip verified end-to-end with trunk installed
- [ ] Phase-1 F8 reconciliation field-verified post-#55-redo
- [ ] Phase-2 fleet RSS at operator's actual ~20-agent count fits host budget
- [ ] Phase-2 stability over 1+ hour: no daemon crashes; no orphan cli-status accumulation; zombie-RA count not regressing past `#61` trajectory
- [ ] Phase-3 orchestration integration works end-to-end
- [ ] Zero LAUNCH-BLOCKER-class findings
- [ ] This report committed to `docs/dogfood/OPERATOR-FLEET-DEPLOYMENT-REPORT.md`

---

## Phase 4 — Internal-dogfood verdict (operator decision)

**Date:** _TBD_

**Decision:** ☐ Ready for Phase 4 public launch (proceed to `docs/release/PHASE-D-OPERATOR-HANDOFF.md` runbook) ☐ Fix items first (file as FIELD FINDING tasks, route dev-fixer pattern, retest) ☐ Hold longer (continue internal use; revisit when more data accumulates) ☐ Abandon

**Rationale:** _operator notes_

---

## FIELD FINDING template (for any findings surfaced during phases)

> Copy this block per finding. Mirror PHASE-2-REPORT shape.

### F_N — _short-name_ (SEVERITY)

**Phase:** 0 / 1 / 2 / 3
**Severity:** LAUNCH-BLOCKER / MEDIUM-HIGH / MEDIUM / LOW
**Status:** OPEN / FIXED + VERIFIED (`commit-sha`) / RESIDUAL / DEFERRED

**Repro:**
```
_minimal reproducible steps + commands + observed output_
```

**Expected:** _what should have happened_
**Observed:** _what did happen_
**File:line citation:** _relevant code path_

**Route:** _operator-fixed inline / routed to dev-fixer pattern as #N / accepted as residual / etc._

**Verification (post-fix):** _re-run repro on fix commit, paste outcome_

---

## References

- Plan: `/Users/iggy/.claude/plans/fluffy-dreaming-allen.md`
- Cargoless v0 launch narrative: `README.md`, `ROADMAP.md`, `docs/launch/BLOG-DRAFT.md` (post-#38 fold @ `4687e3c`)
- Phase-2 dogfood reference: `docs/dogfood/PHASE-2-REPORT.md`
- AC7 fleet-scale numbers: `docs/bench/AC7-THROUGHPUT-REPORT.md §11`
- Dual-tier latency: `docs/design/D-A2-RENEGOTIATION.md §2`
- RAM ladder + config knobs: `docs/design/D-RAM-TIERS.md`
- v1 architectural parking lot: `docs/design/D-FLEET-SHARED-DAEMON.md`
- Phase-4 runbook (for after this report's verdict): `docs/release/PHASE-D-OPERATOR-HANDOFF.md`
