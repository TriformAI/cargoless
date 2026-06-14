FROM registry.triform.cloud/mirror/triform-builder-v2:0.1 AS builder

WORKDIR /src
COPY . .
RUN cargo build --release -p cargoless
# rust-analyzer MUST match the toolchain that ANALYZES the served workspace
# (/workspace/tf-multiverse), NOT the toolchain that builds the cargoless
# daemon. The proc-macro bridge is ABI-versioned: at runtime RA spawns the
# proc-macro-srv from the *workspace* sysroot, and if RA's ABI is older than
# that srv (the field failure: srv v6 ⇄ RA v5) RA refuses ALL macro expansion.
# With checkOnSave off (TF_RA_CHECK_DISABLED=1 below) RA-native diagnostics are
# the ONLY verdict signal, and on a macro-heavy tree (leptos/serde/async-trait)
# no expansion ⇒ RA publishes ZERO diagnostics ⇒ every push timer-settles an
# empty window ⇒ verdict=unknown for every gate (the CGLS pool stall).
#
# cargoless's own rust-toolchain.toml pins 1.85.0 (for building the daemon), so
# a bare `rustup component add rust-analyzer` inside /src resolves the 1.85.0 RA
# — the skew. tf-multiverse has no rust-toolchain.toml, so it is analyzed with
# the image-default toolchain (currently 1.93.1). Pin RA to THAT toolchain
# explicitly; keep RA_TOOLCHAIN in lockstep with the base image's default (=
# the toolchain the served workspace builds with).
ARG RA_TOOLCHAIN=1.93.1-x86_64-unknown-linux-gnu
RUN rustup component add --toolchain "$RA_TOOLCHAIN" rust-analyzer \
    && ra="$(rustup which --toolchain "$RA_TOOLCHAIN" rust-analyzer)" \
    && cp "$ra" /tmp/rust-analyzer \
    && mkdir -p /tmp/rust-analyzer-libs \
    && cp -a "$(dirname "$(dirname "$ra")")"/lib/*.so* /tmp/rust-analyzer-libs/

FROM registry.triform.cloud/mirror/triform-builder-v2:0.1

ARG KUBECTL_VERSION=v1.35.0

RUN apt-get update -qq \
    && apt-get install -y --no-install-recommends \
        binutils \
        ca-certificates \
        clang \
        curl \
        git \
        jq \
        libssl-dev \
        lld \
        pkg-config \
        python3-yaml \
        ripgrep \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSLo /usr/local/bin/kubectl \
        "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/amd64/kubectl" \
    && chmod +x /usr/local/bin/kubectl

COPY --from=builder /src/target/release/cargoless /usr/local/bin/cargoless
COPY --from=builder /tmp/rust-analyzer /usr/local/bin/rust-analyzer
COPY --from=builder /tmp/rust-analyzer-libs/ /usr/local/lib/rust-analyzer/

# RUSTUP_TOOLCHAIN pins the toolchain the served workspace is analyzed with so
# it stays in lockstep with the rust-analyzer copied above (RA_TOOLCHAIN). The
# two MUST agree: the proc-macro-srv RA spawns comes from this toolchain's
# sysroot, and a mismatch silently breaks all macro expansion (see builder note
# above). Keep this value == RA_TOOLCHAIN's channel.
ENV PATH=/usr/local/cargo/bin:$PATH \
    CARGO_HOME=/cache/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    RUSTUP_TOOLCHAIN=1.93.1-x86_64-unknown-linux-gnu \
    CARGO_TARGET_DIR=/cache/target \
    LD_LIBRARY_PATH=/usr/local/lib/rust-analyzer \
    CARGO_INCREMENTAL=0 \
    RUST_ANALYZER=/usr/local/bin/rust-analyzer \
    CARGOLESS_VERDICT_MODE=ra \
    CARGOLESS_PUSH_ONLY=1 \
    TF_RA_CHECK_DISABLED=1

WORKDIR /workspace/tf-multiverse
ENTRYPOINT ["/usr/local/bin/cargoless"]
