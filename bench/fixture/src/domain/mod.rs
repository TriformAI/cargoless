//! Pure (no-`view!`) domain layer. This is the benchmark's **RA-native
//! error target**: errors here (unresolved method, type mismatch) are the
//! kind rust-analyzer detects from its own analysis *without* macro
//! expansion — expected to be its fast path. Contrast with
//! `components::metrics`, the post-`view!`-expansion target.

pub mod model;
pub mod validation;
