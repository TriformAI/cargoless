# Naming-drift inventory — pre-D1 catalog

> **✅ RESOLVED 2026-05-17 — D1 = `cargoless`** (operator decision; see
> [`D1-NAME-RECON.md`](D1-NAME-RECON.md)). The rename landed as the #87
> surgical commit, scope-reduced because #89 first consolidated all
> banner rendering to a single `tf_core::BUILD_ID` constant (already
> `"cargoless"`): the rename touched `[package]`/`[[bin]]` name,
> ci-gate `-p` flag, binstall/release.yml `PKG`/`BIN`, the CAS
> temp-prefix, the test env-var, doc-comment command hints, and the
> forward-facing docs — NOT the banner sites (#89 owned those).
> Internal libs `tf-proto`/`tf-cas`/`tf-core` + the `crates/tf-cli/`
> directory path were intentionally retained (Tier C). This document
> is preserved as the historical pre-rename catalog. The field-tier
> companion (runtime-output cross-check) is
> [`NAMING-DRIFT-FIELD-CROSS-CHECK.md`](NAMING-DRIFT-FIELD-CROSS-CHECK.md).
>
> **Original status (pre-resolution):** evidence-bundle for the post-D1
> rename sweep. Pre-staged the WHERE (this document) so that the D1
> decision (the WHAT — see [`D1-NAME-RECON.md`](D1-NAME-RECON.md))
> translated into a small focused commit instead of a 4-hour discovery
> exercise. Author: `docs-launch-lead` (2026-05-17).

---

## TL;DR

The codebase currently uses **four distinct names** for the same project
in user-visible surfaces:

| Name | Hits (non-git, non-target) | Where (representative) |
|---|---|---|
| `cargoless` | **417** | repo name, package descriptions, `.cargoless/` dir, CAS pointer scheme, error messages, almost all docs |
| `tftrunk` | **99** | binary name (`[[bin]] name = "tftrunk"`), `CARGO_BIN_EXE_tftrunk` test env var, USAGE banner, integration test paths |
| `tf-trunk` | **6** | `BUILD_ID = "tf-trunk <version>"`, CAS temp-dir prefix `tf-trunk-cas-` |
| `TF-Trunk` | **5** | Cargo.toml package descriptions ("The TF-Trunk daemon...") |

The drift exists because the project predates the D1 decision; different
parts of the codebase calcified around different working names. The
dogfood report ([`docs/dogfood/PHASE-2-REPORT.md`](../dogfood/PHASE-2-REPORT.md))
flagged this explicitly: *"D1 must land before any public launch material is
written; copy that says `cargoless watch` while the binary the user
installed is `tftrunk` will be 'command not found'."*

The rename is **structurally non-trivial** because some references are
load-bearing (the `.cargoless/` directory name is part of the on-disk
contract; the CAS pointer scheme string `cargoless-latest-green/v1` is a
versioned protocol identifier) and a careless mass-replace would break
backwards compatibility for any existing users.

This document tiers the references by *rename risk*, so the operator and
the rename-implementer can decide what to change, what to leave alone, and
what needs a migration path.

---

## Risk tiers

### Tier A — Protocol-critical (backwards-incompat if renamed; needs migration plan)

These references are part of the on-disk contract or the pointer-file
scheme; existing user projects depend on them.

#### A.1 — `.cargoless/` directory name

The user-visible per-project working directory cargoless creates inside
the user's project root. Holds the latest-green pointer file, the daemon
CLI status file, etc. Existing users have `.cargoless/` in their working
trees and `.gitignore`s.

| File | Line | Reference |
|---|---|---|
| `crates/tf-cas/src/tree.rs` | 49 | `EXCLUDED_DIRS = &["target", ".git", "dist", ".cargoless"]` — the hashed-source-tree exclusion list |
| `crates/tf-cas/src/tree.rs` | 214, 216, 224 | Tests that create + assert `.cargoless/` is excluded from CAS hash |
| `crates/tf-core/src/build.rs` | 391 | `project_root.join(".cargoless").join("latest-green")` — the publisher's pointer-file path |
| `crates/tf-core/src/build.rs` | 425 | `let dir = project_root.join(".cargoless")` — publisher's working-dir creation |
| `crates/tf-core/src/build.rs` | 664 | Test creating `.cargoless` as not-a-directory edge case |
| `crates/tf-cli/src/statusfile.rs` | 78 | `root.join(".cargoless").join("cli-status")` — daemon status file |
| `crates/tf-cli/src/statusfile.rs` | 260 | `root.join(".cargoless").join("latest-green")` — status-reading path |

**Rename impact:** if D1 picks name `<X>`, the dir becomes `.<X>/`.
Existing users' `.gitignore` lines for `.cargoless/` will not match the
new dir. Existing on-disk state (CAS cache, latest-green pointer) is
ABANDONED — the new install will not see prior state and will need to
re-build from scratch. **Migration path required:** the first run of the
renamed binary should detect a legacy `.cargoless/` dir, log a one-line
migration notice, and either rename in place (preserving state) or
silently coexist (writing new state to `.<X>/` while warning about the
orphan).

For the pre-1.0 release this is *defensible* (the project has no
shipping users on a stable name yet), but it is worth being explicit
about in the release notes for `0.1.0` if the name changes.

#### A.2 — CAS pointer-file scheme identifier

The text-format pointer file at `.cargoless/latest-green` starts with a
literal scheme identifier that is parsed by readers (status command,
v0.1 server adapter, anything consuming the pointer).

| File | Line | Reference |
|---|---|---|
| `crates/tf-proto/src/lib.rs` | 449 | `const POINTER_SCHEME: &str = "cargoless-latest-green/v1"` |
| `crates/tf-proto/src/lib.rs` | 646, 660 | Parser tests asserting the prefix |

**Rename impact:** this is a **versioned protocol identifier**, not a
brand string. Three reasonable resolutions:

1. **Leave it as-is forever** — the `cargoless-latest-green/v1` scheme
   becomes an internal protocol-version identifier divorced from the
   public name. Cleanest, lowest-risk. RECOMMENDED.
2. **Bump to `<X>-latest-green/v1`** — clean brand-coherent rename;
   forces a parser update + a migration window where readers accept
   both `cargoless-latest-green/v1` and `<X>-latest-green/v1` for one
   release cycle.
3. **Mass-replace** — breaks any v0 user's existing pointer files.
   Discouraged.

Recommendation in the rename commit: comment the scheme constant
explicitly as "stable protocol identifier; not the brand name."

---

### Tier B — Build-critical (must rename together for consistency)

These references determine the binary name, install command, USAGE
banner, and what users type at the terminal.

#### B.1 — Binary name (`[[bin]] name`)

| File | Line | Reference | Current value |
|---|---|---|---|
| `crates/tf-cli/Cargo.toml` | 43 | `[[bin]] name = "tftrunk"` | `tftrunk` |
| `crates/tf-cli/Cargo.toml` | 87 | Comment: `tftrunk ← the binary (renamed at D1)` | (documents drift) |
| `crates/tf-cli/Cargo.toml` | 90 | `TODO(D1/CWDL-12): if D1 renames the binary from tftrunk, update both...` | (TODO flag) |
| `crates/tf-cli/Cargo.toml` | 93 | `bin-dir = "{name}-v{version}-{target}/tftrunk"` (binstall metadata) | `tftrunk` |
| `crates/tf-cli/Cargo.toml` | 101 | `bin-dir = "{name}-v{version}-{target}/tftrunk.exe"` (commented Windows override) | `tftrunk` |

**Rename impact:** `cargo install tf-cli` (or post-D1 `cargo install <X>`)
produces a `<binname>` executable. Users type `<binname> check`,
`<binname> watch`, etc. Inconsistency here = "command not found" on
install.

#### B.2 — Auto-generated test env var

| File | Line | Reference |
|---|---|---|
| `crates/tf-cli/tests/diagnostics_field_finding_2.rs` | 73 | `env!("CARGO_BIN_EXE_tftrunk")` |
| `crates/tf-cli/tests/diagnostics_field_finding_2.rs` | 86 | `.expect("spawn tftrunk")` |
| `crates/tf-cli/tests/diagnostics_field_finding_2.rs` | 33 | `tftrunk-ff2-<pid>` temp dir prefix |

**Rename impact:** Cargo auto-generates `CARGO_BIN_EXE_<binname>` from
the `[[bin]] name`. Renaming `[[bin]] name = "X"` forces these
references to `env!("CARGO_BIN_EXE_X")`. Pure mechanical sweep.

#### B.3 — USAGE banner + error messages referencing the binary

| File | Line | Current text | What it should say (post-D1) |
|---|---|---|---|
| `crates/tf-cli/src/main.rs` | 126 | `USAGE: tftrunk <COMMAND> [FLAGS]` | `USAGE: <binname> <COMMAND> [FLAGS]` |
| `crates/tf-cli/src/watch.rs` | 79 | `"cargoless {} — watching ..."` (uses `cargoless` not `tftrunk`!) | `"<binname> {} — watching ..."` |
| `crates/tf-cli/src/build.rs` | 141 | `"build requires --out <DIR>: cargoless build --watch --out <dir>."` | `"... <binname> build --watch --out <dir>."` |
| `crates/tf-cli/src/check.rs` | 541 | `"`tftrunk watch`"` (suggestion in advisory output) | `"`<binname> watch`"` |

**Active drift bug:** `watch.rs:79` says `"cargoless"` but the binary the
user installed is `tftrunk`. This is the exact drift the dogfood report
called out. Should land as part of the D1 rename, not separately.

---

### Tier C — Internal infrastructure (stable on internal name; rename optional)

These references are internal-only — namespace names, builder pod names,
PVC names. They're not user-visible and don't need to match D1; renaming
them is *optional cleanup* for consistency, not a rename requirement.

#### C.1 — Builder pod / infra

| File | Line | Reference |
|---|---|---|
| `deploy/cargoless-builder.k8s.yaml` | (whole file) | Namespace `cargoless-builder`, PVC `cargoless-cache`, Deployment `cargoless-builder` |
| `scripts/ci-gate` | (multiple) | References `cargoless-builder` namespace + pod |
| `.forgejo/workflows/ci.yml` | (varies) | May reference the repo / builder name |

**Recommendation:** **leave as-is.** Infrastructure naming is stable, and
post-D1 renaming creates churn in k8s manifest pinning, CI dispatch
configuration, agent-team mental models, and any cached PVC content
that's keyed on the namespace. The cost > benefit.

#### C.2 — Bench crates (publish=false)

| File | Reference |
|---|---|
| `bench/fixture/Cargo.toml` | `name = "cargoless-bench-fixture"` |
| `bench/harness/Cargo.toml` | `name = "cargoless-bench-harness"`, `name = "cargoless-bench"` (binary) |

These are `publish = false` non-workspace crates. **Recommendation:**
rename optional. If the bench harness ships in the repo as documentation
of methodology, the `cargoless-bench` binary name is user-visible and
should match. If it stays internal, leave as-is.

#### C.3 — Cache/temp-dir prefixes

| File | Line | Reference |
|---|---|---|
| `crates/tf-cas/src/lib.rs` | 92 | `tf-trunk-cas-<tag>-<pid>` (temp dir) |
| `crates/tf-cli/src/config.rs` | 262 | `~/.cache/cargoless/<key>` (XDG cache dir) |
| `crates/tf-cli/src/clean.rs` | 30, 80, 83, 90 | Various `cargoless` path references in clean logic |

**Recommendation:** unify under the post-D1 name. The `tf-cas/src/lib.rs`
`tf-trunk-cas-` prefix is **stale** (predates the working-name drift to
`cargoless`); should be updated regardless of D1. The XDG `~/.cache/<X>/`
dir is user-visible-but-buried; renaming it abandons cached state
similar to A.1.

---

### Tier D — Crate-level metadata (Cargo.toml descriptions)

Inconsistent across crates — three different working names appear in
three different Cargo.tomls. Mechanical cleanup.

| File | Line | Current description |
|---|---|---|
| `crates/tf-core/Cargo.toml` | 8 | "The TF-Trunk daemon: filesystem watcher, rust-analyzer wrapper..." |
| `crates/tf-proto/Cargo.toml` | 8 | "Shared contract types for the TF-Trunk daemon, build pipeline..." |
| `crates/tf-cli/Cargo.toml` | 8 | "The cargoless binary (v0 headless). Subcommands: check, watch, build, status, clean." |

**Rename impact:** these show on crates.io / docs.rs / `cargo search`
output. Inconsistency reads as "is this project even decided about its
name?" Mass-replace to the post-D1 name + a consistent template.

---

### Tier E — Version banner / BUILD_ID

| File | Line | Reference |
|---|---|---|
| `crates/tf-core/src/lib.rs` | 27 | `pub const BUILD_ID: &str = concat!("tf-trunk ", env!("CARGO_PKG_VERSION"))` |
| `crates/tf-core/src/lib.rs` | 41 | `assert!(id.starts_with("tf-trunk "))` (test) |

**Current state: stale.** `BUILD_ID` says `tf-trunk` even though the
project has drifted to `cargoless` / `tftrunk`. Should be updated
regardless of D1 (current value is the OLDEST of the four names).

---

### Tier F — Documentation references (mass-grep replaceable)

| File group | Hits |
|---|---|
| `README.md` | Multiple — title, intro, install command, table |
| `ROADMAP.md` | Multiple — title, v0 capabilities, link references |
| `CONTRIBUTING.md` | Multiple — quick-start, install reference |
| `docs/DESIGN.md` | Many — design narrative |
| `docs/EXECUTION.md` | Many — agent-team playbook |
| `docs/design/D-RELEASE.md` | Many — release pipeline narrative |
| `docs/dogfood/PHASE-2-REPORT.md` | Many — historical dogfood log (may keep some as "historical record") |
| `docs/launch/SEQUENCE.md` | Several — launch venue references |
| `docs/launch/BLOG-DRAFT.md` | Several — title, hook, install command |

**Total:** 153 references across `*.md` files (grep result above).

**Recommendation:** mass-replace `cargoless` / `tftrunk` / `tf-trunk` /
`TF-Trunk` → `<D1>` in markdown. The dogfood report's *historical*
references (e.g., "the binary `tftrunk` produced X output on date Y") may
warrant keeping the old name in a footnote-style "previously known as"
note for the record; not strictly required.

---

## Suggested rename commit shape

Once D1 picks the new name `<X>`, the rename should land as **ONE focused
commit** with the following structure:

```
chore(rename): D1 — rename cargoless / tftrunk / tf-trunk → <X>

Per D1/CWDL-12, the public product name is now <X>. This commit unifies
the four working-name variants (cargoless, tftrunk, tf-trunk, TF-Trunk)
into the single name <X>, per the inventory in
docs/launch/NAMING-DRIFT-INVENTORY.md.

Tier-by-tier:

- A.1 .cargoless/ dir name → .<X>/ — with first-run migration detector
  (logs "found legacy .cargoless/ directory; migrating state to .<X>/")
- A.2 CAS pointer scheme — KEPT as cargoless-latest-green/v1 (stable
  protocol identifier; comment added explaining the divorce from brand)
- B.1 [[bin]] name → "<X>" + binstall bin-dir
- B.2 CARGO_BIN_EXE_<X> in tests
- B.3 USAGE banner + active-drift error messages all → <X>
- C.1 internal infra (cargoless-builder pod) — LEFT AS-IS (internal
  stability > brand coherence)
- C.2 bench crates → <X>-bench-fixture / <X>-bench / <X>-bench-harness
- C.3 tf-trunk-cas- temp prefix → <X>-cas-; ~/.cache/<X>/ with first-run
  migration of legacy ~/.cache/cargoless/
- D crate-level descriptions unified template
- E BUILD_ID → "<X> " (was stale tf-trunk)
- F mass-replace across *.md (historical references in PHASE-2-REPORT
  preserved with "previously known as" footnote)

Net diff: ~XXX files / ~YYY lines. Zero behavioral changes; this is a
mechanical rename gated by D1.

Co-Authored-By: ...
```

The commit's churn is large (~150-400 lines), but the change-shape is
mechanical and reviewable as a unit. Easier to gate-and-revert than a
piecemeal rename spread across 10 commits.

---

## What's NOT in this inventory

- **GitHub repo URL** (`github.com/TriformAI/cargoless`) — this stays
  fixed per D-RELEASE §8 #8 resolution; renaming the GitHub repo would
  break every existing `cargo install --git ...` reference in the wild.
  The repo URL is decoupled from the product name.
- **Plane project name** ("CWDL") — internal tracker; unaffected by D1.
- **Internal team agent names** (`tf-trunk` team in `~/.claude/teams/`)
  — agent-coordination state; unaffected by D1.
- **Forgejo mirror URL** (`forgejo.triform.dev/triform/cargoless`) —
  internal-CI infrastructure; documented as internal-only in
  CONTRIBUTING; rename is optional.

These all stay on the working-repository-name (`cargoless` /
`triform/cargoless`); the D1 rename is *user-facing* only.

---

## Open questions for the rename-implementer

(Cannot be resolved by docs-launch-lead alone; flag for operator
during the rename commit.)

1. **First-run migration logic shape** — `.cargoless/` → `.<X>/`. Auto-rename in place? Silent coexistence with a warning? Refuse-to-start until user removes the legacy dir? Lead/operator picks behavior.
2. **CAS pointer scheme** — keep `cargoless-latest-green/v1` (option 1 above) OR introduce `<X>-latest-green/v1` with a parser-accepts-both transition (option 2)? Option 1 is recommended in this doc; operator confirms.
3. **Bench crate names** — rename or leave? Depends on whether `cargoless-bench` is user-facing documentation or internal-only.
4. **Cargo crate names on crates.io** — per D-RELEASE §4.2, this is the operator decision: (a) keep `tf-{proto,cas,core}` internal-only and publish only `<X>` as the user-facing crate, OR (b) prefix-rename all four to `<X>-{proto,cas,core,cli}`. The rename commit's `crates/*/Cargo.toml` changes depend on this.
