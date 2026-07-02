//! Baseline scenarios: realistic patterns the existing declarative surface
//! should handle. See sibling `stress_*` files for the patterns that
//! required additions to make them declarative.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use serde_json::json;

mod common;
use common::*;

/// **B-01.** Simplest possible proxy call: declare one capability, call it.
#[tokio::test]
async fn b01_proxy_default_call() {
    let yaml = r#"
        version: "1.0.0"
        proxy:
          expose:
            - name: hello.echo
              executor: { kind: noop }
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({ "ok": true })));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("proxy_default", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "running");
    assert_eq!(s.last()["workflow"]["state"], "ready");
    assert_eq!(s.link_rels(), vec!["hello.echo"]);

    s.submit("hello.echo", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "running");
    assert_eq!(s.last()["workflow"]["state"], "ready");
}

/// **B-02.** Multi-state governed flow happy path: planning → review → done.
#[tokio::test]
async fn b02_governed_happy_path() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          change:
            initialState: planning
            states:
              planning:
                transitions:
                  submit:
                    target: reviewing
                    executor: { kind: noop }
              reviewing:
                transitions:
                  approve:
                    target: done
                    guards: [{ kind: permission, permission: change.approve }]
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("change", json!({}), anon()).await;
    s.submit("submit", json!({}), anon()).await;
    assert_eq!(s.last()["workflow"]["state"], "reviewing");

    s.submit("approve", json!({}), principal(&["change.approve"]))
        .await;
    assert_eq!(s.last()["result"]["status"], "succeeded");
    assert_eq!(s.last()["workflow"]["state"], "done");
    assert!(s.last()["links"].as_array().unwrap().is_empty());
}

/// **B-03.** Schema rejection includes the legal links so the caller can
/// recover without restarting.
#[tokio::test]
async fn b03_schema_rejection_returns_legal_links() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: open
            states:
              open:
                transitions:
                  go:
                    target: done
                    inputSchema:
                      type: object
                      required: [name]
                      properties: { name: { type: string } }
                      additionalProperties: false
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({ "name": 42 }), anon()).await; // wrong type

    assert_eq!(s.last()["result"]["status"], "running");
    assert_eq!(s.last()["error"]["code"], "INPUT_SCHEMA_VIOLATION");
    assert!(s.link_rels().contains(&"go".to_string()));
}

/// **B-04.** Guard rejection: workflow stays put, response carries
/// recovery links, audit shows `transition.rejected`.
#[tokio::test]
async fn b04_guard_rejection_audited_and_recoverable() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: open
            states:
              open:
                transitions:
                  approve:
                    target: done
                    guards: [{ kind: permission, permission: demo.approve }]
                    executor: { kind: noop }
                  reject:
                    target: open
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({}), anon()).await;
    s.submit("approve", json!({}), anon()).await; // missing permission

    assert_eq!(s.last()["error"]["code"], "GUARD_REJECTED");
    assert_eq!(s.last()["workflow"]["state"], "open");
    let rels = s.link_rels();
    assert!(rels.contains(&"approve".to_string()));
    assert!(rels.contains(&"reject".to_string()));

    let events = s.audit_event_types();
    assert!(events.iter().any(|e| e == "transition.rejected"));
    assert!(events.iter().any(|e| e == "guard.evaluated"));
}

/// **B-05.** Stale `expectedVersion` is rejected even when guards/schema
/// pass.
#[tokio::test]
async fn b05_stale_version_rejected() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: open
            states:
              open:
                transitions:
                  go:
                    target: done
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({}), anon()).await;
    s.submit_with_version("go", 999, json!({}), anon()).await;

    assert_eq!(s.last()["error"]["code"], "STALE_WORKFLOW_VERSION");
}

/// **B-06.** Reliability: retries exhaust → `failed` status, not state advance.
#[tokio::test]
async fn b06_retry_exhaustion_marks_failed_not_advanced() {
    struct AlwaysFail;
    #[async_trait]
    impl Executor for AlwaysFail {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Err(ExecutorError::Transient("nope".into()))
        }
    }

    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor: { kind: noop }
                    reliability:
                      retry: { maxAttempts: 3, retryOn: [transient_error] }
              done:
                terminal: true
    "#;
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(Arc::new(AlwaysFail))));
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({}), anon()).await;

    assert_eq!(s.last()["result"]["status"], "failed");
    assert_eq!(s.last()["workflow"]["state"], "s"); // didn't advance
    let evt = s.audit_event_types();
    assert!(evt.iter().any(|e| e == "executor.retrying"));
    assert!(evt.iter().any(|e| e == "executor.failed"));
}

/// **B-07.** Reliability: fallback wins after primary exhausts retries.
#[tokio::test]
async fn b07_fallback_succeeds_after_primary_exhausts() {
    struct AlwaysFail;
    #[async_trait]
    impl Executor for AlwaysFail {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Err(ExecutorError::Transient("primary down".into()))
        }
    }
    struct AlwaysOk;
    #[async_trait]
    impl Executor for AlwaysOk {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult::default())
        }
    }
    let registry = ByKind::new()
        .with("primary", Arc::new(AlwaysFail))
        .with("backup", Arc::new(AlwaysOk));

    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor: { kind: primary }
                    reliability:
                      retry: { maxAttempts: 1 }
                      fallback:
                        executors:
                          - { kind: backup }
              done:
                terminal: true
    "#;
    let mut s = Scenario::build(yaml, Arc::new(registry));
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({}), anon()).await;

    assert_eq!(s.last()["result"]["status"], "succeeded");
    assert!(s
        .audit_event_types()
        .iter()
        .any(|e| e == "fallback.selected"));
}

/// **B-08.** Capability reference: same definition reused in proxy and
/// inside a workflow transition.
#[tokio::test]
async fn b08_named_capability_reused() {
    let yaml = r#"
        version: "1.0.0"
        capabilities:
          do_thing:
            executor: { kind: noop }
        proxy:
          expose: [{ capability: do_thing }]
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor: { capability: do_thing }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    // Use it from proxy_default…
    s.start("proxy_default", json!({}), anon()).await;
    assert_eq!(s.link_rels(), vec!["do_thing"]);
    s.submit("do_thing", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "running");

    // …and from the named workflow.
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "succeeded");
}
