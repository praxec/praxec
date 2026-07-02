use std::path::PathBuf;
use std::sync::Arc;

pub(crate) use crate::gateway_config::{
    ack_guards_used, apply_overlays, build_audit_sink, build_workflow_store, cli_principal,
    headless_policy_from, is_ephemeral_path, load_config, parse_since, resolve_embedder,
    ApprovalsCommand, AuditCommand, Cli, Command, CostCommand, InspectCommand, IntentCommand,
    OneshotServer,
};
pub use crate::gateway_config::{
    build_evidence_store, build_guidance_ack_store, build_script_ack_store,
    collect_diagnostics_with, GatewayOverlays, OverlayCtx,
};
// `llm_overlay_registrar` is gated on the optional llm-executor feature; its
// only caller (main.rs) is also gated, so the re-export must carry the same
// cfg or the lean `--no-default-features` build fails to resolve it (E0432).
#[cfg(feature = "llm-executor")]
pub use crate::gateway_config::llm_overlay_registrar;
use anyhow::Context;
use clap::Parser;
use praxec_core::capability::CapabilityRegistry;
use praxec_core::discovery::{
    DiscoveryIndex, DiscoveryKind, InMemoryDiscoveryIndex, SemanticDiscoveryIndex,
};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::ports::{
    EvidenceStore, ExecutorRegistry, GuidanceAcknowledgmentStore, ScriptAcknowledgmentStore,
};
use praxec_core::sandbox::{BwrapProvider, OciProvider, SandboxProvider};
use praxec_core::store::ConfigDefinitionStore;
use praxec_core::SingleKindOverlay;
use praxec_core::WorkflowRuntime;
use praxec_executors::{
    default_registry_with_late_workflow, import_capabilities, CliConnections, McpConnections,
    McpExecutor, RegistryExecutor, ScriptExecutor,
};
use praxec_mcp_server::PraxecServer;
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tracing_subscriber::EnvFilter;

// ── Overlay seam ─────────────────────────────────────────────────────────────
//
// Executors that need post-startup context (the `kind: llm` executor needs the
// live `WorkflowRuntime`; the `kind: agent` executor needs a model
// resolver) are registered as overlays onto the base registry. The caller
// supplies a list of registrars; the gateway applies them — in order, each
// wrapping the previous — BOTH at startup and on every SIGHUP rebuild, so a
// reload never silently drops the overlaid kinds.

/// Wraps an inner registry with one more executor kind. Returns the new
/// registry (typically a [`SingleKindOverlay`]).
pub type OverlayRegistrar = Arc<dyn Fn(OverlayCtx) -> Arc<dyn ExecutorRegistry> + Send + Sync>;

/// Contributes load-time diagnostics for a kind the caller hosts (e.g. the
/// `kind: agent` config doctor).
pub type DiagnosticProvider =
    Arc<dyn Fn(&Value) -> Vec<praxec_core::validate::Diagnostic> + Send + Sync>;

/// Shared CLI entry point. The `praxec` binary calls this with the
/// `kind: llm` overlay (llm-executor feature) and `kind: agent` overlay
/// (agents feature) already registered in `overlays`.
pub async fn run_cli(overlays: GatewayOverlays) -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log_format);

    match cli.command {
        Command::Serve { config } => serve_with(config, overlays).await,
        Command::Orchestrate {
            config,
            workflow,
            definition,
            input,
            model,
            max_steps,
            policy,
        } => {
            orchestrate(
                config, workflow, definition, input, model, max_steps, policy, overlays,
            )
            .await
        }
        Command::Command {
            config,
            human,
            args,
        } => run_command(config, human, args, overlays).await,
        Command::Query {
            config,
            human,
            args,
        } => run_query(config, human, args, overlays).await,
        Command::Check { config } => check(config, &overlays.diagnostics),
        Command::Health { config } => health(config),
        Command::Observe { config } => observe(config),
        Command::Migrate { config } => migrate(config),
        Command::Inspect { command } => match command {
            InspectCommand::Workflow { config, id } => inspect_workflow(&config, &id).await,
        },
        Command::Audit { command } => match command {
            AuditCommand::Tail { config, filter } => audit_tail(&config, &filter),
        },
        Command::Approvals { command } => match command {
            ApprovalsCommand::List { config, all } => approvals_list(&config, all).await,
            ApprovalsCommand::Resolve {
                config,
                id,
                outcome,
            } => approvals_resolve(&config, &id, &outcome).await,
            ApprovalsCommand::Tail { config } => approvals_tail(&config),
        },
        Command::Fuzz {
            config,
            iterations,
            seed,
            report,
            live,
            model,
        } => fuzz_cmd(config, iterations, seed, report, live, model, overlays).await,
        Command::Test { config, scenarios } => test_cmd(config, scenarios).await,
        Command::Cost { command } => match command {
            CostCommand::Report {
                config,
                workflow,
                since,
                json,
            } => cost_report_cmd(&config, workflow, since, json).await,
            CostCommand::Propose {
                config,
                json,
                request_approval,
            } => cost_propose_cmd(&config, json, request_approval).await,
        },
        Command::Intent { command } => match command {
            IntentCommand::Report {
                config,
                task_class,
                json,
            } => intent_report_cmd(&config, task_class, json).await,
        },
    }
}

fn init_tracing(log_format: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match log_format {
        "json" => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .json()
                .try_init();
        }
        _ => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .try_init();
        }
    }
}

/// Production safety guard — refuse to start a long-running `serve` on an
/// EPHEMERAL store (state lost on restart) or a NON-DURABLE / non-retrievable
/// audit sink (the governance trail can't be queried), unless the operator
/// EXPLICITLY opts in via `gateway.allow_ephemeral: true` or the env
/// `PRAXEC_ALLOW_EPHEMERAL`.
///
/// This turns the "don't ship with the memory store / stderr audit sink"
/// documentation prerequisite into a fail-fast. A one-shot ephemeral gateway
/// child (e.g. spawned by an agentic harness) sets the env var, so
/// interactive/dev use is unaffected — only a deliberate `serve` with
/// non-durable storage is stopped.
fn guard_durable_serve(config: &Value) -> anyhow::Result<()> {
    let env_opt_in = std::env::var("PRAXEC_ALLOW_EPHEMERAL")
        .map(|v| !matches!(v.as_str(), "" | "0" | "false"))
        .unwrap_or(false);
    let config_opt_in = config
        .pointer("/gateway/allow_ephemeral")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if env_opt_in || config_opt_in {
        return Ok(());
    }

    // Absent store/audit config defaults to memory/stderr respectively — both
    // ephemeral, so treat "unset" the same as the ephemeral value.
    let store_kind = config
        .pointer("/store/kind")
        .and_then(Value::as_str)
        .unwrap_or("memory");
    let audit_sink = config
        .pointer("/audit/sink")
        .and_then(Value::as_str)
        .unwrap_or("stderr");

    let mut problems = Vec::new();
    if store_kind == "memory" {
        problems.push(format!(
            "store.kind is '{store_kind}' — workflow state is in-memory and lost on \
             restart; set store.kind: file | sqlite"
        ));
    }
    if matches!(audit_sink, "stderr" | "none" | "memory") {
        problems.push(format!(
            "audit.sink is '{audit_sink}' — the governance audit trail is not durably \
             retained / queryable; set audit.sink: file"
        ));
    }
    // Durability invariant: the only durable evidence/acknowledgment backend is
    // sqlite (build_evidence_store / build_*_ack_store). A `file` workflow store
    // persists workflows but leaves governance side-state in memory — a
    // durable/ephemeral split that silently loses evidence and acknowledgments on
    // restart. Refuse it; sqlite carries all of it in one DB file.
    if store_kind == "file" {
        problems.push(format!(
            "store.kind is '{store_kind}' — workflows persist, but evidence and \
             acknowledgments are in-memory and lost on restart; set store.kind: \
             sqlite for durable governance state"
        ));
    }
    // Durability invariant (state LOCATION): a file/sqlite store on an ephemeral
    // filesystem (/tmp, /var/tmp, /dev/shm) is silently wiped on OS cleanup or
    // reboot — the exact failure that motivated this check (a /tmp-backed harness
    // lost a live workflow instance on a /tmp cleanup). Refuse it for serve.
    if matches!(store_kind, "file" | "sqlite") {
        if let Some(p) = config.pointer("/store/path").and_then(Value::as_str) {
            if is_ephemeral_path(p) {
                problems.push(format!(
                    "store.path '{p}' is on an ephemeral filesystem (/tmp, /var/tmp, \
                     /dev/shm) — durable workflow state is silently lost on OS cleanup or \
                     restart; use a persistent path"
                ));
            }
        }
    }

    if !problems.is_empty() {
        anyhow::bail!(
            "refusing to serve with ephemeral / non-durable storage:\n  - {}\n\n\
             Configure durable storage for production, or set \
             `gateway.allow_ephemeral: true` (or env PRAXEC_ALLOW_EPHEMERAL=1) to \
             override for dev/testing.",
            problems.join("\n  - ")
        );
    }
    Ok(())
}

/// ADR-0009 — drive a workflow instance to its outcomes headlessly. Builds the
/// same overlaid runtime `serve` uses (so `kind: agent`/`llm` steps execute),
/// then runs [`drive_mission`] with an in-process [`AgentChooser`] + bus +
/// headless consumer. No server, no UI; HITL gates are answered by `policy`.
#[allow(clippy::too_many_arguments)]
async fn orchestrate(
    config_path: PathBuf,
    workflow: Option<String>,
    definition: Option<String>,
    input: String,
    model: String,
    max_steps: usize,
    policy: String,
    overlays: GatewayOverlays,
) -> anyhow::Result<()> {
    use praxec_agents::orchestrator::{
        drive_mission, run_headless_consumer, AgentChooser, DriveOutcome, MissionGateway,
        RuntimeMissionGateway,
    };
    use praxec_agents::rig_runner::RigSessionRunner;
    use praxec_agents::session::AgentSessionRunner;
    use praxec_core::bus::Bus;
    use praxec_core::model::{Principal, StartWorkflow};

    let config = load_config(&config_path)?;
    let runtime = build_runtime_for_orchestrate(&config, &overlays).await?;

    // Resolve the mission: drive an existing instance, or START a fresh one from a
    // definition and drive it (auto-drive-on-start).
    let mission_id = match (workflow, definition) {
        (Some(id), _) => id,
        (None, Some(def)) => {
            let input_value: Value = serde_json::from_str(&input)
                .map_err(|e| anyhow::anyhow!("--input is not valid JSON: {e}"))?;
            let resp = runtime
                .start(StartWorkflow {
                    definition_id: def.clone(),
                    input: input_value,
                    principal: Principal::anonymous(),
                    trace_id: None,
                    run_id: None,
                    depth: 0,
                    parent: None,
                })
                .await?;
            let id = resp
                .pointer("/workflow/id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    anyhow::anyhow!("started '{def}' but no workflow id was returned")
                })?;
            println!("orchestrate: started {def} → {id}");
            id
        }
        (None, None) => anyhow::bail!("orchestrate needs --workflow <id> or --definition <id>"),
    };

    let gateway = RuntimeMissionGateway::new(runtime, Principal::anonymous());
    let runner: Arc<dyn AgentSessionRunner> = Arc::new(RigSessionRunner::with_default_provider());
    let chooser = AgentChooser::new(runner, model, std::time::Duration::from_secs(120));

    let bus = Bus::new();
    let events = bus.subscribe();
    let consumer = tokio::spawn(run_headless_consumer(
        events,
        bus.clone(),
        headless_policy_from(&policy),
    ));

    let outcome = drive_mission(&gateway, &chooser, &bus, &mission_id, max_steps).await;
    consumer.abort();

    // On a give-up, re-query for a diagnostic snapshot of WHERE it stalled — the
    // bare `GaveUp` is otherwise opaque (the common cause is pointing the agentic
    // driver at a deterministic / human-gated flow with no agent-actionable move).
    let stall_detail = if matches!(outcome, DriveOutcome::GaveUp) {
        Some(match gateway.query(&mission_id).await {
            Ok(s) => format!(
                "stalled at status `{}`; legal actions: [{}]",
                s.status,
                s.legal_actions
                    .iter()
                    .map(|a| a.transition.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(e) => format!("a follow-up state query also failed: {e}"),
        })
    } else {
        None
    };
    drive_outcome_to_result(&mission_id, max_steps, outcome, stall_detail)
}

/// Map a finished drive to a process result. A succeeded mission exits 0; every
/// non-success outcome returns an `Err` (→ printed to stderr by `main`, non-zero
/// exit) so a silent `GaveUp` can no longer masquerade as success (exit 0, no
/// diagnostics). `GaveUp` additionally explains that `orchestrate` is the
/// agentic *mission* driver and points at the deterministic alternatives.
fn drive_outcome_to_result(
    mission_id: &str,
    max_steps: usize,
    outcome: praxec_agents::orchestrator::DriveOutcome,
    stall_detail: Option<String>,
) -> anyhow::Result<()> {
    use praxec_agents::orchestrator::DriveOutcome;
    match outcome {
        DriveOutcome::Resolved { status, .. } if status == "succeeded" => {
            println!("orchestrate {mission_id}: succeeded");
            Ok(())
        }
        DriveOutcome::Resolved { status, reason } => anyhow::bail!(
            "orchestrate {mission_id}: resolved `{status}`{}",
            reason.map(|r| format!(" — {r}")).unwrap_or_default()
        ),
        DriveOutcome::GaveUp => anyhow::bail!(
            "orchestrate {mission_id}: the agentic driver found no actionable move and gave up{}.\n\
             orchestrate steers a *mission* toward its declared `outcomes` via agent-actionable \
             transitions; a deterministic or human-gated flow offers the driver no such move. \
             Drive it instead with `praxec command '<json>'` / `query '<json>'` \
             (step-by-step) or `px walk` (end-to-end).",
            stall_detail.map(|d| format!(" ({d})")).unwrap_or_default()
        ),
        DriveOutcome::Declined => anyhow::bail!(
            "orchestrate {mission_id}: a HITL gate was declined by the headless policy."
        ),
        DriveOutcome::MaxSteps => anyhow::bail!(
            "orchestrate {mission_id}: hit the {max_steps}-step bound without resolving."
        ),
        DriveOutcome::Error(e) => {
            anyhow::bail!("orchestrate {mission_id}: drive error — {e}")
        }
    }
}

async fn run_command(
    config_path: PathBuf,
    human: bool,
    args: String,
    overlays: GatewayOverlays,
) -> anyhow::Result<()> {
    let config = load_config(&config_path)?;
    let bundle = build_oneshot_server(&config, &overlays).await?;
    let args: Value = serde_json::from_str(&args)
        .map_err(|e| anyhow::anyhow!("argument is not valid JSON: {e}"))?;
    let resp = bundle
        .server
        .dispatch_command(args, cli_principal(human))
        .await?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

async fn run_query(
    config_path: PathBuf,
    human: bool,
    args: String,
    overlays: GatewayOverlays,
) -> anyhow::Result<()> {
    let config = load_config(&config_path)?;
    let bundle = build_oneshot_server(&config, &overlays).await?;
    let args: Value = serde_json::from_str(&args)
        .map_err(|e| anyhow::anyhow!("argument is not valid JSON: {e}"))?;
    let resp = bundle
        .server
        .dispatch_query(args, cli_principal(human))
        .await?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

/// Build a static (non-hot-reload) overlaid runtime for a one-shot orchestrate
/// run — the same executor/guard/store/overlay stack `serve` builds, minus the
/// server and SIGHUP hot-reload.
async fn build_runtime_for_orchestrate(
    config: &Value,
    overlays: &GatewayOverlays,
) -> anyhow::Result<Arc<WorkflowRuntime>> {
    use praxec_core::hot_reload::{SwappableDefinitionStore, SwappableExecutorRegistry};

    let diagnostics = collect_diagnostics_with(config, &overlays.diagnostics);
    let error_count = diagnostics.iter().filter(|d| d.is_error()).count();
    if error_count > 0 {
        for d in &diagnostics {
            eprintln!("  {d}");
        }
        anyhow::bail!(
            "refusing to orchestrate: config validation failed with {error_count} error(s)"
        );
    }

    let audit = build_audit_sink(config)?;
    let (initial_defs, initial_executors, _discovery, workflow_handle) =
        build_hot_components(config, &audit).await?;
    let swappable_defs = Arc::new(SwappableDefinitionStore::new(initial_defs));
    let swappable_executors = Arc::new(SwappableExecutorRegistry::new(initial_executors));
    let store = build_workflow_store(config)?;
    let evidence: Arc<dyn EvidenceStore> = build_evidence_store(config)?;
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let repo_locks: Arc<dyn praxec_core::repo_locks::RepoLocks> =
        Arc::new(praxec_core::repo_locks::RepoLockSpace::new());
    let lock_scheduler = Arc::new(praxec_core::lock_scheduler::LockScheduler::new());
    let runtime = WorkflowRuntime::new(
        swappable_defs.clone() as Arc<dyn praxec_core::ports::DefinitionStore>,
        store,
        swappable_executors.clone() as Arc<dyn praxec_core::ports::ExecutorRegistry>,
        guards,
        audit.clone(),
    )
    .with_evidence(evidence)
    .with_repo_locks(repo_locks)
    .with_lock_scheduler(lock_scheduler)
    .with_auto_drive_agents(
        config
            .pointer("/praxec/agents/auto_drive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        config
            .pointer("/praxec/agents/auto_drive_affinity")
            .and_then(Value::as_str)
            .unwrap_or("reasoning"),
        {
            // Auto-drive tool set = every wired connection PLUS any operator
            // `praxec.agents.auto_drive_tools` entries (data-not-code). The
            // latter carries e.g. `file:{{ $.workflow.input.repo_path }}` —
            // templated per-leaf by AgentExecutor; non-coding leaves (no
            // repo_path) get no file tools (CompositeToolHost guard).
            let mut t: Vec<String> = config
                .pointer("/connections")
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            if let Some(extra) = config
                .pointer("/praxec/agents/auto_drive_tools")
                .and_then(Value::as_array)
            {
                t.extend(extra.iter().filter_map(|v| v.as_str().map(str::to_string)));
            }
            t
        },
        config
            .pointer("/praxec/agents/auto_drive_max_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    // C1 — late-bind the runtime into the `kind: workflow` executor.
    workflow_handle.set_runtime(runtime.clone());
    let overlaid = apply_overlays(
        swappable_executors.current(),
        config,
        &audit,
        &runtime,
        &overlays.registrars,
    );
    swappable_executors.swap(overlaid);
    Ok(Arc::new(runtime))
}

/// Build a fully-wired [`PraxecServer`] from `config` — the same executor /
/// guard / store / overlay / lexicon / discovery / embedder / principal stack
/// `serve` builds, minus the stdio transport and SIGHUP loop. Shared by
/// `serve_with` (which then serves + hot-reloads) and the one-shot `command` /
/// `query` subcommands (which dispatch a single call). Runs the same load-time
/// diagnostics gate as `serve`/`orchestrate` (refuses on config errors).
async fn build_oneshot_server(
    config: &Value,
    overlays: &GatewayOverlays,
) -> anyhow::Result<OneshotServer> {
    use praxec_core::hot_reload::{
        SwappableDefinitionStore, SwappableDiscoveryIndex, SwappableExecutorRegistry,
    };

    // CMP-002 — enforce the validation suite on the SERVING path, not just at
    // `praxec check`. An unknown executor kind, an unpinned stable cap, a
    // `tools:` injection, or any other Diagnostic::Error must refuse to boot
    // rather than fail mid-flight once a workflow is already running.
    let diagnostics = collect_diagnostics_with(config, &overlays.diagnostics);
    let error_count = diagnostics.iter().filter(|d| d.is_error()).count();
    if error_count > 0 {
        for d in &diagnostics {
            eprintln!("  {d}");
        }
        anyhow::bail!(
            "refusing to start: config validation failed with {error_count} error(s) — \
             run `praxec check --config <path>` for the full report"
        );
    }

    // #18 — wrap the configured audit sink so events recorded during a drive
    // ALSO push to the connected MCP client as logging notifications. The shared
    // `progress_peer` slot is handed to the PraxecServer below, which captures
    // the live peer per call. Durable audit is unchanged (the bridge delegates).
    let (audit, progress_peer) = praxec_mcp_server::progress_bridge(build_audit_sink(config)?);

    let (initial_defs, initial_executors, initial_discovery, workflow_handle) =
        build_hot_components(config, &audit).await?;

    let swappable_defs = Arc::new(SwappableDefinitionStore::new(initial_defs));
    let swappable_executors = Arc::new(SwappableExecutorRegistry::new(initial_executors));
    let swappable_discovery = Arc::new(SwappableDiscoveryIndex::new(initial_discovery));

    let store = build_workflow_store(config)?;
    // Durable when store.kind=sqlite (evidence lives in a distinct `evidence`
    // table inside the SAME sqlite DB as the workflow store); in-memory
    // otherwise. So evidence survives a restart and a sqlite-backed gateway
    // can satisfy evidence-gated guards across a process boundary.
    let evidence: Arc<dyn EvidenceStore> = build_evidence_store(config)?;

    // H5 — ack stores for the `guidance_acknowledged` / `script_acknowledged`
    // guards. The SAME Arc must reach BOTH the guard evaluator (which READS the
    // last-acknowledged hash) and the PraxecServer (which WRITES on
    // `gateway.describe`). Wiring only one side leaves the guard permanently
    // unsatisfiable. Production previously wired neither (these `.with_*` calls
    // existed only in tests), so any workflow gating on a describe-ack could
    // never advance. Build once here, share both ways. Durable (sqlite) →
    // `guidance_acks` / `script_acks` tables in the workflow DB, so a
    // describe-ack guard stays satisfied across a restart.
    let guidance_ack: Arc<dyn GuidanceAcknowledgmentStore> = build_guidance_ack_store(config)?;
    let script_ack: Arc<dyn ScriptAcknowledgmentStore> = build_script_ack_store(config)?;

    let guards = Arc::new(
        DefaultGuardEvaluator::with_evidence(evidence.clone())
            .with_ack_store(guidance_ack.clone())
            .with_script_ack_store(script_ack.clone()),
    );
    // Global repository write-exclusion: no two agents — across any workflows
    // or sub-workflows — hold a lock on the same file at once. Contention
    // durably suspends (`waiting_on_lock`) and auto-resumes FIFO on release.
    let repo_locks: Arc<dyn praxec_core::repo_locks::RepoLocks> =
        Arc::new(praxec_core::repo_locks::RepoLockSpace::new());
    let lock_scheduler = Arc::new(praxec_core::lock_scheduler::LockScheduler::new());
    let runtime = WorkflowRuntime::new(
        swappable_defs.clone() as Arc<dyn praxec_core::ports::DefinitionStore>,
        store,
        swappable_executors.clone() as Arc<dyn praxec_core::ports::ExecutorRegistry>,
        guards,
        audit.clone(),
    )
    .with_evidence(evidence)
    .with_repo_locks(repo_locks)
    .with_lock_scheduler(lock_scheduler)
    .with_auto_drive_agents(
        config
            .pointer("/praxec/agents/auto_drive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        config
            .pointer("/praxec/agents/auto_drive_affinity")
            .and_then(Value::as_str)
            .unwrap_or("reasoning"),
        {
            // Auto-drive tool set = every wired connection PLUS any operator
            // `praxec.agents.auto_drive_tools` entries (data-not-code). The
            // latter carries e.g. `file:{{ $.workflow.input.repo_path }}` —
            // templated per-leaf by AgentExecutor; non-coding leaves (no
            // repo_path) get no file tools (CompositeToolHost guard).
            let mut t: Vec<String> = config
                .pointer("/connections")
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            if let Some(extra) = config
                .pointer("/praxec/agents/auto_drive_tools")
                .and_then(Value::as_array)
            {
                t.extend(extra.iter().filter_map(|v| v.as_str().map(str::to_string)));
            }
            t
        },
        config
            .pointer("/praxec/agents/auto_drive_max_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    // C1 — late-bind the runtime into the `kind: workflow` executor now that the
    // runtime (built around the registry) exists. Without this, every
    // `kind: workflow` transition fails WORKFLOW_EXECUTOR_NOT_WIRED even though
    // the config passed `praxec check`.
    workflow_handle.set_runtime(runtime.clone());
    // Re-register any workflows suspended on a lock before a restart so they
    // auto-resume once their files free (durable stores only; no-op in-memory).
    runtime.recover_suspended_locks().await;
    // Re-drive any parents suspended on a sub-workflow before a restart so a
    // child that terminated during the downtime still resumes its parent
    // (otherwise the parent would stay `waiting` forever).
    runtime.recover_suspended_subworkflows().await;

    // Apply the caller's overlays onto the base registry. Each registrar needs
    // post-startup context (the `kind: llm` resolver is built from the live
    // runtime, so the runtime had to exist first); each captures the swappable's
    // *currently-held* registry and wraps it, then we swap the stack back in.
    // The captured inner is the pre-wrap registry — never the swappable — so
    // there is no lookup cycle. The SAME registrars are re-applied on SIGHUP
    // below, so a reload never silently drops the overlaid kinds.
    let overlaid = apply_overlays(
        swappable_executors.current(),
        config,
        &audit,
        &runtime,
        &overlays.registrars,
    );
    swappable_executors.swap(overlaid);

    // SPEC §30 — pull the top-level `lexicon:` block out of the
    // resolved config and pass it as the lexicon base. Empty when no
    // block declared. Runtime writes via `gateway.lexicon.define`
    // land in the in-memory overlay; operators persist by editing
    // praxec.yaml.
    let lexicon_base = config
        .get("lexicon")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    // H5 — seed the runtime-mutable pending-subjects set from the resolved
    // config (PENDING_DEFINITION lexicon placeholders). Without this the server
    // never knew which subjects were placeholders, so resolution handlers had
    // nothing to clear and the runtime's pre-start subject walk used the
    // snapshot fallback. `with_pending_subjects` also shares the Arc into the
    // runtime (Phase-1 live-set checks).
    let pending_subjects = praxec_core::lexicon::pending_subjects_from_resolved(config);

    // H5 — prevent → detect → fail-fast. If a workflow gates on a describe-ack
    // guard, the ack store MUST be wired into BOTH the guard evaluator (done
    // above) AND the server (done below); otherwise the guard is permanently
    // unsatisfiable and the workflow stalls forever. We always wire both, so
    // this is a structural guarantee — but assert it so a future edit that drops
    // either side fails the boot loudly instead of silently stalling workflows.
    let (uses_guidance_ack, uses_script_ack) = ack_guards_used(config);

    // H6 — authoring-host opt-ins. These tools are OFF by default (the runtime
    // guidance surface is push-not-pull, §5.4; lexicon is normally curated via
    // CLI). An authoring deployment enables them via config rather than needing
    // a custom binary that calls the `.with_*` builders. Absent → false.
    let skills_search = config
        .pointer("/praxec/authoring/skills_search")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let scripts_search = config
        .pointer("/praxec/authoring/scripts_search")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let lexicon_writes = config
        .pointer("/praxec/authoring/lexicon_writes")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // SPEC §30.5 durability — directory under which MCP-defined lexicon terms
    // persist as `<term>.json` (loaded back at boot). Default `.praxec/lexicon`
    // relative to cwd; override with `praxec.authoring.lexicon_dir`.
    let lexicon_dir = config
        .pointer("/praxec/authoring/lexicon_dir")
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(".praxec/lexicon"));

    let mut server = PraxecServer::new(runtime.clone())
        .with_discovery(swappable_discovery.clone() as Arc<dyn DiscoveryIndex>)
        .with_lexicon(lexicon_base)
        .with_ack_store(guidance_ack.clone())
        .with_script_ack_store(script_ack.clone())
        .with_pending_subjects(pending_subjects)
        .with_skills_search(skills_search)
        .with_scripts_search(scripts_search)
        .with_lexicon_writes(lexicon_writes)
        .with_lexicon_dir(lexicon_dir)
        .with_progress_peer(progress_peer);

    // The assertion: both ack guards, if used, are now backed end-to-end (server
    // write side + guard-evaluator read side). This only fails if a future edit
    // drops the `.with_ack_store` / `.with_script_ack_store` wiring — in which
    // case refuse to boot rather than ship a guard that can never be satisfied.
    if uses_guidance_ack && !server.has_guidance_ack_store() {
        anyhow::bail!(
            "refusing to start: a workflow uses the `guidance_acknowledged` guard but no \
             guidance-acknowledgment store is wired into the server — the guard could never be \
             satisfied and the workflow would stall forever (wiring regression in serve_with)."
        );
    }
    if uses_script_ack && !server.has_script_ack_store() {
        anyhow::bail!(
            "refusing to start: a workflow uses the `script_acknowledged` guard but no \
             script-acknowledgment store is wired into the server — the guard could never be \
             satisfied and the workflow would stall forever (wiring regression in serve_with)."
        );
    }
    // Semantic discovery is an opt-in add-on (see `resolve_embedder`): a live
    // embedder only when a model is registered AND not disabled via
    // `praxec.embeddings.enabled: false`; otherwise the free lexical index +
    // `NoopEmbedder`. When live, we attach it (the lexicon Tier-3 path) and swap
    // a `SemanticDiscoveryIndex` over the items below.
    let embedder = resolve_embedder(config);
    server = server.with_embedder(embedder.clone());
    if embedder.backend_name() != "noop" {
        // H5 — backfill embeddings over the config-loaded lexicon so Tier-3
        // semantic candidate ranking works from the first request. Best-effort
        // (per-term failures are logged, not fatal); no-ops under NoopEmbedder.
        // Must run AFTER `with_embedder`, which is why it lives here.
        server.backfill_lexicon_embeddings().await;
        // Build the semantic index over every item (incl. guidance/skills) and
        // swap it in for this startup. (A config hot-reload reverts to lexical
        // until restart — re-embedding on reload is a follow-up.)
        let mut items = swappable_discovery.list(None).await?;
        items.extend(
            swappable_discovery
                .list(Some(DiscoveryKind::Guidance))
                .await?,
        );
        let count = items.len();
        let semantic = Arc::new(SemanticDiscoveryIndex::build(items, embedder.clone()).await?);
        swappable_discovery.swap(semantic);
        tracing::info!(items = count, "semantic discovery index built");
    }
    // CMP-001 — single-tenant operators can assert a default identity via
    // `gateway.principal: { subject, roles, permissions }`. Per-request `_meta`
    // claims (from the embedding host) still override it; absent both, callers
    // are anonymous (fail-closed).
    if let Some(principal) = config
        .pointer("/gateway/principal")
        .and_then(praxec_core::model::Principal::from_claim)
    {
        server = server.with_principal(principal);
    }
    // CMP-001 — operators who don't trust the `_meta` channel set
    // `gateway.trust_meta_principal: false` to run every caller as the
    // configured default principal (config-only identity). Default: trust.
    if let Some(trust) = config
        .pointer("/gateway/trust_meta_principal")
        .and_then(Value::as_bool)
    {
        server = server.with_trust_meta_principal(trust);
    }

    Ok(OneshotServer {
        server,
        runtime,
        audit,
        swappable_defs,
        swappable_executors,
        swappable_discovery,
    })
}

pub async fn serve_with(config_path: PathBuf, overlays: GatewayOverlays) -> anyhow::Result<()> {
    let config = load_config(&config_path)?;

    // Production safety (poka-yoke): refuse to boot a long-running gateway on an
    // ephemeral store or non-durable audit sink rather than trusting operators
    // to read the docs. Serve-only: a one-shot command/query against an
    // ephemeral store is fine (like `inspect`); a long-running serve is not.
    guard_durable_serve(&config)?;

    let OneshotServer {
        server,
        runtime,
        audit,
        swappable_defs,
        swappable_executors,
        swappable_discovery,
    } = build_oneshot_server(&config, &overlays).await?;

    tracing::info!(
        path = %config_path.display(),
        "starting praxec stdio server"
    );

    let service = server
        .serve(stdio())
        .await
        .context("starting MCP service over stdio")?;

    // SIGHUP: hot-reload config without dropping connections or in-flight work.
    #[cfg(unix)]
    {
        let reload_defs = swappable_defs.clone();
        let reload_executors = swappable_executors.clone();
        let reload_discovery = swappable_discovery.clone();
        let reload_config_path = config_path.clone();
        let reload_audit = audit.clone();
        let reload_runtime = runtime.clone();
        let reload_registrars = overlays.registrars.clone();
        tokio::spawn(async move {
            let mut sighup =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to register SIGHUP handler");
                        return;
                    }
                };
            loop {
                sighup.recv().await;
                tracing::info!("received SIGHUP — reloading config");
                match load_config(&reload_config_path) {
                    Ok(new_config) => {
                        // Mint/reload gate (ADR-0012 harness) — refuse to swap in a
                        // config carrying structural Error diagnostics (e.g. a V23
                        // dead-stall-prone switch with no default). A broken edit
                        // must never replace a working config: keep the previous one
                        // and surface the FULL Error list, so "no execution until the
                        // issue is resolved" holds for the live hot-reload path too.
                        let errors: Vec<String> =
                            praxec_core::validate::validate_workflows(&new_config)
                                .into_iter()
                                .filter(praxec_core::validate::Diagnostic::is_error)
                                .map(|d| d.message().to_string())
                                .collect();
                        if !errors.is_empty() {
                            tracing::error!(
                                error_count = errors.len(),
                                errors = ?errors,
                                "config reload REJECTED — validation errors; keeping previous configuration"
                            );
                            let _ = reload_audit
                                .record(
                                    praxec_core::audit::AuditEvent::new("config.reload_rejected")
                                        .with_payload(json!({
                                            "config": reload_config_path.display().to_string(),
                                            "errors": errors,
                                        })),
                                )
                                .await;
                            continue;
                        }
                        // CMP-031 — a config whose discovery.include is invalid
                        // must NOT swap a partial index into the live runtime.
                        // Keep the existing components and log the failure.
                        match build_hot_components(&new_config, &reload_audit).await {
                            Ok((new_defs, new_executors, new_discovery, new_workflow_handle)) => {
                                // C1 — the fresh registry has a fresh runtime-less
                                // `workflow` executor; re-wire it against the SAME
                                // long-lived runtime so reloaded configs keep
                                // dispatching `kind: workflow`.
                                new_workflow_handle.set_runtime(reload_runtime.clone());
                                // Re-apply the overlays so the reloaded registry
                                // keeps hosting `kind: llm` / `kind: agent`.
                                let new_executors = apply_overlays(
                                    new_executors,
                                    &new_config,
                                    &reload_audit,
                                    &reload_runtime,
                                    &reload_registrars,
                                );
                                reload_defs.swap(new_defs);
                                reload_executors.swap(new_executors);
                                reload_discovery.swap(new_discovery);
                                let _ = reload_audit
                                    .record(
                                        praxec_core::audit::AuditEvent::new("config.reloaded")
                                            .with_payload(json!({
                                                "config": reload_config_path.display().to_string(),
                                            })),
                                    )
                                    .await;
                                tracing::info!("config reloaded successfully");
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "config reload failed to build components; \
                                     keeping previous configuration"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "config reload failed — keeping current config");
                    }
                }
            }
        });
    }

    let drain_deadline_secs: u64 = std::env::var("PRAXEC_DRAIN_DEADLINE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let cancel = service.cancellation_token();
    let drain_runtime = runtime.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        tracing::info!(
            deadline_secs = drain_deadline_secs,
            "received shutdown signal — draining"
        );
        drain_runtime.begin_drain();
        tokio::time::sleep(std::time::Duration::from_secs(drain_deadline_secs)).await;
        tracing::info!("drain deadline reached — closing service");
        cancel.cancel();
    });

    service.waiting().await?;
    signal_task.abort();
    Ok(())
}

async fn build_hot_components(
    config: &Value,
    audit: &Arc<dyn praxec_core::audit::AuditSink>,
) -> anyhow::Result<(
    Arc<dyn praxec_core::ports::DefinitionStore>,
    Arc<dyn praxec_core::ports::ExecutorRegistry>,
    Arc<dyn DiscoveryIndex>,
    // The runtime-less `kind: workflow` executor handle. The caller MUST call
    // `set_runtime` on it once the `WorkflowRuntime` (built around this
    // registry) exists, or every `kind: workflow` transition will fail with
    // WORKFLOW_EXECUTOR_NOT_WIRED. SIGHUP rebuilds produce a fresh handle that
    // is re-wired against the same long-lived runtime.
    Arc<praxec_executors::WorkflowExecutor>,
)> {
    let mcp_conns = McpConnections::from_config(config);
    let mcp_executor = Arc::new(McpExecutor::new(mcp_conns));
    let imported = import_capabilities(config, &mcp_executor, audit).await;
    let effective_config = with_imports(config.clone(), &imported);
    let cli_conns = Arc::new(CliConnections::from_config(&effective_config));
    let (executors, workflow_handle) = default_registry_with_late_workflow(
        &effective_config,
        mcp_executor,
        cli_conns,
        audit.clone(),
    );
    let executors = maybe_enable_authoring(&effective_config, executors, audit)?;
    let executors = maybe_enable_sandbox(&effective_config, executors)?;
    let definitions: Arc<dyn praxec_core::ports::DefinitionStore> =
        Arc::new(ConfigDefinitionStore::from_config(&effective_config));
    // CMP-031 — fails fast on an unknown `discovery.include` token rather than
    // shipping a silently partial index.
    let discovery: Arc<dyn DiscoveryIndex> =
        Arc::new(InMemoryDiscoveryIndex::from_config(&effective_config)?);
    Ok((definitions, executors, discovery, workflow_handle))
}

/// SPEC §8.4 — when `praxec.authoring.write_enabled` is set, overlay an
/// **enabled** `registry` executor backed by a repo-backed writable definition
/// store (the repos declared `writable: true`, carried in `_writableRepos`).
/// Reads keep flowing through the merged config store; this is purely the
/// governed write sink. The provenance gate is seeded with the operator's
/// top-level `connections:` names — an authored definition may reference those
/// but never introduce a raw command of its own.
///
/// Fail-loud (no silent no-op) when the flag is on but no writable repo is
/// declared: that's a misconfiguration the operator must see at startup.
fn maybe_enable_authoring(
    config: &Value,
    executors: Arc<dyn ExecutorRegistry>,
    audit: &Arc<dyn praxec_core::audit::AuditSink>,
) -> anyhow::Result<Arc<dyn ExecutorRegistry>> {
    let write_enabled = config
        .pointer("/praxec/authoring/write_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !write_enabled {
        return Ok(executors);
    }

    let roots: Vec<(PathBuf, bool, bool)> = config
        .pointer("/praxec/_writableRepos")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let root = e.pointer("/root").and_then(Value::as_str)?;
                    let push = e.pointer("/push").and_then(Value::as_bool).unwrap_or(false);
                    Some((PathBuf::from(root), true, push))
                })
                .collect()
        })
        .unwrap_or_default();
    if roots.is_empty() {
        anyhow::bail!(
            "praxec.authoring.write_enabled is true but no repo is declared \
             `writable: true`. Mark an authoring target, e.g. \
             `repos: [{{ path: ./my-workflows, writable: true }}]`."
        );
    }

    let store = praxec_core::store::RepoDefinitionStore::from_repos(roots, audit.clone())?;
    let allowed_connections: Vec<String> = config
        .pointer("/connections")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let registry_exec =
        RegistryExecutor::enabled(Arc::new(store)).with_allowed_connections(allowed_connections);
    Ok(Arc::new(SingleKindOverlay::new(
        executors,
        "registry",
        Arc::new(registry_exec),
    )))
}

/// ADR-0006 — when `praxec.execution.sandbox` selects a backend, overlay the
/// `script` executor with a sandbox-backed one (least-privilege production
/// tier). Fail-fast on misconfiguration rather than silently running unconfined:
/// a configured-but-unusable sandbox aborts startup with the preflight remedy,
/// and `require_confinement` without a usable provider is rejected (no script
/// could ever run). Absent config → unchanged (existing scripts run as before).
fn maybe_enable_sandbox(
    config: &Value,
    executors: Arc<dyn ExecutorRegistry>,
) -> anyhow::Result<Arc<dyn ExecutorRegistry>> {
    let kind = config
        .pointer("/praxec/execution/sandbox")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let require = config
        .pointer("/praxec/execution/require_confinement")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let provider: Option<Arc<dyn SandboxProvider>> = match kind {
        "none" => None,
        "bwrap" => {
            let p = BwrapProvider::new();
            let pf = p.preflight();
            if !pf.usable {
                anyhow::bail!(
                    "praxec.execution.sandbox is `bwrap` but it is not usable here: {}. {}",
                    pf.detail,
                    pf.install_hint.unwrap_or_default()
                );
            }
            Some(Arc::new(p))
        }
        "oci" => {
            let runtime = config
                .pointer("/praxec/execution/oci/runtime")
                .and_then(Value::as_str)
                .unwrap_or("docker");
            let image = config
                .pointer("/praxec/execution/oci/image")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "praxec.execution.sandbox is `oci` but praxec.execution.oci.image is \
                         not set (the base image scripts run against, e.g. debian:stable-slim)"
                    )
                })?;
            let p = OciProvider::new(runtime, image);
            let pf = p.preflight();
            if !pf.usable {
                anyhow::bail!(
                    "praxec.execution.sandbox is `oci` but the runtime is not usable here: {}. {}",
                    pf.detail,
                    pf.install_hint.unwrap_or_default()
                );
            }
            Some(Arc::new(p))
        }
        other => {
            anyhow::bail!("unknown praxec.execution.sandbox `{other}` (expected: none, bwrap, oci)")
        }
    };

    if require && provider.is_none() {
        anyhow::bail!(
            "praxec.execution.require_confinement is set but no usable sandbox provider is \
             configured (praxec.execution.sandbox) — no script could run. Configure a sandbox."
        );
    }
    if provider.is_none() && !require {
        return Ok(executors); // nothing to overlay — default unconfined behavior
    }

    let mut script_exec = ScriptExecutor::new();
    if let Some(p) = provider {
        script_exec = script_exec.with_sandbox(p);
    }
    let script_exec = script_exec.require_confinement(require);
    Ok(Arc::new(SingleKindOverlay::new(
        executors,
        "script",
        Arc::new(script_exec),
    )))
}

fn migrate(config_path: PathBuf) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(&config_path)?;
    let count = raw.matches("kind: jsonpath").count()
        + raw.matches("kind: 'jsonpath'").count()
        + raw.matches("kind: \"jsonpath\"").count();
    if count == 0 {
        println!(
            "migrate: no migrations to run (config: {})",
            config_path.display()
        );
        return Ok(());
    }
    let updated = raw
        .replace("kind: jsonpath", "kind: expr")
        .replace("kind: 'jsonpath'", "kind: 'expr'")
        .replace("kind: \"jsonpath\"", "kind: \"expr\"");
    std::fs::write(&config_path, updated)?;
    println!(
        "migrate: rewrote {} guard(s) from kind: jsonpath → kind: expr (config: {})",
        count,
        config_path.display()
    );
    Ok(())
}

fn check(config_path: PathBuf, extra_diagnostics: &[DiagnosticProvider]) -> anyhow::Result<()> {
    // SPEC §5.4.2 / audit-resolution C.2 — `check` is the surface where
    // soft diagnostics (e.g. non-strict-mode unblessed subject roots)
    // become visible. Use the diagnostics-returning variant.
    let (config, soft_diagnostics) = praxec_core::config::load_resolved_with_repos(&config_path)
        .with_context(|| format!("loading config {}", config_path.display()))?;

    let version = config
        .pointer("/version")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "config {}: 'version' field is required (e.g. version: \"1.0.0\")",
                config_path.display()
            )
        })?;
    println!("config version: {version}");

    let store = ConfigDefinitionStore::from_config(&config);
    let mut ids = store.ids();
    ids.sort();
    println!("config: {}", config_path.display());
    println!("workflows ({}):", ids.len());
    for id in &ids {
        println!("  - {id}");
    }

    let imports: Vec<&str> = config
        .pointer("/proxy/import")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.get("connection").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if !imports.is_empty() {
        println!("imports ({}):", imports.len());
        for c in imports {
            println!("  - from connection: {c}");
        }
    }

    // ADR-0006 — sandbox preflight: if a confinement backend is configured,
    // report whether it's actually usable on this host (a functional probe, not
    // mere presence). Surfaces the same condition `serve` would fail-fast on.
    if let Some(kind) = config
        .pointer("/praxec/execution/sandbox")
        .and_then(Value::as_str)
        .filter(|k| *k != "none")
    {
        let pf = match kind {
            "bwrap" => Some(BwrapProvider::new().preflight()),
            "oci" => {
                let runtime = config
                    .pointer("/praxec/execution/oci/runtime")
                    .and_then(Value::as_str)
                    .unwrap_or("docker");
                let image = config
                    .pointer("/praxec/execution/oci/image")
                    .and_then(Value::as_str)
                    .unwrap_or("<unset>");
                Some(OciProvider::new(runtime, image).preflight())
            }
            other => {
                println!("sandbox: unknown backend `{other}` (expected: none, bwrap, oci)");
                None
            }
        };
        if let Some(pf) = pf {
            if pf.usable {
                println!("sandbox ({kind}): OK — {}", pf.detail);
            } else {
                println!(
                    "sandbox ({kind}): UNUSABLE — {}. {}",
                    pf.detail,
                    pf.install_hint.unwrap_or_default()
                );
            }
        }
    }

    // CMP-002 — the same suite `serve` enforces at startup (validate_workflows
    // + executor-kind doctor + feature-gated cost / kind:llm config doctors).
    let diagnostics = collect_diagnostics_with(&config, extra_diagnostics);
    let errors = diagnostics.iter().filter(|d| d.is_error()).count();
    let warnings = diagnostics.iter().filter(|d| !d.is_error()).count();
    let soft_warnings = soft_diagnostics.len();

    if !diagnostics.is_empty() {
        println!();
        for d in &diagnostics {
            println!("  {d}");
        }
    }
    // SPEC §5.4.2 / audit-resolution C.2 — print soft diagnostics under
    // their own banner so operators see them even when the rest of
    // validation succeeds.
    if !soft_diagnostics.is_empty() {
        println!();
        println!("soft warnings (resolve-time):");
        for d in &soft_diagnostics {
            let loc = d
                .location
                .as_deref()
                .map(|l| format!(" at {l}"))
                .unwrap_or_default();
            let suggestion = d
                .suggestion
                .as_deref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            println!("  warn[{}]{loc}: {}{suggestion}", d.code, d.message);
        }
    }
    if !diagnostics.is_empty() || !soft_diagnostics.is_empty() {
        println!();
        println!(
            "validation: {} error(s), {} warning(s), {} soft warning(s)",
            errors, warnings, soft_warnings
        );
    } else if !ids.is_empty() {
        println!("validation: ok");
    }

    if errors > 0 {
        anyhow::bail!("config validation failed with {errors} error(s)");
    }

    Ok(())
}

fn observe(config_path: PathBuf) -> anyhow::Result<()> {
    let (config, _) = praxec_core::config::load_resolved_with_repos(&config_path)
        .with_context(|| format!("loading config {}", config_path.display()))?;

    let audit_path = config
        .pointer("/audit/path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing /audit/path in config"))?;

    let today = chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let file_name = format!("{today}-audit.log");
    let file_path = std::path::PathBuf::from(audit_path).join(&file_name);

    let mut records: Vec<Value> = Vec::new();
    if file_path.exists() {
        let content = std::fs::read_to_string(&file_path)
            .with_context(|| format!("reading audit file {}", file_path.display()))?;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(v) => records.push(v),
                Err(_) => continue, // skip unparseable lines leniently
            }
        }
    }

    let report = aggregate_calls(&records);

    // Per-model table
    for (model, stats) in &report.models {
        println!(
            "Model: {model}\n  #calls:           {}\n  total_tokens:     {}\n  total_cost_usd:   {:.6}\n  avg_duration_ms:  {:.1}",
            stats.call_count,
            stats.total_tokens,
            stats.total_cost_usd,
            stats.avg_duration_ms()
        );
    }

    // Totals
    println!(
        "---\nTotals\n  #calls:           {}\n  total_tokens:     {}\n  total_cost_usd:   {:.6}",
        report.totals.call_count, report.totals.total_tokens, report.totals.total_cost_usd,
    );

    // HANGS
    if report.hangs.is_empty() {
        println!("\nHANGS\n  (none)");
    } else {
        println!("\nHANGS");
        for hang in &report.hangs {
            println!(
                "  correlation_id: {}  model: {}  affinity: {}",
                hang.correlation_id,
                hang.model.as_deref().unwrap_or("?"),
                hang.affinity.as_deref().unwrap_or("?"),
            );
        }
    }

    // One-line JSON summary
    let summary = json!({
        "models": report.models.iter().map(|(m, s)| json!({
            "model": m,
            "call_count": s.call_count,
            "total_tokens": s.total_tokens,
            "total_cost_usd": s.total_cost_usd,
            "avg_duration_ms": s.avg_duration_ms(),
        })).collect::<Vec<_>>(),
        "totals": {
            "call_count": report.totals.call_count,
            "total_tokens": report.totals.total_tokens,
            "total_cost_usd": report.totals.total_cost_usd,
        },
        "hangs": report.hangs.iter().map(|h| json!({
            "correlation_id": h.correlation_id,
            "model": h.model,
            "affinity": h.affinity,
        })).collect::<Vec<_>>(),
    });
    println!("\n---\n{}", serde_json::to_string(&summary)?);

    Ok(())
}

// ── Pure helper: aggregate audit records ─────────────────────────────────

#[derive(Debug, Default)]
struct CallStats {
    call_count: usize,
    total_tokens: u64,
    total_cost_usd: f64,
    total_duration_ms: u64,
}

impl CallStats {
    fn avg_duration_ms(&self) -> f64 {
        if self.call_count == 0 {
            0.0
        } else {
            self.total_duration_ms as f64 / self.call_count as f64
        }
    }
}

#[derive(Debug)]
struct HangEntry {
    correlation_id: String,
    model: Option<String>,
    affinity: Option<String>,
}

#[derive(Debug)]
struct ObserveReport {
    models: std::collections::BTreeMap<String, CallStats>,
    totals: CallStats,
    hangs: Vec<HangEntry>,
}

fn aggregate_calls(records: &[Value]) -> ObserveReport {
    use std::collections::{BTreeMap, HashMap};

    let mut models: BTreeMap<String, CallStats> = BTreeMap::new();
    let mut totals = CallStats::default();
    let mut invoked: HashMap<String, &Value> = HashMap::new();
    let mut hangs: Vec<HangEntry> = Vec::new();

    for rec in records {
        let event_type = rec.get("event_type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "agent.invoked" => {
                if let Some(cid) = rec.get("correlation_id").and_then(Value::as_str) {
                    invoked.insert(cid.to_string(), rec);
                }
            }
            "agent.completed" => {
                // Audit records nest call telemetry under `payload`; fall back to
                // top-level for resilience to format drift.
                let pget = |k: &str| {
                    rec.get("payload")
                        .and_then(|p| p.get(k))
                        .or_else(|| rec.get(k))
                };
                let model = pget("model").and_then(Value::as_str).map(str::to_string);
                let _affinity = pget("affinity").and_then(Value::as_str).map(str::to_string);
                let key = model.clone().unwrap_or_else(|| "?".to_string());
                let cost_usd = pget("cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
                let prompt_tokens = pget("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
                let completion_tokens = pget("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let duration_ms = pget("duration_ms").and_then(Value::as_u64).unwrap_or(0);
                let tokens = prompt_tokens + completion_tokens;

                let entry = models.entry(key).or_default();
                entry.call_count += 1;
                entry.total_tokens += tokens;
                entry.total_cost_usd += cost_usd;
                entry.total_duration_ms += duration_ms;

                totals.call_count += 1;
                totals.total_tokens += tokens;
                totals.total_cost_usd += cost_usd;

                // Mark invoked as resolved
                if let Some(cid) = rec.get("correlation_id").and_then(Value::as_str) {
                    invoked.remove(cid);
                }
            }
            _ => {}
        }
    }

    // Any remaining invoked entries are hangs
    for (cid, rec) in invoked {
        let pget = |k: &str| {
            rec.get("payload")
                .and_then(|p| p.get(k))
                .or_else(|| rec.get(k))
        };
        hangs.push(HangEntry {
            correlation_id: cid,
            model: pget("model").and_then(Value::as_str).map(str::to_string),
            affinity: pget("affinity").and_then(Value::as_str).map(str::to_string),
        });
    }

    ObserveReport {
        models,
        totals,
        hangs,
    }
}

fn health(config_path: PathBuf) -> anyhow::Result<()> {
    let (config, _) = praxec_core::config::load_resolved_with_repos(&config_path)
        .with_context(|| format!("loading config {}", config_path.display()))?;

    let connections: Vec<String> = config
        .pointer("/connections")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    let repos: Vec<String> = config
        .pointer("/repos")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.get("path").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let definition_count = ConfigDefinitionStore::from_config(&config).ids().len();

    let store = config
        .pointer("/store/kind")
        .and_then(Value::as_str)
        .unwrap_or("memory")
        .to_string();

    let snapshot = json!({
        "connections": connections,
        "repos": repos,
        "definition_count": definition_count,
        "store": store,
    });

    println!("{}", serde_json::to_string(&snapshot)?);
    Ok(())
}

async fn fuzz_cmd(
    config: PathBuf,
    iterations: usize,
    seed: u64,
    report: String,
    live: bool,
    model: Option<String>,
    overlays: GatewayOverlays,
) -> anyhow::Result<()> {
    if live {
        let model =
            model.ok_or_else(|| anyhow::anyhow!("--live requires --model <provider:model>"))?;
        return fuzz_live(config, iterations, model, overlays).await;
    }

    // ── Coverage: graph-walk + per-transition fuzz ────────────────────────────
    // This is the primary signal: every (state, transition) edge is exercised in
    // isolation with a satisfying context and (when possible) a violating context.
    // Orphan states are also surfaced. Coverage failures drive the exit code.
    let cov = praxec_test::fuzz_coverage(&config).await?;

    // ── Capped integration smoke ──────────────────────────────────────────────
    // Force exactly 1 iteration regardless of --iterations: the smoke only proves
    // the end-to-end path (load → start → drive → classify) executes once without
    // an engine error. Coverage, not iteration count, is the depth signal.
    // (--iterations is retained for --live compatibility and future expansion.)
    //
    // For the text path, coverage is printed BEFORE the smoke runs so the primary
    // signal is always emitted first (a slow or error-classified smoke cannot
    // suppress it).

    match report.as_str() {
        "json" => {
            // For JSON, compute the smoke first so both are assembled into one
            // combined object (order doesn't matter; JSON consumers parse the whole
            // object at once).
            let smoke = praxec_test::fuzz_config(&config, 1, seed).await?;

            // Combine coverage (per-def edges/orphans/verdicts) and smoke has_violations
            // into a single JSON object. No new derives — built with serde_json::json!.
            let defs_json: Vec<serde_json::Value> = cov
                .defs
                .iter()
                .map(|d| {
                    let verdicts_json: Vec<serde_json::Value> = d
                        .verdicts
                        .iter()
                        .map(|v| {
                            json!({
                                "state": v.state,
                                "transition": v.transition,
                                "ok": v.ok,
                                "detail": v.detail,
                            })
                        })
                        .collect();
                    json!({
                        "id": d.definition_id,
                        "edges": d.edges,
                        "orphan_states": d.orphan_states,
                        "verdicts": verdicts_json,
                    })
                })
                .collect();
            let smoke_engine_errors = praxec_test::report::has_engine_errors(&smoke);
            let out = json!({
                "coverage": {
                    "defs": defs_json,
                    "has_failures": cov.has_failures(),
                },
                "smoke": {
                    // Advisory: mock-drive Wedge/Livelock on agent flows is expected.
                    "has_violations": praxec_test::report::has_violations(&smoke),
                    // Gating: only a hard execute failure fails the build.
                    "has_engine_errors": smoke_engine_errors,
                },
            });
            println!("{}", serde_json::to_string_pretty(&out)?);

            // Coverage failures are the primary signal; the smoke gates ONLY on an
            // EngineError (the flow cannot execute at all). Mock-drive Wedge/Livelock
            // is advisory — a mock chooser can't navigate agent-heavy flows.
            if cov.has_failures() || smoke_engine_errors {
                anyhow::bail!("fuzz found coverage failures or a smoke execute-failure");
            }
        }
        _ => {
            // Print coverage immediately — before the smoke — so the primary signal
            // is always visible even if the smoke is slow or noisy.
            print!("{}", cov.render_text());

            let smoke = praxec_test::fuzz_config(&config, 1, seed).await?;
            let smoke_engine_errors = praxec_test::report::has_engine_errors(&smoke);
            let smoke_defs = smoke.results.len();
            // Per-def smoke verdicts: EngineError ✗ (gates), Wedge/Livelock ⚠ (advisory).
            print!("{}", praxec_test::report::render_smoke(&smoke));
            println!(
                "smoke: {smoke_defs} def(s){}",
                if smoke_engine_errors {
                    " [EXECUTE-FAILURE]"
                } else {
                    " ok"
                }
            );

            // Coverage failures are the primary signal; the smoke gates ONLY on an
            // EngineError (the flow cannot execute at all). Mock-drive Wedge/Livelock
            // is advisory — a mock chooser can't navigate agent-heavy flows.
            if cov.has_failures() || smoke_engine_errors {
                anyhow::bail!("fuzz found coverage failures or a smoke execute-failure");
            }
        }
    }

    Ok(())
}

/// Drive every workflow definition in `config_path` with the REAL executor
/// registry and a real model chooser (`AgentChooser`) so live provider calls
/// exercise the governed runtime end-to-end, classified by the same oracle that
/// classifies mock fuzz runs. One fresh runtime per (definition × iteration) so
/// a provider error in one run never poisons the rest.
async fn fuzz_live(
    config_path: PathBuf,
    iterations: usize,
    model: String,
    overlays: GatewayOverlays,
) -> anyhow::Result<()> {
    use praxec_agents::orchestrator::{
        drive_mission, run_headless_consumer, AgentChooser, HeadlessPolicy, MissionGateway,
        RuntimeMissionGateway,
    };
    use praxec_agents::rig_runner::RigSessionRunner;
    use praxec_agents::session::AgentSessionRunner;
    use praxec_core::bus::Bus;
    use praxec_core::model::{Principal, StartWorkflow};

    // Load the definition ids once — we only need the id list, not the full runtime.
    let (resolved, _diags) = praxec_core::config::load_resolved_with_repos(&config_path)
        .with_context(|| format!("loading config {}", config_path.display()))?;
    let ids = ConfigDefinitionStore::from_config(&resolved).ids();

    let mut any_violation = false;

    for id in &ids {
        for i in 0..iterations {
            // Build a fresh real runtime per run — mirrors orchestrate()'s pattern.
            let config = match load_config(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    println!("✗ {id} [run {i}] — EngineError: {e}");
                    any_violation = true;
                    continue;
                }
            };
            let runtime = match build_runtime_for_orchestrate(&config, &overlays).await {
                Ok(r) => r,
                Err(e) => {
                    println!("✗ {id} [run {i}] — EngineError: {e}");
                    any_violation = true;
                    continue;
                }
            };

            // Start a fresh workflow instance for this definition.
            let resp = match runtime
                .start(StartWorkflow {
                    definition_id: id.clone(),
                    input: json!({}),
                    principal: Principal::anonymous(),
                    trace_id: None,
                    run_id: None,
                    depth: 0,
                    parent: None,
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    println!("✗ {id} [run {i}] — EngineError: {e}");
                    any_violation = true;
                    continue;
                }
            };
            let mission_id = match resp
                .pointer("/workflow/id")
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                Some(mid) => mid,
                None => {
                    println!("✗ {id} [run {i}] — EngineError: started '{id}' but no workflow id was returned");
                    any_violation = true;
                    continue;
                }
            };

            let gateway = RuntimeMissionGateway::new(runtime, Principal::anonymous());
            let runner: Arc<dyn AgentSessionRunner> =
                Arc::new(RigSessionRunner::with_default_provider());
            let chooser =
                AgentChooser::new(runner, model.clone(), std::time::Duration::from_secs(120));

            let bus = Bus::new();
            let events = bus.subscribe();
            let consumer = tokio::spawn(run_headless_consumer(
                events,
                bus.clone(),
                HeadlessPolicy::AutoApprove,
            ));

            let outcome = drive_mission(&gateway, &chooser, &bus, &mission_id, 40).await;
            consumer.abort();

            let final_state = gateway.query(&mission_id).await;
            let verdict = match final_state {
                Ok(fs) => praxec_test::classify_run(&outcome, &fs),
                Err(_) => praxec_test::RunVerdict::EngineError("final query failed".into()),
            };

            let mark = if verdict.is_violation() { "✗" } else { "✓" };
            println!("{mark} {id} [run {i}] — {verdict:?}");
            if verdict.is_violation() {
                any_violation = true;
            }
        }
    }

    if any_violation {
        anyhow::bail!("live fuzz found violations");
    }
    Ok(())
}

async fn test_cmd(config: PathBuf, scenarios: PathBuf) -> anyhow::Result<()> {
    let report = praxec_test::run_scenarios(&config, &scenarios).await?;
    print!("{}", report.render_text());
    if report.failed() {
        anyhow::bail!("scenario assertions failed");
    }
    Ok(())
}

async fn approvals_list(config_path: &PathBuf, all: bool) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let sink = build_audit_sink(&config)?;

    // VER-001 — `try_list_events` distinguishes a genuine read/IO error
    // (propagated via `?`) from "sink doesn't store events" (`Ok(None)`) and
    // "stored but the queue is empty" (`Ok(Some(vec![]))`). A failed read can
    // no longer masquerade as an empty approval queue.
    let events = match sink.try_list_events().await? {
        None => {
            let sink_kind = config
                .pointer("/audit/sink")
                .and_then(Value::as_str)
                .unwrap_or("stderr");
            match sink_kind {
                "stderr" | "none" => {
                    eprintln!("audit.sink is '{sink_kind}' — events are not stored.");
                    eprintln!("Switch to audit.sink: file to enable approvals tracking.");
                }
                _ => {
                    println!("No approval requests found.");
                }
            }
            return Ok(());
        }
        Some(events) => events,
    };
    if events.is_empty() {
        println!("No approval requests found.");
        return Ok(());
    }

    let mut pending = Vec::new();
    let mut resolved_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for event in &events {
        let _event_id = &event.id;

        if event.event_type == "human.approval.resolved" {
            if let Some(approval_id) = event.payload.get("approval_id").and_then(Value::as_str) {
                resolved_ids.insert(approval_id.to_string());
            }
        }

        if event.event_type == "human.approval.requested" {
            pending.push(event);
        }
    }

    for event in &pending {
        let id = &event.id;
        let status = if resolved_ids.contains(id) {
            "resolved"
        } else {
            "pending"
        };
        if !all && resolved_ids.contains(id) {
            continue;
        }
        println!("[{status}] {id}");
        println!(
            "  queue:      {}",
            event
                .payload
                .get("queue")
                .and_then(Value::as_str)
                .unwrap_or("?")
        );
        println!(
            "  transition: {}",
            event
                .payload
                .get("transition")
                .and_then(Value::as_str)
                .unwrap_or("?")
        );
        println!(
            "  workflow:   {}",
            event.workflow_id.as_deref().unwrap_or("?")
        );
        println!();
    }

    Ok(())
}

/// `cost report` — the value-prop savings report. Reads the same audit sink the
/// runtime writes `agent.completed` telemetry to, aggregates realized cost, and
/// prints the counterfactual savings-vs-ceiling. Read-only.
async fn cost_report_cmd(
    config_path: &PathBuf,
    workflow: Option<String>,
    since: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    use praxec_core::cost_report::{build_cost_report, render_human, ReportOptions};

    let config = load_config(config_path)?;
    let sink = build_audit_sink(&config)?;

    // VER-001 — distinguish a read error (propagated) from "sink doesn't store
    // events" (None) so an operator never mistakes a non-storing sink for $0.
    let events = match sink.try_list_events().await? {
        None => {
            let sink_kind = config
                .pointer("/audit/sink")
                .and_then(Value::as_str)
                .unwrap_or("stderr");
            eprintln!("audit.sink is '{sink_kind}' — events are not stored.");
            eprintln!("Switch to audit.sink: file to enable cost reporting.");
            return Ok(());
        }
        Some(events) => events,
    };

    let since = since.as_deref().map(parse_since).transpose()?;
    let opts = ReportOptions { workflow, since };
    let models = praxec_core::model_catalog::model_catalog().models;
    let report = build_cost_report(&events, &models, &opts);

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_human(&report));
    }
    Ok(())
}

/// `intent report` — the intent-index evidence layer. Aggregates the audit's
/// `outcome.recorded` events into per-(task_class, template) success-rate + mean
/// cost. Read-only; mirrors `cost report`'s sink-loading and degradation.
async fn intent_report_cmd(
    config_path: &PathBuf,
    task_class: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    use praxec_core::intent_index::{
        aggregate, observations_from_audit, render_human, IntentParams,
    };

    let config = load_config(config_path)?;
    let sink = build_audit_sink(&config)?;

    let events = match sink.try_list_events().await? {
        None => {
            let sink_kind = config
                .pointer("/audit/sink")
                .and_then(Value::as_str)
                .unwrap_or("stderr");
            eprintln!("audit.sink is '{sink_kind}' — events are not stored.");
            eprintln!("Switch to audit.sink: file to enable the intent index.");
            return Ok(());
        }
        Some(events) => events,
    };

    let models = praxec_core::model_catalog::model_catalog().models;
    let mut stats = aggregate(&observations_from_audit(&events, &models));
    if let Some(tc) = task_class.as_deref() {
        stats.retain(|s| s.task_class == tc);
    }
    let params = IntentParams::from_tuning();

    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        print!("{}", render_human(&stats, &params));
    }
    Ok(())
}

/// Load the current `models.yaml` chains for the observed affinities, each as an
/// ordered list of `provider:model` strings (base first). An affinity with no
/// matching `overrides:` key falls back to the `default:` chain.
fn load_current_chains(
    models_yaml: &str,
    affinities: &std::collections::BTreeSet<String>,
) -> anyhow::Result<std::collections::BTreeMap<String, Vec<String>>> {
    use praxec_core::model_resolver::config::{Binding, ModelsFile};
    let mf = ModelsFile::from_path(std::path::Path::new(models_yaml))
        .map_err(|e| anyhow::anyhow!("loading models.yaml {models_yaml}: {e}"))?;
    let model_string = |b: &Binding| format!("{}:{}", b.provider.display_name(), b.model);
    let mut chains = std::collections::BTreeMap::new();
    for aff in affinities {
        let bindings = mf
            .overrides
            .iter()
            .find(|(k, _)| k.to_string() == *aff)
            .map(|(_, v)| v)
            .unwrap_or(&mf.default);
        chains.insert(aff.clone(), bindings.iter().map(&model_string).collect());
    }
    Ok(chains)
}

/// `cost propose` — the governed slow loop. Aggregates per-(step, model)
/// pass-rate + cost from the audit, proposes conservative base-model changes,
/// prints the `models.yaml` edit + evidence, and (with `--request-approval`)
/// files each as a `human.approval.requested` event. Never edits `models.yaml`.
async fn cost_propose_cmd(
    config_path: &PathBuf,
    json: bool,
    request_approval: bool,
) -> anyhow::Result<()> {
    use praxec_core::audit::AuditEvent;
    use praxec_core::deescalation::{
        aggregate, apply_to_chain, observations_from_audit, propose, DeescalationParams,
    };

    let config = load_config(config_path)?;
    let sink = build_audit_sink(&config)?;

    let events = match sink.try_list_events().await? {
        None => {
            let sink_kind = config
                .pointer("/audit/sink")
                .and_then(Value::as_str)
                .unwrap_or("stderr");
            eprintln!("audit.sink is '{sink_kind}' — events are not stored.");
            eprintln!("Switch to audit.sink: file to enable de-escalation proposals.");
            return Ok(());
        }
        Some(events) => events,
    };

    let observations = observations_from_audit(&events);
    let stats = aggregate(&observations);

    let models_yaml = match config
        .pointer("/gateway/models_yaml")
        .and_then(Value::as_str)
    {
        Some(p) => p,
        None => {
            eprintln!("gateway.models_yaml is not configured — nothing to propose against.");
            return Ok(());
        }
    };
    let affinities: std::collections::BTreeSet<String> =
        stats.iter().map(|s| s.affinity.clone()).collect();
    let chains = load_current_chains(models_yaml, &affinities)?;

    let params = DeescalationParams::from_tuning();
    let proposals = propose(&stats, &chains, &params);

    // Pair each proposal with the concrete chain edit it implies.
    let edits: Vec<(&praxec_core::deescalation::Proposal, Vec<String>)> = proposals
        .iter()
        .map(|p| {
            let old = chains.get(&p.affinity).cloned().unwrap_or_default();
            (p, apply_to_chain(p, &old))
        })
        .collect();

    if json {
        let arr: Vec<Value> = edits
            .iter()
            .map(|(p, chain)| json!({ "proposal": p, "proposed_chain": chain }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&json!(arr))?);
    } else {
        print_proposals_human(&edits, stats.len());
    }

    if request_approval {
        for (p, chain) in &edits {
            let event = AuditEvent::new("human.approval.requested").with_payload(json!({
                "queue": "model-base-change",
                "transition": p.affinity,
                "direction": p.direction,
                "from_model": p.from_model,
                "to_model": p.to_model,
                "proposed_chain": chain,
                "rationale": p.rationale,
                "evidence": p,
            }));
            let id = event.id.clone();
            sink.record(event).await?;
            println!("filed approval request {id} for affinity '{}'", p.affinity);
        }
        if !edits.is_empty() {
            println!("Review with: praxec approvals list --config <gw>");
        }
    }
    Ok(())
}

/// Human rendering of the base-model proposals + their `models.yaml` edits.
fn print_proposals_human(
    edits: &[(&praxec_core::deescalation::Proposal, Vec<String>)],
    observation_groups: usize,
) {
    use praxec_core::deescalation::Direction;
    if edits.is_empty() {
        println!(
            "No base-model changes proposed — every base is at a healthy, well-priced \
             model (or the evidence is too thin). {observation_groups} (affinity, model) group(s) seen."
        );
        return;
    }
    println!(
        "Base-model proposals — {} change(s) from {observation_groups} (affinity, model) group(s):",
        edits.len()
    );
    for (p, chain) in edits {
        let tag = match p.direction {
            Direction::Lower => "LOWER",
            Direction::Raise => "RAISE",
        };
        println!(
            "\n  [{tag}] {}: {} → {}",
            p.affinity, p.from_model, p.to_model
        );
        let cost = |c: Option<f64>| {
            c.map(|v| format!("${v:.4}"))
                .unwrap_or_else(|| "n/a".into())
        };
        println!(
            "    base:      {:.0}% pass over {} runs, mean {}",
            p.base_pass_rate * 100.0,
            p.base_runs,
            cost(p.base_mean_cost_usd)
        );
        if p.candidate_runs > 0 {
            let save = p
                .savings_pct
                .map(|s| format!("  ({:+.0}% cost)", -s * 100.0))
                .unwrap_or_default();
            println!(
                "    candidate: {:.0}% pass over {} runs, mean {}{save}",
                p.candidate_pass_rate * 100.0,
                p.candidate_runs,
                cost(p.candidate_mean_cost_usd)
            );
        }
        println!("    rationale: {}", p.rationale);
        println!(
            "    models.yaml [{}] new chain: [{}]",
            p.affinity,
            chain.join(", ")
        );
    }
}

async fn approvals_resolve(config_path: &PathBuf, id: &str, outcome: &str) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let sink = build_audit_sink(&config)?;

    // Verify the approval exists. VER-001 — a genuine read error propagates
    // via `?`; `Ok(None)` means the sink doesn't retain events (stderr/null),
    // distinct from "stored, but this id isn't present". Fail-fast naming the
    // real cause rather than masquerading as a missing approval.
    let events = sink.try_list_events().await?.ok_or_else(|| {
        let sink_kind = config
            .pointer("/audit/sink")
            .and_then(Value::as_str)
            .unwrap_or("stderr");
        anyhow::anyhow!(
            "audit.sink is '{sink_kind}' — events are not stored, so approval \
             '{id}' cannot be verified or resolved. Switch to audit.sink: file."
        )
    })?;
    let found = events
        .iter()
        .any(|e| e.event_type == "human.approval.requested" && e.id == id);

    if !found {
        anyhow::bail!("approval event '{}' not found in audit log", id);
    }

    // Record a resolution event via the audit sink
    let resolution = praxec_core::audit::AuditEvent::new("human.approval.resolved").with_payload(
        serde_json::json!({
            "approval_id": id,
            "outcome": outcome,
        }),
    );

    sink.record(resolution).await?;

    println!("resolved approval {id} with outcome '{outcome}'");
    Ok(())
}

fn approvals_tail(config_path: &PathBuf) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let audit_dir = config
        .pointer("/audit/path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("audit.path is required for approvals tail"))?;

    let sink_kind = config
        .pointer("/audit/sink")
        .and_then(Value::as_str)
        .unwrap_or("stderr");
    if sink_kind != "file" {
        // Fail-fast: tailing is a no-op for non-file sinks, and returning
        // Ok(0) here would let a CI/automation wrapper treat the no-op as a
        // successful tail. Make the exit code reflect the unsupported config.
        anyhow::bail!("approvals tail requires audit.sink: file (current: {sink_kind})");
    }

    println!("tailing approvals from {}...", audit_dir);
    println!("(press Ctrl+C to stop)");

    // Track read position per log file so new rotated files are picked up.
    let mut file_offsets: std::collections::HashMap<PathBuf, u64> =
        std::collections::HashMap::new();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        tail_dir_once(audit_dir, &mut file_offsets, |event| {
            if event.get("event_type").and_then(Value::as_str) == Some("human.approval.requested") {
                let id = event.get("id").and_then(Value::as_str).unwrap_or("?");
                let queue = event
                    .get("payload")
                    .and_then(|p| p.get("queue"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let transition = event
                    .get("payload")
                    .and_then(|p| p.get("transition"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                println!("[{id}] queue={queue} transition={transition}");
            }
        });
    }
}

/// Append imported capabilities to the config's `proxy.expose` array. Doesn't
/// touch declared exposures — guards, reliability, etc. on those are
/// preserved.
fn with_imports(mut config: Value, imported: &CapabilityRegistry) -> Value {
    if imported.is_empty() {
        return config;
    }
    let root = match config.as_object_mut() {
        Some(m) => m,
        None => return config,
    };
    let proxy = root.entry("proxy".to_string()).or_insert_with(|| json!({}));
    let proxy_obj = match proxy.as_object_mut() {
        Some(m) => m,
        None => return Value::Object(root.clone()),
    };
    let expose = proxy_obj
        .entry("expose".to_string())
        .or_insert_with(|| json!([]));
    let arr = match expose.as_array_mut() {
        Some(a) => a,
        None => return Value::Object(root.clone()),
    };
    arr.extend(imported.as_proxy_exposures());
    Value::Object(root.clone())
}

/// Poll a directory of rotated log files for new lines. Tracks per-file byte
/// offsets in `file_offsets` so each call only reads appended bytes. Newly
/// appearing files (rotation events) are picked up automatically.
///
/// `handler` is called once per parsed JSON line; errors on individual lines
/// are silently skipped to keep the tail running.
fn tail_dir_once(
    dir: &str,
    file_offsets: &mut std::collections::HashMap<PathBuf, u64>,
    mut handler: impl FnMut(&Value),
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("log"))
        .collect();
    paths.sort();

    for path in paths {
        let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let offset = file_offsets.entry(path.clone()).or_insert(0);
        if file_len <= *offset {
            continue;
        }
        if let Ok(file) = std::fs::File::open(&path) {
            use std::io::{BufRead, BufReader, Seek, SeekFrom};
            let mut reader = BufReader::new(file);
            reader.seek(SeekFrom::Start(*offset)).ok();
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    if let Ok(event) = serde_json::from_str::<Value>(trimmed) {
                        handler(&event);
                    }
                }
                line.clear();
            }
            *offset = reader.stream_position().unwrap_or(file_len);
        }
    }
}

async fn inspect_workflow(config_path: &PathBuf, workflow_id: &str) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let store = build_workflow_store(&config)?;

    // Await directly — `run_cli` already runs inside the `#[tokio::main]`
    // runtime, so a nested `Runtime::new().block_on()` panics.
    let instance = store.load(workflow_id).await?;

    println!("Workflow: {}", instance.id);
    println!("  Definition:  {}", instance.definition_id);
    println!("  State:       {}", instance.state);
    println!("  Version:     {}", instance.version);
    println!("  Started at:  {}", instance.started_at.to_rfc3339());
    println!(
        "  Input:       {}",
        serde_json::to_string_pretty(&instance.input)?
    );
    println!(
        "  Context:     {}",
        serde_json::to_string_pretty(&instance.context)?
    );

    Ok(())
}

fn audit_tail(config_path: &PathBuf, filter: &Option<String>) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let audit_dir = config
        .pointer("/audit/path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("audit.path is required for audit tail"))?;

    println!("tailing audit events from {}...", audit_dir);
    if let Some(f) = filter {
        println!("filter: event_type == \"{f}\"");
    }
    println!("(press Ctrl+C to stop)");

    let mut file_offsets: std::collections::HashMap<PathBuf, u64> =
        std::collections::HashMap::new();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let filter_ref = filter.as_deref();
        tail_dir_once(audit_dir, &mut file_offsets, |event| {
            let event_type = event
                .get("event_type")
                .and_then(Value::as_str)
                .unwrap_or("");
            if let Some(f) = filter_ref {
                if event_type != f {
                    return;
                }
            }
            if let Ok(pretty) = serde_json::to_string_pretty(&event) {
                println!("{pretty}");
                println!("---");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ack_guards_used, aggregate_calls, build_audit_sink, build_evidence_store,
        build_runtime_for_orchestrate, build_workflow_store, drive_outcome_to_result,
        guard_durable_serve, headless_policy_from, is_ephemeral_path, maybe_enable_authoring,
        maybe_enable_sandbox, resolve_embedder, GatewayOverlays,
    };
    use praxec_agents::orchestrator::DriveOutcome;
    use praxec_agents::orchestrator::HeadlessPolicy;
    use praxec_core::sandbox::{BwrapProvider, SandboxProvider};
    use serde_json::json;
    use std::sync::Arc;

    // ── ADR-0009 — the headless `orchestrate` CLI ──────────────────────────

    #[test]
    fn policy_flag_decline_maps_to_decline() {
        assert_eq!(headless_policy_from("decline"), HeadlessPolicy::Decline);
    }

    // ── P5 — per-boot embedding opt-out ────────────────────────────────────

    // ── orchestrate give-up is loud and non-zero (not a silent exit 0) ─────

    #[test]
    fn drive_outcome_gaveup_errors_with_actionable_hint() {
        let detail = Some("stalled at status `running`; legal actions: [begin]".to_string());
        let err = drive_outcome_to_result("wf_1", 50, DriveOutcome::GaveUp, detail)
            .expect_err("GaveUp must be a non-zero error, not a silent Ok")
            .to_string();
        assert!(err.contains("gave up"), "explains it gave up: {err}");
        assert!(err.contains("running"), "includes the stall detail: {err}");
        assert!(
            err.contains("command") && err.contains("query") && err.contains("walk"),
            "points to the deterministic drivers: {err}"
        );
    }

    #[test]
    fn drive_outcome_succeeded_is_ok() {
        let r = drive_outcome_to_result(
            "wf_1",
            50,
            DriveOutcome::Resolved {
                status: "succeeded".into(),
                reason: None,
            },
            None,
        );
        assert!(r.is_ok(), "a succeeded mission exits 0");
    }

    #[test]
    fn drive_outcome_failed_errors_with_reason() {
        let err = drive_outcome_to_result(
            "wf_1",
            50,
            DriveOutcome::Resolved {
                status: "failed".into(),
                reason: Some("boom".into()),
            },
            None,
        )
        .expect_err("a failed mission must exit non-zero")
        .to_string();
        assert!(
            err.contains("failed") && err.contains("boom"),
            "names the failure: {err}"
        );
    }

    #[test]
    fn drive_outcome_max_steps_errors() {
        let err = drive_outcome_to_result("wf_1", 7, DriveOutcome::MaxSteps, None)
            .expect_err("MaxSteps must exit non-zero")
            .to_string();
        assert!(err.contains('7'), "names the step bound: {err}");
    }

    #[test]
    fn embeddings_disabled_via_config_forces_noop_embedder() {
        // `praxec.embeddings.enabled: false` skips the persisted embedding
        // choice (and its per-boot network calls) entirely → free lexical
        // discovery. Deterministic: this path never touches `load_choice`.
        let cfg = json!({ "praxec": { "embeddings": { "enabled": false } } });
        assert_eq!(resolve_embedder(&cfg).backend_name(), "noop");
    }

    #[test]
    fn policy_flag_auto_approve_maps_to_auto_approve() {
        assert_eq!(
            headless_policy_from("auto-approve"),
            HeadlessPolicy::AutoApprove
        );
    }

    #[test]
    fn policy_flag_unknown_value_defaults_to_auto_approve() {
        assert_eq!(
            headless_policy_from("nonsense"),
            HeadlessPolicy::AutoApprove
        );
    }

    #[tokio::test]
    async fn orchestrate_refuses_a_config_with_validation_errors() {
        let bad = json!({ "workflows": { "x": { "initialState": "nope", "states": {} } } });
        let result = build_runtime_for_orchestrate(&bad, &GatewayOverlays::default()).await;
        assert!(result.is_err());
    }

    // ── H5 — describe-ack guard detection (fail-fast oracle) ───────────────

    #[test]
    fn ack_guards_used_detects_both_guard_kinds_in_transitions_and_branches() {
        let config = json!({
            "workflows": {
                "wf": {
                    "states": {
                        "s1": {
                            "transitions": {
                                "t1": {
                                    "guards": [
                                        { "kind": "permission", "permission": "x" },
                                        { "kind": "guidance_acknowledged", "subject": "topic" }
                                    ]
                                },
                                "t2": {
                                    "branches": [
                                        { "when": { "kind": "script_acknowledged", "subject": "deploy" }, "target": "s2" }
                                    ]
                                }
                            }
                        }
                    }
                }
            }
        });
        assert_eq!(ack_guards_used(&config), (true, true));
    }

    #[test]
    fn ack_guards_used_is_false_when_absent() {
        let config = json!({
            "workflows": { "wf": { "states": { "s1": { "transitions": {
                "t": { "guards": [ { "kind": "permission", "permission": "x" } ] }
            } } } } }
        });
        assert_eq!(ack_guards_used(&config), (false, false));
    }

    // ── SPEC §8.4 — production wiring of the authoring write path ───────────

    fn null_audit() -> Arc<dyn praxec_core::audit::AuditSink> {
        Arc::new(praxec_core::audit::NullAuditSink)
    }

    #[test]
    fn authoring_off_leaves_the_registry_untouched() {
        let base = praxec_executors::default_registry(&json!({}));
        // No `praxec.authoring.write_enabled` → returns the same registry,
        // whose `registry` kind dispatches in its disabled (WRITE_DISABLED) form.
        let out = maybe_enable_authoring(&json!({}), base, &null_audit()).unwrap();
        assert!(
            out.get("registry").is_some(),
            "registry kind still dispatches"
        );
    }

    #[test]
    fn write_enabled_without_a_writable_repo_fails_loud() {
        let base = praxec_executors::default_registry(&json!({}));
        let config = json!({ "praxec": { "authoring": { "write_enabled": true } } });
        let Err(err) = maybe_enable_authoring(&config, base, &null_audit()) else {
            panic!("expected fail-loud error when no writable repo is declared");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("write_enabled"), "names the flag: {msg}");
        assert!(
            msg.contains("writable: true"),
            "tells the operator the fix: {msg}"
        );
    }

    // ── ADR-0006 — sandbox wiring (1c) ─────────────────────────────────────

    #[test]
    fn sandbox_absent_leaves_the_registry_untouched() {
        let base = praxec_executors::default_registry(&json!({}));
        let out = maybe_enable_sandbox(&json!({}), base).unwrap();
        assert!(out.get("script").is_some(), "script kind still dispatches");
    }

    #[test]
    fn unknown_sandbox_backend_fails_fast() {
        let base = praxec_executors::default_registry(&json!({}));
        let config = json!({ "praxec": { "execution": { "sandbox": "firejail" } } });
        let Err(err) = maybe_enable_sandbox(&config, base) else {
            panic!("unknown backend must fail fast");
        };
        assert!(format!("{err:#}").contains("unknown praxec.execution.sandbox"));
    }

    #[test]
    fn oci_without_an_image_fails_fast() {
        let base = praxec_executors::default_registry(&json!({}));
        let config = json!({ "praxec": { "execution": { "sandbox": "oci" } } });
        let Err(err) = maybe_enable_sandbox(&config, base) else {
            panic!("oci without an image must fail fast");
        };
        assert!(format!("{err:#}").contains("oci.image"));
    }

    #[test]
    fn require_confinement_without_a_provider_fails_fast() {
        let base = praxec_executors::default_registry(&json!({}));
        // No sandbox backend, but strict mode demanded → no script could run.
        let config = json!({ "praxec": { "execution": { "require_confinement": true } } });
        let Err(err) = maybe_enable_sandbox(&config, base) else {
            panic!("strict mode without a provider must fail fast");
        };
        assert!(format!("{err:#}").contains("require_confinement"));
    }

    #[test]
    fn bwrap_backend_overlays_script_when_usable() {
        // Environment-gated: only assert the overlay where bwrap is actually
        // usable; the misconfig fail-fast branches above are env-independent.
        if !BwrapProvider::new().preflight().usable {
            eprintln!("SKIP: bwrap not usable on this host");
            return;
        }
        let base = praxec_executors::default_registry(&json!({}));
        let config = json!({ "praxec": { "execution": { "sandbox": "bwrap" } } });
        let out = maybe_enable_sandbox(&config, base).expect("usable bwrap wires the overlay");
        assert!(out.get("script").is_some());
        assert!(out.get("noop").is_some(), "non-script kinds still resolve");
    }

    #[test]
    fn write_enabled_with_a_writable_repo_overlays_an_enabled_registry() {
        // A bare repo dir with just a manifest is enough for `from_repos` to
        // load (namespace + layout); the publish path itself is covered by the
        // RepoDefinitionStore + authoring_workflow_e2e tests.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("praxec.repo.yaml"),
            "schema: praxec.repo/v1\nname: mine\nnamespace: mine\nversion: 0.0.0\n",
        )
        .unwrap();
        let config = json!({
            "praxec": {
                "authoring": { "write_enabled": true },
                "_writableRepos": [{ "root": dir.path().display().to_string(), "push": false }],
            }
        });
        let base = praxec_executors::default_registry(&json!({}));
        let out = maybe_enable_authoring(&config, base, &null_audit())
            .expect("writable repo wires the enabled registry");
        assert!(out.get("registry").is_some(), "enabled registry overlaid");
        // The overlay preserves every other kind via its inner registry.
        assert!(
            out.get("noop").is_some(),
            "non-registry kinds still resolve"
        );
    }

    #[test]
    fn serve_guard_refuses_default_memory_store_and_stderr_audit() {
        // Empty config → store defaults to memory, audit to stderr: both ephemeral.
        let err = guard_durable_serve(&json!({})).unwrap_err().to_string();
        assert!(
            err.contains("store.kind"),
            "should flag the memory store: {err}"
        );
        assert!(
            err.contains("audit.sink"),
            "should flag the stderr audit: {err}"
        );
    }

    #[test]
    fn serve_guard_accepts_durable_store_and_audit() {
        // A genuinely durable config: sqlite + file audit on a PERSISTENT path.
        // (A /tmp path is now correctly rejected as ephemeral — see
        // guard_durable_serve_rejects_ephemeral_store_path.)
        let cfg = json!({
            "store": { "kind": "sqlite", "path": "/var/lib/praxec/x.db" },
            "audit": { "sink": "file", "path": "/var/lib/praxec/audit" }
        });
        assert!(guard_durable_serve(&cfg).is_ok());
    }

    #[test]
    fn serve_guard_allows_ephemeral_with_explicit_config_optin() {
        let cfg = json!({ "gateway": { "allow_ephemeral": true } });
        assert!(
            guard_durable_serve(&cfg).is_ok(),
            "explicit gateway.allow_ephemeral must override the durability guard"
        );
    }

    #[test]
    fn serve_guard_flags_file_store_governance_gap_and_stderr_audit() {
        // store.kind=file persists workflows but evidence/acknowledgments are
        // in-memory; with the default stderr audit, BOTH the governance-durability
        // gap and the audit sink are flagged.
        let cfg = json!({ "store": { "kind": "file", "path": "/tmp/s" } });
        let err = guard_durable_serve(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("evidence") && err.contains("acknowledgment"),
            "file store must flag the in-memory governance state: {err}"
        );
        assert!(
            err.contains("audit.sink"),
            "stderr audit must still be flagged: {err}"
        );
    }

    #[test]
    fn serve_guard_refuses_file_store_nondurable_governance() {
        // The durable/ephemeral split must be refused even when the audit sink
        // IS durable: workflows persist, evidence/acks don't.
        let cfg = json!({
            "store": { "kind": "file", "path": "/tmp/s" },
            "audit": { "sink": "file", "path": "/tmp/audit" }
        });
        let err = guard_durable_serve(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("evidence") && err.contains("acknowledgment"),
            "must name the lost governance state: {err}"
        );
        assert!(
            err.contains("sqlite"),
            "must point at the durable backend: {err}"
        );
    }

    #[test]
    fn build_evidence_store_rejects_unknown_kind_and_serves_known_kinds() {
        // No silent fallback for an unknown kind (mirrors build_workflow_store).
        let err = build_evidence_store(&json!({ "store": { "kind": "bogus" } }))
            .err()
            .expect("unknown store kind must fail fast")
            .to_string();
        assert!(
            err.contains("unknown store kind 'bogus'"),
            "unknown kind rejected by name: {err}"
        );
        // memory (default) + file build an ephemeral evidence store.
        assert!(build_evidence_store(&json!({})).is_ok());
        assert!(build_evidence_store(&json!({ "store": { "kind": "file" } })).is_ok());
    }

    #[test]
    fn build_audit_sink_accepts_stderr_and_default_but_rejects_legacy_stdout() {
        // The console sink writes to stderr; `stderr` is its config value and the
        // default when `audit.sink` is unset.
        assert!(build_audit_sink(&json!({ "audit": { "sink": "stderr" } })).is_ok());
        assert!(
            build_audit_sink(&json!({})).is_ok(),
            "absent audit.sink defaults to the stderr console sink"
        );

        // The pre-rename value is gone (greenfield, no compat shim) and fails fast
        // with a message that names the valid values.
        let err = build_audit_sink(&json!({ "audit": { "sink": "stdout" } }))
            .err()
            .expect("legacy stdout value must be rejected")
            .to_string();
        assert!(
            err.contains("unknown audit sink 'stdout'"),
            "legacy stdout value must be rejected by name: {err}"
        );
        assert!(
            err.contains("stderr, memory, file, none"),
            "the error must enumerate the valid sink values: {err}"
        );
    }

    #[test]
    fn build_workflow_store_rejects_unknown_kind() {
        let err = build_workflow_store(&json!({ "store": { "kind": "postgres" } }))
            .err()
            .expect("a removed/unknown store kind must fail fast")
            .to_string();
        assert!(
            err.contains("unknown store kind 'postgres'"),
            "removed postgres backend must be rejected by name: {err}"
        );
    }

    #[test]
    fn is_ephemeral_path_detects_temp_roots_only() {
        assert!(is_ephemeral_path("/tmp"));
        assert!(is_ephemeral_path("/tmp/fg/praxec.db"));
        assert!(is_ephemeral_path("/var/tmp/x"));
        assert!(is_ephemeral_path("/dev/shm/x"));
        assert!(!is_ephemeral_path("/home/u/.fg/praxec.db"));
        assert!(!is_ephemeral_path("/var/lib/praxec/praxec.db"));
        assert!(!is_ephemeral_path("/tmpfoo/x"), "/tmpfoo is not under /tmp");
    }

    #[test]
    fn guard_durable_serve_rejects_ephemeral_store_path() {
        // sqlite on /tmp → refused for serve (the durability trap that lost a flow).
        let err = guard_durable_serve(&json!({
            "store": { "kind": "sqlite", "path": "/tmp/fg/praxec.db" },
            "audit": { "sink": "file", "path": "/home/u/audit" }
        }))
        .expect_err("serve must refuse an ephemeral sqlite store path")
        .to_string();
        assert!(err.contains("ephemeral"), "{err}");

        // explicit opt-in overrides (dev/testing).
        assert!(guard_durable_serve(&json!({
            "gateway": { "allow_ephemeral": true },
            "store": { "kind": "sqlite", "path": "/tmp/fg/praxec.db" }
        }))
        .is_ok());

        // a persistent path is accepted.
        assert!(guard_durable_serve(&json!({
            "store": { "kind": "sqlite", "path": "/home/u/.fg/praxec.db" },
            "audit": { "sink": "file", "path": "/home/u/audit" }
        }))
        .is_ok());
    }

    // ── aggregate_calls — pure audit-record aggregation ─────────────────────

    #[test]
    fn aggregate_calls_per_model_totals() {
        let records = vec![
            json!({
                "event_type": "agent.invoked",
                "correlation_id": "c1",
                "model": "claude-sonnet",
                "affinity": "reasoning"
            }),
            json!({
                "event_type": "agent.completed",
                "correlation_id": "c1",
                "model": "claude-sonnet",
                "affinity": "reasoning",
                "cost_usd": 0.05,
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "duration_ms": 2000
            }),
            json!({
                "event_type": "agent.invoked",
                "correlation_id": "c2",
                "model": "claude-haiku",
                "affinity": "fast"
            }),
            json!({
                "event_type": "agent.completed",
                "correlation_id": "c2",
                "model": "claude-haiku",
                "affinity": "fast",
                "cost_usd": 0.01,
                "prompt_tokens": 200,
                "completion_tokens": 100,
                "duration_ms": 500
            }),
        ];

        let report = aggregate_calls(&records);

        // Per-model: claude-haiku comes first (BTreeMap ordering)
        let haiku = report.models.get("claude-haiku").unwrap();
        assert_eq!(haiku.call_count, 1);
        assert_eq!(haiku.total_tokens, 300);
        assert!((haiku.total_cost_usd - 0.01).abs() < 1e-9);
        assert_eq!(haiku.total_duration_ms, 500);
        assert!((haiku.avg_duration_ms() - 500.0).abs() < 1e-9);

        let sonnet = report.models.get("claude-sonnet").unwrap();
        assert_eq!(sonnet.call_count, 1);
        assert_eq!(sonnet.total_tokens, 150);
        assert!((sonnet.total_cost_usd - 0.05).abs() < 1e-9);
        assert_eq!(sonnet.total_duration_ms, 2000);
        assert!((sonnet.avg_duration_ms() - 2000.0).abs() < 1e-9);

        // Totals
        assert_eq!(report.totals.call_count, 2);
        assert_eq!(report.totals.total_tokens, 450);
        assert!((report.totals.total_cost_usd - 0.06).abs() < 1e-9);

        // No hangs — both invoked had matching completed
        assert!(report.hangs.is_empty());
    }

    #[test]
    fn aggregate_calls_flags_hang_when_invoked_without_completed() {
        let records = vec![
            json!({
                "event_type": "agent.invoked",
                "correlation_id": "hang-1",
                "model": "claude-sonnet",
                "affinity": "reasoning"
            }),
            json!({
                "event_type": "agent.invoked",
                "correlation_id": "ok-1",
                "model": "claude-haiku",
                "affinity": "fast"
            }),
            json!({
                "event_type": "agent.completed",
                "correlation_id": "ok-1",
                "model": "claude-haiku",
                "affinity": "fast",
                "cost_usd": 0.02,
                "prompt_tokens": 50,
                "completion_tokens": 25,
                "duration_ms": 300
            }),
        ];

        let report = aggregate_calls(&records);

        // One completed call for claude-haiku
        assert_eq!(report.totals.call_count, 1);

        // One hang: hang-1 was invoked but never completed
        assert_eq!(report.hangs.len(), 1);
        let hang = &report.hangs[0];
        assert_eq!(hang.correlation_id, "hang-1");
        assert_eq!(hang.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(hang.affinity.as_deref(), Some("reasoning"));
    }

    #[test]
    fn aggregate_calls_handles_empty_and_malformed() {
        // Empty input → empty report
        let report = aggregate_calls(&[]);
        assert!(report.models.is_empty());
        assert_eq!(report.totals.call_count, 0);
        assert!(report.hangs.is_empty());

        // Malformed/unrecognized events are skipped leniently
        let records = vec![
            json!({"event_type": "chain.failed", "correlation_id": "x"}),
            json!({"event_type": "agent.completed"}),
            json!({"foo": "bar"}),
        ];
        let report = aggregate_calls(&records);
        // agent.completed with no model → counted under "?"
        assert_eq!(report.totals.call_count, 1);
        assert!(report.models.contains_key("?"));
    }
}
