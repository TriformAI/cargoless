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

3. **Point a submitter at it.** `POST /lane/enqueue` with `id`, `head`, and
   `changed_files`.

4. **Run it in shadow first** — compare its verdict against whatever builds your
   trunk today, on the same commit, before anything depends on it.

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
