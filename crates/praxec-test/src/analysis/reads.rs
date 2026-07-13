//! Build the context a transition needs to FIRE (all `$.context.*` reads
//! populated; guard slots at satisfying values) and a context that VIOLATES a
//! guard (one guard slot flipped, but still PRESENT).

use crate::analysis::dummy::dummy_for_schema;
use crate::analysis::expr::{
    context_ref, input_ref, parse_clause, parse_comparison, parse_literal_value, satisfying_pair,
    satisfying_value, violating_value,
};
use serde_json::{Map, Value};

/// Seed the workflow INPUT so a transition's `$.workflow.input.*` / `$.input.*`
/// guards are satisfied. These are invisible to [`satisfying_context`] (which
/// only handles `$.context.*`), so an input-gated edge like
/// `$.input.mode == 'auto'` used to fail with GUARD_REJECTED — reported as "could
/// not satisfy guard" against a correct definition.
///
/// Seeded per-EDGE, not per-definition: sibling edges routinely gate on the SAME
/// input slot with mutually-exclusive values (`stack == 'rust'` vs
/// `stack == 'dotnet'`), which no single input can satisfy at once.
pub fn seed_input_guards(input: &mut Value, transition: &Value) {
    let Some(obj) = input.as_object_mut() else {
        return;
    };
    for expr in all_guard_exprs(transition) {
        let Some(cmp) = parse_comparison(&expr) else {
            continue;
        };
        let Some(path) = input_ref(&cmp.lhs) else {
            continue;
        };
        // Only `==` pins a concrete input value; `!=`/comparisons leave the dummy.
        if cmp.op == "==" {
            insert_at_path(obj, path, parse_literal_value(&cmp.rhs));
        }
    }
}

/// A type-appropriate dummy for context slot `slot`, using the workflow's
/// `blackboard` slot-type declaration when present. Falls back to a string
/// (`"fuzz"`) for slots with no declared type. This is what makes a seeded
/// context match the blackboard's declared types — seeding a `{type: integer}`
/// slot with the string `"fuzz"` is exactly what produced false
/// BLACKBOARD_TYPE_ERROR verdicts.
fn typed_dummy(blackboard: &Value, slot: &str) -> Value {
    blackboard
        .get(slot)
        .map(dummy_for_schema)
        .unwrap_or_else(|| Value::String("fuzz".to_owned()))
}

/// Every guard `expr` string reachable from a transition: top-level `guards[]`
/// AND each branch's `when` guard (`branches[].when`). Branch guards are where
/// `actor: deterministic` transitions carry their routing predicates, so they
/// must be satisfied too — otherwise their read slots fall through to the
/// untyped fallback.
fn all_guard_exprs(transition: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(guards) = transition.get("guards").and_then(|g| g.as_array()) {
        for g in guards {
            if let Some(e) = g.get("expr").and_then(Value::as_str) {
                out.push(e.to_owned());
            }
        }
    }
    if let Some(branches) = transition.get("branches").and_then(|b| b.as_array()) {
        for br in branches {
            if let Some(e) = br.pointer("/when/expr").and_then(Value::as_str) {
                out.push(e.to_owned());
            }
        }
    }
    out
}

/// All bare top-level context slots referenced via `$.context.<slot>` anywhere
/// in the transition object (guards `expr`, `output:` values, `executor.args`).
/// For `$.context.a.b` the slot is `a`. Deduplicated.
pub fn context_reads(transition: &Value) -> Vec<String> {
    context_read_paths(transition)
        .into_iter()
        .map(|p| p.split('.').next().unwrap_or(&p).to_owned())
        .fold(Vec::new(), |mut acc, s| {
            if !acc.contains(&s) {
                acc.push(s);
            }
            acc
        })
}

/// Every `$.context.<path>` reference anywhere in `value`, keeping the FULL
/// dotted path (`scoped_out.status`, not `scoped_out`).
///
/// The distinction is load-bearing. A guard reading `$.context.scoped_out.status`
/// is only satisfied if the SUB-path is present — seeding the top-level slot with
/// an empty `{}` dummy leaves `.status` unset, and the runtime fails the guard
/// with GUARD_UNSET_SLOT. That is a fabricated-context artifact, not a defect in
/// the definition.
pub fn context_read_paths(value: &Value) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    collect_reads(value, &mut paths);
    let mut seen = std::collections::HashSet::new();
    paths.retain(|s| seen.insert(s.clone()));
    paths
}

fn collect_reads(value: &Value, paths: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            // A single string value may contain multiple `$.context.*` references
            // (e.g. a guard expr like "$.context.approved == true" or a template
            // that mentions several slots). Scan for all occurrences.
            let mut haystack: &str = s.as_str();
            while let Some(idx) = haystack.find("$.context.") {
                let rest = &haystack[idx + "$.context.".len()..];
                // Take the whole dotted path: stop at a delimiter or an operator.
                let path: String = rest
                    .chars()
                    .take_while(|c| {
                        !matches!(
                            c,
                            '[' | ' '
                                | '='
                                | '!'
                                | '<'
                                | '>'
                                | '\t'
                                | '\n'
                                | '"'
                                | '\''
                                | ')'
                                | ','
                        )
                    })
                    .collect();
                let path = path.trim_end_matches('.').to_owned();
                if !path.is_empty() {
                    paths.push(path);
                }
                // Advance past this occurrence to find further refs in the same string
                haystack = &haystack[idx + "$.context.".len()..];
            }
        }
        Value::Array(arr) => {
            for item in arr {
                collect_reads(item, paths);
            }
        }
        Value::Object(map) => {
            for (_k, v) in map {
                collect_reads(v, paths);
            }
        }
        _ => {}
    }
}

/// Insert `val` at a dotted `path` inside `map`, creating intermediate objects.
/// A non-object sitting where an object is needed is replaced — a `{}` dummy must
/// not block seeding `scoped_out.status`.
fn insert_at_path(map: &mut Map<String, Value>, path: &str, val: Value) {
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    let Some((last, parents)) = parts.split_last() else {
        return;
    };
    let mut cursor = map;
    for p in parents {
        let entry = cursor
            .entry((*p).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        cursor = entry.as_object_mut().expect("coerced to object above");
    }
    cursor.insert((*last).to_string(), val);
}

/// True when `path` already resolves to a value in `map`.
fn path_is_set(map: &Map<String, Value>, path: &str) -> bool {
    let mut cur: Option<&Value> = None;
    for (i, part) in path.split('.').filter(|p| !p.is_empty()).enumerate() {
        cur = match (i, cur) {
            (0, _) => map.get(part),
            (_, Some(v)) => v.get(part),
            _ => None,
        };
        if cur.is_none() {
            return false;
        }
    }
    cur.is_some()
}

/// The context an isolated probe should START from: every slot the DEFINITION
/// could read, not merely the ones the probed transition reads.
///
/// The per-transition prober drops a workflow into ONE state, fires ONE edge, and
/// lets the deterministic chain run wherever it goes. Seeding only the probed
/// edge's reads means every guard further down the chain reads a slot nobody set,
/// and the runtime fails it with GUARD_UNSET_SLOT — reporting a defect the
/// definition does not have. It is an artifact of a fabricated history: on a REAL
/// path, the states that ran before this one wrote those slots.
///
/// So make the fabrication plausible. Seeded, in increasing precedence:
///
/// 1. every `blackboard:` slot, at its declared type;
/// 2. every `$.context.<path>` the definition reads ANYWHERE — at the full dotted
///    path, so a guard on `scoped_out.status` finds a `.status`;
/// 3. `initialContext:`, which is what the author actually declared.
///
/// Whether a slot is genuinely written on every path to the state that reads it is
/// a PATH question, and a fabricated single-state probe cannot answer it. That is
/// what static must-write analysis is for (V24 does it for declared outputs).
/// For every guard clause anywhere in the definition, the `(slot-path, value)`
/// seeds that SATISFY it — including the slot-vs-slot case (`repair_round <
/// max_repair_rounds`), where BOTH sides are seeded to a consistent pair. First
/// writer per path wins: an exhaustive gate partitions on the same slots, and any
/// one consistent assignment routes SOME branch, which is all a base context
/// owes; the per-transition probe overwrites as needed.
fn guard_derived_seeds(def: &Value) -> Vec<(String, Value)> {
    let mut seeds: Vec<(String, Value)> = Vec::new();
    let mut push = |path: String, val: Value| {
        if !seeds.iter().any(|(p, _)| p == &path) {
            seeds.push((path, val));
        }
    };
    let Some(states) = def.pointer("/states").and_then(Value::as_object) else {
        return seeds;
    };
    for state in states.values() {
        let Some(transitions) = state.pointer("/transitions").and_then(Value::as_object) else {
            continue;
        };
        for t in transitions.values() {
            for expr in all_guard_exprs(t) {
                // Slot-vs-slot (`$.context.a OP $.context.b`) — seed a pair.
                if let Some(cmp) = parse_comparison(&expr)
                    && let (Some(lhs), Some(rhs)) = (context_ref(&cmp.lhs), context_ref(&cmp.rhs))
                    && let Some((lval, rval)) = satisfying_pair(&cmp.op)
                {
                    push(lhs.to_owned(), lval);
                    push(rhs.to_owned(), rval);
                    continue;
                }
                if let Some(clause) = parse_clause(&expr)
                    && let Some(v) = satisfying_value(&clause)
                {
                    push(clause.slot.clone(), v);
                }
            }
        }
    }
    seeds
}

pub fn definition_wide_context(def: &Value, blackboard: &Value) -> Map<String, Value> {
    let mut map: Map<String, Value> = Map::new();

    // 1. Every declared blackboard slot, at its declared type.
    if let Some(slots) = blackboard.as_object() {
        for (slot, schema) in slots {
            map.insert(slot.clone(), dummy_for_schema(schema));
        }
    }

    // 2. Every context path the definition reads anywhere (guards, outputs, args).
    for path in context_read_paths(def) {
        if path_is_set(&map, &path) {
            continue;
        }
        let top = path.split('.').next().unwrap_or(&path);
        let val = if path.contains('.') {
            // A sub-path has no blackboard type of its own; a string satisfies
            // presence, and a guard that needs a specific value overwrites it in
            // `satisfying_context`.
            Value::String("fuzz".to_owned())
        } else {
            typed_dummy(blackboard, top)
        };
        insert_at_path(&mut map, &path, val);
    }

    // 2.5. A slot compared in a guard (`validate_codebase == true`) but never
    // declared in `blackboard:` was just seeded as the string "fuzz" above —
    // which satisfies NEITHER `== true` NOR `== false`, so EVERY branch of an
    // exhaustive gate fails and the chain dies with "no viable deterministic
    // transition". Overwrite each guard-compared slot with a value the FIRST
    // clause referencing it accepts: type-correct, and enough to route one branch.
    for (path, val) in guard_derived_seeds(def) {
        insert_at_path(&mut map, &path, val);
    }

    // 3. initialContext wins — it is what the author actually declared.
    if let Some(seeds) = def.pointer("/initialContext").and_then(Value::as_object) {
        for (k, v) in seeds {
            map.insert(k.clone(), v.clone());
        }
    }

    map
}

/// A context object that satisfies the transition's guards and populates every
/// read (guard slots at satisfying values; other reads get a dummy string).
///
/// `base` is the definition-wide seed from [`definition_wide_context`] — pass
/// `None` for the transition-local behavior (used by the unit tests).
pub fn satisfying_context_over(
    transition: &Value,
    blackboard: &Value,
    base: Option<&Map<String, Value>>,
) -> Value {
    let mut map: Map<String, Value> = base.cloned().unwrap_or_default();

    // First pass: guard clauses (top-level AND branch `when`) → satisfying values,
    // written at the FULL dotted path. A guard on `$.context.out.status` needs
    // `out.status`, not a satisfying value parked on `out` itself.
    for expr_str in all_guard_exprs(transition) {
        // Slot-vs-slot comparison (`$.context.iter >= $.context.iter_cap`) — the
        // RHS is another context path, not a literal. `satisfying_value` can't
        // help (it assumes a literal RHS), so seed BOTH sides to a pair that
        // satisfies the operator. Without this, the "exhausted" branch of every
        // bounded loop (`iter >= iter_cap`) failed with GUARD_REJECTED.
        if let Some(cmp) = parse_comparison(&expr_str)
            && let (Some(lhs), Some(rhs)) = (context_ref(&cmp.lhs), context_ref(&cmp.rhs))
            && let Some((lval, rval)) = satisfying_pair(&cmp.op)
        {
            let (lhs, rhs) = (lhs.to_owned(), rhs.to_owned());
            insert_at_path(&mut map, &lhs, lval);
            insert_at_path(&mut map, &rhs, rval);
            continue;
        }
        if let Some(clause) = parse_clause(&expr_str) {
            let top_slot = clause
                .slot
                .split('.')
                .next()
                .unwrap_or(&clause.slot)
                .to_owned();
            let val =
                satisfying_value(&clause).unwrap_or_else(|| typed_dummy(blackboard, &top_slot));
            insert_at_path(&mut map, &clause.slot, val);
        }
    }

    // Second pass: every other read of THIS transition, at its full path, with a
    // blackboard-TYPED dummy for a bare slot so the seed matches its declared type.
    for path in context_read_paths(transition) {
        if path_is_set(&map, &path) {
            continue;
        }
        let top = path.split('.').next().unwrap_or(&path);
        let val = if path.contains('.') {
            Value::String("fuzz".to_owned())
        } else {
            typed_dummy(blackboard, top)
        };
        insert_at_path(&mut map, &path, val);
    }

    Value::Object(map)
}

/// Transition-local satisfying context (no definition-wide base).
pub fn satisfying_context(transition: &Value, blackboard: &Value) -> Value {
    satisfying_context_over(transition, blackboard, None)
}

/// Violating context over a definition-wide base. See [`violating_context`].
pub fn violating_context_over(
    transition: &Value,
    blackboard: &Value,
    base: Option<&Map<String, Value>>,
) -> Option<Value> {
    let first_clause = transition
        .get("guards")
        .and_then(|g| g.as_array())?
        .iter()
        .find_map(|guard| parse_clause(guard.get("expr")?.as_str()?))?;

    // A value that GENUINELY fails the clause. If we can't guarantee one (e.g. a
    // `contains ''` guard), skip the violating-path check rather than assert on a
    // value that might still satisfy — a false "guard did not reject".
    let viol = violating_value(&first_clause)?;

    let mut map = match satisfying_context_over(transition, blackboard, base) {
        Value::Object(m) => m,
        _ => Map::new(),
    };

    // Flip at the FULL path so the guard still READS a present value and merely
    // fails it — an UNSET slot would error instead of rejecting, which is a
    // different verdict entirely.
    insert_at_path(&mut map, &first_clause.slot, viol);

    Some(Value::Object(map))
}

/// Some context that VIOLATES at least one guard (the guard slot is PRESENT but
/// set to a non-satisfying value). `None` if the transition has no parseable
/// guard to violate.
pub fn violating_context(transition: &Value, blackboard: &Value) -> Option<Value> {
    // Only TOP-LEVEL `guards` BLOCK a transition. Branch `when` predicates merely
    // ROUTE — a deterministic router always fires (to a matching branch or its
    // default `target`), so flipping a branch predicate never yields a rejection.
    // Restrict the violating check to real blocking guards; routers have none and
    // correctly get no violating-path check.
    let first_clause = transition
        .get("guards")
        .and_then(|g| g.as_array())?
        .iter()
        .find_map(|guard| parse_clause(guard.get("expr")?.as_str()?))?;

    // Start from a satisfying context
    let mut map = match satisfying_context(transition, blackboard) {
        Value::Object(m) => m,
        _ => Map::new(),
    };

    // Overwrite the first guard's top slot with a non-satisfying but PRESENT value
    let top_slot = first_clause
        .slot
        .split('.')
        .next()
        .unwrap_or(&first_clause.slot)
        .to_owned();

    let sat = satisfying_value(&first_clause);
    // Use bool false unless sat is already false, then use true.
    // A bool type-mismatch will fail any numeric/string comparison too.
    let viol = if sat == Some(Value::Bool(false)) {
        Value::Bool(true)
    } else {
        Value::Bool(false)
    };
    map.insert(top_slot, viol);

    Some(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t() -> Value {
        json!({ "target": "x",
            "guards": [ { "kind": "expr", "expr": "$.context.approved == true" } ],
            "output": { "y": "$.context.note" },
            "executor": { "kind": "noop", "args": ["$.context.id"] } })
    }

    #[test]
    fn collects_reads() {
        let mut r = context_reads(&t());
        r.sort();
        assert_eq!(
            r,
            vec!["approved".to_string(), "id".to_string(), "note".to_string()]
        );
    }

    #[test]
    fn satisfies_guard_and_fills_reads() {
        let c = satisfying_context(&t(), &json!({}));
        assert_eq!(c.pointer("/approved"), Some(&json!(true)));
        assert!(c.get("note").is_some());
        assert!(c.get("id").is_some());
    }

    #[test]
    fn violates_guard_but_keeps_slot_present() {
        let v = violating_context(&t(), &json!({})).expect("guarded");
        assert!(
            v.get("approved").is_some(),
            "slot must stay present (not unset)"
        );
        assert_ne!(
            v.pointer("/approved"),
            Some(&json!(true)),
            "but not satisfying"
        );
    }

    #[test]
    fn no_guards_means_no_violating_context() {
        let t2 = json!({ "target": "x", "executor": { "kind": "noop" } });
        assert_eq!(violating_context(&t2, &json!({})), None);
    }

    #[test]
    fn non_guard_read_seeded_with_blackboard_type() {
        // `count` is read by the output (not a guard) and declared integer.
        // It must be seeded as an integer, not the string "fuzz".
        let t = json!({ "target": "x",
            "output": { "next": { "add": ["$.context.count", 1] } },
            "executor": { "kind": "noop" } });
        let bb = json!({ "count": { "type": "integer" } });
        let c = satisfying_context(&t, &bb);
        assert_eq!(
            c.pointer("/count"),
            Some(&json!(1)),
            "integer slot, not a string"
        );
    }

    #[test]
    fn branched_router_has_no_violating_context() {
        // A deterministic router gates nothing (branches route; default fires).
        // No top-level guard ⇒ no violating-path check.
        let t = json!({ "target": "fallback", "actor": "deterministic",
            "branches": [
                { "when": { "kind": "expr", "expr": "$.context.verdict == 'approved'" },
                  "target": "done" }
            ],
            "executor": { "kind": "noop" } });
        assert_eq!(violating_context(&t, &json!({})), None);
    }

    #[test]
    fn branch_when_guards_are_satisfied() {
        // Guard lives in `branches[].when`, and reads an integer slot.
        let t = json!({ "target": "fallback", "actor": "deterministic",
            "branches": [
                { "when": { "kind": "expr", "expr": "$.context.revision_count >= 3" },
                  "target": "done" }
            ],
            "executor": { "kind": "noop" } });
        let bb = json!({ "revision_count": { "type": "integer" } });
        let c = satisfying_context(&t, &bb);
        let v = c
            .pointer("/revision_count")
            .and_then(Value::as_i64)
            .expect("integer");
        assert!(v >= 3, "branch guard must be satisfied, got {v}");
    }

    // ── Nested guard paths + slot-vs-slot + input + definition-wide seeding ──

    #[test]
    fn a_guard_on_a_sub_path_is_satisfied_at_that_sub_path() {
        // `$.context.out.status == 'pass'` must seed `out.status`, not park a
        // value on `out` and leave `.status` unset (which errors GUARD_UNSET_SLOT).
        let t = json!({ "target": "x", "actor": "deterministic",
            "guards": [ { "kind": "expr", "expr": "$.context.out.status == 'pass'" } ],
            "executor": { "kind": "noop" } });
        let c = satisfying_context(&t, &json!({}));
        assert_eq!(c.pointer("/out/status"), Some(&json!("pass")));
    }

    #[test]
    fn a_slot_vs_slot_guard_seeds_a_consistent_pair() {
        // `iter >= iter_cap` — neither side is a literal. Both must be seeded so
        // the comparison holds (the "exhausted loop" branch of every bounded loop).
        let t = json!({ "target": "x", "actor": "deterministic",
            "guards": [ { "kind": "expr", "expr": "$.context.iter >= $.context.iter_cap" } ],
            "executor": { "kind": "noop" } });
        let c = satisfying_context(&t, &json!({}));
        let iter = c.pointer("/iter").and_then(Value::as_i64).expect("iter");
        let cap = c
            .pointer("/iter_cap")
            .and_then(Value::as_i64)
            .expect("iter_cap");
        assert!(iter >= cap, "pair must satisfy `>=`: {iter} >= {cap}");
    }

    #[test]
    fn seed_input_guards_pins_an_input_scoped_equality() {
        // `$.workflow.input.stack == 'rust'` — the input, not context, must carry
        // the value. Sibling edges gate the same slot on other values, so this is
        // seeded per-edge.
        let t = json!({ "target": "x", "actor": "deterministic",
            "guards": [ { "kind": "expr", "expr": "$.workflow.input.stack == 'rust'" } ],
            "executor": { "kind": "noop" } });
        let mut input = json!({});
        seed_input_guards(&mut input, &t);
        assert_eq!(input.pointer("/stack"), Some(&json!("rust")));
    }

    #[test]
    fn definition_wide_context_seeds_a_slot_only_a_downstream_gate_reads() {
        // A slot read by a gate but written by no probed edge and absent from
        // blackboard/initialContext must still be seeded to a type-correct value
        // that routes SOME branch — else the chain dies "no viable deterministic".
        let def = json!({
            "initialState": "a",
            "states": {
                "a": { "transitions": { "go": {
                    "target": "gate", "actor": "deterministic", "executor": { "kind": "noop" } } } },
                "gate": { "transitions": {
                    "yes": { "target": "done", "actor": "deterministic",
                             "guards": [ { "kind": "expr", "expr": "$.context.ready == true" } ] },
                    "no":  { "target": "done", "actor": "deterministic",
                             "guards": [ { "kind": "expr", "expr": "$.context.ready == false" } ] }
                }},
                "done": { "terminal": true }
            }
        });
        let ctx = definition_wide_context(&def, &json!({}));
        // `ready` is compared `== true`/`== false`; it must be seeded as a BOOL
        // (routing one branch), not the string "fuzz" that satisfies neither.
        // Which bool depends on clause order — either is correct.
        assert!(
            ctx.get("ready").is_some_and(Value::is_boolean),
            "ready must be a bool that routes a branch, got {:?}",
            ctx.get("ready")
        );
    }

    #[test]
    fn definition_wide_context_lets_initial_context_win() {
        let def = json!({
            "initialState": "a",
            "initialContext": { "mode": "declared" },
            "states": {
                "a": { "transitions": { "go": {
                    "target": "done", "actor": "deterministic", "executor": { "kind": "noop" },
                    "guards": [ { "kind": "expr", "expr": "$.context.mode == 'guard'" } ] } } },
                "done": { "terminal": true }
            }
        });
        let ctx = definition_wide_context(&def, &json!({}));
        assert_eq!(
            ctx.get("mode"),
            Some(&json!("declared")),
            "initialContext is the author's declared truth and must win"
        );
    }
}
