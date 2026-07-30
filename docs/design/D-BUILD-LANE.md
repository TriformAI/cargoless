# D-BUILD-LANE — build the candidate merge before it lands

## The problem

The common arrangement builds *after* the merge: land the change, then let CI
build the trunk. Two things go wrong.

The trunk goes red, and everyone is blocked until someone reverts. And — less
obvious, more corrosive — **the tree that ships was never compiled as a tree**.
Each change was compiled on its own, against a base that has since moved. Two
changes that are individually green can be red together, and you find out on the
trunk.

## What the lane does

Enqueue a change, and the lane merges it onto the current base, builds *that*,
and only lets a green build land **and** publish. What ships is exactly what
compiled.

```
enqueue ──► [ capture window ] ──► build candidate ──┬── green ──► land + publish
   ▲                                                 │
   │                                                 └── red ────► eject the
   └──────────── survivors + new arrivals ◄──────────────────────  responsible
                                                                   change(s)
```

One build at a time. Arrivals queue.

## Scope

Cargoless supplies the **queue**, the **attribution**, and the **publish
trigger**. It does not supply the build — that is your `cargoless.checks.yaml`.
This is the whole reason the lane is reusable: a Leptos app declares
`trunk build`; a larger workspace declares `cargo build --release` plus a wasm
target plus `wasm-bindgen`. The lane does not care which.

## Configuration

The lane runs a named profile from `cargoless.checks.yaml`
(see [D-PROJECT-CHECKS](D-PROJECT-CHECKS.md)). A minimal one:

```yaml
version: 1
profiles:
  lane:
    include: ["lane"]
    timeout_ms: 3600000
checks:
  - id: release-build
    title: Release build
    tier: gate
    required: true
    kind: command
    command: ["trunk", "build", "--release"]
    output: cargo-json          # parse cargo's own diagnostics
    timeout_ms: 3600000
```

`output: cargo-json` is what makes attribution work. Without it a failing check
reports one synthetic diagnostic pinned at `cargoless.checks.yaml:1:1` — red,
correctly, but with the file and line discarded, so the lane cannot tell whose
change broke it.

### Lane settings

| Setting | Default | Meaning |
|---|---|---|
| `max_members` | 10 | Cap per build. Bigger amortises the build cost; also widens the blast radius of one red and raises the chance a head goes stale mid-build. |
| `capture_window_ticks` | 60 | How long an idle lane waits for company. `0` builds immediately. |
| `eject_ttl_ticks` | 3600 | Backstop: an ejection lapses after this regardless. |

## The capture window

Without it the lane builds whoever arrives first and everyone else waits a full
cycle. When a cycle is a real release build — tens of minutes — two changes
landing seconds apart cost two cycles instead of one.

The window only ever delays the **first** member of an **idle** lane. Once a
build is running, arrivals queue against it and are picked up the moment it
finishes, so on a busy lane the window costs nothing. It is also short-circuited
when the queue reaches `max_members`: there is nothing left to capture once the
build is full.

## Attribution

On a red build, each erroring file is mapped to the member(s) that changed it.

| Errors owned by | Outcome |
|---|---|
| exactly one member | eject it; everyone else rebuilds immediately |
| several members | eject **all** of them, each told the failure is shared |
| nobody | eject **all** members, each told it could not be attributed |

The third row is the important one. An error in a file nobody in the build
touched is either an interaction between changes or a failure that was already
in the base. Picking a culprit would eject someone innocent, and a gate that
blames the wrong person is a gate people learn to route around. So everyone is
held, everyone is told, and the message says plainly that the cause is unknown.

There is **no separate confirmation build**. The next build — survivors plus
whatever arrived meanwhile — is the verification, and it was going to run
anyway.

A red with **no diagnostics** is reported as infrastructure, not as a code red.
The lane cannot attribute an empty report, and ejecting the whole queue over a
reporting gap punishes everyone for a tooling failure.

## Re-admission

An ejected member is re-admitted when the *error* changes, not when the file
merely moves.

* **Attributed** — the new head must touch at least one file that carried the
  failure. A README edit does not buy a build slot.
* **Unattributed** — **any** new head is re-admitted. We do not know where the
  fault is, so gating on files we could not identify would strand someone whose
  fix legitimately lives elsewhere.

Both lapse after `eject_ttl_ticks` regardless, so nothing is ejected forever.

### Ejection identity is line-insensitive, deliberately

The key is the fingerprint multiset from
[`attribution`](../../crates/cargoless-core/src/attribution.rs) —
`source|code|path|normalized_message`, count-matched. It **omits line and
column**, and that is load-bearing:

> A pusher who inserts three lines shifts the line number of every error below
> their edit; a line-sensitive identity would then see all those shifted base
> errors as "new" and wrongly blame the pusher.

Do not "improve" this by adding line back. A test
(`ejection_identity_survives_a_line_shift`) and a mutation
(`line-sensitive ejection identity`) both fail if you do.

The *file* is still reported to the author — only the *key* drops line.

## Never cancel, only collapse

A running build always finishes and publishes its verdict. Arrivals queue; they
never preempt.

This is not stylistic. On most CI systems a cancelled run is permanently
not-green, so cancelling a build to start a "better" one strands whatever was in
flight. A generation counter makes a late completion cheap to ignore, so letting
the old build finish costs nothing.

## Landing

`LaneLander` is a hook, called **only** on green and **exactly once** per green
build.

Cargoless ships `PointerLander`, which advances `.cargoless/latest-green`. For a
single-app project that *is* "merge and publish together": the pointer only ever
advances on a green candidate, and a failed swap leaves the previous pointer
byte-untouched.

It deliberately does **not** merge anything into git. Cargoless does not know
what a merge means for your forge, and guessing would be worse than asking for a
small adapter. A forge adapter typically:

1. pushes the candidate with a compare-and-swap against the frozen base
   (`git push --force-with-lease=<branch>:<base>`),
2. reconciles each member's PR state,
3. promotes the artifact it already built.

If the CAS is rejected — the base moved during the build — return `Err`. The
lane treats that as infrastructure and re-enqueues every member so the next
build re-merges them against the new base. **Never drop them**: the build was
green, nobody's code was at fault, and losing green work to a push race looks
like nothing happened.

## HTTP

| Route | Purpose |
|---|---|
| `POST /lane/enqueue` | submit a member (`id`, `head`, `changed_files`) |
| `GET /lane` | queue depth, current build, ejections **with reasons** |
| `POST /lane/readmit` | explicit re-admission escape hatch |

`GET /lane` is the product surface. An author whose change stops moving needs to
see which errors are holding it, who else is affected, and what will clear it.

## Testing

The policy is a pure `Event → (State, Vec<Action>)` machine
(`crates/cargoless-core/src/lane.rs`), so every rule above is pinned by a unit
test with no compiler involved
(`crates/cargoless-core/tests/lane_policy.rs`).

Because those tests are the only thing between the lane and an unjust ejection,
`scripts/tests/lane-attribution-mutation-test.sh` proves they can **fail**: it
breaks the attribution four ways — never attribute, always attribute, cancel in
flight, line-sensitive identity — and asserts the suite reddens for each. A
mutation that fails to *apply* is reported as a survivor rather than silently
skipped.

## Non-goals

* **Replacing your build.** The lane runs your legs; it does not know how to
  compile anything.
* **Knowing your forge.** No PR, no branch protection, no merge semantics.
  `LaneLander` is the whole interface.
* **Guessing a culprit.** When attribution is ambiguous the lane says so.
* **Cancelling to go faster.** See above.
