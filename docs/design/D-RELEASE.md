# D-RELEASE — cargoless release & distribution pipeline (CWDL-71)

**Status:** DESIGN (pre-emptive, pre-AC#7-gate). Approved framing per team-lead
2026-05-17 + refreshed 2026-05-17 for §8 #8 resolution to option **(b) GitHub
mirror**. **Not ratified to fire.**

**Author:** `builder-infra` on `agent/builder-infra`. Skeleton lives at
`.github/workflows/release.yml.draft` (canonical release pipeline) and a
`[package.metadata.binstall]` stub lives in `crates/tf-cli/Cargo.toml` — both
are inert until ratified.

**Phase:** Phase 3 of the production-hardening plan (the "launch readiness"
phase; gated on AC#7 PASS via #36 and Phase 2 clean via #37).

---

## 0. Read this first — naming discipline

This project uses the token **"v0.1"** in two completely different vocabularies.
Conflating them produces broken release plans.

| Token | Means | Where used |
|---|---|---|
| **Internal phase `v0.1`** | The server/browser-reload adapter epic (CWDL-40–45, parked `devserver`). Layers on top of the v0 headless checker. | `CLAUDE.md`, `EXECUTION.md`, ROADMAP. NEVER a semver. |
| **Release semver `0.1.0`** | The **first** crates.io / cargo-binstall release of cargoless. Ships the v0 headless surface. | `Cargo.toml` `version`, git tag `v0.1.0`, this document. |

The first **shipping** release is semver **`0.1.0`** (Rust ecosystem norm for a
first public release — pre-1.0 = pre-API-stability, post-0.0.x = no longer
pre-release). The internal phase named "v0.1" (server/browser-reload) ships
under a *later* semver (probably `0.2.0`, exact number TBD when devserver
lands). **Never** put the string `v0.1` into `release.yml` or `Cargo.toml`'s
`version` field as a semver value — that would tie a release tag to an internal
phase name and they will diverge.

This document writes "release `0.1.0`" everywhere it means the semver and
"phase v0.1" everywhere it means the internal epic.

---

## 1. Purpose

This is the design for what happens between "AC#7 GATE passes" and "a Rust
developer types `cargo install cargoless` (final name TBD per D1/CWDL-12) and
gets a working binary." It covers:

1. The git-tag → CI trigger surface.
2. The artifact matrix (which platforms get prebuilts).
3. The two install paths offered to users (source via `cargo install`,
   prebuilt via `cargo binstall`) and the honest scope for each.
4. The crates.io publish topology, ordering, and token handling.
5. The release-pipeline job-granularity — mirrored from
   `.forgejo/workflows/ci.yml`'s "one-job-per-check = observability" discipline
   even on GitHub Actions (where the discipline is less *necessary* — GH
   Actions exposes per-job logs cleanly — but is *consistent* with the rest of
   the project's CI shape).
6. Open decisions and what must be resolved before the skeleton can fire for
   real.

This document is the spec; the workflow draft is the
mechanical-readable form; the `[package.metadata.binstall]` stub is the
in-`Cargo.toml` surface that needs to match the draft.

### 1.1 Where things run after §8 #8 resolves to (b) GitHub mirror

| Concern | Runner | File |
|---|---|---|
| Per-branch CI gate (PRs, ci-gate, S1/AC#2 bench, etc.) | **Forgejo** | `.forgejo/workflows/ci.yml` (UNCHANGED) + `scripts/ci-gate` (dedicated k8s builder pod) |
| Release pipeline (tag → prebuilts → release-page assets) | **GitHub Actions** | `.github/workflows/release.yml` (NEW — currently `.draft`) |
| Source of truth | **GitHub (canonical) + Forgejo (mirror)** | TBD-ORG/cargoless on github.com; push-mirrored from forgejo.triform.dev/triform/cargoless |

The PR/integration loop stays on Forgejo because that's where the dedicated
`cargoless-builder` pod + ci-gate live (kubectl-readable logs, warm PVC
cache, isolated runner). The release pipeline moves to GitHub because (a) GH
Actions provides free-tier macOS runners that solve §8 #2's Mac-builder
question structurally, (b) GitHub releases are the canonical public artifact
surface the broader Rust ecosystem expects, (c) `cargo install --git
https://github.com/...` and `cargo binstall` URL templates land at
discoverable, conventional locations.

---

## 2. Triggers — when does a release fire

**Only on a semver tag push matching `v[0-9]+.*`.** Not on `main` merges, not
on manual UI dispatch, not on a release branch. One tag → one release. The
discipline mirrors `.forgejo/workflows/ci.yml`'s `on: push:` minimalism (it
takes the whole space; release takes only tagged points).

The tag is pushed to BOTH remotes (operator runs `git push origin v0.1.0 &&
git push github v0.1.0`, or the Forgejo push-mirror handles GitHub
automatically — see §8 #9 for the mirror-direction operator decision). The
`.github/workflows/release.yml` triggers on GitHub's receipt of the tag.

Why tag-only:

- **Decouples release cadence from integration cadence.** `main` advances per
  merge; releases advance per operator-cut tag. The two should never be
  conflated.
- **A tag is an explicit human commitment.** The operator decides "this SHA is
  shippable" by running `git tag v0.1.0 <sha> && git push --tags`. Compare to
  main-merge auto-release: any merge bug → bad release; no human-in-the-loop.
- **Reproducible by ref.** A consumer can `git checkout v0.1.0` and the
  bit-for-bit-identical tree built the artifact (provenance lives in the tag).

Tag pattern `v[0-9]+.*`:

- Accepts `v0.1.0`, `v0.1.1`, `v1.0.0`, `v1.0.0-rc.1` etc. (any version,
  including pre-releases).
- Rejects `v0.1` (incomplete semver) and `latest`/`stable` (mutable refs are
  not allowed as release triggers).
- `tag-validate` job (§5) asserts the tag's version exactly matches every
  publishable crate's `Cargo.toml` `version` — drift between tag and Cargo.toml
  is a hard fail, not silent fix.

---

## 3. Targets — the honest install matrix

The §8 #8 (b) resolution unblocks the macOS prebuilt path: GitHub Actions
provides free-tier `macos-14` (Apple Silicon, aarch64) and `macos-13` (Intel,
x86_64) runners. No cross-compile, no Apple SDK gymnastics in a Linux
container — each macOS binary is built on a native macOS runner. Linux
x86_64-gnu is still built either on the cargoless-builder pod (for ci-gate)
or `ubuntu-latest` (for GH Actions release matrix).

### 3.1 Universal source install (every Rust platform)

```
cargo install --git https://github.com/TBD-ORG/cargoless --tag v0.1.0
```

**Or, once published to crates.io:**

```
cargo install <pubname>           # <pubname> = TBD per D1/CWDL-12
```

Works on **every platform with rustc** — Linux x86_64/aarch64, macOS
Intel/ARM, even Windows for the brave (Windows is v1 parking-lot for
*official* support but `cargo install` works there too on a best-effort
basis). This is the **headline install path** in the README. Slow
(local compile of the dep graph) but universal.

> **Note on the URL.** `TBD-ORG` is a placeholder until the operator creates
> the GitHub org/repo (one clarifying question in flight at time of writing).
> Lock the actual org/repo into `[workspace.package].repository` in the root
> `Cargo.toml` once it lands; the `{repo}` template in `[package.metadata.
> binstall]` resolves from that one field. Forgejo
> (`forgejo.triform.dev/triform/cargoless`) remains the integration-CI side
> but is NOT the canonical user-facing URL after §8 #8 resolution.

### 3.2 Prebuilts via `cargo binstall` — three targets at first release

```
cargo binstall <pubname>
```

`cargo-binstall` reads `[package.metadata.binstall]` from the `tf-cli`
crate, fetches the matching `.tgz` from the GitHub release-asset URL, and
extracts the binary. First release ships **three** prebuilts:

| Target | Built on (GH Actions runner) | Tarball |
|---|---|---|
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` | `<pubname>-v0.1.0-x86_64-unknown-linux-gnu.tgz` |
| `aarch64-apple-darwin` | `macos-14` (Apple Silicon free-tier) | `<pubname>-v0.1.0-aarch64-apple-darwin.tgz` |
| `x86_64-apple-darwin` | `macos-13` (Intel free-tier) | `<pubname>-v0.1.0-x86_64-apple-darwin.tgz` |

All three published as assets on the same GitHub release page. SHA-256 sums
attached alongside each tarball. `cargo binstall` on any of the three target
triples gets a matching prebuilt; on unsupported targets (Linux aarch64,
Windows, etc.) it gracefully falls back to `cargo install`.

### 3.3 Targets table

| Target | First release (`0.1.0`) | Path | Notes |
|---|---|---|---|
| `x86_64-unknown-linux-gnu` | **YES — prebuilt + source** | binstall + cargo install | GH Actions `ubuntu-latest`. ci-gate continues to build it on the cargoless-builder pod for PR-gate. |
| `aarch64-apple-darwin` | **YES — prebuilt + source** | binstall + cargo install | GH Actions `macos-14` (Apple Silicon free-tier). Resolved by §8 #8 (b). |
| `x86_64-apple-darwin` | **YES — prebuilt + source** | binstall + cargo install | GH Actions `macos-13` (Intel free-tier). Resolved by §8 #8 (b). |
| `aarch64-unknown-linux-gnu` | **source only** | cargo install | Cross from x86_64-linux is solvable in v0.2 (musl + qemu) or trivially in GH Actions matrix (deferred to avoid scope creep on first release). |
| `x86_64-pc-windows-msvc` | **source only, unsupported** | cargo install | Windows is v1 parking-lot per CLAUDE.md non-goals. |

---

## 4. Crate publish topology

Verified via `cargo metadata --no-deps --format-version 1` in the
cargoless-builder pod on `origin/main @ 3cfc835`. Result:

```
tf-proto deps: []
tf-cas   deps: ['tf-proto']
tf-core  deps: ['notify', 'serde_json', 'tf-cas', 'tf-proto']
tf-cli   deps: ['tf-core']
```

**Publish order (topological, must be sequential — each crate requires its
deps already on crates.io):**

```
1. tf-proto   (no internal deps)
2. tf-cas     (needs tf-proto)
3. tf-core    (needs tf-proto + tf-cas; external deps notify, serde_json already published)
4. tf-cli     (needs tf-core)
```

`bench/{harness,fixture}` are **standalone non-workspace crates** with
`publish = false` baked in — they cannot accidentally be published. (Verified
in `bench/harness/Cargo.toml` and `bench/fixture/Cargo.toml`.) The `bench/`
directory exists in releases (via the tagged tree) but no `crates.io` artifact
is produced for it.

### 4.1 Coordinated version — `[workspace.package].version`

**Current state:** every crate has `version = "0.0.0"` independently. There is
no `version` in `[workspace.package]`. This works fine while the four crates
are path-deps inside one workspace, but is the wrong shape for release —
publish requires version-pinning per dep, and four sources of truth for "what
version is this release" will drift.

**Required change before first release (one focused commit, no behavior
change):**

```toml
# Cargo.toml (root, [workspace.package])
version = "0.1.0"   # bumped from missing → 0.1.0 at first release tag
```

And in each `crates/*/Cargo.toml`:

```toml
[package]
version.workspace = true   # was: version = "0.0.0"
```

After this, **one** number in **one** place gates the whole release. The
`tag-validate` job asserts `tag == v${workspace.package.version}` and refuses
to proceed on mismatch.

**Path-dep cross-references** (`tf-cas = { path = "../tf-cas" }`) need
versions too for `cargo publish` to accept them: change to
`tf-cas = { path = "../tf-cas", version = "0.1.0" }`. This is the standard
publish-ready workspace pattern. Same `version.workspace = true` trick can
inherit; details captured in the bump commit.

### 4.2 crates.io ownership & names

The four crate names (`tf-proto`, `tf-cas`, `tf-core`, `tf-cli`) are the
**internal** names. They are *not* automatically reserved on crates.io and may
collide with prior-art crates. The D1 product-name decision (CWDL-12) is the
moment to either:

- (a) keep the `tf-*` internal names AND prefix-rename them to namespace under
  the picked public name (e.g., `<pubname>-proto`, `<pubname>-cas`, etc.), OR
- (b) keep `tf-*` as internal-only and publish ONLY the user-facing CLI
  crate (`tf-cli`, renamed) — leaving `tf-{proto,cas,core}` as private
  workspace deps via `publish = false`.

**Open decision** routed to the lead in §8.

### 4.3 Publish token & runner

**For the first release: operator-run from their laptop.** Don't inject
`CARGO_REGISTRY_TOKEN` into either the Forgejo Actions runner OR the GitHub
Actions runner OR the cargoless-builder pod. The operator has a crates.io
account; the first release is the moment to verify the human-in-the-loop
publish step works. Documented as a manual step in §6.

Automating crates.io publish via GitHub-Actions-secret (or Forgejo-Actions-
secret if mirroring publishes too) can land in **release `0.2.0`** when the
workflow is proven and the rotation strategy is designed. Until then, the
`publish-*` jobs in the GH Actions release workflow are **declarative
documentation** of the order — they are marked `if: false` and the operator
runs the equivalent `cargo publish -p <crate> --locked` locally in topological
order. (The releases that the operator builds locally are still
bit-reproducible from the tag; the binary artifact prebuilds happen in GH
Actions CI.)

---

## 5. Job topology — granularity-as-observability

The release pipeline lives at `.github/workflows/release.yml` on GitHub
Actions. Forgejo CI (`.forgejo/workflows/ci.yml`) continues to do PR-gate
and S1 bench work but is **NOT** the release-pipeline owner after §8 #8 (b).

GH Actions exposes per-job logs cleanly over its REST/GraphQL API, so the
"granularity-as-observability-because-Forgejo-404s-on-logs" rationale doesn't
apply here. But the **same shape** — one job per check, clear failure
attribution — is kept for project-wide consistency. Future maintainers see a
familiar pattern.

```
.github/workflows/release.yml jobs (sequential = needs:, parallel = matrix):

  tag-validate                                      ── always first, ubuntu-latest
    └─ asserts tag matches v$VERSION exactly
    └─ asserts every crate's Cargo.toml version matches $VERSION
    └─ asserts CHANGELOG.md has a heading for $VERSION
    └─ asserts the SHA being tagged has a green ci.yml + s1-ac2-verdict
       commit status (queried from forgejo.triform.dev — the bench/PR-CI
       side of truth still lives on Forgejo)

  build (matrix)                                     ── needs: tag-validate
    ├─ target=x86_64-unknown-linux-gnu, runner=ubuntu-latest
    ├─ target=aarch64-apple-darwin,     runner=macos-14
    └─ target=x86_64-apple-darwin,      runner=macos-13
      └─ each: cargo build -p tf-cli --release --locked --target $TARGET
      └─ each: strip + tar.gz, SHA-256 sum
      └─ each: upload-artifact (intra-CI hand-off to attach-release-assets)

  attach-release-assets                              ── needs: build
    └─ ubuntu-latest
    └─ download all three build artifacts
    └─ POST each tarball + SHA-256 to the GitHub release at tag $TAG via
       `gh release upload` (or softprops/action-gh-release@v2)
    └─ release notes pulled from CHANGELOG.md section for $VERSION

  # publish-tf-proto       ── if: false (operator-run from laptop, 0.1.0)
  # publish-tf-cas         ── if: false; needs: publish-tf-proto
  # publish-tf-core        ── if: false; needs: publish-tf-cas
  # publish-tf-cli         ── if: false; needs: publish-tf-core
  # When automation lands in 0.2.0+: cargo publish -p <crate> --locked,
  # gated on CARGO_REGISTRY_TOKEN secret, sequential.
```

**Job granularity properties this gives us:**

- `tag-validate` red ⇒ tag/Cargo.toml/CHANGELOG drift, **no artifact built**.
- `build (matrix)` red on any single triple ⇒ that platform's release-profile
  compile regression is isolated; the other two targets still finish, and
  their artifacts are visible in the workflow run for triage. The whole
  release is held back (matrix-failure is hard-fail by default).
- `attach-release-assets` red ⇒ GitHub release-asset API drift or auth
  problem; artifacts built and in workflow artifact store but not yet on the
  release page; safe to re-run after fix.
- Each publish-* job red ⇒ specific crate's `cargo publish` failed; can rerun
  from there without rebuilding earlier crates.

**Anti-pattern explicitly avoided:** a single "release" megajob that compiles
all three targets + uploads + publishes. Failure attribution becomes
impossible.

**Forgejo side (PR-gate, unchanged):** `.forgejo/workflows/ci.yml` continues
to run build/test/fmt/clippy/bench per branch + main; `scripts/ci-gate`
continues to be the fast pre-integration merge gate. The release pipeline
move is additive, not displacing.

---

## 6. The operator playbook (first real fire of `0.1.0`)

This is the human-side complement to the workflow. Captured here so it ships
with the design — not hidden in the operator's head.

**Pre-flight:**
1. AC#7 GATE (#36) PASSED. S1_VERDICT on the release SHA = PASS / D-A2=GO.
2. Phase 2 dogfood (#37) clean — at least one human session per platform
   (Linux x86_64-gnu, macOS aarch64-darwin, macOS x86_64-darwin).
3. D1/CWDL-12 has been resolved → public name picked → all crates renamed
   accordingly (one focused commit, ci-gated green).
4. crates.io name(s) reserved by the operator (placeholder `cargo publish
   --dry-run` to confirm).
5. `[workspace.package].version` bumped from missing → `0.1.0`; all crates
   inherit (§4.1).
6. `[workspace.package].repository` set to `https://github.com/<ORG>/cargoless`
   (the real org/repo, no longer `TBD-ORG`).
7. `CHANGELOG.md` has a `## 0.1.0` heading committed.
8. GitHub repo created; Forgejo→GitHub push-mirror configured (or operator
   manually pushes); first throwaway test tag `v0.0.1-rc.1` fired to verify
   GH Actions matrix + release-asset URL shape end-to-end.

**Cut the tag:**
```bash
git tag -a v0.1.0 <ratified-sha> -m "cargoless 0.1.0 — first public release"
git push origin v0.1.0        # forgejo
git push github v0.1.0        # github (or auto via push-mirror)
```

**CI runs (.github/workflows/release.yml, automatic on GitHub receipt of tag):**
- `tag-validate` PASS
- `build (x86_64-unknown-linux-gnu)` produces `<pubname>-v0.1.0-x86_64-unknown-linux-gnu.tgz`
- `build (aarch64-apple-darwin)` produces `<pubname>-v0.1.0-aarch64-apple-darwin.tgz`
- `build (x86_64-apple-darwin)` produces `<pubname>-v0.1.0-x86_64-apple-darwin.tgz`
- `attach-release-assets` posts all three tarballs + SHA-256 sums to the
  GitHub release page

**Operator publishes to crates.io (manual, from laptop, for 0.1.0):**
```bash
git checkout v0.1.0       # work from the tagged SHA — never from HEAD
cargo publish -p tf-proto --locked
cargo publish -p tf-cas   --locked   # wait until tf-proto is on crates.io (~30s)
cargo publish -p tf-core  --locked   # wait until tf-cas is up
cargo publish -p tf-cli   --locked   # wait until tf-core is up
```

(Or whatever the renamed crates are post-D1.) The `--locked` flag mirrors
ci.yml + ci-gate discipline: deterministic dep resolution, no silent re-solve.

**Post-flight:**
- `cargo install <pubname>` from a clean machine resolves the just-published
  crate (universal source-install path).
- `cargo binstall <pubname>` on Linux x86_64, macOS Apple Silicon, AND macOS
  Intel each fetch the matching prebuilt and unpack to a working binary in <5s.
- `cargo binstall <pubname>` on unsupported targets (Linux aarch64, Windows,
  etc.) falls through to `cargo install` (graceful degradation).
- Release announced (CWDL-9 / launch blog gates here).

---

## 7. The cargo-binstall metadata stub

`crates/tf-cli/Cargo.toml` carries a `[package.metadata.binstall]` table.
Cargo treats `[package.metadata.*]` as opaque (it does NOT validate the
contents), so this stub lands without touching any existing gate. Only
`cargo-binstall` itself ever reads it.

```toml
[package.metadata.binstall]
# GitHub release-asset URL template. Variables resolved by cargo-binstall:
#   {repo}            — package.repository (workspace-inherited, will resolve
#                       to https://github.com/<ORG>/cargoless after §8 #8 (b))
#   {name}            — package.name (the published crate name)
#   {version}         — package.version (no leading "v")
#   {target}          — target triple, e.g. x86_64-unknown-linux-gnu
#   {archive-suffix}  — derived from pkg-fmt, e.g. ".tgz"
pkg-url = "{repo}/releases/download/v{version}/{name}-v{version}-{target}{archive-suffix}"
pkg-fmt = "tgz"

# Inside the tarball, the binary lives at:
#   {name}-v{version}-{target}/tftrunk
# where `tftrunk` is the CURRENT placeholder binary name (D1/CWDL-12 may rename).
bin-dir = "{name}-v{version}-{target}/tftrunk"

# Per-target overrides (defaults above apply to x86_64-unknown-linux-gnu and
# both Apple targets — same tarball layout, same pkg-url template). No
# per-target overrides needed for 0.1.0 since all three triples follow the
# same shape. If a target ever needs a different layout (e.g., a Windows
# .zip post-v1), add:
#   [package.metadata.binstall.overrides."x86_64-pc-windows-msvc"]
#   pkg-fmt = "zip"
#   bin-dir = "{name}-v{version}-{target}/tftrunk.exe"
```

**No real fire required for the stub to be useful** — once committed, anyone
running `cargo binstall <crate>` after the first crates.io publish gets a
prebuilt on Linux x86_64 + both macOS targets, falls back to `cargo install`
otherwise.

---

## 8. Open decisions blocking real-fire

This skeleton CANNOT cut release `0.1.0` until ALL of these resolve. Each is
called out, not invented.

| # | Open question | Owner | Blocks |
|---|---|---|---|
| 1 | **D1/CWDL-12 — public product name.** Affects: crate names on crates.io, the binary name `tftrunk`, the binstall `bin-dir` and `pkg-url` template, the README install commands, the launch blog title. | lead | EVERYTHING. No release can fire under "cargoless" because that is the placeholder/repo identifier, not a public name commitment. |
| 2 | ~~**Mac builder strategy.**~~ **CLOSED 2026-05-17 by §8 #8 (b)**. GitHub Actions free-tier provides `macos-14` (Apple Silicon, aarch64) and `macos-13` (Intel, x86_64) runners. Both targets ship as prebuilts in release `0.1.0`. See §3.3 and §5. No further action required. | — (resolved) | — |
| 3 | **crates.io crate-name resolution.** Are `tf-proto` / `tf-cas` / `tf-core` / `tf-cli` going to be the published names (likely collisions), or are they internal-only with a renamed top-level crate (option 4.2.b), or all renamed under a `<pubname>-*` namespace (option 4.2.a)? | operator + lead, post-D1 | `tag-validate` and `publish-*` job names; the `bin-dir` template. |
| 4 | **CHANGELOG.md format & seeding.** Is the project keepachangelog.com style? semantic-release-style? hand-rolled? `tag-validate` asserts a heading for $VERSION exists; the format determines the assertion regex. | docs / lead | `tag-validate` job; can be a one-line change. |
| 5 | **crates.io token automation timeline.** Operator-run for `0.1.0` is approved. Is `0.2.0`-automatable, or is human-in-the-loop the permanent model? | operator | Whether `publish-*` jobs ever lose `if: false`. |
| 6 | **GitHub release-asset URL shape.** The `pkg-url` template in §7 assumes GitHub's canonical `/{owner}/{repo}/releases/download/v{version}/{file}` shape — verified shape; a one-time throwaway test fire on `v0.0.1-rc.1` against the actual GitHub release should still confirm end-to-end before locking the production tag. | operator (one-time) | binstall first-fire correctness. |
| 7 | **GPG signing of release tags + tarball signatures.** v1 parking-lot per CLAUDE.md non-goals. Recorded here for the record — when someone asks "why no .asc files?" the answer is "deliberate 0.1.0 scope decision; see D-RELEASE §9." | lead | Nothing in v0.1.0; trust model for downstream packagers. |
| 8 | ~~**Canonical install URL / public-source-access strategy.**~~ **CLOSED 2026-05-17 — operator picked (b) GitHub mirror.** Canonical public URL becomes `https://github.com/<ORG>/cargoless` (org name TBD, operator question in flight). Per-repo Forgejo flip from earlier in the day (`private: false`) stays in place but is not load-bearing — the Forgejo instance's site-wide `[service].REQUIRE_SIGNIN_VIEW = true` setting overrides per-repo visibility; outside users access the project via GitHub. Forgejo remains the integration-CI side (cargoless-builder pod, ci-gate, S1 bench). Compound benefit realized: §8 #2 (Mac builder) ALSO closes via GH Actions free-tier macOS runners. See §1.1 for the runner-split summary. Final URL locks into `[workspace.package].repository` when the org name lands. | — (resolved; one URL-confirmation in flight) | URL value in Cargo.toml + README; flips on URL confirmation. |
| 9 | **Forgejo → GitHub mirror direction.** Does the operator (a) configure a Forgejo push-mirror (Forgejo auto-pushes to GitHub on each push to main + tags) or (b) push to both remotes manually from their machine? (a) is more automation but ties Forgejo's outbound credentials to a GitHub PAT held server-side; (b) is more explicit but requires discipline never to push to one without the other. | operator | Branch/tag synchronization mechanics. Doesn't block design; affects §6 operator playbook precise commands. |

---

## 9. Non-goals for release `0.1.0`

All per CLAUDE.md v1 parking lot. **None** of these belong in the release
workflow or any prebuilt artifact.

- **Windows official support.** `cargo install` works there on best-effort; no
  prebuilt, no `[overrides."x86_64-pc-windows-msvc"]` in binstall metadata.
- **Code-signing / notarization.** No Apple Developer ID, no Authenticode, no
  GPG-signed tags. Trust is "operator's crates.io account + tagged SHA on
  GitHub (signed-by-GitHub TLS to crates.io)."
- **Homebrew formula / scoop manifest / winget / AUR / nix flake.** Third
  parties may package; project ships source + crates.io + Linux/macOS binstall
  prebuilts only.
- **Container images.** No `Dockerfile`, no `docker.io/cargoless`. The product
  is a local dev tool; containerizing it makes no sense.
- **deb / rpm / msi / pkg.** Same reasoning — `cargo install` is the canonical
  Rust ecosystem path.
- **Auto-update.** The binary does not check for updates. Users run `cargo
  install --force` or `cargo binstall --force` when they want a new version.

If any of these become user-asks post-launch, they are **v0.2.0+** scope
discussions, not silent re-additions to `release.yml`.

---

## 10. Path to first real-fire — checklist

```
[ ] Lead/operator picks D1 (CWDL-12) — public name locked.
[ ] Rename tf-cli binary if D1 != "tftrunk"; rename crates per §4.2 decision.
[ ] crates.io name(s) reserved by operator (cargo publish --dry-run).
[ ] Hoist version → [workspace.package].version = "0.1.0"; crates inherit.
[ ] Update path-deps to include version (publish-ready); Cargo.lock regenerated.
[ ] CHANGELOG.md format chosen; ## 0.1.0 section seeded.
[ ] §8 #8 GitHub URL confirmed; operator creates github.com/<ORG>/cargoless;
    [workspace.package].repository updated from forgejo to the GitHub URL;
    README/CONTRIBUTING URLs updated in the same commit.
[ ] §8 #9 mirror direction decided (push-mirror server-side vs manual dual-push);
    if push-mirror, configured in Forgejo with appropriate GitHub PAT.
[ ] release.yml.draft → release.yml at `.github/workflows/release.yml`; remove
    `if: false` from build/validate jobs (keep on publish-* until 0.2.0 token-
    automation lands).
[ ] One throwaway test fire on `v0.0.1-rc.1` against the real GitHub release
    page to verify §8 #6 asset-URL shape + GH Actions macOS matrix end-to-end.
[ ] AC#7 GATE (#36) PASSED on the release SHA.
[ ] Phase 2 dogfood (#37) signed off on all three target platforms.
[ ] git tag -a v0.1.0 <sha>; push to both forgejo + github (or rely on mirror).
[ ] Operator runs `cargo publish` topology from laptop (§6).
[ ] `cargo install` and `cargo binstall` smoke-test from clean machines on
    all three prebuilt targets + at least one source-only target.
[ ] CWDL-9 launch blog reviewed by ≥2 (incl. outside reviewer); announce.
```

When the checklist is clean and team-lead ratifies, **then** the skeleton fires.
Not before.

---

## Appendix A — Why GitHub mirror (not Forgejo-only)

§8 #8 was resolved 2026-05-17 in favor of option (b) GitHub mirror, after
discovering that `forgejo.triform.dev` is site-wide auth-walled (Gitea
`[service].REQUIRE_SIGNIN_VIEW = true` in `app.ini`) — a per-repo public
flip is structurally insufficient. The site-wide setting was *intentional*
across the broader triform Forgejo instance (multiple tenants); changing it
would affect every other repo there, an unacceptable blast radius for one
project's OSS-pitch unblock.

GitHub mirror was chosen over the alternatives because:

1. **It unblocks anonymous read** at the URL the Rust ecosystem already
   expects (`github.com/...` is the de facto canonical OSS-Rust source URL,
   not Forgejo).
2. **It compound-resolves §8 #2** (Mac builder) via GH Actions free-tier
   macOS runners — eliminating an entire reserved operator decision.
3. **It provides discoverability.** GitHub's search, topic tags, and the
   trending-Rust-projects surface are where the broader community looks for
   new tools. Forgejo's `forgejo.triform.dev` doesn't index there.
4. **It keeps integration-CI on Forgejo.** The PR-gate work, the dedicated
   cargoless-builder pod with its warm PVC + kubectl-readable logs, the
   ci-gate script, and the S1/AC#2 bench commit-status mechanism all
   continue to live on Forgejo unchanged. The release pipeline move is
   additive — GitHub becomes the *public* face; Forgejo remains the
   *developer-loop* face.

The Forgejo per-repo flip (private → public) from earlier in the day stays
in place. It is harmless (the site-wide wall still walls it) and keeps the
Forgejo state consistent with "this is an OSS project" semantics if/when the
site-wide setting ever changes.

## Appendix B — Why `--locked` everywhere

Mirrors the discipline `e37050a` (`ci(ci-gate): --locked to mirror ci.yml
post-#25`) brought to ci.yml + ci-gate after the workspace gained real
external deps. A release that re-resolves the dep graph differently from CI is
a release that ships untested code. `--locked` makes the committed `Cargo.lock`
the input identity, not a hint. Already part of `tf_proto::BuildIdentity`
(`cargo_lock` field) by design — release just inherits the same axiom.
