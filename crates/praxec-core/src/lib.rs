// T26 — restriction-category lint on production code only.
// `#[cfg(test)]` modules inside production sources DO see this when
// invoked via `cargo build`, but `cargo test` evaluates `not(test)`
// as false (test cfg is on) and silences the warning everywhere —
// which is what we want: tests panic deliberately via unwrap, prod
// code should `.expect("invariant: ...")` or propagate.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

//! praxec-core: workflow runtime, ports, audit, reliability.
//!
//! Every exposed proxy tool is internally represented as a workflow transition.
//! A simple proxy config and a fully-governed workflow share one execution
//! model — see `proxy_workflow::compile_proxy_workflow` for the bridge.
//!
//! # Lock poisoning policy
//!
//! Every `RwLock` / `Mutex` in this crate is acquired via
//! `.expect("LOCK_POISONED: ...")` rather than `.unwrap()`. The
//! invariant: NO holder of any lock in this crate performs fallible
//! I/O or holds an `await` point while the guard is live. Under
//! that invariant, the locks cannot be poisoned (poisoning requires
//! a panic in a holder, which the invariant forbids).
//!
//! If you add a `?`, `.await`, or `panic!()` inside a lock guard,
//! the invariant is broken and the `expect` becomes a real panic
//! risk. Either refactor to release the guard first or upgrade to
//! `parking_lot` (no poisoning).

pub mod audit;
pub mod binding;
pub mod bus;
pub mod cap_verb;
pub mod capability;
pub mod catalog;
pub mod config;
pub mod contract_hash;
pub mod cost_report;
pub mod deescalation;
pub mod discovery;
pub mod embeddings;
pub mod error;
pub mod fs;
pub mod guards;
pub mod hop;
pub mod hot_reload;
pub mod intent_index;
pub mod lexicon;
pub mod lock_scheduler;
pub mod mapping;
pub mod mission;
pub mod model;
pub mod model_catalog;
pub mod model_resolver;
pub mod overlay;
pub mod plan;
pub mod ports;
pub mod promotion;
pub mod provider_keys;
pub mod providers;
pub mod proxy_workflow;
pub mod registry_v3;
pub mod reliability;
pub mod repo;
pub mod repo_git;
pub mod repo_locks;
pub mod runtime;
pub mod sandbox;
pub mod skills;
pub mod slot;
pub mod store;
pub mod structural_fingerprint;
pub mod templating;
pub mod tier;
pub mod tool_descriptor;
pub mod tuning;
pub mod use_binding;
pub mod validate;

pub use audit::{
    AuditEvent, AuditSink, FileAuditSink, MemoryAuditSink, NullAuditSink, RotationInterval,
    StderrAuditSink,
};
pub use capability::{Capability, CapabilityRegistry, CapabilitySource};
pub use discovery::{
    DiscoveryIndex, DiscoveryItem, DiscoveryKind, DiscoveryLink, EvidenceSignal,
    InMemoryDiscoveryIndex, RankedCandidate, SearchHit, SearchRequest, TopologySignal,
    rank_candidates,
};
pub use error::{ErrorClass, ExecutorError, RuntimeError};
pub use fs::{Filesystem, InMemoryFilesystem, RealFilesystem};
pub use guards::{DefaultGuardEvaluator, GuardKind};
pub use mapping::{merge_output, read_in_scopes};
pub use model::*;
pub use overlay::SingleKindOverlay;
pub use ports::*;
pub use proxy_workflow::{DEFAULT_PROXY_STATE, DEFAULT_PROXY_WORKFLOW_ID, compile_proxy_workflow};
pub use registry_v3::{
    CrossmatrixRole, CrossmatrixRow, Pack, PackTier, Registry, RegistryError, RegistrySchema,
    RegistryTool,
};
pub use reliability::{Backoff, FallbackPolicy, ReliabilityPolicy, RetryPolicy};
pub use repo::{REPO_MANIFEST_SCHEMA_V1, RepoLayout, RepoManifest, load_manifest, load_repo};
pub use runtime::WorkflowRuntime;
pub use store::{
    ConfigDefinitionStore, FileWorkflowStore, InMemoryEvidenceStore, InMemoryWorkflowStore,
    SqliteWorkflowStore,
};
pub use tool_descriptor::{ToolDescriptor, ToolDescriptorError, ToolKind};

// ---------------------------------------------------------------------------
// Backward-compat aliases — these modules were moved into subdirectories
// but retain their old top-level paths for external callers.
// ---------------------------------------------------------------------------

/// Backward-compat — types moved to `crate::runtime::runtime_transition_resolver`.
pub mod runtime_transition_resolver {
    pub use crate::runtime::runtime_transition_resolver::*;
}

/// Backward-compat — types moved to `crate::slot::slot_constraint`.
pub mod slot_constraint {
    pub use crate::slot::slot_constraint::*;
}

/// Backward-compat — types moved to `crate::slot::slot_table`.
pub mod slot_table {
    pub use crate::slot::slot_table::*;
}

/// Backward-compat — types moved to `crate::lexicon::lexicon_candidates`.
pub mod lexicon_candidates {
    pub use crate::lexicon::lexicon_candidates::*;
}

/// Backward-compat — types moved to `crate::discovery::discovery_indexer`.
pub mod discovery_indexer {
    pub use crate::discovery::discovery_indexer::*;
}

/// Backward-compat — types moved to `crate::store::store_file`.
pub mod store_file {
    pub use crate::store::store_file::*;
}

/// Backward-compat — types moved to `crate::store::store_sqlite`.
pub mod store_sqlite {
    pub use crate::store::store_sqlite::*;
}
