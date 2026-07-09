use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use serde_json::Value;

use crate::cap_verb::{BLESSED_CAP_VERBS, CapVerb, CapVerbCategory};
use crate::contract_hash::compute_contract_hash;
use crate::tier::Tier;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diagnostic {
    Error(String),
    Warning(String),
}

impl Diagnostic {
    pub fn is_error(&self) -> bool {
        matches!(self, Diagnostic::Error(_))
    }

    pub fn message(&self) -> &str {
        match self {
            Diagnostic::Error(m) | Diagnostic::Warning(m) => m,
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Diagnostic::Error(m) => write!(f, "error: {m}"),
            Diagnostic::Warning(m) => write!(f, "warning: {m}"),
        }
    }
}

pub fn validate_workflows(config: &Value) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // RULE 3 — strict mode: gateway.strict_validation: true promotes the two
    // structural warnings (unreachable state, dead-end non-terminal) to Errors.
    // Default off so existing configs without the flag are unaffected.
    let strict = config
        .pointer("/gateway/strict_validation")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let skill_subjects: HashSet<&str> = config
        .pointer("/skills")
        .and_then(Value::as_object)
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();

    let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) else {
        return diagnostics;
    };

    // SPEC §6.2 — build the cross-workflow context once. The contract-hash
    // index lets V15/V16 compare an `expects_contract_hash:` value against
    // the loaded target capability without recomputing per call site.
    let cap_contract_hashes: HashMap<String, String> = workflows
        .iter()
        .filter(|(id, _)| matches!(Tier::from_id(id), Tier::Cap))
        .filter_map(|(id, def)| {
            def.get("snippet")
                .map(|s| (id.clone(), compute_contract_hash(s)))
        })
        .collect();
    // H11b: index ONLY caps that explicitly declare a `lifecycle:`. Absence
    // is no longer coerced to `experimental` here — `validate_cap_lifecycle`
    // raises MISSING_LIFECYCLE for a cap with no lifecycle, and the V16
    // consumer (`validate_contract_hash_pins`) treats an unindexed target as
    // an error rather than silently exempting it from the pin requirement.
    let cap_lifecycles: HashMap<String, String> = workflows
        .iter()
        .filter(|(id, _)| matches!(Tier::from_id(id), Tier::Cap))
        .filter_map(|(id, def)| {
            def.get("lifecycle")
                .and_then(Value::as_str)
                .map(|token| (id.clone(), token.to_string()))
        })
        .collect();
    // Invokable outputs by definition id — drives slot-table type harvesting
    // for V13/V14. A capability declares them under `snippet.outputs`; a FLOW
    // (now nestable — V11 relaxed) declares them under a top-level `outputs:`.
    // Both are harvested so a `kind: workflow` child of either tier types its
    // `use.outputs`.
    let cap_snippet_outputs: HashMap<String, Value> = workflows
        .iter()
        .filter(|(id, _)| matches!(Tier::from_id(id), Tier::Cap | Tier::Flow))
        .filter_map(|(id, def)| {
            def.pointer("/snippet/outputs")
                .or_else(|| def.pointer("/outputs"))
                .cloned()
                .map(|outputs| (id.clone(), outputs))
        })
        .collect();
    let ctx = ValidationCtx {
        cap_contract_hashes: &cap_contract_hashes,
        cap_lifecycles: &cap_lifecycles,
        cap_snippet_outputs: &cap_snippet_outputs,
    };

    for (id, def) in workflows {
        validate_one_workflow(id, def, &skill_subjects, &ctx, strict, &mut diagnostics);
    }

    diagnostics
}

/// Parse a guard expression of the exact shape `$.some.path == 'literal'`
/// into `(path, string_literal)`. Returns `None` for anything else (range
/// comparisons, boolean/number equality, conjunctions, non-expr guards) —
/// only an open-domain *string* equality is a switch arm we require a default
/// for. Deliberately strict: a guard we can't classify is simply not counted.
fn parse_string_eq_guard(expr: &str) -> Option<(&str, &str)> {
    let (lhs, rhs) = expr.split_once("==")?;
    let path = lhs.trim();
    if !path.starts_with("$.") || path.contains(['&', '|', '<', '>', '!']) {
        return None;
    }
    let rhs = rhs.trim();
    // String literal only: single-quoted (the lexicon's guard form) or double.
    let lit = rhs
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| rhs.strip_prefix('"').and_then(|s| s.strip_suffix('"')))?;
    // A nested quote would mean we mis-split; reject to stay conservative.
    if lit.contains(['\'', '"']) {
        return None;
    }
    Some((path, lit))
}

/// V23 detector: returns the discriminant `$.path` of a deterministic
/// *string-literal switch* that lacks an unguarded default, or `None` when the
/// state is safe. A switch = the SAME `$.path` appears in a string-eq guard of
/// ≥2 deterministic transitions. A deterministic transition with no guards (or
/// an empty guard array) is the default that makes the state safe.
fn deterministic_switch_without_default(
    transitions: &serde_json::Map<String, Value>,
) -> Option<String> {
    let mut has_default = false;
    // path -> how many distinct deterministic transitions switch on it.
    let mut switch_counts: HashMap<String, usize> = HashMap::new();

    for t_def in transitions.values() {
        // Only deterministic transitions can dead-stall the chain selector.
        if t_def.get("actor").and_then(Value::as_str) != Some("deterministic") {
            continue;
        }
        let guards = t_def.get("guards").and_then(Value::as_array);
        let is_unguarded = guards.is_none_or(|g| g.is_empty());
        if is_unguarded {
            has_default = true;
            continue;
        }
        // Record each distinct string-eq discriminant this arm tests.
        let mut seen_here: HashSet<&str> = HashSet::new();
        for guard in guards.into_iter().flatten() {
            if let Some(expr) = guard.get("expr").and_then(Value::as_str) {
                if let Some((path, _lit)) = parse_string_eq_guard(expr) {
                    if seen_here.insert(path) {
                        *switch_counts.entry(path.to_string()).or_default() += 1;
                    }
                }
            }
        }
    }

    if has_default {
        return None;
    }
    switch_counts
        .into_iter()
        .find(|(_, n)| *n >= 2)
        .map(|(path, _)| path)
}

/// Cross-workflow validation context. Lets per-rule helpers reach into
/// other workflows' contract hashes + lifecycle declarations without
/// re-walking the registry per call site.
struct ValidationCtx<'a> {
    cap_contract_hashes: &'a HashMap<String, String>,
    cap_lifecycles: &'a HashMap<String, String>,
    cap_snippet_outputs: &'a HashMap<String, Value>,
}

fn validate_one_workflow(
    id: &str,
    def: &Value,
    skill_subjects: &HashSet<&str>,
    ctx: &ValidationCtx<'_>,
    strict: bool,
    out: &mut Vec<Diagnostic>,
) {
    // SPEC §28 — slot constraint shape validation. Catches typos like
    // unknown constraint kinds, malformed globs, empty allowlists at
    // load time so they don't surface as runtime
    // SLOT_CONSTRAINT_VIOLATED errors on the first transition.
    if let Err(e) = crate::slot_constraint::validate_constraints_in_definition(def) {
        out.push(Diagnostic::Error(format!("workflow '{id}': {e}")));
    }

    // SPEC §5.1, V3/V4/V5 — capability workflows MUST declare a typed
    // `snippet:` contract; flows MUST NOT (V8).
    // The tier is determined by the unprefixed id stem (`cap.` vs `flow.`).
    let tier = Tier::from_id(id);
    match tier {
        Tier::Cap => {
            v1_verb_in_cloud(id, def, out);
            v2_id_matches_verb_name(id, def, out);
            validate_snippet(id, def, out); // V3/V4/V5
            v6_primary_executor_verb_shape(id, def, out);
            v10_capability_does_not_invoke_workflow(id, def, out);
            // SPEC §6.2 — `lifecycle:` is a closed enum. A typo'd token
            // (e.g. `stabel`) would otherwise coerce to `experimental` in
            // `cap_lifecycles` and silently skip the V16 contract-hash
            // requirement. Reject unknown tokens at load.
            validate_cap_lifecycle(id, def, out);
        }
        Tier::Flow => {
            v7_id_matches_flow_pattern(id, def, out);
            v8_flow_has_no_snippet(id, def, out);
            v9_flow_has_no_verb(id, def, out);
            // V11 (flows-must-not-invoke-flows) RELAXED: flows may now invoke
            // other flows via `kind: workflow` to compose multi-flow programs
            // (e.g. the loom: flow.loom → flow.derisk / flow.execute-cohorts →
            // flow.implement.deliverable). Recursion is bounded at runtime by
            // the sub-workflow `max_depth` cap (runtime.rs), so unbounded /
            // cyclic nesting fails fast at depth rather than hanging. A nested
            // flow types its `use.outputs` from its top-level `outputs:` block
            // (harvested above alongside cap `snippet.outputs`).
            v_process_metadata(id, def, out);
            // V13 reachability + V14 type consistency run against the
            // per-flow slot table built in slot_table.rs.
            v13_v14_slot_table(id, def, ctx, out);
        }
        Tier::Other => {}
    }
    // SPEC §6.1, V12 — every `kind: workflow` executor inside this
    // workflow's transitions must conform to the use-binding contract.
    validate_use_bindings(id, def, out);
    // SPEC §33 D6 — the `_llm.*` synthetic namespace is reserved for the
    // in-runtime LLM executor's cumulative-cap bookkeeping. User-declared
    // blackboard slots that begin with `_llm.` (or the legacy synthetic
    // prefixes `_fire_count.` and `_while_iter.`) would shadow runtime
    // state and silently break the per-workflow counters; rejected at
    // load time.
    validate_reserved_blackboard_prefixes(id, def, out);
    // SPEC §6.2, V15/V16 — contract-hash pinning checks against the
    // pre-built `cap_contract_hashes` index (so we don't recompute per
    // call site).
    validate_contract_hash_pins(id, def, ctx, out);
    // SPEC §9 — guard `kind:` is a closed set. A typo (e.g. `permissoin`)
    // would otherwise reach the runtime as INVALID_GUARD_KIND on the
    // first transition; reject at load so `praxec check` catches it
    // before any workflow runs.
    validate_guard_kinds(id, def, out);
    // FALLBACK-05 — `output:` / `onEnter.output` / `prefill:` operator
    // objects (`{ add: [..] }`, `{ concat: [..] }`, …) are evaluated by
    // `mapping::resolve_value`, which silently coerces a MALFORMED operator
    // (wrong arity, non-array/non-numeric operands) to `Value::Null` and
    // writes that Null to the blackboard. Reject those shapes at load so a
    // typo'd operator can't masquerade as a real (null) value at runtime.
    validate_output_operator_shapes(id, def, out);
    // Spec A §7.1 — the fan-in composition check for `kind: parallel` steps.
    // Proves at LOAD that the reduce (aggregator) consumes only what the map
    // produces: every field the reduce requires must be satisfiable from the
    // fan-in envelope of worker outputs, so a mis-wired map-reduce cannot load.
    validate_parallel_edges(id, def, out);

    let Some(initial_state) = def.get("initialState").and_then(Value::as_str) else {
        out.push(Diagnostic::Error(format!(
            "workflow '{id}': missing 'initialState'"
        )));
        return;
    };

    let Some(states) = def.get("states").and_then(Value::as_object) else {
        out.push(Diagnostic::Error(format!(
            "workflow '{id}': missing 'states' map"
        )));
        return;
    };

    let state_names: BTreeSet<&str> = states.keys().map(String::as_str).collect();

    if !state_names.contains(initial_state) {
        out.push(Diagnostic::Error(format!(
            "workflow '{id}': initialState '{initial_state}' is not in states"
        )));
    }

    if let Some(timeout_target) = def.pointer("/onTimeout/target").and_then(Value::as_str) {
        if !state_names.contains(timeout_target) {
            out.push(Diagnostic::Error(format!(
                "workflow '{id}': onTimeout.target '{timeout_target}' is not in states"
            )));
        }
    }

    let mut transition_targets: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();

    for (state_name, state_def) in states {
        let is_terminal = state_def
            .get("terminal")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let transitions = state_def.get("transitions").and_then(Value::as_object);

        if !is_terminal && transitions.is_none_or(|t| t.is_empty()) {
            let has_on_timeout = def.pointer("/onTimeout/target").is_some();
            if !has_on_timeout {
                let msg = format!(
                    "workflow '{id}': state '{state_name}' is non-terminal with no outgoing transitions"
                );
                out.push(if strict {
                    Diagnostic::Error(msg)
                } else {
                    Diagnostic::Warning(msg)
                });
            }
        }

        if let Some(ts) = transitions {
            for (t_name, t_def) in ts {
                if let Some(target) = t_def.get("target").and_then(Value::as_str) {
                    if !state_names.contains(target) {
                        out.push(Diagnostic::Error(format!(
                            "workflow '{id}': transition '{t_name}' in state '{state_name}' \
                             targets '{target}' which is not in states"
                        )));
                    }
                    transition_targets
                        .entry(target)
                        .or_default()
                        .push((state_name, t_name));
                } else {
                    out.push(Diagnostic::Error(format!(
                        "workflow '{id}': transition '{t_name}' in state '{state_name}' \
                         is missing 'target'"
                    )));
                }

                if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                    for (idx, branch) in branches.iter().enumerate() {
                        if let Some(bt) = branch.get("target").and_then(Value::as_str) {
                            if !state_names.contains(bt) {
                                out.push(Diagnostic::Error(format!(
                                    "workflow '{id}': branch {idx} of transition '{t_name}' \
                                     in state '{state_name}' targets '{bt}' which is not in states"
                                )));
                            }
                        }
                    }
                }
            }
        }

        if let Some(on_enter) = state_def.get("onEnter") {
            if on_enter.get("executor").is_none() {
                out.push(Diagnostic::Warning(format!(
                    "workflow '{id}': state '{state_name}' has onEnter but no executor"
                )));
            }
        }

        // V23 DETERMINISTIC_SWITCH_NO_DEFAULT — a deterministic state that
        // branches on a string-literal discriminant (a switch: ≥2 deterministic
        // transitions whose guards `==`-compare the SAME `$.path` against string
        // literals) MUST carry an unguarded deterministic default. Without one, a
        // producer that emits a value outside the enumerated set matches NO guard,
        // and the chain dies with `selection_error` and NO recovery links — the
        // dead-stall poka-yoke gap. String domains are open, so such a switch is
        // never provably total; require the default. (Boolean/number equality
        // switches are NOT flagged — their small domains can be covered without a
        // default. This is the closed-set / exhaustive-match discipline.)
        if let Some(ts) = transitions {
            if let Some(disc) = deterministic_switch_without_default(ts) {
                // WARNING, not Error: the runtime already prevents the dead-stall
                // structurally (an unguarded transition is a lowest-precedence
                // default, and a no-default residual surfaces recovery links
                // instead of a silent selection_error). So this is advisory —
                // adding an explicit default is best practice (it routes the
                // unexpected value deliberately) — not a boot-blocking contract
                // Error. Making it an Error would let one ungated switch refuse
                // the whole gateway's boot, a worse footgun than it prevents.
                out.push(Diagnostic::Warning(format!(
                    "workflow '{id}': state '{state_name}' switches on '{disc}' across \
                     deterministic transitions with no unguarded default — an out-of-domain \
                     '{disc}' value falls through to recovery links rather than a chosen path; \
                     add a default deterministic transition (no guards) routing the unexpected \
                     value to an explicit error/human state"
                )));
            }
        }
    }

    // Blackboard slot check: if blackboard is declared, warn on any output: key not in the set.
    // RULE 1 — OUTPUT_TYPE_MISMATCH: for literal output values, compare the
    // literal's JSON type against the declared blackboard slot type. Skip
    // $.path values and operator objects (their runtime type isn't static).
    if let Some(blackboard) = def.get("blackboard") {
        let declared: HashSet<&str> = match blackboard {
            Value::Array(arr) => arr.iter().filter_map(|v| v.as_str()).collect(),
            Value::Object(obj) => obj.keys().map(String::as_str).collect(),
            _ => HashSet::new(),
        };

        // Build slot-type map only when blackboard is an object with typed entries.
        let slot_types: HashMap<&str, &str> = match blackboard {
            Value::Object(obj) => obj
                .iter()
                .filter_map(|(k, v)| {
                    v.get("type")
                        .and_then(Value::as_str)
                        .map(|t| (k.as_str(), t))
                })
                .collect(),
            _ => HashMap::new(),
        };

        for (state_name, state_def) in states {
            if let Some(ts) = state_def.get("transitions").and_then(Value::as_object) {
                for (t_name, t_def) in ts {
                    if let Some(output) = t_def.get("output").and_then(Value::as_object) {
                        for (key, value) in output {
                            if !declared.contains(key.as_str()) {
                                out.push(Diagnostic::Warning(format!(
                                    "workflow '{id}': transition '{t_name}' in state '{state_name}' \
                                     writes output key '{key}' which is not declared in the blackboard"
                                )));
                                // Key not declared — no point checking type.
                                continue;
                            }
                            // RULE 1 — OUTPUT_TYPE_MISMATCH: only check literals.
                            // Skip: $.path strings, operator objects ({set:}/{add:}/etc).
                            if is_literal_output_value(value) {
                                if let Some(declared_type) = slot_types.get(key.as_str()) {
                                    let actual_type = json_type_name(value);
                                    if !literal_type_matches(value, declared_type) {
                                        out.push(Diagnostic::Error(format!(
                                            "OUTPUT_TYPE_MISMATCH: workflow '{id}' transition \
                                             '{t_name}' in state '{state_name}' writes literal \
                                             output '{key}' of type {actual_type} but the \
                                             blackboard declares it {declared_type}"
                                        )));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Reachability: BFS from initialState
    if state_names.contains(initial_state) {
        let mut reachable = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(initial_state);
        reachable.insert(initial_state);

        while let Some(current) = queue.pop_front() {
            if let Some(state_def) = states.get(current) {
                if let Some(ts) = state_def.get("transitions").and_then(Value::as_object) {
                    for (_t_name, t_def) in ts {
                        if let Some(target) = t_def.get("target").and_then(Value::as_str) {
                            if state_names.contains(target) && reachable.insert(target) {
                                queue.push_back(target);
                            }
                        }
                        if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                            for branch in branches {
                                if let Some(bt) = branch.get("target").and_then(Value::as_str) {
                                    if state_names.contains(bt) && reachable.insert(bt) {
                                        queue.push_back(bt);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(timeout_target) = def.pointer("/onTimeout/target").and_then(Value::as_str) {
            if state_names.contains(timeout_target) {
                reachable.insert(timeout_target);
            }
        }

        for state_name in &state_names {
            if !reachable.contains(state_name) {
                let msg = format!(
                    "workflow '{id}': state '{state_name}' is unreachable from initialState '{initial_state}'"
                );
                out.push(if strict {
                    Diagnostic::Error(msg)
                } else {
                    Diagnostic::Warning(msg)
                });
            }
        }

        // Terminal-reachability ("stuck must be unrepresentable"): every
        // forward-reachable non-terminal state must have SOME path to a terminal.
        // The dead-end check catches a state with zero outgoing transitions and
        // DETERMINISTIC_LOOP catches all-deterministic cycles; this catches the
        // remaining wedge — a state (or group of states) with outgoing
        // transitions of ANY actor that nonetheless has no path to completion, so
        // a `running` mission parked there can never resolve. An `onTimeout`
        // target that can itself reach a terminal is a global escape hatch, so
        // when present-and-resolving it satisfies every state.
        {
            // Reverse adjacency (target -> source states), incl. branch targets.
            let mut predecessors: HashMap<&str, Vec<&str>> = HashMap::new();
            let mut terminals: Vec<&str> = Vec::new();
            for (state_name, state_def) in states {
                if state_def
                    .get("terminal")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    terminals.push(state_name.as_str());
                }
                let Some(ts) = state_def.get("transitions").and_then(Value::as_object) else {
                    continue;
                };
                for (_t_name, t_def) in ts {
                    let mut targets: Vec<&str> = Vec::new();
                    if let Some(tg) = t_def.get("target").and_then(Value::as_str) {
                        targets.push(tg);
                    }
                    if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                        for b in branches {
                            if let Some(bt) = b.get("target").and_then(Value::as_str) {
                                targets.push(bt);
                            }
                        }
                    }
                    for tg in targets {
                        if state_names.contains(tg) {
                            predecessors
                                .entry(tg)
                                .or_default()
                                .push(state_name.as_str());
                        }
                    }
                }
            }
            // Reverse-BFS from the terminals: the set that can reach completion.
            let mut can_reach_terminal: HashSet<&str> = HashSet::new();
            let mut q: VecDeque<&str> = VecDeque::new();
            for t in &terminals {
                if can_reach_terminal.insert(t) {
                    q.push_back(t);
                }
            }
            while let Some(cur) = q.pop_front() {
                if let Some(preds) = predecessors.get(cur) {
                    for &p in preds {
                        if can_reach_terminal.insert(p) {
                            q.push_back(p);
                        }
                    }
                }
            }
            let timeout_escape = def
                .pointer("/onTimeout/target")
                .and_then(Value::as_str)
                .map(|tt| can_reach_terminal.contains(tt))
                .unwrap_or(false);
            if !timeout_escape {
                for (state_name, state_def) in states {
                    let s = state_name.as_str();
                    let is_terminal = state_def
                        .get("terminal")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    // Skip terminals, unreachable states (already reported), and
                    // states that can reach completion.
                    if is_terminal || !reachable.contains(s) || can_reach_terminal.contains(s) {
                        continue;
                    }
                    let msg = format!(
                        "workflow '{id}': state '{s}' cannot reach any terminal state \
                         (no path to completion — a mission parked here would wedge)"
                    );
                    out.push(if strict {
                        Diagnostic::Error(msg)
                    } else {
                        Diagnostic::Warning(msg)
                    });
                }
            }
        }
    }

    check_use_before_def(id, def, states, initial_state, out);
    check_skills_refs(id, def, states, skill_subjects, out);
    validate_outcomes(id, def, states, out);
    // RULE 2 — DETERMINISTIC_LOOP: states in an all-deterministic cycle with
    // no escape (terminal or non-deterministic transition). Called after all
    // basic structural checks pass so state/transition names are valid.
    validate_deterministic_loops(id, def, states, out);
}

/// ADR-0008 — a mission's **outcomes** (its measurable definition of done) and
/// the **`outcome:` marker** on terminal states. Validated at load so a malformed
/// outcome (unparseable `check`, missing `statement`, an `outcome` on a
/// non-terminal, or outcomes with no success terminal to satisfy) is caught by
/// `praxec check` before any mission runs — never as a runtime surprise.
fn validate_outcomes(
    id: &str,
    def: &Value,
    states: &serde_json::Map<String, Value>,
    out: &mut Vec<Diagnostic>,
) {
    // Pass 1 — terminal `outcome` is a closed set {success, failure}, and is
    // meaningful only on a terminal state.
    let mut has_success_terminal = false;
    for (state_name, state_def) in states {
        let Some(outcome) = state_def.get("outcome") else {
            continue;
        };
        let is_terminal = state_def
            .get("terminal")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_terminal {
            out.push(Diagnostic::Error(format!(
                "workflow '{id}': state '{state_name}' declares `outcome` but is not terminal"
            )));
        }
        match outcome.as_str() {
            Some("success") => has_success_terminal = true,
            Some("failure") => {}
            _ => out.push(Diagnostic::Error(format!(
                "workflow '{id}': state '{state_name}' has invalid `outcome` \
                 (expected `success` or `failure`)"
            ))),
        }
    }

    // Pass 2 — the workflow-level `outcomes` list.
    let Some(outcomes) = def.get("outcomes") else {
        return;
    };
    let Some(arr) = outcomes.as_array() else {
        out.push(Diagnostic::Error(format!(
            "workflow '{id}': `outcomes` must be a list of {{id, statement, check}}"
        )));
        return;
    };
    let mut seen_ids: BTreeSet<&str> = BTreeSet::new();
    for (idx, oc) in arr.iter().enumerate() {
        let at = format!("outcomes[{idx}]");
        let Some(obj) = oc.as_object() else {
            out.push(Diagnostic::Error(format!(
                "workflow '{id}': {at} must be an object {{id, statement, check}}"
            )));
            continue;
        };
        match obj.get("id").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => {
                if !seen_ids.insert(s) {
                    out.push(Diagnostic::Error(format!(
                        "workflow '{id}': duplicate outcome id '{s}'"
                    )));
                }
            }
            _ => out.push(Diagnostic::Error(format!(
                "workflow '{id}': {at} is missing a non-empty `id`"
            ))),
        }
        if obj
            .get("statement")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            out.push(Diagnostic::Error(format!(
                "workflow '{id}': {at} is missing a non-empty `statement`"
            )));
        }
        match obj.get("check").and_then(Value::as_str) {
            Some(expr) if crate::guards::expr_parses(expr) => {}
            Some(expr) => out.push(Diagnostic::Error(format!(
                "workflow '{id}': {at} `check` is not a parseable expression: '{expr}'"
            ))),
            None => out.push(Diagnostic::Error(format!(
                "workflow '{id}': {at} is missing a `check` expression"
            ))),
        }
    }

    // Pass 3 — declaring outcomes is pointless without a success terminal to
    // gate (the deterministic definition of done has nowhere to resolve).
    if !arr.is_empty() && !has_success_terminal {
        out.push(Diagnostic::Error(format!(
            "workflow '{id}': declares `outcomes` but has no terminal state with `outcome: success`"
        )));
    }
}

/// Phase 6: SPEC §9, §11 — `$.context.X` referenced by an `expr` guard or
/// `{{ }}` template must have a reachable predecessor writer; `$.context.summary`
/// is never a valid guard input.
///
/// When `blackboard:` is declared, an additional check fires: a guard or
/// template that reads `$.context.X` for X **not in the declared slots** is
/// an error on the read side — independent of whether a writer happens to
/// exist (a writer to the same undeclared slot triggers the separate output:
/// warn from §6.1). Without a declared blackboard this check is skipped so
/// blackboard remains opt-in (SPEC §14 compatibility).
fn check_use_before_def(
    id: &str,
    def: &Value,
    states: &serde_json::Map<String, Value>,
    initial_state: &str,
    out: &mut Vec<Diagnostic>,
) {
    let writers = compute_writers_into(def, states, initial_state);
    let declared = declared_blackboard_slots(def);

    for (state_name, state_def) in states {
        let available = writers
            .get(state_name.as_str())
            .cloned()
            .unwrap_or_default();

        // Templates on the state (state.goal, state.guidance).
        for field in ["goal", "guidance"] {
            if let Some(text) = state_def.get(field).and_then(Value::as_str) {
                for slot in extract_template_context_slots(text) {
                    if slot == "summary" {
                        // summary is a model-authored content slot; reading it
                        // from a template is fine (it gets rendered). Only
                        // guards must not read it.
                        continue;
                    }
                    if let Some(declared) = &declared {
                        if !declared.contains(slot.as_str()) {
                            out.push(Diagnostic::Error(format!(
                                "workflow '{id}': state '{state_name}' template `{field}` reads \
                                 `$.context.{slot}` which is not a declared blackboard slot \
                                 (SPEC §11)"
                            )));
                            // Slot isn't declared — the use-before-def check
                            // is moot. Skip to the next slot.
                            continue;
                        }
                    }
                    if !available.contains(slot.as_str()) {
                        out.push(Diagnostic::Error(format!(
                            "workflow '{id}': state '{state_name}' template `{field}` reads `$.context.{slot}` \
                             which has no reachable writer (use-before-def, SPEC §11). \
                             Runtime will render a stub but this is a likely authoring bug."
                        )));
                    }
                }
            }
        }

        // Guards on every outgoing transition (incl. branch `when` guards).
        if let Some(ts) = state_def.get("transitions").and_then(Value::as_object) {
            for (t_name, t_def) in ts {
                let mut guards = collect_guards(t_def.get("guards"));
                if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                    for branch in branches {
                        if let Some(when) = branch.get("when") {
                            collect_guards_into(when, &mut guards);
                        }
                    }
                }
                for guard in guards {
                    let expr = match guard.get("expr").and_then(Value::as_str) {
                        Some(e) => e,
                        None => continue,
                    };
                    for slot in extract_expr_context_slots(expr) {
                        if slot == "summary" {
                            out.push(Diagnostic::Error(format!(
                                "workflow '{id}': transition '{t_name}' in state '{state_name}' \
                                 guard reads `$.context.summary` — model-authored summary is never \
                                 a valid guard input (SPEC §6.3)"
                            )));
                            continue;
                        }
                        if let Some(declared) = &declared {
                            if !declared.contains(slot.as_str()) {
                                out.push(Diagnostic::Error(format!(
                                    "workflow '{id}': transition '{t_name}' in state '{state_name}' \
                                     guard reads `$.context.{slot}` which is not a declared \
                                     blackboard slot (SPEC §11)"
                                )));
                                continue;
                            }
                        }
                        if !available.contains(slot.as_str()) {
                            out.push(Diagnostic::Error(format!(
                                "workflow '{id}': transition '{t_name}' in state '{state_name}' \
                                 guard reads `$.context.{slot}` which has no reachable writer \
                                 (use-before-def, SPEC §11)"
                            )));
                        }
                    }
                }
            }
        }
    }
}

/// SPEC §33 D6 — reject `blackboard:` slot declarations whose names
/// begin with a reserved synthetic prefix.
///
/// The runtime owns three synthetic namespaces in `WorkflowInstance.context`:
///
/// - `_fire_count.<state>.<transition>` — SPEC §29 per-state fire counters
///   (managed by `runtime_submit.rs`).
/// - `_while_iter.<state>` — SPEC §26 while-loop iteration counters
///   (managed by `runtime_chain.rs`).
/// - `_llm.*` — SPEC §33 D6 cumulative-cap state for the in-runtime LLM
///   executor (`cumulative_tokens`, `cumulative_cost_usd`,
///   `cumulative_iterations`, `consecutive_no_tool_call`, and
///   `session.<state>.started_at`).
///
/// User-declared blackboard slots that overlap these namespaces would
/// silently break the runtime's counters; reject at load time so the
/// workflow author sees the conflict before any instance is dispatched.
fn validate_reserved_blackboard_prefixes(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    const RESERVED_PREFIXES: &[&str] = &["_llm.", "_fire_count.", "_while_iter."];

    let Some(bb) = def.get("blackboard") else {
        return;
    };

    let visit = |slot: &str, out: &mut Vec<Diagnostic>| {
        for prefix in RESERVED_PREFIXES {
            if slot.starts_with(prefix) {
                out.push(Diagnostic::Error(format!(
                    "workflow '{id}': blackboard slot '{slot}' uses the reserved \
                     synthetic prefix '{prefix}' — that namespace is owned by the \
                     runtime (SPEC §33 D6 for `_llm.*`, SPEC §29 for `_fire_count.*`, \
                     SPEC §26 for `_while_iter.*`). Rename the slot."
                )));
                // Only emit one diagnostic per slot even if it matches
                // multiple prefixes (the prefix list is non-overlapping
                // in practice, but defensive).
                break;
            }
        }
    };

    match bb {
        Value::Array(arr) => {
            for v in arr {
                if let Some(s) = v.as_str() {
                    visit(s, out);
                }
            }
        }
        Value::Object(obj) => {
            for k in obj.keys() {
                visit(k, out);
            }
        }
        _ => {}
    }
}

/// Extract declared blackboard slot names from a workflow def. Returns
/// `Some(set)` only when `blackboard:` is present — `None` means "no
/// declaration; skip the read-side declared-slot check entirely" so configs
/// without a blackboard remain compatible (SPEC §14).
fn declared_blackboard_slots(def: &Value) -> Option<HashSet<String>> {
    let bb = def.get("blackboard")?;
    let set: HashSet<String> = match bb {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Value::Object(obj) => obj.keys().cloned().collect(),
        _ => return None,
    };
    Some(set)
}

/// Build per-state writers_into via a fixed-point over the reachable subgraph.
/// `writers_into[S]` = union over every reachable path from initial to S of
/// the slots written by initialContext + every transition output: on that path.
fn compute_writers_into(
    def: &Value,
    states: &serde_json::Map<String, Value>,
    initial_state: &str,
) -> HashMap<String, HashSet<String>> {
    let mut writers: HashMap<String, HashSet<String>> = HashMap::new();

    // Seed: declared `inputs:` + `initialContext` keys + any onEnter output on
    // the initial state are available before the first guard fires. Inputs are
    // included because the runtime seeds every declared input (with its schema
    // default) into `$.context` at start (input→context seeding poka-yoke), so a
    // guard/template/output reading `$.context.<input>` IS reachable — this
    // keeps the static use-before-def analysis consistent with that runtime
    // behavior (and with V13, which already treats inputs as `$.context` slots).
    let mut seed: HashSet<String> = def
        .pointer("/inputs")
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(ctx) = def.get("initialContext").and_then(Value::as_object) {
        seed.extend(ctx.keys().cloned());
    }
    if let Some(state) = states.get(initial_state) {
        if let Some(on_enter_out) = state.pointer("/onEnter/output").and_then(Value::as_object) {
            seed.extend(on_enter_out.keys().cloned());
        }
    }
    writers.insert(initial_state.to_string(), seed);

    // Propagate to a fixed point. Worst case O(|states| * |transitions|),
    // bounded by tens-to-hundreds of states in practice — no need for a
    // worklist-style optimisation.
    let timeout_target = def
        .pointer("/onTimeout/target")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut changed = true;
    while changed {
        changed = false;
        for (state_name, state_def) in states {
            let Some(state_writers) = writers.get(state_name).cloned() else {
                continue;
            };

            // SPEC §9: onTimeout is reachable from EVERY state. Whatever the
            // current state has accumulated, the timeout target can see — so
            // we propagate state_writers (plus the target's onEnter output)
            // into the timeout target as if from every reachable state.
            if let Some(target) = &timeout_target {
                let entry = writers.entry(target.clone()).or_default();
                let mut to_merge = state_writers.clone();
                if let Some(target_state) = states.get(target) {
                    if let Some(on_enter_out) = target_state
                        .pointer("/onEnter/output")
                        .and_then(Value::as_object)
                    {
                        to_merge.extend(on_enter_out.keys().cloned());
                    }
                }
                for key in to_merge {
                    if entry.insert(key) {
                        changed = true;
                    }
                }
            }

            let Some(ts) = state_def.get("transitions").and_then(Value::as_object) else {
                continue;
            };
            for (_t_name, t_def) in ts {
                let mut produced = state_writers.clone();
                if let Some(output) = t_def.get("output").and_then(Value::as_object) {
                    produced.extend(output.keys().cloned());
                }
                let mut targets: Vec<&str> = Vec::new();
                if let Some(target) = t_def.get("target").and_then(Value::as_str) {
                    targets.push(target);
                }
                if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                    for branch in branches {
                        if let Some(bt) = branch.get("target").and_then(Value::as_str) {
                            targets.push(bt);
                        }
                    }
                }
                for target in targets {
                    let entry = writers.entry(target.to_string()).or_default();
                    let mut to_merge = produced.clone();
                    // Add this state's own onEnter output (visible to any
                    // guard leaving the target state).
                    if let Some(target_state) = states.get(target) {
                        if let Some(on_enter_out) = target_state
                            .pointer("/onEnter/output")
                            .and_then(Value::as_object)
                        {
                            to_merge.extend(on_enter_out.keys().cloned());
                        }
                    }
                    for key in to_merge {
                        if entry.insert(key) {
                            changed = true;
                        }
                    }
                }
            }
        }
    }
    writers
}

fn validate_guard_kinds(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(states) = def.get("states").and_then(Value::as_object) else {
        return;
    };
    for (state_name, state_def) in states {
        let Some(ts) = state_def.get("transitions").and_then(Value::as_object) else {
            continue;
        };
        for (t_name, t_def) in ts {
            if let Some(guards) = t_def.get("guards").and_then(Value::as_array) {
                for guard in guards {
                    check_guard_kind_recursive(id, state_name, t_name, guard, out);
                }
            }
            if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                for branch in branches {
                    if let Some(when) = branch.get("when") {
                        check_guard_kind_recursive(id, state_name, t_name, when, out);
                    }
                }
            }
        }
    }
}

fn check_guard_kind_recursive(
    id: &str,
    state_name: &str,
    t_name: &str,
    guard: &Value,
    out: &mut Vec<Diagnostic>,
) {
    let raw_kind = guard.get("kind").and_then(Value::as_str).unwrap_or("");
    // Single source of truth: `GuardKind::from_token` defined alongside the
    // runtime evaluator. Adding a kind there automatically makes it
    // valid here; removing one without adding here is a compile error
    // (the runtime match becomes non-exhaustive).
    let Some(kind) = crate::guards::GuardKind::from_token(raw_kind) else {
        let shown = if raw_kind.is_empty() {
            "<missing>"
        } else {
            raw_kind
        };
        let valid = crate::guards::GuardKind::ALL
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        out.push(Diagnostic::Error(format!(
            "workflow '{id}': transition '{t_name}' in state '{state_name}' \
             declares guard with invalid kind '{shown}' (valid: {valid})"
        )));
        return;
    };
    // SPEC §9 — operand presence. The runtime evaluator coalesces a
    // missing/typo'd operand to `""` (permission/role) or `false`
    // (expr/subject) which silently denies — or, for an empty-string
    // grant, silently passes. Reject at load so the author sees the
    // mistake before any workflow runs.
    let require_operand = |field: &str, kind_label: &str, out: &mut Vec<Diagnostic>| {
        let present = guard
            .get(field)
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !present {
            out.push(Diagnostic::Error(format!(
                "INVALID_GUARD_OPERAND: workflow '{id}': transition '{t_name}' in state \
                 '{state_name}' declares a '{kind_label}' guard missing a non-empty `{field}:` \
                 operand (a missing/empty operand silently denies at runtime)"
            )));
        }
    };
    match kind {
        crate::guards::GuardKind::Permission => require_operand("permission", "permission", out),
        crate::guards::GuardKind::Role => require_operand("role", "role", out),
        crate::guards::GuardKind::GuidanceAcknowledged => {
            require_operand("subject", "guidance_acknowledged", out)
        }
        crate::guards::GuardKind::ScriptAcknowledged => {
            require_operand("subject", "script_acknowledged", out)
        }
        crate::guards::GuardKind::Expr | crate::guards::GuardKind::Jsonpath => {
            let raw_expr = guard.get("expr").and_then(Value::as_str).unwrap_or("");
            if raw_expr.is_empty() {
                out.push(Diagnostic::Error(format!(
                    "INVALID_GUARD_OPERAND: workflow '{id}': transition '{t_name}' in state \
                     '{state_name}' declares an 'expr' guard missing a non-empty `expr:` operand \
                     (a missing/empty expr silently denies at runtime)"
                )));
            } else if !crate::guards::expr_parses(raw_expr) {
                out.push(Diagnostic::Error(format!(
                    "INVALID_GUARD_EXPR: workflow '{id}': transition '{t_name}' in state \
                     '{state_name}' declares an 'expr' guard whose `expr:` ('{raw_expr}') is not a \
                     parseable binary comparison (e.g. `$.context.x == \"y\"`); it would silently \
                     evaluate to false at runtime"
                )));
            }
        }
        _ => {}
    }

    // Recurse into composite kinds so a typo nested inside `all_of` /
    // `any_of` / `not` still surfaces.
    match kind {
        crate::guards::GuardKind::AllOf | crate::guards::GuardKind::AnyOf => {
            if let Some(inner) = guard.get("guards").and_then(Value::as_array) {
                for g in inner {
                    check_guard_kind_recursive(id, state_name, t_name, g, out);
                }
            }
        }
        crate::guards::GuardKind::Not => {
            if let Some(inner) = guard.get("guard") {
                check_guard_kind_recursive(id, state_name, t_name, inner, out);
            }
        }
        _ => {}
    }
}

fn collect_guards(guards: Option<&Value>) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(arr) = guards.and_then(Value::as_array) {
        for g in arr {
            collect_guards_into(g, &mut out);
        }
    }
    out
}

fn collect_guards_into(guard: &Value, out: &mut Vec<Value>) {
    match guard.get("kind").and_then(Value::as_str) {
        Some("all_of") | Some("any_of") => {
            if let Some(inner) = guard.get("guards").and_then(Value::as_array) {
                for g in inner {
                    collect_guards_into(g, out);
                }
            }
        }
        Some("not") => {
            if let Some(inner) = guard.get("guard") {
                collect_guards_into(inner, out);
            }
        }
        _ => out.push(guard.clone()),
    }
}

/// Extract slot names from `$.context.X` paths inside an expression. Conservative
/// regex-free scan — collects identifier-shaped suffixes after each `$.context.`.
fn extract_expr_context_slots(expr: &str) -> Vec<String> {
    extract_context_slots_from(expr)
}

/// Extract slot names from `{{ $.context.X }}` templates in a string.
fn extract_template_context_slots(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find closing `}}`.
            if let Some(end) = find_subslice(&bytes[i + 2..], b"}}") {
                let inner = &text[i + 2..i + 2 + end];
                out.extend(extract_context_slots_from(inner));
                i += 2 + end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    for i in 0..=hay.len() - needle.len() {
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

fn extract_context_slots_from(text: &str) -> Vec<String> {
    const PREFIX: &str = "$.context.";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(idx) = rest.find(PREFIX) {
        let after = &rest[idx + PREFIX.len()..];
        let slot: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !slot.is_empty() {
            out.push(slot);
        }
        rest = &after[after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len())..];
    }
    out
}

/// Phase 6: SPEC §5.5, §11 — skills references resolve to a declared fragment;
/// more than ~4 refs at one scope warns.
fn check_skills_refs(
    id: &str,
    def: &Value,
    states: &serde_json::Map<String, Value>,
    skill_subjects: &HashSet<&str>,
    out: &mut Vec<Diagnostic>,
) {
    const REF_WARN_THRESHOLD: usize = 4;

    // SPEC §9 — a workflow loaded via a `repos:` manifest has an id of
    // the form `<ns>/<stem>`. A bare-subject skill reference like
    // `plan.draft` MAY resolve either to the bare key `plan.draft` OR
    // to the namespace-prefixed `<ns>/plan.draft` — same fall-through
    // pattern PR1 uses for `kind: workflow` references. Pre-compute the
    // workflow's own namespace here so the per-entry check stays O(1).
    let own_namespace: Option<&str> = id.split_once('/').map(|(ns, _)| ns);

    let mut check_scope = |scope: &str, refs: &Value| {
        let Some(arr) = refs.as_array() else { return };
        if arr.len() > REF_WARN_THRESHOLD {
            out.push(Diagnostic::Warning(format!(
                "workflow '{id}': {scope} surfaces {n} skills refs — the menu is itself payload, \
                 consider trimming to ≤{REF_WARN_THRESHOLD}",
                n = arr.len()
            )));
        }
        for entry in arr {
            let Some(subject) = entry.as_str() else {
                continue;
            };
            // Direct match first (bare subject, OR already-prefixed).
            if skill_subjects.contains(subject) {
                continue;
            }
            // Fall through: try the workflow's own-namespace prefix.
            if let Some(ns) = own_namespace {
                let prefixed = format!("{}/{}", ns, subject);
                if skill_subjects.contains(prefixed.as_str()) {
                    continue;
                }
            }
            out.push(Diagnostic::Error(format!(
                "workflow '{id}': {scope} references skills entry '{subject}' \
                 which is not declared in the top-level `skills:` library (SPEC §11)"
            )));
        }
    };

    if let Some(refs) = def.get("skills") {
        check_scope("workflow scope", refs);
    }
    for (state_name, state_def) in states {
        if let Some(refs) = state_def.get("skills") {
            check_scope(&format!("state '{state_name}'"), refs);
        }
        if let Some(ts) = state_def.get("transitions").and_then(Value::as_object) {
            for (t_name, t_def) in ts {
                if let Some(refs) = t_def.get("skills") {
                    check_scope(
                        &format!("transition '{t_name}' in state '{state_name}'"),
                        refs,
                    );
                }
            }
        }
    }
}

/// SPEC §5.1, V3/V4/V5 — capability workflows MUST declare a `snippet:`
/// block with `inputs:` AND `outputs:` keys. Each schema entry must be a
/// JSON object (the runtime later validates against the embedded schema
/// via `jsonschema::validator_for`; here we only insist on shape so V17's
/// runtime check has well-formed material to work with).
fn validate_snippet(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(snippet) = def.get("snippet") else {
        out.push(Diagnostic::Error(format!(
            "MISSING_SNIPPET: capability '{id}' is missing required `snippet:` block \
             (SPEC §5.1, V3)"
        )));
        return;
    };
    let Some(snip_obj) = snippet.as_object() else {
        out.push(Diagnostic::Error(format!(
            "INVALID_SNIPPET: capability '{id}' `snippet:` must be an object (SPEC §5.1, V4)"
        )));
        return;
    };
    for key in ["inputs", "outputs"] {
        let Some(block) = snip_obj.get(key) else {
            out.push(Diagnostic::Error(format!(
                "INVALID_SNIPPET: capability '{id}' `snippet:` is missing required \
                 `{key}:` key (may be `{{}}` — but must be present) (SPEC §5.1, V4)"
            )));
            continue;
        };
        let Some(entries) = block.as_object() else {
            out.push(Diagnostic::Error(format!(
                "INVALID_SNIPPET: capability '{id}' `snippet.{key}` must be a mapping \
                 (SPEC §5.1, V4)"
            )));
            continue;
        };
        for (name, schema) in entries {
            if !schema.is_object() {
                out.push(Diagnostic::Error(format!(
                    "INVALID_SNIPPET: capability '{id}' `snippet.{key}.{name}` must be a \
                     JSON-schema-shaped object (SPEC §5.1, V5)"
                )));
            }
        }
    }
}

/// SPEC §6.1, V12 — walk every transition; for any `kind: workflow`
/// executor targeting a `cap.*` definition, require a `use:` block;
/// validate its shape. Also enforces the `host_path → cap_output_name`
/// shape baked into `expand_use_bindings`.
/// A single executor block in a workflow definition, with the context the
/// governance validators and load-time doctors need to attribute a finding.
///
/// CMP-003 — covers EVERY executor site, not just transitions: the
/// workflow-level `onEnter` executor, each state's `onEnter` executor, AND
/// each transition's executor. The cross-workflow rules (V10/V11/V12/V15/V16)
/// and the `kind`/cost/llm load-time doctors all walk through here, so an
/// executor placed on `onEnter` can no longer slip past a check that only
/// inspected transitions.
pub struct ExecutorSite<'a> {
    /// Owning state, or `None` for a workflow-level `onEnter` executor.
    pub state: Option<&'a str>,
    /// Owning transition, or `None` for an `onEnter` executor.
    pub transition: Option<&'a str>,
    /// The executor block (guaranteed to be a JSON object).
    pub executor: &'a Value,
    /// The block that OWNS the executor — the transition object, or the
    /// `onEnter` block. Carries sibling keys an inspector may need (e.g.
    /// `output:` for cap-slot mapping checks) without a second lookup.
    pub owner: &'a Value,
    /// Pre-rendered location phrase for diagnostics. For a transition this is
    /// exactly `state '<s>' transition '<t>'` (so existing messages are
    /// byte-for-byte unchanged); onEnter sites read `state '<s>' onEnter`
    /// or `workflow onEnter`.
    pub location: String,
}

/// Visit every executor site in a single workflow definition — workflow-level
/// `onEnter`, each state's `onEnter`, and every transition. See [`ExecutorSite`].
pub fn for_each_executor_site(def: &Value, mut f: impl FnMut(&ExecutorSite)) {
    if let Some(owner) = def.pointer("/onEnter") {
        if let Some(exec) = owner.pointer("/executor").filter(|e| e.is_object()) {
            f(&ExecutorSite {
                state: None,
                transition: None,
                executor: exec,
                owner,
                location: "workflow onEnter".to_string(),
            });
        }
    }
    let Some(states) = def.pointer("/states").and_then(Value::as_object) else {
        return;
    };
    for (state_name, state_def) in states {
        if let Some(owner) = state_def.pointer("/onEnter") {
            if let Some(exec) = owner.pointer("/executor").filter(|e| e.is_object()) {
                f(&ExecutorSite {
                    state: Some(state_name),
                    transition: None,
                    executor: exec,
                    owner,
                    location: format!("state '{state_name}' onEnter"),
                });
            }
        }
        let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object) else {
            continue;
        };
        for (t_name, t_def) in transitions {
            if let Some(exec) = t_def.pointer("/executor").filter(|e| e.is_object()) {
                f(&ExecutorSite {
                    state: Some(state_name),
                    transition: Some(t_name),
                    executor: exec,
                    owner: t_def,
                    location: format!("state '{state_name}' transition '{t_name}'"),
                });
            }
        }
    }
}

fn validate_use_bindings(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    for_each_executor_site(def, |site| {
        let exec = site.executor;
        if exec.get("kind").and_then(Value::as_str) != Some("workflow") {
            return;
        }
        let target_def_id = exec.get("definitionId").and_then(Value::as_str);
        let targets_capability = target_def_id
            .map(|d| matches!(Tier::from_id(d), Tier::Cap))
            .unwrap_or(false);
        let has_use = exec.get("use").is_some();
        if targets_capability && !has_use {
            out.push(Diagnostic::Error(format!(
                "MISSING_USE: workflow '{id}' {} invokes capability '{}' via \
                 `kind: workflow` without a `use:` block. Capability invocations \
                 require a typed use-binding (SPEC §6.1, V12).",
                site.location,
                target_def_id.unwrap_or("?")
            )));
            return;
        }
        if let Some(use_val) = exec.get("use") {
            validate_use_block_shape(id, &site.location, use_val, out);
        }
    });
}

fn validate_use_block_shape(id: &str, location: &str, use_val: &Value, out: &mut Vec<Diagnostic>) {
    let Some(obj) = use_val.as_object() else {
        out.push(Diagnostic::Error(format!(
            "INVALID_USE: workflow '{id}' {location} \
             `use:` must be a mapping (SPEC §6.1, V12)"
        )));
        return;
    };
    for key in ["inputs", "outputs"] {
        let Some(block) = obj.get(key) else {
            // inputs OR outputs may be omitted only when literally empty — but
            // since the runtime treats absence as `{}`, we accept either.
            continue;
        };
        if !block.is_object() {
            out.push(Diagnostic::Error(format!(
                "INVALID_USE: workflow '{id}' {location} \
                 `use.{key}` must be a mapping (SPEC §6.1, V12)"
            )));
        }
    }
    // V12 (shape half): every use.outputs value must be a string naming a
    // capability output, every key must match `$.context.<simple-name>`.
    if let Some(outputs) = obj.get("outputs").and_then(Value::as_object) {
        for (host_path, cap_name) in outputs {
            if cap_name.as_str().is_none() {
                out.push(Diagnostic::Error(format!(
                    "INVALID_USE_OUTPUT_VALUE: workflow '{id}' {location} \
                     use.outputs[{host_path}] must be a string \
                     naming a capability output (SPEC §6.1, V12)"
                )));
            }
            if !host_path_tail_ok(host_path) {
                out.push(Diagnostic::Error(format!(
                    "INVALID_USE_OUTPUT_PATH: workflow '{id}' {location} \
                     use.outputs key '{host_path}' must match \
                     `^\\$\\.context\\.[a-z][a-z0-9_-]*$` — v0.6 projects only to \
                     single-segment context slots (SPEC §6.1, V12)"
                )));
            }
        }
    }
}

/// Mirror of `crate::config::host_path_tail` — accept iff the path matches
/// `^\$\.context\.[a-z][a-z0-9_-]*$`. Kept private to validate.rs so
/// callers can't accidentally couple to one of the two implementations.
fn host_path_tail_ok(host_path: &str) -> bool {
    let Some(tail) = host_path.strip_prefix("$.context.") else {
        return false;
    };
    if tail.is_empty() || tail.contains('.') || tail.contains('/') {
        return false;
    }
    let mut chars = tail.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

// ============================================================================
// Capability tier rules (Tier::Cap)
// ============================================================================

/// V1 — `verb:` on a capability MUST be one of the 24 closed-cloud tokens.
fn v1_verb_in_cloud(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(verb_str) = def.get("verb").and_then(Value::as_str) else {
        out.push(Diagnostic::Error(format!(
            "MISSING_VERB: capability '{id}' is missing required `verb:` field; \
             allowed verbs are {BLESSED_CAP_VERBS:?} (SPEC §4, V1)"
        )));
        return;
    };
    if CapVerb::from_token(verb_str).is_none() {
        out.push(Diagnostic::Error(format!(
            "INVALID_VERB: capability '{id}' has verb '{verb_str}'; allowed verbs are \
             {BLESSED_CAP_VERBS:?} (SPEC §4, V1)"
        )));
    }
}

/// V2 — capability id stem must be `cap.<verb>.<name>`. Namespace prefix
/// (`swe/`) is stripped before the check.
fn v2_id_matches_verb_name(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let stem = id.rsplit('/').next().unwrap_or(id);
    let parts: Vec<&str> = stem.split('.').collect();
    // `cap.<verb>.<name...>` → at least 3 segments. Allow longer names
    // (`cap.plan.specify.change-request`) by treating everything from
    // index 2 onward as the name body.
    if parts.len() < 3 || parts[0] != "cap" {
        out.push(Diagnostic::Error(format!(
            "INVALID_ID_SHAPE: capability '{id}' must match `cap.<verb>.<name>` \
             (SPEC §4, V2)"
        )));
        return;
    }
    let id_verb = parts[1];
    // The verb-in-cloud check is V1; here we only assert that the id's
    // verb segment matches the declared `verb:` field. If `verb:` is
    // absent or unrecognized, V1 already fired — silently skip to avoid
    // double-reporting.
    if let Some(declared_verb) = def.get("verb").and_then(Value::as_str) {
        if declared_verb != id_verb {
            out.push(Diagnostic::Error(format!(
                "ID_VERB_MISMATCH: capability '{id}' declares verb '{declared_verb}' but \
                 its id stem uses verb segment '{id_verb}' — must agree (SPEC §4, V2)"
            )));
        }
    }
}

/// V6 — primary-executor verb-shape check. Inspects the executor on the
/// transition leaving the capability's initial state (TRIZ Local Quality:
/// narrow check that catches gross misuse without walking every transition).
fn v6_primary_executor_verb_shape(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(verb_str) = def.get("verb").and_then(Value::as_str) else {
        return; // V1 already flagged
    };
    let Some(verb) = CapVerb::from_token(verb_str) else {
        return; // V1 already flagged
    };
    let Some(initial_state) = def.get("initialState").and_then(Value::as_str) else {
        return; // generic missing-initialState error fired elsewhere
    };
    let Some(transitions) = def
        .pointer(&format!(
            "/states/{}/transitions",
            pointer_escape(initial_state)
        ))
        .and_then(Value::as_object)
    else {
        return;
    };
    if transitions.is_empty() {
        // A capability with no outgoing transitions is a different issue;
        // V6 has nothing to constrain.
        return;
    }

    // Find at least one primary transition whose executor kind matches.
    let mut primary_kinds: Vec<&str> = Vec::new();
    let mut has_human_actor = false;
    for (_t_name, t_def) in transitions {
        if let Some(kind) = t_def.pointer("/executor/kind").and_then(Value::as_str) {
            primary_kinds.push(kind);
        }
        if t_def.get("actor").and_then(Value::as_str) == Some("human")
            || t_def.get("purpose").and_then(Value::as_str) == Some("ask")
        {
            has_human_actor = true;
        }
    }

    let category = verb.category();
    let ok = match category {
        CapVerbCategory::Cognitive => primary_kinds.iter().any(|k| matches!(*k, "mcp" | "noop")),
        CapVerbCategory::Deterministic => {
            primary_kinds.iter().any(|k| matches!(*k, "script" | "mcp"))
        }
        CapVerbCategory::Coordination => match verb {
            CapVerb::Gate => has_human_actor,
            // Spec §4.1 ideal for `coordinate` is `kind: mcp` AND
            // connection `external: true`. PR3 enforces only the
            // `kind: mcp` half (no `external:` field exists yet);
            // documented gap, CHANGELOG entry for v0.7 follow-up.
            CapVerb::Coordinate => primary_kinds.contains(&"mcp"),
            _ => true,
        },
    };
    if !ok {
        let allowed = match category {
            CapVerbCategory::Cognitive => "kind: mcp OR kind: noop (skill-surfacing)",
            CapVerbCategory::Deterministic => "kind: script OR kind: mcp",
            CapVerbCategory::Coordination => match verb {
                CapVerb::Gate => {
                    "at least one initial-state transition with actor: human OR purpose: ask"
                }
                CapVerb::Coordinate => "kind: mcp",
                _ => "?",
            },
        };
        out.push(Diagnostic::Error(format!(
            "INVALID_PRIMARY_EXECUTOR: capability '{id}' (verb '{verb_str}', category \
             {category:?}) has initial-state transitions whose primary executor kinds \
             are {primary_kinds:?}; expected {allowed} (SPEC §4.1, V6)"
        )));
    }
}

/// V10 — capabilities MUST NOT invoke other workflows via `kind: workflow`
/// (the no-nesting rule). Capabilities are leaves of the composition tree.
fn v10_capability_does_not_invoke_workflow(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    if let Some(target) = find_first_workflow_invocation(def) {
        out.push(Diagnostic::Error(format!(
            "CAPABILITY_NESTING: capability '{id}' invokes workflow '{target}' via \
             `kind: workflow`. Capabilities are composition leaves; only flows \
             may invoke other workflows (SPEC §3, V10)"
        )));
    }
}

// ============================================================================
// Flow tier rules (Tier::Flow)
// ============================================================================

/// V7 — flow id stem must match `flow.<name>`.
fn v7_id_matches_flow_pattern(id: &str, _def: &Value, out: &mut Vec<Diagnostic>) {
    let stem = id.rsplit('/').next().unwrap_or(id);
    let parts: Vec<&str> = stem.split('.').collect();
    if parts.len() < 2 || parts[0] != "flow" || parts[1].is_empty() {
        out.push(Diagnostic::Error(format!(
            "INVALID_ID_SHAPE: flow '{id}' must match `flow.<name>` (SPEC §3, V7)"
        )));
    }
}

/// V8 — flows MUST NOT declare a `snippet:` block.
fn v8_flow_has_no_snippet(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    if def.get("snippet").is_some() {
        out.push(Diagnostic::Error(format!(
            "FLOW_HAS_SNIPPET: flow '{id}' declares a `snippet:` block; \
             snippets are capability-only — flows are not externally invokable \
             as snippets (SPEC §5.1, V8)"
        )));
    }
}

/// V9 — flows MUST NOT declare a `verb:` field.
fn v9_flow_has_no_verb(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    if def.get("verb").is_some() {
        out.push(Diagnostic::Error(format!(
            "FLOW_HAS_VERB: flow '{id}' declares `verb:`; verbs are \
             capability-only (SPEC §4, V9)"
        )));
    }
}

/// Walk every executor site (incl. onEnter — CMP-003) and return the first
/// `kind: workflow` `definitionId:` found. Helper for V10's no-nesting check.
fn find_first_workflow_invocation(def: &Value) -> Option<String> {
    let mut found: Option<String> = None;
    for_each_executor_site(def, |site| {
        if found.is_some() {
            return;
        }
        if site.executor.get("kind").and_then(Value::as_str) == Some("workflow") {
            if let Some(target) = site.executor.get("definitionId").and_then(Value::as_str) {
                found = Some(target.to_string());
            }
        }
    });
    found
}

// ============================================================================
// Cross-tier contract-hash rules
// ============================================================================

/// V15 / V16 — `expects_contract_hash:` validation, walked across every
/// `kind: workflow` invocation. V15 fires when an explicit pin doesn't
/// match the target capability's computed contract hash; V16 fires when
/// SPEC §6.2 — a capability's `lifecycle:` token must be one of the closed
/// [`crate::discovery::Lifecycle`] set. Mirrors how `validate_skills`
/// validates `Lifecycle::from_token`. Emitting an error here (rather than
/// coercing a typo to `experimental` as `cap_lifecycles` does) is what stops
/// a stable cap that fat-fingered its lifecycle from silently skipping V16.
fn validate_cap_lifecycle(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(token) = def.get("lifecycle").and_then(Value::as_str) else {
        // H11b: `lifecycle:` is REQUIRED on capabilities. Absence MUST be an
        // error, not a silent `experimental` default — because `experimental`
        // is the one lifecycle that EXEMPTS callers from the V16
        // contract-hash-pin requirement. A `stable` cap that forgot to
        // declare its lifecycle would otherwise silently disable the
        // supply-chain pin check (SPEC §6.2).
        out.push(Diagnostic::Error(format!(
            "MISSING_LIFECYCLE: capability '{id}' is missing a required `lifecycle:` field \
             (valid: {}). A missing lifecycle would otherwise default to `experimental` and \
             silently skip the V16 contract-hash pin requirement.",
            crate::discovery::Lifecycle::ALL_TOKENS.join(", ")
        )));
        return;
    };
    if crate::discovery::Lifecycle::from_token(token).is_none() {
        out.push(Diagnostic::Error(format!(
            "INVALID_LIFECYCLE: capability '{id}' declares unknown `lifecycle: \"{token}\"` \
             (valid: {})",
            crate::discovery::Lifecycle::ALL_TOKENS.join(", ")
        )));
    }
}

/// The optional `process` / `taskClass` tag marks a flow as an auto-selectable
/// *process* for the intent index. It is deliberately an OPEN vocabulary (a flow
/// per industry) — closed-set matching of an *intent* to a process is the
/// runtime classifier's job — but the tag must be well-formed, and a tagged flow
/// MUST declare what "done" means: a process-tagged flow with no `outcomes`
/// "succeeds" vacuously, which would let the intent index learn it as a winner
/// (the R3 reward-gaming guard). Both are author-time, fail-fast.
fn v_process_metadata(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(raw) = def.get("process").or_else(|| def.get("taskClass")) else {
        return; // optional
    };
    let tc = match raw.as_str() {
        Some(s) if !s.trim().is_empty() => s,
        _ => {
            out.push(Diagnostic::Error(format!(
                "INVALID_PROCESS: flow '{id}' declares a malformed `process:` — it must be a \
                 non-empty task-class string (e.g. \"engineering\", \"research\")."
            )));
            return;
        }
    };
    let has_outcome = def
        .get("outcomes")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    if !has_outcome {
        out.push(Diagnostic::Error(format!(
            "PROCESS_REQUIRES_OUTCOME: flow '{id}' is process-tagged ('{tc}') and so is \
             auto-selectable, but declares no `outcomes:`. A zero-outcome process succeeds \
             vacuously and would game the intent index — declare at least one outcome that \
             defines what meeting the intent means."
        )));
    }
}

/// a stable-lifecycle target is invoked without any pin.
fn validate_contract_hash_pins(
    id: &str,
    def: &Value,
    ctx: &ValidationCtx<'_>,
    out: &mut Vec<Diagnostic>,
) {
    for_each_executor_site(def, |site| {
        let exec = site.executor;
        if exec.get("kind").and_then(Value::as_str) != Some("workflow") {
            return;
        }
        let Some(target_id) = exec.get("definitionId").and_then(Value::as_str) else {
            return;
        };
        let Some(actual_hash) = ctx.cap_contract_hashes.get(target_id) else {
            return; // target isn't a snippet-bearing cap; nothing to pin
        };
        let declared_pin = exec.get("expects_contract_hash").and_then(Value::as_str);
        // H11b: a snippet-bearing cap target with NO lifecycle entry is a
        // declaration defect (its own validation emits MISSING_LIFECYCLE).
        // We must NOT silently treat it as `experimental` here — that is
        // precisely the path that exempts a forgotten-lifecycle `stable` cap
        // from the V16 pin requirement. Skip the pin check for this site (the
        // target's own MISSING_LIFECYCLE error already fails the config); a
        // mismatched *explicit* pin (V15) is still checked below regardless.
        let Some(lifecycle) = ctx.cap_lifecycles.get(target_id).map(String::as_str) else {
            if let Some(pin) = declared_pin {
                if pin != actual_hash {
                    out.push(Diagnostic::Error(format!(
                        "CONTRACT_HASH_MISMATCH: workflow '{id}' {} pins capability \
                         '{target_id}' to `{pin}` but the loaded contract hash is \
                         `{actual_hash}` (SPEC §6.2, V15)",
                        site.location
                    )));
                }
            }
            return;
        };
        match (declared_pin, lifecycle) {
            (Some(pin), _) if pin != actual_hash => {
                out.push(Diagnostic::Error(format!(
                    "CONTRACT_HASH_MISMATCH: workflow '{id}' {} pins capability \
                     '{target_id}' to `{pin}` but the loaded contract hash is \
                     `{actual_hash}` (SPEC §6.2, V15)",
                    site.location
                )));
            }
            (None, "stable") => {
                out.push(Diagnostic::Error(format!(
                    "MISSING_CONTRACT_HASH: workflow '{id}' {} invokes \
                     stable-lifecycle capability '{target_id}' without \
                     `expects_contract_hash:`. Add: expects_contract_hash: \
                     \"{actual_hash}\" (SPEC §6.2, V16)",
                    site.location
                )));
            }
            _ => {}
        }
    });
}

// ============================================================================
// Slot-table rules (V13, V14) — flow-only
// ============================================================================

/// V13/V14 — build the flow's slot table, then check every
/// `use:.inputs` reference for reachability against it. V14 (type
/// consistency between two states writing the same path) is enforced
/// inside [`crate::slot_table::build_slot_table`] and surfaces here as
/// part of the returned diagnostic list.
fn v13_v14_slot_table(id: &str, def: &Value, ctx: &ValidationCtx<'_>, out: &mut Vec<Diagnostic>) {
    let table = match crate::slot_table::build_slot_table(def, ctx.cap_snippet_outputs) {
        Ok(t) => t,
        Err(diagnostics) => {
            out.extend(diagnostics);
            return;
        }
    };

    for_each_executor_site(def, |site| {
        let Some(use_inputs) = site
            .executor
            .pointer("/use/inputs")
            .and_then(Value::as_object)
        else {
            return;
        };
        let state_name = site.state.unwrap_or("");
        let t_name = site.transition.unwrap_or("onEnter");
        for (_input_name, expr_value) in use_inputs {
            let Some(expr) = expr_value.as_str() else {
                continue;
            };
            if !expr.starts_with("$.context.") {
                // Non-context references (literals, $.workflow.input.*,
                // $.arguments.*) bypass the slot table — they don't
                // need to resolve through state writes.
                continue;
            }
            if let Some(d) =
                crate::slot_table::assert_reachable(&table, expr, id, state_name, t_name)
            {
                out.push(d);
            }
        }
    });
}

// ============================================================================
// Helpers
// ============================================================================

// ============================================================================
// FALLBACK-05 — output operator-object shape validation
// ============================================================================

/// Walk every mapping object that `mapping::resolve_value` will evaluate
/// at runtime — transition `output:`, `onEnter.output:`, and transition
/// `prefill:` — and reject malformed operator objects (`{ add: [..] }`,
/// `{ concat: [..] }`, etc.) whose arity/operand types would otherwise be
/// silently coerced to `Value::Null` and written to the blackboard.
///
/// Mirrors the operator semantics in `mapping::resolve_value` /
/// `resolve_operands`:
/// - `add|subtract|multiply|divide`: operand MUST be a 2-element array;
///   each element MUST be a path string (`$.…`) or a number. A non-`$.`
///   string or non-numeric literal is rejected (it resolves to neither a
///   path nor a number → runtime Null).
/// - `concat`: operand MUST be an array.
/// - `set`: any value is accepted (literal pass-through).
fn validate_output_operator_shapes(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(states) = def.get("states").and_then(Value::as_object) else {
        return;
    };
    for (state_name, state_def) in states {
        // onEnter.output
        if let Some(map) = state_def
            .pointer("/onEnter/output")
            .and_then(Value::as_object)
        {
            check_mapping_object(id, state_name, "onEnter.output", map, out);
        }
        let Some(ts) = state_def.get("transitions").and_then(Value::as_object) else {
            continue;
        };
        for (t_name, t_def) in ts {
            if let Some(map) = t_def.get("output").and_then(Value::as_object) {
                check_mapping_object(
                    id,
                    state_name,
                    &format!("transition '{t_name}' output"),
                    map,
                    out,
                );
            }
            if let Some(map) = t_def.get("prefill").and_then(Value::as_object) {
                check_mapping_object(
                    id,
                    state_name,
                    &format!("transition '{t_name}' prefill"),
                    map,
                    out,
                );
            }
        }
    }
}

/// The fixed fan-in envelope a `parallel` executor hands its aggregator
/// (`compute_verdict`, `aggregator_input`): the branch results plus the four
/// aggregate counts. The reduce (aggregator) can only consume these top-level
/// fields — every worker output lives nested under `branches[].output`.
const PARALLEL_FANIN_FIELDS: [&str; 5] = [
    "branches",
    "ok_count",
    "failed_count",
    "cancelled_count",
    "n",
];

/// Spec A §7.1 — the fan-in composition check (the load-time half of the typed
/// parallel edges). For every `kind: parallel` transition whose reduce is a
/// declared aggregator with an `inputSchema`, prove that the reduce **consumes
/// only what the map produces**:
///
/// 1. **Envelope coverage** — every top-level field the aggregator `inputSchema`
///    marks `required` must be one of the fixed fan-in envelope fields
///    (`branches`, `ok_count`, `failed_count`, `cancelled_count`, `n`). A
///    required field outside that set can never be produced by the fan-out, so
///    the map-reduce is mis-wired and must not load.
/// 2. **Per-branch `<slot>Out` coverage** — when the aggregator further
///    constrains the per-branch output (`properties.branches.items.properties.
///    output.required`) AND the map worker declares its produced shape
///    (`branches.do.outputSchema`, inline or a `praxec://hop` `$ref`), every
///    output field the reduce requires must be a declared property of the
///    worker's output. A reduce that reads a field the worker never emits is a
///    load error.
///
/// Honest boundary (§4.5): this proves the reduce's *required* inputs are
/// producible. It cannot prove a shape-valid *wrong-field* mapping — that stays
/// a runtime/review concern. When a contract is absent (no aggregator
/// `inputSchema`, or no worker `outputSchema` for the deeper check) there is
/// nothing to prove and the check is skipped.
fn validate_parallel_edges(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(states) = def.get("states").and_then(Value::as_object) else {
        return;
    };
    for (state_name, state_def) in states {
        let Some(ts) = state_def.get("transitions").and_then(Value::as_object) else {
            continue;
        };
        for (t_name, t_def) in ts {
            let Some(exec) = t_def.get("executor") else {
                continue;
            };
            if exec.get("kind").and_then(Value::as_str) != Some("parallel") {
                continue;
            }
            // The reduce contract lives on the aggregator's `inputSchema`.
            // `join: "all" | "any" | { at_least } | { percent } | { expression }`
            // carry no typed reduce contract — nothing to check.
            let Some(reduce_in) = exec.pointer("/join/aggregator/inputSchema") else {
                continue;
            };

            // 1. Envelope coverage — required top-level reduce fields must be
            //    fan-in envelope fields.
            if let Some(required) = schema_required_fields(reduce_in) {
                for field in required {
                    if !PARALLEL_FANIN_FIELDS.contains(&field.as_str()) {
                        out.push(Diagnostic::Error(format!(
                            "PARALLEL_REDUCE_UNSATISFIED_FIELD: workflow '{id}' state \
                             '{state_name}' transition '{t_name}': the parallel reduce \
                             (`join.aggregator`) requires field '{field}', which the fan-in \
                             envelope never produces (available: [{}]). The reduce must consume \
                             only what the map produces (Spec A §7.1).",
                            PARALLEL_FANIN_FIELDS.join(", ")
                        )));
                    }
                }
            }

            // 2. Per-branch `<slot>Out` coverage — reduce's required per-branch
            //    output fields ⊆ worker's declared output properties.
            let reduce_output_required = reduce_in
                .pointer("/properties/branches/items/properties/output")
                .and_then(schema_required_fields);
            let Some(reduce_output_required) = reduce_output_required else {
                continue;
            };
            let worker_out_props = exec
                .pointer("/branches/do/outputSchema")
                .and_then(schema_property_names);
            let Some(worker_out_props) = worker_out_props else {
                // Worker output shape not declared/resolvable — cannot prove the
                // deeper coverage; honest boundary, skip.
                continue;
            };
            for field in reduce_output_required {
                if !worker_out_props.contains(&field) {
                    out.push(Diagnostic::Error(format!(
                        "PARALLEL_REDUCE_OUTPUT_FIELD_UNPRODUCED: workflow '{id}' state \
                         '{state_name}' transition '{t_name}': the parallel reduce requires \
                         per-branch output field '{field}', but the map worker's `outputSchema` \
                         never produces it. The reduce must consume only what the map produces \
                         (Spec A §7.1)."
                    )));
                }
            }
        }
    }
}

/// The `required` field names of a schema, resolving a `praxec://hop` `$ref`
/// against the shipped vocabulary. `None` when neither an inline `required`
/// array nor a resolvable HOP `$ref` is present (an opaque/unprovable schema).
fn schema_required_fields(schema: &Value) -> Option<Vec<String>> {
    if let Some(def) = schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(crate::hop::hop_ref_def)
    {
        return crate::hop::hop_def_required(def);
    }
    schema.get("required").and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    })
}

/// The declared `properties` field names of a schema, resolving a `praxec://hop`
/// `$ref` against the shipped vocabulary. `None` when the shape is not
/// declared/resolvable (an opaque schema whose fields cannot be proven).
fn schema_property_names(schema: &Value) -> Option<HashSet<String>> {
    if let Some(def) = schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(crate::hop::hop_ref_def)
    {
        return crate::hop::hop_def_properties(def).map(|v| v.into_iter().collect());
    }
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
}

/// Check each spec in a mapping object. `where_` is a human label for the
/// diagnostic (e.g. `"transition 'go' output"`).
fn check_mapping_object(
    id: &str,
    state_name: &str,
    where_: &str,
    map: &serde_json::Map<String, Value>,
    out: &mut Vec<Diagnostic>,
) {
    for (key, spec) in map {
        if let Some(msg) = malformed_operator_reason(spec) {
            out.push(Diagnostic::Error(format!(
                "MALFORMED_OUTPUT_OPERATOR: workflow '{id}' state '{state_name}' {where_} key \
                 '{key}': {msg}. A malformed operator silently resolves to null at runtime \
                 (FALLBACK-05)."
            )));
        }
    }
}

/// Returns `Some(reason)` when `spec` is a recognised operator object with
/// an invalid shape; `None` when the spec is well-formed or not an operator
/// object at all (literal pass-through is always fine).
fn malformed_operator_reason(spec: &Value) -> Option<String> {
    let obj = spec.as_object()?;
    if obj.len() != 1 {
        // Multi-key / empty objects are treated as literal pass-through by
        // resolve_value — not an operator, so not our concern here.
        return None;
    }
    let (op, args) = obj.iter().next().expect("len()==1 checked above");
    match op.as_str() {
        "add" | "subtract" | "multiply" | "divide" => {
            let Some(arr) = args.as_array() else {
                return Some(format!(
                    "operator `{op}` requires a 2-element array operand, got {}",
                    json_type_name(args)
                ));
            };
            if arr.len() != 2 {
                return Some(format!(
                    "operator `{op}` requires exactly 2 operands, got {}",
                    arr.len()
                ));
            }
            for (i, operand) in arr.iter().enumerate() {
                if !is_path_or_number(operand) {
                    return Some(format!(
                        "operator `{op}` operand #{i} must be a `$.` path string or a number, \
                         got {}",
                        json_type_name(operand)
                    ));
                }
            }
            None
        }
        "concat" => {
            if args.as_array().is_none() {
                return Some(format!(
                    "operator `concat` requires an array operand, got {}",
                    json_type_name(args)
                ));
            }
            None
        }
        // `set` accepts any literal; anything else is not an operator object
        // (resolve_value passes it through verbatim).
        _ => None,
    }
}

/// An arithmetic operand is valid iff it is a `$.` path string or a JSON
/// number. (A plain non-`$.` string or any other literal resolves to
/// neither at runtime → silent Null, the defect under test.)
fn is_path_or_number(v: &Value) -> bool {
    match v {
        Value::Number(_) => true,
        Value::String(s) => s.starts_with("$.") || s == "$",
        _ => false,
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Returns `true` when `value` is a literal that can be statically type-checked
/// against a declared blackboard slot type.
///
/// Excluded (NOT literals for this purpose):
/// - String values starting with `$.` — these are path references whose
///   runtime type is not statically known.
/// - Single-key objects where the key is a known operator (`set`, `add`,
///   `subtract`, `multiply`, `divide`, `concat`) — these are operator objects
///   evaluated at runtime. Note: `{set: x}` is excluded even though its
///   operand type IS knowable, because the MALFORMED_OUTPUT_OPERATOR check
///   already validates operator shapes, and adding a second type-check here
///   would duplicate logic.
fn is_literal_output_value(value: &Value) -> bool {
    match value {
        // String starting with `$.` → path reference, skip.
        Value::String(s) if s.starts_with("$.") => false,
        // Single-key object → check for operator names.
        Value::Object(obj) if obj.len() == 1 => {
            let key = obj.keys().next().map(String::as_str).unwrap_or("");
            !matches!(
                key,
                "set" | "add" | "subtract" | "multiply" | "divide" | "concat"
            )
        }
        // All other values: booleans, numbers, arrays, null, multi-key objects,
        // plain strings (not starting with `$.`) → literals.
        _ => true,
    }
}

/// Returns `true` when `value`'s JSON type matches `declared_type`.
///
/// Declared types follow JSON Schema conventions (the same set used in
/// `snippet:` schemas): `string`, `number`, `integer`, `boolean`, `array`,
/// `object`. Both `number` and `integer` accept a JSON number value (the
/// distinction between integer and float is not enforced here — that is a
/// runtime concern).
fn literal_type_matches(value: &Value, declared_type: &str) -> bool {
    match declared_type {
        "string" => value.is_string(),
        "number" | "integer" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        // Unknown declared type → no check (don't produce false positives).
        _ => true,
    }
}

// ============================================================================
// RULE 2 — DETERMINISTIC_LOOP detection
// ============================================================================

/// Detect states caught in an all-deterministic cycle with no escape.
///
/// Ported from `crates/praxec-test/src/walk.rs::deterministic_loops`.
///
/// A state `S` is in an infinite deterministic loop when:
/// 1. Following ONLY `actor: deterministic` transitions from `S`, you can
///    never reach a terminal state OR a state that has at least one
///    non-deterministic transition (i.e., where an agent/human can take
///    over).
/// 2. The deterministic-only closure of `S` contains a cycle back to `S`
///    (i.e., `S` is actually in the loop, not merely a predecessor of it).
///
/// The default actor is `"agent"` (non-deterministic); only transitions that
/// explicitly set `actor: deterministic` are treated as deterministic here.
fn validate_deterministic_loops(
    id: &str,
    _def: &Value,
    states: &serde_json::Map<String, Value>,
    out: &mut Vec<Diagnostic>,
) {
    // Build deterministic-only successor graph + escape-point set.
    //
    // Escape points: terminal states OR states with at least one
    // non-deterministic transition.
    let mut det_successors: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut is_escape: HashSet<&str> = HashSet::new();

    for (state_name, state_def) in states {
        let mut has_non_det = false;
        let mut successors: Vec<&str> = Vec::new();

        if state_def
            .get("terminal")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            is_escape.insert(state_name.as_str());
        }

        if let Some(ts) = state_def.get("transitions").and_then(Value::as_object) {
            for (_t_name, t_def) in ts {
                let actor = t_def
                    .get("actor")
                    .and_then(Value::as_str)
                    .unwrap_or("agent");
                if actor == "deterministic" {
                    if let Some(target) = t_def.get("target").and_then(Value::as_str) {
                        successors.push(target);
                    }
                    if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                        for branch in branches {
                            if let Some(bt) = branch.get("target").and_then(Value::as_str) {
                                successors.push(bt);
                            }
                        }
                    }
                } else {
                    has_non_det = true;
                }
            }
        }

        if has_non_det {
            is_escape.insert(state_name.as_str());
        }

        det_successors.insert(state_name.as_str(), successors);
    }

    // For each state, walk the deterministic-only closure (BFS). A state is
    // flagged as a DETERMINISTIC_LOOP when:
    //   1. Its deterministic closure contains no escape state.
    //   2. It is part of a cycle (the closure contains a back-edge to it from
    //      a *different* state, i.e., it can reach itself again via det edges).
    for (state_name, _) in states {
        let s = state_name.as_str();

        let mut visited: HashSet<&str> = HashSet::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        queue.push_back(s);
        visited.insert(s);

        let mut found_escape = false;
        let mut can_reach_self_again = false;

        'bfs: while let Some(current) = queue.pop_front() {
            if is_escape.contains(current) {
                found_escape = true;
                break 'bfs;
            }
            if let Some(succs) = det_successors.get(current) {
                for &succ in succs {
                    if succ == s && current != s {
                        // A different state in the closure has a deterministic
                        // edge back to the starting state — cycle confirmed.
                        can_reach_self_again = true;
                    }
                    if !visited.contains(succ) {
                        visited.insert(succ);
                        queue.push_back(succ);
                    }
                }
            }
        }

        if !found_escape && can_reach_self_again {
            out.push(Diagnostic::Error(format!(
                "DETERMINISTIC_LOOP: workflow '{id}' state '{s}' is in a deterministic \
                 cycle with no exit (terminal or agent/human transition)"
            )));
        }
    }
}

/// Escape a JSON-Pointer path segment per RFC 6901: `~` → `~0`, `/` → `~1`.
/// Lets us index state names that contain `/` (rare but legal — namespace
/// fixtures sometimes have them).
fn pointer_escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_workflow_produces_no_diagnostics() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "target": "done" }
                            }
                        },
                        "done": { "terminal": true }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(d.is_empty(), "expected no diagnostics, got: {d:?}");
    }

    // ADR-0008 — outcomes + terminal `outcome` validation.

    /// Build a single-workflow config with the given `outcomes` block and a
    /// `done` terminal carrying the given `outcome` marker (or none).
    fn wf_outcomes(outcomes: Value, done_outcome: Option<&str>) -> Value {
        let mut done = json!({ "terminal": true });
        if let Some(o) = done_outcome {
            done["outcome"] = json!(o);
        }
        json!({
            "workflows": { "demo": {
                "initialState": "start",
                "outcomes": outcomes,
                "states": {
                    "start": { "transitions": { "go": { "target": "done" } } },
                    "done": done,
                }
            }}
        })
    }

    fn errors_containing(d: &[Diagnostic], needle: &str) -> bool {
        d.iter()
            .any(|x| x.is_error() && x.message().contains(needle))
    }

    #[test]
    fn well_formed_outcomes_with_success_terminal_are_clean() {
        let d = validate_workflows(&wf_outcomes(
            json!([{ "id": "verified", "statement": "It works.", "check": "$.context.ok == true" }]),
            Some("success"),
        ));
        assert!(d.is_empty(), "expected no diagnostics, got: {d:?}");
    }

    #[test]
    fn outcome_check_must_parse() {
        let d = validate_workflows(&wf_outcomes(
            json!([{ "id": "x", "statement": "s", "check": "this is not an expr" }]),
            Some("success"),
        ));
        assert!(
            errors_containing(&d, "not a parseable expression"),
            "got: {d:?}"
        );
    }

    #[test]
    fn duplicate_outcome_ids_are_rejected() {
        let d = validate_workflows(&wf_outcomes(
            json!([
                { "id": "dup", "statement": "a", "check": "$.context.a == 1" },
                { "id": "dup", "statement": "b", "check": "$.context.b == 1" },
            ]),
            Some("success"),
        ));
        assert!(
            errors_containing(&d, "duplicate outcome id 'dup'"),
            "got: {d:?}"
        );
    }

    #[test]
    fn outcome_missing_statement_is_rejected() {
        let d = validate_workflows(&wf_outcomes(
            json!([{ "id": "x", "check": "$.context.a == 1" }]),
            Some("success"),
        ));
        assert!(errors_containing(&d, "non-empty `statement`"), "got: {d:?}");
    }

    #[test]
    fn outcomes_without_a_success_terminal_are_rejected() {
        let d = validate_workflows(&wf_outcomes(
            json!([{ "id": "x", "statement": "s", "check": "$.context.a == 1" }]),
            None, // a bare `terminal: true`, no `outcome: success`
        ));
        assert!(
            errors_containing(&d, "no terminal state with `outcome: success`"),
            "got: {d:?}"
        );
    }

    #[test]
    fn invalid_terminal_outcome_token_is_rejected() {
        let config = json!({
            "workflows": { "demo": {
                "initialState": "start",
                "states": {
                    "start": { "transitions": { "go": { "target": "done" } } },
                    "done": { "terminal": true, "outcome": "winning" },
                }
            }}
        });
        let d = validate_workflows(&config);
        assert!(errors_containing(&d, "invalid `outcome`"), "got: {d:?}");
    }

    #[test]
    fn outcome_marker_on_a_non_terminal_state_is_rejected() {
        let config = json!({
            "workflows": { "demo": {
                "initialState": "start",
                "states": {
                    "start": { "outcome": "success", "transitions": { "go": { "target": "done" } } },
                    "done": { "terminal": true },
                }
            }}
        });
        let d = validate_workflows(&config);
        assert!(
            errors_containing(&d, "declares `outcome` but is not terminal"),
            "got: {d:?}"
        );
    }

    #[test]
    fn missing_initial_state_in_states() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "nonexistent",
                    "states": {
                        "start": { "terminal": true }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("nonexistent"))
        );
    }

    #[test]
    fn dangling_transition_target() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "target": "nowhere" }
                            }
                        }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("nowhere"))
        );
    }

    #[test]
    fn dangling_branch_target() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": {
                                    "target": "done",
                                    "branches": [
                                        { "when": { "kind": "expr", "expr": "1 == 1" }, "target": "ghost" }
                                    ]
                                }
                            }
                        },
                        "done": { "terminal": true }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("ghost"))
        );
    }

    #[test]
    fn unreachable_state_warned() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "target": "done" }
                            }
                        },
                        "done": { "terminal": true },
                        "orphan": {
                            "transitions": {
                                "x": { "target": "done" }
                            }
                        }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(
            d.iter()
                .any(|d| !d.is_error() && d.message().contains("orphan"))
        );
    }

    #[test]
    fn dead_end_non_terminal_warned() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "target": "stuck" }
                            }
                        },
                        "stuck": {}
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(
            d.iter()
                .any(|d| !d.is_error() && d.message().contains("stuck"))
        );
    }

    #[test]
    fn dead_end_suppressed_when_timeout_exists() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "timeoutMs": 5000,
                    "onTimeout": { "target": "timed_out" },
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "target": "waiting" }
                            }
                        },
                        "waiting": {},
                        "timed_out": { "terminal": true }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        let dead_end_warnings: Vec<_> = d
            .iter()
            .filter(|d| !d.is_error() && d.message().contains("no outgoing transitions"))
            .collect();
        assert!(
            dead_end_warnings.is_empty(),
            "dead-end warning should be suppressed when onTimeout exists: {dead_end_warnings:?}"
        );
    }

    #[test]
    fn state_that_cannot_reach_a_terminal_is_flagged() {
        // `wedge`/`wedge2` form a reachable agent cycle with no path to any
        // terminal — DETERMINISTIC_LOOP misses it (the cycle is agent, and a
        // state with a non-deterministic transition is "escape" to that check),
        // but terminal-reachability catches it. `done` IS reachable via the
        // `a -> good -> done` branch, so this isolates the new check.
        let config = json!({
            "workflows": { "demo": {
                "initialState": "start",
                "states": {
                    "start": { "transitions": {
                        "a": { "target": "good" },
                        "b": { "target": "wedge" }
                    }},
                    "good": { "transitions": { "fin": { "target": "done" } } },
                    "wedge":  { "transitions": { "spin": { "target": "wedge2", "actor": "agent" } } },
                    "wedge2": { "transitions": { "spin": { "target": "wedge",  "actor": "agent" } } },
                    "done": { "terminal": true }
                }
            }}
        });
        let d = validate_workflows(&config);
        let flagged: Vec<_> = d
            .iter()
            .filter(|x| x.message().contains("cannot reach any terminal state"))
            .map(|x| x.message().to_string())
            .collect();
        assert!(
            flagged.iter().any(|m| m.contains("'wedge'"))
                && flagged.iter().any(|m| m.contains("'wedge2'")),
            "both wedge states must be flagged, got: {flagged:?}"
        );
        assert!(
            !flagged
                .iter()
                .any(|m| m.contains("'good'") || m.contains("'start'")),
            "states that can reach `done` must NOT be flagged: {flagged:?}"
        );
    }

    #[test]
    fn a_retry_loop_that_can_reach_a_terminal_is_clean() {
        // `reviewing <-> implementing` is a legitimate loop, but `reviewing`
        // can exit to `done` via `approve` — so neither state wedges. Guards
        // against the check false-positiving on normal retry cycles.
        let config = json!({
            "workflows": { "demo": {
                "initialState": "implementing",
                "states": {
                    "implementing": { "transitions": { "build": { "target": "reviewing" } } },
                    "reviewing": { "transitions": {
                        "approve": { "target": "done" },
                        "reject":  { "target": "implementing", "actor": "agent" }
                    }},
                    "done": { "terminal": true }
                }
            }}
        });
        let d = validate_workflows(&config);
        assert!(
            !d.iter()
                .any(|x| x.message().contains("cannot reach any terminal state")),
            "a retry loop with an exit to a terminal must not be flagged: {d:?}"
        );
    }

    #[test]
    fn dangling_timeout_target() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "timeoutMs": 5000,
                    "onTimeout": { "target": "missing_timeout" },
                    "states": {
                        "start": { "terminal": true }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("missing_timeout"))
        );
    }

    #[test]
    fn missing_transition_target_field() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "executor": { "kind": "noop" } }
                            }
                        }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("missing 'target'"))
        );
    }

    #[test]
    fn no_workflows_produces_no_diagnostics() {
        let config = json!({
            "version": "1.0.0",
            "proxy": { "expose": [] }
        });
        let d = validate_workflows(&config);
        assert!(d.is_empty());
    }

    // ---- CMP-019: guard operand / expr presence ---------------------------

    fn guarded_workflow(guard: Value) -> Value {
        json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "target": "done", "guards": [guard] }
                            }
                        },
                        "done": { "terminal": true }
                    }
                }
            }
        })
    }

    #[test]
    fn permission_guard_missing_operand_rejected() {
        let d = validate_workflows(&guarded_workflow(json!({ "kind": "permission" })));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("INVALID_GUARD_OPERAND")),
            "{d:?}"
        );
    }

    #[test]
    fn permission_guard_empty_operand_rejected() {
        let d = validate_workflows(&guarded_workflow(
            json!({ "kind": "permission", "permission": "" }),
        ));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("INVALID_GUARD_OPERAND"))
        );
    }

    #[test]
    fn role_guard_missing_operand_rejected() {
        let d = validate_workflows(&guarded_workflow(json!({ "kind": "role" })));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("INVALID_GUARD_OPERAND"))
        );
    }

    #[test]
    fn expr_guard_missing_expr_rejected() {
        let d = validate_workflows(&guarded_workflow(json!({ "kind": "expr" })));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("INVALID_GUARD_OPERAND"))
        );
    }

    #[test]
    fn expr_guard_unparseable_expr_rejected() {
        let d = validate_workflows(&guarded_workflow(
            json!({ "kind": "expr", "expr": "not an expression" }),
        ));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("INVALID_GUARD_EXPR")),
            "{d:?}"
        );
    }

    #[test]
    fn expr_guard_valid_expr_passes() {
        let d = validate_workflows(&guarded_workflow(
            json!({ "kind": "expr", "expr": "$.context.x == \"y\"" }),
        ));
        assert!(
            !d.iter().any(|d| d.message().contains("INVALID_GUARD")),
            "{d:?}"
        );
    }

    #[test]
    fn nested_guard_missing_operand_rejected() {
        let d = validate_workflows(&guarded_workflow(json!({
            "kind": "all_of",
            "guards": [{ "kind": "role" }]
        })));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("INVALID_GUARD_OPERAND"))
        );
    }

    // ---- CMP-037: capability lifecycle enum -------------------------------

    #[test]
    fn cap_unknown_lifecycle_rejected() {
        let config = json!({
            "workflows": {
                "cap.plan.vet": {
                    "lifecycle": "stabel",
                    "initialState": "start",
                    "snippet": { "verb": "plan", "outputs": {} },
                    "states": { "start": { "terminal": true } }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("INVALID_LIFECYCLE")),
            "{d:?}"
        );
    }

    #[test]
    fn cap_valid_lifecycle_accepted() {
        let config = json!({
            "workflows": {
                "cap.plan.vet": {
                    "lifecycle": "experimental",
                    "initialState": "start",
                    "snippet": { "verb": "plan", "outputs": {} },
                    "states": { "start": { "terminal": true } }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(
            !d.iter().any(|d| d.message().contains("INVALID_LIFECYCLE")),
            "{d:?}"
        );
    }

    // ---- H11b: lifecycle is REQUIRED on caps (no silent experimental) -----

    #[test]
    fn cap_missing_lifecycle_rejected() {
        // A cap with NO `lifecycle:` must FAIL validation. Previously it
        // silently defaulted to `experimental`, which exempts callers from
        // the V16 contract-hash pin requirement — a supply-chain hole.
        let config = json!({
            "workflows": {
                "cap.plan.vet": {
                    "initialState": "start",
                    "snippet": { "verb": "plan", "outputs": {} },
                    "states": { "start": { "terminal": true } }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("MISSING_LIFECYCLE")),
            "{d:?}"
        );
    }

    #[test]
    fn caller_of_lifecycleless_cap_not_silently_exempted_from_pin() {
        // A `stable` caller-target that forgot its lifecycle must not silently
        // pass the V16 pin check. The target itself fails MISSING_LIFECYCLE,
        // and the invoking flow does not get a free pass via an `experimental`
        // default. (The caller carries an explicit pin to isolate that the
        // failure comes from the missing-lifecycle path, not from V16.)
        let target = json!({
            "initialState": "start",
            // NOTE: no `lifecycle:` here — the defect under test.
            "snippet": { "verb": "plan", "outputs": {} },
            "states": { "start": { "terminal": true } }
        });
        let actual_hash =
            crate::contract_hash::compute_contract_hash(target.get("snippet").unwrap());
        let config = json!({
            "workflows": {
                "cap.plan.vet": target,
                "flow.demo": {
                    "initialState": "s",
                    "states": {
                        "s": {
                            "transitions": {
                                "go": {
                                    "executor": {
                                        "kind": "workflow",
                                        "definitionId": "cap.plan.vet",
                                        "expects_contract_hash": actual_hash,
                                        "use": { "inputs": {}, "outputs": {} }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        // The target's missing lifecycle is surfaced as a hard error.
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("MISSING_LIFECYCLE")),
            "missing-lifecycle target must fail: {d:?}"
        );
    }

    // ---- FALLBACK-05: malformed output operators rejected at load --------

    /// Build a single-flow config whose `start` transition `output:` carries
    /// the given mapping value under key `result`.
    fn wf_with_output(output_spec: Value) -> Value {
        json!({
            "workflows": { "flow.demo": {
                "initialState": "start",
                "states": {
                    "start": { "transitions": { "go": {
                        "target": "done",
                        "output": { "result": output_spec }
                    } } },
                    "done": { "terminal": true }
                }
            }}
        })
    }

    #[test]
    fn add_with_three_operands_rejected() {
        let d = validate_workflows(&wf_with_output(
            json!({ "add": ["$.context.a", "$.context.b", "$.context.c"] }),
        ));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("MALFORMED_OUTPUT_OPERATOR")),
            "{d:?}"
        );
    }

    #[test]
    fn add_with_string_operand_rejected() {
        // `{add: "x"}` — operand is a non-array string.
        let d = validate_workflows(&wf_with_output(json!({ "add": "x" })));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("MALFORMED_OUTPUT_OPERATOR")),
            "{d:?}"
        );
    }

    #[test]
    fn add_with_non_path_string_operand_rejected() {
        // Operand "x" is neither a `$.` path nor a number → silent null.
        let d = validate_workflows(&wf_with_output(json!({ "add": ["x", 1] })));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("MALFORMED_OUTPUT_OPERATOR")),
            "{d:?}"
        );
    }

    #[test]
    fn concat_with_scalar_operand_rejected() {
        // `{concat: 5}` — operand must be an array.
        let d = validate_workflows(&wf_with_output(json!({ "concat": 5 })));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("MALFORMED_OUTPUT_OPERATOR")),
            "{d:?}"
        );
    }

    #[test]
    fn well_formed_operators_accepted() {
        // Valid arithmetic, valid concat, valid set, and a literal — none
        // should be flagged.
        for spec in [
            json!({ "add": ["$.context.a", 1] }),
            json!({ "subtract": [10, "$.context.b"] }),
            json!({ "concat": ["$.context.x", "-suffix"] }),
            json!({ "set": { "anything": [1, 2, 3] } }),
            json!("$.context.passthrough"),
            json!("plain literal"),
        ] {
            let d = validate_workflows(&wf_with_output(spec.clone()));
            assert!(
                !d.iter()
                    .any(|d| d.message().contains("MALFORMED_OUTPUT_OPERATOR")),
                "spec {spec:?} should be accepted, got: {d:?}"
            );
        }
    }

    // ---- RULE 1 — OUTPUT_TYPE_MISMATCH -----------------------------------------

    /// Builds a workflow with a typed blackboard slot `x` and a literal output.
    fn wf_typed_output(declared_type: &str, literal_value: Value) -> Value {
        json!({
            "workflows": { "demo": {
                "initialState": "start",
                "blackboard": {
                    "x": { "type": declared_type }
                },
                "states": {
                    "start": { "transitions": { "go": {
                        "target": "done",
                        "output": { "x": literal_value }
                    } } },
                    "done": { "terminal": true }
                }
            }}
        })
    }

    #[test]
    fn output_type_mismatch_bool_to_string_rejected() {
        // Literal `true` → declared `string` must be an error.
        let d = validate_workflows(&wf_typed_output("string", json!(true)));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("OUTPUT_TYPE_MISMATCH")),
            "bool→string mismatch should be rejected, got: {d:?}"
        );
    }

    #[test]
    fn output_type_mismatch_string_to_boolean_rejected() {
        // Literal `"yes"` → declared `boolean` must be an error.
        let d = validate_workflows(&wf_typed_output("boolean", json!("yes")));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("OUTPUT_TYPE_MISMATCH")),
            "string→boolean mismatch should be rejected, got: {d:?}"
        );
    }

    #[test]
    fn output_type_mismatch_number_to_string_rejected() {
        // Literal `42` → declared `string` must be an error.
        let d = validate_workflows(&wf_typed_output("string", json!(42)));
        assert!(
            d.iter()
                .any(|d| d.is_error() && d.message().contains("OUTPUT_TYPE_MISMATCH")),
            "number→string mismatch should be rejected, got: {d:?}"
        );
    }

    #[test]
    fn output_type_match_bool_to_boolean_accepted() {
        // Literal `true` → declared `boolean` is correct — no error.
        let d = validate_workflows(&wf_typed_output("boolean", json!(true)));
        assert!(
            !d.iter()
                .any(|d| d.message().contains("OUTPUT_TYPE_MISMATCH")),
            "bool→boolean match should be accepted, got: {d:?}"
        );
    }

    #[test]
    fn output_type_match_string_to_string_accepted() {
        // Literal `"hello"` → declared `string` is correct — no error.
        let d = validate_workflows(&wf_typed_output("string", json!("hello")));
        assert!(
            !d.iter()
                .any(|d| d.message().contains("OUTPUT_TYPE_MISMATCH")),
            "string→string match should be accepted, got: {d:?}"
        );
    }

    #[test]
    fn output_path_ref_not_type_checked() {
        // `$.context.y` is a path reference — NOT statically typed, must be skipped.
        let d = validate_workflows(&wf_typed_output("string", json!("$.context.y")));
        assert!(
            !d.iter()
                .any(|d| d.message().contains("OUTPUT_TYPE_MISMATCH")),
            "$.path output should not be type-checked, got: {d:?}"
        );
    }

    #[test]
    fn output_operator_object_not_type_checked() {
        // `{set: true}` is an operator, not a literal — must be skipped even if
        // the blackboard declares the slot as `string`.
        let d = validate_workflows(&wf_typed_output("string", json!({ "set": true })));
        assert!(
            !d.iter()
                .any(|d| d.message().contains("OUTPUT_TYPE_MISMATCH")),
            "operator object should not be type-checked, got: {d:?}"
        );
    }

    #[test]
    fn hello_flow_literal_bool_to_boolean_is_clean() {
        // Regression: hello-flow.yaml writes `approved: true` where the
        // blackboard declares `approved: { type: boolean }`. Must NOT fire.
        let config = json!({
            "workflows": { "flow.hello": {
                "initialState": "start",
                "blackboard": {
                    "approved": { "type": "boolean" }
                },
                "states": {
                    "start": { "transitions": {
                        "approve": {
                            "target": "done",
                            "output": { "approved": true }
                        }
                    }},
                    "done": { "terminal": true }
                }
            }}
        });
        let d = validate_workflows(&config);
        assert!(
            !d.iter()
                .any(|x| x.message().contains("OUTPUT_TYPE_MISMATCH")),
            "bool→boolean is a match, got: {d:?}"
        );
    }

    // ---- RULE 2 — DETERMINISTIC_LOOP -------------------------------------------

    /// A↔B mutual deterministic loop with no escape — both states must be flagged.
    #[test]
    fn deterministic_loop_ab_mutual_flagged() {
        let config = json!({
            "workflows": { "demo": {
                "initialState": "a",
                "states": {
                    "a": { "transitions": {
                        "ab": { "target": "b", "actor": "deterministic",
                                "executor": { "kind": "noop" } }
                    }},
                    "b": { "transitions": {
                        "ba": { "target": "a", "actor": "deterministic",
                                "executor": { "kind": "noop" } }
                    }}
                }
            }}
        });
        let d = validate_workflows(&config);
        let loop_errors: Vec<_> = d
            .iter()
            .filter(|x| x.is_error() && x.message().contains("DETERMINISTIC_LOOP"))
            .collect();
        assert!(
            loop_errors
                .iter()
                .any(|x| x.message().contains("state 'a'")),
            "state 'a' should be flagged; got: {d:?}"
        );
        assert!(
            loop_errors
                .iter()
                .any(|x| x.message().contains("state 'b'")),
            "state 'b' should be flagged; got: {d:?}"
        );
    }

    /// A deterministic chain A→B→done (terminal): no loop error.
    #[test]
    fn deterministic_chain_to_terminal_not_flagged() {
        let config = json!({
            "workflows": { "demo": {
                "initialState": "a",
                "states": {
                    "a": { "transitions": {
                        "ab": { "target": "b", "actor": "deterministic",
                                "executor": { "kind": "noop" } }
                    }},
                    "b": { "transitions": {
                        "fin": { "target": "done", "actor": "deterministic",
                                 "executor": { "kind": "noop" } }
                    }},
                    "done": { "terminal": true }
                }
            }}
        });
        let d = validate_workflows(&config);
        assert!(
            !d.iter().any(|x| x.message().contains("DETERMINISTIC_LOOP")),
            "terminating deterministic chain should not be flagged, got: {d:?}"
        );
    }

    /// A deterministic state that hands off to an agent state: no loop error.
    #[test]
    fn deterministic_state_hands_off_to_agent_not_flagged() {
        let config = json!({
            "workflows": { "demo": {
                "initialState": "a",
                "states": {
                    "a": { "transitions": {
                        "ab": { "target": "b", "actor": "deterministic",
                                "executor": { "kind": "noop" } }
                    }},
                    "b": { "transitions": {
                        // default actor is "agent" (non-deterministic) → escape point
                        "fin": { "target": "done", "executor": { "kind": "noop" } }
                    }},
                    "done": { "terminal": true }
                }
            }}
        });
        let d = validate_workflows(&config);
        assert!(
            !d.iter().any(|x| x.message().contains("DETERMINISTIC_LOOP")),
            "handoff to agent state should not be flagged, got: {d:?}"
        );
    }

    /// A state with both det + non-det transitions is an escape; self-loop is fine.
    #[test]
    fn mixed_transition_state_not_flagged() {
        let config = json!({
            "workflows": { "demo": {
                "initialState": "a",
                "states": {
                    "a": { "transitions": {
                        "loop": { "target": "a", "actor": "deterministic",
                                  "executor": { "kind": "noop" } },
                        // non-deterministic exit — makes 'a' an escape
                        "exit": { "target": "done", "executor": { "kind": "noop" } }
                    }},
                    "done": { "terminal": true }
                }
            }}
        });
        let d = validate_workflows(&config);
        assert!(
            !d.iter().any(|x| x.message().contains("DETERMINISTIC_LOOP")),
            "state with mixed transitions should not be flagged, got: {d:?}"
        );
    }

    // ---- RULE 3 — strict_validation --------------------------------------------

    /// Without strict_validation, an unreachable state is a Warning (not Error).
    #[test]
    fn unreachable_state_is_warning_without_strict() {
        let config = json!({
            "workflows": { "demo": {
                "initialState": "start",
                "states": {
                    "start": { "transitions": { "go": { "target": "done" } } },
                    "done": { "terminal": true },
                    "orphan": { "transitions": { "x": { "target": "done" } } }
                }
            }}
        });
        let d = validate_workflows(&config);
        let about_orphan: Vec<_> = d
            .iter()
            .filter(|x| x.message().contains("orphan"))
            .collect();
        assert!(
            !about_orphan.is_empty(),
            "orphan should produce a diagnostic"
        );
        assert!(
            about_orphan.iter().all(|x| !x.is_error()),
            "without strict, orphan should be a Warning, got: {about_orphan:?}"
        );
    }

    /// With strict_validation: true, the unreachable-state warning becomes an Error.
    #[test]
    fn unreachable_state_becomes_error_in_strict_mode() {
        let config = json!({
            "gateway": { "strict_validation": true },
            "workflows": { "demo": {
                "initialState": "start",
                "states": {
                    "start": { "transitions": { "go": { "target": "done" } } },
                    "done": { "terminal": true },
                    "orphan": { "transitions": { "x": { "target": "done" } } }
                }
            }}
        });
        let d = validate_workflows(&config);
        assert!(
            d.iter()
                .any(|x| x.is_error() && x.message().contains("orphan")),
            "strict mode should promote orphan warning to Error, got: {d:?}"
        );
    }

    /// With strict_validation: true, the dead-end non-terminal warning becomes an Error.
    #[test]
    fn dead_end_becomes_error_in_strict_mode() {
        let config = json!({
            "gateway": { "strict_validation": true },
            "workflows": { "demo": {
                "initialState": "start",
                "states": {
                    "start": { "transitions": { "go": { "target": "stuck" } } },
                    "stuck": {}
                }
            }}
        });
        let d = validate_workflows(&config);
        assert!(
            d.iter()
                .any(|x| x.is_error() && x.message().contains("stuck")),
            "strict mode should promote dead-end warning to Error, got: {d:?}"
        );
    }

    /// With strict_validation: false (explicit), warnings remain warnings.
    #[test]
    fn strict_false_explicit_leaves_warnings_as_warnings() {
        let config = json!({
            "gateway": { "strict_validation": false },
            "workflows": { "demo": {
                "initialState": "start",
                "states": {
                    "start": { "transitions": { "go": { "target": "done" } } },
                    "done": { "terminal": true },
                    "orphan": { "transitions": { "x": { "target": "done" } } }
                }
            }}
        });
        let d = validate_workflows(&config);
        let about_orphan: Vec<_> = d
            .iter()
            .filter(|x| x.message().contains("orphan"))
            .collect();
        assert!(
            !about_orphan.is_empty(),
            "orphan should produce a diagnostic"
        );
        assert!(
            about_orphan.iter().all(|x| !x.is_error()),
            "explicit strict:false should keep orphan as Warning, got: {about_orphan:?}"
        );
    }

    #[test]
    fn a_deterministic_string_switch_without_default_is_a_warning() {
        let config = json!({
            "workflows": { "demo": {
                "initialState": "gate",
                "states": {
                    "gate": { "transitions": {
                        "to_a": { "actor": "deterministic", "guards": [{"kind":"expr","expr":"$.context.verdict == 'a'"}], "target": "done" },
                        "to_b": { "actor": "deterministic", "guards": [{"kind":"expr","expr":"$.context.verdict == 'b'"}], "target": "done" }
                    }},
                    "done": { "terminal": true }
                }
            }}
        });
        let d = validate_workflows(&config);
        // Advisory, not boot-blocking — the runtime already prevents the dead-stall.
        assert!(
            d.iter()
                .any(|x| !x.is_error() && x.message().contains("switches on '$.context.verdict'")),
            "a string switch with no default must be a Warning (not an Error), got: {d:?}"
        );
    }

    #[test]
    fn a_string_switch_with_an_unguarded_default_is_clean() {
        let config = json!({
            "workflows": { "demo": {
                "initialState": "gate",
                "states": {
                    "gate": { "transitions": {
                        "to_a": { "actor": "deterministic", "guards": [{"kind":"expr","expr":"$.context.verdict == 'a'"}], "target": "done" },
                        "to_b": { "actor": "deterministic", "guards": [{"kind":"expr","expr":"$.context.verdict == 'b'"}], "target": "done" },
                        "fallback": { "actor": "deterministic", "target": "err" }
                    }},
                    "done": { "terminal": true },
                    "err": { "terminal": true }
                }
            }}
        });
        let d = validate_workflows(&config);
        assert!(
            !d.iter().any(|x| x.message().contains("switches on")),
            "a switch WITH an unguarded default must not be flagged, got: {d:?}"
        );
    }

    #[test]
    fn a_boolean_equality_switch_is_not_flagged_as_a_string_switch() {
        let config = json!({
            "workflows": { "demo": {
                "initialState": "gate",
                "states": {
                    "gate": { "transitions": {
                        "yes": { "actor": "deterministic", "guards": [{"kind":"expr","expr":"$.context.done == true"}], "target": "done" },
                        "no": { "actor": "deterministic", "guards": [{"kind":"expr","expr":"$.context.done == false"}], "target": "done" }
                    }},
                    "done": { "terminal": true }
                }
            }}
        });
        let d = validate_workflows(&config);
        assert!(
            !d.iter().any(|x| x.message().contains("switches on")),
            "a boolean equality switch must NOT be treated as an open-domain string switch, got: {d:?}"
        );
    }

    // ── Spec A §7.1 — fan-in composition check for `kind: parallel` ──────────

    /// Wrap a single `kind: parallel` transition executor in a loadable workflow.
    fn wf_parallel(executor: Value) -> Value {
        json!({
            "workflows": { "demo": {
                "initialState": "fan",
                "states": {
                    "fan": { "transitions": { "go": { "target": "done", "executor": executor } } },
                    "done": { "terminal": true }
                }
            }}
        })
    }

    #[test]
    fn parallel_reduce_requiring_envelope_fields_is_clean() {
        // The aggregator consumes only fan-in envelope fields → no diagnostics.
        let d = validate_workflows(&wf_parallel(json!({
            "kind": "parallel",
            "branches": { "for_each": "$.context.items", "do": { "kind": "noop" } },
            "join": { "aggregator": {
                "kind": "expression",
                "expr": "$.ok_count >= 1",
                "inputSchema": {
                    "type": "object",
                    "properties": { "branches": { "type": "array" }, "ok_count": { "type": "integer" } },
                    "required": ["branches", "ok_count"]
                }
            }}
        })));
        assert!(
            !d.iter().any(|x| x.message().contains("PARALLEL_REDUCE")),
            "a reduce consuming only envelope fields must load clean, got: {d:?}"
        );
    }

    #[test]
    fn parallel_reduce_requiring_unproducible_top_level_field_is_rejected() {
        let d = validate_workflows(&wf_parallel(json!({
            "kind": "parallel",
            "branches": { "for_each": "$.context.items", "do": { "kind": "noop" } },
            "join": { "aggregator": {
                "kind": "script",
                "subject": "reduce.sh",
                "inputSchema": {
                    "type": "object",
                    "properties": { "totals": { "type": "object" } },
                    "required": ["totals"]
                }
            }}
        })));
        assert!(
            errors_containing(&d, "PARALLEL_REDUCE_UNSATISFIED_FIELD"),
            "a reduce requiring a field the fan-in never produces must be a load error, got: {d:?}"
        );
    }

    #[test]
    fn parallel_reduce_requiring_unproduced_branch_output_field_is_rejected() {
        // Worker declares it produces `{score}`; the reduce requires each
        // branch output carry `weight` — never produced → load error.
        let d = validate_workflows(&wf_parallel(json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.items",
                "do": {
                    "kind": "noop",
                    "outputSchema": {
                        "type": "object",
                        "properties": { "score": { "type": "number" } },
                        "required": ["score"]
                    }
                }
            },
            "join": { "aggregator": {
                "kind": "script",
                "subject": "reduce.sh",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branches": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "output": {
                                        "type": "object",
                                        "properties": { "weight": { "type": "number" } },
                                        "required": ["weight"]
                                    }
                                }
                            }
                        }
                    },
                    "required": ["branches"]
                }
            }}
        })));
        assert!(
            errors_containing(&d, "PARALLEL_REDUCE_OUTPUT_FIELD_UNPRODUCED"),
            "a reduce reading a per-branch field the map never produces must be a load error, got: {d:?}"
        );
    }

    #[test]
    fn parallel_reduce_consuming_produced_branch_output_field_is_clean() {
        // Worker produces `{score}`; reduce requires `{score}` per branch → clean.
        let d = validate_workflows(&wf_parallel(json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.items",
                "do": {
                    "kind": "noop",
                    "outputSchema": {
                        "type": "object",
                        "properties": { "score": { "type": "number" } },
                        "required": ["score"]
                    }
                }
            },
            "join": { "aggregator": {
                "kind": "script",
                "subject": "reduce.sh",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "branches": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "output": {
                                        "type": "object",
                                        "properties": { "score": { "type": "number" } },
                                        "required": ["score"]
                                    }
                                }
                            }
                        }
                    },
                    "required": ["branches"]
                }
            }}
        })));
        assert!(
            !d.iter().any(|x| x.message().contains("PARALLEL_REDUCE")),
            "a reduce consuming exactly what the map produces must load clean, got: {d:?}"
        );
    }

    #[test]
    fn parallel_without_aggregator_input_contract_is_not_checked() {
        // No aggregator inputSchema → nothing to prove, skip (honest boundary).
        let d = validate_workflows(&wf_parallel(json!({
            "kind": "parallel",
            "branches": { "for_each": "$.context.items", "do": { "kind": "noop" } },
            "join": "all"
        })));
        assert!(
            !d.iter().any(|x| x.message().contains("PARALLEL_REDUCE")),
            "a parallel step with no declared reduce contract must not be flagged, got: {d:?}"
        );
    }
}
