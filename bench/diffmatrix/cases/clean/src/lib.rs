//! diffmatrix case `clean`: the GREEN control.
//!
//! Must pass ALL three oracles:
//!   * `cargo check --all-targets --locked`                          → green
//!   * `cargo check --all-targets --target wasm32-unknown-unknown`    → green
//!   * `cargo clippy --all-targets --locked -- -D warnings`           → green
//!
//! If cargoless is RED here it is a FALSE-RED (cry-wolf); if any oracle
//! is RED here the fixture itself is broken (the harness BLOCKERs loud —
//! a broken control can never be allowed to look like a pass).

/// A deliberately idiomatic, lint-clean function (no `ptr_arg`,
/// no `needless_return`, no unused bindings).
pub fn sum(values: &[i32]) -> i32 {
    values.iter().sum()
}

#[cfg(test)]
mod tests {
    use super::sum;

    #[test]
    fn sums() {
        assert_eq!(sum(&[1, 2, 3]), 6);
    }
}
