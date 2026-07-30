//! MCP tool discovery, Phase 1 (read-only catalog) — SPEC design doc
//! `docs/design/plans/2026-07-30-mcp-tool-discovery-phase1.md`.
//!
//! Mirrors [`crate::currency`]'s shape: typed data ([`candidate`]) + pure
//! decision logic ([`catalog`]) behind an injectable IO seam
//! ([`registry::CatalogIo`]), with each registry a small adapter
//! ([`adapters`]) that maps its native response to one [`candidate::ToolCandidate`].
//!
//! Phase 1 is reads only: no provisioning, no secrets elicitation, no config
//! mutation. `discover`/`evaluate` rank an assembled catalog; nothing here
//! installs anything.

pub mod adapters;
pub mod candidate;
pub mod catalog;
pub mod real_io;
pub mod registry;

pub use adapters::{GithubOrgAdapter, McpRegistryAdapter, RestAdapter, StaticAdapter};
pub use candidate::{RequiredField, Requires, ToolCandidate, ToolSource, Transport, TrustTier};
pub use catalog::{Cache, assemble, discover, evaluate};
pub use real_io::RealCatalogIo;
pub use registry::{CatalogIo, GhRepo, RegistryAdapter, RegistrySpec, registries_from};
