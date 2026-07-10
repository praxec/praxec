//! SPEC §18 — structural analysis executor.
//!
//! Validates a candidate workflow or skill fragment against the closed set
//! of required structural rules. Output shape:
//!
//! ```jsonc
//! { "issues": [
//!     { "rule":     "CYCLE_DETECTED",
//!       "severity": "error",
//!       "location": "/workflows/demo/states/foo/transitions/bar/target",
//!       "message":  "transition path forms a cycle: foo → bar → foo" } ] }
//! ```
//!
//! A rule that fails to execute returns `Err`, not an empty issue list,
//! so coverage gaps are visible (SPEC §18.3 self-check invariant, FMECA
//! FM-5).

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use praxec_core::discovery::BLESSED_SUBJECT_ROOTS;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use serde_json::{Value, json};

/// Names of every required rule, in spec order. Exposed publicly so the
/// rules-self-check test can iterate the closed set without depending on
/// internal enum identity.
pub const REQUIRED_RULES: &[&str] = &[
    "CYCLE_DETECTED",
    "DEAD_STATE",
    "UNDEFINED_TARGET",
    "UNDECLARED_SLOT_READ",
    "UNBLESSED_SUBJECT_ROOT",
    "NO_TRANSITIONS",
    "OVERSIZED_STATE",
    "UNTRUSTED_RAW_EXECUTION",
];

/// Default threshold for the OVERSIZED_STATE rule. Configurable in T3 when
/// the extensibility hook ships; pinned at 8 for v1.
const OVERSIZED_STATE_THRESHOLD: usize = 8;

pub struct StructuralAnalysisExecutor;

#[async_trait]
impl Executor for StructuralAnalysisExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        // Prefer explicit `arguments.definition`. As a convenience for the
        // reference authoring workflow (§17.1), fall back to
        // `workflow.context.candidate_definition` so deterministic chain
        // transitions — which receive `arguments: {}` — can still hand a
        // candidate to the analyzer via the context slot.
        let definition = request
            .arguments
            .get("definition")
            .cloned()
            .or_else(|| {
                request
                    .workflow
                    .context
                    .get("candidate_definition")
                    .cloned()
            })
            .ok_or_else(|| {
                ExecutorError::Permanent(
                    "structural_analysis: missing required argument `definition` \
                     (and no `candidate_definition` in workflow context)"
                        .into(),
                )
            })?;

        // Normalize: if the candidate is a bare workflow definition (has
        // `initialState` + `states` at the top level, no `workflows:`
        // wrapper), wrap it as `workflows.candidate`. Authoring workflows
        // typically pass bare definitions; production configs always have
        // the wrapper. Both shapes route through the same rule set.
        let normalized = normalize_candidate(definition);

        let mut issues: Vec<Value> = Vec::new();

        // Each rule appends issues OR returns Err. An Err propagates and the
        // caller sees the executor failed — there is no "silently no issues"
        // path that could mask a coverage gap.
        rule_no_transitions(&normalized, &mut issues)?;
        rule_undefined_target(&normalized, &mut issues)?;
        rule_dead_state(&normalized, &mut issues)?;
        rule_cycle_detected(&normalized, &mut issues)?;
        rule_oversized_state(&normalized, &mut issues)?;
        rule_undeclared_slot_read(&normalized, &mut issues)?;
        rule_unblessed_subject_root(&normalized, &mut issues)?;
        rule_untrusted_execution(&normalized, &mut issues)?;

        let issue_count = issues.len();
        Ok(ExecuteResult {
            output: json!({
                "issues":      issues,
                "issue_count": issue_count,
            }),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

/// Normalize a candidate definition into the canonical `{workflows: {<id>: ...}}`
/// shape. A bare workflow (has `initialState` + `states` at top level) is
/// wrapped as `workflows.candidate`; a definition already containing
/// `workflows:` is returned unchanged.
/// SECURITY (README "Who can run what"): an authored/untrusted candidate must
/// not INTRODUCE a raw command — an inline `kind: cli` command, an inline
/// `kind: mcp` spawn, or its own `connections:` block — which would route
/// execution around the hash-pinned script safety net. The `registry` publish
/// gate enforces this hard; this rule surfaces it in the authoring graph so a
/// dry-run/structural pass catches it before publish.
fn rule_untrusted_execution(
    definition: &Value,
    issues: &mut Vec<Value>,
) -> Result<(), ExecutorError> {
    if let Some(reason) = crate::untrusted_execution::introduces_raw_command(definition) {
        issues.push(json!({
            "rule": "UNTRUSTED_RAW_EXECUTION",
            "severity": "error",
            "location": "/workflows",
            "message": format!(
                "authored definition introduces raw execution — {reason}; use a hash-pinned \
                 `kind: script` (and operator-declared connections) instead"
            ),
        }));
    }
    Ok(())
}

fn normalize_candidate(definition: Value) -> Value {
    if definition.get("workflows").is_some() {
        return definition;
    }
    let is_bare_workflow =
        definition.get("initialState").is_some() && definition.get("states").is_some();
    if is_bare_workflow {
        json!({ "workflows": { "candidate": definition } })
    } else {
        definition
    }
}

fn push_issue(
    issues: &mut Vec<Value>,
    rule: &'static str,
    severity: &'static str,
    location: String,
    message: String,
) {
    issues.push(json!({
        "rule":     rule,
        "severity": severity,
        "location": location,
        "message":  message,
    }));
}

// ── NO_TRANSITIONS ──────────────────────────────────────────────────────────

fn rule_no_transitions(definition: &Value, issues: &mut Vec<Value>) -> Result<(), ExecutorError> {
    let workflows = match definition.get("workflows").and_then(Value::as_object) {
        Some(w) => w,
        // A definition with no `workflows:` block isn't necessarily an error
        // — proxy-only configs are valid. The rule simply doesn't apply.
        None => return Ok(()),
    };
    for (wf_id, wf) in workflows {
        let states = wf
            .get("states")
            .and_then(Value::as_object)
            .map(|s| s.values().collect::<Vec<_>>())
            .unwrap_or_default();
        let any_transition = states.iter().any(|s| {
            s.get("transitions")
                .and_then(Value::as_object)
                .map(|t| !t.is_empty())
                .unwrap_or(false)
        });
        if !any_transition {
            push_issue(
                issues,
                "NO_TRANSITIONS",
                "error",
                format!("/workflows/{wf_id}"),
                format!("workflow '{wf_id}' has zero transitions across all states"),
            );
        }
    }
    Ok(())
}

// ── UNDEFINED_TARGET ────────────────────────────────────────────────────────

fn rule_undefined_target(definition: &Value, issues: &mut Vec<Value>) -> Result<(), ExecutorError> {
    let Some(workflows) = definition.get("workflows").and_then(Value::as_object) else {
        return Ok(());
    };
    for (wf_id, wf) in workflows {
        let states = match wf.get("states").and_then(Value::as_object) {
            Some(s) => s,
            None => continue,
        };
        let known: HashSet<&String> = states.keys().collect();
        for (state_name, state) in states {
            let Some(transitions) = state.get("transitions").and_then(Value::as_object) else {
                continue;
            };
            for (t_name, t) in transitions {
                let Some(target) = t.get("target").and_then(Value::as_str) else {
                    continue;
                };
                if !known.contains(&target.to_string()) {
                    push_issue(
                        issues,
                        "UNDEFINED_TARGET",
                        "error",
                        format!(
                            "/workflows/{wf_id}/states/{state_name}/transitions/{t_name}/target"
                        ),
                        format!("transition '{t_name}' targets undefined state '{target}'"),
                    );
                }
            }
        }
    }
    Ok(())
}

// ── DEAD_STATE ──────────────────────────────────────────────────────────────

fn rule_dead_state(definition: &Value, issues: &mut Vec<Value>) -> Result<(), ExecutorError> {
    let Some(workflows) = definition.get("workflows").and_then(Value::as_object) else {
        return Ok(());
    };
    for (wf_id, wf) in workflows {
        let initial = wf.get("initialState").and_then(Value::as_str);
        let Some(states) = wf.get("states").and_then(Value::as_object) else {
            continue;
        };
        let mut inbound: HashMap<String, usize> = states.keys().map(|k| (k.clone(), 0)).collect();
        for state in states.values() {
            let Some(transitions) = state.get("transitions").and_then(Value::as_object) else {
                continue;
            };
            for t in transitions.values() {
                if let Some(target) = t.get("target").and_then(Value::as_str) {
                    if let Some(c) = inbound.get_mut(target) {
                        *c += 1;
                    }
                }
            }
        }
        for (state_name, count) in &inbound {
            if *count == 0 && Some(state_name.as_str()) != initial {
                push_issue(
                    issues,
                    "DEAD_STATE",
                    "error",
                    format!("/workflows/{wf_id}/states/{state_name}"),
                    format!(
                        "state '{state_name}' has no inbound transition and is not the initial state"
                    ),
                );
            }
        }
    }
    Ok(())
}

// ── CYCLE_DETECTED ──────────────────────────────────────────────────────────
//
// A cycle in a transition graph isn't always wrong — self-loops with a guard
// counter (SPEC stress test s01) are legitimate. The rule flags cycles that
// have **no** guards anywhere along the path: those are unbounded loops with
// no exit condition, almost certainly a mistake. Pragmatic v1 heuristic.

fn rule_cycle_detected(definition: &Value, issues: &mut Vec<Value>) -> Result<(), ExecutorError> {
    let Some(workflows) = definition.get("workflows").and_then(Value::as_object) else {
        return Ok(());
    };
    for (wf_id, wf) in workflows {
        let Some(states) = wf.get("states").and_then(Value::as_object) else {
            continue;
        };
        // Build adjacency: state -> [(target, has_guard)]
        let mut adj: HashMap<String, Vec<(String, bool)>> = HashMap::new();
        for (state_name, state) in states {
            let mut edges = Vec::new();
            if let Some(transitions) = state.get("transitions").and_then(Value::as_object) {
                for t in transitions.values() {
                    let Some(target) = t.get("target").and_then(Value::as_str) else {
                        continue;
                    };
                    let has_guard = t
                        .get("guards")
                        .and_then(Value::as_array)
                        .map(|a| !a.is_empty())
                        .unwrap_or(false);
                    edges.push((target.to_string(), has_guard));
                }
            }
            adj.insert(state_name.clone(), edges);
        }
        for start in states.keys() {
            let mut stack: Vec<(String, Vec<String>, bool)> =
                vec![(start.clone(), vec![start.clone()], true)];
            while let Some((node, path, all_unguarded)) = stack.pop() {
                let Some(edges) = adj.get(&node) else {
                    continue;
                };
                for (next, has_guard) in edges {
                    if next == start && all_unguarded && !has_guard {
                        push_issue(
                            issues,
                            "CYCLE_DETECTED",
                            "error",
                            format!("/workflows/{wf_id}/states/{start}"),
                            format!(
                                "unguarded cycle in workflow '{wf_id}': {} → {start}",
                                path.join(" → ")
                            ),
                        );
                        // Stop unrolling this path; one issue per start.
                        stack.clear();
                        break;
                    }
                    if !path.contains(next) {
                        let mut np = path.clone();
                        np.push(next.clone());
                        stack.push((next.clone(), np, all_unguarded && !has_guard));
                    }
                }
            }
        }
    }
    Ok(())
}

// ── OVERSIZED_STATE ─────────────────────────────────────────────────────────

fn rule_oversized_state(definition: &Value, issues: &mut Vec<Value>) -> Result<(), ExecutorError> {
    let Some(workflows) = definition.get("workflows").and_then(Value::as_object) else {
        return Ok(());
    };
    for (wf_id, wf) in workflows {
        let Some(states) = wf.get("states").and_then(Value::as_object) else {
            continue;
        };
        for (state_name, state) in states {
            let count = state
                .get("transitions")
                .and_then(Value::as_object)
                .map(|t| t.len())
                .unwrap_or(0);
            if count > OVERSIZED_STATE_THRESHOLD {
                push_issue(
                    issues,
                    "OVERSIZED_STATE",
                    "warning",
                    format!("/workflows/{wf_id}/states/{state_name}"),
                    format!(
                        "state '{state_name}' has {count} transitions (threshold: {OVERSIZED_STATE_THRESHOLD})"
                    ),
                );
            }
        }
    }
    Ok(())
}

// ── UNDECLARED_SLOT_READ ────────────────────────────────────────────────────

fn rule_undeclared_slot_read(
    definition: &Value,
    issues: &mut Vec<Value>,
) -> Result<(), ExecutorError> {
    let Some(workflows) = definition.get("workflows").and_then(Value::as_object) else {
        return Ok(());
    };
    for (wf_id, wf) in workflows {
        let declared: HashSet<String> = collect_declared_slots(wf);
        let Some(states) = wf.get("states").and_then(Value::as_object) else {
            continue;
        };
        for (state_name, state) in states {
            let Some(transitions) = state.get("transitions").and_then(Value::as_object) else {
                continue;
            };
            for (t_name, t) in transitions {
                if let Some(guards) = t.get("guards").and_then(Value::as_array) {
                    for (idx, guard) in guards.iter().enumerate() {
                        let expr = guard.get("expr").and_then(Value::as_str).unwrap_or("");
                        for slot in extract_context_slots(expr) {
                            if !declared.contains(&slot) {
                                push_issue(
                                    issues,
                                    "UNDECLARED_SLOT_READ",
                                    "error",
                                    format!(
                                        "/workflows/{wf_id}/states/{state_name}/transitions/{t_name}/guards/{idx}"
                                    ),
                                    format!(
                                        "guard reads `$.context.{slot}` which is not declared in workflow blackboard"
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn collect_declared_slots(wf: &Value) -> HashSet<String> {
    let mut out = HashSet::new();
    match wf.get("blackboard") {
        Some(Value::Array(arr)) => {
            for s in arr {
                if let Some(name) = s.as_str() {
                    out.insert(name.to_string());
                }
            }
        }
        Some(Value::Object(map)) => {
            for k in map.keys() {
                out.insert(k.clone());
            }
        }
        _ => {}
    }
    out
}

/// Extract slot names referenced as `$.context.X` in an expression string.
/// Pragmatic regex-free parse: finds occurrences of `$.context.` and reads
/// the following kebab-identifier.
fn extract_context_slots(expr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = expr;
    let needle = "$.context.";
    while let Some(idx) = rest.find(needle) {
        let after = &rest[idx + needle.len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if !name.is_empty() {
            out.push(name);
        }
        rest = after;
    }
    out
}

// ── UNBLESSED_SUBJECT_ROOT ──────────────────────────────────────────────────

fn rule_unblessed_subject_root(
    definition: &Value,
    issues: &mut Vec<Value>,
) -> Result<(), ExecutorError> {
    let Some(skills) = definition.get("skills").and_then(Value::as_object) else {
        return Ok(());
    };
    for subject in skills.keys() {
        let root = subject.split('.').next().unwrap_or("");
        if !BLESSED_SUBJECT_ROOTS.contains(&root) {
            push_issue(
                issues,
                "UNBLESSED_SUBJECT_ROOT",
                "warning",
                format!("/skills/{subject}"),
                format!(
                    "subject '{subject}' has unblessed first segment '{root}'; blessed roots: {:?}",
                    BLESSED_SUBJECT_ROOTS
                ),
            );
        }
    }
    Ok(())
}
