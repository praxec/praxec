//! Build the context a transition needs to FIRE (all `$.context.*` reads
//! populated; guard slots at satisfying values) and a context that VIOLATES a
//! guard (one guard slot flipped, but still PRESENT).

use crate::analysis::dummy::dummy_for_schema;
use crate::analysis::expr::{parse_clause, satisfying_value};
use serde_json::{Map, Value};

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
    let mut slots: Vec<String> = Vec::new();
    collect_reads(transition, &mut slots);
    // Deduplicate while preserving first-seen order
    let mut seen = std::collections::HashSet::new();
    slots.retain(|s| seen.insert(s.clone()));
    slots
}

fn collect_reads(value: &Value, slots: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            // A single string value may contain multiple `$.context.*` references
            // (e.g. a guard expr like "$.context.approved == true" or a template
            // that mentions several slots). Scan for all occurrences.
            let mut haystack: &str = s.as_str();
            while let Some(idx) = haystack.find("$.context.") {
                let rest = &haystack[idx + "$.context.".len()..];
                // Take the first path segment: stop at '.', '[', whitespace, or
                // operator characters ('=', '!', '<', '>', ' ').
                let slot: String = rest
                    .chars()
                    .take_while(|c| {
                        !matches!(c, '.' | '[' | ' ' | '=' | '!' | '<' | '>' | '\t' | '\n')
                    })
                    .collect();
                if !slot.is_empty() {
                    slots.push(slot);
                }
                // Advance past this occurrence to find further refs in the same string
                haystack = &haystack[idx + "$.context.".len()..];
            }
        }
        Value::Array(arr) => {
            for item in arr {
                collect_reads(item, slots);
            }
        }
        Value::Object(map) => {
            for (_k, v) in map {
                collect_reads(v, slots);
            }
        }
        _ => {}
    }
}

/// A context object that satisfies the transition's guards and populates every
/// read (guard slots at satisfying values; other reads get a dummy string).
pub fn satisfying_context(transition: &Value, blackboard: &Value) -> Value {
    let mut map: Map<String, Value> = Map::new();

    // First pass: guard clauses (top-level AND branch `when`) → satisfying values.
    for expr_str in all_guard_exprs(transition) {
        if let Some(clause) = parse_clause(&expr_str) {
            let top_slot = clause
                .slot
                .split('.')
                .next()
                .unwrap_or(&clause.slot)
                .to_owned();
            let val =
                satisfying_value(&clause).unwrap_or_else(|| typed_dummy(blackboard, &top_slot));
            map.insert(top_slot, val);
        }
    }

    // Second pass: all other reads get a blackboard-TYPED dummy (not a bare
    // string) so the seeded context matches each slot's declared type.
    for slot in context_reads(transition) {
        if !map.contains_key(&slot) {
            let val = typed_dummy(blackboard, &slot);
            map.insert(slot, val);
        }
    }

    Value::Object(map)
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
}
