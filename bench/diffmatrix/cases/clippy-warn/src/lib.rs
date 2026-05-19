//! diffmatrix case `clippy-warn`: the clippy-tier coverage probe.
//!
//! `count` takes `&Vec<i32>` — `clippy::ptr_arg` (a default-warn clippy
//! lint that rustc itself does NOT emit). `cargo check` is GREEN on host
//! and wasm; `cargo clippy --all-targets -- -D warnings` is RED.
//! Ground-truth color = RED (the harness always runs the clippy oracle).
//!
//! Chosen because `ptr_arg` is stable, deterministic, and unambiguously
//! clippy-only — a RED here can only mean "the clippy oracle fired",
//! never a compiler change or flake. The body is otherwise correct so
//! the rustc oracles stay GREEN and the *only* RED signal is clippy.

// The lint is intentional; the inner allow keeps any *other* lint from
// muddying the single intended `ptr_arg` signal.
#[allow(clippy::ptr_arg)]
mod intentional {
    /// `&Vec<i32>` (should be `&[i32]`) — clippy::ptr_arg, suppressed
    /// locally so this module proves the fixture *can* be clippy-clean.
    pub fn count_suppressed(v: &Vec<i32>) -> usize {
        v.len()
    }
}

/// The live `ptr_arg` trigger (NOT allowed) — this is the case's RED.
pub fn count(v: &Vec<i32>) -> usize {
    v.len()
}

pub use intentional::count_suppressed;
