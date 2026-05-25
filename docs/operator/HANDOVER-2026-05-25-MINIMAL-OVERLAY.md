# Cargoless Minimal Overlay Handover - 2026-05-25

## Current State

- `main` is at `d325e8c` after PR #14.
- Cluster `cargoless-serve` is live on `registry.triform.cloud/cargoless/cargoless-serve:0.2.2-minimal-overlay`.
- Cluster `cargoless-builder` is healthy again with a fresh `cargoless-cache` PVC.
- Local installed binary was updated during the rollout to `cargoless 0.2.0 git=294922a19bc6 dirty=true built=1779691114`.

## What Landed

- PR #12, `294922a`: minimal push overlay.
  - Client sends changed Rust file bodies and changed workspace config bodies only.
  - Client still sends all changed paths as metadata.
  - Server reads unchanged workspace config from its base checkout and overlays changed config bodies.
  - Push output now reports content files, body bytes, changed paths, and metadata-only paths.
- PR #13, `a04d5c3`: rolled the cluster serve image.
  - Built and pushed `0.2.2-minimal-overlay`.
  - Added `.codex-worktrees/` to `.dockerignore`.
  - Added `cargoless-buildkit-egress` NetworkPolicy for Kubernetes buildx pods.
- PR #14, `d325e8c`: preserved the live-good `cargoless-builder` pod template.
  - Restored `shareProcessNamespace: true`.
  - Restored ephemeral-storage request/limit (`1Gi`/`8Gi`).

## Live Verification

The live cluster smoke against a large tf-multiverse worktree succeeded after the image rollout:

```text
[cargoless:push] overlay content files=116 bytes=6681718 changed_paths=136 metadata_only_paths=20
[cargoless:push] ack from http://127.0.0.1:8787: accepted=true applied_files=116 worktree=/workspace/tf-multiverse
[cargoless:push] fresh verdict via status worktree=/workspace/tf-multiverse verdict=green
```

This is the key behavior change: the same worktree previously hit payload-size failures around the 32 MiB class; the content body in this smoke was about 6.7 MiB.

Current cluster check:

```text
cargoless-builder  1/1 Running
cargoless-serve    1/1 Running
cargoless-cache    Bound pvc-dfb53b5a-1a0c-4d4a-8476-407e15a9be6c
```

The temporary Kubernetes buildx deployment `cargoless-kube0` was removed after the image push. The committed NetworkPolicy remains so the builder can be recreated for the next image build.

## Operational Notes

- The old `cargoless-cache` Longhorn volume `pvc-90356bff-3fe8-4571-86a3-a005d927a293` faulted during the manifest apply. It was cache-only, so it was deleted and replaced.
- The replacement cache starts cold. The next cargoless repository `scripts/ci-gate` run may pay a rebuild cost.
- `cargoless-serve` uses its own tf-multiverse workspace PVC and was not affected by the cache replacement.
- Local `.codex-worktrees/` scratch is ignored by both Docker context and git.

## Next Steps

- Run a few normal tf-multiverse agent merges through `scripts/check-remote` and watch the new push accounting line.
- Add server-side request metrics for overlay body bytes, metadata-only paths, verdict latency, and concurrent pushes.
- Keep the replacement model strict: no legacy cargo-check fallback in tf-multiverse merge paths.
