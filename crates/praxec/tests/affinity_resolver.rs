//! Production models.yaml-backed affinity resolver (gateway.models_yaml).
//!
//! Tasks 1-2 added the `AffinityResolver` trait + the fail-loud default;
//! this guards the production resolver the gateway binary injects when
//! `gateway.models_yaml` is set. The load-bearing regression guard here is
//! `google_binding_maps_to_gemini_prefix`: the resolver MUST emit the
//! factory's `gemini:` key, not the config's `google` display name.

use praxec::affinity_resolver::AgentsYamlAffinityResolver;
use praxec_core::error::ExecutorError;
use praxec_llm_executor::affinity::AffinityResolver;

/// One anthropic override (coding), one gemini override (reasoning), plus
/// an anthropic default. The gemini binding is the regression guard.
const FIXTURE: &str = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:
  coding:
    - provider: { name: anthropic }
      model: claude-opus-4-8
  reasoning:
    - provider: { name: gemini }
      model: gemini-2.5-pro
"#;

fn resolver() -> AgentsYamlAffinityResolver {
    AgentsYamlAffinityResolver::from_yaml_str(FIXTURE).expect("fixture parses")
}

#[tokio::test]
async fn resolves_affinity_to_provider_model() {
    let r = resolver();
    let got = r.resolve("coding").await.expect("coding resolves");
    assert_eq!(got, "anthropic:claude-opus-4-8");
}

#[tokio::test]
async fn google_binding_maps_to_gemini_prefix() {
    let r = resolver();
    let got = r.resolve("reasoning").await.expect("reasoning resolves");
    assert!(
        got.starts_with("gemini:"),
        "google provider must map to the factory's `gemini:` prefix, got: {got}"
    );
    assert!(
        !got.starts_with("google:"),
        "must NOT emit the config display name `google:`, got: {got}"
    );
    assert_eq!(got, "gemini:gemini-2.5-pro");
}

#[tokio::test]
async fn unresolvable_affinity_is_permanent_error() {
    // `default:` is present, so an unknown affinity normally falls through
    // to default. To force exhaustion we need a delegate that does not parse
    // as any affinity/tier — `ModelRef::parse` rejects it before the walk.
    let r = resolver();
    let err = r
        .resolve("totally-bogus-affinity")
        .await
        .expect_err("nonsense affinity must error");
    match &err {
        ExecutorError::Permanent(msg) => {
            assert!(
                msg.contains("totally-bogus-affinity"),
                "error must name the affinity, got: {msg}"
            );
        }
        other => panic!("expected Permanent, got: {other:?}"),
    }
}

/// `resolve_affinity_to_chain` returns ALL bindings in order for a
/// multi-model chain, so the executor can escalate through them on failure.
#[test]
fn resolve_chain_returns_all_bindings_in_order() {
    use praxec::affinity_resolver::resolve_affinity_to_chain;

    const CHAIN_FIXTURE: &str = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-haiku-3-5
overrides:
  coding:
    - provider: { name: anthropic }
      model: claude-haiku-3-5
    - provider: { name: anthropic }
      model: claude-sonnet-4-6
    - provider: { name: anthropic }
      model: claude-opus-4-8
"#;
    let r = AgentsYamlAffinityResolver::from_yaml_str(CHAIN_FIXTURE).expect("fixture parses");
    let chain = resolve_affinity_to_chain(r.resolver(), "coding");
    assert_eq!(
        chain,
        vec![
            "anthropic:claude-haiku-3-5",
            "anthropic:claude-sonnet-4-6",
            "anthropic:claude-opus-4-8",
        ],
        "chain must contain all 3 bindings in order"
    );
}

/// `resolve_affinity_to_chain` returns an empty vec for an unknown affinity.
#[test]
fn resolve_chain_returns_empty_for_bogus_affinity() {
    use praxec::affinity_resolver::resolve_affinity_to_chain;
    let r = resolver();
    let chain = resolve_affinity_to_chain(r.resolver(), "totally-bogus-affinity");
    assert!(
        chain.is_empty(),
        "bogus affinity must yield an empty chain, got: {chain:?}"
    );
}

/// Integration gate (final-review Important gap): drive the REAL affinity
/// closure — the same `resolve_affinity_to_model` the gateway builds in
/// `collect_diagnostics` — through `cost::doctor_check`, exercising the full
/// `ModelRef::parse` → `walk` → `gemini:` prefix → catalog-lookup chain rather
/// than a mock closure. `reasoning` resolves to `gemini:gemini-2.5-pro`, which
/// is NOT in the cost catalog, so under a `max_cost_usd` cap the doctor must
/// emit `COST_CATALOG_MISSING_ENTRY` naming the RESOLVED model.
#[test]
fn real_affinity_closure_drives_cost_doctor_catalog_miss() {
    use chrono::NaiveDate;
    use praxec::affinity_resolver::resolve_affinity_to_model;
    use serde_json::json;

    let r = resolver();
    let closure = move |a: &str| resolve_affinity_to_model(r.resolver(), a);

    let config = json!({
        "workflows": { "wf": { "states": { "s": { "transitions": { "go": {
            "target": "done",
            "executor": {
                "kind": "llm",
                "affinity": "reasoning",
                "prompt_template": "x",
                "max_cost_usd": 1.0
            }
        }}}}}}
    });
    let today = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
    let diags = praxec_llm_executor::cost::doctor_check(&config, today, Some(&closure));

    let errors: Vec<String> = diags
        .iter()
        .filter(|d| d.is_error())
        .map(|d| format!("{d}"))
        .collect();
    assert!(
        errors.iter().any(
            |e| e.contains("COST_CATALOG_MISSING_ENTRY") && e.contains("gemini:gemini-2.5-pro")
        ),
        "the real affinity closure must resolve `reasoning`→`gemini:gemini-2.5-pro` and \
         drive a catalog-miss error under the cap; got: {errors:?}"
    );
}
