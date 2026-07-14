//! Per-transition fuzz engine: exercises every (state, transition) edge in a
//! workflow definition in isolation, verifying that satisfying contexts fire and
//! violating contexts are rejected.

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::Principal;
use praxec_core::ports::{DefinitionStore, ExecutorRegistry, GuidanceAcknowledgmentStore};
use praxec_core::store::{
    ConfigDefinitionStore, InMemoryGuidanceAcknowledgmentStore, InMemoryWorkflowStore,
};
use serde_json::{Value, json};

use crate::analysis::output_map::{
    OutputSource, analyze_output, insert_nested, output_field_paths, whole_output_slots,
};
use crate::analysis::plan::{OutputPlan, add_capability_outputs, derive_plan};
use crate::analysis::reads::{
    definition_wide_context, satisfying_context_over, seed_input_guards, violating_context_over,
};
use crate::isolate::{SubmitResult, submit_isolated, submit_isolated_with_acks};
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
/// Deep-merge `src` INTO `dst`: recurse where both sides are objects, otherwise
/// `src` overwrites. Used to overlay a guard-satisfying value onto a full
/// schema-valid dummy so the object keeps its other required properties.
///
/// The ordering matters and used to be wrong: planting the guard value first
/// (`{status: "pass"}`) then trying to fill in a full `verifyOut` dummy failed,
/// because the fill step won't clobber an existing key — so the emitted object
/// was `{status: "pass"}`, missing `summary`/`criteria`, and the runtime rejected
/// it with BLACKBOARD_TYPE_ERROR. Seed the dummy first, merge the guard value on
/// top, and only the guarded leaf changes.
fn deep_merge_into(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                deep_merge_into(d.entry(k).or_insert(Value::Null), v);
            }
        }
        (d, s) => *d = s,
    }
}

fn def_wide_plan(resolved: &Value, def_id: &str) -> OutputPlan {
    let def = &resolved["workflows"][def_id];

    // Schema-valid dummies FIRST (capability snippet outputs), guard-satisfying
    // values overlaid LAST — see `deep_merge_into`.
    let mut plan = OutputPlan::new();
    add_capability_outputs(&mut plan, def, resolved);

    // Ensure every transition's output fields are present with a typed dummy.
    //
    // For paths like `$.output.json.deployId` (full_path = "json.deployId") we
    // build a NESTED object so the runtime can resolve the full path correctly:
    //   plan["run_deploy"] = { "json": { "deployId": <typed-dummy> } }
    if let Some(states) = def.get("states").and_then(|s| s.as_object()) {
        for state in states.values() {
            if let Some(ts) = state.get("transitions").and_then(|t| t.as_object()) {
                for (tname, tobj) in ts {
                    for (slot, full_path) in output_field_paths(tobj) {
                        // Prefer the slot's DECLARED OUTPUT schema over its
                        // blackboard `type:`. The blackboard type is the coarse
                        // one — a verify slot is declared `{type: object}` there
                        // but `{$ref: praxec://hop#/$defs/verifyOut}` in
                        // snippet.outputs. A dummy built from the blackboard type
                        // is a bare `{}`: type-correct, contract-invalid. It would
                        // then OVERWRITE the seeded value on the way to terminal
                        // and fail the output contract the runtime enforces there.
                        let val = def
                            .pointer("/snippet/outputs")
                            .or_else(|| def.pointer("/outputs"))
                            .and_then(|o| o.get(&slot))
                            .or_else(|| def.get("blackboard").and_then(|b| b.get(&slot)))
                            .map(crate::analysis::dummy::dummy_for_schema)
                            .unwrap_or_else(|| json!("fuzz"));
                        let parts: Vec<&str> = full_path.split('.').collect();
                        let entry = crate::analysis::plan::output_obj(&mut plan, tname.as_str());
                        insert_nested(entry, &parts, val);
                    }
                }
            }
        }
    }

    // Overlay guard-satisfying values ON TOP of the schema-valid dummies. A guard
    // downstream reads `$.context.verify.status == 'pass'`; the dummy gave `verify`
    // a full `verifyOut` shape, and this replaces just its `.status` leaf with the
    // value the guard needs — leaving `summary`/`criteria`/`provenance` intact.
    for (tname, gval) in derive_plan(def) {
        deep_merge_into(plan.entry(tname).or_insert_with(|| json!({})), gval);
    }

    // Whole-output mappings LAST — a `kind: mcp` leaf's result IS the slot's
    // value, with no field to nest it under, and a bare `{}` is what made
    // `corpus_search`'s array-typed doc_evidence look like a contract violation.
    //
    // ONLY for a transition whose output comes ENTIRELY from the whole result.
    // A transition routinely maps both ways at once —
    //   output: { ready: "$.output.ready", report: "$.output" }
    //   output: { d0_id: "$.output.deliverables.0.id", cohort: "$.output" }
    // — and there the whole output IS the object the fields were planned into.
    // Replacing it with a fresh dummy would erase those fields, breaking the
    // downstream guards they exist to satisfy and dead-ending the chain. Leave
    // the field-planned object alone; it already serves both mappings.
    if let Some(states) = def.get("states").and_then(|s| s.as_object()) {
        for state in states.values() {
            if let Some(ts) = state.get("transitions").and_then(|t| t.as_object()) {
                for (tname, tobj) in ts {
                    if !output_field_paths(tobj).is_empty() {
                        continue;
                    }
                    for slot in whole_output_slots(tobj) {
                        let Some(schema) = def
                            .pointer("/snippet/outputs")
                            .or_else(|| def.pointer("/outputs"))
                            .and_then(|o| o.get(&slot))
                            .or_else(|| def.get("blackboard").and_then(|b| b.get(&slot)))
                        else {
                            continue;
                        };
                        let already_valid = plan.get(tname).is_some_and(|planned| {
                            praxec_core::hop::validate_against_schema(schema, planned, &slot)
                                .is_ok()
                        });
                        if !already_valid {
                            plan.insert(
                                tname.clone(),
                                crate::analysis::dummy::dummy_for_schema(schema),
                            );
                        }
                    }
                }
            }
        }
    }

    plan
}

/// Seed the definition's DECLARED outputs (a capability's `snippet.outputs`, a
/// nestable flow's top-level `outputs:`) into an isolated probe's synthetic
/// context, whenever the probe hasn't already put a CONTRACT-VALID value there.
///
/// The per-transition prober fabricates a context, drops the workflow into ONE
/// state, fires ONE edge, and lets the deterministic chain run wherever it goes
/// — which is frequently straight to a terminal. But it seeds only the slots the
/// probed transition *reads*, and it types those from the `blackboard:` block —
/// which flows often don't declare at all, so the slot lands as the literal
/// `"fuzz"`. Any declared output written by a state the probe skipped is missing
/// outright. The runtime enforces the output contract at terminal (SPEC §5.3)
/// and would rightly fail it, reporting a violation the DEFINITION does not
/// have: an artifact of a fabricated history, not a defect.
///
/// So make the fabricated history plausible — a mid-flow context is one in which
/// the states that "already ran" already wrote contract-valid outputs.
///
/// A value the probe DID choose deliberately (a guard-satisfying literal, e.g.
/// `status == 'pass'`) is kept — but only if it satisfies the declared schema.
/// If it doesn't, it can only be the untyped `"fuzz"` fallback, and keeping it
/// would just re-create the false failure. Validity is judged with the same
/// registry-aware compiler the runtime enforces the contract with, so this can't
/// drift from the check it exists to keep honest.
fn seed_declared_outputs(ctx: &mut Value, def: &Value) {
    let Some(schemas) = def
        .pointer("/snippet/outputs")
        .or_else(|| def.pointer("/outputs"))
        .and_then(Value::as_object)
    else {
        return;
    };
    let Some(map) = ctx.as_object_mut() else {
        return;
    };
    for (name, schema) in schemas {
        let keep = map
            .get(name)
            .is_some_and(|v| praxec_core::hop::validate_against_schema(schema, v, name).is_ok());
        if !keep {
            map.insert(
                name.clone(),
                crate::analysis::dummy::dummy_for_schema(schema),
            );
        }
    }
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
///
/// Delegates to `dummy_for_schema`, which resolves a HOP `$ref` and emits every
/// REQUIRED property (recursively). A `hop_slot` transition carries
/// `inputSchema: {$ref: "praxec://hop#/$defs/verifyIn"}` — a bare `$ref` with no
/// `properties` — so the old properties-only walk produced `{}` and the submit
/// failed verifyIn's `required` with INPUT_SCHEMA_VIOLATION. Same for any deeply
/// nested required schema (e.g. `cap.plan.build-graph`'s
/// `graph.deliverables[].{id,…}`): `dummy_for_schema` recurses through `required`.
fn arguments_for(transition: &serde_json::Value) -> serde_json::Value {
    match transition.pointer("/inputSchema") {
        Some(schema) if !schema.is_null() => crate::analysis::dummy::dummy_arguments(schema),
        _ => serde_json::json!({}),
    }
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

    // The context an isolated probe STARTS from: every slot the definition could
    // read, seeded once. Without it, a chain that runs past the probed edge dies
    // on GUARD_UNSET_SLOT the moment it meets a guard on a slot the probe never
    // set — a defect of the fabricated history, not of the definition. Whether a
    // slot is genuinely written on every path to its reader is a PATH question,
    // which a single-state probe cannot answer and static analysis can.
    let def_context = definition_wide_context(def, &blackboard);

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

        let mut sat_ctx = satisfying_context_over(tobj, &blackboard, Some(&def_context));
        seed_declared_outputs(&mut sat_ctx, def);
        // Per-edge input: satisfy THIS edge's `$.workflow.input.*` / `$.input.*`
        // guards. Sibling edges gate the same input slot on exclusive values, so
        // this can't be shared across the definition.
        let mut edge_input = workflow_input.clone();
        seed_input_guards(&mut edge_input, tobj);
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
                edge_input.clone(),
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
                edge_input.clone(),
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
        let (viol_ok, viol_detail) = if let Some(mut viol_ctx) =
            violating_context_over(tobj, &blackboard, Some(&def_context))
        {
            seed_declared_outputs(&mut viol_ctx, def);
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
                edge_input.clone(),
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
