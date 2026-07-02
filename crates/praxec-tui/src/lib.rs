// T26 — restriction-category lint on production code only. See
// praxec-core/src/lib.rs for the rationale.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

//! Library surface for the Praxec TUI crate. Modules that have public
//! contracts (`interpreter`, `agent_config`, `tui_config`, `sub_agent`,
//! `praxec_mcp`) live here so integration tests and the sub-agent
//! spawner can reach them. The bin's `main.rs` re-imports via
//! `use praxec_tui::…`.
//!
//! Runtime-only modules with no test surface (e.g. `theme`) stay in
//! `main.rs`.

pub mod agent_config;
pub mod doctor;
pub mod doctor_probe_cache;
pub mod interpreter;
pub mod keyring;
pub mod lexicon;
pub mod mcp_caller;
pub mod mcp_init;
pub mod migrate;
pub mod praxec_mcp;
pub mod provider_keys;
pub mod reasoning;
pub mod sub_agent;
pub mod tui_config;
