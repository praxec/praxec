//! CLI surface (`Cli` / `Command` + subcommands), config loading, overlay
//! wiring (`OverlayCtx` / `GatewayOverlays` / `OneshotServer`), store/audit
//! builders, and the small parsing/diagnostics helpers. Extracted from
//! `gateway.rs` via StructureOS `propose_decomposition` + `move` (#25).

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{
    AuditRetention, AuditSink, FileAuditSink, MemoryAuditSink, NullAuditSink, RotationInterval,
    StderrAuditSink,
};
use praxec_core::ports::{
    EvidenceStore, ExecutorRegistry, GuidanceAcknowledgmentStore, ScriptAcknowledgmentStore,
    WorkflowStore,
};
use praxec_core::store::{
    InMemoryEvidenceStore, InMemoryGuidanceAcknowledgmentStore, InMemoryScriptAcknowledgmentStore,
    InMemoryWorkflowStore, SqliteAckStore, SqliteEvidenceStore,
};
use praxec_core::store_file::FileWorkflowStore;
use praxec_core::store_sqlite::SqliteWorkflowStore;

use crate::gateway::{DiagnosticProvider, OverlayRegistrar};

// llm_overlay_registrar (gated on the optional llm-executor dep/feature).
#[cfg(feature = "llm-executor")]
use praxec_core::SingleKindOverlay;
#[cfg(feature = "llm-executor")]
use praxec_core::ports::Executor;

#[derive(Parser, Debug)]
#[command(
    name = "praxec",
    version,
    about = "Configurable MCP gateway with HATEOAS workflow governance"
)]
pub(crate) struct Cli {
    /// Log format: "text" (default) or "json".
    #[arg(long, default_value = "text", global = true)]
    pub(crate) log_format: String,

    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum Command {
    /// Run the MCP server over stdio.
    Serve {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Validate a config and print the resolved workflow definition ids.
    Check {
        #[arg(short, long)]
        config: PathBuf,
    },
    /// P15 — preflight this machine for a config: is every provider key the
    /// config's models reference resolvable (env / providers.env), and is every
    /// `kind: mcp` connection binary on PATH? Missing credentials fail (exit
    /// non-zero — nothing agentic could run); missing tools are warnings (they
    /// fail loud at invocation and only affect the steps that use them).
    Doctor {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Print a JSON health snapshot: connections, repos, definition_count, store.
    Health {
        #[arg(short, long)]
        config: PathBuf,
    },
    Observe {
        #[arg(short, long)]
        config: PathBuf,
        /// Stream events live: replay the audit dir (from the start, or
        /// `--since`), then poll for newly-appended lines and per-writer files.
        /// Emits each event as one structured JSON line so a client can
        /// reconstruct the execution tree from
        /// `workflow_id` + `parent_workflow_id` + `depth`. Requires
        /// `audit.sink: file` (fails fast otherwise — a non-file sink would tail
        /// nothing).
        #[arg(long)]
        follow: bool,
        /// With `--follow`, only replay events at/after this instant
        /// (RFC3339 or `YYYY-MM-DD`) before switching to live polling.
        #[arg(long)]
        since: Option<String>,
    },
    /// Rewrite legacy guards in the config file in place: every
    /// `kind: jsonpath` becomes `kind: expr` (a string replace over the
    /// raw file, which is overwritten). A no-op only if no such guards
    /// are present.
    Migrate {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Inspect a running workflow.
    Inspect {
        #[command(subcommand)]
        command: InspectCommand,
    },
    /// Inspect and tail audit events.
    Audit {
        #[command(subcommand)]
        command: AuditCommand,
    },
    /// Manage approval queues.
    #[command(name = "approvals")]
    Approvals {
        #[command(subcommand)]
        command: ApprovalsCommand,
    },
    /// ADR-0009 — drive a workflow instance to its outcomes headlessly: the
    /// orchestrator (an in-process agent) picks transitions toward the mission's
    /// outcomes until it resolves. No UI; HITL gates are answered by `--policy`.
    Orchestrate {
        /// Path to the gateway YAML config (the same store the mission lives in).
        #[arg(short, long)]
        config: PathBuf,
        /// Drive an existing workflow instance by id. (Use one of --workflow / --definition.)
        #[arg(short, long, conflicts_with = "definition")]
        workflow: Option<String>,
        /// Start a fresh instance of this definition, then drive it (auto-drive-on-start).
        #[arg(short, long, required_unless_present = "workflow")]
        definition: Option<String>,
        /// Launch input JSON for --definition (validated against the workflow's inputSchema).
        #[arg(long, default_value = "{}")]
        input: String,
        /// The orchestrator model, `provider:model-id` (e.g. anthropic:claude-...).
        #[arg(short, long)]
        model: String,
        /// Maximum drive steps before giving up.
        #[arg(long, default_value_t = 50)]
        max_steps: usize,
        /// How to answer HITL gates with no human present: `auto-approve` | `decline`.
        #[arg(long, default_value = "auto-approve")]
        policy: String,
    },
    /// Make one governed *command* contract call (start / submit / define /
    /// cancel) against the config's store and print the JSON response. State
    /// persists across calls when `store.kind: sqlite`. Argument is the same
    /// JSON the `praxec.command` MCP tool takes, e.g.
    /// `'{"definitionId":"hello_flow"}'` or
    /// `'{"workflowId":"wf_...","expectedVersion":1,"transition":"begin"}'`.
    Command {
        /// Path to the gateway YAML config (the store the workflow lives in).
        #[arg(short, long)]
        config: PathBuf,
        /// Run as a human principal (answers `actor: human` gates). Default: anonymous.
        #[arg(long)]
        human: bool,
        /// The contract-call arguments as a JSON object.
        args: String,
    },
    /// Make one *query* contract call (get a workflow, or search the lexicon /
    /// discovery) against the config's store and print the JSON response.
    /// Argument is the same JSON the `praxec.query` MCP tool takes, e.g.
    /// `'{"workflowId":"wf_..."}'` or `'{"query":"deploy"}'`.
    Query {
        #[arg(short, long)]
        config: PathBuf,
        #[arg(long)]
        human: bool,
        /// The query arguments as a JSON object.
        args: String,
    },
    /// Fuzz every workflow in a config with mock executors; report invariant
    /// violations (wedges, livelocks, engine errors). Exits non-zero on any.
    /// Pass `--live --model <provider:model>` to drive with the real executor
    /// registry and a real model chooser instead of mock executors.
    Fuzz {
        #[arg(short, long)]
        config: PathBuf,
        /// Number of iterations for the capped integration smoke (always run as exactly 1;
        /// coverage is per-transition graph-walk and is not iteration-driven).
        #[arg(long, default_value_t = 50)]
        iterations: usize,
        #[arg(long, default_value_t = 0)]
        seed: u64,
        #[arg(long, default_value = "text")]
        report: String,
        /// Drive with the real executor registry + a real model (requires --model).
        #[arg(long)]
        live: bool,
        /// The orchestrator model for --live, `provider:model-id`
        /// (e.g. anthropic:claude-haiku-4-5-20251001).
        #[arg(long)]
        model: Option<String>,
    },
    /// Run declared-property scenarios against a config and report pass/fail.
    Test {
        #[arg(short, long)]
        config: PathBuf,
        #[arg(long)]
        scenarios: PathBuf,
    },
    /// Report realized agent-step cost from the audit and the value prop.
    Cost {
        #[command(subcommand)]
        command: CostCommand,
    },
    /// Report which process (template) succeeds for which task-class, from the
    /// mission `outcome.recorded` telemetry (the intent index).
    Intent {
        #[command(subcommand)]
        command: IntentCommand,
    },
    /// Print the generated JSON Schema for a public wire type (code-first:
    /// the Rust struct is canonical; the schema is derived from it).
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
    /// Manage the config's top-level `connections:` block.
    Connections {
        #[command(subcommand)]
        command: ConnectionsCommand,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SchemaCommand {
    /// The structured audit event — the element type of the on-disk audit
    /// trail, `praxec observe --follow`, and the MCP `praxec.query
    /// { observe: true }` read. Includes the execution-tree linkage fields
    /// (`workflow_id`, `parent_workflow_id`, `depth`).
    AuditEvent,
}

/// D4a — the connection kind the operator names on `connections add --kind`.
/// A CLI-local mirror of [`praxec_executors::conn_write::ConnectionKind`] so the
/// clap surface stays typed (exhaustive `--kind` validation) without pulling
/// clap into the executors crate.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[value(rename_all = "lowercase")]
pub(crate) enum CliConnectionKind {
    Mcp,
    Cli,
    Rest,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConnectionsCommand {
    /// D4a — STAGE a new connection (ungranted) under `stagedConnections:`. This
    /// writes ONLY the connection body; it does NOT grant. A staged connection is
    /// inert — never in the live `/connections` registry — and is treated exactly
    /// like a pack-declared, not-yet-granted connection: any spawn attempt fails
    /// typed with the grant remedy. Granting is the separate, explicit
    /// `connections grant` act, so a non-human running `add` cannot silently
    /// obtain a trusted connection. Fail-fast on a duplicate name, an invalid
    /// kind/field combination, or an unwritable config — never a silent overwrite.
    Add {
        /// Path to the gateway YAML config to edit in place.
        #[arg(short, long)]
        config: PathBuf,
        /// The connection name, referenced by `executor.connection:`. Must be
        /// unique in the config and must not contain '/'.
        name: String,
        /// The connection kind.
        #[arg(long, value_enum)]
        kind: CliConnectionKind,
        /// Command to spawn — an mcp stdio server, or the cli command. (mcp/cli)
        #[arg(long)]
        command: Option<String>,
        /// A single command argument; repeat for each. (mcp) MCP server args
        /// often begin with '-' (e.g. `-y`), so hyphen-leading values are
        /// accepted here.
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        /// Endpoint URL — an mcp streamable-http server, or the rest base URL. (mcp/rest)
        #[arg(long)]
        url: Option<String>,
        /// Working directory for the command. (cli)
        #[arg(long)]
        working_directory: Option<String>,
        /// An environment entry `KEY=VALUE`; repeat for each. (mcp/cli)
        #[arg(long = "env")]
        env: Vec<String>,
        /// A request header `Name: value` (or `Name=value`); repeat for each. (rest)
        #[arg(long = "header")]
        headers: Vec<String>,
    },
    /// D4a — GRANT a previously-staged connection: the separate, explicit,
    /// auditable operator trust act. Adds the name to the top-level
    /// `grant_connections:` list so the config-load gate promotes the staged body
    /// into the live `/connections` registry, and records a `connections.granted`
    /// audit event. Fail-fast if the name is not staged or is already granted.
    Grant {
        /// Path to the gateway YAML config to edit in place.
        #[arg(short, long)]
        config: PathBuf,
        /// The staged connection to grant.
        name: String,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum InspectCommand {
    /// Show detailed information about a workflow instance.
    Workflow {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
        /// The workflow instance ID.
        id: String,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ApprovalsCommand {
    /// List pending approvals.
    List {
        /// Path to the gateway YAML config (to find the audit file).
        #[arg(short, long)]
        config: PathBuf,
        /// Show all approvals, including resolved ones.
        #[arg(long)]
        all: bool,
    },
    /// Resolve a pending approval by its audit event id.
    Resolve {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
        /// The audit event id of the approval to resolve.
        id: String,
        /// Resolution outcome (approved | rejected).
        #[arg(short, long, default_value = "approved")]
        outcome: String,
    },
    /// P12 R1.4 — resume a parked agent `await_human` session: deliver the
    /// human's reply to the exact parked frame by re-submitting the parked
    /// transition (as a human principal) with `arguments.reply`. The resumed
    /// agent continues from the awaited turn and the workflow advances on its
    /// result. Requires `store.kind: sqlite` and a stored audit sink.
    Resume {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
        /// The parked session's correlation id (see `approvals list`).
        id: String,
        /// The human's reply to the agent's `await_human` prompt.
        #[arg(short, long)]
        reply: String,
    },
    /// Tail the audit log for new approval requests.
    Tail {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum AuditCommand {
    /// Tail the audit log for new events.
    Tail {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
        /// Only show events matching this type (e.g. "human.approval.requested").
        #[arg(short, long)]
        filter: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum CostCommand {
    /// Aggregate realized agent-step cost from the audit (`agent.completed`
    /// telemetry) and surface the value prop: total / by-model / by-step cost,
    /// plus the counterfactual "saved Z% vs the ceiling model".
    Report {
        /// Path to the gateway YAML config (to find the audit store).
        #[arg(short, long)]
        config: PathBuf,
        /// Scope to a single workflow run by id.
        #[arg(long)]
        workflow: Option<String>,
        /// Scope to steps at/after this instant (YYYY-MM-DD or RFC3339).
        #[arg(long)]
        since: Option<String>,
        /// Emit JSON instead of the human-readable form.
        #[arg(long)]
        json: bool,
    },
    /// Propose governed base-model changes (the slow de-escalation loop): from
    /// the audit's per-(step, model) pass-rate + mean cost, propose lowering a
    /// base to a cheaper model that clears the bar with margin, or raising one
    /// that is chronically failing. Prints the `models.yaml` edit + evidence;
    /// never applies it. With `--request-approval`, files each proposal as a
    /// `human.approval.requested` event for the existing signoff gate.
    Propose {
        /// Path to the gateway YAML config (audit store + gateway.models_yaml).
        #[arg(short, long)]
        config: PathBuf,
        /// Emit JSON instead of the human-readable form.
        #[arg(long)]
        json: bool,
        /// Also record each proposal as a `human.approval.requested` event so it
        /// flows into `approvals list` / `approvals resolve`.
        #[arg(long)]
        request_approval: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum IntentCommand {
    /// Aggregate mission `outcome.recorded` telemetry into per-(task_class,
    /// template) success-rate + mean cost — the evidence the self-aware
    /// orchestrator ranks process choices over. Read-only; never selects.
    Report {
        /// Path to the gateway YAML config (to find the audit store).
        #[arg(short, long)]
        config: PathBuf,
        /// Scope to a single task-class.
        #[arg(long)]
        task_class: Option<String>,
        /// Emit JSON instead of the human-readable form.
        #[arg(long)]
        json: bool,
    },
}

/// The context a registrar receives. Fields are owned (cheap `Arc`/`Value`
/// clones) so registrars can be plain `Fn` closures with no lifetime plumbing.
pub struct OverlayCtx {
    /// The registry to delegate non-overlaid kinds to (the captured pre-wrap
    /// registry — never the swappable, so there is no lookup cycle).
    pub inner: Arc<dyn ExecutorRegistry>,
    /// The resolved gateway config (for reading e.g. `gateway.models_yaml`).
    pub config: Value,
    /// The gateway audit sink.
    pub audit: Arc<dyn AuditSink>,
    /// The live workflow runtime (the `kind: llm` executor's transition
    /// resolver is built from it).
    pub runtime: WorkflowRuntime,
}

/// The set of overlays the gateway binary hosts on top of the base registry.
/// The `praxec` binary supplies the `kind: llm` registrar (behind the
/// `llm-executor` feature) and the `kind: agent` registrar + doctor (behind
/// the `agents` feature).
#[derive(Default, Clone)]
pub struct GatewayOverlays {
    pub registrars: Vec<OverlayRegistrar>,
    pub diagnostics: Vec<DiagnosticProvider>,
}

/// Everything a one-shot CLI call (`command` / `query`) or the long-running
/// `serve` loop needs from a fully-wired gateway: the ready-to-dispatch
/// [`PraxecServer`] plus the handles `serve`'s SIGHUP loop swaps into. Built
/// once by [`build_oneshot_server`] so the server-construction stack lives in
/// exactly one place.
pub(crate) struct OneshotServer {
    pub(crate) server: praxec_mcp_server::PraxecServer,
    pub(crate) runtime: WorkflowRuntime,
    pub(crate) audit: Arc<dyn AuditSink>,
    pub(crate) swappable_defs: Arc<praxec_core::hot_reload::SwappableDefinitionStore>,
    pub(crate) swappable_executors: Arc<praxec_core::hot_reload::SwappableExecutorRegistry>,
    pub(crate) swappable_discovery: Arc<praxec_core::hot_reload::SwappableDiscoveryIndex>,
}

/// Apply every registrar to `base`, folding each result into the next, so the
/// overlays stack (last registrar is outermost). Used at startup and SIGHUP.
pub(crate) fn apply_overlays(
    base: Arc<dyn ExecutorRegistry>,
    config: &Value,
    audit: &Arc<dyn AuditSink>,
    runtime: &WorkflowRuntime,
    registrars: &[OverlayRegistrar],
) -> Arc<dyn ExecutorRegistry> {
    registrars.iter().fold(base, |inner, registrar| {
        registrar(OverlayCtx {
            inner,
            config: config.clone(),
            audit: audit.clone(),
            runtime: runtime.clone(),
        })
    })
}

pub(crate) fn load_config(path: &PathBuf) -> anyhow::Result<Value> {
    // Walks `include:` blocks, loads any declared `repos:` (namespace-prefixing
    // every definitionId), enforces the V20/V21/V22/V23 multi-repo invariants
    // (SPEC §9), then resolves `capabilities:` / `wraps` /
    // `executor: { capability: ... }` references into the inline shapes the
    // runtime expects. Soft diagnostics are discarded here; `check` uses the
    // diagnostics-returning variant.
    praxec_core::config::load_resolved_with_repos(path)
        .map(|(config, _diagnostics)| config)
        .with_context(|| format!("loading config {}", path.display()))
}

/// CMP-002 — the single load-time validation suite. Run by BOTH `check`
/// (advisory, prints everything) AND `serve` (enforcing, refuses to start on
/// errors), so a config that `check` would reject can never boot a live
/// gateway. validate_workflows + the executor-kind doctor are always present;
/// the cost / `kind: llm` config doctors are feature-gated (a build without
/// `llm-executor` can't host `kind: llm`, so they have nothing to inspect).
pub fn collect_diagnostics_with(
    config: &Value,
    extra: &[DiagnosticProvider],
) -> Vec<praxec_core::validate::Diagnostic> {
    let mut diagnostics = praxec_core::validate::validate_workflows(config);
    diagnostics.extend(praxec_executors::kind_doctor::doctor_check(config));
    #[cfg(feature = "llm-executor")]
    {
        use crate::affinity_resolver::{AgentsYamlAffinityResolver, resolve_affinity_to_model};

        let today = chrono::Utc::now().date_naive();

        // SPEC §33 D9 — when `gateway.models_yaml` is set, load the SAME
        // resolver the runtime uses and hand the cost doctor a SYNC
        // closure so affinity-resolved models are validated against the
        // cost catalog AT LOAD TIME (an uncatalogued affinity-resolved
        // model under a `max_cost_usd` cap becomes a load-time Error
        // rather than a soft Warning). A bad/absent models.yaml leaves
        // the closure as `None`, preserving the warn-only fallback (the
        // runtime F8 path still enforces).
        let affinity_loaded = config
            .pointer("/gateway/models_yaml")
            .and_then(Value::as_str)
            .and_then(|path| {
                AgentsYamlAffinityResolver::from_path(std::path::Path::new(path)).ok()
            });
        let resolve_closure = affinity_loaded
            .as_ref()
            .map(|loaded| move |a: &str| resolve_affinity_to_model(loaded.resolver(), a));
        let resolve_affinity = resolve_closure
            .as_ref()
            .map(|f| f as &dyn Fn(&str) -> Option<String>);

        diagnostics.extend(praxec_llm_executor::cost::doctor_check(
            config,
            today,
            resolve_affinity,
        ));
        diagnostics.extend(praxec_llm_executor::config_doctor::doctor_check(config));
    }
    // Caller-contributed doctors (e.g. the `kind: agent` config doctor),
    // so any overlaid kind is validated at load time too.
    for provider in extra {
        diagnostics.extend(provider(config));
    }
    diagnostics
}

/// Map the `--policy` flag to a headless HITL policy. Unknown values fall back to
/// `auto-approve` (the headless default).
pub(crate) fn headless_policy_from(s: &str) -> praxec_agents::orchestrator::HeadlessPolicy {
    use praxec_agents::orchestrator::HeadlessPolicy;
    match s {
        "decline" => HeadlessPolicy::Decline,
        _ => HeadlessPolicy::AutoApprove,
    }
}

/// A human principal for the CLI driver — `actor: human` transitions require the
/// `HUMAN_ROLE`. Default callers are anonymous (fail-closed).
pub(crate) fn cli_principal(human: bool) -> praxec_core::model::Principal {
    use praxec_core::model::Principal;
    if human {
        Principal {
            subject: "operator".into(),
            roles: vec![Principal::HUMAN_ROLE.into()],
            permissions: Vec::new(),
        }
    } else {
        Principal::anonymous()
    }
}

/// Resolve the discovery embedder from `config` + the persisted bootstrap
/// choice. Semantic discovery is an opt-in add-on; this returns
/// [`NoopEmbedder`](praxec_core::embeddings::NoopEmbedder) — free lexical
/// discovery, no network — when ANY of:
///   - `praxec.embeddings.enabled: false` — an explicit opt-out that skips the
///     persisted choice and its per-boot embedding calls entirely. A registered
///     model otherwise re-embeds the lexicon at EVERY boot, including each
///     one-shot `command` / `query`; this lets an operator keep the
///     registration but run offline / fast for a given config. Default (key
///     absent) is enabled, preserving prior behavior.
///   - no embedding model is registered (`load_choice` → `None`); or
///   - the registered model fails to build (unavailable / bad key).
///
/// Only an enabled, registered, buildable choice yields a live embedder.
pub(crate) fn resolve_embedder(
    config: &Value,
) -> Arc<dyn praxec_core::embeddings::EmbeddingProvider> {
    use praxec_core::embeddings::NoopEmbedder;

    let enabled = config
        .pointer("/praxec/embeddings/enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !enabled {
        tracing::info!(
            "semantic discovery disabled (praxec.embeddings.enabled=false) — using free \
             lexical discovery, no per-boot embedding calls"
        );
        return Arc::new(NoopEmbedder);
    }

    match praxec_embeddings::load_choice() {
        Some(choice) => match choice.build() {
            Ok(e) => {
                tracing::info!(
                    vendor = %choice.vendor,
                    model = %choice.model,
                    "embedding model registered — semantic discovery add-on enabled"
                );
                Arc::new(e)
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "embedding model configured but unavailable; staying on lexical discovery"
                );
                Arc::new(NoopEmbedder)
            }
        },
        None => Arc::new(NoopEmbedder),
    }
}

/// SPEC §33 D4/D9 — the in-runtime LLM executor as an [`OverlayRegistrar`].
/// Built from the [`OverlayCtx`]: `RuntimeTransitionResolver` needs the live
/// runtime (so the per-turn tool list mirrors the HATEOAS link set), the audit
/// sink, and (when `gateway.models_yaml` is set) the production affinity
/// resolver so `kind: llm` configs with an `affinity:` field resolve to a
/// concrete `provider:model-id`. A bad/absent models.yaml is logged at WARN and
/// leaves the executor's fail-loud default — affinity configs then fail loud
/// per-request, which is visible and recoverable.
///
/// The overlay delegates non-`llm` kinds to the captured `ctx.inner` (the
/// pre-wrap registry — never the swappable), so there is no lookup cycle.
#[cfg(feature = "llm-executor")]
pub fn llm_overlay_registrar() -> OverlayRegistrar {
    Arc::new(|ctx: OverlayCtx| {
        use crate::affinity_resolver::AgentsYamlAffinityResolver;
        use praxec_core::runtime_transition_resolver::RuntimeTransitionResolver;
        use praxec_llm_executor::LlmExecutor;

        let resolver = Arc::new(RuntimeTransitionResolver::new(ctx.runtime));
        let mut llm_executor = LlmExecutor::new(ctx.audit, resolver);

        if let Some(path) = ctx
            .config
            .pointer("/gateway/models_yaml")
            .and_then(Value::as_str)
        {
            match AgentsYamlAffinityResolver::from_path(std::path::Path::new(path)) {
                Ok(affinity) => {
                    tracing::info!(models_yaml = %path, "wired models.yaml affinity resolver");
                    llm_executor = llm_executor.with_affinity_resolver(Arc::new(affinity));
                }
                Err(err) => {
                    tracing::warn!(
                        models_yaml = %path,
                        error = %err,
                        "failed to load gateway.models_yaml; affinity resolution stays fail-loud"
                    );
                }
            }
        }

        let llm_executor: Arc<dyn Executor> = Arc::new(llm_executor);
        Arc::new(SingleKindOverlay::new(ctx.inner, "llm", llm_executor))
    })
}

/// H5 — scan the resolved config for use of the describe-ack guards and report
/// `(uses_guidance_acknowledged, uses_script_acknowledged)`. Walks every
/// transition's `guards:` array plus branch `when:` guards (the same two guard
/// sites `validate::validate_guard_kinds` checks). Used by `serve_with` to
/// assert the matching ack store is wired before booting (prevent → detect →
/// fail-fast), so a workflow that gates on a describe-ack can never start with a
/// permanently-unsatisfiable guard.
pub(crate) fn ack_guards_used(config: &Value) -> (bool, bool) {
    let mut guidance = false;
    let mut script = false;
    let visit_guard = |g: &Value, guidance: &mut bool, script: &mut bool| match g
        .get("kind")
        .and_then(Value::as_str)
    {
        Some("guidance_acknowledged") => *guidance = true,
        Some("script_acknowledged") => *script = true,
        _ => {}
    };
    if let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) {
        for def in workflows.values() {
            let Some(states) = def.get("states").and_then(Value::as_object) else {
                continue;
            };
            for state_def in states.values() {
                let Some(ts) = state_def.get("transitions").and_then(Value::as_object) else {
                    continue;
                };
                for t_def in ts.values() {
                    if let Some(guards) = t_def.get("guards").and_then(Value::as_array) {
                        for g in guards {
                            visit_guard(g, &mut guidance, &mut script);
                        }
                    }
                    if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                        for branch in branches {
                            if let Some(when) = branch.get("when") {
                                visit_guard(when, &mut guidance, &mut script);
                            }
                        }
                    }
                }
            }
        }
    }
    (guidance, script)
}

/// Parse a `--since` value: full RFC3339, or a bare `YYYY-MM-DD` (midnight UTC).
pub(crate) fn parse_since(s: &str) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
    use chrono::{DateTime, NaiveDate, TimeZone, Utc};
    if let Ok(dt) = s.parse::<DateTime<Utc>>() {
        return Ok(dt);
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let naive = d
            .and_hms_opt(0, 0, 0)
            .expect("invariant: midnight is a valid time");
        return Ok(Utc.from_utc_datetime(&naive));
    }
    anyhow::bail!(
        "--since must be RFC3339 (2026-06-20T00:00:00Z) or a date (2026-06-20), got '{s}'"
    )
}

/// Pick a `WorkflowStore` implementation from `store: { kind, path }` config.
/// Defaults to in-memory.
/// True if `path` lives on a conventionally-ephemeral filesystem whose contents
/// can vanish on reboot / OS cleanup — so a *durable* store placed there is a
/// silent data-loss trap. Conservative exact-or-prefix match on the well-known
/// temp roots; `/tmpfoo` is NOT under `/tmp`.
pub(crate) fn is_ephemeral_path(path: &str) -> bool {
    ["/tmp", "/var/tmp", "/dev/shm"]
        .iter()
        .any(|root| path == *root || path.starts_with(&format!("{root}/")))
}

/// Loudly warn when a persistent (`file`/`sqlite`) store is configured on an
/// ephemeral path. `serve` fails fast on this via [`guard_durable_serve`]; the
/// `command` path (used for scripted/CLI driving across many invocations) can't
/// fail-fast without breaking legitimate tempdir-backed tests, so it warns — the
/// failure mode that motivated this was a `/tmp`-backed harness driven by
/// repeated `command` calls, wiped on a `/tmp` cleanup mid-run.
pub(crate) fn warn_if_ephemeral_store(path: &str) {
    if is_ephemeral_path(path) {
        tracing::warn!(
            store_path = %path,
            "store.path is on an EPHEMERAL filesystem (/tmp, /var/tmp, /dev/shm); \
             durable workflow state will be LOST on OS cleanup or restart — use a \
             persistent path for any state meant to survive across invocations"
        );
    }
}

pub(crate) fn build_workflow_store(config: &Value) -> anyhow::Result<Arc<dyn WorkflowStore>> {
    let kind = config
        .pointer("/store/kind")
        .and_then(Value::as_str)
        .unwrap_or("memory");
    let path = config.pointer("/store/path").and_then(Value::as_str);

    match kind {
        "memory" => Ok(Arc::new(InMemoryWorkflowStore::new())),
        "file" => {
            let path =
                path.ok_or_else(|| anyhow::anyhow!("store.path is required when store.kind=file"))?;
            warn_if_ephemeral_store(path);
            Ok(Arc::new(FileWorkflowStore::new(path)?))
        }
        "sqlite" => {
            let path = path
                .ok_or_else(|| anyhow::anyhow!("store.path is required when store.kind=sqlite"))?;
            warn_if_ephemeral_store(path);
            Ok(Arc::new(SqliteWorkflowStore::open(path)?))
        }
        other => anyhow::bail!("unknown store kind '{other}'"),
    }
}

/// Build the evidence store from `config`. The only durable evidence backend is
/// `store.kind=sqlite` (a distinct `evidence` table coexisting in the same DB
/// file as the workflow store). `memory` and `file` get an ephemeral in-memory
/// store — `file` persists workflows but not governance side-state, a split that
/// [`guard_durable_serve`] refuses for a production `serve`. An unknown kind
/// fails fast, mirroring [`build_workflow_store`] — no silent fallback.
pub fn build_evidence_store(config: &Value) -> anyhow::Result<Arc<dyn EvidenceStore>> {
    let kind = config
        .pointer("/store/kind")
        .and_then(Value::as_str)
        .unwrap_or("memory");
    let path = config.pointer("/store/path").and_then(Value::as_str);

    match kind {
        "sqlite" => {
            let path = path
                .ok_or_else(|| anyhow::anyhow!("store.path is required when store.kind=sqlite"))?;
            Ok(Arc::new(SqliteEvidenceStore::open(path)?))
        }
        "memory" | "file" => Ok(Arc::new(InMemoryEvidenceStore::new())),
        other => anyhow::bail!("unknown store kind '{other}'"),
    }
}

/// Build the guidance-acknowledgment store from `config`. `store.kind=sqlite`
/// → durable `guidance_acks` table in the workflow DB; `memory`/`file` →
/// ephemeral in-memory (the durable/ephemeral split [`guard_durable_serve`]
/// refuses for `serve`). An unknown kind fails fast — no silent fallback.
pub fn build_guidance_ack_store(
    config: &Value,
) -> anyhow::Result<Arc<dyn GuidanceAcknowledgmentStore>> {
    let kind = config
        .pointer("/store/kind")
        .and_then(Value::as_str)
        .unwrap_or("memory");
    let path = config.pointer("/store/path").and_then(Value::as_str);

    match kind {
        "sqlite" => {
            let path = path
                .ok_or_else(|| anyhow::anyhow!("store.path is required when store.kind=sqlite"))?;
            Ok(Arc::new(SqliteAckStore::open(path, "guidance_acks")?))
        }
        "memory" | "file" => Ok(Arc::new(InMemoryGuidanceAcknowledgmentStore::new())),
        other => anyhow::bail!("unknown store kind '{other}'"),
    }
}

/// Build the script-acknowledgment store from `config`. `store.kind=sqlite`
/// → durable `script_acks` table in the workflow DB; `memory`/`file` →
/// ephemeral in-memory (the durable/ephemeral split [`guard_durable_serve`]
/// refuses for `serve`). An unknown kind fails fast — no silent fallback.
pub fn build_script_ack_store(
    config: &Value,
) -> anyhow::Result<Arc<dyn ScriptAcknowledgmentStore>> {
    let kind = config
        .pointer("/store/kind")
        .and_then(Value::as_str)
        .unwrap_or("memory");
    let path = config.pointer("/store/path").and_then(Value::as_str);

    match kind {
        "sqlite" => {
            let path = path
                .ok_or_else(|| anyhow::anyhow!("store.path is required when store.kind=sqlite"))?;
            Ok(Arc::new(SqliteAckStore::open(path, "script_acks")?))
        }
        "memory" | "file" => Ok(Arc::new(InMemoryScriptAcknowledgmentStore::new())),
        other => anyhow::bail!("unknown store kind '{other}'"),
    }
}

/// Build the parked-agent-session store from `config` (P12 R1.4 — the durable
/// half of agent `await_human` suspend/resume). The only durable backend is
/// `store.kind=sqlite` (a `parked_agent_sessions` table coexisting in the same
/// DB file as the workflow store — a parked conversation must survive a power
/// cycle). `memory`/`file` get `None`: the runner then fails fast on any
/// `await_enabled` session (AGENT_AWAIT_UNSUPPORTED) rather than parking a
/// frame that a restart would lose. An unknown kind fails fast, mirroring
/// [`build_workflow_store`].
pub fn build_parked_session_store(
    config: &Value,
) -> anyhow::Result<Option<Arc<dyn praxec_core::ports::ParkedSessionStore>>> {
    let kind = config
        .pointer("/store/kind")
        .and_then(Value::as_str)
        .unwrap_or("memory");
    let path = config.pointer("/store/path").and_then(Value::as_str);

    match kind {
        "sqlite" => {
            let path = path
                .ok_or_else(|| anyhow::anyhow!("store.path is required when store.kind=sqlite"))?;
            Ok(Some(Arc::new(
                praxec_core::store::SqliteParkedSessionStore::open(path)?,
            )))
        }
        "memory" | "file" => Ok(None),
        other => anyhow::bail!("unknown store kind '{other}'"),
    }
}

pub(crate) fn build_audit_sink(config: &Value) -> anyhow::Result<Arc<dyn AuditSink>> {
    let sink_kind = config
        .pointer("/audit/sink")
        .and_then(Value::as_str)
        .unwrap_or("stderr");

    // Poka-yoke: `audit.retention` prunes rotated files in `audit.path`, which
    // only the file sink writes. On any other sink it would be a silent no-op
    // knob — reject the config instead.
    if sink_kind != "file" && config.pointer("/audit/retention").is_some() {
        anyhow::bail!(
            "audit.retention requires `audit.sink: file` (current: `{sink_kind}`) — \
             retention prunes rotated files in audit.path, which only the file sink writes"
        );
    }

    let sink: Arc<dyn AuditSink> = match sink_kind {
        "stderr" => Arc::new(StderrAuditSink),
        "memory" => Arc::new(MemoryAuditSink::new()),
        "none" => Arc::new(NullAuditSink),
        "file" => {
            let path = config
                .pointer("/audit/path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("audit.path is required when audit.sink=file"))?;
            let rotation = parse_rotation_interval(config)?;
            let mut sink = FileAuditSink::new(path, rotation);
            if let Some(retention) = parse_audit_retention(config)? {
                sink = sink.with_retention(retention);
            }
            Arc::new(sink)
        }
        other => anyhow::bail!(
            "unknown audit sink '{other}' — valid values are: stderr, memory, file, none"
        ),
    };
    Ok(sink)
}

/// Parse `audit.rotation` from config; defaults to `Daily` when absent.
/// Accepts `"daily"` / `"hourly"` / `"weekly"` or the sub-daily granule
/// object `{ minutes: <1..=1440> }`. An unrecognized value is a config
/// ERROR (fail-fast) — it must not silently degrade to daily.
pub(crate) fn parse_rotation_interval(config: &Value) -> anyhow::Result<RotationInterval> {
    let Some(raw) = config.pointer("/audit/rotation") else {
        return Ok(RotationInterval::Daily);
    };
    let interval: RotationInterval = serde_json::from_value(raw.clone()).map_err(|e| {
        anyhow::anyhow!(
            "invalid audit.rotation {raw}: {e} — valid values: \"daily\", \"hourly\", \
             \"weekly\", or {{ minutes: <1..=1440> }}"
        )
    })?;
    if let RotationInterval::Minutes(m) = interval {
        anyhow::ensure!(
            m.get() <= 1440,
            "audit.rotation minutes must be 1..=1440 (got {m}); use daily/weekly for \
             coarser granules"
        );
    }
    Ok(interval)
}

/// Parse the opt-in `audit.retention` block (`{ keep_for_hours: <hours ≥ 1> }`).
/// Absent → `None` (nothing is ever deleted). Unknown keys or a zero window
/// are config ERRORS — a typo'd retention block must not silently keep
/// everything forever.
pub(crate) fn parse_audit_retention(config: &Value) -> anyhow::Result<Option<AuditRetention>> {
    let Some(raw) = config.pointer("/audit/retention") else {
        return Ok(None);
    };
    let retention: AuditRetention = serde_json::from_value(raw.clone()).map_err(|e| {
        anyhow::anyhow!(
            "invalid audit.retention {raw}: {e} — expected {{ keep_for_hours: <hours ≥ 1> }}"
        )
    })?;
    Ok(Some(retention))
}

#[cfg(test)]
mod audit_config_tests {
    use super::{build_audit_sink, parse_audit_retention, parse_rotation_interval};
    use praxec_core::audit::RotationInterval;
    use serde_json::json;

    #[test]
    fn rotation_defaults_to_daily_and_parses_named_granules() {
        assert_eq!(
            parse_rotation_interval(&json!({})).unwrap(),
            RotationInterval::Daily
        );
        assert_eq!(
            parse_rotation_interval(&json!({ "audit": { "rotation": "hourly" } })).unwrap(),
            RotationInterval::Hourly
        );
        assert_eq!(
            parse_rotation_interval(&json!({ "audit": { "rotation": "weekly" } })).unwrap(),
            RotationInterval::Weekly
        );
    }

    #[test]
    fn rotation_parses_sub_daily_minutes_granule() {
        let interval =
            parse_rotation_interval(&json!({ "audit": { "rotation": { "minutes": 5 } } }))
                .expect("minutes granule parses");
        assert_eq!(
            interval,
            RotationInterval::Minutes(std::num::NonZeroU32::new(5).unwrap())
        );
    }

    /// Fail-fast: an unrecognized/invalid rotation is a config ERROR, not a
    /// silent fall-through to daily (a typo'd granule must be visible).
    #[test]
    fn rotation_rejects_invalid_values_loudly() {
        for bad in [
            json!({ "audit": { "rotation": "fortnightly" } }),
            json!({ "audit": { "rotation": { "minutes": 0 } } }),
            json!({ "audit": { "rotation": { "minutes": 2000 } } }),
        ] {
            let err = parse_rotation_interval(&bad).expect_err("invalid rotation must error");
            assert!(
                err.to_string().contains("audit.rotation"),
                "message names the config key: {err}"
            );
        }
    }

    #[test]
    fn retention_is_opt_in_and_rejects_typos() {
        assert_eq!(parse_audit_retention(&json!({})).unwrap(), None);
        let retention =
            parse_audit_retention(&json!({ "audit": { "retention": { "keep_for_hours": 72 } } }))
                .expect("valid retention parses")
                .expect("present");
        assert_eq!(retention.keep_for_hours.get(), 72);

        // Unknown keys and a zero window are errors — a typo'd block must not
        // silently keep everything forever.
        for bad in [
            json!({ "audit": { "retention": { "keep_files": 5 } } }),
            json!({ "audit": { "retention": { "keep_for_hours": 0 } } }),
        ] {
            assert!(parse_audit_retention(&bad).is_err(), "must reject: {bad}");
        }
    }

    /// Poka-yoke: `audit.retention` on a non-file sink is a dead knob — the
    /// config is rejected instead of the retention silently never running.
    #[test]
    fn retention_on_non_file_sink_is_rejected() {
        let cfg = json!({ "audit": { "sink": "stderr", "retention": { "keep_for_hours": 24 } } });
        let err = match build_audit_sink(&cfg) {
            Ok(_) => panic!("retention on a non-file sink must be rejected"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("audit.retention"),
            "message names the offending key: {err}"
        );
    }

    /// The file sink builds with rotation granule + retention wired through.
    #[test]
    fn file_sink_builds_with_minutes_granule_and_retention() {
        let cfg = json!({
            "audit": {
                "sink": "file",
                "path": "/tmp/praxec-audit-test",
                "rotation": { "minutes": 15 },
                "retention": { "keep_for_hours": 48 }
            }
        });
        let sink = build_audit_sink(&cfg).expect("file sink builds");
        assert_eq!(sink.sink_kind(), "file");
    }
}

#[cfg(test)]
mod parked_store_wiring_tests {
    use super::build_parked_session_store;
    use praxec_core::model::ParkedAgentSession;
    use serde_json::json;

    /// P12 R1.4 wiring — `store.kind: sqlite` yields a REAL durable parked
    /// store on the same DB path the workflow store uses, and it round-trips
    /// a parked session (the bin-path integration assert).
    #[tokio::test]
    async fn sqlite_store_kind_wires_a_working_parked_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("praxec.db");
        let cfg = json!({ "store": { "kind": "sqlite", "path": db.to_str().unwrap() } });
        let store = build_parked_session_store(&cfg)
            .expect("sqlite parked store builds")
            .expect("sqlite yields Some(store)");
        store
            .park(ParkedAgentSession {
                correlation_id: "corr-wire".into(),
                prompt: "ship it?".into(),
                session: json!({}),
                conversation: json!({}),
                parked_at: chrono::Utc::now(),
            })
            .await
            .expect("park persists");
        let listed = store.list().await.expect("list works");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].correlation_id, "corr-wire");
        assert_eq!(listed[0].prompt, "ship it?");
    }

    /// `memory`/`file` stores are non-durable → no parked store (the runner
    /// then fails fast on `await_enabled` instead of parking losable frames).
    #[test]
    fn non_durable_store_kinds_yield_no_parked_store() {
        let mem = json!({ "store": { "kind": "memory" } });
        assert!(build_parked_session_store(&mem).unwrap().is_none());
        let file = json!({ "store": { "kind": "file", "path": "/tmp/x" } });
        assert!(build_parked_session_store(&file).unwrap().is_none());
    }

    /// sqlite without a path fails fast (mirrors `build_workflow_store`).
    #[test]
    fn sqlite_without_a_path_fails_fast() {
        let cfg = json!({ "store": { "kind": "sqlite" } });
        let err = match build_parked_session_store(&cfg) {
            Err(e) => e,
            Ok(_) => panic!("sqlite without a path must fail fast"),
        };
        assert!(err.to_string().contains("store.path is required"));
    }
}
