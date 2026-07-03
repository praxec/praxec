use serde_json::{Value, json};

use crate::capability::CapabilityRegistry;

pub const DEFAULT_PROXY_WORKFLOW_ID: &str = "proxy_default";
pub const DEFAULT_PROXY_STATE: &str = "ready";

/// Compile a `proxy.expose: [...]` block into a single null-op workflow.
/// Each exposure becomes one transition `ready -> ready`.
///
/// Returns `None` when the config has no `proxy.expose` array.
pub fn compile_proxy_workflow(config: &Value) -> Option<Value> {
    let exposures = config.pointer("/proxy/expose")?.as_array()?;
    compile_proxy_workflow_from_exposures(exposures)
}

/// Compile from a `CapabilityRegistry` instead of raw config. Lets imported
/// tools (from `proxy.import`) participate in `proxy_default` alongside
/// declared exposures.
pub fn compile_proxy_workflow_from_registry(registry: &CapabilityRegistry) -> Option<Value> {
    if registry.is_empty() {
        return None;
    }
    let exposures = registry.as_proxy_exposures();
    compile_proxy_workflow_from_exposures(&exposures)
}

fn compile_proxy_workflow_from_exposures(exposures: &[Value]) -> Option<Value> {
    let mut transitions = serde_json::Map::new();
    for exposure in exposures {
        let Some(name) = exposure.get("name").and_then(Value::as_str) else {
            continue;
        };

        let mut t = serde_json::Map::new();
        t.insert(
            "title".into(),
            exposure
                .get("title")
                .cloned()
                .unwrap_or_else(|| json!(name)),
        );
        if let Some(d) = exposure.get("description") {
            t.insert("description".into(), d.clone());
        }
        t.insert("target".into(), json!(DEFAULT_PROXY_STATE));
        t.insert("actor".into(), json!("agent"));
        t.insert(
            "inputSchema".into(),
            exposure
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(empty_object_schema),
        );
        t.insert(
            "guards".into(),
            exposure.get("guards").cloned().unwrap_or_else(|| json!([])),
        );
        // CMP-033 — a proxy exposure MUST declare an explicit `executor`.
        // Defaulting a missing executor to `{ kind: noop }` (the old behavior)
        // silently produces a callable tool that does nothing — a footgun that
        // looks wired but quietly drops every call. Require the operator to be
        // explicit: a genuine no-op exposure must opt in with
        // `executor: { kind: noop }`. This runs once at config-compile /
        // startup, so a panic here is a fail-fast that surfaces the
        // misconfiguration before any tool becomes callable (mirrors the
        // construction-bug panic style elsewhere, e.g. parallel.rs).
        let Some(executor) = exposure.get("executor").cloned() else {
            panic!(
                "INVALID_PROXY_EXPOSURE: proxy exposure '{name}' has no `executor`. \
                 Declare one explicitly (e.g. `executor: {{ kind: mcp, ... }}`), or opt \
                 into a deliberate no-op with `executor: {{ kind: noop }}`."
            );
        };
        t.insert("executor".into(), executor);
        if let Some(rel) = exposure.get("reliability") {
            t.insert("reliability".into(), rel.clone());
        }
        t.insert("output".into(), json!({ "lastResult": "$.output" }));

        transitions.insert(name.to_string(), Value::Object(t));
    }

    Some(json!({
        "version": "0",
        "description": "Generated null-op workflow for configurable proxy exposures.",
        "initialState": DEFAULT_PROXY_STATE,
        "states": {
            DEFAULT_PROXY_STATE: {
                "description": "Proxy-ready state. All transitions return to this state.",
                "transitions": transitions
            }
        }
    }))
}

fn empty_object_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposure_with_explicit_executor_compiles() {
        let cfg = json!({
            "proxy": { "expose": [
                { "name": "echo", "executor": { "kind": "noop" } }
            ]}
        });
        let wf = compile_proxy_workflow(&cfg).expect("proxy workflow");
        let exec = wf
            .pointer("/states/ready/transitions/echo/executor")
            .expect("echo executor");
        assert_eq!(exec.get("kind").and_then(Value::as_str), Some("noop"));
    }

    #[test]
    #[should_panic(expected = "INVALID_PROXY_EXPOSURE")]
    fn exposure_missing_executor_fails_fast_not_defaulted_to_noop() {
        // CMP-033 — a proxy exposure with no `executor` must FAIL-FAST at
        // compile time rather than silently becoming a do-nothing noop tool.
        let cfg = json!({
            "proxy": { "expose": [
                { "name": "phantom", "title": "Looks wired, does nothing" }
            ]}
        });
        let _ = compile_proxy_workflow(&cfg);
    }

    #[test]
    fn explicit_noop_opt_in_is_allowed() {
        // The deliberate no-op escape hatch: `executor: { kind: noop }`.
        let cfg = json!({
            "proxy": { "expose": [
                { "name": "deliberate", "executor": { "kind": "noop" } }
            ]}
        });
        assert!(compile_proxy_workflow(&cfg).is_some());
    }
}
