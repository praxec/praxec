//! Library surface for the `praxec` gateway.
//!
//! The gateway implementation lives in [`gateway`] as a reusable library: the
//! `praxec` binary (`src/main.rs`) is a thin shim that registers the
//! `kind: llm` overlay (behind the `llm-executor` feature) and the `kind: agent`
//! overlay (behind the `agents` feature), then calls [`gateway::run_cli`]. Both
//! features are default-on so a standard `cargo install praxec` ships with
//! both kinds available.

/// Production models.yaml-backed affinity resolver (gateway.models_yaml).
/// Always compiled: agents (first-class, ADR-0007) resolve affinity → model
/// binding through it; the llm-executor trait impl inside the module is
/// separately gated on `llm-executor`.
pub mod affinity_resolver;

pub mod gateway;
pub mod gateway_config;
