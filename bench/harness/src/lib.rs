//! Shared library for the cargoless comparative bench harness.
//!
//! Used by the `cargoless-bench` binary (the new two-mode comparative
//! driver). The legacy `ra-latency` binary (`src/main.rs`) is self-contained
//! and does NOT depend on this library — that preserves the well-known
//! `S1_VERDICT:` output line the ci-gate `--bench` mode publishes.
//!
//! Std-only on purpose: the harness must not add a build/lock surface that
//! would muddy the latency it measures, and `--locked` CI's `Cargo.lock` is
//! kept dep-free across the whole tree.

pub mod fsutil;
pub mod modes;
pub mod proc;
pub mod stats;
pub mod tools;
pub mod verdict;
