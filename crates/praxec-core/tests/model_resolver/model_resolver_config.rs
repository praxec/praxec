//! FMECA U3/U4/T3 + PR1-vetted-plan tests for `models.yaml` loader.
//!
//! Each test name maps directly to a row in the FMECA mapping table in
//! the design plan.

use praxec_core::model_resolver::{
    Affinity, ModelConfigError, ModelsFile, OverrideKey, Provider, ProviderFeatures, Tier,
};
use praxec_core::providers::ProviderId;

// ── happy path: confirms the round-trip shape ───────────────────────────────

#[test]
fn minimal_valid_file_loads() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let f = ModelsFile::from_yaml(yaml).expect("loads");
    assert_eq!(f.version, 1);
    assert!(!f.strict_specificity);
    assert_eq!(f.default.len(), 1);
    assert_eq!(
        f.default[0].provider,
        Provider::Known(ProviderId::Anthropic)
    );
    assert_eq!(f.default[0].model, "claude-sonnet-4-6");
    assert!(matches!(
        f.default[0].features,
        ProviderFeatures::Anthropic(_)
    ));
    assert!(f.overrides.is_empty());
}

#[test]
fn overrides_keyed_by_affinity_tier_round_trip() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:
  coding-frontier:
    - provider: { name: openai }
      model: gpt-5
  coding:
    - provider: { name: anthropic }
      model: claude-sonnet-4-6
  frontier:
    - provider: { name: anthropic }
      model: claude-opus-4-7
"#;
    let f = ModelsFile::from_yaml(yaml).expect("loads");
    let key_full = OverrideKey {
        affinity: Some(Affinity::Coding),
        tier: Some(Tier::Frontier),
    };
    let key_aff = OverrideKey {
        affinity: Some(Affinity::Coding),
        tier: None,
    };
    let key_tier = OverrideKey {
        affinity: None,
        tier: Some(Tier::Frontier),
    };
    assert!(f.overrides.contains_key(&key_full));
    assert!(f.overrides.contains_key(&key_aff));
    assert!(f.overrides.contains_key(&key_tier));
    assert_eq!(f.overrides[&key_full][0].model, "gpt-5");
}

// ── FMECA U4 ────────────────────────────────────────────────────────────────

#[test]
fn default_required_at_load() {
    let yaml = r#"
version: 1
overrides:
  coding:
    - provider: { name: anthropic }
      model: claude-sonnet-4-6
"#;
    let err = ModelsFile::from_yaml(yaml).expect_err("no default field → error");
    assert!(
        matches!(err, ModelConfigError::MissingDefault),
        "expected MissingDefault, got {err:?}"
    );
}

#[test]
fn empty_default_rejected() {
    let yaml = r#"
version: 1
default: []
"#;
    let err = ModelsFile::from_yaml(yaml).expect_err("empty default → error");
    assert!(
        matches!(err, ModelConfigError::EmptyDefault),
        "expected EmptyDefault, got {err:?}"
    );
}

// ── FMECA U3 (UnknownOverrideKey) ───────────────────────────────────────────

#[test]
fn unknown_affinity_named_in_error() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:
  vision-frontier:
    - provider: { name: anthropic }
      model: claude-sonnet-4-6
"#;
    let err = ModelsFile::from_yaml(yaml).expect_err("unknown affinity → error");
    let msg = format!("{err}");
    assert!(
        msg.contains("vision-frontier"),
        "error must name the offending key (got: {msg})"
    );
}

// ── FMECA T3 (deny_unknown_fields on per-provider features) ─────────────────

#[test]
fn unknown_feature_key_named() {
    let yaml = r#"
version: 1
default:
  - provider: { name: openai }
    model: gpt-5
    features:
      reasoning_effrt: high
"#;
    let err = ModelsFile::from_yaml(yaml).expect_err("typo in feature key → error");
    let msg = format!("{err}");
    assert!(
        msg.contains("reasoning_effrt"),
        "error must name the typo'd key (got: {msg})"
    );
}

// ── provider custom requires endpoint ───────────────────────────────────────

#[test]
fn provider_custom_requires_endpoint() {
    let yaml = r#"
version: 1
default:
  - provider: { name: custom, endpoint: "" }
    model: my-model
"#;
    let err = ModelsFile::from_yaml(yaml).expect_err("custom w/o endpoint → error");
    assert!(
        matches!(err, ModelConfigError::ProviderEndpointRequired),
        "expected ProviderEndpointRequired, got {err:?}"
    );
}

#[test]
fn provider_custom_with_endpoint_loads() {
    let yaml = r#"
version: 1
default:
  - provider: { name: custom, endpoint: "https://my-llm.internal/v1" }
    model: my-model
"#;
    let f = ModelsFile::from_yaml(yaml).expect("loads");
    let p = &f.default[0].provider;
    assert!(matches!(p, Provider::Custom { endpoint } if endpoint == "https://my-llm.internal/v1"));
}

// ── version mismatch ────────────────────────────────────────────────────────

#[test]
fn version_mismatch_surfaces() {
    let yaml = r#"
version: 99
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let err = ModelsFile::from_yaml(yaml).expect_err("version mismatch → error");
    assert!(
        matches!(
            err,
            ModelConfigError::VersionMismatch {
                got: 99,
                expected: 1
            }
        ),
        "expected VersionMismatch{{got:99,expected:1}}, got {err:?}"
    );
}

// ── deny_unknown_fields at top level ────────────────────────────────────────

#[test]
fn deny_unknown_fields_top_level() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
foo: bar
"#;
    let err = ModelsFile::from_yaml(yaml).expect_err("unknown top-level field → error");
    let msg = format!("{err}");
    assert!(
        msg.contains("foo"),
        "error must name the offending key (got: {msg})"
    );
}

// ── strict_specificity is parseable as a bool (truthy on YAML true) ─────────

#[test]
fn strict_specificity_parses() {
    let yaml = r#"
version: 1
strict_specificity: true
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let f = ModelsFile::from_yaml(yaml).expect("loads");
    assert!(f.strict_specificity);
}

// ── provider catalog cutover (Known(ProviderId) | Custom) ───────────────────

#[test]
fn provider_gemini_and_openrouter_parse() {
    let p: Provider = serde_yaml::from_str("name: gemini").unwrap();
    assert_eq!(p, Provider::Known(ProviderId::Gemini));
    let p: Provider = serde_yaml::from_str("name: openrouter").unwrap();
    assert_eq!(p, Provider::Known(ProviderId::Openrouter));
}

#[test]
fn provider_custom_requires_endpoint_yaml() {
    let p: Provider = serde_yaml::from_str("name: custom\nendpoint: http://x/v1").unwrap();
    assert!(matches!(p, Provider::Custom { .. }));
}

#[test]
fn legacy_google_and_lmstudio_are_rejected() {
    assert!(serde_yaml::from_str::<Provider>("name: google").is_err());
    assert!(serde_yaml::from_str::<Provider>("name: lmstudio").is_err());
}

/// Drift guard: every catalog provider must be nameable in models.yaml.
/// `Provider` deserializes through the hand-maintained `RawProvider` tag-enum,
/// which is NOT compile-forced against `ProviderId`. This test fails loudly if
/// a `ProviderId` variant is added without a matching `RawProvider` tag,
/// keeping the "single source of truth" promise honest on the YAML surface.
#[test]
fn every_catalog_slug_round_trips_through_yaml() {
    for &p in ProviderId::ALL {
        let yaml = format!("name: {}", p.slug());
        let parsed: Provider = serde_yaml::from_str(&yaml).unwrap_or_else(|e| {
            panic!(
                "catalog slug `{}` must deserialize as a Provider (add it to RawProvider): {e}",
                p.slug()
            )
        });
        assert_eq!(
            parsed,
            Provider::Known(p),
            "slug `{}` must map to Provider::Known({p:?})",
            p.slug()
        );
    }
}
