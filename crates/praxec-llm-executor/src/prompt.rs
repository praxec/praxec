//! SPEC §33 D5 — prompt + tool-definition builder for the in-runtime
//! LLM executor.
//!
//! Two responsibilities, kept together because they both translate the
//! HATEOAS-shaped per-turn state into provider-shaped inputs:
//!
//! 1. [`render_template`] — render `config.prompt_template` against the
//!    same `{$.blackboard, $.context, $.input}` scopes
//!    `praxec_executors::arg_render::render_arg` does, but with `{{ }}`
//!    placeholder semantics that mirror `core::templating::render_template`.
//!    Note: `$.blackboard.*` is a sugar alias for `$.context.*` (per
//!    project memory: blackboard == workflow.context).
//!
//! 2. [`links_to_tool_definitions`] — turn the guard-filtered link list
//!    [`TransitionResolver::available_transitions`] returns into the
//!    `Vec<aether_llm::tools::ToolDefinition>` the provider needs. Each
//!    link becomes exactly one tool — state-aware narrowing is already
//!    done upstream. SPEC §33 FMECA F7: duplicate `rel`s are rejected
//!    before the provider call as
//!    [`LlmErrorCode::DuplicateTransitionRel`].
//!
//! Both helpers are fail-fast: missing fields surface as
//! `ExecutorError::Llm(LlmErrorCode::*, …)`; nothing is silently
//! defaulted away from an obvious workflow-author bug.

use std::collections::HashMap;

use praxec_core::error::{ExecutorError, LlmErrorCode};
use praxec_core::mapping::read_in_scopes;
use praxec_core::model::ExecuteRequest;
use rig::completion::ToolDefinition;
use serde_json::Value;

/// Render `template` with `{{ $.path }}` placeholders resolved against
/// the request's blackboard/context/input scopes, exactly mirroring how
/// `core::templating::render_template` handles `goal:` and `guidance:`
/// strings.
///
/// Placeholder syntax mirrors core templating:
/// `{{` optional-whitespace `$.`-rooted-path optional-whitespace `}}`.
///
/// Supported roots:
/// - `$.context.*` and `$.blackboard.*` → `instance.context`
///   (blackboard is a sugar alias for context).
/// - `$.workflow.input.*` and `$.input.*` → `instance.input`
///   (`$.input.*` is the executor-scope sugar matching the spec text).
/// - `$.arguments.*` → executor request arguments.
/// - `$.workflow.id`, `$.workflow.state`, `$.workflow.version` →
///   scalar instance metadata.
///
/// Unresolved placeholders render as `(lastSegment: unset)` — same
/// stub convention as `core::templating`, so authors get a recognizable
/// rendered string instead of an opaque empty span. The function never
/// fails: returning a `String` keeps audit pipelines that capture the
/// rendered prompt for replay deterministic.
pub fn render_template(template: &str, request: &ExecuteRequest) -> String {
    let mut output = String::with_capacity(template.len());
    let mut remaining = template;

    while let Some(start) = remaining.find("{{") {
        output.push_str(&remaining[..start]);
        let after_open = &remaining[start + 2..];

        let Some(end_rel) = after_open.find("}}") else {
            // Unterminated `{{` — emit the rest literally and stop.
            output.push_str(&remaining[start..]);
            return output;
        };

        let inner = after_open[..end_rel].trim();
        if inner.is_empty() {
            output.push_str("{{}}");
        } else {
            output.push_str(&resolve_token(inner, request));
        }
        remaining = &after_open[end_rel + 2..];
    }

    output.push_str(remaining);
    output
}

/// Resolve one `{{ … }}` token against the request. Mirrors the
/// stub-on-miss semantics of `core::templating::resolve_template_path`
/// so audit replay output stays comparable across goal/guidance
/// templates and LLM prompt templates.
fn resolve_token(raw: &str, request: &ExecuteRequest) -> String {
    // Scalar instance metadata — no scope lookup needed.
    if raw == "$.workflow.id" {
        return request.workflow.id.clone();
    }
    if raw == "$.workflow.state" {
        return request.workflow.state.clone();
    }
    if raw == "$.workflow.version" {
        return request.workflow.definition_version.clone();
    }

    // `$.blackboard.*` is the executor-scope sugar; rewrite to
    // `$.context.*` so `read_in_scopes` (which knows the canonical
    // names) can resolve it.
    let normalized: String = if let Some(rest) = raw.strip_prefix("$.blackboard.") {
        format!("$.context.{rest}")
    } else if let Some(rest) = raw.strip_prefix("$.input.") {
        // `$.input.*` is the executor-scope sugar for
        // `$.workflow.input.*`. Both spellings must resolve the same
        // way; the canonical form is `$.workflow.input.*`.
        format!("$.workflow.input.{rest}")
    } else {
        raw.to_string()
    };

    match read_in_scopes(
        &normalized,
        &request.arguments,
        &request.workflow.context,
        &request.workflow.input,
        None,
        Some(&request.workflow.run_env),
    ) {
        Some(Value::String(s)) => s,
        Some(Value::Null) => "(null)".to_string(),
        Some(v) => v.to_string(),
        None => {
            let last = raw.rsplit('.').next().unwrap_or(raw);
            format!("({last}: unset)")
        }
    }
}

/// Convert the guard-filtered link list from `TransitionResolver` into
/// the provider's `ToolDefinition` shape.
///
/// Per-link mapping:
/// - `name`        ← link's `rel` field (REQUIRED; missing → `ExecutorError::Other`).
/// - `description` ← link's `title` field, else
///   `"Advance the workflow via the '{rel}' transition."`.
/// - `parameters`  ← JSON-stringify the link's `inputSchema` field,
///   else `{"type":"object","properties":{},"additionalProperties":false}`.
/// - `server`      ← `None` (the runtime IS the tool host).
///
/// SPEC §33 FMECA F7 — if two links share `rel`, the model's tool
/// selection would be ambiguous. The executor refuses to call the
/// provider and returns
/// `ExecutorError::Llm(LlmErrorCode::DuplicateTransitionRel, …)` with
/// the rel + state for operator triage.
pub fn links_to_tool_definitions(
    links: &[Value],
    state: &str,
) -> Result<Vec<ToolDefinition>, ExecutorError> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut tools: Vec<ToolDefinition> = Vec::with_capacity(links.len());

    for link in links {
        let Some(rel) = link.get("rel").and_then(Value::as_str) else {
            return Err(ExecutorError::Other(anyhow::anyhow!(
                "LLM executor: malformed link (no `rel`) at state '{state}': {link}"
            )));
        };

        *seen.entry(rel.to_string()).or_insert(0) += 1;

        let description = link
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("Advance the workflow via the '{rel}' transition."));

        // rig's ToolDefinition carries the JSON-Schema as a `Value` (not a
        // string), so the per-link `inputSchema` passes straight through.
        let parameters = match link.get("inputSchema") {
            Some(schema) => schema.clone(),
            None => serde_json::json!({
                "type": "object", "properties": {}, "additionalProperties": false
            }),
        };

        tools.push(ToolDefinition {
            name: rel.to_string(),
            description,
            parameters,
        });
    }

    // SPEC §33 FMECA F7 — reject before the provider call. Reporting
    // every offending rel (not just the first) helps the workflow
    // author fix the YAML in one round-trip.
    let duplicates: Vec<&str> = seen
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(rel, _)| rel.as_str())
        .collect();
    if !duplicates.is_empty() {
        let mut sorted = duplicates;
        sorted.sort_unstable();
        let listed = sorted
            .iter()
            .map(|r| format!("'{r}'"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ExecutorError::Llm(
            LlmErrorCode::DuplicateTransitionRel,
            format!(
                "DUPLICATE_TRANSITION_REL: rel(s) {listed} appear in 2+ \
                 available transitions at state '{state}'; tool selection \
                 would be ambiguous"
            ),
        ));
    }

    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxec_core::model::WorkflowInstance;
    use serde_json::json;

    fn make_request(arguments: Value, context: Value, input: Value) -> ExecuteRequest {
        ExecuteRequest {
            workflow: WorkflowInstance {
                id: "wf_test".into(),
                definition_id: "demo".into(),
                definition_version: "1.0.0".into(),
                definition: json!({}),
                state: "thinking".into(),
                version: 0,
                input,
                context,
                started_at: chrono::Utc::now(),
                run_env: praxec_core::RunEnv::for_test(),
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            transition: None,
            arguments,
            executor_config: json!({}),
            idempotency_key: None,
            correlation_id: None,
        }
    }

    #[test]
    fn render_template_literal_passthrough() {
        let req = make_request(json!({}), json!({}), json!({}));
        assert_eq!(render_template("hello world", &req), "hello world");
    }

    #[test]
    fn render_template_substitutes_context() {
        let req = make_request(json!({}), json!({ "goal": "ship it" }), json!({}));
        let s = render_template("Goal: {{ $.context.goal }}", &req);
        assert_eq!(s, "Goal: ship it");
    }

    #[test]
    fn render_template_substitutes_blackboard_sugar() {
        let req = make_request(json!({}), json!({ "goal": "ship it" }), json!({}));
        let s = render_template("Goal: {{ $.blackboard.goal }}", &req);
        assert_eq!(s, "Goal: ship it");
    }

    #[test]
    fn render_template_substitutes_input_sugar() {
        let req = make_request(json!({}), json!({}), json!({ "topic": "rust" }));
        let s = render_template("Topic: {{ $.input.topic }}", &req);
        assert_eq!(s, "Topic: rust");
    }

    #[test]
    fn render_template_unresolved_path_uses_stub() {
        let req = make_request(json!({}), json!({}), json!({}));
        let s = render_template("X={{ $.context.missing }}", &req);
        assert_eq!(s, "X=(missing: unset)");
    }

    #[test]
    fn render_template_unterminated_emits_verbatim_tail() {
        let req = make_request(json!({}), json!({}), json!({}));
        let s = render_template("ok {{ stuff", &req);
        assert_eq!(s, "ok {{ stuff");
    }

    #[test]
    fn render_template_workflow_state_scalar() {
        let req = make_request(json!({}), json!({}), json!({}));
        let s = render_template("state={{ $.workflow.state }}", &req);
        assert_eq!(s, "state=thinking");
    }

    #[test]
    fn links_to_tools_minimal_link_uses_defaults() {
        let links = vec![json!({ "rel": "advance" })];
        let tools = links_to_tool_definitions(&links, "thinking").unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "advance");
        assert!(tools[0].description.contains("advance"));
        // Default schema is an empty object schema, not a JSON Schema dialect URI.
        let parsed = &tools[0].parameters;
        assert_eq!(parsed["type"], "object");
        assert_eq!(parsed["additionalProperties"], false);
    }

    #[test]
    fn links_to_tools_uses_title_and_schema() {
        let links = vec![json!({
            "rel": "approve",
            "title": "Approve the proposal",
            "inputSchema": { "type": "object", "properties": { "note": { "type": "string" } } }
        })];
        let tools = links_to_tool_definitions(&links, "review").unwrap();
        assert_eq!(tools[0].description, "Approve the proposal");
        let parsed = &tools[0].parameters;
        assert_eq!(parsed["properties"]["note"]["type"], "string");
    }

    #[test]
    fn links_to_tools_duplicate_rel_rejected() {
        let links = vec![
            json!({ "rel": "advance" }),
            json!({ "rel": "advance", "title": "another one" }),
        ];
        let err = links_to_tool_definitions(&links, "thinking").unwrap_err();
        match err {
            ExecutorError::Llm(LlmErrorCode::DuplicateTransitionRel, msg) => {
                assert!(msg.contains("advance"), "msg missing rel: {msg}");
                assert!(msg.contains("thinking"), "msg missing state: {msg}");
            }
            other => panic!("expected DuplicateTransitionRel, got {other:?}"),
        }
    }

    /// SPEC §33 audit fixup (F6 ORPHAN-002): when MULTIPLE distinct
    /// rels are each duplicated (e.g. two `advance` + two `reject`),
    /// the error must name ALL of them so an operator debugging a
    /// fat-fingered workflow YAML doesn't have to play whack-a-mole.
    /// The duplicate enumeration is the operator-facing value of F7.
    #[test]
    fn links_to_tools_multiple_duplicate_rels_all_named_in_error() {
        let links = vec![
            json!({ "rel": "advance" }),
            json!({ "rel": "advance", "title": "dup-1" }),
            json!({ "rel": "reject" }),
            json!({ "rel": "reject", "title": "dup-2" }),
            json!({ "rel": "noop" }),
        ];
        let err = links_to_tool_definitions(&links, "thinking").unwrap_err();
        match err {
            ExecutorError::Llm(LlmErrorCode::DuplicateTransitionRel, msg) => {
                assert!(msg.contains("advance"), "msg missing first dup: {msg}");
                assert!(msg.contains("reject"), "msg missing second dup: {msg}");
                // The non-duplicate rel must NOT appear in the error —
                // surfacing it would mislead the operator.
                assert!(
                    !msg.contains("noop"),
                    "non-duplicate rel must not appear: {msg}"
                );
            }
            other => panic!("expected DuplicateTransitionRel, got {other:?}"),
        }
    }

    #[test]
    fn links_to_tools_missing_rel_is_other_error() {
        let links = vec![json!({ "title": "no rel here" })];
        let err = links_to_tool_definitions(&links, "thinking").unwrap_err();
        match err {
            ExecutorError::Other(e) => assert!(e.to_string().contains("rel")),
            other => panic!("expected Other(rel error), got {other:?}"),
        }
    }
}
