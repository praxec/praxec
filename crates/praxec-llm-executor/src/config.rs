//! SPEC §33 D4 — typed config for the in-runtime LLM executor.
//!
//! Mirrors the `executor.config:` block in the workflow YAML. Closed by
//! design: `#[serde(deny_unknown_fields)]` rejects any `tools:` field at
//! parse time, surfacing FMECA F3 (forbidden tool injection) as a clean
//! deserialization failure long before the executor's `execute()` body
//! runs.
//!
//! Future passes (D5–D8) consume the additional fields:
//! - D5 (provider call): `model`, `affinity`, `reasoning_effort`.
//! - D6 (caps): `max_iterations`, `max_seconds`, `max_tokens`,
//!   `max_cost_usd`.
//! - D8 (cost map): `model` name → USD/token table.

use praxec_core::model_resolver::{Affinity, ModelRef};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmExecutorConfig {
    /// EITHER `model` (direct `"provider/model"` string) OR `affinity`
    /// (models.yaml binding) — exactly one must be set. Doctor enforces
    /// at workflow load.
    #[serde(default)]
    pub model: Option<String>,

    /// Affinity binding into `models.yaml`. Mutually exclusive with
    /// `model`. Typed as `ModelRef` (`<affinity> | <tier> |
    /// <affinity>-<tier>`) — typo'd values fail at deserialization
    /// (poka-yoke; checked at `praxec check` via the doctor).
    #[serde(default)]
    pub affinity: Option<ModelRef>,

    /// What this step **needs** — a list of [`Affinity`] capabilities (e.g.
    /// `needs: [coding, agentic]`). The model suggestor ranks candidates by their
    /// affinity scores against this list (a typed closed set; `math` aliases
    /// `reasoning`). Part of the affinity-based selection path: a step sets
    /// EITHER `model:` (an explicit pin, which wins) OR an affinity side
    /// (`affinity:` curated binding, or `needs:` capability list) — not both.
    #[serde(default)]
    pub needs: Vec<Affinity>,

    /// Required: the prompt template rendered against
    /// `{$.blackboard, $.context, $.input}` per existing
    /// `core::templating` semantics.
    ///
    /// SPEC §33 audit fixup (F1 STUB-002): no `#[serde(default)]` —
    /// missing this field fails fast at load time. An empty string is
    /// still accepted by serde (it's a valid `String`); `execute()`
    /// catches that case post-render via the LLM_EMPTY_PROMPT guard.
    pub prompt_template: String,

    /// Per-turn cap (default 3 per FMECA F1). The reliability layer
    /// does NOT auto-retry `LLM_NO_TOOL_CALL`; this is the only retry
    /// budget the executor has.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,

    /// Per-turn wall-clock cap in seconds.
    #[serde(default)]
    pub max_seconds: Option<u64>,

    /// Per-turn token cap.
    #[serde(default)]
    pub max_tokens: Option<u64>,

    /// Per-workflow cumulative cost cap; checked before each turn.
    /// `cost.rs` (D8) maps model name → USD-per-token.
    #[serde(default)]
    pub max_cost_usd: Option<f64>,

    /// Reasoning effort hint for providers that support extended thinking.
    /// Maps to `aether_llm::ReasoningEffort` at D5.
    #[serde(default)]
    pub reasoning_effort: Option<String>,

    /// SPEC §33 D7 — when `false`, the `llm.invocation` audit event
    /// records reasoning as the literal sentinel `"<elided>"` instead
    /// of the captured text. Privacy / compliance opt-out for operators
    /// who can't legally retain model thought traces. Default `true`
    /// (capture, per the locked decision).
    #[serde(default = "default_capture_reasoning")]
    pub capture_reasoning: bool,
    // SPEC §33 FMECA F3: NO `tools:` field accepted here. If a workflow
    // author tries `executor: { kind: llm, tools: [...] }`, deserialization
    // fails via `deny_unknown_fields` above. Closed by design.
}

fn default_max_iterations() -> u32 {
    3
}

fn default_capture_reasoning() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_minimal_config_with_defaults() {
        let cfg: LlmExecutorConfig = serde_json::from_value(json!({
            "model": "openai/gpt-4o",
            "prompt_template": "hello {{ input }}"
        }))
        .expect("minimal config must parse");
        assert_eq!(cfg.model.as_deref(), Some("openai/gpt-4o"));
        assert_eq!(cfg.prompt_template, "hello {{ input }}");
        assert_eq!(cfg.max_iterations, 3);
        assert!(cfg.max_seconds.is_none());
        assert!(cfg.max_tokens.is_none());
        assert!(cfg.max_cost_usd.is_none());
        assert!(cfg.reasoning_effort.is_none());
        assert!(cfg.affinity.is_none());
        // D7 — default reasoning capture is ON (privacy opt-out only).
        assert!(cfg.capture_reasoning);
    }

    #[test]
    fn parses_needs_as_typed_affinities_with_aliases() {
        // `needs:` is the capability list; `math` aliases reasoning, `agents`
        // aliases agentic (poka-yoke: typed, not free strings).
        let cfg: LlmExecutorConfig = serde_json::from_value(json!({
            "needs": ["coding", "agents", "math"],
            "prompt_template": "do x"
        }))
        .expect("needs list must deserialize to typed affinities");
        assert_eq!(
            cfg.needs,
            vec![Affinity::Coding, Affinity::Agentic, Affinity::Reasoning]
        );
        assert!(cfg.model.is_none() && cfg.affinity.is_none());
    }

    #[test]
    fn rejects_an_unknown_affinity_in_needs() {
        let err = serde_json::from_value::<LlmExecutorConfig>(json!({
            "needs": ["codign"],
            "prompt_template": "x"
        }))
        .expect_err("a typo'd affinity must fail at the boundary");
        assert!(err.to_string().contains("codign") || err.to_string().contains("variant"));
    }

    #[test]
    fn capture_reasoning_opts_out_when_set_false() {
        let cfg: LlmExecutorConfig = serde_json::from_value(json!({
            "model": "openai/gpt-4o",
            "prompt_template": "x",
            "capture_reasoning": false
        }))
        .expect("explicit capture_reasoning: false must parse");
        assert!(!cfg.capture_reasoning);
    }

    #[test]
    fn rejects_tools_field_via_deny_unknown_fields() {
        // SPEC §33 FMECA F3 — closed by design.
        let err = serde_json::from_value::<LlmExecutorConfig>(json!({
            "model": "openai/gpt-4o",
            "prompt_template": "hello",
            "tools": [{ "name": "evil" }]
        }))
        .expect_err("tools field must be rejected at parse time");
        assert!(
            err.to_string().contains("tools"),
            "error must mention `tools`: {err}"
        );
    }

    #[test]
    fn rejects_other_unknown_fields() {
        let err = serde_json::from_value::<LlmExecutorConfig>(json!({
            "model": "openai/gpt-4o",
            "prompt_template": "hello",
            "rogue_field": true
        }))
        .expect_err("unknown fields must be rejected");
        assert!(err.to_string().contains("rogue_field"));
    }

    #[test]
    fn rejects_unknown_affinity_at_parse() {
        // poka-yoke: a typo'd affinity must fail at deserialization (the
        // doctor runs this at `check`), not pass through to resolve time.
        let err = serde_json::from_value::<LlmExecutorConfig>(serde_json::json!({
            "affinity": "codign",
            "prompt_template": "do x"
        }))
        .unwrap_err();
        assert!(
            err.to_string().contains("does not parse"),
            "expected ModelRef parse error, got: {err}"
        );
    }

    #[test]
    fn accepts_affinity_tier_composite() {
        // The `affinity:` domain is `ModelRef` (<affinity> | <tier> |
        // <affinity>-<tier>), not a bare affinity — composites must parse.
        let cfg: LlmExecutorConfig = serde_json::from_value(serde_json::json!({
            "affinity": "coding-frontier",
            "prompt_template": "hi"
        }))
        .expect("affinity-tier composite must deserialize");
        assert_eq!(
            cfg.affinity.map(|d| d.to_string()).as_deref(),
            Some("coding-frontier")
        );
    }

    /// SPEC §33 audit fixup (F1 STUB-002): `prompt_template` is documented
    /// as required and now has no `#[serde(default)]` — missing the field
    /// must fail at the deserialization boundary, not silently render an
    /// empty prompt at runtime.
    #[test]
    fn rejects_missing_prompt_template() {
        let err = serde_json::from_value::<LlmExecutorConfig>(json!({
            "model": "openai/gpt-4o",
            // prompt_template intentionally absent
        }))
        .expect_err("missing prompt_template must be rejected");
        assert!(
            err.to_string().contains("prompt_template"),
            "error must name the missing field: {err}"
        );
    }
}
