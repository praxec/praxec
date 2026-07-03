//! Mutation operators for workflow configs.
//!
//! Each operator takes a **resolved** config `&serde_json::Value` and produces a
//! `Vec<(label, mutant)>` — one mutant per applicable injection site. The label
//! is human-readable and uniquely identifies the site (used in reports).
//!
//! Seven operators are provided:
//!
//! | Operator              | Kind      | Expected kill? |
//! |-----------------------|-----------|----------------|
//! | orphan_state          | Structural | KILLED          |
//! | dangle_target         | Structural | KILLED          |
//! | deadend               | Structural | KILLED          |
//! | det_cycle             | Structural | KILLED          |
//! | literal_type_break    | Type       | KILLED          |
//! | delete_guard          | Semantic   | SURVIVES        |
//! | flip_guard_op         | Semantic   | SURVIVES        |
//!
//! The harness collects per-operator kill rates: structural/type operators
//! document the tool's guarantees; semantic operators document its honest limits.
//!
//! ## Implementation note — workflow IDs with slashes
//!
//! The resolved config from a repo-backed fixture uses namespace-prefixed IDs
//! like `cognitive/cap.coordinate.label-and-route`. JSON Pointer uses `/` as a
//! path separator, so `pointer_mut("/workflows/cognitive/cap...")` silently
//! mis-navigates. All mutations use direct `Map::get_mut` calls on the
//! `workflows` object to avoid this.

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

/// Run all seven mutation operators over a resolved config `Value`.
///
/// Each mutant is assessed three ways:
/// 1. `config::resolve(mutant)` — a load/resolve error = KILLED.
/// 2. `validate_workflows()` — any Error diagnostic = KILLED.
/// 3. `fuzz_coverage(temp_path)` — `has_failures()` = KILLED.
///
/// A mutant that passes all three checks SURVIVES.
pub async fn mutation_score(resolved: &Value) -> anyhow::Result<MutationReport> {
    type MutationOperator = (&'static str, fn(&Value) -> Vec<(String, Value)>);
    let operators: Vec<MutationOperator> = vec![
        ("orphan_state", orphan_state),
        ("dangle_target", dangle_target),
        ("deadend", deadend),
        ("det_cycle", det_cycle),
        ("literal_type_break", literal_type_break),
        ("delete_guard", delete_guard),
        ("flip_guard_op", flip_guard_op),
    ];

    let mut per_operator = Vec::new();

    for (op_name, op_fn) in &operators {
        let mutants = op_fn(resolved);
        let total = mutants.len();
        let mut killed = 0usize;
        let mut sample_survivor: Option<String> = None;

        for (label, mutant) in &mutants {
            let is_killed = assess_mutant(mutant).await;
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

/// Assess a single mutant: returns `true` if KILLED, `false` if SURVIVED.
///
/// Kill conditions (any one suffices):
/// - `config::resolve()` errors (config structurally invalid).
/// - `validate_workflows()` returns at least one Error-level diagnostic.
/// - `fuzz_coverage()` on a temp file returns `has_failures() == true`.
async fn assess_mutant(mutant: &Value) -> bool {
    // Gate 1: resolve (idempotent on already-resolved docs; catches dangling targets, etc.)
    let re_resolved = match praxec_core::config::resolve(mutant.clone()) {
        Err(_) => return true, // KILLED at resolve
        Ok(v) => v,
    };

    // Gate 2: validate_workflows (structural checks: orphans, dead-ends, loops, etc.)
    // Both errors AND warnings are treated as kills — we're running on known-good
    // configs (no pre-existing diagnostics), so any diagnostic on a mutant is new.
    let diagnostics = praxec_core::validate::validate_workflows(&re_resolved);
    if !diagnostics.is_empty() {
        return true; // KILLED by validator (error or warning)
    }

    // Gate 3: fuzz_coverage via a temp JSON file.
    // JSON is valid YAML, so load_yaml (called by load_resolved_with_repos) parses it.
    let tmp_path = {
        use std::io::Write;
        let mut path = std::env::temp_dir();
        // Use thread ID + subsec nanos for a unique name to avoid collisions in parallel tests.
        let tid = std::thread::current().id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        path.push(format!("praxec_mutant_{tid:?}_{nanos}.json"));
        let mut f = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(_) => return false,
        };
        let json_bytes = serde_json::to_vec(&re_resolved).unwrap_or_default();
        let _ = f.write_all(&json_bytes);
        path
    };

    let result = crate::fuzz_coverage(&tmp_path).await;
    let _ = std::fs::remove_file(&tmp_path);

    match result {
        Ok(cov) => cov.has_failures(),
        Err(_) => true, // load error = KILLED
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
