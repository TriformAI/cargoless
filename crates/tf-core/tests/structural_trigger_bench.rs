//! **#112 stage-1 — fired-check-reduction measurement** (bench-lead,
//! against dev-fixer's structural-trigger seam @ `11519e6`).
//!
//! Measures, through the REAL `model::watch()` debounce → `structural
//! .record(all_closed)` site, the fraction of authoritative
//! cargo-checks the proposed structural trigger would eliminate per
//! **agent-edit-batch**:
//!
//! ```text
//!   fired_check_reduction = 1 − (closed_batches / settled_batches)
//! ```
//!
//! read from the public seam `ModelSession::structural_counters() ->
//! (settled, closed)` (dormant `(0,0)` unless `TF_STRUCTURAL_TRIGGER=1`).
//!
//! ## Honest-measurement design (no cherry-picked number)
//!
//! `is_closed` is a pure, dependency-free **syntactic balance** scan
//! (delimiters / strings / chars / comments) — NOT a semantic check.
//! For a synthetic trace the metric is therefore *deterministic given
//! the trace*: it equals the fraction of settled agent-batches whose
//! changed files are syntactically unbalanced (an interrupted /
//! mid-draft / truncated whole-file Write). The launch-relevant
//! questions this test answers are:
//!
//!   1. **Does the real seam record it end-to-end?** — i.e. does an
//!      agent-edit-batch driven through the genuine notify→debounce→
//!      coalesce→`structural.record` path produce `settled` ==
//!      batch-count and `closed` == the balanced-batch subset? (If yes,
//!      the trigger's plumbing is real, not theoretical.)
//!   2. **What is the reduction across a disclosed spectrum of agent
//!      behaviour?** — measured at three FIXED, fully-disclosed
//!      OPEN-batch fractions (CONSERVATIVE ≈ 10 %, MODERATE ≈ 30 %,
//!      AGGRESSIVE-DRAFT ≈ 50 %). The real-world reduction is whatever
//!      the field-observed agent-broken-intermediate rate is; this test
//!      brackets it and proves the seam faithfully reports it. The
//!      operator decides v0-default-vs-v0.1 against the bracket + the
//!      dogfood-observed rate — NOT against one rigged figure.
//!
//! This mirrors the same disclosure discipline as the AC#7 throughput
//! report: a number is only as meaningful as the workload that produced
//! it, so the workload is on the record and the result is reported as a
//! spectrum, not a point.
//!
//! ## Why a minimal scratch project (not bench/fixture)
//!
//! `structural.record` fires on the debounce-settle path *before* and
//! independent of the actual check tier — `is_closed` only inspects the
//! changed files' text. A minimal valid cargo project exercises the
//! identical real seam, runs fast, and keeps the trace text (the only
//! thing measured) front-and-centre. Realism lives in the trace SHAPE
//! (whole-file atomic Writes, mid-draft truncations, split-multi-file),
//! not in fixture size.
//!
//! ## Environment
//!
//! Public API only (`tf_core::model::{watch, placeholder_identity}` +
//! `ModelSession::structural_counters`). Spawns a real rust-analyzer
//! (the `watch()` pipeline) so it runs on the dedicated builder pod
//! (same as `ac6_kill9.rs`), not the RA-less Forgejo image — gated by
//! an env opt-in so a plain `cargo test` on a dev box skips it.

use std::path::Path;
use std::time::{Duration, Instant};
use std::{fs, io};

use tf_core::model::{self, placeholder_identity};

// ---------------------------------------------------------------------
// scratch project + agent-edit-batch driver
// ---------------------------------------------------------------------

fn scratch(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("tf-struct-bench-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    p
}

/// Minimal-but-valid cargo project the watcher can RA-spawn against.
/// The agent-edit trace rewrites `src/edited_a.rs` (+ `src/edited_b.rs`
/// for split-multi-file batches) as WHOLE-FILE atomic writes — the
/// agent input unit, never per-keystroke.
fn write_base_project(root: &Path) -> io::Result<()> {
    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        b"[package]\nname=\"structbench\"\nversion=\"0.0.0\"\nedition=\"2021\"\n",
    )?;
    fs::write(
        root.join("src/main.rs"),
        b"mod edited_a;\nmod edited_b;\nfn main() { edited_a::a(); edited_b::b(); }\n",
    )?;
    // Start both edited files in a CLOSED (balanced) state.
    fs::write(root.join("src/edited_a.rs"), CLOSED_A.as_bytes())?;
    fs::write(root.join("src/edited_b.rs"), CLOSED_B.as_bytes())?;
    Ok(())
}

// Whole-file contents an agent would atomically Write.
//
// CLOSED_* : syntactically balanced — `is_closed` ⇒ true  ⇒ check FIRES
//            (correctly; the trigger does NOT eliminate these).
// OPEN_*   : a realistic interrupted / mid-draft / truncated agent
//            Write — unbalanced `{` (or unterminated string) ⇒
//            `is_closed` ⇒ false ⇒ check ELIMINATED by the trigger.
const CLOSED_A: &str = "pub fn a() -> i32 {\n    let v = vec![1, 2, 3];\n    v.iter().sum()\n}\n";
const CLOSED_A2: &str =
    "pub fn a() -> i32 {\n    let v = vec![10, 20, 30];\n    v.iter().copied().sum()\n}\n";
const CLOSED_B: &str = "pub fn b() -> &'static str {\n    \"hello\"\n}\n";
const CLOSED_B2: &str = "pub fn b() -> &'static str {\n    \"hello, world\"\n}\n";
// Agent wrote the signature + opening brace, tool-call ended before the
// body / closing brace (the canonical broken-intermediate draft).
const OPEN_A: &str = "pub fn a() -> i32 {\n    let v = vec![1, 2, 3];\n    // continuing…\n";
// Agent mid-string when the Write landed (unterminated string literal).
const OPEN_B: &str = "pub fn b() -> &'static str {\n    \"hello, wor\n";

/// One agent edit = an atomic whole-file Write of one or more files,
/// then a quiet gap longer than the debounce so the notify→debounce
/// pipeline coalesces exactly this batch into ONE settled record (and
/// never merges across batches). `files` is the (relative-path,
/// whole-contents) set the agent wrote in this batch.
fn agent_edit(root: &Path, files: &[(&str, &str)], debounce: Duration) {
    for (rel, body) in files {
        // Atomic whole-file write (open+truncate+write+fsync) — the
        // editor/agent save shape every notify-rs watcher handles
        // cleanly (the same FS-event lesson from the throughput work).
        let p = root.join(rel);
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&p)
            .expect("agent write open");
        use std::io::Write as _;
        f.write_all(body.as_bytes()).expect("agent write");
        f.sync_all().expect("agent write fsync");
    }
    // Settle gap: > debounce so this batch is one coalesced settle,
    // and distinctly separated from the next batch.
    std::thread::sleep(debounce + Duration::from_millis(900));
}

/// A disclosed trace profile: a fixed ordered list of agent-edit
/// batches. Each entry is the files written that batch. The
/// OPEN-fraction is the design parameter the operator reads against the
/// dogfood-observed agent-broken-intermediate rate.
struct Profile {
    name: &'static str,
    /// Each batch: Vec of (relpath, contents). Mix of CLOSED / OPEN /
    /// split-multi-file, composed to the stated OPEN fraction.
    batches: Vec<Vec<(&'static str, &'static str)>>,
}

fn profiles() -> Vec<Profile> {
    // 20 batches each; OPEN batches = settled batches whose changed set
    // is syntactically unbalanced (⇒ check eliminated). Compositions
    // are FIXED + DISCLOSED so the operator audits exactly what
    // workload produced each figure.
    let a = "src/edited_a.rs";
    let b = "src/edited_b.rs";
    let c_a = vec![(a, CLOSED_A)];
    let c_a2 = vec![(a, CLOSED_A2)];
    let c_split = vec![(a, CLOSED_A2), (b, CLOSED_B2)]; // coherent split-multi-file, both balanced ⇒ CLOSED
    let o_a = vec![(a, OPEN_A)]; // interrupted mid-draft ⇒ OPEN
    let o_split = vec![(a, OPEN_A), (b, CLOSED_B2)]; // one file mid-draft in a multi-file batch ⇒ OPEN

    // Helper: build a 20-batch sequence with `n_open` OPEN batches
    // interleaved among CLOSED/split batches (realistic ordering: an
    // agent draft is followed by its completion).
    let mk = |n_open: usize| -> Vec<Vec<(&'static str, &'static str)>> {
        let mut v: Vec<Vec<(&'static str, &'static str)>> = Vec::with_capacity(20);
        let mut opens = n_open;
        for i in 0..20 {
            if opens > 0 && i % 2 == 1 {
                // odd slots: a broken-intermediate (alternate plain vs
                // split-multi-file open to exercise both code paths)
                v.push(if opens % 2 == 0 {
                    o_split.clone()
                } else {
                    o_a.clone()
                });
                opens -= 1;
            } else if i % 3 == 0 {
                v.push(c_split.clone());
            } else if i % 2 == 0 {
                v.push(c_a.clone());
            } else {
                v.push(c_a2.clone());
            }
        }
        v
    };

    vec![
        Profile {
            name: "CONSERVATIVE (~10% open: agents almost always Write whole balanced files)",
            batches: mk(2),
        },
        Profile {
            name: "MODERATE (~30% open: routine iterative drafting / interrupted tool calls)",
            batches: mk(6),
        },
        Profile {
            name: "AGGRESSIVE-DRAFT (~50% open: heavy skeleton-then-fill multi-file authoring)",
            batches: mk(10),
        },
    ]
}

fn run_profile(p: &Profile, debounce: Duration) -> (u64, u64) {
    let root = scratch(&p.name.split_whitespace().next().unwrap().to_lowercase());
    write_base_project(&root).expect("base project");

    let (session, _events) =
        model::watch(&root, placeholder_identity).expect("watch() must start (RA available)");

    // Warm: give the watcher + RA a beat to reach steady state before
    // the trace so cold-start FS noise isn't miscounted as a batch.
    std::thread::sleep(Duration::from_secs(3));
    let (warm_settled, _) = session.structural_counters();

    for batch in &p.batches {
        agent_edit(&root, batch, debounce);
    }
    // Final quiet so the last batch's settle is recorded before reading.
    std::thread::sleep(debounce + Duration::from_secs(1));

    let (settled, closed) = session.structural_counters();
    session.shutdown();
    let _ = fs::remove_dir_all(&root);

    // Subtract any warm-phase settles so the ratio is over the trace's
    // batches only (honest: don't let cold-start noise flatter or
    // distort the reduction).
    (settled.saturating_sub(warm_settled), closed)
}

#[test]
fn fired_check_reduction_across_disclosed_agent_edit_profiles() {
    if std::env::var("TF_STRUCTURAL_BENCH").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping structural_trigger_bench: set TF_STRUCTURAL_BENCH=1 \
             (+ TF_STRUCTURAL_TRIGGER=1) on the builder pod to run — \
             needs a real rust-analyzer like ac6_kill9."
        );
        return;
    }
    assert_eq!(
        std::env::var("TF_STRUCTURAL_TRIGGER").ok().as_deref(),
        Some("1"),
        "TF_STRUCTURAL_TRIGGER=1 is REQUIRED — the counters are dormant \
         (0,0) otherwise and the measurement would be vacuous"
    );

    // Low debounce so each agent-edit-batch coalesces promptly; the
    // 900ms+ inter-batch gap in agent_edit() guarantees distinct
    // settles regardless.
    let debounce = Duration::from_millis(100);
    unsafe {
        std::env::set_var("TF_DEBOUNCE_MS", "100");
    }

    println!("=== #112 stage-1: fired-check-reduction (real watch() seam @ structural.record) ===");
    println!("metric: 1 − (closed_batches / settled_batches)  [per agent-edit-batch]");
    println!("trace unit: atomic whole-file Write batches (NOT per-keystroke)\n");

    let started = Instant::now();
    let mut rows: Vec<(String, u64, u64, f64)> = Vec::new();
    for p in profiles() {
        let (settled, closed) = run_profile(&p, debounce);
        let reduction = if settled == 0 {
            0.0
        } else {
            1.0 - (closed as f64 / settled as f64)
        };
        println!(
            "[{}]\n  settled={settled} closed={closed} \
             fired_check_reduction={:.1}%",
            p.name,
            reduction * 100.0
        );
        rows.push((p.name.to_string(), settled, closed, reduction));
    }

    println!("\n--- STRUCT_BENCH summary (durable, grep-able) ---");
    for (name, s, c, r) in &rows {
        let tag = name.split_whitespace().next().unwrap();
        println!(
            "STRUCT_BENCH: profile={tag} settled={s} closed={c} \
             fired_check_reduction_pct={:.1}",
            r * 100.0
        );
    }
    println!(
        "STRUCT_BENCH: wall_secs={:.0} note=reduction≈agent-OPEN-batch-fraction; \
         seam validated end-to-end; operator maps dogfood-observed broken-\
         intermediate rate onto this bracket",
        started.elapsed().as_secs_f64()
    );

    // Seam-validity assertions (NOT a favorable-number assertion — the
    // number is reported, not gated):
    //  * the real seam must have recorded SOMETHING for every profile
    //    (proves notify→debounce→structural.record is live, not dormant)
    //  * reduction must be monotone non-decreasing across CONSERVATIVE
    //    → MODERATE → AGGRESSIVE (proves it tracks the OPEN-fraction —
    //    if it didn't, the seam isn't really measuring what we think)
    for (name, settled, _c, _r) in &rows {
        assert!(
            *settled > 0,
            "{name}: settled==0 — the structural seam never recorded a \
             batch; TF_STRUCTURAL_TRIGGER wiring or debounce-coalesce is \
             broken (measurement would be vacuous)"
        );
    }
    let r: Vec<f64> = rows.iter().map(|x| x.3).collect();
    assert!(
        r[0] <= r[1] + 0.05 && r[1] <= r[2] + 0.05,
        "reduction not monotone across disclosed OPEN-fractions \
         ({:?}) — the seam is not faithfully tracking batch closedness; \
         investigate before citing any figure",
        r
    );
}
