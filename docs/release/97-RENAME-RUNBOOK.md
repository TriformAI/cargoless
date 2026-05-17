# #97 — Atomic Workspace Internal-Crate Rename: RUNBOOK + DRY-RUN

**Status:** PREPARATION ARTIFACT. This branch (`agent/dev-fixer-97-plan`,
off main @ `04c7de5`) exists **for diffing/validation only**. It is **NOT
the real rename commit** — that branches off the FINAL post-Phase-C +
Phase-D-finalize `main` to avoid rebase churn (lead discipline, same as
#87's base-confirmation gating).

**Author:** dev-fixer · **Executes:** dev-fixer (author + self-gate) ·
**ci-gate + ff:** builder-infra.

---

## 0. Preconditions (execution is STRICTLY HELD until ALL three)

| Gate | State (as of authoring) |
|---|---|
| #87-ff landed | ✅ DONE @ `61d8324` (verified: `crates/tf-cli/Cargo.toml` already `name = "cargoless"`, `[[bin]] name = "cargoless"`, lock pkg `cargoless`) |
| Phase C green | ⏳ pending (builder-infra) |
| Phase-D-finalize | ⏳ pending (after Phase C) |

Execute ONLY after the lead's explicit **"#97 unblocked, branch off main @
\<SHA\>"** signal. Branch off **that** SHA, not `04c7de5`.

---

## 1. ⚠ CORRECTED SCOPE FINDING — `bench/` is NOT in scope

The mission brief stated *"`bench/` included as a structural necessity (it
depends on the renamed crates)"*. **This premise is factually wrong** —
verified, not assumed:

- `bench/harness/Cargo.toml`: `[workspace]` (standalone), `[dependencies]`
  **empty** (std-only by explicit design). Package `cargoless-bench-harness`,
  bin `ra-latency`. No `tf-*` path-dep.
- `bench/fixture/Cargo.toml`: `[workspace]` (standalone), sole dep
  `leptos = "=0.6.15"`. Package `cargoless-bench-fixture`. No `tf-*` path-dep.
- `bench/fixture/Cargo.lock`: leptos tree only — **zero** internal-crate
  entries.
- `grep -rnE 'tf_proto|tf_cas|tf_core|tf-proto|tf-cas|tf-core|tf-cli'
  bench/` → **zero hits**.

`bench/` neither depends on nor textually references the renamed crates.
It is **excluded from #97**. Touching it would be unfounded scope, not
structural necessity. (bench-lead therefore needs no coordination for
#97; the lead's bench-lead FYI can be retracted/relaxed.)

> This is the verify-don't-assume discipline applied to the spec itself —
> same class as the F8-redo spec-error catch and the F13a
> `cargo`-vs-`cargoless` catch.

---

## 2. Exact rename surface (enumerated, @ `04c7de5`)

### 2a. Directories (`git mv`, history-preserving)

```
crates/tf-proto  → crates/cargoless-proto
crates/tf-cas    → crates/cargoless-cas
crates/tf-core   → crates/cargoless-core
crates/tf-cli    → crates/cargoless          # bin-crate dir (lead-APPROVED scheme)
```

`crates/cargoless` (suffixless) — lead-approved: matches the published
crate name (`cargo install cargoless` ↔ `crates/cargoless/`), is the
serde/tokio suffixless-flagship + suffixed-libs norm, avoids a permanent
dir≠package mismatch. No collision (`crates/cargoless` does not exist).

### 2b. Package-name + path-dep map

| Old pkg | New pkg | Cargo.toml path-deps to rewrite |
|---|---|---|
| `tf-proto` | `cargoless-proto` | (none) |
| `tf-cas` | `cargoless-cas` | `tf-proto = { path = "../tf-proto", version = "0.0.0" }` → `cargoless-proto = { path = "../cargoless-proto", version = "0.0.0" }` |
| `tf-core` | `cargoless-core` | `tf-proto`→`cargoless-proto` (`../cargoless-proto`), `tf-cas`→`cargoless-cas` (`../cargoless-cas`); both keep `, version = "0.0.0"` |
| `cargoless` (was tf-cli; name already done by #87) | `cargoless` (unchanged) | `tf-core = { path = "../tf-core", version = "0.0.0" }` → `cargoless-core = { path = "../cargoless-core", version = "0.0.0" }` |

**Phase-B boundary (builder-infra flagged):** every path-dep MUST keep its
explicit `, version = "0.0.0"`. `git diff` every `[dependencies]` hunk to
confirm none lost it.

### 2c. Workspace root `Cargo.toml`

`members` (only list; **no `default-members`** — verified):
```
members = [
    "crates/cargoless-proto",
    "crates/cargoless-cas",
    "crates/cargoless-core",
    "crates/cargoless",
]
```

### 2d. Rust source — snake idents (live path refs)

`tf_proto` ×24, `tf_cas` ×8, `tf_core` ×102 across:
`crates/{cargoless-proto,cargoless-cas,cargoless-core,cargoless}/src/**`
+ `crates/cargoless-core/tests/{ac5_dedupe.rs,ac6_kill9.rs}`.
Transform: `tf_proto→cargoless_proto`, `tf_cas→cargoless_cas`,
`tf_core→cargoless_core`. Includes doc-comment refs that name a **live**
path (e.g. `cargoless-proto/src/lib.rs:382` `tf_core::model::
check_once_with_diagnostics` — a live cross-reference; rewriting keeps the
doc accurate).

### 2e. `scripts/ci-gate` — path refs (NOT `-p` flags)

`-p cargoless` flags are **already correct** (post-#87). The hazard is the
hardcoded **directory path** `crates/tf-cli/`:

- Line 235: `grep -qE '^[[:space:]]*integration...' $REMOTE_SRC/crates/tf-cli/Cargo.toml`
  → `$REMOTE_SRC/crates/cargoless/Cargo.toml`. **If missed, the
  integration-feature detection silently fails → integ-build/test/clippy
  SKIPPED (false-green).** This is the concrete gate-self-reference hazard.
- Comment lines 37, 228, 237, 256: prose `crates/tf-cli/` → `crates/cargoless/`.

### 2f. Cargo.lock (root) — surgical, NO local cargo

Exact transform (Cargo sorts `[[package]]` blocks AND each `dependencies`
array alphabetically):

- `cargoless` block: `dependencies = ["tf-core"]` → `["cargoless-core"]`.
  Block stays in place (`cargoless` < `cargoless-cas`: shorter prefix sorts
  first; `cargoless` < `cfg-if`).
- **Move** the three renamed blocks from the `t…` region (current lines
  ~239–259) to **immediately after the `cargoless` block, before
  `cfg-if`** (since `cargoless-*` < `cfg-if`: `a`<`f` at index 1), in this
  order: `cargoless-cas`, `cargoless-core`, `cargoless-proto`.
  - `cargoless-cas`: deps `["tf-proto"]` → `["cargoless-proto"]`.
  - `cargoless-core`: deps `["notify","serde_json","tf-cas","tf-proto"]`
    → re-sorted `["cargoless-cas","cargoless-proto","notify","serde_json"]`
    (the two renamed entries move to the **top**: `c` < `n` < `s`).
  - `cargoless-proto`: no `dependencies`.
- Delete the old `tf-cas` / `tf-core` / `tf-proto` blocks at the `t…`
  region. `version`/`source`/`checksum` lines unchanged (path crates have
  none).

`bench/fixture/Cargo.lock` — **untouched** (no internal-crate entries).

### 2g. `NAMING-DRIFT-INVENTORY.md` Tier-C override note

Add one line recording the operator override (Tier-C "keep internal
`tf-*`" recommendation was reversed → `cargoless-*` everywhere). **Default:
flag to docs-launch-lead** to keep #97 a code-only atomic commit (cleaner
gate surface; docs is their lane). If the lead wants it in #97, it's a
1-line append, no gate risk.

---

## 3. Ordered execution steps (the actual recipe)

```
0.  git fetch origin && git checkout -b agent/dev-fixer-97 <UNBLOCK_SHA>
1.  git mv crates/tf-proto crates/cargoless-proto
    git mv crates/tf-cas   crates/cargoless-cas
    git mv crates/tf-core  crates/cargoless-core
    git mv crates/tf-cli   crates/cargoless
2.  Edit 5 Cargo.tomls per §2b/§2c (Edit tool, exact-string — NOT sed):
      - root: members[] (§2c)
      - cargoless-cas:  [package] name + dep cargoless-proto (path+version)
      - cargoless-core: [package] name + deps cargoless-proto, cargoless-cas
      - cargoless-proto:[package] name
      - cargoless:      dep cargoless-core (name already "cargoless" via #87);
                        ALSO fix now-stale CURRENT-STATE comments in this
                        file ("internal libs ... stay tf-* per ... Tier C.
                        Crate directory stays crates/tf-cli/" — #97
                        falsifies these → rewrite to the cargoless-*
                        reality) and live path refs in comments
                        (`tf_core::build::{...}`, `tf_proto::PublishedArtifact`)
3.  Rust snake transform — see §4 (verify-don't-blind-sed). Per-file via
    enumerated grep, reviewed; NOT a global sed.
4.  scripts/ci-gate: Edit §2e (one functional path @235 + 4 comments).
5.  Cargo.lock: hand-edit per §2f. Then visual block-order audit.
6.  (If lead opts in) NAMING-DRIFT-INVENTORY Tier-C note; else SendMessage
    docs-launch-lead.
7.  rustfmt --edition 2024 crates/**/*.rs   (EXECUTION.md discipline:
    NEVER bare rustfmt on this Edition-2024 tree — #94).
8.  git add <explicit paths> ; ONE atomic commit:
      fix(#97): rename internal crates tf-{proto,cas,core}→cargoless-*
      (Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@…>)
9.  Self-gate: scripts/ci-gate agent/dev-fixer-97  — see §5 (MUST use the
    BRANCH'S OWN post-rename gate script).
10. Inspect enumeration (ALL GREEN + test count vs source), report SHA to
    lead → builder-infra ci-gates + ffs.
```

**Atomicity:** steps 1–8 are ONE commit. A half-renamed workspace does not
compile; there is no valid intermediate state. ci-gate is the arbiter that
every import resolved + every Cargo.toml is consistent + Cargo.lock
matches.

---

## 4. The 6-step verify-don't-blind-sed plan

The #87/F13a lesson, scaled workspace-wide. A blind
`s/tf-/cargoless-/g` **will corrupt the tree** — confirmed landmines:

1. **Enumerate by category first** (done in §2; re-run on the real base
   SHA — counts may shift). Categories: (i) Rust snake idents; (ii)
   Cargo.toml kebab names/paths; (iii) ci-gate path refs; (iv)
   doc-comments/strings.
2. **Two transforms, NEVER conflated:** snake `tf_{proto,cas,core}` →
   `cargoless_{proto,cas,core}` (Rust source only); kebab
   `tf-{proto,cas,core,cli}` → `cargoless-{proto,cas,core}` /
   `crates/cargoless` (Cargo.toml / ci-gate / members). Match the **exact
   tokens with word boundaries** — never a bare `tf-`/`tf_` prefix.
3. **PRESERVE false-match / historical tokens — do NOT rename:**
   - **`tf-multiverse`** — the *other* monorepo / shared-CI environment
     (ci-gate header lines 7/14/19, docs). Unrelated. A prefix sed →
     `cargoless-multiverse` = corruption.
   - **`tf-trunk`** — the *rejected pre-#89 placeholder name*, referenced
     as **history** in doc-comments (`cargoless-core/src/lib.rs:31,45,46,84`;
     `crates/cargoless/src/watch.rs:81`; etc.). These DOCUMENT the old
     state ("the old `tf-trunk` placeholder leaked the rejected `tf`…") and
     MUST stay verbatim. (Kebab `tf-trunk` ≠ any crate name, and the snake
     transform never matches it — clean separation, but diff-review
     confirms.)
   - Any doc/CHANGELOG/inventory line that names an old identifier **as
     narrative about the past** (NAMING-DRIFT-INVENTORY's own catalog) —
     preserve; only *current-entity* refs change.
4. **Per-file-type bulk edit → `git diff` EVERY hunk**, scanning
   specifically for: (a) path-deps that lost `, version = "0.0.0"`; (b)
   Cargo.lock block/array order; (c) any hit inside a `"string"` or
   `//`/`///`/`//!` that is historical-narrative not a live ref.
5. **Cargo.lock by hand** (§2f) — never sed; builder-infra lock-review like
   #87.
6. **One atomic commit; ci-gate is the validator** that every import
   resolved (build/clippy), every Cargo.toml consistent (build --locked),
   Cargo.lock matches (`--locked`), tests still enumerate.

---

## 5. Gate-self-reference hazard (lead-flagged; concrete)

`scripts/ci-gate` streams the **branch's** tree to the builder and runs
`cargo` *inside that tree*. The `-p cargoless` flags already resolve
post-#87. **But** the integration-feature probe greps a **hardcoded
path** `$REMOTE_SRC/crates/tf-cli/Cargo.toml` (line 235). After #97 that
path is `crates/cargoless/Cargo.toml`:

- The fix (§2e) is **part of #97's atomic commit** — so the branch's own
  ci-gate is self-consistent.
- **Self-gate + builder-infra ci-gate MUST invoke the #97 branch's
  ci-gate**, never a pre-rename sibling-checkout copy. A stale pre-rename
  gate would grep the missing `crates/tf-cli/Cargo.toml`, find no
  `integration` line, and **silently SKIP** integ-build/test/clippy →
  false-green. Same class as the #87/#95 `-p cargoless` lesson; folded
  into #96's ci-gate-self-consistency note (builder-infra owns that).

---

## 6. #96 (D1-drift-guard) coupling — FLAG for builder-infra/lead

#96's allowlist was scoped to the #87 end-state (`crates/tf-cli/` dir
kept). #97 **moves** that dir to `crates/cargoless/` and renames 3 lib
dirs, so #96's grep pattern + allowlist go stale the instant #97 lands.
Required ordering (no dev-fixer action — builder-infra owns #96):

```
#97 lands on main  →  builder-infra updates #96 allowlist/pattern to the
                       cargoless-* reality  →  drift-guard re-enforces
```

Flagging only so the sequence is explicit; the guard must not run against
a cargoless-* tree with a tf-* allowlist (it would false-fire on every
renamed path).

---

## 7. Risk register

| Risk | Mitigation |
|---|---|
| Cargo.lock hand-edit rejected by `--locked` | Exact layout in §2f; builder-infra lock-review (proven on #87); dry-run diff (§8) lets builder-infra **pre-ci-gate the recipe** before the real window |
| Blind-sed corrupts `tf-multiverse`/`tf-trunk` | §4 step 3 explicit do-not-touch list; per-hunk diff review |
| ci-gate integ silently skipped post-rename | §5 — fix in same commit + run branch's own gate |
| Stale current-state comments survive (e.g. tf-cli Cargo.toml "stays crates/tf-cli/") | §3 step 2 calls them out explicitly as rewrite targets |
| Real base ≠ `04c7de5` (Phase-C/D commits land) | Re-run §2 enumeration on `<UNBLOCK_SHA>`; counts/lines may shift, recipe shape is stable |
| bench/ scope-creep | §1 — verified out of scope; do not touch |

---

## 8. Deliverable status

- This runbook: committed on `agent/dev-fixer-97-plan`.
- Dry-run unified diff: `docs/release/97-DRY-RUN.diff` (the full mechanical
  rename applied at `04c7de5`, captured as `git diff`, 31 files /
  +228 −192). It is a **preview + recipe-validation artifact** —
  builder-infra MAY ci-gate this scratch branch to pre-validate the lock
  recipe compiles, de-risking the real window. It is **NOT** the commit
  that ffs to `main`.

---

## 9. ⚠⚠ CRITICAL FINDING — wire-format magic-byte constants are FROZEN

Enumeration surfaced that the **kebab** tokens `tf-cas` / `tf-core` /
`tf-proto` occur in **two semantically distinct roles**. This is the
single highest-risk item in #97 and is **invisible to the gate** (a
mis-rename here compiles + passes build/test/clippy, then corrupts at
runtime):

### 9a. FROZEN — serialization / cache-format magic bytes (NEVER rename)

| File:line | Constant | Why frozen |
|---|---|---|
| `cargoless-cas/src/identity.rs:32` | `const SCHEME: &[u8] = b"tf-cas/input-hash/v1"` | The InputHash domain-separation tag. Change it ⇒ **every BuildIdentity→InputHash changes** ⇒ entire content-addressed cache invalidated + the `equal BuildIdentity ⇒ equal InputHash` determinism invariant (the reason `cargoless-cas` exists, AC#4) silently broken |
| `cargoless-cas/src/identity.rs:100` | `b"tf-cas/absent:"` | Absent-input hash sentinel — same hash-domain corruption |
| `cargoless-cas/src/tree.rs:127` | `b"tf-cas/source-tree/v1\n"` | Source-tree blob header — changes every tree hash |
| `cargoless-core/src/build.rs:203` | `const DIST_BLOB_HEADER: &[u8] = b"tf-core/dist/v1\n"` | On-disk dist-blob container header — old artifacts become unreadable |

These are **format identifiers, not name references**. They are a
hard do-not-touch list **stricter than `tf-multiverse`/`tf-trunk`**: those
are wrong-but-cosmetic; these are gate-green-but-data-corrupting. The
snake transform (`\btf_core\b` etc.) does **not** match them (they are
kebab byte-strings) — the dry-run correctly leaves all four byte-identical.
**Any future "finish the brand rename" pass must carry this exclusion
list verbatim.** (Versioning these formats to a cargoless-prefixed tag is
a deliberate, migration-bearing change — explicitly out of #97 scope; if
ever wanted it is its own task with a cache-migration story.)

### 9b. Prose crate-name refs in doc-comments — SCOPE DECISION for the lead

~40 kebab `tf-{proto,cas,core,cli}` occurrences are **prose in
`//`/`///`/`//!` comments** naming a crate descriptively (e.g.
`//! \`tf-proto\` — the cross-crate contract`, `// the D1 rename is one
literal in tf-core`). They are **not code paths** — the snake transform
already converted every real Rust path (0 snake residuals), so the
workspace **compiles and gate-passes with these untouched**. They are a
brand-coherence/doc-accuracy concern only.

**Two options — lead's call (flagged, not unilaterally decided):**

- **(A) Minimal-safe core (what the dry-run captures):** #97 = the
  compile/gate-affecting rename only. Prose-comment sweep deferred to a
  separate low-risk docs-consistency pass. Smallest diff, smallest
  false-positive surface, but ~40 comments keep saying `tf-core` (the
  #96 drift-guard must be scoped to ignore *comment* residuals, or it
  false-fires).
- **(B) Full brand coherence in #97:** also sweep the ~40 prose refs
  (kebab → cargoless-*), with §9a's frozen list as a hard exclusion.
  Matches the operator's "one token everywhere" intent and keeps #96's
  guard simple, but multiplies the catastrophic-false-positive surface
  (every edit must be diffed against the §9a list) and widens the diff
  ~3×.

**My recommendation: (B) but as a clearly separated SECOND commit on the
same branch** — commit 1 = the compile-affecting atomic core (this
dry-run); commit 2 = the prose sweep with §9a exclusions. Same branch,
same ff, ci-gate covers both, but bisectable: if commit 2 ever corrupts a
§9a constant the gate stays green so the *diff review of commit 2 in
isolation* is the safety net, not a needle in a 220-line core diff.
Atomicity requirement ("half-renamed doesn't compile") only binds
commit 1; commit 2 is pure comments → always compiles. Defer to lead.
