# cargoless app-serve image — the cargoless daemon that *runs* the apps it
# certifies (not just the check/verdict gate). Unlike cargoless-serve.Dockerfile
# (the gate image), this one also carries the full WASM build toolchain,
# because app-serve executes the target repo's `cargoless.app.yaml` build steps
# in-container — for a Rust+WASM workspace like tf-multiverse that means a
# wasm32 portal build (wasm-bindgen + wasm-opt + tailwindcss), exactly the
# steps tf-multiverse's own production Dockerfile runs.
#
# Toolchain versions are PINNED to match tf-multiverse's Dockerfile so the
# preview builds the *same* artifacts staging does:
#   - wasm-bindgen-cli 0.2.114
#   - binaryen (wasm-opt) 122
#   - tailwindcss 4.2.1
#
# The cargoless binary is built the same way as the gate image (release, the
# `cargoless` bin). A rust-analyzer copy is NOT needed here: app-serve does not
# run the RA verdict path (that is the gate fleet's job, a separate
# deployment). app-serve's "verdict" is the health probe of a real booted app.

FROM registry.triform.cloud/mirror/triform-builder-v2:0.1 AS builder

WORKDIR /src
COPY . .
RUN cargo build --release -p cargoless

FROM registry.triform.cloud/mirror/triform-builder-v2:0.1

ARG KUBECTL_VERSION=v1.35.0
# Pinned WASM toolchain versions — keep in lockstep with tf-multiverse's
# Dockerfile (the preview must build byte-comparable artifacts to staging).
ARG WASM_BINDGEN_VERSION=0.2.114
ARG BINARYEN_VERSION=122
ARG TAILWIND_VERSION=4.2.1

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

# --- WASM toolchain (the app-serve-specific addition) ---------------------
# wasm32 target for the portal crate's cdylib.
RUN rustup target add wasm32-unknown-unknown

# wasm-bindgen-cli, version-locked to the workspace's wasm-bindgen dep.
RUN cargo install wasm-bindgen-cli --version "${WASM_BINDGEN_VERSION}" --locked

# binaryen's wasm-opt (release-size pass, `wasm-opt -Oz`).
RUN curl -fsSL \
        "https://github.com/WebAssembly/binaryen/releases/download/version_${BINARYEN_VERSION}/binaryen-version_${BINARYEN_VERSION}-x86_64-linux.tar.gz" \
        | tar -xz -C /opt \
    && ln -sf "/opt/binaryen-version_${BINARYEN_VERSION}/bin/wasm-opt" /usr/local/bin/wasm-opt

# tailwindcss standalone CLI (the portal's CSS pipeline).
RUN curl -fsSLo /usr/local/bin/tailwindcss \
        "https://github.com/tailwindlabs/tailwindcss/releases/download/v${TAILWIND_VERSION}/tailwindcss-linux-x64" \
    && chmod +x /usr/local/bin/tailwindcss

COPY --from=builder /src/target/release/cargoless /usr/local/bin/cargoless

# Toolchain matches the builder image; CARGO_TARGET_DIR points at the PVC-backed
# cache so warm incremental rebuilds are fast (cold is tens of minutes). Unlike
# the gate image, CARGO_INCREMENTAL is ON — app-serve rebuilds the same
# workspace across many shas, so incremental compilation is a real win.
ENV PATH=/usr/local/cargo/bin:$PATH \
    CARGO_HOME=/cache/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    RUSTUP_TOOLCHAIN=1.93.1-x86_64-unknown-linux-gnu \
    CARGO_TARGET_DIR=/cache/target \
    CARGO_INCREMENTAL=1

WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/cargoless"]
