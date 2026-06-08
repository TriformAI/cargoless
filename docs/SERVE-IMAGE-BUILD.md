# cargoless-serve image: build & deploy story

## The gap this fixes

`cargoless-serve` (the central verdict daemon + coalescer) is deployed in the
Triform cluster via Flux, but its container image has **no automated build
path**. Today the only way to produce a new `cargoless-serve` image is:

```bash
scripts/build-cargoless-serve-image    # docker build … && docker push
```

…on a machine that has a Docker daemon. That is an **implicit dependency on a
human's laptop**: agents (and headless environments) have no Docker, the
in-cluster `cargoless-builder` pod ships no image-build tooling
(docker/buildah/kaniko all absent), and CI (`.github/workflows/release.yml`)
only builds *release binaries* on tag-push — never the serve image.

Consequence: a code change that lands on `main` (e.g. the batch coalescer in
PR #1) cannot reach the running daemon without a human running a local build.
That is the bottleneck the rest of this document removes.

## Current topology (so the next agent doesn't re-derive it)

- **Image:** `registry.triform.cloud/cargoless/cargoless-serve:<tag>`, built
  from `deploy/cargoless-serve.Dockerfile` — a standard multi-stage
  `cargo build --release -p cargoless` on `triform-builder-v2`, plus a bundled
  `rust-analyzer` and `kubectl`.
- **Deployed by:** Flux Kustomization `cargoless-builder` (flux-system),
  `prune=false`, `interval=5m`, sourced from the **tf-multiverse** repo at
  `deployment/cargoless-builder/{serve,shards}.yaml` — **NOT** the cargoless
  repo's own `deploy/*.k8s.yaml` (those are reference copies Flux never reads).
- **Live tuning:** `CARGOLESS_PROJECT_CHECKS_MODE=off`,
  `CARGOLESS_REMOTE_PROJECT_CHECKS=1`. `off` is deliberate — heavy compiler
  witnesses are disabled in the serve path until the batch-coalescing throughput
  proof exists (see `docs/bench/HEAVY-PROJECT-CHECK-BATCHING.md`).
- **Registry creds in-cluster:** only `registry-pull`
  (`kubernetes.io/dockerconfigjson`, pull-scoped). **No push secret exists in
  `cargoless-builder` yet** — that is the one new primitive an in-cluster build
  needs.

## Recommended long-term solution

A repo-hosted, in-cluster, push-on-merge image build. Three layers, smallest
first; ship layer 1, then 2 when activation is near.

### Layer 1 — a checked-in kaniko build Job (removes the laptop dependency)

Add `deploy/jobs/build-serve-image.yaml`: a kaniko `Job` that builds
`deploy/cargoless-serve.Dockerfile` from a given git ref and pushes
`cargoless-serve:<version>-<sha>`. This mirrors the existing privileged-build
pattern already in tf-multiverse (`deployment/isolation/jobs/*` run privileged
builds with `registry-pull`; `deployment/builders/v2` is a DinD pool tagged
`builder:true,dind:true`).

Prerequisite (one-time, operator): provision a **push-scoped** registry secret
in `cargoless-builder` (e.g. `registry-push`). kaniko consumes it as
`/kaniko/.docker/config.json`. Pull secret is insufficient — kaniko must push.

Why kaniko over DinD: no privileged daemon, no Docker socket, runs as an
ordinary Job, and the cluster already pulls from this registry so the network
path is proven.

```
kaniko --context=<git-or-tar> \
       --dockerfile=deploy/cargoless-serve.Dockerfile \
       --destination=registry.triform.cloud/cargoless/cargoless-serve:<tag> \
       --cache=true
```

The build is plain `cargo build --release -p cargoless`; no cross-compile, no
Apple SDK — the same base image the cluster builder pool already uses.

### Layer 2 — wire it to merge (or a workflow_dispatch)

Two viable hosts; pick by where the org wants the trigger to live:

- **Forgejo Actions** (tf-multiverse already mirrors there): a job on
  push-to-main (path-filtered to `crates/**`, `deploy/cargoless-serve.Dockerfile`)
  that applies the Layer-1 Job and waits for completion, then bumps the image
  tag in `tf-multiverse/deployment/cargoless-builder/{serve,shards}.yaml` via a
  commit (Flux rolls it within its 5m interval).
- **GitHub Actions** extension of `release.yml`: add a `serve-image` job that
  builds+pushes the serve image alongside the existing binary matrix. Keeps one
  release surface, but needs a self-hosted runner with registry push (the
  binary build already uses native runners).

Either way the **tag bump in the tf-multiverse manifest is the deploy action** —
that file is the Flux source of truth, so the build is only half the loop.

### Layer 3 — close the two-repo manifest drift (root cause)

`cargoless/deploy/*.k8s.yaml` and `tf-multiverse/deployment/cargoless-builder/*`
are two copies of the same Deployment with no parity gate. The cargoless copy
drifted to `mode=hard` while live (tf-multiverse) is `mode=off`; an agent (this
one, first pass) read the wrong file and nearly reasoned from a false premise.

Long-term: **single source of truth.** Either delete the cargoless-repo serve
manifests (keep only the Dockerfile + build Job; deployment lives in
tf-multiverse) or add a CI parity check that diffs the env/image between the two.
Pending that, the cargoless copy is marked reference-only (this PR).

## Activation ladder (why none of this auto-enables heavy checks)

Building/deploying a coalescer image does **not** turn heavy witnesses on. That
is a separate, evidence-gated flip in the tf-multiverse manifest:

`mode=off` (today) → deploy coalescer image, `mode=warn` (run, never block;
measure `queue_wait_ms` / deduped `physical_runs` / sync_lock hold time under a
real 40-agent burst) → `mode=hard` only if the burst does not recreate the
3–8 min push-ingest sync_lock stall that caused `off` in the first place.

This is the repo's existing advisory→gate promotion pattern
(`.triform/guides/cargoless-advisory-promotion.md`), not a new mechanism.
