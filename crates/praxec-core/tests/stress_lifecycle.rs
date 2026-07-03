//! Stress scenarios that exercise multi-step lifecycle features:
//! approvals quorum, link prefill for LLM callers, idempotency keys,
//! workflow-level timeouts, and transition auto-branching. Each test
//! pins a declarative replacement for a previously procedural workaround.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{Evidence, ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use serde_json::json;

mod common;
use common::*;

/// **S-03. Multi-approver quorum.** "Two of any three reviewers must
/// approve." A common change-management pattern. Without counted evidence,
/// the only options are a custom guard (not declarative) or recording
/// distinct evidence kinds per approver and listing all combinations
/// (combinatorial).
///
/// Fix: the `evidence` guard's `requires` accepts `{ kind, count }` for
/// quorums alongside the bare-string form.
#[tokio::test]
async fn s03_multi_approver_quorum() {
    // Each approve transition records a fresh `approval` evidence record.
    struct Approver;
    #[async_trait]
    impl Executor for Approver {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult {
                output: json!({ "approved": true }),
                evidence: vec![Evidence {
                    kind: "approval".into(),
                    id: format!("ev_{}", uuid::Uuid::new_v4().simple()),
                    uri: None,
                    summary: None,
                    digest: None,
                    confidence: None,
                }],
                child_workflow_id: None,
                next_transition: None,
                suspend: None,
                telemetry: None,
            })
        }
    }
    struct Noop;
    #[async_trait]
    impl Executor for Noop {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult::default())
        }
    }
    let registry = ByKind::new()
        .with("noop", Arc::new(Noop))
        .with("approver", Arc::new(Approver));

    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: pending
            states:
              pending:
                transitions:
                  approve:
                    target: pending          # self-loop until quorum reached
                    actor: human
                    executor: { kind: approver }
                  finalize:
                    target: done
                    guards:
                      - kind: evidence
                        requires:
                          - { kind: approval, count: 2 }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let mut s = Scenario::build_with_evidence(yaml, Arc::new(registry), true);

    s.start("demo", json!({}), anon()).await;

    // First approval: not yet enough; finalize must reject. `approve` is
    // tagged `actor: human`, so submits must come from a human principal.
    s.submit("approve", json!({}), human()).await;
    assert_eq!(s.last()["result"]["status"], "running");
    s.submit("finalize", json!({}), anon()).await;
    assert_eq!(s.last()["error"]["code"], "GUARD_REJECTED");

    // Second approval: now quorum is reached.
    s.submit("approve", json!({}), human()).await;
    s.submit("finalize", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "succeeded");
    assert_eq!(s.last()["workflow"]["state"], "done");
}

/// **S-07. Link prefill (LLM guidance).** A transition declares `prefill`:
/// at link-generation time those values resolve against current scopes
/// and land in `link.args.arguments`. The LLM caller takes that block as
/// the starting point and only generates the genuinely-LLM-required
/// fields (e.g. PR title and body), instead of having to assemble every
/// argument the call needs.
///
/// Fix: transition `prefill` block + reuse of `mapping::resolve_value` at
/// link generation.
#[tokio::test]
async fn s07_link_prefill_arguments() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: tested
            initialContext:
              branch: feat/x
            inputSchema:
              type: object
              properties:
                repo: { type: string }
            states:
              tested:
                transitions:
                  create_pr:
                    target: review
                    inputSchema:
                      type: object
                      required: [repo, base, head, title]
                      properties:
                        repo:  { type: string }
                        base:  { type: string }
                        head:  { type: string }
                        title: { type: string }
                    prefill:
                      repo: "$.workflow.input.repo"
                      base: "main"
                      head: "$.context.branch"
                      labels: ["auto-generated"]
                    executor: { kind: noop }
              review:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({ "repo": "owner/repo" }), anon())
        .await;
    let link = s.last()["links"]
        .as_array()
        .and_then(|a| a.iter().find(|l| l["rel"] == "create_pr"))
        .expect("create_pr link present");
    let prefilled = &link["args"]["arguments"];
    assert_eq!(prefilled["repo"], "owner/repo");
    assert_eq!(prefilled["base"], "main");
    assert_eq!(prefilled["head"], "feat/x");
    assert_eq!(prefilled["labels"], json!(["auto-generated"]));
    // The LLM only has to fill `title` (in inputSchema.required, not in prefilled).
    let inputs = link["inputSchema"]["required"].as_array().unwrap();
    assert!(inputs.iter().any(|v| v == "title"));
}

/// **S-08. Idempotency key auto.** With `executor.idempotencyKey: true`,
/// the runtime computes a stable key per `submit` and feeds it to the
/// executor (REST → header, CLI → env, MCP → arg). Retries within the
/// same submit share the key so downstream services can dedupe.
#[tokio::test]
async fn s08_idempotency_key_auto() {
    use std::sync::Mutex as StdMutex;
    struct Recorder {
        keys: StdMutex<Vec<Option<String>>>,
    }
    #[async_trait]
    impl Executor for Recorder {
        async fn execute(&self, req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            self.keys.lock().unwrap().push(req.idempotency_key.clone());
            Ok(ExecuteResult::default())
        }
    }
    let recorder = Arc::new(Recorder {
        keys: StdMutex::new(vec![]),
    });

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
                    executor:
                      kind: noop
                      idempotencyKey: true
              done:
                terminal: true
    "#;
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(recorder.clone())));
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({}), anon()).await;

    let keys = recorder.keys.lock().unwrap();
    assert_eq!(
        keys.len(),
        1,
        "executor invoked exactly once on a happy path"
    );
    let key = keys[0].as_ref().expect("idempotency key present");
    assert!(
        key.starts_with("wf_") && key.contains(".go."),
        "auto key shape includes workflowId.transition.correlationId; got {key}"
    );
}

/// **S-09. Idempotency key custom template.** A workflow author can
/// provide a custom template using `{workflowId}`, `{transition}`,
/// `{correlationId}` placeholders. Useful when downstream APIs require a
/// specific key format.
#[tokio::test]
async fn s09_idempotency_key_custom_template() {
    use std::sync::Mutex as StdMutex;
    struct Recorder {
        keys: StdMutex<Vec<Option<String>>>,
    }
    #[async_trait]
    impl Executor for Recorder {
        async fn execute(&self, req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            self.keys.lock().unwrap().push(req.idempotency_key.clone());
            Ok(ExecuteResult::default())
        }
    }
    let recorder = Arc::new(Recorder {
        keys: StdMutex::new(vec![]),
    });

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
                    executor:
                      kind: noop
                      idempotencyKey: "praxec:{transition}:{workflowId}"
              done:
                terminal: true
    "#;
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(recorder.clone())));
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({}), anon()).await;

    let keys = recorder.keys.lock().unwrap();
    let key = keys[0].as_ref().unwrap();
    assert!(key.starts_with("praxec:go:wf_"), "got {key}");
}

/// **S-10. Workflow-level lazy timeout.** A workflow declares
/// `timeoutMs` + `onTimeout.target`. If the next `submit`/`get` arrives
/// after the deadline, the runtime auto-transitions to the timeout
/// state, emits `workflow.timed_out`, and short-circuits without
/// running the requested transition.
#[tokio::test]
async fn s10_workflow_level_lazy_timeout() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          short_lived:
            initialState: open
            timeoutMs: 50
            onTimeout:
              target: timed_out_state
            states:
              open:
                transitions:
                  approve:
                    target: done
                    executor: { kind: noop }
              timed_out_state:
                terminal: true
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("short_lived", json!({}), anon()).await;
    // Sleep comfortably past the 50ms deadline. (Margins kept generous — a
    // 1ms deadline + 20ms sleep was a knife-edge that flaked under heavy
    // parallel-test load, where `start` itself could straddle a 1ms boundary.)
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    s.submit("approve", json!({}), anon()).await;
    assert_eq!(s.last()["workflow"]["state"], "timed_out_state");
    assert_eq!(s.last()["result"]["status"], "succeeded");
    assert!(
        s.audit_event_types()
            .iter()
            .any(|t| t == "workflow.timed_out"),
        "audit must include workflow.timed_out"
    );
}

/// **S-14. Transition auto-branches.** "Run tests; if pass go green,
/// if fail go red." Single submit, two outcomes — the branching is
/// declared, not procedurally chosen by the caller.
///
/// Fix: `branches: [{ when, target }]` on transitions, evaluated after
/// the executor's output mapping is applied. First match wins; falls
/// back to the declared `target`.
#[tokio::test]
async fn s14_transition_auto_branches() {
    struct ReturnsBool(bool);
    #[async_trait]
    impl Executor for ReturnsBool {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult {
                output: json!({ "success": self.0 }),
                evidence: vec![],
                child_workflow_id: None,
                next_transition: None,
                suspend: None,
                telemetry: None,
            })
        }
    }

    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: red
            states:
              red:
                transitions:
                  run_tests:
                    target: red                # default fallback
                    executor: { kind: noop }
                    output:
                      passed: "$.output.success"
                    branches:
                      - when:   { kind: expr, expr: "$.context.passed == true" }
                        target: green
                      - when:   { kind: expr, expr: "$.context.passed == false" }
                        target: red
              green:
                terminal: true
    "#;

    // Tests pass → go green.
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(Arc::new(ReturnsBool(true)))));
    s.start("demo", json!({}), anon()).await;
    s.submit("run_tests", json!({}), anon()).await;
    assert_eq!(s.last()["workflow"]["state"], "green");
    assert!(
        s.audit_event_types()
            .iter()
            .any(|t| t == "transition.branched")
    );

    // Tests fail → stay in red.
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(Arc::new(ReturnsBool(false)))));
    s.start("demo", json!({}), anon()).await;
    s.submit("run_tests", json!({}), anon()).await;
    assert_eq!(s.last()["workflow"]["state"], "red");
}
