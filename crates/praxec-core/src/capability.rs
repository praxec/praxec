//! Capability registry — a uniform view of "things this gateway can do."
//!
//! Capabilities come from three places:
//!
//! - **Defined**  — declared inline in the gateway config (`proxy.expose`).
//! - **Imported** — discovered from a downstream MCP server via `tools/list`
//!   (`proxy.import`). Vendor-neutral: the connection might spawn a native
//!   binary, an `npx -y …` shim, a `docker run …`, a `podman run …`, or a
//!   `uvx …`. Everything is just a process the gateway speaks MCP to.
//! - **Cli / Rest** — non-MCP executors that still expose a tool surface.
//!
//! The registry is the source of truth that feeds the discovery index and
//! the proxy-workflow compiler.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One thing the gateway can do, regardless of where it came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    /// User-visible id. For `proxy.expose` entries this is `name`. For
    /// imported tools it's typically `<prefix>.<tool>`.
    pub id: String,

    pub source: CapabilitySource,

    pub title: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// JSON Schema the caller's arguments must conform to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,

    /// Executor config to dispatch this capability through. Conforms to the
    /// `executor` def in the gateway config schema.
    pub executor: Value,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Where a capability came from. Deliberately vendor-neutral — `Imported`
/// references a connection by name and a tool name on that connection;
/// whether the connection is backed by Docker, Podman, npx, or a native
/// binary is a runtime detail of the connection, not of the capability.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapabilitySource {
    /// Inline `proxy.expose` declaration in the config.
    Defined,
    /// Discovered via `tools/list` on an MCP connection. The connection's
    /// `command`/`args`/`env` decide how it gets launched.
    Imported { connection: String, tool: String },
    /// CLI executor (not an MCP tool).
    Cli,
    /// REST executor.
    Rest,
}

impl CapabilitySource {
    pub fn token(&self) -> &'static str {
        match self {
            CapabilitySource::Defined => "defined",
            CapabilitySource::Imported { .. } => "imported",
            CapabilitySource::Cli => "cli",
            CapabilitySource::Rest => "rest",
        }
    }
}

/// In-memory registry. The runtime doesn't consult it directly — the proxy
/// compiler reads from it when building the `proxy_default` workflow, and the
/// discovery indexer pulls from it for search.
#[derive(Debug, Clone, Default)]
pub struct CapabilityRegistry {
    pub capabilities: Vec<Capability>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_defined(config: &Value) -> Self {
        let mut reg = Self::new();
        reg.add_defined_from_config(config);
        reg
    }

    pub fn add(&mut self, c: Capability) {
        self.capabilities.push(c);
    }

    pub fn extend(&mut self, items: impl IntoIterator<Item = Capability>) {
        self.capabilities.extend(items);
    }

    pub fn len(&self) -> usize {
        self.capabilities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Capability> {
        self.capabilities.iter()
    }

    /// Walk `proxy.expose[*]` and add a `Defined` capability for each entry.
    pub fn add_defined_from_config(&mut self, config: &Value) {
        let Some(arr) = config.pointer("/proxy/expose").and_then(Value::as_array) else {
            return;
        };
        for ex in arr {
            let Some(name) = ex.get("name").and_then(Value::as_str) else {
                continue;
            };
            let title = ex
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(name)
                .to_string();
            let description = ex
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            // A `proxy.expose` entry MUST carry an `executor:`. Defaulting a
            // missing one to `{ "kind": "noop" }` silently registers a
            // capability that does nothing when invoked — a footgun. Warn and
            // skip the malformed entry rather than fabricating a noop.
            let Some(executor) = ex.get("executor").cloned() else {
                tracing::warn!(
                    capability = %name,
                    "MISSING_EXECUTOR: proxy.expose entry '{name}' has no `executor:`; \
                     skipping (a capability with no executor cannot be dispatched)"
                );
                continue;
            };
            let input_schema = ex.get("inputSchema").cloned();
            let tags = ex
                .get("tags")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();

            // Best-effort source classification by executor kind.
            let kind = ex
                .get("executor")
                .and_then(|e| e.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let source = match kind {
                "cli" => CapabilitySource::Cli,
                "rest" => CapabilitySource::Rest,
                _ => CapabilitySource::Defined,
            };

            self.add(Capability {
                id: name.to_string(),
                source,
                title,
                description,
                input_schema,
                executor,
                tags,
            });
        }
    }

    /// Render every capability as a `proxy.expose` entry. Used by the proxy
    /// compiler so that the same `proxy_default` workflow surfaces both
    /// declared and imported capabilities.
    pub fn as_proxy_exposures(&self) -> Vec<Value> {
        self.capabilities
            .iter()
            .map(|c| {
                let mut ex = serde_json::Map::new();
                ex.insert("name".into(), Value::String(c.id.clone()));
                ex.insert("title".into(), Value::String(c.title.clone()));
                if let Some(d) = &c.description {
                    ex.insert("description".into(), Value::String(d.clone()));
                }
                if let Some(s) = &c.input_schema {
                    ex.insert("inputSchema".into(), s.clone());
                }
                if !c.tags.is_empty() {
                    ex.insert(
                        "tags".into(),
                        Value::Array(c.tags.iter().map(|t| Value::String(t.clone())).collect()),
                    );
                }
                ex.insert("executor".into(), c.executor.clone());
                Value::Object(ex)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // CMP-033b — a `proxy.expose` entry without `executor:` must NOT be
    // silently registered as a noop capability; it is skipped.
    #[test]
    fn expose_missing_executor_is_skipped_not_defaulted_to_noop() {
        let config = json!({
            "proxy": {
                "expose": [
                    { "name": "no_exec.tool", "title": "Bad" },
                    {
                        "name": "good.tool",
                        "executor": { "kind": "cli", "command": "echo" }
                    }
                ]
            }
        });
        let reg = CapabilityRegistry::from_defined(&config);
        assert_eq!(reg.len(), 1, "the executor-less entry must be skipped");
        assert_eq!(reg.iter().next().unwrap().id, "good.tool");
        assert!(
            !reg.iter().any(|c| c.id == "no_exec.tool"),
            "no phantom noop capability should be registered"
        );
    }

    #[test]
    fn expose_with_executor_is_registered() {
        let config = json!({
            "proxy": {
                "expose": [
                    { "name": "t", "executor": { "kind": "rest", "url": "http://x" } }
                ]
            }
        });
        let reg = CapabilityRegistry::from_defined(&config);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.iter().next().unwrap().source, CapabilitySource::Rest);
    }
}
