# Changelog

All notable changes to **cargoless** (working name — public product name is
open decision D1 / Plane CWDL-12) are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) v1.1.0; this
project will adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once `0.1.0` releases.

`.github/workflows/release.yml` (currently `.draft` per D-RELEASE
§10 path-to-real-fire checklist) asserts at tag-validate time that this file
contains a `## <version>` (or `## [<version>]`) heading matching the cut
tag's semver. Drift between the tag and this file is a hard release-pipeline
fail — keep entries here in lockstep with version bumps.

Entries within a version follow these section names (keepachangelog
canonical, in this order):

- **Added** — new features
- **Changed** — changes in existing functionality
- **Deprecated** — soon-to-be-removed features
- **Removed** — now-removed features
- **Fixed** — bug fixes
- **Security** — vulnerability-related changes

## [Unreleased]

Production-hardening round in progress (Plane CWDL — see
[`docs/dogfood/PHASE-2-REPORT.md`](docs/dogfood/PHASE-2-REPORT.md)). All
landed work between v0-feature-complete (3cfc835, 2026-05-17) and the
first cut tag will roll into the `0.1.0` section at release time.

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
