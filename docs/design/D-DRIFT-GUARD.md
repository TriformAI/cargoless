# D-DRIFT-GUARD — the D1 naming-drift CI invariant (#96)

**Status:** SPEC, ready to wire. Authored by `docs-launch-lead` on
`agent/docs-launch-lead-prep` (staging — NOT merged until the
operator's launch-scope decision + #97 land).

**Implementation owner:** `builder-infra` (#96 — wiring the grep into
`scripts/ci-gate` + committing the live allowlist file is its lane).
This document is the *spec + the exact allowlist content*, ground-
truthed against landed `origin/main` @ `04c7de5`. builder-infra should
be able to drop the §4 block in verbatim.

**Why this exists.** The D1 source-tier rename (#87/#99) is complete
(`git grep tftrunk -- crates/**/*.rs` == 0). But the rename arc proved
empirically — four separate times — that a *parallel* branch lands new
`tftrunk`/`tf-cli` literals while a rename is in flight (lsp.rs:233,
orphan.rs, build.rs, watch.rs, statusfile.rs each reintroduced drift
after a "clean" grep). D1-completeness is not a one-time state; it is
an **invariant that decays unless enforced**. #96 makes the invariant
a CI gate: a new un-allowlisted placeholder literal fails the build.

---

## 1. The scope boundary (the load-bearing decision)

A naive `grep -r 'tf-cli'` is **unimplementable** — it fails CI on
dozens of *correct* references. The guard must separate three things
that look alike to a substring match:

| Token class | Example | Drift? | Guard action |
|---|---|---|---|
| **D1 placeholder name** | `name = "tf-cli"`, `-p tf-cli`, `publish-tf-cli`, `tftrunk watch` in prose/commands, `tftrunk`/`tf-trunk` binary literal | **YES** — must be `cargoless` | **FAIL** unless allowlisted |
| **Directory path** | `crates/tf-cli/`, `crates/tf-cli/Cargo.toml` | NO — the dir genuinely *is* `crates/tf-cli/` until #97 | **IGNORE** (path, not name) |
| **Internal lib crate** | `tf-proto`, `tf-cas`, `tf-core` | NO in v0 — intentionally retained until #97 | **OUT OF SCOPE** for #96 |

The guard therefore matches the **name-in-name-position**, never the
bare substring:

- `tftrunk` and `tf-trunk` — always drift in the D1 surface (no
  legitimate non-historical use); match the bare token.
- `tf-cli` — match only as a *crate/package identifier*:
  `\bname\s*=\s*"tf-cli"`, `-p\s+tf-cli`, `publish-tf-cli`,
  `\btf-cli\b` **not** immediately preceded by `crates/` and **not**
  followed by `/`. The `crates/tf-cli/` directory path is explicitly
  *not* drift and is excluded by construction, not by allowlist
  (otherwise the allowlist would be ~20 ever-churning path lines).

**In scope (the D1-completeness surface):** `crates/**/*.rs`,
`crates/**/Cargo.toml`, root `Cargo.toml`, `.github/workflows/*.yml`,
`scripts/ci-gate`, and forward-facing user docs (`README.md`,
`ROADMAP.md`, `CONTRIBUTING.md`, `docs/launch/BLOG-DRAFT.md`).

**Out of scope (never grepped):** `Cargo.lock` (mechanical), the
naming-drift catalogs whose *purpose* is to enumerate the literals
(`docs/launch/NAMING-DRIFT-INVENTORY.md`,
`docs/launch/NAMING-DRIFT-FIELD-CROSS-CHECK.md`,
`docs/launch/D1-NAME-RECON.md`), and the immutable evidence/decision
record (`docs/dogfood/PHASE-2-REPORT.md`, `docs/design/D-RELEASE.md`,
`docs/release/PHASE-D-OPERATOR-HANDOFF.md`). Rewriting a field-finding
transcript to say `cargoless` would *falsify the evidence trail* —
these files record what the binary was literally called during
dogfooding. They are excluded by path, not allowlisted line-by-line.

---

## 2. The mechanism (builder-infra wires this into `scripts/ci-gate`)

A new gate step, after the existing checks, run **from the streamed
worktree** (same gate-self-reference discipline as the rest of
ci-gate):

```sh
# D1 drift-guard (#96). Fails iff an un-allowlisted placeholder
# name-token appears in the D1-completeness surface.
d1_surface() {
  git -C "$REMOTE_SRC" ls-files \
    'crates/*.rs' 'crates/*/Cargo.toml' 'Cargo.toml' \
    '.github/workflows/*.yml' 'scripts/ci-gate' \
    'README.md' 'ROADMAP.md' 'CONTRIBUTING.md' \
    'docs/launch/BLOG-DRAFT.md'
}
# name-in-name-position patterns (NOT bare substring):
PAT='tftrunk|tf-trunk|(\bname[[:space:]]*=[[:space:]]*"tf-cli")|(-p[[:space:]]+tf-cli\b)|(publish-tf-cli\b)'
hits=$(d1_surface | xargs grep -nED "$PAT" 2>/dev/null \
        | grep -vFf scripts/.d1-drift-allowlist || true)
if [ -n "$hits" ]; then
  echo "[ci-gate] D1 drift-guard FAIL — un-allowlisted placeholder literal:"
  echo "$hits"
  exit 1
fi
```

`scripts/.d1-drift-allowlist` is a committed plain-text file of
`grep -vFf` fixed-string suppressors — one `path:linenum:` *or* a
stable substring of the offending line per entry. Fixed-string (`-F`)
match keeps the allowlist legible and avoids regex-escaping churn; the
trade-off (a line-number shift silently un-suppresses) is *desirable*
— it forces re-review when an intentional-residual file is edited.

> **Pattern caveat for builder-infra:** `grep -D` / `-P` portability
> differs between BusyBox and GNU grep on the builder pod. If `-D` is
> unavailable, use `grep -nE` with the same alternation; the `tf-cli`
> name-position sub-patterns are already `-E`-safe. Validate the
> pattern matches §4's known residuals and *nothing in* `crates/tf-cli/`
> path references before committing the wired gate.

---

## 3. Allowlist entries — categorized by disposition

Every entry below was confirmed present in landed `origin/main` @
`04c7de5`. Disposition codes:

- **PERMANENT-LOADBEARING** — the literal *is the test*; "fixing" it
  breaks a regression guard. Never remove.
- **PERMANENT-NARRATION** — deliberate past-tense "was X, now
  cargoless" provenance comment; removing it erases the D1 audit
  trail. Never auto-rewrite.
- **STALE-DEFERRED** — genuinely wants to become `cargoless`
  eventually, but the rename is owned by a *different* lane (#97 or a
  cross-owner docs pass) and is not blocking v0. Allowlisted *with a
  tracked disposition* so it cannot silently regress while also not
  blocking launch.

### 3a. PERMANENT-LOADBEARING — `crates/tf-core/src/lib.rs` (#89)

`crates/tf-core/src/lib.rs:31,45,46,84` — `tf-trunk` inside the #89
`BUILD_ID` rationale doc-comments (explains *why* `tf` / `tf-trunk`
were rejected: Terraform collision per CLAUDE.md).
`crates/tf-core/src/lib.rs:88` — `!id.contains("tf-trunk")` — the #89
**regression-guard assertion itself**.
`crates/tf-core/src/lib.rs:89` — `"the stale tf-trunk placeholder must
not survive #89: got {id:?}"` — that assertion's failure message.

> Editing any of these to say `cargoless` either deletes the
> regression guard or makes its message a lie. This is the canonical
> PERMANENT-LOADBEARING case.

`crates/tf-cli/src/watch.rs:81` — `tf-trunk` in the #89 rationale
comment (`// was the divergent site — --version said "tf-trunk {ver}"`).
PERMANENT-LOADBEARING (documents the exact bug #89 closed).

### 3b. PERMANENT-NARRATION — manifest + release D1 provenance

- `crates/tf-cli/Cargo.toml:50` — `# D1 RESOLVED = cargoless (was placeholder tftrunk).`
- `crates/tf-cli/Cargo.toml:99` — `# cargoless ← the binary (D1 RESOLVED = cargoless, was tftrunk)`
- `.github/workflows/release.yml:224` — `# D1 RESOLVED 2026-05-17 = cargoless (was placeholder tftrunk).`
- `.github/workflows/release.yml:247` — `# … tftrunk pre-D1), so PKG == BIN == "cargoless" here;`
- `.github/workflows/release.yml:288` — `# cargoless, was tf-cli/tftrunk pre-D1):`
- `.github/workflows/release.yml:391` — `publish-cargoless:   # D1 RESOLVED: was publish-tf-cli (crate renamed tf-cli→cargoless)`
- `docs/launch/BLOG-DRAFT.md:474` — AC#9 reviewer-checklist line recording that `tftrunk`/`tf-cli` drift *was renamed* in #87.

> These are the D1 audit trail. A future reader needs "this used to be
> `tftrunk`" to understand the rename; auto-rewriting them to
> `cargoless` makes the comments tautological and erases provenance.

### 3c. STALE-DEFERRED — owned by another lane, tracked not blocked

- `docs/design/D-A2-RENEGOTIATION.md:205,215,335,346` — `tftrunk
  check` / `tftrunk watch` / R6 `tftrunk`-owned. Stale forward-doc
  prose in dev-fixer's authored engineering-authority doc.
  **Disposition:** rename to `cargoless` in a future docs pass *by the
  doc's owner*; cross-owner edit, not a #96 blocker. Allowlisted.
- `docs/design/D-A2-RENEGOTIATION.md:463` — `tf-cli/src/watch.rs::stamp`
  cross-reference. Becomes correct again post-#97 (dir rename);
  STALE-DEFERRED to #97.
- `crates/tf-core/src/build.rs:335` — `// — tf-cli never reaches into
  CAS internals …`. Internal-crate-name reference; correct *as a crate
  identity* until #97 renames the crate. STALE-DEFERRED to #97.
- `crates/tf-core/src/lib.rs:33` — `// straight off CARGO_PKG_VERSION
  in tf-cli.` #89 rationale referencing the crate by its current
  identity. STALE-DEFERRED to #97 (rename in lockstep with the crate).
- `.gitignore:6` — `.tf-trunk-cache/`. **Genuinely dead**: tf-cas now
  uses the `cargoless-cas-` temp prefix (#87 @ `lib.rs:92`), so this
  ignore pattern matches nothing. Harmless but stale. **Disposition:**
  one-line delete is correct, but it is a `.gitignore` cleanup, *not*
  part of the scope-invariant launch-docs or the allowlist-spec
  commit. Allowlisted as STALE-DEFERRED with an explicit "safe to
  delete in any cleanup commit" note so it does not block the gate
  meanwhile. (`.gitignore` is outside the §1 surface anyway; listed
  here for completeness so it is not "discovered" as a surprise later.)

---

## 4. The committed allowlist file (drop in verbatim)

builder-infra: commit this as `scripts/.d1-drift-allowlist` in the #96
wiring commit. Fixed-string (`grep -vFf`) entries; keep the comments —
they are the disposition record and cost nothing at match time
(comment lines never match a real grep hit).

```
# scripts/.d1-drift-allowlist — D1 naming-drift guard suppressors (#96).
# Spec + rationale: docs/design/D-DRIFT-GUARD.md. Ground-truth: origin/main @ 04c7de5.
# Format: a fixed substring of an intentional-residual line. grep -vFf.
# Disposition per entry: LOADBEARING | NARRATION | STALE-#97 | STALE-DOCS.
#
# --- 3a PERMANENT-LOADBEARING (#89 BUILD_ID regression guard) ---
crates/tf-core/src/lib.rs:31:
crates/tf-core/src/lib.rs:45:
crates/tf-core/src/lib.rs:46:
crates/tf-core/src/lib.rs:84:
crates/tf-core/src/lib.rs:88:
crates/tf-core/src/lib.rs:89:
crates/tf-cli/src/watch.rs:81:
# --- 3b PERMANENT-NARRATION (D1 provenance / audit trail) ---
crates/tf-cli/Cargo.toml:50:
crates/tf-cli/Cargo.toml:99:
.github/workflows/release.yml:224:
.github/workflows/release.yml:247:
.github/workflows/release.yml:288:
.github/workflows/release.yml:391:
docs/launch/BLOG-DRAFT.md:474:
# --- 3c STALE-DEFERRED (tracked; owned by #97 or a cross-owner docs pass) ---
docs/design/D-A2-RENEGOTIATION.md:205:   # STALE-DOCS: dev-fixer-owned doc
docs/design/D-A2-RENEGOTIATION.md:215:   # STALE-DOCS
docs/design/D-A2-RENEGOTIATION.md:335:   # STALE-DOCS
docs/design/D-A2-RENEGOTIATION.md:346:   # STALE-DOCS
docs/design/D-A2-RENEGOTIATION.md:463:   # STALE-#97
crates/tf-core/src/build.rs:335:        # STALE-#97 internal-crate ref
crates/tf-core/src/lib.rs:33:           # STALE-#97 internal-crate ref
# .gitignore:6 (.tf-trunk-cache/) — dead pattern; outside §1 surface; safe to
# delete in any cleanup commit; listed for completeness, not grepped.
```

> Line-number anchoring is deliberate: it is precise *and* it
> self-invalidates when an intentional-residual file is edited,
> forcing a human to re-confirm the residual is still intentional.
> If churn becomes painful, builder-infra may switch a specific entry
> to a stable line-substring — but default to line anchors; the
> re-review friction is the feature.

---

## 5. The #97 fold (builder-infra, when #97 lands)

#97 renames the internal crates `tf-{proto,cas,core}` →
`cargoless-{proto,cas,core}` and the directory `crates/tf-cli/` →
its #97-decided path. When that lands, builder-infra:

1. **Deletes** every `STALE-#97` allowlist entry (those literals
   become `cargoless-*` and must then be enforced, not suppressed).
2. **Removes** the §1 "directory path `crates/tf-cli/` is excluded by
   construction" carve-out and the `crates/tf-cli/` path exclusion in
   the pattern — post-#97 there is no legitimate `tf-cli` path.
3. **Extends** the guard pattern to also fail on `\btf-proto\b`,
   `\btf-cas\b`, `\btf-core\b` *as crate identifiers* (same
   name-in-name-position discipline as §1's `tf-cli` rule).
4. Re-runs the gate from the worktree; the PERMANENT-LOADBEARING and
   PERMANENT-NARRATION entries (which intentionally still say
   `tf-trunk`/`tftrunk` as historical record) stay.

The #97 delta is **builder-infra's to fold** — flagged here, not done
here, to keep this a single-concern spec commit and respect crate/file
ownership (`scripts/ci-gate` is builder-infra's; `D-A2-RENEGOTIATION.md`
is dev-fixer's).

---

## 6. Cross-reference

- #87/#99 — the D1 source-tier rename this guard protects.
- #96 — the implementation task (builder-infra) this spec feeds.
- #97 — the internal-crate + dir rename; §5 is its fold contract.
- `docs/launch/NAMING-DRIFT-INVENTORY.md` /
  `NAMING-DRIFT-FIELD-CROSS-CHECK.md` — the source-tier + field-tier
  catalogs this guard operationalizes (RESOLVED; this is their
  enforcement arm).
- EXECUTION.md self-gate checklist (#94) — the drift-guard is the
  *automated* counterpart to the manual rustfmt/grep self-gate; both
  exist because "clean now" decays without a gate.
