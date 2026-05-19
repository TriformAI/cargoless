//! diffmatrix case `wasm-only-err`: the host-vs-wasm divergence oracle.
//!
//! On the host (`x86_64`/`aarch64`) the `cfg(target_arch = "wasm32")`
//! item is excluded, so `cargo check` and `cargo clippy` are GREEN.
//! Under `cargo check --target wasm32-unknown-unknown` the
//! `compile_error!` fires ⇒ RED. Ground-truth color = RED (the harness
//! always runs the wasm oracle for this case).
//!
//! This is deterministic and dependency-free on purpose — a
//! `compile_error!` cannot be confused with a flaky build or a network
//! failure, so a RED here is unambiguously the divergence we are
//! gating, never noise.
//!
//! EXPECTED today (v0.2.0): cargoless = GREEN (host-only authority) ⇒
//! classified FALSE-GREEN ⇒ the gate FAILS LOUD. That failure is the
//! point: it is the standing proof that Increment-3 has not yet added a
//! wasm-target oracle. The case turns bit-identical (RED ≡ RED) only
//! when that authority lands.

#[cfg(target_arch = "wasm32")]
compile_error!(
    "wasm-only-err: intentional wasm32-only failure (differential fixture). \
     A host cargo-check passes; only `--target wasm32-unknown-unknown` \
     reaches this. If cargoless reports GREEN, that is the load-bearing \
     FALSE-GREEN this harness exists to catch."
);

/// Host-buildable surface so the crate is a real (non-empty) check on
/// every target; lint-clean so the host clippy oracle stays GREEN.
pub fn host_ok(values: &[u32]) -> u32 {
    values.iter().copied().max().unwrap_or(0)
}
