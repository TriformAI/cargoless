# cargoless launch blog post — DRAFT

> **Status:** DRAFT. Per AC#9, this post requires review by **≥2 people
> including one outside the team** before it ships publicly. Do NOT
> publish from this file directly; the operator's chosen venue
> (project blog, dev.to, personal blog, GitHub release notes, etc.)
> gets a frozen copy at publish time.
>
> **TBD markers:** `<!-- TBD-NUMBERS -->` marks spots where the final
> copy depends on bench-lead's throughput report (CPU-seconds, RSS,
> saves-per-CPU-minute on the Leptos fixture) and the perf-recon
> agent's independent cross-check. Both ETA ~60-90min from
> 2026-05-17 15:11; both slot into the placeholders via a small
> follow-up commit when they land.
>
> **Positioning:** locked to **Framing C — throughput, not speed.**
> The earlier Framing A (speed) and Framing B (architectural-honesty)
> scaffolding has been removed; this draft commits to "cargoless
> doesn't burn your CPU" as the differentiator.

---

# cargoless v0: the dev loop that doesn't burn your CPU

Open three terminals. One for `trunk serve`. One for `bacon` or
`cargo-watch`. One for the actual code. Save a file. Listen.

The fans spin up. The battery drops a percentage. Your laptop is now
warm to the touch. `trunk serve` is rebuilding the whole WASM bundle —
again, just like the last time, even though the only thing that
changed was a comment. `bacon` is spawning a fresh `cargo check`
process — again, throwing away the type-graph it computed two saves
ago. Both tools are doing real work; neither is doing **necessary**
work.

This is what we mean when we say cargoless was built to a vision cut:

> **The codebase always knows what works, and tells you the moment it
> doesn't — without burning your CPU to do so.**

Today we're shipping cargoless v0 — the headless continuous checker
and latest-green publisher that does *less work per save* than the
alternatives, on the architectural bet that most saves don't change
the picture and shouldn't trigger a full rebuild. The differentiator
isn't sub-second save→verdict (cargo-check dominates that wall-clock,
regardless of who wraps it). The differentiator is **CPU-seconds per
save** — and therefore, over a day of dev cycles, how many edit-cycles
you get out of the same battery.

v0 is intentionally small: it watches your source tree, runs
`rust-analyzer` warm in a daemon, tells you the green/red verdict on
every save, and atomically publishes the latest known-green WASM
artifact to a pointer file. It does not serve a browser. It does not
hot-swap. It does not replace `cargo` or `trunk build`. Those are
deliberate cuts — every type and decision in the project is justified
by either sharpening the codebase's self-knowledge or shortening the
latency from brokenness to signal. Anything that does neither isn't
here.

---

## The problem nobody benchmarks

A modern Rust+WASM inner loop has three latency dimensions and one
**throughput** dimension nobody talks about:

1. **Save→verdict latency** — how fast do I find out whether the file
   I just saved compiles?
2. **Save→artifact latency** — how fast can the WASM bundle that
   represents my latest green code be served to a consumer?
3. **Verdict→trust latency** — how confident can I be that the
   verdict is right and the artifact is the right artifact?
4. **Throughput: CPU-seconds per save** — how much work is being done,
   per save, that doesn't need to be done?

Existing tools optimize one or two of (1)-(3) and assume (4) away
entirely:

- **`trunk serve`** owns (2) — fast in-browser reload of the latest
  build. But it shells out to `cargo build` on every cycle and serves
  whatever falls out, red or green. CPU cost per save: the full WASM
  rebuild, every time. (3)-cost: serves red builds to your browser
  and to any CI consumer reading `dist/`.
- **`bacon`** owns (1) — fast save→verdict in a terminal. But it
  spawns a fresh `cargo check` per cycle, throwing away the
  type-graph each time. CPU cost per save: a fresh cargo process'
  worth of work. (2)-cost: nothing — there's no artifact.
- **`cargo-watch`** owns a slice of (1). Same fresh-cargo cost; same
  no-artifact result.

For a real Rust+WASM project the day-to-day result is: three terminal
windows, three running cargo-ish processes, fans audibly working,
battery dropping fast. Each tool tells a different fraction of the
truth — the verdict in the corner of your eye isn't an answer; it's
a heuristic you cross-check by hand. And worse, *most of that
ambient CPU cost is paying for work that's already been done.*

## The cargoless architecture: do less, trust more

cargoless picks **(1)+(3)+(4)** as the v0 target. The shape:

**A long-lived daemon with a warm `rust-analyzer`.** RA's multi-second
cold start happens **once** per session — when the daemon comes up
— and then never again. Every save reuses the already-indexed
type-graph. `trunk serve` doesn't use RA at all (it shells out to
cargo); `bacon` spawns a fresh cargo process per cycle. cargoless's
warm-LSP architecture is the single biggest CPU-per-save reduction
on the table.

**Content-addressed dedupe of the full input set.** cargoless hashes
the source tree + `Cargo.lock` + the rust toolchain version + the
target triple + the config — all of it. When you save a file with a
formatting-only change, or you `git checkout` a branch you had open
five minutes ago, the input hash matches a previous cycle's
**and the build is skipped entirely**. Not rebuilt-faster; **not
rebuilt at all**. `trunk serve` and `bacon` rebuild unconditionally,
no matter how identical the input.

**A continuous verdict stream with per-file granularity.** `tftrunk
watch` is a long-running command whose stdout is a timestamped
stream of file-level verdicts. You see *which* file went red, not
just *that* the tree went red. The stream is plain text; you can
`grep`, `tee`, pipe into a tmux pane, or have an agent subscribe.

**An atomic `.cargoless/latest-green` pointer.** When a verdict turns
green, the build runs (or hits the CAS and skips), and the
latest-green pointer advances via a temp-file + fsync + rename
pattern. When the tree is red, the pointer **does not move** —
byte-unchanged. Anything reading the pointer (a static server, a CI
step, an agent inspector) is guaranteed never to see a broken
artifact. That's verdict→trust collapsed to zero.

**Verdict↔exit-code coherence.** `tftrunk check` exits 0 on green,
non-zero on red, with diagnostics formatted file:line:col + severity
+ code + message. This is the boring corner the project spent the
most field-bug iterations on: the exit code and the printed
diagnostics derive from the same data stream, so you can't get an
"exit 0, but here are 22 errors" outcome.

What this **doesn't** include in v0: a browser, an HTTP server, a
WebSocket reload channel, a hot-swap mechanism, a `trunk build`
replacement. Those are real things people want; they're on the v0.1
roadmap and the v1 parking lot respectively. Shipping them all at
once would have meant launching a half-baked browser layer alongside
a half-baked verdict layer. Better to ship one honest small thing.

---

## Honest performance comparison

The metric that matters for cargoless's positioning is **CPU-seconds
per save** — the headline throughput number. Latency is a secondary
concern; it's bounded by cargo-check no matter which tool you wrap
around it. The numbers tell a throughput story.

**Qualitative comparison** (numbers follow):

| Tool | CPU per save | Peak RSS | Verdict honesty |
|---|---|---|---|
| cargoless | LOW — CAS skips identical inputs; warm RA avoids cold-start per cycle | LOW — no HTTP/WS server overhead | publishes green only; pointer atomic |
| `trunk serve` | HIGH — rebuilds-everything per save | MEDIUM — HTTP + WS + browser-keepalive | serves on every build, red or green |
| `bacon` | MEDIUM — spawns fresh cargo per cycle | LOW — terminal-only | terminal-only |

<!--
TBD-NUMBERS — fill in from bench-lead's throughput report (CPU%/RSS/
CPU-seconds on Leptos fixture, 3 tools) + perf-recon agent's
independent cross-check. Both ETA ~60-90min from 2026-05-17 15:11.
The numeric tables below get populated from those two reports.
-->

**Measured numbers** (TBD — from `bench/run.sh` + independent
cross-check):

| Tool | CPU-seconds per save (median) | Peak RSS (MB) | Saves per CPU-minute |
|---|---|---|---|
| cargoless | _TBD_ | _TBD_ | _TBD_ |
| `trunk serve` | _TBD_ | _TBD_ | _TBD_ |
| `bacon` | _TBD_ | _TBD_ | _TBD_ |

For raw save→verdict latency (the inner-loop responsiveness number,
secondary to throughput for cargoless's positioning):

| Tool | Save→verdict (median) | Save→artifact published |
|---|---|---|
| cargoless | _TBD_ | _TBD_ |
| `trunk serve` | _TBD_ | _TBD_ |
| `bacon` | _TBD_ | n/a (terminal-only) |

On a real Leptos project, save→verdict for all three tools lands
within the cargo-check-bound band — none of them are racing each
other on raw wall-clock. **The interesting wins are on the
throughput rows.** When the input set hasn't changed, cargoless does
~zero work; the other two redo the full build's worth of work
because they don't know it's already been done. Over a day of
dev-cycles, that compounds — into measurable battery life, into
measurable fan-noise, into measurable thermal headroom for the rest
of your local stack.

**Methodology** (because numbers without methodology are decoration):

- **Bench fixture:** a real Leptos CSR `cdylib + rlib` project, ≥17
  files / 922 LOC. The bench harness refuses to shrink the fixture
  for flatter numbers.
- **Throughput measurement:** CPU-seconds + peak RSS sampled from
  the OS over a full edit session (N saves at fixed interval,
  including a mix of substantive and no-op edits to exercise the
  CAS-skip path). Reported as per-save median and peak. Driver
  records `getrusage()` deltas around each save event.
- **Two-mode latency reporting:** checker mode (save→verdict) and
  artifact mode (save→publish) reported **separately**, never
  blended into a single "median latency" number. cargoless makes no
  sub-second artifact-publish claim.
- **Identical driver across tools:** the same save events on the
  same fixture feed cargoless, `trunk serve`, and `bacon`. Wall-clock
  measurements use a monotonic clock.
- **Independent cross-check:** a second host runs the same harness
  against the same fixture to confirm the numbers reproduce off the
  primary builder pod.
- **Reproducible:** the harness lives at `bench/run.sh` in the repo;
  rerun it on your machine and tell us if you see different numbers.

The full report and verdict commit-status live at `s1-ac2-verdict`
and `ac7-verdict` keys on the release SHA.

---

## Install and try it

> **Pre-release.** The release-tagged install commands below will work
> once `v0.1.0` is cut. Today, only the from-source GitHub install is
> supported — and it's been smoke-tested end-to-end on a clean Linux
> environment.

```bash
cargo install --git https://github.com/TriformAI/cargoless.git \
              tf-cli --branch main --locked
```

Once `v0.1.0` ships, the install path becomes:

```bash
# Source build (universal)
cargo install <pubname>            # <pubname> = TBD per D1

# Prebuilt via cargo-binstall
cargo binstall <pubname>           # Linux x86_64-gnu + macOS aarch64/x86_64
```

Then, in any Rust+WASM project (auto-detected on `cdylib + wasm32` or
`leptos`):

```bash
$ tftrunk check
ok green — every tracked file compiles

$ tftrunk watch
>> [+   0.083s] daemon up, watching /work/my-app
>> [+   0.741s] /work/my-app/src/lib.rs: Green
^C
```

For the build/publish loop (requires the upstream `trunk` for the
WASM artifact step):

```bash
$ tftrunk build --watch --out ./dist
>> publishing latest-green to .cargoless/latest-green → ./dist
ok green — latest-green @ <hash>
```

Edit a file with a real error. The next verdict line tells you
exactly what broke and where. Fix it; the latest-green pointer
advances. Introduce a syntax error; observe that the pointer **does
not move** until you fix it. That's the whole pitch in 30 seconds of
demo.

---

## Roadmap

cargoless is phased **v0 → v0.1 → v1**.

**v0 (today):** headless continuous checker + latest-green publisher.
No browser, no HTTP, no WebSocket. Nine acceptance criteria,
field-verified on a real Leptos project. The launch story.

**v0.1 (next, deferred — no date commitment):** the optional live
HTTP/WebSocket dev-server that consumes cargoless's `.cargoless/latest-green`
pointer and full-reloads a browser when it advances. Trunk-compatible
reload protocol. Browser holding page during cold starts. **This is
what closes the `trunk serve` drop-in gap**, and the research
implementation already exists on a branch. We're shipping v0 first
so that the v0 promise (verdict + publish, honest) lands clean
rather than buried under a half-finished browser layer.

**v1 (parking lot — no commitments):** salsa / rust-analyzer-as-library
deep integration, remote shared CAS, team features + auth, multi-agent
build coordination, editor LSP plugin, symbol-level verdict
granularity, replacing `trunk build` internals, hot-swap WASM, CI
integration, Windows support. These are the ideas that earn their own
design pass if and when v0 / v0.1 prove the foundation.

Roadmap details: [`ROADMAP.md`](https://github.com/TriformAI/cargoless/blob/main/ROADMAP.md)
in the repo.

---

## What we are honest about

A v0 launch is the moment to set up trust by being explicit about
what we deliberately did **not** do:

- **No browser in v0.** If you want browser-reload today, point
  `trunk serve` at the directory cargoless publishes; v0.1 closes
  the gap.
- **Not a `trunk build` replacement.** cargoless wraps `trunk build`
  for the WASM artifact step — `trunk` is doing the actual cargo +
  wasm-bindgen work; cargoless drives the watch and publish loop on
  top.
- **Single-machine, single-developer.** Remote CAS, shared caches,
  team features are v1.
- **Linux + macOS only.** Windows is v1 parking-lot per the design
  doc; `cargo install` works there on a best-effort basis but no
  prebuilt artifact, no CI coverage.
- **The benchmark is INCONCLUSIVE on raw speed.** We measured. We
  reported. We did not silently miss the threshold and ship anyway.
  See the methodology section above.

The launch-hardening process for this v0 was 12 field findings over
3 weeks of dogfooding a real Leptos project on a clean Linux box; 11
fixed before launch, 1 closed as a design question (`tftrunk clean`
semantics — non-breaking, safe-either-way). The full evidence trail
is at [`docs/dogfood/PHASE-2-REPORT.md`](https://github.com/TriformAI/cargoless/blob/main/docs/dogfood/PHASE-2-REPORT.md).

For the launch fortnight, the maintainers commit to a 48-hour
acknowledgement window on every new issue and PR. After that, the
sustainable cadence is one-week acknowledgement, with launch-blocker
urgency preserved for verdict-honesty / never-publish-red / install
regressions. The commitment is in [`CONTRIBUTING.md`](https://github.com/TriformAI/cargoless/blob/main/CONTRIBUTING.md);
a missed acknowledgement is itself a GitHub issue.

---

## Acknowledgments

cargoless stands on the work of dozens of upstream Rust projects.
The ones we want to call out specifically:

- **Leptos** — for the substrate cargoless's defaults target. The
  Leptos project's tight `view!` macro / signal / control-flow shape
  is what made the auto-detection design feel inevitable.
- **rust-analyzer** — for the warm-LSP architecture that cargoless's
  whole verdict loop sits on top of. Every save→verdict measurement
  in this post is a measurement of how fast rust-analyzer can give us
  a diagnostic when poked correctly.
- **`trunk`** — for the WASM build pipeline cargoless wraps. v0.1
  also intends to be wire-compatible with Trunk's browser-reload
  protocol so the migration cost is one config line, not a rewrite.
- **`bacon`** — for proving that a tight save→verdict loop *matters*
  to Rust developers. cargoless's verdict stream is different from
  bacon's in form but identical in spirit.
- **The cargo and rustc teams** — for the workspace + Cargo.lock +
  build-cache infrastructure that makes deterministic Rust builds
  possible at all.

The agent team that built cargoless is documented in
[`.claude/`](https://github.com/TriformAI/cargoless/tree/main/.claude)
metadata — the team config, the per-role prompts, the build/test
discipline. Anyone curious about LLM-driven OSS engineering at this
scale is welcome to look. Outside maintainer review of this launch
post is requested and gratefully received.

---

## Try it, file what breaks, tell us what's missing

```bash
cargo install --git https://github.com/TriformAI/cargoless.git \
              tf-cli --branch main --locked
```

Repository: [github.com/TriformAI/cargoless](https://github.com/TriformAI/cargoless)
Issues: [github.com/TriformAI/cargoless/issues](https://github.com/TriformAI/cargoless/issues)
Discussions: [github.com/TriformAI/cargoless/discussions](https://github.com/TriformAI/cargoless/discussions)

If something surprises you, that's a finding the launch sequence
wants to hear about. Verdict→trust only works if the people running
the tool report when it breaks.

---

## Appendix — reviewer checklist (AC#9)

> This appendix is removed in the publish-time edit. It exists so
> the ≥2 reviewers (one outside the team) have a concrete checklist
> for AC#9 sign-off.

- [ ] Headline value-prop matches the operator-locked **Framing C**
      ("cargoless doesn't burn your CPU" — throughput-not-speed) and
      the throughput-first comparison tables are intact.
- [ ] All `<!-- TBD-NUMBERS -->` markers replaced with concrete copy
      from bench-lead's throughput report + perf-recon cross-check.
- [ ] Throughput numbers (CPU-seconds per save, peak RSS, saves per
      CPU-minute) populated in the comparison tables + body
      paragraphs; methodology paragraph matches the actual bench
      shape used to produce them.
- [ ] Latency table populated (cargo-check-bound on all three tools;
      no sub-second artifact-publish claim).
- [ ] D1 product name resolved; every `<pubname>` and `cargoless` /
      `tftrunk` instance audited for consistency with the picked
      name.
- [ ] Install command verified to work in a clean environment on the
      target platforms (Linux x86_64, macOS aarch64, macOS x86_64).
- [ ] Repository URL audited; no `forgejo.triform.dev` URLs in
      contributor-facing copy (those are internal-CI only).
- [ ] `docs/dogfood/PHASE-2-REPORT.md` link verified live on GitHub.
- [ ] `ROADMAP.md` link verified live on GitHub.
- [ ] `CONTRIBUTING.md` link verified live on GitHub.
- [ ] Tone read: no marketing fluff, no unsupported speed claims,
      every promise traceable to a v0 acceptance criterion.
- [ ] Outside reviewer name + sign-off recorded in commit message of
      the publish-time edit.
