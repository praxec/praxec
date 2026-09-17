//! The single source of truth for praxec's default model routing.
//!
//! [`SEED_MODELS_YAML`] is the embedded `data/seed-models.yaml` — the curated,
//! commodity-first `models.yaml` shipped as the default routing table. It is
//! consumed by BOTH the TUI `seed`/`migrate` output (praxec-tui) and the
//! `praxec init` scaffold (praxec), so model choices live in exactly one file.
//! Update the data file, not a Rust literal.

/// The shipped default `models.yaml` content (curated routing table). Embedded
/// at build time from `data/seed-models.yaml`; an operator's own
/// `gateway.models_yaml` overrides it at runtime.
pub const SEED_MODELS_YAML: &str = include_str!("../data/seed-models.yaml");
