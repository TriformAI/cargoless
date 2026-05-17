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

## Source & mirrors

- **Canonical public source:** [`github.com/TriformAI/cargoless`](https://github.com/TriformAI/cargoless) — the OSS-facing home; where issues, PRs, releases, and prebuilts live.
- **Internal dev mirror:** [`forgejo.triform.dev/triform/cargoless`](https://forgejo.triform.dev/triform/cargoless) — where the agent team's integration CI runs (dedicated cargoless-builder pod + `scripts/ci-gate` + Forgejo Actions). Contributor PRs are welcome on GitHub; the maintainers cherry-pick into Forgejo for the integration loop.

## Install

> Pre-release. The release-tagged install commands below will work once
> `v0.1.0` is cut (see Status). Today, only the from-source install against
> the development tip is supported.

**Install the current development tip (works today):**

```bash
cargo install --git https://github.com/TriformAI/cargoless.git \
              tf-cli --branch main --locked
```

**Why the explicit `tf-cli`:** `cargo install --git` walks the entire repo
for `Cargo.toml` files and refuses to pick when multiple installable binary
crates exist. This repo's `bench/{harness,fixture}` sub-workspaces produce
`ra-latency`, `cargoless-bench`, and `cargoless-bench-fixture` binaries
that cargo treats as candidates. Without `tf-cli`, you get:
> error: multiple packages with binaries found: cargoless-bench-fixture,
> cargoless-bench-harness, tf-cli.

**Why `--locked`:** the workspace ships a committed `Cargo.lock`; `--locked`
makes the dependency graph identical to what CI / `scripts/ci-gate` proved
green. See [D-RELEASE Appendix B](docs/design/D-RELEASE.md).

> No `--features integration` needed: as of commit 1c25017, `integration` is
> the default feature on `tf-cli`. The wired daemon (`build --watch --out`
> publisher pipeline) is included in the default install. Users who want the
> standalone holding-page semantics can still opt out via
> `--no-default-features`.

**Once `v0.1.0` releases (planned):**

```bash
# Source build via crates.io (universal: any platform with rustc)
cargo install <pubname>           # <pubname> = TBD per D1/CWDL-12

# Prebuilt via cargo-binstall (Linux x86_64-gnu + macOS aarch64/x86_64)
cargo binstall <pubname>
```

Prebuilts at first release: `x86_64-unknown-linux-gnu`,
`aarch64-apple-darwin`, `x86_64-apple-darwin`. Other targets (`aarch64-linux`,
Windows) fall back to `cargo install` (source compile). See
[docs/design/D-RELEASE.md §3](docs/design/D-RELEASE.md) for the full targets
matrix.

## Status

v0 in active development. Tracked in Plane project **CWDL** (one issue per
epic/AC). This repo builds **only via Forgejo CI** (push-to-build); there is
no local-cargo workflow — see [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Workspace

| Crate | Role |
|---|---|
| `tf-proto` | Shared contract types (daemon ↔ build ↔ future remote backends). |
| `tf-cas` | Content-addressed store. `ContentStore` trait + local-disk impl. |
| `tf-core` | The daemon: watcher, rust-analyzer wrapper, green/red model, build, serve. |
| `tf-cli` | The binary: `check` / `watch` / `build` / `status` / `clean`. |

## License

Apache-2.0. Decided — do not relitigate.
