FROM registry.triform.cloud/mirror/triform-builder-v2:0.1 AS builder

WORKDIR /src
COPY . .
RUN cargo build --release -p cargoless
RUN rustup component add rust-analyzer \
    && ra="$(rustup which rust-analyzer)" \
    && cp "$ra" /tmp/rust-analyzer \
    && mkdir -p /tmp/rust-analyzer-libs \
    && cp -a "$(dirname "$(dirname "$ra")")"/lib/*.so* /tmp/rust-analyzer-libs/

FROM registry.triform.cloud/mirror/triform-builder-v2:0.1

RUN apt-get update -qq \
    && apt-get install -y --no-install-recommends \
        binutils \
        ca-certificates \
        clang \
        git \
        jq \
        libssl-dev \
        lld \
        pkg-config \
        python3-yaml \
        ripgrep \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/cargoless /usr/local/bin/cargoless
COPY --from=builder /tmp/rust-analyzer /usr/local/bin/rust-analyzer
COPY --from=builder /tmp/rust-analyzer-libs/ /usr/local/lib/rust-analyzer/

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
