# Preview status surface — `cargoless.preview.triform.dev`

The cargoless app-serve daemon publishes a **public, no-bearer** read
surface so agents and operators can observe a rolling preview without
`kubectl port-forward` or holding the control-plane bearer.

## Hosts

| Host | Backend | What it serves |
|---|---|---|
| `https://preview.triform.dev` | `cargoless-preview-app:dev` | the `dev` canary — full staging data |
| `https://feature-x.preview.triform.dev` | `cargoless-preview-app:feature-x` | the `feature-x` instance, own DB |
| `https://merge.preview.triform.dev` | `cargoless-preview-app:merge` | the `merge` lane (free push, shares staging data) |
| `https://cargoless.preview.triform.dev` | `cargoless-preview-ctl:8787` | **status surface** — see below |

The `dev` canary lives at the bare `preview.triform.dev`; every other
instance is `<name>.preview.triform.dev` by convention.

## Public routes on `cargoless.preview.triform.dev`

Three GET routes are STRUCTURALLY auth-exempt — answered BEFORE the
bearer gate in `cargoless-core::transport::http`:

| Route | Meaning |
|---|---|
| `GET /healthz` | serve loop is up (`200` ready, `503` starting) |
| `GET /readyz` | RA warm + at least one ever-green instance serves (`200`/`503`) |
| `GET /app` | full per-instance JSON snapshot |

Every other control-plane route (`/admin/*`, `/status`, `/verdict`,
`/worktrees*`, `/diagnostics`, `/events`) still requires the bearer
token from `cargoless-serve-auth` — an unauthenticated caller on
`cargoless.preview.triform.dev` gets `401` for those paths. The split
is enforced by route, not by host.

## `/app` JSON shape

```json
{
  "instances": [
    {
      "name": "dev",
      "phase": "serving",
      "serving_sha": "<sha or null>",
      "last_green": "<sha or null>",
      "last_red_sha": null,
      "last_red_reason": null,
      "pending_sha": null,
      "draining": 0
    }
  ],
  "ready": true
}
```

`phase` ∈ `building` | `queued` | `probing` | `probing+serving` |
`serving` | `idle`.

## Watching a roll-in-progress

```bash
# Poll the dev canary's phase until a new sha settles serving:
while :; do
  curl -s https://cargoless.preview.triform.dev/app \
    | jq -r '.instances[] | select(.name=="dev") |
        "\(.phase) serving=\(.serving_sha[0:8]) pending=\(.pending_sha[0:8] // "—") last_red=\(.last_red_sha[0:8] // "—")"'
  sleep 2
done
```

The daemon's **never-serve-red** guarantee is structural: the `serving`
field only advances on a successful health probe (single Promote site in
`crates/cargoless-core/src/appstate.rs`). A red build leaves
`serving_sha` byte-unmoved and surfaces in `last_red_sha` /
`last_red_reason`; the old image keeps answering on the public app host
throughout.

`last_red_sha` / `last_red_reason` always reflect *current* brokenness: the
moment a newer build serves green, the prior red is cleared (it is superseded
by a known-good, servable sha). So the canary never advertises a stale
`last_red=<old-sha>` after a newer green is already serving — there is no
phantom red to age out. (Crash-recovery respawn of an older green bundle keeps
`last_red`, because in that case the tip genuinely is still red.)

## Source-of-truth split

| What | Where |
|---|---|
| LIVE manifest (Flux-reconciled) | tf-multiverse `deployment/kubernetes/apps/staging/cargoless-preview.yaml` |
| GENERATOR for the live manifest | tf-multiverse `scripts/cargoless-app/gen-preview-manifest.py` |
| Reference copy in cargoless | `deploy/cargoless-appserve.k8s.yaml` (kept in sync) |
| Auth-exemption itself | `crates/cargoless-core/src/transport/http.rs` (search `/app — read-only app-serve status, structurally auth-exempt`) |
| Operator runbook | tf-multiverse `scripts/cargoless-app/PREVIEW-SETUP.md` |

Edit the live manifest by editing the generator + regenerating, not the
generated YAML directly (the file header carries the same warning).
