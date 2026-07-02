//! Agent config — a `name → (provider, model)` mapping consumed by the
//! sub-agent spawner. v1 ships CLI-only via `--agent name=provider/model`
//! (repeated). TOML files at `~/.praxec/agents/*.toml` are deferred to
//! v2 (see docs/architecture/tui-agent-design.md §2.3 for the design note).
//!
//! Resolution: `delegate: <name>` on a workflow state lookups this map.
//! Missing name → `InterpreterError::UnknownAgent(name)` so the operator
//! sees exactly which agent config they forgot to wire.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub name: String,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AgentConfigParseError {
    #[error("agent spec '{0}' is missing '=' — expected `name=provider/model`")]
    MissingEquals(String),

    #[error("agent spec '{0}' has empty name (left of '=')")]
    EmptyName(String),

    #[error("agent spec '{spec}' value '{value}' is missing '/' — expected `name=provider/model`")]
    MissingProviderModelSlash { spec: String, value: String },

    #[error("agent spec '{spec}' has empty provider (left of '/')")]
    EmptyProvider { spec: String },

    #[error("agent spec '{spec}' has empty model (right of '/')")]
    EmptyModel { spec: String },
}

impl AgentConfig {
    /// Parse one `--agent name=provider/model` CLI value. Strict: each
    /// of the three segments must be non-empty. The provider name can
    /// contain only `[A-Za-z0-9._-]` characters in practice (mirrors
    /// the Aether agent spec convention), but the parser is permissive
    /// — it only rejects empty segments. Aether downstream rejects
    /// truly malformed provider/model strings.
    pub fn parse_cli_arg(spec: &str) -> Result<Self, AgentConfigParseError> {
        let (name, value) = spec
            .split_once('=')
            .ok_or_else(|| AgentConfigParseError::MissingEquals(spec.to_string()))?;
        if name.is_empty() {
            return Err(AgentConfigParseError::EmptyName(spec.to_string()));
        }
        let (provider, model) = value.split_once('/').ok_or_else(|| {
            AgentConfigParseError::MissingProviderModelSlash {
                spec: spec.to_string(),
                value: value.to_string(),
            }
        })?;
        if provider.is_empty() {
            return Err(AgentConfigParseError::EmptyProvider {
                spec: spec.to_string(),
            });
        }
        if model.is_empty() {
            return Err(AgentConfigParseError::EmptyModel {
                spec: spec.to_string(),
            });
        }
        Ok(Self {
            name: name.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
        })
    }
}

/// Build a name→AgentConfig registry from a vector of CLI specs.
/// Duplicate names → the last spec wins (mirrors clap's repeated-value
/// semantics; operator can override an earlier setting on the same line).
pub fn build_registry(
    specs: &[String],
) -> Result<HashMap<String, AgentConfig>, AgentConfigParseError> {
    let mut registry = HashMap::with_capacity(specs.len());
    for spec in specs {
        let cfg = AgentConfig::parse_cli_arg(spec)?;
        registry.insert(cfg.name.clone(), cfg);
    }
    Ok(registry)
}
