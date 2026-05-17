# cargoless — v0 Design & Contract (D8 deliverable)

**Plane:** CWDL-19 (D8) · **Epic 1, Sprint 1** · joint-authored, sign-off gated.
**Status:** v0 contract proposed by `proto-contracts`; D1–D7 recorded at their
documented v0 stance (owners per CWDL-12…18). This document is the single place
the daemon / build+CAS / dev-server / CLI engineers reconcile against. If code
and this doc disagree, that is a bug in one of them — raise it, do not fork.

> **Vision cut.** *The codebase always knows what works, and tells you the
> moment it doesn't.* Every type and decision below is justified by either
> sharpening that self-knowledge or shortening the latency from brokenness to
> signal. Anything that does neither is v1 parking-lot, not v0.

---

## 1. Scope in one paragraph

cargoless **v0** is a single-developer, single-machine, **headless** inner-loop
tool for Rust+WASM. A daemon watches the source tree, an analyzer assigns each
file a green/red verdict, and the **moment** the tree is green the build/CAS
layer produces (or dedupe-skips) a WASM artifact and **publishes** it: the
`.cargoless/latest-green` pointer/dir advances only on a servable green build —
a red tree or failed build never moves it (AC#4, "never publish red"). v0 has
**no browser and no HTTP**. The live HTTP/WebSocket dev-server that serves the
published artifact to a browser and full-reloads it is the **v0.1** adapter
(decisions D3/D5 below) layered on top of this published output; it is
deferred, not deleted (preserved on `agent/devserver*`). v0 is explicitly
*not* browser-serving, *not* distributed, *not* multi-agent, *not* hot-swap,
*not* symbol-level — those are v0.1/v1 by construction (ROADMAP).

## 2. Decisions of record (D1–D7)

These are owned by the Lead/Engineers (CWDL-12…18); recorded here so the proto
contract is built against a fixed target. Where a decision is still formally
open, the contract is built so the open choice is **additive**, never a
reshape.

### D1 — Product name *(owner: Lead; status: OPEN, due Sprint 1 Fri)*
The shipping name is undecided. `cargoless` is the repo/binary placeholder; `tf`
is rejected (Terraform collision). **Contract impact:** none — no public name is
hardcoded in `tf-proto`; crate is `tf-proto` purely as an internal seam name.
Rationale captured so downstream README/crates.io reservation is unblocked the
moment the Lead picks; nothing in code blocks on it.

### D2 — Audience wedge: Leptos-first vs broad Rust+WASM *(owner: Lead; OPEN)*
Documented stance: **Leptos-first for zero-config defaults, mechanism stays
framework-agnostic.** Rationale: deep adoption in a focused community beats
diffuse reach for a launch; but the contract must not bake Leptos in. **Contract
impact:** none — `BuildIdentity`/`TargetTriple`/`Profile` are framework-neutral;
"Leptos-first" lives in auto-detection defaults (D7), not the seam.

### D3 — Reload protocol: Trunk-compatible vs clean *(**v0.1**; documented
stance: **Trunk-compatible**)*
Bias is compatibility so existing Trunk projects migrate with one command and
keep their bundled JS. **This is a v0.1 concern** — v0 is headless and has no
browser channel. **Contract impact:** none in v0. When the v0.1 dev-server is
built, the dev-server↔browser WebSocket is the *first place serde will be
needed*; `tf-proto` stays serde-free in v0 (§4) and the v0.1 `server` owner
adds an off-by-default `serde` feature on exactly the reload-signal types then.
The frozen v0 `BuildResult`/publisher output is shaped so the reload signal is
derivable without reshaping the contract.

### D4 — Green/red granularity *(owner: Engineer A; CONFIRMED: file-level for
v0)*
File-level is enough; symbol-level is rust-analyzer's internal model and a v1
want. **Contract impact:** `FileState`/`FileVerdict` are file-path keyed and
`Copy`; no diagnostic payload in v0 (the verdict *is* the signal — detail is the
CLI/daemon's to surface from analyzer state). Building the state model once
against this is the entire reason D4 is confirmed in Sprint 1.

### D5 — Hot-swap vs full reload *(**v0.1**; CONFIRMED: full reload)*
The browser reload mode is a **v0.1** concern (v0 publishes, it does not
serve). Full reload always works; hot-swap has edge cases and is v1+ if users
ask. **Contract impact:** none structural — the v0 publisher names the new
artifact via `ArtifactMeta`; "reload the page" vs "patch modules" is purely the
v0.1 `server` owner's behavior behind D3's channel.

### D6 — Config location: `tf.toml` vs `[package.metadata]` *(owner: Eng A/B;
documented stance: **dedicated `tf.toml`**)*
A separate file keeps `Cargo.toml` unpolluted and is unambiguous to detect.
**Contract impact:** `BuildIdentity.tf_config` is a `ContentHash` of that file —
config changes the build, so it is part of the cache identity. The *path/name*
`tf.toml` is a config-parser concern (Epic 5), not frozen in the seam.

### D7 — Zero-config auto-detection *(owner: Engineer A; SPEC)*
Detect a `Cargo.toml` whose crate is `cdylib` + a `wasm32` target; infer the
rest; fail loudly with a specific message if it cannot. Feeds AC#1 and Epic 5.
**Contract impact:** the *output* of detection populates `TargetTriple` and the
hashed config/toolchain inputs of `BuildIdentity`; detection logic itself is
Epic 5, not the seam.

## 3. The contract (`tf-proto`)

One crate, every other crate depends on it; data crosses module boundaries only
as these types. Three flows:

```
watcher → analyzer → model ──StateEvent──▶ all subscribers (verdict stream)
                       │
                       └─on BecameGreen──▶ BuildTrigger ─▶ build / tf-cas
                                                                  │
              latest-green publisher ◀──BuildResult── build/tf-cas┘
```

The v0 data-flow **ends at the publisher** — there is no browser/server sink
in v0. A future v0.1 serve adapter consumes the published output (the
`.cargoless/latest-green` pointer) without any core rewrite.

### 3.1 Content identity

| Type | Meaning | Owner of the *value* |
|---|---|---|
| `ContentHash(String)` | opaque hex hash; algorithm deliberately unspecified | `tf-cas` |
| `TargetTriple(String)` | e.g. `wasm32-unknown-unknown` | daemon (from D7) |
| `Profile { Dev, Release }` | cargo profile; v0 inner loop is always `Dev` | daemon |
| `BuildIdentity` | the **full input set**: `source_tree` + `cargo_lock` + `rust_toolchain` + `tf_config` + `target` + `profile`, each as its own field | assembled by daemon, hashed by `tf-cas` |
| `InputHash(String)` | the single derived CAS key | `tf-cas` |

The split of `BuildIdentity` into named components (rather than one opaque
string) is the contract being explicit about **what makes a build distinct**.
The reduction `BuildIdentity → InputHash` is intentionally *not* in the
contract: that hashing implementation is `tf-cas`'s, and freezing it here would
couple every crate to a hash choice. The invariant every consumer may rely on:
**equal `BuildIdentity` ⇒ equal `InputHash` ⇒ substitutable artifact.** This is
exactly the AC#5 dedupe key and the AC#4 provenance record. Adding a field to
`BuildIdentity` is therefore a deliberate, reviewed contract change — a missing
input here is a wrong-artifact bug, not a detail.

### 3.2 State model

* `FileState { Green, Red }` — file-level verdict (D4), `Copy`, no payload.
* `TreeState { Green, Red }` — aggregate; `Red` ⇒ publisher does not advance
  `.cargoless/latest-green` (AC#4, never publish red).
* `StateEvent` — the model's emitted stream:
  * `FileVerdict { path, state }` — **level-triggered**, idempotent, re-emit OK.
  * `BecameGreen { identity }` — **edge**, red→green crossing. Carries the
    now-green `BuildIdentity` so the build is triggered without a second
    round-trip to the model.
  * `BecameRed` — **edge**, green→red crossing. The latency-to-signal event:
    the instant the publisher must stop advancing latest-green (and, in v0.1,
    the instant the server freezes on last-green).

Level vs edge is the key distinction. Verdicts are level so a late subscriber
can be told current state idempotently; transitions are edges so "build now"
and "freeze now" fire exactly once per crossing — the build is *only* ever
caused by a `BecameGreen`, never a red input (AC#4 by construction).

### 3.3 Build trigger / result

* `BuildTrigger { identity }` — daemon → build/CAS; only caused by `BecameGreen`.
* `BuildOutcome` — `Deduplicated` (CAS hit, no compile — observing this proves
  **AC#5**) · `Compiled` · `Failed { reason }` (green verdict but build broke,
  e.g. link/toolchain error the analyzer can't see; one-line human reason, not
  a structured diagnostic — same v0-simple cut as `FileState`).
* `ArtifactMeta { input_hash, identity }` — persisted in the CAS beside the
  artifact: the key it's stored under **and** full provenance.
* `BuildResult { outcome, artifact: Option<ArtifactMeta> }` — `artifact` is
  `Some` iff `outcome.is_servable()`; `None` on `Failed`, where the publisher
  keeps the prior `.cargoless/latest-green` (never publish red). `BuildResult`
  drives the v0 publisher; in v0.1 it is also the input to the D3/D5 reload
  decision, but it does not itself encode *how* the browser is told.

### 3.4 The latest-green publisher (the only additive v0 surface)

The publisher is the single new v0 contract surface (ratified ledger):

- **Pointer file** `.cargoless/latest-green`, written **atomically** —
  temp file + `fsync` + `rename` — so a reader never observes a torn or
  partial pointer, and a crash mid-publish leaves the prior green intact.
- **tf-proto additive types only** (serde-free, consistent with §4):
  `PublishedArtifact { artifact: ArtifactMeta, published_at: UnixSeconds }`
  and `UnixSeconds(u64)`. Additive — the four existing seams
  (`StateEvent` / `BuildTrigger` / `BuildResult` / `ArtifactMeta`) are
  **frozen on `main` and unchanged**; this is not a reshape.
- The on-disk pointer surfaces `input_hash` / `profile` / `target` /
  `timestamp` in a **human-readable** form (so `status` and a human can read
  it without the binary).
- **Invariant = AC#4 never-publish-red:** on a servable green `BuildResult`
  the pointer advances; on `Failed` or a red tree the pointer is left
  **byte-unmoved**. Verified headless.

`server::Bundle` is **not** part of v0 — artifact framing for a browser
belongs to the v0.1 server adapter. Per-step `Cargo.lock` discipline applies
(committed lock, `--locked`).

## 4. Why dependency-free & serde-free in v0 (D8 sub-decision)

v0 is one process: every consumer links `tf-proto` and passes these by value
over in-memory channels. Nothing crosses a process/network boundary, so nothing
needs serialization. Adding `serde` now would (a) put a proc-macro dependency in
the crate the entire workspace depends on — directly taxing the cold-build time
AC#1/#2 are measured against — and (b) freeze a wire format with zero v0
consumers. **Decision: `tf-proto` carries no dependencies in v0.** When a
boundary genuinely needs the wire — the D3 WebSocket reload channel first,
remote CAS in v1 — the owning crate adds an **off-by-default `serde` feature**
to `tf-proto` and derives it on exactly the types that cross that boundary. The
shapes above are designed so that is purely additive.

`#![forbid(unsafe_code)]` is set: a pure contract crate has no business with
`unsafe`, and it keeps the crate trivially audit-clean for the OSS launch.

## 5. Acceptance-criteria traceability

| AC | Mechanism in this contract |
|---|---|
| 4 never **publish** red | `BecameRed` edge + `BuildResult.artifact: None` on failure ⇒ publisher provably keeps the prior `.cargoless/latest-green` (headless; the v0.1 server is a downstream consumer of this guarantee) |
| 5 CAS dedupe | `BuildIdentity` componentwise equality ⇒ `InputHash` equality; `BuildOutcome::Deduplicated` is the observable proof |
| 6 survives RA kill | model emits `StateEvent`s; a restarted analyzer re-emits **level** `FileVerdict`s to re-sync subscribers — no edge replay needed |
| 1 (headless) /2/ 3 (publish latency) /7 (two-mode) | unblocked, not closed here — depend on auto-detect (D7), the S1/two-mode bench, and the publisher; contract is the seam they build against |

## 6. Change protocol

`tf-proto` is frozen-by-convention after D8 sign-off. Any field add/remove/
rename is a contract change: proposed via the proto-contracts owner, reviewed by
every affected crate owner, landed before dependents adapt. Cross-crate
divergence is the specific failure D8 exists to prevent — when in doubt, this
document and the crate are authority; reconcile, never fork.
