//! E2E #1 (testing-strategy, resource-lifecycle) — the **durable headless
//! lifecycle**, all the way down through the filesystem. Unlike E1 (in-memory,
//! ephemeral), this boots the real binary against an on-disk **SQLite** store in
//! a tempdir, drives `hello-flow` to `succeeded` over stdio, then **restarts a
//! fresh binary process against the same DB file** and confirms the resolved
//! mission persisted and is still queryable.
//!
//! What it proves end-to-end that nothing else does: config off disk → the
//! durable store path (not the ephemeral guard) → real persistence + flush →
//! recovery across a process restart. The restart-reads-it invariant is also the
//! strongest evidence of a clean teardown (a fresh process sees a consistent DB).

use std::io::Write;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{Peer, RoleClient};
use rmcp::transport::TokioChildProcess;
use serde_json::{Value, json};

/// The shared fixture (human-operator hello-flow) + an on-disk SQLite store.
const HELLO_FLOW: &str = include_str!("../../../examples/hello-flow.yaml");

async fn tool(peer: &Peer<RoleClient>, name: &str, args: Value) -> Value {
    let params = CallToolRequestParams::new(name.to_string())
        .with_arguments(args.as_object().cloned().unwrap_or_default());
    let result = peer.call_tool(params).await.expect("mcp tool call");
    result.structured_content.expect("structured response body")
}

/// Spawn a fresh `praxec serve` against `config_path`, run the MCP
/// handshake, and return the running service (its `Drop`/`cancel` reaps the child).
async fn serve(config_path: &str) -> rmcp::service::RunningService<RoleClient, ()> {
    let bin = env!("CARGO_BIN_EXE_praxec");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("serve").arg("--config").arg(config_path);
    let transport = TokioChildProcess::new(cmd).expect("spawn praxec serve");
    ().serve(transport).await.expect("mcp client handshake")
}

#[tokio::test]
async fn a_resolved_mission_survives_a_process_restart_against_a_sqlite_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("missions.db");
    let config_path = dir.path().join("praxec.yaml");
    // hello-flow + a durable SQLite store (top-level `store:` — gateway.rs build).
    let config = format!(
        "{HELLO_FLOW}\nstore:\n  kind: sqlite\n  path: {}\n",
        db.display()
    );
    {
        let mut f = std::fs::File::create(&config_path).expect("write config");
        f.write_all(config.as_bytes()).expect("write config bytes");
    }
    let config_str = config_path.to_string_lossy().to_string();

    // ── Run 1: drive hello_flow to succeeded, then shut the process down. ──
    let id = {
        let service = serve(&config_str).await;
        let peer = service.peer().clone();

        let launched = tool(
            &peer,
            "praxec.command",
            json!({ "definitionId": "hello_flow" }),
        )
        .await;
        let id = launched["workflow"]["id"]
            .as_str()
            .expect("mission id")
            .to_string();
        let v0 = launched["workflow"]["version"].as_u64().unwrap_or(0);

        let at_gate = tool(
            &peer,
            "praxec.command",
            json!({ "workflowId": id, "expectedVersion": v0, "transition": "begin" }),
        )
        .await;
        let v1 = at_gate["workflow"]["version"].as_u64().unwrap_or(0);

        let resolved = tool(
            &peer,
            "praxec.command",
            json!({ "workflowId": id, "expectedVersion": v1, "transition": "approve" }),
        )
        .await;
        assert_eq!(
            resolved.pointer("/result/status").and_then(Value::as_str),
            Some("succeeded"),
            "run 1 should resolve the mission; got {resolved:#}"
        );
        // Explicit clean shutdown (reaps the child) before the restart.
        let _ = service.cancel().await;
        id
    };

    // ── Run 2: a fresh process against the SAME db must still see it succeeded. ──
    let service2 = serve(&config_str).await;
    let peer2 = service2.peer().clone();
    let recovered = tool(&peer2, "praxec.query", json!({ "workflowId": id })).await;
    assert_eq!(
        recovered.pointer("/result/status").and_then(Value::as_str),
        Some("succeeded"),
        "the resolved mission must persist across a restart; got {recovered:#}"
    );
    let _ = service2.cancel().await;
}
