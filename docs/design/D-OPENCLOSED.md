# D-OPENCLOSED — Structural-Completeness Trigger (investigation + design)

**Status:** DESIGN ONLY. No behavior change on `main`. This is an
investigate-and-propose note; the operator decides implement-or-not on
the findings. Author: dev-fixer. Scratch branch
`agent/dev-fixer-openclosed-design` off `main @ 04c7de5`.

## VERDICT

> **FEASIBLE-WITH-PROXY.** rust-analyzer does **not** expose a clean,
> stable-LSP "syntax-tree / parse-complete" state object over the LSP
> surface cargoless already speaks. **But** RA's parser runs continuously
> and *already* emits its syntax errors into the
> `textDocument/publishDiagnostics` stream cargoless already consumes
> (`source: "rust-analyzer"`, `severity: Error`) — so an RA-derived OPEN
> signal is available **with zero new transport**. Because RA-native
> diagnostics conflate *syntax* with *semantic* errors and RA's
> syntax-error code/message is an RA-internal detail (fragility), the
> **recommended primary signal is a dependency-free local
> delimiter/string/brace-balance lexer** over the changed buffer (the
> operator's own proxy #1), with the RA-native syntax-error signal as an
> optional corroborating secondary. The state machine is a *trigger
> heuristic only* — the authoritative cargo-check tier still decides
> green/red, so cache correctness (AC#4) is structurally unaffected
> regardless of proxy accuracy. Net: implementable, low-blast-radius,
> additive; recommend proceeding to a spike on the local-lexer proxy.

---

## 1. Feasibility — is the structural signal available?

### 1.1 What cargoless already observes (exact sites)

cargoless does not parse Rust itself; it drives rust-analyzer over LSP
and folds events:

- `crates/tf-core/src/lsp.rs:343` `PublishDiagnostics` — one
  `textDocument/publishDiagnostics` reduced. It already splits by
  source: `authoritative_errors` (`source == "rustc"` — cargo-check /
  flycheck) vs `advisory_errors` (`source == "rust-analyzer"` —
  RA-native). `lsp.rs:388 has_any_severity_error()`,
  `lsp.rs:369 is_green()`.
- `crates/tf-core/src/model.rs:281 Model::apply_event` folds
  `LspEvent::{Diagnostics, FlycheckEnded, IndexingEnded}`. GREEN is
  gated strictly on `flycheck_done` + zero authoritative errors
  (`model.rs:366` reconcile rule; header `model.rs:11-33`).
- The streaming entry point is `tf_core::model::watch` (consumed by
  `crates/tf-cli/src/watch.rs`); the FS save-burst coalescer is
  `crates/tf-core/src/watcher.rs:206 Debouncer` +
  `model.rs:81-94 resolve_watch_debounce` / `DEFAULT_WATCH_DEBOUNCE`
  (150 ms) / `TF_DEBOUNCE_MS` (#49 knob).
- Publish gate: `StateEvent::BecameGreen { identity }` →
  `crates/tf-cli/src/build.rs` → build-cas atomically advances
  `.cargoless/latest-green` (AC#4 fail-closed).

### 1.2 Does RA expose parse/syntax state directly?

**No clean dedicated API over our LSP surface.** rust-analyzer has a
rich internal syntax tree (rowan) and an *unstable* extension
`rust-analyzer/syntaxTree`, but that returns a debug-rendered tree for a
document on request — it is a debugging endpoint, not a
parse-error/closedness state, and is explicitly unstable. Stable LSP has
no "is this buffer structurally complete" request.

**However — RA's syntax errors are already in the stream we consume.**
RA's parser is error-resilient and runs independent of cargo; it reports
parse failures (unbalanced delimiters, incomplete items/expressions) as
diagnostics with `source: "rust-analyzer"` and `severity: Error`. The
codebase *already depends on this fact*: `model.rs:287-300` (F8-redo)
documents that "RA-native parse errors don't make it to cargo-check …
yet are unambiguous evidence the file cannot compile" and folds them
into per-file RED. So an **RA-derived OPEN signal needs zero new
transport** — it is a classification of diagnostics already arriving at
`model.rs:283`.

The gap: `advisory_errors` lumps RA *syntax* errors together with RA
*semantic* errors (unresolved import, type mismatch). OPEN must mean
*parse-incomplete*, not "RA found a semantic problem" (a buffer can be
structurally CLOSED yet semantically wrong — that is exactly the case
the authoritative tier exists to catch). Distinguishing them requires
reading `Diagnostic.code` / `message` (captured at
`lsp.rs:~484 extract_one_diagnostic` into `tf_proto::Diagnostic{ code:
Option<String>, message }`). RA tags syntax errors with a recognizable
code/message family ("Syntax Error: …"), but that string/code shape is
an **RA-internal detail with no stability guarantee** — see Risk R4.

### 1.3 Cheapest faithful proxy (recommended primary)

A **local, dependency-free lexer** computing delimiter/string/char/raw-
string/block-comment balance over the changed buffer. It does NOT need
to be a Rust parser — OPEN/CLOSED is a *worth-checking heuristic*, not
the verdict. Faithful enough: the dominant "mid-edit, not meaningful"
states (unbalanced `{}`/`()`/`[]`, unterminated string/char, open block
comment, trailing `.`/`::`/`,` operator with no RHS) are exactly what a
~150-line scanner catches with zero false *negatives* on the cases that
matter (it can be conservative — see §2.4). This matches the codebase's
explicit hand-rolled-minimal ethos: `watcher.rs` hand-rolls
gitignore+debounce, `statusfile.rs` hand-rolls its format, the CLI
hand-rolls arg parsing — all "no dep until it earns its place". The
read site is naturally **`model.rs` at the `LspEvent::Diagnostics`
fold / the `watch` loop's debounce tick**, where the changed buffer
text is already in hand (RA `didChange`/our watcher batch).

> Recommendation: **local-lexer balance proxy as the authoritative
> closedness signal**; RA-native-syntax-error as an *optional*
> corroborating secondary (if present it strengthens OPEN; its absence
> never forces CLOSED — the lexer is sufficient). This removes the
> RA-internal-format dependency from the critical path (R4).

---

## 2. State-machine specification

### 2.1 States

| State | Meaning | Trigger behavior |
|---|---|---|
| **OPEN** | Buffer not structurally meaningful (unbalanced / parse-error) | Suppress the authoritative (cargo-check) tier; **never** advance `.cargoless/latest-green`; do not treat any verdict as cacheable |
| **CLOSED** | Buffer parses structurally clean | *Eligible* for the authoritative tier; only a CLOSED tree may advance latest-green |
| **NEUTRAL** | A CLOSED→OPEN transition just occurred | Reset: discard the pending closed-quiescence timer; equivalent to OPEN for triggering, distinct only for instrumentation |

### 2.2 Transitions

```
        structural-clean            structural-broken
OPEN ───────────────────────▶ CLOSED ───────────────────────▶ NEUTRAL
  ▲                              │                                │
  │           (re-edit broke it) │                                │
  └──────────────────────────────┘◀───────────────────────────────┘
                       NEUTRAL collapses to OPEN immediately
                       (it is OPEN + "we were just CLOSED" tag)
```

The *real trigger* is the **edge into "CLOSED ∧ quiescent"**:
`CLOSED` holds **and** the #49 debouncer's quiet window has elapsed with
no further change. Not pure time (today's model), not pure keystroke
edge — the conjunction.

### 2.3 Granularity — per-file gate, workspace verdict (unchanged)

- **Closedness is computed per changed file** (the lexer runs on the
  buffer that changed). A workspace is "closed enough to check" when
  **no changed-since-last-check file is OPEN**. This is deliberately a
  *heuristic for worth-checking*, NOT a per-file verdict.
- The **verdict stays workspace-level and authoritative**: the existing
  `flycheck_done + zero severity:Error` rule (`model.rs:366`) is
  untouched. Closedness only decides *whether to spend a cargo-check*,
  never *what colour the tree is*. A per-file-closed-but-crate-
  incomplete situation (R3) therefore cannot mis-colour the tree — worst
  case is a *skipped* check, caught on the next CLOSED∧quiescent edge.

### 2.4 Composition with the three existing invariants

1. **#49 debouncer** (`watcher.rs:206`, `model.rs:81-94`): unchanged
   mechanism; closedness becomes an **additional precondition** on the
   debounce *fire*. Pseudologic at the watch-loop tick:
   `if debouncer.poll(now).is_some() && workspace_closed() { run_authoritative() } else { skip }`.
   The debouncer keeps its 150 ms/`TF_DEBOUNCE_MS` window; we add the
   closedness conjunction. Conservative-OPEN bias: if the lexer is
   uncertain, treat as OPEN (skip) — a skipped check is self-healing
   (the next quiescent edge re-evaluates); a wrongly-fired check is
   merely the *current* cost we are trying to reduce, never a
   correctness bug.
2. **F8-redo verdict gating** (`model.rs:287-300`): **untouched.** RED
   still flips on any `severity:Error` from any source the instant it
   arrives (asymmetric-evidence rule). Closedness does NOT gate RED —
   only the *authoritative-tier spend* and *GREEN/publish eligibility*.
   Rationale: RED is "we have positive evidence of breakage"; that
   evidence is valid even mid-edit and must not be suppressed.
3. **never-publish-red / AC#4** (`build.rs` → build-cas): **strengthened,
   not weakened.** Today: latest-green advances on
   `BecameGreen{identity}` (flycheck-backed green). Proposed: an
   additional necessary precondition — the tree was **CLOSED** at the
   flycheck pass that produced the green. A green derived from a
   transiently-closed-then-reopened buffer is *still* a real cargo-check
   green (cargo only succeeds on compilable input), so this is purely
   *more* conservative: it can only ever *withhold* a publish, never
   cause a wrong one. The fail-closed direction is preserved.

---

## 3. Risk list

| # | Risk | Disposition |
|---|---|---|
| **R1** | "Parses clean ≠ compiles" — a CLOSED buffer can still be semantically broken | **Non-issue by construction.** CLOSED only makes the buffer *eligible* for the authoritative tier; cargo-check still runs and still decides. CLOSED is a worth-checking gate, never a green claim. The asymmetry is the same one F8-redo already encodes: structural-clean is *necessary, not sufficient* for green. |
| **R2** | Transient CLOSED mid-thought (you finish a brace, pause, keep typing) fires a wasted check | **Handled by CLOSED ∧ quiescent.** The debounce quiet window (150 ms+, user-tunable via #49) must elapse *while still CLOSED*. A keep-typing burst re-opens (or resets via NEUTRAL) before the window matures, so no fire. This is strictly better than today (today a quiescent *broken* buffer still fires). |
| **R3** | Per-file CLOSED but crate incomplete (file A balanced, but it references an item you haven't written in file B) | **No wrong-cache path.** Worst case: the authoritative tier runs and cargo-check returns RED (real evidence) → handled by the untouched F8-redo path; OR a check is *skipped* because another changed file is OPEN → self-heals on the next CLOSED∧quiescent edge. Neither path can advance latest-green on non-green (AC#4 untouched). Proven by §2.3/§2.4.4: closedness never feeds the verdict, only the spend decision. |
| **R4** | RA-internal-dependency fragility (RA syntax-error code/message format changes across RA versions) | **Removed from critical path by the §1.3 recommendation.** Primary signal = local lexer (no RA dependency). RA-native-syntax-error is optional corroboration only; if RA's format drifts, the lexer still fully drives OPEN/CLOSED — graceful degradation, no gate breakage. (Contrast: making RA the *primary* signal would inherit AC#6-class fragility.) |
| **R5** *(added)* | Lexer false-CLOSED on exotic-but-valid token soup (nested raw strings `r#"…"#`, byte strings, lifetimes-vs-char `'a` vs `'a'`, `<>` turbofish vs comparison) | Lexer must be **delimiter/string/comment-balance only**, NOT a Rust grammar. `<>` are *not* counted (they are not delimiters in the lexical grammar); raw-string hashes and char-vs-lifetime are the only genuinely tricky lexemes and are well-specified (~30 extra lines). Conservative-OPEN on any scanner uncertainty (R2 bias) makes false-CLOSED self-healing, not corrupting. Spike must include the raw-string/char/lifetime test corpus. |

---

## 4. Expected effect (qualitative) + measurement hook

### 4.1 Why this is strictly-fewer / strictly-more-meaningful checks

Today the authoritative tier fires on **time-quiescence alone** (debounce
window after the last save). That includes every pause over a
*structurally broken* buffer: every time you stop typing mid-expression
to think, look something up, or get interrupted, a full cargo-check is
scheduled against input that *cannot possibly* be green and whose RED is
already known cheaply from RA-native evidence. Those checks are pure
waste — they consume the most expensive resource in the system
(cargo-check / rust-analyzer flycheck, the AC#2/#3 cost center) to
re-derive a verdict the parser already knew.

Under CLOSED∧quiescent, the authoritative tier fires **only** on pauses
over a *structurally complete* buffer — i.e. the moments a green is
actually *possible*. The set of fired checks becomes a strict subset of
today's (every CLOSED∧quiescent instant is also a quiescent instant; the
converse fails for every mid-edit pause). So: **strictly ≤ checks, and
every suppressed check was provably incapable of producing a new green**
(it would have been RED-by-parse or unchanged). The user-visible verdict
latency for *real* greens is unchanged (the triggering quiescent edge
still fires); only the wasted broken-buffer checks are removed.

Secondary effect: fewer RA flycheck invocations ⇒ lower sustained
CPU/RSS during active editing (the Framing-C throughput concern, #71) —
but that is a *consequence to be measured*, not claimed here.

### 4.2 bench-lead quantification hook (do not measure here)

The seam where this is quantified is the existing two-mode bench
harness (`bench/harness`, S1/AC#2). The concrete hook: instrument the
authoritative-tier trigger site (the `model.rs` watch-loop
debounce-fire branch identified in §1.1/§2.4.1) to emit a counter on
**(a)** every quiescent edge (would-fire-today) and **(b)** every
CLOSED∧quiescent edge (would-fire-proposed), over a scripted realistic
edit trace (e.g. the `bench/fixture` Leptos `view!` corpus — RA's
documented weak spot, already the AC#2 substrate). The metric is
**`1 − (b/a)` = fraction of authoritative checks eliminated**, plus the
delta in cargo-check CPU-seconds over the trace. **bench-lead owns
running this**; this note only names the counter site and the metric.

---

## 5. Recommendation & operator decision points

**Recommend: PROCEED to a bounded spike** on the local-lexer proxy
(§1.3) behind a default-off flag (env idiom matching `TF_DEBOUNCE_MS`,
e.g. `TF_STRUCTURAL_TRIGGER=1`), additive alongside the frozen
`StateEvent` seam — zero risk to the byte-frozen contract or AC#4, fully
reversible, measurable via §4.2 before any default flip.

Operator decisions this note surfaces:
1. **Proxy choice:** local-lexer-primary (recommended, robust) vs
   RA-native-syntax-error-primary (zero new code but R4-fragile) vs
   both-required (most conservative, fewest fires).
2. **Default disposition:** ship default-off (knob) and let §4.2 evidence
   justify a later default-on, or keep permanently opt-in.
3. **Scope of CLOSED for publish (§2.4.3):** adopt the *strengthened*
   never-publish-unless-was-CLOSED precondition (recommended — strictly
   safer, costs nothing) or leave AC#4 exactly as-is and apply
   closedness only to the *trigger* (smaller change).
4. Whether the eventual implementation is dev-fixer's lane (model/lsp
   trigger seam) with bench-lead owning the §4.2 measurement — proposed
   ownership split, lead to confirm.

No code changes proposed in this note; nothing on `main` is touched.
