//! cargoless S1 / AC#2 reference fixture — entry point.
//!
//! A realistically-structured Leptos CSR application: multiple components,
//! signals, derived state, control flow (`For` / `Show`), event handlers, a
//! domain layer with trait impls, validation, and formatting helpers. The
//! shape (lots of `view!` macro expansion + a real trait/type surface) is
//! chosen to exercise exactly the rust-analyzer code paths the S1 spike
//! interrogates.

mod app;
mod components;
mod domain;
mod pages;
mod util;

use leptos::*;

fn main() {
    // CSR mount. In a real cargoless inner loop this is what the dev server
    // ships once the build is green; here it only needs to typecheck so
    // rust-analyzer has a complete, analyzable crate graph.
    console_error_panic_hook_noop();
    mount_to_body(app::App);
}

/// Intentionally dependency-free stand-in so the fixture needs exactly one
/// external crate (`leptos`). Keeps the determinism surface minimal while
/// still being a non-trivial crate.
fn console_error_panic_hook_noop() {
    // no-op
}
