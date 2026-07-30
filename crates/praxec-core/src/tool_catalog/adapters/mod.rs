//! Registry adapters — one module per `RegistrySpec` variant that knows how
//! to turn that registry's native shape into [`super::candidate::ToolCandidate`]s.
//! Each adapter is pure over [`super::registry::CatalogIo`] (see that
//! module's docs) so it is unit-tested with a fake IO, no network.

pub mod github_org;
pub mod mcp_registry;
pub mod rest;
pub mod static_adapter;

pub use github_org::GithubOrgAdapter;
pub use mcp_registry::McpRegistryAdapter;
pub use rest::RestAdapter;
pub use static_adapter::StaticAdapter;
