# TF-Trunk (working name — product name is decision D1)

> **Positioning:** the Rust+WASM inner loop that actually knows what's green
> and tells you the moment it isn't.
>
> **Vision:** The codebase always knows what works, and tells you the moment it
> doesn't.

A local-first, open-source CLI for Rust + WASM development. **v0 is a headless
continuous checker + latest-green artifact publisher**: it keeps a warm
`rust-analyzer`, gives sub-second save→verdict feedback, and **publishes the
latest green build while never publishing a red one** (`.cargoless/latest-green`
only advances on green). The live browser dev-server that replaces `trunk
serve` (HTTP + WebSocket reload) is the **v0.1** adapter layered on top of that
published output — see `ROADMAP.md`.

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
| `tf-core` | The daemon: watcher, rust-analyzer wrapper, green/red model, build, latest-green publisher. (`server` module = v0.1.) |
| `tf-cli` | The binary (v0): `check` / `watch` / `build --watch --out` / `status` / `clean`. `serve` = v0.1. |

## License

Apache-2.0. Decided — do not relitigate.
