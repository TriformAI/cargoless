# diffmatrix case `proc-macro` — Leptos `view!` post-expansion error

This case is **not a standalone crate**. It deliberately reuses the
existing Leptos fixture at `bench/fixture/` (17 files / ~1009 LOC,
honest-size, the same substrate `bench/run.sh` measures) rather than
duplicating the Leptos dependency tree into the matrix. Duplicating it
would add a second crates.io / Cargo.lock surface for no analytic gain.

## What it proves

`bench/diffharness.sh`, for this case:

1. Copies `bench/fixture/` to a scratch dir.
2. Injects the **same anchor `bench/run.sh` uses** —
   `count.get() /* BENCH_MACRO_ANCHOR */` →
   `count.get_oops() /* BENCH_MACRO_ANCHOR */` in
   `src/components/metrics.rs` — an error that exists **only after
   Leptos `view!` proc-macro expansion**.
3. Ground-truth = `cargo check --all-targets --locked` (the rustc
   front-end runs full proc-macro expansion ⇒ **RED**).
4. cargoless verdict is taken **with Tier-3 proc-macro-off active**
   (`TF_RA_PROCMACRO_OFF=1`), because that is the shipped default-safe
   RAM rung (#126 / field-verified #130).

## The invariant under test

cargoless **must be RED here even with proc-macro-off** — the cargo-check
authority catches the post-`view!`-expansion error regardless of RA's
proc-macro view. This is the §9a / #126 no-wrong-verdict safety
property. A **FALSE-GREEN here is a #126 regression** and a launch-class
defect — the harness fails loud.

Ground-truth color = **RED**. Required cargoless color = **RED** (today
*and* after every Increment-3 increment — this case must never regress).
