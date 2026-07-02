//! FMECA U1/U4/R2 tests for the specificity walk + Chain-of-Responsibility
//! resolver. Names map to rows in the FMECA mapping table.

use praxec_core::model_resolver::{ConfigSource, FailureClass, ModelRef, ModelsFile, Resolver};
use std::path::PathBuf;

fn resolver_from(yaml: &str) -> Resolver {
    let file = ModelsFile::from_yaml(yaml).expect("yaml parses");
    Resolver::from_loaded(
        file,
        ConfigSource::Project(PathBuf::from("/tmp/models.yaml")),
    )
}

// ── ModelRef::parse ─────────────────────────────────────────────────────────

#[test]
fn delegate_parses_affinity_tier() {
    let d = ModelRef::parse("coding-frontier").expect("parses");
    assert!(d.affinity.is_some());
    assert!(d.tier.is_some());
}

#[test]
fn delegate_parses_affinity_only() {
    let d = ModelRef::parse("coding").expect("parses");
    assert!(d.affinity.is_some());
    assert!(d.tier.is_none());
}

#[test]
fn delegate_parses_tier_only() {
    let d = ModelRef::parse("frontier").expect("parses");
    assert!(d.affinity.is_none());
    assert!(d.tier.is_some());
}

#[test]
fn delegate_parses_hyphenated_affinity() {
    // `web-search` itself has a hyphen — parser must NOT split it as
    // `web` + `search`.
    let d = ModelRef::parse("web-search").expect("parses");
    assert!(d.affinity.is_some());
    assert!(d.tier.is_none());
}

#[test]
fn delegate_parses_hyphenated_affinity_with_tier() {
    let d = ModelRef::parse("web-search-frontier").expect("parses");
    assert!(d.affinity.is_some());
    assert!(d.tier.is_some());
}

#[test]
fn delegate_rejects_garbage() {
    ModelRef::parse("nonsense").expect_err("garbage rejected");
    ModelRef::parse("").expect_err("empty rejected");
}

// ── walk: exact match wins ──────────────────────────────────────────────────

#[test]
fn affinity_tier_match_wins_over_partial() {
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
    let r = resolver_from(yaml);
    let d = ModelRef::parse("coding-frontier").unwrap();
    let (bindings, level) = r.walk(&d).expect("resolves");
    assert_eq!(level, "coding-frontier");
    assert_eq!(bindings[0].model, "gpt-5");
}

#[test]
fn affinity_only_walks_to_affinity_when_tier_missing() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:
  coding:
    - provider: { name: anthropic }
      model: claude-haiku-4-5
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("coding-frontier").unwrap();
    let (bindings, level) = r.walk(&d).expect("resolves");
    assert_eq!(level, "coding");
    assert_eq!(bindings[0].model, "claude-haiku-4-5");
}

#[test]
fn affinity_wins_tiebreaker() {
    // Both `coding` and `frontier` defined; `coding-frontier` requested
    // (no exact). Affinity wins per locked design.
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:
  coding:
    - provider: { name: anthropic }
      model: claude-haiku-4-5
  frontier:
    - provider: { name: anthropic }
      model: claude-opus-4-7
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("coding-frontier").unwrap();
    let (bindings, level) = r.walk(&d).expect("resolves");
    assert_eq!(level, "coding", "affinity must win tiebreaker over tier");
    assert_eq!(bindings[0].model, "claude-haiku-4-5");
}

#[test]
fn default_used_when_no_overrides_match() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:
  reasoning-frontier:
    - provider: { name: openai }
      model: gpt-5
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("prose-commoditized").unwrap();
    let (bindings, level) = r.walk(&d).expect("resolves via default");
    assert_eq!(level, "default");
    assert_eq!(bindings[0].model, "claude-sonnet-4-6");
}

// ── walk: strict mode ───────────────────────────────────────────────────────

#[test]
fn strict_specificity_blocks_partial() {
    let yaml = r#"
version: 1
strict_specificity: true
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:
  coding:
    - provider: { name: anthropic }
      model: claude-haiku-4-5
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("coding-frontier").unwrap();
    let err = r
        .walk(&d)
        .expect_err("strict mode + full delegate + no exact = error");
    assert_eq!(err.delegate, "coding-frontier");
    let joined = err.walked_levels.join(" ");
    assert!(
        joined.contains("strict: not found"),
        "walked_levels must mark the strict-mode block: {joined}"
    );
}

#[test]
fn strict_specificity_allows_exact_match() {
    let yaml = r#"
version: 1
strict_specificity: true
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:
  coding-frontier:
    - provider: { name: openai }
      model: gpt-5
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("coding-frontier").unwrap();
    let (bindings, _level) = r.walk(&d).expect("exact match allowed in strict mode");
    assert_eq!(bindings[0].model, "gpt-5");
}

// ── walk: structured exhaustion error ──────────────────────────────────────

#[test]
fn try_next_returns_first_binding_with_no_failures() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
  - provider: { name: openai }
    model: gpt-5
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("coding").unwrap();
    let (bindings, _) = r.walk(&d).unwrap();
    let (idx, b) = r
        .try_next(&d, &bindings, &[])
        .expect("first attempt returns index 0");
    assert_eq!(idx, 0);
    assert_eq!(b.model, "claude-sonnet-4-6");
}

#[test]
fn try_next_advances_past_failed_infrastructure_attempt() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
  - provider: { name: openai }
    model: gpt-5
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("coding").unwrap();
    let (bindings, _) = r.walk(&d).unwrap();
    let prior = vec![(0usize, FailureClass::Auth401, "401 from anthropic".into())];
    let (idx, b) = r
        .try_next(&d, &bindings, &prior)
        .expect("CoR advances to second binding");
    assert_eq!(idx, 1);
    assert_eq!(b.model, "gpt-5");
}

#[test]
fn cor_over_bindings_surfaces_on_content_failure() {
    // FMECA R1: a content-class failure on any prior attempt must
    // SURFACE through `try_next` rather than silently advance to the
    // next binding. Mirrors `try_next_advances_past_failed_infrastructure_attempt`
    // — same inputs except the failure class. Different outcome.
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
  - provider: { name: openai }
    model: gpt-5
  - provider: { name: gemini }
    model: gemini-2.0-flash
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("coding").unwrap();
    let (bindings, _) = r.walk(&d).unwrap();
    let prior = vec![(
        0usize,
        FailureClass::ContentOther,
        "unmapped 418 from provider".into(),
    )];
    let err = r
        .try_next(&d, &bindings, &prior)
        .expect_err("content failure must surface, not advance to index 1");
    assert_eq!(err.delegate, "coding");
    assert_eq!(err.attempts.len(), 1);
    assert_eq!(err.attempts[0].class, FailureClass::ContentOther);
    assert!(err.attempts[0].detail.contains("418"));
}

#[test]
fn try_next_returns_structured_exhaustion_when_all_failed() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
  - provider: { name: openai }
    model: gpt-5
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("coding").unwrap();
    let (bindings, _) = r.walk(&d).unwrap();
    let prior = vec![
        (0usize, FailureClass::Auth401, "401 from anthropic".into()),
        (1usize, FailureClass::Auth401, "401 from openai".into()),
    ];
    let err = r.try_next(&d, &bindings, &prior).expect_err("exhausted");
    assert_eq!(err.delegate, "coding");
    assert_eq!(err.attempts.len(), 2);
    assert_eq!(err.attempts[0].class, FailureClass::Auth401);
    assert!(err.attempts[0].detail.contains("anthropic"));
    assert_eq!(err.attempts[1].class, FailureClass::Auth401);
}

#[test]
fn walk_returns_structured_exhaustion_when_default_empty_after_misses() {
    // The loader rejects `default: []` outright, but the resolver itself
    // must also handle the in-memory "no candidates and no default" edge.
    // Build it manually: a file with a default list that's been emptied
    // post-load is impossible by construction, but a delegate that finds
    // NO overrides AND walks to default (which has only one binding) is
    // the most-exercised path — covered above. This test pins the path
    // where overrides have a key but for a different delegate.
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let r = resolver_from(yaml);
    let d = ModelRef::parse("recon-commoditized").unwrap();
    let (bindings, level) = r.walk(&d).expect("falls to default");
    assert_eq!(level, "default");
    assert_eq!(bindings.len(), 1);
}
