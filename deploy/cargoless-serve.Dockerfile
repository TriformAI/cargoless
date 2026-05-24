FROM registry.triform.cloud/mirror/triform-builder-v2:0.1 AS builder

WORKDIR /src
COPY . .
RUN cargo build --release -p cargoless

FROM registry.triform.cloud/mirror/triform-builder-v2:0.1

COPY --from=builder /src/target/release/cargoless /usr/local/bin/cargoless

ENV CARGO_HOME=/cache/cargo \
    RUSTUP_HOME=/cache/rustup \
    CARGO_TARGET_DIR=/cache/target \
    CARGO_INCREMENTAL=0 \
    CARGOLESS_VERDICT_MODE=ra \
    CARGOLESS_PUSH_ONLY=1 \
    TF_RA_CHECK_DISABLED=1

WORKDIR /workspace/tf-multiverse
ENTRYPOINT ["/usr/local/bin/cargoless"]
