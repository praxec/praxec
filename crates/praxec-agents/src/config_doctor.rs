//! Load-time validation for `kind: agent` steps (mirrors
//! `llm-executor::config_doctor`). Runs in `praxec check` so a malformed
//! agent step — unknown/unenforceable field, both/neither `agent`+`affinity`,
//! empty `goal` — fails at config time, not at first dispatch (poka-yoke).
//!
//! Re-entrancy (FM5) is prevented *by construction*: the executor only ever
//! exposes the step's declared MCP connections + the hosted `final_answer`
//! tool — never praxec's own command tool — so an agent cannot drive its own
//! blocked workflow. This doctor additionally rejects the reserved self name.

use praxec_core::validate::{Diagnostic, for_each_executor_site};
use serde_json::Value;

use crate::config::AgentExecutorConfig;

/// Reserved connection name an agent step may not list in `tools` (would point
/// the agent back at the gateway it runs inside — re-entrancy, FM5).
const RESERVED_SELF_TOOL: &str = "praxec";

/// Walk every `kind: agent` executor site and validate its config.
pub fn doctor_check(workflow_registry: &Value) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let Some(workflows) = workflow_registry
        .pointer("/workflows")
        .and_then(Value::as_object)
    else {
        return out;
    };

    for (wf_id, wf_def) in workflows {
        for_each_executor_site(wf_def, |site| {
            match site.executor.get("kind").and_then(Value::as_str) {
                Some("agent") => check_agent(wf_id, &site.location, site.executor, &mut out),
                // FM13: concurrent agents must edit disjoint files. A
                // `parallel` step whose `kind: agent` branches declare
                // overlapping `owned_files` would clobber each other — reject
                // at check (prevention), before any run.
                Some("parallel") => {
                    check_parallel_owned_files(wf_id, &site.location, site.executor, &mut out)
                }
                _ => {}
            }
        });
    }
    out
}

/// Reject overlapping `owned_files` across the `kind: agent` branches of a
/// `parallel` step (FM13). Two agents that may modify the same file cannot
/// run concurrently safely.
fn check_parallel_owned_files(
    wf_id: &str,
    location: &str,
    executor: &Value,
    out: &mut Vec<Diagnostic>,
) {
    let Some(branches) = executor.get("branches").and_then(Value::as_array) else {
        return;
    };
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, branch) in branches.iter().enumerate() {
        if branch.get("kind").and_then(Value::as_str) != Some("agent") {
            continue;
        }
        let files = branch
            .get("owned_files")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        for f in files {
            if let Some(prev) = seen.insert(f.to_string(), i) {
                out.push(Diagnostic::Error(format!(
                    "AGENT_PARALLEL_FILE_OVERLAP: workflow '{wf_id}' {location} parallel branches \
                     {prev} and {i} both declare `owned_files` entry '{f}' — concurrent agents \
                     must edit disjoint files (FM13)."
                )));
            }
        }
    }
}

fn check_agent(wf_id: &str, location: &str, executor: &Value, out: &mut Vec<Diagnostic>) {
    match AgentExecutorConfig::from_value(executor.clone()) {
        Err(e) => out.push(Diagnostic::Error(format!(
            "workflow '{wf_id}' {location}: {e}"
        ))),
        Ok(cfg) => {
            if cfg.tools.iter().any(|t| t == RESERVED_SELF_TOOL) {
                out.push(Diagnostic::Error(format!(
                    "AGENT_FORBIDDEN_SELF_TOOL: workflow '{wf_id}' {location} lists `{RESERVED_SELF_TOOL}` \
                     in `tools` — an agent must not be given a tool that drives its own gateway \
                     (re-entrancy). Expose external MCP connections only."
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reg(executor: Value) -> Value {
        json!({
            "workflows": {
                "wf": { "states": { "s": { "transitions": {
                    "go": { "target": "done", "executor": executor }
                } } } }
            }
        })
    }

    #[test]
    fn well_formed_agent_step_passes() {
        let d = doctor_check(&reg(json!({
            "kind": "agent", "affinity": "coding", "goal": "do it", "tools": ["github"]
        })));
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn unenforceable_field_fails_at_check() {
        let d = doctor_check(&reg(json!({
            "kind": "agent", "affinity": "coding", "goal": "g", "max_cost_usd": 5.0
        })));
        assert_eq!(d.len(), 1);
        assert!(matches!(&d[0], Diagnostic::Error(m) if m.contains("AGENT_CONFIG_PARSE_ERROR")));
    }

    #[test]
    fn both_bindings_fail_at_check() {
        let d = doctor_check(&reg(json!({
            "kind": "agent", "agent": "a", "affinity": "reasoning", "goal": "g"
        })));
        assert!(matches!(&d[0], Diagnostic::Error(m) if m.contains("AGENT_INVALID_MODEL_BINDING")));
    }

    #[test]
    fn praxec_self_in_tools_rejected() {
        let d = doctor_check(&reg(json!({
            "kind": "agent", "affinity": "coding", "goal": "g", "tools": ["praxec"]
        })));
        assert_eq!(d.len(), 1);
        assert!(matches!(&d[0], Diagnostic::Error(m) if m.contains("AGENT_FORBIDDEN_SELF_TOOL")));
    }

    #[test]
    fn non_agent_kinds_ignored() {
        assert!(doctor_check(&reg(json!({ "kind": "cli", "command": "ls" }))).is_empty());
    }

    #[test]
    fn parallel_agents_with_overlapping_owned_files_rejected() {
        let d = doctor_check(&reg(json!({
            "kind": "parallel",
            "join": "all",
            "branches": [
                { "kind": "agent", "affinity": "coding", "goal": "g", "owned_files": ["src/lib.rs"] },
                { "kind": "agent", "affinity": "reasoning", "goal": "g", "owned_files": ["src/lib.rs"] }
            ]
        })));
        assert!(
            d.iter().any(
                |x| matches!(x, Diagnostic::Error(m) if m.contains("AGENT_PARALLEL_FILE_OVERLAP"))
            ),
            "got: {d:?}"
        );
    }

    #[test]
    fn parallel_agents_with_disjoint_owned_files_pass() {
        let d = doctor_check(&reg(json!({
            "kind": "parallel",
            "join": "all",
            "branches": [
                { "kind": "agent", "affinity": "coding", "goal": "g", "owned_files": ["src/a.rs"] },
                { "kind": "agent", "affinity": "reasoning", "goal": "g", "owned_files": ["src/b.rs"] }
            ]
        })));
        assert!(d.is_empty(), "disjoint files must pass; got: {d:?}");
    }
}
