# cargoless

<!--
TBD-POSITIONING — operator picks one of:

  Framing B (DEFAULT, leading candidate per 2026-05-17 lead message):
      ARCHITECTURAL HONESTY: "the codebase always knows what works"
      → verdict honesty + never-publish-red + CAS dedupe.
      NOT a speed claim.

  Framing A (commented-out, swap-in if AC#7 numbers support it):
      SPEED: "faster than trunk serve / bacon."
      Provisional — current cargo-check-bound numbers suggest the
      INCONCLUSIVE-WITH-CAUSE bench verdict will not support a
      clean speed-win headline.

Headline below uses Framing B as the live copy; Framing A sits in the
same file as a one-line HTML comment, ready to swap in via a small
follow-up commit when the positioning decision lands.
-->

> **The codebase always knows what works, and tells you the moment it
> doesn't.**

<!-- TBD-POSITIONING, Framing A alternative (if numbers support it):
> Sub-second save→verdict for Rust+WASM. Faster than `trunk serve` on
> the inner loop; honest about what it doesn't replace.
-->

cargoless is a headless dev-loop daemon for Rust+WASM projects. It
keeps a warm `rust-analyzer`, watches your tree, and emits a continuous
green/red verdict the moment a saved file changes the picture. When the
tree goes green, it publishes the latest green WASM artifact to a
pointer file; when the tree goes red, the pointer **does not move** —
so anything consuming that pointer (a static server, a CI step, an
agent) can rely on never seeing a broken build.

It is the result of a vision cut: every type and decision in the
project is justified by either sharpening the codebase's self-knowledge
or shortening the latency from brokenness to signal. Anything that
does neither isn't here.

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

<!--
TBD-NUMBERS — the AC#7 comparative benchmark (bench/run.sh, in flight
as Plane CWDL-36) measures cargoless's save→verdict and
save→publish latencies on a controlled Leptos fixture, alongside the
equivalent dimensions for `trunk serve` and `bacon`. When the bench
PASSes (or settles INCONCLUSIVE-WITH-CAUSE), the headline numbers
below are filled in via a small follow-up commit.

Current operator framing: the AC#7 bench will likely report
INCONCLUSIVE-WITH-CAUSE (cargo-check-bound save→verdict is ~26s on
real Leptos projects; this is not a speed-win race against bacon).
The positioning below leans into ARCHITECTURAL HONESTY rather than
SPEED as the value-prop — see the TBD-POSITIONING note at the top of
this file.
-->

cargoless's design priority is **verdict honesty**, not raw speed:

- **Never publish red.** The `.cargoless/latest-green` pointer is
  atomic (temp + fsync + rename) and only ever advances on a servable
  green build. A red tree or a failed build leaves it byte-unmoved.
  Verified headless (no browser dependency).
- **CAS dedupe.** Identical source state is a cache hit — the build
  is skipped, not re-run. Saves a full WASM rebuild on no-op edits and
  on `git checkout`-style round-trips.
- **Per-file verdict granularity.** The watch stream tells you which
  file went red, not just that the tree went red.

For raw save→verdict latency:

<!-- TBD-NUMBERS: filled in when AC#7 bench reports. -->

| Tool | Save→verdict (median) | Save→artifact published (median) | Verdict honesty |
|---|---|---|---|
| cargoless | _TBD from AC#7 bench_ | _TBD from AC#7 bench_ | publishes green only |
| `trunk serve` | _TBD comparative measurement_ | _TBD_ | serves on every build, red or green |
| `bacon` | _TBD comparative measurement_ | n/a (terminal-only, no publish) | terminal-only |

**Methodology** (for transparency once numbers land):

- Bench fixture: a real Leptos `cdylib + rlib` CSR project at the
  honest-size floor (≥17 files / 922 LOC; the bench refuses to shrink
  the fixture for flatter numbers).
- Two-mode reporting: checker mode (save→verdict) and artifact mode
  (save→publish) reported separately — never blended into a single
  number, and no sub-second artifact claim is made.
- Driver: identical save events on the same fixture across all three
  tools; each measurement is `(t_verdict - t_save)` from monotonic
  clock samples.
- Reproducible: `bench/run.sh` is committed in this repo; rerun on
  your own machine.

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
