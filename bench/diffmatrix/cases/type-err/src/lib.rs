//! diffmatrix case `type-err`: the load-bearing baseline RED.
//!
//! `answer` is annotated `-> i32` but returns a `&str` ⇒ E0308 from the
//! rustc front-end on a plain `cargo check`. This is the error class the
//! whole tool exists to catch; if cargoless reports GREEN here the
//! verdict authority is fundamentally broken (regression of FIELD
//! FINDING #2 / #8-redo). Ground-truth color = RED.
#[allow(clippy::all)]
pub fn answer() -> i32 {
    "not an integer"
}
