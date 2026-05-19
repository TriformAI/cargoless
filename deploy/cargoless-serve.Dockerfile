# cargoless-serve — the central-daemon product-runtime image.
#
# WHY THIS EXISTS (Increment 1b, surfaced by #226)
#   deploy/cargoless-serve.k8s.yaml runs the long-lived networked Model R
#   daemon. Unlike the PASSIVE cargoless-builder pod (`sleep infinity` on a
#   pure toolchain image — ci-gate compiles per-SHA on demand, the product
#   binary is never resident), THIS pod must EXECUTE the product. It needs:
#
#     • `cargoless` built `--features integration` — the WIRED daemon is
#       deliberately EXCLUDED from the default workspace build (ci-gate's
#       integ-* checks gate exactly this feature). A default-feature
#       release binary (the one .github/workflows/release.yml's `build`
#       matrix already ships for binstall) is NOT the serve daemon. This
#       image is the ONLY artifact carrying the integration build.
#     • `rust-analyzer` + the 1.85.0 toolchain on PATH — servedrv spawns
#       `rust-analyzer` (rust_analyzer_command()) and RA shells `cargo
#       check` for flycheck; both must resolve offline INSIDE the pod
#       (cluster pod egress to the public internet is blocked — verified
#       in deploy/cargoless-builder.k8s.yaml's header). They are therefore
#       BAKED here at image-build time, where network IS available (the
#       GH Actions runner), NOT added at pod runtime.
#
#   This is OPT-1 from the #226 lane-A recommendation (baked image) — a
#   REAL new release-pipeline CD artifact, not a manifest tweak. See
#   docs/design/D-RELEASE.md §5.1 (image-cargoless-serve job topology) and
#   the §9 scoped carve-out of the historical "no container images" v0
#   non-goal (superseded for the operational serve image ONLY, with
#   provenance — the local-dev-tool binstall/crates.io distribution is
#   UNCHANGED).
#
# BUILD CONTEXT / INPUT
#   Built by .github/workflows/release.yml's `image-cargoless-serve` job
#   AFTER it runs, on the GH ubuntu runner (network available there):
#     cargo build -p cargoless --features integration --release --locked \
#       --target x86_64-unknown-linux-gnu
#   then `docker build` with build context = repo root and
#   --build-arg BIN_SRC=target/x86_64-unknown-linux-gnu/release/cargoless.
#   The base image is glibc/Debian-class (matches the gnu triple) — do NOT
#   switch the binary to musl without switching FROM in lockstep.
#
# VERSION DISCIPLINE (digest == version)
#   The job tags this image `:${VERSION}` (== the git tag, e.g. v0.2.0 →
#   `0.2.0`, the exact string deploy/cargoless-serve.k8s.yaml references)
#   AND an immutable `:git-${SHA}`. tag-validate already proves tag ==
#   [workspace.package].version, so the image version is the release
#   version BY CONSTRUCTION. The OCI labels below carry the same identity
#   so `docker inspect` is self-describing.
#
# VALIDATION BOUNDARY (honest — same discipline as #226)
#   The exact rust-analyzer discovery path (PATH vs `rustup run`) and the
#   offline flycheck behaviour inside the pod are VALIDATED end-to-end
#   only at the Increment-0+1 deploy milestone against a real binding
#   daemon (#225). This Dockerfile is design-complete; it is not asserted
#   live-correct ahead of that milestone.

# ── base: the same pre-baked toolchain image the builder fleet trusts ──
# triform-builder-v2:0.1 ships a Rust toolchain + rustfmt + clippy and is
# node-reachable in-cluster via the replicated registry-pull secret. Its
# baked default toolchain is NOT 1.85.0, so we pin + seed 1.85.0 (matching
# rust-toolchain.toml / ci.yml's rust:1.85-bookworm) at build time.
FROM registry.triform.cloud/mirror/triform-builder-v2:0.1

# OCI provenance — set by the workflow via --build-arg; digest==version.
ARG VERSION="0.0.0-dev"
ARG VCS_REF="unknown"
LABEL org.opencontainers.image.title="cargoless-serve" \
      org.opencontainers.image.description="cargoless central Model R daemon (serve --repo, --features integration)" \
      org.opencontainers.image.source="https://github.com/TriformAI/cargoless" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${VCS_REF}" \
      org.opencontainers.image.licenses="Apache-2.0"

# Pin + seed the 1.85.0 toolchain and bake rust-analyzer + rust-src so
# flycheck resolves with ZERO network inside the pod. Network IS available
# HERE (image build on the GH runner) — this is the whole reason the
# components are baked rather than `rustup component add`'d at runtime
# (in-pod egress is blocked; see the builder manifest header). `rustup`
# is on PATH in triform-builder-v2; if a future base lacks a given
# component the build fails LOUD here, not silently at pod runtime.
RUN set -eux; \
    rustup toolchain install 1.85.0 --profile minimal; \
    rustup default 1.85.0; \
    rustup component add rust-analyzer rust-src --toolchain 1.85.0; \
    rustup which --toolchain 1.85.0 rust-analyzer; \
    rust-analyzer --version

# The integration-built product binary, produced by the workflow step
# immediately before `docker build` (NOT compiled in-image — keeps the
# image a thin runtime layer over the trusted toolchain base, and the
# release binary is the gated `--locked` artifact, not an in-Docker
# re-resolve).
ARG BIN_SRC=target/x86_64-unknown-linux-gnu/release/cargoless
COPY ${BIN_SRC} /usr/local/bin/cargoless
RUN set -eux; \
    chmod 0755 /usr/local/bin/cargoless; \
    /usr/local/bin/cargoless --version

# The k8s manifest supplies the full arg vector (serve --repo … --bind …
# --state-dir … --cas-dir …) and the CARGOLESS_AUTH_TOKEN env. ENTRYPOINT
# is the bare binary so `args:` in the Deployment is the single source of
# the invocation (and `cargoless --version` / `--help` work for probes /
# debugging without overriding the entrypoint).
ENTRYPOINT ["cargoless"]
