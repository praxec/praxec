//! Shared argument templating for the `cli` and `script` executors.
//!
//! Both kinds materialize their argv from a config array, where each entry is
//! either a literal string or a `$.context.x` / `$.arguments.x` /
//! `$.workflow.input.x` path that resolves against the request's blackboard /
//! args / input scopes. The two executors had byte-identical copies of this
//! helper; this is the single source of truth.
//!
//! NOTE: the resolvable scope in an arg is `$.workflow.input.*`, NOT bare
//! `$.input.*` — `$.input.*` names no arg scope and used to slip through to the
//! shell as its literal token. See `is_scope_path`.

use praxec_core::error::ExecutorError;
use praxec_core::mapping::read_in_scopes;
use praxec_core::model::ExecuteRequest;
use serde_json::Value;

/// Render one config argument into a shell argv token. Literal strings pass
/// through unchanged; `$.context.x` / `$.arguments.x` / `$.workflow.input.x`
/// (and `$.output` / `$`) paths resolve against the request's scopes. A path
/// that resolves to a non-string JSON value is stringified.
///
/// FALLBACK-01: a string that MATCHES the `$.<scope>.…` path grammar but
/// fails to resolve is an ERROR (`ExecutorError::Permanent`), NOT a literal
/// pass-through. Previously a typo'd slot like `$.context.titel` reached the
/// shell as the literal token `$.context.titel`. A plain literal that was
/// never a path (no `$.` prefix) is still passed through verbatim.
pub(crate) fn render_arg(value: &Value, request: &ExecuteRequest) -> Result<String, ExecutorError> {
    let Some(raw) = value.as_str() else {
        return Ok(value.to_string());
    };

    if let Some(v) = read_in_scopes(
        raw,
        &request.arguments,
        &request.workflow.context,
        &request.workflow.input,
        None,
        Some(&request.workflow.run_env),
    ) {
        return Ok(match v {
            Value::String(s) => s,
            other => other.to_string(),
        });
    }

    // Resolution returned None. Distinguish a path that failed to resolve
    // (error) from a literal that was never a path (pass-through).
    if is_scope_path(raw) {
        return Err(ExecutorError::Permanent(format!(
            "unresolved arg path '{raw}'"
        )));
    }
    Ok(raw.to_string())
}

/// True iff `raw` LOOKS like a `$.`-rooted path expression (as opposed to a plain
/// literal flag). Any such token that failed to resolve is a FALLBACK-01 error,
/// not a silent literal pass-through.
///
/// This is deliberately broad — ANY `$.`-rooted token, not just the three
/// resolvable arg scopes. The narrow version silently passed an unresolvable but
/// path-SHAPED token (`$.input.gateway_config_path`, an `$.output.*` used where no
/// executor output exists, a typo'd `$.contxt.x`) straight to the shell as its
/// literal text. A token an author wrote as `$.<something>` is always a path
/// attempt; if it didn't resolve, that's a bug to surface, never a flag to run.
fn is_scope_path(raw: &str) -> bool {
    raw.starts_with("$.") || raw == "$"
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use praxec_core::model::WorkflowInstance;
    use serde_json::json;

    fn req(args: Value, context: Value) -> ExecuteRequest {
        ExecuteRequest {
            workflow: WorkflowInstance {
                id: "wf".into(),
                definition_id: "stub".into(),
                definition_version: "0".into(),
                definition: Value::Null,
                state: "s".into(),
                version: 0,
                input: json!({}),
                context,
                started_at: Utc::now(),
                run_env: praxec_core::RunEnv::for_test(),
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            transition: None,
            arguments: args,
            executor_config: Value::Null,
            idempotency_key: None,
            correlation_id: None,
        }
    }

    #[test]
    fn resolved_path_returns_value() {
        let r = req(json!({}), json!({ "title": "hello" }));
        let out = render_arg(&json!("$.context.title"), &r).expect("resolves");
        assert_eq!(out, "hello");
    }

    #[test]
    fn plain_literal_passes_through() {
        let r = req(json!({}), json!({}));
        // No `$.` prefix → never a path → literal pass-through (not an error).
        let out = render_arg(&json!("just-a-flag"), &r).expect("literal");
        assert_eq!(out, "just-a-flag");
    }

    #[test]
    fn unresolved_path_is_error_not_literal() {
        // FALLBACK-01: a typo'd slot must NOT reach the shell as its literal
        // `$.context.titel` text — it must error.
        let r = req(json!({}), json!({ "title": "hello" }));
        let err =
            render_arg(&json!("$.context.titel"), &r).expect_err("unresolved path must error");
        match err {
            ExecutorError::Permanent(msg) => {
                assert!(msg.contains("unresolved arg path"), "{msg}");
                assert!(msg.contains("$.context.titel"), "{msg}");
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[test]
    fn unresolved_arguments_path_is_error() {
        let r = req(json!({ "present": 1 }), json!({}));
        let err = render_arg(&json!("$.arguments.absent"), &r)
            .expect_err("unresolved arguments path must error");
        assert!(matches!(err, ExecutorError::Permanent(_)));
    }

    #[test]
    fn non_string_value_stringifies() {
        let r = req(json!({}), json!({}));
        let out = render_arg(&json!(42), &r).expect("number literal");
        assert_eq!(out, "42");
    }

    #[test]
    fn bare_dollar_input_is_an_error_not_a_literal() {
        // `$.input.x` is NOT a resolvable arg scope (the scope is
        // `$.workflow.input.x`). It used to slip through to the shell as its
        // literal token; now it fails fast.
        let mut r = req(json!({}), json!({}));
        r.workflow.input = json!({ "x": "val" });
        let err = render_arg(&json!("$.input.x"), &r).expect_err("must error");
        assert!(
            matches!(&err, ExecutorError::Permanent(m) if m.contains("$.input.x")),
            "{err:?}"
        );
    }

    #[test]
    fn workflow_input_path_resolves() {
        let mut r = req(json!({}), json!({}));
        r.workflow.input = json!({ "x": "val" });
        assert_eq!(
            render_arg(&json!("$.workflow.input.x"), &r).expect("resolves"),
            "val"
        );
    }
}
