//! The cargoless binary — `serve` / `check` / `status` / `clean`.
//!
//! `serve` is the headline: a zero-config replacement for `trunk serve`.
//! Migrating is one command — in your Rust + WASM (Leptos CSR) project root:
//!
//! ```text
//!   trunk serve            # before
//!   cargoless serve        # after — no config, no Trunk.toml required
//! ```
//!
//! cargoless auto-detects a `cdylib` + `leptos` project (decision **D7**); a
//! `tf.toml` (decision **D6**) overrides any default if you need to. Unlike
//! `trunk serve`, it keeps a warm rust-analyzer, knows which files compile,
//! and never serves a broken build to the browser.
//!
//! Naming: `cargoless` is the working repo/binary identifier; the shipping
//! product name is open decision **D1** (Plane CWDL-12). `tf` is explicitly
//! not the name (Terraform collision).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::config::Config;

mod check;
mod clean;
mod config;
mod holding;
mod serve;
mod status;
mod ui;

#[derive(Parser)]
#[command(
    name = "cargoless",
    version,
    about = "trunk serve, but it knows what's green and tells you the moment it isn't.",
    long_about = "cargoless — a zero-config, local-first replacement for `trunk serve` \
for Rust + WASM (Leptos CSR) development.\n\n\
Migrate in one command from your project root:\n  \
trunk serve   ->   cargoless serve\n\n\
No Trunk.toml or tf.toml is required: the project is auto-detected (D7). A \
tf.toml overrides any default (D6).\n\n\
Working name only — the shipping product name is decision D1."
)]
struct Cli {
    /// Project root (default: current directory).
    #[arg(long, global = true, value_name = "DIR")]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Watch, check, build, and serve — brings a holding page up instantly,
    /// zero config (the AC#1 path).
    Serve {
        /// Bind host (overrides tf.toml `[serve] host`).
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// Bind port (overrides tf.toml `[serve] port`).
        #[arg(long, value_name = "PORT")]
        port: Option<u16>,
    },
    /// One-shot verdict; exit code reflects green/red (0 green, 1 red).
    Check,
    /// Daemon state: is it up, and the latest green build hash.
    Status,
    /// Wipe the local content-addressed cache.
    Clean,
}

fn resolve_root(arg: Option<PathBuf>) -> PathBuf {
    arg.or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = resolve_root(cli.root);

    // Config resolution is the shared front door. Its error *is* the
    // onboarding UX for a zero-config tool, so it is surfaced once, here,
    // with the actionable message the ConfigError already carries.
    let mut cfg = match Config::resolve(&root) {
        Ok(c) => c,
        Err(e) => {
            ui::error(e.to_string());
            return ExitCode::from(2);
        }
    };

    match cli.command {
        Cmd::Serve { host, port } => {
            if let Some(h) = host {
                cfg.host = h;
            }
            if let Some(p) = port {
                cfg.port = p;
            }
            serve::run(&cfg)
        }
        Cmd::Check => check::run(&cfg),
        Cmd::Status => status::run(&cfg),
        Cmd::Clean => clean::run(&cfg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // clap's own internal consistency check — catches arg/subcommand
        // wiring mistakes at test time (CI has no way to run the binary).
        Cli::command().debug_assert();
    }

    #[test]
    fn all_four_subcommands_parse() {
        for (argv, ok) in [
            (vec!["cargoless", "serve"], true),
            (vec!["cargoless", "serve", "--port", "3000"], true),
            (vec!["cargoless", "check"], true),
            (vec!["cargoless", "status"], true),
            (vec!["cargoless", "clean"], true),
            (vec!["cargoless", "--root", "/p", "clean"], true),
            (vec!["cargoless", "frobnicate"], false),
        ] {
            assert_eq!(Cli::try_parse_from(&argv).is_ok(), ok, "argv: {argv:?}");
        }
    }

    #[test]
    fn root_defaults_to_cwd_not_panic() {
        let r = resolve_root(None);
        assert!(!r.as_os_str().is_empty());
        assert_eq!(resolve_root(Some(PathBuf::from("/x"))), PathBuf::from("/x"));
    }
}
