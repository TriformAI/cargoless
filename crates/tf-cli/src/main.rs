//! TF-Trunk CLI. v0 surface: `serve` / `check` / `status` / `clean`.
//!
//! Skeleton dispatch only — each subcommand is owned by the CLI/UX epic
//! (Plane CWDL Epic 5) and wired to `tf-core` as the daemon lands. Kept
//! compiling + tested so CI is green-on-empty (decision D10).

use std::process::ExitCode;

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Serve,
    Check,
    Status,
    Clean,
    Help,
    Unknown(String),
}

fn parse(args: &[String]) -> Command {
    match args.first().map(String::as_str) {
        Some("serve") => Command::Serve,
        Some("check") => Command::Check,
        Some("status") => Command::Status,
        Some("clean") => Command::Clean,
        None | Some("help") | Some("-h") | Some("--help") => Command::Help,
        Some(other) => Command::Unknown(other.to_string()),
    }
}

fn usage() {
    println!("{}", tf_core::build_id());
    println!();
    println!("USAGE: tftrunk <COMMAND>");
    println!();
    println!("  serve    Watch, check, build, and serve the latest green build");
    println!("  check    One-shot verdict; exit code reflects green/red");
    println!("  status   Daemon state, green/red, latest green hash");
    println!("  clean    Wipe the local content-addressed cache");
    println!();
    println!("Product name is decision D1 (Plane CWDL-12); `tftrunk` is a");
    println!("name-neutral working binary name, not the shipping name.");
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args) {
        Command::Help => {
            usage();
            ExitCode::SUCCESS
        }
        Command::Unknown(cmd) => {
            eprintln!("unknown command: {cmd}");
            usage();
            ExitCode::from(2)
        }
        // Not-yet-implemented subcommands exit non-zero so scripts and CI do
        // not mistake the skeleton for a working tool.
        Command::Serve | Command::Check | Command::Status | Command::Clean => {
            eprintln!("that subcommand is not implemented yet — tracked in Plane CWDL Epic 5.");
            ExitCode::from(69) // EX_UNAVAILABLE
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
    fn parses_known_subcommands() {
        assert_eq!(parse(&v(&["serve"])), Command::Serve);
        assert_eq!(parse(&v(&["check"])), Command::Check);
        assert_eq!(parse(&v(&["status"])), Command::Status);
        assert_eq!(parse(&v(&["clean"])), Command::Clean);
    }

    #[test]
    fn empty_and_help_are_help() {
        assert_eq!(parse(&v(&[])), Command::Help);
        assert_eq!(parse(&v(&["--help"])), Command::Help);
    }

    #[test]
    fn unknown_is_unknown() {
        assert_eq!(
            parse(&v(&["frobnicate"])),
            Command::Unknown("frobnicate".into())
        );
    }
}
