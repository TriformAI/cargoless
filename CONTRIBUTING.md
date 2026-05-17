# Contributing

Thanks for considering a contribution. cargoless is small, the surface area
is well-defined, and the project genuinely benefits from outside eyes —
especially around UX rough edges, install-path corner cases, and
real-world Leptos/WASM project shapes the maintainers haven't tried.

## Quick start for new contributors

1. **Install the development tip** (see [`README.md`](README.md) for the
   exact command — single `cargo install` against the GitHub repo).
2. **Try it on your own project.** Run `tftrunk check` and `tftrunk
   watch` against a real Rust+WASM tree you care about; report anything
   that surprises you.
3. **Open an issue** at
   [github.com/TriformAI/cargoless/issues](https://github.com/TriformAI/cargoless/issues)
   describing what you saw. Bug, feature request, doc gap, "this command
   confused me for 10 minutes" — all welcome.
4. **For code changes**, open a PR against `main` at
   [github.com/TriformAI/cargoless](https://github.com/TriformAI/cargoless).
   See "How code changes flow into the project" below for what happens
   after that.

If you're not sure whether something is worth filing — file it. The
maintainers would rather close a duplicate than miss a real signal.

## Where to file what

- **Bug reports / feature requests:**
  [GitHub Issues](https://github.com/TriformAI/cargoless/issues)
- **Pull requests:** open against `main` at
  [github.com/TriformAI/cargoless](https://github.com/TriformAI/cargoless)
- **Open-ended discussion / questions / ideas:**
  [GitHub Discussions](https://github.com/TriformAI/cargoless/discussions)
  on the same repo

GitHub is the canonical public face of the project. The internal
integration loop runs on a Forgejo mirror; you do not need a Forgejo
account, and maintainers handle the cross-mirror plumbing.

## What makes a good bug report

cargoless's whole pitch is *trust* — that the verdict you see is the
ground truth. So when you find something that breaks that trust, the
maintainers want to fix it fast, and a precise repro shortens that loop:

- **Your environment**: OS + arch, `tftrunk --version`, `rustc --version`.
- **The project shape**: framework (Leptos, Yew, Sycamore, none), is it
  `cdylib`-only or `cdylib+rlib`, anything unusual about the workspace?
- **The exact commands you ran**, with their output.
- **What you expected vs what you got.**
- **Whether `cargo check` agrees or disagrees with cargoless's verdict.**
  (This is the most useful single piece of information for diagnosing a
  verdict bug.)

A 4-line repro on a shape we've never tried is more valuable than a
"cargoless is broken on my big private project" message — the second
one we can't act on.

## What makes a good pull request

- **One concern per PR** — even a tiny diff. Easier to review, easier to
  revert if it doesn't pan out.
- **Run `cargo fmt` before committing** (Rust 2024, MSRV 1.85). CI gates
  fmt + clippy under `-D warnings`.
- **If you're adding behaviour, add a test.** If you're changing
  behaviour, update the test that pinned the old behaviour.
- **Reference the issue in the PR description** if one exists. If you're
  fixing a class of bug we haven't filed yet, file the issue and
  cross-link.
- **Don't worry about commit-message ceremony** — squash-merging means
  the maintainers can clean up the final message.

The project's design discipline is "additive-alongside" — when adding a
new shape, prefer adding new types/functions next to the existing ones
over reshaping them. This keeps cross-crate contracts stable. See
[`docs/DESIGN.md`](docs/DESIGN.md) §4 and §6 for the contract / change
protocol if you're touching `tf-proto`.

## How code changes flow into the project

The agent team's CI gate runs on an internal Forgejo mirror with a
dedicated Kubernetes builder pod (see "Internal mechanics" below for
why). For outside contributors, the user-visible flow is the standard
GitHub one:

1. You open a PR against `main` on GitHub.
2. A maintainer reviews + applies the patch on the Forgejo mirror's
   branch space, runs `scripts/ci-gate` against the dedicated builder
   pod, and merges to Forgejo's `main` once ALL-GREEN.
3. Forgejo's push-mirror replicates the merge to GitHub within seconds,
   so your PR is marked merged on GitHub once the internal integration
   completes.

You do **not** need to interact with Forgejo, the cargoless-builder
pod, or `scripts/ci-gate` directly. If your PR needs review iterations,
the maintainers handle the loop on GitHub.

## Internal mechanics (for the agent team and the curious)

The project is built by an agent team running under a workspace policy
that **blocks local `cargo`/`rustc`**. The authoritative build/test
path is **Forgejo CI** on the internal `forgejo.triform.dev/triform/cargoless`
mirror: commit, push, and CI (`.forgejo/workflows/ci.yml`) runs
`cargo build` / `test` / `fmt --check` / `clippy` in a pinned
`rust:1.85` container, plus `scripts/ci-gate` on a dedicated
`cargoless-builder` Kubernetes pod for fast pre-integration merge
gating.

The crate ownership is disjoint (`tf-proto`, `tf-cas`, `tf-core`,
`tf-cli`, `bench/`) — see the crate table in [`README.md`](README.md).

**Workflow for internal agent-team contributors** (not required for
outside contributors):

1. Work on a branch (`agent/<role>-<topic>` or `feat/<topic>`) on the
   Forgejo mirror.
2. Keep crate ownership disjoint per the table above; cross-cutting
   changes go via `tf-proto`.
3. Commit small and push often. **Uncommitted/unpushed work is invisible
   to CI and to teammates.**
4. Self-gate via `scripts/ci-gate <branch>` (the dedicated k8s builder)
   AND wait for `.forgejo/workflows/ci.yml`; ALL-GREEN required before
   reporting branch+SHA to the lead.
5. Read CI logs via the Forgejo API / `gh` (authed to forgejo.triform.dev),
   not by running cargo locally.
6. Agent-team commits include the
   `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`
   trailer per `.claude/CLAUDE.md`.

## Code conventions

- **Rust edition:** 2024, MSRV 1.85.
- **Formatting:** `cargo fmt` canonical; CI gates `cargo fmt --check`.
- **Linting:** `cargo clippy --workspace --all-targets -- -D warnings`
  is a hard gate. Zero unused imports, zero dead code.
- **Dependencies:** no new external dep in a crate without the owner's
  sign-off. The committed `Cargo.lock` is authoritative — `--locked`
  everywhere (CI, ci-gate, future release pipeline). New deps are
  evaluated against the cold-build budget AC#1/#2 measure against.
- **Cross-crate contract:** all data crossing crate boundaries goes
  through `tf-proto` types — never element-style hardcoding. See
  [`docs/DESIGN.md`](docs/DESIGN.md) §3.
- **Unsafe:** `#![forbid(unsafe_code)]` is set on `tf-proto`; other
  crates use `unsafe` only with a written justification.

## Governance (v0)

Benevolent-maintainer model during v0: the technical lead owns
cross-cutting decisions and the `tf-proto` contract; crate owners own
their crate. Decisions of record live in Plane (internal project
"CWDL"); they are mirrored to public GitHub issues when they affect
contributor-visible behaviour.

This will evolve into a documented governance model
(AC#8 in [`ROADMAP.md`](ROADMAP.md)) before — or shortly after — the
v0 public launch.

## Post-launch responsiveness commitment

For the first **two weeks after the v0 public launch**, the
maintainers commit to:

- **Acknowledging every new issue and PR within 48 hours** (business
  days; weekends best-effort). "Acknowledge" means a real human/agent
  reply that confirms the issue is understood, not a bot triage label.
- **Resolving or routing every launch-blocker bug within 5 business
  days.** Launch-blocker = anything that materially breaks the v0
  promise (verdict honesty, never-publish-red, install-path success on
  a supported platform).
- **Triaging feature requests within one week.** Triaging = a written
  decision of "accepted for v0.x", "deferred to v1 parking lot", or
  "not in scope, here's why".

After the two-week launch window, the project moves to a sustainable
maintenance cadence: acknowledgement within one week, with the same
launch-blocker urgency for critical bugs.

The point of this commitment is to give outside contributors a
concrete signal that the project is **alive and listening**, not the
typical "thrown over the wall at launch, dies in two months" OSS
trajectory. If a maintainer misses this commitment, that itself is a
GitHub-issue-worthy event.

## Code of conduct

cargoless adopts the
[Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct)
for all project spaces (issues, PRs, discussions, any synchronous
channels). Maintainer contact for code-of-conduct concerns is via the
GitHub issue tracker or by emailing the address listed in the published
release notes once `0.1.0` ships.

## Thank you

Genuinely — every issue filed, every typo fix, every "I tried this and
it didn't work" message is a contribution to the vision claim. The
codebase only knows what works to the extent that real people exercise
it and tell us when it doesn't.
