//! SPEC §33 D9 — FMECA F5 gate runner.
//!
//! `tests/common/mock_provider.rs` declares `#[test]` functions for the
//! variant-coverage and catalog-size checks; this thin wrapper pulls
//! the module in so `cargo test -p praxec-llm-executor` actually
//! runs them. Otherwise `common/` is just a subdirectory cargo never
//! compiles standalone.

mod common;

// The gate fires the moment the `common::mock_provider` module is
// compiled (its `#[test]` fns are visible through this re-export).
use common::mock_provider::MockProviderScenarios;

#[test]
fn d9_scenario_catalog_present() {
    // Smoke: the catalog returns the documented count. The F5 gate
    // proper (variant coverage) lives inside `mock_provider.rs`.
    assert!(MockProviderScenarios::all().len() >= 11);
}
