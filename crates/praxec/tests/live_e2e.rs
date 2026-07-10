//! E1 (testing-strategy) — the **live binary over stdio**, end-to-end. Spawns the
//! real `praxec serve` as a subprocess, speaks MCP over the same stdio
//! transport the cockpit's `StdioGateway` uses, and drives `hello-flow` through
//! the §32 surface: launch → begin → human gate (waiting) → approve → succeeded.
//!
//! This is the ~5% of the pyramid that proves the wiring the mocks stand in for:
//! the real binary, config loading, the MCP serialization boundary, and a full
//! launch→drive→resolve loop. The drive needs no LLM (noop + human steps only).

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use serde_json::{Value, json};

const CONFIG: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/hello-flow.yaml"
);

async fn call(peer: &rmcp::service::Peer<rmcp::service::RoleClient>, args: Value) -> Value {
    let params = CallToolRequestParams::new("praxec.command".to_string())
        .with_arguments(args.as_object().cloned().unwrap_or_default());
    let result = peer.call_tool(params).await.expect("praxec.command call");
    result.structured_content.expect("structured response body")
}

#[tokio::test]
async fn the_live_binary_drives_hello_flow_to_succeeded_over_stdio() {
    let bin = env!("CARGO_BIN_EXE_praxec");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("serve").arg("--config").arg(CONFIG);
    // The fixture's `gateway` block carries both `allow_ephemeral` (in-memory boot)
    // and the default `human` principal (so this caller can drive the human gate).

    let transport = TokioChildProcess::new(cmd).expect("spawn praxec serve");
    let service = ().serve(transport).await.expect("mcp client handshake");
    let peer = service.peer().clone();

    // 1. Launch: `praxec.command { definitionId }` starts the mission at `start`.
    let launched = call(&peer, json!({ "definitionId": "hello_flow" })).await;
    assert_eq!(
        launched.pointer("/result/status").and_then(Value::as_str),
        Some("running"),
        "a fresh mission is running; got: {launched:#}"
    );
    let id = launched
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .expect("launched mission id")
        .to_string();
    let v0 = launched
        .pointer("/workflow/version")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    // 2. Begin → reaches the human gate (`review`, waiting).
    let at_gate = call(
        &peer,
        json!({ "workflowId": id, "expectedVersion": v0, "transition": "begin" }),
    )
    .await;
    assert_eq!(
        at_gate.pointer("/result/status").and_then(Value::as_str),
        Some("waiting"),
        "the human gate is a waiting mission; got: {at_gate:#}"
    );
    let v1 = at_gate
        .pointer("/workflow/version")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    // 3. Approve → resolves to succeeded (the outcome `approved == true` holds).
    let resolved = call(
        &peer,
        json!({ "workflowId": id, "expectedVersion": v1, "transition": "approve" }),
    )
    .await;
    assert_eq!(
        resolved.pointer("/result/status").and_then(Value::as_str),
        Some("succeeded"),
        "approving resolves the mission to succeeded; got: {resolved:#}"
    );

    let _ = service.cancel().await;
}

/// E2 (testing-strategy) — the headless `orchestrate` CLI end to end against a
/// **real provider**, the orchestrator picking transitions toward hello-flow's
/// outcomes with HITL gates auto-approved. Live-only by nature (a real LLM
/// decides), so it's `#[ignore]`-gated: CI skips it; run on demand with
/// `PRAXEC_E2E_MODEL=anthropic:… <provider key> cargo test -p praxec
/// --test live_e2e -- --ignored`. The *deterministic* equivalent (the same
/// drive_mission + chooser + bus + consumer spine, LLM mocked) is S6/S7/S8.
///
/// **Local / deterministic endpoint:** with the base-URL override
/// (`provider_factory::openai_base_override`) this same test runs against any
/// OpenAI-*compatible* server (a local LiteLLM/Ollama/vLLM proxy or a mock) —
/// set `PRAXEC_LLM_BASE_URL=http://127.0.0.1:PORT` and
/// `PRAXEC_E2E_MODEL=openai:MODEL`. That exercises the full headless → agent →
/// LLM → transition wiring through the real provider/streaming stack without
/// hitting a paid API, and (pointed at a scripted server) deterministically.
#[tokio::test]
#[ignore = "live: needs a real provider — set PRAXEC_E2E_MODEL + key, run with --ignored"]
async fn orchestrate_drives_hello_flow_to_succeeded_live() {
    // No model configured → a clean skip even under `--ignored` (no false failure).
    let Ok(model) = std::env::var("PRAXEC_E2E_MODEL") else {
        eprintln!("skipping live E2: set PRAXEC_E2E_MODEL to run");
        return;
    };
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_praxec"))
        .args([
            "orchestrate",
            "--config",
            CONFIG,
            "--definition",
            "hello_flow",
            "--model",
            &model,
            "--policy",
            "auto-approve",
        ])
        .output()
        .await
        .expect("run orchestrate");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "orchestrate exited non-zero.\nstdout: {stdout}\nstderr: {stderr}"
    );
    // The printed DriveOutcome must be `Resolved { status: "succeeded", .. }`.
    assert!(
        stdout.contains("Resolved") && stdout.contains("succeeded"),
        "expected a succeeded resolution; got:\n{stdout}"
    );
}
