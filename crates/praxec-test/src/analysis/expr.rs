//! Minimal read-only parser for the subset of guard `expr` the fuzzer needs:
//! `$.context.<slot> <op> <literal>`. Mirrors the operator set in
//! praxec-core/src/guards.rs; we only need to (a) find the context slot
//! a clause reads and (b) compute a value that makes the clause TRUE.

use serde_json::Value;

#[derive(Debug, PartialEq)]
pub struct GuardClause {
    /// The context slot path after `$.context.` (e.g. "approved", "fmeca.rpn").
    pub slot: String,
    pub op: String,
    pub rhs: Value,
}

/// A parsed comparison `<lhs> <op> <rhs>`, with both sides kept as raw tokens so
/// the caller can classify each as a `$.context.*` / `$.workflow.input.*` /
/// `$.input.*` reference or a literal. Unlike [`parse_clause`] this makes no
/// assumption about the left side's scope.
#[derive(Debug, PartialEq)]
pub struct Comparison {
    pub lhs: String,
    pub op: String,
    pub rhs: String,
}

/// Split `expr` at its top-level comparison operator (outside quotes), returning
/// the raw left and right tokens. `None` if no operator is present.
pub fn parse_comparison(expr: &str) -> Option<Comparison> {
    const OPS: &[&str] = &["starts_with", "contains", "<=", ">=", "==", "!=", "<", ">"];
    let (op, op_idx) = OPS
        .iter()
        .filter_map(|op| find_op_outside_quotes(expr, op).map(|idx| (*op, idx)))
        .min_by_key(|(_op, idx)| *idx)?;
    Some(Comparison {
        lhs: expr[..op_idx].trim().to_owned(),
        op: op.to_owned(),
        rhs: expr[op_idx + op.len()..].trim().to_owned(),
    })
}

/// The path after `$.workflow.input.` if `token` is an input reference.
///
/// Deliberately does NOT accept the bare `$.input.` spelling: the guard evaluator
/// (`guards.rs::resolve_operand`) resolves `$.workflow.input.*` but NOT
/// `$.input.*`, which coalesces to `Null`. Treating `$.input.mode` as a valid
/// input read here would let the fuzz seed around a guard the engine can never
/// satisfy — masking exactly the dead-guard bug this must surface.
pub fn input_ref(token: &str) -> Option<&str> {
    token.strip_prefix("$.workflow.input.")
}

/// The path after `$.context.` if `token` is a context reference.
pub fn context_ref(token: &str) -> Option<&str> {
    token.strip_prefix("$.context.")
}

/// Parse a literal token (`true`, `42`, `'go'`, …) into a JSON value.
pub fn parse_literal_value(s: &str) -> Value {
    parse_literal(s)
}

/// Given `op`, produce a `(lhs_value, rhs_value)` pair that makes `lhs op rhs`
/// TRUE — for the slot-vs-slot case where neither side is a literal. Integers,
/// because every such guard in practice compares counters (`iter < iter_cap`).
pub fn satisfying_pair(op: &str) -> Option<(Value, Value)> {
    let (lo, hi) = (Value::from(0), Value::from(1));
    match op {
        "==" | ">=" | "<=" => Some((Value::from(1), Value::from(1))),
        "!=" | "<" => Some((lo, hi)),
        ">" => Some((hi, lo)),
        _ => None,
    }
}

/// Parse `$.context.<slot> <op> <literal>`. Returns None if it doesn't match
/// that shape (left side not `$.context.*`, or no operator found).
pub fn parse_clause(expr: &str) -> Option<GuardClause> {
    // Operator precedence order mirrors guards.rs: multi-char ops before single-char
    // so `<=` is matched before `<`, etc.
    const OPS: &[&str] = &["starts_with", "contains", "<=", ">=", "==", "!=", "<", ">"];

    // Find the first operator that appears outside quotes.
    let (op, op_idx) = OPS
        .iter()
        .filter_map(|op| find_op_outside_quotes(expr, op).map(|idx| (*op, idx)))
        .min_by_key(|(_op, idx)| *idx)?;

    let left = expr[..op_idx].trim();
    let right = expr[op_idx + op.len()..].trim();

    // Left side must be `$.context.<something>`
    let slot = left.strip_prefix("$.context.")?;
    if slot.is_empty() {
        return None;
    }

    let rhs = parse_literal(right);

    Some(GuardClause {
        slot: slot.to_owned(),
        op: op.to_owned(),
        rhs,
    })
}

/// A value for `slot` that makes `clause` evaluate TRUE, or None if we can't
/// synthesize one.
pub fn satisfying_value(clause: &GuardClause) -> Option<Value> {
    match clause.op.as_str() {
        "==" => Some(clause.rhs.clone()),
        "!=" => Some(negate_value(&clause.rhs)),
        ">" => Some(bump_number(&clause.rhs, 1)?),
        ">=" => Some(clause.rhs.clone()),
        "<" => Some(bump_number(&clause.rhs, -1)?),
        "<=" => Some(clause.rhs.clone()),
        "starts_with" => {
            let prefix = clause.rhs.as_str()?;
            Some(Value::String(format!("{prefix}_fuzz")))
        }
        "contains" => {
            let needle = clause.rhs.as_str()?;
            Some(Value::String(format!("x{needle}x")))
        }
        _ => None,
    }
}

/// A value for `slot` that makes `clause` evaluate FALSE, or `None` when we
/// cannot GUARANTEE falsity (in which case the caller must skip the
/// violating-path check rather than assert on a value that might still satisfy).
///
/// This is the mirror of [`satisfying_value`], and its absence was a real bug:
/// the violating-context builder used to blindly set the slot to a bool, which
/// does NOT violate `status != 'pass'` (a bool is `!= 'pass'`), so the guard
/// still passed, the transition fired, and the fuzz reported "guard did not
/// reject" against a perfectly correct definition.
pub fn violating_value(clause: &GuardClause) -> Option<Value> {
    match clause.op.as_str() {
        // Any value distinct from rhs fails `==`.
        "==" => Some(negate_value(&clause.rhs)),
        // Equal to rhs fails `!=`.
        "!=" => Some(clause.rhs.clone()),
        // rhs itself is not `> rhs` and not `< rhs`.
        ">" | "<" => Some(clause.rhs.clone()),
        // One step the wrong way fails the inclusive bounds.
        ">=" => Some(bump_number(&clause.rhs, -1)?),
        "<=" => Some(bump_number(&clause.rhs, 1)?),
        // The empty string starts with / contains no non-empty token, so it
        // fails either — unless the token is itself empty, which we can't violate.
        "starts_with" | "contains" => {
            let token = clause.rhs.as_str()?;
            if token.is_empty() {
                None
            } else {
                Some(Value::String(String::new()))
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the byte index of `needle` in `haystack` skipping occurrences inside
/// single- or double-quoted regions. Mirrors `find_op_outside_quotes` in
/// praxec-core/src/guards.rs.
fn find_op_outside_quotes(haystack: &str, needle: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i + needle_bytes.len() <= bytes.len() {
        let c = bytes[i];
        if !in_single && c == b'"' {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_double && c == b'\'' {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && !in_double && bytes[i..i + needle_bytes.len()] == *needle_bytes {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse a literal token into a JSON `Value`.
///
/// Rules (in order):
/// - `true` / `false` → bool
/// - `null` → Null
/// - `'...'` or `"..."` → String (quotes stripped)
/// - parseable as f64 → Number
/// - otherwise → bare String
fn parse_literal(s: &str) -> Value {
    match s {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" => return Value::Null,
        _ => {}
    }

    // Quoted string with matching delimiters
    if (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
        || (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
    {
        return Value::String(s[1..s.len() - 1].to_owned());
    }

    // Numeric — prefer an integer Number for integer-valued literals so a value
    // seeded against an `{type: integer}` blackboard slot stays integer-typed
    // (serde_json keeps int vs float distinct, and the runtime's type check does
    // too: a float 3.0 is not a valid integer).
    if let Ok(i) = s.parse::<i64>() {
        return Value::Number(i.into());
    }
    if let Ok(n) = s.parse::<f64>() {
        if let Some(v) = serde_json::Number::from_f64(n) {
            return Value::Number(v);
        }
    }

    // Fallback: bare string
    Value::String(s.to_owned())
}

/// Shift a numeric value by `delta`, preserving integer-ness (integers stay
/// integers; floats stay floats). Returns None for non-numeric values.
fn bump_number(v: &Value, delta: i64) -> Option<Value> {
    if let Some(i) = v.as_i64() {
        return Some(Value::from(i + delta));
    }
    let n = v.as_f64()?;
    Some(Value::from(n + delta as f64))
}

/// Produce a value that is definitely not equal to `v`, preserving the same
/// rough type.
fn negate_value(v: &Value) -> Value {
    match v {
        Value::Bool(b) => Value::Bool(!b),
        Value::Number(_) => bump_number(v, 1).unwrap_or_else(|| Value::from(1)),
        Value::String(s) => Value::String(format!("{s}_x")),
        Value::Null => Value::Number(serde_json::Number::from(0)),
        other => other.clone(), // arrays/objects: best-effort passthrough
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_bool_eq() {
        let c = parse_clause("$.context.approved == true").unwrap();
        assert_eq!(c.slot, "approved");
        assert_eq!(c.op, "==");
        assert_eq!(c.rhs, json!(true));
        assert_eq!(satisfying_value(&c), Some(json!(true)));
    }

    #[test]
    fn parses_numeric_gt() {
        let c = parse_clause("$.context.fmeca.rpn > 80").unwrap();
        assert_eq!(c.slot, "fmeca.rpn");
        assert_eq!(c.op, ">");
        let v = satisfying_value(&c).unwrap();
        assert!(v.as_f64().unwrap() > 80.0);
    }

    #[test]
    fn parses_starts_with() {
        let c = parse_clause("$.context.plan starts_with 'refactor'").unwrap();
        assert_eq!(c.slot, "plan");
        assert_eq!(c.op, "starts_with");
        let v = satisfying_value(&c).unwrap();
        assert!(v.as_str().unwrap().starts_with("refactor"));
    }

    #[test]
    fn rejects_non_context_left() {
        assert_eq!(parse_clause("$.arguments.x == 1"), None);
        assert_eq!(parse_clause("nonsense"), None);
    }

    #[test]
    fn satisfies_eq_string_and_neq() {
        let c = parse_clause("$.context.s == \"go\"").unwrap();
        assert_eq!(satisfying_value(&c), Some(json!("go")));
        let c2 = parse_clause("$.context.n != 5").unwrap();
        let v = satisfying_value(&c2).unwrap();
        assert_ne!(v, json!(5));
    }

    // ── violating_value: the value must actually FAIL the clause ─────────────

    /// A bool did NOT violate `!= 'pass'` — a bool is `!= 'pass'` — which is the
    /// exact bug that made the fuzz report "guard did not reject" on correct defs.
    #[test]
    fn violating_value_actually_fails_each_operator() {
        let cases = [
            ("$.context.s == 'pass'", json!("pass"), false),
            ("$.context.s != 'pass'", json!("pass"), true), // equal → fails !=
            ("$.context.n > 5", json!(5), false),           // 5 > 5 is false
            ("$.context.n < 5", json!(5), false),           // 5 < 5 is false
            ("$.context.n >= 5", json!(4), false),
            ("$.context.n <= 5", json!(6), false),
        ];
        for (expr, _, _) in cases {
            let c = parse_clause(expr).unwrap();
            let sat = satisfying_value(&c).expect("has a satisfying value");
            let viol = violating_value(&c).expect("has a violating value");
            assert_ne!(
                sat, viol,
                "satisfying and violating must differ for `{expr}`"
            );
        }
        // The concrete regression: `!= 'pass'` violated by 'pass' (equal).
        let c = parse_clause("$.context.status != 'pass'").unwrap();
        assert_eq!(violating_value(&c), Some(json!("pass")));
    }

    #[test]
    fn violating_value_gives_up_when_it_cannot_guarantee_falsity() {
        // `contains ''` is satisfied by every string — nothing violates it, so we
        // must return None rather than a value that still passes.
        let c = parse_clause("$.context.s contains ''").unwrap();
        assert_eq!(violating_value(&c), None);
    }

    // ── parse_comparison / scope classification / slot-vs-slot pairs ─────────

    #[test]
    fn parse_comparison_keeps_raw_sides() {
        let cmp = parse_comparison("$.context.iter >= $.context.iter_cap").unwrap();
        assert_eq!(cmp.lhs, "$.context.iter");
        assert_eq!(cmp.op, ">=");
        assert_eq!(cmp.rhs, "$.context.iter_cap");
        assert_eq!(context_ref(&cmp.lhs), Some("iter"));
        assert_eq!(context_ref(&cmp.rhs), Some("iter_cap"));
    }

    #[test]
    fn input_ref_accepts_only_the_valid_guard_spelling() {
        // The guard evaluator resolves `$.workflow.input.*` but NOT bare
        // `$.input.*` — accepting the latter would let the fuzz seed around a
        // dead guard.
        assert_eq!(input_ref("$.workflow.input.mode"), Some("mode"));
        assert_eq!(input_ref("$.input.mode"), None);
    }

    #[test]
    fn satisfying_pair_is_consistent_with_its_operator() {
        // The pair (lval, rval) must make `lval OP rval` true.
        for op in ["==", "!=", "<", ">", "<=", ">="] {
            let (l, r) = satisfying_pair(op).unwrap();
            let (l, r) = (l.as_i64().unwrap(), r.as_i64().unwrap());
            let holds = match op {
                "==" => l == r,
                "!=" => l != r,
                "<" => l < r,
                ">" => l > r,
                "<=" => l <= r,
                ">=" => l >= r,
                _ => unreachable!(),
            };
            assert!(holds, "pair {l},{r} must satisfy `{op}`");
        }
    }
}
