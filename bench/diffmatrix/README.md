# bench/diffmatrix — the Increment-3 differential proof harness

**The no-false-GREEN oracle.** This is the gate every Increment-3
(check / clippy authority) increment must pass before it is "done":
cargoless's verdict, for a fixed matrix of fixtures, must be
**bit-identical in color** to ground-truth `cargo`.

```
ground-truth color := RED if ANY applicable oracle is RED, else GREEN
  oracle 1  cargo check  --all-targets --locked
  oracle 2  cargo check  --all-targets --locked --target wasm32-unknown-unknown   (cases with run_wasm=1)
  oracle 3  cargo clippy --all-targets --locked -- -D warnings                     (cases with run_clippy=1)

cargoless color  := exit(cargoless check)  →  0 GREEN | 1 RED | 2 BLOCKER(setup)
```

## The asymmetry that matters (the §9a-trap safety net)

| classification | meaning | severity |
|---|---|---|
| **MATCH** | cargoless color ≡ ground-truth color | pass |
| **FALSE-GREEN** | cargoless GREEN, ground-truth RED | **catastrophic — a broken tree shipped green. The harness exits LOUD (code 3).** |
| FALSE-RED | cargoless RED, ground-truth GREEN | cry-wolf — real defect, exits 4, but not the §9a class |
| BLOCKER | an oracle or cargoless could not run (env/setup) | not a pass — exits 2; never silently downgraded to GREEN |

A FALSE-GREEN is the only class that can let a launch ship broken; it
is therefore the one the harness is built around. **Silently skipping a
case is itself treated as a harness-level false-GREEN** — e.g. if the
`wasm32-unknown-unknown` target is unavailable, the load-bearing
`wasm-only-err` case BLOCKERs loud rather than passing by omission.

## The honest gap inventory (no we-already-win)

Run against **v0.2.0 (cc206da)** cargoless — host `cargo check`
authority, proc-macro-off-safe, **no wasm-target oracle, no clippy
oracle** — the matrix is *expected to fail*, and that failure is the
point. `EXPECTED.tsv` records the honest per-case baseline:

| case | ground-truth | cargoless @ v0.2.0 | klass | what Increment-3 owes |
|---|---|---|---|---|
| `clean` | GREEN | GREEN | MATCH-NOW | nothing — control; regression if not GREEN |
| `type-err` | RED | RED | MATCH-NOW | nothing — #2/#8-redo invariant; regression if GREEN |
| `proc-macro` | RED | RED | MATCH-NOW | nothing — #126/#130 safety; **FALSE-GREEN here = #126 regression** |
| `test-only-err` | RED | GREEN (suspected) | GAP-INC3 | `--all-targets` test-target authority parity |
| `clippy-warn` | RED | GREEN | GAP-INC3 | a clippy `-D warnings` tier in cargoless's authority |
| `wasm-only-err` | RED | GREEN | GAP-INC3 | **(load-bearing)** a `--target wasm32` oracle — the #1 currently-shipping false-GREEN per dev-fixer lane B |

Two run modes:

* **`diffharness.sh` (strict, default)** — the Increment-3 *completion*
  gate. Every case must MATCH ground-truth. Exits 0 only when the whole
  matrix is bit-identical, i.e. when Increment-3 is fully landed. Today
  it exits 3 (false-GREEN on the three GAP-INC3 cases) — by design.
* **`diffharness.sh --expect-baseline`** — the day-one *regression*
  gate. Asserts cargoless matches the **documented** per-case baseline
  in `EXPECTED.tsv`. Green iff reality == the honestly-recorded state;
  a surprise fails loud in **either** direction — a MATCH-NOW case
  regressing to false-GREEN (e.g. `proc-macro`), *or* a GAP-INC3 case
  silently changing without `EXPECTED.tsv` being updated alongside the
  increment. This mode is usable as a CI gate from the first commit and
  is how each Increment-3 increment proves its case-flip: land the fix,
  flip that row's `cargoless_base` to match `ground_truth` in the same
  commit, harness stays green.

## Build-vehicle discipline

No local cargo. `diffharness.sh` runs in the cargoless-builder pod / CI
bench job exactly like `bench/run.sh` and `bench/modelr-fleet.sh`
(`APPROVE` prefix, pinned PATH, `cargo fetch --locked` egress BLOCKER).
The dep-free crate cases need no crates.io egress; only the
`proc-macro` case (reusing `bench/fixture/`) does, and it BLOCKERs
honestly if egress is absent.
