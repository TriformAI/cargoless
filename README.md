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

The net effect is fewer CPU-seconds spent per save — and therefore more
dev cycles per battery — without giving up the verdict honesty that makes
the codebase trust-worthy in the first place. (Memory is a different
story, and we are honest about it below: steady-state RSS is
rust-analyzer-dominated, not a by-default win.) It
is the result of a vision cut: every type and decision in the project
is justified by either sharpening the codebase's self-knowledge or
shortening the latency from brokenness to signal. Anything that does
neither isn't here.

> **Name:** the product, the published crate, and the binary you run are
> all **`cargoless`** (operator decision D1, 2026-05-17). The internal
> library crates remain `tf-proto` / `tf-cas` / `tf-core`.

---

## What cargoless v0 is (and isn't)

**v0 IS** — a *headless continuous checker and latest-green publisher*:

- `cargoless check` — one-shot verdict + diagnostics. Green or red, exit
  code reflects it, errors are formatted file:line:col + severity + code
  + message.
- `cargoless watch` — continuous timestamped verdict stream with
  per-file granularity.
- `cargoless build --watch --out <dir>` — wraps `trunk build` and
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
              cargoless --branch main --locked
```

**Why the explicit `cargoless` package arg:** `cargo install --git`
walks the entire repo for `Cargo.toml` files and refuses to pick when
multiple installable binary crates exist. This repo's
`bench/{harness,fixture}` sub-workspaces produce `ra-latency`,
`cargoless-bench`, and `cargoless-bench-fixture` binaries that cargo
treats as candidates. Without the explicit `cargoless` arg, you get:

> error: multiple packages with binaries found: cargoless,
> cargoless-bench-fixture, cargoless-bench-harness.

**Why `--locked`:** the workspace ships a committed `Cargo.lock`; `--locked`
makes the dependency graph identical to what CI / `scripts/ci-gate` proved
green. See [D-RELEASE Appendix B](docs/design/D-RELEASE.md#appendix-b--why---locked-everywhere).

> The default install includes the wired daemon (`build --watch --out`
> publisher pipeline). As of commit `1c25017`, the `integration` feature
> is on by default on `cargoless`. Users who want only the standalone
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

`cargoless build --watch --out` wraps `trunk build` — install the
upstream `trunk` for the WASM artifact step:

```bash
cargo install --locked trunk
```

cargoless surfaces an actionable error if `trunk` is missing from PATH.

---

## Quick start

```bash
# In a Rust + WASM project root (auto-detected: cdylib + wasm32 / leptos)
$ cargoless check
>> checking /work/my-app (auto-detected: cdylib + leptos (Leptos CSR))
ok green — every tracked file compiles

# A continuous verdict stream — first verdict in under a second
$ cargoless watch
>> [+   0.083s] daemon up, watching /work/my-app
>> [+   0.741s] /work/my-app/src/lib.rs: Green
^C

# Publish the latest green WASM artifact to ./dist; pointer never
# advances on red.
$ cargoless build --watch --out ./dist
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
wraps it. The differentiator is **per-edit CPU throughput**: how much
work cargoless avoids redoing per save, and therefore how many dev
cycles you get out of the same battery before the fans spin up.

The honest one-paragraph summary (bench-lead's wording, unedited):

> cargoless does ~half the per-edit CPU of `trunk serve` — it rebuilds
> on confirmed-green edges, not blindly every keystroke. Memory is
> rust-analyzer-dominated (~2 GB default on proc-macro projects); the
> `--features` knob cuts ~75% and a v0.1 auto-narrow change moves the
> default there.

Two things to read carefully out of that: the CPU win is real and
roughly halves `trunk serve`'s per-edit cost; the memory picture is
**not** a by-default win — steady-state RSS is dominated by
rust-analyzer (which runs proc-macro expansion by default), so
cargoless's footprint is comparable to an editor running RA, not
lean. The `--features` knob already recovers most of that today; the
v0.1 auto-narrow change (see [`ROADMAP.md`](ROADMAP.md)) makes the
narrowed configuration the default. We say this plainly rather than
quoting a flattering RSS number we can't honestly default to.

> **Numbers below are PENDING bench-lead's Component-2 two-source
> confirmation.** The `~half` / `~2 GB` / `~75%` figures above are
> bench-lead's pre-confirmation estimate; the tables stay marked
> _PENDING_ until the second-host cross-check lands, at which point a
> small follow-up commit fills them. No headline number is finalized
> here.

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
PENDING bench-lead Component-2 two-source confirmation. The numeric
cells below stay _PENDING_ until BOTH (a) bench-lead's throughput
report (CPU-seconds/edit, peak RSS, saves-per-CPU-minute on the Leptos
fixture, 3 tools) AND (b) the independent second-host cross-check land.
Then a small follow-up commit fills the tables. Do not substitute a
single-source estimate for _PENDING_; the verbatim "~half / ~2 GB /
~75%" prose above is bench-lead's explicitly pre-confirmation estimate
and is marked as such inline.
-->

**Qualitative comparison** (numbers follow):

| Tool | CPU per save | Peak RSS | Verdict honesty |
|---|---|---|---|
| cargoless | LOW — ~½ of `trunk serve`; CAS skips identical inputs, warm RA avoids cold-start per cycle | HIGH by default — RA-dominated (~2 GB on proc-macro projects); `--features` knob cuts ~75%, v0.1 auto-narrows the default | publishes green only; pointer atomic |
| `trunk serve` | HIGH — rebuilds-everything per save | MEDIUM — HTTP + WS + browser-keepalive | serves on every build, red or green |
| `bacon` †| MEDIUM — re-runs cargo per cycle | LOW — terminal-only | terminal-only |

† Not like-for-like vs `bacon`: `bacon` is a terminal save→verdict
checker, not a build+publish loop. The comparison row is for the
checker tier only; the artifact-publish dimension has no `bacon`
counterpart.

**Measured numbers** — _PENDING bench-lead Component-2 two-source
confirmation_ (from `bench/run.sh` + an independent second-host
cross-check; a follow-up commit fills these in):

<!-- PENDING bench-lead Component-2: numeric cells fill in only when
the throughput report AND the second-host cross-check both land. Until
then every cell stays _PENDING_ — do not substitute a single-source
estimate. -->

| Tool | CPU-seconds per edit (median) | Peak RSS (MB) | Saves per CPU-minute |
|---|---|---|---|
| cargoless | _PENDING_ | _PENDING_ | _PENDING_ |
| `trunk serve` | _PENDING_ | _PENDING_ | _PENDING_ |
| `bacon` †| _PENDING_ | _PENDING_ | _PENDING_ |

† `bacon` is not a like-for-like comparator — it is a checker, not a
build+publish loop; its row covers the checker tier only.

For raw save→verdict latency cargoless reports **two tiers**, not one
number (see [`docs/design/D-A2-RENEGOTIATION.md`](docs/design/D-A2-RENEGOTIATION.md)):

- **AC#2a — RA-incremental hint:** median ≤1s (field-measured ~0.74s
  on the dogfood Leptos project post-debouncer-fix). A fast hint that
  can flip the verdict RED instantly; it does **not** by itself prove
  the tree compiles.
- **AC#2b — authoritative verdict:** bounded by `cargo check` itself
  (seconds on small projects, ~20-30s on a Leptos-sized tree). Only
  this tier drives GREEN. cargoless's *added* overhead targets ≤10% of
  cargo's own time — _PENDING_ the relative-cost number from the
  comparative bench.

No sub-second artifact-publish claim is made.

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
| `cargoless` | The binary: `check` / `watch` / `build` / `status` / `clean`. (Crate dir: `crates/tf-cli/`.) |

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
