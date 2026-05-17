//! `cargoless-bench` — two-mode COMPARATIVE bench harness.
//!
//! Subcommands (std-only hand-rolled parse — same `dependencies = []`
//! discipline as `tf-cli`):
//!
//!   cargoless-bench checker   [--tool=cargoless|trunk|bacon|all] ...
//!   cargoless-bench artifact  [--tool=cargoless|trunk|bacon|all] ...
//!   cargoless-bench all       (runs both modes for all tools + AC#7 line)
//!
//! Shared flags:
//!   --fixture <DIR>        cargoless fixture project (default bench/fixture)
//!   --cargoless-bin <PATH> the cargoless binary to test (default `tftrunk`
//!                          on PATH; in CI the driver builds it into
//!                          `target/release/tftrunk` and passes that path)
//!   --out <DIR>            artifact-mode --out for cargoless (default
//!                          `<fixture>/.cargoless-bench-out`); not used
//!                          in checker mode
//!   --reps N               measurement reps per (tool, mode); 1st discarded
//!   --edit-timeout-ms MS   per-edit budget; misses recorded at this ceiling
//!   --warm-timeout-ms MS   max wait for the tool's first ready signal
//!                          (Leptos cold builds are minutes — default 5min)
//!
//! Output:
//!   * Human-readable per-(tool, mode) block (median, p90, detected/attempt).
//!   * `AC2_VERDICT: ...`, `AC3_VERDICT: ...`, `AC7_VERDICT: ...` lines —
//!     single-line, ≤254 chars each, designed for ci-gate to grep + POST
//!     as Forgejo commit statuses (same shape as the existing
//!     `s1-ac2-verdict` line emitted by `bench/run.sh`).
//!
//! Exit code: 0 on a captured measurement (even if AC#7 = FAIL — a real
//! FAIL is data, not a transport error). 2 on a setup/transport error
//! that prevented capture. Mirrors `bench/run.sh`'s "evidence, not gate"
//! contract — gating happens in task #36, downstream of this binary.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use cargoless_bench_harness::modes::{artifact, checker, Cfg, RunOutcome};
use cargoless_bench_harness::stats;
use cargoless_bench_harness::tools::{self, Tool};
use cargoless_bench_harness::verdict::{self, Ac7Verdict};

const NAME: &str = "cargoless-bench";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Checker,
    Artifact,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolSel {
    Named(String),
    All,
}

struct Opts {
    mode: Mode,
    tool: ToolSel,
    fixture: PathBuf,
    cargoless_bin: String,
    out: PathBuf,
    cfg: Cfg,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse(&args) {
        Ok(Some(o)) => o,
        Ok(None) => {
            usage();
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("{NAME}: {e}");
            usage();
            return ExitCode::from(2);
        }
    };

    let fixture = match opts.fixture.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{NAME}: fixture {:?} not found: {e}", opts.fixture);
            return ExitCode::from(2);
        }
    };

    // Make sure --out exists; for cargoless `build --watch --out` writes there.
    if let Err(e) = std::fs::create_dir_all(&opts.out) {
        eprintln!("{NAME}: could not create --out {:?}: {e}", opts.out);
        return ExitCode::from(2);
    }

    println!("=== {NAME} (two-mode comparative; AC#2 + AC#3 → AC#7) ===");
    println!("fixture:        {}", fixture.display());
    println!("cargoless bin:  {}", opts.cargoless_bin);
    println!("artifact --out: {}", opts.out.display());
    println!(
        "config: reps={} edit_timeout={}ms warm_timeout={}ms settle={}ms",
        opts.cfg.reps,
        opts.cfg.edit_timeout.as_millis(),
        opts.cfg.warm_timeout.as_millis(),
        opts.cfg.settle.as_millis()
    );
    println!();

    let registry = tools::registry(&opts.cargoless_bin, &opts.out);
    let selected = select_tools(&opts.tool, &registry);
    if selected.is_empty() {
        eprintln!(
            "{NAME}: tool selector {:?} matched no registered tool",
            opts.tool
        );
        return ExitCode::from(2);
    }

    let want_checker = matches!(opts.mode, Mode::Checker | Mode::All);
    let want_artifact = matches!(opts.mode, Mode::Artifact | Mode::All);

    let mut checker_runs: Vec<checker::CheckerRun> = Vec::new();
    let mut artifact_runs: Vec<artifact::ArtifactRun> = Vec::new();

    if want_checker {
        println!("---- mode: checker (save→verdict, AC#2) ----");
        for t in &selected {
            print_tool_header(t, "checker");
            let r = checker::run(t, &fixture, &opts.cfg);
            print_checker_block(&r);
            checker_runs.push(r);
        }
        println!();
    }

    if want_artifact {
        println!("---- mode: artifact (save→publish, AC#3) ----");
        for t in &selected {
            print_tool_header(t, "artifact");
            let r = artifact::run(t, &fixture, &opts.cfg);
            print_artifact_block(&r);
            artifact_runs.push(r);
        }
        println!();
    }

    // ---- verdict lines (single-line, ≤254 chars each) ----
    println!("==== single-line verdict markers ====");

    if want_checker {
        let triple = render_triple_from(&checker_runs);
        let (cargoless, trunk, bacon) = median_triple(&checker_runs);
        let claim = describe_claim(cargoless, trunk, bacon);
        verdict::emit(
            "AC2_VERDICT",
            &format!("checker(save→verdict) {triple}  cargoless-vs-rest: {claim}"),
        );
    }

    if want_artifact {
        let triple = render_triple_from_artifact(&artifact_runs);
        let (cargoless, trunk, bacon) = median_triple_artifact(&artifact_runs);
        let claim = describe_claim(cargoless, trunk, bacon);
        verdict::emit(
            "AC3_VERDICT",
            &format!(
                "artifact(save→publish) {triple}  cargoless-vs-rest: {claim} \
                 (NO sub-1s claim for artifacts — D-A2)"
            ),
        );
    }

    if matches!(opts.mode, Mode::All) {
        let (cc, ct, cb) = median_triple(&checker_runs);
        let (ac, at, ab) = median_triple_artifact(&artifact_runs);
        let (v, rationale) = verdict::judge_ac7(cc, ct, cb, ac, at, ab);
        verdict::emit(
            "AC7_VERDICT",
            &format!(
                "comparative vs {{trunk,bacon}} on ≥2 dims = {} ({rationale})",
                v.as_str()
            ),
        );
        if v == Ac7Verdict::Pass {
            println!("\n=== AC#7: PASS — cargoless beats {{trunk,bacon}} on ≥2 contested dims.");
        } else {
            println!(
                "\n=== AC#7: {} — see AC7_VERDICT line above. Renegotiate or land fixes.",
                v.as_str()
            );
        }
    }

    println!(
        "\n(harness exit 0 by design — evidence; the AC#7 gate is task #36, \
         downstream of this binary.)"
    );
    ExitCode::SUCCESS
}

fn select_tools<'a>(sel: &ToolSel, registry: &'a [Tool]) -> Vec<&'a Tool> {
    match sel {
        ToolSel::All => registry.iter().collect(),
        ToolSel::Named(name) => registry.iter().filter(|t| t.name == name).collect(),
    }
}

fn print_tool_header(t: &Tool, mode: &str) {
    println!(
        "\n  [{}] {} :: argv={:?}",
        mode,
        t.name,
        if mode == "artifact" {
            t.artifact_argv.clone().unwrap_or_default()
        } else {
            t.checker_argv.clone()
        }
    );
    if let Some(n) = t.note {
        println!("      note: {n}");
    }
}

fn print_checker_block(r: &checker::CheckerRun) {
    println!("      warm: {:.2}s", r.warm_secs);
    println!(
        "      detected: {}/{} (red-edge edits with a matching signal)",
        r.detected_red, r.attempted
    );
    println!("      outcome: {}", r.outcome.as_tag());
    println!("      samples: {}", stats::summary_line(&r.samples_red_ms));
    if let Some(m) = r.median_ms() {
        println!("      → MEDIAN save→red = {m} ms");
    }
}

fn print_artifact_block(r: &artifact::ArtifactRun) {
    println!("      warm: {:.2}s", r.warm_secs);
    println!(
        "      detected: {}/{} (saves whose witness advanced)",
        r.detected, r.attempted
    );
    println!("      outcome: {}", r.outcome.as_tag());
    println!("      samples: {}", stats::summary_line(&r.samples_ms));
    if let Some(m) = r.median_ms() {
        println!("      → MEDIAN save→publish = {m} ms");
    }
}

fn median_triple(rs: &[checker::CheckerRun]) -> (Option<u64>, Option<u64>, Option<u64>) {
    let f = |name: &str| {
        rs.iter()
            .find(|r| r.tool == name && matches!(r.outcome, RunOutcome::Measured))
            .and_then(|r| r.median_ms())
    };
    (f("cargoless"), f("trunk"), f("bacon"))
}

fn median_triple_artifact(rs: &[artifact::ArtifactRun]) -> (Option<u64>, Option<u64>, Option<u64>) {
    let f = |name: &str| {
        rs.iter()
            .find(|r| r.tool == name && matches!(r.outcome, RunOutcome::Measured))
            .and_then(|r| r.median_ms())
    };
    (f("cargoless"), f("trunk"), f("bacon"))
}

fn render_triple_from(rs: &[checker::CheckerRun]) -> String {
    let (c, t, b) = median_triple(rs);
    verdict::render_triple(c, t, b)
}

fn render_triple_from_artifact(rs: &[artifact::ArtifactRun]) -> String {
    let (c, t, b) = median_triple_artifact(rs);
    verdict::render_triple(c, t, b)
}

/// "cargoless WINS vs trunk (cargoless=X<trunk=Y)" / "TIES" / "LOSES" /
/// "N/A" — short enough to fit into the 254-char verdict line.
fn describe_claim(ours: Option<u64>, trunk: Option<u64>, bacon: Option<u64>) -> String {
    fn one(label: &str, ours: Option<u64>, theirs: Option<u64>) -> String {
        match (ours, theirs) {
            (Some(o), Some(t)) if o < t => format!("WIN/{label}({o}<{t})"),
            (Some(o), Some(t)) if o == t => format!("TIE/{label}({o}={t})"),
            (Some(o), Some(t)) => format!("LOSE/{label}({o}>{t})"),
            _ => format!("N/A/{label}"),
        }
    }
    format!(
        "{} {}",
        one("trunk", ours, trunk),
        one("bacon", ours, bacon)
    )
}

// ----- arg parsing -----

fn parse(args: &[String]) -> Result<Option<Opts>, String> {
    let mut it = args.iter();
    let Some(first) = it.next() else {
        return Ok(None); // -> usage
    };

    let mode = match first.as_str() {
        "checker" => Mode::Checker,
        "artifact" => Mode::Artifact,
        "all" => Mode::All,
        "-h" | "--help" | "help" => return Ok(None),
        other => return Err(format!("unknown subcommand: {other}")),
    };

    let mut tool = ToolSel::All;
    let mut fixture = PathBuf::from("bench/fixture");
    let mut cargoless_bin = "tftrunk".to_string();
    let mut out: Option<PathBuf> = None;
    let mut cfg = Cfg::default_for_ci();

    while let Some(a) = it.next() {
        match a.as_str() {
            "--tool" => {
                tool = parse_tool(it.next().ok_or("--tool needs a value")?)?;
            }
            "--fixture" => {
                fixture = PathBuf::from(it.next().ok_or("--fixture needs a value")?);
            }
            "--cargoless-bin" => {
                cargoless_bin = it.next().ok_or("--cargoless-bin needs a value")?.clone();
            }
            "--out" => {
                out = Some(PathBuf::from(it.next().ok_or("--out needs a value")?));
            }
            "--reps" => {
                cfg.reps = it
                    .next()
                    .ok_or("--reps needs a value")?
                    .parse::<usize>()
                    .map_err(|e| format!("--reps: {e}"))?;
            }
            "--edit-timeout-ms" => {
                let v: u64 = it
                    .next()
                    .ok_or("--edit-timeout-ms needs a value")?
                    .parse()
                    .map_err(|e| format!("--edit-timeout-ms: {e}"))?;
                cfg.edit_timeout = Duration::from_millis(v);
            }
            "--warm-timeout-ms" => {
                let v: u64 = it
                    .next()
                    .ok_or("--warm-timeout-ms needs a value")?
                    .parse()
                    .map_err(|e| format!("--warm-timeout-ms: {e}"))?;
                cfg.warm_timeout = Duration::from_millis(v);
            }
            "--settle-ms" => {
                let v: u64 = it
                    .next()
                    .ok_or("--settle-ms needs a value")?
                    .parse()
                    .map_err(|e| format!("--settle-ms: {e}"))?;
                cfg.settle = Duration::from_millis(v);
            }
            "-h" | "--help" => return Ok(None),
            other => return Err(format!("unknown flag: {other}")),
        }
    }

    let out = out.unwrap_or_else(|| fixture.join(".cargoless-bench-out"));

    Ok(Some(Opts {
        mode,
        tool,
        fixture,
        cargoless_bin,
        out,
        cfg,
    }))
}

fn parse_tool(s: &str) -> Result<ToolSel, String> {
    match s {
        "all" => Ok(ToolSel::All),
        "cargoless" | "trunk" | "bacon" => Ok(ToolSel::Named(s.to_string())),
        other => Err(format!(
            "--tool {other:?}: expected one of cargoless|trunk|bacon|all"
        )),
    }
}

fn usage() {
    println!("{NAME} — two-mode comparative bench harness (cargoless vs trunk/bacon)");
    println!();
    println!("USAGE:");
    println!("  {NAME} <SUBCOMMAND> [FLAGS]");
    println!();
    println!("SUBCOMMANDS:");
    println!("  checker        save→verdict latency (AC#2 dim)");
    println!("  artifact       save→publish  latency (AC#3 dim, reported");
    println!("                 separately — no sub-1s claim per D-A2)");
    println!("  all            both modes + AC#7 comparative verdict line");
    println!();
    println!("SHARED FLAGS:");
    println!("  --tool <NAME>           cargoless|trunk|bacon|all  (default all)");
    println!("  --fixture <DIR>         default bench/fixture");
    println!("  --cargoless-bin <PATH>  default `tftrunk` on PATH");
    println!("  --out <DIR>             artifact mode --out");
    println!("                          (default <fixture>/.cargoless-bench-out)");
    println!("  --reps N                measurement reps (1st discarded)");
    println!("  --edit-timeout-ms MS    per-edit budget (miss recorded at this");
    println!("                          ceiling, never silently skipped)");
    println!("  --warm-timeout-ms MS    max wait for the tool's first ready");
    println!("                          signal (cold Leptos build = minutes)");
    println!("  --settle-ms MS          quiet-window after a save/revert");
    println!();
    println!("Output: human-readable blocks + single-line markers");
    println!("`AC2_VERDICT:` / `AC3_VERDICT:` / `AC7_VERDICT:` (≤254 chars");
    println!("each — Forgejo commit-status description shape).");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parse_help_returns_none() {
        assert!(parse(&v(&[])).unwrap().is_none());
        assert!(parse(&v(&["help"])).unwrap().is_none());
        assert!(parse(&v(&["--help"])).unwrap().is_none());
        assert!(parse(&v(&["-h"])).unwrap().is_none());
    }

    #[test]
    fn parse_checker_defaults() {
        let o = parse(&v(&["checker"])).unwrap().unwrap();
        assert_eq!(o.mode, Mode::Checker);
        assert_eq!(o.tool, ToolSel::All);
        assert_eq!(o.cargoless_bin, "tftrunk");
    }

    #[test]
    fn parse_named_tool() {
        let o = parse(&v(&["artifact", "--tool", "cargoless"]))
            .unwrap()
            .unwrap();
        assert_eq!(o.tool, ToolSel::Named("cargoless".into()));
    }

    #[test]
    fn parse_rejects_bad_tool() {
        let err = parse(&v(&["checker", "--tool", "frobnicate"])).unwrap_err();
        assert!(err.contains("frobnicate"), "msg = {err}");
    }

    #[test]
    fn parse_rejects_unknown_flag() {
        let err = parse(&v(&["all", "--bogus"])).unwrap_err();
        assert!(err.contains("--bogus"), "msg = {err}");
    }

    #[test]
    fn parse_threads_through_overrides() {
        let o = parse(&v(&[
            "all",
            "--fixture",
            "/tmp/fx",
            "--cargoless-bin",
            "/opt/tftrunk",
            "--out",
            "/tmp/out",
            "--reps",
            "11",
            "--edit-timeout-ms",
            "3000",
            "--warm-timeout-ms",
            "120000",
            "--settle-ms",
            "500",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(o.mode, Mode::All);
        assert_eq!(o.fixture, PathBuf::from("/tmp/fx"));
        assert_eq!(o.cargoless_bin, "/opt/tftrunk");
        assert_eq!(o.out, PathBuf::from("/tmp/out"));
        assert_eq!(o.cfg.reps, 11);
        assert_eq!(o.cfg.edit_timeout, Duration::from_millis(3000));
        assert_eq!(o.cfg.warm_timeout, Duration::from_millis(120_000));
        assert_eq!(o.cfg.settle, Duration::from_millis(500));
    }

    #[test]
    fn select_tools_named_matches_one() {
        let reg = tools::registry("tftrunk", Path::new("/tmp/out"));
        let sel = select_tools(&ToolSel::Named("trunk".into()), &reg);
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].name, "trunk");
    }

    #[test]
    fn select_tools_all_yields_three() {
        let reg = tools::registry("tftrunk", Path::new("/tmp/out"));
        let sel = select_tools(&ToolSel::All, &reg);
        assert_eq!(sel.len(), 3);
    }

    #[test]
    fn describe_claim_handles_all_combinations() {
        // WIN
        assert!(describe_claim(Some(100), Some(200), Some(300)).contains("WIN/trunk"));
        // LOSE
        assert!(describe_claim(Some(300), Some(100), Some(200)).contains("LOSE/trunk"));
        // TIE
        assert!(describe_claim(Some(100), Some(100), None).contains("TIE/trunk"));
        // N/A — competitor missing
        assert!(describe_claim(Some(100), None, Some(50)).contains("N/A/trunk"));
        // N/A — we are missing
        assert!(describe_claim(None, Some(100), Some(50)).contains("N/A/trunk"));
    }
}
