//! Mutation operators for workflow configs.
//!
//! Each operator takes a **resolved** config `&serde_json::Value` and produces a
//! `Vec<(label, mutant)>` — one mutant per applicable injection site. The label
//! is human-readable and uniquely identifies the site (used in reports).
//!
//! Fourteen operators are provided:
//!
//! | Operator                  | Kind       | Expected kill? | Detector           |
//! |---------------------------|------------|----------------|--------------------|
//! | orphan_state              | Structural | KILLED         | reachability       |
//! | dangle_target             | Structural | KILLED         | validate           |
//! | deadend                   | Structural | KILLED         | validate           |
//! | det_cycle                 | Structural | KILLED         | loop detection     |
//! | literal_type_break        | Type       | KILLED         | runtime type check |
//! | drop_output_write         | Contract   | KILLED         | V24 (must-write)   |
//! | drop_initial_context_seed | Contract   | KILLED         | V24 (must-write)   |
//! | weaken_output_source      | Contract   | KILLED         | V26 (scalar-null)  |
//! | retarget_guard_scope      | Scope      | KILLED         | V25 (guard scope)  |
//! | retarget_output_scope     | Scope      | KILLED         | V27 (write scope)  |
//! | retarget_use_input_scope  | Scope      | KILLED         | V28 (use-input)    |
//! | retarget_executor_arg_scope | Scope    | KILLED         | V29 (executor arg) |
//! | delete_guard              | Semantic   | KILLED         | violating-ctx probe|
//! | flip_guard_op             | Semantic   | KILLED         | violating-ctx probe|
//!
//! The harness collects per-operator kill rates, which is how the tool's actual
//! guarantees stay distinguishable from its assumed ones.
//!
//! The two SEMANTIC operators were originally documented as blind spots — "the
//! tool cannot know correct guard intent". That is no longer true, and the
//! measurement is what said so: the per-transition fuzz submits a deliberately
//! VIOLATING context to every edge and asserts the guard rejects it, so deleting
//! a guard (or flipping its operator) makes that probe fire and is caught. The
//! prose was just older than the harness.
//!
//! ## Why the CONTRACT operators exist
//!
//! They were added after a defect class shipped that no operator modeled: a
//! definition reaching a terminal owing a declared output it never wrote. The
//! caller binds the slot and reads `null` — or, for an `array`/`object` slot, the
//! deterministic-repair rung hands it `[]`/`{}` and the run goes GREEN with an
//! empty result. Nothing in the harness generated that mutant, so nothing
//! measured that the tool was blind to it.
//!
//! That is the whole argument for mutation testing here: a gate you never attack
//! is a gate you are only ASSUMING works.
//!
//! ## Implementation note — workflow IDs with slashes
//!
//! The resolved config from a repo-backed fixture uses namespace-prefixed IDs
//! like `cognitive/cap.coordinate.label-and-route`. JSON Pointer uses `/` as a
//! path separator, so `pointer_mut("/workflows/cognitive/cap...")` silently
//! mis-navigates. All mutations use direct `Map::get_mut` calls on the
//! `workflows` object to avoid this.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Deep-clone the config and apply `f` to the mutable clone.
fn mutate<F: FnOnce(&mut Value)>(base: &Value, f: F) -> Value {
    let mut m = base.clone();
    f(&mut m);
    m
}

/// Get mutable reference to `config["workflows"][wf_id]["states"][state_name]`.
/// Uses direct map key access (safe for IDs containing `/`).
fn get_state_mut<'a>(
    workflows: &'a mut Map<String, Value>,
    wf_id: &str,
    state_name: &str,
) -> Option<&'a mut Map<String, Value>> {
    workflows
        .get_mut(wf_id)?
        .as_object_mut()?
        .get_mut("states")?
        .as_object_mut()?
        .get_mut(state_name)?
        .as_object_mut()
}

/// Get mutable reference to a specific transition object within a workflow.
fn get_transition_mut<'a>(
    workflows: &'a mut Map<String, Value>,
    wf_id: &str,
    state_name: &str,
    t_name: &str,
) -> Option<&'a mut Map<String, Value>> {
    get_state_mut(workflows, wf_id, state_name)?
        .get_mut("transitions")?
        .as_object_mut()?
        .get_mut(t_name)?
        .as_object_mut()
}

/// Iterate over all `(wf_id, state_name, t_name, t_def)` transition quads.
fn each_transition(config: &Value) -> Vec<(String, String, String, &Value)> {
    let mut out = Vec::new();
    let Some(wfs) = config.pointer("/workflows").and_then(Value::as_object) else {
        return out;
    };
    for (wf_id, wf_def) in wfs {
        let Some(states) = wf_def.pointer("/states").and_then(Value::as_object) else {
            continue;
        };
        for (state_name, state_def) in states {
            let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object)
            else {
                continue;
            };
            for (t_name, t_def) in transitions {
                out.push((wf_id.clone(), state_name.clone(), t_name.clone(), t_def));
            }
        }
    }
    out
}

// ── Mutation operators ────────────────────────────────────────────────────────

/// STRUCTURAL — for each non-initial state, remove ALL incoming transitions
/// targeting it, making it an orphan (unreachable from `initialState`).
///
/// Implementation: for each candidate orphan state `S` (non-initial),
/// build a mutant where every transition that targets `S` is removed,
/// effectively severing all inbound edges to `S`.
pub fn orphan_state(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();
    let Some(wfs) = config.pointer("/workflows").and_then(Value::as_object) else {
        return mutants;
    };

    for (wf_id, wf_def) in wfs {
        let initial = match wf_def.get("initialState").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let Some(states) = wf_def.pointer("/states").and_then(Value::as_object) else {
            continue;
        };

        // Candidates: every non-initial state that has at least one inbound edge
        let candidates: Vec<String> = states
            .keys()
            .filter(|s| *s != &initial)
            .filter(|target_state| {
                states.iter().any(|(_sn, sd)| {
                    sd.pointer("/transitions")
                        .and_then(Value::as_object)
                        .map(|ts| {
                            ts.values().any(|td| {
                                td.get("target").and_then(Value::as_str)
                                    == Some(target_state.as_str())
                            })
                        })
                        .unwrap_or(false)
                })
            })
            .cloned()
            .collect();

        for target_state in candidates {
            let label = format!("orphan_state/{wf_id}/{target_state}");
            let wf_id_clone = wf_id.clone();
            let ts_clone = target_state.clone();
            let mutant = mutate(config, |v| {
                let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut) else {
                    return;
                };
                let Some(wf) = wfs_mut.get_mut(&wf_id_clone).and_then(Value::as_object_mut) else {
                    return;
                };
                let Some(sts) = wf.get_mut("states").and_then(Value::as_object_mut) else {
                    return;
                };
                let state_names: Vec<String> = sts.keys().cloned().collect();
                for sn in &state_names {
                    if sn == &ts_clone {
                        continue; // don't touch the orphan's own transitions
                    }
                    let Some(st) = sts.get_mut(sn).and_then(Value::as_object_mut) else {
                        continue;
                    };
                    let Some(ts) = st.get_mut("transitions").and_then(Value::as_object_mut) else {
                        continue;
                    };
                    // Remove transitions that point at the target state
                    let to_remove: Vec<String> = ts
                        .iter()
                        .filter(|(_tn, td)| {
                            td.get("target").and_then(Value::as_str) == Some(ts_clone.as_str())
                        })
                        .map(|(tn, _)| tn.clone())
                        .collect();
                    for tn in to_remove {
                        ts.remove(&tn);
                    }
                }
            });
            mutants.push((label, mutant));
        }
    }
    mutants
}

/// STRUCTURAL — for each transition, replace `target` with a nonexistent state name.
/// The validator detects dangling targets at load/check time.
pub fn dangle_target(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();
    let transitions = each_transition(config);
    const GHOST: &str = "__mutant_nonexistent_state__";

    for (wf_id, state_name, t_name, _t_def) in transitions {
        let label = format!("dangle_target/{wf_id}/{state_name}/{t_name}");
        let wf_id_c = wf_id.clone();
        let sn_c = state_name.clone();
        let tn_c = t_name.clone();
        let mutant = mutate(config, |v| {
            let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut) else {
                return;
            };
            if let Some(td) = get_transition_mut(wfs_mut, &wf_id_c, &sn_c, &tn_c) {
                td.insert("target".into(), Value::String(GHOST.to_string()));
            }
        });
        mutants.push((label, mutant));
    }
    mutants
}

/// STRUCTURAL — for each non-terminal, non-initial state that has outgoing
/// transitions, replace its `transitions` with `{}` (leaving it non-terminal).
/// The state becomes a dead-end: reachable but with no way out.
pub fn deadend(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();
    let Some(wfs) = config.pointer("/workflows").and_then(Value::as_object) else {
        return mutants;
    };

    for (wf_id, wf_def) in wfs {
        let initial = wf_def
            .get("initialState")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(states) = wf_def.pointer("/states").and_then(Value::as_object) else {
            continue;
        };

        // BFS reachability so we only mutate reachable, non-initial states
        let mut reachable = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        if !initial.is_empty() {
            queue.push_back(initial.clone());
            reachable.insert(initial.clone());
        }
        while let Some(cur) = queue.pop_front() {
            let Some(sd) = states.get(&cur) else { continue };
            let Some(ts) = sd.pointer("/transitions").and_then(Value::as_object) else {
                continue;
            };
            for td in ts.values() {
                if let Some(tgt) = td.get("target").and_then(Value::as_str) {
                    if reachable.insert(tgt.to_string()) {
                        queue.push_back(tgt.to_string());
                    }
                }
            }
        }

        for (state_name, state_def) in states {
            if state_name == &initial {
                continue;
            }
            let is_terminal = state_def
                .get("terminal")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_terminal {
                continue;
            }
            let has_transitions = state_def
                .pointer("/transitions")
                .and_then(Value::as_object)
                .map(|t| !t.is_empty())
                .unwrap_or(false);
            if !has_transitions {
                continue; // already a dead-end, not a useful mutation site
            }
            if !reachable.contains(state_name) {
                continue; // unreachable — no point
            }

            let label = format!("deadend/{wf_id}/{state_name}");
            let wf_id_c = wf_id.clone();
            let sn_c = state_name.clone();
            let mutant = mutate(config, |v| {
                let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut) else {
                    return;
                };
                if let Some(st) = get_state_mut(wfs_mut, &wf_id_c, &sn_c) {
                    st.insert("transitions".into(), Value::Object(Map::new()));
                    st.remove("terminal");
                }
            });
            mutants.push((label, mutant));
        }
    }
    mutants
}

/// STRUCTURAL — for each state `A` that has a deterministic transition `A→B`
/// where `B` has NO other incoming edges (i.e., `A→B` is `B`'s sole inbound
/// edge), redirect `A→B` to `A→A` (self-loop).
///
/// This mutation guarantees detection via TWO mechanisms:
/// 1. `B` becomes an orphan (its only inbound edge was severed) → orphan detection.
/// 2. If A has only deterministic transitions and no terminal reachable via them,
///    `deterministic_loops()` also flags it → loop detection.
///
/// Restricting to states where the original target has a single inbound edge
/// ensures orphan detection always fires, giving a 100% guaranteed kill.
pub fn det_cycle(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();
    let Some(wfs) = config.pointer("/workflows").and_then(Value::as_object) else {
        return mutants;
    };

    for (wf_id, wf_def) in wfs {
        let Some(states) = wf_def.pointer("/states").and_then(Value::as_object) else {
            continue;
        };

        // Count inbound edges per state: how many transitions target each state.
        let mut inbound_count: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for state_def in states.values() {
            let Some(ts) = state_def.pointer("/transitions").and_then(Value::as_object) else {
                continue;
            };
            for t_def in ts.values() {
                if let Some(tgt) = t_def.get("target").and_then(Value::as_str) {
                    *inbound_count.entry(tgt).or_insert(0) += 1;
                }
            }
        }

        let initial = wf_def
            .get("initialState")
            .and_then(Value::as_str)
            .unwrap_or("");

        for (state_name, state_def) in states {
            let is_terminal = state_def
                .get("terminal")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_terminal {
                continue;
            }
            let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object)
            else {
                continue;
            };
            if transitions.is_empty() {
                continue;
            }

            // All transitions must be deterministic
            let all_det = transitions.values().all(|td| {
                td.get("actor").and_then(Value::as_str).unwrap_or("agent") == "deterministic"
            });
            if !all_det {
                continue;
            }

            // Find a transition whose target has exactly ONE inbound edge (this one).
            // Redirecting it to self orphans that target state.
            let candidate = transitions.iter().find_map(|(tn, td)| {
                let target = td.get("target").and_then(Value::as_str)?;
                // Skip: target == initial (initial state can't be an orphan),
                //       target == state_name (already a self-loop, useless),
                //       target has more than 1 inbound (won't become orphan)
                if target == initial || target == state_name.as_str() {
                    return None;
                }
                let inbound = inbound_count.get(target).copied().unwrap_or(0);
                if inbound == 1 {
                    Some((tn.clone(), target.to_string()))
                } else {
                    None
                }
            });
            let Some((t_name, old_target)) = candidate else {
                continue;
            };

            let label = format!("det_cycle/{wf_id}/{state_name}/{t_name}→self(was:{old_target})");
            let wf_id_c = wf_id.clone();
            let sn_c = state_name.clone();
            let tn_c = t_name.clone();
            let mutant = mutate(config, |v| {
                let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut) else {
                    return;
                };
                if let Some(td) = get_transition_mut(wfs_mut, &wf_id_c, &sn_c, &tn_c) {
                    td.insert("target".into(), Value::String(sn_c.clone()));
                }
            });
            mutants.push((label, mutant));
        }
    }
    mutants
}

/// TYPE — for each transition, inject a wrong-typed literal output for a typed
/// blackboard slot. Catches literal-type mismatches at runtime.
pub fn literal_type_break(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();
    let Some(wfs) = config.pointer("/workflows").and_then(Value::as_object) else {
        return mutants;
    };

    for (wf_id, wf_def) in wfs {
        let Some(blackboard) = wf_def.pointer("/blackboard").and_then(Value::as_object) else {
            continue;
        };
        let typed_slots: Vec<(String, String)> = blackboard
            .iter()
            .filter_map(|(slot, schema)| {
                schema
                    .get("type")
                    .and_then(Value::as_str)
                    .map(|t| (slot.clone(), t.to_string()))
            })
            .collect();
        if typed_slots.is_empty() {
            continue;
        }

        let Some(states) = wf_def.pointer("/states").and_then(Value::as_object) else {
            continue;
        };

        for (state_name, state_def) in states {
            let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object)
            else {
                continue;
            };

            for (t_name, t_def) in transitions {
                // Find a typed slot this transition already writes, or use the first
                let (slot, slot_type) = {
                    let written = t_def
                        .pointer("/output")
                        .and_then(Value::as_object)
                        .and_then(|output| {
                            typed_slots.iter().find_map(|(s, t)| {
                                if output.contains_key(s) {
                                    Some((s.clone(), t.clone()))
                                } else {
                                    None
                                }
                            })
                        });
                    match written {
                        Some(p) => p,
                        None => typed_slots[0].clone(),
                    }
                };

                let wrong_val = wrong_typed_literal(&slot_type);
                let label = format!("literal_type_break/{wf_id}/{state_name}/{t_name}/{slot}");
                let wf_id_c = wf_id.clone();
                let sn_c = state_name.clone();
                let tn_c = t_name.clone();
                let slot_c = slot.clone();
                let mutant = mutate(config, |v| {
                    let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut)
                    else {
                        return;
                    };
                    if let Some(td) = get_transition_mut(wfs_mut, &wf_id_c, &sn_c, &tn_c) {
                        let output = td
                            .entry("output")
                            .or_insert_with(|| Value::Object(Map::new()))
                            .as_object_mut()
                            .expect("output is an object");
                        output.insert(slot_c.clone(), wrong_val.clone());
                    }
                });
                mutants.push((label, mutant));
            }
        }
    }
    mutants
}

/// Return a literal value of the WRONG type for the given declared type.
fn wrong_typed_literal(declared_type: &str) -> Value {
    match declared_type {
        "string" => json!(true),                // bool into string slot
        "boolean" => json!(42),                 // number into bool slot
        "number" | "integer" => json!("wrong"), // string into number slot
        "array" => json!(false),                // bool into array slot
        "object" => json!("wrong"),             // string into object slot
        _ => json!(true),
    }
}

/// CONTRACT — for each transition that writes a DECLARED output (a capability's
/// `snippet.outputs`, a nestable flow's top-level `outputs:`), delete that one
/// write.
///
/// This is the operator the harness was missing, and its absence is why nothing
/// warned us about the defect class that shipped: a definition that reaches a
/// terminal owing an output it never wrote. A caller binding the slot reads
/// `null` — or worse, for an `array`/`object` slot, the deterministic-repair rung
/// hands it `[]`/`{}` and the run goes GREEN with an empty result.
///
/// Killed by V24 (`UNWRITTEN_DECLARED_OUTPUT`) at load time. Before V24 existed,
/// the scalar cases were killed at runtime by the terminal check and the
/// array/object cases SURVIVED — silently. The kill rate on this operator is the
/// number that says whether that hole is still open.
pub fn drop_output_write(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();
    let Some(wfs) = config.pointer("/workflows").and_then(Value::as_object) else {
        return mutants;
    };

    for (wf_id, wf_def) in wfs {
        let Some(declared) = wf_def
            .pointer("/snippet/outputs")
            .or_else(|| wf_def.pointer("/outputs"))
            .and_then(Value::as_object)
            .filter(|o| !o.is_empty())
        else {
            continue;
        };
        let Some(states) = wf_def.pointer("/states").and_then(Value::as_object) else {
            continue;
        };

        for (state_name, state_def) in states {
            let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object)
            else {
                continue;
            };
            for (t_name, t_def) in transitions {
                let Some(output) = t_def.pointer("/output").and_then(Value::as_object) else {
                    continue;
                };
                for slot in output.keys().filter(|k| declared.contains_key(*k)) {
                    let label = format!("drop_output_write/{wf_id}/{state_name}/{t_name}/{slot}");
                    let wf_id_c = wf_id.clone();
                    let sn_c = state_name.clone();
                    let tn_c = t_name.clone();
                    let slot_c = slot.clone();
                    let mutant = mutate(config, |v| {
                        let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut)
                        else {
                            return;
                        };
                        if let Some(td) = get_transition_mut(wfs_mut, &wf_id_c, &sn_c, &tn_c)
                            && let Some(o) = td.get_mut("output").and_then(Value::as_object_mut)
                        {
                            o.remove(&slot_c);
                        }
                    });
                    mutants.push((label, mutant));
                }
            }
        }
    }
    mutants
}

/// CONTRACT — remove the `required`/`default` that keeps a scalar output's source
/// argument non-null.
///
/// The operator that proves V26. A declared scalar output sourced from an agent
/// argument is safe only while that argument is `required` or has a `default`.
/// Strip that safety and the output can land null at terminal — the exact
/// "written-but-nullable" class V26 warns on. It must be caught; before V26 it
/// survived silently, which is the whole reason to model it.
pub fn weaken_output_source(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();
    let Some(wfs) = config.pointer("/workflows").and_then(Value::as_object) else {
        return mutants;
    };

    for (wf_id, wf_def) in wfs {
        // Declared SCALAR outputs of this workflow.
        let Some(declared) = wf_def
            .pointer("/snippet/outputs")
            .or_else(|| wf_def.pointer("/outputs"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        let scalar: HashSet<&str> = declared
            .iter()
            .filter(|(_, s)| {
                matches!(
                    s.get("type").and_then(Value::as_str),
                    Some("string" | "integer" | "number" | "boolean")
                )
            })
            .map(|(n, _)| n.as_str())
            .collect();
        if scalar.is_empty() {
            continue;
        }
        let Some(states) = wf_def.pointer("/states").and_then(Value::as_object) else {
            continue;
        };

        for (state_name, state_def) in states {
            let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object)
            else {
                continue;
            };
            for (t_name, t_def) in transitions {
                let Some(output) = t_def.pointer("/output").and_then(Value::as_object) else {
                    continue;
                };
                for (out_slot, src) in output {
                    if !scalar.contains(out_slot.as_str()) {
                        continue;
                    }
                    let Some(field) = src
                        .as_str()
                        .and_then(|s| s.strip_prefix("$.arguments."))
                        .filter(|f| !f.contains('.'))
                        .map(str::to_string)
                    else {
                        continue;
                    };
                    // Only a currently-SAFE source is worth weakening (an
                    // already-optional one would just re-warn what V26 caught).
                    let is_required = t_def
                        .pointer("/inputSchema/required")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().any(|v| v.as_str() == Some(field.as_str())))
                        .unwrap_or(false);
                    let has_default = t_def
                        .pointer(&format!("/inputSchema/properties/{field}/default"))
                        .is_some();
                    if !is_required && !has_default {
                        continue;
                    }

                    let label =
                        format!("weaken_output_source/{wf_id}/{state_name}/{t_name}/{field}");
                    let (wf_id_c, sn_c, tn_c, field_c) = (
                        wf_id.clone(),
                        state_name.clone(),
                        t_name.clone(),
                        field.clone(),
                    );
                    let mutant = mutate(config, |v| {
                        let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut)
                        else {
                            return;
                        };
                        let Some(td) = get_transition_mut(wfs_mut, &wf_id_c, &sn_c, &tn_c) else {
                            return;
                        };
                        let Some(schema) = td.get_mut("inputSchema").and_then(Value::as_object_mut)
                        else {
                            return;
                        };
                        // Drop the field from `required`...
                        if let Some(req) = schema.get_mut("required").and_then(Value::as_array_mut)
                        {
                            req.retain(|v| v.as_str() != Some(field_c.as_str()));
                        }
                        // ...and remove any `default`.
                        if let Some(prop) = schema
                            .get_mut("properties")
                            .and_then(Value::as_object_mut)
                            .and_then(|p| p.get_mut(&field_c))
                            .and_then(Value::as_object_mut)
                        {
                            prop.remove("default");
                        }
                    });
                    mutants.push((label, mutant));
                }
            }
        }
    }
    mutants
}

/// CONTRACT — for each declared output seeded in `initialContext`, remove the
/// seed.
///
/// The mirror of [`drop_output_write`]: seeding is the OTHER way a definition
/// discharges its contract on a path that has no writer (it is the one-line fix
/// V24 asks for). Deleting the seed must therefore re-open the same hole — if it
/// doesn't, the seed was load-bearing for nothing and the rule isn't actually
/// checking what it claims.
pub fn drop_initial_context_seed(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();
    let Some(wfs) = config.pointer("/workflows").and_then(Value::as_object) else {
        return mutants;
    };

    for (wf_id, wf_def) in wfs {
        let Some(declared) = wf_def
            .pointer("/snippet/outputs")
            .or_else(|| wf_def.pointer("/outputs"))
            .and_then(Value::as_object)
            .filter(|o| !o.is_empty())
        else {
            continue;
        };
        let Some(seeds) = wf_def.pointer("/initialContext").and_then(Value::as_object) else {
            continue;
        };

        for slot in seeds.keys().filter(|k| declared.contains_key(*k)) {
            let label = format!("drop_initial_context_seed/{wf_id}/{slot}");
            let wf_id_c = wf_id.clone();
            let slot_c = slot.clone();
            let mutant = mutate(config, |v| {
                let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut) else {
                    return;
                };
                if let Some(wf) = wfs_mut.get_mut(&wf_id_c).and_then(Value::as_object_mut)
                    && let Some(ic) = wf.get_mut("initialContext").and_then(Value::as_object_mut)
                {
                    ic.remove(&slot_c);
                }
            });
            mutants.push((label, mutant));
        }
    }
    mutants
}

/// SCOPE — for each `use.inputs` value binding `$.workflow.input.<x>`, rewrite it
/// to the bare `$.input.<x>` spelling `resolve_one` does NOT resolve. Killed by
/// V28. (The compose-boundary read twin of the guard/output operators.)
pub fn retarget_use_input_scope(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();
    for (wf_id, state_name, t_name, t_def) in each_transition(config) {
        let Some(inputs) = t_def
            .pointer("/executor/use/inputs")
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (name, value) in inputs {
            let Some(src) = value.as_str() else { continue };
            if !src.starts_with("$.workflow.input.") {
                continue;
            }
            let broken = src.replace("$.workflow.input.", "$.input.");
            let label = format!("retarget_use_input_scope/{wf_id}/{state_name}/{t_name}/{name}");
            let (wf_id_c, sn_c, tn_c, name_c) = (
                wf_id.clone(),
                state_name.clone(),
                t_name.clone(),
                name.clone(),
            );
            let mutant = mutate(config, |v| {
                let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut) else {
                    return;
                };
                if let Some(td) = get_transition_mut(wfs_mut, &wf_id_c, &sn_c, &tn_c)
                    && let Some(inp) = td
                        .get_mut("executor")
                        .and_then(Value::as_object_mut)
                        .and_then(|e| e.get_mut("use"))
                        .and_then(Value::as_object_mut)
                        .and_then(|u| u.get_mut("inputs"))
                        .and_then(Value::as_object_mut)
                {
                    inp.insert(name_c.clone(), Value::String(broken.clone()));
                }
            });
            mutants.push((label, mutant));
        }
    }
    mutants
}

/// SCOPE — for each executor `args:` entry binding `$.workflow.input.<x>`, rewrite
/// it to the bare `$.input.<x>` spelling that reaches the shell as a literal.
/// Killed by V29.
pub fn retarget_executor_arg_scope(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();
    for (wf_id, state_name, t_name, t_def) in each_transition(config) {
        let Some(args) = t_def.pointer("/executor/args").and_then(Value::as_array) else {
            continue;
        };
        for (i, arg) in args.iter().enumerate() {
            let Some(src) = arg.as_str() else { continue };
            if !src.starts_with("$.workflow.input.") {
                continue;
            }
            let broken = src.replace("$.workflow.input.", "$.input.");
            let label = format!("retarget_executor_arg_scope/{wf_id}/{state_name}/{t_name}/{i}");
            let (wf_id_c, sn_c, tn_c) = (wf_id.clone(), state_name.clone(), t_name.clone());
            let mutant = mutate(config, |v| {
                let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut) else {
                    return;
                };
                if let Some(td) = get_transition_mut(wfs_mut, &wf_id_c, &sn_c, &tn_c)
                    && let Some(a) = td
                        .get_mut("executor")
                        .and_then(Value::as_object_mut)
                        .and_then(|e| e.get_mut("args"))
                        .and_then(Value::as_array_mut)
                        .and_then(|a| a.get_mut(i))
                {
                    *a = Value::String(broken.clone());
                }
            });
            mutants.push((label, mutant));
        }
    }
    mutants
}

/// SCOPE — for each `output:` mapping that writes a `$.workflow.input.<x>` value,
/// rewrite it to the bare `$.input.<x>` spelling the write resolver does NOT
/// resolve.
///
/// The write-side twin of [`retarget_guard_scope`]. `$.input.*` coalesces to
/// `null` in `resolve_value` and silently writes it — the bug V27
/// (`UNRESOLVABLE_WRITE_SCOPE`) exists to catch. Before V27 it survived silently.
pub fn retarget_output_scope(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();

    for (wf_id, state_name, t_name, t_def) in each_transition(config) {
        let Some(output) = t_def.get("output").and_then(Value::as_object) else {
            continue;
        };
        for (key, spec) in output {
            let Some(src) = spec.as_str() else { continue };
            if !src.starts_with("$.workflow.input.") {
                continue;
            }
            let broken = src.replace("$.workflow.input.", "$.input.");
            let label = format!("retarget_output_scope/{wf_id}/{state_name}/{t_name}/{key}");
            let (wf_id_c, sn_c, tn_c, key_c) = (
                wf_id.clone(),
                state_name.clone(),
                t_name.clone(),
                key.clone(),
            );
            let mutant = mutate(config, |v| {
                let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut) else {
                    return;
                };
                if let Some(td) = get_transition_mut(wfs_mut, &wf_id_c, &sn_c, &tn_c)
                    && let Some(o) = td.get_mut("output").and_then(Value::as_object_mut)
                {
                    o.insert(key_c.clone(), Value::String(broken.clone()));
                }
            });
            mutants.push((label, mutant));
        }
    }
    mutants
}

/// SCOPE — for each `expr` guard reading `$.workflow.input.<x>`, rewrite it to the
/// bare `$.input.<x>` spelling the guard evaluator does NOT resolve.
///
/// This is the mutant that proves V25 works. `$.input.*` coalesces to `null` at
/// eval, so the guard becomes permanently false — the exact dead-guard bug that
/// shipped in `cap.gate.human-approve-plan`. V25 (`UNRESOLVABLE_GUARD_SCOPE`) must
/// kill it at load; before V25 it survived silently.
pub fn retarget_guard_scope(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();

    for (wf_id, state_name, t_name, t_def) in each_transition(config) {
        let Some(guards) = t_def.get("guards").and_then(Value::as_array) else {
            continue;
        };
        for (gi, guard) in guards.iter().enumerate() {
            if guard.get("kind").and_then(Value::as_str) != Some("expr") {
                continue;
            }
            let Some(expr) = guard.get("expr").and_then(Value::as_str) else {
                continue;
            };
            if !expr.contains("$.workflow.input.") {
                continue;
            }
            let broken = expr.replace("$.workflow.input.", "$.input.");
            let label = format!("retarget_guard_scope/{wf_id}/{state_name}/{t_name}/guard[{gi}]");
            let (wf_id_c, sn_c, tn_c) = (wf_id.clone(), state_name.clone(), t_name.clone());
            let mutant = mutate(config, |v| {
                let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut) else {
                    return;
                };
                if let Some(td) = get_transition_mut(wfs_mut, &wf_id_c, &sn_c, &tn_c)
                    && let Some(gs) = td.get_mut("guards").and_then(Value::as_array_mut)
                    && let Some(g) = gs.get_mut(gi).and_then(Value::as_object_mut)
                {
                    g.insert("expr".into(), Value::String(broken.clone()));
                }
            });
            mutants.push((label, mutant));
        }
    }
    mutants
}

/// SEMANTIC — for each guarded transition, remove all its guards.
///
/// Known blind spot: the tool cannot tell whether a guard is intentional without
/// scenario tests. These mutants are EXPECTED to SURVIVE.
pub fn delete_guard(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();
    let transitions = each_transition(config);

    for (wf_id, state_name, t_name, t_def) in transitions {
        let has_guards = t_def
            .get("guards")
            .and_then(Value::as_array)
            .map(|g| !g.is_empty())
            .unwrap_or(false);
        if !has_guards {
            continue;
        }

        let label = format!("delete_guard/{wf_id}/{state_name}/{t_name}");
        let wf_id_c = wf_id.clone();
        let sn_c = state_name.clone();
        let tn_c = t_name.clone();
        let mutant = mutate(config, |v| {
            let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut) else {
                return;
            };
            if let Some(td) = get_transition_mut(wfs_mut, &wf_id_c, &sn_c, &tn_c) {
                td.remove("guards");
            }
        });
        mutants.push((label, mutant));
    }
    mutants
}

/// SEMANTIC — for each `kind: expr` guard, flip the comparison operator.
///
/// Supported flips: `==`↔`!=`, `<`↔`>=`, `>`↔`<=`.
/// These mutants are EXPECTED to SURVIVE (the tool cannot know correct guard intent).
pub fn flip_guard_op(config: &Value) -> Vec<(String, Value)> {
    let mut mutants = Vec::new();
    let transitions = each_transition(config);

    for (wf_id, state_name, t_name, t_def) in transitions {
        let Some(guards) = t_def.get("guards").and_then(Value::as_array) else {
            continue;
        };

        for (gi, guard) in guards.iter().enumerate() {
            if guard.get("kind").and_then(Value::as_str) != Some("expr") {
                continue;
            }
            let Some(expr) = guard.get("expr").and_then(Value::as_str) else {
                continue;
            };
            let Some(flipped_expr) = flip_operator(expr) else {
                continue;
            };

            let label = format!("flip_guard_op/{wf_id}/{state_name}/{t_name}/guard[{gi}]");
            let wf_id_c = wf_id.clone();
            let sn_c = state_name.clone();
            let tn_c = t_name.clone();
            let mutant = mutate(config, |v| {
                let Some(wfs_mut) = v.get_mut("workflows").and_then(Value::as_object_mut) else {
                    return;
                };
                if let Some(td) = get_transition_mut(wfs_mut, &wf_id_c, &sn_c, &tn_c) {
                    if let Some(gs) = td.get_mut("guards").and_then(Value::as_array_mut) {
                        if let Some(g) = gs.get_mut(gi).and_then(Value::as_object_mut) {
                            g.insert("expr".into(), Value::String(flipped_expr.clone()));
                        }
                    }
                }
            });
            mutants.push((label, mutant));
        }
    }
    mutants
}

/// Flip a comparison operator in an expression string.
/// Returns `None` if no flippable operator is found.
fn flip_operator(expr: &str) -> Option<String> {
    // Order matters: try longer operators first to avoid partial matches
    let flips: &[(&str, &str)] = &[
        ("!=", "=="),
        ("==", "!="),
        (">=", "<"),
        ("<=", ">"),
        ("<", ">="),
        (">", "<="),
    ];
    for (from, to) in flips {
        if expr.contains(from) {
            return Some(expr.replacen(from, to, 1));
        }
    }
    None
}

// ── Mutation report ───────────────────────────────────────────────────────────

/// Per-operator mutation result.
#[derive(Debug, Clone)]
pub struct OperatorResult {
    pub operator: String,
    pub total: usize,
    pub killed: usize,
    pub survived: usize,
    /// Label of a surviving mutant (if any), for diagnosis.
    pub sample_survivor: Option<String>,
}

impl OperatorResult {
    pub fn kill_rate(&self) -> f64 {
        if self.total == 0 {
            return 1.0; // vacuously "100%" (no mutants generated)
        }
        self.killed as f64 / self.total as f64
    }
}

/// Full mutation report across all operators.
#[derive(Debug)]
pub struct MutationReport {
    pub per_operator: Vec<OperatorResult>,
    pub overall_score: f64,
}

impl MutationReport {
    /// Render a plain-text table suitable for test output.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{:<26} {:>6} {:>7} {:>9} {:>9}\n",
            "operator", "total", "killed", "survived", "kill_rate"
        ));
        out.push_str(&"-".repeat(64));
        out.push('\n');
        for r in &self.per_operator {
            out.push_str(&format!(
                "{:<26} {:>6} {:>7} {:>9} {:>8.0}%\n",
                r.operator,
                r.total,
                r.killed,
                r.survived,
                r.kill_rate() * 100.0
            ));
        }
        out.push_str(&"-".repeat(64));
        out.push('\n');
        out.push_str(&format!(
            "Overall mutation score: {:.1}%\n",
            self.overall_score * 100.0
        ));
        out
    }
}

// ── Harness ───────────────────────────────────────────────────────────────────

/// What the gates already say about the UNMUTATED config.
///
/// A mutant may only be credited as KILLED for a complaint it actually CAUSED.
/// Without this, the score is a lie: run the harness against any real corpus with
/// a single pre-existing warning (the live cognitive-architectures pack has four,
/// plus 116 known fuzz findings) and EVERY mutant is "killed" by a defect that was
/// already there — including the two SEMANTIC operators that exist precisely to
/// document what the tool CANNOT catch. A 100% score with the blind spots also at
/// 100% is the tell.
struct Baseline {
    diagnostics: std::collections::HashSet<String>,
    fuzz_findings: std::collections::HashSet<String>,
}

impl Baseline {
    async fn capture(resolved: &Value) -> Self {
        let diagnostics = praxec_core::validate::validate_workflows(resolved)
            .into_iter()
            .map(|d| d.message().to_string())
            .collect();
        let fuzz_findings = fuzz_findings_of(resolved).await.unwrap_or_default();
        Self {
            diagnostics,
            fuzz_findings,
        }
    }
}

/// Every fuzz complaint about a config, as a stable set of keys — so a mutant's
/// findings can be diffed against the baseline's instead of collapsed into a
/// single "did anything fail?" bit.
async fn fuzz_findings_of(resolved: &Value) -> anyhow::Result<std::collections::HashSet<String>> {
    let tmp_path = write_temp_config(resolved)?;
    let result = crate::fuzz_coverage(&tmp_path).await;
    let _ = std::fs::remove_file(&tmp_path);
    let cov = result?;

    let mut findings = std::collections::HashSet::new();
    for d in &cov.defs {
        for orphan in &d.orphan_states {
            findings.insert(format!("{}|orphan|{orphan}", d.definition_id));
        }
        for loop_state in &d.det_loops {
            findings.insert(format!("{}|loop|{loop_state}", d.definition_id));
        }
        for v in d.verdicts.iter().filter(|v| !v.ok) {
            findings.insert(format!(
                "{}|edge|{}.{}|{}",
                d.definition_id, v.state, v.transition, v.detail
            ));
        }
    }
    Ok(findings)
}

/// Serialize a resolved config to a temp file. JSON is valid YAML, so the normal
/// loader parses it.
fn write_temp_config(resolved: &Value) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write;
    let mut path = std::env::temp_dir();
    let tid = std::thread::current().id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let uniq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.push(format!("praxec_mutant_{tid:?}_{nanos}_{uniq}.json"));
    let mut f = std::fs::File::create(&path)?;
    f.write_all(&serde_json::to_vec(resolved)?)?;
    Ok(path)
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Run every mutation operator over a resolved config `Value`.
///
/// A mutant is KILLED when it makes a gate say something NEW relative to the
/// unmutated baseline:
/// 1. `config::resolve(mutant)` errors (and the baseline resolved), or
/// 2. `validate_workflows()` emits a diagnostic the baseline did not, or
/// 3. `fuzz_coverage()` reports a finding the baseline did not.
///
/// A mutant that changes nothing any gate says SURVIVES — the tool is blind to it.
pub async fn mutation_score(resolved: &Value) -> anyhow::Result<MutationReport> {
    type MutationOperator = (&'static str, fn(&Value) -> Vec<(String, Value)>);
    let operators: Vec<MutationOperator> = vec![
        ("orphan_state", orphan_state),
        ("dangle_target", dangle_target),
        ("deadend", deadend),
        ("det_cycle", det_cycle),
        ("literal_type_break", literal_type_break),
        ("drop_output_write", drop_output_write),
        ("drop_initial_context_seed", drop_initial_context_seed),
        ("weaken_output_source", weaken_output_source),
        ("retarget_guard_scope", retarget_guard_scope),
        ("retarget_output_scope", retarget_output_scope),
        ("retarget_use_input_scope", retarget_use_input_scope),
        ("retarget_executor_arg_scope", retarget_executor_arg_scope),
        ("delete_guard", delete_guard),
        ("flip_guard_op", flip_guard_op),
    ];

    let baseline = Baseline::capture(resolved).await;
    let mut per_operator = Vec::new();

    for (op_name, op_fn) in &operators {
        let mutants = op_fn(resolved);
        let total = mutants.len();
        let mut killed = 0usize;
        let mut sample_survivor: Option<String> = None;

        for (label, mutant) in &mutants {
            let is_killed = assess_mutant(mutant, &baseline).await;
            if is_killed {
                killed += 1;
            } else if sample_survivor.is_none() {
                sample_survivor = Some(label.clone());
            }
        }

        let survived = total - killed;
        per_operator.push(OperatorResult {
            operator: op_name.to_string(),
            total,
            killed,
            survived,
            sample_survivor,
        });
    }

    let total_all: usize = per_operator.iter().map(|r| r.total).sum();
    let killed_all: usize = per_operator.iter().map(|r| r.killed).sum();
    let overall_score = if total_all == 0 {
        1.0
    } else {
        killed_all as f64 / total_all as f64
    };

    Ok(MutationReport {
        per_operator,
        overall_score,
    })
}

/// Assess a single mutant against the baseline: `true` if KILLED, `false` if it
/// SURVIVED.
///
/// A mutant is killed only by a complaint it CAUSED. Every gate is diffed against
/// what it already said about the unmutated config:
/// - `config::resolve()` errors (the baseline resolved, or there'd be no report),
/// - `validate_workflows()` emits a diagnostic not in the baseline,
/// - `fuzz_coverage()` reports a finding not in the baseline.
async fn assess_mutant(mutant: &Value, baseline: &Baseline) -> bool {
    // Gate 1: resolve (catches dangling targets and the like).
    let re_resolved = match praxec_core::config::resolve(mutant.clone()) {
        Err(_) => return true, // KILLED at resolve
        Ok(v) => v,
    };

    // Gate 2: a NEW diagnostic. Warnings count — a mutant that provokes a warning
    // the clean config never produced has been noticed, which is the whole
    // question. But it must be new: a corpus with pre-existing diagnostics would
    // otherwise "kill" every mutant, including the ones the tool is blind to.
    let new_diagnostic = praxec_core::validate::validate_workflows(&re_resolved)
        .into_iter()
        .any(|d| !baseline.diagnostics.contains(d.message()));
    if new_diagnostic {
        return true;
    }

    // Gate 3: a NEW fuzz finding.
    match fuzz_findings_of(&re_resolved).await {
        Ok(findings) => findings
            .difference(&baseline.fuzz_findings)
            .next()
            .is_some(),
        Err(_) => true, // the mutant broke the loader outright
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// THE test that keeps the score honest: an UNMUTATED config must SURVIVE its
    /// own baseline.
    ///
    /// If baseline capture silently fails (or is never subtracted), a corpus with
    /// any pre-existing complaint "kills" every mutant it is handed — including
    /// the identity one — and the mutation score reads 100% while measuring
    /// nothing at all. That is not hypothetical: it is exactly what the harness
    /// did before, and the tell was the two SEMANTIC operators, which exist to
    /// document what the tool CANNOT catch, also reporting 100%.
    ///
    /// The config below carries a deliberate pre-existing defect (`dead` is a
    /// non-terminal state with no way out). A mutant that introduces nothing new
    /// must still come back SURVIVED.
    #[tokio::test]
    async fn an_unmutated_config_survives_its_own_baseline() {
        let config = json!({
            "version": "1.0.0",
            "workflows": { "wf": {
                "version": "1.0.0",
                "initialState": "start",
                "states": {
                    "start": { "transitions": {
                        "go":   { "target": "done", "actor": "agent", "executor": { "kind": "noop" } },
                        "trap": { "target": "dead", "actor": "agent", "executor": { "kind": "noop" } }
                    }},
                    // Pre-existing defect: reachable, non-terminal, no way out.
                    "dead": { "transitions": {} },
                    "done": { "terminal": true, "transitions": {} }
                }
            }}
        });

        let baseline = Baseline::capture(&config).await;
        assert!(
            !baseline.diagnostics.is_empty() || !baseline.fuzz_findings.is_empty(),
            "fixture must actually have a pre-existing complaint, or this test proves nothing"
        );

        assert!(
            !assess_mutant(&config, &baseline).await,
            "the identity mutant changed NOTHING, so it must SURVIVE. Killing it means the \
             kill criterion is absolute rather than baseline-relative, and every score the \
             harness reports on a real corpus is meaningless."
        );
    }

    fn simple_config() -> Value {
        // A minimal resolved config: start →(go)→ review →(approve, guarded)→ done
        json!({
            "version": "1.0.0",
            "workflows": {
                "wf": {
                    "version": "1.0.0",
                    "initialState": "start",
                    "blackboard": {
                        "approved": { "type": "boolean" }
                    },
                    "states": {
                        "start": {
                            "transitions": {
                                "go": {
                                    "target": "review",
                                    "actor": "agent",
                                    "executor": { "kind": "noop" }
                                }
                            }
                        },
                        "review": {
                            "transitions": {
                                "approve": {
                                    "target": "done",
                                    "actor": "agent",
                                    "executor": { "kind": "noop" },
                                    "guards": [
                                        { "kind": "expr", "expr": "$.context.approved == true" }
                                    ]
                                }
                            }
                        },
                        "done": { "terminal": true, "transitions": {} }
                    }
                }
            }
        })
    }

    #[test]
    fn orphan_state_produces_mutants() {
        let cfg = simple_config();
        let mutants = orphan_state(&cfg);
        assert!(
            !mutants.is_empty(),
            "should produce at least one orphan mutant"
        );
        for (label, mutant) in &mutants {
            let state = label.split('/').next_back().unwrap_or("");
            if let Some(states) = mutant
                .pointer("/workflows/wf/states")
                .and_then(Value::as_object)
            {
                for (sn, sd) in states {
                    if sn == state {
                        continue;
                    }
                    if let Some(ts) = sd.pointer("/transitions").and_then(Value::as_object) {
                        for td in ts.values() {
                            let tgt = td.get("target").and_then(Value::as_str).unwrap_or("");
                            assert_ne!(
                                tgt, state,
                                "inbound edge to {state} should be severed in mutant from {sn}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn dangle_target_replaces_all_targets() {
        let cfg = simple_config();
        let mutants = dangle_target(&cfg);
        assert!(
            !mutants.is_empty(),
            "should produce mutants for every transition"
        );
        for (_label, mutant) in &mutants {
            let has_ghost = mutant
                .pointer("/workflows/wf/states")
                .and_then(Value::as_object)
                .map(|states| {
                    states.values().any(|sd| {
                        sd.pointer("/transitions")
                            .and_then(Value::as_object)
                            .map(|ts| {
                                ts.values().any(|td| {
                                    td.get("target").and_then(Value::as_str)
                                        == Some("__mutant_nonexistent_state__")
                                })
                            })
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            assert!(has_ghost, "mutant should contain ghost target");
        }
    }

    #[test]
    fn deadend_empties_transitions() {
        let cfg = simple_config();
        let mutants = deadend(&cfg);
        assert!(
            !mutants.is_empty(),
            "should produce at least one dead-end mutant"
        );
        for (label, mutant) in &mutants {
            let state = label.split('/').next_back().unwrap_or("");
            let ts = mutant
                .pointer(&format!("/workflows/wf/states/{state}/transitions"))
                .and_then(Value::as_object);
            assert!(
                ts.map(|t| t.is_empty()).unwrap_or(false),
                "dead-end mutant should have empty transitions for state {state}"
            );
        }
    }

    #[test]
    fn flip_guard_op_flips_eq() {
        let cfg = simple_config();
        let mutants = flip_guard_op(&cfg);
        assert!(
            !mutants.is_empty(),
            "should produce flip mutants for guarded transitions"
        );
        for (_label, mutant) in &mutants {
            if let Some(g0) =
                mutant.pointer("/workflows/wf/states/review/transitions/approve/guards/0")
            {
                let expr = g0.get("expr").and_then(Value::as_str).unwrap_or("");
                assert!(
                    !expr.contains("== true"),
                    "flipped expr should not still be == true: {expr}"
                );
                assert!(
                    expr.contains("!= true"),
                    "flipped expr should be != true: {expr}"
                );
            }
        }
    }

    #[test]
    fn delete_guard_removes_guards() {
        let cfg = simple_config();
        let mutants = delete_guard(&cfg);
        assert!(!mutants.is_empty(), "should produce delete-guard mutants");
        for (_label, mutant) in &mutants {
            let guards = mutant.pointer("/workflows/wf/states/review/transitions/approve/guards");
            assert!(
                guards.is_none(),
                "delete_guard mutant should have no guards"
            );
        }
    }

    #[test]
    fn weaken_output_source_strips_safety_and_is_killed_by_v26() {
        // A scalar output sourced from a currently-SAFE (defaulted) argument.
        let cfg = json!({ "workflows": { "cap.review.thing": {
            "verb": "review",
            "initialState": "s",
            "snippet": { "inputs": {}, "outputs": { "summary": { "type": "string" } } },
            "states": {
                "s": { "transitions": { "submit": {
                    "target": "done", "actor": "agent",
                    "inputSchema": { "type": "object", "required": [],
                        "properties": { "summary": { "type": "string", "default": "" } } },
                    "executor": { "kind": "noop" },
                    "output": { "summary": "$.arguments.summary" }
                }}},
                "done": { "terminal": true }
            }
        }}});
        let mutants = weaken_output_source(&cfg);
        assert_eq!(mutants.len(), 1, "one safe scalar source to weaken");
        let (_l, mutant) = &mutants[0];
        // The default is gone.
        assert!(
            mutant
                .pointer("/workflows/cap.review.thing/states/s/transitions/submit/inputSchema/properties/summary/default")
                .is_none(),
            "default must be stripped"
        );
        // ...and V26 catches it.
        assert!(
            praxec_core::validate::validate_workflows(mutant)
                .iter()
                .any(|d| d.message().contains("SCALAR_OUTPUT_FROM_OPTIONAL_SOURCE")),
            "the mutant must be killed by V26"
        );
    }

    #[test]
    fn retarget_guard_scope_breaks_only_input_guards_and_is_killed_by_v25() {
        let cfg = json!({ "workflows": { "wf": {
            "initialState": "start",
            "states": {
                "start": { "transitions": {
                    "go": { "target": "done", "actor": "deterministic", "executor": { "kind": "noop" },
                            "guards": [ { "kind": "expr", "expr": "$.workflow.input.mode == 'auto'" } ] }
                }},
                "done": { "terminal": true }
            }
        }}});
        let mutants = retarget_guard_scope(&cfg);
        assert_eq!(mutants.len(), 1, "one input-scoped guard to retarget");
        let (_label, mutant) = &mutants[0];
        let expr = mutant
            .pointer("/workflows/wf/states/start/transitions/go/guards/0/expr")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(
            expr, "$.input.mode == 'auto'",
            "rewritten to the dead spelling"
        );

        // The whole point: V25 must catch it.
        assert!(
            praxec_core::validate::validate_workflows(mutant)
                .iter()
                .any(|d| d.message().contains("UNRESOLVABLE_GUARD_SCOPE")),
            "the mutant must be killed by V25"
        );
    }

    #[test]
    fn retarget_output_scope_breaks_input_writes_and_is_killed_by_v27() {
        let cfg = json!({ "workflows": { "wf": {
            "initialState": "start",
            "states": {
                "start": { "transitions": {
                    "go": { "target": "done", "actor": "deterministic", "executor": { "kind": "noop" },
                            "output": { "plan_final": "$.workflow.input.plan" } }
                }},
                "done": { "terminal": true }
            }
        }}});
        let mutants = retarget_output_scope(&cfg);
        assert_eq!(
            mutants.len(),
            1,
            "one input-scoped output write to retarget"
        );
        let (_l, mutant) = &mutants[0];
        assert_eq!(
            mutant
                .pointer("/workflows/wf/states/start/transitions/go/output/plan_final")
                .and_then(Value::as_str),
            Some("$.input.plan"),
            "rewritten to the dead spelling"
        );
        assert!(
            praxec_core::validate::validate_workflows(mutant)
                .iter()
                .any(|d| d.message().contains("UNRESOLVABLE_WRITE_SCOPE")),
            "the mutant must be killed by V27"
        );
    }

    #[test]
    fn retarget_use_input_scope_is_killed_by_v28() {
        let cfg = json!({ "workflows": {
            "cap.thing": { "verb": "review", "initialState": "s",
                "snippet": { "inputs": { "x": { "type": "string" } }, "outputs": {} },
                "states": { "s": { "transitions": { "go": {
                    "target": "d", "actor": "deterministic", "executor": { "kind": "noop" } } } },
                    "d": { "terminal": true } } },
            "flow.h": { "initialState": "a", "states": {
                "a": { "transitions": { "call": {
                    "target": "d", "actor": "deterministic",
                    "executor": { "kind": "workflow", "definitionId": "cap.thing",
                        "use": { "inputs": { "x": "$.workflow.input.src" }, "outputs": {} } } } } },
                "d": { "terminal": true } } }
        }});
        let mutants = retarget_use_input_scope(&cfg);
        assert_eq!(mutants.len(), 1, "one use.inputs binding to retarget");
        assert!(
            praxec_core::validate::validate_workflows(&mutants[0].1)
                .iter()
                .any(|d| d.message().contains("UNRESOLVABLE_USE_INPUT_SCOPE")),
            "must be killed by V28"
        );
    }

    #[test]
    fn retarget_executor_arg_scope_is_killed_by_v29() {
        let cfg = json!({ "workflows": { "wf": {
            "initialState": "s",
            "states": {
                "s": { "transitions": { "go": {
                    "target": "d", "actor": "deterministic",
                    "executor": { "kind": "script", "subject": "x",
                                  "args": ["$.workflow.input.path"] } } } },
                "d": { "terminal": true }
            }
        }}});
        let mutants = retarget_executor_arg_scope(&cfg);
        assert_eq!(mutants.len(), 1, "one executor arg to retarget");
        assert_eq!(
            mutants[0]
                .1
                .pointer("/workflows/wf/states/s/transitions/go/executor/args/0")
                .and_then(Value::as_str),
            Some("$.input.path")
        );
        assert!(
            praxec_core::validate::validate_workflows(&mutants[0].1)
                .iter()
                .any(|d| d.message().contains("UNRESOLVABLE_EXECUTOR_ARG_SCOPE")),
            "must be killed by V29"
        );
    }

    #[test]
    fn literal_type_break_injects_wrong_type() {
        let cfg = simple_config();
        let mutants = literal_type_break(&cfg);
        assert!(
            !mutants.is_empty(),
            "should produce literal-type-break mutants"
        );
        // Each mutant should have a wrong-typed value for the 'approved' slot
        for (_label, mutant) in &mutants {
            let states = mutant
                .pointer("/workflows/wf/states")
                .and_then(Value::as_object);
            if let Some(states) = states {
                for (_sn, sd) in states {
                    if let Some(ts) = sd.pointer("/transitions").and_then(Value::as_object) {
                        for (_tn, td) in ts {
                            if let Some(approved_val) = td.pointer("/output/approved") {
                                // declared type is boolean → wrong type is a number (42)
                                assert!(
                                    !approved_val.is_boolean(),
                                    "approved slot should have wrong type (not bool) in mutant"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
