//! T19 — translation from `models.yaml` per-provider feature toggles
//! to aether's effective `ReasoningEffort`. Pure function; no I/O.

use llm::ReasoningEffort;
use praxec_core::model_resolver::{
    AnthropicFeatures, GoogleFeatures, OpenAIFeatures, ProviderFeatures,
};
use praxec_tui::sub_agent::features_to_reasoning_effort;

// ── ProviderFeatures::None ─────────────────────────────────────────────────

#[test]
fn none_maps_to_no_effort() {
    assert_eq!(features_to_reasoning_effort(&ProviderFeatures::None), None);
}

// ── Anthropic ──────────────────────────────────────────────────────────────

#[test]
fn anthropic_default_is_no_effort() {
    let f = ProviderFeatures::Anthropic(AnthropicFeatures::default());
    assert_eq!(features_to_reasoning_effort(&f), None);
}

#[test]
fn anthropic_extended_thinking_true_no_budget_defaults_high() {
    let f = ProviderFeatures::Anthropic(AnthropicFeatures {
        extended_thinking: true,
        thinking_budget_tokens: None,
    });
    assert_eq!(
        features_to_reasoning_effort(&f),
        Some(ReasoningEffort::High)
    );
}

#[test]
fn anthropic_budget_overrides_flag() {
    // When a budget is set, the explicit number wins regardless of the
    // flag — setting a budget without enabling thinking would be a
    // contradictory state we resolve by honoring the budget.
    let f = ProviderFeatures::Anthropic(AnthropicFeatures {
        extended_thinking: false,
        thinking_budget_tokens: Some(4096),
    });
    assert_eq!(
        features_to_reasoning_effort(&f),
        Some(ReasoningEffort::Medium)
    );
}

#[test]
fn anthropic_budget_snaps_to_nearest_effort() {
    let cases = [
        (512u32, ReasoningEffort::Low),
        (1024, ReasoningEffort::Low),
        (2048, ReasoningEffort::Low),
        (2049, ReasoningEffort::Medium),
        (4096, ReasoningEffort::Medium),
        (6144, ReasoningEffort::Medium),
        (6145, ReasoningEffort::High),
        (10240, ReasoningEffort::High),
        (16384, ReasoningEffort::High),
        (16385, ReasoningEffort::Xhigh),
        (32000, ReasoningEffort::Xhigh),
    ];
    for (budget, expected) in cases {
        let f = ProviderFeatures::Anthropic(AnthropicFeatures {
            extended_thinking: true,
            thinking_budget_tokens: Some(budget),
        });
        assert_eq!(
            features_to_reasoning_effort(&f),
            Some(expected),
            "budget {budget} → {expected:?}"
        );
    }
}

// ── OpenAI ─────────────────────────────────────────────────────────────────

#[test]
fn openai_no_effort_maps_to_none() {
    let f = ProviderFeatures::OpenAI(OpenAIFeatures::default());
    assert_eq!(features_to_reasoning_effort(&f), None);
}

#[test]
fn openai_known_levels_parse() {
    let cases = [
        ("low", ReasoningEffort::Low),
        ("medium", ReasoningEffort::Medium),
        ("high", ReasoningEffort::High),
        ("xhigh", ReasoningEffort::Xhigh),
    ];
    for (raw, expected) in cases {
        let f = ProviderFeatures::OpenAI(OpenAIFeatures {
            reasoning_effort: Some(raw.into()),
        });
        assert_eq!(
            features_to_reasoning_effort(&f),
            Some(expected),
            "{raw} should parse to {expected:?}"
        );
    }
}

#[test]
fn openai_case_insensitive_parse() {
    let f = ProviderFeatures::OpenAI(OpenAIFeatures {
        reasoning_effort: Some("HIGH".into()),
    });
    assert_eq!(
        features_to_reasoning_effort(&f),
        Some(ReasoningEffort::High)
    );
}

#[test]
fn openai_unknown_level_passes_through_as_none() {
    // Operator typoed `hi` — translation drops it with no effort
    // applied. The spawner is expected to log a warning at the boundary
    // so the typo is operator-visible.
    let f = ProviderFeatures::OpenAI(OpenAIFeatures {
        reasoning_effort: Some("hi".into()),
    });
    assert_eq!(features_to_reasoning_effort(&f), None);
}

// ── Google ─────────────────────────────────────────────────────────────────

#[test]
fn google_no_budget_maps_to_none() {
    let f = ProviderFeatures::Google(GoogleFeatures::default());
    assert_eq!(features_to_reasoning_effort(&f), None);
}

#[test]
fn google_budget_snaps_to_effort() {
    let f = ProviderFeatures::Google(GoogleFeatures {
        thinking_budget_tokens: Some(10240),
    });
    assert_eq!(
        features_to_reasoning_effort(&f),
        Some(ReasoningEffort::High)
    );
}
