//! Single-line discoverable verdict markers.
//!
//! The ci-gate `--bench` mode greps `^S1_VERDICT:` out of the harness
//! stdout and POSTs the (truncated to 255-char) value as a Forgejo commit
//! status description (context `s1-ac2-verdict`). This module emits the
//! comparative analogs in the same shape so a follow-up ci-gate mode can
//! publish them per-AC without parsing freeform prose:
//!
//!   AC2_VERDICT: ...        ← checker-mode (save→verdict) result
//!   AC3_VERDICT: ...        ← artifact-mode (save→publish) result
//!   AC7_VERDICT: ...        ← comparative result (cargoless vs trunk/bacon)
//!
//! Each line is truncated to `MAX_LEN` chars (Forgejo commit-status
//! description limit, with a 1-char safety margin).
//!
//! v0 contract: a missing measurement (UNAVAILABLE tool, no samples, etc.)
//! is reported EXPLICITLY — never silently dropped — because a comparative
//! claim with hidden gaps is worse than a NO-DATA one.

use std::fmt::Write as _;

/// Forgejo commit-status `description` cap is 255 chars; keep one for safety.
pub const MAX_LEN: usize = 254;

/// Truncate to `MAX_LEN`, appending an unambiguous `…` marker so a
/// truncation never looks like a complete sentence. Operates on bytes —
/// safe for ASCII (everything we emit). If the input is shorter, returns
/// it unchanged.
pub fn truncate(s: &str) -> String {
    if s.len() <= MAX_LEN {
        return s.to_string();
    }
    let mut t = s[..MAX_LEN.saturating_sub(3)].to_string();
    t.push_str("...");
    t
}

/// Emit a discoverable single-line marker.
///
/// The leading `tag: ` is the literal anchor downstream consumers grep for.
/// Always one line, always to stdout, always flushed.
pub fn emit(tag: &str, body: &str) {
    let line = truncate(&format!("{tag}: {body}"));
    println!("{line}");
}

/// One tool's measured median (`None` if NO DATA / UNAVAILABLE).
#[derive(Debug, Clone, Copy)]
pub struct ToolNumber {
    pub name: &'static str,
    pub median_ms: Option<u64>,
}

impl ToolNumber {
    pub fn render(self) -> String {
        match self.median_ms {
            Some(m) => format!("{}={}ms", self.name, m),
            None => format!("{}=N/A", self.name),
        }
    }
}

/// Render a comma-separated comparative figure. Always emits all three
/// tools even if some are N/A — silent omission is a fudge.
pub fn render_triple(cargoless: Option<u64>, trunk: Option<u64>, bacon: Option<u64>) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "{} {} {}",
        ToolNumber {
            name: "cargoless",
            median_ms: cargoless,
        }
        .render(),
        ToolNumber {
            name: "trunk",
            median_ms: trunk,
        }
        .render(),
        ToolNumber {
            name: "bacon",
            median_ms: bacon,
        }
        .render(),
    );
    s
}

/// AC#7 comparative judgment.
///
/// "Better on ≥2 dimensions vs trunk/bacon": we score across the (tool ×
/// dim) pairs `cargoless` actually has a number for AND its competitor also
/// does. A dim where the competitor is N/A is reported separately
/// (`N/A-competitor`) and does NOT count toward the ≥2 threshold — winning
/// uncontested doesn't count, that would be silently rigging.
///
/// Returns (verdict, rationale).
pub fn judge_ac7(
    checker_cargoless: Option<u64>,
    checker_trunk: Option<u64>,
    checker_bacon: Option<u64>,
    artifact_cargoless: Option<u64>,
    artifact_trunk: Option<u64>,
    artifact_bacon: Option<u64>,
) -> (Ac7Verdict, String) {
    let mut wins: Vec<&'static str> = Vec::new();
    let mut losses: Vec<&'static str> = Vec::new();
    let mut na: Vec<&'static str> = Vec::new();

    judge_pair(
        "checker:cargoless<trunk",
        checker_cargoless,
        checker_trunk,
        &mut wins,
        &mut losses,
        &mut na,
    );
    judge_pair(
        "checker:cargoless<bacon",
        checker_cargoless,
        checker_bacon,
        &mut wins,
        &mut losses,
        &mut na,
    );
    judge_pair(
        "artifact:cargoless<trunk",
        artifact_cargoless,
        artifact_trunk,
        &mut wins,
        &mut losses,
        &mut na,
    );
    // bacon has no artifact mode by design — never contributes here.
    judge_pair(
        "artifact:cargoless<bacon",
        artifact_cargoless,
        artifact_bacon,
        &mut wins,
        &mut losses,
        &mut na,
    );

    let total_contested = wins.len() + losses.len();
    let verdict = if total_contested == 0 {
        Ac7Verdict::Inconclusive
    } else if wins.len() >= 2 {
        Ac7Verdict::Pass
    } else {
        Ac7Verdict::Fail
    };
    let rationale = format!("wins={:?} losses={:?} uncontested={:?}", wins, losses, na);
    (verdict, rationale)
}

fn judge_pair(
    label: &'static str,
    ours: Option<u64>,
    theirs: Option<u64>,
    wins: &mut Vec<&'static str>,
    losses: &mut Vec<&'static str>,
    na: &mut Vec<&'static str>,
) {
    match (ours, theirs) {
        (Some(o), Some(t)) if o < t => wins.push(label),
        (Some(_), Some(_)) => losses.push(label),
        _ => na.push(label),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ac7Verdict {
    Pass,
    Fail,
    Inconclusive,
}

impl Ac7Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Ac7Verdict::Pass => "PASS",
            Ac7Verdict::Fail => "FAIL",
            Ac7Verdict::Inconclusive => "INCONCLUSIVE",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_unchanged() {
        let s = "hello";
        assert_eq!(truncate(s), s);
    }

    #[test]
    fn truncate_long_marks_truncation() {
        let s = "x".repeat(MAX_LEN + 100);
        let t = truncate(&s);
        assert!(t.len() <= MAX_LEN);
        assert!(t.ends_with("..."));
    }

    #[test]
    fn render_triple_emits_all_three_even_with_na() {
        let s = render_triple(Some(100), None, Some(2000));
        assert!(s.contains("cargoless=100ms"));
        assert!(s.contains("trunk=N/A"));
        assert!(s.contains("bacon=2000ms"));
    }

    #[test]
    fn ac7_pass_when_cargoless_wins_two_dims() {
        let (v, _) = judge_ac7(
            Some(100), // checker cargoless
            Some(500), // checker trunk     -> WIN
            Some(800), // checker bacon     -> WIN
            Some(800), // artifact cargoless
            Some(900), // artifact trunk    -> WIN (3 wins, only 2 needed)
            None,      // artifact bacon    -> uncontested
        );
        assert_eq!(v, Ac7Verdict::Pass);
    }

    #[test]
    fn ac7_fail_when_only_one_win() {
        let (v, _) = judge_ac7(
            Some(100),
            Some(500), // WIN
            Some(50),  // LOSS
            Some(800),
            Some(700), // LOSS
            None,
        );
        assert_eq!(v, Ac7Verdict::Fail);
    }

    #[test]
    fn ac7_inconclusive_when_all_uncontested() {
        let (v, _) = judge_ac7(Some(100), None, None, Some(800), None, None);
        assert_eq!(v, Ac7Verdict::Inconclusive);
    }

    #[test]
    fn ac7_uncontested_doesnt_count_as_win() {
        // cargoless has artifact, but only bacon (no artifact) to compare:
        // one checker win is not enough.
        let (v, _) = judge_ac7(
            Some(100),
            Some(50),
            None, // checker: cargoless LOSES to trunk
            Some(800),
            None,
            None, // artifact: bacon=N/A, no trunk -> uncontested
        );
        assert_eq!(v, Ac7Verdict::Fail);
    }
}
