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

COPY --from=builder /src/target/release/cargoless /usr/local/bin/cargoless
COPY --from=builder /tmp/rust-analyzer /usr/local/bin/rust-analyzer
COPY --from=builder /tmp/rust-analyzer-libs/ /usr/local/lib/rust-analyzer/

ENV CARGO_HOME=/cache/cargo \
    RUSTUP_HOME=/cache/rustup \
    CARGO_TARGET_DIR=/cache/target \
    LD_LIBRARY_PATH=/usr/local/lib/rust-analyzer \
    CARGO_INCREMENTAL=0 \
    RUST_ANALYZER=/usr/local/bin/rust-analyzer \
    CARGOLESS_VERDICT_MODE=ra \
    CARGOLESS_PUSH_ONLY=1 \
    TF_RA_CHECK_DISABLED=1

WORKDIR /workspace/tf-multiverse
ENTRYPOINT ["/usr/local/bin/cargoless"]
