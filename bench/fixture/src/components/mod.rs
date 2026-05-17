//! Reusable UI components. Each is `view!`-macro heavy on purpose: the
//! benchmark's whole premise is that `view!` expansion is rust-analyzer's
//! documented weak spot, so the fixture must contain a realistic amount of it.

pub mod counter;
pub mod form;
pub mod metrics;
pub mod nav;
pub mod todo;
