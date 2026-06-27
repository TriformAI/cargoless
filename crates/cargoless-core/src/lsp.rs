//! Minimal LSP client over rust-analyzer's stdio (Epic 2 / CWDL #4, #21).
//!
//! Scope is exactly what the green/red model needs and no more:
//! `initialize`/`initialized`, `textDocument/didOpen|didChange|didSave`,
//! consuming `textDocument/publishDiagnostics`, and observing flycheck
//! progress. This is **not** a general LSP library — RA-specific, v0-shaped,
//! single workspace.
//!
//! ## #21 verdict-provenance (load-bearing for v0)
//!
//! rust-analyzer's *native* analysis is BLIND to the type/trait/method/macro
//! error class (e.g. E0599) — only `cargo check` (RA's *flycheck*) produces
//! it. So a diagnostic's authority depends on WHO produced it: RA tags
//! flycheck/cargo-check diagnostics with `source: "rustc"`, native ones with
//! `source: "rust-analyzer"`. [`PublishDiagnostics`] therefore splits error
//! counts into **authoritative** (rustc/cargo-check) vs **advisory**
//! (native). The authoritative *tree* verdict is only trustworthy at a
//! flycheck-pass boundary, so we also surface [`LspEvent::FlycheckEnded`]
//! from `$/progress`. The model gates GREEN on the authoritative tier; the
//! mapping/edge logic lives in `model`, not here.
//!
//! ## Testability
//!
//! Framing, diagnostics classification, and flycheck-end detection are pure
//! functions unit-tested over in-memory buffers — the CI `test` job (no
//! rust-analyzer in the image) exercises every parsing branch. The live
//! [`LspClient`] is generic over `Read`/`Write`, so the handshake is testable
//! against a scripted fake server too.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use cargoless_proto::{Diagnostic, Severity};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// RA weight-shedding initializationOptions (#74)
//
// rust-analyzer is configurable via LSP `initializationOptions`. By default
// it does FAR more work than cargoless ever consumes: inlay hints, hover
// actions, code lenses, cache priming, completion snippets, full-features
// cargo check, all-targets cargo check, parallel-per-workspace-member
// flycheck, etc. We use the publishDiagnostics stream and the
// `$/progress` cargo-check end signal — full stop. Everything else is
// resource waste on a tool whose pitch is throughput.
//
// `InitOpts` is the small shape the CLI threads through (proc-macro
// enable/disable per-project, cargo features list). `lean_init_options()`
// is the pure JSON builder unit-tested against the resulting nested key
// shape; `detect_proc_macro()` is the pure Cargo.toml-scan implementing
// the "auto" knob (default).
//
// Option B+ refinement (#74 lead clarification post-#48 design catch):
// `checkOnSave` STAYS ENABLED. The F8-redo verdict architecture
// (model::Model::authoritative_tree, ff1feaf) gates GREEN on flycheck
// completion; disabling checkOnSave would break the verdict path. What
// we DO soften is checkOnSave's subsettings: allTargets=false (lib+bin
// only, skip tests/benches/examples); invocationStrategy=once +
// invocationLocation=workspace (one cargo subprocess per workspace, not
// per workspace). Combined with the other 4 settings + honorable mentions
// the expected RA resource reduction is 30-50%.
// ---------------------------------------------------------------------------

/// CLI/env-resolved options threaded into [`LspClient::initialize`].
/// Constructed via [`InitOpts::from_env_and_project`] (mirrors the
/// `TF_DEBOUNCE_MS` pattern from #49); tests construct directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOpts {
    /// Per-project resolved proc-macro server enable. Drives
    /// `procMacro.enable` in the lean init options.
    pub proc_macro_enabled: bool,
    /// Cargo features to enable for RA's cargo-check invocation. Drives
    /// `cargo.features` in the lean init options. Empty = RA picks default.
    pub features: Vec<String>,
    /// Cargo package selector for RA's flycheck path. Mirrors
    /// `cargo check -p <pkg>` from tf-multiverse's `scripts/check-remote`.
    pub package: Option<String>,
    /// Cargo compilation target for RA's flycheck path. Drives
    /// `cargo.target` / `check.targets` instead of hand-adding a duplicate
    /// `--target` extra arg.
    pub target: Option<String>,
    /// Whether RA should pass `--no-default-features` to cargo.
    pub no_default_features: bool,
    /// Whether RA should pass `--release` to cargo check.
    pub release: bool,
    /// Whether RA should run its own cargo flycheck. Remote-push
    /// deployments can set `TF_RA_CHECK_DISABLED=1` and let the per-push
    /// direct Cargo check own the fresh green/red boundary.
    pub cargo_check_enabled: bool,
    /// Whether RA should activate **all** cargo features
    /// (`cargo.allFeatures` / `--all-features`). Drives the `allFeatures`
    /// init-option keys. Default `false` (cargo's own default-feature
    /// behavior). On a mutually-exclusive-feature workspace (a Leptos SSR
    /// app's `ssr`/`hydrate`/`csr`) this is the WRONG knob — prefer an
    /// explicit `features` list there; `all_features` suits simple/CSR
    /// crates whose features compose.
    pub all_features: bool,
    /// Extra cfg options for RA to treat as active
    /// (`rust-analyzer.cargo.cfgs`). Each entry is `"key"`, `"key=value"`,
    /// or `"!key"` to disable, per RA's schema. These are **appended onto
    /// RA's own defaults** (`debug_assertions`, `miri`) rather than
    /// replacing them: `cargo.cfgs` has replace semantics in RA, so emitting
    /// a bare set would silently drop `debug_assertions` and flip every
    /// `#[cfg(debug_assertions)]` item in the analyzed tree. Default empty ⇒
    /// RA's defaults are emitted unchanged. The `erase_components` Leptos
    /// 0.7+ dev cfg is the motivating case.
    pub cfgs: Vec<String>,
}

/// RA's built-in `rust-analyzer.cargo.cfgs` default. `cargo.cfgs` REPLACES
/// (not merges) RA's defaults, so any operator cfgs must be appended onto
/// this base or `debug_assertions`/`miri` silently vanish from the analyzed
/// tree. Verified against `rust-analyzer --print-config-schema` (1.94.1;
/// same schema family as the deployed 1.93.1).
const RA_DEFAULT_CFGS: &[&str] = &["debug_assertions", "miri"];

impl Default for InitOpts {
    /// Safe defaults: proc-macros ON (most projects need them — the only
    /// non-proc-macro-using projects are rare; defaulting OFF would
    /// silently mis-analyze macro-heavy code), `features = []` (empty
    /// list ⇒ RA invokes cargo WITHOUT `--features`, letting cargo
    /// use its own default-feature behavior).
    ///
    /// **Why empty, not `["default"]`:** passing `--features default`
    /// to cargo errors out on crates that don't define a `[features]`
    /// table (the fixture in the F2 integration test is exactly this
    /// shape — caught by self-gate). Empty list = "no override" =
    /// safe across every cargo project. The CLI's `--features csr`
    /// flag becomes a real opt-in override on top.
    fn default() -> Self {
        Self {
            proc_macro_enabled: true,
            features: Vec::new(),
            package: None,
            target: None,
            no_default_features: false,
            release: false,
            cargo_check_enabled: true,
            all_features: false,
            cfgs: Vec::new(),
        }
    }
}

impl InitOpts {
    /// Read the CLI-exported env vars, resolving `TF_PROC_MACRO=auto`
    /// via a Cargo.toml scan at `project_root`. The CLI's cargo-shaped
    /// check flags set these env vars before invoking the daemon path
    /// (same pattern as `TF_DEBOUNCE_MS` / `--debounce-ms` from #49 —
    /// keeps `LspClient::initialize`'s signature stable across callers
    /// that don't care about env).
    pub fn from_env_and_project(project_root: &Path) -> Self {
        // #126 Tier-3: `TF_RA_PROCMACRO_OFF=1` forces RA proc-macro OFF
        // (the −53 % RSS lever) regardless of TF_PROC_MACRO / auto-
        // detect. SAFE only because #126 simultaneously down-ranks
        // RA-native diagnostics out of the verdict in the model (see
        // crate::procmacro / D-PROCMACRO-DOWNRANK) — the two are a pair.
        // Default-off: unset ⇒ the #74 resolution below is byte-
        // identical to pre-#126.
        let proc_macro_enabled = if crate::procmacro::enabled() {
            false
        } else {
            match std::env::var("TF_PROC_MACRO")
                .ok()
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("enabled") | Some("on") | Some("true") | Some("1") => true,
                Some("disabled") | Some("off") | Some("false") | Some("0") => false,
                // "auto" or unset → Cargo.toml scan. False if the scan
                // errors (defensive: avoid mis-enabling RA's heavy
                // proc-macro server on a tree we can't read).
                _ => detect_proc_macro(project_root).unwrap_or(false),
            }
        };
        // Unset / parse-failure ⇒ empty list (let cargo use its own
        // defaults). See InitOpts::default() rationale on why we do NOT
        // default to `["default"]`.
        let features = csv_list_env("TF_FEATURES");
        let package = nonempty_env("TF_CHECK_PACKAGE");
        let target = nonempty_env("TF_CHECK_TARGET");
        let no_default_features = truthy_env("TF_CHECK_NO_DEFAULT_FEATURES");
        let release = truthy_env("TF_CHECK_RELEASE");
        let cargo_check_enabled = !truthy_env("TF_RA_CHECK_DISABLED");
        let all_features = truthy_env("TF_ALL_FEATURES");
        // Extra RA cfgs (e.g. `erase_components`), comma/space-separated.
        // Same split as TF_FEATURES; appended onto RA's default cfgs in
        // `lean_init_options`.
        let cfgs = csv_list_env("TF_RA_CFGS");
        Self {
            proc_macro_enabled,
            features,
            package,
            target,
            no_default_features,
            release,
            cargo_check_enabled,
            all_features,
            cfgs,
        }
    }

    fn check_extra_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.release {
            args.push("--release".to_string());
        }
        args
    }

    fn check_override_command(&self) -> Option<Vec<String>> {
        let package = normalize_check_package_name(self.package.as_ref()?);
        let mut cmd = vec![
            "cargo".to_string(),
            "check".to_string(),
            "-p".to_string(),
            package,
            "--message-format=json".to_string(),
        ];
        if let Some(target) = &self.target {
            cmd.push("--target".to_string());
            cmd.push(target.clone());
        }
        if self.no_default_features {
            cmd.push("--no-default-features".to_string());
        }
        if !self.features.is_empty() {
            cmd.push("--features".to_string());
            cmd.push(self.features.join(","));
        }
        if self.release {
            cmd.push("--release".to_string());
        }
        Some(cmd)
    }
}

pub fn normalize_check_package_name(package: &str) -> String {
    match package {
        "physics" => "triform-physics",
        "portal" => "triform-portal",
        "server" => "triform-server",
        "alchemy" => "triform-alchemy",
        "isolator" | "isolation" => "isolation-executor",
        "runtime-types" => "triform-runtime-types",
        "widget" => "triform-widget",
        "exposure-map" | "exposure" => "triform-exposure",
        "cli" => "triform-cli",
        "mcp" => "triform-mcp",
        "sdk" => "triform-sdk",
        "tool-dispatch" => "triform-tool-dispatch",
        "codegen" => "triform-codegen",
        other => other,
    }
    .to_string()
}

fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse a comma/space-separated env var into a list, trimming entries and
/// dropping empties (order preserved, no dedup). Unset / empty ⇒ `vec![]`.
/// Shared by `TF_FEATURES` and `TF_RA_CFGS` so both honor the same `"a,b c"`
/// operator ergonomics.
fn csv_list_env(key: &str) -> Vec<String> {
    std::env::var(key)
        .ok()
        .map(|s| {
            s.split([',', ' '])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn truthy_env(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// FIELD FINDING #74: "auto" proc-macro detector — scan `<root>/Cargo.toml`
/// for any of the well-known proc-macro-bearing crate names (direct deps,
/// dev-deps, build-deps, or workspace members) PLUS `[lib] proc-macro =
/// true` (this crate itself emits proc macros).
///
/// Pure / shell-out-free: just `fs::read_to_string` + substring/heuristic
/// matching. Returns `Ok(true)` if any signal hits, `Ok(false)` otherwise,
/// `Err` only if Cargo.toml is absent or unreadable (caller defaults to
/// false in that case — see [`InitOpts::from_env_and_project`]).
pub fn detect_proc_macro(project_root: &Path) -> io::Result<bool> {
    let cargo_toml = project_root.join("Cargo.toml");
    let text = std::fs::read_to_string(&cargo_toml)?;
    Ok(cargo_toml_signals_proc_macro(&text))
}

/// Inner pure function — operates on Cargo.toml text. Pulled out so
/// unit tests don't need a filesystem.
pub(crate) fn cargo_toml_signals_proc_macro(text: &str) -> bool {
    // Signal 1: [lib] proc-macro = true (this crate defines proc macros).
    let mut in_lib_section = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_lib_section = name.trim() == "lib";
            continue;
        }
        if in_lib_section && line.starts_with("proc-macro") && line.contains("true") {
            return true;
        }
    }
    // Signal 2: depends on a well-known proc-macro-bearing crate. The list
    // is the lead's spec + the syn/quote/proc-macro2 trio that signals
    // heavy proc-macro use even when the obvious culprits aren't direct
    // deps. Per-line substring match on the dep-name token at the start
    // of a key=…/key.* line (no full TOML parser needed — the false-
    // positive cost on a name collision is acceptable, the false-negative
    // cost on a real proc-macro project is "wrong RA state, harder to
    // debug").
    const KNOWN_PROC_MACRO_DEPS: &[&str] = &[
        "leptos",
        "serde_derive",
        "tokio-macros",
        "async-trait",
        "derive_more",
        "thiserror",
        "syn",
        "quote",
        "proc-macro2",
    ];
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        // Only check inside [*dependencies*] sections. A bare name match
        // OUTSIDE a deps section would false-positive on `name = "syn"`
        // in [package].
        if !section.contains("dependencies") {
            continue;
        }
        let key = line.split(['=', '.']).next().unwrap_or("").trim();
        if KNOWN_PROC_MACRO_DEPS.contains(&key) {
            return true;
        }
    }
    false
}

/// #112-B Tier-2 — RA salsa LRU cap. Default is 64, deliberately half
/// of rust-analyzer's built-in default of 128: a RAM-vs-recompute trade
/// tuned for cargoless's batchy agent-edit access pattern.
/// Correctness-neutral — LRU eviction triggers recompute that yields the
/// identical query result, so this can never change a diagnostic or the
/// verdict, only latency/CPU (a non-issue between agent batches).
/// `TF_RA_LRU_CAP` overrides it for bench-lead's RSS/recompute sweep;
/// the value is clamped to a floor of 16 so a fat-finger setting cannot
/// drive pathological thrash, and a non-numeric setting falls back to
/// the default rather than erroring.
fn ra_lru_capacity() -> u32 {
    const DEFAULT: u32 = 64;
    const FLOOR: u32 = 16;
    match std::env::var("TF_RA_LRU_CAP") {
        Ok(v) => v.parse::<u32>().map(|n| n.max(FLOOR)).unwrap_or(DEFAULT),
        Err(_) => DEFAULT,
    }
}

/// Build the full lean `initializationOptions` JSON for RA. Pure: takes
/// the resolved [`InitOpts`], emits the JSON value the LSP `initialize`
/// `params.initializationOptions` field carries.
///
/// Settings (post-Option B+ refinement on #1; see module-doc comment):
///   1. checkOnSave: enabled (load-bearing for #21/F8-redo verdict),
///      subsettings softened (allTargets/invocationStrategy/Location/
///      features) for ~15-30% checkOnSave-cost reduction without
///      breaking the verdict path.
///   2. inlayHints.*: ALL DISABLED (cargoless never displays them).
///   3. cachePriming.enable: false (skip eager startup analysis).
///   4. procMacro.enable: from InitOpts.proc_macro_enabled (auto-detected
///      from Cargo.toml or explicit per `cargoless.proc-macro` knob).
///   5. cargo.allTargets: false; allFeatures + features + cfgs: from
///      InitOpts (operator knobs `--all-features`/`--cfg`, default off/empty)
///      + workspace symbol narrowed (only_types, workspace scope).
///   6. package-scoped profiles set `check.overrideCommand` with the
///      normalized Cargo package ID so tf-multiverse shorthands
///      (`-p alchemy`) run the same package as `scripts/check-remote`
///      (`-p triform-alchemy`).
///   + Honorable mentions: hover.actions, lens.enable, completion.
///      snippets.custom, assist.expressionFillDefault, references.
///      excludeImports — all idle-cost reductions on signals cargoless
///      doesn't consume.
pub fn lean_init_options(opts: &InitOpts) -> Value {
    let check_override_command = opts
        .cargo_check_enabled
        .then(|| opts.check_override_command())
        .flatten();
    let check_extra_args = if !opts.cargo_check_enabled || check_override_command.is_some() {
        Vec::new()
    } else {
        opts.check_extra_args()
    };
    let check_workspace = opts.cargo_check_enabled && opts.package.is_none();
    // For a package-scoped remote-check profile, RA's cargo flycheck is
    // narrowed through `check.overrideCommand` with the normalized Cargo
    // package name. That preserves cargoless's cargo-shaped operator
    // shorthand while avoiding a workspace-wide flycheck. RA's separate
    // build-script/proc-macro preflight otherwise starts from `cargo check
    // --workspace ...`, which is exactly the tf-multiverse fan-out path
    // this mode is replacing.
    let cargo_build_scripts_enabled = opts.cargo_check_enabled && opts.package.is_none();
    let proc_macro_enabled = opts.proc_macro_enabled && cargo_build_scripts_enabled;
    // `rust-analyzer.cargo.cfgs` REPLACES RA's defaults rather than merging,
    // so append operator cfgs onto RA's base (`debug_assertions`, `miri`)
    // instead of emitting a bare set — otherwise every `#[cfg(debug_assertions)]`
    // item in the analyzed tree silently flips. With no operator cfgs this is
    // exactly RA's default, so the key is a no-op. (Verified against
    // `rust-analyzer --print-config-schema`.)
    let cargo_cfgs: Vec<String> = RA_DEFAULT_CFGS
        .iter()
        .map(|s| (*s).to_owned())
        .chain(opts.cfgs.iter().cloned())
        .collect();
    json!({
        // Current RA's generated schema names are flat
        // `rust-analyzer.<section>.<key>` settings. Keep the historical
        // nested shape below too; RA tolerates unknown keys, and emitting
        // both lets older deployments keep working while this flat block
        // carries the 2026-03+ path.
        "rust-analyzer.checkOnSave": opts.cargo_check_enabled,
        "rust-analyzer.check.command": "check",
        "rust-analyzer.check.allTargets": false,
        "rust-analyzer.check.invocationStrategy": "per_workspace",
        "rust-analyzer.check.noDefaultFeatures": opts.no_default_features,
        "rust-analyzer.check.features": opts.features.clone(),
        "rust-analyzer.check.extraArgs": check_extra_args.clone(),
        "rust-analyzer.check.overrideCommand": check_override_command.clone(),
        "rust-analyzer.check.workspace": check_workspace,
        "rust-analyzer.check.targets": opts.target.clone(),
        "rust-analyzer.cachePriming.enable": false,
        "rust-analyzer.lru.capacity": ra_lru_capacity(),
        "rust-analyzer.procMacro.enable": proc_macro_enabled,
        "rust-analyzer.cargo.allTargets": false,
        "rust-analyzer.cargo.allFeatures": opts.all_features,
        "rust-analyzer.cargo.buildScripts.enable": cargo_build_scripts_enabled,
        "rust-analyzer.cargo.features": opts.features.clone(),
        "rust-analyzer.cargo.noDefaultFeatures": opts.no_default_features,
        "rust-analyzer.cargo.cfgs": cargo_cfgs.clone(),
        "rust-analyzer.cargo.target": opts.target.clone(),
        "rust-analyzer.workspace.symbol.search.scope": "workspace",
        "rust-analyzer.workspace.symbol.search.kind": "only_types",
        "rust-analyzer.hover.actions.enable": false,
        "rust-analyzer.lens.enable": false,
        "rust-analyzer.completion.snippets.custom": {},
        "rust-analyzer.assist.expressionFillDefault": "todo",
        "rust-analyzer.references.excludeImports": true,

        // (1) checkOnSave — enabled (not disabled).
        // F8-redo's GREEN gate requires `LspEvent::FlycheckEnded`, which
        // requires RA's cargo-check to actually run. Disabling
        // checkOnSave breaks the verdict path. Current RA expects this
        // key to be a boolean; the detailed cargo knobs live under
        // `check.*` below.
        "checkOnSave": opts.cargo_check_enabled,
        // RA's check/flycheck settings.
        "check": {
            "command": "check",
            "allTargets": false,
            "invocationStrategy": "per_workspace",
            "invocationLocation": "workspace",
            "noDefaultFeatures": opts.no_default_features,
            "features": opts.features.clone(),
            "extraArgs": check_extra_args.clone(),
            "overrideCommand": check_override_command.clone(),
            "workspace": check_workspace,
            "targets": opts.target.clone(),
        },

        // (2) inlayHints — ALL OFF. cargoless's stream + check output
        // formats are file:line:col text; we never render inlay-style
        // augmentations. Every one of these is pure idle cost.
        "inlayHints": {
            "parameterHints":          { "enable": false },
            "typeHints":               { "enable": false },
            "chainingHints":           { "enable": false },
            "closureReturnTypeHints":  { "enable": "never" },
            "bindingModeHints":        { "enable": false },
            "closingBraceHints":       { "enable": false },
            "discriminantHints":       { "enable": "never" },
            "implicitDrops":           { "enable": false },
            "lifetimeElisionHints":    { "enable": "never" },
            "rangeExclusiveHints":     { "enable": false },
            "reborrowHints":           { "enable": "never" },
        },

        // (3) cachePriming — RA's startup-time eager analysis. Skipping
        // it makes initial-handshake faster + lowers idle CPU; we don't
        // need pre-warmed caches because our access pattern is
        // diagnostic-driven, not editor-driven.
        "cachePriming": { "enable": false },

        // (3b) #112-B Tier-2 — bound RA's salsa query-memoization LRU.
        // RA's default (128) trades RAM for recompute-avoidance tuned for
        // a human in an editor. cargoless's access pattern is batchy
        // (one analysis per agent-edit-batch, not per-keystroke), so a
        // smaller cap reclaims a large slab of the ~2 GB RSS at the cost
        // of a little recompute on the next batch — and recompute yields
        // the IDENTICAL query result, so this is **correctness-neutral**:
        // it cannot change a diagnostic, the F8-redo verdict, or the
        // never-publish-red invariant (it only affects latency/CPU, which
        // under the agent model is a non-issue between batches). Default
        // halves RA's default; `TF_RA_LRU_CAP` lets bench-lead sweep the
        // RSS/recompute curve (see D-RAM-TIERS §Tier-2).
        "lru": { "capacity": ra_lru_capacity() },

        // (4) procMacro — the heaviest single configurable.
        // proc-macro-server is a separate process that re-runs proc
        // macros on every analysis. On non-proc-macro projects it is
        // pure waste; on proc-macro projects it is mandatory for
        // correctness. The InitOpts.proc_macro_enabled bool is resolved
        // upstream (env var explicit OR Cargo.toml auto-detect).
        "procMacro": { "enable": proc_macro_enabled },

        // (5) cargo.* + workspace.symbol.* — narrow what RA indexes.
        // allFeatures defaults false (cargo's actual default behavior — RA's
        // "all features" was always a divergence) but is operator-settable
        // via `--all-features`/`TF_ALL_FEATURES` for crates whose features
        // compose; the features list flows from InitOpts (CLI `--features`
        // flag); cfgs appends operator cfgs (e.g. `erase_components`) onto
        // RA's defaults; symbol search narrowed to workspace + types-only
        // avoids indexing 3rd-party crate APIs we never query.
        "cargo": {
            "allTargets": false,
            "allFeatures": opts.all_features,
            "buildScripts": {
                "enable": cargo_build_scripts_enabled,
            },
            "features": opts.features.clone(),
            "noDefaultFeatures": opts.no_default_features,
            "cfgs": cargo_cfgs.clone(),
            "target": opts.target.clone(),
        },
        "workspace": {
            "symbol": {
                "search": {
                    "scope": "workspace",
                    "kind": "only_types",
                }
            }
        },

        // Honorable mentions — small but cumulative idle-cost wins.
        "hover":      { "actions": { "enable": false } },
        "lens":       { "enable": false },
        "completion": { "snippets": { "custom": {} } },
        "assist":     { "expressionFillDefault": "todo" },
        "references": { "excludeImports": true },
    })
}

/// LSP `DiagnosticSeverity.Error`.
const SEVERITY_ERROR: i64 = 1;
/// LSP `DiagnosticSeverity.Warning`.
const SEVERITY_WARNING: i64 = 2;
/// LSP `DiagnosticSeverity.Information`.
const SEVERITY_INFO: i64 = 3;
/// LSP `DiagnosticSeverity.Hint`.
const SEVERITY_HINT: i64 = 4;

/// One `textDocument/publishDiagnostics` notification, reduced to what the
/// model cares about, split by **provenance** (#21).
///
/// The count fields (`authoritative_errors`/`advisory_errors`/`total`) are
/// the byte-frozen #21 surface the model's authoritative-vs-advisory logic
/// binds to; the `diagnostics` list is the FIELD FINDING #2 additive surface
/// the CLI uses to print actionable errors. Both are populated by the same
/// extraction so they stay consistent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishDiagnostics {
    /// The document URI exactly as RA sent it (`file://...`).
    pub uri: String,
    /// `severity == Error` diagnostics with `source == "rustc"` — produced by
    /// cargo-check/flycheck. These are AUTHORITATIVE for the verdict.
    pub authoritative_errors: usize,
    /// `severity == Error` diagnostics from any non-rustc source (chiefly
    /// `"rust-analyzer"` native). ADVISORY only — never asserts green.
    pub advisory_errors: usize,
    /// Total diagnostics (errors + warnings + hints, any source).
    pub total: usize,
    /// Full per-diagnostic detail (file/line/col/severity/code/message/source),
    /// in publish order. Additive — the model's count fields above remain the
    /// authority for green/red; this is what the CLI renders. May be empty on
    /// a "cleared" publish (RA's way of saying the file now has zero
    /// diagnostics).
    pub diagnostics: Vec<Diagnostic>,
}

impl PublishDiagnostics {
    /// Total error-severity diagnostics regardless of source.
    pub fn error_count(&self) -> usize {
        self.authoritative_errors + self.advisory_errors
    }

    /// No error of any source. (Not authority on its own — see module docs.)
    pub fn is_green(&self) -> bool {
        self.error_count() == 0
    }

    /// This file has a cargo-check (rustc) error — authoritative red.
    pub fn has_authoritative_error(&self) -> bool {
        self.authoritative_errors > 0
    }

    /// FIELD FINDING #8-redo (#55-reopen): this file has at LEAST one
    /// `severity == Error` diagnostic, from ANY source (rustc, rust-
    /// analyzer-native, or otherwise). Used by the model's per-file
    /// state to flip RED on parse-tier evidence even before cargo
    /// check has had a chance to fire — RA's parser catching a syntax
    /// error means cargo cannot compile this file (strictly stronger
    /// evidence than "rustc has not yet reported"). The original #21
    /// distinction stands for the *advisory channel* + GREEN gating
    /// (which still requires a completed flycheck), but RED is honest
    /// on any severity-error.
    pub fn has_any_severity_error(&self) -> bool {
        self.error_count() > 0
    }
}

/// What the reader thread streams to the model: a diagnostics notification,
/// the boundary of a completed flycheck (`cargo check`) pass, a failed
/// flycheck process, or the boundary of RA's initial workspace indexing
/// (FIELD FINDING #3a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspEvent {
    Diagnostics(PublishDiagnostics),
    /// RA reported a flycheck/`cargo check` `$/progress` `end`. The set of
    /// `source:"rustc"` diagnostics as of now is an AUTHORITATIVE snapshot.
    FlycheckEnded,
    /// RA reported that its flycheck/cargo subprocess failed before it could
    /// produce usable diagnostics. This is authoritative RED: a checker that
    /// cannot run cargo must never publish GREEN.
    FlycheckFailed {
        message: String,
    },
    /// FIELD FINDING #3a: RA reported `$/progress` `end` for its initial
    /// project-indexing (workspace scan / proc-macro server bring-up /
    /// dependency analysis). Before this signal, RA is publishing diagnostics
    /// from an *incomplete* model and the one-shot check loop must NOT
    /// settle-early on its quiet windows. This is what "project ready"
    /// means in LSP terms — the same signal a human IDE waits for before
    /// trusting "Go to Definition".
    IndexingEnded,
}

// ---------------------------------------------------------------------------
// Wire framing (pure)
// ---------------------------------------------------------------------------

/// Frame a JSON body with the LSP `Content-Length` header. Length is in
/// **bytes** (UTF-8), per the spec.
pub fn encode_message(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

/// Read exactly one LSP message body, or `Ok(None)` at clean EOF (the stream
/// ended on a frame boundary — RA exited). Malformed framing is an error.
pub fn read_message<R: BufRead>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut content_len: Option<usize> = None;
    let mut saw_any_header = false;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            if saw_any_header {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF mid-LSP-header",
                ));
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        saw_any_header = true;
        if let Some(v) = trimmed.strip_prefix("Content-Length:") {
            content_len = v.trim().parse::<usize>().ok();
        }
    }
    let len = content_len.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing/invalid Content-Length")
    })?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// True iff a diagnostic is `severity == Error` and produced by cargo-check
/// (`source == "rustc"`). Everything else (native `"rust-analyzer"`, etc.) is
/// advisory.
fn is_rustc_source(d: &Value) -> bool {
    d.get("source").and_then(Value::as_str) == Some("rustc")
}

/// Map an LSP `diagnostic.severity` integer to the typed [`Severity`].
/// `None` ⇒ the severity was missing or out of range; the caller folds that
/// into [`Severity::Info`] so a diagnostic is never silently dropped from
/// the CLI surface (the #21 count fields still skip unknowns).
fn severity_from_lsp(n: i64) -> Option<Severity> {
    match n {
        SEVERITY_ERROR => Some(Severity::Error),
        SEVERITY_WARNING => Some(Severity::Warning),
        SEVERITY_INFO => Some(Severity::Info),
        SEVERITY_HINT => Some(Severity::Hint),
        _ => None,
    }
}

/// Extract one [`Diagnostic`] from a single LSP `Diagnostic` JSON object
/// against a known `file_path`. Tolerant of malformed entries — anything
/// missing/unsorted is filled with a sensible default so the CLI still
/// surfaces *something* useful (the FIELD FINDING #2 contract: a red tree
/// always tells you *what*).
fn extract_one_diagnostic(d: &Value, file_path: &std::path::Path) -> Diagnostic {
    let sev_int = d.get("severity").and_then(Value::as_i64).unwrap_or(0);
    let severity = severity_from_lsp(sev_int).unwrap_or(Severity::Info);
    // LSP positions are 0-based; rustc/cargo display is 1-based — convert at
    // the boundary so every consumer sees the friendly convention.
    let lsp_line = d
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let lsp_col = d
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("character"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let line = (lsp_line as u32).saturating_add(1);
    let col = (lsp_col as u32).saturating_add(1);
    // `code` may be a string ("E0277") or a number; render either as a string.
    let code = d.get("code").and_then(|c| {
        c.as_str()
            .map(str::to_owned)
            .or_else(|| c.as_i64().map(|n| n.to_string()))
    });
    let message = d
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("(no message)")
        .to_string();
    let source = d.get("source").and_then(Value::as_str).map(str::to_owned);
    Diagnostic {
        file_path: file_path.to_path_buf(),
        line,
        col,
        severity,
        code,
        message,
        source,
    }
}

/// Pull a [`PublishDiagnostics`] out of a decoded JSON-RPC message, or `None`
/// if it is not a `textDocument/publishDiagnostics` notification. Splits
/// error counts by provenance (#21) **and** extracts the full per-diagnostic
/// detail (FIELD FINDING #2 additive surface).
pub fn extract_publish_diagnostics(v: &Value) -> Option<PublishDiagnostics> {
    if v.get("method")?.as_str()? != "textDocument/publishDiagnostics" {
        return None;
    }
    let params = v.get("params")?;
    let uri = params.get("uri")?.as_str()?.to_string();
    let diags = params.get("diagnostics")?.as_array()?;
    let mut authoritative_errors = 0usize;
    let mut advisory_errors = 0usize;
    // Pre-compute the per-publish file_path once. `path_from_uri` returns
    // `None` for non-`file:` schemes (e.g. `untitled:`); fall back to the raw
    // URI string so callers still get something stable.
    let file_path = path_from_uri(&uri)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(&uri));
    let mut rich = Vec::with_capacity(diags.len());
    for d in diags {
        if d.get("severity").and_then(Value::as_i64) == Some(SEVERITY_ERROR) {
            if is_rustc_source(d) {
                authoritative_errors += 1;
            } else {
                advisory_errors += 1;
            }
        }
        rich.push(extract_one_diagnostic(d, &file_path));
    }
    Some(PublishDiagnostics {
        uri,
        authoritative_errors,
        advisory_errors,
        total: diags.len(),
        diagnostics: rich,
    })
}

/// Common pre-check: `v` is a `$/progress` notification with `kind: "end"`.
/// Returns the lowercased (`token`, `title`) tuple for the caller's
/// generous matching, or `None` if `v` is not such a notification.
fn progress_end_token_title(v: &Value) -> Option<(String, String)> {
    if v.get("method").and_then(Value::as_str) != Some("$/progress") {
        return None;
    }
    let params = v.get("params")?;
    let value = params.get("value")?;
    if value.get("kind").and_then(Value::as_str) != Some("end") {
        return None;
    }
    let token = params
        .get("token")
        .map(|t| t.to_string())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    Some((token, title))
}

/// True iff `v` is a `$/progress` notification ending a flycheck /
/// `cargo check` pass. RA's flycheck progress carries `"check"` in its token
/// or title; matching generously on that (case-insensitive) is safe because
/// a missed end only degrades to the model's settle/timeout path — it can
/// never manufacture a false green (the model needs a *seen* end to upgrade
/// to authoritative).
pub fn extract_flycheck_end(v: &Value) -> bool {
    let Some((token, title)) = progress_end_token_title(v) else {
        return false;
    };
    // Guard: the indexing-end is also a `$/progress`/`end`; it is NOT a
    // flycheck end. Without this, RA's first indexing-end would falsely
    // trip the flycheck-end path and let the check loop upgrade to
    // Authoritative without a real cargo-check pass.
    if title.contains("indexing") || title.contains("scanning") || token.contains("indexing") {
        return false;
    }
    token.contains("check") || title.contains("check") || title.contains("flycheck")
}

fn is_flycheck_failure_message(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("flycheck failed")
        || msg.contains("cargo watcher failed")
        || msg.contains("cargo check failed")
        || msg.contains("failed to run the following command")
        || msg.contains("produced no valid metadata")
}

/// Extract a rust-analyzer flycheck/cargo execution failure from a decoded
/// JSON-RPC notification. RA versions differ in whether this appears as a
/// `window/showMessage` notification or as text on a flycheck progress item,
/// so the matcher is deliberately limited to those two LSP surfaces and then
/// keyed on the cargo/flycheck failure phrases above.
pub fn extract_flycheck_failure(v: &Value) -> Option<String> {
    match v.get("method").and_then(Value::as_str)? {
        "window/showMessage" | "window/showMessageRequest" => {
            let message = v
                .get("params")
                .and_then(|p| p.get("message"))
                .and_then(Value::as_str)?;
            is_flycheck_failure_message(message).then(|| message.to_string())
        }
        "$/progress" => {
            let value = v.get("params")?.get("value")?;
            let mut parts = Vec::new();
            if let Some(title) = value.get("title").and_then(Value::as_str) {
                parts.push(title);
            }
            if let Some(message) = value.get("message").and_then(Value::as_str) {
                parts.push(message);
            }
            let message = parts.join(": ");
            is_flycheck_failure_message(&message).then_some(message)
        }
        _ => None,
    }
}

/// Synthetic diagnostic used when rust-analyzer reports that cargo/flycheck
/// itself failed. It flows through the same red-verdict machinery as rustc
/// diagnostics, but with a sentinel path because RA had no file diagnostic
/// to publish.
pub fn flycheck_failure_diagnostics(message: impl Into<String>) -> PublishDiagnostics {
    let message = message.into();
    let file_path = std::path::PathBuf::from("/__cargoless__/flycheck");
    PublishDiagnostics {
        uri: uri_from_path("/__cargoless__/flycheck"),
        authoritative_errors: 1,
        advisory_errors: 0,
        total: 1,
        diagnostics: vec![Diagnostic {
            file_path,
            line: 1,
            col: 1,
            severity: Severity::Error,
            code: Some("CARGOLESS_FLYCHECK_FAILED".to_string()),
            message,
            source: Some("rustc".to_string()),
        }],
    }
}

/// FIELD FINDING #3a: true iff `v` is a `$/progress` notification ending RA's
/// initial workspace indexing (or the related roots-scanning / proc-macro
/// server bring-up phases that gate "the model is ready"). Generous matching
/// on the lowercased token/title so RA version bumps that rename
/// `rustAnalyzer/Indexing` → `rust-analyzer/indexing` (or similar) keep
/// working. False positives are SAFE here — the worst case is allowing the
/// check loop to settle-early as it did before the fix; false negatives are
/// the trust-broken case (cold-start false-red), so we accept slightly more
/// permissive matching than `extract_flycheck_end`.
pub fn extract_indexing_end(v: &Value) -> bool {
    let Some((token, title)) = progress_end_token_title(v) else {
        return false;
    };
    // Exclude flycheck/check ends — those are signalled separately and the
    // model treats them with very different authority.
    if title.contains("check") || token.contains("check") {
        return false;
    }
    token.contains("indexing")
        || token.contains("rootscanning")
        || token.contains("rootsscanned")
        || token.contains("roots scanned")
        || token.contains("roots_scanned")
        || title.contains("indexing")
        || title.contains("roots scanned")
        || title.contains("scanning")
        || title.contains("loading")
}

/// `/abs/path` → `file:///abs/path`. v0: assumes an already-absolute,
/// space-free path; percent-encoding is a documented v1 refinement.
pub fn uri_from_path(abs_path: &str) -> String {
    if abs_path.starts_with('/') {
        format!("file://{abs_path}")
    } else {
        format!("file:///{abs_path}")
    }
}

/// Inverse of [`uri_from_path`] for the `file:` scheme; `None` for non-`file:`.
pub fn path_from_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    Some(rest.to_string())
}

// ---------------------------------------------------------------------------
// Live client
// ---------------------------------------------------------------------------

/// LSP client bound to one rust-analyzer process's stdio. Construction runs
/// the `initialize`/`initialized` handshake synchronously (enabling flycheck
/// via `checkOnSave`), then a reader thread streams [`LspEvent`]s.
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub struct LspClient {
    writer: SharedWriter,
    next_id: AtomicI64,
}

impl LspClient {
    /// Handshake against an RA speaking LSP over (`w` = its stdin, `r` = its
    /// stdout). `root_path` is the absolute workspace root. flycheck
    /// (`cargo check` on save) is enabled — it is the authoritative tier.
    ///
    /// FIELD FINDING #74: now takes an `InitOpts` carrying the
    /// proc-macro-enable + features list. The lean
    /// `initializationOptions` JSON is built by [`lean_init_options`]
    /// and includes the full Option-B+ refinement (softened checkOnSave,
    /// inlayHints disabled, cachePriming off, procMacro per-opts,
    /// cargo features narrowed, plus honorable mentions). See module-
    /// doc on the 30-50% RA-resource-reduction rationale.
    pub fn initialize<W, R>(
        w: W,
        r: R,
        root_path: &str,
        opts: &InitOpts,
    ) -> io::Result<(Self, Receiver<LspEvent>)>
    where
        W: Write + Send + 'static,
        R: Read + Send + 'static,
    {
        let root_uri = uri_from_path(root_path);
        let init_options = lean_init_options(opts);
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(w)));
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                // #21 + F8-redo + #74: lean init options. checkOnSave
                // stays ON (load-bearing for the GREEN gate); everything
                // else is tuned down.
                "initializationOptions": init_options.clone(),
                "capabilities": {
                    "window": { "workDoneProgress": true },
                    "workspace": { "configuration": true },
                    "textDocument": {
                        "publishDiagnostics": { "relatedInformation": false }
                    }
                }
            }
        });
        write_lsp_message(&writer, &init)?;

        let mut br = BufReader::new(r);
        loop {
            match read_message(&mut br)? {
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "RA exited during initialize handshake",
                    ));
                }
                Some(body) => {
                    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
                        continue;
                    };
                    if v.get("id").and_then(Value::as_i64) == Some(1) && v.get("method").is_none() {
                        break;
                    }
                    let _ = respond_to_server_request(&v, &writer, &init_options);
                }
            }
        }

        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        write_lsp_message(&writer, &initialized)?;
        let config = json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeConfiguration",
            "params": {
                "settings": nested_ra_config(&init_options),
            }
        });
        write_lsp_message(&writer, &config)?;

        let (tx, rx): (Sender<LspEvent>, Receiver<LspEvent>) = channel();
        let reader_writer = Arc::clone(&writer);
        let reader_config = init_options.clone();
        let _reader: JoinHandle<()> = thread::Builder::new()
            .name("tf-lsp-reader".into())
            .spawn(move || reader_loop(br, tx, reader_writer, reader_config))
            .expect("spawn tf-lsp-reader thread");

        Ok((
            Self {
                writer,
                next_id: AtomicI64::new(2),
            },
            rx,
        ))
    }

    fn notify(&self, method: &str, params: Value) -> io::Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        write_lsp_message(&self.writer, &msg)
    }

    /// `textDocument/didOpen`.
    pub fn did_open(&self, abs_path: &str, text: &str, version: i64) -> io::Result<()> {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri_from_path(abs_path),
                    "languageId": "rust",
                    "version": version,
                    "text": text
                }
            }),
        )
    }

    /// `textDocument/didChange` (full-document sync — v0 keeps it simple).
    pub fn did_change(&self, abs_path: &str, text: &str, version: i64) -> io::Result<()> {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri_from_path(abs_path), "version": version },
                "contentChanges": [ { "text": text } ]
            }),
        )
    }

    /// `textDocument/didSave` — triggers RA flycheck (`cargo check`), the
    /// authoritative tier.
    pub fn did_save(&self, abs_path: &str) -> io::Result<()> {
        self.notify(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": uri_from_path(abs_path) } }),
        )
    }

    /// RA extension: explicitly trigger flycheck for a document. This is
    /// the headless-daemon equivalent of an editor's "run flycheck"
    /// command and avoids relying on save-event heuristics alone.
    pub fn run_flycheck(&self, abs_path: &str) -> io::Result<()> {
        self.notify(
            "rust-analyzer/runFlycheck",
            json!({ "textDocument": { "uri": uri_from_path(abs_path) } }),
        )
    }

    /// `textDocument/didClose` — RA drops the buffer overlay for `uri`
    /// and reverts to its base/on-disk content for that file.
    ///
    /// #5 (Model R Stream C) I/O-shell primitive: the overlay multiplexer
    /// lowers [`crate::overlay::OverlayOp::Close`] to exactly this. It is
    /// the load-bearing isolation op — when switching the single shared RA
    /// from worktree V to worktree W, every file V overlaid but W does not
    /// must be `did_close`d, else V's content contaminates W's verdict
    /// (the failure the one-RA-multiplex must never allow; `overlay::diff`
    /// guarantees the Close set, this verb executes it).
    pub fn did_close(&self, abs_path: &str) -> io::Result<()> {
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri_from_path(abs_path) } }),
        )
    }

    /// Monotonic LSP id for any future request-style call.
    pub fn next_request_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }
}

fn write_lsp_message(writer: &SharedWriter, msg: &Value) -> io::Result<()> {
    let bytes = encode_message(msg.to_string().as_bytes());
    let mut w = writer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    w.write_all(&bytes)?;
    w.flush()
}

fn respond_to_server_request(v: &Value, writer: &SharedWriter, config: &Value) -> io::Result<bool> {
    let Some(id) = v.get("id").cloned() else {
        return Ok(false);
    };
    let Some(method) = v.get("method").and_then(Value::as_str) else {
        return Ok(false);
    };
    trace_lsp_request(method, v);
    let result = if method == "workspace/configuration" {
        workspace_configuration_result(v, config)
    } else {
        Value::Null
    };
    write_lsp_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
    )?;
    Ok(true)
}

fn trace_lsp_request(method: &str, v: &Value) {
    if std::env::var_os("CARGOLESS_LSP_TRACE").is_none() {
        return;
    }
    let sections: Vec<String> = v
        .get("params")
        .and_then(|params| params.get("items"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("section").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    eprintln!("[cargoless:lsp] request method={method} sections={sections:?}");
}

fn trace_lsp_notification(v: &Value) {
    if std::env::var_os("CARGOLESS_LSP_TRACE").is_none() {
        return;
    }
    let Some(method) = v.get("method").and_then(Value::as_str) else {
        return;
    };
    match method {
        "$/progress" | "window/showMessage" | "window/showMessageRequest" => {
            eprintln!("[cargoless:lsp] notification {method}: {v}");
        }
        "textDocument/publishDiagnostics" => {
            let uri = v
                .get("params")
                .and_then(|p| p.get("uri"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let count = v
                .get("params")
                .and_then(|p| p.get("diagnostics"))
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            eprintln!("[cargoless:lsp] notification publishDiagnostics uri={uri} count={count}");
        }
        _ => {}
    }
}

fn workspace_configuration_result(v: &Value, config: &Value) -> Value {
    let Some(items) = v
        .get("params")
        .and_then(|params| params.get("items"))
        .and_then(Value::as_array)
    else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        items
            .iter()
            .map(|item| {
                workspace_configuration_value(item.get("section").and_then(Value::as_str), config)
            })
            .collect(),
    )
}

fn workspace_configuration_value(section: Option<&str>, config: &Value) -> Value {
    let Some(section) = section else {
        return nested_ra_config(config);
    };
    if section == "rust-analyzer" {
        return nested_ra_config(config);
    }
    if let Some(value) = config.get(section) {
        return value.clone();
    }
    if let Some(tail) = section.strip_prefix("rust-analyzer.") {
        if let Some(value) = config.get(tail) {
            return value.clone();
        }
    }
    Value::Null
}

fn nested_ra_config(config: &Value) -> Value {
    let Some(obj) = config.as_object() else {
        return Value::Null;
    };
    let mut out: serde_json::Map<String, Value> = obj
        .iter()
        .filter(|(key, _)| !key.starts_with("rust-analyzer.") && key.as_str() != "checkOnSave")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    out.insert(
        "checkOnSave".to_string(),
        config
            .get("rust-analyzer.checkOnSave")
            .cloned()
            .unwrap_or(Value::Bool(true)),
    );
    Value::Object(out)
}

fn reader_loop<R: BufRead>(mut br: R, tx: Sender<LspEvent>, writer: SharedWriter, config: Value) {
    loop {
        match read_message(&mut br) {
            Ok(None) => break, // RA exited cleanly
            Err(_) => break,   // stream died / supervisor will restart RA
            Ok(Some(body)) => {
                let Ok(v) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                if matches!(respond_to_server_request(&v, &writer, &config), Ok(true)) {
                    continue;
                }
                trace_lsp_notification(&v);
                let ev = if let Some(pd) = extract_publish_diagnostics(&v) {
                    LspEvent::Diagnostics(pd)
                } else if let Some(message) = extract_flycheck_failure(&v) {
                    LspEvent::FlycheckFailed { message }
                } else if extract_flycheck_end(&v) {
                    LspEvent::FlycheckEnded
                } else if extract_indexing_end(&v) {
                    LspEvent::IndexingEnded
                } else {
                    continue;
                };
                if tx.send(ev).is_err() {
                    break; // model gone
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // -----------------------------------------------------------------------
    // #180: env-test serialization (fleet-wide gate-reliability fix)
    //
    // `init_opts_from_env_handles_explicit_{disabled,enabled}` (+ the
    // `_features_from_env_csv` test, which also reads `TF_PROC_MACRO` via
    // `InitOpts::from_env_and_project`) mutate the PROCESS-GLOBAL
    // environment. cargo's default test harness runs tests on multiple
    // threads, so two of them racing the same var intermittently makes
    // the "disabled" assertion observe the "enabled" writer ⇒ a spurious
    // gate RED — byte-identical on main, fleet-wide intermittent, and was
    // blocking bench-lead's #8-ready. Fix: serialize EVERY env-mutating
    // test in this module behind one process-global lock, and
    // save/restore the var as an RAII critical section. The LOCK is the
    // race fix (save/restore alone does not prevent the interleave);
    // dependency-free (no `serial_test`); poison-tolerant via
    // `into_inner` (a panicking test must not wedge the rest — same
    // discipline as `analyzer.rs`'s `lock()`).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Holds `ENV_LOCK` for its whole lifetime and save/restores one env
    /// var. Field drop order is declaration order, and `Drop::drop` runs
    /// before fields drop, so the restore happens *while still locked*
    /// and the lock releases *after* restore — exactly the required
    /// set→read→assert→restore→unlock ordering.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var(key).ok();
            // SAFETY: ENV_LOCK serializes every env-mutating test in this
            // module, so no concurrent env reader/writer exists for the
            // guard's lifetime (the edition-2024 set_var hazard the
            // `unsafe` marks is exactly cross-thread concurrency).
            unsafe { std::env::set_var(key, val) };
            Self { key, prev, _lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: still under ENV_LOCK (`_lock` drops after this).
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn encode_then_read_roundtrips() {
        let body = br#"{"jsonrpc":"2.0","method":"x"}"#;
        let framed = encode_message(body);
        let mut cur = Cursor::new(framed);
        let got = read_message(&mut cur).unwrap().unwrap();
        assert_eq!(got, body);
        assert!(read_message(&mut cur).unwrap().is_none());
    }

    #[test]
    fn read_handles_back_to_back_messages() {
        let mut stream = encode_message(b"AAAA");
        stream.extend(encode_message(b"BB"));
        let mut cur = Cursor::new(stream);
        assert_eq!(read_message(&mut cur).unwrap().unwrap(), b"AAAA");
        assert_eq!(read_message(&mut cur).unwrap().unwrap(), b"BB");
        assert!(read_message(&mut cur).unwrap().is_none());
    }

    #[test]
    fn missing_content_length_is_error() {
        let mut cur = Cursor::new(b"X-Foo: 1\r\n\r\n".to_vec());
        assert!(read_message(&mut cur).is_err());
    }

    #[test]
    fn provenance_split_rustc_vs_native() {
        // E0599 (method-not-found) is rustc/cargo-check ONLY — the exact
        // class RA-native is blind to (#21). Native borrow error is advisory.
        let v: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///p/src/a.rs","diagnostics":[
                  {"severity":1,"source":"rustc","code":"E0599","message":"no method"},
                  {"severity":1,"source":"rust-analyzer","message":"native"},
                  {"severity":2,"source":"rustc","message":"warn"},
                  {"severity":1,"source":"rustc","code":"E0308","message":"mismatch"}]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&v).unwrap();
        assert_eq!(pd.uri, "file:///p/src/a.rs");
        assert_eq!(pd.authoritative_errors, 2, "two rustc errors");
        assert_eq!(pd.advisory_errors, 1, "one native error");
        assert_eq!(pd.total, 4);
        assert!(pd.has_authoritative_error());
        assert!(!pd.is_green());
        assert_eq!(pd.error_count(), 3);
        // FIELD FINDING #2 additive surface: the rich list mirrors the count
        // fields and carries everything the CLI needs to print.
        assert_eq!(pd.diagnostics.len(), 4, "rich list mirrors total");
        let codes: Vec<&str> = pd
            .diagnostics
            .iter()
            .filter_map(|d| d.code.as_deref())
            .collect();
        assert!(codes.contains(&"E0599"));
        assert!(codes.contains(&"E0308"));
        // Every diagnostic shares the publish file_path.
        for d in &pd.diagnostics {
            assert_eq!(d.file_path, std::path::PathBuf::from("/p/src/a.rs"));
        }
    }

    #[test]
    fn diagnostic_position_severity_and_code_extracted_one_based() {
        // FIELD FINDING #2: the CLI must print file:line:col + severity +
        // code + message; verify each piece round-trips and that LSP's
        // 0-based positions are converted to 1-based at the boundary.
        let v: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///r/src/lib.rs","diagnostics":[
                  {"severity":1,"source":"rustc","code":"E0277",
                   "message":"the trait bound `T: Foo` is not satisfied",
                   "range":{"start":{"line":41,"character":4},
                            "end":{"line":41,"character":11}}},
                  {"severity":2,"source":"rust-analyzer","code":"unused_imports",
                   "message":"unused import: `Bar`",
                   "range":{"start":{"line":0,"character":0},
                            "end":{"line":0,"character":11}}},
                  {"severity":3,"message":"hint-ish","source":"rustc"},
                  {"severity":4,"code":123,"message":"numeric code",
                   "range":{"start":{"line":9,"character":7},"end":{"line":9,"character":9}}}
                ]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&v).unwrap();
        assert_eq!(pd.diagnostics.len(), 4);

        let d0 = &pd.diagnostics[0];
        assert_eq!(d0.severity, Severity::Error);
        assert_eq!(d0.code.as_deref(), Some("E0277"));
        assert_eq!(d0.line, 42, "0-based LSP line 41 → 1-based 42");
        assert_eq!(d0.col, 5, "0-based LSP col 4 → 1-based 5");
        assert!(d0.message.contains("trait bound"));
        assert_eq!(d0.source.as_deref(), Some("rustc"));
        assert_eq!(d0.file_path, std::path::PathBuf::from("/r/src/lib.rs"));

        let d1 = &pd.diagnostics[1];
        assert_eq!(d1.severity, Severity::Warning);
        assert_eq!(d1.code.as_deref(), Some("unused_imports"));
        assert_eq!(d1.line, 1, "0-based line 0 → 1-based 1");
        assert_eq!(d1.col, 1);
        assert_eq!(d1.source.as_deref(), Some("rust-analyzer"));

        let d2 = &pd.diagnostics[2];
        assert_eq!(d2.severity, Severity::Info, "severity:3 → Info");
        assert_eq!(d2.line, 1, "missing range defaults to 1-based 1");
        assert_eq!(d2.col, 1);
        assert_eq!(d2.code, None);

        let d3 = &pd.diagnostics[3];
        assert_eq!(d3.severity, Severity::Hint, "severity:4 → Hint");
        assert_eq!(d3.code.as_deref(), Some("123"), "numeric code → string");
    }

    #[test]
    fn empty_publish_clears_the_rich_list_too() {
        // RA sends an empty `diagnostics: []` to "clear" a file once it goes
        // clean. The rich list must reflect that — Vec::is_empty is what the
        // model uses to drop stale per-file diagnostics.
        let v: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///r/src/lib.rs","diagnostics":[]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&v).unwrap();
        assert!(pd.diagnostics.is_empty());
        assert!(pd.is_green());
    }

    #[test]
    fn has_any_severity_error_covers_both_tiers() {
        // FIELD FINDING #8-redo: RA-native severity:Error alone (no
        // rustc-tier error) MUST still register as "this file has an
        // error" — the dogfood reproducer's exact case (RA's parser
        // catches `let bad =` before cargo check runs).
        let ra_only: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///r/src/lib.rs","diagnostics":[
                  {"severity":1,"source":"rust-analyzer",
                   "message":"Syntax Error: expected an item"}]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&ra_only).unwrap();
        assert!(
            !pd.has_authoritative_error(),
            "no rustc-source ⇒ not authoritative-only error"
        );
        assert!(
            pd.has_any_severity_error(),
            "RA-native severity:Error counts toward `any` (the #8-redo invariant)"
        );

        let rustc_only: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///r/src/lib.rs","diagnostics":[
                  {"severity":1,"source":"rustc","code":"E0277",
                   "message":"trait bound"}]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&rustc_only).unwrap();
        assert!(pd.has_authoritative_error());
        assert!(pd.has_any_severity_error());

        let both: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///r/src/lib.rs","diagnostics":[
                  {"severity":1,"source":"rustc","code":"E0277","message":"a"},
                  {"severity":1,"source":"rust-analyzer","message":"b"}]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&both).unwrap();
        assert!(pd.has_authoritative_error());
        assert!(pd.has_any_severity_error());

        // Warnings/notes from any source do NOT count as severity-errors.
        let warnings: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///r/src/lib.rs","diagnostics":[
                  {"severity":2,"source":"rustc","message":"warn"},
                  {"severity":2,"source":"rust-analyzer","message":"lint"}]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&warnings).unwrap();
        assert!(!pd.has_authoritative_error());
        assert!(
            !pd.has_any_severity_error(),
            "severity:Warning is not an error in any tier"
        );
    }

    #[test]
    fn empty_diagnostics_is_green_no_authoritative() {
        let v: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///p/src/a.rs","diagnostics":[]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&v).unwrap();
        assert!(pd.is_green());
        assert!(!pd.has_authoritative_error());
        assert_eq!(pd.authoritative_errors, 0);
        assert_eq!(pd.advisory_errors, 0);
    }

    #[test]
    fn native_only_error_is_not_authoritative() {
        let v: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics",
                "params":{"uri":"file:///x.rs","diagnostics":[
                  {"severity":1,"source":"rust-analyzer","message":"syntax"}]}}"#,
        )
        .unwrap();
        let pd = extract_publish_diagnostics(&v).unwrap();
        assert!(!pd.has_authoritative_error());
        assert_eq!(pd.advisory_errors, 1);
        assert!(!pd.is_green());
    }

    // -----------------------------------------------------------------------
    // FIELD FINDING #3a — indexing-end detection + flycheck-end disambiguation
    // -----------------------------------------------------------------------

    #[test]
    fn indexing_end_detection_matches_ra_progress_tokens() {
        // The canonical RA indexing token + an `end` event.
        let v: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/Indexing",
                "value":{"kind":"end","title":"Indexing"}}}"#,
        )
        .unwrap();
        assert!(extract_indexing_end(&v), "canonical Indexing/end");

        // Roots-scanned (RA's pre-indexing phase) also gates project-ready.
        let v2: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/RootsScanned",
                "value":{"kind":"end","title":"Roots Scanned"}}}"#,
        )
        .unwrap();
        assert!(extract_indexing_end(&v2));

        // Current RA reports this as a token with a space and no title on
        // the final end notification.
        let v2b: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/Roots Scanned",
                "value":{"kind":"end","message":"1318/1318"}}}"#,
        )
        .unwrap();
        assert!(extract_indexing_end(&v2b));

        // Begin/report on the same token is NOT an end.
        let v3: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/Indexing",
                "value":{"kind":"begin","title":"Indexing"}}}"#,
        )
        .unwrap();
        assert!(!extract_indexing_end(&v3));

        // A flycheck end is NOT an indexing end (they ride the same
        // `$/progress`/`end` shape; the cargo-check token must not leak).
        let v4: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/cargoCheck",
                "value":{"kind":"end","title":"cargo check"}}}"#,
        )
        .unwrap();
        assert!(!extract_indexing_end(&v4));
    }

    #[test]
    fn flycheck_end_does_not_match_indexing_end() {
        // The reverse direction: an indexing end must NOT be misread as a
        // flycheck end (the bug the #43 fix exists to prevent — RA's first
        // indexing-end firing before any real cargo-check pass would let the
        // check loop upgrade to Authoritative on incomplete data).
        let idx: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/Indexing",
                "value":{"kind":"end","title":"Indexing"}}}"#,
        )
        .unwrap();
        assert!(!extract_flycheck_end(&idx), "indexing end is NOT flycheck");
        // Roots-scanned likewise.
        let rs: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/RootsScanned",
                "value":{"kind":"end","title":"Roots Scanned"}}}"#,
        )
        .unwrap();
        assert!(!extract_flycheck_end(&rs));
    }

    #[test]
    fn flycheck_end_detection() {
        let end: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/cargoCheck",
                "value":{"kind":"end"}}}"#,
        )
        .unwrap();
        assert!(extract_flycheck_end(&end));

        let end_by_title: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"x",
                "value":{"kind":"end","title":"cargo check"}}}"#,
        )
        .unwrap();
        assert!(extract_flycheck_end(&end_by_title));

        // begin/report of the same is NOT an end
        let begin: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/cargoCheck",
                "value":{"kind":"begin","title":"cargo check"}}}"#,
        )
        .unwrap();
        assert!(!extract_flycheck_end(&begin));

        // unrelated progress (indexing) end is not a flycheck end
        let indexing: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/Indexing",
                "value":{"kind":"end","title":"Indexing"}}}"#,
        )
        .unwrap();
        assert!(!extract_flycheck_end(&indexing));

        // a publishDiagnostics is not a flycheck end
        let pd: Value = serde_json::from_str(
            r#"{"method":"textDocument/publishDiagnostics","params":{"uri":"file:///a","diagnostics":[]}}"#,
        )
        .unwrap();
        assert!(!extract_flycheck_end(&pd));
    }

    #[test]
    fn flycheck_failure_detection_matches_show_message_and_progress() {
        let show: Value = serde_json::from_str(
            r#"{"method":"window/showMessage",
                "params":{"type":1,
                "message":"Flycheck failed to run the following command: cargo check"}}"#,
        )
        .unwrap();
        assert_eq!(
            extract_flycheck_failure(&show).as_deref(),
            Some("Flycheck failed to run the following command: cargo check")
        );

        let progress: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/flycheck/0",
                "value":{"kind":"end","title":"cargo check",
                "message":"Cargo watcher failed, the command produced no valid metadata"}}}"#,
        )
        .unwrap();
        assert!(
            extract_flycheck_failure(&progress)
                .unwrap()
                .contains("Cargo watcher failed")
        );

        let clean_end: Value = serde_json::from_str(
            r#"{"method":"$/progress","params":{"token":"rustAnalyzer/cargoCheck",
                "value":{"kind":"end","title":"cargo check"}}}"#,
        )
        .unwrap();
        assert!(extract_flycheck_failure(&clean_end).is_none());
    }

    #[test]
    fn flycheck_failure_diagnostic_is_authoritative_red() {
        let pd = flycheck_failure_diagnostics("cargo check failed to start");
        assert_eq!(pd.uri, "file:///__cargoless__/flycheck");
        assert_eq!(pd.authoritative_errors, 1);
        assert!(pd.has_authoritative_error());
        assert_eq!(
            pd.diagnostics[0].code.as_deref(),
            Some("CARGOLESS_FLYCHECK_FAILED")
        );
        assert_eq!(pd.diagnostics[0].source.as_deref(), Some("rustc"));
    }

    #[test]
    fn uri_path_roundtrip() {
        assert_eq!(uri_from_path("/abs/x.rs"), "file:///abs/x.rs");
        assert_eq!(
            path_from_uri("file:///abs/x.rs").as_deref(),
            Some("/abs/x.rs")
        );
        assert!(path_from_uri("http://x").is_none());
    }

    // -----------------------------------------------------------------------
    // #74 RA weight-shedding — InitOpts + lean_init_options + proc-macro
    // -----------------------------------------------------------------------

    #[test]
    fn lean_init_options_shape_has_load_bearing_keys_in_right_places() {
        // The full JSON tree the lean init builder produces. Assert that
        // the EXPECTED nested keys are present at the right paths — a
        // future refactor that drops e.g. "checkOnSave" or
        // "procMacro.enable" would silently regress the verdict path
        // (#21/F8-redo) or the polish savings (#74).
        let v = lean_init_options(&InitOpts::default());

        // checkOnSave is enabled (the F8-redo invariant — disabling would
        // break the GREEN gate). Detailed flycheck settings live under
        // `check.*`.
        assert_eq!(v["rust-analyzer.checkOnSave"], json!(true));
        assert_eq!(v["rust-analyzer.check"]["allTargets"], json!(null));
        assert_eq!(v["rust-analyzer.check.allTargets"], json!(false));
        assert_eq!(v["rust-analyzer.check.overrideCommand"], json!(null));
        assert_eq!(v["rust-analyzer.check.workspace"], json!(true));
        assert_eq!(v["checkOnSave"], json!(true));

        // check settings carry the cargo/flycheck profile.
        assert_eq!(v["check"]["command"], json!("check"));
        assert_eq!(v["check"]["allTargets"], json!(false));
        assert_eq!(v["check"]["extraArgs"], json!([]));
        assert_eq!(v["check"]["overrideCommand"], json!(null));
        assert_eq!(v["check"]["workspace"], json!(true));
        assert_eq!(v["check"]["targets"], json!(null));

        // inlayHints all OFF (cargoless never renders them).
        for k in [
            "parameterHints",
            "typeHints",
            "chainingHints",
            "bindingModeHints",
            "closingBraceHints",
            "implicitDrops",
            "rangeExclusiveHints",
        ] {
            assert_eq!(
                v["inlayHints"][k]["enable"],
                json!(false),
                "inlayHints.{k}.enable must be false (rendered nowhere): {v}"
            );
        }
        for k in [
            "closureReturnTypeHints",
            "discriminantHints",
            "lifetimeElisionHints",
            "reborrowHints",
        ] {
            // RA accepts "never" string (not bool) for these.
            assert_eq!(v["inlayHints"][k]["enable"], json!("never"));
        }

        // cachePriming OFF.
        assert_eq!(v["cachePriming"]["enable"], json!(false));

        // procMacro tracks InitOpts.proc_macro_enabled (default=true).
        assert_eq!(v["rust-analyzer.procMacro.enable"], json!(true));
        assert_eq!(v["procMacro"]["enable"], json!(true));

        // cargo: allFeatures OFF, features list from InitOpts (default
        // = empty so cargo uses its own defaults), defaults kept
        // (noDefaultFeatures: false).
        assert_eq!(v["rust-analyzer.cargo.allTargets"], json!(false));
        assert_eq!(v["rust-analyzer.cargo.allFeatures"], json!(false));
        assert_eq!(v["rust-analyzer.cargo.buildScripts.enable"], json!(true));
        assert_eq!(v["cargo"]["allTargets"], json!(false));
        assert_eq!(v["cargo"]["allFeatures"], json!(false));
        assert_eq!(v["cargo"]["buildScripts"]["enable"], json!(true));
        assert_eq!(v["cargo"]["features"], json!([]));
        assert_eq!(v["cargo"]["noDefaultFeatures"], json!(false));
        assert_eq!(v["cargo"]["target"], json!(null));
        // cargo.cfgs default = RA's OWN defaults (NOT empty) — `cargo.cfgs`
        // replaces rather than merges, so with no operator cfgs we must
        // re-emit RA's base or `debug_assertions`/`miri` silently vanish.
        assert_eq!(
            v["rust-analyzer.cargo.cfgs"],
            json!(["debug_assertions", "miri"])
        );
        assert_eq!(v["cargo"]["cfgs"], json!(["debug_assertions", "miri"]));

        // workspace.symbol narrowed.
        assert_eq!(
            v["workspace"]["symbol"]["search"]["scope"],
            json!("workspace")
        );
        assert_eq!(
            v["workspace"]["symbol"]["search"]["kind"],
            json!("only_types")
        );

        // Honorable mentions.
        assert_eq!(v["hover"]["actions"]["enable"], json!(false));
        assert_eq!(v["lens"]["enable"], json!(false));
        assert_eq!(v["completion"]["snippets"]["custom"], json!({}));
        assert_eq!(v["assist"]["expressionFillDefault"], json!("todo"));
        assert_eq!(v["references"]["excludeImports"], json!(true));
    }

    #[test]
    fn lean_init_options_can_disable_ra_cargo_flycheck_for_push_daemons() {
        let v = lean_init_options(&InitOpts {
            cargo_check_enabled: false,
            package: Some("alchemy".into()),
            ..InitOpts::default()
        });

        assert_eq!(v["rust-analyzer.checkOnSave"], json!(false));
        assert_eq!(v["checkOnSave"], json!(false));
        assert_eq!(v["rust-analyzer.check.overrideCommand"], json!(null));
        assert_eq!(v["check"]["overrideCommand"], json!(null));
        assert_eq!(v["rust-analyzer.check.workspace"], json!(false));
        assert_eq!(v["cargo"]["buildScripts"]["enable"], json!(false));
        assert_eq!(v["procMacro"]["enable"], json!(false));
    }

    #[test]
    fn lean_init_options_threads_proc_macro_disabled() {
        let v = lean_init_options(&InitOpts {
            proc_macro_enabled: false,
            features: vec!["default".into()],
            ..InitOpts::default()
        });
        assert_eq!(v["procMacro"]["enable"], json!(false));
    }

    #[test]
    fn lean_init_options_threads_custom_features() {
        let v = lean_init_options(&InitOpts {
            proc_macro_enabled: true,
            features: vec!["foo".into(), "bar".into()],
            ..InitOpts::default()
        });
        // Features appear in both cargo.features and check.features.
        assert_eq!(v["cargo"]["features"], json!(["foo", "bar"]));
        assert_eq!(v["check"]["features"], json!(["foo", "bar"]));
        assert_eq!(v["rust-analyzer.check.features"], json!(["foo", "bar"]));
    }

    #[test]
    fn lean_init_options_threads_all_features() {
        let v = lean_init_options(&InitOpts {
            all_features: true,
            ..InitOpts::default()
        });
        // Both the flat and nested allFeatures keys flip to true.
        assert_eq!(v["rust-analyzer.cargo.allFeatures"], json!(true));
        assert_eq!(v["cargo"]["allFeatures"], json!(true));
    }

    #[test]
    fn lean_init_options_appends_cfgs_onto_ra_defaults() {
        let v = lean_init_options(&InitOpts {
            cfgs: vec!["erase_components".into(), "foo=bar".into()],
            ..InitOpts::default()
        });
        // Operator cfgs are APPENDED onto RA's defaults (not replacing them),
        // in both the flat and nested shapes.
        let expected = json!(["debug_assertions", "miri", "erase_components", "foo=bar"]);
        assert_eq!(v["rust-analyzer.cargo.cfgs"], expected);
        assert_eq!(v["cargo"]["cfgs"], expected);
    }

    #[test]
    fn lean_init_options_threads_tf_multiverse_check_profile() {
        let v = lean_init_options(&InitOpts {
            proc_macro_enabled: true,
            features: vec!["ssr-frontend".into(), "telephony".into()],
            package: Some("triform-server".into()),
            target: Some("wasm32-unknown-unknown".into()),
            no_default_features: true,
            release: true,
            cargo_check_enabled: true,
        });

        // Mirrors the tf-multiverse remote-check matrix:
        //   cargo check --release -p triform-server
        //   cargo check --target wasm32-unknown-unknown --no-default-features
        //              --features ...
        //
        // rust-analyzer's documented split is:
        // - package-scoped remote checks: check.overrideCommand carries the
        //   normalized `-p <package-id>`. buildScripts stay disabled to avoid
        //   a separate workspace preflight
        // - target: cargo.target / check.targets
        // - features/no-default-features: cargo.* and check/checkOnSave.*
        let override_command = json!([
            "cargo",
            "check",
            "-p",
            "triform-server",
            "--message-format=json",
            "--target",
            "wasm32-unknown-unknown",
            "--no-default-features",
            "--features",
            "ssr-frontend,telephony",
            "--release"
        ]);
        assert_eq!(v["check"]["extraArgs"], json!([]));
        assert_eq!(v["check"]["workspace"], json!(false));
        assert_eq!(v["check"]["overrideCommand"], override_command);
        assert_eq!(v["rust-analyzer.check.overrideCommand"], override_command);
        assert_eq!(v["rust-analyzer.check.workspace"], json!(false));
        assert_eq!(v["rust-analyzer.cargo.buildScripts.enable"], json!(false));
        assert_eq!(v["rust-analyzer.procMacro.enable"], json!(false));
        assert_eq!(v["procMacro"]["enable"], json!(false));
        assert_eq!(v["cargo"]["buildScripts"]["enable"], json!(false));
        assert_eq!(v["cargo"]["allTargets"], json!(false));
        assert_eq!(v["cargo"]["target"], json!("wasm32-unknown-unknown"));
        assert_eq!(v["check"]["targets"], json!("wasm32-unknown-unknown"));
        assert_eq!(v["cargo"]["noDefaultFeatures"], json!(true));
        assert_eq!(v["check"]["noDefaultFeatures"], json!(true));
        assert_eq!(v["check"]["features"], json!(["ssr-frontend", "telephony"]));
    }

    #[test]
    fn lean_init_options_normalizes_tf_multiverse_package_aliases() {
        let v = lean_init_options(&InitOpts {
            package: Some("alchemy".into()),
            ..InitOpts::default()
        });

        assert_eq!(
            v["check"]["overrideCommand"],
            json!([
                "cargo",
                "check",
                "-p",
                "triform-alchemy",
                "--message-format=json"
            ])
        );
    }

    #[test]
    fn workspace_configuration_response_serves_nested_and_flat_ra_settings() {
        let v = lean_init_options(&InitOpts {
            proc_macro_enabled: true,
            package: Some("alchemy".into()),
            ..InitOpts::default()
        });
        let req = json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "workspace/configuration",
            "params": {
                "items": [
                    { "section": "rust-analyzer" },
                    { "section": "rust-analyzer.check.workspace" }
                ]
            }
        });

        let result = workspace_configuration_result(&req, &v);

        assert_eq!(result[0]["check"]["workspace"], json!(false));
        assert_eq!(result[0]["check"]["extraArgs"], json!([]));
        assert_eq!(result[0]["checkOnSave"], json!(true));
        assert_eq!(result[1], json!(false));
        assert_eq!(result[0]["rust-analyzer.check.workspace"], json!(null));
    }

    // ----------------------------------------------------------------
    // #112-B Tier-2 — RA salsa LRU cap (RAM↔recompute, correctness-neutral)
    // ----------------------------------------------------------------

    #[test]
    fn lean_init_options_emits_bounded_lru_capacity() {
        // The new RAM lever must be present, numeric, and (with no env
        // override) the conservative default that halves RA's built-in
        // 128 — without disturbing the load-bearing checkOnSave/procMacro
        // keys the #74 shape test pins.
        let v = lean_init_options(&InitOpts::default());
        assert_eq!(
            v["lru"]["capacity"],
            json!(64),
            "Tier-2 default LRU cap (RA default is 128): {v}"
        );
        assert!(v["lru"]["capacity"].is_u64(), "must be a number");
        // Coexists with the verdict-load-bearing key (regression guard).
        assert_eq!(v["checkOnSave"], json!(true));
    }

    #[test]
    fn ra_lru_capacity_default_and_clamp_rule() {
        // `ra_lru_capacity()` itself reads process env (unsafe to mutate
        // across threads on edition 2024), so pin the pure parse/clamp
        // RULE via a mirror — same discipline as structural::enabled's
        // test. Default on unset/garbage; floor-clamped; honored when
        // sane.
        fn rule(v: Option<&str>) -> u32 {
            const DEFAULT: u32 = 64;
            const FLOOR: u32 = 16;
            match v {
                Some(s) => s.parse::<u32>().map(|n| n.max(FLOOR)).unwrap_or(DEFAULT),
                None => DEFAULT,
            }
        }
        assert_eq!(rule(None), 64, "unset ⇒ default");
        assert_eq!(rule(Some("not-a-number")), 64, "garbage ⇒ default");
        assert_eq!(rule(Some("256")), 256, "sane value honored");
        assert_eq!(rule(Some("4")), 16, "below floor ⇒ clamped to 16");
        assert_eq!(rule(Some("16")), 16, "floor exact");
        // Default really is half of RA's built-in 128 (the documented
        // RAM↔recompute trade — correctness-neutral, not a verdict knob).
        assert_eq!(rule(None) * 2, 128);
    }

    #[test]
    fn cargo_toml_signals_proc_macro_detects_leptos() {
        let cargo = r#"
            [package]
            name = "app"
            [dependencies]
            leptos = { version = "0.6", features = ["csr"] }
            serde = "1"
        "#;
        assert!(cargo_toml_signals_proc_macro(cargo));
    }

    #[test]
    fn cargo_toml_signals_proc_macro_detects_serde_derive() {
        let cargo = r#"
            [dependencies]
            serde = "1"
            serde_derive = "1"
        "#;
        assert!(cargo_toml_signals_proc_macro(cargo));
    }

    #[test]
    fn cargo_toml_signals_proc_macro_detects_lib_section() {
        let cargo = r#"
            [package]
            name = "my_macros"
            [lib]
            proc-macro = true
        "#;
        assert!(cargo_toml_signals_proc_macro(cargo));
    }

    #[test]
    fn cargo_toml_signals_proc_macro_detects_syn_quote_trio() {
        // syn/quote/proc-macro2 as direct deps strongly signals heavy
        // proc-macro use even without the obvious culprits.
        let cargo = r#"
            [dependencies]
            syn = "2"
            quote = "1"
        "#;
        assert!(cargo_toml_signals_proc_macro(cargo));
    }

    #[test]
    fn cargo_toml_signals_proc_macro_negative_simple_project() {
        // A plain CLI with serde + clap + anyhow: no proc-macro-server
        // need. serde with derive feature IS proc-macro-heavy but the
        // dep entry says `serde` not `serde_derive`; we accept that
        // false-negative (the false-positive cost of treating serde
        // alone as proc-macro signal would over-detect).
        let cargo = r#"
            [package]
            name = "cli"
            [dependencies]
            serde = "1"
            clap = "4"
            anyhow = "1"
        "#;
        assert!(!cargo_toml_signals_proc_macro(cargo));
    }

    #[test]
    fn cargo_toml_signals_proc_macro_detects_in_dev_deps() {
        let cargo = r#"
            [dev-dependencies]
            tokio-macros = "2"
        "#;
        assert!(cargo_toml_signals_proc_macro(cargo));
    }

    #[test]
    fn cargo_toml_signals_proc_macro_ignores_dep_name_outside_deps_section() {
        // "syn" as the PACKAGE name (not a dep) must not false-positive.
        let cargo = r#"
            [package]
            name = "syn"
            [dependencies]
            serde = "1"
        "#;
        assert!(
            !cargo_toml_signals_proc_macro(cargo),
            "package name 'syn' must not trigger"
        );
    }

    #[test]
    fn init_opts_from_env_handles_explicit_disabled() {
        // #180: serialized + save/restored via the shared ENV_LOCK.
        let _g = EnvGuard::set("TF_PROC_MACRO", "disabled");
        let opts = InitOpts::from_env_and_project(std::path::Path::new("/nonexistent"));
        assert!(!opts.proc_macro_enabled);
    }

    #[test]
    fn init_opts_from_env_handles_explicit_enabled() {
        let _g = EnvGuard::set("TF_PROC_MACRO", "enabled");
        let opts = InitOpts::from_env_and_project(std::path::Path::new("/nonexistent"));
        assert!(opts.proc_macro_enabled);
    }

    #[test]
    fn init_opts_features_from_env_csv() {
        // #180: also serialized — `from_env_and_project` reads
        // `TF_PROC_MACRO` too, so this must not run concurrently with the
        // proc-macro env tests (every env-mutating test shares ENV_LOCK).
        let _g = EnvGuard::set("TF_FEATURES", "csr, hydrate ssr-frontend,telephony");
        let opts = InitOpts::from_env_and_project(std::path::Path::new("/nonexistent"));
        assert_eq!(
            opts.features,
            vec![
                "csr".to_string(),
                "hydrate".to_string(),
                "ssr-frontend".to_string(),
                "telephony".to_string()
            ]
        );
    }

    #[test]
    fn init_opts_default_safe_for_macro_heavy_projects() {
        // The safe default is proc_macro_enabled=true: most projects
        // need it; defaulting OFF would silently mis-analyze leptos/
        // serde-derive code. Defaulting ON is a small idle cost vs a
        // potentially-wrong verdict.
        //
        // features default = empty (let cargo use its own defaults).
        // Passing `--features default` to cargo errors on crates that
        // don't define a [features] table — caught by F2 integration
        // test's fixture and our first-self-gate red. Empty is the
        // safe-across-every-cargo-project choice.
        let opts = InitOpts::default();
        assert!(opts.proc_macro_enabled);
        assert_eq!(opts.features, Vec::<String>::new());
        // New knobs default off/empty — preserves pre-change behavior.
        assert!(!opts.all_features);
        assert_eq!(opts.cfgs, Vec::<String>::new());
    }

    #[test]
    fn init_opts_all_features_from_env() {
        // One var per test: EnvGuard takes the (non-reentrant) ENV_LOCK, so a
        // single test must not hold two guards at once.
        let _g = EnvGuard::set("TF_ALL_FEATURES", "1");
        let opts = InitOpts::from_env_and_project(std::path::Path::new("/nonexistent"));
        assert!(opts.all_features);
    }

    #[test]
    fn init_opts_cfgs_from_env_csv() {
        let _g = EnvGuard::set("TF_RA_CFGS", "erase_components, foo=bar otherflag");
        let opts = InitOpts::from_env_and_project(std::path::Path::new("/nonexistent"));
        assert_eq!(
            opts.cfgs,
            vec![
                "erase_components".to_string(),
                "foo=bar".to_string(),
                "otherflag".to_string()
            ]
        );
    }

    #[test]
    fn handshake_then_events_over_fakes() {
        // Scripted "RA": initialize response + a rustc diag + flycheck end.
        let mut server = encode_message(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        server.extend(encode_message(
            br#"{"method":"textDocument/publishDiagnostics","params":{"uri":"file:///r/src/lib.rs","diagnostics":[{"severity":1,"source":"rustc","code":"E0599"}]}}"#,
        ));
        server.extend(encode_message(
            br#"{"method":"$/progress","params":{"token":"rustAnalyzer/cargoCheck","value":{"kind":"end"}}}"#,
        ));
        let reader = Cursor::new(server);
        let writer: Vec<u8> = Vec::new();

        let opts = InitOpts::default();
        let (client, rx) = LspClient::initialize(writer, reader, "/r", &opts).expect("handshake");
        client.next_request_id();

        let e1 = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("first event");
        match e1 {
            LspEvent::Diagnostics(pd) => {
                assert_eq!(pd.uri, "file:///r/src/lib.rs");
                assert_eq!(pd.authoritative_errors, 1);
            }
            other => panic!("expected Diagnostics, got {other:?}"),
        }
        let e2 = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("second event");
        assert_eq!(e2, LspEvent::FlycheckEnded);
    }

    #[test]
    fn handshake_streams_flycheck_failure_events_over_fakes() {
        let mut server = encode_message(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        server.extend(encode_message(
            br#"{"method":"window/showMessage","params":{"type":1,"message":"Flycheck failed to run the following command: cargo check"}}"#,
        ));
        let reader = Cursor::new(server);
        let writer: Vec<u8> = Vec::new();

        let opts = InitOpts::default();
        let (_client, rx) = LspClient::initialize(writer, reader, "/r", &opts).expect("handshake");

        let ev = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("flycheck failure event");
        assert_eq!(
            ev,
            LspEvent::FlycheckFailed {
                message: "Flycheck failed to run the following command: cargo check".into()
            }
        );
    }
}
