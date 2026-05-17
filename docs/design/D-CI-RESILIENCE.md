# D-CI-RESILIENCE — release/CI single-point-wedge audit + hardened topology

**Owner:** `builder-infra` · **Task:** CWDL #123 · **Status:** PROPOSAL
(propose-only; the lead routes anything v0-blocking — no design change
is landed by this doc beyond what was already authorized in `3baf659`).

**Why this exists.** CWDL-71 Phase C (the throwaway-`v0.0.0` rehearsal)
caught **three** real launch-blockers that were invisible to static
review and only a live tag-fire exposed:

1. `tag-validate` `grep -A1` workspace-version extraction (fixed `9f0c9cf`)
2. anonymous Forgejo `s1-ac2-verdict` read under `REQUIRE_SIGNIN_VIEW`
   (fixed `c033aa3` + operator `FORGEJO_READONLY_TOKEN`)
3. `gh release upload <tag>` against a non-existent Release object
   (a tag push does NOT auto-create a Release) — fix proposed, pending route

…plus a topology fragility (one unavailable free-tier `macos-13` runner
wedged the **entire** release via an all-legs `needs:`) **fixed in
`3baf659`** by decoupling the Intel leg.

Pattern: **"one external/single thing unavailable or wrong ⇒ the whole
launch (or merge gate) wedges or silently ships wrong, with weak or no
signal."** This doc audits the whole topology (`release.yml` +
`.forgejo/workflows/ci.yml` + Forgejo↔GitHub mirror + cargoless-builder
pod + cross-system coupling) for every member of that class.

---

## 0. Topology recap (where the single points live)

```
 operator ── git tag vX ──► Forgejo (forgejo.triform.dev, triform/cargoless)
                              │  ├─ .forgejo/workflows/ci.yml  (build/test/fmt/clippy/bench)
                              │  │     bench POSTs `s1-ac2-verdict` commit status
                              │  └─ push-mirror (sync_on_commit) ──► GitHub (TriformAI/cargoless)
                              │                                        └─ .github/workflows/release.yml
                              │                                             tag-validate ─┐
                              │   release.yml@GitHub  ◄── FORGEJO_READONLY_TOKEN ──────────┘
                              │   reads s1-ac2-verdict back FROM Forgejo (cross-cloud)
 cargoless-builder pod ◄── scripts/ci-gate (kubectl exec) — the merge gate
```

Three trust/availability crossings stand out: **(i)** the mirror is the
*only* path a tag reaches the thing that builds the release; **(ii)**
`release.yml`@GitHub reaches *back* into Forgejo for the verdict (secret +
cross-cloud + Forgejo-uptime dependency on the critical tag path);
**(iii)** the verdict *producer* (Forgejo `bench`) and *consumer*
(`release.yml tag-validate`) are decoupled in time — the producer can
fail silently and the consumer only finds out at tag time.

---

## 1. Fragility register

Severity key — **V0-BLOCK**: must fix before the real `v0.1.0` tag ·
**HARDEN-NOW**: cheap, high-leverage, recommend landing in v0 ·
**V0.1**: real but deferrable · **RESIDUAL**: accept with a documented
operator runbook check.

### F-A — Runner-monoculture all-legs wedge — RESOLVED (`3baf659`)
*Exemplar of the class.* `attach-release-assets: needs: build` waited on
**every** matrix leg; one unavailable free-tier `macos-13` runner (queued
87 min, never assigned) wedged the entire release/asset upload while
ubuntu + macos-14 + tag-validate were all green. **Blast radius:** total
launch wedge from an infra event entirely outside our control.
**Fix (landed `3baf659`):** structural decouple — `build` matrix =
2 REQUIRED legs only (ubuntu x86_64-linux + macos-14 aarch64-darwin);
Intel x86_64-darwin is a standalone `continue-on-error` job that nothing
`needs:` and that self-attaches its tarball; `attach-release-assets`
gates on the required legs only. Re-fire **validated** the decouple
(attach ran without the stuck leg) and **re-confirmed** the macos-13
stall is systemic (queued/never-assigned twice). Intel prebuilt is now a
bonus; `cargo binstall` source-falls-back for x86_64-darwin (documented
v0 known-limitation).

### F-B — `gh release upload <tag>` with no Release object — V0-BLOCK (Phase-C Finding #3, fix proposed)
A tag push creates a *tag*, **not** a GitHub *Release* object;
`release.yml`'s comment "GitHub auto-creates a release for a pushed tag"
is false. `attach-release-assets` → `Upload assets → failure`
(`release not found`). **Blast radius:** green builds, **zero** published
assets — the real `v0.1.0` would be an empty release; `cargo binstall`
and the asset URLs all 404. **Proposed hardening (pending lead route):**
idempotent + race-safe at *both* upload sites (`attach-release-assets`
and the `build-darwin-x86-besteffort` self-upload):
```
gh release view "$TAG" >/dev/null 2>&1 \
  || gh release create "$TAG" --title "cargoless $TAG" \
       --notes "Automated release for $TAG." --verify-tag || true
gh release upload "$TAG" <files> --clobber --repo "$REPO"
```

### F-C — Cross-cloud verdict read on the critical tag path — V0-RESIDUAL + V0.1-HARDEN
`tag-validate` (GitHub) authenticates back into Forgejo to read
`s1-ac2-verdict`. Single secret `FORGEJO_READONLY_TOKEN`; depends on
forgejo.triform.dev being **up and reachable from GitHub at tag time**.
**Blast radius:** secret missing/expired/revoked OR Forgejo down/slow at
tag time ⇒ `tag-validate` hard-fails ⇒ total launch block — at the worst
moment (operator mid-launch-sequence). Sub-points: (i) no token-rotation
/ expiry strategy — a silently-expired read-only token is a launch-day
surprise; (ii) the assertion cannot distinguish "verdict genuinely
absent" from "Forgejo unreachable" — both look like "no s1-ac2-verdict".
**Proposed hardening:**
- **V0.1 (preferred long-term): Option-2 verdict-mirroring** — have the
  Forgejo `bench` job *also* publish the verdict where GitHub can read it
  natively (a GitHub commit status on the mirrored SHA, or a release
  note), so `tag-validate` never crosses to Forgejo. Removes the secret,
  the cross-cloud hop, and the Forgejo-uptime dependency from the critical
  path in one move.
- **V0-RESIDUAL (now): operator runbook** — PHASE-D §3 pre-tag checklist
  gains "FORGEJO_READONLY_TOKEN present + a 1-line authed probe returns
  the verdict for the launch SHA" and "forgejo.triform.dev reachable"
  immediately before cutting the tag. Cheap; converts a launch-time
  surprise into a pre-flight check.
- Make `tag-validate`'s s1-ac2 step distinguish *HTTP/transport failure*
  (Forgejo down) from *empty array* (verdict absent) with distinct
  actionable `::error::` messages (extends the `c033aa3` defensive check).

### F-D — Mirror is a silent single point for tag arrival — V0-RESIDUAL (runbook) + V0.1-MONITOR
`release.yml` only fires when the **GitHub mirror** receives the tag
(Forgejo→GitHub push-mirror, `sync_on_commit`, ~3–15 s observed).
**Blast radius:** if the mirror is paused/broken, the operator pushes the
launch tag and **nothing happens anywhere** — no run, no error, no signal
(the worst failure mode: silent non-launch). **Proposed hardening:**
- **V0-RESIDUAL (now):** PHASE-D §0 launch runbook gains an explicit
  post-push verification: "within 60 s, `v0.1.0` is on
  github.com/TriformAI/cargoless **and** a `release.yml` run has started;
  if not → mirror health (Forgejo repo → Settings → Mirroring) and the
  documented `git push github vX.Y.Z` manual fallback." (PHASE-D §0
  currently says the operator does NOT push github — keep that as the
  norm but document the explicit break-glass.)
- **V0.1:** a lightweight mirror-health monitor / a CI job that asserts
  last-mirror-sync age.

### F-E — Verdict producer fails *silently* — HARDEN-NOW (recommend v0)
`.forgejo/workflows/ci.yml` `bench` POSTs `s1-ac2-verdict` with
`curl … || true` and does **not** assert the POST returned 2xx. If the
Forgejo actions token (`secrets.GITHUB_TOKEN`) lacks status-write, or the
endpoint changes, **the status silently never posts** and CI stays green
— the break is invisible until a future `tag-validate` fails the s1-ac2
assertion (compounds with F-C; misattributed as "no bench run").
**Blast radius:** launch-time discovery of a long-broken verdict pipe.
**Proposed hardening (cheap, high-leverage — same make-silent-loud
spirit as Phase-C Findings 1–3):** the `bench` step asserts the POST
HTTP is 2xx and **fails the step** otherwise — a broken verdict-post then
reds `main` at *commit* time (loud, immediate) instead of surprising the
operator at tag time. Recommend landing in v0; routing to lead.

### F-F — CI base-image / apt monoculture — V0.1-HARDEN + V0-RESIDUAL
Every Forgejo CI job (`build/test/fmt/clippy/bench`) is
`runs-on: docker`, `image: rust:1.85-bookworm`, and `apt-get install
nodejs` at job start. **Blast radius:** Docker Hub rate-limit / pull
failure ⇒ **all** Forgejo CI red simultaneously ⇒ no green main ⇒ no ff
**and** no `s1-ac2-verdict` (compounds into a launch block). Transient
Debian-mirror apt failure ⇒ spurious red. **Proposed hardening:**
**V0.1** — pre-bake a CI image (node already present; eliminates the
per-run apt + the documented checkout gotcha) and pull it from
`registry.triform.cloud` (already used for the builder image) instead of
Docker Hub, removing Docker Hub as a shared single point. **V0-RESIDUAL**
— transient apt failure = re-run (documented).

### F-G — Gate-self-reference hazard — V0-RESIDUAL (→ #96) + V0.1 self-detect
`scripts/ci-gate`'s `-p <pkg>` orchestration runs from the *invoking*
checkout, not the streamed tree. A package-rename commit gated by a
pre-rename script false-REDs the integ tier (this bit #95 — diagnosed,
not a defect). **Blast radius:** wasted "code is broken" investigation;
caught only by discipline. **Proposed hardening:** the ci-gate
self-consistency note already queued for **#96**; **V0.1** — ci-gate
self-detects (diff its package list vs the streamed tree's workspace
members; warn on mismatch instead of false-RED).

### F-H — 404 log routes = diagnosis-time fragility — V0-RESIDUAL (documented) + V0.1
Forgejo returns 404 on REST log routes; job-granularity is the only
observability. **Blast radius:** not a wedge, but a *MTTR multiplier*
under launch pressure — a red gate with no log forces source-reasoning.
Mitigated by the one-job-per-check design + the CLAUDE.md
diagnose-from-source heuristic. **Proposed hardening:** **V0.1** — a
failure-only CI step that lifts the failing command's tail into a
commit-status/artifact (self-describing failure within the 404
constraint), same trick the `bench` job already uses for the verdict.

### F-I — cargoless-builder single pod / PVC — V0-RESIDUAL (dev-velocity only)
`scripts/ci-gate` kubectl-execs one pod (single replica + warm PVC; the
git-archive-mtime trap is already mitigated by the `#81` mtime-touch).
**Blast radius:** pod down/evicted ⇒ the *merge gate* is unavailable ⇒
all ff blocked — **dev-velocity, not launch-correctness** (the release
pipeline is GitHub-side and independent of this pod). **Proposed
hardening:** **V0-RESIDUAL** — verify the Deployment restartPolicy +
readiness probe and add a "rebuild the builder pod" runbook;
**V0.1** — HA only if gate-availability becomes a measured pain.

### F-J — Operator-manual crates.io publish (2nd credential gate) — V0-RESIDUAL (runbook) + V0.1 automate
`publish-*` are `if: false`; the first crates.io publish is operator-run:
4 crates, topological order, ~30 s propagation waits, `CRATES_IO_TOKEN`.
**Blast radius:** a misstep (order / token scope / propagation race) ⇒
*partial* crates.io state; rollback is **yank-only** (append-only
registry). **Proposed hardening:** **V0-RESIDUAL** — already in PHASE-D
§2.2; add an operator publish-preflight (`cargo publish --dry-run` all 4
+ token-scope check + `cargo metadata` dep-order assertion) to de-risk
the manual step. **V0.1** — publish automation once token-rotation is
designed (already a tracked 0.2.0 concern).

### F-K — Gate-tier non-determinism under concurrent load — HARDEN-NOW (recommend v0) + V0.1
The `cargoless-builder` gate pod runs all 7 ci-gate checks
(build/test/fmt/clippy + integ-*) against a streamed tarball; under
concurrent gate load it exhibits **non-deterministic failures that a
zero-change re-gate clears** — the dangerous signature, because it can
both *spuriously red a good SHA* (velocity tax) and *mask a real
failure* (a green that should be red). Two live observations:
- **#122 gate1:** `integ-build`+`integ-test` SIGKILL'd `exit 137`
  (OOM) while build/test/fmt/clippy/integ-clippy passed on the *same
  SHA*; same-SHA re-gate → ALL GREEN. → memory-bound under concurrent
  load (on-thesis: corroborates the operator's RAM priority).
- **#123 bundle step-4 (this audit's own integration gate):** `clippy`
  RED with `error finding Clippy's config: No such file` +
  `couldn't read crates/tf-cli/tests/diagnostics_field_finding_2.rs:
  No such file` — on a file that demonstrably exists; 6/7 green on the
  identical archived tree (incl. `integ-clippy` compiling the same
  tests). FS/tar-stream race, not a lint. Same-SHA re-gate → 7/7.
**Blast radius:** a launch-critical gate that is non-deterministic
erodes trust in every verdict; the masking direction is the severe one
(a memory-evicted check can `exit 137` *or* skip work and look green).
**Proposed hardening:** **HARDEN-NOW (recommend v0)** — serialize the
two high-memory integ steps (don't run `integ-build`/`integ-test`
concurrently with the base tier) and/or raise the gate-pod memory
request/limit + cap concurrent gate jobs to 1 (the gate is already a
serialized merge gate by intent — enforce it at the pod). **V0.1** —
a re-gate-on-transient-signature auto-retry *with an explicit
"transient-retry" annotation* so a masked real failure can't hide
behind silent retries (retry must be visible, bounded, and logged).
Interim mitigation IN USE: diagnosed same-SHA re-gate (never a blind
retry — distinguish "file-not-found on an existing file / exit 137" =
transient from a real compile/lint error by reading the failure +
checking the other 6 checks on the identical tree first).

---

## 2. Classification summary (lead routes V0-BLOCK / HARDEN-NOW)

| ID | Fragility | Class | Status |
|----|-----------|-------|--------|
| F-A | Runner-monoculture all-legs wedge | (exemplar) | **RESOLVED `3baf659`** |
| F-B | No Release object for `gh release upload` | **V0-BLOCK** | fix proposed, pending route (Phase-C Finding #3) |
| F-C | Cross-cloud verdict read on tag path | V0-RESIDUAL + V0.1 | runbook (now) + Option-2 (v0.1) |
| F-D | Mirror = silent single point for tag arrival | V0-RESIDUAL + V0.1 | runbook check (now) + monitor (v0.1) |
| F-E | Verdict producer fails silently (`\|\| true`) | **HARDEN-NOW** | **LANDED this commit** — ci.yml POST asserts HTTP 2xx → fail-loud (run.sh exit-0 evidence semantics preserved) |
| F-F | CI base-image / apt monoculture | V0.1 + residual | pre-bake+mirror (v0.1) |
| F-G | Gate-self-reference (`-p` from invoker) | V0-RESIDUAL + V0.1 | → #96 note (now) + self-detect (v0.1) |
| F-H | 404 log routes (diagnosis MTTR) | V0-RESIDUAL + V0.1 | documented + self-describe (v0.1) |
| F-I | Single builder pod/PVC | V0-RESIDUAL | dev-velocity only; runbook |
| F-J | Operator-manual crates.io publish | V0-RESIDUAL + V0.1 | preflight (now) + automate (v0.1) |
| F-K | Gate-tier non-determinism under concurrent load (OOM/FS-race) | **HARDEN-NOW** | serialize integ + gate-pod mem/concurrency-cap (v0); visible-bounded auto-retry (v0.1). 2 live observations (#122 OOM, #123 FS-race) |

**Recommended before the real `v0.1.0` tag:** F-B (in flight) and **F-E**
(silent-producer → loud, cheap one-liner), plus the F-C/F-D **operator
runbook pre-flight checks** folded into PHASE-D §0/§3 at Phase-D-finalize
(no code, pure de-risk). Everything else is honest v0.1 / accepted
residual with the rationale above.

## 3. Cross-cutting principle

Every fragility here is an instance of the same discipline the Phase-C
findings taught: **a silent or single-path dependency must be made loud
and/or decoupled before launch.** 3baf659 decoupled (F-A); F-B/F-E make
silent-fail loud; F-C/F-D add a pre-flight so a launch-time surprise
becomes a checklist line. This is the same family as the ci-gate
mtime-touch (`#81`), the `c033aa3` defensive JSON guard, and the #96
drift-guard — make the codebase/pipeline *know* when it is about to be
wrong, and say so *early*.
