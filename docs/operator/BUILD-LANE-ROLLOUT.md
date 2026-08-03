# Build lane — rollout and operation

The design is in [D-BUILD-LANE](../design/D-BUILD-LANE.md). This is the part you
need at 2am: how to turn it on without breaking the repo for everyone else, and
how to read it when someone's change stops moving.

## Rollout order — this one bites

`cargoless.checks.yaml` is parsed with `reject_unknown()`. An unrecognised key
is a **hard error**, which is what stops a typo from silently disabling a check.
It also means the daemon must learn a key before any repo may declare it — and
a repo that declares it early reds the manifest for **every** agent pushing
there, not just the author.

So:

| # | step | how you know it happened |
|---|---|---|
| 1 | the cargoless version carrying `output` merges | it is on `main` |
| 2 | a `cargoless-serve` image is built from that main | the image tag exists in the registry |
| 3 | the fleet rolls | `GET /daemon` on each pod shows the new `build_id` |
| 4 | **only now** add `output: cargo-json` to the repo | the pre-push gate accepts it |

Doing 4 before 3 is the mistake to avoid. It was caught in development by the
tf-multiverse pre-push gate (`unknown key 'output'`) — do not assume it will be
caught again, because a repo without that gate has nothing to stop it.

**The staged state is safe, not merely tolerable.** Before step 4 the lane legs
still run and still red correctly. What they lose is *attribution*: the daemon
emits one synthetic `command.failed` diagnostic instead of per-file errors, so
a red is unattributable and the lane holds the whole queue rather than ejecting
a member on no evidence. Fail-safe.

## Turning the lane on for a repo

1. **Declare the legs.** A `lane` profile plus `tier: lane` checks that run your
   *real* build — not `cargo check`. Codegen and linking are exactly the
   failures a check-only lane cannot see.

2. **Check your other profiles.** If any uses `include: ["*"]`, it will now run
   your lane legs too: cargoless resolves `"*"` against **every** check
   regardless of tier. A gate profile with a 180-second budget inheriting a
   40-minute release build times out every PR, and it looks like the gate
   spontaneously breaking rather than a config change.

   Enumerate the tiers instead. In tf-multiverse,
   `scripts/ci/check-lane-profile-isolation.sh` asserts this both ways.

2b. **Switch it on.** The daemon runs no lane unless asked — a lane merges and
   publishes, so it must never be acquired as a side effect of starting up:

   | env var | meaning |
   |---|---|
   | `CARGOLESS_LANE_PROFILE` | the `cargoless.checks.yaml` profile to run. Unset ⇒ no lane, and `GET /lane` 404s. |
   | `CARGOLESS_LANE_BASE` | ref candidates are built on. Default `main`. |
   | `CARGOLESS_LANE_ARTIFACT` | path, relative to the candidate root, of the artifact to publish on green. |

   Leave `CARGOLESS_LANE_ARTIFACT` unset for a **check-only lane**: it proves
   the merged tree compiles and deliberately leaves `.cargoless/latest-green`
   alone. That is the safe way to start — it cannot move your pointer, so a
   wrong profile costs build minutes and nothing else.

   Confirm from the boot log rather than assuming:

   ```
   [cargoless:obs] build-lane enabled profile=lane base=dev artifact=<none: check-only>
   ```

   No such line means no lane, whatever the environment claims to hold.

   **Read `profile=` and `where=` together — they are one statement.** Both are
   derived from the plan the daemon actually constructed, not from the
   environment it read, so `where=` names every stage that will run and
   `profile=` names the profile that will really be consulted. A destination
   that consults none says so:

   ```
   profile=<none: this destination runs no profile>   where=dispatched:...
   ```

   plus a `WARNING` line if a profile was configured anyway. That distinction is
   not cosmetic: the line used to print the raw `CARGOLESS_LANE_PROFILE` beside
   the destination, so a preview lane announced `profile=lane where=preview:lane`
   while running **no profile leg at all**. The config was not wrong and the
   daemon agreed with it, which is why it went unnoticed. A preview lane with a
   profile now announces both halves:

   ```
   where=profile:lane legs, then preview:lane daemon=... remote=origin
   ```

   A **wrong** profile name does not start a lane either — the daemon refuses
   to boot and names the profiles the manifest actually declares. That is
   deliberate: an unrecognised name would otherwise inherit a fallback that
   runs *every* check under a 12-second budget, and the resulting flood of
   timeout diagnostics is attributable to nobody, so the lane would eject the
   whole queue on its first build.

2c. **Confirm it is actually building.** Enqueue one member and watch
   `GET /lane` move off `queue_depth: 1`:

   ```sh
   curl -s -H "Authorization: Bearer $TOKEN" localhost:8787/lane | jq '.phase, .queue_depth'
   ```

   A lane stuck at `"idle"` with a non-zero queue for longer than
   `capture_window_ticks` seconds is not waiting — something is wrong. The
   window is driven by a tick from the serve loop; if that tick ever stops, the
   lane accepts submissions and silently never builds, which looks like a
   transport or auth problem rather than a stalled clock. This was a real bug
   (nothing drove the tick at all), so it is worth the ten seconds to check.

### Staging the manifest keys

`stage:`, `target_key:` and `output:` are all subject to the `reject_unknown()`
ordering rule above — the daemon must ship them before a repo declares them.
Adding them early does not fail one push; it reds the manifest for **every**
agent pushing to that repo.

The staged state is fail-safe in the useful direction: without `stage:` the
cheap check legs run *alongside* the expensive build legs rather than before
them. That is strictly more work than the staged form and never less coverage.
Verify that property for your own manifest before staging anything — a staged
state that ran *fewer* checks until the key landed would fail OPEN, and would
not be acceptable.

3. **Point a submitter at it.** `POST /lane/enqueue` with `id`, `head`, and
   `changed_files`.

4. **Run it in shadow first** — compare its verdict against whatever builds your
   trunk today, on the same commit, before anything depends on it.

## Shadow-running before you arm it

Run the lane where it cannot hurt anyone first. Not ceremony — the lane's leg
runner is the seam between the queue and a real build, and until it has run
against your project nobody knows what it does there.

Set `CARGOLESS_LANE_PROFILE` and leave `CARGOLESS_LANE_ARTIFACT` **unset**. The
lane then builds real candidates with your real legs and lands nothing: no
pointer moves, no tag is pushed, no PR is touched. Whatever merges changes today
stays authoritative throughout.

Three things to compare per candidate, none of which the lane can tell you
itself:

| question | how |
|---|---|
| does it agree with what you have now? | lane verdict vs your existing build for the same SHA |
| is the staging actually paying off? | wall-clock to a red — a type error should die in stage 1, minutes not tens of minutes |
| is attribution right? | on a red, does the member the lane named actually own the failing file? |

**Divergence is a signal to investigate, never a reason to arm.** And a shadow
lane nobody compares is just a runner burning CPU — if you are not going to read
the results, do not start it.

Watch for one specific shape: a green build where the legs did not produce the
artifact you declared. That reports as *infrastructure*, not as a red, and the
lane holds the whole queue rather than blaming anyone — correct, but it looks
like a stuck lane. `GET /lane` shows the reason.

## Reading the lane

```sh
curl -s -H "Authorization: Bearer $TOKEN" localhost:8787/lane | jq
```

| you see | it means |
|---|---|
| `404` | no lane configured on this daemon (not "an empty lane") |
| queue depth > 0, no build | the capture window is still open, or a build just finished |
| a build with members | that set is compiling; arrivals queue, nothing preempts |
| ejections with `Attributed` | those members own a file that carried an error |
| ejections with `Unattributed` | the errors are in files nobody touched — **everyone** is held |

## "My change is stuck"

Ask which ejection it has.

**`Attributed`** — the message names the files that carried the failure. The
change is re-admitted when its head moves *and* touches one of them. A README
edit will not do it: at tens of minutes per build, a push that cannot possibly
fix the error must not buy a slot.

**`Unattributed`** — nobody was blamed. The errors landed in files no queued
member touched, so it is either an interaction between them or a failure already
in the base. **Any** new head re-admits, because gating on files we could not
identify would strand someone whose fix legitimately lives elsewhere. The
message says so; it also says every implicated author must check, because
assuming it is someone else's problem is how these sit for hours.

**Neither, and it is still stuck** — check `eject_ttl_ticks`. Every ejection
lapses on a timer regardless, so a member held longer than that is a bug worth
reporting, not a policy outcome.

`POST /lane/readmit` forces a member back in. It is an escape hatch for a fix
the attribution cannot see, not a way to skip the queue — using it does not make
the previous failure untrue.

## Things that look broken and are not

**A build ran for 40 minutes and the queue grew.** Intended. A running build is
never cancelled; arrivals queue behind it and are picked up the moment it ends.
Cancelling to start a "better" build strands whatever was in flight, and on most
CI a cancelled run is permanently not-green.

**A red held everyone instead of ejecting one member.** Either the errors were
unattributable, or the build reported red with *no* diagnostics — which the lane
classifies as infrastructure rather than blaming someone for a reporting gap.

**Two changes landed a second apart and only one build ran.** That is the
capture window doing its job. It only ever delays the first member of an idle
lane.

**A green build did not advance the pointer.** A green with no artifact is
legitimate — a check-only lane proves a tree compiles without emitting one. The
lander deliberately leaves the pointer alone rather than erasing the last real
green.

## Tuning

| knob | default | raise it when | lower it when |
|---|---|---|---|
| `capture_window_ticks` | 60 | arrivals cluster and you want fewer, fuller builds | a single-developer repo where waiting buys nothing (`0` = off) |
| `max_members` | 10 | builds are cheap and reds are rare | a red re-tests too much, or heads go stale mid-build |
| `eject_ttl_ticks` | 3600 | ejections are usually correct | attribution is being wrong and stranding people |

Raising `max_members` amortises the build cost and widens the blast radius of
one red. That is the whole trade; there is no setting that avoids it.
