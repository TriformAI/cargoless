//! The cargoless binary — v0 **headless** surface:
//! `check` / `watch` (`check --watch`) / `build --watch --out <dir>` /
//! `status` / `clean`.
//!
//! v0 is a headless continuous checker + latest-green publisher: it always
//! knows what compiles and publishes the latest green build to a pointer
//! file. There is **no `serve`, no HTTP, no browser** in v0 — the live
//! server / browser-reload adapter is v0.1, layered on this output.
//!
//! Arg parsing is hand-rolled std-only on purpose: the v0 surface is five
//! commands with three flags, `Cargo.lock` is committed and CI builds
//! `--locked`, and there is no local cargo to regenerate the lock — so a new
//! parser dependency would red-line the gate for zero real benefit. This
//! matches the repo's dependency-minimal ethos (cargoless-proto is dep-free; the
//! watcher hand-rolls its gitignore/debounce).
//!
//! Naming: `cargoless` is the working repo/binary identifier; the shipping
//! product name is open decision **D1** (Plane CWDL-12). `tf` is explicitly
//! not the name (Terraform collision).

use std::path::PathBuf;
use std::process::ExitCode;

use cargoless_core::transport::{CargoSubcommand, CheckProfile};

mod appserve;
mod batchcheck;
mod build;
mod check;
mod checks;
mod clean;
mod config;
mod cratemap;
mod orphan;
mod push; // #240/2c — thin push-client (POST /overlay).
mod serve;
mod serveapi;
mod servedrv;
mod statusfile;
mod telemetry; // #246 Wave-1 5a — OTEL+SigNoz init seam.
mod ui;
mod verdict; // A1 (0.4) — one-shot merge-gate verdict client.
mod watch;

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Check,
    Watch,
    Build,
    Status,
    Clean,
    /// Native project-check manifest inspection and execution.
    Checks,
    /// Model R Stream B #3: repo-scoped daemon (`serve --repo <path>`).
    Serve,
    /// app-serve: run the apps the checker certifies — one L4-proxied
    /// instance per configured ref, latest-green-per-ref, never serve red
    /// (`app-serve --repo <path> --instances <file> --port-range A-B`).
    AppServe,
    /// #240/2c: thin push-client — push a local overlay-set to a remote
    /// `serve --repo --bind` daemon via `POST /overlay`.
    Push,
    /// Native batch gate client — POST `/batch-check` and print the
    /// machine-readable attribution report.
    BatchCheck,
    /// A1 (0.4) — one-shot gate verdict client: push the local diff,
    /// await the SHA-attributed verdict, print machine-readable JSON.
    /// Owns routing-header injection (C1) and the endpoint failover
    /// ladder so gate wrappers collapse to a single binary call.
    Verdict,
    Help,
    Version,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Opts {
    root: Option<PathBuf>,
    watch: bool,
    out: Option<PathBuf>,
    /// FIELD FINDING #5 (#49): user-tunable file-watcher debounce quiet
    /// window in milliseconds. Plumbed into the live watch/build pipeline
    /// by exporting `TF_DEBOUNCE_MS` before invoking `cargoless_core::model::watch`
    /// — keeps the `watch()` signature byte-frozen (the env-var idiom
    /// matches `TF_CHECK_TIMEOUT_SECS` from #21/#43).
    debounce_ms: Option<u64>,
    /// #74 RA weight-shedding: `auto` (default; Cargo.toml scan picks
    /// per-project), `enabled` (force on — proc-macro projects), or
    /// `disabled` (force off — non-proc-macro projects, max savings).
    /// Plumbed via `TF_PROC_MACRO` env to `cargoless_core::lsp::InitOpts`.
    proc_macro: Option<String>,
    /// #74 RA weight-shedding: feature set for RA analysis.
    /// Comma/space-separated. Plumbed via `TF_FEATURES` env.
    features: Option<String>,
    /// Package selector for RA analysis. Mirrors the tf-multiverse
    /// `check-remote` surface without turning `push` into a Cargo wrapper.
    package: Option<String>,
    /// Target triple for RA analysis.
    target: Option<String>,
    /// Disable default features for RA analysis.
    no_default_features: bool,
    /// Release-profile hint for RA analysis.
    release: bool,
    /// Compatibility marker accepted from old `check-remote` callers.
    /// `push` never treats this as permission to run Cargo; Cargoless is the
    /// replacement verdict path, not a Cargo wrapper.
    cargo_subcommand: Option<CargoSubcommand>,
    /// Compatibility selectors accepted from old `check-remote` callers.
    /// They are parsed so callers do not fail, but ignored by `push`.
    cargo_extra_args: Vec<String>,
    // ── Model R Stream B #3 `serve` flags ───────────────────────────
    // Plain Option-of-value (no clap types): main builds a
    // `serve::ServeOpts` from these, which maps to the frozen
    // `cargoless_core::FleetOverrides`. cargoless-core never gains an
    // arg-parsing dep (the frozen A↔B contract boundary).
    /// `serve --repo <path>` — repo root for the repo-scoped daemon.
    repo: Option<PathBuf>,
    /// `serve --bind HOST:PORT` — network transport addr (Stream E #10
    /// binds it; #3 resolves+carries).
    bind: Option<String>,
    /// `serve --no-corun` — disable corun batching (design §7).
    no_corun: bool,
    /// `serve --cas-dir <path>` — shared CAS dir (fleet dedup).
    cas_dir: Option<PathBuf>,
    /// `serve --state-dir <path>` — state/cache root override.
    state_dir: Option<PathBuf>,
    /// `serve --auth-token <secret>` — bearer token (#14 enforces;
    /// prefer the `CARGOLESS_AUTH_TOKEN` env for secrets).
    auth_token: Option<String>,
    /// `app-serve --instances <file>` — the instances file (which refs to
    /// serve). Absent ⇒ zero-config single `default` instance on repo HEAD.
    instances: Option<PathBuf>,
    /// `app-serve --port-range START-END` — app child ports the daemon
    /// allocates (one per running instance build).
    port_range: Option<String>,
    /// `app-serve --poll-interval-ms <N>` — ref poll cadence (default 2000).
    poll_interval_ms: Option<u64>,
    /// `app-serve --max-concurrent-builds N` (also `CARGOLESS_APP_PARALLEL_BUILDS`):
    /// how many instances may build at once. Default 0 = use the env var or
    /// fall back to 1 (serialised, today's behaviour). Also see
    /// `AppServeOpts::max_concurrent_builds`.
    max_concurrent_builds: usize,
    /// `status --remote <url>` — query a remote `serve --bind` fleet
    /// daemon over the shipped HTTP(S) transport instead of the on-disk
    /// `cli-status`. Resolved through `transport::discovery` (explicit
    /// operator intent — `--remote` wins the §10.3 precedence).
    remote: Option<String>,
    /// `push --worktree <key>` — explicit server-side worktree key. If
    /// absent, defaults to the canonical absolute `--repo` path
    /// (path-keyed identity, D-INC2-2B §11 open-Q1 default).
    push_worktree: Option<String>,
    /// `push --base <ref>` / `checks run --base <ref>` — git base ref for
    /// `git diff --name-only`. Push defaults to `HEAD`; checks default to a
    /// full profile unless this is provided. In checks mode the changed-file
    /// list prunes project checks whose triggers do not match the branch diff.
    push_base: Option<String>,
    /// `push --server-root <path>` — server-side repo root for central
    /// daemon mode.
    push_server_root: Option<PathBuf>,
    /// `push --await-verdict` — block until the remote publishes a fresh
    /// verdict for this pushed worktree.
    push_await_verdict: bool,
    /// `push --gate` — merge-gate push: the daemon promotes this push's
    /// project-check mode from Warn to Hard (witness-gated verdict).
    push_gate: bool,
    /// `push --await-timeout-secs <N>` — max wait for fresh verdict.
    push_await_timeout_secs: Option<u64>,
    /// `batch-check --request-json <PATH>` — native batch_check transport
    /// request body to send to a remote daemon.
    batch_request_json: Option<PathBuf>,
    // ── A1 (0.4) `verdict` flags ────────────────────────────────────
    /// `verdict --remote <url>` repeatable — failover ladder, tried in
    /// order (first = primary). Separate from the scalar `remote` so the
    /// status/push/batch-check single-endpoint grammar is untouched.
    verdict_remotes: Vec<String>,
    /// `verdict --header "Name: value"` repeatable — raw client-wide HTTP
    /// headers (C1: the pool ingress consistent-hashes
    /// `X-Cargoless-Routing-Key`, so headers must ride every request,
    /// pushes AND status polls). Parsed/validated in `verdict::run`.
    verdict_headers: Vec<String>,
    /// `verdict --output json|text` — stdout format (default json).
    verdict_output: Option<String>,
    /// `verdict --check-id <id>` repeatable — B3 witness check-ids,
    /// attached to the push as `PushOverlayOptions::check_ids`.
    verdict_check_ids: Vec<String>,
    /// `verdict -- <repo>` — positional repo path after the `--`
    /// separator (the thin-wrapper calling convention).
    verdict_repo_positional: Option<PathBuf>,
    /// `checks list|run|explain`.
    checks_action: Option<String>,
    /// Optional check id for `checks run <id>` / `checks explain <id>`.
    checks_id: Option<String>,
    /// Optional profile for `checks run --profile <name>`.
    checks_profile: Option<String>,
    /// `checks run --allow-existing-red` — compare required reds to `--base`
    /// and exit green when every red already exists at the base ref.
    checks_allow_existing_red: bool,
    /// `checks run --report-json <path>` — write machine-readable check
    /// decision data for repo-specific ticketing wrappers.
    checks_report_json: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
struct Parsed {
    cmd: Cmd,
    opts: Opts,
}

#[derive(Debug, PartialEq, Eq)]
enum ParseError {
    UnknownCommand(String),
    UnknownFlag(String),
    MissingValue(&'static str),
}

/// Pure arg parser (no I/O) so the grammar is unit-tested deterministically.
fn parse(args: &[String]) -> Result<Parsed, ParseError> {
    let mut it = args.iter();
    let Some(first) = it.next() else {
        return Ok(Parsed {
            cmd: Cmd::Help,
            opts: Opts::default(),
        });
    };

    let cmd = match first.as_str() {
        "check" => Cmd::Check,
        "watch" => Cmd::Watch,
        "build" => Cmd::Build,
        "status" => Cmd::Status,
        "clean" => Cmd::Clean,
        "checks" => Cmd::Checks,
        "serve" => Cmd::Serve,
        "app-serve" => Cmd::AppServe,
        "push" => Cmd::Push,
        "batch-check" => Cmd::BatchCheck,
        "verdict" => Cmd::Verdict,
        "help" | "-h" | "--help" => Cmd::Help,
        "version" | "-V" | "--version" => Cmd::Version,
        other => return Err(ParseError::UnknownCommand(other.to_string())),
    };

    let mut opts = Opts::default();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--watch" => opts.watch = true,
            "--root" => {
                opts.root = Some(PathBuf::from(
                    it.next().ok_or(ParseError::MissingValue("--root"))?,
                ));
            }
            "--out" => {
                opts.out = Some(PathBuf::from(
                    it.next().ok_or(ParseError::MissingValue("--out"))?,
                ));
            }
            "--debounce-ms" => {
                let v = it.next().ok_or(ParseError::MissingValue("--debounce-ms"))?;
                opts.debounce_ms = Some(
                    v.parse::<u64>()
                        .map_err(|_| ParseError::MissingValue("--debounce-ms (numeric ms)"))?,
                );
            }
            "--proc-macro" => {
                let v = it.next().ok_or(ParseError::MissingValue("--proc-macro"))?;
                match v.as_str() {
                    "auto" | "enabled" | "disabled" => opts.proc_macro = Some(v.clone()),
                    _ => {
                        return Err(ParseError::MissingValue(
                            "--proc-macro (auto|enabled|disabled)",
                        ));
                    }
                }
            }
            "--features" => {
                let v = it.next().ok_or(ParseError::MissingValue("--features"))?;
                opts.features = Some(v.clone());
            }
            a if a.starts_with("--features=") => {
                opts.features = Some(a["--features=".len()..].to_string());
            }
            "-p" | "--package" => {
                opts.package = Some(
                    it.next()
                        .ok_or(ParseError::MissingValue("--package"))?
                        .clone(),
                );
            }
            a if a.starts_with("--package=") => {
                opts.package = Some(a["--package=".len()..].to_string());
            }
            a if a.starts_with("-p=") => {
                opts.package = Some(a["-p=".len()..].to_string());
            }
            "--target" => {
                opts.target = Some(
                    it.next()
                        .ok_or(ParseError::MissingValue("--target"))?
                        .clone(),
                );
            }
            a if a.starts_with("--target=") => {
                opts.target = Some(a["--target=".len()..].to_string());
            }
            "--no-default-features" => opts.no_default_features = true,
            "--release" => opts.release = true,
            "--cargo-subcommand" => {
                let v = it
                    .next()
                    .ok_or(ParseError::MissingValue("--cargo-subcommand"))?;
                opts.cargo_subcommand = Some(CargoSubcommand::parse(v).ok_or(
                    ParseError::MissingValue("--cargo-subcommand (check|clippy)"),
                )?);
            }
            "--profile" if cmd == Cmd::Checks => {
                opts.checks_profile = Some(
                    it.next()
                        .ok_or(ParseError::MissingValue("--profile"))?
                        .clone(),
                );
            }
            "--allow-existing-red" if cmd == Cmd::Checks => {
                opts.checks_allow_existing_red = true;
            }
            "--report-json" if cmd == Cmd::Checks => {
                opts.checks_report_json = Some(PathBuf::from(
                    it.next().ok_or(ParseError::MissingValue("--report-json"))?,
                ));
            }
            a if cargo_extra_arg_takes_value(a).is_some() => {
                let flag = cargo_extra_arg_takes_value(a).unwrap();
                opts.cargo_extra_args.push(flag.to_string());
                opts.cargo_extra_args
                    .push(it.next().ok_or(ParseError::MissingValue(flag))?.clone());
            }
            a if cargo_extra_arg_equals_form(a) || cargo_extra_arg_flag(a) => {
                opts.cargo_extra_args.push(a.to_string());
            }
            // ── Model R Stream B #3 `serve` flags ───────────────────
            "--repo" => {
                opts.repo = Some(PathBuf::from(
                    it.next().ok_or(ParseError::MissingValue("--repo"))?,
                ));
            }
            "--bind" => {
                opts.bind = Some(it.next().ok_or(ParseError::MissingValue("--bind"))?.clone());
            }
            "--no-corun" => opts.no_corun = true,
            "--cas-dir" => {
                opts.cas_dir = Some(PathBuf::from(
                    it.next().ok_or(ParseError::MissingValue("--cas-dir"))?,
                ));
            }
            "--state-dir" => {
                opts.state_dir = Some(PathBuf::from(
                    it.next().ok_or(ParseError::MissingValue("--state-dir"))?,
                ));
            }
            "--auth-token" => {
                opts.auth_token = Some(
                    it.next()
                        .ok_or(ParseError::MissingValue("--auth-token"))?
                        .clone(),
                );
            }
            "--instances" => {
                opts.instances = Some(PathBuf::from(
                    it.next().ok_or(ParseError::MissingValue("--instances"))?,
                ));
            }
            "--port-range" => {
                opts.port_range = Some(
                    it.next()
                        .ok_or(ParseError::MissingValue("--port-range"))?
                        .clone(),
                );
            }
            "--poll-interval-ms" => {
                opts.poll_interval_ms = Some(
                    it.next()
                        .ok_or(ParseError::MissingValue("--poll-interval-ms"))?
                        .parse()
                        .map_err(|_| ParseError::MissingValue("--poll-interval-ms"))?,
                );
            }
            "--max-concurrent-builds" => {
                opts.max_concurrent_builds = it
                    .next()
                    .ok_or(ParseError::MissingValue("--max-concurrent-builds"))?
                    .parse()
                    .map_err(|_| ParseError::MissingValue("--max-concurrent-builds"))?;
            }
            // verdict: --remote is repeatable (failover ladder) — must
            // precede the scalar arm below (match arms are tried in order).
            "--remote" if cmd == Cmd::Verdict => {
                opts.verdict_remotes.push(
                    it.next()
                        .ok_or(ParseError::MissingValue("--remote"))?
                        .clone(),
                );
            }
            "--remote" => {
                opts.remote = Some(
                    it.next()
                        .ok_or(ParseError::MissingValue("--remote"))?
                        .clone(),
                );
            }
            // ── Model R / #240 2c `push` flags ──────────────────────
            "--worktree" => {
                opts.push_worktree = Some(
                    it.next()
                        .ok_or(ParseError::MissingValue("--worktree"))?
                        .clone(),
                );
            }
            "--base" => {
                opts.push_base = Some(it.next().ok_or(ParseError::MissingValue("--base"))?.clone());
            }
            "--server-root" => {
                opts.push_server_root = Some(PathBuf::from(
                    it.next().ok_or(ParseError::MissingValue("--server-root"))?,
                ));
            }
            "--await-verdict" => opts.push_await_verdict = true,
            "--gate" => opts.push_gate = true,
            "--request-json" => {
                opts.batch_request_json = Some(PathBuf::from(
                    it.next()
                        .ok_or(ParseError::MissingValue("--request-json"))?,
                ));
            }
            "--await-timeout-secs" => {
                let v = it
                    .next()
                    .ok_or(ParseError::MissingValue("--await-timeout-secs"))?;
                opts.push_await_timeout_secs = Some(v.parse::<u64>().map_err(|_| {
                    ParseError::MissingValue("--await-timeout-secs (numeric seconds)")
                })?);
            }
            // ── A1 (0.4) `verdict` flags ────────────────────────────
            "--header" if cmd == Cmd::Verdict => {
                opts.verdict_headers.push(
                    it.next()
                        .ok_or(ParseError::MissingValue("--header"))?
                        .clone(),
                );
            }
            "--output" if cmd == Cmd::Verdict => {
                let v = it.next().ok_or(ParseError::MissingValue("--output"))?;
                match v.as_str() {
                    "json" | "text" => opts.verdict_output = Some(v.clone()),
                    _ => return Err(ParseError::MissingValue("--output (json|text)")),
                }
            }
            "--check-id" if cmd == Cmd::Verdict => {
                opts.verdict_check_ids.push(
                    it.next()
                        .ok_or(ParseError::MissingValue("--check-id"))?
                        .clone(),
                );
            }
            // `verdict ... -- <repo>`: everything after the separator is
            // positional; exactly one (the repo path) is accepted.
            "--" if cmd == Cmd::Verdict => {
                for rest in it.by_ref() {
                    if opts.verdict_repo_positional.is_none() {
                        opts.verdict_repo_positional = Some(PathBuf::from(rest));
                    } else {
                        return Err(ParseError::UnknownFlag(rest.clone()));
                    }
                }
            }
            other
                if cmd == Cmd::Checks
                    && !other.starts_with('-')
                    && opts.checks_action.is_none() =>
            {
                opts.checks_action = Some(other.to_string());
            }
            other if cmd == Cmd::Checks && !other.starts_with('-') && opts.checks_id.is_none() => {
                opts.checks_id = Some(other.to_string());
            }
            "-h" | "--help" => {
                return Ok(Parsed {
                    cmd: Cmd::Help,
                    opts,
                });
            }
            other => return Err(ParseError::UnknownFlag(other.to_string())),
        }
    }
    Ok(Parsed { cmd, opts })
}

fn usage() {
    println!("{}", cargoless_core::build_id());
    println!();
    println!("USAGE: cargoless <COMMAND> [FLAGS]");
    println!();
    println!("  check                 One-shot verdict; exit 0=green 1=red 2=setup-error");
    println!("  check --watch         Continuous headless verdict stream (alias: watch)");
    println!("  watch                 Continuous headless verdict stream");
    println!("  build --watch --out <DIR>");
    println!("                        Maintain the latest-green artifact in <DIR>");
    println!("  status                Daemon liveness + current verdict + latest-green");
    println!("  clean                 Remove the local content-addressed cache");
    println!("  checks list|run|explain");
    println!("                        Inspect or run cargoless.checks.yaml project checks");
    println!("  serve --repo <DIR>    Model R repo-scoped daemon: auto-discovers");
    println!("                        worktrees, one shared daemon for the fleet");
    println!("  app-serve --repo <DIR> --instances <FILE> --port-range A-B");
    println!("                        Run the apps the checker certifies: one");
    println!("                        L4-proxied instance per ref, never serve red");
    println!("  batch-check --remote <URL> --request-json <PATH>");
    println!("                        Native optimistic batch gate; prints report JSON");
    println!("  verdict --remote <URL> [--remote <URL>...] [-- <REPO>]");
    println!("                        One-shot gate verdict: push the diff, await the");
    println!("                        SHA-attributed verdict, print one JSON object;");
    println!("                        exit 0=green 1=red 75=unknown/infra 2=setup");
    println!();
    println!("FLAGS:");
    println!("  --root <DIR>          Project root (default: current directory)");
    println!("  --watch               Run continuously instead of one-shot");
    println!("  --out <DIR>           Artifact output directory (build only)");
    println!(
        "  --debounce-ms <N>     Save-burst quiet window before re-checking \
         (default 150ms;"
    );
    println!(
        "                        tune up if mid-edit reds flicker, down for \
         faster verdicts;"
    );
    println!("                        also settable via TF_DEBOUNCE_MS env)");
    println!(
        "  --proc-macro <MODE>   rust-analyzer proc-macro server: \
         auto|enabled|disabled"
    );
    println!(
        "                        (default auto = Cargo.toml-scan picks; \
         also TF_PROC_MACRO env)"
    );
    println!("  --features <FEATS>    feature set for RA analysis (comma/space-separated;");
    println!("                        also TF_FEATURES env)");
    println!("  -p, --package <PKG>   package selector for RA analysis (TF_CHECK_PACKAGE)");
    println!("  --target <TRIPLE>     target triple for RA analysis (TF_CHECK_TARGET)");
    println!("  --release             release-profile hint for RA analysis");
    println!("                        (TF_CHECK_RELEASE=1)");
    println!("  --no-default-features disable default features for RA analysis");
    println!("                        (TF_CHECK_NO_DEFAULT_FEATURES=1)");
    println!(
        "  --remote <URL>        status: query a remote `serve --bind` daemon \
         over HTTP(S)"
    );
    println!(
        "                        (e.g. https://host or http://host:8080) instead of the local \
         cli-status file"
    );
    println!("  --worktree <KEY>      status/push: query or push one served worktree");
    println!("  --base <REF>          push/checks: git base ref for changed-file pruning");
    println!("  --allow-existing-red  checks: allow reds already present at --base");
    println!("  --report-json <PATH>  checks: write machine-readable check decision data");
    println!("  --server-root <DIR>   push: server-side repo root for central daemon mode");
    println!("  --request-json <PATH> batch-check: transport batch_check request JSON");
    println!("  -h, --help            Show this help");
    println!("  -V, --version         Show the build identifier");
    println!();
    println!("SERVE FLAGS (Model R repo-scoped daemon):");
    println!("  --repo <DIR>          Repo root to serve (required for serve)");
    println!(
        "  --bind HOST:PORT      Network transport addr (default: none — \
         loopback/in-proc;"
    );
    println!("                        non-loopback requires --auth-token; also TF_BIND)");
    println!("  --no-corun            Disable corun batching (also TF_NO_CORUN)");
    println!("  --cas-dir <DIR>       Shared CAS dir for fleet dedup (also TF_CAS_DIR)");
    println!("  --state-dir <DIR>     State/cache root (also TF_STATE_DIR)");
    println!(
        "  --auth-token <SECRET> Bearer token for authed HTTP \
         (prefer CARGOLESS_AUTH_TOKEN env)"
    );
    println!("  --await-verdict      push: wait for a fresh remote verdict");
    println!("  --await-timeout-secs <N>");
    println!("                        push/verdict: max verdict wait (default 900)");
    println!("  --gate               push/verdict: witness-gated (Hard) verdict");
    println!();
    println!("VERDICT FLAGS (one-shot gate client):");
    println!("  --remote <URL>        repeatable: failover ladder, tried in order");
    println!("  --header \"Name: v\"    repeatable: HTTP header on EVERY request (push +");
    println!("                        status polls — e.g. X-Cargoless-Routing-Key for");
    println!("                        shard-affine pool routing)");
    println!("  --output json|text    stdout format (default json: one object with");
    println!("                        verdict/base_sha/remote/source keys)");
    println!("  --check-id <ID>       repeatable: witness check-ids attached to the push");
    println!("  -- <REPO>             positional repo path (after the -- separator)");
    println!();
    println!(
        "check/watch/build/status/clean are single-project (headless, no \
         HTTP/browser)."
    );
    println!(
        "serve is the Model R repo-scoped daemon (one shared daemon \
         auto-discovering the fleet)."
    );
}

fn apply_runtime_env(opts: &Opts) {
    // FIELD FINDING #5 (#49): the `--debounce-ms` flag (when given) is
    // plumbed to `cargoless_core::model::watch` via the `TF_DEBOUNCE_MS` env var,
    // keeping the frozen `watch()` signature unchanged. Idiomatic match to
    // `TF_CHECK_TIMEOUT_SECS` (the #21/#43 path). Setting an env var from
    // a CLI is process-local; no risk of leaking outward.
    if let Some(ms) = opts.debounce_ms {
        // SAFETY: single-threaded init phase, no other threads observe env
        // yet. set_var is unsafe on 2024 edition due to multi-thread reads.
        unsafe {
            std::env::set_var("TF_DEBOUNCE_MS", ms.to_string());
        }
    }
    // #74 RA weight-shedding + tf-multiverse cargo-profile compatibility.
    // The CLI exports env vars; cargoless_core::lsp::InitOpts consumes
    // them while constructing RA initializationOptions. This keeps the
    // core API stable while giving `check`, `watch`, and `serve` the same
    // cargo-shaped selectors as `scripts/check-remote`.
    if let Some(pm) = opts.proc_macro.as_deref() {
        unsafe {
            std::env::set_var("TF_PROC_MACRO", pm);
        }
    }
    if let Some(fs) = opts.features.as_deref() {
        unsafe {
            std::env::set_var("TF_FEATURES", fs);
        }
    }
    if let Some(package) = opts.package.as_deref() {
        unsafe {
            std::env::set_var("TF_CHECK_PACKAGE", package);
        }
    }
    if let Some(target) = opts.target.as_deref() {
        unsafe {
            std::env::set_var("TF_CHECK_TARGET", target);
        }
    }
    if opts.no_default_features {
        unsafe {
            std::env::set_var("TF_CHECK_NO_DEFAULT_FEATURES", "1");
        }
    }
    if opts.release {
        unsafe {
            std::env::set_var("TF_CHECK_RELEASE", "1");
        }
    }
}

fn auth_token_for_push(cli: Option<String>) -> Option<String> {
    cli.or_else(|| std::env::var("CARGOLESS_AUTH_TOKEN").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn cargo_extra_arg_takes_value(flag: &str) -> Option<&'static str> {
    match flag {
        "--manifest-path" => Some("--manifest-path"),
        "--bin" => Some("--bin"),
        "--example" => Some("--example"),
        "--test" => Some("--test"),
        "--bench" => Some("--bench"),
        "--profile" => Some("--profile"),
        "--exclude" => Some("--exclude"),
        _ => None,
    }
}

fn cargo_extra_arg_equals_form(arg: &str) -> bool {
    [
        "--manifest-path=",
        "--bin=",
        "--example=",
        "--test=",
        "--bench=",
        "--profile=",
        "--exclude=",
    ]
    .iter()
    .any(|prefix| arg.starts_with(prefix))
}

fn cargo_extra_arg_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--lib"
            | "--bins"
            | "--examples"
            | "--tests"
            | "--benches"
            | "--all-targets"
            | "--workspace"
            | "--all"
            | "--all-features"
            | "--locked"
            | "--offline"
            | "--frozen"
            | "--keep-going"
    )
}

fn check_profile_from_opts(_opts: &Opts) -> Option<CheckProfile> {
    // Cargoless replaces iterative cargo check/clippy. Push-time cargo
    // selectors remain accepted for caller compatibility, but they must never
    // request a direct Cargo subprocess from the daemon.
    None
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match parse(&args) {
        Ok(p) => p,
        Err(e) => {
            match e {
                ParseError::UnknownCommand(c) => ui::error(format!("unknown command: {c}")),
                ParseError::UnknownFlag(f) => ui::error(format!("unknown flag: {f}")),
                ParseError::MissingValue(f) => ui::error(format!("{f} requires a value")),
            }
            usage();
            return ExitCode::from(2);
        }
    };

    apply_runtime_env(&parsed.opts);

    match &parsed.cmd {
        Cmd::Help => {
            usage();
            return ExitCode::SUCCESS;
        }
        Cmd::Version => {
            println!("{}", cargoless_core::build_id());
            return ExitCode::SUCCESS;
        }
        // Model R Stream B #3: `serve` is repo-scoped (FleetConfig), NOT a
        // single-WASM-project command — it must dispatch BEFORE the v0
        // `config::Config::resolve` front-door below (that detector would
        // wrongly reject a repo root that isn't a cdylib/leptos crate).
        // serve owns its own config resolution via FleetConfig.
        Cmd::Serve => {
            return serve::run(&serve::ServeOpts {
                repo: parsed.opts.repo.clone(),
                bind: parsed.opts.bind.clone(),
                no_corun: parsed.opts.no_corun,
                cas_dir: parsed.opts.cas_dir.clone(),
                state_dir: parsed.opts.state_dir.clone(),
                auth_token: parsed.opts.auth_token.clone(),
            });
        }
        // app-serve is repo-scoped like serve — dispatch BEFORE the v0
        // `config::Config::resolve` front-door (that detector would wrongly
        // reject a repo root that isn't a single cdylib/leptos crate;
        // app-serve resolves its own instance config).
        Cmd::AppServe => {
            return appserve::run(&appserve::AppServeOpts {
                repo: parsed.opts.repo.clone(),
                bind: parsed.opts.bind.clone(),
                instances: parsed.opts.instances.clone(),
                port_range: parsed.opts.port_range.clone(),
                state_dir: parsed.opts.state_dir.clone(),
                auth_token: parsed.opts.auth_token.clone(),
                poll_interval_ms: parsed.opts.poll_interval_ms,
                max_concurrent_builds: parsed.opts.max_concurrent_builds,
            });
        }
        // `status --remote <url>` queries a remote fleet `serve --bind`
        // daemon over the shipped HTTP(S) transport. Dispatch BEFORE the
        // `config::Config::resolve` front-door (exactly like `serve`):
        // that detector would wrongly reject a non-WASM cwd, and asking a
        // *remote* daemon must not require a local cargoless project.
        Cmd::Status => {
            if let Some(url) = parsed.opts.remote.as_deref() {
                return statusfile::run_status_remote(url, parsed.opts.push_worktree.as_deref());
            }
        }
        // #240/2c — `push --remote <url>` pushes a local overlay-set to
        // a remote daemon. Dispatch BEFORE the `config::Config::resolve`
        // front-door (same rationale as serve/status --remote): push is
        // a server-protocol command, not a local-WASM-project command.
        // --remote is REQUIRED for push (no local fallback).
        Cmd::Push => {
            let Some(remote) = parsed.opts.remote.clone() else {
                ui::error("push: --remote <url> is required");
                return ExitCode::from(2);
            };
            let repo = parsed
                .opts
                .repo
                .clone()
                .or_else(|| parsed.opts.root.clone())
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            // Default worktree-key = canonical absolute repo path
            // (D-INC2-2B path-keyed identity). std::fs::canonicalize
            // resolves symlinks + relative components; fall back to the
            // raw path if canonicalize fails (e.g. ephemeral test dirs).
            let worktree = parsed.opts.push_worktree.clone().unwrap_or_else(|| {
                std::fs::canonicalize(&repo)
                    .unwrap_or_else(|_| repo.clone())
                    .to_string_lossy()
                    .into_owned()
            });
            let base = parsed
                .opts
                .push_base
                .clone()
                .unwrap_or_else(|| "HEAD".to_string());
            return push::run(&push::PushOpts {
                remote,
                auth_token: auth_token_for_push(parsed.opts.auth_token.clone()),
                repo,
                worktree,
                base,
                check_profile: check_profile_from_opts(&parsed.opts),
                server_root: parsed.opts.push_server_root.clone(),
                await_verdict: parsed.opts.push_await_verdict,
                await_timeout_secs: parsed.opts.push_await_timeout_secs.unwrap_or(900),
                gate: parsed.opts.push_gate,
            });
        }
        Cmd::BatchCheck => {
            let Some(remote) = parsed.opts.remote.clone() else {
                ui::error("batch-check: --remote <url> is required");
                return ExitCode::from(2);
            };
            let Some(request_json) = parsed.opts.batch_request_json.clone() else {
                ui::error("batch-check: --request-json <path> is required");
                return ExitCode::from(2);
            };
            return batchcheck::run(&batchcheck::BatchCheckOpts {
                remote,
                auth_token: auth_token_for_push(parsed.opts.auth_token.clone()),
                request_json,
            });
        }
        // A1 (0.4) — `verdict` is a server-protocol command like push:
        // dispatch BEFORE the `config::Config::resolve` front-door. Repo
        // resolution mirrors push exactly; the positional `-- <repo>`
        // form (the thin-wrapper convention) wins over --repo/--root.
        Cmd::Verdict => {
            let repo = parsed
                .opts
                .verdict_repo_positional
                .clone()
                .or_else(|| parsed.opts.repo.clone())
                .or_else(|| parsed.opts.root.clone())
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            let worktree = parsed.opts.push_worktree.clone().unwrap_or_else(|| {
                std::fs::canonicalize(&repo)
                    .unwrap_or_else(|_| repo.clone())
                    .to_string_lossy()
                    .into_owned()
            });
            let base = parsed
                .opts
                .push_base
                .clone()
                .unwrap_or_else(|| "HEAD".to_string());
            let output = match parsed.opts.verdict_output.as_deref() {
                Some(mode) => match verdict::OutputMode::parse(mode) {
                    Some(mode) => mode,
                    None => {
                        ui::error(format!("verdict: --output must be json|text, got {mode}"));
                        return ExitCode::from(2);
                    }
                },
                None => verdict::OutputMode::Json,
            };
            return verdict::run(&verdict::VerdictOpts {
                remotes: parsed.opts.verdict_remotes.clone(),
                headers: parsed.opts.verdict_headers.clone(),
                output,
                auth_token: auth_token_for_push(parsed.opts.auth_token.clone()),
                repo,
                worktree,
                base,
                server_root: parsed.opts.push_server_root.clone(),
                gate: parsed.opts.push_gate,
                check_ids: parsed.opts.verdict_check_ids.clone(),
                await_timeout_secs: parsed.opts.push_await_timeout_secs.unwrap_or(900),
            });
        }
        _ => {}
    }

    // Config resolution is the shared front door; its error is the entire
    // onboarding UX for a zero-config tool, surfaced once here.
    let root = parsed
        .opts
        .root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let cfg = match config::Config::resolve(&root) {
        Ok(c) => c,
        Err(e) => {
            ui::error(e.to_string());
            return ExitCode::from(2);
        }
    };

    match parsed.cmd {
        Cmd::Check if parsed.opts.watch => watch::run(&cfg),
        Cmd::Check => check::run(&cfg),
        Cmd::Watch => watch::run(&cfg),
        Cmd::Build => build::run(&cfg, parsed.opts.out.as_deref()),
        Cmd::Status => statusfile::run_status(&cfg),
        Cmd::Checks => checks::run(
            &cfg,
            parsed.opts.checks_action.as_deref(),
            parsed.opts.checks_id.as_deref(),
            parsed.opts.checks_profile.as_deref(),
            parsed.opts.push_base.as_deref(),
            parsed.opts.checks_allow_existing_red,
            parsed.opts.checks_report_json.as_deref(),
        ),
        Cmd::Clean => clean::run(&cfg),
        Cmd::Help
        | Cmd::Version
        | Cmd::Serve
        | Cmd::AppServe
        | Cmd::Push
        | Cmd::BatchCheck
        | Cmd::Verdict => {
            unreachable!("handled above")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn empty_and_help_are_help() {
        assert_eq!(parse(&v(&[])).unwrap().cmd, Cmd::Help);
        assert_eq!(parse(&v(&["--help"])).unwrap().cmd, Cmd::Help);
        assert_eq!(parse(&v(&["-h"])).unwrap().cmd, Cmd::Help);
    }

    #[test]
    fn commands_parse() {
        for (s, c) in [
            ("check", Cmd::Check),
            ("watch", Cmd::Watch),
            ("build", Cmd::Build),
            ("status", Cmd::Status),
            ("clean", Cmd::Clean),
            ("checks", Cmd::Checks),
            ("serve", Cmd::Serve),
            ("push", Cmd::Push),
            ("verdict", Cmd::Verdict),
            ("version", Cmd::Version),
        ] {
            assert_eq!(parse(&v(&[s])).unwrap().cmd, c);
        }
    }

    // -----------------------------------------------------------------------
    // A1 (0.4) — `verdict` grammar: repeatable --remote/--header/--check-id,
    // --output validation, positional `-- <repo>` capture
    // -----------------------------------------------------------------------

    #[test]
    fn verdict_full_grammar_parses() {
        let p = parse(&v(&[
            "verdict",
            "--output",
            "json",
            "--header",
            "X-Cargoless-Routing-Key: route-7",
            "--header",
            "X-Trace: abc",
            "--remote",
            "http://pool:8787",
            "--remote",
            "http://shard-0:8787",
            "--check-id",
            "portal-compile",
            "--gate",
            "--base",
            "origin/dev",
            "--await-timeout-secs",
            "300",
            "--",
            "/workspace/repo",
        ]))
        .unwrap();
        assert_eq!(p.cmd, Cmd::Verdict);
        assert_eq!(p.opts.verdict_output.as_deref(), Some("json"));
        assert_eq!(
            p.opts.verdict_headers,
            vec![
                "X-Cargoless-Routing-Key: route-7".to_string(),
                "X-Trace: abc".to_string()
            ]
        );
        // Ladder order is failover order — preserved verbatim.
        assert_eq!(
            p.opts.verdict_remotes,
            vec![
                "http://pool:8787".to_string(),
                "http://shard-0:8787".to_string()
            ]
        );
        assert_eq!(p.opts.verdict_check_ids, vec!["portal-compile".to_string()]);
        assert!(p.opts.push_gate);
        assert_eq!(p.opts.push_base.as_deref(), Some("origin/dev"));
        assert_eq!(p.opts.push_await_timeout_secs, Some(300));
        assert_eq!(
            p.opts.verdict_repo_positional,
            Some(PathBuf::from("/workspace/repo"))
        );
        // The scalar --remote used by status/push/batch-check stays unset:
        // verdict's repeatable arm captured every occurrence.
        assert_eq!(p.opts.remote, None);
    }

    #[test]
    fn verdict_output_rejects_unknown_mode() {
        let r = parse(&v(&["verdict", "--output", "yaml"]));
        assert!(matches!(r, Err(ParseError::MissingValue(s)) if s.contains("--output")));
    }

    #[test]
    fn verdict_rejects_second_positional() {
        let r = parse(&v(&["verdict", "--", "/repo-a", "/repo-b"]));
        assert_eq!(r, Err(ParseError::UnknownFlag("/repo-b".into())));
    }

    #[test]
    fn verdict_flags_are_scoped_to_verdict() {
        // --header/--output/--check-id exist only in the verdict grammar;
        // other commands must keep rejecting them (no surface creep).
        assert_eq!(
            parse(&v(&["push", "--header", "X: y"])),
            Err(ParseError::UnknownFlag("--header".into()))
        );
        assert_eq!(
            parse(&v(&["status", "--output", "json"])),
            Err(ParseError::UnknownFlag("--output".into()))
        );
        // And push's scalar --remote semantics are untouched.
        let p = parse(&v(&["push", "--remote", "http://h:1"])).unwrap();
        assert_eq!(p.opts.remote.as_deref(), Some("http://h:1"));
        assert!(p.opts.verdict_remotes.is_empty());
    }

    #[test]
    fn check_watch_flag_and_root() {
        let p = parse(&v(&["check", "--watch", "--root", "/p"])).unwrap();
        assert_eq!(p.cmd, Cmd::Check);
        assert!(p.opts.watch);
        assert_eq!(p.opts.root, Some(PathBuf::from("/p")));
    }

    #[test]
    fn build_out_flag() {
        let p = parse(&v(&["build", "--watch", "--out", "dist"])).unwrap();
        assert_eq!(p.cmd, Cmd::Build);
        assert!(p.opts.watch);
        assert_eq!(p.opts.out, Some(PathBuf::from("dist")));
    }

    #[test]
    fn errors_are_actionable() {
        assert_eq!(
            parse(&v(&["frob"])),
            Err(ParseError::UnknownCommand("frob".into()))
        );
        assert_eq!(
            parse(&v(&["check", "--nope"])),
            Err(ParseError::UnknownFlag("--nope".into()))
        );
        assert_eq!(
            parse(&v(&["check", "--root"])),
            Err(ParseError::MissingValue("--root"))
        );
    }

    // -----------------------------------------------------------------------
    // FIELD FINDING #5 (#49) — --debounce-ms parses, validates, defaults
    // -----------------------------------------------------------------------

    #[test]
    fn debounce_ms_parses_into_opts() {
        let p = parse(&v(&["watch", "--debounce-ms", "300"])).unwrap();
        assert_eq!(p.cmd, Cmd::Watch);
        assert_eq!(p.opts.debounce_ms, Some(300));
    }

    #[test]
    fn debounce_ms_works_alongside_other_flags() {
        // Order independence + composability with --root / --watch.
        let p = parse(&v(&[
            "build",
            "--watch",
            "--debounce-ms",
            "750",
            "--root",
            "/p",
            "--out",
            "dist",
        ]))
        .unwrap();
        assert_eq!(p.cmd, Cmd::Build);
        assert!(p.opts.watch);
        assert_eq!(p.opts.debounce_ms, Some(750));
        assert_eq!(p.opts.root.as_deref(), Some(std::path::Path::new("/p")));
        assert_eq!(p.opts.out.as_deref(), Some(std::path::Path::new("dist")));
    }

    #[test]
    fn debounce_ms_missing_value_is_actionable() {
        assert_eq!(
            parse(&v(&["watch", "--debounce-ms"])),
            Err(ParseError::MissingValue("--debounce-ms"))
        );
    }

    #[test]
    fn debounce_ms_non_numeric_is_actionable() {
        // The error variant carries enough context for the user to know
        // what failed (numeric ms expected, not free-form text).
        let r = parse(&v(&["watch", "--debounce-ms", "nope"]));
        assert!(matches!(r, Err(ParseError::MissingValue(s)) if s.contains("--debounce-ms")));
    }

    #[test]
    fn debounce_ms_default_is_none() {
        // Default-Opts: no --debounce-ms ⇒ None (the env var / model default
        // applies; the CLI does not impose a value over the existing 150ms).
        let p = parse(&v(&["watch"])).unwrap();
        assert_eq!(p.opts.debounce_ms, None);
    }

    // -----------------------------------------------------------------------
    // #74 RA weight-shedding knobs — --proc-macro + --features
    // -----------------------------------------------------------------------

    #[test]
    fn proc_macro_flag_accepts_three_modes() {
        for mode in ["auto", "enabled", "disabled"] {
            let p = parse(&v(&["watch", "--proc-macro", mode])).unwrap();
            assert_eq!(p.opts.proc_macro.as_deref(), Some(mode));
        }
    }

    #[test]
    fn proc_macro_flag_rejects_invalid_value() {
        let r = parse(&v(&["watch", "--proc-macro", "maybe"]));
        assert!(
            matches!(r, Err(ParseError::MissingValue(s)) if s.contains("--proc-macro")),
            "invalid proc-macro mode must be actionable: {r:?}"
        );
    }

    #[test]
    fn proc_macro_flag_missing_value_is_actionable() {
        assert_eq!(
            parse(&v(&["watch", "--proc-macro"])),
            Err(ParseError::MissingValue("--proc-macro"))
        );
    }

    #[test]
    fn features_flag_parses_comma_separated_string() {
        let p = parse(&v(&["watch", "--features", "csr,hydrate"])).unwrap();
        assert_eq!(p.opts.features.as_deref(), Some("csr,hydrate"));
    }

    #[test]
    fn features_flag_accepts_equals_form() {
        let p = parse(&v(&["check", "--features=ssr-frontend telephony"])).unwrap();
        assert_eq!(p.opts.features.as_deref(), Some("ssr-frontend telephony"));
    }

    #[test]
    fn push_gate_flag_parses_and_defaults_off() {
        let p = parse(&v(&["push", "--remote", "http://h:1", "--gate"])).unwrap();
        assert_eq!(p.cmd, Cmd::Push);
        assert!(p.opts.push_gate);
        let p = parse(&v(&["push", "--remote", "http://h:1"])).unwrap();
        assert!(
            !p.opts.push_gate,
            "gate must default off — plain dev pushes stay warn-fast"
        );
    }

    #[test]
    fn features_flag_missing_value_is_actionable() {
        assert_eq!(
            parse(&v(&["watch", "--features"])),
            Err(ParseError::MissingValue("--features"))
        );
    }

    #[test]
    fn proc_macro_and_features_flags_compose_with_other_flags() {
        let p = parse(&v(&[
            "watch",
            "--proc-macro",
            "disabled",
            "--features",
            "csr",
            "-p",
            "triform-portal",
            "--target",
            "wasm32-unknown-unknown",
            "--no-default-features",
            "--release",
            "--debounce-ms",
            "300",
            "--root",
            "/p",
        ]))
        .unwrap();
        assert_eq!(p.cmd, Cmd::Watch);
        assert_eq!(p.opts.proc_macro.as_deref(), Some("disabled"));
        assert_eq!(p.opts.features.as_deref(), Some("csr"));
        assert_eq!(p.opts.package.as_deref(), Some("triform-portal"));
        assert_eq!(p.opts.target.as_deref(), Some("wasm32-unknown-unknown"));
        assert!(p.opts.no_default_features);
        assert!(p.opts.release);
        assert_eq!(p.opts.debounce_ms, Some(300));
        assert_eq!(p.opts.root.as_deref(), Some(std::path::Path::new("/p")));
    }

    #[test]
    fn check_profile_flags_parse_cargo_compatible_forms() {
        let p = parse(&v(&[
            "check",
            "--package=triform-server",
            "--target=x86_64-unknown-linux-gnu",
            "--release",
            "--no-default-features",
        ]))
        .unwrap();
        assert_eq!(p.opts.package.as_deref(), Some("triform-server"));
        assert_eq!(p.opts.target.as_deref(), Some("x86_64-unknown-linux-gnu"));
        assert!(p.opts.release);
        assert!(p.opts.no_default_features);

        let p2 = parse(&v(&["check", "-p=physics"])).unwrap();
        assert_eq!(p2.opts.package.as_deref(), Some("physics"));
    }

    #[test]
    fn checks_command_parses_action_id_and_profile() {
        let p = parse(&v(&[
            "checks",
            "run",
            "generated-frontend",
            "--profile",
            "canary",
            "--base",
            "origin/main",
            "--allow-existing-red",
            "--report-json",
            "checks.json",
        ]))
        .unwrap();
        assert_eq!(p.cmd, Cmd::Checks);
        assert_eq!(p.opts.checks_action.as_deref(), Some("run"));
        assert_eq!(p.opts.checks_id.as_deref(), Some("generated-frontend"));
        assert_eq!(p.opts.checks_profile.as_deref(), Some("canary"));
        assert_eq!(p.opts.push_base.as_deref(), Some("origin/main"));
        assert!(p.opts.checks_allow_existing_red);
        assert_eq!(
            p.opts.checks_report_json.as_deref(),
            Some(std::path::Path::new("checks.json"))
        );
        assert!(p.opts.cargo_extra_args.is_empty());
    }

    #[test]
    fn batch_check_command_parses_remote_auth_and_request_json() {
        let p = parse(&v(&[
            "batch-check",
            "--remote",
            "http://127.0.0.1:8080",
            "--auth-token",
            "secret",
            "--request-json",
            "batch.json",
        ]))
        .unwrap();
        assert_eq!(p.cmd, Cmd::BatchCheck);
        assert_eq!(p.opts.remote.as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(p.opts.auth_token.as_deref(), Some("secret"));
        assert_eq!(
            p.opts.batch_request_json.as_deref(),
            Some(std::path::Path::new("batch.json"))
        );
    }

    #[test]
    fn batch_check_request_json_requires_value() {
        assert_eq!(
            parse(&v(&["batch-check", "--request-json"])),
            Err(ParseError::MissingValue("--request-json"))
        );
    }

    #[test]
    fn push_cargo_subcommand_parses_check_and_clippy() {
        let check = parse(&v(&["push", "--cargo-subcommand", "check"])).unwrap();
        assert_eq!(check.opts.cargo_subcommand, Some(CargoSubcommand::Check));

        let clippy = parse(&v(&["push", "--cargo-subcommand", "clippy"])).unwrap();
        assert_eq!(clippy.opts.cargo_subcommand, Some(CargoSubcommand::Clippy));
    }

    #[test]
    fn push_cargo_subcommand_rejects_invalid_values() {
        assert_eq!(
            parse(&v(&["push", "--cargo-subcommand"])),
            Err(ParseError::MissingValue("--cargo-subcommand"))
        );
        let r = parse(&v(&["push", "--cargo-subcommand", "test"]));
        assert!(
            matches!(r, Err(ParseError::MissingValue(s)) if s.contains("--cargo-subcommand")),
            "invalid cargo subcommand must be actionable: {r:?}"
        );
    }

    #[test]
    fn push_cargo_selectors_do_not_create_direct_cargo_profile() {
        let parsed = parse(&v(&[
            "push",
            "--cargo-subcommand",
            "clippy",
            "-p",
            "triform-portal",
            "--target",
            "wasm32-unknown-unknown",
            "--no-default-features",
            "--features",
            "hydrate",
            "--tests",
        ]))
        .unwrap();

        assert_eq!(check_profile_from_opts(&parsed.opts), None);
    }

    #[test]
    fn push_extra_cargo_selectors_are_parsed_for_compatibility() {
        let p = parse(&v(&[
            "push",
            "--manifest-path",
            "tools/Cargo.toml",
            "--tests",
            "--all-targets",
            "--locked",
            "--bin=worker",
        ]))
        .unwrap();
        assert_eq!(
            p.opts.cargo_extra_args,
            vec![
                "--manifest-path",
                "tools/Cargo.toml",
                "--tests",
                "--all-targets",
                "--locked",
                "--bin=worker",
            ]
        );
    }

    #[test]
    fn check_profile_flags_missing_values_are_actionable() {
        assert_eq!(
            parse(&v(&["check", "-p"])),
            Err(ParseError::MissingValue("--package"))
        );
        assert_eq!(
            parse(&v(&["check", "--target"])),
            Err(ParseError::MissingValue("--target"))
        );
        assert_eq!(
            parse(&v(&["push", "--manifest-path"])),
            Err(ParseError::MissingValue("--manifest-path"))
        );
    }

    #[test]
    fn proc_macro_and_features_default_to_none_unset() {
        let p = parse(&v(&["watch"])).unwrap();
        assert_eq!(p.opts.proc_macro, None);
        assert_eq!(p.opts.features, None);
        assert_eq!(p.opts.package, None);
        assert_eq!(p.opts.target, None);
        assert_eq!(p.opts.cargo_subcommand, None);
        assert!(p.opts.cargo_extra_args.is_empty());
        assert!(!p.opts.no_default_features);
        assert!(!p.opts.release);
    }

    #[test]
    fn push_auth_token_prefers_cli_and_ignores_blank() {
        assert_eq!(
            auth_token_for_push(Some(" cli-token ".to_string())).as_deref(),
            Some("cli-token")
        );
        assert_eq!(auth_token_for_push(Some("   ".to_string())), None);
    }
}
