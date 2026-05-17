# D-RELEASE — cargoless release & distribution pipeline (CWDL-71)

**Status:** DESIGN (pre-emptive, pre-AC#7-gate). Approved framing per team-lead
2026-05-17 with adjustments folded in. **Not ratified to fire.**

**Author:** `builder-infra` on `agent/builder-infra`. Skeleton lives at
`.forgejo/workflows/release.yml.draft` and a `[package.metadata.binstall]` stub
lives in `crates/tf-cli/Cargo.toml` — both are inert until ratified.

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
5. The Forgejo workflow job-granularity, mirroring the
   "one-job-per-check = observability" discipline `.forgejo/workflows/ci.yml`
   established (Forgejo's REST 404s on job logs; failing-job-name *is* the
   diagnosis).
6. Open decisions and what must be resolved before the skeleton can fire for
   real.

This document is the spec; the workflow draft is the
mechanical-readable form; the `[package.metadata.binstall]` stub is the
in-`Cargo.toml` surface that needs to match the draft.

---

## 2. Triggers — when does a release fire

**Only on a semver tag push matching `v[0-9]+.*`.** Not on `main` merges, not
on manual UI dispatch, not on a release branch. One tag → one release. The
discipline mirrors `.forgejo/workflows/ci.yml`'s `on: push:` minimalism (it
takes the whole space; release takes only tagged points).

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

The cluster builder pod is `x86_64-unknown-linux-gnu` only and has no Darwin
SDK / cross-toolchain. Cross-compiling `aarch64-apple-darwin` from there is
**not solvable in scope** — Apple's `cctools` + macOS SDK + codesigning chain
cannot honestly be reproduced in a Linux pod without committing to a Mac runner
strategy. The first release ships an honest matrix that reflects that.

### 3.1 Universal source install (every Rust platform)

```
cargo install --git https://forgejo.triform.dev/triform/cargoless --tag v0.1.0
```

> ⚠ **The above URL is broken for outside users today.** dogfood-lead
> confirmed (2026-05-17, direct git+curl probes from the cluster pod) that
> `forgejo.triform.dev` is **auth-walled on every git protocol endpoint**
> (HTTP 401 + `www-authenticate: Basic Gitea` on anonymous git fetch). An
> anonymous `cargo install --git https://forgejo.triform.dev/...` fails at
> first byte. **This is a hard launch-blocker** for the OSS install pitch
> and is recorded as §8 #8. The URL template stays templated here; the
> *canonical* install URL is locked when the operator picks (a) flip
> Forgejo public, (b) mirror to github.com, or (c) document auth.

**Or, once published to crates.io:**

```
cargo install <pubname>           # <pubname> = TBD per D1/CWDL-12
```

The crates.io path is unaffected by §8 #8 (crates.io vends the published
tarball, not a git fetch from forgejo.triform.dev) — but it requires the
tagged release to already be published, which still needs §8 #8 resolved
because the operator must be able to consult the public source at the tag.

Works on **every platform with rustc** — Linux x86_64/aarch64, macOS
Intel/ARM, even Windows for the brave (Windows is v1 parking-lot for
*official* support but `cargo install` works there too on a best-effort
basis). This is the **headline install path** in the README. Slow
(local compile of the dep graph) but universal.

### 3.2 Prebuilt via `cargo binstall` — Linux x86_64 ONLY (first release)

```
cargo binstall <pubname>
```

`cargo-binstall` reads `[package.metadata.binstall]` from the `tf-cli`
crate, fetches the matching `.tgz` from the Forgejo release-asset URL, and
extracts the binary. **First release ships exactly one prebuilt: Linux
x86_64-gnu.**

**macOS prebuilts: DEFERRED.** Not because users don't want them — they do
— but because the **operator-decision** of "where Mac builds run" is real
and unsolved (see §8). Honest deferral beats inventing a strategy.
`cargo-binstall` on macOS will see no matching asset and fall back to
`cargo install` (binstall's documented graceful degradation).

### 3.3 Targets table

| Target | First release (`0.1.0`) | Path | Notes |
|---|---|---|---|
| `x86_64-unknown-linux-gnu` | **YES — prebuilt + source** | binstall + cargo install | Built in cargoless-builder pod (the same one that runs ci-gate). |
| `aarch64-apple-darwin` | **source only** | cargo install | Operator decision blocks prebuilt; see §8. |
| `x86_64-apple-darwin` | **source only** | cargo install | Same. |
| `aarch64-unknown-linux-gnu` | **source only** | cargo install | Cross from x86_64-linux is solvable in v0.2 (musl + qemu); deferred. |
| `x86_64-pc-windows-msvc` | **source only, unsupported** | cargo install | Windows is v1 parking-lot per CLAUDE.md. |

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
`CARGO_REGISTRY_TOKEN` into the Forgejo Actions runner or the cargoless
builder pod. The operator has a crates.io account; the first release is the
moment to verify the human-in-the-loop publish step works. Documented as a
manual step in §6.

Automating crates.io publish via Forgejo-Actions-secret can land in
**release `0.2.0`** when the workflow is proven and the rotation strategy is
designed. Until then, the `publish-*` jobs in the workflow are **declarative
documentation** of the order — they are marked `if: false` and the operator
runs the equivalent `cargo publish -p <crate> --locked` locally in topological
order. (The releases that the operator builds locally are still
bit-reproducible from the tag; the binary artifact prebuilds happen in CI.)

---

## 5. Job topology — granularity-as-observability

Following the same discipline as `.forgejo/workflows/ci.yml` (split into one
job per check because Forgejo's REST API 404s on per-job logs; the failing job
NAME is the diagnosis):

```
release.yml jobs (sequential = needs:, parallel = nothing):

  tag-validate              ── always first
    └─ asserts tag matches v$VERSION exactly
    └─ asserts every crate's Cargo.toml version matches $VERSION (workspace inheritance makes this one read)
    └─ asserts CHANGELOG.md has a heading for $VERSION
    └─ asserts the SHA being tagged has a green ci.yml + s1-ac2-verdict commit status

  build-linux               ── needs: tag-validate
    └─ cargo build -p tf-cli --release --locked --target x86_64-unknown-linux-gnu
    └─ strip + tar.gz the binary, name = <pubname>-v$VERSION-x86_64-unknown-linux-gnu.tgz
    └─ artifact upload

  # build-macos             ── DROPPED for 0.1.0 per lead 2026-05-17. See §8.
  # TODO(operator-decision): Mac builder strategy. Re-add when resolved.

  attach-release-assets     ── needs: build-linux
    └─ POST the tarball to Forgejo release at tag $TAG via /api/v1/repos/.../releases/.../assets
    └─ POST the SHA-256 sum as a separate asset
    └─ Forgejo release notes pull from CHANGELOG.md section for $VERSION

  # publish-tf-proto        ── if: false (operator-run from laptop, 0.1.0)
  # publish-tf-cas          ── if: false; needs: publish-tf-proto
  # publish-tf-core         ── if: false; needs: publish-tf-cas
  # publish-tf-cli          ── if: false; needs: publish-tf-core
  # When automation lands in 0.2.0+: cargo publish -p <crate> --locked,
  # gated on CARGO_REGISTRY_TOKEN secret, sequential.
```

**Job granularity properties this gives us:**

- `tag-validate` red ⇒ tag/Cargo.toml/CHANGELOG drift, **no artifact built**.
- `build-linux` red ⇒ compile regression on a release-profile build (which
  the ci-gate's dev-profile build wouldn't have caught — release builds
  exercise different codegen paths).
- `attach-release-assets` red ⇒ Forgejo API surface drift; artifact built and
  in CI artifacts but not on the release page.
- Each publish-* job red ⇒ specific crate's `cargo publish` failed; can rerun
  from there without rebuilding earlier crates.

**Anti-pattern explicitly avoided:** a single "release" megajob that compiles
+ uploads + publishes. Forgejo's opaque-logs reality makes that
undebuggable.

---

## 6. The operator playbook (first real fire of `0.1.0`)

This is the human-side complement to the workflow. Captured here so it ships
with the design — not hidden in the operator's head.

**Pre-flight:**
1. AC#7 GATE (#36) PASSED. S1_VERDICT on the release SHA = PASS / D-A2=GO.
2. Phase 2 dogfood (#37) clean — at least one human session per platform.
3. D1/CWDL-12 has been resolved → public name picked → all crates renamed
   accordingly (one focused commit, ci-gated green).
4. crates.io name(s) reserved by the operator (placeholder `cargo publish
   --dry-run` to confirm).
5. `[workspace.package].version` bumped from missing → `0.1.0`; all crates
   inherit (§4.1).
6. `CHANGELOG.md` has a `## 0.1.0` heading committed.

**Cut the tag:**
```bash
git tag -a v0.1.0 <ratified-sha> -m "cargoless 0.1.0 — first public release"
git push origin v0.1.0
```

**CI runs (release.yml, automatic):**
- `tag-validate` PASS
- `build-linux` produces `<pubname>-v0.1.0-x86_64-unknown-linux-gnu.tgz`
- `attach-release-assets` posts it + SHA-256 to the Forgejo release page

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
  crate.
- `cargo binstall <pubname>` on Linux x86_64 fetches the prebuilt and unpacks
  to a working binary in <5s.
- `cargo binstall <pubname>` on macOS falls through to `cargo install`
  (graceful degradation, intentional for 0.1.0).
- Release announced (CWDL-9 / launch blog gates here).

---

## 7. The cargo-binstall metadata stub

`crates/tf-cli/Cargo.toml` gains a `[package.metadata.binstall]` table. Cargo
treats `[package.metadata.*]` as opaque (it does NOT validate the contents),
so this lands **today** without touching any existing gate. Only
`cargo-binstall` itself ever reads it.

```toml
[package.metadata.binstall]
# Forgejo release-asset URL template. Variables resolved by cargo-binstall:
#   {repo}    — package.repository (root Cargo.toml workspace.package)
#   {name}    — package.name (this crate)
#   {version} — package.version (no leading "v")
#   {target}  — target triple, e.g. x86_64-unknown-linux-gnu
#   {archive-suffix} — derived from pkg-fmt, e.g. ".tgz"
# TODO(D1/CWDL-12): when the public name is picked, decide whether `{name}` is
#   the renamed `tf-cli` (e.g., `<pubname>`) and update the URL accordingly.
# TODO(release-0.1.0): the URL template assumes Forgejo's release-asset path
#   shape stays POST /api/v1/repos/{owner}/{repo}/releases/{id}/assets — verify
#   download URL is /{repo}/releases/download/v{version}/{file} before fire.
pkg-url = "{repo}/releases/download/v{version}/{name}-v{version}-{target}{archive-suffix}"
pkg-fmt = "tgz"

# Inside the tarball, the binary lives at the top level (built by
# `cargo build -p tf-cli --release` → target/release/tftrunk). The
# binary name `tftrunk` is the CURRENT placeholder; D1 may rename.
# TODO(D1): if the binary name changes from `tftrunk`, update bin-dir.
bin-dir = "{name}-v{version}-{target}/tftrunk"

# x86_64-unknown-linux-gnu only for 0.1.0 (see D-RELEASE §3).
# When macOS prebuilts ship (release 0.2.0+ pending operator-decision in §8),
# add a [package.metadata.binstall.overrides."aarch64-apple-darwin"] block here.
```

**No real fire required for the stub to be useful** — once committed, anyone
running `cargo binstall <crate>` after the first crates.io publish gets a
prebuilt if Linux x86_64, falls back to `cargo install` otherwise.

---

## 8. Open decisions blocking real-fire

This skeleton CANNOT cut release `0.1.0` until ALL of these resolve. Each is
called out, not invented.

| # | Open question | Owner | Blocks |
|---|---|---|---|
| 1 | **D1/CWDL-12 — public product name.** Affects: crate names on crates.io, the binary name `tftrunk`, the binstall `bin-dir` and `pkg-url` template, the README install commands, the launch blog title. | lead | EVERYTHING. No release can fire under "cargoless" because that is the placeholder/repo identifier, not a public name commitment. |
| 2 | **Mac builder strategy.** Three real options: (a) **GitHub Actions free-tier macOS runner** (cross-repo mirror; ties release infra to GitHub, not just Forgejo); (b) **operator-laptop one-shot** (operator runs `cargo build --release --target aarch64-apple-darwin` from their Mac and uploads manually; honest but unautomated); (c) **ship-source-only-and-honest** (no macOS prebuilt at all; `cargo install` is the macOS path; document as such). The team-lead suggested all three are real reserved options. | operator + lead | `build-macos` job in release.yml; macOS user experience. Drops from `0.1.0` either way; affects `0.2.0` design. |
| 3 | **crates.io crate-name resolution.** Are `tf-proto` / `tf-cas` / `tf-core` / `tf-cli` going to be the published names (likely collisions), or are they internal-only with a renamed top-level crate (option 4.2.b), or all renamed under a `<pubname>-*` namespace (option 4.2.a)? | operator + lead, post-D1 | `tag-validate` and `publish-*` job names; the `bin-dir` template. |
| 4 | **CHANGELOG.md format & seeding.** Is the project keepachangelog.com style? semantic-release-style? hand-rolled? `tag-validate` asserts a heading for $VERSION exists; the format determines the assertion regex. | docs / lead | `tag-validate` job; can be a one-line change. |
| 5 | **crates.io token automation timeline.** Operator-run for `0.1.0` is approved. Is `0.2.0`-automatable, or is human-in-the-loop the permanent model? | operator | Whether `publish-*` jobs ever lose `if: false`. |
| 6 | **Forgejo release-asset URL shape.** The `pkg-url` template in §7 assumes `/{repo}/releases/download/v{version}/{file}` — Forgejo follows GitHub's convention, but a placeholder release should be cut on a non-production tag (e.g., `v0.0.1-test`) to verify before locking the template. | operator (one-time) | binstall first-fire correctness. |
| 7 | **GPG signing of release tags + tarball signatures.** v1 parking-lot per CLAUDE.md non-goals. Recorded here for the record — when someone asks "why no .asc files?" the answer is "deliberate 0.1.0 scope decision; see D-RELEASE §9." | lead | Nothing in v0.1.0; trust model for downstream packagers. |
| 8 | **Canonical install URL / public-source-access strategy** *(launch-blocker, surfaced by dogfood-lead 2026-05-17)*. Current Forgejo repo is **auth-walled on every git protocol endpoint** (HTTP 401 + `www-authenticate: Basic Gitea` on anonymous fetch — confirmed by direct git+curl probes from the cluster pod). Anonymous `cargo install --git https://forgejo.triform.dev/...` fails at first byte; the OSS-claim install path is technically broken today. Three reserved real options: **(a) flip Forgejo repo to public** (Gitea per-repo "publicly visible" toggle; minimal surgery, keeps source of truth on Forgejo, does NOT solve Mac-builder); **(b) mirror to github.com** and document GitHub as the canonical public-source URL (compound benefit: ALSO solves §8 #2 Mac builder via GH Actions free-tier macOS runners + provides discoverability — collapses two opens into one decision); **(c) document HTTP-Basic auth with a public-read deploy token** (defeats the OSS pitch; not recommended). The release pipeline cannot fire until this resolves. **If (b) is chosen, §8 #2 collapses into this decision and `build-macos` re-enters the workflow as a GitHub Actions job.** Until resolved, the `pkg-url` template stays templated (`{repo}` resolves from `[workspace.package].repository`) — flipping its value is a one-line `Cargo.toml` change post-decision. | operator + lead | `cargo install --git` install path; README install instructions; `pkg-url` template's `{repo}` substitution; potentially §8 #2 (compound resolution if (b)); the "OSS pitch" claim in launch blog (CWDL-9). |

---

## 9. Non-goals for release `0.1.0`

All per CLAUDE.md v1 parking lot. **None** of these belong in the release
workflow or any prebuilt artifact.

- **Windows official support.** `cargo install` works there on best-effort; no
  prebuilt, no `[overrides."x86_64-pc-windows-msvc"]` in binstall metadata.
- **Code-signing / notarization.** No Apple Developer ID, no Authenticode, no
  GPG-signed tags. Trust is "operator's crates.io account + tagged SHA in
  Forgejo."
- **Homebrew formula / scoop manifest / winget / AUR / nix flake.** Third
  parties may package; project ships source + crates.io + Linux binstall only.
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
[ ] §8 #8 canonical install URL decided (Forgejo-public / GitHub-mirror /
    documented-auth). If GitHub-mirror, also resolves §8 #2 (Mac builder).
    [workspace.package].repository updated to the chosen canonical URL.
[ ] Mac builder strategy decided (§8 #2) — even if the decision is "no macOS
    prebuilt in 0.1.0", record it. (Collapses into §8 #8 if (b) GitHub-mirror.)
[ ] release.yml.draft → release.yml; remove `if: false` from build/validate
    jobs (keep on publish-* until 0.2.0 token-automation lands).
[ ] One throwaway test fire on `v0.0.1-rc.1` against a test release page to
    verify the Forgejo asset-URL shape (§8 #6).
[ ] AC#7 GATE (#36) PASSED on the release SHA.
[ ] Phase 2 dogfood (#37) signed off.
[ ] git tag -a v0.1.0 <sha>; git push origin v0.1.0.
[ ] Operator runs `cargo publish` topology from laptop (§6).
[ ] `cargo install` and `cargo binstall` smoke-test from clean machines.
[ ] CWDL-9 launch blog reviewed by ≥2 (incl. outside reviewer); announce.
```

When the checklist is clean and team-lead ratifies, **then** the skeleton fires.
Not before.

---

## Appendix A — Why no GitHub mirror in the design?

Asked but not specced. The project lives at `forgejo.triform.dev/triform/
cargoless`; that's the source of truth for tags, releases, and binstall asset
URLs. A GitHub mirror could be added as a **read-only convenience** post-launch
(for discoverability) but the release pipeline does NOT depend on it. If
operator picks Mac builder option (a) — GitHub Actions free-tier macOS runner
— a *narrow* GitHub mirror+workflow appears, scoped to Mac-builds only,
posting back to the Forgejo release page. Recorded for the moment that
decision lands; not specced ahead of it.

## Appendix B — Why `--locked` everywhere

Mirrors the discipline `e37050a` (`ci(ci-gate): --locked to mirror ci.yml
post-#25`) brought to ci.yml + ci-gate after the workspace gained real
external deps. A release that re-resolves the dep graph differently from CI is
a release that ships untested code. `--locked` makes the committed `Cargo.lock`
the input identity, not a hint. Already part of `tf_proto::BuildIdentity`
(`cargo_lock` field) by design — release just inherits the same axiom.
