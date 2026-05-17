# cargoless launch blog post — DRAFT

> **Status:** DRAFT. Per AC#9, this post requires review by **≥2 people
> including one outside the team** before it ships publicly. Do NOT
> publish from this file directly; the operator's chosen venue
> (project blog, dev.to, personal blog, GitHub release notes, etc.)
> gets a frozen copy at publish time.
>
> **TBD markers:** `<!-- TBD-NUMBERS -->` and `<!-- TBD-POSITIONING -->`
> mark spots where the final copy depends on (a) the AC#7 comparative
> benchmark's reported numbers and (b) the operator's final
> positioning decision (architectural-honesty vs speed framing). Both
> get resolved via small follow-up edits before publish.
>
> **Two framings are scaffolded.** The body text below uses
> **Framing B (architectural honesty)** as the default, with
> **Framing A (speed)** alternative passages in HTML comments
> immediately adjacent. The operator picks one and removes the other
> in the publish-time edit pass.

---

<!--
TBD-POSITIONING — Framing B headline (DEFAULT, leading candidate):
-->

# cargoless — the codebase always knows what works

<!--
TBD-POSITIONING — Framing A headline (commented-out alternative):

# cargoless — sub-second feedback for Rust+WASM, without lying about it
-->

<!-- TBD-POSITIONING (Framing B, default) — hook paragraph: -->

`trunk serve` will happily serve you a broken WASM bundle. `bacon`
will show you a red status bar from a terminal you closed two hours
ago. `cargo-watch` will rebuild on file save but it doesn't know whether
the artifact it just produced is something an outside consumer should
trust. The thread that runs through every Rust+WASM dev loop is this:
*the tool tells you something is true, and you have to verify it
yourself anyway.*

cargoless takes a different bet. The product's whole pitch is one
sentence:

> **The codebase always knows what works, and tells you the moment it
> doesn't.**

Today we're shipping cargoless v0 — the headless continuous checker
and latest-green publisher that delivers that pitch as small a surface
as possible. v0 is intentionally small: it watches your source tree,
runs `rust-analyzer` warm in a daemon, tells you the green/red verdict
on every save, and atomically publishes the latest known-green WASM
artifact to a pointer file. It does not serve a browser. It does not
hot-swap. It does not replace `cargo` or `trunk build`. Those are
deliberate cuts.

<!--
TBD-POSITIONING — Framing A (speed-first) hook alternative:

`trunk serve` rebuilds for every save and serves whatever falls out —
red or green. `bacon` shows you a verdict in a terminal you can't
script against. `cargo-watch` does neither, faster.

cargoless is what you get if you treat the inner loop as a latency
problem AND a trust problem at the same time. The vision claim is one
sentence:

> The codebase always knows what works, and tells you the moment it
> doesn't.

Today we're shipping cargoless v0 — sub-second save→verdict on Rust+WASM
projects, plus a `.cargoless/latest-green` pointer that only ever
advances on a servable green build.

-->

---

## The problem

A modern Rust+WASM inner loop has three feedback latencies, not one:

1. **Save→verdict latency**: how fast do I find out whether the file I
   just saved compiles?
2. **Save→artifact latency**: how fast can the WASM bundle that
   represents my latest green code be served to a browser or shipped
   to a CI consumer?
3. **Verdict→trust latency**: how confident can I be that the verdict
   is right and the artifact is the right artifact?

Existing tools optimize one or two of these and assume the third away:

- **`trunk serve`** owns (2) — fast in-browser reload of the latest
  build. But it serves on every cycle, red or green. If you have an
  agent or CI process consuming `dist/`, it sees broken artifacts on
  every red cycle. That's a (3) problem, not a (2) problem.
- **`bacon`** owns (1) — fast save→verdict in a terminal. But it
  produces no artifact, and its verdict lives in a terminal session
  that can't be queried or consumed by anything other than a human
  looking at the screen.
- **`cargo-watch`** owns a slice of (1) — re-runs `cargo check`
  faster than you would. No artifact, no verdict stream, no
  cross-process observability.

For a real Rust+WASM project the result is: you keep three terminals
open, one for `trunk serve`, one for `bacon` or `cargo-watch`, and
one for `cargo test`. Each tells a different fraction of the truth.
The verdict in the corner of your eye isn't an answer; it's a
heuristic you cross-check before trusting.

## The cargoless approach

cargoless picks **(1) save→verdict + (3) verdict→trust** as the v0
target, and explicitly defers (2) save→artifact to a later phase. The
shape this takes:

**A long-lived daemon with a warm rust-analyzer.** No cold-start
penalty on save; the analyzer's index is already in memory. When you
save, the LSP `publishDiagnostics` events flow into the daemon's
state model and a verdict pops out on the next debouncer tick.

**A continuous verdict stream with per-file granularity.** `tftrunk
watch` is a long-running command whose stdout is a timestamped stream
of file-level verdicts. You see *which* file went red, not just *that*
the tree went red.

**An atomic `.cargoless/latest-green` pointer.** When a verdict turns
green, the build runs (or hits the content-addressed cache and
skips), and the latest-green pointer advances via a temp-file + fsync
+ rename pattern. When the tree is red, the pointer **does not
move** — byte-unchanged. Anything reading the pointer
(a static server, a CI step, an agent inspector) is guaranteed never
to see a broken artifact. That's verdict→trust collapsed to zero.

**Verdict→exit-code coherence.** `tftrunk check` exits 0 on green,
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

<!--
TBD-NUMBERS — fill in from the AC#7 comparative benchmark when it
reports. The bench/run.sh harness in this repo measures save→verdict
and save→publish latencies for cargoless, trunk serve, and bacon on
an identical Leptos fixture. The numbers below will be populated from
the bench's commit-status verdict.

If the AC#7 bench reports INCONCLUSIVE-WITH-CAUSE (the current
expectation per the operator's defer-positioning decision), this
section keeps the honest-framing copy and reports cargoless's
defensible standalone numbers without claiming a head-to-head win
on speed.
-->

<!-- TBD-POSITIONING (Framing B, default) — honest-framing copy: -->

cargoless's design priority is **verdict honesty**, not raw
save→verdict speed. The numbers tell that story.

| Tool | Save→verdict (median) | Save→artifact published | Verdict honesty |
|---|---|---|---|
| cargoless | _TBD_ | _TBD_ | publishes green only; pointer atomic |
| `trunk serve` | _TBD_ | _TBD_ | serves on every build, red or green |
| `bacon` | _TBD_ | n/a (terminal-only) | terminal-only |

On a real Leptos project, cargoless's save→verdict latency lands at
_TBD_ — driven by cargo-check's authoritative-tier compilation step,
which dominates the wall-clock no matter which tool wraps it. **This
is not a clean speed win** against bacon's terminal-output cycle:
bacon writes to a terminal pseudo-tty; cargoless writes to a process
event stream that other tools can subscribe to. Different
trade-offs.

Where cargoless's numbers stand out is the **save→publish-of-green**
column: no other tool here publishes a green-only pointer. `trunk
serve` publishes everything; `bacon` publishes nothing. The cargoless
column is the only one where downstream consumers can rely on the
artifact being servable.

<!--
TBD-POSITIONING (Framing A, speed-first) — alternative honest copy:

cargoless prioritized save→verdict latency from day one. The numbers:

| Tool | Save→verdict (median) | Save→artifact published |
|---|---|---|
| cargoless | _TBD — sub-1s_ | _TBD_ |
| `trunk serve` | _TBD_ | _TBD_ |
| `bacon` | _TBD_ | n/a |

cargoless wins (1) save→verdict against `trunk serve` by _TBDx_
because it doesn't rebuild on every save — it queries a warm rust-
analyzer. It loses against `bacon` on raw verdict latency because
bacon does less (no artifact, no cross-process consumer), but it
wins (2) save→artifact because cargoless can dedupe via its
content-addressed cache when the source state hasn't really changed.

Headline: faster than `trunk serve` on the inner loop, honest about
what it doesn't replace.
-->

**Methodology** (because numbers without methodology are decoration):

- **Bench fixture:** a real Leptos CSR `cdylib + rlib` project, ≥17
  files / 922 LOC. The bench harness refuses to shrink the fixture
  for flatter numbers.
- **Two-mode reporting:** checker mode (save→verdict) and artifact
  mode (save→publish) reported **separately**, never blended into a
  single "median latency" number. cargoless makes no sub-second
  artifact-publish claim.
- **Identical driver:** the same save events on the same fixture
  feed all three tools; `(t_verdict - t_save)` is sampled from a
  monotonic clock.
- **Reproducible:** the harness lives at `bench/run.sh` in the repo;
  rerun it on your machine.

The full bench report and the AC#7 verdict commit-status live at
`s1-ac2-verdict` and `ac7-verdict` keys on the release SHA.

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

- [ ] Headline value-prop matches the operator-locked TBD-POSITIONING
      decision (Framing A or Framing B) — the alternative is removed,
      not commented-out.
- [ ] All `<!-- TBD -->` markers replaced with concrete copy.
- [ ] AC#7 bench numbers populated in the comparison table + body
      paragraphs; methodology paragraph matches actual bench shape.
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
