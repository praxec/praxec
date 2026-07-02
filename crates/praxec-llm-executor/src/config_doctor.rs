//! SPEC §33 D10 follow-up — load-time validation of `kind: llm`
//! executor blocks.
//!
//! D9 wired the cost-catalog `doctor_check` so workflow authors see
//! `COST_CATALOG_MISSING_ENTRY` and `COST_CATALOG_STALE` before deploy.
//! D10's reviewer flagged that the closed-by-design `tools:` rejection
//! (FMECA F3) and the generic config-parse failure (which surfaces
//! `deny_unknown_fields` violations) still fire only at the FIRST
//! `LlmExecutor::execute()` call — not at `praxec check`. This
//! module closes that gap by walking the workflow registry the same
//! way `cost::doctor_check` does and running each executor block
//! through `LlmExecutorConfig` deserialization.
//!
//! Same `Diagnostic` shape as `cost::doctor_check` so the binary's
//! `check` subcommand can extend its diagnostic list with both calls.

use praxec_core::validate::Diagnostic;
use serde_json::Value;

use crate::config::LlmExecutorConfig;

/// SPEC §33 FMECA F3 — structural check for a forbidden `tools:` field
/// on a `kind: llm` executor block.
///
/// Single source of truth shared by the runtime path
/// ([`crate::LlmExecutor::execute`]) and the load-time path
/// ([`doctor_check`]). Pre-extraction the same predicate lived in both
/// places; if one drifted from the other the operator could see
/// "doctor passed but runtime rejected" (or vice versa). The check is
/// deliberately structural (raw JSON key existence) — NOT a substring
/// match on a serde error message — so lookalikes (`tools_dir`,
/// `tools_path`, `model_tools`, `use_tools`) classify as generic config
/// parse errors, not as forbidden-tools security events.
pub fn has_forbidden_tools_field(executor: &Value) -> bool {
    executor
        .as_object()
        .is_some_and(|obj| obj.contains_key("tools"))
}

/// Walk every `kind: llm` executor block in the workflow registry and
/// surface load-time errors for:
///
/// - `LLM_EXECUTOR_FORBIDDEN_TOOLS` — author wrote a `tools:` field
///   (closed-by-design per FMECA F3).
/// - `LLM_CONFIG_PARSE_ERROR` — any other `deny_unknown_fields`
///   violation, type mismatch, or missing required field.
///
/// The runtime path still enforces both via `execute()`, but the
/// load-time gate means `praxec check` rejects broken workflows
/// before they're ever scheduled — matching the operator UX the
/// cost-catalog gate already established.
pub fn doctor_check(workflow_registry: &Value) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    let Some(workflows) = workflow_registry
        .pointer("/workflows")
        .and_then(Value::as_object)
    else {
        return out;
    };

    // CMP-046 — share core's single executor-site walker rather than
    // re-implementing the states/onEnter/transitions traversal.
    for (wf_id, wf_def) in workflows {
        praxec_core::validate::for_each_executor_site(wf_def, |site| {
            let state_name = site.state.unwrap_or("");
            check_executor(
                wf_id,
                state_name,
                site.transition.unwrap_or("onEnter"),
                site.executor,
                &mut out,
            );
            // CMP-022: an executor's `output:` mapping (on the owning
            // transition or onEnter block) is the ONLY bridge that persists
            // the synthetic `_llm.*` cap counters into `instance.context`.
            // If a cap is configured but its slot isn't wired, the counter
            // never persists and the cap silently never fires. Check it at
            // load time.
            check_cap_slots(
                wf_id,
                &site.location,
                state_name,
                site.executor,
                site.owner,
                &mut out,
            );
        });
    }

    out
}

fn check_executor(
    wf_id: &str,
    state_name: &str,
    site: &str,
    executor: &Value,
    out: &mut Vec<Diagnostic>,
) {
    if executor.get("kind").and_then(Value::as_str) != Some("llm") {
        return;
    }

    // FMECA F3: structural check via the shared helper. The same
    // predicate is used by LlmExecutor::execute at runtime, so "doctor
    // passed" and "runtime accepted" stay aligned.
    if has_forbidden_tools_field(executor) {
        out.push(Diagnostic::Error(format!(
            "LLM_EXECUTOR_FORBIDDEN_TOOLS: workflow '{wf_id}': state '{state_name}' \
             executor at '{site}' declares a `tools:` field, which is closed by design \
             (SPEC §33 FMECA F3); the per-turn tool list IS the workflow's available transitions"
        )));
        return;
    }

    // Strip `kind` discriminator — the dispatcher routes by it, but
    // `LlmExecutorConfig` has `deny_unknown_fields` so we'd reject it
    // here without the strip.
    let mut config_value = executor.clone();
    if let Some(obj) = config_value.as_object_mut() {
        obj.remove("kind");
    }

    match serde_json::from_value::<LlmExecutorConfig>(config_value) {
        Err(err) => {
            out.push(Diagnostic::Error(format!(
                "LLM_CONFIG_PARSE_ERROR: workflow '{wf_id}': state '{state_name}' executor at \
                 '{site}' has an invalid `kind: llm` config: {err}"
            )));
        }
        Ok(config) => {
            // Model XOR affinity-side, via the shared validator both executor
            // kinds use. The affinity side is the curated `affinity:` OR the
            // `needs:` capability list (both select by affinity). Enforced at
            // `check` so an executor that sets BOTH a model and an affinity side
            // (or neither) fails at load, matching `kind: agent`.
            let affinity_side = config
                .affinity
                .as_ref()
                .map(|a| a.to_string())
                .or_else(|| config.needs.first().map(|n| n.to_string()));
            if let Err(msg) = praxec_core::binding::validate_exclusive_binding(
                config.model.as_deref(),
                affinity_side.as_deref(),
                "model",
            ) {
                out.push(Diagnostic::Error(format!(
                    "LLM_INVALID_MODEL_BINDING: workflow '{wf_id}': state '{state_name}' \
                     executor at '{site}': {msg}"
                )));
            }
        }
    }
}

/// CMP-022 — verify a `kind: llm` transition wires the `_llm.*` cap
/// slots its configured caps depend on.
///
/// The executor WRITES nested `{"_llm": {...}}` into `ExecuteResult.output`
/// but READS flat dotted keys (`_llm.cumulative_tokens`, …) back out of
/// `instance.context`. The bridge between the two is the transition's
/// hand-written `output:` mapping. If an author configures a cap but
/// omits the matching `output:` line, the counter never persists and the
/// cap can never fire — a silently dead protection.
///
/// Required-slot determination (the set we can determine with confidence
/// from the cap config; see SPEC §33 D6 `apply_caps`):
///
/// - `max_tokens`   → `_llm.cumulative_tokens`
/// - `max_cost_usd` → `_llm.cumulative_cost_usd`
/// - `max_seconds`  → `_llm.session.<state>.started_at` (per-state key)
/// - `max_iterations` set to a NON-DEFAULT value → `_llm.consecutive_no_tool_call`
///   (the F1 consecutive-no-tool-call counter).
///
/// `max_iterations` carries a serde default of 3, so it is *always*
/// "present" in the parsed config; requiring its slot unconditionally
/// would flag every LLM transition. We therefore only require the F1
/// counter slot when the author set `max_iterations` to something other
/// than the default — a deliberate, narrowly-scoped heuristic. The
/// `_llm.cumulative_iterations` slot is written by the executor but no
/// cap reads it, so it is NOT required here.
fn check_cap_slots(
    wf_id: &str,
    location: &str,
    state_name: &str,
    executor: &Value,
    owner: &Value,
    out: &mut Vec<Diagnostic>,
) {
    if executor.get("kind").and_then(Value::as_str) != Some("llm") {
        return;
    }

    // Parse the config (strip the `kind` discriminator first, as the
    // runtime path does). If it doesn't parse, the generic
    // `LLM_CONFIG_PARSE_ERROR` from `check_executor` already fired —
    // skip the slot check rather than double-report.
    let mut config_value = executor.clone();
    if let Some(obj) = config_value.as_object_mut() {
        obj.remove("kind");
    }
    let Ok(config) = serde_json::from_value::<LlmExecutorConfig>(config_value) else {
        return;
    };

    // Build the required-slot list from the active caps.
    let mut required: Vec<String> = Vec::new();
    if config.max_tokens.is_some() {
        required.push("_llm.cumulative_tokens".to_string());
    }
    if config.max_cost_usd.is_some() {
        required.push("_llm.cumulative_cost_usd".to_string());
    }
    if config.max_seconds.is_some() {
        required.push(crate::caps::session_started_at_key(state_name));
    }
    if config.max_iterations != default_max_iterations() {
        required.push("_llm.consecutive_no_tool_call".to_string());
    }

    if required.is_empty() {
        return;
    }

    // The `output:` mapping keys are the destination context slots.
    let output_keys: std::collections::HashSet<&str> = owner
        .get("output")
        .and_then(Value::as_object)
        .map(|obj| obj.keys().map(String::as_str).collect())
        .unwrap_or_default();

    let missing: Vec<String> = required
        .into_iter()
        .filter(|slot| !output_keys.contains(slot.as_str()))
        .collect();

    if !missing.is_empty() {
        out.push(Diagnostic::Error(format!(
            "LLM_MISSING_CAP_SLOT_MAPPING: workflow '{wf_id}' {location} configures LLM \
             cap(s) but its `output:` mapping does not wire the required synthetic slot(s): \
             [{}]. Without these lines the executor's cumulative counters never persist into \
             the workflow context and the cap(s) silently never fire. Add an `output:` entry \
             mapping each slot, e.g. \"{first}\": \"$.output.{first}\".",
            missing.join(", "),
            first = missing[0],
        )));
    }
}

/// CMP-022 — mirror of the serde default on
/// [`LlmExecutorConfig::max_iterations`]. Kept here so the doctor's
/// "author changed the cap" heuristic stays in lockstep with the config
/// default without making the private `config::default_max_iterations`
/// pub.
fn default_max_iterations() -> u32 {
    3
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry_with_executor(executor: Value) -> Value {
        json!({
            "workflows": {
                "wf_under_test": {
                    "states": {
                        "thinking": {
                            "transitions": {
                                "advance": {
                                    "target": "done",
                                    "executor": executor,
                                }
                            }
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn happy_config_yields_no_diagnostics() {
        let registry = registry_with_executor(json!({
            "kind": "llm",
            "model": "anthropic:claude-sonnet-4-6",
            "prompt_template": "do the thing",
        }));
        assert!(doctor_check(&registry).is_empty());
    }

    /// F4 (DRY-001) — the shared structural helper must reject a
    /// literal `tools` key. Pinned because both `LlmExecutor::execute`
    /// (runtime) AND `doctor_check` (load-time) key off this single
    /// predicate; a regression here would silently weaken both paths.
    #[test]
    fn has_forbidden_tools_field_detects_literal_tools() {
        assert!(has_forbidden_tools_field(&json!({
            "kind": "llm",
            "tools": []
        })));
        assert!(has_forbidden_tools_field(&json!({
            "kind": "llm",
            "tools": [{"name": "evil"}]
        })));
    }

    /// F4 (DRY-001) — and must NOT reject lookalikes. Operators who
    /// type `tools_dir`, `tools_path`, `model_tools`, `use_tools` are
    /// not declaring forbidden tools; they get a generic config-parse
    /// error instead of a security-flavored one.
    #[test]
    fn has_forbidden_tools_field_ignores_lookalikes() {
        for lookalike in [
            "tools_dir",
            "tools_path",
            "model_tools",
            "use_tools",
            "atools",
            "toolsX",
        ] {
            let mut obj = serde_json::Map::new();
            obj.insert("kind".into(), json!("llm"));
            obj.insert(lookalike.into(), json!("anything"));
            assert!(
                !has_forbidden_tools_field(&Value::Object(obj)),
                "lookalike `{lookalike}` must NOT trigger F3"
            );
        }
    }

    /// F4 (DRY-001) — non-object inputs are not "objects with a tools
    /// key" by definition. The helper must treat them as "no tools
    /// field present" (the doctor caller's other code paths will
    /// surface a different error for the malformed shape).
    #[test]
    fn has_forbidden_tools_field_returns_false_on_non_object() {
        assert!(!has_forbidden_tools_field(&json!(null)));
        assert!(!has_forbidden_tools_field(&json!([])));
        assert!(!has_forbidden_tools_field(&json!("tools")));
        assert!(!has_forbidden_tools_field(&json!(42)));
    }

    #[test]
    fn tools_field_surfaces_forbidden_tools_error() {
        let registry = registry_with_executor(json!({
            "kind": "llm",
            "model": "anthropic:claude-sonnet-4-6",
            "prompt_template": "do the thing",
            "tools": [{"name": "evil"}],
        }));
        let diags = doctor_check(&registry);
        assert_eq!(diags.len(), 1);
        match &diags[0] {
            Diagnostic::Error(msg) => {
                assert!(msg.contains("LLM_EXECUTOR_FORBIDDEN_TOOLS"));
                assert!(msg.contains("wf_under_test"));
                assert!(msg.contains("thinking"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn tools_lookalike_field_is_not_misclassified() {
        // `tools_dir` is NOT the F3-forbidden `tools:` — it should
        // surface as a generic config parse error instead, never as
        // ExecutorForbiddenTools.
        let registry = registry_with_executor(json!({
            "kind": "llm",
            "model": "anthropic:claude-sonnet-4-6",
            "prompt_template": "do the thing",
            "tools_dir": "/some/path",
        }));
        let diags = doctor_check(&registry);
        assert_eq!(diags.len(), 1);
        match &diags[0] {
            Diagnostic::Error(msg) => {
                assert!(
                    !msg.contains("FORBIDDEN_TOOLS"),
                    "tools_dir misclassified as F3: {msg}"
                );
                assert!(msg.contains("LLM_CONFIG_PARSE_ERROR"));
                assert!(msg.contains("tools_dir"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn non_llm_executor_is_ignored() {
        let registry = registry_with_executor(json!({
            "kind": "script",
            "subject": "any.thing",
            "tools": [],
        }));
        assert!(doctor_check(&registry).is_empty());
    }

    #[test]
    fn both_model_and_affinity_flags_invalid_binding() {
        // Model XOR affinity, enforced at `check` via the shared validator —
        // setting both must surface LLM_INVALID_MODEL_BINDING.
        let registry = registry_with_executor(json!({
            "kind": "llm",
            "model": "anthropic:claude-sonnet-4-6",
            "affinity": "coding",
            "prompt_template": "do the thing",
        }));
        let diags = doctor_check(&registry);
        assert_eq!(diags.len(), 1);
        match &diags[0] {
            Diagnostic::Error(msg) => {
                assert!(msg.contains("LLM_INVALID_MODEL_BINDING"), "got: {msg}");
                assert!(msg.contains("wf_under_test"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn needs_only_is_a_valid_binding() {
        // `needs:` is the affinity side — a step may select by needs alone.
        let registry = registry_with_executor(json!({
            "kind": "llm",
            "needs": ["coding", "agentic"],
            "prompt_template": "do the thing",
        }));
        assert!(
            doctor_check(&registry).is_empty(),
            "needs-only must be valid"
        );
    }

    #[test]
    fn both_model_and_needs_flags_invalid_binding() {
        // `needs:` counts as the affinity side, so model + needs is "both" set.
        let registry = registry_with_executor(json!({
            "kind": "llm",
            "model": "anthropic:claude-sonnet-4-6",
            "needs": ["coding"],
            "prompt_template": "do the thing",
        }));
        let diags = doctor_check(&registry);
        assert_eq!(diags.len(), 1);
        assert!(
            matches!(&diags[0], Diagnostic::Error(m) if m.contains("LLM_INVALID_MODEL_BINDING"))
        );
    }

    // Note: most `LlmExecutorConfig` fields carry `#[serde(default)]`,
    // so a load-time "missing/wrong-type" check has weak teeth at this
    // layer. The F3 tools-injection path is what THIS doctor closes.
    // A follow-up could tighten `LlmExecutorConfig` to make
    // `prompt_template` actually required (drop its `#[serde(default)]`),
    // but that's out of scope for the D10 follow-up.

    // ── CMP-022: cap-slot output-mapping check ──────────────────────

    fn registry_with_executor_and_output(executor: Value, output: Value) -> Value {
        json!({
            "workflows": {
                "wf_under_test": {
                    "states": {
                        "thinking": {
                            "transitions": {
                                "advance": {
                                    "target": "done",
                                    "executor": executor,
                                    "output": output,
                                }
                            }
                        }
                    }
                }
            }
        })
    }

    fn cap_slot_diags(diags: &[Diagnostic]) -> Vec<&str> {
        diags
            .iter()
            .filter_map(|d| match d {
                Diagnostic::Error(m) if m.contains("LLM_MISSING_CAP_SLOT_MAPPING") => {
                    Some(m.as_str())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn max_tokens_without_output_slot_flags_missing_mapping() {
        let registry = registry_with_executor(json!({
            "kind": "llm",
            "model": "anthropic:claude-sonnet-4-6",
            "prompt_template": "x",
            "max_tokens": 1000,
        }));
        let diags = doctor_check(&registry);
        let cap = cap_slot_diags(&diags);
        assert_eq!(cap.len(), 1, "expected one cap-slot error, got: {diags:?}");
        assert!(cap[0].contains("_llm.cumulative_tokens"));
        assert!(cap[0].contains("advance"));
    }

    #[test]
    fn max_tokens_with_output_slot_is_clean() {
        let registry = registry_with_executor_and_output(
            json!({
                "kind": "llm",
                "model": "anthropic:claude-sonnet-4-6",
                "prompt_template": "x",
                "max_tokens": 1000,
            }),
            json!({
                "_llm.cumulative_tokens": "$.output._llm.cumulative_tokens"
            }),
        );
        assert!(cap_slot_diags(&doctor_check(&registry)).is_empty());
    }

    #[test]
    fn max_cost_usd_requires_cumulative_cost_slot() {
        let registry = registry_with_executor(json!({
            "kind": "llm",
            "model": "anthropic:claude-sonnet-4-6",
            "prompt_template": "x",
            "max_cost_usd": 0.5,
        }));
        let diags = doctor_check(&registry);
        let cap = cap_slot_diags(&diags);
        assert_eq!(cap.len(), 1);
        assert!(cap[0].contains("_llm.cumulative_cost_usd"));
    }

    #[test]
    fn max_seconds_requires_per_state_session_slot() {
        let registry = registry_with_executor(json!({
            "kind": "llm",
            "model": "anthropic:claude-sonnet-4-6",
            "prompt_template": "x",
            "max_seconds": 60,
        }));
        let diags = doctor_check(&registry);
        let cap = cap_slot_diags(&diags);
        assert_eq!(cap.len(), 1);
        // Per-state key includes the state name.
        assert!(cap[0].contains("_llm.session.thinking.started_at"));
    }

    #[test]
    fn non_default_max_iterations_requires_consecutive_slot() {
        let registry = registry_with_executor(json!({
            "kind": "llm",
            "model": "anthropic:claude-sonnet-4-6",
            "prompt_template": "x",
            "max_iterations": 2,
        }));
        let diags = doctor_check(&registry);
        let cap = cap_slot_diags(&diags);
        assert_eq!(cap.len(), 1);
        assert!(cap[0].contains("_llm.consecutive_no_tool_call"));
    }

    #[test]
    fn default_max_iterations_alone_requires_no_slots() {
        // max_iterations defaults to 3; with no other cap there is
        // nothing to persist, so no cap-slot error.
        let registry = registry_with_executor(json!({
            "kind": "llm",
            "model": "anthropic:claude-sonnet-4-6",
            "prompt_template": "x",
        }));
        assert!(cap_slot_diags(&doctor_check(&registry)).is_empty());
    }

    #[test]
    fn multiple_caps_report_all_missing_slots_in_one_error() {
        let registry = registry_with_executor(json!({
            "kind": "llm",
            "model": "anthropic:claude-sonnet-4-6",
            "prompt_template": "x",
            "max_tokens": 1000,
            "max_cost_usd": 0.5,
        }));
        let diags = doctor_check(&registry);
        let cap = cap_slot_diags(&diags);
        assert_eq!(cap.len(), 1, "all missing slots roll up into one error");
        assert!(cap[0].contains("_llm.cumulative_tokens"));
        assert!(cap[0].contains("_llm.cumulative_cost_usd"));
    }

    #[test]
    fn partial_mapping_flags_only_the_missing_slot() {
        let registry = registry_with_executor_and_output(
            json!({
                "kind": "llm",
                "model": "anthropic:claude-sonnet-4-6",
                "prompt_template": "x",
                "max_tokens": 1000,
                "max_cost_usd": 0.5,
            }),
            json!({
                // tokens wired, cost slot omitted
                "_llm.cumulative_tokens": "$.output._llm.cumulative_tokens"
            }),
        );
        let diags = doctor_check(&registry);
        let cap = cap_slot_diags(&diags);
        assert_eq!(cap.len(), 1);
        assert!(
            !cap[0].contains("cumulative_tokens"),
            "tokens wired: {}",
            cap[0]
        );
        assert!(cap[0].contains("_llm.cumulative_cost_usd"));
    }
}
