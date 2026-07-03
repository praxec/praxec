//! Derives, per transition, the executor-output fields+values to emit so that
//! downstream guards (on the TARGET state's outgoing transitions) are satisfied
//! — letting a guard-gated flow traverse under mock execution.

use serde_json::{Map, Value};
use std::collections::HashMap;

use crate::analysis::dummy::dummy_for_schema;
use crate::analysis::expr::{GuardClause, parse_clause, satisfying_value};
use crate::analysis::output_map::{OutputSource, analyze_output};

/// transitionName -> { outputField -> value }
pub type OutputPlan = HashMap<String, Map<String, Value>>;

/// `definition` is a resolved workflow definition JSON (one `workflows.<id>` value).
pub fn derive_plan(definition: &Value) -> OutputPlan {
    // TODO(P1.1): composite keys if names collide across states
    let mut plan: OutputPlan = HashMap::new();

    let Some(states) = definition["states"].as_object() else {
        return plan;
    };

    // Step 1: Build downstream guard clauses per state name.
    // downstream[stateName] = all GuardClauses from that state's OUTGOING transitions.
    // These are the clauses a flow must satisfy to LEAVE that state.
    let mut downstream: HashMap<&str, Vec<GuardClause>> = HashMap::new();
    for (state_name, state_obj) in states {
        let Some(transitions) = state_obj["transitions"].as_object() else {
            continue;
        };
        for (_t_name, t_obj) in transitions {
            let Some(guards) = t_obj["guards"].as_array() else {
                continue;
            };
            for guard in guards {
                let Some(expr_str) = guard["expr"].as_str() else {
                    continue;
                };
                if let Some(clause) = parse_clause(expr_str) {
                    downstream
                        .entry(state_name.as_str())
                        .or_default()
                        .push(clause);
                }
            }
        }
    }

    // Step 2: For each outgoing transition T from state S -> target S2,
    // look at downstream[S2] to find which context slots must be satisfied,
    // then check if T's output map feeds any of those slots from an executor field.
    for (_state_name, state_obj) in states {
        let Some(transitions) = state_obj["transitions"].as_object() else {
            continue;
        };
        for (t_name, t_obj) in transitions {
            let Some(target) = t_obj["target"].as_str() else {
                continue;
            };

            // What does this transition write to context?
            let output_mappings = analyze_output(t_obj);

            // What does the target state require?
            let Some(clauses) = downstream.get(target) else {
                continue;
            };

            for (slot, source) in &output_mappings {
                let OutputSource::Field(field) = source else {
                    continue;
                };
                // For each downstream clause that reads this slot, compute a satisfying value.
                for clause in clauses {
                    if &clause.slot == slot {
                        if let Some(v) = satisfying_value(clause) {
                            // Last-writer-wins on conflicting clauses is acceptable for P1.
                            plan.entry(t_name.clone())
                                .or_default()
                                .insert(field.clone(), v);
                        }
                    }
                }
            }
        }
    }

    plan
}

/// For each `kind: workflow` transition, emit the referenced capability's
/// declared `snippet.outputs` keys with type-appropriate dummy values, so the
/// orchestrator's `use.outputs` binding propagates and the flow advances.
/// Does NOT overwrite a guard-satisfying value already planned for the same key.
pub fn add_capability_outputs(plan: &mut OutputPlan, definition: &Value, resolved_root: &Value) {
    let Some(states) = definition["states"].as_object() else {
        return;
    };

    for (_state_name, state_obj) in states {
        let Some(transitions) = state_obj["transitions"].as_object() else {
            continue;
        };
        for (t_name, t_obj) in transitions {
            if t_obj["executor"]["kind"].as_str() != Some("workflow") {
                continue;
            }
            let Some(cap_id) = t_obj["executor"]["definitionId"].as_str() else {
                continue;
            };

            // Look up the capability definition.
            // First try exact key match, then try suffix match for namespaced ids.
            let cap_def = {
                let direct = &resolved_root["workflows"][cap_id];
                if !direct.is_null() {
                    direct
                } else if let Some(workflows) = resolved_root["workflows"].as_object() {
                    // Find any key whose suffix after the last '/' equals cap_id,
                    // or that ends with /<cap_id>.
                    workflows
                        .iter()
                        .find(|(k, _)| {
                            k.as_str()
                                .rsplit('/')
                                .next()
                                .map(|suffix| suffix == cap_id)
                                .unwrap_or(false)
                        })
                        .map(|(_, v)| v)
                        .unwrap_or(&Value::Null)
                } else {
                    &Value::Null
                }
            };

            let Some(outputs) = cap_def["snippet"]["outputs"].as_object() else {
                continue;
            };

            for (out_name, schema) in outputs {
                plan.entry(t_name.clone())
                    .or_default()
                    .entry(out_name.clone())
                    .or_insert_with(|| dummy_for_schema(schema));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def() -> Value {
        json!({
            "initialState": "start",
            "states": {
                "start": { "transitions": {
                    "go": { "target": "gated", "executor": { "kind": "noop" },
                            "output": { "approved": "$.output.approved" } }
                }},
                "gated": { "transitions": {
                    "finish": { "target": "done", "executor": { "kind": "noop" },
                                "guards": [ { "kind": "expr", "expr": "$.context.approved == true" } ] }
                }},
                "done": { "terminal": true, "transitions": {} }
            }
        })
    }

    #[test]
    fn plans_field_to_satisfy_downstream_guard() {
        let plan = derive_plan(&def());
        let go = plan.get("go").expect("go has a plan");
        assert_eq!(go.get("approved"), Some(&json!(true)));
    }

    #[test]
    fn no_plan_when_no_downstream_guard() {
        let d = json!({
            "initialState": "a",
            "states": {
                "a": { "transitions": { "x": { "target": "b", "executor": {"kind":"noop"},
                        "output": { "v": "$.output.v" } } } },
                "b": { "terminal": true, "transitions": {} }
            }
        });
        let plan = derive_plan(&d);
        assert!(plan.get("x").map(|m| m.is_empty()).unwrap_or(true));
    }

    #[test]
    fn capability_outputs_are_planned() {
        use serde_json::json;
        let resolved = json!({ "workflows": {
            "o": { "initialState": "s", "states": {
                "s": { "transitions": { "call": {
                    "target": "done",
                    "executor": { "kind": "workflow", "definitionId": "c", "use": { "outputs": { "$.context.sev": "severity" } } }
                } } },
                "done": { "terminal": true, "transitions": {} }
            }},
            "c": { "snippet": { "outputs": { "severity": { "type": "string", "enum": ["S1","S2"] } } } }
        }});
        let mut plan = derive_plan(&resolved["workflows"]["o"]);
        add_capability_outputs(&mut plan, &resolved["workflows"]["o"], &resolved);
        assert_eq!(
            plan.get("call").and_then(|m| m.get("severity")),
            Some(&json!("S1"))
        );
    }

    #[test]
    fn renamed_field_satisfies_guard() {
        // T maps output.verdict -> context.approved; downstream guard reads context.approved.
        let d = json!({
            "initialState": "s",
            "states": {
                "s": { "transitions": { "t": { "target": "g", "executor": {"kind":"noop"},
                        "output": { "approved": "$.output.verdict" } } } },
                "g": { "transitions": { "f": { "target": "d", "executor": {"kind":"noop"},
                        "guards": [ { "kind": "expr", "expr": "$.context.approved == true" } ] } } },
                "d": { "terminal": true, "transitions": {} }
            }
        });
        let plan = derive_plan(&d);
        // The OUTPUT FIELD is `verdict` (not the slot `approved`).
        assert_eq!(
            plan.get("t").and_then(|m| m.get("verdict")),
            Some(&json!(true))
        );
    }
}
