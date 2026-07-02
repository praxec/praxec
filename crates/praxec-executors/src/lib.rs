// T26 — restriction-category lint on production code only. See
// praxec-core/src/lib.rs for the rationale.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

//! Default executors for praxec.

mod arg_render;
pub mod cli;
mod conn_util;
pub mod diff;
pub mod dry_run;
pub mod human;
pub mod idle;
pub mod import;
pub mod ingest;
pub mod kind_doctor;
pub mod mcp;
pub mod noop;
pub mod parallel;
pub mod pipeline;
pub mod registry;
pub mod registry_executor;
pub mod rest;
pub mod script;
pub mod structural_analysis;
pub mod untrusted_execution;
pub mod workflow;

pub use cli::{CliConnection, CliConnections, CliExecutor};
pub use dry_run::DryRunExecutor;
pub use human::HumanExecutor;
pub use import::import_capabilities;
pub use ingest::IngestExecutor;
pub use mcp::{McpConnection, McpConnections, McpExecutor};
pub use noop::NoopExecutor;
pub use parallel::ParallelExecutor;
pub use pipeline::PipelineExecutor;

/// SPEC §24 GAP-E mitigation — the canonical list of executor kinds the
/// default registry builders wire in. Tooling (drift tests, schema
/// validators) reads this to assert parity against the JSON schema's
/// `executor.properties.kind.examples` array. Adding a new executor kind
/// means appending here AND to the schema in the SAME commit; the drift
/// test fails the build if they diverge.
///
/// The authoring executors (`registry`, `dry_run`, `structural_analysis`,
/// `ingest`) are wired into the default registry — `registry` is always present
/// but reports `WRITE_DISABLED` unless a writable definition store is injected
/// (`praxec.authoring.write_enabled`). `workflow` is registered runtime-less
/// in `default_registry_with_late_workflow` and gets its `WorkflowRuntime`
/// injected via `WorkflowExecutor::set_runtime` once the runtime (built around
/// the registry) exists. Because every kind here is actually dispatchable in the
/// production registry shape, this set == [`ALL_EXECUTOR_KINDS`], so the `check`
/// oracle is not lenient: a kind it accepts is a kind the runtime can run.
pub const REGISTERED_EXECUTOR_KINDS: &[&str] = &[
    "cli",
    "human",
    "llm",
    // `agent` (like `llm`) is added by the binary's overlay, not the default
    // registry — listed here so `check` recognizes it.
    "agent",
    "mcp",
    "noop",
    "parallel",
    "pipeline",
    "rest",
    "script",
    // `workflow` is registered runtime-less; `set_runtime` late-binds it.
    "workflow",
    "registry",
    "dry_run",
    "structural_analysis",
    "ingest",
    "diff",
];

/// Every executor `kind` the codebase implements. This is now identical to
/// [`REGISTERED_EXECUTOR_KINDS`] — every kind the codebase implements is also
/// wired into the default registry (the authoring executors via
/// [`with_authoring_executors`], `workflow` via the late-bound `set_runtime`
/// handle). The two constants are kept distinct so a future kind that is
/// implemented but deliberately *not* wired can be added here without the
/// `kind_doctor` accepting it as dispatchable.
///
/// This is the oracle for load-time kind validation ([`kind_doctor`]): a
/// `kind` outside this set is a typo or an unsupported executor. (`import` is a
/// startup proxy mechanism, and `expression` a parallel-aggregator join
/// sub-kind — neither is a transition executor kind, so neither appears here.)
///
/// Keep in sync when adding an executor; `kind_doctor`'s tests assert that
/// `REGISTERED_EXECUTOR_KINDS` is a subset of this list.
pub const ALL_EXECUTOR_KINDS: &[&str] = &[
    // Mirrors REGISTERED_EXECUTOR_KINDS — every kind here is dispatchable.
    "cli",
    "human",
    "llm",
    "agent",
    "mcp",
    "noop",
    "parallel",
    "pipeline",
    "rest",
    "script",
    "workflow",
    // Authoring-time executors — wired into the default registry via
    // `with_authoring_executors` (`registry` in its disabled, WRITE_DISABLED
    // form unless `praxec.authoring.write_enabled` overlays the enabled one).
    "registry",
    "dry_run",
    "structural_analysis",
    "ingest",
    "diff",
];
pub use registry::HashMapExecutorRegistry;
pub use registry_executor::RegistryExecutor;
pub use rest::{RestConnection, RestConnections, RestExecutor};
pub use script::ScriptExecutor;
pub use structural_analysis::{StructuralAnalysisExecutor, REQUIRED_RULES};
pub use workflow::WorkflowExecutor;

use std::sync::Arc;

use praxec_core::ports::ExecutorRegistry;
use serde_json::Value;

/// Build a registry containing the default executor set wired up against the
/// given config. Convenient one-shot entry point for the binary.
pub fn default_registry(config: &Value) -> Arc<dyn ExecutorRegistry> {
    let cli_conns = Arc::new(CliConnections::from_config(config));
    let mcp_conns = McpConnections::from_config(config);
    default_registry_with_mcp(
        config,
        Arc::new(McpExecutor::new(mcp_conns)),
        cli_conns,
        Arc::new(praxec_core::audit::NullAuditSink),
    )
}

/// Same as `default_registry` but lets the caller supply pre-built CLI and
/// MCP executors and an audit sink. Useful when you want to share the MCP
/// executor with the importer (so the connection cache is reused) and route
/// human-approval audit events to the gateway's main audit stream — see the
/// `praxec` binary for the canonical wiring.
pub fn default_registry_with_mcp(
    config: &Value,
    mcp_executor: Arc<McpExecutor>,
    cli_connections: Arc<CliConnections>,
    audit: Arc<dyn praxec_core::audit::AuditSink>,
) -> Arc<dyn ExecutorRegistry> {
    // Production callers that need to drive `kind: workflow` sub-workflows use
    // [`default_registry_with_late_workflow`] and call `set_runtime` once the
    // runtime exists. This convenience wrapper discards the handle for callers
    // (tests, tooling) that don't spawn sub-workflows.
    default_registry_with_late_workflow(config, mcp_executor, cli_connections, audit).0
}

/// Same as [`default_registry_with_mcp`] but also registers a runtime-less
/// `workflow` executor and returns its handle so the binary can inject the
/// `WorkflowRuntime` via [`WorkflowExecutor::set_runtime`] once the runtime —
/// which is built *around* this registry — exists.
///
/// This resolves the construction cycle (`WorkflowExecutor` → `WorkflowRuntime`
/// → registry → `WorkflowExecutor`) the SAME way `ParallelExecutor::set_registry`
/// does for `parallel`: register an unwired executor, then late-bind. Because
/// `workflow` is now in the production registry, every kind in
/// [`REGISTERED_EXECUTOR_KINDS`] is genuinely dispatchable.
pub fn default_registry_with_late_workflow(
    config: &Value,
    mcp_executor: Arc<McpExecutor>,
    cli_connections: Arc<CliConnections>,
    audit: Arc<dyn praxec_core::audit::AuditSink>,
) -> (Arc<dyn ExecutorRegistry>, Arc<WorkflowExecutor>) {
    let rest_connections = Arc::new(RestConnections::from_config(config));
    // SPEC §24 — `ParallelExecutor` needs a back-reference to the registry
    // so its branches can invoke other executors. Construct first, register
    // with a clone, then wire the registry back into the parallel executor
    // after the registry Arc exists.
    let parallel = Arc::new(ParallelExecutor::new(audit.clone()));
    let pipeline = Arc::new(PipelineExecutor::new(audit.clone()));
    // `workflow` is registered runtime-less here; the runtime is injected via
    // `set_runtime` after it has been built around this registry.
    let workflow = Arc::new(WorkflowExecutor::late(audit.clone()));
    let registry = HashMapExecutorRegistry::new()
        .with("cli", Arc::new(CliExecutor::new(cli_connections)))
        .with("mcp", mcp_executor as Arc<dyn praxec_core::ports::Executor>)
        .with("rest", Arc::new(RestExecutor::new(rest_connections)))
        .with("human", Arc::new(HumanExecutor::with_audit(audit)))
        .with("noop", Arc::new(NoopExecutor))
        .with("script", Arc::new(ScriptExecutor::new()))
        .with(
            "workflow",
            workflow.clone() as Arc<dyn praxec_core::ports::Executor>,
        )
        .with(
            "parallel",
            parallel.clone() as Arc<dyn praxec_core::ports::Executor>,
        )
        .with(
            "pipeline",
            pipeline.clone() as Arc<dyn praxec_core::ports::Executor>,
        );
    let registry = with_authoring_executors(registry);

    let registry: Arc<dyn ExecutorRegistry> = Arc::new(registry);
    parallel.set_registry(registry.clone());
    pipeline.set_registry(registry.clone());
    (registry, workflow)
}

/// Add the authoring executors (SPEC §17–19) to a registry. `dry_run`,
/// `structural_analysis`, and `ingest` are self-contained; `registry` is wired
/// in its disabled form (reports `WRITE_DISABLED`) so the `kind` always
/// dispatches — the binary overlays an enabled `RegistryExecutor` backed by a
/// writable definition store when `praxec.authoring.write_enabled` is set.
fn with_authoring_executors(registry: HashMapExecutorRegistry) -> HashMapExecutorRegistry {
    registry
        .with("dry_run", Arc::new(crate::dry_run::DryRunExecutor))
        .with("structural_analysis", Arc::new(StructuralAnalysisExecutor))
        .with("ingest", Arc::new(crate::ingest::IngestExecutor))
        .with("diff", Arc::new(crate::diff::DiffExecutor))
        .with("registry", Arc::new(RegistryExecutor::disabled()))
}
