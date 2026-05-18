//! Per-crate diagnostic attribution (Model R #9, `D-FLEET-SHARED-DAEMON`
//! §9). tf-multiverse is one Cargo workspace of many crates; the operator's
//! fleet pattern has different agents owning different crates, so a single
//! workspace-level green/red is too coarse — an `isolation`-agent should be
//! able to proceed while `physics` is red. This module maps each
//! [`cargoless_proto::Diagnostic`]'s file to its owning workspace crate and
//! rolls the per-file diagnostics up into a per-crate verdict, serialised
//! into the schema=2 `cli-status` `crates=` field by [`crate::statusfile`].
//!
//! ## Attribution rule (must match the authoritative tree verdict)
//!
//! A crate is **red iff it owns ≥1 `Severity::Error` diagnostic**. This is
//! deliberately the *same* rule the boolean `TreeState` uses (the
//! rustc-authoritative error rule documented in
//! `cargoless_core::model`) — warnings/info/hint never flip a crate red. A
//! per-crate verdict that disagreed with the top-level `verdict=` line
//! would be a correctness defect: the operator's agents gate on this.
//!
//! ## Honesty invariant: never a false per-crate green
//!
//! If an error diagnostic resolves to **no** known workspace crate (a path
//! dep outside the tree, a generated file, a workspace-detection miss),
//! emitting an all-green `crates=` map would tell an agent "every crate is
//! clean" while the tree is in fact red. So [`aggregate`] reports
//! `all_errors_attributed`; [`crate::statusfile`] omits the `crates=` line
//! entirely when it is false, and the authoritative `verdict=` line always
//! stands on its own. Asymmetric-stream principle (§8): never claim
//! per-crate coverage we cannot substantiate.
//!
//! ## Crate-map source (dependency-free, house pattern)
//!
//! `D-FLEET-SHARED-DAEMON` §9 calls this "cargo-metadata-derivable". We
//! realise the *same grouping* without spawning `cargo` and without a
//! `toml`/`serde` dependency — matching the existing house pattern
//! (`config::detect_from_cargo_toml` is hand-parsed, pure-over-text,
//! filesystem-free in its core). [`CrateMap::from_workspace`] reads the
//! workspace `Cargo.toml` `[workspace] members`, then each member's
//! `[package] name`. The pure parsers ([`parse_workspace_members`],
//! [`parse_package_name`]) are exhaustively unit-tested without a
//! filesystem; [`CrateMap::from_pairs`] builds a map directly for tests.
//! A future `cargo metadata` adapter can replace the source without
//! touching [`aggregate`] or the serialisation — the seam is the
//! `(crate_dir, crate_name)` pair list.

use std::path::{Path, PathBuf};

// The `cargoless` cli crate depends only on `cargoless-core`; `Diagnostic`
// + `Severity` are the `cargoless_proto` types re-exported there (house
// pattern — cf. `check.rs` using `cargoless_core::Diagnostic`).
use cargoless_core::{Diagnostic, Severity};

use crate::statusfile::Verdict;

/// Strip a `#` comment, respecting `#` inside a double-quoted string.
/// Local copy of the `config` house helper (kept module-local rather than
/// widening `config`'s private API for an unrelated consumer — the pattern,
/// not the symbol, is what's shared; cf. `config::strip_comment`).
fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Extract `name = "<x>"` from the `[package]` section of a `Cargo.toml`'s
/// text. Pure over the text (no filesystem) so it is unit-tested directly.
/// Returns `None` for a virtual manifest (no `[package]`) or a malformed
/// name line.
pub fn parse_package_name(cargo_toml: &str) -> Option<String> {
    let mut section = String::new();
    for raw in cargo_toml.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        if section == "package" {
            let key = line.split(['=', '.']).next().unwrap_or("").trim();
            if key == "name" {
                let (_, v) = line.split_once('=')?;
                let v = v.trim().trim_matches('"').trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Extract the `[workspace] members = [ ... ]` entries from a workspace
/// `Cargo.toml`'s text. Pure over the text. Handles the multi-line array
/// form (the house style — see this repo's own root `Cargo.toml`) and
/// inline form. Each returned entry is a path string relative to the
/// workspace root, with a single optional trailing `/*` glob preserved
/// verbatim for [`CrateMap::from_workspace`] to expand.
pub fn parse_workspace_members(cargo_toml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut section = String::new();
    let mut in_members = false;
    for raw in cargo_toml.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if !in_members {
            if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = name.trim().to_string();
                continue;
            }
        }
        if section != "workspace" {
            continue;
        }
        // Find the start of the members array (inline or multi-line).
        let scan = if !in_members {
            let key = line.split(['=', '[']).next().unwrap_or("").trim();
            if key != "members" {
                continue;
            }
            in_members = true;
            // Everything after the first '['.
            line.split_once('[').map(|(_, r)| r).unwrap_or("")
        } else {
            line
        };
        for tok in scan.split(',') {
            let tok = tok.trim().trim_end_matches(']').trim();
            if tok.is_empty() {
                continue;
            }
            let entry = tok.trim_matches('"').trim();
            if !entry.is_empty() {
                out.push(entry.to_string());
            }
        }
        if scan.contains(']') {
            in_members = false;
            section.clear();
        }
    }
    out
}

/// A resolved workspace crate map: `(crate_root_dir, crate_name)` pairs.
/// File→crate attribution is "longest matching crate-root prefix wins"
/// (a nested crate dir beats its workspace ancestor).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrateMap {
    /// Sorted longest-dir-first so [`Self::crate_of`] takes the first
    /// (most specific) match.
    roots: Vec<(PathBuf, String)>,
}

impl CrateMap {
    /// Build directly from `(crate_root_dir, crate_name)` pairs — the test
    /// seam, and the shape a future `cargo metadata` adapter produces.
    pub fn from_pairs<I, P, S>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (P, S)>,
        P: Into<PathBuf>,
        S: Into<String>,
    {
        let mut roots: Vec<(PathBuf, String)> = pairs
            .into_iter()
            .map(|(p, s)| (p.into(), s.into()))
            .collect();
        // Longest path first ⇒ most-specific crate wins in `crate_of`.
        roots.sort_by(|a, b| {
            b.0.components()
                .count()
                .cmp(&a.0.components().count())
                .then(b.0.cmp(&a.0))
        });
        Self { roots }
    }

    /// True when no crate could be resolved (single-crate / detection
    /// miss) — caller then skips the `crates=` line entirely.
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// All crate names, deduped, in stable (sorted) order — every
    /// workspace crate starts green even if it owns no diagnostics.
    pub fn crate_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.roots.iter().map(|(_, n)| n.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    /// The crate owning `file` (longest crate-root prefix), or `None` when
    /// the file is under no known workspace crate.
    pub fn crate_of(&self, file: &Path) -> Option<&str> {
        self.roots
            .iter()
            .find(|(dir, _)| file.starts_with(dir))
            .map(|(_, name)| name.as_str())
    }

    /// Production source: derive the map from the workspace `Cargo.toml`
    /// reachable from `root`. Walks up at most [`WORKSPACE_WALK_MAX`]
    /// parents to find a manifest containing `[workspace]`; resolves each
    /// member's `[package] name`. If no workspace manifest is found, falls
    /// back to treating `root` as a single crate (its own `[package]
    /// name`). Any unreadable manifest is skipped (best-effort — a missing
    /// per-crate breakdown must never take the daemon down; the
    /// authoritative `verdict=` line is unaffected).
    pub fn from_workspace(root: &Path) -> Self {
        // 1. Find the workspace root (nearest ancestor whose Cargo.toml
        //    has a [workspace] table), bounded.
        let mut ws_root: Option<PathBuf> = None;
        let mut cur = Some(root.to_path_buf());
        // Note: no let-chains — CI is pinned Rust 1.85 (let-chains land
        // 1.88). Plain nested `if let` / `match` throughout this module.
        for _ in 0..WORKSPACE_WALK_MAX {
            let Some(dir) = cur.clone() else { break };
            if let Ok(txt) = std::fs::read_to_string(dir.join("Cargo.toml")) {
                if txt
                    .lines()
                    .any(|l| strip_comment(l).trim() == "[workspace]")
                {
                    ws_root = Some(dir);
                    break;
                }
            }
            cur = dir.parent().map(Path::to_path_buf);
        }

        let Some(ws_root) = ws_root else {
            // No workspace manifest — single-crate project. Map the root
            // to its own package name if it has one.
            if let Ok(txt) = std::fs::read_to_string(root.join("Cargo.toml")) {
                if let Some(name) = parse_package_name(&txt) {
                    return Self::from_pairs([(root.to_path_buf(), name)]);
                }
            }
            return Self::default();
        };

        let Ok(ws_txt) = std::fs::read_to_string(ws_root.join("Cargo.toml")) else {
            return Self::default();
        };
        let mut pairs: Vec<(PathBuf, String)> = Vec::new();
        // A root manifest can be BOTH a workspace and a package.
        if let Some(name) = parse_package_name(&ws_txt) {
            pairs.push((ws_root.clone(), name));
        }
        for member in parse_workspace_members(&ws_txt) {
            // Expand a single trailing `/*` one directory level (literal
            // paths — tf-multiverse's shape — need no expansion).
            let dirs: Vec<PathBuf> = if let Some(prefix) = member.strip_suffix("/*") {
                let base = ws_root.join(prefix);
                std::fs::read_dir(&base)
                    .map(|rd| {
                        rd.flatten()
                            .map(|e| e.path())
                            .filter(|p| p.is_dir())
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                vec![ws_root.join(&member)]
            };
            for dir in dirs {
                if let Ok(txt) = std::fs::read_to_string(dir.join("Cargo.toml")) {
                    if let Some(name) = parse_package_name(&txt) {
                        pairs.push((dir, name));
                    }
                }
            }
        }
        Self::from_pairs(pairs)
    }
}

/// Bounded ancestor walk for workspace-root discovery (defends against a
/// pathological `/`-rooted scan; 8 is far deeper than any real layout).
pub const WORKSPACE_WALK_MAX: usize = 8;

/// The per-crate roll-up result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerCrate {
    /// `(crate_name, verdict)` in stable sorted order. Every known
    /// workspace crate appears (green unless it owns an error).
    pub verdicts: Vec<(String, Verdict)>,
    /// Count of `Severity::Error` diagnostics (the asymmetric-stream
    /// "how bad is red" scalar — `D-FLEET-SHARED-DAEMON` §9.2
    /// `red_diagnostics=`). Counts errors only, matching the red rule.
    pub error_count: u32,
    /// False iff ≥1 error diagnostic resolved to no known crate. When
    /// false the caller MUST omit the `crates=` line (a partial per-crate
    /// map would falsely read all-green). The authoritative `verdict=`
    /// line is unaffected either way.
    pub all_errors_attributed: bool,
}

/// Pure roll-up of per-file diagnostics into per-crate verdicts. No
/// filesystem, no clock — exhaustively unit-tested. See the module-level
/// attribution rule + honesty invariant.
pub fn aggregate(diags: &[Diagnostic], map: &CrateMap) -> PerCrate {
    use std::collections::BTreeMap;
    // Seed every known crate green.
    let mut verdicts: BTreeMap<String, Verdict> = map
        .crate_names()
        .into_iter()
        .map(|n| (n, Verdict::Green))
        .collect();
    let mut error_count: u32 = 0;
    let mut all_errors_attributed = true;
    for d in diags {
        if d.severity != Severity::Error {
            continue; // warnings/info/hint never flip a crate red
        }
        error_count = error_count.saturating_add(1);
        match map.crate_of(&d.file_path) {
            Some(name) => {
                verdicts.insert(name.to_string(), Verdict::Red);
            }
            None => {
                // An error we cannot attribute ⇒ a partial map would lie.
                all_errors_attributed = false;
            }
        }
    }
    PerCrate {
        verdicts: verdicts.into_iter().collect(),
        error_count,
        all_errors_attributed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn err(path: &str) -> Diagnostic {
        Diagnostic {
            file_path: PathBuf::from(path),
            line: 1,
            col: 1,
            severity: Severity::Error,
            code: Some("E0308".into()),
            message: "mismatched types".into(),
            source: Some("rustc".into()),
        }
    }
    fn warn(path: &str) -> Diagnostic {
        Diagnostic {
            file_path: PathBuf::from(path),
            line: 1,
            col: 1,
            severity: Severity::Warning,
            code: None,
            message: "unused".into(),
            source: Some("rustc".into()),
        }
    }

    // ---- pure parsers (no filesystem) ----

    #[test]
    fn package_name_basic_and_virtual() {
        assert_eq!(
            parse_package_name("[package]\nname = \"physics\"\nversion = \"0.1\"\n").as_deref(),
            Some("physics")
        );
        // Virtual manifest (workspace-only) ⇒ no package name.
        assert_eq!(parse_package_name("[workspace]\nmembers = []\n"), None);
        // `name` outside [package] (e.g. a [[bin]] name) must NOT match.
        assert_eq!(
            parse_package_name("[package]\nversion=\"1\"\n[[bin]]\nname=\"x\"\n"),
            None
        );
        // Comment-tolerant (house `strip_comment`).
        assert_eq!(
            parse_package_name("[package] # pkg\nname = \"chemistry\" # the name\n").as_deref(),
            Some("chemistry")
        );
    }

    #[test]
    fn workspace_members_multiline_and_inline() {
        // The multi-line house form (this repo's own root Cargo.toml).
        let ml = "[workspace]\nresolver = \"3\"\nmembers = [\n  \"crates/cargoless-proto\",\n  \"crates/cargoless\",\n]\n[workspace.package]\nedition=\"2024\"\n";
        assert_eq!(
            parse_workspace_members(ml),
            vec!["crates/cargoless-proto", "crates/cargoless"]
        );
        // Inline form + a trailing glob preserved verbatim.
        let inline = "[workspace]\nmembers = [\"a\", \"crates/*\"]\n";
        assert_eq!(parse_workspace_members(inline), vec!["a", "crates/*"]);
        // No workspace table ⇒ empty.
        assert_eq!(
            parse_workspace_members("[package]\nname=\"x\"\n"),
            Vec::<String>::new()
        );
    }

    // ---- CrateMap attribution ----

    #[test]
    fn longest_prefix_wins() {
        let m = CrateMap::from_pairs([
            ("/ws", "root-crate"),
            ("/ws/crates/physics", "physics"),
            ("/ws/crates/isolation", "isolation"),
        ]);
        assert_eq!(
            m.crate_of(&PathBuf::from("/ws/crates/physics/src/orbit.rs")),
            Some("physics")
        );
        assert_eq!(
            m.crate_of(&PathBuf::from("/ws/src/main.rs")),
            Some("root-crate")
        );
        assert_eq!(m.crate_of(&PathBuf::from("/elsewhere/x.rs")), None);
    }

    // ---- aggregate: the attribution rule + honesty invariant ----

    #[test]
    fn green_crates_seeded_even_with_no_diagnostics() {
        let m = CrateMap::from_pairs([("/ws/p", "physics"), ("/ws/i", "isolation")]);
        let pc = aggregate(&[], &m);
        assert_eq!(
            pc.verdicts,
            vec![
                ("isolation".to_string(), Verdict::Green),
                ("physics".to_string(), Verdict::Green),
            ]
        );
        assert_eq!(pc.error_count, 0);
        assert!(pc.all_errors_attributed);
    }

    #[test]
    fn only_errors_flip_a_crate_red_not_warnings() {
        let m = CrateMap::from_pairs([("/ws/p", "physics"), ("/ws/i", "isolation")]);
        // physics: a warning only ⇒ stays GREEN (matches the
        // authoritative tree-verdict rustc-error rule).
        // isolation: a real error ⇒ RED.
        let pc = aggregate(&[warn("/ws/p/src/a.rs"), err("/ws/i/src/b.rs")], &m);
        assert_eq!(
            pc.verdicts,
            vec![
                ("isolation".to_string(), Verdict::Red),
                ("physics".to_string(), Verdict::Green),
            ]
        );
        assert_eq!(pc.error_count, 1);
        assert!(pc.all_errors_attributed);
    }

    #[test]
    fn unattributed_error_marks_map_untrustworthy() {
        // THE honesty invariant: an error under no known crate must NOT
        // yield a silently all-green per-crate map. `all_errors_attributed`
        // goes false ⇒ caller omits the `crates=` line; `verdict=` stands.
        let m = CrateMap::from_pairs([("/ws/p", "physics")]);
        let pc = aggregate(&[err("/somewhere/else/gen.rs")], &m);
        assert_eq!(pc.verdicts, vec![("physics".to_string(), Verdict::Green)]);
        assert_eq!(pc.error_count, 1);
        assert!(
            !pc.all_errors_attributed,
            "an unattributable error must mark the per-crate map untrustworthy"
        );
    }

    #[test]
    fn multiple_errors_same_crate_count_each() {
        let m = CrateMap::from_pairs([("/ws/p", "physics")]);
        let pc = aggregate(&[err("/ws/p/src/a.rs"), err("/ws/p/src/b.rs")], &m);
        assert_eq!(pc.verdicts, vec![("physics".to_string(), Verdict::Red)]);
        assert_eq!(pc.error_count, 2, "scalar counts errors, not crates");
        assert!(pc.all_errors_attributed);
    }
}
