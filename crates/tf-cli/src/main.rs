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
//! matches the repo's dependency-minimal ethos (tf-proto is dep-free; the
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
mod orphan;
mod statusfile;
mod ui;
mod watch;

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Check,
    Watch,
    Build,
    Status,
    Clean,
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
    /// by exporting `TF_DEBOUNCE_MS` before invoking `tf_core::model::watch`
    /// — keeps the `watch()` signature byte-frozen (the env-var idiom
    /// matches `TF_CHECK_TIMEOUT_SECS` from #21/#43).
    debounce_ms: Option<u64>,
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
    println!("{}", tf_core::build_id());
    println!();
    println!("USAGE: tftrunk <COMMAND> [FLAGS]");
    println!();
    println!("  check                 One-shot verdict; exit 0=green 1=red 2=setup-error");
    println!("  check --watch         Continuous headless verdict stream (alias: watch)");
    println!("  watch                 Continuous headless verdict stream");
    println!("  build --watch --out <DIR>");
    println!("                        Maintain the latest-green artifact in <DIR>");
    println!("  status                Daemon liveness + current verdict + latest-green");
    println!("  clean                 Remove the local content-addressed cache");
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
    println!("  -h, --help            Show this help");
    println!("  -V, --version         Show the build identifier");
    println!();
    println!("v0 is headless: no `serve`, no HTTP/browser (that is v0.1).");
    println!("Working name only — the shipping name is decision D1 (CWDL-12).");
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
            println!("{}", tf_core::build_id());
            return ExitCode::SUCCESS;
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
    // plumbed to `tf_core::model::watch` via the `TF_DEBOUNCE_MS` env var,
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

    match parsed.cmd {
        Cmd::Check if parsed.opts.watch => watch::run(&cfg),
        Cmd::Check => check::run(&cfg),
        Cmd::Watch => watch::run(&cfg),
        Cmd::Build => build::run(&cfg, parsed.opts.out.as_deref()),
        Cmd::Status => statusfile::run_status(&cfg),
        Cmd::Clean => clean::run(&cfg),
        Cmd::Help | Cmd::Version => unreachable!("handled above"),
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
}
