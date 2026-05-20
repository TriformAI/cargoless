# Deploy-Milestone Pre-Stage Runbook (operator-facing)

**Audience:** the human operator authorising and configuring the
cargoless-serve deploy-milestone (Plane #235).
**Goal:** when #235 fires, the deploy is one command away — every
credential and configuration this runbook lists is in place, so
activation is a kubectl-apply, not a concept-to-commands translation.
**Authored:** 2026-05-20 against `origin/main = 929a5d3`. **Refreshed
post-#273 ROADMAP fold to `origin/main = 9807534`** (the SHA anchor
below now points at the post-#266/#268/#270/#273 state; no content
shift to source-anchored claims, all line refs verified byte-identical
because the intervening commits touched docs + scripts/ci-gate only).
**Source anchor for the manifest:** parked branch
`agent/builder-infra-serve-k8s` @ `7bd82a4cd757399381ad24b4854aaaf72d3271de`
(off `cc206da`). **The manifest needs a rebase** (`cc206da → 9807534`)
before integration — flagged in §6.

---

## 0. Status / honest scope (read this first)

This runbook documents pre-stage steps the operator can complete
**today** (against current `main` 9807534) so #235's activation is
mechanical. It does **NOT** mean the deploy is ready to fire:

1. **#226 manifest is parked, not on `main`.** See §6 — the
   manifest file lives on `agent/builder-infra-serve-k8s` @ 7bd82a4
   (off `cc206da`). A rebase + integration cycle is required before
   `kubectl apply` is even possible.
2. **The product-runtime image does not yet exist.** The manifest
   references `registry.triform.cloud/cargoless/cargoless-serve:0.2.0`
   — a placeholder. The release-pipeline image-bake step that
   produces it is itself a pending increment. The registry secrets
   in §1 are pre-stage for that bake.
3. **Wave-2 OTEL metrics are not on `main`.** The OTEL collector
   ConfigMap in §2 is valid TODAY for traces + logs (Wave-1 keystone
   spans emit per [D-INC2-OBSERVABILITY](../design/D-INC2-OBSERVABILITY.md)).
   The AC4 metric divergence alert is dormant until Wave-2 lands —
   see [AC4-DIVERGENCE-RUNBOOK](../observability/AC4-DIVERGENCE-RUNBOOK.md)
   §6.
4. **#225 Increment-0 wiring is on `main`** (verified: servedrv
   binds `HttpServer::bind` + `authorizer_for`, the bearer-gated
   read plane is live). The manifest is authored against this
   wiring; do not apply against pre-#225 main.

Each pre-stage step below is INDEPENDENT — the operator can complete
them in any order, and partial completion does not break the system
(no step changes anything currently-running on `main` or in-cluster).

---

## 1. Forgejo repo secrets — `REGISTRY_TRIFORM_USER` + `REGISTRY_TRIFORM_TOKEN`

### 1.1 What they are

Forgejo container-registry credentials used by the **release-pipeline
image-bake step** that produces
`registry.triform.cloud/cargoless/cargoless-serve:<version>`. The bake
step pushes the freshly-compiled image to that registry; the in-cluster
`registry-pull` `imagePullSecret` pulls it down. The pre-stage step
makes those credentials available to the workflow.

These are **Forgejo repo-level secrets** (workflow secrets), NOT
Kubernetes Secrets. They are exposed to GitHub-Actions-compatible
workflows running on the Forgejo runner, the same way
`FORGEJO_READONLY_TOKEN` (#100) was set.

### 1.2 Operator action — mint + set

**Step A: Mint a Forgejo packages access token.** Sign in to
`forgejo.triform.dev` as the operator account that will OWN the
`cargoless-serve` image:

1. Click avatar (top-right) → **Settings**.
2. **Applications** → **Access Tokens** → **Generate New Token**.
3. Name: `cargoless-serve-registry-push` (or similar — operator's
   choice; this name is the token's local identifier only).
4. **Scopes** — minimum needed:
   - `package: write` (push images to the registry)
   - `package: read` (sanity; optional but useful for `crane manifest`
     diagnostics)
5. **Generate Token** → COPY THE TOKEN IMMEDIATELY (Forgejo shows it
   once). This is the value of `REGISTRY_TRIFORM_TOKEN`.

**Step B: Set the repo secrets.** Still in `forgejo.triform.dev`:

1. Navigate to `triform/cargoless` → **Settings**.
2. **Actions** → **Secrets** (sidebar).
3. Add `REGISTRY_TRIFORM_USER` — set to the Forgejo username of the
   account that minted the token in Step A.
4. Add `REGISTRY_TRIFORM_TOKEN` — paste the token from Step A.
5. Save. (Forgejo's secrets are write-only via the UI; once saved you
   cannot read them back, which is the intended hygiene.)

### 1.3 Verify

The repo-secrets are write-only via the UI; verify via the API that
they exist (without reading their values):

```bash
# Authenticate as a user with admin access to triform/cargoless.
FORGEJO_TOKEN="$(printf 'protocol=https\nhost=forgejo.triform.dev\n\n' \
  | git credential fill 2>/dev/null \
  | sed -n 's/^password=//p')"

curl -s -H "Authorization: token $FORGEJO_TOKEN" \
  https://forgejo.triform.dev/api/v1/repos/triform/cargoless/actions/secrets \
  | jq '[.[].name] | sort'
```

Expected output includes both names:
```json
["FORGEJO_READONLY_TOKEN","REGISTRY_TRIFORM_TOKEN","REGISTRY_TRIFORM_USER"]
```
(Other existing secrets are fine; the two new ones MUST be present.)

### 1.4 Honest scope note

These secrets pre-stage the IMAGE BAKE. They do not by themselves
build or push anything — that's the release-pipeline workflow's job.
The workflow (a parked increment per #234) consumes these credentials.
Pre-staging the secrets unblocks that workflow's first fire.

---

## 2. SigNoz collector endpoint ConfigMap — `cargoless-otel-config`

### 2.1 What it is

A Kubernetes ConfigMap in the `cargoless-serve` namespace that
carries the OTEL exporter environment variables the cargoless-serve
container reads at startup. The deployment manifest (§6) will be
edited to `envFrom: configMapRef` this ConfigMap during the
integration rebase — pre-staging the ConfigMap means that integration
step is a one-line manifest tweak.

### 2.2 Env vars cargoless reads (anchored to `crates/cargoless-core/src/config.rs` @ 9807534, lines 622-639; byte-identical to 929a5d3 authoring base — verified via post-#266 backstop)

| Var | Required? | Default | What it does |
|---|---|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | **YES** (no endpoint ⇒ telemetry init is a no-op; see [D-INC2-OBSERVABILITY §1.3](../design/D-INC2-OBSERVABILITY.md)) | `None` | OTLP HTTP/protobuf collector URL. The single load-bearing predicate for `enabled()`. |
| `OTEL_EXPORTER_OTLP_HEADERS` | no (use only if collector needs auth) | `None` | Comma-separated `key=value` pairs per OTEL spec (e.g. `Authorization=Bearer xxx`). See §3. |
| `OTEL_SERVICE_NAME` | no | `"cargoless"` | `service.name` resource attr. |
| `OTEL_LOG_LEVEL` | no | `"warn"` | OTLP log filter level. |
| `OTEL_TRACES_SAMPLER_ARG` | no | `1.0` | Trace sampler ratio (AlwaysOn at 1.0 for v0/v0.1 low volume). |

### 2.3 Env vars the OTel SDK auto-reads (no cargoless code involvement)

The standard OTel SDK honors several env vars regardless of whether
cargoless's `TelemetryConfig` exposes them. The load-bearing one for
operator pre-stage:

| Var | What it does |
|---|---|
| `OTEL_RESOURCE_ATTRIBUTES` | Comma-separated `key=value` pairs MERGED into the resource the SDK constructs. cargoless's `telemetry.rs` explicitly sets `service.name` / `service.version` / `cargoless.build_id` — those take precedence; everything else from this env var is additive (deployment-environment, git-commit, instance id, etc.). |

### 2.4 ConfigMap shape

Apply this when ready:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: cargoless-otel-config
  namespace: cargoless-serve
  labels:
    app.kubernetes.io/name: cargoless-serve
    app.kubernetes.io/part-of: cargoless
data:
  # ── REQUIRED ─────────────────────────────────────────────────────
  # OTLP HTTP/protobuf collector endpoint. Replace with the operator's
  # in-cluster SigNoz collector URL. Common shapes:
  #   in-cluster SigNoz:  http://signoz-otel-collector.signoz.svc.cluster.local:4318
  #   SigNoz Cloud:       https://ingest.<region>.signoz.cloud:443
  OTEL_EXPORTER_OTLP_ENDPOINT: "http://signoz-otel-collector.signoz.svc.cluster.local:4318"

  # ── OPTIONAL (cargoless defaults are sensible) ───────────────────
  OTEL_SERVICE_NAME:           "cargoless"
  OTEL_LOG_LEVEL:              "warn"
  OTEL_TRACES_SAMPLER_ARG:     "1.0"

  # ── OPTIONAL (SDK-auto-read; deployment-tagging) ─────────────────
  # Comma-separated key=value. The SDK merges these into the
  # resource. cargoless's explicitly-set service.name/version/
  # build_id take precedence; the entries below are additive.
  OTEL_RESOURCE_ATTRIBUTES: >-
    deployment.environment=production,
    service.namespace=triform,
    service.instance.id=cargoless-serve-0
  # ── NOT recommended to set via ConfigMap ─────────────────────────
  # Do NOT set OTEL_EXPORTER_OTLP_HEADERS here if it carries a
  # secret value (e.g. Authorization=Bearer). Use the Secret in §3
  # instead, since ConfigMap contents are not treated as sensitive.
```

### 2.5 Operator action — create

```bash
kubectl create namespace cargoless-serve --dry-run=client -o yaml \
  | kubectl apply -f -

kubectl apply -f - <<'YAML'
# (paste the YAML from §2.4 here)
YAML
```

Or save the YAML to a file and apply:
```bash
kubectl apply -f cargoless-otel-config.yaml
```

### 2.6 Verify

```bash
kubectl -n cargoless-serve get configmap cargoless-otel-config \
  -o jsonpath='{.data.OTEL_EXPORTER_OTLP_ENDPOINT}{"\n"}'
```

Should print the endpoint URL. Confirm it is reachable from
the cluster (the deployment pod will be the actual sender; an
operator-side `curl` from a debug pod is a good smoke):

```bash
kubectl -n cargoless-serve run otel-smoke --rm -i --tty \
  --image=curlimages/curl --restart=Never \
  -- curl -sv "http://signoz-otel-collector.signoz.svc.cluster.local:4318/v1/traces"
```
(404 or 405 on `GET /v1/traces` is normal — the endpoint expects
`POST`; a connection failure is the signal that requires fixing.)

### 2.7 Notes on the ConfigMap vs the manifest

**The parked manifest does NOT currently consume this ConfigMap.**
The §6 rebase + integration step adds an `envFrom` block to the
`serve` container spec:

```yaml
spec:
  containers:
    - name: serve
      envFrom:
        - configMapRef:
            name: cargoless-otel-config
        - secretRef:                              # only if §3 is used
            name: cargoless-otel-headers
            optional: true
```

This is a small, surgical edit that belongs to the rebase step, not to
operator pre-stage. Pre-staging the ConfigMap means the manifest can
be applied without a separate ConfigMap-create step at integration
time.

---

## 3. Optional `cargoless-otel-headers` Secret (if collector requires Authorization)

### 3.1 When you need this

- Your collector accepts only authenticated traffic (e.g. SigNoz
  Cloud with an ingestion key, or an in-cluster collector behind an
  Authorization-gated proxy).
- You do NOT need this for an in-cluster SigNoz collector that
  trusts the namespace network policy as the auth gate.

### 3.2 Shape

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: cargoless-otel-headers
  namespace: cargoless-serve
  labels:
    app.kubernetes.io/name: cargoless-serve
type: Opaque
stringData:
  # The cargoless env var name (per crates/cargoless-core/src/config.rs:626).
  # Comma-separated key=value pairs per OTEL spec.
  OTEL_EXPORTER_OTLP_HEADERS: "signoz-access-token=REPLACE_ME_OUT_OF_BAND"
```

### 3.3 Operator action — create

```bash
# Generate the secret out-of-band (do NOT commit a real token):
SIGNOZ_TOKEN="<paste-real-token-from-signoz-portal>"
kubectl -n cargoless-serve create secret generic cargoless-otel-headers \
  --from-literal=OTEL_EXPORTER_OTLP_HEADERS="signoz-access-token=${SIGNOZ_TOKEN}" \
  --dry-run=client -o yaml \
  | kubectl apply -f -
```

### 3.4 Verify

```bash
kubectl -n cargoless-serve get secret cargoless-otel-headers \
  -o jsonpath='{.data.OTEL_EXPORTER_OTLP_HEADERS}' | base64 -d ; echo
```

Should print your headers string. (Do NOT log it; this is here as the
verification mechanism only.)

---

## 4. Verification — pre-stage end-to-end check

Once §1–§3 are complete, run this single sanity check:

```bash
echo "=== Forgejo repo secrets ===" && \
curl -s -H "Authorization: token $FORGEJO_TOKEN" \
  https://forgejo.triform.dev/api/v1/repos/triform/cargoless/actions/secrets \
  | jq -r '.[] | select(.name | startswith("REGISTRY_TRIFORM_")) | .name' \
&& \
echo && \
echo "=== Kubernetes namespace ===" && \
kubectl get ns cargoless-serve && \
echo && \
echo "=== ConfigMap ===" && \
kubectl -n cargoless-serve get cm cargoless-otel-config \
  -o jsonpath='{.data.OTEL_EXPORTER_OTLP_ENDPOINT}{"\n"}' && \
echo && \
echo "=== Optional headers Secret (only if used) ===" && \
kubectl -n cargoless-serve get secret cargoless-otel-headers 2>/dev/null \
  || echo "(not configured — only needed if collector requires auth)"
```

Expected output:

```
=== Forgejo repo secrets ===
REGISTRY_TRIFORM_TOKEN
REGISTRY_TRIFORM_USER

=== Kubernetes namespace ===
NAME              STATUS   AGE
cargoless-serve   Active   ...

=== ConfigMap ===
http://signoz-otel-collector.signoz.svc.cluster.local:4318

=== Optional headers Secret (only if used) ===
NAME                      TYPE     DATA   AGE
cargoless-otel-headers    Opaque   1      ...
  -- or --
(not configured — only needed if collector requires auth)
```

If any of the first three is missing, that step is incomplete — go
back to §1 / §2 as appropriate.

### What this check does NOT verify

- It does NOT confirm the registry credentials actually work for
  pushing — that requires the release-pipeline workflow to actually
  attempt a push. The first bake-then-deploy cycle is the empirical
  proof.
- It does NOT confirm the OTLP endpoint will accept traces — that
  requires a real `serve` process running. The smoke `curl` in §2.6
  is a connection check, not a payload check.
- It does NOT verify the bearer-auth `cargoless-serve-auth` Secret —
  that one is INSIDE the manifest and is operator-created at #235
  fire time, not at pre-stage (see §5).

---

## 5. When #235 fires — activation sequence

When the operator authorises #235 (deploy-milestone activation), the
sequence is:

1. **Release pipeline produces the image.** The image-bake workflow
   (parked increment #234) uses the `REGISTRY_TRIFORM_USER` /
   `REGISTRY_TRIFORM_TOKEN` from §1 to compile and push
   `registry.triform.cloud/cargoless/cargoless-serve:<version>`.
   Until this completes, the deployment cannot start (the
   `imagePullSecret` mechanism doesn't help if the image doesn't
   exist).
2. **Operator creates the bearer-auth Secret out of band.** This is
   MANDATORY by construction — `cargoless-core`'s
   `FleetConfig::security_check` fail-closes on a non-loopback bind
   with no token (the manifest's placeholder is intentionally
   whitespace-only so an unmodified apply fails-closed instead of
   coming up with a guessable credential):
   ```bash
   kubectl -n cargoless-serve create secret generic cargoless-serve-auth \
     --from-literal=token="$(openssl rand -hex 32)"
   ```
3. **Operator applies the (post-rebase) manifest:**
   ```bash
   kubectl apply -f deploy/cargoless-serve.k8s.yaml
   ```
   The manifest creates the namespace (idempotent — §2.5 already
   created it), the bearer-auth Secret placeholder (the actual value
   came from step 2), the state PVC, the Deployment, the Service,
   and the NetworkPolicy.
4. **Pod startup sequence:**
   - `initContainer:repo-clone` shallow-clones the cargoless repo
     into the `repo` emptyDir.
   - Main container `serve` starts; reads `CARGOLESS_AUTH_TOKEN`
     from the bearer-auth Secret + the OTEL env vars from the
     `cargoless-otel-config` ConfigMap (post-rebase manifest
     consumes it via `envFrom`).
   - cargoless discovers worktrees on `/repo`, classifies them,
     prints the §3.3 bring-up banner, then enters the serve loop.
   - First `cargo check` per cluster takes minutes (cold RA + cold
     workspace analyze); the AC#1 budget is generous (300s startup
     probe failureThreshold = 60 × 5s = 5min).
   - The `tcpSocket` readiness probe goes Ready once the listener
     is bound (`HttpServer::bind` succeeded post-config-resolve).
5. **Operator validates:**
   - Check pod status: `kubectl -n cargoless-serve get pods`.
     Expect `STATUS=Running, READY=1/1`.
   - Probe the listener from inside the cluster (a consumer-namespace
     pod with the `cargoless.triform/serve-consumer=true` namespace
     label):
     ```bash
     curl -sv -H "Authorization: Bearer $CARGOLESS_AUTH_TOKEN" \
       http://cargoless-serve.cargoless-serve.svc.cluster.local:8787/worktrees
     ```
     Expect a JSON array of worktree summaries.
   - Check SigNoz dashboard
     ([`docs/observability/cargoless-dashboard.json`](../observability/cargoless-dashboard.json))
     — the Wave-1-LIVE panels (panel 60 "Wave-1 keystone span emission
     counts" via SigNoz trace search) should start populating within
     ~1 minute of the first verdict.

### What can go wrong (the honest list)

- **Image pull fails** → registry credentials missing or wrong.
  Re-check §1; confirm the release pipeline actually pushed the
  image at the expected tag.
- **Pod CrashLoop, logs show "refusing to start: bad bind …"** →
  bearer-auth Secret missing or empty / whitespace-only. Step-2
  Secret was the unmodified manifest placeholder. Create the real
  Secret out-of-band (step 2 above).
- **Pod Running but readiness probe never passes** → check whether
  the binary in the image actually has the `--features integration`
  daemon wired (the placeholder image needs the right build flags;
  see manifest header).
- **Pod Ready but SigNoz dashboard empty** → ConfigMap endpoint
  unreachable from the pod. Re-run the §2.6 smoke from a pod in the
  `cargoless-serve` namespace.
- **Cold-RA stall on first start** → expected; gradle-style cold
  build takes minutes. Watch the verdict stream via
  `kubectl -n cargoless-serve logs deploy/cargoless-serve -f`; the
  first GREEN may be 1-5min after Ready.

---

## 6. Cross-references + the rebase gap

### 6.1 Parked manifest

- File: `deploy/cargoless-serve.k8s.yaml`
- Branch: `agent/builder-infra-serve-k8s` @ `7bd82a4`
- Branch base: `cc206da` (the v0.2.0 SHA)
- **Rebase gap:** `cc206da..9807534` includes (complete enumeration
  verified via `git log --oneline cc206da..9807534`):
  - Stage-1 acceptance suite (#228 — `test(stage1): Stage-1 acceptance suite`)
  - Increment-0 /healthz route (#236 — `feat(#225): 0d`)
  - Increment-2/2a PushOverlay transport contract delta (#240/2a)
  - #247 STOP-class structural fix (`fix(#247): ClusterDriver::reset_after_respawn + Ctrl::Spawned wire fix`, 6290333)
  - Stage-1 harness rework + bug fixes (#239 — `test(stage1):` multi-commit)
  - Increment-4 de-WASM-gate source change (#241, 4d56021 — `feat(#241): de-WASM-gate`)
  - Wave-1 OTEL telemetry foundation + 5b TelemetryConfig + 5c keystone spans + Layer-3 CATCH fixes (#246, multiple commits)
  - Increment-2/2b servedrv-consume (#240/2b)
  - Increment-2/2c thin push-client (#240/2c)
  - de-WASM-gate docs sweep (#255)
  - brand-coherence sweep (#258)
  - 2b servedrv-consume design spike → D-INC2-2B.md (#254)
  - ci-gate housekeeping (#263) + ci-gate --update-lock opt-in mode (#266)
  - Wave-2 OTEL docs (D-INC2-OBSERVABILITY + AC4-DIVERGENCE-RUNBOOK + cargoless-dashboard.json, #268)
  - Operator pre-stage runbook (this doc, #270)
  - ROADMAP refresh (#273)
- The rebase is the responsibility of **builder-infra** at integration
  time; it's mechanical (pure cherry-pick onto current main).
  **Verified**: 0 commits in `cc206da..9807534` touched `deploy/`
  (per `git log --oneline cc206da..9807534 -- deploy/` returning empty);
  all the integrating work was orthogonal.

### 6.2 Manifest edits needed during integration

In addition to the rebase, the manifest needs **one** envFrom edit
to consume the §2 ConfigMap and (if used) the §3 Secret. The edit:

```yaml
spec:
  containers:
    - name: serve
      # add this block:
      envFrom:
        - configMapRef:
            name: cargoless-otel-config
        - secretRef:
            name: cargoless-otel-headers
            optional: true
```

This is small enough to land in the same rebase commit.

### 6.3 Related docs

- **Plan / architecture:**
  [`docs/design/D-FLEET-SHARED-DAEMON.md`](../design/D-FLEET-SHARED-DAEMON.md)
  — the central-service architecture this deploy realises.
- **Increment-2 design:**
  [`docs/design/D-PUSHOVERLAY.md`](../design/D-PUSHOVERLAY.md) +
  [`docs/design/D-INC2-2B.md`](../design/D-INC2-2B.md) — the
  overlay-push contract the deployed service exposes.
- **OTEL Wave-1 + Wave-2:**
  [`docs/design/D-INC2-OBSERVABILITY.md`](../design/D-INC2-OBSERVABILITY.md)
  — what spans/metrics emit (Wave-1 landed vs Wave-2 in-flight).
- **AC4 alert runbook:**
  [`docs/observability/AC4-DIVERGENCE-RUNBOOK.md`](../observability/AC4-DIVERGENCE-RUNBOOK.md)
  — the operator-facing runbook for the AC4 metric divergence
  alert (dormant until Wave-2 metrics land).
- **Dashboard:**
  [`docs/observability/cargoless-dashboard.json`](../observability/cargoless-dashboard.json)
  — the SigNoz dashboard JSON to import once the deployment is
  emitting (Wave-1 trace panels render today; Wave-2 metric panels
  render as Wave-2 lands).

---

## 7. Operator quick-reference card

```
┌──────────────────────────────────────────────────────────────────┐
│ DEPLOY-MILESTONE PRE-STAGE — pre-#235 checklist                 │
├──────────────────────────────────────────────────────────────────┤
│ ☐ Forgejo repo secrets set:                                      │
│   - REGISTRY_TRIFORM_USER                                        │
│   - REGISTRY_TRIFORM_TOKEN                                       │
│   verify: API call in §1.3 returns both names                    │
│                                                                  │
│ ☐ Namespace cargoless-serve exists                               │
│   verify: kubectl get ns cargoless-serve                         │
│                                                                  │
│ ☐ ConfigMap cargoless-otel-config exists in that namespace       │
│   verify: kubectl get cm cargoless-otel-config (§2.6)            │
│   key OTEL_EXPORTER_OTLP_ENDPOINT points at your SigNoz endpoint │
│                                                                  │
│ ☐ (OPTIONAL) Secret cargoless-otel-headers if collector needs    │
│   Authorization headers                                          │
│                                                                  │
│ NOT pre-stage (operator does at #235 fire time):                 │
│   - cargoless-serve-auth bearer-token Secret (random hex 32)     │
│   - kubectl apply -f deploy/cargoless-serve.k8s.yaml             │
│                                                                  │
│ NOT pre-stage (builder-infra does at integration):               │
│   - Rebase agent/builder-infra-serve-k8s onto current main       │
│   - Add envFrom block to serve container (§6.2)                  │
│   - First image bake via release pipeline (#234)                 │
└──────────────────────────────────────────────────────────────────┘
```

---

**End of pre-stage runbook. Authored against
`origin/main = 929a5d3`; freshness-refreshed to `9807534` (post-#266
ci-gate, post-#268+#270 Wave-2 docs, post-#273 ROADMAP refresh). Update
this doc if any of the OTEL env vars in §2.2/§2.3 change, if the
registry-secret names in §1 change, or if the parked manifest in §6
lands on `main`.**
