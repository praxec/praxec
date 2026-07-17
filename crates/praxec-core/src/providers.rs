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
    /// Fireworks AI — US OpenAI-compatible open-weight host (first fleet member).
    Fireworks,
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

    /// Derive the env var name for a **named** account.
    ///
    /// Convention: `<PRIMARY_VAR>_<ACCOUNT>` where `ACCOUNT` is uppercased and
    /// hyphens are replaced with underscores.  Returns `None` for keyless
    /// (native/local) providers, which do not support named accounts.
    ///
    /// # Example
    /// ```
    /// use praxec_core::providers::{Credentials, ProviderId};
    /// let creds = ProviderId::Anthropic.credentials();
    /// assert_eq!(
    ///     creds.account_env_var("work"),
    ///     Some("ANTHROPIC_API_KEY_WORK".to_string())
    /// );
    /// assert_eq!(ProviderId::Ollama.credentials().account_env_var("any"), None);
    /// ```
    pub fn account_env_var(&self, account: &str) -> Option<String> {
        self.primary()
            .map(|base| format!("{}_{}", base, account.to_uppercase().replace('-', "_")))
    }
}

/// Whether a provider is in every build or behind a cargo feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Always,
    Feature(&'static str),
}

/// How a provider's client is constructed — which determines *which execution
/// paths can serve it*. This is the single marker path-specific surfaces filter
/// on (e.g. the aether/TUI parser seam skips rig-only fleet members).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireStyle {
    /// Built via a dedicated rig client **and** recognized by aether-llm's
    /// parser — served on both the governed (rig) path and the TUI (aether)
    /// path. All of anthropic/openai/gemini/openrouter/ollama/llamacpp/bedrock.
    Dedicated,
    /// Built via the shared OpenAI-compatible **completions** client at
    /// [`ProviderDescriptor::base_url`]. A **rig-path-only** fleet member
    /// (Fireworks, …); aether-llm does not route it, so aether-facing surfaces
    /// must skip it rather than treat it as an unknown-provider drift.
    OpenAiCompletions,
}

/// Static metadata for one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub slug: &'static str,
    pub display: &'static str,
    pub credentials: Credentials,
    pub availability: Availability,
    /// Base URL for the OpenAI-compatible **completions** path — the US
    /// open-weight fleet (Fireworks, …) is built via one shared client at this
    /// URL. `None` for providers built through their own dedicated rig client
    /// (anthropic / openai / gemini / openrouter / ollama / …).
    pub base_url: Option<&'static str>,
    /// How this provider's client is built, and therefore which paths serve it.
    pub wire: WireStyle,
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
        ProviderId::Fireworks,
    ];

    /// The single authoring point. Adding a variant fails to compile here.
    pub const fn descriptor(self) -> ProviderDescriptor {
        match self {
            ProviderId::Anthropic => ProviderDescriptor {
                slug: "anthropic",
                display: "Anthropic",
                credentials: Credentials::Single("ANTHROPIC_API_KEY"),
                availability: Availability::Always,
                base_url: None,
                wire: WireStyle::Dedicated,
            },
            ProviderId::Openai => ProviderDescriptor {
                slug: "openai",
                display: "OpenAI",
                credentials: Credentials::Single("OPENAI_API_KEY"),
                availability: Availability::Always,
                base_url: None,
                wire: WireStyle::Dedicated,
            },
            ProviderId::Gemini => ProviderDescriptor {
                slug: "gemini",
                display: "Google Gemini",
                credentials: Credentials::Single("GEMINI_API_KEY"),
                availability: Availability::Always,
                base_url: None,
                wire: WireStyle::Dedicated,
            },
            ProviderId::Openrouter => ProviderDescriptor {
                slug: "openrouter",
                display: "OpenRouter",
                credentials: Credentials::Single("OPENROUTER_API_KEY"),
                availability: Availability::Always,
                base_url: None,
                wire: WireStyle::Dedicated,
            },
            ProviderId::Ollama => ProviderDescriptor {
                slug: "ollama",
                display: "Ollama",
                credentials: Credentials::None,
                availability: Availability::Always,
                base_url: None,
                wire: WireStyle::Dedicated,
            },
            ProviderId::Llamacpp => ProviderDescriptor {
                slug: "llamacpp",
                display: "llama.cpp",
                credentials: Credentials::None,
                availability: Availability::Always,
                base_url: None,
                wire: WireStyle::Dedicated,
            },
            ProviderId::Bedrock => ProviderDescriptor {
                slug: "bedrock",
                display: "AWS Bedrock",
                credentials: Credentials::Multi(BEDROCK_VARS),
                availability: Availability::Feature("bedrock"),
                base_url: None,
                wire: WireStyle::Dedicated,
            },
            ProviderId::Fireworks => ProviderDescriptor {
                slug: "fireworks",
                display: "Fireworks AI",
                credentials: Credentials::Single("FIREWORKS_API_KEY"),
                availability: Availability::Always,
                base_url: Some("https://api.fireworks.ai/inference/v1"),
                wire: WireStyle::OpenAiCompletions,
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

/// True if a **named** account on `provider` is reachable.
///
/// For keyless/native providers (ollama, llamacpp) this always returns `false`
/// — named accounts are meaningless without credentials, and handing them out
/// would silently ignore the account qualifier.
///
/// For API-key providers the env var `<PRIMARY_KEY>_<ACCOUNT_UPPER>` must be
/// present.  See [`Credentials::account_env_var`] for the naming convention.
///
/// This generalizes [`vendor_available`] (which checks the *default* env var)
/// to support N named accounts per provider.
pub fn account_available(provider: &str, account: &str) -> bool {
    match ProviderId::from_slug(provider) {
        Some(p) => match p.credentials().account_env_var(account) {
            // keyless provider — named accounts not supported
            None => false,
            Some(var) => std::env::var(&var).is_ok(),
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
        assert_eq!(ProviderId::ALL.len(), 8);
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

    #[test]
    fn fireworks_is_an_openai_compatible_fleet_member() {
        let d = ProviderId::Fireworks.descriptor();
        assert_eq!(d.slug, "fireworks");
        assert_eq!(d.credentials.primary(), Some("FIREWORKS_API_KEY"));
        assert_eq!(d.base_url, Some("https://api.fireworks.ai/inference/v1"));
        assert_eq!(d.wire, WireStyle::OpenAiCompletions);
        // Just a base URL — always compiled in; reachability is key-gated.
        assert!(ProviderId::Fireworks.available_in_build());
        assert_eq!(
            ProviderId::from_slug("fireworks"),
            Some(ProviderId::Fireworks)
        );
    }

    /// The invariant path-specific surfaces rely on: an `OpenAiCompletions`
    /// member carries a `base_url`, and a `Dedicated` member does not — so
    /// `wire` and `base_url` can never disagree about how a provider is built.
    #[test]
    fn wire_style_and_base_url_agree_for_every_provider() {
        for &p in ProviderId::ALL {
            let d = p.descriptor();
            match d.wire {
                WireStyle::OpenAiCompletions => assert!(
                    d.base_url.is_some(),
                    "{p:?} is OpenAiCompletions but has no base_url"
                ),
                WireStyle::Dedicated => assert!(
                    d.base_url.is_none(),
                    "{p:?} is Dedicated but carries a completions base_url"
                ),
            }
        }
    }

    #[test]
    fn account_env_var_derives_correct_name() {
        assert_eq!(
            ProviderId::Anthropic.credentials().account_env_var("work"),
            Some("ANTHROPIC_API_KEY_WORK".to_string())
        );
        assert_eq!(
            ProviderId::Openai.credentials().account_env_var("my-org"),
            Some("OPENAI_API_KEY_MY_ORG".to_string())
        );
        assert_eq!(
            ProviderId::Fireworks.credentials().account_env_var("prod"),
            Some("FIREWORKS_API_KEY_PROD".to_string())
        );
    }

    #[test]
    fn keyless_providers_have_no_account_env_var() {
        assert_eq!(
            ProviderId::Ollama.credentials().account_env_var("any"),
            None
        );
        assert_eq!(
            ProviderId::Llamacpp.credentials().account_env_var("any"),
            None
        );
    }

    #[test]
    fn account_available_returns_false_for_unknown_provider() {
        assert!(!account_available("nonsense", "work"));
    }

    #[test]
    fn account_available_returns_false_for_keyless_provider() {
        // Keyless providers do not accept named accounts.
        assert!(!account_available("ollama", "work"));
        assert!(!account_available("llamacpp", "work"));
    }

    #[test]
    fn account_available_respects_env_var() {
        // Inject a synthetic env var for the test account and verify
        // account_available picks it up (edition-2024 unsafe env; the test
        // harness serialises env-touching tests on a single thread).
        unsafe { std::env::set_var("ANTHROPIC_API_KEY_TESTACCT", "sk-test") };
        assert!(account_available("anthropic", "testacct"));
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY_TESTACCT") };
        assert!(!account_available("anthropic", "testacct"));
    }
}
