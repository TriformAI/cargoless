# Operator Handoff — First Real Release (CWDL-71 Phase D)

**Status:** SKELETON (CWDL-71 Phase D, pre-emptive draft 2026-05-17).
Finalized after CWDL-71 Phase C (the `v0.0.0` rehearsal) verifies the
activated pipeline end-to-end. **Not the launch trigger itself** — this
is the operator's runbook for *when* launch is greenlit.

**Author:** `builder-infra`. Companion to
[`docs/design/D-RELEASE.md`](../design/D-RELEASE.md) (the design) — this
doc is the *operational* "what the human does at launch-tag time."

**Product name:** `cargoless` (D1 resolved 2026-05-17, Plane CWDL-12;
crates.io name `cargoless` confirmed free).

---

## 0. TL;DR — the one-command launch

Once the pre-tag checklist (§3) is fully green, the entire public release
is **one command**:

```bash
git tag -a v0.1.0 <ratified-sha> -m "cargoless 0.1.0 — first public release"
git push origin v0.1.0          # → Forgejo
# Forgejo→GitHub push-mirror (sync_on_commit, §8 #9) auto-replicates the
# tag to github.com/TriformAI/cargoless within ~3–15s.
```

GitHub's receipt of the mirrored tag fires `.github/workflows/release.yml`.
Everything below §1 happens automatically. The operator does NOT manually
`git push github` — the mirror handles it.

---

## 1. What the activated pipeline does automatically

On `v[0-9]+.*` tag arrival at GitHub, `.github/workflows/release.yml`:

1. **`tag-validate`** — asserts `tag == v$([workspace.package].version)`,
   asserts every crate inherits `version.workspace = true`, asserts
   `CHANGELOG.md` has a `## [<version>]` section, asserts the tagged SHA
   has an `s1-ac2-verdict` commit status on Forgejo. Any drift → hard
   fail, no artifacts built.
2. **`build` (matrix)** — three native runners in parallel:
   - `ubuntu-latest` → `x86_64-unknown-linux-gnu`
   - `macos-14` → `aarch64-apple-darwin` (Apple Silicon)
   - `macos-13` → `x86_64-apple-darwin` (Intel)
   Each: `cargo build -p cargoless --release --locked --target <triple>`,
   strip, tarball as `cargoless-v<version>-<triple>.tgz` (the binary file
   inside is `cargoless`; layout matches the binstall `bin-dir`).
   `fail-fast: false` — one target regressing doesn't kill the others.
3. **`attach-release-assets`** — `gh release upload` posts all three
   tarballs + their `.sha256` sums to the GitHub release page.
4. **`publish-*`** — DECLARATIVE ONLY for 0.1.0 (`if: false`). See §2.

**Result**: a GitHub release at `v0.1.0` with three prebuilt tarballs.
End users immediately get:
- `cargo install --git https://github.com/TriformAI/cargoless.git cargoless --tag v0.1.0 --locked` (source, any rustc platform)
- `cargo binstall cargoless` (prebuilt: linux-x86_64 + macOS aarch64/x86_64; falls back to `cargo install` on other targets)

---

## 2. What's automated vs operator-manual

| Step | Automated? | Notes |
|---|---|---|
| Tag → build matrix → release-page assets | ✅ automated | The whole `release.yml` chain on tag push |
| Forgejo → GitHub tag/branch mirror | ✅ automated | Push-mirror, `sync_on_commit` (§8 #9) |
| `cargo install --git` install path | ✅ works on tag | No operator action; users run it |
| `cargo binstall` prebuilt fetch | ✅ works on tag | Asset URL matches binstall metadata (verified §8 #6) |
| **crates.io publish** (`cargo publish`) | ❌ **OPERATOR-MANUAL** | **§8 #5 — the ONE thing needing operator hands before first publish.** See §2.1. |
| GPG-signed tags / `.asc` tarball sigs | ❌ not done | §8 #7 — v1+ parking-lot per CLAUDE.md. Trust model = operator's crates.io account + GitHub TLS. |

### 2.1 crates.io publish — the operator-manual step (§8 #5)

For `0.1.0`, crates.io publish is **operator-run from their laptop**, NOT
automated (the `publish-*` jobs in release.yml are `if: false`). Sequence,
from the tagged SHA, in topological order (each waits ~30s for crates.io
propagation before the next):

```bash
git checkout v0.1.0     # work from the tag, never HEAD
cargo publish -p tf-proto --locked     # (internal crate names — see NOTE)
cargo publish -p tf-cas   --locked
cargo publish -p tf-core  --locked
cargo publish -p cargoless --locked    # the user-facing crate
```

**NOTE on crate names** (depends on D1-rename #87's final decision):
- If only the top-level crate renamed (`tf-cli`→`cargoless`, internals stay
  `tf-proto`/`tf-cas`/`tf-core`): publish order as above. The internal
  crates either publish under `tf-*` (if free) or are marked
  `publish = false` and only `cargoless` ships to crates.io as a
  self-contained binary crate. **Confirm which at finalize-time** (TBD-marker).
- crates.io token: operator configures `~/.cargo/credentials.toml` or
  `CARGO_REGISTRY_TOKEN` env once, before the first `cargo publish`.
  Never committed; never in CI for 0.1.0.

Automating this (Forgejo/GitHub-Actions-secret) is a **0.2.0+** concern —
token-rotation strategy needs design first. Tracked, not launch-blocking.

---

## 3. Pre-tag checklist (all must be ✅ before cutting the launch tag)

State as of skeleton-draft 2026-05-17 (✅ done / ⏳ pending / TBD = fill at finalize):

```
[✅] D1 product name resolved — `cargoless` (CWDL-12, 2026-05-17)
[⏳] D1-rename landed on main (#87 — docs-launch-lead surgical rename;
     builder-infra ci-gate-reviews binstall/CI hunk)
[✅] §8 #2 Mac builder — resolved (GH Actions macos-{13,14})
[✅] §8 #4 CHANGELOG format + scaffold — Keep a Changelog v1.1.0
[✅] §8 #6 GitHub asset-URL shape — verified (static analysis + PKG/BIN fix)
[✅] §8 #8 canonical install URL — github.com/TriformAI/cargoless, seeded,
     anonymous install verified end-to-end
[✅] §8 #9 Forgejo→GitHub push-mirror — live, sync_on_commit
[✅] CWDL-71 Phase A — release.yml activated on GitHub Actions
[✅] CWDL-71 Phase B — version hoisted to [workspace.package]
[⏳] CWDL-71 Phase C — v0.0.0 rehearsal: GH Actions 3-runner matrix
     verified end-to-end (TBD: rehearsal outcome — fill after Phase C)
[⏳] §8 #5 crates.io token — operator-configured (operator-time, §2.1)
[ ] §8 #7 GPG signing — v1+ parking-lot (intentionally NOT a 0.1.0 gate)
[TBD] AC#7 comparative verdict — bench-lead (#36); throughput numbers in README
[TBD] D-A2 / AC#2 renegotiation — operator decision (#48); honest
      save→verdict claim wording in README/blog
[✅] Phase 2 dogfood — DOGFOOD-REPORT.md on main (12 findings; F1–F12 fixed)
[TBD] AC#9 launch blog — reviewed by ≥2 incl. outside (human-gated)
[TBD] `[workspace.package].version` bumped 0.0.0 → 0.1.0 (one focused
      commit at launch-prep time; tag-validate enforces tag==version)
```

**The launch tag is cut only when every line above is ✅** (the `[ ]`
GPG line stays unchecked deliberately — it's an explicit non-gate, not
an omission).

---

## 4. Rollback procedure (if a tagged release is bad)

If `v0.1.0` ships and a launch-blocker is discovered post-tag:

1. **Delete the GitHub release + tag**:
   ```bash
   gh release delete v0.1.0 --repo TriformAI/cargoless --yes --cleanup-tag
   git push origin --delete v0.1.0     # also remove from Forgejo
   ```
2. **crates.io yank** (if any crate was already `cargo publish`'d — yank
   does NOT delete, it prevents NEW dependents; existing lockfiles keep
   working, which is correct):
   ```bash
   cargo yank --version 0.1.0 cargoless
   cargo yank --version 0.1.0 tf-core    # + tf-cas, tf-proto if published
   ```
3. **Fix forward** — the bug fix lands on main via the normal
   ci-gate→ff flow; cut `v0.1.1` when green. Never re-cut `v0.1.0`
   (immutable-tag discipline; a re-pointed tag is a supply-chain
   anti-pattern — consumers cache by tag).
4. **Communicate** — if the bad release was announced (blog/social),
   a correction note. crates.io yank is silent to existing users;
   a re-pointed-tag would NOT be — hence never re-cut.

**Pre-publish rollback is cheap** (delete tag+release, no yank needed —
nothing consumed it yet). **Post-`cargo publish` rollback is yank-only**
(crates.io is append-only by design). This asymmetry is why §2.1's
operator-manual publish is deliberately the LAST step, after the
GitHub-release artifacts have been smoke-tested.

---

## 5. Finalize-time TODO (fill these after Phase C + before first real launch)

- [ ] Phase C rehearsal outcome: did the GH Actions 3-runner matrix
      produce all 3 installable tarballs? macOS-runner build path
      exercised? `cargo binstall cargoless` fetch verified? (builder-infra
      fills after Phase C runs)
- [ ] Final throughput numbers for README/blog (bench-lead, AC#7 #36)
- [ ] D-A2 honest save→verdict claim wording (operator decision #48)
- [ ] crate-publish name confirmation (all-internal-renamed vs
      top-level-only — depends on #87 final scope)
- [ ] Actual launch date (operator decides; this doc is name-and-mechanism
      ready, date is a business call)
- [ ] AC#9 launch blog ≥2-reviewer sign-off (human-gated; will NOT close
      in an agent session)

When all §5 boxes are filled and §3 is fully ✅, the operator runs §0.
That is launch.
