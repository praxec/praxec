//! Derives, per transition, the executor-output fields+values to emit so that
//! downstream guards (on the TARGET state's outgoing transitions) are satisfied
//! — letting a guard-gated flow traverse under mock execution.

use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

use crate::analysis::dummy::dummy_for_schema;
use crate::analysis::expr::{GuardClause, parse_clause, satisfying_value};
use crate::analysis::output_map::{OutputSource, analyze_output, insert_nested, remove_nested};

/// transitionName -> the executor output the mock emits for it.
///
/// A whole JSON `Value`, not a field map: a transition may map a context slot
/// from the ENTIRE executor result (`slot: "$.output"`, the shape every
/// `kind: mcp` leaf uses), and that result is often not an object — e.g.
/// `corpus_search` returns a bare array of passages. A field-map-only plan can
/// only ever emit `{}` there, which then fails the slot's declared `array`
/// contract for a reason the definition is not guilty of.
pub type OutputPlan = HashMap<String, Value>;

/// The mock's output object for a transition, created empty if absent.
///
/// Coerces a non-object entry back to an object: callers that plan individual
/// FIELDS need somewhere to put them. A whole-output plan (see
/// `OutputSource::Whole`) is therefore applied LAST, so it wins over field
/// planning for the same transition rather than being silently flattened.
pub(crate) fn output_obj<'a>(
    plan: &'a mut OutputPlan,
    transition: &str,
) -> &'a mut Map<String, Value> {
    let entry = plan
        .entry(transition.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    entry
        .as_object_mut()
        .expect("just coerced to an object above")
}

/// Guard clauses that an auto-chained deterministic path can reach from
/// `state`.  A mock output is often consumed after one or more deterministic
/// routing states, so planning only the immediate target state's guards leaves
/// a real context value overwritten by an incomplete mock before its reader.
fn reachable_deterministic_guards(
    states: &Map<String, Value>,
    state: &str,
    visited: &mut HashSet<String>,
) -> Vec<GuardClause> {
    if !visited.insert(state.to_owned()) {
        return Vec::new();
    }

    let mut clauses = Vec::new();
    let Some(transitions) = states
        .get(state)
        .and_then(|state| state.get("transitions"))
        .and_then(Value::as_object)
    else {
        return clauses;
    };

    for transition in transitions.values() {
        // Every outgoing edge can consume an upstream mock output, regardless
        // of who performs that edge. Only automatic deterministic edges are
        // safe to traverse beyond this state; agent and human edges stop the
        // chain but their own guard still needs a satisfiable producer value.
        if let Some(guards) = transition["guards"].as_array() {
            for guard in guards {
                if let Some(expr) = guard["expr"].as_str()
                    && let Some(clause) = parse_clause(expr)
                {
                    clauses.push(clause);
                }
            }
        }
        // Omitted actor defaults to deterministic in the resolved workflow
        // contract; preserve that behavior for compact fixtures as well.
        if transition["actor"]
            .as_str()
            .is_some_and(|actor| actor != "deterministic")
        {
            continue;
        }
        if let Some(target) = transition["target"].as_str() {
            clauses.extend(reachable_deterministic_guards(states, target, visited));
        }
    }

    clauses
}

/// `definition` is a resolved workflow definition JSON (one `workflows.<id>` value).
pub fn derive_plan(definition: &Value) -> OutputPlan {
    // TODO(P1.1): composite keys if names collide across states
    let mut plan: OutputPlan = HashMap::new();

    let Some(states) = definition["states"].as_object() else {
        return plan;
    };

    // For each outgoing transition T from state S -> target S2, collect guards
    // reachable through deterministic auto-chains and plan values for any output
    // slot T writes.  This is intentionally transitive: routing states can sit
    // between a producer and the first guard that reads its output.
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

            let mut visited = HashSet::new();
            let clauses = reachable_deterministic_guards(states, target, &mut visited);
            if clauses.is_empty() {
                continue;
            }

            for (slot, source) in &output_mappings {
                let OutputSource::Field(field) = source else {
                    continue;
                };
                // For each downstream clause that reads this slot, compute a satisfying value.
                //
                // A guard routinely reads INTO the slot — `$.context.codegen.ok`,
                // `$.context.verify.status` — while the transition writes the slot
                // WHOLE (`codegen: "$.output.json"`). Matching only on an exact
                // slot name misses every one of those, so the mock emitted a bare
                // `{}` for the object, the guard found no `.ok`, and the runtime
                // failed it with GUARD_UNSET_SLOT: a defect of the mock, reported
                // against the definition.
                //
                // So match on the clause's TOP segment and plant the satisfying
                // value at the sub-path INSIDE the emitted field.
                for clause in &clauses {
                    let (top, rest) = match clause.slot.split_once('.') {
                        Some((t, r)) => (t, Some(r)),
                        None => (clause.slot.as_str(), None),
                    };
                    if top != slot.as_str() {
                        continue;
                    }
                    let Some(v) = satisfying_value(clause) else {
                        continue;
                    };
                    // Nest at `field` + the guard's sub-path. `field` itself may be
                    // dotted (`build_passed: "$.output.json.passed"` → field
                    // "json.passed"): inserting it as a LITERAL key would leave the
                    // runtime resolving `$.output.json.passed` to the dummy instead,
                    // so a bool build_passed reads as the string "fuzz" and every
                    // exhaustive gate branch fails ("no viable deterministic
                    // transition"). Split BOTH into path segments.
                    let mut parts: Vec<&str> = field.split('.').collect();
                    if let Some(sub) = rest {
                        parts.extend(sub.split('.'));
                    }
                    // Last-writer-wins on conflicting clauses is acceptable for P1.
                    // `insert_nested` won't clobber, so remove any prior leaf first.
                    let entry = output_obj(&mut plan, t_name);
                    remove_nested(entry, &parts);
                    insert_nested(entry, &parts, v);
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

            // A capability declares its invokable outputs under `snippet.outputs`;
            // a nestable FLOW declares them at the top level (flows have no
            // `snippet:`). Same harvest rule `expand_use_bindings` uses to embed
            // `_snippetOutputs` — so the mock plans exactly what the callee is on
            // the hook for, whichever kind it is.
            let Some(outputs) = cap_def["snippet"]["outputs"]
                .as_object()
                .or_else(|| cap_def["outputs"].as_object())
            else {
                continue;
            };

            let outputs: Vec<(String, Value)> = outputs
                .iter()
                .map(|(n, s)| (n.clone(), s.clone()))
                .collect();
            for (out_name, schema) in outputs {
                output_obj(plan, t_name)
                    .entry(out_name)
                    .or_insert_with(|| dummy_for_schema(&schema));
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
        assert!(
            plan.get("x")
                .map(|v| v.as_object().is_none_or(Map::is_empty))
                .unwrap_or(true)
        );
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

    #[test]
    fn output_satisfies_guard_after_deterministic_routing_hop() {
        // `advance` produces the whole item, then the deterministic `route`
        // transition moves through a routing state before `kind` reads it.
        // The fuzz mock must preserve a shape that reaches that second gate.
        let d = json!({
            "initialState": "producer",
            "states": {
                "producer": { "transitions": { "advance": {
                    "target": "router", "actor": "deterministic", "executor": {"kind":"noop"},
                    "output": { "current_item": "$.output.json.item" }
                } } },
                "router": { "transitions": { "route": {
                    "target": "kind_gate", "actor": "deterministic", "executor": {"kind":"noop"}
                } } },
                "kind_gate": { "transitions": { "kind": {
                    "target": "done", "actor": "deterministic", "executor": {"kind":"noop"},
                    "guards": [ { "kind": "expr", "expr": "$.context.current_item.kind == 'doc'" } ]
                } } },
                "done": { "terminal": true, "transitions": {} }
            }
        });
        let plan = derive_plan(&d);
        assert_eq!(
            plan.get("advance")
                .and_then(|value| value.pointer("/json/item/kind")),
            Some(&json!("doc"))
        );
    }
}
