# TF-Trunk (working name — product name is decision D1)

> **Positioning:** `trunk serve`, but it actually knows what's green and tells
> you the moment it isn't.
>
> **Vision:** The codebase always knows what works, and tells you the moment it
> doesn't.

A local-first, open-source CLI that replaces `trunk serve` for Rust + WASM
development. It keeps a warm `rust-analyzer`, always knows which files compile,
**never serves a broken build to the browser**, and gives sub-second
save→verdict feedback.

> **Name notice:** `TF-Trunk` / `tf-trunk` is a *name-neutral working
> identifier* for the repo and crates. The shipping product/binary name is an
> open decision (Plane **CWDL-12 / D1**) and must be chosen before any public
> release. `tf` is **not** the name — it collides with Terraform.

## Status

v0 in active development. Tracked in Plane project **CWDL** (one issue per
epic/AC). This repo builds **only via Forgejo CI** (push-to-build); there is no
local-cargo workflow — see `CONTRIBUTING.md`.

## Workspace

| Crate | Role |
|---|---|
| `tf-proto` | Shared contract types (daemon ↔ build ↔ future remote backends). |
| `tf-cas` | Content-addressed store. `ContentStore` trait + local-disk impl. |
| `tf-core` | The daemon: watcher, rust-analyzer wrapper, green/red model, build, serve. |
| `tf-cli` | The binary: `serve` / `check` / `status` / `clean`. |

## License

Apache-2.0. Decided — do not relitigate.
