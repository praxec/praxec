//! Sub-agent spawner. Wraps `aether_cli::headless::run_headless` to run
//! an isolated model session whose only tools are the seven Praxec MCP
//! tools. The session inherits the workflow's `guidance` (goal +
//! instructions) and the blackboard as the user prompt the sub-agent
//! acts on.
//!
//! Lifecycle (per SPEC §21):
//!
//! 1. Build the sub-agent prompt from `response.guidance` +
//!    `response.context` (already done by the interpreter; passed in as
//!    `system_prompt`).
//! 2. Warn (don't block) if context exceeds `max_blackboard_bytes`.
//! 3. Spawn an Aether headless session with `provider:model` + Praxec
//!    MCP config (Praxec is the sole MCP server the sub-agent can see).
//! 4. Wait for the session to either issue `praxec.command` (advancing
//!    the workflow) or hit the operator-configured timeout.
//! 5. Return `Ok(())` on natural completion (the LLM emitted
//!    `AgentMessage::Done`). The interpreter then re-fetches `praxec.query`
//!    and compares `version` to confirm the sub-agent actually advanced
//!    the workflow.
//!
//! The Aether `run_headless` API is run-to-completion: it doesn't expose
//! a per-tool-call hook. That's fine — the interpreter detects whether
//! the sub-agent actually advanced the workflow by re-fetching
//! `praxec.query` after each spawn and comparing `version`. A spawn that
//! returns `Ok` but didn't advance is treated as a soft timeout (the
//! retry path covers it).
//!
//! ## v1 limitations
//!
//! - **Step limits are not enforced.** `aether_cli::headless::run_headless`
//!   has no built-in step counter; the LLM runs until it emits
//!   `AgentMessage::Done` OR the wall-clock timeout fires. The
//!   `max_sub_agent_steps` field on `TuiConfig` is currently surfaced
//!   only as a logged hint; enforcement would require intercepting
//!   `ToolCall` events (deferred to v2).
//! - **Sub-agent stdout goes to the parent process's stdout.** Aether's
//!   `CliOutputFormat::Text` streams the agent's reasoning text. For
//!   parallel sub-agent fan-out (future), output multiplexing will need
//!   work — for now, sub-agents run sequentially (one delegate state at
//!   a time) so stdout interleaving is not an issue.

use std::time::Duration;

use aether_cli::headless::{OutputFormat, RunConfig, run::run as run_aether_headless};
use aether_cli::mcp_config_args::McpConfigArgs;
use aether_core::agent_spec::AgentSpec;
use async_trait::async_trait;
use tokio::time::timeout;

use crate::interpreter::{InterpreterError, ResolvedAgent, SubAgentSpawner};
use crate::praxec_mcp;
use crate::tui_config::TuiConfig;
use praxec_core::model_resolver::{OpenAIFeatures, ProviderFeatures};

// Feature-toggle → reasoning-effort translation lives alongside its one
// consumer (this external-spine spawner); the in-step `kind: agent` executor
// takes the effort as a config string and needs none of it.
pub use crate::reasoning::features_to_reasoning_effort;

/// Production sub-agent spawner. Holds a reference to the TUI config so
/// each spawn applies the operator's timeout / blackboard caps.
pub struct AetherSubAgentSpawner {
    pub config: TuiConfig,
}

impl AetherSubAgentSpawner {
    pub fn new(config: TuiConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl SubAgentSpawner for AetherSubAgentSpawner {
    async fn spawn_and_wait(
        &self,
        agent: &ResolvedAgent,
        system_prompt: &str,
        workflow_response: &serde_json::Value,
    ) -> Result<(), InterpreterError> {
        // Pre-spawn: warn on oversized blackboard. Don't block; the
        // timeout catches genuine overload while small overshoot is
        // tolerable.
        let context_size = workflow_response
            .get("context")
            .map(|c| c.to_string().len())
            .unwrap_or(0);
        if context_size > self.config.max_blackboard_bytes {
            tracing::warn!(
                agent = %agent.label,
                provider = %agent.provider,
                model = %agent.model,
                context_size,
                threshold = self.config.max_blackboard_bytes,
                "sub-agent context exceeds blackboard-size threshold; consider \
                 scoping the upstream output mapping to drop fields the \
                 downstream agent doesn't need"
            );
        }

        // FMECA T3: translate the typed per-provider feature toggles to
        // aether's effective `ReasoningEffort`. v0.3.0 parsed + stored
        // these but didn't apply them at spawn time; v0.3.1 wires them
        // through.
        let reasoning_effort = features_to_reasoning_effort(&agent.features);
        if let ProviderFeatures::OpenAI(OpenAIFeatures {
            reasoning_effort: Some(raw),
        }) = &agent.features
            && reasoning_effort.is_none()
        {
            tracing::warn!(
                agent = %agent.label,
                provider = %agent.provider,
                raw = %raw,
                "OpenAI reasoning_effort `{raw}` not recognized — passing through with no \
                 effort level (valid: low|medium|high|xhigh)"
            );
        }

        let workflow_state = workflow_response
            .pointer("/workflow/state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
            .to_string();

        tracing::info!(
            agent = %agent.label,
            provider = %agent.provider,
            model = %agent.model,
            state = %workflow_state,
            ?reasoning_effort,
            max_seconds = self.config.max_sub_agent_seconds,
            max_steps = self.config.max_sub_agent_steps,
            "spawning sub-agent (max_steps is currently advisory only — \
             aether headless has no built-in step counter; the timeout is \
             the enforced cap)"
        );

        // Build mcp config + sources. The interpreter-built system_prompt
        // becomes the USER PROMPT (the sub-agent's "go do this thing"
        // directive); we don't set Aether's system_prompt override — the
        // agent's own settings.json system prompt + the user prompt
        // together drive the session.
        let mut mcp_config = McpConfigArgs::default();
        praxec_mcp::set_as_sole_mcp(&mut mcp_config).map_err(|e| InterpreterError::Mcp {
            tool: "aether/sub_agent/mcp_wiring".into(),
            source: e,
        })?;
        let cwd = std::path::PathBuf::from(".")
            .canonicalize()
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        let mcp_sources = mcp_config.sources(&cwd);

        // Build the AgentSpec directly so we can carry `reasoning_effort`
        // through to the aether runtime. The default `run_headless` path
        // would build a spec with `reasoning_effort: None` because
        // `HeadlessArgs` has no field for it; bypassing it lets the
        // operator's models.yaml feature toggles actually take effect.
        let model_str = format!("{}:{}", agent.provider, agent.model);
        let parsed_model = model_str
            .parse()
            .map_err(|e: String| InterpreterError::Mcp {
                tool: format!("aether/sub_agent/{}/model_parse", agent.label),
                source: anyhow::anyhow!("invalid model `{model_str}`: {e}"),
            })?;
        let spec = AgentSpec::default_spec(&parsed_model, reasoning_effort, Vec::new());

        let config = RunConfig {
            prompt: system_prompt.to_string(),
            cwd,
            mcp_config_sources: mcp_sources,
            spec,
            system_prompt: None,
            output: OutputFormat::Text,
            verbose: false,
            events: vec![],
        };

        let total_timeout = Duration::from_secs(self.config.max_sub_agent_seconds);
        match timeout(total_timeout, run_aether_headless(config)).await {
            Ok(Ok(_exit_code)) => {
                // Natural completion — LLM emitted AgentMessage::Done.
                // The interpreter checks workflow.version post-return; if
                // version didn't advance, it treats the spawn as a soft
                // timeout and retries.
                tracing::info!(
                    agent = %agent.label,
                    state = %workflow_state,
                    "sub-agent completed naturally"
                );
                Ok(())
            }
            Ok(Err(e)) => {
                // Aether's CliError. Pass through as an Mcp-style error
                // since the interpreter has no SubAgent-specific variant
                // and this is operationally close — the sub-agent's
                // tool-call surface IS MCP through to Praxec.
                tracing::warn!(
                    agent = %agent.label,
                    state = %workflow_state,
                    error = %e,
                    "sub-agent failed inside aether headless"
                );
                Err(InterpreterError::Mcp {
                    tool: format!("aether/sub_agent/{}", agent.label),
                    source: anyhow::anyhow!("{e}"),
                })
            }
            Err(_elapsed) => {
                tracing::warn!(
                    agent = %agent.label,
                    state = %workflow_state,
                    timeout_seconds = self.config.max_sub_agent_seconds,
                    "sub-agent exceeded timeout"
                );
                Err(InterpreterError::SubAgentTimeout {
                    agent: agent.label.clone(),
                    state: workflow_state,
                })
            }
        }
    }
}
