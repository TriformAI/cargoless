# D-SELF-SERVE-PREVIEWS — any agent previews any branch, on demand

**Status:** Design + increment 1 (this branch). Greenlit by the operator after the
static 2-instance preview went live in `triform-staging` (CWDL-87): the
hand-curated instances file becomes a **dynamic, on-demand** model — an agent
(or person) asks the daemon to preview a branch, and `<branch>.preview.triform.dev`
comes up with no per-branch infra.

**Supersedes** the "per-push/per-branch previews" parking-lot item and the
`feature-x` placeholder instance (which pointed at a non-existent `feature/x`).

---

## 1. The shift

Today (`D-APP-SERVE`): the daemon reads a fixed instances file once at startup;
each instance has a unique `app_bind` SocketAddr that a dedicated k8s Service
targets. N instances ⇒ N Services ⇒ N hand-written config blocks. That does not
scale to "anyone with a branch".

Target: the instance set is **mutable at runtime**, driven by a control-plane
API. `POST /instances {name, ref, env?, own_db?}` adds a preview; `DELETE
/instances/<name>` tears it down and reclaims its resources. One wildcard host
`*.preview.triform.dev` + one k8s Service front every preview.

### Operator decisions (settled)

- **Trigger:** on-demand control-plane API, **available to agents** (bearer-authed,
  the same token gate the read plane already uses). Not auto-discovery, not a
  marker file — explicit request. The instance set is dynamic but human/agent-driven.
- **Per-branch DB:** **opt-in**. Default = share the staging `dev` database
  (read-mostly UI previews). A request may set `own_db: true` to get an
  auto-provisioned `preview_<name>` database (CREATE on add, DROP on remove).
- **Routing:** a **host-routing front inside cargoless** (§3) — not per-instance
  ports + external ingress. One listener reads the `Host:` header and dispatches
  to the named instance's upstream slot.
- **TLS:** a **wildcard `*.preview.triform.dev` cert** via cert-manager DNS-01
  (§5) — per-host HTTP-01 can't issue a wildcard and re-issues per branch.

---

## 2. Why the daemon already mostly fits

The seam map (recon, `agent/app-serve` @ c1f5172) found the core is dynamic-friendly:

- `AppState::step` **no-ops unknown instances** (appstate.rs) — an add can race a
  build with no corruption; a remove mid-flight makes in-flight actions safe no-ops.
- `PortAllocator` is already a shared alloc/release pool (appdrv.rs).
- Every `Driver` runtime accessor handles a missing instance (`if let Some(rt) …
  else return`).
- `L4Proxy` / `HttpServer` have `Drop`-based teardown.
- The auth gate already covers any new authenticated route (transport/http.rs:480).
- A single mpsc `(instance, Event)` channel feeds the **single-mutator** control
  loop — the natural conduit to inject "add/remove" requests.

What is hard-coded at startup and must change (the increment work):

1. `Driver` / `AppState` need `add_instance` / `remove_instance`.
2. `serve_loop`'s per-spec setup (proxy, config, run-plan, poller) must be
   factored into a `setup_instance(spec)` callable at runtime.
3. `ProcLauncher.plans` must gain interior mutability (`Mutex<BTreeMap<…>>`).
4. `spawn_ref_poller` needs a per-instance stop flag (today only the global
   `SHUTDOWN`) so DELETE can stop one poller.
5. A new `ControlMsg { Add(InstanceSpec, AddOpts), Remove(String) }` widening of
   the loop's channel + a `Sender<ControlMsg>` threaded into the HTTP server.
6. New routes `POST /instances` / `DELETE /instances/<name>` (mirror
   `/admin/quiesce` for the shape, `/overlay` for body parsing, the
   `strip_prefix` idiom for the path segment).
7. The **host-routing front** (§3) — the one genuinely new component.

`git worktree add` (the per-instance worktree) already landed (PR #46,
`ensure_instance_worktree`), so dynamic adds reuse it.

---

## 3. Host-routing front (the one new component)

Today `l4proxy::L4Proxy` binds one TCP listener per instance and byte-splices to
a pinned upstream — zero host awareness (it's deliberately L4). For a wildcard
host we add a front that is host-aware but reuses the existing per-upstream
machinery (`UpstreamSlot`, `ConnGauge`, the half-close splice).

```
                       *.preview.triform.dev:443 (one k8s Service)
                                   │
                    ┌──────────────▼───────────────┐
                    │  HostRouter (new)             │
                    │  peek Host: header (HTTP/1.1) │   BTreeMap<host, Arc<UpstreamSlot>>
                    │  → instance's UpstreamSlot    │   "dev.preview…"  → slot(dev)
                    └──────────────┬───────────────┘   "feat-x.preview…"→ slot(feat-x)
                       splice (reuse l4proxy splice) │
                                   ▼
                         127.0.0.1:<instance app port>
```

- **L7-lite, not a full proxy:** read only the request line + headers of the
  FIRST request to extract `Host:`, then hand the (already-buffered) bytes +
  the live socket to the existing splice. WebSocket/SSE upgrades still pass
  through untouched after the initial header peek (the pin is per-connection,
  same as today).
- **The map is the dynamic instance set:** add inserts `host → slot`, remove
  deletes it. An unknown host gets the holding page (or 404). `Arc<UpstreamSlot>`
  is the same type the driver flips on promote — host-routing front and driver
  share it, so a promote is visible to new connections immediately, exactly as
  today.
- **TLS:** terminated at the cluster ingress (nginx) with the wildcard cert; the
  front sees plaintext HTTP and reads `Host:`. (If cargoless ever terminates TLS
  itself, peek SNI instead — same map, keyed by SNI host. Out of scope for inc 1.)
- The legacy fixed-`app_bind` per-instance `L4Proxy` stays supported for the
  static/zero-config path; the host-router is additive.

---

## 4. Per-branch database (opt-in)

- Request without `own_db` ⇒ the instance inherits the shared staging `dev`
  `DATABASE_URL` (the canary data plane). Cheapest; fine for read/UI previews.
- Request with `own_db: true` ⇒ the daemon ensures `preview_<name>` exists
  (CREATE DATABASE on the direct primary — NOT pgbouncer, which has a static db
  list; see the deploy-state memory), injects `DATABASE_URL` /
  `MIGRATION_DATABASE_URL` pointing at it, and physics boot-migrates it. On
  DELETE, DROP the database (gated: only databases the daemon created, named
  `preview_<name>`, never the shared one).
- DB admin creds: the daemon needs a privileged URL to CREATE/DROP. Provided via
  a secret (e.g. `cargoless-preview-dbadmin`), separate from the per-instance
  app creds. The app URL is derived by swapping the db name (proven in the live
  bring-up).

---

## 5. TLS + DNS (infra dependency)

- DNS: `*.preview.triform.dev` is **already a live wildcard** → cluster LB
  (verified: arbitrary `<x>.preview.triform.dev` resolves). Zero per-branch DNS.
- TLS: a wildcard `*.preview.triform.dev` cert. **Blocker:** the cluster's
  ClusterIssuers (`letsencrypt-prod` etc.) are HTTP-01 only — a wildcard needs
  **DNS-01**, which needs the DNS provider's API credentials configured in a new
  ClusterIssuer. That is an operator/infra step (cred material I can't fabricate).
  Until it exists, previews work over the cluster-internal HTTP path or fall back
  to the current per-host HTTP-01 issuance for named instances.

---

## 6. Increments

1. **(this branch) Dynamic instance add/remove, in-process.** `AppState` /
   `Driver` `add_instance`/`remove_instance`; `serve_loop` `setup_instance`
   refactor; `ProcLauncher.plans` → `Mutex`; per-instance poller stop flag;
   `ControlMsg` channel; `POST /instances` + `DELETE /instances/<name>` routes
   (bearer-authed). Exhaustively unit-tested (add then build, remove drains +
   frees port, add/remove of unknown is safe, duplicate add rejected). The
   instance set becomes mutable with NO restart. Routing still per-`app_bind`
   for this increment (host-router is inc 2) — so inc 1 is testable purely in
   the state/driver layer.
2. **Host-routing front** (§3) + the wildcard k8s Service/Ingress. One listener,
   `Host:`→slot map, reuse splice. Replaces N Services with 1.
3. **Per-branch DB provisioning** (§4) — opt-in CREATE/DROP + the dbadmin secret.
4. **Operator surface**: a `cargoless preview <branch>` CLI / the agent-facing
   API docs; GC for stale previews (TTL or branch-deleted sweep); the DNS-01
   ClusterIssuer for wildcard TLS.

---

## 7. Non-goals / parked

Auto-discovering ALL branches (explicit request only); previewing arbitrary
forks/remotes (same repo only); seeding a preview DB from a snapshot; per-preview
NATS/Iggy isolation (DB-only, like today); symbol-level anything. GC policy
(TTL vs. branch-gone) is inc 4, not inc 1.
