use anyhow::anyhow;
use serde_json::{Value, json};

use crate::run_env::RunEnv;

/// Apply an output-mapping object to the workflow's context.
///
/// Each mapping value is either:
/// - A **path string** like `"$.output.plan"` resolved against
///   `executor_output`, or any of the broader scopes (`$.context.*`,
///   `$.arguments.*`, `$.workflow.input.*`).
/// - An **operator object** for declarative computation: `{ add: [a, b] }`,
///   `{ subtract: […] }`, `{ multiply: […] }`, `{ divide: […] }`,
///   `{ set: <literal> }`. Operands may themselves be path strings or literal
///   numbers.
/// - Any other JSON literal — used as the value verbatim.
///
/// Numeric operations treat missing/null operands as 0 so a counter can be
/// incremented even before it's first written.
pub fn merge_output(
    context: &mut Value,
    mapping: Option<&Value>,
    arguments: &Value,
    workflow_input: &Value,
    executor_output: &Value,
    run_env: Option<&RunEnv>,
) -> anyhow::Result<()> {
    let Some(mapping) = mapping.and_then(Value::as_object) else {
        return Ok(());
    };

    if !context.is_object() {
        return Err(anyhow!("workflow context must be an object"));
    }

    // Collect first so we can read context while building. The borrow checker
    // doesn't love &mut context + &context simultaneously.
    let pending: Vec<(String, Value)> = mapping
        .iter()
        .map(|(k, spec)| {
            let v = resolve_value(
                spec,
                arguments,
                context,
                workflow_input,
                executor_output,
                run_env,
            );
            (k.clone(), v)
        })
        .collect();

    // Invariant: the `is_object()` guard above already proved this is
    // an object — the `as_object_mut()` cannot return None here.
    let obj = context
        .as_object_mut()
        .expect("invariant: context.is_object() checked above");
    for (k, v) in pending {
        obj.insert(k, v);
    }
    Ok(())
}

/// Resolve a single mapping value against the available scopes.
///
/// Public so other parts of the runtime (link prefill, executor maps) can
/// reuse the same expression syntax — string paths, operator objects
/// (`{ add: [a, b] }`, `{ set: x }`, etc.), or literal pass-through.
///
/// FALLBACK-05: this function is infallible by design — a MALFORMED operator
/// (wrong arity, non-numeric/non-array operands) resolves to `Value::Null`
/// here rather than erroring. That is SAFE only because malformed operator
/// shapes are now rejected at config-load time by
/// `validate::validate_output_operator_shapes`, so they cannot reach this
/// runtime path. The one intentional runtime null is divide-by-zero (a
/// data-dependent condition that can't be caught at load). Do NOT relax the
/// load-time validator without also making this function fallible.
pub fn resolve_value(
    spec: &Value,
    arguments: &Value,
    context: &Value,
    workflow_input: &Value,
    executor_output: &Value,
    run_env: Option<&RunEnv>,
) -> Value {
    match spec {
        Value::String(s) => {
            // Strings starting with "$." are path expressions; everything
            // else is a literal. Lets authors write `base: "main"` instead
            // of having to wrap every literal in `{ set: "main" }`.
            if s.starts_with("$.") || s == "$" {
                read_in_scopes(
                    s,
                    arguments,
                    context,
                    workflow_input,
                    Some(executor_output),
                    run_env,
                )
                .unwrap_or(Value::Null)
            } else {
                Value::String(s.clone())
            }
        }

        Value::Object(obj) if obj.len() == 1 => {
            // Invariant: the `len() == 1` match guard above guarantees
            // iter().next() yields Some.
            let (op, args) = obj
                .iter()
                .next()
                .expect("invariant: obj.len() == 1 checked in match guard");
            match op.as_str() {
                "set" => args.clone(),

                "add" | "subtract" | "multiply" | "divide" => {
                    let nums = match resolve_operands(
                        args,
                        arguments,
                        context,
                        workflow_input,
                        executor_output,
                        run_env,
                    ) {
                        Some(n) => n,
                        None => return Value::Null,
                    };
                    if nums.len() != 2 {
                        return Value::Null;
                    }
                    let (a, b) = (nums[0], nums[1]);
                    let result = match op.as_str() {
                        "add" => a + b,
                        "subtract" => a - b,
                        "multiply" => a * b,
                        "divide" => {
                            if b == 0.0 {
                                return Value::Null;
                            }
                            a / b
                        }
                        _ => unreachable!(),
                    };
                    json_number(result)
                }

                "concat" => {
                    let parts = match args.as_array() {
                        Some(arr) => arr,
                        None => return Value::Null,
                    };
                    let mut result = String::new();
                    for part in parts {
                        let resolved = resolve_value(
                            part,
                            arguments,
                            context,
                            workflow_input,
                            executor_output,
                            run_env,
                        );
                        match resolved {
                            Value::String(s) => result.push_str(&s),
                            Value::Number(n) => result.push_str(&n.to_string()),
                            Value::Bool(b) => result.push_str(&b.to_string()),
                            Value::Null => result.push_str("null"),
                            other => {
                                result.push_str(&serde_json::to_string(&other).unwrap_or_default())
                            }
                        }
                    }
                    Value::String(result)
                }

                // `{ pick: { from, by, eq } }` — the FIRST element of the
                // `from` array whose `by` dot-path equals the resolved `eq`
                // (serde_json `Value` equality — a string `eq` matches string
                // element keys, a numeric `eq` matches numeric ones; numbers
                // are NOT string-rendered). `from` and `eq` resolve like any
                // other operand: path strings or literals. No match, a
                // non-array `from`, or a malformed declaration resolve to
                // `Value::Null` (FALLBACK-05 — this function stays
                // infallible). The governed fail-fast fence lives at the
                // submit-time choice guard (CHOICE_MISMATCH), NOT here: the
                // guard proves the chosen key is in-set before a gate submit
                // ever reaches this operator.
                "pick" => {
                    let Some(decl) = args.as_object() else {
                        return Value::Null;
                    };
                    let (Some(from_spec), Some(by), Some(eq_spec)) = (
                        decl.get("from"),
                        decl.get("by").and_then(Value::as_str),
                        decl.get("eq"),
                    ) else {
                        return Value::Null;
                    };
                    let from = resolve_value(
                        from_spec,
                        arguments,
                        context,
                        workflow_input,
                        executor_output,
                        run_env,
                    );
                    let Some(items) = from.as_array() else {
                        return Value::Null;
                    };
                    let eq = resolve_value(
                        eq_spec,
                        arguments,
                        context,
                        workflow_input,
                        executor_output,
                        run_env,
                    );
                    let by_pointer = crate::guards::path_to_pointer(by);
                    items
                        .iter()
                        .find(|element| element.pointer(&by_pointer) == Some(&eq))
                        .cloned()
                        .unwrap_or(Value::Null)
                }

                _ => spec.clone(),
            }
        }

        other => other.clone(),
    }
}

/// Parse the operands of an arithmetic operator. Each operand is either a
/// path string or a literal number; missing/null path resolutions become 0.
fn resolve_operands(
    spec: &Value,
    arguments: &Value,
    context: &Value,
    workflow_input: &Value,
    executor_output: &Value,
    run_env: Option<&RunEnv>,
) -> Option<Vec<f64>> {
    let arr = spec.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let resolved = match v {
            Value::String(s) => read_in_scopes(
                s,
                arguments,
                context,
                workflow_input,
                Some(executor_output),
                run_env,
            )
            .unwrap_or(Value::Null),
            other => other.clone(),
        };
        let n = match &resolved {
            Value::Null => 0.0,
            Value::Number(n) => n.as_f64().unwrap_or(0.0),
            _ => return None,
        };
        out.push(n);
    }
    Some(out)
}

fn json_number(n: f64) -> Value {
    if n.is_finite() {
        // Prefer integers when round.
        if n.fract() == 0.0 && n.abs() <= i64::MAX as f64 {
            return json!(n as i64);
        }
        json!(n)
    } else {
        Value::Null
    }
}

/// Reads any of the supported expression roots against the relevant scopes.
/// Used by the CLI executor and similar places that need late-bound values.
///
/// SPEC §24 — supports bracket-wildcard **array projection** via `[*]`.
/// `$.output.branches[*].field` resolves `branches` to an array (under the
/// `$.output` root) and plucks `field` from each element, returning a JSON
/// array of plucked values in original order. `[*]` against a non-array
/// returns `None` (consistent with the existing unresolved-path contract).
/// Multiple `[*]` in the same path are NOT supported in v1 — only the
/// first wildcard expands; subsequent literal segments treat the projected
/// array's elements as individual roots.
pub fn read_in_scopes(
    expr: &str,
    arguments: &Value,
    context: &Value,
    workflow_input: &Value,
    executor_output: Option<&Value>,
    run_env: Option<&RunEnv>,
) -> Option<Value> {
    // Whole-scope reads — symmetric with `$` / `$.output` (the whole output). An
    // author writing `fix_result: "$.arguments"` means "the whole submission",
    // exactly as `$` means the whole executor output.
    match expr {
        "$.arguments" => return Some(arguments.clone()),
        "$.context" => return Some(context.clone()),
        "$.workflow.input" => return Some(workflow_input.clone()),
        // Run-ambient root (v0.0.21). Resolves ONLY when the caller supplies the
        // run env (an executor/instance context); load-time/validation callers
        // pass `None` and get an unresolved read, exactly like any other scope
        // they can't see. Kept in lockstep with `is_resolvable_write_scope`.
        "$.run.repo_root" => {
            return run_env.map(|e| Value::String(e.repo_root.as_str().to_string()));
        }
        // Run-scoped evidence dir (engine-created). Resolves only with a run env
        // and a minted run_ref; a load-time caller gets an unresolved read.
        "$.run.artifacts_dir" => {
            return run_env
                .and_then(|e| e.artifacts_dir())
                .map(|p| Value::String(p.to_string_lossy().into_owned()));
        }
        _ => {}
    }
    // Run-scoped exclusive-pool lease: `$.run.leased.<pool>` → the leased
    // connection name. Resolves only when the run env is supplied (executor
    // context); a load-time caller passes `None` and gets an unresolved read.
    if let Some(pool) = expr.strip_prefix("$.run.leased.") {
        return run_env
            .and_then(|e| e.leased_member(pool))
            .map(|m| Value::String(m.to_string()));
    }
    if let Some(path) = expr.strip_prefix("$.arguments.") {
        return resolve_path_with_projection(arguments, path);
    }
    if let Some(path) = expr.strip_prefix("$.context.") {
        return resolve_path_with_projection(context, path);
    }
    if let Some(path) = expr.strip_prefix("$.workflow.input.") {
        return resolve_path_with_projection(workflow_input, path);
    }
    if let Some(out) = executor_output {
        if expr == "$.output" || expr == "$" {
            return Some(out.clone());
        }
        if let Some(path) = expr.strip_prefix("$.output.") {
            return resolve_path_with_projection(out, path);
        }
    }
    None
}

/// Does `s` name a scope that [`read_in_scopes`] resolves on the WRITE side
/// (transition `output:`, `onEnter.output:`, `prefill:`, and their operator
/// operands)? A `false` for a `$.`-rooted string means [`resolve_value`] would
/// coalesce it to `null` and silently write that null to the blackboard — the
/// write-side analog of the `$.input.mode` dead-guard bug.
///
/// This is the single source of truth for the write-side scope set: `read_in_scopes`
/// dispatches on exactly these prefixes, and a poka-yoke test asserts the two can't
/// drift. V27 (`validate.rs`) rejects an unrecognized `$.`-rooted write operand at load.
pub fn is_resolvable_write_scope(s: &str) -> bool {
    let s = s.trim();
    matches!(
        s,
        "$" | "$.output" | "$.arguments" | "$.context" | "$.workflow.input" | "$.run.repo_root"
    ) || s.starts_with("$.output.")
        || s.starts_with("$.context.")
        || s.starts_with("$.arguments.")
        || s.starts_with("$.workflow.input.")
}

/// Resolve a dot-separated path against `root`, with `[*]` projection
/// support. Falls back to plain JSON Pointer when no `[*]` is present.
fn resolve_path_with_projection(root: &Value, path: &str) -> Option<Value> {
    // No wildcard → plain JSON Pointer (legacy path).
    if !path.contains("[*]") {
        return root
            .pointer(&format!("/{}", path.replace('.', "/")))
            .cloned();
    }
    // Split on FIRST `[*]`. Prefix is the array root; suffix (if any) is
    // plucked from each element. `prefix` may be empty when path starts
    // with `[*]` (e.g. raw `[*].x` against a Vec root — unusual).
    let (prefix, suffix_after) = path.split_once("[*]")?;
    let prefix_clean = prefix.trim_end_matches('.');
    let array = if prefix_clean.is_empty() {
        root.clone()
    } else {
        root.pointer(&format!("/{}", prefix_clean.replace('.', "/")))
            .cloned()?
    };
    let arr = array.as_array()?;
    let suffix = suffix_after.trim_start_matches('.');
    let projected: Vec<Value> = arr
        .iter()
        .map(|element| {
            if suffix.is_empty() {
                element.clone()
            } else {
                // Recurse: support nested `[*]` in the suffix.
                resolve_path_with_projection(element, suffix).unwrap_or(Value::Null)
            }
        })
        .collect();
    Some(Value::Array(projected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// POKA-YOKE: `is_resolvable_write_scope` must agree with what `read_in_scopes`
    /// actually resolves. A scope the predicate calls resolvable must resolve to
    /// `Some` (given present data); one it rejects must resolve to `None`. If a
    /// scope arm is added to `read_in_scopes` without updating the predicate (or
    /// vice-versa), this fails.
    #[test]
    fn write_scope_predicate_agrees_with_read_in_scopes() {
        let args = json!({ "a": 1 });
        let ctx = json!({ "c": 1 });
        let input = json!({ "i": 1 });
        let output = json!({ "o": 1 });
        let run_env = crate::RunEnv::for_test();

        let cases = [
            ("$.arguments.a", true),
            ("$.context.c", true),
            ("$.workflow.input.i", true),
            ("$.output.o", true),
            ("$.output", true),
            ("$", true),
            // Whole-scope reads (symmetric with `$`).
            ("$.arguments", true),
            ("$.context", true),
            ("$.workflow.input", true),
            // Run-ambient root (v0.0.21) — resolvable on both sides.
            ("$.run.repo_root", true),
            ("$.run.bogus", false),       // no other `$.run.*` is a scope
            ("$.run.repo_root.x", false), // adversarial: exact match, NOT a prefix
            ("$.run.repo_rootx", false),  // adversarial: no fuzzy suffix match
            ("$.run", false),             // adversarial: bare namespace
            ("$.run.", false),            // adversarial: dangling dot
            ("$.input.mode", false),      // the bug — not a write scope
            ("$.outpt.plan", false),      // typo
            ("$.ctx.c", false),
        ];
        for (expr, resolvable) in cases {
            assert_eq!(
                is_resolvable_write_scope(expr),
                resolvable,
                "predicate wrong for `{expr}`"
            );
            let resolved =
                read_in_scopes(expr, &args, &ctx, &input, Some(&output), Some(&run_env)).is_some();
            assert_eq!(
                resolved, resolvable,
                "read_in_scopes disagrees with predicate for `{expr}`"
            );
        }
    }

    /// Resolve a `pick` spec against a candidates array in context and a
    /// chosen id in arguments — the shape the HITL choice gates use.
    fn pick(spec: Value, context: Value, arguments: Value) -> Value {
        resolve_value(&spec, &arguments, &context, &json!({}), &json!({}), None)
    }

    #[test]
    fn pick_selects_the_matching_element() {
        let ctx = json!({ "candidates": [
            { "id": "a", "name": "Alpha" },
            { "id": "b", "name": "Beta" },
        ]});
        let spec = json!({ "pick": {
            "from": "$.context.candidates", "by": "id", "eq": "$.arguments.chosen_id"
        }});
        let got = pick(spec, ctx, json!({ "chosen_id": "b" }));
        assert_eq!(got, json!({ "id": "b", "name": "Beta" }));
    }

    #[test]
    fn pick_no_match_is_null() {
        let ctx = json!({ "candidates": [{ "id": "a" }] });
        let spec = json!({ "pick": {
            "from": "$.context.candidates", "by": "id", "eq": "$.arguments.chosen_id"
        }});
        assert_eq!(pick(spec, ctx, json!({ "chosen_id": "z" })), Value::Null);
    }

    #[test]
    fn pick_over_non_array_is_null() {
        let spec = json!({ "pick": {
            "from": "$.context.candidates", "by": "id", "eq": "a"
        }});
        // Non-array and absent `from` both coalesce to null.
        for ctx in [json!({ "candidates": "not-an-array" }), json!({})] {
            assert_eq!(pick(spec.clone(), ctx, json!({})), Value::Null);
        }
        // A malformed declaration (missing `by`) is null too — never a panic.
        let malformed = json!({ "pick": { "from": "$.context.candidates", "eq": "a" } });
        assert_eq!(
            pick(
                malformed,
                json!({ "candidates": [{ "id": "a" }] }),
                json!({})
            ),
            Value::Null
        );
    }

    #[test]
    fn pick_operands_may_be_paths_or_literals() {
        // Literal `from` array + literal `eq`.
        let spec = json!({ "pick": {
            "from": [{ "id": "a" }, { "id": "b" }], "by": "id", "eq": "b"
        }});
        assert_eq!(pick(spec, json!({}), json!({})), json!({ "id": "b" }));
        // Path `from` + literal numeric `eq` — Value equality, no string
        // rendering of numbers.
        let ctx = json!({ "candidates": [{ "id": 1 }, { "id": 2 }] });
        let spec = json!({ "pick": { "from": "$.context.candidates", "by": "id", "eq": 2 } });
        assert_eq!(pick(spec, ctx, json!({})), json!({ "id": 2 }));
    }

    /// Adversarial: `$.run.repo_root` is a single leaf, matched EXACTLY — a
    /// trailing path must not resolve, or a typo'd deeper read would silently
    /// coalesce to null at runtime.
    #[test]
    fn run_repo_root_is_an_exact_match_not_a_prefix() {
        let run_env = crate::RunEnv::for_test();
        let got = read_in_scopes(
            "$.run.repo_root.x",
            &json!({}),
            &json!({}),
            &json!({}),
            None,
            Some(&run_env),
        );
        assert!(
            got.is_none(),
            "a trailing path must not resolve, got {got:?}"
        );
    }
}
