# D-CARGO-LOCK-MAINTENANCE — when (and when not) to refresh Cargo.lock

**Status:** Structural rule extracted from #266 ci-gate `--update-lock`
corroboration on `main = f88f4c3` (2026-05-20). Converts a recurring
gut-feel pre-tag question ("should we update `Cargo.lock` before
tagging?") into a falsifiable, operator-verifiable structural answer.
**Audience:** the next pre-tag decision-maker (any future release-cycle
operator + future-you).

---

## TL;DR

> **`Cargo.lock` does NOT need a refresh before any tag UNLESS the
> workspace MSRV has moved.** The lock at current `main` is provably at
> latest-MSRV-1.85-compatible; a `cargo update --workspace` is a true
> no-op. Verify in 30 seconds via `scripts/ci-gate <ref> --update-lock`
> (#266) — the no-op-write-skip path is the signal.

---

## 1. The finding (#266 corroboration evidence)

The cargoless workspace pins `rust-version = "1.85"` in
`[workspace.package]` (`Cargo.toml`) and `rust-toolchain.toml` channel
`1.85.0`. When `cargo update --workspace` runs in the cargoless-builder
pod against `main = f88f4c3` (the #266 ratification commit), `cargo`
emits:

```
     Locking 0 packages to latest Rust 1.85 compatible versions
note: pass `--verbose` to see 17 unchanged dependencies behind latest
```

**Two facts established:**

1. The committed `Cargo.lock` IS already at every dep's
   latest-MSRV-1.85-compatible version. A `cargo update --workspace`
   moves **nothing**.
2. There ARE 17 deps with newer-incompatible upstream versions, but
   they're MSRV-pinned correctly — the version constraints in
   `Cargo.toml` (or transitive `package.rust-version` per cargo's
   MSRV-aware resolver) keep cargo from picking them.

The `--update-lock` flow's
[`cmp -s` no-op-write-skip path](../../scripts/ci-gate) (introduced in
#266) fires deterministically when this is the case:

```
[update-lock] host Cargo.lock is byte-identical to regenerated copy (no-op write skipped)
```

That output is the operator-verifiable signal that the lockfile is at
MSRV-latest and a pre-tag refresh would change nothing.

---

## 2. The rule (and its narrow exception)

**Default rule:** **DO NOT refresh `Cargo.lock` before a release tag.**
The lock that builds-green on `main` is the lock the tag should ship.
Spurious lock-bumps add diff noise without changing build behavior and
muddy the rebase / cherry-pick semantics of integration cycles.

**Exception (the only one):** when the workspace MSRV moves
(e.g. 1.85 → 1.90). Bumping MSRV legitimately unlocks newer-version
candidates the previous MSRV held back. In that case `cargo update
--workspace` will produce a non-empty diff and the `--update-lock` flow
will write the regenerated lock to the host worktree (the non-no-op
path — `cmp -s` returns differing, the diff is surfaced first 200 lines,
the lock is `cp`'d into place atomically).

The exception's structure mirrors the rule: **the operator does not
GUESS whether a refresh is needed; they OBSERVE the no-op vs non-no-op
output of `scripts/ci-gate <ref> --update-lock` and let the cargo
resolver itself answer the question.**

---

## 3. The verification recipe (operator-runnable)

When uncertain (e.g. ahead of a tag, or after a long stretch of
upstream-dependency churn), the falsifying check is:

```bash
scripts/ci-gate <ref> --update-lock
```

Behaviour matrix:

| Output                                                              | Interpretation                                                       |
|---------------------------------------------------------------------|----------------------------------------------------------------------|
| `Locking 0 packages to latest Rust X.Y compatible versions`         | Lock IS at MSRV-latest; no refresh needed; rule §2 default applies.  |
| `host Cargo.lock is byte-identical to regenerated copy (no-op …)`   | Confirmation of the above from the capture-back path.                |
| `Locking N packages` (N ≥ 1)                                        | A refresh DID move deps. Inspect the diff, then commit `Cargo.lock`. |
| `host Cargo.lock REPLACED ($REPO_ROOT/Cargo.lock)`                  | Capture-back wrote the new lock to the host worktree; commit it.     |
| `cargo update --workspace failed (exit N)`                          | Resolver itself errored (rare). Gate aborts BEFORE 7-phase (§9a-trap defense). |

Cost: ~3-5 min on warm cache (the same `--update-lock` cycle that
#266's own self-corroboration ran). The gate's `--locked` 7 phases run
**after** the lock-regen step, so a successful gate is ALSO end-to-end
proof that the (newly-regenerated, if applicable) lock builds + tests +
lints green.

---

## 4. Why this rule beats "refresh-as-discipline"

Some projects refresh `Cargo.lock` on a regular schedule (weekly,
pre-release, etc.) as defensive maintenance. cargoless does NOT, because:

- **MSRV-pinning is the actual safety mechanism.** `Cargo.toml` version
  constraints + `package.rust-version` per-dep are what bind the
  resolver to compatible deps; the lock is a *consequence* of those
  constraints, not an independent surface to maintain.
- **Diff noise has a real cost in agent-edit-batch cadence.** A
  spurious lock-bump shows up in every subsequent `git diff origin/main`
  on every long-lived agent branch, inflating cherry-pick reviews + L3
  backstop surface for zero behavioural change.
- **The lockfile is a witness, not a control surface.** When the lock
  changes, SOMETHING ELSE caused it (a `Cargo.toml` edit, an MSRV move,
  a new dep). Refreshing the lock without a triggering change inverts
  the causality: it makes the lock the cause and forces a hunt for the
  effect.

The structural rule preserves this discipline: refresh-on-need,
verified-by-resolver, never-by-schedule.

---

## 5. Cross-references

- **The `--update-lock` opt-in mode** ([`scripts/ci-gate`](../../scripts/ci-gate),
  #266) — the operator's in-band path to refresh + verify, in-pod,
  without bypassing the cargo-safety hook. Reads the regenerated
  lockfile back to the host worktree atomically; the §9a-trap defense
  on regen failure means a claimed-fresh-lock that didn't actually
  regenerate can't slip past as gate-green.
- **CI parity:** [`.forgejo/workflows/ci.yml`](../../.forgejo/workflows/ci.yml)
  + [`scripts/ci-gate`](../../scripts/ci-gate) both pin `--locked` on every
  cargo invocation — so any lock-drift between `Cargo.toml` and
  `Cargo.lock` fails LOUD as a gate-red on integration, not silent at
  build time.
- **MSRV anchor:** [`rust-toolchain.toml`](../../rust-toolchain.toml)
  (`channel = "1.85.0"`) + [`Cargo.toml`](../../Cargo.toml)
  (`[workspace.package].rust-version = "1.85"`) are the two places to
  edit if MSRV ever moves. Both must move together; the cargo-resolver's
  MSRV-aware mode reads `package.rust-version`.
- **The 17 newer-incompatible deps** are visible via `cargo update --verbose
  --workspace` (the `note: pass --verbose to see 17 unchanged
  dependencies behind latest` hint in §1) — useful as MSRV-bump cost
  reconnaissance when considering moving to 1.86 / 1.87 / etc.

---

**End of note.** Update if the workspace MSRV ever moves (anchor §2's
"only exception"), or if cargo's resolver semantics around MSRV-aware
selection change in a way that invalidates the no-op-write-skip
verification path.
