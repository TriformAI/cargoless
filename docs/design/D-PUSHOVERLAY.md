# D-PUSHOVERLAY — Increment-2 design-ahead spec (overlay ingest + thin push-client)

**Status:** design-ahead spec (SPEC ONLY — no code in this increment's lane).
**Source anchor:** `v0.2.0` tag = `cc206dafd2a811f8ae82004ca72fd2905f46cc11`
(== `origin/main` at authoring; every symbol/line below is `git show v0.2.0:<path>`,
never the stale primary checkout). Line numbers are v0.2.0-tag-relative.
**Extends:** `docs/design/D-FLEET-SHARED-DAEMON.md` §10 (the transport seam).
**Gating:** hard-gated on **Increment 0 (#225)** landing the read-plane wiring
(`servedrv` constructing a `VerdictService` over its per-WT state + spinning
`HttpServer::bind` / `authorizer_for`, passing the standard 7-check ci-gate).
This spec is design-ahead so dev-fixer can move fast the moment #225 unblocks.
**Discipline:** purely **additive** — no reshape of any frozen variant; serde-free
`serde_json::Value` codec; std-only; best-effort, never-panic — the exact
discipline used to add `Diagnostic` to the contract (an adjacent additive
surface, existing types byte-unaffected).

---

## 0. Precondition (the first boundary item — repeated here so the spec stands alone)

There is **no network endpoint of any kind at v0.2.0**. `crates/cargoless/src/servedrv.rs`
publishes verdicts only to per-worktree statusfiles; the transport library
(`crates/cargoless-core/src/transport/{mod,http,unix,inproc,discovery}.rs`) is
complete and exhaustively unit-tested but has **zero non-test callers**. The
central service therefore comes up in two increments:

* **Increment 0 (#225)** — wire the *read* plane: `servedrv` builds a
  `VerdictService` over its per-WT state, `HttpServer::bind(addr, svc, authorizer_for(&cfg))`,
  pass 7-check ci-gate. (Not this spec.)
* **Increment 2 (this spec)** — add the *write* plane: an additive `PushOverlay`
  ingest verb + the thin push-client + redirecting `servedrv`'s overlay source
  from disk to the pushed payload. Every capability here is downstream of
  Increment 0.

---

## 1. The four filesystem taps Increment 2 must decouple (v0.2.0 anchors)

`servedrv.rs` (the live `serve --repo` driver, `servedrv::run` L186) acquires
worktree content from a **shared filesystem** at four sites:

| # | Tap | v0.2.0 site | What it does |
|---|---|---|---|
| 1 | Worktree set | `cargoless_core::repo::topology::list_worktrees` → `scope.worktrees`, consumed in the cluster-assignment loop at `servedrv.rs:196` | `git -C <repo> worktree list --porcelain` |
| 2 | Workspace config / cluster id | `clustermgr::read_workspace_config(&wt.path)` at `servedrv.rs:196` | `std::fs::read_to_string` of the 4 workspace-defining files |
| 3 | File-watch | `cargoless_core::repo::watch::raw_repo_watch(&repo_root)` at `servedrv.rs:220` | local `notify` OS watcher |
| 4 | Overlay content | `std::fs::read_to_string(f)` at `servedrv.rs:472`, fed into `OverlaySet::from_pairs` at `servedrv.rs:477`, inside the `ClusterAction::SwitchOverlay` arm (`servedrv.rs:463`) | reads each changed file's bytes off disk |

Verdict egress is `publish_verdict` (`servedrv.rs:513`) → `statusfile::write(wt, &st)`
(`servedrv.rs:529`) — **unchanged by Increment 2** (see §4).

The **composing pivot:** `cargoless_core::overlay::diff(prev, next) -> Vec<OverlayOp>`
(`overlay.rs`) is already **content-based, not path-based** — it takes
`OverlaySet` (`BTreeMap<PathBuf,String>`) and never touches disk. Increment 2
changes *where the `OverlaySet` content comes from*; `diff()` and the entire
proven isolation core (`multiplex` / `clusterdrv` / `barrier`) stay
**byte-untouched**.

---

## 2. (a) The `PushOverlay` proto verb — additive to the transport `Request` enum

### 2.1 Wire type (additive variant — no reshape)

`crates/cargoless-core/src/transport/mod.rs`, append **after** `Request::Subscribe`
(currently `transport/mod.rs:331`); the five existing variants stay byte-frozen:

```text
pub enum Request {
    GetStatus(String),
    GetVerdict(String),
    GetDiagnostics(String),
    ListWorktrees,
    Subscribe,
    // ── Increment 2, additive ──
    PushOverlay { worktree: String, base_ref: String, files: Vec<(String, String)> },
}
```

`files` is `(path, full-content)` pairs — whole-file content, never a keystroke
diff (the client owns the overlay-set; see §3).

### 2.2 Codec extension (`from_json` `transport/mod.rs:338` / `to_json` :357)

Op tag `"push_overlay"`. Wire JSON:

```json
{"op":"push_overlay","worktree":"W","base_ref":"origin/main",
 "files":[{"path":"src/lib.rs","content":"<bytes>"}, ...]}
```

`from_json` best-effort (mirror the existing rules exactly): unknown op ⇒ `None`
(unchanged); `op=="push_overlay"` with a missing/`!array` `files` ⇒ empty `files`
vec, never a panic; a malformed element (no `path`) is skipped, not fatal —
same posture as `crate_verdicts_from_json` / `summaries_from_json`. Stay on
`serde_json::Value` + `json!`, **no derive**, no new dep.

### 2.3 Response DTO (cheap ack; verdict via the existing read plane)

`PushOverlay` is **write-only ingest**. It does **not** block on the verdict.
Define a small additive DTO + codec helpers (same hand-rolled style as
`status_to_json`):

```text
pub struct PushOverlayAck {
    pub worktree: String,
    pub accepted: bool,
    pub applied_files: u32,   // count the server stored
}
```

Wire: `{"worktree":"W","accepted":true,"applied_files":3}`. The client then uses
the **already-shipped** `subscribe` (SSE) or `get_status` to obtain the verdict —
**no new verdict-egress surface, no long-poll semantics** (keeps the push cheap
and the verdict path the one that already exists and is tested).

### 2.4 `VerdictService` / `TransportClient` trait extension

Add **one** method, with a **default body** so the trait extension is additive
to callers (no existing impl is forced to change; the v0.2.0 test
`MockService` keeps compiling):

```text
// transport/mod.rs:111  trait VerdictService
fn push_overlay(&self, _worktree: &str, _base_ref: &str,
                 _files: &[(String, String)]) -> PushOverlayAck {
    PushOverlayAck { worktree: _worktree.into(), accepted: false, applied_files: 0 }
}
```

Mirror on `TransportClient` (`transport/mod.rs:141`) returning
`Result<PushOverlayAck, TransportError>` (default `Err(TransportError::Protocol("push_overlay unsupported"))`).
**Flag (contained):** a defaulted trait method is additive at the trait
boundary; the *real* implementor is Increment 0's `VerdictService` (none exists
at v0.2.0) which overrides it for real. The default keeps every other adapter
(`inproc`, `unix`, `http` read paths) and the test mock untouched — this is the
single "more than a new enum variant" point, and the default body contains it.
`TransportError` (`transport/mod.rs:288`) is unchanged (reuse `Io`/`Protocol`).

### 2.5 HTTP route — the server's FIRST body-reading route

Today `parse_request` (`transport/http.rs:55`) reads only the request line +
headers; its doc states "Body is never read — the API is all GET"
(`transport/http.rs:53`). Increment 2 adds, **bounded by construction**:

* **Route:** `POST /overlay` (one new method+path; every existing GET route
  unchanged and still body-less).
* **Bearer:** unchanged. The existing gate in `handle` (`transport/http.rs:160`)
  — `if !auth.authorize(req.bearer.as_deref()) { 401 }` — runs **before**
  dispatch, so `POST /overlay` inherits the same `Authorizer` /
  `BearerToken` / `authorizer_for` (`transport/mod.rs:157` / `199` / `267`)
  for free. The fail-closed non-loopback-no-token refusal already enforced at
  `serve.rs` `security_check` + `authorizer_for` covers it. **No new auth
  surface.**
* **Bounded body read:** extend `parse_request` to capture `Content-Length`.
  For `POST /overlay` only: after the header blank line, `read_exact`
  `Content-Length` bytes from the `BufReader` (std-only). Reject:
  * absent / non-numeric `Content-Length` on a POST → `400`;
  * `Content-Length` > a hard cap `MAX_OVERLAY_BYTES` (config-driven, fail-closed
    default e.g. 32 MiB) → `413`;
  GET paths never read a body (the bounded-by-construction property of the
  existing surface is preserved — only the new POST path reads, and only an
  exact, capped length).
* **Dispatch (`handle`, `transport/http.rs:160`):** after the auth gate, before
  the `/events` and one-shot GET branches, add `if method == "POST" && path ==
  "/overlay"` → decode body via `Request::from_json` → on
  `Request::PushOverlay{..}` call `svc.push_overlay(..)` →
  `write_response(200, "OK", "application/json", ack_json)` | `400` bad body |
  `413` too-large | `401` (existing path). `parse_request` must now also return
  the method (today `_method` is discarded at `transport/http.rs:59`).
* **`HttpClient`:** add a `push_overlay` that does `POST /overlay` with a
  `Content-Length` body + `Authorization: Bearer` (the read client is GET-only
  today, `transport/http.rs:320/368`); parse the ack JSON. Mirror on
  `UnixClient` (new NDJSON op) and `inproc` (direct call).

---

## 3. (b) The thin push-client protocol — dev box AND pipeline stage

**Common invariant:** the client owns its overlay-set. Per the standing fact
that cargoless's primary consumer is an **agent writing whole files
atomically**, the client never diffs keystrokes — it enumerates the files it
changed relative to `base_ref` and sends `(path, full-content)`.

* **(i) Local dev box, on local-edit** — a thin client (e.g.
  `cargoless push --remote <url> --repo <path> [--worktree W] [--base <ref>]`,
  or a watch-mode that pushes once per settled save-batch). It computes the
  overlay-set **on its own local FS** (the daemon has none):
  `git -C <repo> diff --name-only <base_ref>` → read each listed file's current
  bytes → `PushOverlay`. Then `subscribe` (SSE) or `get_status` for the verdict.
* **(ii) CI / pipeline stage at job-start** — identical payload, computed once
  at stage start from the checked-out workspace vs the pipeline base ref
  (e.g. `origin/main`): push → block on `get_verdict`/`subscribe` until terminal
  → map green/red to the stage exit code. **This is the `scripts/check-remote`
  replacement shape** (one stage, one push, one verdict).

**Discovery / fallback (reuse shipped, unchanged):** the client uses
`transport::discovery::resolve` precedence as-shipped — `--remote <url>` →
live Unix socket → `cli-status` FileRead → SpawnLocal. For the no-shared-FS
central topology the client passes `--remote https://<svc>`, so `resolve()`
short-circuits to `Resolution::Remote` (HTTP) and the socket / FileRead /
SpawnLocal tiers are correctly inapplicable (they assume a shared FS). The
#185 stale-socket connect-liveness still protects the local tiers if ever
used. **No discovery change** — the spec only states the client wires
`--remote`.

**Exact sequence:** client → `POST /overlay` (bearer) → `200` ack → client
`GET /events` (SSE, bearer) **or** poll `GET /status?worktree=W` until the
verdict settles (`published_at` / `heartbeat_age_secs` advance) → map verdict
→ exit. **Idempotent:** a re-push of an identical overlay-set yields no
`overlay::diff` ops and a stable verdict — no special client retry/dedupe
logic needed.

---

## 4. (c) `servedrv` consumption — reuse `overlay::diff()` UNCHANGED + decouple the 4 taps

### 4.1 The one-line source swap (the pivot)

`servedrv.rs:463-477` (the `ClusterAction::SwitchOverlay` arm) currently builds
`next` via `std::fs::read_to_string(f)` (`:472`) → `OverlaySet::from_pairs`
(`:477`). Increment 2 swaps the **source** of that `OverlaySet` from disk-read
to a per-worktree pushed-overlay store. `overlay::diff(prev, next)`, `mux.switch_to`,
the `did_open`/`did_change`/`did_close` verbs, the `did_save` flycheck trigger,
`EmitVerdict`, and `publish_verdict`→`statusfile::write` (`:529`) are **all
byte-unchanged** — the proven isolation core's precondition is *restored at the
new ingest seam by sourcing content there*, never by weakening the core.

### 4.2 Server-side overlay store + change signal

Add a `BTreeMap<WtId, (String /*base_ref*/, OverlaySet)>` owned by the serve
loop, populated by `push_overlay`. On a `PushOverlay`:

1. register/refresh the WtId (the push *is* the worktree-existence signal — tap 1);
2. record the `OverlaySet` from the pushed `files`;
3. synthesize the **same** `DriverEvent::RoutedBatch { wt }` the watcher path
   feeds, so `clusterdrv` / `multiplex` see an identical event shape;
4. in the `SwitchOverlay` arm, read `next` from the store (replacing tap 4);
   `pending_batch`'s file-list role is served by the pushed file set.

A small server-side coalesce window is OPTIONAL (only if a client streams
rapid pushes); the client already coalesces to one whole-file-set push, so the
keystroke-storm debounce that `repo::watch` provides is unnecessary for pushed
WTs.

### 4.3 The four taps, decoupled for pushed worktrees

| Tap | v0.2.0 site | Pushed-mode replacement |
|---|---|---|
| 1 Worktree set | `repo::topology::list_worktrees` → `scope.worktrees` @ `servedrv.rs:196` | a worktree becomes "known" on its first `PushOverlay` (registry keyed by the push's `worktree`) — no `git worktree list` |
| 2 Workspace config / cluster id | `clustermgr::read_workspace_config(&wt.path)` @ `servedrv.rs:196` (`fs::read_to_string`) | the 4 workspace-defining files (`Cargo.toml`, `Cargo.lock`, `rust-toolchain[.toml]`, `.cargo/config[.toml]`) are just files — carried IN the overlay-set; cluster hash via the **existing** pure `cluster::WorkspaceConfig::hash` over the pushed content (already content-shaped) — no disk read |
| 3 File-watch | `repo::watch::raw_repo_watch(&repo_root)` @ `servedrv.rs:220` (`notify`) | no FS to watch; the `PushOverlay` receipt **is** the change signal — an mpsc fed by `push_overlay` replaces the `raw_rx` path for pushed WTs |
| 4 Overlay content | `std::fs::read_to_string(f)` @ `servedrv.rs:472` | read from the per-WT overlay store (§4.1 — the single line whose source changes) |

### 4.4 Verdict egress — unchanged

`publish_verdict` still writes the per-WT statusfile (`servedrv.rs:513-529`);
Increment 0's `VerdictService` reads that per-WT state for
`get_status`/`subscribe`. The push path's verdict reaches the client via the
**same** read plane — clean write-ingest / read-egress separation, no new
egress surface.

### 4.5 Pushed-vs-FS is a MODE, not a replacement

Shared-FS deployments are unaffected: the FS taps remain the default; pushed
mode is selected per-worktree by the arrival of `PushOverlay` (or a serve flag,
dev-fixer's call at impl). Additive end-to-end.

---

## 5. Test / 7-check-readiness plan (so the spec is implementable as-written)

Pure, std-only, `serde_json::Value`, no new deps ⇒ passes build/test/fmt/clippy;
the integration arm runs under `--features integration` as today.

1. **Codec roundtrip + best-effort** — `Request::PushOverlay` `to_json`→`from_json`
   identity incl. empty/multi `files`; unknown-op still `None`; missing `files`
   ⇒ empty, no panic (extend the existing `request_roundtrips_and_rejects_unknown_op`,
   `transport/mod.rs:669`).
2. **Bounded body read** — absent/zero/non-numeric/oversize `Content-Length`
   → `400`/`413`; exact-length happy path; GET still reads no body.
3. **Composing-equivalence (load-bearing)** — for the same content, the
   `Vec<OverlayOp>` produced via the pushed store is **byte-identical** to the
   FS path's output: prove `overlay::diff` is source-agnostic (this is the
   "core untouched" guarantee, tested).
4. **Bearer gate on `POST /overlay`** — `DenyAll` ⇒ `401`, no panic (mirror
   `denying_authorizer_yields_401_not_a_panic` in `transport/http.rs` tests).
5. **Ack shape** — `PushOverlayAck` JSON roundtrip + `applied_files` count.

---

## 6. Non-goals / honest boundary (carry the register)

Increment 2 does **NOT**: add TLS (`http://` only — a #14/post-v1 future);
change verdict egress (statusfile, unchanged); touch the proven isolation
cores (`overlay`/`multiplex`/`clusterdrv`/`barrier` byte-untouched — precondition
restored at the seam, not by core weakening); remove the FS path (pushed-vs-FS
is a mode; shared-FS unaffected). It is wholly gated behind **Increment 0
(#225)**. The trait-method-with-default (§2.4) is the single non-trivial
seam-touch and is contained by the default body.
