//! SPEC §21 — Deterministic graph-walking interpreter for Praxec
//! workflows. One function, `walk_workflow`, advances a workflow from its
//! current state to a terminal state by:
//!
//!  1. **Asking the gateway** for the current state via `praxec.query`.
//!  2. **Returning** if the workflow has reached a `completed` status.
//!  3. **Delegating** the current state to a sub-agent when the response
//!     carries a `delegate` field (SPEC §21). The sub-agent decides which
//!     `praxec.command` call to make; the interpreter doesn't.
//!  4. **Auto-advancing** when only one non-deterministic link remains
//!     (deterministic chains were already auto-advanced by the gateway —
//!     see SPEC §6).
//!  5. **Picking the first non-escalate link** when multiple links remain
//!     and no sub-agent is delegated. Wrong picks are corrected by the
//!     critic/retry cycle on the next iteration.
//!
//! The interpreter is structurally simple by design: a `loop { match … }`,
//! ~100 lines of logic, no clever metaprogramming. Errors propagate via
//! `InterpreterError`; sub-agent timeouts get a retry budget before
//! escalation.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent_config::AgentConfig;
use praxec_core::model_resolver::{
    Binding, FailureClass, ModelRef, ModelRefParseError, ModelResolutionExhausted, Provider,
    ProviderFeatures, Resolver,
};
use praxec_core::providers::ProviderId;

/// Maximum sub-agent retries on `SubAgentTimeout` before the interpreter
/// submits the `escalate` transition (if one exists) or propagates.
///
/// Three is the established retry budget in this codebase (see
/// runtime_chain.rs's recovery policy). Increasing this without bounding
/// it elsewhere is how we'd accidentally rack up cost; decreasing it
/// makes flaky-but-recoverable sub-agents look broken.
pub const SUB_AGENT_RETRY_BUDGET: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum InterpreterError {
    /// A sub-agent ran past its time or step budget. The interpreter
    /// caught the timeout and is asking the workflow to escalate (or
    /// propagating if no escalate transition is declared).
    #[error("sub-agent '{agent}' exceeded its budget at state '{state}'")]
    SubAgentTimeout { agent: String, state: String },

    /// A workflow declared `delegate: <name>` but `<name>` was not
    /// declared in the agent registry. The error message varies by
    /// registry kind — legacy CLI mode points at `--agent` flags;
    /// YAML mode points at `models.yaml` and the specificity walk.
    #[error("workflow state '{state}': {source}")]
    AgentResolution {
        state: String,
        #[source]
        source: ResolutionError,
    },

    /// Underlying `praxec.command` was rejected by the gateway (likely
    /// `INVALID_TRANSITION` or guard failure). The interpreter surfaces
    /// the gateway's error body so the operator sees why.
    #[error("gateway rejected submit at state '{state}': {reason}")]
    SubmitRejected { state: String, reason: String },

    /// No actionable link from the current state, no delegate, and no
    /// escalate transition. Workflow is stuck; this is an architecture
    /// bug to fix in YAML.
    #[error(
        "workflow stuck at state '{state}': no delegate, no actionable links, \
         no `escalate` transition. Add one, fix the guards, or set a delegate."
    )]
    WorkflowStuck { state: String },

    /// An MCP-level error — connection lost, malformed response, etc.
    #[error("MCP call '{tool}' failed: {source}")]
    Mcp {
        tool: String,
        #[source]
        source: anyhow::Error,
    },

    /// The gateway responded, but the response was missing a field the
    /// interpreter relies on to make a control-flow decision (e.g. the
    /// workflow version used to detect advancement). Surfaced as a distinct
    /// error rather than silently defaulting — a default here corrupts the
    /// decision instead of failing it (e.g. version 0 vs 0 reads as "didn't
    /// advance" and spends the retry budget on a phantom timeout).
    #[error("gateway response from '{tool}' is malformed: {detail}")]
    MalformedResponse { tool: String, detail: String },
}

/// Abstraction for "call this MCP tool with these arguments and give me
/// the structured response." The production impl wraps an rmcp client
/// connected to a `praxec` child process; tests substitute a
/// canned-response mock.
///
/// The trait stays minimal on purpose. The interpreter only ever issues
/// `praxec.query` and `praxec.command` calls — adding methods would
/// signal scope creep.
#[async_trait]
pub trait McpToolCaller: Send + Sync {
    async fn call(&self, tool: &str, args: Value) -> anyhow::Result<Value>;
}

/// Abstraction for "run an isolated sub-agent session and wait for it
/// to advance the workflow." The production impl spawns an Aether
/// headless run with our `McpToolCaller` as the only tool backend; tests
/// substitute a mock that simulates submit-or-timeout.
///
/// The `spawn_and_wait` contract: the implementation MUST either issue
/// a `praxec.command` call against the MCP caller (advancing the
/// workflow's version) OR time out. Returning `Ok(())` without
/// advancing the workflow is a contract violation that the interpreter
/// will treat as a stuck state.
#[async_trait]
pub trait SubAgentSpawner: Send + Sync {
    async fn spawn_and_wait(
        &self,
        agent: &ResolvedAgent,
        system_prompt: &str,
        workflow_response: &Value,
    ) -> Result<(), InterpreterError>;
}

// ── agent registry (legacy + yaml) ─────────────────────────────────────────

/// One resolved agent ready to be spawned: provider + model + the typed
/// feature set for that provider. Source-agnostic; both the legacy
/// `--agent` flag path and the new YAML resolver path produce this shape.
#[derive(Debug, Clone)]
pub struct ResolvedAgent {
    /// Operator-facing label (the delegate name or the legacy `--agent`
    /// name). Used for logging and the workflow's escalate path.
    pub label: String,
    /// Aether canonical provider name (e.g. `"anthropic"`).
    pub provider: String,
    /// Aether model identifier.
    pub model: String,
    /// Typed feature toggles for the binding's provider. Legacy CLI
    /// path always produces `ProviderFeatures::None`.
    pub features: ProviderFeatures,
}

/// A delegate's full binding list — the candidate set the v0.3.1 runtime
/// CoR walks when the primary binding fails with an infrastructure-class
/// error. The legacy `--agent` flag path yields a 1-element list (no
/// CoR alternative); YAML-backed registries return the full override
/// list for the resolved level (`<affinity>-<tier>`, `<affinity>`,
/// `<tier>`, or `default`).
#[derive(Debug, Clone)]
pub struct ResolvedBindingList {
    /// The operator-facing delegate name (`coding-frontier` etc.).
    pub label: String,
    /// The list level the resolver chose (`coding-frontier`, `coding`,
    /// `default`, ...). Logged in the audit trail.
    pub level: String,
    /// Bindings in attempt order. Index 0 is the primary.
    pub bindings: Vec<Binding>,
}

impl ResolvedBindingList {
    /// Convert binding at `idx` to the `ResolvedAgent` shape the
    /// spawner expects. Panics if `idx` is out of range — callers
    /// should be operating on indices returned by `Resolver::try_next`.
    pub fn agent_at(&self, idx: usize) -> ResolvedAgent {
        let b = &self.bindings[idx];
        ResolvedAgent {
            label: self.label.clone(),
            provider: b.provider.display_name().to_string(),
            model: b.model.clone(),
            features: b.features.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolutionError {
    #[error(
        "delegate `{delegate}` is not registered. Either pass `--agent {delegate}=provider/model` \
         (legacy CLI mode) OR add it to your models.yaml under `overrides:`."
    )]
    UnknownLegacyAgent { delegate: String },

    #[error(
        "delegate `{delegate}` is not a valid <affinity> | <tier> | <affinity>-<tier>: {source}"
    )]
    InvalidDelegate {
        delegate: String,
        #[source]
        source: ModelRefParseError,
    },

    #[error("{0}")]
    Exhausted(#[from] ModelResolutionExhausted),
}

/// Source-agnostic resolution of `delegate:` strings to a `ResolvedAgent`.
///
/// `resolve` returns the PRIMARY binding only — the legacy contract.
/// `resolve_bindings` returns the full candidate list for v0.3.1+ CoR.
/// The default `resolve_bindings` impl wraps `resolve` in a single-entry
/// list, so registries that only know about one binding per delegate
/// (legacy CLI mode) work without change.
pub trait AgentRegistry: Send + Sync {
    fn resolve(&self, delegate: &str) -> Result<ResolvedAgent, ResolutionError>;

    fn resolve_bindings(&self, delegate: &str) -> Result<ResolvedBindingList, ResolutionError> {
        let agent = self.resolve(delegate)?;
        // Reconstruct a Binding from the ResolvedAgent. Legacy registries
        // only carry provider/model strings; the closed-enum Provider has
        // a Custom variant we use as a fallback when the string isn't a
        // known catalog slug.
        let provider = match ProviderId::from_slug(&agent.provider) {
            Some(id) => Provider::Known(id),
            None => Provider::Custom {
                endpoint: format!("legacy://{}", agent.provider),
            },
        };
        let binding = Binding {
            provider,
            model: agent.model.clone(),
            features: agent.features.clone(),
        };
        Ok(ResolvedBindingList {
            label: agent.label.clone(),
            level: "(legacy single binding)".to_string(),
            bindings: vec![binding],
        })
    }
}

/// v0.2-compatible registry: a HashMap of `--agent` flag values keyed by
/// delegate name. Wraps the existing `AgentConfig` shape.
pub struct LegacyAgentRegistry {
    pub agents: HashMap<String, AgentConfig>,
}

impl LegacyAgentRegistry {
    pub fn new(agents: HashMap<String, AgentConfig>) -> Self {
        Self { agents }
    }
}

impl AgentRegistry for LegacyAgentRegistry {
    fn resolve(&self, delegate: &str) -> Result<ResolvedAgent, ResolutionError> {
        let c = self
            .agents
            .get(delegate)
            .ok_or_else(|| ResolutionError::UnknownLegacyAgent {
                delegate: delegate.to_string(),
            })?;
        Ok(ResolvedAgent {
            label: c.name.clone(),
            provider: c.provider.clone(),
            model: c.model.clone(),
            features: ProviderFeatures::None,
        })
    }
}

/// v0.3 YAML-backed registry. Parses the delegate string, walks the
/// specificity ladder, and returns the FIRST binding from the chosen
/// list. (Full per-list Chain-of-Responsibility at spawn time is
/// deferred to v0.3.1 once aether's error surface exposes the failure
/// class we need to classify per-attempt failures.)
pub struct YamlAgentRegistry {
    pub resolver: Resolver,
}

impl YamlAgentRegistry {
    pub fn new(resolver: Resolver) -> Self {
        Self { resolver }
    }
}

impl AgentRegistry for YamlAgentRegistry {
    fn resolve(&self, delegate: &str) -> Result<ResolvedAgent, ResolutionError> {
        let list = self.resolve_bindings(delegate)?;
        // resolve_bindings guarantees a non-empty list — the walker
        // returns ModelResolutionExhausted if the chosen level is empty.
        Ok(list.agent_at(0))
    }

    fn resolve_bindings(&self, delegate: &str) -> Result<ResolvedBindingList, ResolutionError> {
        let d = ModelRef::parse(delegate).map_err(|source| ResolutionError::InvalidDelegate {
            delegate: delegate.to_string(),
            source,
        })?;
        let (bindings, level) = self.resolver.walk(&d)?;
        if bindings.is_empty() {
            return Err(ResolutionError::Exhausted(ModelResolutionExhausted {
                delegate: delegate.to_string(),
                walked_levels: vec!["(empty list at chosen level)".to_string()],
                attempts: Vec::new(),
            }));
        }
        Ok(ResolvedBindingList {
            label: delegate.to_string(),
            level,
            bindings: bindings.into_owned(),
        })
    }
}

/// Drive a workflow to a terminal state. See module-level docs for the
/// algorithm. Returns the final `context` blackboard map on success.
///
/// **Side effects.** Issues `praxec.query` and `praxec.command` calls
/// against `mcp`. May invoke `spawner.spawn_and_wait` zero or more times
/// (once per delegate state visited).
pub async fn walk_workflow(
    mcp: &dyn McpToolCaller,
    spawner: &dyn SubAgentSpawner,
    workflow_id: &str,
    registry: &dyn AgentRegistry,
) -> Result<Value, InterpreterError> {
    let mut retries: u32 = 0;
    loop {
        let resp = mcp_get(mcp, workflow_id).await?;

        if is_resolved(&resp) {
            return Ok(extract_context(&resp));
        }

        let state_before = current_state(&resp);
        let version_before = current_version(&resp)?;

        if let Some(agent_name) = resp.get("delegate").and_then(Value::as_str) {
            let list = registry.resolve_bindings(agent_name).map_err(|source| {
                InterpreterError::AgentResolution {
                    state: state_before.clone(),
                    source,
                }
            })?;
            let prompt = build_sub_agent_prompt(&resp);
            match spawn_with_cor(spawner, &list, &prompt, &resp).await {
                Ok(used_idx) => {
                    // Sub-agent claims success; verify the workflow
                    // actually advanced. Aether headless can return
                    // cleanly even when the model declined to submit —
                    // in that case we treat it as an implicit timeout
                    // and let the retry budget cover it.
                    tracing::info!(
                        delegate = %list.label,
                        level = %list.level,
                        used_binding_index = used_idx,
                        "sub-agent CoR succeeded"
                    );
                    let resp_after = mcp_get(mcp, workflow_id).await?;
                    if current_version(&resp_after)? > version_before {
                        retries = 0;
                        continue;
                    }
                    // Sub-agent ran without advancing — count as a
                    // soft timeout for retry purposes.
                    retries = retries.saturating_add(1);
                    if retries >= SUB_AGENT_RETRY_BUDGET {
                        try_escalate_or_propagate(mcp, workflow_id, &resp_after, agent_name)
                            .await?;
                        retries = 0;
                        continue;
                    }
                    continue;
                }
                Err(InterpreterError::SubAgentTimeout { .. }) => {
                    retries = retries.saturating_add(1);
                    if retries >= SUB_AGENT_RETRY_BUDGET {
                        // Re-fetch in case the sub-agent partially
                        // advanced before timing out.
                        let resp_now = mcp_get(mcp, workflow_id).await?;
                        try_escalate_or_propagate(mcp, workflow_id, &resp_now, &list.label).await?;
                        retries = 0;
                        continue;
                    }
                    continue;
                }
                Err(other) => return Err(other),
            }
        }

        // No delegate: auto-advance based on links.
        let pick = pick_link(&resp).ok_or_else(|| InterpreterError::WorkflowStuck {
            state: state_before.clone(),
        })?;
        submit_link(mcp, &pick, &state_before).await?;
        retries = 0;
    }
}

// ── runtime CoR over the binding list ──────────────────────────────────────

/// Classify an `InterpreterError` returned by a spawn into a
/// `FailureClass`. Used by `spawn_with_cor` to decide whether to advance
/// to the next binding (infrastructure-class) or surface to the caller
/// (content-class).
///
/// Today aether's `run_headless` only returns `CliError` for setup-time
/// failures (model parse, build, channel send). Runtime LLM API errors
/// are streamed as `AgentMessage::Error` events INSIDE the run and
/// surface as "natural completion that didn't advance the workflow"
/// (the retry-budget path handles those, not CoR). When aether grows a
/// typed error pass-through, this matcher gets a structured signal
/// instead of the string heuristics it uses today.
///
/// Mapping (string substrings on `InterpreterError::Mcp.source`):
/// - "401" / "InvalidApiKey" / "MissingApiKey" → `Auth401`
/// - "403" → `Auth403`
/// - "429" / "RateLimited" → `RateLimit429`
/// - "404" → `NotFound404`
/// - "Network" / "Timeout" / "Stream interrupted" → `NetworkTimeout`
/// - anything else → `ContentOther` (surfaces; no CoR fall-through)
///
/// `SubAgentTimeout` is NOT classified as infrastructure — the
/// existing retry-budget path covers "the LLM ran past the wall clock"
/// without involving CoR. Classifying it here would double-handle.
pub fn classify_spawn_error(err: &InterpreterError) -> FailureClass {
    let InterpreterError::Mcp { source, .. } = err else {
        return FailureClass::ContentOther;
    };
    let s = source.to_string();
    if s.contains("401") || s.contains("InvalidApiKey") || s.contains("MissingApiKey") {
        FailureClass::Auth401
    } else if s.contains("403") {
        FailureClass::Auth403
    } else if s.contains("429") || s.contains("Rate limited") || s.contains("RateLimited") {
        FailureClass::RateLimit429
    } else if s.contains("404") {
        FailureClass::NotFound404
    } else if s.contains("Network error")
        || s.contains("Request timed out")
        || s.contains("Stream interrupted")
    {
        FailureClass::NetworkTimeout
    } else {
        FailureClass::ContentOther
    }
}

/// Walk the binding list, spawning each in order until one succeeds
/// or all infrastructure-class failures exhaust. On content-class
/// failure (e.g. a 400 BadRequest from the provider), surface
/// immediately — no silent fallback. Returns the index of the binding
/// that ran on success.
///
/// CoR is intentionally narrow: it routes around *infrastructure*
/// trouble (auth, rate limit, model-not-found, network). It does NOT
/// route around behavior-level disagreements (the model returning a
/// shape the prompt didn't ask for, refusing on policy, etc.) — those
/// surface so the operator sees the real issue.
pub async fn spawn_with_cor(
    spawner: &dyn SubAgentSpawner,
    list: &ResolvedBindingList,
    prompt: &str,
    workflow_response: &Value,
) -> Result<usize, InterpreterError> {
    let mut prior: Vec<(usize, FailureClass, String)> = Vec::new();
    for (idx, _) in list.bindings.iter().enumerate() {
        let agent = list.agent_at(idx);
        match spawner
            .spawn_and_wait(&agent, prompt, workflow_response)
            .await
        {
            Ok(()) => return Ok(idx),
            Err(InterpreterError::SubAgentTimeout { agent, state }) => {
                // Timeout is the existing retry-budget's domain; bubble
                // up unchanged so walk_workflow handles it.
                return Err(InterpreterError::SubAgentTimeout { agent, state });
            }
            Err(other) => {
                let class = classify_spawn_error(&other);
                if !class.is_infrastructure() {
                    // Content-class → surface. The remaining bindings
                    // are NOT tried — same model behavior would likely
                    // recur and the operator needs the real signal.
                    return Err(other);
                }
                prior.push((idx, class, other.to_string()));
                tracing::warn!(
                    delegate = %list.label,
                    level = %list.level,
                    binding_index = idx,
                    provider = %agent.provider,
                    model = %agent.model,
                    ?class,
                    "binding failed (infrastructure-class); advancing to next in list"
                );
            }
        }
    }
    Err(InterpreterError::AgentResolution {
        state: workflow_response
            .pointer("/workflow/state")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
        source: ResolutionError::Exhausted(ModelResolutionExhausted {
            delegate: list.label.clone(),
            walked_levels: vec![format!("CoR over {} (level: {})", list.label, list.level)],
            attempts: prior
                .into_iter()
                .map(|(i, c, d)| praxec_core::model_resolver::AttemptRecord {
                    binding: list.bindings[i].clone(),
                    class: c,
                    detail: d,
                })
                .collect(),
        }),
    })
}

// ── helpers ────────────────────────────────────────────────────────────────

async fn mcp_get(mcp: &dyn McpToolCaller, workflow_id: &str) -> Result<Value, InterpreterError> {
    mcp.call("praxec.query", json!({ "workflowId": workflow_id }))
        .await
        .map_err(|e| InterpreterError::Mcp {
            tool: "praxec.query".into(),
            source: e,
        })
}

fn is_resolved(resp: &Value) -> bool {
    // ADR-0008 — a mission is done when it resolves, either way: `succeeded` or
    // `failed` (was the flat `completed`). The interpreter returns the final
    // context on resolution; the caller reads `result.status`/`reason` for which.
    matches!(
        resp.pointer("/result/status").and_then(Value::as_str),
        Some("succeeded") | Some("failed")
    )
}

fn extract_context(resp: &Value) -> Value {
    resp.get("context").cloned().unwrap_or_else(|| json!({}))
}

fn current_state(resp: &Value) -> String {
    resp.pointer("/workflow/state")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string()
}

fn current_version(resp: &Value) -> Result<u64, InterpreterError> {
    // STUB-109 — a running workflow's query response always carries
    // `/workflow/version`. Defaulting a missing/non-numeric value to 0 (the
    // old behavior) silently breaks the advance-detection comparison below,
    // so fail loud on schema drift instead.
    resp.pointer("/workflow/version")
        .and_then(Value::as_u64)
        .ok_or_else(|| InterpreterError::MalformedResponse {
            tool: "praxec.query".into(),
            detail: "missing numeric `/workflow/version`; cannot detect whether \
                     the workflow advanced"
                .into(),
        })
}

/// Build the sub-agent system prompt from the response's `guidance` +
/// `context`. The sub-agent inherits goal + instructions and sees the
/// blackboard verbatim. Size threshold for warnings is handled by the
/// production spawner (TuiConfig.max_blackboard_bytes), not here — the
/// interpreter doesn't make policy calls about prompt length.
fn build_sub_agent_prompt(resp: &Value) -> String {
    let goal = resp
        .pointer("/guidance/goal")
        .and_then(Value::as_str)
        .unwrap_or("(no goal declared)");
    let instructions = resp
        .pointer("/guidance/instructions")
        .and_then(Value::as_str)
        .unwrap_or("");
    let context = resp.get("context").cloned().unwrap_or_else(|| json!({}));
    let context_str = serde_json::to_string_pretty(&context).unwrap_or_default();
    format!(
        "You are a sub-agent inside a governed Praxec workflow.\n\n\
         Goal: {goal}\n\n\
         Instructions: {instructions}\n\n\
         Blackboard (current context):\n{context_str}\n\n\
         When you are ready to advance the workflow, pick one of the \
         links from the current `praxec.query` response and call the \
         tool named in that link's `method` field (`praxec.command`) \
         with the link's `args`. Use `praxec.query` to re-read the \
         workflow state at any time."
    )
}

/// Links the interpreter is allowed to **auto-submit unattended**.
///
/// The auto-advance path runs with no human present and no sub-agent
/// delegated, so it may only drive links whose actor needs no
/// interaction:
///
/// - `deterministic` — dropped: the gateway auto-chains these itself
///   (SPEC §6), they're not the interpreter's to drive.
/// - `human` — dropped: HITL gates. The gateway enforces
///   `Principal::is_human()` and rejects an agent/anonymous submitter
///   with `ACTOR_MISMATCH` (GOVERNANCE §"Actor enforcement"). Auto-
///   submitting one would be a *silent auto-approve of a human gate*
///   (H10) — the interpreter must halt and surface it for a human,
///   never advance past it.
/// - any other/unknown actor — dropped, fail-safe: an actor the
///   interpreter doesn't understand is treated as requiring
///   interaction, not as free to auto-drive.
///
/// Only `agent`-actor links (the interpreter's own lane) remain. When
/// none remain, `pick_link` returns `None` and the caller raises
/// `WorkflowStuck` rather than blindly submitting a gated link.
fn actionable_links(resp: &Value) -> Vec<Value> {
    resp.get("links")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(interpreter_may_auto_submit)
        .collect()
}

/// True only when this link requires **no** human/agent interaction to
/// submit — i.e. it is the interpreter's `agent` lane. Any `human`,
/// `deterministic`, or unrecognized actor returns false so it is never
/// auto-advanced (poka-yoke: allowlist, not denylist).
fn interpreter_may_auto_submit(link: &Value) -> bool {
    matches!(link.get("actor").and_then(Value::as_str), Some("agent"))
}

/// Pick the link the interpreter will submit. Algorithm:
/// 1. Filter `actor == "deterministic"` (handled by the gateway).
/// 2. If exactly one remains → return it (the "obvious" path).
/// 3. If multiple remain → return the first non-`escalate` link.
///    Picking `escalate` aggressively would short-circuit useful work.
fn pick_link(resp: &Value) -> Option<Value> {
    let actionable = actionable_links(resp);
    if actionable.is_empty() {
        return None;
    }
    if actionable.len() == 1 {
        return Some(actionable[0].clone());
    }
    // Multi-link case: prefer first non-escalate. Falls back to first
    // link if every option happens to be `escalate` (degenerate config).
    actionable
        .iter()
        .find(|l| l.get("rel").and_then(Value::as_str) != Some("escalate"))
        .cloned()
        .or_else(|| actionable.into_iter().next())
}

/// Submit one link's command. `state` is the workflow state the caller
/// observed *before* this submit — a link object has no
/// `/workflow/state` of its own, so deriving it here always yielded "?"
/// (CMP-040). Callers thread their known state in instead.
async fn submit_link(
    mcp: &dyn McpToolCaller,
    link: &Value,
    state: &str,
) -> Result<(), InterpreterError> {
    let args = link.get("args").cloned().unwrap_or_else(|| json!({}));
    let resp = mcp
        .call("praxec.command", args)
        .await
        .map_err(|e| InterpreterError::Mcp {
            tool: "praxec.command".into(),
            source: e,
        })?;
    // The gateway returns rejections in the body (`error.code`) not as
    // MCP-level errors. Translate so the interpreter sees them.
    if let Some(err) = resp.get("error") {
        let reason = err.get("message").and_then(Value::as_str).unwrap_or("");
        return Err(InterpreterError::SubmitRejected {
            state: state.to_string(),
            reason: reason.to_string(),
        });
    }
    Ok(())
}

/// After SUB_AGENT_RETRY_BUDGET timeouts: try to submit an `escalate`
/// transition if one exists in the current response's links. Otherwise
/// propagate `SubAgentTimeout`.
async fn try_escalate_or_propagate(
    mcp: &dyn McpToolCaller,
    _workflow_id: &str,
    resp: &Value,
    agent_name: &str,
) -> Result<(), InterpreterError> {
    let escalate_link = resp.get("links").and_then(Value::as_array).and_then(|arr| {
        arr.iter()
            .find(|l| l.get("rel").and_then(Value::as_str) == Some("escalate"))
            .cloned()
    });
    let state = current_state(resp);
    let Some(link) = escalate_link else {
        return Err(InterpreterError::SubAgentTimeout {
            agent: agent_name.to_string(),
            state,
        });
    };
    submit_link(mcp, &link, &state).await
}
