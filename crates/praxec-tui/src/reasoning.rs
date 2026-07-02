//! Map a `models.yaml` per-provider feature set to aether's effective
//! `ReasoningEffort`. Used by the TUI's external-spine sub-agent spawner
//! (`sub_agent.rs`), which drives the `aether` models — so the provider-shaped
//! intent → effort-level mapping lives next to its one consumer. (The in-step
//! `kind: agent` executor takes `reasoning_effort` as a config string and passes
//! it straight to the `aether` subprocess, so it needs none of this — which is
//! why this lives in the TUI, not the now-aether-llm-free `agents` crate.)

use llm::ReasoningEffort;
use praxec_core::model_resolver::{
    AnthropicFeatures, GoogleFeatures, OpenAIFeatures, ProviderFeatures,
};

/// aether-llm normalizes all "think harder" knobs (Anthropic extended_thinking,
/// OpenAI reasoning_effort, Google thinking_budget) into a single
/// `ReasoningEffort`. This does the reverse — provider-shaped operator intent
/// → the aether knob. An explicit budget overrides `extended_thinking: bool`.
pub fn features_to_reasoning_effort(features: &ProviderFeatures) -> Option<ReasoningEffort> {
    match features {
        ProviderFeatures::None => None,
        ProviderFeatures::Anthropic(AnthropicFeatures {
            extended_thinking,
            thinking_budget_tokens,
        }) => match (thinking_budget_tokens, extended_thinking) {
            (Some(n), _) => Some(budget_to_effort(*n)),
            (None, true) => Some(ReasoningEffort::High),
            (None, false) => None,
        },
        ProviderFeatures::OpenAI(OpenAIFeatures { reasoning_effort }) => {
            reasoning_effort.as_deref().and_then(parse_openai_effort)
        }
        ProviderFeatures::Google(GoogleFeatures {
            thinking_budget_tokens,
        }) => thinking_budget_tokens.map(budget_to_effort),
    }
}

/// Snap a budget-token count onto the nearest aether effort level (the same
/// thresholds aether-llm's anthropic provider uses internally).
pub fn budget_to_effort(n: u32) -> ReasoningEffort {
    if n <= 2048 {
        ReasoningEffort::Low
    } else if n <= 6144 {
        ReasoningEffort::Medium
    } else if n <= 16384 {
        ReasoningEffort::High
    } else {
        ReasoningEffort::Xhigh
    }
}

/// Pass through the four known OpenAI effort levels; unknown → `None`.
pub fn parse_openai_effort(s: &str) -> Option<ReasoningEffort> {
    match s.to_ascii_lowercase().as_str() {
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" => Some(ReasoningEffort::Xhigh),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_snaps_to_levels() {
        assert_eq!(budget_to_effort(1024), ReasoningEffort::Low);
        assert_eq!(budget_to_effort(4096), ReasoningEffort::Medium);
        assert_eq!(budget_to_effort(10240), ReasoningEffort::High);
        assert_eq!(budget_to_effort(32768), ReasoningEffort::Xhigh);
    }

    #[test]
    fn openai_effort_parses_known_and_rejects_unknown() {
        assert_eq!(parse_openai_effort("HIGH"), Some(ReasoningEffort::High));
        assert_eq!(parse_openai_effort("bogus"), None);
    }

    #[test]
    fn anthropic_budget_overrides_extended_thinking() {
        let f = ProviderFeatures::Anthropic(AnthropicFeatures {
            extended_thinking: false,
            thinking_budget_tokens: Some(8000),
        });
        assert_eq!(
            features_to_reasoning_effort(&f),
            Some(ReasoningEffort::High)
        );
    }
}
