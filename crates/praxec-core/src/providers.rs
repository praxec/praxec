//! Single source of truth for the curated LLM provider set.
//!
//! Canonical [`ProviderId::slug`] values match aether-llm's
//! `ModelProviderParser` tokens (the agent path hands the runtime a
//! `provider:model` string). This catalog carries provider **identity**
//! only — never model IDs; models stay free-form `String` everywhere.
//!
//! Every surface (the `kind: llm` factory, `set-provider-keys`, the agent
//! resolver) projects from this enum via an exhaustive `match`, so adding a
//! provider is a compile error until each surface handles it.

/// The curated providers praxec supports first-class. The open-ended
/// OpenAI-compatible long tail is handled by `model_resolver::Provider::Custom`,
/// not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProviderId {
    Anthropic,
    Openai,
    Gemini,
    Openrouter,
    Ollama,
    Llamacpp,
    Bedrock,
}

/// What credential, if any, a provider needs in the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credentials {
    /// Local, keyless (ollama, llamacpp).
    None,
    /// A single API-key env var.
    Single(&'static str),
    /// Several env vars, all required (bedrock's AWS triplet).
    Multi(&'static [&'static str]),
}

impl Credentials {
    /// The primary key env var (the first, for multi-var providers).
    pub fn primary(&self) -> Option<&'static str> {
        match self {
            Credentials::None => None,
            Credentials::Single(v) => Some(v),
            Credentials::Multi(vs) => vs.first().copied(),
        }
    }

    /// Every env var this credential occupies (empty for keyless).
    pub fn env_vars(&self) -> Vec<&'static str> {
        match self {
            Credentials::None => Vec::new(),
            Credentials::Single(v) => vec![*v],
            Credentials::Multi(vs) => vs.to_vec(),
        }
    }
}

/// Whether a provider is in every build or behind a cargo feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Always,
    Feature(&'static str),
}

/// Static metadata for one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub slug: &'static str,
    pub display: &'static str,
    pub credentials: Credentials,
    pub availability: Availability,
}

const BEDROCK_VARS: &[&str] = &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_REGION"];

impl ProviderId {
    /// Every curated provider, in display order.
    pub const ALL: &'static [ProviderId] = &[
        ProviderId::Anthropic,
        ProviderId::Openai,
        ProviderId::Gemini,
        ProviderId::Openrouter,
        ProviderId::Ollama,
        ProviderId::Llamacpp,
        ProviderId::Bedrock,
    ];

    /// The single authoring point. Adding a variant fails to compile here.
    pub const fn descriptor(self) -> ProviderDescriptor {
        match self {
            ProviderId::Anthropic => ProviderDescriptor {
                slug: "anthropic",
                display: "Anthropic",
                credentials: Credentials::Single("ANTHROPIC_API_KEY"),
                availability: Availability::Always,
            },
            ProviderId::Openai => ProviderDescriptor {
                slug: "openai",
                display: "OpenAI",
                credentials: Credentials::Single("OPENAI_API_KEY"),
                availability: Availability::Always,
            },
            ProviderId::Gemini => ProviderDescriptor {
                slug: "gemini",
                display: "Google Gemini",
                credentials: Credentials::Single("GEMINI_API_KEY"),
                availability: Availability::Always,
            },
            ProviderId::Openrouter => ProviderDescriptor {
                slug: "openrouter",
                display: "OpenRouter",
                credentials: Credentials::Single("OPENROUTER_API_KEY"),
                availability: Availability::Always,
            },
            ProviderId::Ollama => ProviderDescriptor {
                slug: "ollama",
                display: "Ollama",
                credentials: Credentials::None,
                availability: Availability::Always,
            },
            ProviderId::Llamacpp => ProviderDescriptor {
                slug: "llamacpp",
                display: "llama.cpp",
                credentials: Credentials::None,
                availability: Availability::Always,
            },
            ProviderId::Bedrock => ProviderDescriptor {
                slug: "bedrock",
                display: "AWS Bedrock",
                credentials: Credentials::Multi(BEDROCK_VARS),
                availability: Availability::Feature("bedrock"),
            },
        }
    }

    pub fn slug(self) -> &'static str {
        self.descriptor().slug
    }

    pub fn display(self) -> &'static str {
        self.descriptor().display
    }

    pub fn credentials(self) -> Credentials {
        self.descriptor().credentials
    }

    /// Parse a canonical slug. `None` for unknown/legacy slugs
    /// (e.g. `"google"`, `"lmstudio"`).
    pub fn from_slug(s: &str) -> Option<Self> {
        ProviderId::ALL.iter().copied().find(|p| p.slug() == s)
    }

    /// Whether this provider's code is compiled into the current build.
    pub fn available_in_build(self) -> bool {
        match self.descriptor().availability {
            Availability::Always => true,
            Availability::Feature("bedrock") => cfg!(feature = "bedrock"),
            Availability::Feature(_) => false,
        }
    }
}

/// True if `vendor`'s provider is reachable — keyless/local (e.g. ollama), or its
/// primary API key is present in the environment (keys are loaded into env at
/// startup). The one canonical reachability check, shared by the model catalog,
/// the cockpit picker, and the embedding catalog.
pub fn vendor_available(vendor: &str) -> bool {
    match ProviderId::from_slug(vendor) {
        Some(p) => match p.credentials().primary() {
            None => true, // local / keyless
            Some(var) => std::env::var(var).is_ok(),
        },
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_is_complete_and_slugs_round_trip() {
        for &p in ProviderId::ALL {
            assert_eq!(ProviderId::from_slug(p.slug()), Some(p), "{p:?}");
        }
        assert_eq!(ProviderId::ALL.len(), 7);
    }

    #[test]
    fn legacy_slugs_are_not_recognized() {
        assert_eq!(ProviderId::from_slug("google"), None);
        assert_eq!(ProviderId::from_slug("lmstudio"), None);
        assert_eq!(ProviderId::from_slug("nonsense"), None);
    }

    #[test]
    fn gemini_uses_the_aether_token_and_gemini_key() {
        assert_eq!(ProviderId::Gemini.slug(), "gemini");
        assert_eq!(
            ProviderId::Gemini.credentials().primary(),
            Some("GEMINI_API_KEY")
        );
    }

    #[test]
    fn local_providers_are_keyless() {
        assert!(ProviderId::Ollama.credentials().env_vars().is_empty());
        assert!(ProviderId::Llamacpp.credentials().env_vars().is_empty());
    }

    #[test]
    fn bedrock_is_multi_var_and_feature_gated() {
        assert_eq!(
            ProviderId::Bedrock.credentials().env_vars(),
            vec!["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_REGION"]
        );
        assert_eq!(
            ProviderId::Bedrock.available_in_build(),
            cfg!(feature = "bedrock")
        );
    }
}
