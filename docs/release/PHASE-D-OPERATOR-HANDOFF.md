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
| **`FORGEJO_READONLY_TOKEN` secret** (`tag-validate`) | ❌ **OPERATOR-MANUAL** | **NEARER operator gate.** CWDL-71 Phase C confirmed `tag-validate`'s s1-ac2-verdict read is a HARD gate on every real tag (Forgejo `REQUIRE_SIGNIN_VIEW`). See §2.1. |
| **crates.io publish** (`cargo publish`) | ❌ **OPERATOR-MANUAL** | **LATER operator gate.** §8 #5 — operator hands needed before first publish. See §2.2. |
| GPG-signed tags / `.asc` tarball sigs | ❌ not done | §8 #7 — v1+ parking-lot per CLAUDE.md. Trust model = operator's crates.io account + GitHub TLS. |

> **Two operator-time credentials, distinct stages** (record per CWDL-71
> Phase C): **Stage 1 = `FORGEJO_READONLY_TOKEN`** (§2.1) — the *nearer*
> gate; blocks Phase-C-green and every real release tag. **Stage 2 =
> crates.io token** (§2.2) — the *later* gate; blocks first `cargo
> publish` only. Stage 1 is needed strictly before Stage 2.

### 2.1 `FORGEJO_READONLY_TOKEN` — the NEARER operator-credential stage

**Stage 1 of two operator-time credentials. Gates Phase-C-green AND every
real release tag** — strictly before the crates.io token (§2.2).

`release.yml`'s `tag-validate` asserts the tagged SHA carries an
`s1-ac2-verdict` Forgejo commit status. Forgejo runs site-wide
`REQUIRE_SIGNIN_VIEW`, so the statuses API is **not anonymously readable**
(returns `{"message":"Only signed in user is allowed to call APIs."}` — a
JSON object). **CWDL-71 Phase C confirmed this is a hard launch-blocker**:
the prior anonymous `curl` crashed `jq` (`Cannot index string with
string "context"`) and false-failed *every* tag with "no s1-ac2-verdict"
when the status actually existed. `release.yml` (commit `c033aa3`) now
reads it authenticated + validates the JSON shape; the operator must
pre-stage the token once:

1. **forgejo.triform.dev** → User Settings → Applications → "Generate New
   Token". Name `cargoless-ci-readonly`. Scope: **read-only repository**
   (enough to GET commit statuses on `triform/cargoless`; no write).
2. **github.com/TriformAI/cargoless** → Settings → Secrets and variables →
   Actions → "New repository secret". Name **exactly**
   `FORGEJO_READONLY_TOKEN`. Value = the token from step 1.

**VERIFIED working mint mechanism (CWDL-71 Phase C, 2026-05-17, Forgejo
`14.0.2+gitea-1.22.0`).** The naive recipe (`gh auth token --hostname
forgejo.triform.dev` → `POST /api/v1/users/<u>/tokens`) does NOT work
here: `gh auth token` returns empty for the Forgejo host, and the
git-credential-store token can READ the API but `POST .../tokens`
returns `403 token does not have at least one of required scope(s):
[write:user]`. The authoritative, least-privilege path that worked is
the in-pod Forgejo admin CLI (operator has cluster `kubectl`):
```
kubectl exec -n forgejo <forgejo-pod> -- \
  forgejo admin user generate-access-token \
  -u triform-admin -t cargoless-ci-readonly \
  --scopes read:repository --raw
# `--raw` = bare-token stdout → capture into a var, never echo/log;
# pipe directly:  printf %s "$TOK" | gh secret set FORGEJO_READONLY_TOKEN \
#   --repo TriformAI/cargoless
```
Least-privilege confirmed post-mint via an authed `GET .../tokens`
(scopes must be exactly `["read:repository"]`). Zero token-value
leakage: `--raw`→var→stdin-pipe; only name/scopes/id ever printed;
`unset` after. This step was operator-AUTHORIZED + builder-infra-executed
in Phase C (#100). For the real launch the operator either re-mints the
same way or confirms the existing `cargoless-ci-readonly` (id=45) token
+ the GitHub secret are still present (`gh secret list`).

No code change needed (`release.yml` already consumes
`secrets.FORGEJO_READONLY_TOKEN`). Never committed; GH masks it in logs.
**Option 2** (also publish the verdict as a GitHub commit status /
release asset so GH Actions never crosses to Forgejo — D-CI-RESILIENCE
F-C) is a **0.2.0+** hardening — parked, deliberately not
launch-blocking.

### 2.2 crates.io publish — the LATER operator-credential stage (§8 #5)

For `0.1.0`, crates.io publish is **operator-run from their laptop**, NOT
automated (the `publish-*` jobs in release.yml are `if: false`). Sequence,
from the tagged SHA, in topological order (each waits ~30s for crates.io
propagation before the next):

```bash
git checkout v0.1.0     # work from the tag, never HEAD

# MANDATORY pre-publish gate (D-CI-RESILIENCE F-J). crates.io is
# append-only — a packaging/manifest/order error is UNRECOVERABLE once
# published (you burn the version). This dry-runs every crate + asserts
# the topo order + token presence FIRST. Non-zero exit ⇒ STOP, fix,
# re-run; do NOT proceed to the publishes below until it is all-green.
# (Crate set + order are cargo-metadata-derived, so this is correct
# whether internals are tf-* or, post-#97, cargoless-*.)
./scripts/crates-io-preflight        # MUST exit 0 before any line below

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
[✅] D1-rename landed on main — #87 @ `61d8324` (cherry-picked + re-gated);
     binstall/CI hunk ci-gate-reviewed; D1 source-tier complete
     (#99 statusfile follow-up @ `348c85c`; `git grep tftrunk crates/`=0)
[✅] §8 #2 Mac builder — resolved (GH Actions macos-{13,14})
[✅] §8 #4 CHANGELOG format + scaffold — Keep a Changelog v1.1.0
[✅] §8 #6 GitHub asset-URL shape — verified (static analysis + PKG/BIN fix
     + Phase-C live: sha256 byte-verified, layout==binstall bin-dir)
[✅] §8 #8 canonical install URL — github.com/TriformAI/cargoless, seeded,
     anonymous install verified end-to-end
[✅] §8 #9 Forgejo→GitHub push-mirror — live, sync_on_commit (Phase-C:
     tag+branch replication validated ~3–15s across 5 fires)
[✅] CWDL-71 Phase A — release.yml activated on GitHub Actions
[✅] CWDL-71 Phase B — version hoisted to [workspace.package]
[✅] CWDL-71 Phase C — v0.0.0 rehearsal GREEN (2026-05-18). Topology
     REVISED: 2 REQUIRED legs (ubuntu x86_64-linux + macos-14
     aarch64-darwin) + DECOUPLED best-effort Intel (x86_64-darwin /
     macos-13, `continue-on-error`, nothing `needs:` it). Validated:
     tag-validate BOTH assertions ✓, 2 required builds ✓,
     attach-release-assets ✓ (Release-object created + assets),
     sha256 byte-verified, layout==binstall bin-dir, notes==CHANGELOG.
     Rehearsal peeled 4 launch-blockers — see §5.
[⏳] §8 #5 crates.io token — operator-configured (operator-time, §2.2 — the LATER credential stage)
[✅] FORGEJO_READONLY_TOKEN secret — minted+set+verified in Phase C #100
     (`cargoless-ci-readonly` id=45, scope `read:repository`); operator
     re-confirms presence at launch via `gh secret list` (§2.1)
[ ] §8 #7 GPG signing — v1+ parking-lot (intentionally NOT a 0.1.0 gate)
[OPERATOR PRE-FLIGHT — D-CI-RESILIENCE F-C/F-D, do immediately before §0]
     ( ) authed probe returns `s1-ac2-verdict` for the launch SHA
         (`curl -H "Authorization: token <FORGEJO_READONLY_TOKEN>"
         .../commits/<sha>/statuses` → context present) AND
         forgejo.triform.dev reachable — else tag-validate hard-fails
     ( ) after `git push origin v0.1.0`: within 60s the tag is on
         github.com/TriformAI/cargoless AND a `release.yml` run started
         — else mirror health (Forgejo→Settings→Mirroring) + the
         break-glass `git push github v0.1.0` fallback
KNOWN LIMITATION (documented, NOT a gate): x86_64-apple-darwin (Intel
     Mac) prebuilt is best-effort — free-tier macos-13 runner
     availability is systemic-flaky (Phase-C: queued/never-assigned
     3×). When absent, `cargo binstall` auto-falls-back to a source
     build for Intel Mac. Recorded per D-CI-RESILIENCE F-A.
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
(crates.io is append-only by design). This asymmetry is why §2.2's
operator-manual publish is deliberately the LAST step, after the
GitHub-release artifacts have been smoke-tested.

---

## 5. Finalize-time TODO (fill these after Phase C + before first real launch)

- [✅] **Phase C rehearsal outcome (builder-infra, 2026-05-18):** GREEN.
      Topology was REVISED mid-rehearsal (the all-3-required matrix was a
      real fragility): now **2 REQUIRED** legs (ubuntu x86_64-linux +
      macos-14 aarch64-darwin) both ✅ produced installable tarballs +
      `.sha256` (byte-verified) with layout == binstall bin-dir
      `cargoless-vX-<target>/cargoless`; Release object created with
      notes == CHANGELOG section; anonymous asset fetch (= the
      `cargo binstall` fetch path) ✅. **Intel x86_64-darwin is
      best-effort/DECOUPLED** — macos-13 free-tier runner is
      systemic-flaky (queued/never-assigned 3×); absent ⇒ binstall
      source-fallback (documented known-limitation, §3). The rehearsal
      **peeled 4 launch-blockers, each invisible to static review**:
      F-1 `grep -A1` workspace-version extraction (`9f0c9cf`);
      F-2 anonymous Forgejo s1-ac2 read under REQUIRE_SIGNIN_VIEW
      (`c033aa3` + operator token #100);
      F-A all-legs `needs:` runner-wedge (`3baf659` decouple);
      F-3 `gh release upload` with no auto-created Release object
      (`1ef8dab`, + CHANGELOG-notes). 5th fire clean end-to-end. Full
      topology audit → `docs/design/D-CI-RESILIENCE.md` (#123).
- [✅] Final throughput numbers for README/blog — AC#7 #36 complete;
      numbers available for docs-launch-lead/operator to wire (not a
      builder-infra artifact; pointer only).
- [ ] D-A2 honest save→verdict claim wording (operator decision #48 —
      human/operator-gated; will NOT close in an agent session)
- [ ] crate-publish name confirmation — **depends on #97** (internal
      tf-{proto,cas,core}→cargoless-* rename). If #97 lands: §2.2 publish
      list becomes `cargoless-proto/cas/core` + `cargoless`. If #97 is
      deferred: internals stay `tf-*` (publish under `tf-*` if free, else
      `publish = false`). Confirm at the #97 decision point; §2.2 NOTE
      already enumerates both branches.
- [ ] Actual launch date (operator decides; this doc is
      name-and-mechanism ready, date is a business call)
- [ ] AC#9 launch blog ≥2-reviewer sign-off (human-gated; will NOT close
      in an agent session)
- [ ] **(D-CI-RESILIENCE F-E, HARDEN-NOW recommended pre-real-v0.1.0):**
      make the Forgejo `bench` verdict-POST assert HTTP 2xx (fail the
      step on non-2xx) so a silently-broken s1-ac2 producer reds `main`
      at commit time instead of surprising the operator at tag time —
      lead-routed item, cheap, not yet landed.

When all §5 boxes are filled and §3 is fully ✅, the operator runs §0.
That is launch.
