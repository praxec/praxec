//! Integration-test entry point for the `model_resolver` suite.
//!
//! The individual test files live in `tests/model_resolver/`. Cargo only
//! compiles top-level `tests/*.rs` as test targets, so this module re-declares
//! each sub-file as a `#[path]`-attributed module to keep them in the build.

#[path = "model_resolver/model_resolver_classify.rs"]
mod model_resolver_classify;
#[path = "model_resolver/model_resolver_config.rs"]
mod model_resolver_config;
#[path = "model_resolver/model_resolver_main_args.rs"]
mod model_resolver_main_args;
#[path = "model_resolver/model_resolver_preflight.rs"]
mod model_resolver_preflight;
#[path = "model_resolver/model_resolver_validate_envelope.rs"]
mod model_resolver_validate_envelope;
#[path = "model_resolver/model_resolver_walk.rs"]
mod model_resolver_walk;
