# cargoless

> **The codebase always knows what works, and tells you the moment it
> doesn't — without burning your CPU to do so.**

cargoless is a headless dev-loop daemon for Rust+WASM projects, built
on one premise: most of the work `trunk serve` and `bacon` do on every
save is **redundant work** — rebuilding state the previous cycle
already proved correct. cargoless keeps a warm `rust-analyzer`,
content-addresses every build's input set, and skips the rebuild
entirely when the source state hasn't changed. When the tree does go
green, it publishes the latest green WASM artifact to a pointer file
via an atomic temp+fsync+rename; when the tree goes red, the pointer
**does not move** — so anything consuming that pointer (a static
server, a CI step, an agent) can rely on never seeing a broken build.

The net effect is fewer CPU-seconds spent per save, lower peak RSS,
and more dev cycles per battery — without giving up the verdict
honesty that makes the codebase trust-worthy in the first place. It
is the result of a vision cut: every type and decision in the project
is justified by either sharpening the codebase's self-knowledge or
shortening the latency from brokenness to signal. Anything that does
neither isn't here.

> **Naming note:** the public product name is still TBD. The repository
> and binary name `tftrunk` / `cargoless` is a working placeholder; the
> capabilities below are unaffected.

---

## What cargoless v0 is (and isn't)

**v0 IS** — a *headless continuous checker and latest-green publisher*:

- `tftrunk check` — one-shot verdict + diagnostics. Green or red, exit
  code reflects it, errors are formatted file:line:col + severity + code
  + message.
- `tftrunk watch` — continuous timestamped verdict stream with
  per-file granularity.
- `tftrunk build --watch --out <dir>` — wraps `trunk build` and
  publishes the latest green WASM artifact via an atomic
  `.cargoless/latest-green` pointer that **only advances on green**.
- Zero-config — auto-detects `cdylib` + `wasm32` / `leptos` projects.
- Survives `kill -9` of the underlying rust-analyzer subprocess; the
  daemon restarts it transparently.

**v0 is NOT** — a `trunk serve` drop-in replacement (yet). cargoless v0
does not include a browser/HTTP/WebSocket layer; that's v0.1, deferred.
If you need a browser-reload loop today, point `trunk serve`
(or any static server) at the directory cargoless publishes.

See [`ROADMAP.md`](ROADMAP.md) for v0 acceptance criteria, the v0.1
deferred work, and the v1 parking lot.

---

## Source & mirrors

- **Canonical public source:** [`github.com/TriformAI/cargoless`](https://github.com/TriformAI/cargoless) — the OSS-facing home; where issues, PRs, releases, and prebuilts live.
- **Internal dev mirror:** [`forgejo.triform.dev/triform/cargoless`](https://forgejo.triform.dev/triform/cargoless) — where the agent team's integration CI runs (dedicated cargoless-builder pod + `scripts/ci-gate` + Forgejo Actions). Contributor PRs are welcome on GitHub; the maintainers cherry-pick into Forgejo for the integration loop.

## Install

> **Pre-release.** The release-tagged install commands below will work
> once `v0.1.0` is cut. Today, only the from-source install against the
> GitHub development tip is supported and proven end-to-end in a clean
> environment.

**Install the current development tip (works today):**

```bash
cargo install --git https://github.com/TriformAI/cargoless.git \
              tf-cli --branch main --locked
```

**Why the explicit `tf-cli`:** `cargo install --git` walks the entire
repo for `Cargo.toml` files and refuses to pick when multiple
installable binary crates exist. This repo's `bench/{harness,fixture}`
sub-workspaces produce `ra-latency`, `cargoless-bench`, and
`cargoless-bench-fixture` binaries that cargo treats as candidates.
Without `tf-cli`, you get:

> error: multiple packages with binaries found: cargoless-bench-fixture,
> cargoless-bench-harness, tf-cli.

**Why `--locked`:** the workspace ships a committed `Cargo.lock`; `--locked`
makes the dependency graph identical to what CI / `scripts/ci-gate` proved
green. See [D-RELEASE Appendix B](docs/design/D-RELEASE.md#appendix-b--why---locked-everywhere).

> The default install includes the wired daemon (`build --watch --out`
> publisher pipeline). As of commit `1c25017`, the `integration` feature
> is on by default on `tf-cli`. Users who want only the standalone
> checker semantics can opt out via `--no-default-features`.

**Once `v0.1.0` releases:**

```bash
# Source build via crates.io (universal: any platform with rustc)
cargo install <pubname>           # <pubname> = TBD per D1

# Prebuilt via cargo-binstall (Linux x86_64-gnu + macOS aarch64/x86_64)
cargo binstall <pubname>
```

Prebuilts at first release: `x86_64-unknown-linux-gnu`,
`aarch64-apple-darwin`, `x86_64-apple-darwin`. Other targets (Linux
aarch64, Windows) fall back to `cargo install` (source compile). See
[docs/design/D-RELEASE.md §3](docs/design/D-RELEASE.md#3-targets--the-honest-install-matrix)
for the full matrix.

`tftrunk build --watch --out` wraps `trunk build` — install the
upstream `trunk` for the WASM artifact step:

```bash
cargo install --locked trunk
```

cargoless surfaces an actionable error if `trunk` is missing from PATH.

---

## Quick start

```bash
# In a Rust + WASM project root (auto-detected: cdylib + wasm32 / leptos)
$ tftrunk check
>> checking /work/my-app (auto-detected: cdylib + leptos (Leptos CSR))
ok green — every tracked file compiles

# A continuous verdict stream — first verdict in under a second
$ tftrunk watch
>> [+   0.083s] daemon up, watching /work/my-app
>> [+   0.741s] /work/my-app/src/lib.rs: Green
^C

# Publish the latest green WASM artifact to ./dist; pointer never
# advances on red.
$ tftrunk build --watch --out ./dist
>> publishing latest-green to .cargoless/latest-green → ./dist
ok green — latest-green @ <hash>
```

Edit a file with a real error; the next verdict line tells you what
broke. Fix it; the next verdict line says green; `./dist` advances. Try
introducing a syntax error; observe that `.cargoless/latest-green`
**does not move** until you fix it.

For the full v0 surface, see [`ROADMAP.md`](ROADMAP.md#v0-capabilities-available-today-on-main).

---

## Performance vs alternatives

The differentiator isn't raw save→verdict latency — that's bounded by
`cargo check`, which dominates the wall-clock no matter which tool
wraps it. The differentiator is **throughput**: how much CPU and
memory cargoless burns per save compared to the alternatives, and
therefore how many dev cycles you get out of the same battery before
the fans spin up.

The architectural asymmetry that produces the gap:

- **Warm `rust-analyzer`.** RA's multi-second cold start happens
  **once** per cargoless session. `trunk serve` doesn't use RA at
  all (it shells out to `cargo build` on every save). `bacon` spawns
  a fresh `cargo check` process per cycle. cargoless's warm-LSP
  architecture means a save that doesn't actually change the
  type-graph costs near-zero work.
- **CAS dedupe.** cargoless content-addresses the full input set
  (source tree + `Cargo.lock` + toolchain + target + config). On a
  save where the hashed input set is unchanged — a no-op edit, a
  `git checkout` round-trip, or a save where only comments / strings
  / formatting changed — **the build is skipped entirely**. `trunk
  serve` rebuilds-everything-on-each-save unconditionally; `bacon`
  re-runs `cargo` regardless of whether the work was already done.
- **Headless.** No HTTP server, no WebSocket channel, no
  browser-keepalive overhead. The v0 surface is a daemon + a CLI;
  the v0.1 server adapter is an opt-in layer for users who want it.

<!--
TBD-NUMBERS — bench-lead's throughput report (CPU%/RSS/CPU-seconds on
Leptos fixture, 3 tools) + perf-recon agent's independent cross-check
(~60-90min ETA from 2026-05-17 15:11). Numbers slot into the tables
below via a small follow-up commit when both reports land.

Throughput report covers: CPU-seconds per save (median), peak RSS,
saves-per-CPU-minute. Cross-check covers: methodology audit + repro
on a second host. Both required before the headline is concrete.
-->

**Qualitative comparison** (numbers follow):

| Tool | CPU per save | Peak RSS | Verdict honesty |
|---|---|---|---|
| cargoless | LOW — CAS skips identical inputs; warm RA avoids cold-start per cycle | LOW — no HTTP/WS server overhead | publishes green only; pointer atomic |
| `trunk serve` | HIGH — rebuilds-everything per save | MEDIUM — HTTP + WS + browser-keepalive | serves on every build, red or green |
| `bacon` | MEDIUM — re-runs cargo per cycle | LOW — terminal-only | terminal-only |

**Measured numbers** (TBD — from `bench/run.sh` + independent cross-check):

<!-- TBD-NUMBERS: filled in when bench-lead's throughput report and the perf-recon cross-check both land. -->

| Tool | CPU-seconds per save (median) | Peak RSS (MB) | Saves per CPU-minute |
|---|---|---|---|
| cargoless | _TBD_ | _TBD_ | _TBD_ |
| `trunk serve` | _TBD_ | _TBD_ | _TBD_ |
| `bacon` | _TBD_ | _TBD_ | _TBD_ |

For raw save→verdict latency (the inner-loop responsiveness number),
cargoless lands at _TBD_ on the reference Leptos fixture — bounded
by `cargo check`'s authoritative-tier compile step, which dominates
the wall-clock regardless of which tool wraps it. No sub-second
artifact-publish claim is made.

**Methodology** (for transparency once numbers land):

- Bench fixture: a real Leptos `cdylib + rlib` CSR project at the
  honest-size floor (≥17 files / 922 LOC; the bench refuses to
  shrink the fixture for flatter numbers).
- Two-mode reporting: checker mode (save→verdict) and artifact mode
  (save→publish) reported **separately**, never blended into a
  single number.
- Throughput measurement: CPU-seconds + RSS sampled from the OS over
  a full edit session (N saves at fixed interval); reported per-save
  median and peak.
- Driver: identical save events on the same fixture across all
  three tools; cross-checked by a second host running the same
  harness.
- Reproducible: `bench/run.sh` is committed in this repo; rerun on
  your own machine and tell us if you see different numbers.

See [`docs/dogfood/PHASE-2-REPORT.md`](docs/dogfood/PHASE-2-REPORT.md)
for the launch-hardening evidence trail (12 field findings, 11 fixed
before launch).

---

## Workspace

| Crate | Role |
|---|---|
| `tf-proto` | Shared contract types (daemon ↔ build ↔ future remote backends). |
| `tf-cas` | Content-addressed store. `ContentStore` trait + local-disk impl. |
| `tf-core` | The daemon: watcher, rust-analyzer wrapper, green/red model, build orchestration. |
| `tf-cli` | The binary: `check` / `watch` / `build` / `status` / `clean`. |

`bench/{harness,fixture}` are standalone non-workspace crates with
`publish = false` baked in — they exist to run the AC#7 comparative
benchmark and are not shipped to crates.io.

For the cross-crate contract and why `tf-proto` is dependency-free in
v0, see [`docs/DESIGN.md`](docs/DESIGN.md).

---

## Contributing

Issues, PRs, and discussions: see [`CONTRIBUTING.md`](CONTRIBUTING.md).

The maintainers commit to a **48-hour acknowledgement window for the
first two weeks after launch**, then a sustainable one-week cadence
after that. The commitment is documented in CONTRIBUTING.md; a missed
acknowledgement is itself a GitHub-issue-worthy event.

---

## Status

v0 feature-complete on `main`; launch hardening in progress. Tracked
publicly via [GitHub Issues](https://github.com/TriformAI/cargoless/issues);
the internal agent-team backlog lives in Plane (project "CWDL"). See
[`ROADMAP.md`](ROADMAP.md) for the 9-criterion v0 definition-of-done and
the v0.1 / v1 phases.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
