//! Public `praxec` gateway binary — a thin shim over
//! [`praxec::gateway`]. It registers the governed `kind: llm` overlay
//! (behind the `llm-executor` feature) and the `kind: agent` overlay (behind
//! the `agents` feature), then hands control to the shared CLI entry point.
//! Both features are default-on so a standard install ships with both kinds
//! available.

use praxec::gateway::{self, GatewayOverlays};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[allow(unused_mut)]
    let mut overlays = GatewayOverlays::default();

    #[cfg(feature = "llm-executor")]
    overlays.registrars.push(gateway::llm_overlay_registrar());

    // ADR-0007 — agents are first-class: the `kind: agent` overlay is always
    // wired, not behind a feature.
    overlays.registrars.push(agent::agent_overlay_registrar());
    overlays.diagnostics.push(agent::agent_diagnostics());
    // Fail-fast at load when `gateway.models_yaml` is set but unloadable: both
    // `kind: agent` and affinity-resolved `kind: llm` steps depend on it, and a
    // silent WARN-and-degrade lets serve boot with every such step doomed to
    // fail at first dispatch. This doctor turns it into a serve-blocking error.
    overlays
        .diagnostics
        .push(agent::models_yaml_load_diagnostic());

    gateway::run_cli(overlays).await
}

/// `kind: agent` wiring (ADR-0007 — first-class, always compiled). Governed
/// agent execution is the in-process rig runner — no subprocess, no aether
/// dependency. (The `aether` binary is the terminal-UI product only; it is never
/// spawned to run governed steps.)
mod agent {
    use std::sync::Arc;

    use praxec::gateway::{DiagnosticProvider, OverlayCtx, OverlayRegistrar};
    use praxec_core::SingleKindOverlay;
    use praxec_core::ports::Executor;
    use serde_json::Value;

    struct RejectingAgentModelResolver;

    #[async_trait::async_trait]
    impl praxec_agents::session::AgentModelResolver for RejectingAgentModelResolver {
        async fn resolve(
            &self,
            _binding: &praxec_agents::config::ModelBinding,
        ) -> Result<String, praxec_core::error::ExecutorError> {
            Err(praxec_core::error::ExecutorError::Permanent(
                "AGENT_NO_AGENTS_YAML: `kind: agent` requires `gateway.models_yaml` to resolve \
                 its model binding"
                    .into(),
            ))
        }
    }

    struct AgentsYamlModelResolver {
        inner: praxec::affinity_resolver::AgentsYamlAffinityResolver,
    }

    #[async_trait::async_trait]
    impl praxec_agents::session::AgentModelResolver for AgentsYamlModelResolver {
        async fn resolve(
            &self,
            binding: &praxec_agents::config::ModelBinding,
        ) -> Result<String, praxec_core::error::ExecutorError> {
            use praxec_agents::config::ModelBinding;
            let name = match binding {
                ModelBinding::Affinity(d) => d.to_string(),
                ModelBinding::Activity(s) => s.clone(),
                ModelBinding::Agent(n) => n.clone(),
            };
            praxec::affinity_resolver::resolve_affinity_to_model(self.inner.resolver(), &name)
                .ok_or_else(|| {
                    praxec_core::error::ExecutorError::Permanent(format!(
                        "AGENT_INVALID_MODEL_BINDING: agent binding `{name}` could not be \
                         resolved against models.yaml"
                    ))
                })
        }

        async fn resolve_chain(
            &self,
            binding: &praxec_agents::config::ModelBinding,
        ) -> Result<Vec<String>, praxec_core::error::ExecutorError> {
            use praxec_agents::config::ModelBinding;
            let name = match binding {
                ModelBinding::Affinity(d) => d.to_string(),
                ModelBinding::Activity(s) => s.clone(),
                ModelBinding::Agent(n) => n.clone(),
            };
            let chain =
                praxec::affinity_resolver::resolve_affinity_to_chain(self.inner.resolver(), &name);
            if chain.is_empty() {
                Err(praxec_core::error::ExecutorError::Permanent(format!(
                    "AGENT_INVALID_MODEL_BINDING: agent binding `{name}` could not be \
                     resolved against models.yaml"
                )))
            } else {
                Ok(chain)
            }
        }
    }

    fn build_agent_model_resolver(
        config: &Value,
    ) -> Arc<dyn praxec_agents::session::AgentModelResolver> {
        match config
            .pointer("/gateway/models_yaml")
            .and_then(Value::as_str)
        {
            Some(path) => {
                match praxec::affinity_resolver::AgentsYamlAffinityResolver::from_path(
                    std::path::Path::new(path),
                ) {
                    Ok(inner) => {
                        tracing::info!(models_yaml = %path, "wired models.yaml model resolver for kind: agent");
                        Arc::new(AgentsYamlModelResolver { inner })
                    }
                    Err(err) => {
                        // Defense-in-depth: the load-time `models_yaml_load_diagnostic`
                        // already turns a present-but-unloadable file into a
                        // serve-blocking error, so reaching here means either the
                        // file vanished between check and serve, or a non-serve
                        // path built the resolver. Keep the fail-loud resolver.
                        tracing::warn!(models_yaml = %path, error = %err, "failed to load gateway.models_yaml; kind: agent will fail loud");
                        Arc::new(RejectingAgentModelResolver)
                    }
                }
            }
            None => Arc::new(RejectingAgentModelResolver),
        }
    }

    /// ADR-0007 — production [`ToolHost`]: exposes + executes a session's MCP
    /// tools by reusing the executors crate's `McpToolCaller` (the same rmcp
    /// machinery `kind: mcp` uses). Stateless — safe to share across agents.
    struct McpToolHost {
        caller: Arc<dyn praxec_executors::mcp::McpToolCaller>,
    }

    #[async_trait::async_trait]
    impl praxec_agents::rig_runner::ToolHost for McpToolHost {
        async fn tools(
            &self,
            connections: &[String],
        ) -> Result<Vec<(rig::completion::ToolDefinition, String)>, praxec_core::error::ExecutorError>
        {
            let mut out = Vec::new();
            for conn in connections {
                // BEST-EFFORT per connection (not fail-fast-at-startup): the
                // auto-drive hands EVERY agent ALL wired connections, so a single
                // unreachable one (missing binary / idle-timed-out / errored)
                // must NOT brick agents that never use it. Skip it with a LOUD
                // warning instead. Fail-fast is preserved where it belongs —
                // `call()` errors loudly if the agent actually invokes a tool
                // from a connection that isn't reachable (fail at the point of
                // real need, not at startup for unrelated agents).
                match self.caller.list_remote_tools(conn).await {
                    Ok(tools) => {
                        for t in tools {
                            // C5 — the mapping lives in the agents lib so it's contract-tested.
                            out.push((
                                praxec_agents::rig_runner::tool_definition_from(&t),
                                conn.clone(),
                            ));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            connection = %conn,
                            error = %e,
                            "MCP_TOOLS_UNREACHABLE: skipping connection '{conn}' from this \
                             agent's toolset (listing failed); a call to its tools will fail \
                             loud at invocation time",
                        );
                    }
                }
            }
            Ok(out)
        }

        async fn call(
            &self,
            connection: &str,
            name: &str,
            arguments: &str,
        ) -> Result<String, String> {
            let args = serde_json::from_str::<serde_json::Map<String, Value>>(arguments).ok();
            let result = self
                .caller
                .call_tool(connection, name, args)
                .await
                .map_err(|e| e.to_string())?;
            Ok(match result.structured_content {
                Some(v) => v.to_string(),
                None => serde_json::json!({ "content": result.content }).to_string(),
            })
        }
    }

    pub fn agent_overlay_registrar() -> OverlayRegistrar {
        Arc::new(|ctx: OverlayCtx| {
            use praxec_agents::executor::AgentExecutor;
            use praxec_agents::file_tools::CompositeToolHost;
            use praxec_agents::rig_runner::{RigSessionRunner, ToolHost};
            use praxec_agents::session::AgentSessionRunner;
            use praxec_executors::mcp::{McpConnections, RmcpToolCaller};

            // ADR-0007 — governed agent execution is ALWAYS the in-process rig
            // runner (light, shares the gateway's LockSpace, no subprocess
            // spawn). Wired with a production ToolHost over the operator's MCP
            // connections, composed with a scoped file-edit host: a coding step
            // declares `file:<repo-root>` among its tools to get
            // write_file/read_file rooted there (the trusted in-process coding
            // agent's toolbelt).
            let caller = Arc::new(RmcpToolCaller::new(McpConnections::from_config(
                &ctx.config,
            )));
            let mcp_host: Arc<dyn ToolHost> = Arc::new(McpToolHost { caller });
            let host: Arc<dyn ToolHost> = Arc::new(CompositeToolHost::new(mcp_host));
            let runner: Arc<dyn AgentSessionRunner> =
                Arc::new(RigSessionRunner::with_default_provider().with_tool_host(host));
            let resolver = build_agent_model_resolver(&ctx.config);
            let mut agent_executor = AgentExecutor::new(runner, resolver);

            // ADR-0007 — enable the untrusted branch when a sandbox is usable on
            // this host, sharing the runtime's RepoLocks authority so
            // untrusted-agent promotion coordinates with transition `owned_files`
            // locks (one authority — not a separate space). Without a usable
            // sandbox, `untrusted: true` steps fail fast rather than running
            // unconfined; without shared locks (unreachable in the wired binary)
            // untrusted is disabled rather than run against a divergent authority.
            let bwrap = praxec_core::sandbox::BwrapProvider::new();
            if praxec_core::sandbox::SandboxProvider::preflight(&bwrap).usable {
                match ctx.runtime.repo_locks() {
                    Some(locks) => {
                        agent_executor =
                            agent_executor.with_untrusted_support(Arc::new(bwrap), locks);
                    }
                    _ => {
                        tracing::warn!(
                            "untrusted kind: agent disabled: runtime has no repo_locks to share"
                        );
                    }
                }
            }

            let agent_executor: Arc<dyn Executor> = Arc::new(agent_executor);
            Arc::new(SingleKindOverlay::new(ctx.inner, "agent", agent_executor))
        })
    }

    pub fn agent_diagnostics() -> DiagnosticProvider {
        Arc::new(|config: &Value| praxec_agents::config_doctor::doctor_check(config))
    }

    /// Load-time fail-fast for an unloadable `gateway.models_yaml`. When the key
    /// is set, points at a file that exists, but fails to parse/load, emit a
    /// `Diagnostic::Error` so `serve`'s validation gate refuses to boot. Without
    /// this, the only signal was a WARN as the resolver silently degraded to
    /// `RejectingAgentModelResolver` (every `kind: agent` step — and every
    /// affinity-resolved `kind: llm` step — then failing at first dispatch).
    ///
    /// `set + present + fails to load` is the trigger: a missing file or an
    /// unset key is NOT an error here (a build may legitimately run without
    /// `models.yaml` and use no affinity/agent bindings); only a configured file
    /// that is present-but-broken fails fast. The runtime resolver fallback is
    /// kept as defense-in-depth.
    pub fn models_yaml_load_diagnostic() -> DiagnosticProvider {
        use praxec_core::validate::Diagnostic;
        Arc::new(|config: &Value| {
            let Some(path) = config
                .pointer("/gateway/models_yaml")
                .and_then(Value::as_str)
            else {
                return Vec::new(); // not configured → nothing to validate
            };
            let p = std::path::Path::new(path);
            if !p.exists() {
                // "present" gate: an absent file is reported by other layers
                // (and may be intentional); this doctor only fails on a file
                // that IS there but cannot be loaded.
                return Vec::new();
            }
            match praxec::affinity_resolver::AgentsYamlAffinityResolver::from_path(p) {
                Ok(_) => Vec::new(),
                Err(err) => vec![Diagnostic::Error(format!(
                    "MODELS_YAML_LOAD_FAILED: gateway.models_yaml = `{path}` is present but failed \
                     to load ({err}). `kind: agent` model bindings and affinity-resolved \
                     `kind: llm` steps cannot resolve, so every such step would fail at dispatch. \
                     Fix or remove the file before serving."
                ))],
            }
        })
    }
}

#[cfg(test)]
mod models_yaml_doctor_tests {
    use praxec_core::validate::Diagnostic;
    use serde_json::json;

    fn run(config: &serde_json::Value) -> Vec<Diagnostic> {
        crate::agent::models_yaml_load_diagnostic()(config)
    }

    #[test]
    fn unset_models_yaml_is_clean() {
        assert!(run(&json!({})).is_empty());
    }

    #[test]
    fn absent_file_is_not_an_error_here() {
        // "present" gate: a configured-but-missing file is not flagged by THIS
        // doctor (it only fails on present-but-unloadable).
        let cfg = json!({ "gateway": { "models_yaml": "/no/such/models.yaml" } });
        assert!(run(&cfg).is_empty());
    }

    #[test]
    fn present_but_unloadable_file_is_a_serve_blocking_error() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("praxec_bad_models_{}.yaml", std::process::id()));
        // Not valid models.yaml — malformed YAML so from_path fails.
        std::fs::write(&path, "this: is: not: valid: models: yaml: [").unwrap();
        let cfg = json!({ "gateway": { "models_yaml": path.to_str().unwrap() } });
        let diags = run(&cfg);
        std::fs::remove_file(&path).ok();
        assert_eq!(
            diags.len(),
            1,
            "a broken present file must produce exactly one error"
        );
        assert!(
            matches!(&diags[0], Diagnostic::Error(m) if m.contains("MODELS_YAML_LOAD_FAILED")),
            "expected MODELS_YAML_LOAD_FAILED error, got: {:?}",
            diags[0]
        );
    }
}
