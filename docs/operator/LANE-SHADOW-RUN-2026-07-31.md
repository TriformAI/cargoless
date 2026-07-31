# Lane shadow run — 2026-07-31

The first time the build lane ran against a real repository. Report-only
throughout: `CARGOLESS_LANE_ARTIFACT` unset selects `ReportOnlyLander`, so a
green candidate is *reported* green and publishes nothing. Nothing here could
land, merge, or move a pointer.

It found two real defects before it produced a single verdict. That is the
outcome the shadow existed to produce, and it is the argument for shadowing
anything whose I/O seam has never executed.

## Topology

A **separate deployment**, not a swap of a live witness. Rolling a witness while
the fleet is building would take its shard out of service for the duration; a
new deployment costs one PVC and touches nothing.

| | |
|---|---|
| deployment | `cargoless-lane-shadow` (ns `cargoless-builder`) |
| image | `cargoless-serve:lane-5e79f45`, built from `agent/lane-stages` |
| volume | `cargoless-lane-shadow-workspace`, 120Gi, `triform-replicated-fsn-1x` |
| lane | `CARGOLESS_LANE_PROFILE=lane`, `CARGOLESS_LANE_BASE=dev`, no artifact |
| witness role | **none** — `CARGOLESS_PROJECT_CHECKS_MODE=off`, `REMOTE_PROJECT_CHECKS=0` |

The tag is deliberately `lane-<sha>` rather than the fleet's
`<epoch>-main-<sha>`: Flux's ImagePolicy elects the epoch-prefixed form, so this
tag is invisible to it and cannot auto-roll a production witness.

## Three things a new pod in this namespace needs

1. **A NetworkPolicy.** `default-deny-egress` selects every pod and egress is
   granted by name-matched policies. An unrecognised
   `app.kubernetes.io/name` does not get a refusal — it gets a **134-second
   connect timeout** to forgejo, which reads like a network fault rather than a
   policy one. Copy `cargoless-serve-witness-netpol`, swap the selector.
2. **`refs/pull/*/head`.** See defect 1.
3. **A complete clone.** See defect 1.

## Defect 1 — the daemon could not reach the members' commits

The witness template fetches `dev` and nothing else, because that is all a
witness needs. The lane merges arbitrary PR heads onto the base, and those
commits were simply **absent**:

```
$ git merge 5164e818
merge: 5164e818 - not something we can merge
```

Two of the four enqueued heads were missing outright; the other two resolved
only because they had already merged into `dev`. Every candidate materialize
therefore failed.

**Fix.** Fetch `+refs/pull/*/head:refs/remotes/origin/pr/*` in both the
`repo-bootstrap` init and the `repo-sync` loop, and drop `--depth` from both. A
shallow tree is its own version of this bug: an integration whose common
ancestor sits past the shallow boundary fails as *infrastructure*, so a shallow
clone silently converts real reds into "could not build".

After the fix: 10,312 PR refs present, `.git/shallow` absent.

## Defect 2 — an infrastructure failure retried forever, invisibly

This one had shipped. `LaneBuildOutcome::Infra` requeued its members at the
front of the queue with **no backoff and no attempt cap**, and the
`LaneAction::Report` carrying the reason is a no-op in the driver's `execute`.
So the lane retried as fast as the failure returned — roughly one candidate
attempt every 2.5 seconds — while `GET /lane` reported a steady:

```json
{"phase":"building","queue_depth":0,"in_flight":["pr-10320", ...],"generation":63}
```

which is indistinguishable from a slow compile. The `generation` counter was the
only tell, and only if you sampled it twice.

**How to recognise it.** Inside the pod: no `cargo`/`rustc` process, and
`lane-candidates/` empty, while the lane claims to be building.

**Fix** (`agent/lane-stages` @f04011e):

* `infra_backoff_ticks` (30) — requeued members are not eligible until it
  elapses.
* `infra_max_attempts` (5) — retrying forever assumes every infra failure is
  transient. Some are permanent from the lane's side: an unreachable commit does
  not become mergeable by waiting, and retrying it blocks every later submission
  behind a build that cannot succeed.
* `EjectReason::Infrastructure` — a **third** variant, deliberately not folded
  into `Unattributed`. Unattributed says "your tree is red and we could not tell
  whose change did it"; the code *is* implicated. This says nothing was ever
  compiled, so nothing was judged. An author who reads the wrong one goes
  hunting a bug that was never diagnosed.

The streak resets on any non-`Infra` outcome, so an occasional transient cannot
accumulate across hours and eject an innocent member.

Four mutations were added to `scripts/tests/lane-attribution-mutation-test.sh`,
including the exact code that shipped, so the harness proves these tests would
catch it returning.

## What the corrected run showed

| | |
|---|---|
| members | `pr-10327` (17 files), `pr-10321` (2), `pr-10315` (1) |
| candidate | `candidate-1-0` — **sequence 0**, materialized first try |
| generation | 1 — one build, no retries |
| merge shape | three `--no-ff` "lane candidate: pr-N" commits stacked on `dev`, in submission order |
| ancestry | all three heads confirmed ancestors of the candidate HEAD |

`ProfileLegRunner` — the seam with one caller and zero tests going in — executed
against a real manifest and started a real release build.

**A verification note worth keeping.** Checking ancestry with the pod's cwd
rather than the worktree produced three confident `NO`s and a candidate that
appeared to contain none of its members. Always pass `-C <worktree>` *and* an
explicit rev; `git log` with an ambient cwd will answer about a different repo
without complaining.

## Defect 3 — the build reported its verdict nowhere

The corrected run compiled for **76 minutes** and then left nothing readable.
`GET /lane` reports only current state (it went straight back to
`phase=idle`), `LaneAction::Report` is `Vec::new()` in the driver, and
`CandidateTree::release` removes the candidate worktree — and with it the
target dir and every artifact — the moment the build ends. Ten minutes later
there was no way to tell green from red from inside the pod.

That is fatal to the exercise: a shadow run whose verdict cannot be read cannot
be compared against `dev-staging-build`, which is the only reason to run one.

Fixed by writing `<state_dir>/lane-runs.log` — one `[cargoless:obs]` line per
leg (id, tree, required, elapsed_ms) plus one per build outcome. The data was
already being computed and thrown away: `LegOutcome.legs` carried all of it and
`run_build` discarded it through two `..` patterns.

The shape is the witness's, not a new invention:
`scripts/ci/_witness_leg_obs.sh` already appends
`[cargoless:obs] witness-leg id=… elapsed_s=… rc=…` to `witness-legs.log` in
the same state dir, for the same reason.

## What this run says about the three pipes

The lane was built as though tf-multiverse had nothing, and each defect above
was a rediscovery of something the repo already solved:

| defect | already solved in |
|---|---|
| PR heads unreachable | `merge-train-controller` fetches per member |
| infra retry with no backoff/cap | preview tier's ENOSPC non-latching red + retry cap (PR #73) |
| verdict not persisted | the witness's `witness-legs.log` |
| wasm-bindgen skew | `build-portal.sh`'s `wbg_bin()` self-heal (PR #5876 + #5877) |
| build an unmerged ref in isolation | the **preview tier** — `POST /instances`, worktree per instance, exact-sha pin, manifest-driven steps, artifact bundle |

Two further facts the comparison surfaced, both acted on:

* **Coverage.** The lane's three legs miss `feature="csr"`, `cfg(test)` and
  `feature="vsock"` — the cones behind a ~6h triform.dev outage, 5.5 weeks of
  silent portal test rot, and an 8-day isolation-bake freeze. Three legs added
  (`agent/lane-cfg-cone-legs`), bringing the set to exactly
  `merge-train-candidate.yml`'s five plus bindgen.
* **Where it compiles.** `ProfileLegRunner` ran `cargo build` on unmerged
  member code inside a pod holding `FORGEJO_TOKEN`. `cargo` executes `build.rs`
  and proc-macros, so "we only compile it" is false —
  `merge-train-candidate.yml` exists precisely to prevent that, and its
  credential-free contract is test-pinned. The daemon also carries
  wasm-bindgen 0.2.114 against a workspace that resolves 0.2.118, where the
  deploy builder pins the right one. Compilation belongs in the builder;
  the shadow's in-daemon execution was expedient for one run, not the design.

## Still open

* The comparison itself: lane verdict vs `dev-staging-build` for the same
  members. Baseline from the last 24h — a real build is 2200-3145s (37-52 min),
  a no-op cron pass 24-48s.
* Attribution on a red cannot be fully exercised yet. `output: cargo-json` is
  **not** declared in tf-mv's manifest, because `cargoless-serve` and all 13
  shards run `4bb358b`, which predates the `output` key, and `reject_unknown()`
  makes an unknown key a hard error — declaring it now would red the manifest
  for every agent pushing to a shard. Until those daemons are promoted, a red
  yields one synthetic diagnostic at `cargoless.checks.yaml:1:1` with no file
  paths, which the lane correctly refuses to attribute.
