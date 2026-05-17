# D-OPENCLOSED — Structural-Completeness Trigger (agent-input model)

**Status:** DESIGN ONLY. No behavior change on `main`. Investigate-and-
propose; the operator decides implement-or-not. Author: dev-fixer.
Scratch branch `agent/dev-fixer-openclosed-design` off `main @ 04c7de5`.

**Reframe (operator, central — not a footnote):** cargoless's primary
input is **AI agents writing whole pieces** — atomic `Write`/`Edit` of
complete files — **not humans streaming keystrokes**. cargoless is an
**agent-loop tool**: it tells *the agent* the moment its code stops
compiling. Every design choice below is centered on that. The honest
cost unit is **per agent-edit-batch**, never per-keystroke.

## VERDICT

> **FEASIBLE-WITH-PROXY — and materially *stronger* under the agent-input
> model than under human keystrokes.** RA exposes no clean stable-LSP
> parse-state object, but its parser's syntax errors already arrive on
> the `publishDiagnostics` stream cargoless consumes
> (`source:"rust-analyzer"`, `severity:Error`) — zero new transport.
> Recommended primary signal: a dependency-free local
> delimiter/string/comment-balance lexer over each file in the
> fs-watcher's coalesced `ChangeBatch`. Under agent input the
> structural gate is **almost free and almost always trivially CLOSED**
> (agent edits are intended-complete by construction), so the proxy's
> accuracy budget is generous; its real job is the two cases that *do*
> occur in agent loops — (a) a genuinely broken intermediate edit, and
> (b) a multi-file batch observed mid-batch — and in both, closedness
> only gates the *cargo-check spend*, never the verdict, so there is
> **no wrong-cache path** regardless of proxy accuracy. Recommend a
> bounded spike behind a default-off knob.

---

## 1. Feasibility — is the structural signal available?

### 1.1 What cargoless already observes (exact sites)

cargoless drives rust-analyzer over LSP and folds events; it also runs
its **own** `notify` fs-watcher that coalesces change events:

- **fs-watcher coalesce (the agent-write seam):**
  `crates/tf-core/src/watcher.rs:206 Debouncer` — `record(path, now)`
  (`:222`) accumulates a `BTreeSet<PathBuf>`; `poll(now)` (`:229`)
  drains a coalesced **`ChangeBatch`** once the tree has been quiet for
  `quiet`. An agent's atomic `Write` of a file surfaces as exactly one
  (or, for a multi-file edit, a few) notify events → one `ChangeBatch`.
- **debounce duration:** `crates/tf-core/src/model.rs:81-94
  resolve_watch_debounce` / `DEFAULT_WATCH_DEBOUNCE` (150 ms) /
  `TF_DEBOUNCE_MS` (#49 knob).
- **diagnostics fold:** `crates/tf-core/src/lsp.rs:343
  PublishDiagnostics` splits errors by source —
  `authoritative_errors` (`source=="rustc"`, cargo-check/flycheck) vs
  `advisory_errors` (`source=="rust-analyzer"`, RA-native). Folded at
  `crates/tf-core/src/model.rs:283 Model::apply_event`. GREEN gated on
  `flycheck_done` + zero authoritative errors (`model.rs:366`
  reconcile; header `model.rs:11-33`).
- **publish gate:** `StateEvent::BecameGreen{identity}` →
  `crates/tf-cli/src/build.rs` → build-cas atomically advances
  `.cargoless/latest-green` (AC#4 fail-closed).

### 1.2 Does RA expose parse/syntax state directly?

**No clean dedicated API over our LSP surface.** RA has a rich internal
syntax tree and an *unstable* `rust-analyzer/syntaxTree` debug
extension (renders a tree for a doc on request — not a closedness
state, explicitly unstable). Stable LSP has no "is this buffer
structurally complete" request.

**But RA's syntax errors are already in the stream we consume.** RA's
parser is error-resilient and runs independent of cargo; it reports
parse failures (unbalanced delimiters, incomplete items/exprs) as
diagnostics with `source:"rust-analyzer"`, `severity:Error`. The
codebase already depends on this: `model.rs:287-300` (F8-redo)
documents that RA-native parse errors never reach cargo-check yet are
unambiguous "cannot compile" evidence. So an RA-derived OPEN signal
needs **zero new transport** — it is a classification of diagnostics
already arriving at `model.rs:283`. Gap: `advisory_errors` lumps RA
*syntax* with RA *semantic* errors; isolating syntax requires reading
`Diagnostic.code`/`message` (captured at `lsp.rs:~484
extract_one_diagnostic`), whose shape is an **RA-internal detail with
no stability guarantee** (Risk R3).

### 1.3 Cheapest faithful proxy (recommended primary)

A **local, dependency-free balance lexer** run over each file in the
`ChangeBatch` (`watcher.rs:229 poll`): track `{}`/`()`/`[]` nesting,
string/char/byte/raw-string termination, block-comment nesting. NOT a
Rust grammar — closedness is a *worth-checking heuristic*, not the
verdict. This matches the codebase's explicit hand-rolled-minimal ethos
(watcher hand-rolls gitignore+debounce; statusfile its format; CLI its
argparse — "no dep until it earns its place"). Read-site:
**`model.rs` watch-loop, at the `Debouncer::poll` → `ChangeBatch`
consumer** — the batch's file contents are already in hand there.

> Under agent input the lexer is almost always trivially CLOSED (agents
> emit complete files). Its accuracy budget is therefore generous and
> conservative-OPEN bias (R5) is nearly costless. RA-native-syntax-error
> is an *optional* corroborating secondary — never on the critical path
> (keeps R3 fragility out).

---

## 2. State machine — centered on agent-whole-file-write

### 2.1 The agent-input observation that drives everything

A human keystroke stream spends **most** of its wall-clock in OPEN
(mid-token, mid-expression). An **agent** does not type — it emits a
*finished artifact* in one `Write`. So:

- **OPEN is a RARE, SHORT TRANSIENT**, occurring only when (a) the agent
  produced genuinely broken code (real syntax error in generated
  output), or (b) we observe a *multi-file* edit mid-batch (file A
  landed, file B in flight). It is **not** the steady state.
- **CLOSED is the by-construction default** the instant the fs-watcher's
  coalesced `ChangeBatch` settles on a well-formed agent edit.
- Therefore the **natural trigger is "fs-watcher emitted a coalesced
  batch AND that batch parses CLEAN"** — which, for a well-formed agent
  edit, is true the moment the batch is observed. No keystroke
  debouncing is being modelled.

### 2.2 States

| State | Meaning (agent-input) | Behavior |
|---|---|---|
| **CLOSED** | The coalesced `ChangeBatch` parses structurally clean (the common case for an agent edit) | Eligible for the authoritative cargo-check tier; only a CLOSED tree may advance `.cargoless/latest-green` |
| **OPEN** | A file in the batch is parse-broken (rare: bad generated code, or batch observed mid-multi-file-write) | Suppress the authoritative tier; never advance latest-green; not cacheable |
| **NEUTRAL** | A CLOSED→OPEN transition (a follow-up write re-broke a previously-clean batch) | Reset pending closed-quiescence; collapses to OPEN for triggering, distinct only for instrumentation |

### 2.3 The #49 debouncer is demoted to a multi-file-batch safety-net

Today the debouncer is the *primary* trigger mechanism (time-quiescence
fires the check). Under the agent model it becomes a **safety-net for
one specific case**: an agent writes file A, then file B ~200 ms later
(two `Write` tool calls for one logical change). Without coalescing
we'd check twice — once on A alone (crate incomplete → spurious RED),
once on A+B. The existing `Debouncer` (`watcher.rs:206`) **already**
solves this: `record` accumulates, `poll` yields the *union* batch once
quiet for `quiet`. We keep its mechanism **unchanged**; we only add
closedness as a **conjunctive precondition on the fire**:

```
// model.rs watch-loop, at the Debouncer::poll consumer:
if let Some(batch) = debouncer.poll(now) {
    if batch_all_closed(&batch) { run_authoritative(batch) }
    else { /* OPEN: skip; re-evaluated when the batch next settles */ }
}
```

So: debounce window still coalesces the multi-`Write` agent batch
(its real remaining job); closedness gates whether that coalesced
batch is worth a cargo-check. "Only meaningful entries
checked/cached" becomes **nearly free** — an agent edit is
intended-complete by construction, so the common path is
batch→CLOSED→check, exactly once per logical agent edit.

### 2.4 Granularity: per-batch gate, workspace-authoritative verdict

- Closedness is computed **per file in the `ChangeBatch`**; the batch is
  "worth checking" iff **no file in it is OPEN**.
- The **verdict stays workspace-level and authoritative** —
  `flycheck_done + zero severity:Error` (`model.rs:366`) is **untouched**.
  Closedness decides only *whether to spend a cargo-check on this
  batch*, never *the colour*. This is the single load-bearing
  invariant: see §3.

### 2.5 Composition with the two frozen invariants

1. **F8-redo asymmetric RED gating** (`model.rs:287-300`): **untouched.**
   Any `severity:Error` from any source still flips per-file RED the
   instant it arrives. Closedness never suppresses RED — RED is
   positive breakage evidence and is valid even for an OPEN batch
   (indeed an OPEN batch is *trivially* RED-worthy). Closedness gates
   only the authoritative *spend* and GREEN/publish eligibility.
2. **never-publish-red / AC#4** (`build.rs` → build-cas):
   **strengthened, fail-closed preserved.** Add a necessary
   precondition: latest-green advances only if the tree was **CLOSED**
   at the flycheck pass that produced the green. A green off a
   transiently-OPEN-then-CLOSED batch is *still* a real cargo-check
   green (cargo only succeeds on compilable input) — so this can only
   ever *withhold* a publish, never cause a wrong one.

---

## 3. Risk list (agent-input framing)

| # | Risk | Disposition |
|---|---|---|
| **R1** | Parse-clean ≠ compiles (a CLOSED agent file can still be semantically wrong) | **Non-issue by construction.** CLOSED only makes the batch *eligible*; cargo-check still runs and still decides green/red. CLOSED is structural-necessary, never green-sufficient — the same asymmetry F8-redo already encodes. |
| **R2** *(primary risk under agent input)* | Multi-file agent batch: file A written CLOSED, but the crate is incomplete until file B lands (A references an item B defines) | **No wrong-cache path — proven.** Two sub-cases: **(i)** the `Debouncer` coalesces A+B into one `ChangeBatch` (the common case — the safety-net of §2.3), so the check sees the complete batch; **(ii)** if A and B arrive far enough apart to be separate batches, A's batch is CLOSED → cargo-check runs → returns **RED** (real "unresolved reference" evidence) → handled by the *untouched* F8-redo path; the next batch (B) re-checks and settles green. In neither sub-case does closedness feed the verdict (§2.4) — worst case is an *extra* RED or a *skipped* check, never a green/publish on non-green. AC#4 is untouched. |
| **R3** | RA-internal-dependency fragility (RA syntax-error code/message format drifts across RA versions) | **Off the critical path.** Primary signal = local lexer (no RA dep); RA-native-syntax-error is optional corroboration only. RA format drift ⇒ lexer still fully drives OPEN/CLOSED — graceful degradation, no gate breakage (contrast: RA-primary would inherit AC#6-class fragility). |
| **R4** | Transient broken intermediate (agent writes a syntactically-invalid draft, then immediately fixes it) | **This is the *intended* win, not a risk.** Under the current model both the broken draft *and* the fix get cargo-checked on their respective quiescence edges. Under closedness the broken draft is skipped (RA-native RED already tells the agent it's broken, cheaply) and only the fixed CLOSED batch is cargo-checked. Strictly fewer, strictly more-meaningful. |
| **R5** | Lexer false-CLOSED on exotic-but-valid lexemes (nested raw strings `r#"…"#`, byte strings, lifetime `'a` vs char `'a'`) | Balance-lexer only, NOT a grammar (`<>` are not delimiters lexically, so no turbofish/comparison ambiguity). Raw-string hashes + char-vs-lifetime are the only tricky lexemes (~30 lines, well-specified). Conservative-OPEN on scanner uncertainty ⇒ self-healing (skipped batch re-evaluated). Spike must carry a raw-string/char/lifetime corpus. |

---

## 4. Expected effect under AGENT input + measurement seam

### 4.1 Why this is strictly-fewer / strictly-more-meaningful

The cost unit is **per agent-edit-batch**. Two distinct wins, both
specific to agent input:

1. **Broken intermediate elimination (R4).** Agent loops routinely emit
   syntactically-invalid intermediate states — partial refactors,
   a `Write` that will be corrected by the next tool call, generated
   code with a brace bug. Today every such state that reaches a
   debounce-quiescence edge triggers a full cargo-check to re-derive a
   RED the RA parser already knew for free. Under closedness those are
   skipped. The fired set becomes a **strict subset**: every
   CLOSED∧settled instant is also a settled instant; the converse fails
   for every broken intermediate. The agent still gets the RED verdict
   immediately (RA-native, F8-redo path) — only the *redundant
   cargo-check* is removed.
2. **Multi-file-batch collapse.** An agent rewriting N files for one
   logical change triggers **one** authoritative check (when the
   coalesced batch is CLOSED+settled) instead of up to **N** (one as
   each file lands, several of them against a transiently-incomplete
   crate). This is the larger win under agent input and is exactly the
   §2.3 safety-net behavior.

Real-green latency for a *correct* agent edit is **unchanged** — the
batch is CLOSED the moment it settles, so the triggering edge fires as
today. Only provably-incapable-of-a-new-green checks are removed.
Secondary (consequence, not claimed here): fewer cargo-check
invocations ⇒ lower sustained CPU/RSS during an agent run — the
Framing-C/#71 throughput concern.

### 4.2 bench-lead quantification hook (do NOT measure here)

Seam: instrument the `model.rs` watch-loop **`Debouncer::poll →
ChangeBatch` consumer** (§2.3) to emit a counter on **(a)** every
settled batch (would-fire today) and **(b)** every settled *CLOSED*
batch (would-fire proposed), over a **synthetic agent-edit trace** —
sequences of atomic multi-file `Write` batches including deliberate
broken-intermediate and split multi-file cases (the realistic
agent-loop shape; the `bench/fixture` Leptos `view!` corpus is the
substrate, driven as agent-style whole-file rewrites rather than
keystrokes). Metric: **`1 − (b/a)` = fraction of authoritative
cargo-checks eliminated per agent-edit-batch**, plus cargo-check
CPU-seconds delta over the trace. **bench-lead owns running this**;
this note only names the counter site, the unit (agent-edit-batch),
and the metric.

---

## 5. Recommendation & operator decision points

**Recommend: PROCEED to a bounded spike** on the local-lexer proxy
(§1.3) behind a default-off env knob (idiom matching `TF_DEBOUNCE_MS`,
e.g. `TF_STRUCTURAL_TRIGGER=1`), additive alongside the byte-frozen
`StateEvent` seam — zero risk to the frozen contract or AC#4, fully
reversible, measurable via §4.2 before any default flip. The
agent-input reframe *strengthens* the case: the gate is almost-free,
almost-always-CLOSED, and its wins (broken-intermediate skip,
multi-file collapse) are exactly the wasteful patterns agent loops
generate.

Operator decisions surfaced:
1. **Proxy choice:** local-lexer-primary (recommended) vs
   RA-native-syntax-error-primary (zero new code, R3-fragile) vs
   both-required (most conservative).
2. **Default disposition:** ship default-off knob and let §4.2 evidence
   justify a later default-on, or keep permanently opt-in.
3. **Publish precondition (§2.5.2):** adopt the strengthened
   never-publish-unless-was-CLOSED (recommended — strictly safer, free)
   vs apply closedness to the *trigger* only (smaller change, AC#4
   byte-unchanged).
4. **Ownership split (proposed, lead to confirm):** dev-fixer owns the
   `model.rs`/`watcher.rs` trigger-seam implementation; bench-lead owns
   the §4.2 agent-edit-batch quantification.

No code changes proposed; nothing on `main` is touched.
