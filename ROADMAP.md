# Roadmap

Authoritative plan lives in Plane project **CWDL** (Continuous WASM Dev Loop).
This file is the public-facing summary.

## v0 (current)

The single-developer, single-machine, local inner-loop tool. Nine
non-negotiable acceptance criteria (Plane CWDL-1 umbrella):

1. Zero-config `serve` on a clean macOS/Linux machine (cold-build semantics per D-A1).
2. Median save→verdict < 1s on a committed reference project (gated by spike S1 / D-A2).
3. Median green-save → browser-updated WASM < 5s.
4. Never serve red — last-green stays served while the codebase is red.
5. CAS dedupe — identical source state is a cache hit, build skipped.
6. Daemon survives `kill -9` of rust-analyzer and transparently restarts.
7. Published benchmarks beat `trunk serve` / `bacon` on ≥2 dimensions.
8. README / ROADMAP / CONTRIBUTING / LICENSE present.
9. Launch blog post reviewed by ≥2 incl. one outside the team.

## v1 (NOT v0 — parking lot)

salsa / rust-analyzer-as-library deep integration · remote/shared CAS backend ·
team features + remote auth · multi-agent build coordination · editor LSP-style
interface · symbol-level green/red granularity · replacing `trunk build`
internals · hot-swap WASM · CI integration · Windows support.

Everything in v1 stays out of v0 sprints by construction.
