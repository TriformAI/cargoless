//! diffmatrix case `test-only-err`: the `--all-targets` coverage probe.
//!
//! The library itself is correct (so a bare `cargo check` is GREEN). The
//! `#[cfg(test)]` block calls `add` with a `&str` where an `i32` is
//! required ⇒ E0308, but ONLY when the test target is compiled
//! (`cargo check --all-targets` / `cargo test`). Ground-truth color =
//! RED (because the harness's host oracle always passes `--all-targets`).
//!
//! Diagnostic value: if cargoless is GREEN here, its verdict authority
//! does not cover test targets — a real, currently-shipping false-GREEN
//! class that Increment-3 must close. Honest: surfaced, not papered over.

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::add;

    #[test]
    fn add_works() {
        // intentional E0308: `&str` where `i32` is expected.
        assert_eq!(add(2, "two"), 4);
    }
}
