# Naming-drift inventory — field cross-check (Tier-2 layer)

> **Status:** companion to [`docs/launch/NAMING-DRIFT-INVENTORY.md`](NAMING-DRIFT-INVENTORY.md) (source-tier). This doc verifies every user-visible string in `tftrunk`'s actual output against the source-tier catalog, classifies each occurrence by remediation-cost class, and flags categories the source-grep missed. Author: `dogfood-lead` (2026-05-17).

---

## TL;DR

Three layers of evidence for the post-D1 rename sweep:

| Tier | Owner | What it covers | Where |
|---|---|---|---|
| **Source-grep** | `docs-launch-lead` | literal `cargoless`/`tftrunk`/`tf-trunk`/`TF-Trunk` matches across `crates/`, `*.md`, `Cargo.toml`, `scripts/`, `.forgejo/`, `deploy/` | [`NAMING-DRIFT-INVENTORY.md`](NAMING-DRIFT-INVENTORY.md) |
| **Field-output** | `dogfood-lead` (this doc) | every user-visible name occurrence across 21 `tftrunk` commands run on a real Leptos project | (this file) |
| **Together** | both | complete picture: literal source strings + dynamically-rendered field strings + categories the grep can't see | both files; cross-referenced |

The **single most evidentially compelling case** for why field-tier matters — **one line of program output** (`tftrunk watch`'s banner) contains TWO independently-rendered working-name strings, each from an unrelated source site:

```
$ tftrunk watch
>> cargoless 0.0.0 — watching /work/realapp (auto-detected: cdylib + leptos (Leptos CSR))
```

The two independently-rendered working-name strings emitted by that single line:

- **`cargoless`** — a static literal from `crates/tf-cli/src/watch.rs:79` (cataloged in Tier B.3 of the source-tier).
- **`0.0.0`** — version *without* the product-name prefix: where `BUILD_ID`'s `tf-trunk 0.0.0` would have rendered if the banner had used `BUILD_ID` (per Tier E `concat!("tf-trunk ", env!(…))`). Instead `watch.rs:80` uses `env!("CARGO_PKG_VERSION")` directly, BYPASSING `BUILD_ID` and silently dropping the `tf-trunk` prefix. **New finding — inconsistency between `--version` output and watch banner; see §gap-3.**

No amount of source-grep at the literal-string layer can catch the compound visual effect of two unrelated drift sources rendering on the same line of output. That's the field-tier's contribution.

---

## Methodology

Ran 21 commands against `tftrunk` built from current main (sha `6248f3a`, post-F12-fix) on the `dogfood-realapp` Leptos CSR project (48 files, 1324 LOC). Captured stdout+stderr per command. Categorized each user-visible name occurrence into a 5-class remediation-cost taxonomy.

**Commands run:**
- Banner/help: `tftrunk --version`, `tftrunk --help`, `tftrunk -h`, `tftrunk` (no args), `tftrunk help`
- Subcommand help: `tftrunk <sub> --help` for sub ∈ {check, watch, build, status, clean}
- Error paths: `tftrunk frobnicate`, `tftrunk check --no-such-flag`, `tftrunk check --root /nonexistent`, `tftrunk check --root /tmp/empty-dir`, `tftrunk check --root /tmp/non-wasm-rust`
- Happy paths: `tftrunk check` (green tree), `tftrunk check` (red tree), `tftrunk status` (no daemon), `tftrunk watch` (5s capture)
- Misuse paths: `tftrunk build --watch` (--out missing)
- Env-var introspection: `grep TF_` over all 21 outputs + `grep env::var(` over crates/ source

**Environment:** `cargoless-builder` k8s pod, Linux x86_64, glibc 2.36, rust 1.85.0.

---

## Remediation-cost taxonomy

Each occurrence categorized into one of **5 classes** by what's required to rename it post-D1:

| Class | Mechanism | Remediation cost | Auto-propagates from `[package].name` rename? |
|---|---|---|---|
| **compile-env** | `env!("CARGO_PKG_NAME")` or `env!("CARGO_BIN_EXE_<name>")` | zero — recompile picks it up | YES (after `[package].name` / `[[bin]].name` change) |
| **compile-concat** | `concat!("literal-name", env!(…))` | manual edit of the LITERAL portion of the concat | NO — the literal is baked in at compile time |
| **runtime-format** | `format!("… {} …", NAME_CONST)` | manual edit of `NAME_CONST` + every format site | NO |
| **runtime-Display** | `Display`/`Debug` impl embeds the literal | manual edit of impl | NO |
| **static-literal** | bare string literal in source (no interpolation) | mass-replace via `sed` over the captured occurrences | NO |

The **compile-env** class is the only "free" one — everything else is manual work.

---

## Field evidence table

Each row: command run, occurrence in output, classification, **source-site** (`file:line` of the originating literal for direct `git grep`), source-tier mapping.

| # | Command | Field occurrence (verbatim excerpt) | Class | Source-site (`git grep` anchor) | Source-tier mapping |
|---|---|---|---|---|---|
| 1 | `tftrunk --version` | `tf-trunk 0.0.0` | compile-concat | `tf-core/src/lib.rs:27` | Tier E (`BUILD_ID`) |
| 2 | `tftrunk --help` (line 1) | `tf-trunk 0.0.0` | compile-concat | `tf-core/src/lib.rs:27` | Tier E (same `BUILD_ID`) |
| 3 | `tftrunk --help` USAGE | `USAGE: tftrunk <COMMAND> [FLAGS]` | static-literal | `tf-cli/src/main.rs:126` | Tier B.3 |
| 4 | `tftrunk --help` debounce line | `… also settable via TF_DEBOUNCE_MS env)` | static-literal | `tf-cli/src/main.rs` (help-text block; literal `TF_DEBOUNCE_MS`) | NOT IN SOURCE-TIER (env var; §gap-1) |
| 5 | `tftrunk --help` footer | `Working name only — the shipping name is decision D1 (CWDL-12).` | static-literal | `tf-cli/src/main.rs` (help-text footer) | meta-acknowledgment; removed at D1, not a rename target |
| 6 | `tftrunk -h` | identical to `--help` | (same as 1–5) | (same as 1–5) | |
| 7 | `tftrunk` (no args) | identical to `--help` | (same as 1–5) | (same as 1–5) | |
| 8 | `tftrunk help` | identical to `--help` | (same as 1–5) | (same as 1–5) | |
| 9 | `tftrunk check --help` | identical to `--help` (no subcommand-specific text) | (same as 1–5) | (same as 1–5) | |
| 10 | `tftrunk watch --help` | identical to `--help` | (same as 1–5) | (same as 1–5) | |
| 11 | `tftrunk build --help` | identical to `--help` | (same as 1–5) | (same as 1–5) | |
| 12 | `tftrunk status --help` | identical to `--help` | (same as 1–5) | (same as 1–5) | |
| 13 | `tftrunk clean --help` | identical to `--help` | (same as 1–5) | (same as 1–5) | |
| 14 | `tftrunk frobnicate` (unknown cmd) | `xx unknown command: frobnicate` then USAGE block | static-literal | `tf-cli/src/main.rs:126` (USAGE re-render) | Tier B.3 |
| 15 | `tftrunk check --no-such-flag` | `xx unknown flag: --no-such-flag` then USAGE block | static-literal | `tf-cli/src/main.rs:126` (USAGE re-render) | Tier B.3 |
| 16 | `tftrunk check --root /nonexistent/...` | `xx no Cargo.toml in /nonexistent/path/xyz (and no tf.toml). run cargoless from your Rust + WASM project root, or pass --root <dir>.` | static-literal | `tf-cli/src/config.rs` (NoManifest error formatter; `git grep "run cargoless from"`) | NOT IN SOURCE-TIER (§gap-2) |
| 17 | `tftrunk check --root /tmp/empty-dir` | same as #16, root substituted | static-literal | same as #16 | same as #16 |
| 18 | `tftrunk check --root /tmp/non-wasm-rust` | `xx /tmp/.../Cargo.toml is not a recognisable Rust + WASM project. looked for a cdylib crate-type or a leptos dependency …` | static-literal | `tf-cli/src/config.rs` (NotWasmProject formatter) | (no name reference — exempt from rename) |
| 19 | `tftrunk check` (green tree) | `>> checking /work/realapp (auto-detected: cdylib + leptos (Leptos CSR))` then `ok green — every tracked file compiles …` | static-literal | `tf-cli/src/check.rs` (verdict formatter) | (no name reference — exempt) |
| 20 | `tftrunk check` (red tree, F8-fix output) | `xx red — at least one tracked file does not compile (2 errors, 0 warnings surfaced (20 rust-analyzer advisory hints suppressed; \`tftrunk watch\` shows the live stream)).` | static-literal | `tf-cli/src/check.rs:541` (references `tftrunk watch`) | Tier B.3 |
| 21 | `tftrunk status` (no daemon) | `!! no cargoless daemon for /work/realapp — start one: \`cargoless watch\` or \`cargoless build --watch --out <dir>\`.` | static-literal | `tf-cli/src/statusfile.rs` (`git grep "no cargoless daemon"`) — **2 `cargoless` occurrences in one literal** | NOT IN SOURCE-TIER (§gap-2) |
| 22 | `tftrunk build --watch` (no --out) | `xx \`build\` requires \`--out <DIR>\`: \`cargoless build --watch --out <dir>\`.` | static-literal | `tf-cli/src/build.rs:141` | Tier B.3 — **says `cargoless` not `tftrunk`** |
| 23 | `tftrunk watch` (banner line) | `>> cargoless 0.0.0 — watching /work/realapp (auto-detected: …)` | static-literal + compile-env (compound) | `tf-cli/src/watch.rs:79` (literal `cargoless`) + `tf-cli/src/watch.rs:80` (`env!("CARGO_PKG_VERSION")` for `0.0.0`) | Tier B.3 + §gap-3 |
| 24 | `tftrunk watch` (AC#1 line) | `ok verdict pipeline live in 0.07s (AC#1 budget 30s) — headless, no browser` | (no name reference — exempt) | `tf-cli/src/watch.rs` | |
| 25 | `tftrunk watch` (Ctrl-C line) | `.. Ctrl-C to stop. Streaming verdicts…` | (no name reference — exempt) | `tf-cli/src/watch.rs` | |
| 26 | `tftrunk watch` (per-file verdicts) | `>> [+   1.656s] /work/realapp/src/app.rs: Green` | (no name reference — exempt) | `tf-cli/src/watch.rs` | |
| 27 | `tftrunk watch` (per-file warnings, F8-fix output) | `warning[non_snake_case; rust-analyzer]: src/components/button.rs:6:8: Function \`Button\` should have snake_case name, e.g. \`button\`` | (no name reference — exempt; RA-sourced) | (RA output, not cargoless source) | |
| 28 | `tftrunk watch` (tree summary) | `ok [+   3.378s] GREEN — tree compiles` | (no name reference — exempt) | `tf-cli/src/watch.rs` | |

**Field surface count of name references**: 7 distinct user-visible name patterns, surfaced across 21 commands.

---

## Gaps the source-grep missed (field-tier discoveries)

### §gap-1: TF_* env var prefixes — Tier B.4 candidate

**Recommendation:** add a Tier B.4 section to the source-tier inventory covering `TF_*` env var names. These are visible to users (in `--help` and as suggested config knobs), and they're derived from the `tf-trunk` working name. If D1 renames, they need renaming too (or legacy aliases for backwards-compat).

Source-grep evidence (defined in `crates/tf-core/src/model.rs`):
```
/work/dogfood-src/crates/tf-core/src/model.rs:89:    std::env::var("TF_DEBOUNCE_MS")
/work/dogfood-src/crates/tf-core/src/model.rs:489:   let cap = std::env::var("TF_CHECK_TIMEOUT_SECS")
/work/dogfood-src/crates/tf-core/src/model.rs:619:   let cap = std::env::var("TF_CHECK_TIMEOUT_SECS")
/work/dogfood-src/crates/tf-core/src/model.rs:1232:  let prev = std::env::var("TF_DEBOUNCE_MS").ok();
```

Field-output evidence:
- `TF_DEBOUNCE_MS` — surfaces in `tftrunk --help` (and all subcommand `--help` re-renders): *"also settable via `TF_DEBOUNCE_MS` env"*. User-visible.
- `TF_CHECK_TIMEOUT_SECS` — **defined-but-not-surfaced**. Source-tree has it at two call sites; ZERO user-visible help / error / banner text mentions it. Power users who know to set it will continue to find it; everyone else won't discover it. This is a **separate documentation gap** unrelated to D1 rename: regardless of name, this env var should appear in `--help` (or a `man page` / `--help-advanced` flag).

**Renaming class:** `static-literal` — the env var name is a bare string literal passed to `env::var(…)`; there is no formatting/interpolation, so it classifies as `static-literal` (not `runtime-format`, which requires a `format!("…{}…", …)` interpolation). Remediation cost is the same (per-site manual edit at rename time), but the class matters for downstream by-class grepping: `git grep 'env::var("TF_'` finds them all.

### §gap-2: error-path strings not in source-grep's Tier B.3

Several error messages contain `cargoless` literals at file/line locations the source-grep inventory's Tier B.3 didn't enumerate explicitly. The grep would have caught them (they ARE literal `cargoless`), but they weren't called out as binary-name-reference rename targets in the Tier B.3 table. Worth promoting them:

- **`config.rs` (likely):** "run cargoless from your Rust + WASM project root, or pass --root <dir>." — surfaced in `err_empty`, `err_badroot` paths. **Should say `<binname>` post-D1.**
- **`statusfile.rs` (likely):** "no cargoless daemon for X — start one: `cargoless watch` or `cargoless build --watch --out <dir>`." — surfaced in `status_no` path. **TWO occurrences in this single message; both should rename.**

(Source-grep would find them as bare `cargoless` literals among the 417 hits; the field-tier just clarifies they're user-visible bin-name references, not docs or internal references.)

### §gap-3: BUILD_ID inconsistency between `--version` and watch banner

**Compound finding** — two source-tier locations interact to produce an inconsistency the source-grep can't see standalone:

- `tf-core/src/lib.rs:27` (Tier E): `BUILD_ID = concat!("tf-trunk ", env!("CARGO_PKG_VERSION"))` — renders as `tf-trunk 0.0.0` everywhere `BUILD_ID` is printed (`--version`, `--help` first line, error-with-usage-banner blocks).
- `tf-cli/src/watch.rs:79-80`: `format!("cargoless {} — watching …", env!("CARGO_PKG_VERSION"))` — uses `CARGO_PKG_VERSION` DIRECTLY (bypassing `BUILD_ID`); renders as `cargoless 0.0.0` (the `tf-trunk` prefix is silently dropped).

So a user running `tftrunk --version` sees `tf-trunk 0.0.0` but running `tftrunk watch` sees `cargoless 0.0.0` — **same binary, same version, two different banner-product-names**. This is the compound case the TL;DR cites.

**Rename remediation:** the right cleanup is to make watch.rs (and every other site that displays version-with-product-name) use `tf_core::BUILD_ID` instead of constructing its own banner from `env!("CARGO_PKG_VERSION")`. That gives **one source of truth** for the rendered product-name+version banner. Then the Tier E `BUILD_ID` rename single-handedly fixes every banner everywhere.

This is a **structural recommendation for the rename-implementer**: consolidate banner-rendering to `BUILD_ID`, then only one site needs renaming. Without consolidation, every banner-site renders its own product-name and must be hunted down individually.

### §gap-4: `<binname> --help` text is identical to `--help` for all subcommands

`tftrunk check --help`, `tftrunk watch --help`, etc. ALL emit the SAME global help text rather than subcommand-specific help. This isn't a naming-drift issue per se, but it's adjacent: a user wanting to know "what flags does `build` accept?" gets the global help (which mentions all subcommands generically). **Subcommand-specific `--help` doesn't exist in v0.** Out of scope for this cross-check; flagging for the rename-implementer to know they don't need to scope-handle different help-text variants.

### §gap-5: Field-tier confirms BUILD_ID is THE oldest stale string

The Tier E observation in source-tier — that `BUILD_ID = "tf-trunk ..."` is the oldest of the four working names — is **maximally user-visible** in field-tier:

- Every `--help` output's first line: `tf-trunk 0.0.0`.
- Every error-with-usage-banner output's first line: `tf-trunk 0.0.0` (#14, #15 above).
- `--version`: `tf-trunk 0.0.0`.

That's 12+ surface points where users see `tf-trunk` (not even `tftrunk`, not `cargoless`). **Tier E should be highest-rename-priority of the visible tiers** because it's both the most stale AND the most user-encountered.

---

## Out-of-scope for field-tier (acknowledged)

Per docs-launch-lead's flag in our coordination thread, these categories require source-tier (already covered) or external-surface inspection:

- **Tier A** (`.cargoless/` directory + CAS pointer scheme) — these are on-disk contracts, not user-visible CLI output. Not in field-tier scope.
- **Tier C** (builder pod / namespace / bench crate names) — internal infrastructure, not user-visible. Not in field-tier scope.
- **Tier D** (`Cargo.toml [package].description` text) — visible on crates.io / docs.rs / `cargo search` output, not in CLI. Field-tier can't see these.
- **Tier F** (markdown docs) — source-tier covers these completely; no field-tier additions needed.

---

## Cross-tier rename checklist (suggestion for the rename-implementer)

When operator decides D1 → `<X>`, this is what the rename commit needs to touch, in order of cascading effect:

1. **Choose `<X>` and update `Cargo.toml [package].name`** for `tf-cli` (and possibly `tf-core`/`tf-proto`/`tf-cas` if those get renamed too).
2. **Update `[[bin]] name = "<X>"`** in `tf-cli/Cargo.toml`. This auto-propagates `env!("CARGO_BIN_EXE_<X>")` test references (Tier B.2).
3. **Update Tier E `BUILD_ID`** in `tf-core/src/lib.rs:27` to `concat!("<X> ", env!("CARGO_PKG_VERSION"))`. Then EITHER consolidate watch banner to use `BUILD_ID` (per §gap-3) OR update `watch.rs:79` literal independently.
4. **Mass-replace `cargoless` / `tftrunk` literals** per source-tier Tier B.3 and the §gap-2 additions (config.rs error formatter, statusfile.rs error formatter). The §gap-3 compound-drift case folds in here.
5. **TF_* env vars**: decide rename strategy (rename vs alias-legacy). Per §gap-1, update at the 4 source sites.
6. **Tier D** Cargo.toml descriptions — mass-replace for consistency.
7. **Tier F** docs — mass-replace.
8. **Tier A** `.cargoless/` dir — migration-path decision per source-tier's recommendation.
9. **Tier C** infra — recommendation: leave as-is (per source-tier).

After commit: re-run this cross-check field-tier sweep against the renamed binary. Expected: ZERO occurrences of the old name in any of the 21 captured commands. That's the verification gate.

---

## Companion files

- **Source-tier:** [`docs/launch/NAMING-DRIFT-INVENTORY.md`](NAMING-DRIFT-INVENTORY.md) (source-grep tier; 417/99/6/5 literal hits across crates + docs)
- **Field-tier (this file):** `docs/launch/NAMING-DRIFT-FIELD-CROSS-CHECK.md`
- **Generative dogfood report referencing both:** [`docs/dogfood/PHASE-2-REPORT.md`](../dogfood/PHASE-2-REPORT.md) — the original Phase 2 dogfood that flagged the drift as a D1 launch-prerequisite

**Reporter:** `dogfood-lead` (Claude Opus 4.7, 1M context, agent role)
**Tested against:** `origin/main` @ `6248f3a` (post-F12-fix, post-F8-redo, post-RA-config not-yet-landed)
**Generated:** 2026-05-17
