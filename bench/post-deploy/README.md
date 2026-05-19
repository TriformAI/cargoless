# bench/post-deploy — M1–M5 post-deploy measurement scaffold

These are **scaffolds, not measurements**. Each `mN-*.sh` documents its
metric / method / success-threshold / **falsifier** in a machine-readable
header and then **exits 2 (NOT-IMPLEMENTED)**. A scaffold deliberately
cannot exit 0: a stub that "passed" would be exactly the we-already-win
dishonesty this whole harness exists to prevent. They are wired for real
only when the central-daemon deploy lands and there is something true to
measure.

Derived from the Lane C report (task #222). Every metric has an explicit
falsifier — the condition under which the efficiency claim is **disproven**.

| ID | Metric | Success threshold | Falsifier |
|---|---|---|---|
| **M1** | Fleet RAM vs N (flatness of one multiplexed RA) | aggregate peak within ~1.5× of N=1 across N=1..20 on the *real* cluster | RSS grows ≥ linearly with N, **or** `lsp=`>1 at any cell when worktrees share Cargo.toml/Cargo.lock |
| **M2** | Per-edit CPU vs the **actual** cold-pod status quo (closes the trunk-only-proxy gap) | one `serve --repo` ≤ ~0.5× the CPU-s/edit-batch of N cold `check-remote` cargo-pod runs | serve ≥ cold-pod CPU, or CAS identical-input skip-rate ≈ 0 in the fleet |
| **M3** | Overlay-push round-trip, split transport-vs-RA | p95 transport ≤ 100 ms (network) / ≤ 5 ms (local); RA-component p95 at N=20 not >2× the N=1 single-WT `watch` baseline | RA-component p95 degrades >2× from N=1→N=20 (serialization-bound — the primary risk realized) |
| **M4** | corun §7.3 safety + effectiveness | zero combined-green-that-is-solo-red escapes **and** throughput ≥ ~0.7×N | any combined-green WT that is solo-red and was acted on; or fallback so frequent throughput ≈ 1× |
| **M5** | Restart-churn RA-reap hygiene (FF-A #198/#200/#206 regression watch) | zombie RA count returns to 0 after every `serve` exit | monotonic `stat=Z` / PID growth across restart cycles (ReapOnDrop regression at the deploy seam) |

Honest framing notes that travel with these (no we-already-win):
* M1's absolute GiB **will** differ from the Leptos fixture's ~1 GiB on
  the real tf-multiverse cluster — that is **expected and not a
  falsifier**; only the *flat-vs-N structure* is the claim
  (AC7-THROUGHPUT-REPORT §11.4 caveat 1).
* The ~19–30× and 2.05× are MEASURED (N≤20 / vs trunk); the 589-WT case
  and the cold-pod CPU delta are **projections M1/M2 exist to convert
  into measurements**, not pre-won numbers (AC7 §11.4 caveat 2; §8.5).
* M5 is process-hygiene only — a regression there does **not** impugn
  the steady-state fleet-RAM thesis (zombies are 0-RSS, descendant-
  scoped, reparented to init; AC7 §11.4 caveat 4).
