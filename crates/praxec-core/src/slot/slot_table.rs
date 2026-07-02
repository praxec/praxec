//! SPEC §7 — per-flow slot table.
//!
//! The host flow's `$.context.*` slots are typed. Building this
//! table is what powers V13 (every `use:.inputs` RHS path must resolve
//! to a slot that's either declared in `inputs:` or written by some
//! state's `use:.outputs`) and V14 (two states writing the same host
//! path must declare structurally identical output types).
//!
//! Construction is **flat** — no topological walk, no inference. Spec
//! §7.4 explicitly says state-graph cycles do not participate in type
//! inference; a slot's type is decided at its declared write site.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::validate::Diagnostic;

/// Where a slot's type came from. Drives error messages so an operator
/// can navigate straight to the offending declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotSource {
    /// Declared on the flow's top-level `inputs:` block.
    Input,
    /// Written by a state's transition via `use:.outputs`. The string
    /// is the state name. (Use it in error messages to point at the
    /// declaration site.)
    State(String),
}

/// A single entry in the slot table.
#[derive(Debug, Clone)]
pub struct SlotEntry {
    pub schema: Value,
    pub source: SlotSource,
}

/// The per-flow slot table. Keyed by host path
/// (`$.context.<name>`); each entry carries the declared schema and the
/// declaration site.
///
/// **Storage:** [`BTreeMap`] so iteration order is deterministic for
/// stable error-message output (helps test snapshots stay reproducible).
#[derive(Debug, Clone, Default)]
pub struct SlotTable {
    entries: BTreeMap<String, SlotEntry>,
}

impl SlotTable {
    /// Returns `Some(entry)` iff `host_path` (e.g. `$.context.verdict`)
    /// is declared. Used by V13's Check A.
    pub fn get(&self, host_path: &str) -> Option<&SlotEntry> {
        self.entries.get(host_path)
    }

    /// Number of declared slots.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Useful for snapshot tests.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate slot entries in deterministic (lexicographic by host path)
    /// order. Surfaces stable error-message output.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &SlotEntry)> {
        self.entries.iter()
    }
}

/// SPEC §7.2 — build the slot table for one flow.
///
/// - Seeds entries from the flow's top-level `inputs:` block,
///   keyed `$.context.<input_name>`.
/// - Walks every state. For each transition whose executor has a
///   `use:.outputs` block, contributes one entry per `host_path → cap_output`
///   pair. The schema is harvested from the target capability's
///   `snippet.outputs[cap_output]` via `cap_snippet_outputs`.
///
/// Returns the table on success, or a [`Vec<Diagnostic>`] of V14 (type
/// consistency) errors when two states write incompatible types to the
/// same host slot. V13 (reachability) is checked separately via
/// [`assert_reachable`] — that has different "caller knows the host_path"
/// semantics, so we keep the checks composable.
pub fn build_slot_table(
    flow_def: &Value,
    cap_snippet_outputs: &HashMap<String, Value>,
) -> Result<SlotTable, Vec<Diagnostic>> {
    let mut table = SlotTable::default();
    let mut errors: Vec<Diagnostic> = Vec::new();

    // Seed from inputs:.
    if let Some(inputs) = flow_def.pointer("/inputs").and_then(Value::as_object) {
        for (name, schema) in inputs {
            let host_path = format!("$.context.{name}");
            table.entries.insert(
                host_path,
                SlotEntry {
                    schema: schema.clone(),
                    source: SlotSource::Input,
                },
            );
        }
    }

    // Walk states → transitions → executor.use.outputs.
    if let Some(states) = flow_def.pointer("/states").and_then(Value::as_object) {
        for (state_name, state_def) in states {
            let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object)
            else {
                continue;
            };
            for (_t_name, t_def) in transitions {
                let Some(exec) = t_def.pointer("/executor").and_then(Value::as_object) else {
                    continue;
                };
                if exec.get("kind").and_then(Value::as_str) != Some("workflow") {
                    continue;
                }
                let Some(use_outputs) = exec
                    .get("use")
                    .and_then(|u| u.get("outputs"))
                    .and_then(Value::as_object)
                else {
                    continue;
                };
                let target_id = exec
                    .get("definitionId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let snippet = cap_snippet_outputs.get(target_id);
                for (host_path, cap_name_value) in use_outputs {
                    let Some(cap_name) = cap_name_value.as_str() else {
                        continue;
                    };
                    // Distinguish "this cap declares no such output" from a
                    // schema that is literally `null`. When the target cap's
                    // snippet outputs are known (Some) but don't carry
                    // `cap_name`, the `use:.outputs` RHS is a typo / dangling
                    // reference — emit UNKNOWN_CAP_OUTPUT rather than coercing
                    // to Value::Null (which would silently pass V13/V14).
                    let schema = match snippet {
                        Some(s) => match s.get(cap_name) {
                            Some(v) => v.clone(),
                            None => {
                                errors.push(Diagnostic::Error(format!(
                                    "UNKNOWN_CAP_OUTPUT: state '{state_name}' maps \
                                     '{host_path}' from output '{cap_name}' of capability \
                                     '{target_id}', but that capability's snippet declares no \
                                     such output (SPEC §7.2)"
                                )));
                                continue;
                            }
                        },
                        // FIX SLOT: the target's snippet outputs are unknown to
                        // the validator — the `definitionId` resolves to no
                        // snippet-bearing capability in this config (a typo'd /
                        // dangling target, or a cap that declares no outputs to
                        // map from). Previously this silently coerced to
                        // `Value::Null`, inserting an untyped slot that passes
                        // V13/V14 unconditionally — the mirror-image of the
                        // UNKNOWN_CAP_OUTPUT defect on the sibling arm. Emit a
                        // diagnostic instead of fabricating a null-typed slot.
                        None => {
                            errors.push(Diagnostic::Error(format!(
                                "UNKNOWN_CAP_OUTPUT_TARGET: state '{state_name}' maps \
                                 '{host_path}' from output '{cap_name}' of capability \
                                 '{target_id}', but no snippet-bearing capability with that \
                                 `definitionId` is loaded — cannot type the slot (SPEC §7.2)"
                            )));
                            continue;
                        }
                    };
                    insert_with_v14_check(
                        &mut table,
                        &mut errors,
                        host_path.clone(),
                        SlotEntry {
                            schema,
                            source: SlotSource::State(state_name.clone()),
                        },
                    );
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(table)
    } else {
        Err(errors)
    }
}

/// Insert into the slot table; on a host-path collision, structurally
/// compare schemas. Equal → keep the existing entry (first writer wins
/// on declaration order, deterministic per [`BTreeMap`] iteration).
/// Different → push a V14 diagnostic naming both states.
fn insert_with_v14_check(
    table: &mut SlotTable,
    errors: &mut Vec<Diagnostic>,
    host_path: String,
    new_entry: SlotEntry,
) {
    if let Some(existing) = table.entries.get(&host_path) {
        if schemas_equal(&existing.schema, &new_entry.schema) {
            return;
        }
        errors.push(Diagnostic::Error(format!(
            "SLOT_TYPE_CONFLICT: '{host_path}' is written by {} and {} with structurally \
             different schemas (SPEC §7.3, V14)",
            describe_source(&existing.source),
            describe_source(&new_entry.source)
        )));
        return;
    }
    table.entries.insert(host_path, new_entry);
}

/// Structural equality on canonical JSON. Reuses
/// [`crate::contract_hash::canonical_json_string`] to compare
/// sorted-key-canonicalized output — same algorithm operators see when
/// pinning a contract hash, so the equality intuition stays consistent.
fn schemas_equal(a: &Value, b: &Value) -> bool {
    use crate::contract_hash::canonical_json_string;
    canonical_json_string(a) == canonical_json_string(b)
}

fn describe_source(s: &SlotSource) -> String {
    match s {
        SlotSource::Input => "the flow's `inputs:` block".to_string(),
        SlotSource::State(name) => format!("state '{name}'"),
    }
}

/// SPEC §7.3, V13 (Check A) — a single reachability check. Caller passes
/// the slot table and a `use:.inputs` host path; we emit a structured
/// diagnostic if it's not in the table.
pub fn assert_reachable(
    table: &SlotTable,
    host_path: &str,
    flow_id: &str,
    state_name: &str,
    transition_name: &str,
) -> Option<Diagnostic> {
    if table.entries.contains_key(host_path) {
        return None;
    }
    // A `use:.inputs` may read a SUB-FIELD of a declared object slot —
    // `$.context.<slot>.<subpath>` is reachable when `$.context.<slot>` is
    // declared (e.g. passing a child's `schedule.plan_id` onward). The parent
    // slot's writer establishes reachability; the sub-path resolves at runtime.
    if let Some(rest) = host_path.strip_prefix("$.context.") {
        if let Some((slot, _subpath)) = rest.split_once('.') {
            if table.entries.contains_key(&format!("$.context.{slot}")) {
                return None;
            }
        }
    }
    Some(Diagnostic::Error(format!(
        "UNREACHABLE_SLOT: flow '{flow_id}' state '{state_name}' \
         transition '{transition_name}' references '{host_path}' via `use:.inputs`, \
         but no state writes that slot and it is not declared in `inputs:` \
         (SPEC §7.3, V13)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cap_outputs() -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert(
            "cap.plan.vet".to_string(),
            json!({ "verdict": { "type": "string", "enum": ["pass", "fail"] } }),
        );
        m
    }

    #[test]
    fn build_seeds_from_inputs_block() {
        let orch = json!({
            "inputs": {
                "feature_brief": { "type": "string" }
            },
            "states": {}
        });
        let t = build_slot_table(&orch, &HashMap::new()).expect("no errors");
        let entry = t.get("$.context.feature_brief").expect("present");
        assert!(matches!(entry.source, SlotSource::Input));
    }

    #[test]
    fn build_harvests_use_outputs_from_states() {
        let orch = json!({
            "states": {
                "vetting": {
                    "transitions": {
                        "vet": {
                            "executor": {
                                "kind": "workflow",
                                "definitionId": "cap.plan.vet",
                                "use": {
                                    "outputs": { "$.context.verdict": "verdict" }
                                }
                            }
                        }
                    }
                }
            }
        });
        let caps = cap_outputs();
        let t = build_slot_table(&orch, &caps).expect("no errors");
        let entry = t.get("$.context.verdict").expect("present");
        assert!(matches!(&entry.source, SlotSource::State(s) if s == "vetting"));
        assert_eq!(
            entry.schema.pointer("/type").and_then(Value::as_str),
            Some("string")
        );
    }

    #[test]
    fn build_flags_v14_when_two_states_write_incompatible_types() {
        let mut caps = HashMap::new();
        caps.insert("cap.a".to_string(), json!({ "v": { "type": "string" } }));
        caps.insert("cap.b".to_string(), json!({ "v": { "type": "integer" } }));
        let orch = json!({
            "states": {
                "s1": {
                    "transitions": { "t1": { "executor": {
                        "kind": "workflow",
                        "definitionId": "cap.a",
                        "use": { "outputs": { "$.context.x": "v" } }
                    } } }
                },
                "s2": {
                    "transitions": { "t1": { "executor": {
                        "kind": "workflow",
                        "definitionId": "cap.b",
                        "use": { "outputs": { "$.context.x": "v" } }
                    } } }
                }
            }
        });
        let err = build_slot_table(&orch, &caps).expect_err("V14 should fire");
        assert!(
            err.iter()
                .any(|d| d.message().contains("SLOT_TYPE_CONFLICT")),
            "{err:?}"
        );
    }

    #[test]
    fn build_flags_unknown_cap_output() {
        // `use:.outputs` names `nonexistent`, which the target cap's snippet
        // does not declare → UNKNOWN_CAP_OUTPUT (CMP-025), not a silent null.
        let orch = json!({
            "states": {
                "vetting": {
                    "transitions": {
                        "vet": {
                            "executor": {
                                "kind": "workflow",
                                "definitionId": "cap.plan.vet",
                                "use": {
                                    "outputs": { "$.context.verdict": "nonexistent" }
                                }
                            }
                        }
                    }
                }
            }
        });
        let caps = cap_outputs();
        let err = build_slot_table(&orch, &caps).expect_err("UNKNOWN_CAP_OUTPUT should fire");
        assert!(
            err.iter()
                .any(|d| d.message().contains("UNKNOWN_CAP_OUTPUT")
                    && d.message().contains("nonexistent")),
            "{err:?}"
        );
    }

    #[test]
    fn build_flags_unknown_cap_output_target() {
        // FIX SLOT: `definitionId` resolves to no snippet-bearing cap in the
        // provided map. Previously this silently coerced the slot schema to
        // Value::Null (passing V13/V14 unconditionally). Now it must emit a
        // diagnostic, mirroring the UNKNOWN_CAP_OUTPUT sibling arm.
        let orch = json!({
            "states": {
                "vetting": {
                    "transitions": {
                        "vet": {
                            "executor": {
                                "kind": "workflow",
                                "definitionId": "cap.does.not.exist",
                                "use": {
                                    "outputs": { "$.context.verdict": "verdict" }
                                }
                            }
                        }
                    }
                }
            }
        });
        // Empty map → target is unknown.
        let err = build_slot_table(&orch, &HashMap::new())
            .expect_err("UNKNOWN_CAP_OUTPUT_TARGET should fire");
        assert!(
            err.iter()
                .any(|d| d.message().contains("UNKNOWN_CAP_OUTPUT_TARGET")
                    && d.message().contains("cap.does.not.exist")),
            "{err:?}"
        );
    }

    #[test]
    fn build_allows_null_schema_when_output_declared() {
        // A literally-null schema for a declared output is NOT an error —
        // only an *absent* output name is (CMP-025 distinction).
        let mut caps = HashMap::new();
        caps.insert("cap.x".to_string(), json!({ "out": null }));
        let orch = json!({
            "states": {
                "s": {
                    "transitions": {
                        "t": {
                            "executor": {
                                "kind": "workflow",
                                "definitionId": "cap.x",
                                "use": { "outputs": { "$.context.v": "out" } }
                            }
                        }
                    }
                }
            }
        });
        let t = build_slot_table(&orch, &caps).expect("null schema for declared output is ok");
        assert!(t.get("$.context.v").is_some());
    }

    #[test]
    fn assert_reachable_returns_none_for_declared_slot() {
        let orch = json!({
            "inputs": { "x": { "type": "string" } },
            "states": {}
        });
        let t = build_slot_table(&orch, &HashMap::new()).unwrap();
        assert!(assert_reachable(&t, "$.context.x", "flow", "s", "t").is_none());
    }

    #[test]
    fn assert_reachable_returns_diagnostic_for_undeclared_slot() {
        let t = SlotTable::default();
        let d = assert_reachable(&t, "$.context.missing", "flow", "s", "t").expect("must emit");
        assert!(d.message().contains("UNREACHABLE_SLOT"));
        assert!(d.message().contains("$.context.missing"));
    }

    #[test]
    fn assert_reachable_allows_subfield_of_a_declared_object_slot() {
        let orch = json!({
            "inputs": { "schedule": { "type": "object" } },
            "states": {}
        });
        let t = build_slot_table(&orch, &HashMap::new()).unwrap();
        // Reading a sub-field of a declared slot is reachable (the parent's
        // writer establishes it; the sub-path resolves at runtime).
        assert!(assert_reachable(&t, "$.context.schedule.plan_id", "flow", "s", "t").is_none());
    }

    #[test]
    fn assert_reachable_still_rejects_subfield_of_an_undeclared_slot() {
        let t = SlotTable::default();
        let d =
            assert_reachable(&t, "$.context.nope.plan_id", "flow", "s", "t").expect("must emit");
        assert!(d.message().contains("UNREACHABLE_SLOT"));
    }
}
