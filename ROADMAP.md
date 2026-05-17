# Roadmap

Authoritative plan lives in Plane project **CWDL** (Continuous WASM Dev Loop);
the **CWDL-1** umbrella is the single source of truth for the acceptance
criteria and this file mirrors it. Phasing is **v0 → v0.1 → v1**.

## v0 (current) — headless continuous checker + latest-green publisher

v0 is single-developer, single-machine, and **headless**: it always knows
what's green and **publishes** the latest green build. It does **not** serve a
browser — the live HTTP/WebSocket dev-server is v0.1. Nine non-negotiable
acceptance criteria (Plane CWDL-1):

1. **Zero-config headless startup** — on a clean macOS/Linux box, install, run
   in a Rust+WASM project; daemon up + config auto-detected + watch→verdict
   pipeline live, zero manual config (D-A1: within 30s; first green when the
   cold build finishes). No browser, no holding page.
2. **Save→verdict < 1s** (primary) on a committed reference project (gated by
   spike S1 / D-A2).
3. **Median green-save → latest-green artifact *published* latency** under the
   ratified threshold. No sub-second artifact claim; threshold set from S1
   evidence (D-A2). (Browser-reload latency is a v0.1 metric.)
4. **Never publish red** — the latest-green pointer/dir
   (`.cargoless/latest-green`) only ever advances on a servable green build; a
   red tree or a failed build never moves it. Verified headless on the
   publisher, not a browser.
5. **CAS dedupe** — identical source state is a cache hit, build skipped.
6. **Daemon survives `kill -9` of rust-analyzer** and transparently restarts.
7. **Published benchmarks beat `trunk serve` / `bacon` on ≥2 dimensions,
   two-mode** — checker mode (save→verdict) and artifact mode (save→publish)
   reported separately, never blended; no sub-second artifact claim.
8. README / ROADMAP / CONTRIBUTING / LICENSE present.
9. Launch blog post reviewed by ≥2 incl. one outside the team.

CLI surface in v0: `check` (one-shot verdict, exit code reflects green/red),
`watch` (continuous headless verdict stream), `build --watch --out <dir>`
(publish latest-green), `status` (verdict + latest-green hash + path),
`clean`. `serve` is v0.1.

## v0.1 — optional live server / browser-reload adapter

A thin adapter on top of the v0 latest-green publisher; it consumes the
published `.cargoless/latest-green` output and adds the browser. None of this
is required for the v0 launch:

- HTTP static server over the latest-green directory.
- WebSocket channel to the browser; Trunk-compatible reload protocol (D3),
  full-reload (D5), browser reload shim.
- Cold-start holding page (browser).
- Browser "never serve red" (server keeps serving last-green while red — the
  browser-facing consumer of v0's never-publish-red guarantee).
- `serve` command (one-command drop-in for `trunk serve`).
- Serve-latest-green integration tests.

The std-only implementation already exists as research on branches
`agent/devserver` and `agent/devserver-bundle` — preserved, not deleted.

## v1 (NOT v0 — parking lot)

salsa / rust-analyzer-as-library deep integration · remote/shared CAS backend ·
team features + remote auth · multi-agent build coordination · editor LSP-style
interface · symbol-level green/red granularity · replacing `trunk build`
internals · hot-swap WASM · CI integration · Windows support.

Everything in v0.1 and v1 stays out of v0 sprints by construction.
