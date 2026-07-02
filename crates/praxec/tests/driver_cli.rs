//! P4 — the `command` / `query` driver CLI. Each invocation is its own process;
//! a `store.kind: sqlite` backend persists workflow state between them, so an
//! operator can drive and observe a workflow step-by-step from a shell.

use std::io::Write;
use std::process::Command;

use serde_json::Value;

// Reuse the same hello-flow fixture durable_e2e uses.
const HELLO_FLOW: &str = include_str!("../../../examples/hello-flow.yaml");

/// Run `praxec <sub> <json>` against `config_path`, return parsed stdout JSON.
fn run(config_path: &str, sub: &str, json_arg: &str, human: bool) -> Value {
    let bin = env!("CARGO_BIN_EXE_praxec");
    let mut cmd = Command::new(bin);
    cmd.arg(sub).arg("--config").arg(config_path);
    if human {
        cmd.arg("--human");
    }
    cmd.arg(json_arg);
    // Skip per-boot embeddings (no network in tests); the lexical index suffices.
    cmd.env("PRAXEC_EMBEDDING_FILE", "/tmp/praxec-no-embed.json");
    let out = cmd.output().expect("run praxec subcommand");
    assert!(
        out.status.success(),
        "{sub} exited non-zero.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).expect("subcommand stdout is JSON")
}

/// Regression: `inspect workflow` is a sync CLI handler that used to build a
/// nested `tokio::runtime::Runtime` and `block_on` — which panics ("Cannot
/// start a runtime from within a runtime") because `run_cli` is already async.
/// It must run cleanly and name the workflow.
#[test]
fn inspect_workflow_succeeds_without_a_nested_runtime_panic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("inspect.db");
    let config_path = dir.path().join("praxec.yaml");
    let config = format!(
        "{HELLO_FLOW}\nstore:\n  kind: sqlite\n  path: {}\n",
        db.display()
    );
    std::fs::File::create(&config_path)
        .and_then(|mut f| f.write_all(config.as_bytes()))
        .expect("write config");
    let cfg = config_path.to_string_lossy().to_string();

    // Start a workflow so there is an instance to inspect.
    let started = run(&cfg, "command", r#"{"definitionId":"hello_flow"}"#, false);
    let id = started["workflow"]["id"]
        .as_str()
        .expect("workflow id")
        .to_string();

    // `inspect workflow --config <cfg> <id>` — pre-fix this panicked.
    let bin = env!("CARGO_BIN_EXE_praxec");
    let out = Command::new(bin)
        .arg("inspect")
        .arg("workflow")
        .arg("--config")
        .arg(&cfg)
        .arg(&id)
        .env("PRAXEC_EMBEDDING_FILE", "/tmp/praxec-no-embed.json")
        .output()
        .expect("run inspect");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "inspect exited non-zero (nested-runtime panic?).\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains(&id),
        "inspect output should name the workflow instance: {stdout}"
    );
}

#[test]
fn command_starts_and_query_observes_across_processes_against_sqlite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("driver.db");
    let config_path = dir.path().join("praxec.yaml");
    let config = format!(
        "{HELLO_FLOW}\nstore:\n  kind: sqlite\n  path: {}\n",
        db.display()
    );
    std::fs::File::create(&config_path)
        .and_then(|mut f| f.write_all(config.as_bytes()))
        .expect("write config");
    let cfg = config_path.to_string_lossy().to_string();

    // Process 1: start the workflow.
    let started = run(&cfg, "command", r#"{"definitionId":"hello_flow"}"#, false);
    let id = started["workflow"]["id"]
        .as_str()
        .expect("workflow id")
        .to_string();

    // Process 2 (fresh process): query it back — state survived via sqlite.
    let observed = run(&cfg, "query", &format!(r#"{{"workflowId":"{id}"}}"#), false);
    assert_eq!(observed["workflow"]["id"].as_str(), Some(id.as_str()));
    assert!(
        observed["workflow"]["state"].is_string(),
        "query returns the persisted workflow state: {observed}"
    );
}
