//! `praxec` — a control plane for AI coding agents.
//!
//! Praxec inverts the generative-first paradigm: instead of generating first
//! and governing after, it puts a deterministic *execution harness* first and
//! lets the model generate only inside it — never another agent. It runs a
//! model behind praxec as its **sole MCP server**: the model's only
//! tools are the two stable gateway tools — `praxec.query` (read) and
//! `praxec.command` (write) — so every action routes through governed
//! workflows. The runtime's built-in tool surface (filesystem, shell, etc.)
//! is replaced entirely.
//!
//! Generation happens *inside* the harness: workflows define the legal moves,
//! guards and evidence gate them, locks bound them. The human sees the same
//! HATEOAS link surface the model does — governance is transparent, and
//! nothing flows ungoverned.
//!
//! All modes are supported: TUI (default), headless, ACP (editor), and
//! agent configuration.

mod theme;

// Library surface (interpreter, agent_config, tui_config, sub_agent,
// praxec_mcp) lives in src/lib.rs so integration tests + the
// sub-agent spawner can reach them.
use praxec_core::model_resolver::{
    validate_model_source_exclusivity, verify_all_primary_bindings, ConfigSource, ModelsFile,
    Resolver,
};
use praxec_tui::interpreter::{
    AgentRegistry, LegacyAgentRegistry, McpToolCaller, YamlAgentRegistry,
};
use praxec_tui::{
    agent_config, keyring, lexicon as lexicon_mod, mcp_init, praxec_mcp, provider_keys, tui_config,
};

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use wisp::runtime_state::RuntimeState;

#[derive(Parser)]
// MAINTENANCE: keep this grouped listing in sync with the `Command` enum
// below. clap does not support per-group headings for subcommand
// listings, so this prose is the only grouped view in `--help`. Adding
// a variant without updating here will silently drop it from the
// curated section.
#[command(
    name = "px",
    version,
    about = "Praxec — a control plane for AI coding agents: harness-first, not generation-first",
    long_about = "Praxec is a control plane for AI coding agents — not another agent.\n\
\n\
It inverts the generative-first paradigm. Where today's tools generate first and\n\
govern later, Praxec puts a deterministic execution harness first: the model\n\
generates only inside governed workflows, routed through praxec as its sole\n\
MCP server. Workflows define the legal moves; guards and evidence gate them.\n\
Nothing flows ungoverned.\n\
\n\
Run a governed agent:\n\
  (default)   Interactive TUI\n\
  headless    Run a single prompt non-interactively\n\
  acp         Start ACP server for editor integration\n\
  walk        Drive a workflow via the deterministic interpreter\n\
\n\
Configure the harness:\n\
  agent       Manage agent configurations\n\
  validate-models-config\n\
              Validate an models.yaml at any path; JSON envelope on stdout\n\
  migrate-agents-from-cli\n\
              Migrate v0.2 --agent flags to a v0.3 models.yaml\n\
  set-provider-keys\n\
              Write provider API keys to ~/.praxec/providers.env\n\
\n\
Lexicon management:\n\
  lexicon define  Define or redefine a term\n\
  lexicon alias   Add an alias to an existing term\n\
  lexicon cancel  Drop a PENDING_DEFINITION placeholder\n\
  lexicon list    List all lexicon entries\n\
  lexicon pending List PENDING_DEFINITION placeholders\n\
\n\
Diagnostics & generators:\n\
  doctor      Pre-flight checks before walk\n\
  mcp init    Generate .mcp.json (and optional editor configs)\n\
  completions Print a shell completion script to stdout\n\
  man         Render the man page to stdout (roff format)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run a single prompt non-interactively
    #[command(
        next_help_heading = "Run a governed agent",
        long_about = "Run a single prompt non-interactively. Praxec is wired as the sole MCP \
                      server, so every model action goes through governed workflows."
    )]
    Headless(aether_cli::headless::HeadlessArgs),
    /// Start the ACP server (for editor integration)
    #[command(
        next_help_heading = "Run a governed agent",
        long_about = "Start the ACP (Agent Client Protocol) server for editor integration. \
                      The TUI spawns this mode as a subprocess; editors connect via ACP."
    )]
    Acp(aether_cli::acp::AcpArgs),
    // NOTE: aether-agent-cli removed its `agent` subcommand module as of
    // 0.7.9. We track the latest aether, so the `praxec agent
    // new|list|remove` passthrough is dropped accordingly; agent/model
    // configuration is managed via `models.yaml` + `validate-models-config`.
    /// Walk a Praxec workflow to completion using the deterministic
    /// interpreter (SPEC §21). Spawns isolated sub-agents per delegate
    /// state; auto-advances states with no delegate.
    #[command(
        next_help_heading = "Run a governed agent",
        long_about = "Walk a Praxec workflow end-to-end through the deterministic interpreter \
                      (SPEC §21). Spawns isolated sub-agents per `delegate:` state; \
                      auto-advances states with no delegate. Returns the final blackboard JSON \
                      on stdout.\n\n\
                      Example:\n  \
                      px walk --workflow swe_agent \\\n    \
                      --input '{\"issue\":\"add timeout to RegistryExecutor\"}' \\\n    \
                      --agent planning=anthropic/claude-sonnet-4 \\\n    \
                      --agent editing=anthropic/claude-haiku-4-5-20251001"
    )]
    Walk(WalkArgs),
    /// Pre-flight checks for `px walk` — binary discovery, config
    /// resolution, workflow declared, agent API keys, script file URIs.
    /// Exits 0 if all pass; 1 if any fail. Run before `walk` to catch
    /// env / config issues before the workflow starts.
    #[command(
        next_help_heading = "Diagnostics & generators",
        long_about = "Pre-flight checks before `px walk` — binary discovery, config \
                      resolution, workflow declared, agent API keys reachable, script file URIs \
                      hash-verified. Exits 0 if all pass; 1 if any fail."
    )]
    Doctor(DoctorCliArgs),
    /// MCP client config generators.
    #[command(
        subcommand,
        next_help_heading = "Diagnostics & generators",
        long_about = "MCP client config generators. `mcp init` writes .mcp.json (and optional \
                      editor-specific outputs) so MCP hosts see praxec as the sole MCP server."
    )]
    Mcp(McpCommand),
    /// Validate an `models.yaml` file at an arbitrary path. Emits a
    /// JSON envelope `{ok, summary, error}` on stdout; exits 0 on
    /// pass, 1 on fail. Used by the meta library's
    /// `cap.implement.write-agents-config` for post-write round-trip
    /// validation (FMECA U3) when the operator's target path is
    /// outside the resolver's standard `.praxec/models.yaml` /
    /// `~/.praxec/models.yaml` lookup.
    #[command(
        next_help_heading = "Configure the harness",
        long_about = "Validate an `models.yaml` file at an arbitrary path. Emits a JSON \
                      envelope {ok, summary, error} on stdout; exits 0 on pass, 1 on fail."
    )]
    ValidateAgentsConfig(ValidateAgentsConfigArgs),
    /// Migrate v0.2 `--agent NAME=PROVIDER/MODEL` flags to a v0.3
    /// `models.yaml`. Operators with many workflows still on the legacy
    /// CLI path can run this once + commit the file. Names must parse
    /// as a valid `<affinity>` | `<tier>` | `<affinity>-<tier>` or the
    /// literal `default`.
    #[command(
        next_help_heading = "Configure the harness",
        long_about = "Migrate v0.2 --agent NAME=PROVIDER/MODEL flags to a v0.3 models.yaml. \
                      Operators with many workflows still on the legacy CLI path can run this \
                      once + commit the file."
    )]
    MigrateAgentsFromCli(MigrateAgentsArgs),
    /// Write provider API keys to ~/.praxec/providers.env
    /// (override via $PRAXEC_PROVIDER_KEYS_FILE). Loaded into env at
    /// px startup; existing env vars take precedence.
    /// Supported providers: anthropic, openai, openrouter, bedrock,
    /// gemini.
    #[command(
        next_help_heading = "Configure the harness",
        long_about = "Write provider API keys to ~/.praxec/providers.env (override via \
                      $PRAXEC_PROVIDER_KEYS_FILE). Loaded into env at px startup; \
                      existing env vars take precedence. Without flags, interactively walks all \
                      supported providers (anthropic, openai, openrouter, bedrock, gemini)."
    )]
    SetProviderKeys(provider_keys::SetProviderKeysArgs),
    /// Out-of-band lexicon management: define, alias, cancel, list, pending.
    ///
    /// Mutations (define/alias/cancel) update an in-memory overlay for the
    /// current run; they do not persist to `praxec.yaml` automatically.
    /// `list` and `pending` read from the resolved config file.
    #[command(
        subcommand,
        next_help_heading = "Lexicon management",
        long_about = "Out-of-band lexicon management. Subcommands: define (create/overwrite a \
                      term), alias (add an alias), cancel (drop a pending placeholder), list \
                      (print all entries), pending (print PENDING_DEFINITION entries). Reads \
                      config from --config or $PRAXEC_CONFIG."
    )]
    Lexicon(lexicon_mod::LexiconCmd),
    /// Print a shell completion script to stdout. Source it from your
    /// shell rc to get tab-completion for every praxec subcommand
    /// and flag. Example:
    ///   px completions bash > ~/.local/share/bash-completion/completions/praxec
    #[command(
        next_help_heading = "Diagnostics & generators",
        long_about = "Print a shell completion script to stdout. Source it from your shell rc \
                      to tab-complete subcommands and flags."
    )]
    Completions(CompletionsArgs),
    /// Render the man page to stdout (roff format). Install to a
    /// MANPATH directory to enable `man praxec`. Example:
    ///   px man | sudo tee /usr/local/share/man/man1/praxec.1
    #[command(
        next_help_heading = "Diagnostics & generators",
        long_about = "Render the man page to stdout (roff format). Install to a MANPATH \
                      directory to enable `man praxec`."
    )]
    Man,
}

#[derive(clap::Args, Debug)]
pub struct MigrateAgentsArgs {
    /// Agent specs (same shape as `walk --agent`). Pass one or more.
    #[arg(long = "agent", required = true)]
    pub agents: Vec<String>,
    /// Output path. Default writes to `.praxec/models.yaml`.
    #[arg(long, default_value = ".praxec/models.yaml")]
    pub out: PathBuf,
    /// Print the generated YAML to stdout instead of writing to disk.
    /// Useful for diffing / piping through other tools before commit.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(clap::Args, Debug)]
pub struct ValidateAgentsConfigArgs {
    /// Path to the `models.yaml` file to validate.
    pub path: PathBuf,
}

/// `px mcp <subcommand>` — operator-facing MCP wiring helpers.
/// Today: `init` generates `.mcp.json` (and optionally `.cursor/mcp.json`,
/// `claude_desktop_config.json`) so editors connecting via ACP — or any
/// MCP host like Cursor / Claude Desktop / Claude Code — see praxec as
/// the sole MCP server.
#[derive(Subcommand)]
enum McpCommand {
    /// Generate MCP client config files for the project (`.mcp.json` plus
    /// optional editor-specific outputs via `--cursor` / `--claude-desktop`).
    Init(mcp_init::McpInitArgs),
}

#[derive(clap::Args, Debug)]
pub struct CompletionsArgs {
    /// Shell to generate completions for (bash, zsh, fish, powershell, elvish).
    pub shell: Shell,
}

#[derive(clap::Args, Debug)]
pub struct DoctorCliArgs {
    /// Path to the gateway YAML config (defaults to $PRAXEC_CONFIG).
    #[arg(long)]
    pub config: Option<String>,
    /// Workflow id that walk will run — checked against declared workflows.
    #[arg(long)]
    pub workflow: Option<String>,
    /// Agent specs (same as walk's --agent). Each agent's provider's
    /// API key env var presence is verified.
    #[arg(long = "agent")]
    pub agents: Vec<String>,
    /// Re-probe every binding in models.yaml against its provider's
    /// `/v1/models` endpoint and write the result to
    /// `~/.praxec/agents-last-probe.json`. Without this flag,
    /// doctor only reads the cache and surfaces stale-since-N-days
    /// warnings (cheap, no network).
    #[arg(long, default_value_t = false)]
    pub refresh_agents: bool,
}

/// CLI args for `px walk` — drives a workflow end-to-end through
/// the deterministic interpreter.
#[derive(clap::Args, Debug)]
pub struct WalkArgs {
    /// Workflow id to start (e.g. `swe_agent`). Must match a workflow
    /// declared in the Praxec config.
    #[arg(long)]
    pub workflow: String,

    /// JSON object passed as `input` to `workflow.start`.
    #[arg(long, default_value = "{}")]
    pub input: String,

    /// Agent config in `name=provider/model` form. Repeat for each
    /// sub-agent referenced by `delegate:` fields in the workflow.
    /// Example: `--agent planning=anthropic/claude-sonnet-4 --agent editing=anthropic/claude-haiku-4-5-20251001`
    ///
    /// **Deprecated in v0.3 in favor of `models.yaml`** — prefer the
    /// file-based config for per-affinity overrides + feature toggles.
    /// Mutually exclusive with `--agents-config` and on-disk
    /// `.praxec/models.yaml` / `~/.praxec/models.yaml`.
    #[arg(long = "agent")]
    pub agents: Vec<String>,

    /// Path to an `models.yaml` file (v0.3+). When unset, the resolver
    /// looks for `.praxec/models.yaml` (project) then
    /// `~/.praxec/models.yaml` (user). Setting this AND
    /// `--agent` flags is a startup error (FMECA T1 mitigation).
    #[arg(long, env = "PRAXEC_AGENTS_CONFIG")]
    pub agents_config: Option<PathBuf>,

    /// Hard ceiling on wall-clock seconds per sub-agent. No default by
    /// design — operators must declare their tolerance for orphan
    /// sub-agents.
    #[arg(long)]
    pub max_sub_agent_seconds: Option<u64>,

    /// **Advisory** tool-call hint per sub-agent (no default; must be
    /// set explicitly so operators declare a number they consider
    /// reasonable). Currently logged + surfaced for observability;
    /// not enforced — aether's headless API has no per-tool-call
    /// hook, so the enforced cap is `--max-sub-agent-seconds`. The
    /// hint will be enforced once aether exposes a step callback;
    /// the CLI contract stays valid either way.
    #[arg(long)]
    pub max_sub_agent_steps: Option<usize>,

    /// Warning threshold for blackboard size (serialized JSON bytes).
    /// Defaults to 16 KiB. Exceeding this logs a warning but does not
    /// block the spawn.
    #[arg(long)]
    pub max_blackboard_bytes: Option<usize>,

    /// Path to the praxec.yaml config used by the spawned
    /// `praxec` child process. Becomes `PRAXEC_CONFIG` on the
    /// child env. When unset, praxec falls back to its own
    /// resolution (cwd `praxec.yaml`).
    #[arg(long)]
    pub config: Option<String>,
}

fn run_completions(args: CompletionsArgs) -> ExitCode {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(args.shell, &mut cmd, name, &mut std::io::stdout());
    ExitCode::SUCCESS
}

fn run_man() -> anyhow::Result<ExitCode> {
    let cmd = Cli::command();
    let man = clap_mangen::Man::new(cmd);
    man.render(&mut std::io::stdout())
        .map_err(|e| anyhow::anyhow!("man render failed: {e}"))?;
    Ok(ExitCode::SUCCESS)
}

/// Only the agent-running entry points need the platform keyring: the upstream
/// ACP runtime eagerly initializes D-Bus Secret Service for OAuth credential
/// storage. Utility / generator / diagnostic commands (`doctor`, `completions`,
/// `man`, `set-provider-keys`, `lexicon`, validate/migrate, `mcp init`) must run
/// WITHOUT it — they have no business failing on a missing keyring, notably in
/// headless CI. Exhaustive on purpose: a new command must consciously opt in.
fn command_needs_keyring(cmd: &Option<Command>) -> bool {
    match cmd {
        // Bare `praxec` launches the agent TUI; the rest run agents.
        None | Some(Command::Acp(_)) | Some(Command::Headless(_)) | Some(Command::Walk(_)) => true,
        Some(Command::Doctor(_))
        | Some(Command::Mcp(_))
        | Some(Command::ValidateAgentsConfig(_))
        | Some(Command::MigrateAgentsFromCli(_))
        | Some(Command::SetProviderKeys(_))
        | Some(Command::Lexicon(_))
        | Some(Command::Completions(_))
        | Some(Command::Man) => false,
    }
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let cli = Cli::parse();

    // Pre-flight: ensure the platform keyring service is running, but ONLY for
    // the agent-running commands that actually need it (the upstream ACP runtime
    // eagerly initializes D-Bus Secret Service for OAuth). Gating this here keeps
    // utility/generator commands working headless. See `keyring.rs`. No-op on
    // macOS and Windows.
    if command_needs_keyring(&cli.command) {
        keyring::ensure_keyring_available();
    }
    provider_keys::load_into_env_if_present();

    match cli.command {
        None => run_tui().await,
        Some(Command::Headless(args)) => run_headless(args).await,
        Some(Command::Acp(args)) => run_acp(args).await,
        Some(Command::Walk(args)) => run_walk(args).await,
        Some(Command::Doctor(args)) => run_doctor(args).await,
        Some(Command::Mcp(McpCommand::Init(args))) => {
            mcp_init::run_init(&args).map(|_| ExitCode::SUCCESS)
        }
        Some(Command::ValidateAgentsConfig(args)) => Ok(run_validate_agents_config(&args.path)),
        Some(Command::MigrateAgentsFromCli(args)) => run_migrate_agents_from_cli(args),
        Some(Command::SetProviderKeys(args)) => provider_keys::run(args),
        Some(Command::Lexicon(cmd)) => lexicon_mod::run(cmd),
        Some(Command::Completions(args)) => Ok(run_completions(args)),
        Some(Command::Man) => run_man(),
    }
}

fn run_migrate_agents_from_cli(args: MigrateAgentsArgs) -> Result<ExitCode> {
    let yaml = praxec_tui::migrate::cli_args_to_yaml(&args.agents)
        .map_err(|e| anyhow::anyhow!("migration failed: {e}"))?;
    if args.dry_run {
        print!("{yaml}");
        return Ok(ExitCode::SUCCESS);
    }
    if args.out.exists() {
        anyhow::bail!(
            "{} already exists. Move it aside or pass --dry-run to inspect the proposed output \
             before overwriting.",
            args.out.display()
        );
    }
    praxec_tui::migrate::write_atomic(&yaml, &args.out)
        .map_err(|e| anyhow::anyhow!("write {} failed: {e}", args.out.display()))?;
    println!("wrote {} ({} bytes)", args.out.display(), yaml.len());
    Ok(ExitCode::SUCCESS)
}

/// Walk a workflow to completion via the deterministic interpreter
/// (SPEC §21). Validates args, builds the agent registry, spawns
/// praxec as an rmcp child process, starts the workflow, then
/// drives it through `walk_workflow` against the real
/// `AetherSubAgentSpawner`.
async fn run_walk(args: WalkArgs) -> Result<ExitCode> {
    let tui_cfg = tui_config::TuiConfig::from_cli(
        args.max_sub_agent_seconds,
        args.max_sub_agent_steps,
        args.max_blackboard_bytes,
    )?;

    let registry: Box<dyn AgentRegistry> = build_agent_registry(&args).await?;

    let input_value: serde_json::Value = serde_json::from_str(&args.input)
        .map_err(|e| anyhow::anyhow!("--input is not valid JSON: {e}"))?;

    let spawner = praxec_tui::sub_agent::AetherSubAgentSpawner::new(tui_cfg);

    let caller = praxec_tui::mcp_caller::PraxecChildCaller::spawn(
        args.config.as_deref(),
        std::collections::HashMap::new(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("spawning praxec child for walk: {e}"))?;

    // Start the workflow to acquire a workflowId.
    let start_resp = caller
        .call(
            "praxec.command",
            serde_json::json!({ "definitionId": args.workflow, "input": input_value }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("praxec.command (start) failed: {e}"))?;

    let workflow_id = start_resp
        .pointer("/workflow/id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!("praxec.command (start) response missing /workflow/id: {start_resp}")
        })?
        .to_string();

    tracing::info!(workflow = %args.workflow, %workflow_id, "walking workflow");

    let final_ctx =
        praxec_tui::interpreter::walk_workflow(&caller, &spawner, &workflow_id, registry.as_ref())
            .await
            .map_err(|e| anyhow::anyhow!("walk failed: {e}"))?;

    println!(
        "{}",
        serde_json::to_string_pretty(&final_ctx).unwrap_or_else(|_| final_ctx.to_string())
    );

    Ok(ExitCode::SUCCESS)
}

/// Resolve the agent config source and build a registry the interpreter
/// can use. Precedence (highest wins):
///
/// 1. `--agents-config <PATH>` (or `$PRAXEC_AGENTS_CONFIG`).
/// 2. `.praxec/models.yaml` in the current working directory (project).
/// 3. `~/.praxec/models.yaml` (user — `dirs::home_dir()`).
/// 4. `--agent name=provider/model` CLI flags (deprecated v0.2 path).
///
/// Mutual exclusion (FMECA T1 mitigation): if any models.yaml file is
/// resolvable AND `--agent` flags are present, return a startup error
/// rather than silently picking one source.
///
/// On YAML path success, runs `verify_all_primary_bindings` for the
/// eager auth preflight (FMECA U2). The preflight honors
/// `PRAXEC_SKIP_PREFLIGHT=1`.
async fn build_agent_registry(args: &WalkArgs) -> Result<Box<dyn AgentRegistry>> {
    let yaml_path: Option<(PathBuf, ConfigSource)> = if let Some(p) = &args.agents_config {
        Some((p.clone(), ConfigSource::Project(p.clone())))
    } else {
        let project = std::path::Path::new(".praxec").join("models.yaml");
        if project.exists() {
            Some((project.clone(), ConfigSource::Project(project)))
        } else if let Some(user_dir) = dirs::home_dir() {
            let user = user_dir.join(".praxec").join("models.yaml");
            if user.exists() {
                Some((user.clone(), ConfigSource::User(user)))
            } else {
                None
            }
        } else {
            None
        }
    };

    validate_model_source_exclusivity(yaml_path.is_some(), !args.agents.is_empty())
        .map_err(|e| anyhow::anyhow!(e))?;

    if let Some((path, source)) = yaml_path {
        let file = ModelsFile::from_path(&path)
            .map_err(|e| anyhow::anyhow!("failed to load {}: {e}", path.display()))?;
        let resolver = Resolver::from_loaded(file, source);
        // FMECA U2: eager auth preflight on every distinct primary
        // binding declared in the file. Hard error on 401/403 or
        // missing-credential; warn-and-continue on transient infra.
        if let Err(errors) = verify_all_primary_bindings(&resolver).await {
            let summary: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
            anyhow::bail!(
                "preflight failed for {} primary binding(s):\n  - {}\n\
                 Set the missing credential(s) or pass \
                 `PRAXEC_SKIP_PREFLIGHT=1` to bypass.",
                errors.len(),
                summary.join("\n  - ")
            );
        }
        return Ok(Box::new(YamlAgentRegistry::new(resolver)));
    }

    // Legacy CLI path (deprecated).
    if !args.agents.is_empty() {
        tracing::warn!(
            "--agent CLI flag is deprecated; prefer .praxec/models.yaml (see \
             /guides/agent-config.mdx)"
        );
    }
    let agents = agent_config::build_registry(&args.agents)
        .map_err(|e| anyhow::anyhow!("agent config parse error: {e}"))?;
    Ok(Box::new(LegacyAgentRegistry::new(agents)))
}

/// TUI mode (default) — interactive terminal with Praxec branding.
///
/// Spawns `px acp` as a subprocess and connects via ACP.
/// The ACP subprocess inherits the sole-MCP wiring so the model
/// always routes through governed workflows.
async fn run_tui() -> Result<ExitCode> {
    let log_dir = resolve_log_dir();
    // Best-effort mkdir — wisp creates the file inside, but the dir must
    // exist. We don't fail if mkdir fails; logging falls back to stderr.
    let _ = std::fs::create_dir_all(&log_dir);
    wisp::setup_logging(Some(&log_dir.to_string_lossy()));

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("praxec"));
    let acp_command = format!("{} acp", exe.display());

    let mut state: RuntimeState = RuntimeState::new(&acp_command)
        .await
        .map_err(|e| anyhow::anyhow!("TUI initialization failed: {e}"))?;

    // Branding
    state.agent_name = "Praxec".into();
    state.theme = theme::praxec_theme();

    wisp::run_with_state(state)
        .await
        .map_err(|e| anyhow::anyhow!("TUI error: {e}"))?;

    Ok(ExitCode::SUCCESS)
}

/// SPEC §B.4 — resolve the TUI's log directory. Order:
/// 1. `$PRAXEC_LOG_DIR` (operator override).
/// 2. `~/.praxec/logs` — the single praxec home dir on every platform
///    (`dirs::home_dir().join(".praxec/logs")`), consolidated with all
///    other on-disk state.
/// 3. `./praxec-logs` as last-resort fallback (if `dirs::home_dir`
///    returns `None`, e.g. in some sandboxed CI environments).
///
/// Exposed as a free function so tests can exercise it directly.
pub fn resolve_log_dir() -> PathBuf {
    if let Ok(override_path) = std::env::var("PRAXEC_LOG_DIR") {
        if !override_path.trim().is_empty() {
            return PathBuf::from(override_path);
        }
    }
    match dirs::home_dir() {
        Some(home) => home.join(".praxec").join("logs"),
        None => PathBuf::from("praxec-logs"),
    }
}

/// Headless mode — run a single prompt, output result.
///
/// Injects praxec as the **sole MCP server**, replacing
/// aether's built-in tool surface entirely.
async fn run_headless(mut args: aether_cli::headless::HeadlessArgs) -> Result<ExitCode> {
    // SPEC §B.3 — fail fast at startup if `MCP_PRAXEC_PATH` is set to a
    // non-existent file. A bare PATH fallback is still permitted (silent +
    // logged) so end-users don't need the env var in the common install case.
    praxec_mcp::set_as_sole_mcp(&mut args.mcp_config)?;

    aether_cli::headless::run_headless(args)
        .await
        .map(|_| ExitCode::SUCCESS)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// ACP mode — Agent Client Protocol server for editor integration.
///
/// The TUI spawns this mode as a subprocess. Editors connect via ACP.
/// ACP resolves its MCP config from the agent's settings or `.mcp.json`,
/// not from CLI args, so the sole-MCP wiring happens through the agent
/// configuration rather than programmatic injection.
async fn run_acp(args: aether_cli::acp::AcpArgs) -> Result<ExitCode> {
    aether_cli::acp::run_acp(args)
        .await
        .map(|_| ExitCode::SUCCESS)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// `px doctor` — pre-flight checks. Exits 0 if all pass; 1 if any fail.
async fn run_doctor(args: DoctorCliArgs) -> Result<ExitCode> {
    let doctor_args = praxec_tui::doctor::DoctorArgs {
        config: args.config,
        workflow: args.workflow,
        agents: args.agents,
        refresh_agents: args.refresh_agents,
    };
    let results = praxec_tui::doctor::run_doctor(&doctor_args).await;
    print!("{}", praxec_tui::doctor::render_results(&results));
    if praxec_tui::doctor::count_failures(&results) > 0 {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// `px validate-models-config <path>` — load the file via the
/// same `ModelsFile::from_path` the resolver uses at workflow start
/// and emit the typed envelope on stdout. Used by
/// `cap.implement.write-agents-config` for post-write round-trip
/// validation (FMECA U3).
fn run_validate_agents_config(path: &std::path::Path) -> ExitCode {
    let envelope = praxec_core::model_resolver::validate_models_config_envelope(path);
    println!("{}", envelope);
    if envelope["ok"].as_bool().unwrap_or(false) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
