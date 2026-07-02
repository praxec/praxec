//! Per-transition fuzz engine: exercises every (state, transition) edge in a
//! workflow definition in isolation, verifying that satisfying contexts fire and
//! violating contexts are rejected.

use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::Principal;
use praxec_core::ports::{DefinitionStore, ExecutorRegistry, GuidanceAcknowledgmentStore};
use praxec_core::store::{
    ConfigDefinitionStore, InMemoryGuidanceAcknowledgmentStore, InMemoryWorkflowStore,
};
use praxec_core::WorkflowRuntime;
use serde_json::{json, Value};

use crate::analysis::output_map::{
    analyze_output, insert_nested, output_field_paths, OutputSource,
};
use crate::analysis::plan::{add_capability_outputs, derive_plan, OutputPlan};
use crate::analysis::reads::{satisfying_context, violating_context};
use crate::isolate::{submit_isolated, submit_isolated_with_acks, SubmitResult};
use crate::smartmock::SmartMockRegistry;
use crate::walk::{reachable_from, walk};

/// The verdict for a single (state, transition) edge.
pub struct TransitionVerdict {
    pub state: String,
    pub transition: String,
    pub ok: bool,
    pub detail: String,
}

/// Build a fresh (runtime, workflow_store, ack_store) triple backed by the given
/// executor registry and the provided resolved config.
///
/// An `InMemoryGuidanceAcknowledgmentStore` is wired into the guard evaluator so
/// `guidance_acknowledged` guards can be satisfied by pre-recording acks before
/// each submit (see Fix 2).
fn build_runtime(
    resolved: &Value,
    executors: Arc<dyn ExecutorRegistry>,
) -> (
    WorkflowRuntime,
    Arc<InMemoryWorkflowStore>,
    Arc<InMemoryGuidanceAcknowledgmentStore>,
) {
    let definitions: Arc<dyn DefinitionStore> =
        Arc::new(ConfigDefinitionStore::from_config(resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let ack_store = Arc::new(InMemoryGuidanceAcknowledgmentStore::new());
    let guards = Arc::new(
        DefaultGuardEvaluator::new()
            .with_ack_store(ack_store.clone() as Arc<dyn GuidanceAcknowledgmentStore>),
    );
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store.clone(),
        executors,
        guards,
        audit as Arc<dyn AuditSink>,
    );
    (runtime, store, ack_store)
}

/// Extract `(subject, expected_hash)` pairs for every `guidance_acknowledged` guard
/// in `tobj`, looking up hashes from the workflow snapshot's `_skillsLibrary`.
fn guidance_ack_subjects(tobj: &Value, snapshot: &Value) -> Vec<(String, String)> {
    let guards = match tobj.get("guards").and_then(|g| g.as_array()) {
        Some(g) => g,
        None => return vec![],
    };
    let mut pairs = Vec::new();
    for guard in guards {
        if guard.get("kind").and_then(Value::as_str) != Some("guidance_acknowledged") {
            continue;
        }
        let Some(subject) = guard.get("subject").and_then(Value::as_str) else {
            continue;
        };
        // Hash lives at `snapshot._skillsLibrary.<subject>.hash`.
        let Some(hash) = snapshot
            .pointer("/_skillsLibrary")
            .and_then(Value::as_object)
            .and_then(|lib| lib.get(subject))
            .and_then(|entry| entry.get("hash"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        pairs.push((subject.to_string(), hash.to_string()));
    }
    pairs
}

/// Build a definition-wide output plan covering EVERY transition's output fields.
///
/// Starts from guard-satisfying values (`derive_plan`) and capability outputs
/// (`add_capability_outputs`), then ensures every transition's own output fields
/// have at least a blackboard-typed dummy — without overwriting guard-satisfying
/// values already planned.
///
/// This ensures that when the runtime AUTO-CHAINS deterministic transitions (e.g.
/// `draft → vet → …`), the mock can resolve outputs for any chained transition,
/// not only the transition under test.
fn def_wide_plan(resolved: &Value, def_id: &str) -> OutputPlan {
    let def = &resolved["workflows"][def_id];

    // Guard-satisfying values for downstream guards + capability snippet outputs.
    let mut plan = derive_plan(def);
    add_capability_outputs(&mut plan, def, resolved);

    // Ensure every transition's output fields are present with a typed dummy,
    // without overwriting guard-satisfying values already planned.
    //
    // For paths like `$.output.json.deployId` (full_path = "json.deployId") we
    // build a NESTED object so the runtime can resolve the full path correctly:
    //   plan["run_deploy"] = { "json": { "deployId": <typed-dummy> } }
    if let Some(states) = def.get("states").and_then(|s| s.as_object()) {
        for state in states.values() {
            if let Some(ts) = state.get("transitions").and_then(|t| t.as_object()) {
                for (tname, tobj) in ts {
                    for (slot, full_path) in output_field_paths(tobj) {
                        let val = def
                            .get("blackboard")
                            .and_then(|b| b.get(&slot))
                            .map(crate::analysis::dummy::dummy_for_schema)
                            .unwrap_or_else(|| json!("fuzz"));
                        let parts: Vec<&str> = full_path.split('.').collect();
                        let entry = plan.entry(tname.clone()).or_default();
                        insert_nested(entry, &parts, val);
                    }
                }
            }
        }
    }

    plan
}

/// Build a human principal (carries the "human" role the runtime checks for
/// `actor: "human"` transitions).
fn human_principal() -> Principal {
    Principal {
        subject: "fuzz-human".to_string(),
        roles: vec![Principal::HUMAN_ROLE.to_string()],
        permissions: Vec::new(),
    }
}

/// Build a satisfying `$.workflow.input` object from the workflow-level
/// `inputSchema` (the compiled form of `inputs:`). Output mappings that read
/// `$.workflow.input.*` (e.g. an entry transition copying caller input onto the
/// blackboard) resolve against this — without it they read null and trip a
/// false BLACKBOARD_TYPE_ERROR.
fn input_for(def: &serde_json::Value) -> serde_json::Value {
    // Emit EVERY declared input property (optional ones too) — an output that
    // copies an optional `$.workflow.input.<x>` onto a typed blackboard slot
    // still needs a value, otherwise it writes null and trips a type error.
    def.get("inputSchema")
        .map(crate::analysis::dummy::dummy_all_properties)
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Build a satisfying `arguments` object from a transition's `inputSchema`.
/// For each property in `inputSchema/properties`, emit a typed dummy value.
/// Returns `{}` when the transition has no `inputSchema` or no `properties`.
fn arguments_for(transition: &serde_json::Value) -> serde_json::Value {
    let Some(props) = transition
        .pointer("/inputSchema/properties")
        .and_then(|v| v.as_object())
    else {
        return serde_json::json!({});
    };
    let mut m = serde_json::Map::new();
    for (name, schema) in props {
        m.insert(
            name.clone(),
            crate::analysis::dummy::dummy_for_schema(schema),
        );
    }
    serde_json::Value::Object(m)
}

/// Test every transition in `definition` (one resolved workflow def) in
/// isolation. `resolved` is the whole config root (for snapshot lookup).
pub async fn fuzz_transitions(
    resolved: &Value,
    definition_id: &str,
) -> anyhow::Result<Vec<TransitionVerdict>> {
    let def = &resolved["workflows"][definition_id];
    let edges = walk(def).edges;

    // Load the definition snapshot once — every isolated submit needs it.
    let definitions = ConfigDefinitionStore::from_config(resolved);
    let snapshot = definitions.load(definition_id).await?;

    // Build a definition-wide mock plan ONCE: covers every transition's outputs
    // so that auto-chained deterministic transitions can also resolve their
    // output mappings (fixes CHAIN_FAILED residue).
    let def_plan = def_wide_plan(resolved, definition_id);

    // Blackboard slot-type map + a satisfying workflow-input object, derived once.
    // `blackboard` lets the seeded context match each slot's declared type;
    // `workflow_input` populates `$.workflow.input.*` reads.
    let blackboard = def.get("blackboard").cloned().unwrap_or_else(|| json!({}));
    let workflow_input = input_for(def);

    let mut verdicts = Vec::new();

    for edge in edges {
        let state = &edge.state;
        let transition = &edge.transition;
        let target = &edge.target;

        let tobj = &def["states"][state.as_str()]["transitions"][transition.as_str()];

        // ── Fix 3: Skip synthetic `ask_human` transitions ─────────────────────
        // Workflows with `enable_human_ask: true` get a synthetic `ask_human`
        // self-loop injected into every non-terminal state by the config resolver
        // (crates/praxec-core/src/config.rs `inject_human_ask_transitions`).
        // These carry `"purpose": "ask"` in their definition — that marker
        // distinguishes them from genuine author-defined transitions.
        // The per-transition harness cannot drive paging interactions, so we skip
        // them rather than emitting a false CHAIN_FAILED verdict.
        if tobj.get("purpose").and_then(Value::as_str) == Some("ask") {
            continue;
        }

        // Determine actor.
        let is_human = tobj["actor"].as_str() == Some("human");
        let principal = if is_human {
            human_principal()
        } else {
            Principal::anonymous()
        };

        // ── Fix 2: Extract guidance_acknowledged subjects to pre-record ────────
        // `guidance_acknowledged` guards check that the subject skill's hash was
        // acknowledged for this specific instance ID. We collect (subject, hash)
        // pairs here; submit_isolated_with_acks records them after the instance ID
        // is known but before the runtime evaluates guards.
        let ack_subjects = guidance_ack_subjects(tobj, &snapshot);

        // ── Satisfying submit ─────────────────────────────────────────────────
        // Determine whether every MOCK-EMITTED (Field) output slot has a typed
        // blackboard declaration. Literal/Other outputs are the workflow's own
        // values — a type mismatch there is always a real defect, never excused.
        //
        // We compute this over ONLY OutputSource::Field slots:
        //   - If there are no Field slots (e.g. all outputs are literals), the
        //     mock emits nothing, so there's no mock-typing ambiguity at all.
        //     all_output_slots_typed is vacuously true.
        //   - If there are Field slots and all are typed → true (typed dummies
        //     are correct; any BLACKBOARD_TYPE_ERROR is a structural error).
        //   - If there are Field slots and any lack a type → false (excuse is
        //     valid: the mock emitted null for an untyped slot).
        //
        // Consequence: a literal output `x: true` where blackboard says
        // `x: { type: string }` → no Field slots → all_output_slots_typed=true
        // → BLACKBOARD_TYPE_ERROR is NOT excused → ok=false (defect caught).
        let all_output_slots_typed = {
            let outputs = analyze_output(tobj);
            let field_slots: Vec<&str> = outputs
                .iter()
                .filter_map(|(slot, src)| {
                    if matches!(src, OutputSource::Field(_)) {
                        Some(slot.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            // Vacuously true when no Field slots exist (no mock-emitted values).
            field_slots.iter().all(|slot| {
                def.get("blackboard")
                    .and_then(|b| b.get(*slot))
                    .and_then(|s| s.get("type"))
                    .is_some()
            })
        };

        let sat_ctx = satisfying_context(tobj, &blackboard);
        let executors: Arc<dyn ExecutorRegistry> =
            Arc::new(SmartMockRegistry::new(def_plan.clone()));
        let (rt, store, ack_store) = build_runtime(resolved, executors);

        let sat_result = if ack_subjects.is_empty() {
            submit_isolated(
                &rt,
                &store,
                &snapshot,
                definition_id,
                state,
                sat_ctx,
                workflow_input.clone(),
                transition,
                principal.clone(),
                arguments_for(tobj),
            )
            .await?
        } else {
            submit_isolated_with_acks(
                &rt,
                &store,
                &snapshot,
                definition_id,
                state,
                sat_ctx,
                workflow_input.clone(),
                transition,
                principal.clone(),
                arguments_for(tobj),
                ack_store.clone() as Arc<dyn GuidanceAcknowledgmentStore>,
                ack_subjects.clone(),
            )
            .await?
        };

        let (sat_ok, sat_detail) = match sat_result {
            SubmitResult::Fired { to_state, .. } => {
                if to_state == *target || reachable_from(def, target).contains(&to_state) {
                    (true, String::new())
                } else {
                    (
                        false,
                        format!("fired to '{to_state}', not reachable from target '{target}'"),
                    )
                }
            }
            SubmitResult::Rejected { ref code } if code == "ACTOR_MISMATCH" => {
                // Human-gated transition submitted as anonymous — expected when
                // we cannot yet construct the right human principal path OR when
                // the runtime still rejects. Mark ok only for non-human edges.
                // For human-actor edges we try a human principal, so if it still
                // rejects that's a real failure.
                if is_human {
                    (false, "ACTOR_MISMATCH despite human principal".to_string())
                } else {
                    (true, "human-gated (ACTOR_MISMATCH on anon)".to_string())
                }
            }
            SubmitResult::Rejected { ref code } if code == "GUARD_REJECTED" => {
                // The satisfier failed — evidence/role guard the analysis can't satisfy.
                (
                    false,
                    "could not satisfy guard (evidence/role?)".to_string(),
                )
            }
            SubmitResult::Rejected { code } => (false, format!("unexpected rejection: {code}")),
            SubmitResult::Errored { code } if code == "BLACKBOARD_TYPE_ERROR" => {
                // With typed dummies this should rarely fire. If it does and all
                // output slots are fully typed (i.e., we emitted correct dummies),
                // treat it as a real failure — something structural is wrong.
                // Only excuse it when at least one slot lacked a blackboard type
                // declaration and we fell back to an untyped dummy.
                if all_output_slots_typed {
                    (
                        false,
                        "BLACKBOARD_TYPE_ERROR despite typed dummy — structural error".to_string(),
                    )
                } else {
                    (true, "mock type mismatch (BLACKBOARD_TYPE_ERROR — slot has no blackboard type declaration)".to_string())
                }
            }
            SubmitResult::Errored { code } => (false, format!("errored: {code}")),
        };

        // ── Violating submit ──────────────────────────────────────────────────
        // Only run when a parseable violating context exists (i.e., there's an
        // expr guard we can flip).
        let (viol_ok, viol_detail) = if let Some(viol_ctx) = violating_context(tobj, &blackboard) {
            let executors2: Arc<dyn ExecutorRegistry> =
                Arc::new(SmartMockRegistry::new(def_plan.clone()));
            let (rt2, store2, ack_store2) = build_runtime(resolved, executors2);

            // For the violating path, do NOT pre-record guidance acks — we want
            // to test that the guard blocks when not acknowledged. (The expr guard
            // is what we're flipping; guidance_acknowledged stays as-is on the
            // violating path.)
            let viol_result = submit_isolated(
                &rt2,
                &store2,
                &snapshot,
                definition_id,
                state,
                viol_ctx,
                workflow_input.clone(),
                transition,
                principal,
                arguments_for(tobj),
            )
            .await?;
            let _ = ack_store2; // suppress unused warning

            match viol_result {
                SubmitResult::Rejected { .. } => (true, String::new()),
                SubmitResult::Fired { to_state, .. } => (
                    false,
                    format!("guard did not reject a violating context (fired to '{to_state}')"),
                ),
                SubmitResult::Errored { code } => {
                    // An error on the violating path is not a guard bypass;
                    // treat as ok (the transition didn't fire).
                    (true, format!("errored on violating (ok): {code}"))
                }
            }
        } else {
            (true, String::new())
        };

        let ok = sat_ok && viol_ok;
        let detail = [sat_detail, viol_detail]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("; ");

        verdicts.push(TransitionVerdict {
            state: state.clone(),
            transition: transition.clone(),
            ok,
            detail,
        });
    }

    Ok(verdicts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxec_core::config;
    use serde_json::json;

    /// Guarded workflow: draft --submit[guard ready==true]--> review.
    /// Mirrors the inline config used in isolate.rs tests.
    fn guarded_config() -> Value {
        let raw = json!({
            "version": "1.0.0",
            "workflows": {
                "wf": {
                    "version": "1.0.0",
                    "initialState": "draft",
                    "states": {
                        "draft": {
                            "transitions": {
                                "submit": {
                                    "target": "review",
                                    "actor": "agent",
                                    "guards": [
                                        { "kind": "expr", "expr": "$.context.ready == true" }
                                    ],
                                    "executor": { "kind": "noop" }
                                }
                            }
                        },
                        "review": { "terminal": true, "transitions": {} }
                    }
                }
            }
        });
        config::resolve(raw).expect("resolve guarded_config")
    }

    /// Unguarded workflow: start --go--> done (no guards).
    fn unguarded_config() -> Value {
        let raw = json!({
            "version": "1.0.0",
            "workflows": {
                "ug": {
                    "version": "1.0.0",
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": {
                                    "target": "done",
                                    "actor": "agent",
                                    "executor": { "kind": "noop" }
                                }
                            }
                        },
                        "done": { "terminal": true, "transitions": {} }
                    }
                }
            }
        });
        config::resolve(raw).expect("resolve unguarded_config")
    }

    #[tokio::test]
    async fn guarded_submit_fires_and_rejects() {
        let resolved = guarded_config();
        let verdicts = fuzz_transitions(&resolved, "wf")
            .await
            .expect("fuzz_transitions");
        // Only one edge: draft --submit--> review
        assert_eq!(verdicts.len(), 1, "expected exactly one edge");
        let v = &verdicts[0];
        assert_eq!(v.state, "draft");
        assert_eq!(v.transition, "submit");
        assert!(
            v.ok,
            "submit verdict should be ok=true; detail: {}",
            v.detail
        );
    }

    #[tokio::test]
    async fn unguarded_transition_fires() {
        let resolved = unguarded_config();
        let verdicts = fuzz_transitions(&resolved, "ug")
            .await
            .expect("fuzz_transitions");
        // Only one edge: start --go--> done
        assert_eq!(verdicts.len(), 1, "expected exactly one edge");
        let v = &verdicts[0];
        assert_eq!(v.state, "start");
        assert_eq!(v.transition, "go");
        assert!(
            v.ok,
            "unguarded go verdict should be ok=true; detail: {}",
            v.detail
        );
    }
}
