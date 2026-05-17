# Contributing

## Build & test model — push-to-build, no local cargo

This project is developed by an agent team under a workspace policy that
**blocks local `cargo`/`rustc`**. The authoritative build/test path is
**Forgejo CI**: commit, push, and CI (`.forgejo/workflows/ci.yml`) runs
`cargo build` / `test` / `fmt --check` / `clippy` in a pinned `rust:1.85`
container.

**Workflow for every contributor (human or agent):**

1. Work on a branch (`agent/<role>-<topic>` or `feat/<topic>`).
2. Keep crate ownership disjoint — see the crate table in `README.md`.
3. Commit small and push often. **Uncommitted/unpushed work is invisible to CI
   and to teammates.**
4. Open a PR against `main` on Forgejo. CI must be green to merge.
5. Read CI logs via the Forgejo API / `gh` (authed to forgejo.triform.dev),
   not by running cargo locally.

## Governance (v0)

Benevolent-maintainer model during v0: the technical lead owns cross-cutting
decisions and the `tf-proto` contract; crate owners own their crate. Decisions
of record live in Plane (project CWDL). This evolves to a documented governance
model before public launch (CWDL AC#8).

## Code conventions

Rust Edition 2024, MSRV 1.85. `cargo fmt` canonical. Clippy warnings are
errors in CI once the skeleton is real. No element-style hardcoding across
crate boundaries — talk through `tf-proto`.
