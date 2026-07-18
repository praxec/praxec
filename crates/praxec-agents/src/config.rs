//! `kind: agent` executor config — mirrors `LlmExecutorConfig`.
//!
//! `#[serde(deny_unknown_fields)]` is the poka-yoke: a config that sets an
//! unenforceable knob (e.g. `max_cost_usd`, `max_iterations`) **fails at
//! `check`** rather than silently no-op (FM3). `agent` XOR `affinity` and a
//! non-empty `goal` are enforced by `validate()`. The transition's `kind`
//! field is stripped before deserialization (same as the llm executor).

use serde::Deserialize;

use praxec_core::error::ExecutorError;
use praxec_core::model_resolver::ModelRef;

use crate::error::{AgentErrorCode, permanent};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentExecutorConfig {
    /// Direct named-agent binding (XOR `affinity`).
    #[serde(default)]
    pub agent: Option<String>,
    /// Role/affinity resolved to a model via models.yaml (XOR `agent`).
    /// An [`AffinityRef`]: either a typed `ModelRef`
    /// (`<affinity> | <tier> | <affinity>-<tier>`) OR an OPEN `activity:` key
    /// (any string, e.g. `review`) resolved against the models.yaml `activity:`
    /// map at resolve time (SPEC §21 open activity-keyed chains).
    #[serde(default)]
    pub affinity: Option<AffinityRef>,
    /// The task the agent is asked to accomplish (templated like
    /// `prompt_template`). Required, non-empty.
    pub goal: String,
    /// MCP connections to expose to the agent. The praxec-self connection
    /// is rejected by `config_doctor` (re-entrancy, FM5).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Paths/globs the agent may modify; partitioned by the Planner cohort-lock
    /// so concurrent agents are file-disjoint (FM13).
    #[serde(default)]
    pub owned_files: Vec<String>,
    /// The only enforceable cap (subprocess wall-clock timeout). No
    /// `max_cost_usd`/`max_iterations` in v1 — unenforceable → rejected by
    /// `deny_unknown_fields` (FM3).
    #[serde(default)]
    pub max_seconds: Option<u64>,
    /// Inter-event no-progress (stall) bound, in seconds: the maximum span of
    /// total stream silence tolerated within a turn before the model is declared
    /// stalled and the chain-walk escalates to the next model. Enforceable
    /// (a real wall-clock bound, like `max_seconds`), so it is an accepted knob
    /// — defaults to [`DEFAULT_STALL_SECONDS`]. Raise it for a provider that
    /// buffers a long "thinking" phase and emits no stream events until it is
    /// done (else the watchdog would false-positive on a model that is in fact
    /// making progress).
    #[serde(default)]
    pub stall_seconds: Option<u64>,
    /// (CR#1) Wall-clock ceiling on the *whole* model chain-walk for a single
    /// step, in seconds. Without it, an all-`AGENT_NO_RESULT` walk gives every
    /// model in the chain its own full `max_seconds` wall (N×600s of silent
    /// churn with no forward progress). This bounds the entire escalation: each
    /// model's effective wall is shrunk to the budget REMAINING, and once the
    /// budget is spent the walk stops and returns a terminal, human-routable
    /// `AGENT_STEP_BUDGET_EXHAUSTED` instead of escalating into yet another
    /// full-wall attempt. Enforceable → an accepted knob; defaults to
    /// [`DEFAULT_STEP_BUDGET_SECONDS`].
    #[serde(default)]
    pub step_budget_seconds: Option<u64>,
    /// Tool-setup (MCP `host.tools()` discovery/connection) bound, in seconds:
    /// the maximum time to list every declared connection's tools BEFORE the
    /// first model turn. A hung/slow tool server is bounded here rather than
    /// stalling the run. Enforceable → an accepted knob; defaults to
    /// [`DEFAULT_TOOL_SETUP_SECONDS`] and is clamped to the step's `max_seconds`
    /// (a setup bound that outlived the wall would be dead code). This is the
    /// **call-level override**: raise it for a step that connects to a slow or
    /// heavily-loaded tool server (setup can exceed the 60s default on a loaded
    /// box), leaving other steps at the default.
    #[serde(default)]
    pub tool_setup_seconds: Option<u64>,
    /// Reasoning-effort hint passed through to the Aether session.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Top-level keys the agent's `output` object must contain. Used by the
    /// runner to validate a salvaged text answer and to phrase in-session
    /// conformance feedback. Defaulted empty for directly-authored steps; the
    /// auto-drive composer populates it from the capability's `inputSchema`.
    #[serde(default)]
    pub expected_output_keys: Vec<String>,
    /// Declared JSON type per output key (`{"spec":"string","ready":"boolean"}`),
    /// from the transition `inputSchema.properties[key].type`. The runner
    /// enforces these at the `final_answer` boundary and RE-PROMPTS on a
    /// mismatch — so a non-deterministic agent that returns the right keys with
    /// the wrong type (e.g. an object where a string was declared) is corrected
    /// in-session instead of failing the post-run snippet contract and wasting
    /// the whole run. Keys without a declared type are not type-checked.
    #[serde(default)]
    pub expected_output_types: std::collections::BTreeMap<String, String>,
    /// P12 R1.4 — opt-in suspend capability: when `true` the runner offers the
    /// reserved `await_human` tool, and calling it durably parks the session
    /// (`AGENT_SUSPENDED` + a persisted conversation keyed by correlation_id)
    /// until a human reply resumes it. Default `false`: a step that doesn't
    /// opt in can never suspend. Requires a `ParkedSessionStore` wired on the
    /// runner (fail-fast `AGENT_AWAIT_UNSUPPORTED` otherwise).
    #[serde(default)]
    pub await_enabled: bool,
}

/// An agent's `affinity:` value — a closed [`ModelRef`] OR an open `activity:`
/// key. Parses a string: if it's a valid `ModelRef` (`coding`, `reasoning-frontier`,
/// …) it's [`AffinityRef::Model`]; otherwise it's treated as an open activity key
/// ([`AffinityRef::Activity`]) and resolved against the models.yaml `activity:`
/// map at resolve time. NOT `Copy` (holds a `String`) — deliberately separate
/// from the `Copy` `ModelRef` hot path so open keys don't cascade into the
/// resolver's closed enums.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AffinityRef {
    /// A typed model reference (closed affinity/tier domain).
    Model(ModelRef),
    /// An open activity key resolved via the models.yaml `activity:` map.
    Activity(String),
}

impl<'de> serde::Deserialize<'de> for AffinityRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match ModelRef::parse(&s) {
            Ok(m) => AffinityRef::Model(m),
            Err(_) => AffinityRef::Activity(s),
        })
    }
}

impl std::fmt::Display for AffinityRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AffinityRef::Model(m) => write!(f, "{m}"),
            AffinityRef::Activity(s) => write!(f, "{s}"),
        }
    }
}

/// Which model the agent runs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelBinding {
    /// Resolved through the affinity resolver (models.yaml) by a typed ModelRef.
    Affinity(ModelRef),
    /// Resolved through the models.yaml `activity:` map by an open key.
    Activity(String),
    /// A direct named-agent binding.
    Agent(String),
}

impl AgentExecutorConfig {
    /// Parse from a transition's `executor_config` (strips `kind`, then
    /// `deny_unknown_fields` + `validate`).
    pub fn from_value(mut value: serde_json::Value) -> Result<Self, ExecutorError> {
        if let Some(obj) = value.as_object_mut() {
            obj.remove("kind");
        }
        let cfg: AgentExecutorConfig =
            serde_json::from_value(value).map_err(|e| permanent(AgentErrorCode::ConfigParse, e))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Enforce `agent` XOR `affinity` (via the shared validator both executor
    /// kinds use) and a non-empty `goal`.
    pub fn validate(&self) -> Result<(), ExecutorError> {
        praxec_core::binding::validate_exclusive_binding(
            self.agent.as_deref(),
            self.affinity.as_ref().map(|d| d.to_string()).as_deref(),
            "agent",
        )
        .map_err(|m| permanent(AgentErrorCode::InvalidModelBinding, m))?;
        if self.goal.trim().is_empty() {
            return Err(permanent(
                AgentErrorCode::ConfigParse,
                "`goal` must not be empty",
            ));
        }
        Ok(())
    }

    pub fn model_binding(&self) -> ModelBinding {
        match &self.affinity {
            Some(AffinityRef::Model(m)) => ModelBinding::Affinity(*m),
            Some(AffinityRef::Activity(s)) => ModelBinding::Activity(s.clone()),
            None => ModelBinding::Agent(self.agent.clone().unwrap_or_default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn err_str(v: serde_json::Value) -> String {
        format!("{:?}", AgentExecutorConfig::from_value(v).unwrap_err())
    }

    #[test]
    fn parses_minimal_affinity_config() {
        let c = AgentExecutorConfig::from_value(json!({
            "kind": "agent", "affinity": "coding", "goal": "fix the failing test"
        }))
        .expect("valid config");
        assert_eq!(
            c.model_binding(),
            ModelBinding::Affinity(ModelRef::parse("coding").unwrap())
        );
        assert_eq!(c.goal, "fix the failing test");
    }

    #[test]
    fn parses_direct_agent_binding() {
        let c = AgentExecutorConfig::from_value(json!({
            "agent": "reviewer", "goal": "review the diff", "owned_files": ["src/lib.rs"]
        }))
        .expect("valid");
        assert_eq!(c.model_binding(), ModelBinding::Agent("reviewer".into()));
        assert_eq!(c.owned_files, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn parses_tool_setup_seconds_call_level_override() {
        // The call-level override for a slow/loaded tool server: an accepted,
        // enforceable knob (unlike max_cost_usd), unset by default.
        let c = AgentExecutorConfig::from_value(json!({
            "affinity": "coding", "goal": "g", "tool_setup_seconds": 180
        }))
        .expect("tool_setup_seconds is an accepted knob");
        assert_eq!(c.tool_setup_seconds, Some(180));

        let default = AgentExecutorConfig::from_value(json!({
            "affinity": "coding", "goal": "g"
        }))
        .expect("valid");
        assert_eq!(default.tool_setup_seconds, None);
    }

    #[test]
    fn rejects_unenforceable_max_cost_usd_at_parse() {
        // FM3: an unenforceable budget knob must fail, not silently no-op.
        assert!(
            err_str(json!({ "affinity": "coding", "goal": "g", "max_cost_usd": 5.0 }))
                .contains("AGENT_CONFIG_PARSE_ERROR")
        );
    }

    #[test]
    fn open_activity_key_parses_as_activity_binding() {
        // SPEC §21: an affinity that is NOT a closed ModelRef is an OPEN
        // `activity:` key (e.g. `review` = the senior-reviewer chain). It parses
        // (no longer AGENT_CONFIG_PARSE_ERROR) and resolves against the
        // models.yaml `activity:` map; an unknown key fails fast at RESOLVE with
        // a clear AGENT_INVALID_MODEL_BINDING (and at `praxec check` via the
        // activity-aware validator), not at deserialization.
        let c = AgentExecutorConfig::from_value(json!({
            "affinity": "review", "goal": "qa the slice"
        }))
        .expect("open activity key must parse");
        assert_eq!(c.model_binding(), ModelBinding::Activity("review".into()));
    }

    #[test]
    fn accepts_affinity_tier_composite() {
        // The affinity domain is ModelRef: composites must parse.
        let c = AgentExecutorConfig::from_value(json!({
            "affinity": "coding-frontier", "goal": "g"
        }))
        .expect("composite affinity must parse");
        assert_eq!(
            c.model_binding(),
            ModelBinding::Affinity(ModelRef::parse("coding-frontier").unwrap())
        );
    }

    #[test]
    fn rejects_both_agent_and_affinity() {
        assert!(
            err_str(json!({ "agent": "a", "affinity": "reasoning", "goal": "g" }))
                .contains("AGENT_INVALID_MODEL_BINDING")
        );
    }

    #[test]
    fn rejects_missing_model_binding() {
        assert!(err_str(json!({ "goal": "g" })).contains("AGENT_INVALID_MODEL_BINDING"));
    }

    #[test]
    fn rejects_missing_goal() {
        assert!(err_str(json!({ "affinity": "coding" })).contains("AGENT_CONFIG_PARSE_ERROR"));
    }

    #[test]
    fn rejects_empty_goal() {
        assert!(
            err_str(json!({ "affinity": "coding", "goal": "   " }))
                .contains("AGENT_CONFIG_PARSE_ERROR")
        );
    }
}
