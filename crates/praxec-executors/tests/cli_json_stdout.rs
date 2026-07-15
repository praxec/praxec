//! Verifies the CLI executor auto-parses stdout as JSON when it parses
//! cleanly, exposing the result under `$.output.json` for the workflow.

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::NullAuditSink;
use praxec_core::config::resolve_str;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_executors::default_registry;
use serde_json::json;

#[tokio::test]
async fn cli_stdout_parsed_as_json_into_output_json() {
    // Skip if bash isn't available in this environment.
    if std::process::Command::new("bash")
        .arg("-c")
        .arg("true")
        .output()
        .is_err()
    {
        eprintln!("skipping: bash not available");
        return;
    }

    let yaml = r#"
        version: "1.0.0"
        connections:
          shell:
            kind: cli
            command: bash
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor:
                      kind: cli
                      connection: shell
                      args:
                        - "-c"
                        # Single-quote the JSON so bash brace expansion leaves it alone.
                        - "echo '{\"count\": 42, \"label\": \"alpha\"}'"
                    output:
                      count: "$.output.json.count"
                      label: "$.output.json.label"
              done:
                terminal: true
    "#;
    let config = resolve_str(yaml).unwrap();
    let runtime = WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&config)),
        Arc::new(InMemoryWorkflowStore::new()),
        default_registry(&config),
        Arc::new(DefaultGuardEvaluator::new()),
        Arc::new(NullAuditSink),
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);

    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let id = started["workflow"]["id"].as_str().unwrap().to_string();
    let v = started["workflow"]["version"].as_u64().unwrap();
    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: id,
            expected_version: v,
            transition: "go".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(resp["result"]["status"], "succeeded");
    assert_eq!(resp["context"]["count"], 42);
    assert_eq!(resp["context"]["label"], "alpha");
}
