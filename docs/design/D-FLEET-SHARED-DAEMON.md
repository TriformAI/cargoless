# D-FLEET-SHARED-DAEMON — v1 architectural target for fleet-scale RAM (Model R)

**Status:** Design note (v1 parking lot, per `CLAUDE.md` and `docs/design/D-RAM-TIERS.md`). Not v0; not v0.1 scope as currently shipped. To be picked up when post-launch data + operator priority warrant it.

**Provenance:** Original Model B+ sketch (operator-orchestrated base-daemon + per-worktree delta-clients) was superseded by **Model R** through iterative operator-architect dialogue. Model R is repo-scoped, activity-driven, overlay-multiplexed through one RA, with corun batching and a fully decoupled pinned-base / per-worktree cache lifecycle. Substantially more capable than B+ on the axes that actually matter at the operator's real workload scale.

**Motivates:** the Leg-C fleet-scale RAM constraint measured in `docs/bench/AC7-THROUGHPUT-REPORT.md §11` (commit `6497273`) **at the operator's actual scale** — 589 worktrees on the tf-multiverse repo (96.6% nested under `.claude/worktrees/`, 17 sibling `tf-multiverse-*`, 9 other), not the launch-narrative's "~20 agents" hypothetical. v0 main ships Model A with the Tier-3 ladder, getting 20 agents / 16 GB-box to BORDERLINE-fit (~19.4 GiB). At even the operator's *active subset* (probably 10-30 of those 589), Model A is the structural bottleneck the architecture was always going to need.

**Model R collapses fleet RAM from ~19.4 GiB (Model A, 20 agents) to ~1 GiB total (Model R, one RA multiplexed across N worktrees)** — a **~19× reduction**, fundamentally different than the 6× Model B+ initially estimated. Plus opt-in corun batching for N-fold verdict throughput on the common-case agent workload.

---

## 1. Problem statement

Fleet deployment of cargoless against the operator's actual workload is **~20-30 *active* AI-agent worktrees** drawn from a pool of **589 total worktrees** on the tf-multiverse Cargo workspace, on a single host. The v0 architecture (Model A in the `cargoless-real-deployment-is-agent-fleet-scale` memory framework) is N independent daemons, each with its own rust-analyzer instance.

**Topology of the 589 worktrees (operator-corrected from recon)**:
- **569 nested under `<repo>/.claude/worktrees/<name>/`** — Claude Code's agent-worktree convention; dot-prefix makes them invisible to default `ls`
- **17 sibling at `/Users/iggy/Documents/GitHub/tf-multiverse-<name>/`** — manually-managed worktrees
- **9 in other locations** — special-case checkouts

All discoverable via `git worktree list --porcelain` regardless of placement.

**Per `AC7-THROUGHPUT-REPORT §11`, measured fleet-scale per-daemon RSS under Model A:**

| Path | Per-daemon | 20 agents | 16 GB box? |
|---|---:|---:|---|
| Tier-1/2 default | ~1.5 GiB | ~30 GiB | NO (OOMs @ ~10-11) |
| + idle-evict alone (bench 75s gap) | ~1.43 GiB | ~28.6 GiB | NO |
| **+ Tier-3 #126 (shipped default-safe)** | **~0.97 GiB** | **~19.4 GiB** | **BORDERLINE** (+~3 GiB) |
| + Tier-3 + idle-evict @ real minute-gaps | ~0.7–0.9 GiB | ~14–18 GiB | PROBABLY YES |
| `--features csr` (narrowable) | ~0.53 GiB | ~10.6 GiB | COMFORTABLE YES |

The per-daemon ladder is real RAM optimization, but every step is bounded by what *one daemon* can do alone. The structural lever — sharing the heavy RA across worktrees — is what unlocks the next order of magnitude. **At 589 worktrees, even Model A's "Tier-3 default-safe BORDERLINE 20-agent" framing becomes irrelevant: manual setup of 20 cargoless daemons against an active subset of 589 worktrees is operationally infeasible. Auto-discovery + activity-activation is required.**

---

## 2. Why current sharing is limited (cargoless v0 today)

Two axes:

- **Disk-cache (CAS)**: `tf-cas` is content-addressed by `InputHash` (see `crates/cargoless-cas/src/lib.rs`). Two daemons with identical inputs produce identical CAS keys, so disk dedup happens **if they share a CAS directory**. But the current scaffolding PID-scopes the CAS path (`crates/cargoless-cas/src/lib.rs:92` — `scratch_dir` returns `cargoless-cas-{tag}-{pid}`), and no production config layer exposes `--cas-dir` or a `tf.toml` field. Nothing's shared today.
- **RAM-resident state**: each daemon spawns its own rust-analyzer process. RA's resident workspace (parsed crates, type-checked modules, salsa cache) is the load-bearing RAM consumer (~0.97 GiB at Tier-3 default-safe). Currently not shared between daemons.

The disk-cache axis is a small engineering increment (config + CLI flag); the RAM-resident axis is a structural change. **Model R addresses the RAM axis directly**: one RA per repo-workspace, multiplexed via LSP overlay across N worktrees.

---

## 3. Model R: repo-scoped, activity-driven, overlay-multiplexed

### 3.1 Architecture overview

```
                    cargoless serve --repo /path/to/<repo>
                                  │
                                  ▼
                  ┌─────────────────────────────────────┐
                  │  ONE process per repo               │
                  │  ─────────────────────              │
                  │  • auto-discovers worktrees via     │
                  │    `git worktree list`              │
                  │  • file-watcher across base + WTs   │
                  │  • activity-activated per-WT state  │
                  │  • base.cache pinned (git-advance)  │
                  │  • multiple transport adapters:     │
                  │      in-proc / unix-sock / HTTP+SSE │
                  │                                     │
                  │  ┌─────────────────────────────┐    │
                  │  │ ONE RA per workspace-cluster│    │
                  │  │ (almost always one for      │    │
                  │  │  tf-multiverse — all WTs    │    │
                  │  │  share Cargo.toml/Cargo.lock)│   │
                  │  │ ────────────────────────    │    │
                  │  │ • LSP overlay multiplexing  │    │
                  │  │ • salsa amortizes across WT │    │
                  │  │ • RSS ~0.97 GiB total       │    │
                  │  │ • idle-evictable per Tier-4 │    │
                  │  └─────────────────────────────┘    │
                  └─────────────────────────────────────┘
                                  │
                                  ▼
                       per-WT diagnostic streams
                       (asymmetric: terse green,
                        verbose red w/ file:line:crate)
```

### 3.2 The four-axis improvement over Model B+ (operator-driven)

| Axis | B+ (original sketch) | Model R |
|---|---|---|
| Setup ergonomics | Manual `cargoless serve --base <path>` + `cargoless watch --delta-of <socket>` per worktree | One `cargoless serve --repo <path>` for the whole repo |
| Worktree discovery | Operator names each one | Auto-discovery via `git worktree list` |
| Dormant worktrees | Still consume resources (delta-client process running) | **Zero resources** (no per-worktree state until activity) |
| 589-worktree fleet | Infeasible to manually configure | Trivial — one command per repo |
| Cache structure | Shared base CAS + per-client implicit | **Pinned base + per-WT tree + combined-corun caches** (decoupled lifecycles) |
| Multi-worktree verdict throughput | Serialized per-worktree by default | **Corun batching** (optimistic combined; solo fallback on red) |
| Communication | Implicit per-process | **Transport abstraction**: in-proc / unix-sock / HTTP+SSE |

The activity-activation insight is the substantive operator contribution: my B+ assumed "operator decides which worktrees get a delta-client." The reframe is **"daemon discovers + decides based on filesystem activity."** The latter scales naturally; the former doesn't.

### 3.3 Concrete daemon shape

```
$ cargoless serve --repo /Users/iggy/Documents/GitHub/tf-multiverse
[serve] discovering worktrees via `git worktree list` ...
[serve] found 589 worktrees; setting up watchers
[serve] base-RA spawned on tf-multiverse [dev] @ 49bb9c126 (RSS ~0.97 GiB at Tier-3 default-safe)
[serve] base.cache loaded from .triform/cargoless/base.cache/ (pinned @ 49bb9c126)
[serve] transports: unix socket at /tmp/cargoless-<hash>.sock; HTTP not bound
[+12s] activity in tf-multiverse-flat [flat-model]; activating per-WT state
[+12s]   overlay-set HW=a1b2c3 (3 files differ from base)
[+12s]   solo cache: miss; computing
[+12s]   verdict: green
[+47s] activity in tf-multiverse-check-queue [agent/check-remote-queue]; activating per-WT state
[+47s]   overlay-set HW=d4e5f6 (1 file differs)
[+47s]   verdict: red — type-error physics/orbit.rs:142:18 (expected f64, found f32)
[+5min] tf-multiverse-flat idle 4m+; deactivating per-WT state, freeing 80 MiB
[+5min]   (overlay-set HW=a1b2c3 retained in solo cache; no re-compute needed if re-activated)
```

One process. Zero manual setup. Activity-driven. Idle worktrees genuinely cost nothing.

---

## 4. File-watcher discovery (gitignore-inversion)

Two separate concerns require opposite treatment of `.gitignore`:

| Concern | Mechanism |
|---|---|
| **What's in base RA's workspace?** | Respect `.gitignore` + Cargo's `[workspace] members` — `.claude/worktrees/*` correctly excluded; base RA never tries to compile worktree checkouts as part of base |
| **What worktrees exist for monitoring?** | **Consciously override** `.gitignore` and walk INTO `git worktree list` paths — that's the whole point of monitoring them |

The `ignore` crate (used by ripgrep and conventional Rust file-watchers) respects `.gitignore` by default — perfect for base RA's view. For worktree-discovery, the daemon uses `git worktree list --porcelain` (the canonical source) + sets up dedicated file-watchers per worktree path regardless of gitignore status.

**Practical: one file-watcher rooted at the base path catches 96.6% of activity naturally** (since 569/589 are nested under `.claude/worktrees/` which is inside the base subtree). Only the 17 siblings + 9 other-locations need separate watchers — cheap; one inotify/FSEvents subscription per non-nested path.

---

## 5. Cache layout (operator-designed decoupled lifecycles)

The operator's key insight: **base.cache and tree.cache live on different lifecycles, decoupled by design**.

```
<repo-root>/.triform/cargoless/
  base.cache/                       ← pinned to last git pull/rebase
                                      immutable until operator explicitly advances
  tree.cache/                       ← base worktree [dev]'s in-flight overlay
                                      (operator's own edits live here)
  solo/
    <hash(HW_A)>                    ← per-worktree A solo verdict cache
    <hash(HW_B)>                    ← per-worktree B solo verdict cache
    ...
  combined/
    <hash(sort({HW_A, HW_B}))>      ← combined corun cache: WT-A + WT-B together
    <hash(sort({HW_A, HW_B, HW_C}))> ← different combinations get distinct keys
    ...

<each-worktree>/.triform/cargoless/
  tree.cache/                       ← per-WT overlay state + diagnostics
                                      (overlay-set HW computed from this)
```

**Cache lifecycle properties:**

| Cache | Invalidated by | NOT invalidated by |
|---|---|---|
| `base.cache/` | Explicit `git pull` / `git rebase` on base worktree | In-flight edits in [dev] (those go to base's tree.cache) |
| `<wt>/tree.cache/` | That worktree's edits | Other worktrees' edits; base advances (just shifts the overlay reference) |
| `solo/<HW>` | Content-addressed (HW changes ⇒ new entry, old retained) | Anything else (CAS immutability) |
| `combined/<H(set)>` | Same as solo (content-addressed) | Per-WT changes; base advances (each is a new key) |

**The decoupling matters operationally:** rapid edits in base [dev] don't trigger re-derivation across 20 active worktrees. base.cache only advances when operator explicitly chooses to via git. Per-worktree edits only affect that worktree's cache. The two lifecycles converge ONLY when operator runs git on base — a deliberate act, predictable timing.

**Cache location at `.triform/cargoless/`** matches the operator's existing organizational convention. Configurable via:
- `--state-dir <PATH>` CLI flag
- `TF_STATE_DIR=<PATH>` env
- `tf.toml` `[project] state_dir = ".triform/cargoless"`

Default: `.cargoless/` for backward-compat / OSS-out-of-the-box; operator's tf-multiverse sets `state_dir = ".triform/cargoless"`.

---

## 6. Verdict computation: one RA multiplexing N overlay-sets

### 6.1 The mechanism (not novel; RA's existing IDE-mode at scale)

`★ Observation worth naming:` cargoless's "one RA serving N worktrees" pattern is exactly how rust-analyzer serves real IDE clients today. When you open files in VS Code, each open file gets an LSP overlay — your buffer content overrides on-disk content for RA's analysis. Switch between files, edit, type — RA handles N concurrent overlays gracefully via salsa's incremental computation. **We're not inventing something exotic — we're using RA's existing IDE-mode capability with worktree-overlay semantics layered on top.**

### 6.2 Multiplexing algorithm

1. Daemon maintains per-worktree overlay-set: `{(file_path, content_or_remove), ...}` derived from `git diff base.cache..<worktree>`
2. To check worktree W:
   - Diff W's overlay-set against currently-applied overlay state
   - Send RA the diff via LSP `didChange` notifications (minimal messages)
   - RA's salsa engine incrementally re-derives only what changed
   - RA emits diagnostics via `publishDiagnostics`
3. Daemon tags diagnostics with worktree provenance: `{worktree: W, crate: <from-cargo-metadata>, file, line, column, severity, message, source}`
4. Diagnostics written to `<W>/.triform/cargoless/tree.cache/diagnostics`; verdict to `<W>/.triform/cargoless/tree.cache/cli-status`
5. Next worktree: diff to next overlay-set → didChange the delta → re-derive → tag

### 6.3 Workspace-cluster manager (rare-case handling)

LSP overlay works cleanly for `.rs` file content changes. It does **not** cleanly handle changes to:
- `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.cargo/config.toml`

These define the workspace itself, not file content. Solution: **group worktrees by `WorkspaceConfigHash`** (hash of those four files); spawn one RA per unique cluster.

For tf-multiverse's case, the dominant cluster is "everyone shares the base Cargo.toml/Cargo.lock" — one RA covers most worktrees. Worktrees experimenting with dep changes (different Cargo.toml) form their own cluster; spawn additional RA only when needed.

---

## 7. Corun batching (operator's optimization)

### 7.1 Protocol

For worktrees with non-overlapping overlay file sets (the common case in agent fleets where each agent works on independent features):

1. **Optimistic combined check**: apply the union of N worktrees' overlays as one overlay-set; run RA's analysis once; cache combined-state result at `combined/<H(set)>`
2. **Combined GREEN** → emit per-worktree green for all N worktrees in the batch
3. **Combined RED** → fall back to per-worktree solo checks for attribution (each worktree's overlay applied alone; tag diagnostics to the responsible worktree)

### 7.2 Cache layering (additive, not invalidating)

`★ Insight ─────────────────────────────────────`
**Content-addressing makes corun batching layer naturally.** Each worktree's overlay-set has a content-hash; the combined check is just a *new cache key* (sorted hash of the included overlay-hashes against the same base.cache key), additive to per-worktree solo cache entries. Nothing invalidates: per-worktree solo caches stay valid; combined caches are extra entries. The cache structure isn't being traded for another — it's adding a layer.
`─────────────────────────────────────────────────`

### 7.3 Honest caveat: combined-green doesn't strictly guarantee solo-green

**When worktrees have cross-dependencies, combined-green can hide solo-red.**

Example: worktree A adds `pub fn new_thing()` to `foo.rs`; worktree B in `bar.rs` calls `foo::new_thing()`.
- A solo: ✅ (`foo.rs` compiles with new function, no callers needed)
- B solo: ❌ (`bar.rs` references function that doesn't exist in base)
- A+B combined: ✅ (union has both)

If operator merges A based on "combined green" first, B becomes solo-verifiable. If operator merges B without A, B fails. **For agent-fleet workloads where each agent works on independent features in different parts of the workspace, cross-deps are rare.** The optimization is a real N×-throughput-win on the common case, with solo-fallback-on-red being the safety net for cross-dep cases (and merge ordering catching the rest).

**Trade-off worth accepting**, named explicitly so operator can decide:
- Combined-batch: fast (N worktrees → 1 check on common case); occasional false-positive-green requires post-merge CI as safety net
- Per-worktree-serial (Model R without corun): always-correct verdicts; N× slower throughput
- Default: corun ON; operator can disable via `--no-corun` if cross-dep prevalence is high

### 7.4 Detection options

| Approach | Description |
|---|---|
| **Always optimistic** (recommended default) | Trust combined-green; rely on merge-order/CI for rare cross-dep case |
| Optimistic + periodic audit | Occasionally run solo checks as confidence check |
| File-set-disjoint heuristic | Only batch worktrees touching disjoint file paths — limited value (cross-deps are often at type-resolution layer, not file layer) |

---

## 8. Verdict stream design (asymmetric green/red)

`★ Operator-specified product principle:` *"All Green doesn't need one per, but if something is red we need to know all about it so we can relay the information or fix correctly."*

GREEN is the boring case (no info needed beyond the verdict); RED is the entire reason the tool exists — agents need full diagnostics to fix or to route the fix to another agent. The daemon's stream/API design reflects this asymmetry:

### 8.1 GREEN-state event shape (terse)
```json
{"type":"verdict.transition","worktree":"tf-multiverse-flat","verdict":"green","crate_verdicts":{"isolation":"green","physics":"green","chemistry":"green"},"published_at":1234567890}
```

### 8.2 RED-state event shape (verbose, full provenance retained)
```json
{
  "type":"verdict.transition","worktree":"tf-multiverse-check-queue","verdict":"red",
  "crate_verdicts":{"isolation":"green","physics":"red","chemistry":"green"},
  "diagnostics":[
    {
      "crate":"physics",
      "file":"physics/src/orbit.rs",
      "line":142,"column":18,
      "severity":"error",
      "code":"E0308",
      "message":"expected `f64`, found `f32`",
      "suggestion":"convert with `as f64`",
      "source":"rustc"
    }
  ],
  "published_at":1234567891
}
```

### 8.3 Diagnostic retention

For any worktree currently in red state, the daemon retains **full diagnostics** in `<worktree>/.triform/cargoless/tree.cache/diagnostics` — queryable on-demand via API (`GET /worktrees/<W>/diagnostics`). This supports the agent-orchestration use case: "tell agent X to fix the type error at file:line in worktree Y" needs full context, not just "Y is red."

---

## 9. Per-crate verdicts + file:line diagnostics

### 9.1 Why per-crate matters

tf-multiverse is one Cargo workspace containing many crates (`isolation`, `physics`, `chemistry`, `triform-server`, ...). Today (v0): workspace-level verdict only ("green if all crates compile, red otherwise"). Per-crate verdicts enable independent agent gating ("isolation-agents can proceed even if physics is red") — important for the operator's fleet pattern where different agents work on different crates.

### 9.2 Schema extension to `cli-status`

```
schema=2
pid=<u32>
root=<canonical-path>
started=<unix-seconds>
updated=<unix-seconds>
verdict=red
crates=isolation:green,physics:red,chemistry:green
red_diagnostics=2
```

Backward-compatible: schema=1 consumers ignore unknown fields (`crates=`, `red_diagnostics=`). Cheap to implement (diagnostics ARE per-file with cargo-metadata-derivable per-crate grouping; aggregation is just a roll-up change in cargoless-core, not new state).

---

## 10. Transport abstraction (operator-specified flexibility)

### 10.1 Logical API surface (transport-agnostic)

```
get_status(worktree)         → current verdict + heartbeat + per-crate breakdown
get_verdict(worktree)        → just the verdict (light)
get_diagnostics(worktree)    → full diagnostics for current red state (heavy)
list_worktrees()             → discovered worktrees + per-WT verdict summary
subscribe(filter?)           → SSE-style transition event stream
```

### 10.2 Three transport adapters from the same codebase

```
                  Logical API
              ┌─────────────────┐
              │ get_status(wt)  │
              │ get_verdict(wt) │
              │ get_diags(wt)   │
              │ subscribe(...)  │
              └────────┬────────┘
                       │
        ┌──────────────┼──────────────┐
        │              │              │
   ┌─────────┐   ┌──────────┐   ┌───────────┐
   │in-proc  │   │unix-sock │   │HTTP/SSE   │
   │channels │   │JSON-RPC  │   │+ TCP      │
   └─────────┘   └──────────┘   └───────────┘
   single-binary  local-default   network-mode
   mode (CLI +    (existing       (--bind <addr>
   daemon in      cargoless        + auth)
   one process)   pattern)
```

| Mode | Use case | Setup |
|---|---|---|
| **Single-binary** | Developer running cargoless on their own machine; one CLI invocation does everything | `cargoless watch` — daemon + CLI in one process; in-process channels; no IPC overhead |
| **Local-split** (default fleet) | Long-running daemon + many short CLI invocations + agent orchestration on same host | `cargoless serve --repo <path>` + `cargoless status/events` CLI calls go through Unix socket |
| **Network-split** | Daemon on machine A, orchestration on machine B; "remote check" setup | `cargoless serve --repo <path> --bind 127.0.0.1:8080` (or external IP with auth); consumers hit `http://<addr>/events` (SSE) + `/status` (REST) |

### 10.3 CLI auto-discovery fallback chain

`cargoless status` and similar commands follow this chain:

1. If `--remote <url>` flag → use HTTP
2. Else look for Unix socket at conventional path (`/tmp/cargoless-<repo-hash>.sock`) → connect if present
3. Else fall back to file-reading `.triform/cargoless/.../cli-status` (current v0 behavior; works without a daemon)
4. Else spawn local daemon in single-binary mode

Operator-friendly defaults (CLI just works) + deployment flexibility (network-distribute when wanted) from one codebase.

### 10.4 Security for network mode

- HTTP exposure requires explicit `--bind <addr>` (default: don't bind to network at all)
- Auth via shared bearer token (`--auth-token <secret>` or `CARGOLESS_AUTH_TOKEN` env), mTLS, or firewall restriction
- Default to localhost-only HTTP if SSE is needed but no cross-host consumers expected (`--bind 127.0.0.1:8080` is safe; `--bind 0.0.0.0:8080` requires auth)

---

## 11. SSE vs polling consumer patterns

| Consumer pattern | Best mechanism |
|---|---|
| "What's the current verdict for worktree X right now?" | **Polling** — read `.triform/cargoless/<W>/cli-status` or `GET /worktrees/<W>/status` |
| "Tell me the moment any worktree transitions red, with full diagnostics" | **SSE stream** — `GET /events?worktrees=X,Y,Z&severity=red-only` |
| "Replay everything since timestamp T" | SSE with `Last-Event-ID` header (standard SSE replay semantics) |
| "Cheap shell-script integration" | `cargoless status` exit codes (0=live, 3=not-live) + file reads |

Both polling and SSE are first-class; consumers pick by use case. SSE handles the "react in real time to red" agent-orchestration case; polling handles "what's the dashboard state" without needing a long-lived connection.

---

## 12. Components + engineering scope

In rough order of cost:

| # | Component | Scope | Cost |
|---|---|---|---|
| 1 | Config layer + `--cas-dir`, `--state-dir`, `--repo`, `--bind`, `--no-corun` CLI flags | `cargoless-core::config`, `cargoless` CLI | Small (1 sprint) |
| 2 | CAS concurrent-writer safety verification | `cargoless-cas` stress test (likely already correct via content-addressing + atomic rename) | Small |
| 3 | Repo-scoped daemon mode (`serve --repo <path>`) + worktree auto-discovery via `git worktree list` | `cargoless` CLI restructure; `cargoless-core::repo` (new module) | Medium |
| 4 | File-watcher with per-WT routing + gitignore-inversion for worktree paths | `cargoless-core::watcher` (extend); `cargoless-core::repo::topology` | Medium |
| 5 | LSP overlay multiplexing per worktree through one RA | `cargoless-core::analyzer` (extend `LspClient` with overlay tracking + per-WT tag attribution) | Medium-large (~2 sprints) |
| 6 | Workspace-cluster manager | `cargoless-core::cluster` (new module) | Medium-large (~2 sprints) |
| 7 | Pinned-base + tree.cache + solo + combined cache layout | `cargoless-cas` extend; `cargoless-core::cache_layout` | Medium |
| 8 | Corun batching: combined overlay-set application + cache + solo-fallback on red | `cargoless-core::corun` (new module) | Medium |
| 9 | Per-crate verdict aggregation + schema=2 cli-status extension | `cargoless-core::model` extend; `cargoless-cli::statusfile` | Small-medium |
| 10 | Transport abstraction: in-process / Unix socket / HTTP+SSE adapters | `cargoless-core::transport` (new); `cargoless-proto` (event types); `cargoless` CLI | Medium |
| 11 | Diagnostic retention + queryable API (`get_diagnostics`) | `cargoless-core::diagnostics_store` (new) | Small-medium |
| 12 | Activity-activation + idle-deactivation (per-WT state lifecycle) | `cargoless-core::activity` (new) | Medium |
| 13 | Crash + restart handling (RA respawn, transport reconnect, queue replay) | `cargoless-core::supervisor` (extend AC#6) | Medium |
| 14 | Auth + security for network mode | `cargoless-core::transport::auth` | Small-medium |
| 15 | Bench characterization (Leg-C v4 with Model R + corun, measured-not-extrapolated) | `bench/` (extend harness from #116) | Small |

**Total estimate: ~5-7 sprints** (1.5-2 engineer-months) assuming familiarity with the cargoless codebase. The workspace-cluster manager (#6), LSP overlay multiplexing (#5), and transport abstraction (#10) are the load-bearing engineering items.

---

## 13. Sequencing

Recommended post-launch sequence:

1. **v0** (now, on `main @ 4687e3c`): ship Model A with the Tier-3 ladder. BORDERLINE-fit at 20 agents / 16 GB. Honest narrative per operator's Option-1 chosen at #101.
2. **v0.1 quick-win — Model C (shared CAS only)**: components #1-#2. Small engineering, modest fleet-RAM win (no RA sharing, but eliminates CAS-disk duplication + cleaner multi-worktree config story). Pre-requisite for the Model R work anyway.
3. **v1 architectural target — Model R (this doc)**: components #3-#15. Substantial engineering. Large payoff: collapses fleet RAM from ~19 GiB to ~1 GiB; auto-discovery + activity-activation make 589-worktree deployment trivial; corun batching N×-multiplies verdict throughput on the common case.

---

## 14. Open questions / unknowns

- **Cross-worktree CAS write contention**: content-addressing means two concurrent writers of the same key produce identical bytes; atomic temp+rename should prevent corruption. Verify with stress test (#2).
- **RA overlay correctness under high churn**: rapid `didChange` sequences across N worktrees. RA's existing concurrency model should handle this (it's designed for IDE LSP clients) but needs validation at fleet load.
- **Workspace-cluster cardinality in real fleet**: how often do worktrees diverge in `Cargo.toml`/`Cargo.lock`? If always 1 cluster (everyone on same base): maximum efficiency, ~1 GiB total. If 5+ clusters (worktrees experimenting with different dep versions): per-cluster RA overhead may dominate the win. Empirical question for bench characterization (#15).
- **Cross-dep prevalence in corun batching**: how often do agent worktrees have cross-deps that hide solo-red in combined-green checks? Cheap empirical answer: count solo-fallback rate during dogfood; if high, default `--no-corun`; if low, default ON.
- **Activity threshold tuning**: how much idle time before per-WT state deactivates? Trade-off: aggressive deactivation saves RAM but causes re-activation cost on returning activity. Likely tunable per `tf.toml` with conservative default (5-15 min).
- **§9a STOP-class invariants under shared CAS**: the frozen wire-format byte-constants (`b"tf-cas/input-hash/v1"` etc.) must remain inviolable when multiple writers share the CAS. Content-addressing means this is automatically safe (same input → same hash → same bytes), but worth explicit verification in #2's stress test.
- **Idle-evict (Tier-4 #122) inside Model R**: base RA can idle-evict when no per-WT state has been active for the configured window. RAM-profile under fleet-active vs fleet-idle becomes a new measurement axis.
- **Coexistence with Model A**: should `cargoless watch` default to model-A standalone if no Model R daemon is reachable, vs fail-loud? Probably default to standalone with a clear log line; explicit `--standalone` to disable auto-attach.

---

## 15. Why not just remote check?

The operator referenced remote-check-against-a-server as an existing pattern. That's effectively a hosted-cargoless service: one big remote daemon, many local clients. Different tradeoffs from Model R:

| Dimension | Remote check | Model R (local) |
|---|---|---|
| Latency | network roundtrip (10s-100s ms) | Unix socket / in-proc, sub-ms |
| Offline | requires connectivity | works offline |
| Resource location | server-side | local host |
| Trust boundary | server has source code | source stays local |
| Operational overhead | provision + scale a service | one local process |
| Multi-host fleet scaling | natural | requires N base-daemons (one per host) |

For the operator's single-host fleet (~20-30 active worktrees on one Mac), **Model R dominates remote-check on every axis except multi-host scaling.** Remote check becomes interesting at multi-host fleet scale (different problem); Model R is the right answer for single-host fleet, which is the actual workload per the `cargoless-real-deployment-is-agent-fleet-scale` memory.

**Note**: Model R's transport abstraction (§10) supports the network-mode case too. The same daemon can be `--bind`'d to a network address for the remote-check pattern. So Model R subsumes the remote-check option; operator picks deployment shape per use case.

---

## 16. References

- `docs/bench/AC7-THROUGHPUT-REPORT.md §11` (commit `6497273`) — Leg-C fleet-scale compound-fit numbers; the BORDERLINE-at-20-agents framing is the existence-pressure for Model R.
- `docs/design/D-RAM-TIERS.md` — v0/v0.1 RAM ladder; this doc is the v1 successor (the structural lever beyond per-daemon tuning).
- `docs/design/D-IDLE-EVICT.md` (#122) — Tier-4 mechanism; still applies inside Model R (base-RA can idle-evict when no per-WT state is active).
- `docs/design/D-PROCMACRO-DOWNRANK.md` (#126) — Tier-3 mechanism; default-safe.
- `CLAUDE.md` v1 parking lot — "shared cargoless daemon, N worktree clients" and "shared-CAS-backend" items; this doc operationalizes both with Model R + corun + activity-activation.
- Memory: `cargoless-real-deployment-is-agent-fleet-scale` — A/B/C framework Model R extends.
- Memory: `ram-reduction-is-operator-priority-1` — RAM as operator #1 priority motivating Model R.
- Memory: `cargoless-primary-consumer-is-agents` — agent-loop framing shapes UX expectations (per-worktree state, file:line diagnostics for fix-routing).
- Memory: `cargoless-three-layer-validation-launch-critical` — the validation discipline this design should be subjected to when implementation lands.

---

## Authorship attribution

This design is substantially operator-driven through iterative dialogue. Major contributions:

- **Operator**: activity-activated discovery (the key insight beyond manual orchestration); pinned-base + tree.cache decoupling; per-crate + file:line diagnostic requirement; asymmetric verdict stream principle (terse green / verbose red); cache location at `.triform/cargoless/`; corun batching with solo-fallback safety valve; transport abstraction (single-binary / Unix socket / HTTP+SSE); gitignore-inversion framing; topology correction (96.6% nested under `.claude/worktrees/`).
- **Lead (me)**: original Model B+ sketch (superseded), LSP overlay mechanism naming, workspace-cluster manager concept, security-for-network-mode considerations, content-addressing-makes-corun-layer insight.

The architecture is materially better for living with the workload than for designing in abstract — the operator's contributions surface from "actually running 589 worktrees with many agents" in a way an external designer wouldn't see. This authorship trail should propagate when implementation lands.

**Engineering kickoff prereq when this is picked up:** a small spike on component #2 (CAS concurrent-writer stress test under shared-dir) is the cheapest first step to validate the v0.1 model-C foundation that Model R builds on. If the CAS protocol stress-tests clean, the model-C v0.1 work is mostly config-layer plumbing and can land in 1 sprint, derisking the larger Model R engineering.
