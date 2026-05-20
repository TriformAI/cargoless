# D-INC2-2B — Increment-2 2b servedrv-consume implementation anchor

**Status:** Implementation-anchor record. Originally a design spike;
promoted to durable doc alongside the 2b implementation
(`agent/dev-fixer-2b @ d1e13bc`, L2 CLEAR / L3 in-flight) — the risk
register + composing-equivalence framing is reusable for 2c
implementation + Wave-2 metrics design.
**Source spec:** D-PUSHOVERLAY.md @ `2fb1a2a:docs/design/D-PUSHOVERLAY.md`
(docs-launch-lead's #229 Increment-2 design-ahead — authoritative
2a + 2b + 2c contract).
**Authored:** 2026-05-20 idle window during #246 Layer-3 wait.
**Worktree:** authored in `/tmp/cl-otel-w1`; implementation landed on
`agent/dev-fixer-2b` in `/tmp/cl-2b`.

---

## 0. State of the world (verified against current main 4d56021)

**2a is FULLY integrated** — every transport-side contract is on `main`:

| Surface | Status on main 4d56021 |
|---|---|
| `Request::PushOverlay { worktree, base_ref, files }` enum variant | ✓ `transport/mod.rs:375` |
| `from_json` / `to_json` codec for PushOverlay | ✓ `transport/mod.rs:405, 427` (best-effort) |
| `PushOverlayAck { worktree, accepted, applied_files }` | ✓ `transport/mod.rs:482` |
| `VerdictService::push_overlay` trait method + default body | ✓ `transport/mod.rs:141-149` (default returns `accepted: false`) |
| `TransportClient::push_overlay` + default `Err(Protocol)` | ✓ `transport/mod.rs:178-186` |
| HTTP `POST /overlay` server route + bounded body read + 401/400/413 | ✓ `transport/http.rs:305-326` |
| `HttpClient::push_overlay` (client-side wire) | ✓ `transport/http.rs:567` |
| Wire-level test `http_client_push_overlay_roundtrips_over_the_wire` | ✓ `transport/http.rs:956` |
| `ServeVerdictState` (cargoless binary's VerdictService impl) | EXISTS at `serveapi.rs:61` |
| `ServeVerdictState::push_overlay` ACTUAL OVERRIDE | ✗ **uses default = `accepted: false` no-op** |

**Net:** the WIRE plane is complete. **2b is exactly one thing: implement `ServeVerdictState::push_overlay` so it actually does something + thread the data through to the serve loop's `SwitchOverlay` arm.**

---

## 1. The four FS taps — current line locations (post-cascade)

Spec's line numbers are v0.2.0-relative; here are the LIVE locations on `agent/dev-fixer-otel-wave1 @ 30dc7d6`:

| # | Tap | Current site | What it does |
|---|---|---|---|
| 1 | Worktree set | `cargoless_core::repo::topology::list_worktrees` via `RepoScope::discover` (in `serve.rs`'s `run`) → `scope.classified()` consumed by the cluster-assignment loop in `servedrv::run` | `git -C <repo> worktree list --porcelain` |
| 2 | Workspace config | `read_workspace_config(&wt.path)` at **servedrv.rs:196** (in `run()` per-WT loop) | `std::fs::read_to_string` of 4 workspace-defining files (Cargo.toml/lock/rust-toolchain/.cargo/config) |
| 3 | File-watch | `raw_repo_watch(&repo_root)` at **servedrv.rs:220** | local `notify` watcher |
| 4 | Overlay content | `std::fs::read_to_string(f)` at **servedrv.rs:650** (inside `exec()`'s `SwitchOverlay` arm, the `for f in files` loop building `pairs`) | reads each changed file's bytes from disk |

`publish_verdict` lives at `servedrv.rs:701` — UNCHANGED by 2b (verdict egress stays statusfile-via-VerdictService, per spec §4.4).

---

## 2. Data flow sketch (the implementation pivot)

```
                              ┌──────────────────────────────┐
                              │ external client / pipeline   │
                              │ stage / dev box thin-push    │
                              └──────────────┬───────────────┘
                                             │ POST /overlay (HTTP, bearer)
                                             │ body = {worktree, base_ref, files}
                                             ▼
                              ┌──────────────────────────────┐
                              │ transport/http.rs:305-326    │
                              │ (parse + 401/400/413 gates)  │
                              └──────────────┬───────────────┘
                                             │ svc.push_overlay(wt, base_ref, files)
                                             ▼
        ┌────────────────────────────────────────────────────────────────┐
        │ ServeVerdictState::push_overlay  (THE 2b OVERRIDE — new code)  │
        │                                                                │
        │ 1. Build OverlaySet::from_pairs(files) — uses existing core    │
        │ 2. Store (worktree, base_ref, OverlaySet) in a guarded         │
        │    BTreeMap<WtId, PushedOverlay>                               │
        │ 3. Send PushIngest event on push_tx (mpsc) so the serve loop   │
        │    learns about the push                                       │
        │ 4. Return PushOverlayAck { accepted: true, applied_files }     │
        └─────────────────┬──────────────────────────────────────────────┘
                          │ Ctrl::PushIngest { wt } on push_rx
                          ▼
        ┌────────────────────────────────────────────────────────────────┐
        │ servedrv::run (the serve loop)                                 │
        │                                                                │
        │ - Drain push_rx (alongside ctrl_rx/lsp_rx/raw_rx)              │
        │ - For each PushIngest{wt}:                                     │
        │   • register WtId on first push (tap 1 substitute)              │
        │   • derive workspace config hash from pushed Cargo.toml /       │
        │     Cargo.lock content (tap 2 substitute — hash from pushed     │
        │     content, no disk read)                                      │
        │   • activity.touch(wt, now)                                     │
        │   • spawn_cluster if 0→1 edge                                   │
        │   • feed DriverEvent::RoutedBatch{wt} to clusterdrv             │
        │     (same event shape as raw_repo_watch's path — tap 3 sub)     │
        └─────────────────┬──────────────────────────────────────────────┘
                          │ ClusterAction::SwitchOverlay{wt}
                          ▼
        ┌────────────────────────────────────────────────────────────────┐
        │ exec() SwitchOverlay arm (servedrv.rs:620)                     │
        │                                                                │
        │ pairs ← (pushed-mode?)                                         │
        │   YES: ServeVerdictState::take_overlay_for(wt) → OverlaySet    │
        │        .pairs() — NO disk read (tap 4 substitute)              │
        │   NO:  std::fs::read_to_string(f) loop — UNCHANGED FS path     │
        │                                                                │
        │ Then identical: mux.switch_to(&target) → did_open/did_change/  │
        │ did_close → did_save → flycheck → barrier settle → EmitVerdict │
        │ → publish_verdict (UNCHANGED — Judgment B's sole-attribution)  │
        └────────────────────────────────────────────────────────────────┘
```

---

## 3. New state in `ServeVerdictState` + serve loop

### 3.1 ServeVerdictState gains a pushed-overlay store

```rust
// serveapi.rs additions (sketch)
pub struct PushedOverlay {
    pub base_ref: String,
    pub overlay: OverlaySet,
    pub last_push_unix: u64,
}

pub struct ServeVerdictState {
    // ... existing fields ...
    pushed: Mutex<BTreeMap<WtId, PushedOverlay>>,
    push_tx: Sender<Ctrl::PushIngest>,  // NEW — push notifications back to serve loop
}

impl VerdictService for ServeVerdictState {
    fn push_overlay(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
    ) -> PushOverlayAck {
        let wt = WtId::from(PathBuf::from(worktree));  // or path-keyed appropriately
        let overlay = OverlaySet::from_pairs(files.iter().cloned());
        let n = files.len() as u32;
        {
            let mut g = self.pushed.lock().unwrap_or_else(|e| e.into_inner());
            g.insert(wt.clone(), PushedOverlay {
                base_ref: base_ref.to_string(),
                overlay,
                last_push_unix: statusfile::now_unix(),
            });
        }
        // best-effort signal — a wedged channel doesn't fail the ack
        let _ = self.push_tx.send(Ctrl::PushIngest { wt });
        PushOverlayAck { worktree: worktree.into(), accepted: true, applied_files: n }
    }
}
```

### 3.2 Ctrl enum extension (servedrv.rs)

```rust
enum Ctrl {
    Spawned(WorkspaceConfigHash, Arc<LspClient>),
    PushIngest { wt: WtId },   // NEW — overlay was pushed for this WT
}
```

### 3.3 Serve loop drains push_rx alongside others

```rust
// Inside servedrv::run's loop body, sibling to the existing drains:
while let Ok(Ctrl::PushIngest { wt }) = ??? {
    // Register WT on first push (tap 1 substitute):
    let h = derive_cluster_hash_from_pushed_overlay(&wt, &state.pushed);
    wt_hash.entry(wt.clone()).or_insert(h.clone());
    cluster_root.entry(h.clone()).or_insert(wt.clone());

    activity.touch(wt.clone(), Instant::now());
    if let LifecycleAction::SpawnRa(_) = lifecycle.activate(path_key(&wt), h.clone()) {
        spawn_cluster(&mut clusters, &h, /* root */ wt.clone(),
                       lsp_tx.clone(), ctrl_tx.clone());
    }
    step(&mut clusters, &h, DriverEvent::RoutedBatch { wt }, &pending_batch, &api);
}
```

### 3.4 SwitchOverlay arm picks the source

```rust
ClusterAction::SwitchOverlay { wt } => {
    // ... existing tracing span + eprintln ...
    let pairs: Vec<(String, String)> = if let Some(pushed) =
        api.take_overlay_for(&wt)  // returns Option<OverlaySet> from the store
    {
        pushed.into_pairs()  // pushed-mode source
    } else {
        // existing FS-mode source (unchanged)
        let mut pairs = Vec::new();
        if let Some(files) = pending_batch.get(&wt) {
            for f in files {
                if let Ok(text) = std::fs::read_to_string(f) {
                    pairs.push((f.to_string_lossy().into_owned(), text));
                }
            }
        }
        pairs
    };
    let target = OverlaySet::from_pairs(pairs.iter().cloned());
    // ... rest unchanged: mux.switch_to(&target) → LSP verbs → did_save ...
}
```

**Mode arbitration:** **per-WT** (not global) — if a WT has received a push, its content is sourced from the store; if not, the FS path is used. Spec §4.5 ("Pushed-vs-FS is a MODE, not a replacement") fits cleanly with `if let Some(...) = take_overlay_for(wt) { ... } else { FS path }`.

---

## 4. The 4 FS taps × pushed-mode replacements

| Tap | Pushed-mode replacement | New code site |
|---|---|---|
| 1 Worktree set | First `PushOverlay` for a WT ⇒ register in `wt_hash` map | `Ctrl::PushIngest` handler in serve loop |
| 2 Workspace config | Pushed `files` contains the 4 workspace-defining files; `WorkspaceConfig::hash` runs on that content directly | `derive_cluster_hash_from_pushed_overlay` helper |
| 3 File-watch | The `Ctrl::PushIngest{wt}` event IS the change signal — directly synthesizes `DriverEvent::RoutedBatch{wt}` | Serve loop's push_rx drain |
| 4 Overlay content | `ServeVerdictState::take_overlay_for(wt)` reads from the in-memory store | SwitchOverlay arm `let pairs = ...` |

---

## 5. Tests (per spec §5 + my additions)

Plan to ship in the 2b commit:

1. **`ServeVerdictState::push_overlay` accepts + stores + signals** — given a push, the store contains the OverlaySet and a PushIngest event is sent. Validates the 4-step happy path.
2. **Composing-equivalence (load-bearing per spec §5.3)** — for the same `(prev, files)`, the `Vec<OverlayOp>` produced via the pushed store is byte-identical to the FS-disk path's output. Proves `overlay::diff` is source-agnostic + the proven isolation core is untouched. This is THE structural-correctness assertion 2b makes.
3. **Mode arbitration** — a WT with NO push falls through to FS path; a WT WITH a push uses store. Independent worktrees can be in different modes simultaneously.
4. **Sole-attribution preserved** — `publish_verdict` still called from EXACTLY one site (the EmitVerdict arm in exec). The pushed-mode change is at SOURCE of OverlaySet, NOT at egress. grep-level check.
5. **Best-effort channel** — a closed push_tx doesn't panic the push_overlay ack (returns `accepted: true` anyway — the push is recorded, only the wakeup is missed; next routine tick or next push catches up).
6. **Multiple-push coalesce** — N rapid pushes for same WT collapse to one `RoutedBatch` (since the store holds the LATEST overlay; the per-push event is a wakeup, not a queue). Avoids storm.
7. **Workspace-config hash from pushed content** — `WorkspaceConfig::hash` invoked on pushed bytes (NOT disk) yields the same hash as the FS path for the same content.

---

## 6. Implementation order (when GO)

1. **Add `Ctrl::PushIngest { wt }` variant** + plumb `push_tx`/`push_rx` channel. Compile-check.
2. **Extend `ServeVerdictState`** with `pushed: Mutex<BTreeMap<WtId, PushedOverlay>>` + `push_tx` field. Constructor takes push_tx.
3. **Implement `ServeVerdictState::push_overlay`** override (the actual ingest). Plus a `pub fn take_overlay_for(wt) -> Option<OverlaySet>` reader.
4. **Add serve loop's push_rx drain** — registers WT, computes hash from pushed config, spawn_cluster, step(RoutedBatch).
5. **Update SwitchOverlay arm** — if-let-Some pushed-mode source vs FS fallback.
6. **Write the 7 tests** (per §5 above).
7. **rustfmt + cargo clippy + ci-gate via kubectl-exec from in-tree script.**
8. **Push branch, request Layer-2/3.**

Probably 2-3 commits on the same branch: (a) `Ctrl::PushIngest` + ServeVerdictState extension, (b) serve loop drain + SwitchOverlay arm, (c) tests. Or a single atomic — TBD by the diff size at impl time.

---

## 7. 2c (thin push-client) honest-boundary call

Per spec §3:
- **Reuses** `HttpClient::push_overlay` (already on main via 2a, `transport/http.rs:567`)
- **Reuses** `transport::discovery::resolve` (precedence: `--remote <url>` → Unix socket → cli-status FileRead → SpawnLocal — already shipped)
- Adds a CLI command `cargoless push --remote <url> --repo <path> [--worktree W] [--base <ref>]` that:
  - Computes `git -C <repo> diff --name-only <base_ref>` → list of changed files
  - Reads each file's bytes via `std::fs::read_to_string` (local FS — client side)
  - Calls `HttpClient::push_overlay(wt, base_ref, files)` via discovery
  - Polls `get_status` or subscribes via SSE for the verdict
- ~50-80 LOC of CLI code (push.rs sibling to check.rs / watch.rs)
- 2c is INDEPENDENT of 2b's serve-side implementation — they meet at the wire (which is 2a's responsibility, done). 2c can land independently or together with 2b.

**Recommendation:** Land 2b first (server-side ingest is more load-bearing — the wire works whether or not the client ships); 2c is a 1-day follow-up after 2b lands.

---

## 8. Risk register

| Risk | Mitigation |
|---|---|
| `OverlaySet`'s internal repr is `BTreeMap<PathBuf, String>` — pushed `files: Vec<(String, String)>` needs path conversion. | `OverlaySet::from_pairs(files.iter().cloned())` — already content-shaped, takes `(String, String)` tuples per existing call site. Trivial. |
| Workspace-config-hash from pushed content might differ if file order matters. | `WorkspaceConfig::hash` is content-deterministic (hash is over (path, content) pairs sorted by path). Pushed `files` will hash identically to FS for same content + same path set. Test #7 verifies. |
| Push received BEFORE `RepoScope::discover` runs (empty `scope.worktrees`). | Pushed-mode bypasses `scope.worktrees` — WT registration happens on first push. Serve loop's existing `scope.classified()` iteration still runs for FS-mode WTs; pushed WTs are additive to the `wt_hash` map. |
| Concurrent pushes for the same WT race. | The store is `Mutex<BTreeMap>` — strict serialization. Latest push wins. Per-WT serialization is the natural semantic (a push REPLACES the prior overlay). |
| push_tx queue depth — unbounded? | Use a bounded channel with `try_send` + drop-on-full. A wedged loop missing a push wakeup is recoverable (next push catches up, or activity tick + RoutedBatch from elsewhere). Telemetry span `overlay.push_ingest` would catch a queue-full pattern. |
| Pushed overlay store grows unbounded as more WTs push. | Idle-evict policy (Wave-2 / out-of-scope for 2b). For now, simple `BTreeMap` is fine; pushed-mode is opt-in per WT. |

---

## 9. Out-of-scope (per spec §6)

- TLS (HTTPS) on `POST /overlay` — `http://` only for v0.2.x. #14/post-v1.
- Coalesce window server-side (optional, deferred — client already coalesces).
- Replacing the FS path. Pushed-mode is ADDITIVE; FS path remains for shared-FS deployments.
- The 2c client (separately scoped, ~80 LOC follow-up).
- TLS root certs (collector is in-cluster HTTP; flip back to `tls-roots` only for SigNoz Cloud direct).
- Wave-2 metrics (`cargoless_overlay_push_total`, `cargoless_pushed_worktrees_gauge` — the regression-sentry counter pattern from #246).

---

## 10. Estimated effort

- 2b coding: ~150-200 LOC (Ctrl variant + ServeVerdictState extension + serve loop drain + SwitchOverlay arm + 7 tests).
- 2c coding: ~80-120 LOC.
- ci-gate: standard 7-phase via kubectl-exec. ~5-10 min wallclock.
- Layer-2: ~30 min team-lead source-verify (load-bearing: sole-attribution preserved + composing-equivalence test passes).
- Layer-3: ~30 min builder-infra §9a-trap criteria pre-loaded.

**Total budget: ~3-4 hours of focused work** (post-#246 ff, with the design already complete).

---

## 11. Open questions for team-lead at impl-start

1. **Worktree identifier on push** — the spec says `worktree: String`. Is this a PATH (matching the local FS path-key convention of WtId) or a canonical name (e.g. `feature-foo`)? The transport contract uses `String`; the serve loop needs to map this to a `WtId`. **Recommendation:** path-keyed for v0.2.x simplicity (matches FS-mode semantics); a canonical-name registry can be Wave-2.
2. **base_ref usage** — spec carries `base_ref: String` but doesn't say what the server does with it. **Recommendation:** STORE it (for diagnostics / a future "diff vs base_ref" feature) but DON'T act on it in 2b (the wire-shape change would be additive in a future increment if needed).
3. **Mode arbitration semantic** — when a WT receives its FIRST push, does it forever-after become pushed-mode? Or can it revert to FS-mode if no push arrives for some timeout? **Recommendation:** `take_overlay_for(wt)` is `pop`-semantic (consume + clear) — each push services exactly one SwitchOverlay; FS path resumes if no fresh push. Cleaner than a sticky-mode flag.

These are non-blocking; reasonable defaults can ship in the first 2b commit and be tuned via Layer-2 feedback.

---

**End of spike. Ready to hit the ground running on GO routing post-#246 ff.**
