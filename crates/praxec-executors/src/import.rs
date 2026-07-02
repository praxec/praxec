//! Vendor-neutral MCP tool import.
//!
//! Each `proxy.import` block names an existing `kind: mcp` connection. At
//! startup we connect to that connection (whatever it spawns underneath —
//! native binary, npx, uvx, docker run, podman run, an HTTP-backed MCP
//! server when URL transports land), call the standard `tools/list` MCP
//! method, and turn each returned tool into a `Capability` with
//! `source: CapabilitySource::Imported{ connection, tool }`.
//!
//! These imported capabilities flow into:
//!
//! 1. The proxy compiler — they show up as transitions in `proxy_default`.
//! 2. The discovery index — `gateway.search` finds them.
//! 3. Any custom registry consumer (audit, dashboards, etc.).

use std::sync::Arc;

use praxec_core::audit::{AuditEvent, AuditSink};
use praxec_core::capability::{Capability, CapabilityRegistry, CapabilitySource};
use serde_json::{json, Value};

use crate::mcp::McpExecutor;

/// Filter rules from a single `proxy.import` block.
#[derive(Debug, Clone, Default)]
struct ImportRule {
    connection: String,
    prefix: Option<String>,
    include: Vec<String>,
    exclude: Vec<String>,
    tags: Vec<String>,
}

impl ImportRule {
    fn allows(&self, tool_name: &str) -> bool {
        if !self.include.is_empty() && !self.include.iter().any(|s| s == tool_name) {
            return false;
        }
        if self.exclude.iter().any(|s| s == tool_name) {
            return false;
        }
        true
    }

    fn id_for(&self, tool_name: &str) -> String {
        match &self.prefix {
            Some(p) if !p.is_empty() => format!("{p}.{tool_name}"),
            _ => tool_name.to_string(),
        }
    }
}

fn parse_rules(config: &Value) -> Vec<ImportRule> {
    let Some(arr) = config.pointer("/proxy/import").and_then(Value::as_array) else {
        return vec![];
    };
    arr.iter()
        .filter_map(|v| {
            let connection = v.get("connection").and_then(Value::as_str)?.to_string();
            let prefix = v.get("prefix").and_then(Value::as_str).map(str::to_string);
            let include = string_array(v.get("include"));
            let exclude = string_array(v.get("exclude"));
            let tags = string_array(v.get("tags"));
            Some(ImportRule {
                connection,
                prefix,
                include,
                exclude,
                tags,
            })
        })
        .collect()
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Run the importer against the parsed config, using `executor` to talk to
/// each `proxy.import` connection. Each successful import emits a
/// `capability.discovered` audit event. Failures are logged via tracing and
/// recorded as `capability.discovery_failed` audit events; the gateway
/// continues to start with whatever was successfully imported.
pub async fn import_capabilities(
    config: &Value,
    executor: &McpExecutor,
    audit: &Arc<dyn AuditSink>,
) -> CapabilityRegistry {
    let mut registry = CapabilityRegistry::new();
    let rules = parse_rules(config);

    for rule in rules {
        match executor.list_remote_tools(&rule.connection).await {
            Ok(tools) => {
                for tool in tools {
                    if !rule.allows(tool.name.as_ref()) {
                        continue;
                    }
                    let id = rule.id_for(tool.name.as_ref());
                    let title = tool
                        .title
                        .clone()
                        .unwrap_or_else(|| tool.name.clone().into_owned());
                    let description = tool.description.as_ref().map(|c| c.to_string());
                    let input_schema = Some(Value::Object((*tool.input_schema).clone()));
                    let executor_cfg = json!({
                        "kind": "mcp",
                        "connection": rule.connection,
                        "tool": tool.name,
                    });

                    registry.add(Capability {
                        id: id.clone(),
                        source: CapabilitySource::Imported {
                            connection: rule.connection.clone(),
                            tool: tool.name.clone().into_owned(),
                        },
                        title: title.clone(),
                        description,
                        input_schema,
                        executor: executor_cfg,
                        tags: rule.tags.clone(),
                    });

                    audit
                        .record(
                            AuditEvent::new("capability.discovered").with_payload(json!({
                                "id": id,
                                "title": title,
                                "connection": rule.connection,
                                "tool": tool.name,
                            })),
                        )
                        .await
                        .unwrap_or_else(
                            |e| tracing::warn!(error = %e, "audit emit failed; event dropped"),
                        );
                }
            }
            Err(err) => {
                tracing::warn!(
                    connection = %rule.connection,
                    error = %err,
                    "tools/list import failed; gateway continues without imported tools from this connection"
                );
                audit
                    .record(
                        AuditEvent::new("capability.discovery_failed").with_payload(json!({
                            "connection": rule.connection,
                            "error": err.to_string(),
                        })),
                    )
                    .await
                    .unwrap_or_else(
                        |e| tracing::warn!(error = %e, "audit emit failed; event dropped"),
                    );
            }
        }
    }

    registry
}
