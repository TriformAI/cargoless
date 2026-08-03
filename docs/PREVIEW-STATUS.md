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
| `GET /readyz` | the daemon can accept and serve work (`200`/`503`) — see below |
| `GET /app` | full per-instance JSON snapshot |

Every other control-plane route (`/admin/*`, `/status`, `/verdict`,
`/worktrees*`, `/diagnostics`, `/events`) still requires the bearer
token from `cargoless-serve-auth` — an unauthenticated caller on
`cargoless.preview.triform.dev` gets `401` for those paths. The split
is enforced by route, not by host.

## What `/readyz` means

**"This daemon can accept and serve work."** Concretely:

```
ready  ⟺  some instance is currently serving
      AND  no LIVE instance is failing to serve a green it owns
```

*Live* is a **liveness test**, not merely "this slot exists and is not
green": an instance counts only while its observable state changed within
the last hour (`readiness.stale_after_secs`). An instance on someone's
critical path re-arms that stamp simply by being used — every push moves
its ref, which advances its phase — so a broken `dev` people are actively
pushing to holds the probe at `503` for exactly as long as it is broken.
A slot nobody has touched in weeks emits no transitions, goes quiet, and
**ages out of the calculation** by rule, with no per-name allowlist.

This is what stops the two useless readiness probes:

- **Not always-green.** "Some instance is currently serving" is a fact
  about the present with no stamp to age, so it can never be excused. A
  daemon with nothing serving is `503` however quiet every slot is.
- **Not wedged-red.** Before this rule, one abandoned instance — `merge`
  last built 2026-08-02, `feature-x` 2026-06-24 — held `/readyz` at `503`
  indefinitely, delisting the pod and its *healthy* `dev` and `lane` with
  it.

An aged-out instance is reported, never hidden: it keeps its full row on
`/app` (including `stale: true` and `idle_secs`) and is named in
`readiness.stale_degraded`. `/readyz` answers "is this daemon working";
`/app` answers "…and here is exactly what is not".

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
      "draining": 0,
      "last_change_unix": 1754000000,
      "idle_secs": 12,
      "stale": false
    }
  ],
  "ready": true,
  "readiness": {
    "stale_after_secs": 3600,
    "stale_degraded": []
  }
}
```

`phase` ∈ `building` | `queued` | `probing` | `probing+serving` |
`serving` | `idle`.

`last_change_unix` / `idle_secs` / `stale` are the per-instance liveness
stamp `/readyz` ages against. `stale_degraded` lists the instances that
*are* failing to serve a green they own but were aged out of the
readiness verdict — empty on a healthy daemon; non-empty means "ready,
but these slots are abandoned".

```bash
# Which slots were excused from the readiness answer, and for how long?
curl -s https://cargoless.preview.triform.dev/app \
  | jq '{ready, stale_degraded: .readiness.stale_degraded,
          idle: [.instances[] | {name, phase, idle_secs, stale}]}'
```

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
