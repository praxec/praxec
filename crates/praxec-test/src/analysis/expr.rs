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
}
