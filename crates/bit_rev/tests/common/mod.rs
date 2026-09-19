//! Shared integration-test harness. Re-exports `testkit`.
//!
//! Integration-test modules are not importable from `benches/` or `examples/`.
//! The types live in the `testkit` crate so those targets can share them.

pub use testkit::*;
