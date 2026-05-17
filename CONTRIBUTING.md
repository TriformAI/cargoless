# Contributing

## Where to file issues & PRs (outside contributors)

- **Issues:** [github.com/TriformAI/cargoless/issues](https://github.com/TriformAI/cargoless/issues)
- **Pull requests:** open against `main` at [github.com/TriformAI/cargoless](https://github.com/TriformAI/cargoless)
- **Discussions:** GitHub Discussions on the same repo

GitHub is the canonical public face. The internal integration loop runs on
Forgejo (see below); maintainers cherry-pick accepted GitHub PRs into the
Forgejo branch space for CI gating. You do not need a Forgejo account to
contribute.

## Build & test model — push-to-build, no local cargo (agent-team workflow)

This project is developed by an agent team under a workspace policy that
**blocks local `cargo`/`rustc`**. The authoritative build/test path is
**Forgejo CI** on the internal `forgejo.triform.dev/triform/cargoless` mirror:
commit, push, and CI (`.forgejo/workflows/ci.yml`) runs `cargo build` /
`test` / `fmt --check` / `clippy` in a pinned `rust:1.85` container, plus
`scripts/ci-gate` on a dedicated `cargoless-builder` Kubernetes pod for
faster pre-integration merge gating.

**Workflow for the internal agent team (not required for outside contributors):**

1. Work on a branch (`agent/<role>-<topic>` or `feat/<topic>`) on the Forgejo
   mirror.
2. Keep crate ownership disjoint — see the crate table in `README.md`.
3. Commit small and push often. **Uncommitted/unpushed work is invisible to CI
   and to teammates.**
4. Branch is gated via `scripts/ci-gate <branch>` (the dedicated k8s builder)
   AND `.forgejo/workflows/ci.yml`; ALL-GREEN required before integration.
5. Read CI logs via the Forgejo API / `gh` (authed to forgejo.triform.dev),
   not by running cargo locally.

**Outside contributors:** open your PR against `main` on GitHub. The
maintainers will cherry-pick into Forgejo, run the internal CI gate, and
merge to Forgejo's `main` on green. The GitHub mirror auto-updates from
Forgejo (push-mirror) so your PR will be marked merged on GitHub once the
internal integration completes. You do **not** need to interact with
Forgejo, the cargoless-builder pod, or `scripts/ci-gate` directly.

## Governance (v0)

Benevolent-maintainer model during v0: the technical lead owns cross-cutting
decisions and the `tf-proto` contract; crate owners own their crate. Decisions
of record live in Plane (project CWDL). This evolves to a documented governance
model before public launch (CWDL AC#8).

## Code conventions

Rust Edition 2024, MSRV 1.85. `cargo fmt` canonical. Clippy warnings are
errors in CI once the skeleton is real. No element-style hardcoding across
crate boundaries — talk through `tf-proto`.
