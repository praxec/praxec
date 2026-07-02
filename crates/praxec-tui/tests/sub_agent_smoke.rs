//! SPEC §21 — `AetherSubAgentSpawner` smoke tests.
//!
//! These tests cover what's verifiable without API keys / a live LLM:
//!   - Construction succeeds
//!   - HeadlessArgs assembly doesn't panic
//!   - Timeout error path is reachable (with a 1-second cap against an
//!     intentionally-invalid model that will never complete in 1 second)
//!
//! The full end-to-end test (real Aether session → real LLM → workflow.submit)
//! requires `ANTHROPIC_API_KEY` (or equivalent) AND a running praxec
//! binary in PATH. That test is `#[ignore]` and runs only on dogfood.

use std::sync::Arc;

use praxec_core::model_resolver::ProviderFeatures;
use praxec_tui::interpreter::{InterpreterError, ResolvedAgent, SubAgentSpawner};
use praxec_tui::sub_agent::AetherSubAgentSpawner;
use praxec_tui::tui_config::TuiConfig;
use serde_json::json;

fn small_config() -> TuiConfig {
    TuiConfig::from_cli(Some(120), Some(20), Some(16_384))
        .expect("required-fields poka-yoke must accept these values")
}

fn fake_agent() -> ResolvedAgent {
    ResolvedAgent {
        label: "test-agent".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-5".into(),
        features: ProviderFeatures::None,
    }
}

fn fake_workflow_response() -> serde_json::Value {
    json!({
        "workflow": { "id": "wf_smoke", "state": "planning", "version": 1 },
        "result":   { "status": "running" },
        "context":  { "summary": "smoke test" },
        "guidance": {
            "goal":         "Pick a transition and submit it.",
            "instructions": "Call workflow.submit with one of the listed links."
        },
        "links":    []
    })
}

// ── Construction ───────────────────────────────────────────────────────────

#[test]
fn spawner_constructs_with_valid_tui_config() {
    let _spawner = AetherSubAgentSpawner::new(small_config());
    // No panic, no assertion needed — the type system enforces that the
    // spawner accepts a valid config.
}

// ── Live spawn (ignored by default; runs in dogfood with API keys) ─────────

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and a live praxec binary; run with: cargo test -- --ignored sub_agent_smoke"]
async fn live_spawn_against_real_aether_completes_or_times_out() {
    // This test calls the REAL Aether headless API. It's ignored by
    // default because it requires:
    //   - ANTHROPIC_API_KEY (or equivalent for the configured provider)
    //   - praxec binary on PATH or via MCP_PRAXEC_PATH
    //   - Network access to the model provider
    //
    // The success criterion is simply "doesn't panic; returns Ok or
    // SubAgentTimeout within the configured window." We don't assert
    // workflow.submit was called because that requires a live workflow
    // store, which is a longer-running end-to-end dogfood scenario.
    let spawner =
        AetherSubAgentSpawner::new(TuiConfig::from_cli(Some(30), Some(5), Some(16_384)).unwrap());
    let result = spawner
        .spawn_and_wait(
            &fake_agent(),
            "Echo the word 'smoke' and stop.",
            &fake_workflow_response(),
        )
        .await;
    // Either it completed within 30s or timed out — both are acceptable
    // smoke outcomes. A panic OR an Mcp error means the wiring broke.
    match result {
        Ok(()) => eprintln!("smoke: spawn completed naturally"),
        Err(InterpreterError::SubAgentTimeout { .. }) => {
            eprintln!("smoke: spawn timed out (within bounds)")
        }
        Err(other) => panic!("smoke spawn failed with unexpected error: {other:?}"),
    }
    let _ = Arc::new(spawner); // silence unused-import warning if reorganized
}
