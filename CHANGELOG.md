# Changelog

All notable changes to **cargoless** (working name — public product name is
open decision D1 / Plane CWDL-12) are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) v1.1.0; this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
from `0.2.0` (the first public release) onward.

`.github/workflows/release.yml` (active — CWDL-71 Phase A) asserts at
tag-validate time that this file contains a `## <version>` (or `##
[<version>]`) heading matching the cut tag's semver. Drift between the
tag and this file is a hard release-pipeline fail — keep entries here
in lockstep with version bumps.

Entries within a version follow these section names (keepachangelog
canonical, in this order):

- **Added** — new features
- **Changed** — changes in existing functionality
- **Deprecated** — soon-to-be-removed features
- **Removed** — now-removed features
- **Fixed** — bug fixes
- **Security** — vulnerability-related changes

## [Unreleased]

_Nothing yet — all pre-tag work is captured in [0.2.0] below._

## [0.2.0] - 2026-05-19

First public release — the Model R shared repo-scoped daemon (version
operator-decided 2026-05-19; the public-launch GO is the operator's).
Capabilities, honestly bounded:

### Added

- **Repo-scoped shared daemon** — `cargoless serve --repo <path>`:
  one daemon per repository, auto-discovers worktrees
  (`git worktree list`) and LSP-overlay-multiplexes a **single** warm
  rust-analyzer across all of them (Model R).
- Headless continuous checker + atomic latest-green publisher
  (`check` / `watch` / `build --watch --out` / `status` / `clean`;
  `.cargoless/latest-green` only ever advances on a servable green
  build — never-publish-red, written atomically).
- Per-crate verdicts + **schema=2** `cli-status` (backward-compatible
  both ways; only `Severity::Error` flips a crate red; an
  unattributable error omits the `crates=` line — `verdict=` stays
  authoritative).
- Queryable diagnostic retention (`get_diagnostics`; a green crate
  atomically clears to `[]`).
- Transport abstraction — in-process, Unix-socket, and HTTP+SSE
  adapters behind one logical API; CLI auto-discovery with a
  stale-socket liveness connect-probe; an Authorizer seam.
- Workspace-cluster manager, corun verdict-batching, activity-
  activation (idle worktrees deactivated by design), and crash/restart
  recovery (replay-queue + graceful-shutdown drain).
- rust-analyzer weight-shedding: shipped default −19 % peak RSS
  (behaviour-neutral); Tier-3 proc-macro-off shipped default-safe;
  idle-evict opt-in (`TF_RA_IDLE_EVICT=1`).

### Changed

- The earlier per-worktree `watch` daemon — a superseded internal
  intermediate, never publicly launched — is subsumed by the one
  repo-scoped daemon multiplexing a single RA across the worktree
  fleet. Single-tree `watch` is retained as a documented convenience.

### Fixed

- **FF-A** (#198): the Model-R serve-loop's `SIGTERM` path now routes
  every shutdown through the proven rust-analyzer Supervisor reap
  discipline (FF #3b/#44/#61/#128). Correctness is established by the
  structural fix (source-verified, integrated) and live-fleet-
  corroborated positive on the #199-rolled infra — corroboration
  confirmed, **not** a fleet-test-proof. It is zombie/PID-hygiene
  under restart-churn, **not** a RAM leak.
- Phase-2 dogfood field findings F1–F12
  ([`docs/dogfood/PHASE-2-REPORT.md`](docs/dogfood/PHASE-2-REPORT.md)).

### Performance (measured; conditions stated inline, not headline scalars)

- **≈2.05× less per-edit CPU than `trunk serve`** —
  two-source-confirmed (`AC7-THROUGHPUT-REPORT §8.5`); unchanged under
  Model R (green-edge-rebuild preserved; re-asserted, not re-derived).
- **Fleet RAM measured flat ≈1 GiB total** across
  N ∈ {1,2,4,8,16,20} active worktrees (one shared RA), **≈19–30×**
  below the per-worktree-daemon model (`AC7-THROUGHPUT-REPORT §11.4`,
  Model-R Leg-C v4). The win is **structural** ("Model R removes the
  per-worktree multiplication"); the absolute ≈0.9–1 GiB is
  fixture-dependent (Leptos-class). **Measured to N=20**; the
  589/617-worktree fleet is a stated **projection**, not measured.
- Save→verdict is the honest dual-tier split (RA-incremental hint
  ≤1 s / authoritative cargo-check-bound) — never a single sub-1s
  headline.

## [0.0.0] - 2026-05-17

### Added

- Pre-launch development entry. cargoless reached v0-feature-complete at
  commit `3cfc835` (2026-05-17) — the headless continuous-checker +
  latest-green publisher implementation passes ACs 4/5/6, with AC#7 (#36
  comparative bench), AC#2 D-A2 renegotiation (#48), and AC#1/8/9 either
  closed or operator-time at first-tag-fire.
- This CHANGELOG.md scaffold itself, closing D-RELEASE §8 #4 (the
  tag-validate regex check in `.github/workflows/release.yml.draft` now
  has a real file to validate against).

### Notes

- No release tag has been cut. This entry exists to provide a structural
  CHANGELOG.md for the release pipeline's tag-validate regex check (per
  D-RELEASE §5). The first real release will be `## [0.1.0]` once D1
  resolves and the launch checklist (D-RELEASE §10) is clean.
- The release pipeline (`.github/workflows/release.yml.draft`) is INERT
  (`.yml.draft` extension means GitHub Actions does not pick it up). It
  activates on rename to `release.yml`, which happens at the launch-fire
  moment, not before.
