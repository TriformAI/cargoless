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

mod build;
mod check;
mod clean;
mod config;
mod cratemap;
mod orphan;
mod serve;
mod serveapi;
mod servedrv;
mod statusfile;
mod telemetry; // #246 Wave-1 5a — OTEL+SigNoz init seam.
mod ui;
mod watch;

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Check,
    Watch,
    Build,
    Status,
    Clean,
    /// Model R Stream B #3: repo-scoped daemon (`serve --repo <path>`).
    Serve,
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
    /// #74 RA weight-shedding: cargo features to enable in RA's
    /// cargo-check invocation. Comma-separated. Default (when unset):
    /// `default`. Plumbed via `TF_FEATURES` env.
    features: Option<String>,
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
    /// `status --remote <url>` — query a remote `serve --bind` fleet
    /// daemon over the shipped HTTP transport instead of the on-disk
    /// `cli-status`. Resolved through `transport::discovery` (explicit
    /// operator intent — `--remote` wins the §10.3 precedence).
    remote: Option<String>,
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
        "serve" => Cmd::Serve,
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
            "--remote" => {
                opts.remote = Some(
                    it.next()
                        .ok_or(ParseError::MissingValue("--remote"))?
                        .clone(),
                );
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
    println!("  serve --repo <DIR>    Model R repo-scoped daemon: auto-discovers");
    println!("                        worktrees, one shared daemon for the fleet");
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
    println!(
        "  --features <FEATS>    cargo features for RA's check (comma-separated; \
         default 'default';"
    );
    println!("                        also TF_FEATURES env)");
    println!(
        "  --remote <URL>        status: query a remote `serve --bind` daemon \
         over HTTP"
    );
    println!(
        "                        (e.g. http://host:8080) instead of the local \
         cli-status file"
    );
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
        // `status --remote <url>` queries a remote fleet `serve --bind`
        // daemon over the shipped HTTP transport. Dispatch BEFORE the
        // `config::Config::resolve` front-door (exactly like `serve`):
        // that detector would wrongly reject a non-WASM cwd, and asking a
        // *remote* daemon must not require a local cargoless project.
        Cmd::Status => {
            if let Some(url) = parsed.opts.remote.as_deref() {
                return statusfile::run_status_remote(url);
            }
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

    // FIELD FINDING #5 (#49): the `--debounce-ms` flag (when given) is
    // plumbed to `cargoless_core::model::watch` via the `TF_DEBOUNCE_MS` env var,
    // keeping the frozen `watch()` signature unchanged. Idiomatic match to
    // `TF_CHECK_TIMEOUT_SECS` (the #21/#43 path). Setting an env var from
    // a CLI is process-local; no risk of leaking outward.
    if let Some(ms) = parsed.opts.debounce_ms {
        // SAFETY: single-threaded init phase, no other threads observe env
        // yet. set_var is unsafe on 2024 edition due to multi-thread reads.
        unsafe {
            std::env::set_var("TF_DEBOUNCE_MS", ms.to_string());
        }
    }
    // #74 RA weight-shedding knobs — same pattern as TF_DEBOUNCE_MS:
    // CLI flag exports the env var, cargoless_core::lsp::InitOpts reads it in
    // `from_env_and_project`. Keeps cargoless-core's API surface stable.
    if let Some(pm) = parsed.opts.proc_macro.as_deref() {
        unsafe {
            std::env::set_var("TF_PROC_MACRO", pm);
        }
    }
    if let Some(fs) = parsed.opts.features.as_deref() {
        unsafe {
            std::env::set_var("TF_FEATURES", fs);
        }
    }

    match parsed.cmd {
        Cmd::Check if parsed.opts.watch => watch::run(&cfg),
        Cmd::Check => check::run(&cfg),
        Cmd::Watch => watch::run(&cfg),
        Cmd::Build => build::run(&cfg, parsed.opts.out.as_deref()),
        Cmd::Status => statusfile::run_status(&cfg),
        Cmd::Clean => clean::run(&cfg),
        Cmd::Help | Cmd::Version | Cmd::Serve => unreachable!("handled above"),
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
            ("version", Cmd::Version),
        ] {
            assert_eq!(parse(&v(&[s])).unwrap().cmd, c);
        }
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
            "--debounce-ms",
            "300",
            "--root",
            "/p",
        ]))
        .unwrap();
        assert_eq!(p.cmd, Cmd::Watch);
        assert_eq!(p.opts.proc_macro.as_deref(), Some("disabled"));
        assert_eq!(p.opts.features.as_deref(), Some("csr"));
        assert_eq!(p.opts.debounce_ms, Some(300));
        assert_eq!(p.opts.root.as_deref(), Some(std::path::Path::new("/p")));
    }

    #[test]
    fn proc_macro_and_features_default_to_none_unset() {
        let p = parse(&v(&["watch"])).unwrap();
        assert_eq!(p.opts.proc_macro, None);
        assert_eq!(p.opts.features, None);
    }
}
