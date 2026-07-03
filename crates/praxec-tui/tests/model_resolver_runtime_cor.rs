//! T18 — per-list runtime CoR over actual provider failures.
//!
//! Pins the contract for `spawn_with_cor`:
//!  - 401/429/network-class failures advance to the next binding
//!  - content-class failures (mapped to ContentOther) surface immediately
//!  - all-bindings-fail returns structured `ModelResolutionExhausted`
//!  - SubAgentTimeout bypasses CoR (retry-budget path owns it)
//!
//! Uses an in-test `CountingSpawner` that returns scripted outcomes.

use std::sync::Mutex;

use async_trait::async_trait;
use praxec_core::model_resolver::{
    Binding, ConfigSource, FailureClass, ModelRef, ModelsFile, Provider, ProviderFeatures, Resolver,
};
use praxec_tui::interpreter::{
    AgentRegistry, InterpreterError, ResolutionError, ResolvedAgent, ResolvedBindingList,
    SubAgentSpawner, YamlAgentRegistry, classify_spawn_error, spawn_with_cor,
};
use serde_json::{Value, json};
use std::path::PathBuf;

// ── test double ────────────────────────────────────────────────────────────

struct ScriptedSpawner {
    outcomes: Mutex<Vec<Result<(), InterpreterError>>>,
    seen: Mutex<Vec<(String, String, String)>>, // (label, provider, model)
}

impl ScriptedSpawner {
    fn new(outcomes: Vec<Result<(), InterpreterError>>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes),
            seen: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl SubAgentSpawner for ScriptedSpawner {
    async fn spawn_and_wait(
        &self,
        agent: &ResolvedAgent,
        _system_prompt: &str,
        _workflow_response: &Value,
    ) -> Result<(), InterpreterError> {
        self.seen.lock().unwrap().push((
            agent.label.clone(),
            agent.provider.clone(),
            agent.model.clone(),
        ));
        let mut o = self.outcomes.lock().unwrap();
        if o.is_empty() {
            panic!("scripted spawner exhausted (test rigging error)");
        }
        o.remove(0)
    }
}

// ── fixtures ───────────────────────────────────────────────────────────────

fn resp_at(state: &str) -> Value {
    json!({"workflow": {"state": state}})
}

fn three_binding_yaml() -> &'static str {
    r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
  - provider: { name: openai }
    model: gpt-5
  - provider: { name: gemini }
    model: gemini-2.0-flash
"#
}

fn list_of_three() -> ResolvedBindingList {
    let file = ModelsFile::from_yaml(three_binding_yaml()).unwrap();
    let resolver = Resolver::from_loaded(
        file,
        ConfigSource::Project(PathBuf::from("/tmp/models.yaml")),
    );
    let reg = YamlAgentRegistry::new(resolver);
    reg.resolve_bindings("coding").expect("resolves")
}

fn auth_401_error() -> InterpreterError {
    InterpreterError::Mcp {
        tool: "aether/sub_agent/coding".into(),
        source: anyhow::anyhow!("API error: HTTP 401 invalid_api_key"),
    }
}

fn content_other_error() -> InterpreterError {
    InterpreterError::Mcp {
        tool: "aether/sub_agent/coding".into(),
        source: anyhow::anyhow!("API error: HTTP 422 schema validation failed"),
    }
}

// ── classify_spawn_error ──────────────────────────────────────────────────

#[test]
fn classify_auth_401() {
    assert_eq!(
        classify_spawn_error(&auth_401_error()),
        FailureClass::Auth401
    );
}

#[test]
fn classify_rate_limit_429() {
    let err = InterpreterError::Mcp {
        tool: "x".into(),
        source: anyhow::anyhow!("Rate limited: 429 too many requests"),
    };
    assert_eq!(classify_spawn_error(&err), FailureClass::RateLimit429);
}

#[test]
fn classify_network_error() {
    let err = InterpreterError::Mcp {
        tool: "x".into(),
        source: anyhow::anyhow!("Network error: connection reset"),
    };
    assert_eq!(classify_spawn_error(&err), FailureClass::NetworkTimeout);
}

#[test]
fn classify_unknown_falls_through_as_content_other() {
    // The FMECA R1 anchor: an unmapped failure surfaces, not advance.
    let err = InterpreterError::Mcp {
        tool: "x".into(),
        source: anyhow::anyhow!("Some new error mode we haven't seen"),
    };
    assert_eq!(classify_spawn_error(&err), FailureClass::ContentOther);
    assert!(!classify_spawn_error(&err).is_infrastructure());
}

#[test]
fn classify_sub_agent_timeout_is_content_other() {
    // Timeouts go through the retry budget, NOT through CoR.
    let err = InterpreterError::SubAgentTimeout {
        agent: "coding".into(),
        state: "thinking".into(),
    };
    assert_eq!(classify_spawn_error(&err), FailureClass::ContentOther);
}

// ── spawn_with_cor ────────────────────────────────────────────────────────

#[tokio::test]
async fn cor_returns_index_zero_when_primary_succeeds() {
    let list = list_of_three();
    let spawner = ScriptedSpawner::new(vec![Ok(())]);
    let idx = spawn_with_cor(&spawner, &list, "go", &resp_at("running"))
        .await
        .expect("primary succeeds");
    assert_eq!(idx, 0);
    assert_eq!(spawner.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cor_advances_to_index_one_on_401() {
    let list = list_of_three();
    let spawner = ScriptedSpawner::new(vec![Err(auth_401_error()), Ok(())]);
    let idx = spawn_with_cor(&spawner, &list, "go", &resp_at("running"))
        .await
        .expect("secondary succeeds after primary 401");
    assert_eq!(idx, 1);
    let seen = spawner.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].1, "anthropic");
    assert_eq!(seen[1].1, "openai");
}

#[tokio::test]
async fn cor_surfaces_on_content_failure_without_advancing() {
    // FMECA R1 inverse: ContentOther on the primary must NOT trigger
    // advance to the secondary — the operator needs the real signal.
    let list = list_of_three();
    let spawner = ScriptedSpawner::new(vec![Err(content_other_error())]);
    let err = spawn_with_cor(&spawner, &list, "go", &resp_at("running"))
        .await
        .expect_err("content failure surfaces");
    assert!(matches!(err, InterpreterError::Mcp { .. }));
    // Only one binding was tried.
    assert_eq!(spawner.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cor_returns_exhaustion_when_all_bindings_fail_infrastructure() {
    let list = list_of_three();
    let spawner = ScriptedSpawner::new(vec![
        Err(auth_401_error()),
        Err(auth_401_error()),
        Err(auth_401_error()),
    ]);
    let err = spawn_with_cor(&spawner, &list, "go", &resp_at("running"))
        .await
        .expect_err("exhausted");
    match err {
        InterpreterError::AgentResolution {
            source: ResolutionError::Exhausted(e),
            ..
        } => {
            assert_eq!(e.delegate, "coding");
            assert_eq!(e.attempts.len(), 3);
            assert!(e.attempts.iter().all(|a| a.class == FailureClass::Auth401));
        }
        other => panic!("unexpected error shape: {other:?}"),
    }
    assert_eq!(spawner.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn cor_bubbles_sub_agent_timeout_without_advancing() {
    // SubAgentTimeout is the retry-budget's domain, not CoR's. The
    // primary's timeout MUST propagate unchanged so walk_workflow can
    // count it against the retry budget.
    let list = list_of_three();
    let spawner = ScriptedSpawner::new(vec![Err(InterpreterError::SubAgentTimeout {
        agent: "coding".into(),
        state: "thinking".into(),
    })]);
    let err = spawn_with_cor(&spawner, &list, "go", &resp_at("running"))
        .await
        .expect_err("timeout propagates");
    assert!(matches!(err, InterpreterError::SubAgentTimeout { .. }));
    assert_eq!(spawner.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cor_mixed_429_then_200_advances_once() {
    let list = list_of_three();
    let rate_limited = InterpreterError::Mcp {
        tool: "aether/sub_agent/coding".into(),
        source: anyhow::anyhow!("Rate limited: 429 retry-after 60s"),
    };
    let spawner = ScriptedSpawner::new(vec![Err(rate_limited), Ok(())]);
    let idx = spawn_with_cor(&spawner, &list, "go", &resp_at("running"))
        .await
        .expect("secondary succeeds after primary 429");
    assert_eq!(idx, 1);
}

// ── legacy registry: single-binding compatibility ─────────────────────────

#[test]
fn legacy_registry_default_resolve_bindings_returns_single_entry() {
    use praxec_tui::agent_config::AgentConfig;
    use praxec_tui::interpreter::LegacyAgentRegistry;
    use std::collections::HashMap;

    let mut m = HashMap::new();
    m.insert(
        "planner".to_string(),
        AgentConfig {
            name: "planner".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
        },
    );
    let reg = LegacyAgentRegistry::new(m);
    let list = reg.resolve_bindings("planner").expect("resolves");
    assert_eq!(list.bindings.len(), 1);
    assert_eq!(list.bindings[0].model, "claude-sonnet-4-6");
    assert!(matches!(
        list.bindings[0].provider,
        Provider::Known(praxec_core::providers::ProviderId::Anthropic)
    ));
    assert!(list.level.contains("legacy"));
}

// Silence "unused" warnings from the test-only imports above. Bindings
// + ProviderFeatures show up across test modules so we exercise them
// via a no-op compile-touch.
#[allow(dead_code)]
fn _import_touch(_: &Binding, _: &ProviderFeatures, _: &ModelRef) {}
